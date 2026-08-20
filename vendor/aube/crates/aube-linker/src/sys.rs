//! Platform-specific directory-link and bin-shim creation.
//!
//! ## Directory links ([`create_dir_link`])
//!
//! On Unix, [`create_dir_link`] is a thin wrapper around
//! `std::os::unix::fs::symlink` — same semantics as any other
//! symlink-based linker.
//!
//! On Windows, [`create_dir_link`] creates an **NTFS junction**
//! rather than a real symlink. Junctions don't require Developer
//! Mode or admin rights, which is the whole reason pnpm and npm use
//! them for `node_modules` tree layout on Windows (they go through
//! Node's `fs.symlink(target, path, 'junction')`, which translates
//! to the same `FSCTL_SET_REPARSE_POINT` dance the `junction` crate
//! wraps). Real Windows symlinks via `std::os::windows::fs::
//! symlink_dir` would require either elevated privileges or
//! Developer Mode — neither of which is available on GitHub-hosted
//! `windows-latest` runners or on vanilla Windows developer
//! machines, so using real symlinks would break installs in both
//! places.
//!
//! There is one wrinkle vs. Unix symlinks that callers must honor:
//! **Junctions only accept absolute targets.** If the caller passes
//! a relative target, this helper resolves it against the link's
//! parent directory before handing it to `junction::create`.
//!
//! ## Bin shims ([`create_bin_shim`])
//!
//! Two dials control the shape of each entry:
//!
//! - `prefer_symlinked_executables` (POSIX only). Default `None` is
//!   "platform default", which on POSIX is a plain symlink — same as
//!   pnpm's `preferSymlinkedExecutables=true`. `Some(false)` falls
//!   back to a shell-script shim matching the Windows shell wrapper;
//!   callers opt into this when they need `extendNodePath` to
//!   actually set `NODE_PATH` (a bare symlink can't export env vars).
//!   Windows never creates real symlinks here — Developer Mode /
//!   admin rights would be required, and both are commonly absent on
//!   CI and developer machines.
//!
//! - `extend_node_path`. When `true`, shell/cmd/powershell shims set
//!   `NODE_PATH` to `$basedir/..` (the top-level `node_modules`) so
//!   the shimmed binary can resolve modules regardless of where it's
//!   invoked from. Matches pnpm's `extendNodePath=true`. No-op when
//!   the final output is a symlink (POSIX default) — symlinks can't
//!   export env vars, which is why callers who care pair it with
//!   `prefer_symlinked_executables=false`.
//!
//! On Windows, `create_bin_shim` writes three plain-text wrapper
//! scripts into the bin directory — `.cmd` (for cmd.exe), `.ps1`
//! (PowerShell), and an extensionless shell script (Git Bash /
//! MSYS2). This is the same approach pnpm and npm use via
//! `cmd-shim`, and it avoids the need for Developer Mode or admin
//! rights entirely.

use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

/// Create a directory link from `link` to `target`.
///
/// - Unix: a plain symlink (relative or absolute target OK).
/// - Windows: an NTFS junction (relative targets are resolved to
///   absolute against `link`'s parent first).
///
/// A leftover link, dangling link, or EMPTY directory occupying `link`
/// is cleared and the create retried once — `junction::create` opens
/// with `fs::create_dir`, so anything at the path surfaces as
/// `ERROR_ALREADY_EXISTS` (os 183) before the reparse write, and the
/// Unix `symlink` gives the equivalent `EEXIST`. Same shape as
/// `write_shim_file` below, and the same deliberate restraint: the
/// non-recursive `remove_dir` will NOT wipe a POPULATED directory, so a
/// real package tree in the slot still surfaces the error rather than
/// being silently destroyed by a generic helper. Callers that own the
/// slot and know a full clear is correct do it themselves before
/// calling (see the `Stale` branch in `link.rs`).
pub fn create_dir_link(target: &Path, link: &Path) -> io::Result<()> {
    match create_dir_link_once(target, link) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(link);
            let _ = std::fs::remove_dir(link);
            create_dir_link_once(target, link)
        }
        other => other,
    }
}

fn create_dir_link_once(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        let abs_target = if target.is_absolute() {
            target.to_path_buf()
        } else {
            let parent = link.parent().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "junction link has no parent directory",
                )
            })?;
            normalize_path(&parent.join(target))
        };
        create_junction_with_retry(&abs_target, link)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "directory links are not supported on this platform",
        ))
    }
}

#[cfg(windows)]
fn create_junction_with_retry(target: &Path, link: &Path) -> io::Result<()> {
    let mut attempt = 0;
    let mut delay_ms = 50u64;
    loop {
        match junction::create(target, link) {
            Ok(()) => return Ok(()),
            Err(e) if is_retriable_link_error(&e) && attempt < 9 => {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                delay_ms = (delay_ms * 2).min(2000);
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(windows)]
fn is_retriable_link_error(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5 | 32))
}

/// Options controlling the shape of a generated bin entry.
///
/// `Default` preserves the pre-settings behavior: POSIX symlink,
/// Windows shim without `NODE_PATH`.
#[derive(Debug, Clone, Copy, Default)]
pub struct BinShimOptions<'a> {
    /// Export `NODE_PATH` in shell / cmd / PowerShell shims so the
    /// shimmed binary can resolve transitives that live outside the
    /// directory tree walked by Node from the cwd. Has no effect when
    /// the final entry is a POSIX symlink (symlinks can't export env
    /// vars). When `hidden_modules_dir` is set, the shim's NODE_PATH
    /// becomes a colon/semicolon-separated list of: the bin's
    /// top-level `node_modules`, then the hidden modules dir at
    /// `<virtual_store>/node_modules`. Otherwise it's just the
    /// top-level `node_modules`.
    pub extend_node_path: bool,
    /// POSIX-only. `None` → platform default (symlink). `Some(true)` is
    /// equivalent. `Some(false)` writes a shell-script shim instead, so
    /// `extend_node_path` can actually inject `NODE_PATH`. Ignored on
    /// Windows — shims are always used there.
    pub prefer_symlinked_executables: Option<bool>,
    /// Absolute path to the virtual store's hidden modules dir
    /// (`<project>/node_modules/.aube/node_modules`). When set and
    /// `extend_node_path=true`, the generated shim includes it in
    /// `NODE_PATH` so transitives hoisted there resolve when the
    /// shimmed binary asks Node for them — pnpm's `.pnpm/node_modules`
    /// behavior. Independent of `bin_dir` so workspace-member bin
    /// shims (whose `bin_dir` is nowhere near `.aube/`) get the same
    /// resolution shape as the root importer's `.bin/`.
    pub hidden_modules_dir: Option<&'a Path>,
}

/// Target and environment recovered from an aube-generated bin wrapper.
///
/// Paths are resolved against the wrapper's parent. `node_path` is an
/// OS-native path list ready to pass to [`std::process::Command::env`].
#[derive(Debug, PartialEq, Eq)]
pub struct ResolvedBinShim {
    pub target: PathBuf,
    pub node_path: Option<OsString>,
}

/// Create bin shims for a package binary.
///
/// - Unix (default / `prefer_symlinked_executables != Some(false)`):
///   a symlink from `bin_dir/<name>` to `target`, with the target
///   chmod'd to 755. The link stores a relative path — see
///   [`symlink_bin_target`] for which directory it is relative to.
/// - Unix (`prefer_symlinked_executables = Some(false)`): a shell
///   wrapper that `exec`s `target` directly or via its detected
///   interpreter. If `extend_node_path` is set, the wrapper exports
///   `NODE_PATH` first.
/// - Windows: three wrapper scripts in `bin_dir`:
///   - `<name>.cmd` — batch wrapper for cmd.exe
///   - `<name>.ps1` — PowerShell wrapper
///   - `<name>` (no extension) — shell wrapper for Git Bash / MSYS2
///
///   `extend_node_path` sets `NODE_PATH` near the top of each wrapper.
///
/// The `target` path should be absolute; neither wrappers nor symlinks
/// store it as given — both embed a relative path so the tree stays
/// relocatable even for scoped bin names under `.bin/@scope/`.
pub fn create_bin_shim(
    bin_dir: &Path,
    name: &str,
    target: &Path,
    opts: BinShimOptions<'_>,
) -> io::Result<()> {
    validate_bin_name(name)?;
    #[cfg(unix)]
    {
        let write_shim = matches!(opts.prefer_symlinked_executables, Some(false));
        let link_path = bin_dir.join(name);
        let link_parent = link_path.parent().unwrap_or(bin_dir);
        std::fs::create_dir_all(link_parent)?;
        let _ = std::fs::remove_file(&link_path);
        if write_shim {
            let rel = relative_bin_target(link_parent, target);
            let node_path = opts
                .extend_node_path
                .then(|| shim_node_path(link_parent, bin_dir, opts.hidden_modules_dir, "/", ":"));
            let launch = detect_bin_launch(target);
            if let Some(prog) = shim_name_shadows_its_interpreter(name, &launch) {
                warn_bin_shim_name_is_interpreter(name, prog);
                return Ok(());
            }
            std::fs::write(
                &link_path,
                generate_posix_shim(&launch, &rel, node_path.as_deref()),
            )?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&link_path, std::fs::Permissions::from_mode(0o755))?;
            if matches!(launch, BinLaunch::Direct) && target.exists() {
                let _ = std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755));
            }
        } else {
            std::os::unix::fs::symlink(symlink_bin_target(link_parent, target), &link_path)?;
            use std::os::unix::fs::PermissionsExt;
            if target.exists() {
                let _ = std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755));
            }
        }
    }
    #[cfg(windows)]
    {
        let link_path = bin_dir.join(name);
        let link_parent = link_path.parent().unwrap_or(bin_dir);
        // Clear stale shims or legacy symlinks. Old aube versions wrote
        // these as junctions. `remove_file` fails on a junction, so
        // fall through to `remove_dir` to avoid leaving a stale entry
        // that later `fs::write` cannot overwrite (ERROR_ALREADY_EXISTS).
        for p in win_shim_paths(bin_dir, name) {
            if std::fs::remove_file(&p).is_err() {
                let _ = std::fs::remove_dir(&p);
            }
        }
        // Tolerate `AlreadyExists` from the parent mkdir. Rayon-parallel
        // callers race on the same `.bin/`. Windows also returns os 183
        // spuriously when the dir sits behind a junction, even when the
        // dir is visible.
        if let Err(e) = std::fs::create_dir_all(link_parent)
            && e.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(e);
        }

        let rel = relative_bin_target(link_parent, target);
        let rel_backslash = rel.replace('/', "\\");
        let rel_fwdslash = rel.replace('\\', "/");

        // Classifies a native-executable target as `BinLaunch::Direct`, so
        // each wrapper below execs the binary itself rather than handing it
        // to `node` (#394).
        let launch = detect_bin_launch(target);
        if let Some(prog) = shim_name_shadows_its_interpreter(name, &launch) {
            warn_bin_shim_name_is_interpreter(name, prog);
            return Ok(());
        }
        // cmd.exe wants backslash paths; PowerShell + the Git-Bash `.sh`
        // wrapper want forward-slash paths. NODE_PATH itself is parsed by
        // Node.js, which on Windows always splits on `;` (`path.delimiter`)
        // regardless of which shell launched it, so every Windows shim uses
        // `;`. Mixing `:` here would make Node treat the multi-entry value
        // as one invalid path and silently drop the hidden-modules entry.
        let node_path_backslash = opts
            .extend_node_path
            .then(|| shim_node_path(link_parent, bin_dir, opts.hidden_modules_dir, "\\", ";"));
        let node_path_fwdslash = opts
            .extend_node_path
            .then(|| shim_node_path(link_parent, bin_dir, opts.hidden_modules_dir, "/", ";"));

        write_shim_file(
            &bin_dir.join(format!("{name}.cmd")),
            generate_cmd_shim(&launch, &rel_backslash, node_path_backslash.as_deref()).as_bytes(),
        )?;
        write_shim_file(
            &bin_dir.join(format!("{name}.ps1")),
            generate_ps1_shim(&launch, &rel_fwdslash, node_path_fwdslash.as_deref()).as_bytes(),
        )?;
        write_shim_file(
            &bin_dir.join(name),
            generate_sh_shim(&launch, &rel_fwdslash, node_path_fwdslash.as_deref()).as_bytes(),
        )?;
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (bin_dir, name, target, opts);
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "bin shims are not supported on this platform",
        ));
    }
    Ok(())
}

/// Reject bin-entry keys that would let a hostile `package.json`
/// aim a shim outside its `.bin/` directory. npm/pnpm had the same
/// class of bug (GHSA-p4v2-fp7g-q4rg / CVE-2024-27298). Accepts a
/// bare filename, or exactly one scope-prefix segment `@scope/name`
/// to match pnpm's `.bin/@scope/` layout.
pub fn validate_bin_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name.len() > 255 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid bin name: {name:?}"),
        ));
    }
    let parts: Vec<&str> = name.split('/').collect();
    let ok = match parts.as_slice() {
        [bare] => is_safe_bin_component(bare),
        [scope, bare] => {
            scope.starts_with('@')
                && scope.len() > 1
                && is_safe_bin_component(scope)
                && is_safe_bin_component(bare)
        }
        _ => false,
    };
    if !ok {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid bin name: {name:?}"),
        ));
    }
    Ok(())
}

/// Reject relative bin target paths that escape the package root,
/// are absolute, or carry Windows drive / UNC prefixes.
pub fn validate_bin_target(rel: &str) -> io::Result<()> {
    if rel.is_empty() || rel.contains('\0') || rel.contains('\\') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid bin target: {rel:?}"),
        ));
    }
    // Shell-metachar reject: the generated `.cmd` / `.ps1` / sh shims
    // splice this string into double-quoted command lines that PowerShell
    // (`$(...)`, `` ` ``, `$env:`) and cmd.exe (`%VAR%`) re-evaluate
    // before invocation. npm / pnpm / yarn all reject these on `bin`
    // targets too — no real package ships such a path.
    for ch in rel.chars() {
        if matches!(
            ch,
            '$' | '`'
                | '%'
                | '"'
                | '\''
                | '&'
                | '|'
                | '^'
                | ';'
                | '<'
                | '>'
                | '('
                | ')'
                | '!'
                | '*'
                | '?'
        ) || ch.is_control()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bin target contains shell metacharacter: {rel:?}"),
            ));
        }
    }
    let path = Path::new(rel);
    if path.is_absolute()
        || path.has_root()
        || rel.starts_with('/')
        || rel.len() >= 2 && rel.as_bytes()[1] == b':'
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("absolute bin target: {rel:?}"),
        ));
    }
    for comp in path.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("bin target escapes package: {rel:?}"),
                ));
            }
        }
    }
    Ok(())
}

fn is_safe_bin_component(s: &str) -> bool {
    if s.is_empty() || s == "." || s == ".." {
        return false;
    }
    if s.bytes()
        .any(|b| b == 0 || b == b'/' || b == b'\\' || b.is_ascii_control())
    {
        return false;
    }
    // Windows-only extras: `:` opens an NTFS alternate data stream
    // and separates drive letters, reserved device names map to
    // physical devices, and trailing dot / space gets stripped by
    // the filesystem so `con.` collides with `con`. npm, pnpm, and
    // bun all accept these on POSIX so this reject must stay
    // platform-gated — otherwise packages with a legitimate `:` in
    // their bin key (a handful of cordova / ionic tools) stop
    // linking on Linux and macOS.
    #[cfg(windows)]
    {
        if s.contains(':') || is_windows_reserved(s) || s.ends_with('.') || s.ends_with(' ') {
            return false;
        }
    }
    true
}

#[cfg(windows)]
fn is_windows_reserved(s: &str) -> bool {
    let stem = match s.find('.') {
        Some(i) => &s[..i],
        None => s,
    };
    let upper = stem.to_ascii_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "NUL" | "AUX" => true,
        s if s.len() == 4
            && (s.starts_with("COM") || s.starts_with("LPT"))
            && s.as_bytes()[3].is_ascii_digit()
            && s.as_bytes()[3] != b'0' =>
        {
            true
        }
        _ => false,
    }
}

/// Remove bin shims previously created by [`create_bin_shim`].
///
/// On Unix, removes the symlink. On Windows, removes the `.cmd`,
/// `.ps1`, and extensionless wrapper scripts.
pub fn remove_bin_shim(bin_dir: &Path, name: &str) {
    if validate_bin_name(name).is_err() {
        return;
    }
    let link_path = bin_dir.join(name);
    let _ = std::fs::remove_file(&link_path);
    #[cfg(windows)]
    for p in win_shim_paths(bin_dir, name).into_iter().skip(1) {
        let _ = std::fs::remove_file(&p);
    }
    if let Some(parent) = link_path.parent()
        && parent != bin_dir
    {
        let _ = std::fs::remove_dir(parent);
    }
}

/// Atomic shim write. Stale dir or junction at `dst` makes `fs::write`
/// fail with `ERROR_ALREADY_EXISTS` (os 183). Try direct write first.
/// On that error, wipe whatever blocks the path (file, dir, junction)
/// and retry once. Fast path stays allocation-free.
#[cfg(windows)]
fn write_shim_file(dst: &Path, contents: &[u8]) -> io::Result<()> {
    match std::fs::write(dst, contents) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists || e.raw_os_error() == Some(183) => {
            // `remove_dir` (non-recursive) clears an empty dir or a
            // junction. A populated dir at a shim path is a real
            // conflict. Let the retry write surface that error instead
            // of silently wiping the subtree with `remove_dir_all`.
            let _ = std::fs::remove_file(dst);
            let _ = std::fs::remove_dir(dst);
            std::fs::write(dst, contents)
        }
        Err(e) => Err(e),
    }
}

/// Paths of every Windows shim file `create_bin_shim` writes for
/// `name`: the extensionless wrapper, the `.cmd` stub, and the
/// `.ps1` stub. Index 0 is the extensionless wrapper — callers that
/// already unlinked it (the unix-first branch of `remove_bin_shim`)
/// can skip it with `.into_iter().skip(1)`.
///
/// Public so an ownership check can be driven off the SAME list the writer
/// uses. A guard that hardcodes its own extensions drifts from this one
/// silently, and the drift is invisible until a foreign shim is overwritten.
#[cfg(windows)]
pub fn win_shim_paths(bin_dir: &Path, name: &str) -> [PathBuf; 3] {
    [
        bin_dir.join(name),
        bin_dir.join(format!("{name}.cmd")),
        bin_dir.join(format!("{name}.ps1")),
    ]
}

/// Compute the relative path from `base_dir` to `target`, using
/// forward slashes.
///
/// On Windows, strip any `\\?\` verbatim drive prefix from both inputs
/// before diffing. Mixing a plain `C:\…` base with a verbatim
/// `\\?\C:\…` target makes `pathdiff` treat the two `Component::Prefix`
/// values as distinct (`Disk` != `VerbatimDisk`) and fall back to
/// returning the raw absolute target. The raw target then gets
/// interpolated into the `.cmd` shim as `"%~dp0\\\\?\\<target>"`, which
/// `cmd.exe` + Node surface as the classic `Cannot find module
/// '<bin>\\?\\<target>'` error. Stripping on both sides keeps the
/// prefix components equal so `pathdiff` produces the expected
/// `..\\…` form.
fn relative_bin_target(base_dir: &Path, target: &Path) -> String {
    let base = aube_util::path::strip_verbatim_prefix(base_dir);
    let target = aube_util::path::strip_verbatim_prefix(target);
    pathdiff::diff_paths(&target, &base)
        .unwrap_or(target)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Pick what a `.bin` symlink actually stores. Relative is the portable,
/// npm/pnpm-matching form: it keeps `node_modules` self-contained, so the
/// tree survives a bind-mount into a container or a multi-stage `COPY`.
///
/// Which directory to anchor on is not free, because the kernel resolves
/// a symlink's `..` against the link's PHYSICAL parent, not the path it
/// was reached through — a distinction the sibling shim branch never
/// faces, since a shim resolves `$basedir/<rel>` lexically from `argv[0]`.
///
/// - Anchoring on the SURFACE `link_parent` is right whenever `.bin/`
///   sits where it appears to: every root `.bin/`, and per-dep `.bin/`
///   dirs materialized inside the project.
/// - A per-dep `.bin/` in a store-shared package is reached through a
///   `.store/<name>@<ver>` symlink into the global virtual store, whose
///   entries carry a `-<hash>` suffix the surface names lack. The
///   surface-anchored path dangles there. When the target is store-
///   resident too, the physical form is additionally project-INDEPENDENT,
///   which matters more than portability in a shared directory: an
///   absolute target wrote one project's path into a directory every
///   project shares, so the next install silently repointed the earlier
///   project's bin, and deleting that project left it dangling. A target
///   that resolves back into the project (a `diskMaterializePackages`
///   dep) still yields a project-specific entry — no worse than the
///   absolute form it replaces, but not fixed by this either.
///
/// Falls back to the absolute target when nothing resolves — a bin whose
/// target does not exist yet keeps the pre-fix behavior rather than
/// gaining a guessed relative path.
#[cfg(unix)]
fn symlink_bin_target(link_parent: &Path, target: &Path) -> std::path::PathBuf {
    let Ok(resolved) = std::fs::canonicalize(target) else {
        return target.to_path_buf();
    };
    let surface = relative_bin_target(link_parent, target);
    if std::fs::canonicalize(link_parent.join(&surface)).is_ok_and(|p| p == resolved) {
        return surface.into();
    }
    match std::fs::canonicalize(link_parent) {
        Ok(physical) => relative_bin_target(&physical, &resolved).into(),
        Err(_) => target.to_path_buf(),
    }
}

/// Build the value the bin shim assigns to `NODE_PATH`. Always starts
/// with the `node_modules/` that holds the `.bin/` directory itself
/// (recovers Node's `cwd` walk-up from a shim invoked outside its
/// project). When the caller supplies `hidden_modules_dir`, that path
/// is appended so transitives hoisted to `<virtual_store>/node_modules`
/// — the only place auto-installed peers like `typescript` live for an
/// isolated install — resolve too. Matches the load-bearing entries of
/// pnpm's own NODE_PATH (the bin's `node_modules`, then the hidden
/// `.pnpm/node_modules`).
///
/// `path_sep` is `/` on POSIX/PowerShell/Git-Bash and `\` for cmd.exe;
/// `list_sep` is `:` on POSIX, `;` on cmd.exe. Each entry is prefixed
/// with `$basedir/` (or `%~dp0` for cmd via the caller's prefix —
/// cmd's `%~dp0` already ends with a backslash so no extra path-sep is
/// emitted between prefix and entry).
fn shim_node_path(
    link_parent: &Path,
    bin_dir: &Path,
    hidden_modules_dir: Option<&Path>,
    path_sep: &str,
    list_sep: &str,
) -> String {
    let (basedir_prefix, basedir_suffix) = if path_sep == "\\" {
        // cmd: `%~dp0` already ends in a backslash, so don't emit one.
        ("%~dp0", "")
    } else {
        ("$basedir", "/")
    };
    let normalize = |rel: String| -> String {
        if path_sep == "\\" {
            rel.replace('/', "\\")
        } else {
            rel.replace('\\', "/")
        }
    };
    let mut entries: Vec<String> = Vec::with_capacity(2);
    let top = normalize(relative_bin_target(
        link_parent,
        bin_dir.parent().unwrap_or(bin_dir),
    ));
    entries.push(format!("{basedir_prefix}{basedir_suffix}{top}"));
    if let Some(hidden) = hidden_modules_dir {
        let rel = normalize(relative_bin_target(link_parent, hidden));
        entries.push(format!("{basedir_prefix}{basedir_suffix}{rel}"));
    }
    entries.join(list_sep)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BinLaunch {
    Direct,
    Interpreter(String),
}

/// The interpreter a wrapper for `name` would need, when writing that
/// wrapper is impossible because `name` IS that interpreter.
///
/// Such a wrapper is unwritable rather than merely awkward. It lands at
/// `.bin/<prog>`, and `.bin` goes on `PATH` for every lifecycle script,
/// so BOTH of the launch paths a wrapper has resolve back to it: the
/// `$basedir/<prog>` preference finds it on disk, and the bare `exec
/// <prog>` fallback finds it again through `PATH`. There is no third
/// path and no absolute interpreter to bake in — a wrapper stores only
/// relative paths so the tree stays relocatable. pnpm declines the bin
/// in this situation; do the same, and say so, rather than emit a
/// wrapper that cannot terminate (#656).
///
/// Compares the WHOLE name, which makes a scoped `@scope/node` no
/// collision at all. That is deliberate, not an oversight: a scoped bin
/// lands one level down at `.bin/@scope/node`, and `PATH` carries
/// `.bin`, not `.bin/@scope` — so the `exec <prog>` fallback reaches the
/// real interpreter, and only the `$basedir/<prog>` half self-refers,
/// which [`interpreter_launch_block`]'s `#!` test already rejects.
/// Declining it would delete a bin that works.
///
/// Returns `None` for a `Direct` launch, which names no interpreter and
/// therefore cannot collide: that is the path a package whose bin really
/// is a native `node` binary takes, and it keeps working.
fn shim_name_shadows_its_interpreter<'a>(name: &str, launch: &'a BinLaunch) -> Option<&'a str> {
    let BinLaunch::Interpreter(prog) = launch else {
        return None;
    };
    (name == prog).then_some(prog.as_str())
}

fn warn_bin_shim_name_is_interpreter(name: &str, prog: &str) {
    tracing::warn!(
        code = aube_codes::warnings::WARN_AUBE_BIN_SHIM_NAME_IS_INTERPRETER,
        "skipping bin {name:?}: it is named after the interpreter it needs ({prog}), \
         so any wrapper would resolve back to itself"
    );
}

/// Read the shebang line of `target` to determine how a bin shim
/// launches it. Known script extensions retain their interpreter
/// fallback. Existing targets with native executable magic are launched
/// directly, as are `.exe` targets that a postinstall may replace with a
/// host-native executable after the shim has already been written.
///
/// Only reads the first 256 bytes — enough for any realistic shebang
/// line without pulling large bundled scripts into memory.
fn detect_bin_launch(target: &Path) -> BinLaunch {
    let mut buf = [0u8; 256];
    let (n, target_exists) = match std::fs::File::open(target) {
        Ok(mut file) => (file.read(&mut buf).unwrap_or(0), true),
        Err(_) => (0, false),
    };
    let content = &buf[..n];
    if n > 2
        && content.starts_with(b"#!")
        && let Some(line_end) = content.iter().position(|&b| b == b'\n')
    {
        let line = String::from_utf8_lossy(&content[2..line_end]);
        let line = line.trim();
        // Strip `/usr/bin/env ` prefix (with optional -S flag)
        let prog = if let Some(rest) = line.strip_prefix("/usr/bin/env") {
            let rest = rest.trim_start();
            let rest = rest.strip_prefix("-S").map_or(rest, |r| r.trim_start());
            // Strip leading env var assignments (KEY=val)
            rest.split_whitespace()
                .find(|s| !s.contains('='))
                .unwrap_or("node")
        } else {
            // Absolute path like /usr/bin/node → take basename
            line.split_whitespace()
                .next()
                .and_then(|p| p.rsplit('/').next())
                .unwrap_or("node")
        };
        // `prog` is later interpolated verbatim into `.cmd` / `.ps1`
        // / `.sh` shim templates. Any byte outside a conservative
        // identifier class would let an attacker-published bin
        // script (whose shebang we are parsing right here) break
        // out of the shim's quoted strings and run arbitrary cmd
        // commands on every shim invocation. Reject anything that
        // is not shell-safe on every supported platform and fall
        // through to the extension-based default.
        if is_safe_prog(prog) {
            return BinLaunch::Interpreter(prog.to_string());
        }
        // Unsafe shebang. Log it rather than rewriting silently so
        // the fall-through is visible in install output. Both path
        // and prog go through Debug formatting so any terminal
        // escape sequences smuggled in either one are printed as
        // escaped literals rather than acted on by the terminal.
        tracing::warn!("ignoring unsafe shebang interpreter in {target:?}: {prog:?}");
    }
    default_launch_for_target(
        target,
        content,
        target_exists && !content.starts_with(b"#!"),
    )
}

/// The character class `prog` is allowed to draw from. Derived from
/// the set of tokens that appear as real npm package interpreter
/// shebangs (`node`, `bash`, `sh`, `python3`, `python3.11`, `ruby`,
/// `deno`, `bun`) — all ASCII alphanumerics plus `.`, `_`, `+`, `-`.
/// Rejects `"`, `&`, `|`, `<`, `>`, `^`, `%`, NUL, whitespace, and
/// every other cmd.exe / PowerShell / sh metacharacter.
fn is_safe_prog(prog: &str) -> bool {
    if prog.is_empty() || prog.len() > 64 {
        return false;
    }
    // The first character must be alphanumeric. A leading `-`, `.`,
    // `_`, or `+` is rejected even though those characters are safe
    // in the interior, because no real interpreter name starts with
    // one and a leading `-` would otherwise produce a shim that
    // looks like a CLI flag when inspected.
    let mut chars = prog.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

fn default_launch_for_target(target: &Path, content: &[u8], allow_direct: bool) -> BinLaunch {
    match target.extension().and_then(|e| e.to_str()) {
        Some("js" | "cjs" | "mjs") => BinLaunch::Interpreter("node".to_string()),
        Some("cmd" | "bat") => BinLaunch::Interpreter("cmd".to_string()),
        Some("ps1") => BinLaunch::Interpreter("pwsh".to_string()),
        Some("sh") => BinLaunch::Interpreter("sh".to_string()),
        Some(ext) if allow_direct && ext.eq_ignore_ascii_case("exe") => BinLaunch::Direct,
        _ if allow_direct && is_native_executable(content) => BinLaunch::Direct,
        _ => BinLaunch::Interpreter("node".to_string()),
    }
}

/// True when the leading bytes are the magic number of a native
/// executable format: ELF (Linux/BSD), Mach-O incl. fat/universal
/// (macOS), or PE (`MZ`, Windows). A bin target in one of these formats
/// must be exec'd directly by the kernel — wrapping it in `node <target>`
/// (the JS-launcher default) hands a binary to a JS engine and fails
/// (#394: esbuild's postinstall replaces its JS launcher with the native
/// binary, and the pre-existing `node` shim then chokes on it).
pub fn is_native_executable(bytes: &[u8]) -> bool {
    const MACHO_MAGICS: [&[u8; 4]; 8] = [
        b"\xFE\xED\xFA\xCE", // Mach-O 32-bit BE
        b"\xFE\xED\xFA\xCF", // Mach-O 64-bit BE
        b"\xCE\xFA\xED\xFE", // Mach-O 32-bit LE
        b"\xCF\xFA\xED\xFE", // Mach-O 64-bit LE
        b"\xCA\xFE\xBA\xBE", // fat/universal BE
        b"\xBE\xBA\xFE\xCA", // fat/universal LE
        b"\xCA\xFE\xBA\xBF", // fat/universal 64-bit BE
        b"\xBF\xBA\xFE\xCA", // fat/universal 64-bit LE
    ];
    if bytes.starts_with(b"\x7FELF") || bytes.starts_with(b"MZ") {
        return true;
    }
    bytes.len() >= 4 && MACHO_MAGICS.iter().any(|m| bytes.starts_with(*m))
}

/// Run-time substitute for any `prog` that reaches a shim generator
/// without passing `is_safe_prog`. Every caller in this crate goes
/// through `detect_bin_launch` and never trips this branch, but a
/// future caller that bypasses that path would otherwise produce a
/// shim with attacker-controlled bytes. A `tracing::error!` is emitted
/// so the regression is visible in release builds too, not only in
/// debug.
fn safe_prog(prog: &str) -> &str {
    if is_safe_prog(prog) {
        prog
    } else {
        tracing::error!(
            code = aube_codes::errors::ERR_AUBE_UNSAFE_SHEBANG_INTERPRETER,
            "refusing to splice unsafe prog {prog:?} into shim, substituting \"node\""
        );
        "node"
    }
}

/// Extract the `%~dp0`-relative target a Windows `.cmd` shim execs.
///
/// Both shapes `create_bin_shim` emits: the direct-exec wrapper for a native
/// binary (`@"%~dp0\<rel>" %*`) and the node wrapper, whose IF branch names
/// `node.exe` and whose ELSE branch carries the real target. Mirrors the parse
/// `unlink_bins` performs, and reads the same on any platform so the logic can
/// be unit-tested without a Windows runner.
pub fn parse_win_shim_target(content: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let line = line.trim();
        if let Some(after) = line.strip_prefix("@\"%~dp0\\") {
            let end = after.find('"')?;
            return Some(after[..end].to_string());
        }
        if line.contains("%~dp0\\") && !line.contains(".exe\"") {
            let start = line.find("%~dp0\\")?;
            let after = &line[start + 6..];
            let end = after.find('"')?;
            return Some(after[..end].to_string());
        }
        None
    })
}

/// Render the `.cmd` wrapper text for a Windows bin shim.
///
/// Deliberately NOT `#[cfg(windows)]`, and public: this is pure string
/// formatting with no platform API, and the ownership guard's parser has to be
/// testable against what this actually emits. Gating it to Windows is what
/// forced that test onto hand-written fixtures, and a parser checked only
/// against invented input is how the writer and the reader drifted apart in the
/// first place.
///
/// `cfg(test)` keeps it out of a non-Windows release build, where nothing but
/// the round-trip test calls it.
#[cfg(any(windows, test))]
fn generate_cmd_shim(
    launch: &BinLaunch,
    rel_target_backslash: &str,
    node_path_value: Option<&str>,
) -> String {
    if matches!(launch, BinLaunch::Direct) {
        let node_path =
            node_path_value.map_or(String::new(), |val| format!("@SET NODE_PATH={val}\r\n"));
        return format!(
            "@SETLOCAL\r\n\
             {node_path}\
             @\"%~dp0\\{rel_target_backslash}\" %*\r\n"
        );
    }
    let BinLaunch::Interpreter(prog) = launch else {
        unreachable!();
    };
    let prog = safe_prog(prog);
    let node_path =
        node_path_value.map_or(String::new(), |val| format!("@SET NODE_PATH={val}\r\n"));
    format!(
        "@SETLOCAL\r\n\
         {node_path}\
         @IF EXIST \"%~dp0\\{prog}.exe\" (\r\n\
         \x20 \"%~dp0\\{prog}.exe\" \"%~dp0\\{rel_target_backslash}\" %*\r\n\
         ) ELSE (\r\n\
         \x20 @SET PATHEXT=%PATHEXT:;.JS;=;%\r\n\
         \x20 {prog} \"%~dp0\\{rel_target_backslash}\" %*\r\n\
         )\r\n"
    )
}

#[cfg(windows)]
fn generate_ps1_shim(
    launch: &BinLaunch,
    rel_target_fwdslash: &str,
    node_path_value: Option<&str>,
) -> String {
    if matches!(launch, BinLaunch::Direct) {
        let node_path =
            node_path_value.map_or(String::new(), |val| format!("$env:NODE_PATH=\"{val}\"\n"));
        return format!(
            "#!/usr/bin/env pwsh\n\
             $basedir=Split-Path $MyInvocation.MyCommand.Definition -Parent\n\
             {node_path}\
             $ret=0\n\
             if ($MyInvocation.ExpectingInput) {{\n\
             \x20 $input | & \"$basedir/{rel_target_fwdslash}\" $args\n\
             }} else {{\n\
             \x20 & \"$basedir/{rel_target_fwdslash}\" $args\n\
             }}\n\
             $ret=$LASTEXITCODE\n\
             exit $ret\n"
        );
    }
    let BinLaunch::Interpreter(prog) = launch else {
        unreachable!();
    };
    let prog = safe_prog(prog);
    let node_path =
        node_path_value.map_or(String::new(), |val| format!("$env:NODE_PATH=\"{val}\"\n"));
    format!(
        "#!/usr/bin/env pwsh\n\
         $basedir=Split-Path $MyInvocation.MyCommand.Definition -Parent\n\
         \n\
         {node_path}\
         $exe=\"\"\n\
         if ($PSVersionTable.PSVersion -lt \"6.0\" -or $IsWindows) {{\n\
         \x20 $exe=\".exe\"\n\
         }}\n\
         $ret=0\n\
         if (Test-Path \"$basedir/{prog}$exe\") {{\n\
         \x20 if ($MyInvocation.ExpectingInput) {{\n\
         \x20\x20\x20 $input | & \"$basedir/{prog}$exe\" \"$basedir/{rel_target_fwdslash}\" $args\n\
         \x20 }} else {{\n\
         \x20\x20\x20 & \"$basedir/{prog}$exe\" \"$basedir/{rel_target_fwdslash}\" $args\n\
         \x20 }}\n\
         \x20 $ret=$LASTEXITCODE\n\
         }} else {{\n\
         \x20 if ($MyInvocation.ExpectingInput) {{\n\
         \x20\x20\x20 $input | & \"{prog}$exe\" \"$basedir/{rel_target_fwdslash}\" $args\n\
         \x20 }} else {{\n\
         \x20\x20\x20 & \"{prog}$exe\" \"$basedir/{rel_target_fwdslash}\" $args\n\
         \x20 }}\n\
         \x20 $ret=$LASTEXITCODE\n\
         }}\n\
         exit $ret\n"
    )
}

#[cfg(windows)]
fn generate_sh_shim(
    launch: &BinLaunch,
    rel_target_fwdslash: &str,
    node_path_value: Option<&str>,
) -> String {
    if matches!(launch, BinLaunch::Direct) {
        let node_path =
            node_path_value.map_or(String::new(), |val| format!("export NODE_PATH=\"{val}\"\n"));
        return format!(
            "#!/bin/sh\n\
             basedir=$(dirname \"$(echo \"$0\" | sed -e 's,\\\\,/,g')\")\n\
             \n\
             case `uname` in\n\
             \x20\x20\x20 *CYGWIN*|*MINGW*|*MSYS*)\n\
             \x20\x20\x20\x20\x20\x20\x20 if command -v cygpath > /dev/null 2>&1; then\n\
             \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20 basedir=`cygpath -w \"$basedir\"`\n\
             \x20\x20\x20\x20\x20\x20\x20 fi\n\
             \x20\x20\x20 ;;\n\
             esac\n\
             \n\
             {node_path}\
             exec \"$basedir/{rel_target_fwdslash}\" \"$@\"\n"
        );
    }
    let BinLaunch::Interpreter(prog) = launch else {
        unreachable!();
    };
    let launch_block = interpreter_launch_block(safe_prog(prog), rel_target_fwdslash);
    let node_path =
        node_path_value.map_or(String::new(), |val| format!("export NODE_PATH=\"{val}\"\n"));
    format!(
        "#!/bin/sh\n\
         basedir=$(dirname \"$(echo \"$0\" | sed -e 's,\\\\,/,g')\")\n\
         \n\
         case `uname` in\n\
         \x20\x20\x20 *CYGWIN*|*MINGW*|*MSYS*)\n\
         \x20\x20\x20\x20\x20\x20\x20 if command -v cygpath > /dev/null 2>&1; then\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20 basedir=`cygpath -w \"$basedir\"`\n\
         \x20\x20\x20\x20\x20\x20\x20 fi\n\
         \x20\x20\x20 ;;\n\
         esac\n\
         \n\
         {node_path}\
         {launch_block}"
    )
}

/// The `sh` block that launches `rel_target_fwdslash` through `prog`,
/// shared verbatim by [`generate_posix_shim`] and the Windows Git-Bash
/// `generate_sh_shim` so the two cannot drift. They previously carried
/// the same text twice behind opposite `#[cfg]`s, which is how a fix for
/// #656 came to be applied to the Windows copy — whose test passed —
/// while the unix copy that actually runs stayed broken.
///
/// The `-x` preference is inherited from npm's `cmd-shim` and means
/// "prefer an interpreter installed next to me over `PATH`". The `#!`
/// test is what keeps it honest: `.bin/` is a namespace of
/// package-declared bin NAMES, not of interpreters, so `$basedir/<prog>`
/// is very often another wrapper of ours rather than a real `<prog>`
/// binary. Delegating to one silently prepends that wrapper's own target
/// to `argv`, and when the name collides with the wrapper being written
/// it exec'd itself forever. A real interpreter is a native executable
/// and never opens with `#!`, so the test admits exactly the case the
/// clause was written for and rejects every wrapper.
///
/// The probe has to fail CLOSED, and both halves of that are
/// load-bearing rather than defensive dressing. An empty command
/// substitution compares unequal to `#!` and would take the delegate
/// branch — the exact behavior this guard exists to prevent — so every
/// way the probe can fail has to be steered onto the `PATH` fallback
/// instead.
///
/// `|| echo '#!'` covers the failures that report themselves: the target
/// is executable but not readable, or `head` is unavailable.
///
/// `command -p` covers the failure that does NOT. `PATH` carries `.bin`
/// for every lifecycle script (`aube-scripts`), so a dependency
/// declaring a bin named `head` shadows the probe with its own wrapper —
/// and a wrapper that exits 0 printing anything other than `#!` makes
/// the probe answer "native interpreter" with no error for `||` to
/// catch. `command -p` resolves through the system default `PATH`, which
/// a package-declared bin cannot substitute into. It is POSIX and works
/// in `sh`, `bash`, `zsh` and `dash`; where `command -p` or `head` is
/// missing entirely the non-zero exit still lands on `|| echo '#!'`, so
/// the combination is closed in every direction. Each mode reproduced by
/// hand — a zero-exit shadowing `head` delegates without `command -p`.
fn interpreter_launch_block(prog: &str, rel_target_fwdslash: &str) -> String {
    format!(
        "if [ -x \"$basedir/{prog}\" ] && [ \"$(command -p head -c 2 \"$basedir/{prog}\" 2>/dev/null || echo '#!')\" != '#!' ]; then\n\
         \x20 exec \"$basedir/{prog}\" \"$basedir/{rel_target_fwdslash}\" \"$@\"\n\
         else\n\
         \x20 exec {prog} \"$basedir/{rel_target_fwdslash}\" \"$@\"\n\
         fi\n"
    )
}

/// Marker the POSIX shim writer stamps into every generated file so
/// [`parse_posix_shim_target`] can unambiguously identify our shims and
/// recover the `$basedir`-relative target path on uninstall. Any format
/// change here must bump the `v1` suffix so older shims stop being
/// recognized (forcing a reinstall) rather than being silently
/// misparsed.
pub const POSIX_SHIM_MARKER_PREFIX: &str = "# aube-bin-shim v1 target=";

/// POSIX shell-script shim used when `prefer_symlinked_executables=false`
/// (so `extend_node_path` can actually inject `NODE_PATH`). Mirrors the
/// Windows `generate_sh_shim` output without the cygpath dance, with a
/// stamped [`POSIX_SHIM_MARKER_PREFIX`] comment at the top so
/// `unlink_bins` can locate the embedded target without having to parse
/// the shell body.
///
/// The interpreter branch comes from [`interpreter_launch_block`], which
/// both generators share so their templates cannot drift apart.
#[cfg(unix)]
fn generate_posix_shim(
    launch: &BinLaunch,
    rel_target_fwdslash: &str,
    node_path_value: Option<&str>,
) -> String {
    let node_path =
        node_path_value.map_or(String::new(), |val| format!("export NODE_PATH=\"{val}\"\n"));
    if matches!(launch, BinLaunch::Direct) {
        return format!(
            "#!/bin/sh\n\
             {POSIX_SHIM_MARKER_PREFIX}{rel_target_fwdslash}\n\
             basedir=$(dirname \"$0\")\n\
             {node_path}\
             exec \"$basedir/{rel_target_fwdslash}\" \"$@\"\n"
        );
    }
    let BinLaunch::Interpreter(prog) = launch else {
        unreachable!();
    };
    let launch_block = interpreter_launch_block(safe_prog(prog), rel_target_fwdslash);
    format!(
        "#!/bin/sh\n\
         {POSIX_SHIM_MARKER_PREFIX}{rel_target_fwdslash}\n\
         basedir=$(dirname \"$0\")\n\
         {node_path}\
         {launch_block}"
    )
}

/// Recover the `$basedir`-relative target embedded by
/// [`generate_posix_shim`]. Returns `None` for any content that lacks
/// the [`POSIX_SHIM_MARKER_PREFIX`] marker — including shims written by
/// other tools and older aube versions if the marker is ever bumped.
/// Lives in this module so the format contract stays in one file with
/// its writer.
pub fn parse_posix_shim_target(content: &str) -> Option<&str> {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix(POSIX_SHIM_MARKER_PREFIX) {
            return Some(rest);
        }
    }
    None
}

/// Maximum wrapper size accepted by [`resolve_bin_shim`]. Generated wrappers
/// are normally under 2 KiB; the larger cap accommodates long Windows paths
/// without reading arbitrary foreign files into memory.
const MAX_BIN_SHIM_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy)]
enum BinShimStyle {
    Posix,
    Cmd,
}

/// Decode an aube-generated wrapper without executing it.
///
/// Only regular files at most 64 KiB are inspected. POSIX wrappers must carry
/// aube's versioned marker; cmd wrappers must match the generated `@SETLOCAL`
/// and local-interpreter branch shape. Symlinks and unrecognized wrappers
/// return `Ok(None)`.
pub fn resolve_bin_shim(path: &Path) -> io::Result<Option<ResolvedBinShim>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_BIN_SHIM_BYTES {
        return Ok(None);
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::fs::File::open(path)?
        .take(MAX_BIN_SHIM_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BIN_SHIM_BYTES {
        return Ok(None);
    }
    let Ok(content) = std::str::from_utf8(&bytes) else {
        return Ok(None);
    };
    let Some(parent) = path.parent() else {
        return Ok(None);
    };

    let parsed = if let Some(target) = parse_posix_shim_target(content) {
        Some((
            BinShimStyle::Posix,
            target,
            content.lines().find_map(|line| {
                line.strip_prefix("export NODE_PATH=\"")
                    .and_then(|value| value.strip_suffix('"'))
            }),
        ))
    } else {
        parse_cmd_shim_target(content).map(|target| {
            (
                BinShimStyle::Cmd,
                target,
                content
                    .lines()
                    .find_map(|line| line.strip_prefix("@SET NODE_PATH="))
                    .map(|value| value.trim_end_matches('\r')),
            )
        })
    };
    let Some((style, target, raw_node_path)) = parsed else {
        return Ok(None);
    };
    let Some(target) = resolve_shim_relative_path(parent, target, style) else {
        return Ok(None);
    };

    let node_path = match raw_node_path {
        Some(value) => {
            let Some(node_path) = resolve_shim_node_path(parent, value, style) else {
                return Ok(None);
            };
            Some(node_path)
        }
        None => None,
    };

    Ok(Some(ResolvedBinShim { target, node_path }))
}

fn parse_cmd_shim_target(content: &str) -> Option<&str> {
    let mut lines = content.lines();
    if lines.next()?.trim_end_matches('\r') != "@SETLOCAL" {
        return None;
    }

    let mut line = lines.next()?.trim_end_matches('\r');
    if line.starts_with("@SET NODE_PATH=") {
        line = lines.next()?.trim_end_matches('\r');
    }

    let if_prefix = "@IF EXIST \"%~dp0\\";
    let program = line.strip_prefix(if_prefix)?.strip_suffix(".exe\" (")?;
    if !is_safe_prog(program) {
        return None;
    }

    let local_line = lines.next()?.trim_end_matches('\r');
    let target = local_line
        .strip_prefix("  \"%~dp0\\")?
        .strip_prefix(program)?
        .strip_prefix(".exe\" \"%~dp0\\")?
        .strip_suffix("\" %*")?;
    if lines.next()?.trim_end_matches('\r') != ") ELSE ("
        || lines.next()?.trim_end_matches('\r') != "  @SET PATHEXT=%PATHEXT:;.JS;=;%"
    {
        return None;
    }

    let fallback_target = lines
        .next()?
        .trim_end_matches('\r')
        .strip_prefix("  ")?
        .strip_prefix(program)?
        .strip_prefix(" \"%~dp0\\")?
        .strip_suffix("\" %*")?;
    if fallback_target != target || lines.next()?.trim_end_matches('\r') != ")" {
        return None;
    }
    lines.next().is_none().then_some(target)
}

fn resolve_shim_relative_path(
    parent: &Path,
    relative: &str,
    style: BinShimStyle,
) -> Option<PathBuf> {
    if relative.is_empty()
        || relative.contains('\0')
        || relative.starts_with('/')
        || relative.starts_with('\\')
        || relative.len() >= 2 && relative.as_bytes()[1] == b':'
    {
        return None;
    }
    let relative = match style {
        BinShimStyle::Posix => relative.to_string(),
        BinShimStyle::Cmd => relative.replace('\\', std::path::MAIN_SEPARATOR_STR),
    };
    Some(normalize_path(&parent.join(relative)))
}

fn resolve_shim_node_path(parent: &Path, value: &str, style: BinShimStyle) -> Option<OsString> {
    // Windows extensionless shims use a semicolon-delimited NODE_PATH even
    // though their shell syntax otherwise resembles the POSIX wrapper.
    if matches!(style, BinShimStyle::Posix) && value.contains(';') {
        return None;
    }
    let (separator, prefix) = match style {
        BinShimStyle::Posix => (':', "$basedir/"),
        BinShimStyle::Cmd => (';', "%~dp0"),
    };
    let paths = value
        .split(separator)
        .map(|entry| {
            let relative = entry.strip_prefix(prefix)?;
            resolve_shim_relative_path(parent, relative, style)
        })
        .collect::<Option<Vec<_>>>()?;
    std::env::join_paths(paths).ok()
}

/// Collapse `.` / `..` components without touching the filesystem.
/// Used on Windows to give `junction::create` an absolute target when
/// the caller computed a relative `../../foo` — `canonicalize` isn't
/// an option because it requires the target to already exist and
/// strips the UNC prefix the junction API is happy to accept.
/// Also exposed cross-platform so callers can resolve relative paths
/// stored in POSIX shims without tripping over macOS's `/var` →
/// `/private/var` symlink (canonicalize eagerly follows that symlink,
/// which throws off the `..` count in shim-embedded relative targets).
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                if !matches!(
                    out.last(),
                    None | Some(Component::RootDir) | Some(Component::Prefix(_))
                ) {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out.iter().map(|c| c.as_os_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_bin_name_accepts_bare_and_scope() {
        assert!(validate_bin_name("foo").is_ok());
        assert!(validate_bin_name("foo-bar.js").is_ok());
        assert!(validate_bin_name("@scope/foo").is_ok());
    }

    #[test]
    fn validate_bin_name_rejects_traversal_and_separators() {
        for bad in [
            "",
            "..",
            ".",
            "../../../etc/passwd",
            "a/b/c",
            "a\\b",
            "foo\0",
            "/etc/cron.d/evil",
            "\\\\server\\share\\x",
            "C:\\x",
            "@scope/../x",
            "@/foo",
            "scope/foo",
        ] {
            assert!(validate_bin_name(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn validate_bin_target_rejects_shell_metacharacters() {
        for bad in [
            "bin/$(calc).js",
            "bin/$env:USERPROFILE.js",
            "bin/`id`.js",
            "bin/%PATH%.js",
            "bin/foo&bar.js",
            "bin/foo|bar.js",
            "bin/foo;bar.js",
            "bin/foo>bar.js",
            "bin/foo<bar.js",
            "bin/foo\"bar.js",
            "bin/foo'bar.js",
            "bin/foo!bar.js",
        ] {
            assert!(
                validate_bin_target(bad).is_err(),
                "must reject shell metachar payload {bad:?}"
            );
        }
    }

    #[test]
    fn validate_bin_target_rejects_absolute_and_traversal() {
        assert!(validate_bin_target("bin/cli.js").is_ok());
        assert!(validate_bin_target("./cli.js").is_ok());
        for bad in [
            "",
            "/etc/passwd",
            "../../../etc/passwd",
            "bin/../../../etc/passwd",
            "C:/Windows/x",
            "bin\\cli.js",
            "cli\0.js",
        ] {
            assert!(validate_bin_target(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn create_bin_shim_rejects_traversing_name() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join(".bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let target = dir.path().join("cli.js");
        std::fs::write(&target, "#!/usr/bin/env node\n").unwrap();
        let err = create_bin_shim(
            &bin_dir,
            "../../../evil",
            &target,
            BinShimOptions::default(),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn detect_interpreter_shebang_env_node() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.js");
        std::fs::write(&script, "#!/usr/bin/env node\nconsole.log('hi');\n").unwrap();
        assert_eq!(
            detect_bin_launch(&script),
            BinLaunch::Interpreter("node".to_string())
        );
    }

    #[test]
    fn detect_interpreter_shebang_env_with_s_flag() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.js");
        std::fs::write(
            &script,
            "#!/usr/bin/env -S node --harmony\nconsole.log('hi');\n",
        )
        .unwrap();
        assert_eq!(
            detect_bin_launch(&script),
            BinLaunch::Interpreter("node".to_string())
        );
    }

    #[test]
    fn detect_interpreter_shebang_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.js");
        std::fs::write(&script, "#!/usr/bin/node\nconsole.log('hi');\n").unwrap();
        assert_eq!(
            detect_bin_launch(&script),
            BinLaunch::Interpreter("node".to_string())
        );
    }

    #[test]
    fn detect_interpreter_shebang_env_python() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.py");
        std::fs::write(&script, "#!/usr/bin/env python3\nprint('hi')\n").unwrap();
        assert_eq!(
            detect_bin_launch(&script),
            BinLaunch::Interpreter("python3".to_string())
        );
    }

    #[test]
    fn detect_interpreter_shebang_with_env_vars() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.js");
        std::fs::write(
            &script,
            "#!/usr/bin/env NODE_OPTIONS=--max-old-space-size=4096 node\nconsole.log('hi');\n",
        )
        .unwrap();
        assert_eq!(
            detect_bin_launch(&script),
            BinLaunch::Interpreter("node".to_string())
        );
    }

    #[test]
    fn detect_interpreter_no_shebang_js() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.js");
        std::fs::write(&script, "console.log('hi');\n").unwrap();
        assert_eq!(
            detect_bin_launch(&script),
            BinLaunch::Interpreter("node".to_string())
        );
    }

    #[test]
    fn detect_interpreter_nonexistent_file_defaults_to_node() {
        assert_eq!(
            detect_bin_launch(Path::new("/nonexistent/file.js")),
            BinLaunch::Interpreter("node".to_string())
        );
    }

    #[test]
    fn detect_launch_uses_direct_mode_for_no_shebang_native_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("native.exe");
        std::fs::write(&target, b"\x7fELF").unwrap();
        assert_eq!(detect_bin_launch(&target), BinLaunch::Direct);
    }

    #[test]
    fn detect_launch_uses_direct_mode_for_extensionless_native_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("native");
        std::fs::write(&target, b"\xcf\xfa\xed\xfe").unwrap();
        assert_eq!(detect_bin_launch(&target), BinLaunch::Direct);
    }

    #[test]
    fn detect_launch_keeps_extensionless_javascript_on_node() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("cli");
        std::fs::write(&target, b"console.log('hi')\n").unwrap();
        assert_eq!(
            detect_bin_launch(&target),
            BinLaunch::Interpreter("node".to_string())
        );
    }

    #[test]
    fn detect_launch_keeps_unknown_text_extension_on_node() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("cli.custom");
        std::fs::write(&target, b"console.log('hi')\n").unwrap();
        assert_eq!(
            detect_bin_launch(&target),
            BinLaunch::Interpreter("node".to_string())
        );
    }

    #[test]
    fn relative_bin_target_computes_path() {
        let bin_dir = Path::new("/project/node_modules/.bin");
        let target =
            Path::new("/project/node_modules/.aube/is-odd@3.0.1/node_modules/is-odd/cli.js");
        let rel = relative_bin_target(bin_dir, target);
        assert_eq!(rel, "../.aube/is-odd@3.0.1/node_modules/is-odd/cli.js");
    }

    #[cfg(windows)]
    #[test]
    fn relative_bin_target_strips_verbatim_prefix_from_target() {
        // `std::fs::canonicalize` on Windows returns `\\?\C:\…`. If a
        // canonicalized `target` flows in next to a plain-drive
        // `base_dir`, `pathdiff` sees `Disk` vs `VerbatimDisk` prefix
        // components and falls back to the absolute target — which
        // then gets spliced into the `.cmd` shim as
        // `%~dp0\\?\<target>` and surfaces as Node's
        // `Cannot find module '<bin>\?\<target>'`.
        let base = Path::new(r"C:\pkg\bin");
        let target = Path::new(r"\\?\C:\pkg\global-aube\abc\node_modules\p\bin\p.cjs");
        let rel = relative_bin_target(base, target);
        assert_eq!(rel, "../global-aube/abc/node_modules/p/bin/p.cjs");
    }

    #[cfg(windows)]
    #[test]
    fn relative_bin_target_strips_verbatim_prefix_from_base() {
        let base = Path::new(r"\\?\C:\pkg\bin");
        let target = Path::new(r"C:\pkg\global-aube\abc\node_modules\p\bin\p.cjs");
        let rel = relative_bin_target(base, target);
        assert_eq!(rel, "../global-aube/abc/node_modules/p/bin/p.cjs");
    }

    #[cfg(windows)]
    #[test]
    fn relative_bin_target_preserves_unc_share_prefix() {
        // `\\?\UNC\…` identifies a real network share and has no
        // non-verbatim equivalent — strip_verbatim must leave it
        // alone so the shim points at the share, not at a bogus
        // drive-rooted path.
        let base = Path::new(r"\\?\UNC\server\share\pkg\bin");
        let target = Path::new(r"\\?\UNC\server\share\pkg\lib\cli.js");
        let rel = relative_bin_target(base, target);
        assert_eq!(rel, "../lib/cli.js");
    }

    #[cfg(windows)]
    #[test]
    fn normalize_collapses_parent_and_cur_dir() {
        let p = Path::new(r"C:\a\b\.\..\c\d\..\e");
        assert_eq!(normalize_path(p), PathBuf::from(r"C:\a\c\e"));
    }

    #[cfg(windows)]
    #[test]
    fn creates_junction_without_developer_mode() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("marker.txt"), b"hi").unwrap();

        let link = dir.path().join("parent").join("link");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        // Relative target, mimicking how the linker builds them.
        let rel = Path::new("..").join("target");
        create_dir_link(&rel, &link).unwrap();

        assert_eq!(std::fs::read(link.join("marker.txt")).unwrap(), b"hi");
    }

    #[cfg(windows)]
    #[test]
    fn create_bin_shim_writes_three_files() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let pkg_dir = dir
            .path()
            .join("node_modules/.aube/is-odd@3.0.1/node_modules/is-odd");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let script = pkg_dir.join("cli.js");
        std::fs::write(&script, "#!/usr/bin/env node\nconsole.log('hi');\n").unwrap();

        create_bin_shim(&bin_dir, "is-odd", &script, BinShimOptions::default()).unwrap();

        // All three files must exist
        assert!(bin_dir.join("is-odd.cmd").exists());
        assert!(bin_dir.join("is-odd.ps1").exists());
        assert!(bin_dir.join("is-odd").exists());

        // .cmd should reference node and the relative target
        let cmd = std::fs::read_to_string(bin_dir.join("is-odd.cmd")).unwrap();
        assert!(cmd.contains("node.exe"));
        assert!(cmd.contains(".aube"));

        // .ps1 should reference node
        let ps1 = std::fs::read_to_string(bin_dir.join("is-odd.ps1")).unwrap();
        assert!(ps1.contains("node$exe"));

        // extensionless should be a shell script
        let sh = std::fs::read_to_string(bin_dir.join("is-odd")).unwrap();
        assert!(sh.starts_with("#!/bin/sh"));
    }

    #[cfg(windows)]
    #[test]
    fn create_bin_shim_cleans_old_files() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let script = pkg_dir.join("cli.js");
        std::fs::write(&script, "#!/usr/bin/env node\nconsole.log('v1');\n").unwrap();

        // First shim
        create_bin_shim(&bin_dir, "mycli", &script, BinShimOptions::default()).unwrap();
        let cmd1 = std::fs::read_to_string(bin_dir.join("mycli.cmd")).unwrap();

        // Update script and re-shim
        std::fs::write(&script, "#!/usr/bin/env node\nconsole.log('v2');\n").unwrap();
        create_bin_shim(&bin_dir, "mycli", &script, BinShimOptions::default()).unwrap();
        let cmd2 = std::fs::read_to_string(bin_dir.join("mycli.cmd")).unwrap();

        // Content should be the same (same target path), but no error from overwrite
        assert_eq!(cmd1, cmd2);
    }

    #[cfg(windows)]
    #[test]
    fn remove_bin_shim_removes_all_files() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let script = pkg_dir.join("cli.js");
        std::fs::write(&script, "console.log('hi');\n").unwrap();

        create_bin_shim(&bin_dir, "mycli", &script, BinShimOptions::default()).unwrap();
        assert!(bin_dir.join("mycli.cmd").exists());
        assert!(bin_dir.join("mycli.ps1").exists());
        assert!(bin_dir.join("mycli").exists());

        remove_bin_shim(&bin_dir, "mycli");
        assert!(!bin_dir.join("mycli.cmd").exists());
        assert!(!bin_dir.join("mycli.ps1").exists());
        assert!(!bin_dir.join("mycli").exists());
    }

    #[cfg(unix)]
    #[test]
    fn create_bin_shim_creates_symlink_on_unix() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let script = pkg_dir.join("cli.js");
        std::fs::write(&script, "#!/usr/bin/env node\nconsole.log('hi');\n").unwrap();

        create_bin_shim(&bin_dir, "mycli", &script, BinShimOptions::default()).unwrap();

        let link = bin_dir.join("mycli");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());

        // Target should be executable
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&script).unwrap().permissions().mode();
        assert_eq!(mode & 0o755, 0o755);
    }

    #[test]
    #[cfg(unix)]
    fn create_bin_shim_creates_parent_for_scoped_bin_name() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let pkg_dir = dir.path().join(
            "node_modules/.aube/config-inspector@1.4.2/node_modules/@eslint/config-inspector",
        );
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let script = pkg_dir.join("bin.mjs");
        std::fs::write(&script, "#!/usr/bin/env node\nconsole.log('hi');\n").unwrap();

        create_bin_shim(
            &bin_dir,
            "@eslint/config-inspector",
            &script,
            BinShimOptions {
                extend_node_path: true,
                prefer_symlinked_executables: Some(false),
                hidden_modules_dir: None,
            },
        )
        .unwrap();

        let shim_path = bin_dir.join("@eslint/config-inspector");
        assert!(shim_path.exists());
        let content = std::fs::read_to_string(shim_path).unwrap();
        let rel = parse_posix_shim_target(&content).expect("shim should carry its marker");
        assert_eq!(
            rel,
            "../../.aube/config-inspector@1.4.2/node_modules/@eslint/config-inspector/bin.mjs",
        );
        assert!(content.contains("export NODE_PATH=\"$basedir/../..\""));
    }

    #[test]
    fn remove_bin_shim_removes_empty_scoped_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let script = pkg_dir.join("cli.js");
        std::fs::write(&script, "#!/usr/bin/env node\nconsole.log('hi');\n").unwrap();

        create_bin_shim(
            &bin_dir,
            "@scope/mycli",
            &script,
            BinShimOptions {
                extend_node_path: false,
                prefer_symlinked_executables: Some(false),
                hidden_modules_dir: None,
            },
        )
        .unwrap();
        assert!(bin_dir.join("@scope").exists());

        remove_bin_shim(&bin_dir, "@scope/mycli");
        assert!(!bin_dir.join("@scope/mycli").exists());
        assert!(!bin_dir.join("@scope").exists());
    }

    /// Runs on every platform, unlike the two generators that consume it —
    /// `generate_sh_shim` is Windows-only, so before the block was shared a
    /// unix test run compiled none of it. That gap is why an earlier fix for
    /// #656 landed on the Windows copy, passed its test, and left the copy
    /// that actually runs untouched.
    #[test]
    fn the_shared_launch_block_refuses_to_delegate_to_a_shell_wrapper() {
        let block = interpreter_launch_block("node", "../pkg/cli.js");
        assert!(
            block.contains("head -c 2") && block.contains("!= '#!'"),
            "the delegate must be proven a native interpreter, not a wrapper:\n{block}",
        );
        // The probe must fail CLOSED in BOTH directions, so assert the
        // exact form rather than that the words appear somewhere.
        //
        // `|| echo '#!'` catches the failures that report themselves —
        // target executable but unreadable, `head` unavailable.
        //
        // `command -p` catches the one that does not: `.bin` is on PATH for
        // every lifecycle script, so a dependency-declared `head` that exits
        // 0 printing anything but `#!` answers "native interpreter" with no
        // error for `||` to see, and the wrapper delegates.
        assert!(
            block.contains("$(command -p head -c 2 \"$basedir/node\" 2>/dev/null || echo '#!')"),
            "the probe must resolve `head` outside the inherited PATH and fail \
             closed; dropping either half re-opens a delegate path:\n{block}",
        );
        assert!(
            block.contains("exec node \"$basedir/../pkg/cli.js\" \"$@\""),
            "the PATH fallback must still launch the target with its own args:\n{block}",
        );
    }

    /// Which names collide, and which only look like they do.
    ///
    /// An UNSCOPED bin whose name is the interpreter it needs cannot be
    /// wrapped: the wrapper lands on PATH as `<prog>`, so both the
    /// `$basedir/<prog>` preference and the bare `exec <prog>` fallback find
    /// it again. Decline it, as pnpm does, rather than write something that
    /// cannot terminate (#656). Every other shape — a different name, or the
    /// same name under a scope — is written, and the scoped one is executed
    /// here to prove it actually works.
    #[cfg(unix)]
    #[test]
    fn only_an_unscoped_bin_named_after_its_interpreter_is_declined() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&pkg_dir).unwrap();

        let opts = BinShimOptions {
            extend_node_path: false,
            prefer_symlinked_executables: Some(false),
            hidden_modules_dir: None,
        };

        // The reported case: `node@26.5.1` declares `bin: {node: "bin/node"}`
        // and its own `preinstall` downloads that target, so at link time the
        // target is absent and classifies as a `node` script. Wrapping it put
        // the wrapper on PATH as `node`, and it exec'd itself forever.
        create_bin_shim(&bin_dir, "node", &pkg_dir.join("bin/node"), opts).unwrap();
        assert!(
            !bin_dir.join("node").exists(),
            "the `node` package's `node` bin must be declined, not wrapped",
        );

        // Same rule for any other interpreter, and for a target that IS on
        // disk — the collision is about the name, not about the target.
        let script = pkg_dir.join("cli.sh");
        std::fs::write(&script, "#!/bin/sh\necho \"argc=$# args=$*\"\n").unwrap();
        create_bin_shim(&bin_dir, "sh", &script, opts).unwrap();
        assert!(!bin_dir.join("sh").exists());

        // A DIFFERENT name over the same target is unaffected — only the
        // collision is declined, never the whole package.
        create_bin_shim(&bin_dir, "mysh", &script, opts).unwrap();
        assert!(bin_dir.join("mysh").exists());

        // A SCOPED bin is not a collision. It lands at `.bin/@scope/sh`,
        // and PATH carries `.bin`, not `.bin/@scope`, so the `exec <prog>`
        // fallback reaches the real interpreter; only the `$basedir/<prog>`
        // half self-refers, and the `#!` test already rejects that.
        // Declining it would delete a bin that works — so prove it RUNS,
        // rather than only that it was written. Writing a broken bin is a
        // worse outcome than declining one.
        create_bin_shim(&bin_dir, "@scope/sh", &script, opts).unwrap();
        let scoped = bin_dir.join("@scope/sh");
        assert!(scoped.exists(), "a scoped bin is written, not declined");
        // Check the guard BEFORE running it. `$basedir/sh` here IS this
        // wrapper, so without the `#!` test the exec below would loop and
        // hang the suite instead of failing it. Assert cheaply first, so a
        // regression reports as a failed assertion, not a stalled CI job.
        let body = std::fs::read_to_string(&scoped).unwrap();
        assert!(
            body.contains("!= '#!'"),
            "the self-reference guard must be present before this is safe to run:\n{body}",
        );
        let out = std::process::Command::new(&scoped)
            .args(["one", "two"])
            .output()
            .expect("the scoped wrapper should be executable");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "argc=2 args=one two",
            "the scoped wrapper must reach the real `sh` with argv intact; \
             stderr={:?}",
            String::from_utf8_lossy(&out.stderr),
        );
    }

    /// POSITIVE CONTROL for the `#!` probe: a REAL interpreter next to the
    /// wrapper must still be preferred.
    ///
    /// Every other test here asserts the FALLBACK path, so all of them would
    /// keep passing if the probe silently never succeeded — `command -p`
    /// unavailable, `head` missing from the standard path, a botched quote.
    /// The clause would be dead and the suite would say nothing. This is the
    /// one test that fails in that direction.
    ///
    /// `/bin/echo` stands in for the interpreter because it is a real native
    /// executable (so it does not open with `#!`) and it makes the two
    /// branches distinguishable: delegating ECHOES the target path, while
    /// falling back EXECUTES the target through `sh`.
    #[cfg(unix)]
    #[test]
    fn a_real_interpreter_beside_the_wrapper_is_still_preferred() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let script = pkg_dir.join("cli.sh");
        std::fs::write(&script, "#!/bin/sh\necho EXECUTED-VIA-FALLBACK\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        create_bin_shim(
            &bin_dir,
            "tool",
            &script,
            BinShimOptions {
                extend_node_path: false,
                prefer_symlinked_executables: Some(false),
                hidden_modules_dir: None,
            },
        )
        .unwrap();
        // A genuine native binary under the interpreter's name.
        std::os::unix::fs::symlink("/bin/echo", bin_dir.join("sh")).unwrap();

        let out = std::process::Command::new(bin_dir.join("tool"))
            .arg("one")
            .output()
            .expect("the wrapper should be executable");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("cli.sh") && stdout.contains("one"),
            "a real interpreter beside the wrapper must be PREFERRED — `/bin/echo` \
             should have echoed the target and args. Falling back here means the \
             probe never succeeds and the preference clause is dead: stdout={stdout:?} \
             stderr={:?}",
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(
            !stdout.contains("EXECUTED-VIA-FALLBACK"),
            "the wrapper took the PATH fallback instead of the local interpreter:\n{stdout}",
        );
    }

    /// The classification of an absent target is deliberately UNCHANGED: an
    /// importer's own `bin` is routinely a build output that does not exist
    /// at install time, and `bin_linking.rs` relies on the wrapper invoking
    /// `node <target>` so that neither a missing file nor a stripped exec bit
    /// breaks `aube run`. #656 is fixed by declining the name collision, not
    /// by second-guessing this.
    #[test]
    fn an_absent_target_still_classifies_as_a_node_script() {
        let dir = tempfile::tempdir().unwrap();
        for missing in ["pkg/dist/cli.js", "pkg/bin/node"] {
            assert_eq!(
                detect_bin_launch(&dir.path().join(missing)),
                BinLaunch::Interpreter("node".to_string()),
                "{missing} is absent, so it keeps the node-script fallback",
            );
        }
    }

    /// The Windows arm declines the collision too, and declines ALL THREE
    /// wrappers rather than only the extension-less Git-Bash one: `node.cmd`
    /// re-resolves through `PATHEXT` from `.bin` and loops exactly as the
    /// `sh` wrapper does. Windows is where this class of defect hides —
    /// nub#566 / nub#576 both reached users because the test that would have
    /// caught them was itself `#[cfg(unix)]`.
    #[cfg(windows)]
    #[test]
    fn a_bin_named_after_its_interpreter_is_declined_on_windows_too() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&pkg_dir).unwrap();

        let opts = BinShimOptions {
            extend_node_path: false,
            prefer_symlinked_executables: None,
            hidden_modules_dir: None,
        };
        create_bin_shim(&bin_dir, "node", &pkg_dir.join("bin/node"), opts).unwrap();
        for wrapper in win_shim_paths(&bin_dir, "node") {
            assert!(
                !wrapper.exists(),
                "{} must not be written: it would resolve back to itself",
                wrapper.display(),
            );
        }

        // A non-colliding bin over an equally absent target is unaffected.
        create_bin_shim(&bin_dir, "tool", &pkg_dir.join("cli.js"), opts).unwrap();
        assert!(bin_dir.join("tool.cmd").exists());
    }

    /// The blast radius that string assertions miss: with a `node` dependency
    /// present, EVERY other JS bin's wrapper routed through `.bin/node` and
    /// inherited its fate. This RUNS the wrapper and checks the arguments the
    /// target actually received — a wrapper that delegates to a sibling
    /// wrapper silently prepends its own target, so a passing exit status
    /// alone would not catch it (#656).
    #[cfg(unix)]
    #[test]
    fn a_wrapper_does_not_delegate_through_a_sibling_wrapper() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&pkg_dir).unwrap();

        // The target reports exactly what it was handed.
        let script = pkg_dir.join("cli.sh");
        std::fs::write(&script, "#!/bin/sh\necho \"argc=$# args=$*\"\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let opts = BinShimOptions {
            extend_node_path: false,
            prefer_symlinked_executables: Some(false),
            hidden_modules_dir: None,
        };
        // A sibling that happens to be called `sh` — a wrapper of ours, not
        // an interpreter. Its own name does not collide with ITS interpreter
        // (`node`), so it is written normally.
        create_bin_shim(&bin_dir, "sh", &pkg_dir.join("cli.js"), opts).unwrap();
        assert!(
            bin_dir.join("sh").exists(),
            "the sibling wrapper is written"
        );
        // `.bin/tool` needs `sh`, and a `.bin/sh` now sits next to it.
        create_bin_shim(&bin_dir, "tool", &script, opts).unwrap();

        let out = std::process::Command::new(bin_dir.join("tool"))
            .args(["one", "two"])
            .output()
            .expect("the wrapper should be executable");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "the wrapper must reach the real `sh`, not the sibling wrapper: \
             status={:?} stdout={stdout:?} stderr={:?}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
        );
        assert_eq!(
            stdout.trim(),
            "argc=2 args=one two",
            "the target must receive the caller's arguments verbatim; a \
             delegated-through sibling prepends its own target path",
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_bin_shim_writes_posix_shim_when_symlink_opt_out() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let script = pkg_dir.join("cli.js");
        std::fs::write(&script, "#!/usr/bin/env node\nconsole.log('hi');\n").unwrap();

        create_bin_shim(
            &bin_dir,
            "mycli",
            &script,
            BinShimOptions {
                extend_node_path: false,
                prefer_symlinked_executables: Some(false),
                hidden_modules_dir: None,
            },
        )
        .unwrap();

        let path = bin_dir.join("mycli");
        // Must be a regular file, not a symlink.
        let meta = path.symlink_metadata().unwrap();
        assert!(!meta.file_type().is_symlink());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("#!/bin/sh"));
        assert!(content.contains("exec \"$basedir/node\""));
        // Marker comment has to land in the shim so `parse_posix_shim_target`
        // can round-trip the target on uninstall.
        assert!(content.contains(POSIX_SHIM_MARKER_PREFIX));
        // NODE_PATH should NOT be exported when extend_node_path=false.
        assert!(!content.contains("NODE_PATH"));
        // Must be marked executable.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
    }

    #[test]
    fn is_native_executable_classifies_magic_bytes() {
        // Native formats → true.
        assert!(is_native_executable(b"\x7FELF\x02\x01\x01"));
        assert!(is_native_executable(b"\xCF\xFA\xED\xFE...")); // Mach-O 64 LE
        assert!(is_native_executable(b"\xFE\xED\xFA\xCF...")); // Mach-O 64 BE
        assert!(is_native_executable(b"\xCA\xFE\xBA\xBE...")); // fat/universal
        assert!(is_native_executable(b"MZ\x90\x00")); // PE
        // JS launchers / shell scripts → false. `node <target>` is correct
        // for these, so they must NOT be classified as native.
        assert!(!is_native_executable(b"#!/usr/bin/env node\n"));
        assert!(!is_native_executable(b"#!/bin/sh\n"));
        assert!(!is_native_executable(b"module.exports = 1;\n"));
        assert!(!is_native_executable(b""));
        assert!(!is_native_executable(b"\x7FEL")); // too short for ELF
    }

    #[cfg(unix)]
    #[test]
    fn create_bin_shim_direct_execs_native_target_on_shim_optout() {
        // #394: a native-executable target must never get a `node` shim (the
        // kernel has to exec it directly). nub used to force the symlink
        // layout here, overriding `preferSymlinkedExecutables=false`.
        // `BinLaunch::Direct` reaches the same end while honoring the opt-out
        // and keeping NODE_PATH, which a symlink cannot carry.
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let target = pkg_dir.join("esbuild");
        // Minimal ELF header — enough for the magic-byte classifier.
        std::fs::write(&target, b"\x7FELF\x02\x01\x01\x00rest-of-binary").unwrap();

        create_bin_shim(
            &bin_dir,
            "esbuild",
            &target,
            BinShimOptions {
                extend_node_path: true,
                prefer_symlinked_executables: Some(false),
                hidden_modules_dir: None,
            },
        )
        .unwrap();

        let path = bin_dir.join("esbuild");
        assert!(!path.symlink_metadata().unwrap().file_type().is_symlink());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("exec \"$basedir/../../pkg/esbuild\""),
            "native target must be exec'd directly:\n{content}"
        );
        assert!(
            !content.contains("$basedir/node\""),
            "native target must not be wrapped in a node shim:\n{content}"
        );
        assert!(
            content.contains("export NODE_PATH="),
            "the direct-exec shim carries NODE_PATH the symlink could not:\n{content}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_bin_shim_symlink_target_is_relative_so_the_tree_relocates() {
        // #568: an absolute `.bin` target pins the tree to the machine
        // that installed it — a bind-mounted or `COPY`d `node_modules`
        // arrives with every bin dangling. npm and pnpm both write
        // relative, and the shim branch already did.
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        let pkg_bin = dir.path().join("node_modules/esbuild/bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&pkg_bin).unwrap();
        let target = pkg_bin.join("esbuild");
        std::fs::write(&target, b"\x7FELF\x02\x01\x01\x00rest-of-binary").unwrap();

        create_bin_shim(&bin_dir, "esbuild", &target, BinShimOptions::default()).unwrap();

        let path = bin_dir.join("esbuild");
        assert_eq!(
            std::fs::read_link(&path).unwrap(),
            Path::new("../esbuild/bin/esbuild")
        );
        assert!(path.exists(), "relative target must resolve");
    }

    #[cfg(unix)]
    #[test]
    fn create_bin_shim_symlink_anchors_on_the_physical_dir_inside_a_shared_store() {
        // A per-dep `.bin/` in a store-shared package is reached through a
        // `.store/<name>@<ver>` symlink, but the kernel resolves the link's
        // `..` inside the store, where entries carry a `-<hash>` suffix.
        // Anchoring on the surface path would dangle; anchoring on the
        // physical one also keeps the link project-independent, so a second
        // project installing the same package can't repoint this one's bin.
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        let host_bin = store.join("host@1.0.0-aaaaaaaa/node_modules/.bin");
        let dep_bin = store.join("dep@1.0.0-bbbbbbbb/node_modules/dep/bin");
        std::fs::create_dir_all(&host_bin).unwrap();
        std::fs::create_dir_all(&dep_bin).unwrap();
        let real_target = dep_bin.join("dep");
        std::fs::write(&real_target, b"\x7FELF\x02\x01\x01\x00rest-of-binary").unwrap();

        // The project's surface view: unsuffixed names symlinked into the store.
        let surface_store = dir.path().join("proj/node_modules/.store");
        std::fs::create_dir_all(&surface_store).unwrap();
        for (surface, real) in [
            ("host@1.0.0", "host@1.0.0-aaaaaaaa"),
            ("dep@1.0.0", "dep@1.0.0-bbbbbbbb"),
        ] {
            std::os::unix::fs::symlink(store.join(real), surface_store.join(surface)).unwrap();
        }

        let bin_dir = surface_store.join("host@1.0.0/node_modules/.bin");
        let target = surface_store.join("dep@1.0.0/node_modules/dep/bin/dep");
        create_bin_shim(&bin_dir, "dep", &target, BinShimOptions::default()).unwrap();

        let path = bin_dir.join("dep");
        let link = std::fs::read_link(&path).unwrap();
        assert_eq!(
            link,
            Path::new("../../../dep@1.0.0-bbbbbbbb/node_modules/dep/bin/dep")
        );
        assert!(path.exists(), "store-anchored target must resolve");
        // Written through the surface path, but readable from the store
        // itself — which is what makes it safe to share across projects.
        assert!(host_bin.join("dep").exists());
    }

    #[cfg(unix)]
    #[test]
    fn create_bin_shim_symlink_falls_back_to_absolute_for_a_missing_target() {
        // Nothing to canonicalize, so no relative path can be derived
        // safely. Keep the pre-#568 absolute form rather than guessing.
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let target = dir.path().join("node_modules/gone/bin/gone");

        create_bin_shim(&bin_dir, "gone", &target, BinShimOptions::default()).unwrap();

        assert_eq!(std::fs::read_link(bin_dir.join("gone")).unwrap(), target);
    }

    #[cfg(unix)]
    #[test]
    fn posix_shim_executes_non_script_target_replaced_after_linking() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let target = pkg_dir.join("native.exe");
        std::fs::write(&target, "postinstall has not run yet\n").unwrap();

        create_bin_shim(
            &bin_dir,
            "native",
            &target,
            BinShimOptions {
                extend_node_path: true,
                prefer_symlinked_executables: Some(false),
                hidden_modules_dir: None,
            },
        )
        .unwrap();

        let shim = bin_dir.join("native");
        let content = std::fs::read_to_string(&shim).unwrap();
        assert!(content.contains("exec \"$basedir/../../pkg/native.exe\" \"$@\""));
        assert!(!content.contains("exec node"));

        std::fs::write(&target, "#!/bin/sh\nprintf 'native-%s\\n' \"$1\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();

        let output = std::process::Command::new(&shim)
            .arg("ok")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"native-ok\n");
    }

    #[cfg(unix)]
    #[test]
    fn parse_posix_shim_target_round_trips_generator_output() {
        // The parser and generator live together so this loop-back
        // guards the format contract end-to-end: anything that
        // changes the marker on one side breaks this test.
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let pkg_dir = dir
            .path()
            .join("node_modules/.aube/semver@1.0.0/node_modules/semver");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let script = pkg_dir.join("bin/semver.js");
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::write(&script, "#!/usr/bin/env node\n").unwrap();

        create_bin_shim(
            &bin_dir,
            "semver",
            &script,
            BinShimOptions {
                extend_node_path: true,
                prefer_symlinked_executables: Some(false),
                hidden_modules_dir: None,
            },
        )
        .unwrap();

        let content = std::fs::read_to_string(bin_dir.join("semver")).unwrap();
        let rel = parse_posix_shim_target(&content).expect("shim should carry its marker");
        assert_eq!(
            rel,
            "../.aube/semver@1.0.0/node_modules/semver/bin/semver.js",
        );
    }

    #[test]
    fn parse_posix_shim_target_rejects_foreign_scripts() {
        // Arbitrary shell content without our marker must not match —
        // otherwise `unlink_bins` would start removing bins owned by
        // other tooling.
        assert!(parse_posix_shim_target("#!/bin/sh\necho hi\n").is_none());
        // A stray `exec` line with `$basedir/...` isn't enough: the
        // dedicated marker is the only anchor.
        assert!(
            parse_posix_shim_target("#!/bin/sh\nexec node \"$basedir/../pkg/cli.js\" \"$@\"\n",)
                .is_none()
        );
    }

    #[test]
    fn resolve_bin_shim_rejects_oversized_and_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let oversized = dir.path().join("oversized");
        std::fs::write(&oversized, vec![b'x'; MAX_BIN_SHIM_BYTES as usize + 1]).unwrap();
        assert_eq!(resolve_bin_shim(&oversized).unwrap(), None);

        let foreign = dir.path().join("foreign.cmd");
        std::fs::write(
            &foreign,
            "@SETLOCAL\r\n\
             @IF EXIST \"%~dp0\\node.exe\" (\r\n\
             \x20 \"%~dp0\\node.exe\" \"%~dp0\\payload.exe\" %*\r\n\
             ) ELSE (\r\n\
             \x20 @SET PATHEXT=%PATHEXT:;.JS;=;%\r\n\
             \x20 node \"%~dp0\\payload.exe\" %*\r\n\
             )\r\n\
             @ECHO foreign behavior\r\n",
        )
        .unwrap();
        assert_eq!(resolve_bin_shim(&foreign).unwrap(), None);

        let malformed_env = dir.path().join("malformed-env");
        std::fs::write(
            &malformed_env,
            "#!/bin/sh\n\
             # aube-bin-shim v1 target=pkg/tool\n\
             export NODE_PATH=\"not-basedir-relative\"\n",
        )
        .unwrap();
        assert_eq!(resolve_bin_shim(&malformed_env).unwrap(), None);

        let windows_env = dir.path().join("windows-env");
        std::fs::write(
            &windows_env,
            "#!/bin/sh\n\
             # aube-bin-shim v1 target=pkg/tool\n\
             export NODE_PATH=\"$basedir/..;$basedir/../.aube/node_modules\"\n",
        )
        .unwrap();
        assert_eq!(resolve_bin_shim(&windows_env).unwrap(), None);
    }

    #[test]
    fn resolve_bin_shim_decodes_cmd_target_and_multi_entry_node_path() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let shim = bin_dir.join("tool.cmd");
        std::fs::write(
            &shim,
            "@SETLOCAL\r\n\
             @SET NODE_PATH=%~dp0..;%~dp0..\\.aube\\node_modules\r\n\
             @IF EXIST \"%~dp0\\node.exe\" (\r\n\
             \x20 \"%~dp0\\node.exe\" \"%~dp0\\..\\pkg\\tool.exe\" %*\r\n\
             ) ELSE (\r\n\
             \x20 @SET PATHEXT=%PATHEXT:;.JS;=;%\r\n\
             \x20 node \"%~dp0\\..\\pkg\\tool.exe\" %*\r\n\
             )\r\n",
        )
        .unwrap();

        let resolved = resolve_bin_shim(&shim).unwrap().unwrap();
        assert_eq!(
            resolved.target,
            dir.path().join("node_modules/pkg/tool.exe")
        );
        assert_eq!(
            resolved.node_path,
            Some(
                std::env::join_paths([
                    dir.path().join("node_modules"),
                    dir.path()
                        .join("node_modules")
                        .join(".aube")
                        .join("node_modules"),
                ])
                .unwrap()
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_bin_shim_injects_node_path_in_posix_shim() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let script = pkg_dir.join("cli.js");
        std::fs::write(&script, "#!/usr/bin/env node\nconsole.log('hi');\n").unwrap();

        create_bin_shim(
            &bin_dir,
            "mycli",
            &script,
            BinShimOptions {
                extend_node_path: true,
                prefer_symlinked_executables: Some(false),
                hidden_modules_dir: None,
            },
        )
        .unwrap();

        let content = std::fs::read_to_string(bin_dir.join("mycli")).unwrap();
        assert!(content.contains("export NODE_PATH=\"$basedir/..\""));
    }

    #[cfg(unix)]
    #[test]
    fn create_bin_shim_appends_hidden_modules_to_node_path() {
        // The regression this guards: without the hidden-modules entry,
        // tools like `astro check` invoked from a shimmed bin can't see
        // auto-installed peers (e.g. `typescript`) that aube hoists to
        // `<project>/node_modules/.aube/node_modules/`. The single
        // `$basedir/..` entry only covers the top-level `node_modules/`,
        // which holds direct deps but never transitives.
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let hidden = dir.path().join("node_modules/.aube/node_modules");
        std::fs::create_dir_all(&hidden).unwrap();
        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let script = pkg_dir.join("cli.js");
        std::fs::write(&script, "#!/usr/bin/env node\n").unwrap();

        create_bin_shim(
            &bin_dir,
            "mycli",
            &script,
            BinShimOptions {
                extend_node_path: true,
                prefer_symlinked_executables: Some(false),
                hidden_modules_dir: Some(hidden.as_path()),
            },
        )
        .unwrap();

        let content = std::fs::read_to_string(bin_dir.join("mycli")).unwrap();
        assert!(
            content.contains("export NODE_PATH=\"$basedir/..:$basedir/../.aube/node_modules\""),
            "expected two-entry NODE_PATH, got:\n{content}"
        );
        let resolved = resolve_bin_shim(&bin_dir.join("mycli")).unwrap().unwrap();
        assert_eq!(resolved.target, script);
        assert_eq!(
            resolved.node_path,
            Some(std::env::join_paths([dir.path().join("node_modules"), hidden]).unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_bin_shim_ignores_node_path_for_symlink() {
        // extend_node_path is meaningless when the output is a bare
        // symlink — no file to inject an env export into. The symlink
        // still gets created, and the test only confirms that the
        // Some(true) / None paths behave identically.
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let script = pkg_dir.join("cli.js");
        std::fs::write(&script, "#!/usr/bin/env node\nconsole.log('hi');\n").unwrap();

        create_bin_shim(
            &bin_dir,
            "mycli",
            &script,
            BinShimOptions {
                extend_node_path: true,
                prefer_symlinked_executables: None,
                hidden_modules_dir: None,
            },
        )
        .unwrap();

        let link = bin_dir.join("mycli");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    }

    #[cfg(windows)]
    #[test]
    fn create_bin_shim_injects_node_path_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let script = pkg_dir.join("cli.js");
        std::fs::write(&script, "#!/usr/bin/env node\nconsole.log('hi');\n").unwrap();

        create_bin_shim(
            &bin_dir,
            "mycli",
            &script,
            BinShimOptions {
                extend_node_path: true,
                prefer_symlinked_executables: None,
                hidden_modules_dir: None,
            },
        )
        .unwrap();

        let cmd = std::fs::read_to_string(bin_dir.join("mycli.cmd")).unwrap();
        assert!(cmd.contains("@SET NODE_PATH=%~dp0.."));
        let ps1 = std::fs::read_to_string(bin_dir.join("mycli.ps1")).unwrap();
        assert!(ps1.contains("$env:NODE_PATH=\"$basedir/..\""));
        let sh = std::fs::read_to_string(bin_dir.join("mycli")).unwrap();
        assert!(sh.contains("export NODE_PATH=\"$basedir/..\""));
    }

    #[cfg(windows)]
    #[test]
    fn create_bin_shim_appends_hidden_modules_on_windows_uses_semicolon() {
        // Regression: Node.js on Windows splits NODE_PATH on `;`
        // (`path.delimiter`) regardless of which shell launched it.
        // The ps1 / .sh wrappers use forward-slash paths but must
        // still join with `;`, or Node treats the multi-entry value
        // as one invalid path and drops the hidden-modules entry.
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let hidden = dir.path().join("node_modules/.aube/node_modules");
        std::fs::create_dir_all(&hidden).unwrap();
        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let script = pkg_dir.join("cli.js");
        std::fs::write(&script, "#!/usr/bin/env node\n").unwrap();

        create_bin_shim(
            &bin_dir,
            "mycli",
            &script,
            BinShimOptions {
                extend_node_path: true,
                prefer_symlinked_executables: None,
                hidden_modules_dir: Some(hidden.as_path()),
            },
        )
        .unwrap();

        let cmd = std::fs::read_to_string(bin_dir.join("mycli.cmd")).unwrap();
        assert!(
            cmd.contains("@SET NODE_PATH=%~dp0..;%~dp0..\\.aube\\node_modules"),
            "cmd shim should join with `;` and use backslashes:\n{cmd}"
        );
        let ps1 = std::fs::read_to_string(bin_dir.join("mycli.ps1")).unwrap();
        assert!(
            ps1.contains("$env:NODE_PATH=\"$basedir/..;$basedir/../.aube/node_modules\""),
            "ps1 shim should join with `;` even though paths use `/`:\n{ps1}"
        );
        let sh = std::fs::read_to_string(bin_dir.join("mycli")).unwrap();
        assert!(
            sh.contains("export NODE_PATH=\"$basedir/..;$basedir/../.aube/node_modules\""),
            "windows .sh shim must use `;` so Node parses both entries:\n{sh}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn create_bin_shim_omits_node_path_when_false() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let script = pkg_dir.join("cli.js");
        std::fs::write(&script, "console.log('hi');\n").unwrap();

        create_bin_shim(
            &bin_dir,
            "mycli",
            &script,
            BinShimOptions {
                extend_node_path: false,
                prefer_symlinked_executables: None,
                hidden_modules_dir: None,
            },
        )
        .unwrap();

        let cmd = std::fs::read_to_string(bin_dir.join("mycli.cmd")).unwrap();
        assert!(!cmd.contains("NODE_PATH"));
    }

    // ---------------------------------------------------------------
    // Shebang sanitization (defense against shim-injection RCE).
    //
    // `detect_bin_launch` feeds `prog` verbatim into the cmd / ps1 /
    // sh shim templates via `format!`. An attacker-published bin
    // script whose shebang carries cmd.exe metacharacters would break
    // out of the quoted path in the generated `.cmd` and execute
    // arbitrary commands on every shim invocation. `is_safe_prog`
    // must block every such case and fall through to the
    // extension-based default.
    // ---------------------------------------------------------------

    #[test]
    fn is_safe_prog_accepts_real_world_interpreters() {
        assert!(is_safe_prog("node"));
        assert!(is_safe_prog("bash"));
        assert!(is_safe_prog("sh"));
        assert!(is_safe_prog("python3"));
        assert!(is_safe_prog("python3.11"));
        assert!(is_safe_prog("ruby"));
        assert!(is_safe_prog("deno"));
        assert!(is_safe_prog("bun"));
        assert!(is_safe_prog("node18"));
        assert!(is_safe_prog("node-18"));
        assert!(is_safe_prog("pwsh"));
        assert!(is_safe_prog("c++"));
        assert!(is_safe_prog("ocaml-ng"));
        assert!(is_safe_prog("tsx_dev"));
    }

    #[test]
    fn is_safe_prog_rejects_cmd_metachars() {
        assert!(!is_safe_prog("node\"&calc&\""));
        assert!(!is_safe_prog("node&calc"));
        assert!(!is_safe_prog("node|evil"));
        assert!(!is_safe_prog("node>out"));
        assert!(!is_safe_prog("node<in"));
        assert!(!is_safe_prog("node^x"));
        assert!(!is_safe_prog("node%PATH%"));
        assert!(!is_safe_prog("a b"));
        assert!(!is_safe_prog("node;rm"));
        assert!(!is_safe_prog("node`evil`"));
        assert!(!is_safe_prog("node$(evil)"));
        assert!(!is_safe_prog("node\\evil"));
        assert!(!is_safe_prog("node/evil"));
        assert!(!is_safe_prog("node'evil'"));
    }

    #[test]
    fn is_safe_prog_rejects_non_ascii() {
        // Non-ASCII Unicode identifiers are valid in some systems but
        // never appear in legitimate shebangs and are a signal of an
        // attack attempting to smuggle lookalike glyphs past naive
        // string compares. Reject on principle.
        assert!(!is_safe_prog("ｎode"));
        assert!(!is_safe_prog("node\u{00a0}"));
        assert!(!is_safe_prog("nöde"));
    }

    #[test]
    fn is_safe_prog_rejects_control_chars() {
        assert!(!is_safe_prog("node\0"));
        assert!(!is_safe_prog("node\n"));
        assert!(!is_safe_prog("node\r"));
        assert!(!is_safe_prog("node\t"));
    }

    #[test]
    fn is_safe_prog_rejects_empty_and_oversize() {
        assert!(!is_safe_prog(""));
        let oversize = "a".repeat(65);
        assert!(!is_safe_prog(&oversize));
        let at_limit = "a".repeat(64);
        assert!(is_safe_prog(&at_limit));
    }

    #[test]
    fn is_safe_prog_rejects_non_alphanumeric_leading_char() {
        // No real interpreter name starts with `-`, `.`, `_`, or
        // `+`, and a leading `-` would make the resulting shim
        // resemble a CLI flag. Reject these even though the same
        // characters are fine in the interior.
        assert!(!is_safe_prog("-node"));
        assert!(!is_safe_prog(".node"));
        assert!(!is_safe_prog("_node"));
        assert!(!is_safe_prog("+node"));
        // Interior punctuation still allowed.
        assert!(is_safe_prog("python3.11"));
        assert!(is_safe_prog("node-18"));
        assert!(is_safe_prog("tsx_dev"));
        assert!(is_safe_prog("c++"));
    }

    #[test]
    fn detect_interpreter_absolute_path_with_cmd_injection_falls_back() {
        // The classic payload. Without sanitization the generated
        // .cmd shim would contain `"%~dp0\node"&calc&".exe"` which
        // cmd.exe parses as an `&calc&` command sequence.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.js");
        std::fs::write(&script, b"#!/usr/bin/node\"&calc&\"\nbody\n").unwrap();
        assert_eq!(
            detect_bin_launch(&script),
            BinLaunch::Interpreter("node".to_string())
        );
    }

    #[test]
    fn detect_interpreter_env_style_with_cmd_injection_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.js");
        std::fs::write(&script, b"#!/usr/bin/env \"node&calc&\"\nbody\n").unwrap();
        assert_eq!(
            detect_bin_launch(&script),
            BinLaunch::Interpreter("node".to_string())
        );
    }

    #[test]
    fn detect_interpreter_env_flags_with_cmd_injection_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.js");
        std::fs::write(&script, b"#!/usr/bin/env \"x&calc.exe&\"\nbody\n").unwrap();
        assert_eq!(
            detect_bin_launch(&script),
            BinLaunch::Interpreter("node".to_string())
        );
    }

    #[test]
    fn detect_interpreter_fallback_uses_extension() {
        // Unsafe shebang plus a `.sh` extension falls back to `sh`,
        // not `node`, because the extension-based default is chosen
        // after the sanitization rejection.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.sh");
        std::fs::write(&script, b"#!/usr/bin/env \"bash&evil&\"\nbody\n").unwrap();
        assert_eq!(
            detect_bin_launch(&script),
            BinLaunch::Interpreter("sh".to_string())
        );
    }

    #[test]
    fn detect_interpreter_valid_dotted_version_passes() {
        // Legitimate case: `python3.11` must still work.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.py");
        std::fs::write(&script, b"#!/usr/bin/env python3.11\n").unwrap();
        assert_eq!(
            detect_bin_launch(&script),
            BinLaunch::Interpreter("python3.11".to_string())
        );
    }

    #[test]
    fn detect_interpreter_long_prog_rejected_falls_back() {
        // Anything past 64 chars falls back. No legitimate
        // interpreter name approaches this length.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.js");
        let long = "a".repeat(128);
        let shebang = format!("#!/usr/bin/env {long}\nbody\n");
        std::fs::write(&script, shebang.as_bytes()).unwrap();
        assert_eq!(
            detect_bin_launch(&script),
            BinLaunch::Interpreter("node".to_string())
        );
    }

    // ---------------------------------------------------------------
    // Production safety net. Even if a future caller hands an unsafe
    // string straight to a shim generator without going through
    // `detect_bin_launch`, `safe_prog` must substitute a harmless
    // default rather than splice attacker bytes into the template.
    // Runs in both debug and release, unlike `debug_assert!`.
    // ---------------------------------------------------------------

    #[test]
    fn safe_prog_passes_through_valid() {
        assert_eq!(safe_prog("node"), "node");
        assert_eq!(safe_prog("python3.11"), "python3.11");
    }

    #[test]
    fn safe_prog_substitutes_on_unsafe() {
        // The core attack payload the shim templates would otherwise
        // interpolate verbatim. `safe_prog` must never return it.
        assert_eq!(safe_prog("node\"&calc&\""), "node");
        assert_eq!(safe_prog(""), "node");
        assert_eq!(safe_prog("a b"), "node");
        assert_eq!(safe_prog("node\0"), "node");
    }

    #[cfg(windows)]
    #[test]
    fn generate_cmd_shim_never_splices_unsafe_prog() {
        // Direct call bypassing `detect_bin_launch`. The generated
        // batch file must not contain the attacker's payload bytes.
        let shim = generate_cmd_shim(
            &BinLaunch::Interpreter("node\"&calc&\"".to_string()),
            "..\\pkg\\entry.js",
            None,
        );
        assert!(
            !shim.contains("&calc&"),
            "unsafe prog spliced into cmd shim:\n{shim}"
        );
        assert!(
            !shim.contains("\"&"),
            "stray quote-ampersand in cmd shim:\n{shim}"
        );
        // Substituted with the safe default.
        assert!(shim.contains("node.exe"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_direct_shims_execute_the_target_without_node() {
        let cmd = generate_cmd_shim(&BinLaunch::Direct, "..\\pkg\\native.exe", None);
        assert!(cmd.contains("@\"%~dp0\\..\\pkg\\native.exe\" %*"));
        assert!(!cmd.contains("node"));

        let ps1 = generate_ps1_shim(&BinLaunch::Direct, "../pkg/native.exe", None);
        assert!(ps1.contains("& \"$basedir/../pkg/native.exe\" $args"));
        assert!(!ps1.contains("node"));

        let sh = generate_sh_shim(&BinLaunch::Direct, "../pkg/native.exe", None);
        assert!(sh.contains("exec \"$basedir/../pkg/native.exe\" \"$@\""));
        assert!(!sh.contains("node"));
    }

    #[cfg(windows)]
    #[test]
    fn generate_ps1_shim_never_splices_unsafe_prog() {
        let shim = generate_ps1_shim(
            &BinLaunch::Interpreter("bash&rm".to_string()),
            "../pkg/entry.js",
            None,
        );
        assert!(
            !shim.contains("&rm"),
            "unsafe prog spliced into ps1 shim:\n{shim}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn generate_sh_shim_never_splices_unsafe_prog() {
        let shim = generate_sh_shim(
            &BinLaunch::Interpreter("sh;rm".to_string()),
            "../pkg/entry.js",
            None,
        );
        assert!(
            !shim.contains(";rm"),
            "unsafe prog spliced into sh shim:\n{shim}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn generate_posix_shim_never_splices_unsafe_prog() {
        let shim = generate_posix_shim(
            &BinLaunch::Interpreter("sh;rm".to_string()),
            "../pkg/entry.js",
            None,
        );
        assert!(
            !shim.contains(";rm"),
            "unsafe prog spliced into posix shim:\n{shim}"
        );
    }
    /// Round-trip the ownership parser against what the WRITER emits, rather
    /// than against a hand-written fixture.
    ///
    /// The parser exists to tell a shim this tool wrote from one npm, pnpm or
    /// yarn wrote. Checking it on invented input only confirms the invention:
    /// two Windows-only defects reached review that way, because the local
    /// suite compiled the Windows arms away and the fixtures encoded the same
    /// assumption the code did. Runs on every platform — `generate_cmd_shim`
    /// is pure formatting and is deliberately not gated to Windows.
    #[test]
    fn parse_win_shim_target_recovers_what_generate_cmd_shim_embeds() {
        for launch in [BinLaunch::Direct, BinLaunch::Interpreter("node".to_string())] {
            let rel = r"..\share\nub\global\1a-2b\node_modules\pkg\cli.js";
            let text = generate_cmd_shim(&launch, rel, None);
            assert_eq!(
                parse_win_shim_target(&text).as_deref(),
                Some(rel),
                "the parser must recover exactly the target the writer embedded \
                 ({launch:?}); emitted text was:\n{text}"
            );
        }
    }

    /// The NODE_PATH line the node dialect emits is unquoted `%~dp0`, so it must
    /// not be mistaken for the target — the failure mode would be silently
    /// claiming somebody else's slot.
    #[test]
    fn parse_win_shim_target_ignores_the_node_path_line() {
        let rel = r"..\pkg\cli.js";
        let text = generate_cmd_shim(&BinLaunch::Interpreter("node".to_string()), rel, Some("%~dp0\\..\\node_modules"));
        assert_eq!(parse_win_shim_target(&text).as_deref(), Some(rel));
    }

}

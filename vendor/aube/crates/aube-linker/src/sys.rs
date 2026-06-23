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

use std::io;
use std::path::{Component, Path, PathBuf};

/// Create a directory link from `link` to `target`.
///
/// - Unix: a plain symlink (relative or absolute target OK).
/// - Windows: an NTFS junction (relative targets are resolved to
///   absolute against `link`'s parent first).
pub fn create_dir_link(target: &Path, link: &Path) -> io::Result<()> {
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

/// Create bin shims for a package binary.
///
/// - Unix (default / `prefer_symlinked_executables != Some(false)`):
///   a symlink from `bin_dir/<name>` to `target`, with the target
///   chmod'd to 755.
/// - Unix (`prefer_symlinked_executables = Some(false)`): a shell
///   wrapper that `exec`s `target` via its detected interpreter. If
///   `extend_node_path` is set, the wrapper exports `NODE_PATH` first.
/// - Windows: three wrapper scripts in `bin_dir`:
///   - `<name>.cmd` — batch wrapper for cmd.exe
///   - `<name>.ps1` — PowerShell wrapper
///   - `<name>` (no extension) — shell wrapper for Git Bash / MSYS2
///
///   `extend_node_path` sets `NODE_PATH` near the top of each wrapper.
///
/// The `target` path should be absolute; generated wrappers embed a
/// path relative to the wrapper's own parent directory so the tree
/// stays relocatable even for scoped bin names under `.bin/@scope/`.
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
            let prog = detect_interpreter(target);
            std::fs::write(
                &link_path,
                generate_posix_shim(&prog, &rel, node_path.as_deref()),
            )?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&link_path, std::fs::Permissions::from_mode(0o755))?;
        } else {
            std::os::unix::fs::symlink(target, &link_path)?;
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
        let prog = detect_interpreter(target);

        let rel_backslash = rel.replace('/', "\\");
        let rel_fwdslash = rel.replace('\\', "/");
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
            generate_cmd_shim(&prog, &rel_backslash, node_path_backslash.as_deref()).as_bytes(),
        )?;
        write_shim_file(
            &bin_dir.join(format!("{name}.ps1")),
            generate_ps1_shim(&prog, &rel_fwdslash, node_path_fwdslash.as_deref()).as_bytes(),
        )?;
        write_shim_file(
            &bin_dir.join(name),
            generate_sh_shim(&prog, &rel_fwdslash, node_path_fwdslash.as_deref()).as_bytes(),
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
#[cfg(windows)]
fn win_shim_paths(bin_dir: &Path, name: &str) -> [PathBuf; 3] {
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

/// Read the shebang line of `target` to determine the interpreter.
/// Falls back to `"node"` for `.js` / `.cjs` / `.mjs` files, or if
/// the target doesn't exist or has no shebang.
///
/// Only reads the first 256 bytes — enough for any realistic shebang
/// line without pulling large bundled scripts into memory.
fn detect_interpreter(target: &Path) -> String {
    use std::io::Read;
    let mut buf = [0u8; 256];
    let n = std::fs::File::open(target)
        .and_then(|mut f| f.read(&mut buf))
        .unwrap_or(0);
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
            return prog.to_string();
        }
        // Unsafe shebang. Log it rather than rewriting silently so
        // the fall-through is visible in install output. Both path
        // and prog go through Debug formatting so any terminal
        // escape sequences smuggled in either one are printed as
        // escaped literals rather than acted on by the terminal.
        tracing::warn!("ignoring unsafe shebang interpreter in {target:?}: {prog:?}");
    }
    default_interpreter_for_extension(target)
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

fn default_interpreter_for_extension(target: &Path) -> String {
    match target.extension().and_then(|e| e.to_str()) {
        Some("js" | "cjs" | "mjs") | None => "node".to_string(),
        Some("cmd" | "bat") => "cmd".to_string(),
        Some("ps1") => "pwsh".to_string(),
        Some("sh") => "sh".to_string(),
        Some(_) => "node".to_string(),
    }
}

/// Run-time substitute for any `prog` that reaches a shim generator
/// without passing `is_safe_prog`. Every caller in this crate goes
/// through `detect_interpreter` and never trips this branch, but a
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

#[cfg(windows)]
fn generate_cmd_shim(
    prog: &str,
    rel_target_backslash: &str,
    node_path_value: Option<&str>,
) -> String {
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
    prog: &str,
    rel_target_fwdslash: &str,
    node_path_value: Option<&str>,
) -> String {
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
    prog: &str,
    rel_target_fwdslash: &str,
    node_path_value: Option<&str>,
) -> String {
    let prog = safe_prog(prog);
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
         if [ -x \"$basedir/{prog}\" ]; then\n\
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
#[cfg(unix)]
fn generate_posix_shim(
    prog: &str,
    rel_target_fwdslash: &str,
    node_path_value: Option<&str>,
) -> String {
    let prog = safe_prog(prog);
    let node_path =
        node_path_value.map_or(String::new(), |val| format!("export NODE_PATH=\"{val}\"\n"));
    format!(
        "#!/bin/sh\n\
         {POSIX_SHIM_MARKER_PREFIX}{rel_target_fwdslash}\n\
         basedir=$(dirname \"$0\")\n\
         {node_path}\
         if [ -x \"$basedir/{prog}\" ]; then\n\
         \x20 exec \"$basedir/{prog}\" \"$basedir/{rel_target_fwdslash}\" \"$@\"\n\
         else\n\
         \x20 exec {prog} \"$basedir/{rel_target_fwdslash}\" \"$@\"\n\
         fi\n"
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
        assert_eq!(detect_interpreter(&script), "node");
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
        assert_eq!(detect_interpreter(&script), "node");
    }

    #[test]
    fn detect_interpreter_shebang_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.js");
        std::fs::write(&script, "#!/usr/bin/node\nconsole.log('hi');\n").unwrap();
        assert_eq!(detect_interpreter(&script), "node");
    }

    #[test]
    fn detect_interpreter_shebang_env_python() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.py");
        std::fs::write(&script, "#!/usr/bin/env python3\nprint('hi')\n").unwrap();
        assert_eq!(detect_interpreter(&script), "python3");
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
        assert_eq!(detect_interpreter(&script), "node");
    }

    #[test]
    fn detect_interpreter_no_shebang_js() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.js");
        std::fs::write(&script, "console.log('hi');\n").unwrap();
        assert_eq!(detect_interpreter(&script), "node");
    }

    #[test]
    fn detect_interpreter_nonexistent_file_defaults_to_node() {
        assert_eq!(
            detect_interpreter(Path::new("/nonexistent/file.js")),
            "node"
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
    // `detect_interpreter` feeds `prog` verbatim into the cmd / ps1 /
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
        assert_eq!(detect_interpreter(&script), "node");
    }

    #[test]
    fn detect_interpreter_env_style_with_cmd_injection_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.js");
        std::fs::write(&script, b"#!/usr/bin/env \"node&calc&\"\nbody\n").unwrap();
        assert_eq!(detect_interpreter(&script), "node");
    }

    #[test]
    fn detect_interpreter_env_flags_with_cmd_injection_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.js");
        std::fs::write(&script, b"#!/usr/bin/env \"x&calc.exe&\"\nbody\n").unwrap();
        assert_eq!(detect_interpreter(&script), "node");
    }

    #[test]
    fn detect_interpreter_fallback_uses_extension() {
        // Unsafe shebang plus a `.sh` extension falls back to `sh`,
        // not `node`, because the extension-based default is chosen
        // after the sanitization rejection.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.sh");
        std::fs::write(&script, b"#!/usr/bin/env \"bash&evil&\"\nbody\n").unwrap();
        assert_eq!(detect_interpreter(&script), "sh");
    }

    #[test]
    fn detect_interpreter_valid_dotted_version_passes() {
        // Legitimate case: `python3.11` must still work.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.py");
        std::fs::write(&script, b"#!/usr/bin/env python3.11\n").unwrap();
        assert_eq!(detect_interpreter(&script), "python3.11");
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
        assert_eq!(detect_interpreter(&script), "node");
    }

    // ---------------------------------------------------------------
    // Production safety net. Even if a future caller hands an unsafe
    // string straight to a shim generator without going through
    // `detect_interpreter`, `safe_prog` must substitute a harmless
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
        // Direct call bypassing `detect_interpreter`. The generated
        // batch file must not contain the attacker's payload bytes.
        let shim = generate_cmd_shim("node\"&calc&\"", "..\\pkg\\entry.js", None);
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
    fn generate_ps1_shim_never_splices_unsafe_prog() {
        let shim = generate_ps1_shim("bash&rm", "../pkg/entry.js", None);
        assert!(
            !shim.contains("&rm"),
            "unsafe prog spliced into ps1 shim:\n{shim}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn generate_sh_shim_never_splices_unsafe_prog() {
        let shim = generate_sh_shim("sh;rm", "../pkg/entry.js", None);
        assert!(
            !shim.contains(";rm"),
            "unsafe prog spliced into sh shim:\n{shim}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn generate_posix_shim_never_splices_unsafe_prog() {
        let shim = generate_posix_shim("sh;rm", "../pkg/entry.js", None);
        assert!(
            !shim.contains(";rm"),
            "unsafe prog spliced into posix shim:\n{shim}"
        );
    }
}

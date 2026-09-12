//! Global install layout — `aube add -g`, `aube remove -g`, `aube list -g`.
//!
//! Two roots, deliberately separate. Bins go to the SHARED user-binary
//! directory that systems already put on PATH; the installs themselves go
//! under our own namespaced data root, beside the content store:
//!
//! ```text
//! ~/.local/bin/                    # <bin_dir>: shared, on PATH, NOT ours alone
//! └── some-bin        -> <pkg_dir>/<install>/node_modules/<alias>/<bin>
//!
//! $XDG_DATA_HOME/<ns>/global/      # <pkg_dir>: one subdir per global package
//! ├── <pid>-<ts>/                  # physical install dir (normal aube project)
//! │   ├── package.json
//! │   └── node_modules/
//! └── <hash>              -> <pid>-<ts>   # stable pointer keyed on aliases
//! ```
//!
//! The split is the point. A tool-owned bin directory is on PATH for nobody
//! until something wires it up, which made a successful install produce
//! commands that would not run. Sharing the conventional directory fixes that
//! and costs one obligation: every write there must prove ownership first,
//! because the neighbours are other tools' binaries.
//!
//! Each `aube add -g <pkg>` runs a full normal install into a fresh
//! `<pid>-<ts>` directory, then:
//!   1. Computes a hash of the resolved aliases.
//!   2. Creates `<pkg_dir>/<hash>` as a symlink to the install dir. Any
//!      existing installs of the same aliases are removed first.
//!   3. Symlinks each package's bins from the install dir into `<global_bin>`.
//!
//! `remove -g` / `list -g` walk the hash symlinks in `<pkg_dir>` to find
//! installed packages.

use miette::{Context, IntoDiagnostic, miette};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where aube puts globally-installed packages and their PATH-visible bins.
///
/// `bin_dir` is the shared user-binary directory the system already has on
/// `PATH` — it holds entries from every tool that installs there, so it is
/// never ours to write blindly. `pkg_dir` holds the per-install directories
/// and hash pointers, under our own data namespace where nothing else lives.
#[derive(Debug, Clone)]
pub struct GlobalLayout {
    pub bin_dir: PathBuf,
    pub pkg_dir: PathBuf,
}

impl GlobalLayout {
    pub fn resolve() -> miette::Result<Self> {
        let cwd = std::env::current_dir().unwrap_or_default();

        // `bin_dir` and `pkg_dir` are independent, and now resolve from
        // DIFFERENT roots: bins go to the shared user-binary directory that is
        // already on PATH ([`default_bin_dir`]), while package installs go
        // under our own namespaced data root ([`prefix_dir`]). They used to
        // share one root, which is how bins ended up in a directory named
        // after another package manager and on nobody's PATH.
        let (setting_bin, setting_pkg) = super::with_settings_ctx(&cwd, |ctx| {
            let bin = aube_settings::resolved::global_bin_dir(ctx)
                .and_then(|raw| super::expand_setting_path(&raw, &cwd));
            let pkg = aube_settings::resolved::global_dir(ctx)
                .and_then(|raw| super::expand_setting_path(&raw, &cwd));
            (bin, pkg)
        });

        let bin_dir = setting_bin.map_or_else(default_bin_dir, Ok)?;
        // A `global` leaf under the resolved root, matching pnpm's
        // `<home>/global`. Under our own data namespace there is no sibling
        // install to collide with, so the directory does not need the
        // embedder's name in it — unlike the pre-2.0 layout, whose
        // `global-<embedder>` leaf is what the migration scan still looks for.
        let pkg_dir =
            setting_pkg.map_or_else(|| default_pkg_dir("global"), |p| Ok(p.join("global")))?;

        let legacy_subdir = format!("global-{}", aube_util::embedder().name);
        warn_on_legacy_global_dir(&pkg_dir, &legacy_subdir);
        Ok(Self { bin_dir, pkg_dir })
    }
}

/// The branded home override (standalone aube → `AUBE_HOME`). When set it
/// *is* the PATH-visible bin dir, and package installs go in a subdir of
/// it — the pre-existing contract for people who opted in explicitly. An
/// embedder with no `env_prefix` (nub) skips the branded var and takes the
/// conventional resolution below.
fn branded_home() -> Option<PathBuf> {
    let prefix = aube_util::embedder().env_prefix?;
    std::env::var(format!("{prefix}_HOME"))
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The tool's own data root: `$XDG_DATA_HOME/<ns>`, falling back to
/// `~/.local/share/<ns>` (`%LOCALAPPDATA%\<ns>` on Windows). Same
/// resolution `aube_store::dirs::store_dir` uses, so global installs land
/// beside `store/`, `nodejs/`, and `shims/` instead of in a directory
/// named after another package manager. `<ns>` is the active embedder's
/// `data_namespace` (standalone aube → `aube`).
///
/// XDG is honored on every Unix, macOS included — aube already does that
/// for the store and the packument cache, and the previous `~/Library/pnpm`
/// special case was the one place a macOS user's explicit `XDG_DATA_HOME`
/// was ignored (Discussion #1219).
///
/// Precedence matches `store_dir` exactly, including `%LOCALAPPDATA%`
/// winning over `XDG_DATA_HOME` on Windows: the global dir and the content
/// store must not end up under different roots on the same machine.
fn data_root() -> miette::Result<PathBuf> {
    let ns = aube_util::embedder().data_namespace;
    #[cfg(windows)]
    if let Ok(local) = std::env::var("LOCALAPPDATA")
        && !local.is_empty()
    {
        return Ok(PathBuf::from(local).join(ns));
    }
    // Reached on every Unix, and on Windows when `%LOCALAPPDATA%` is
    // missing — where an explicitly-set `XDG_DATA_HOME` is a better answer
    // than failing outright, again mirroring `store_dir`.
    let data_home = match aube_util::env::xdg_data_home() {
        Some(xdg) => xdg,
        None => aube_util::env::home_dir()
            .ok_or_else(|| miette!("HOME is not set; can't locate global directory"))?
            .join(".local/share"),
    };
    Ok(data_home.join(ns))
}

/// Resolve the PATH-visible directory global bins link into.
///
/// This follows the shared user-binary convention rather than owning a
/// directory of our own, in that order:
///
/// - the branded `<PREFIX>_HOME`, when the embedder declares one and the
///   user set it — an explicit instruction, not a default to second-guess
/// - `XDG_BIN_HOME`
/// - `$XDG_DATA_HOME/../bin`, so a relocated XDG root is respected
/// - `~/.local/bin`
///
/// On every platform, matching uv and pipx. The point is that most Linux
/// distributions already put `~/.local/bin` on PATH — the XDG spec asks them
/// to — so a global install is runnable without editing a shell profile. A
/// tool-owned directory is on PATH for nobody until something wires it up,
/// which is the whole defect this resolution exists to avoid.
///
/// It also means the directory is SHARED with every other tool that installs
/// there, so nothing may be overwritten without proving we own it — see
/// [`bin_slot_is_writable`].
fn default_bin_dir() -> miette::Result<PathBuf> {
    if let Some(home) = branded_home() {
        return Ok(home);
    }
    if let Ok(v) = std::env::var("XDG_BIN_HOME")
        && !v.is_empty()
    {
        return Ok(PathBuf::from(v));
    }
    // `parent()` rather than joining a literal `..`: the path is printed by
    // `bin -g`, compared against PATH entries, and written into shell
    // profiles, and a `..` component makes all three read wrong.
    if let Some(parent) = aube_util::env::xdg_data_home()
        .as_deref()
        .and_then(Path::parent)
    {
        return Ok(parent.join("bin"));
    }
    let home = aube_util::env::home_dir()
        .ok_or_else(|| miette!("HOME is not set; can't locate the global bin directory"))?;
    Ok(home.join(".local/bin"))
}

/// Default for `globalDir` — where the physical per-package install dirs
/// and their hash pointers live. A sibling of the bin directory, never a
/// child: the PATH entry stays a directory of executables.
fn default_pkg_dir(pkg_subdir: &str) -> miette::Result<PathBuf> {
    if let Some(home) = branded_home() {
        return Ok(home.join(pkg_subdir));
    }
    data_root().map(|d| d.join(pkg_subdir))
}

/// Resolve the root holding global package installs — distinct from the bin
/// directory, which is shared and lives on PATH.
///
/// This is our own namespaced data directory, the same root and the same
/// `data_namespace` the content store already uses, so a global install lands
/// beside the store instead of inside a directory named after another package
/// manager. No `PNPM_HOME` and no pnpm-named path: a global operation does not
/// consult whatever tool a project happens to use.
pub fn prefix_dir() -> miette::Result<PathBuf> {
    if let Some(home) = branded_home() {
        return Ok(home);
    }
    data_root()
}

/// Directories a pre-2.0 aube used as its global root, in the order that
/// version consulted them. Read only to warn: aube never installs into,
/// reads packages out of, or deletes anything under a pnpm-owned path.
fn legacy_home_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(v) = std::env::var("PNPM_HOME")
        && !v.is_empty()
    {
        out.push(PathBuf::from(v));
    }
    if cfg!(windows) {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            out.push(PathBuf::from(local).join("pnpm"));
        }
    } else if cfg!(target_os = "macos")
        && let Some(home) = aube_util::env::home_dir()
    {
        out.push(home.join("Library/pnpm"));
    }
    if !cfg!(windows) {
        match aube_util::env::xdg_data_home() {
            Some(xdg) => out.push(xdg.join("pnpm")),
            None => {
                if let Some(home) = aube_util::env::home_dir() {
                    out.push(home.join(".local/share/pnpm"));
                }
            }
        }
    }
    out
}

/// True when `pkg_dir` holds at least one hash pointer — i.e. at least one
/// global package is installed there.
fn has_global_installs(pkg_dir: &Path) -> bool {
    std::fs::read_dir(pkg_dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.file_type().is_ok_and(|t| t.is_symlink()))
    })
}

/// Warn once per process when the caller has global packages stranded in
/// a pre-2.0 (pnpm-named) location and none in the current one. Without
/// this, `aube list -g` just comes back empty and the bins already on
/// `$PATH` keep working while `remove -g` claims they aren't installed —
/// the failure mode is silent, so the warning is the migration path.
///
/// `legacy_subdir` is the leaf the OLD layout used (`global-<embedder>`),
/// which is not the leaf the current layout writes.
fn warn_on_legacy_global_dir(pkg_dir: &Path, legacy_subdir: &str) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if has_global_installs(pkg_dir) {
            return;
        }
        let Some(legacy) = legacy_home_candidates()
            .into_iter()
            .find(|home| has_global_installs(&home.join(legacy_subdir)))
        else {
            return;
        };
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_GLOBAL_DIR_LEGACY_LOCATION,
            legacy_dir = %legacy.display(),
            current_dir = %pkg_dir.display(),
            "global packages from an older aube are still in {}; aube now keeps its own global \
             directory at {}. Reinstall them with `{}`, or set {}_HOME={} to keep using the old \
             location.",
            legacy.display(),
            pkg_dir.display(),
            aube_util::cmd("add -g <pkg>"),
            aube_util::embedder().env_prefix.unwrap_or("AUBE"),
            legacy.display(),
        );
    });
}

/// Whether `bin_dir` is one of the directories in `path_var`. Compared
/// canonically so a `$PATH` entry that reaches the same directory through a
/// symlink (or a `~`-relative vs absolute spelling) still counts as a
/// match; entries that don't resolve are compared verbatim.
///
/// `None` (an unset `PATH`) is not on `PATH` — nothing is — so it answers
/// `false` rather than being treated as "can't tell, assume fine".
fn bin_dir_on_path(bin_dir: &Path, path_var: Option<&std::ffi::OsStr>) -> bool {
    let Some(path) = path_var else {
        return false;
    };
    let want = std::fs::canonicalize(bin_dir).unwrap_or_else(|_| bin_dir.to_path_buf());
    std::env::split_paths(path).any(|entry| std::fs::canonicalize(&entry).unwrap_or(entry) == want)
}

/// Whether `dir` is already an entry in `PATH`. The public form of
/// [`bin_dir_on_path`], for an embedder that wires PATH itself and so owns
/// both the decision and the message — see [`warn_if_bin_dir_not_on_path`],
/// which is the answer for one that does not.
pub fn dir_is_on_path(dir: &Path) -> bool {
    bin_dir_on_path(dir, std::env::var_os("PATH").as_deref())
}

/// Warn when `bin_dir` is absent from `$PATH`.
pub fn warn_if_bin_dir_not_on_path(bin_dir: &Path) {
    if bin_dir_on_path(bin_dir, std::env::var_os("PATH").as_deref()) {
        return;
    }
    tracing::warn!(
        code = aube_codes::warnings::WARN_AUBE_GLOBAL_BIN_DIR_NOT_ON_PATH,
        bin_dir = %bin_dir.display(),
        "{} is not on your PATH, so globally installed commands won't be found. Add it to PATH \
         (e.g. `export PATH=\"{}:$PATH\"`), or set globalBinDir to a directory that already is.",
        bin_dir.display(),
        bin_dir.display(),
    );
}

/// Create a fresh install directory under `pkg_dir`. Matches pnpm's naming
/// convention (`<pid-hex>-<time-hex>`) so the dirs sort intuitively and
/// the orphan-cleanup logic can't confuse them with hash pointer symlinks.
pub fn create_install_dir(pkg_dir: &Path) -> miette::Result<PathBuf> {
    std::fs::create_dir_all(pkg_dir)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to create global dir {}", pkg_dir.display()))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = format!("{:x}-{:x}", std::process::id(), now);
    let dir = pkg_dir.join(name);
    std::fs::create_dir_all(&dir)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to create install dir {}", dir.display()))?;
    Ok(dir)
}

/// Compute a stable hash for a set of aliases plus the registry map. Two
/// `aube add -g` invocations with the same aliases (and registry config)
/// land on the same pointer, so the second overwrites the first.
pub fn cache_key(aliases: &[String], registries: &BTreeMap<String, String>) -> String {
    let mut sorted = aliases.to_vec();
    sorted.sort();
    let registries_vec: Vec<(&String, &String)> = registries.iter().collect();
    let payload = serde_json::json!([sorted, registries_vec]).to_string();
    let digest = Sha256::digest(payload.as_bytes());
    hex::encode(digest)
}

/// Path to the hash pointer (symlink) for a given cache key.
pub fn hash_link(pkg_dir: &Path, hash: &str) -> PathBuf {
    pkg_dir.join(hash)
}

#[derive(Debug, Clone)]
pub struct GlobalPackageInfo {
    pub hash: String,
    pub install_dir: PathBuf,
    /// Aliases from the install dir's `package.json` `dependencies`.
    pub aliases: Vec<String>,
}

/// Walk `pkg_dir`, resolve every symlink entry to its physical install
/// directory, and read the aliases out of that directory's `package.json`.
/// Non-symlinks (raw install dirs) and dangling/broken symlinks are skipped.
pub fn scan_packages(pkg_dir: &Path) -> Vec<GlobalPackageInfo> {
    let Ok(entries) = std::fs::read_dir(pkg_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_symlink() {
            continue;
        }
        let link_path = entry.path();
        // `crate::dirs::canonicalize` strips the Windows `\\?\` verbatim
        // prefix so the `install_dir` we hand back can be compared with
        // `==` / `starts_with` against paths produced by `run_global` (also
        // routed through the same helper). Without this, the prior-cleanup
        // branch in `run_global_inner` never matches on Windows and stale
        // hash pointers / install dirs accumulate.
        let Ok(install_dir) = crate::dirs::canonicalize(&link_path) else {
            continue;
        };
        let manifest_path = install_dir.join("package.json");
        let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(deps) = json.get("dependencies").and_then(|d| d.as_object()) else {
            continue;
        };
        if deps.is_empty() {
            continue;
        }
        let aliases: Vec<String> = deps.keys().cloned().collect();
        out.push(GlobalPackageInfo {
            hash: entry.file_name().to_string_lossy().into_owned(),
            install_dir,
            aliases,
        });
    }
    out
}

/// Find the global install that owns `alias` (if any). pnpm parity:
/// returns the first match; there should only ever be one because each
/// install is keyed on its alias set.
pub fn find_package(pkg_dir: &Path, alias: &str) -> Option<GlobalPackageInfo> {
    scan_packages(pkg_dir)
        .into_iter()
        .find(|info| info.aliases.iter().any(|a| a == alias))
}

/// Create a symlink (replacing any existing entry). Used both for hash
/// pointers and for global bin entries. Delegates removal to
/// `super::remove_existing` so an entry that happens to be a regular
/// directory or a non-symlink file gets cleaned up correctly instead of
/// silently failing the subsequent create with `EEXIST`.
pub fn symlink_force(target: &Path, link: &Path) -> miette::Result<()> {
    super::remove_existing(link)?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to symlink {}", link.display()))?;
    }
    #[cfg(windows)]
    {
        // Hash pointers target install dirs, so the common path uses
        // `create_dir_link` (an NTFS junction — no Developer Mode
        // required). The non-directory fallback is rare but still
        // goes through the file-symlink syscall, which *does* need
        // Developer Mode until cmd-shim generation lands.
        if target.is_dir() {
            aube_linker::create_dir_link(target, link)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to symlink {}", link.display()))?;
        } else {
            std::os::windows::fs::symlink_file(target, link)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to symlink {}", link.display()))?;
        }
    }
    Ok(())
}

/// Whether a global install may write `<bin_dir>/<name>`.
///
/// An empty slot is free. A slot owned by some install under `pkg_dir` is
/// ours to replace — that covers this install's own prior links, which
/// `add -g` legitimately overwrites on a re-add. Anything else is FOREIGN:
/// another tool's binary, or one the user put there by hand.
///
/// The global bin dir is shared by construction — it is whatever directory
/// the user has on `PATH`, so it holds entries from every tool that installs
/// there — which is why the mere existence of the slot can never be taken as
/// permission to overwrite it.
fn bin_slot_is_writable(bin_dir: &Path, pkg_dir: &Path, name: &str) -> bool {
    // Windows writes THREE files per bin (`<name>`, `<name>.cmd`, `<name>.ps1`)
    // and overwrites each unconditionally, so the name is occupied when ANY of
    // them is. Checking only the extensionless path misses the common case
    // outright: npm, pnpm and yarn install a `<name>.cmd` with no extensionless
    // sibling, so the slot reads as empty and their shim is replaced — the very
    // thing this guard exists to prevent. Driven off the writer's own list so
    // the two cannot drift apart.
    #[cfg(windows)]
    {
        // The `.cmd` decides the whole slot. Of the three files the writer
        // emits, only that one carries a recoverable target: the `.ps1` and the
        // extensionless wrapper resolve `$basedir` at run time and stamp no
        // marker, so demanding that every path prove itself rejects the shims
        // this tool wrote moments earlier. `unlink_bins` keys on `{name}.cmd`
        // for the same reason, so the two now agree.
        //
        // Getting this wrong is not a warning, it is data loss: an unwritable
        // slot makes `link_bins` skip every bin, and `add -g` reads the empty
        // result as "nothing was re-linked", which disarms the filter guarding
        // the prior install's bins — so re-adding a package you already have
        // removes it and leaves no command behind.
        let cmd = bin_dir.join(format!("{name}.cmd"));
        if cmd.symlink_metadata().is_ok() {
            return slot_entry_is_ours(&cmd, pkg_dir);
        }
        // No `.cmd`, so nothing here came from this writer — it never emits a
        // sibling without one. Any other occupant is somebody else's.
        aube_linker::win_shim_paths(bin_dir, name)
            .iter()
            .all(|p| p.symlink_metadata().is_err())
    }
    #[cfg(not(windows))]
    {
        slot_entry_is_ours(&bin_dir.join(name), pkg_dir)
    }
}

/// Whether one concrete path in the bin dir is free, or is occupied by an
/// entry this tool created. See [`bin_slot_is_writable`] for the policy.
fn slot_entry_is_ours(link: &Path, pkg_dir: &Path) -> bool {
    let bin_dir = link.parent().unwrap_or(Path::new(""));
    let Ok(meta) = link.symlink_metadata() else {
        return true; // nothing there
    };
    // Both forms of the package dir. A lexical path never matches a
    // canonicalized one once any component is a symlink — macOS `/tmp` ->
    // `/private/tmp`, a symlinked `$HOME` under Docker or Nix, a relocated
    // `XDG_DATA_HOME` — and the dangling-link arm below compares a LEXICAL
    // target, so testing only the canonical form reports a bin we created as
    // somebody else's. `unlink_bins` keeps both for the same reason.
    let pkg_canon = std::fs::canonicalize(pkg_dir).unwrap_or_else(|_| pkg_dir.to_path_buf());
    let pkg_lex = aube_linker::normalize_path(pkg_dir);

    if meta.file_type().is_symlink() {
        let Ok(raw) = std::fs::read_link(link) else {
            return false;
        };
        let absolute = if raw.is_absolute() {
            raw
        } else {
            link.parent().unwrap_or(bin_dir).join(raw)
        };
        // Surface shape: the link points straight into the global pkg dir.
        let lex = aube_linker::normalize_path(&absolute);
        if lex.starts_with(&pkg_lex) || lex.starts_with(&pkg_canon) {
            return true;
        }
        match std::fs::canonicalize(&absolute) {
            // Store shape: the link resolves through `node_modules/<alias>`
            // into the shared content store, landing outside `pkg_dir`
            // entirely, so it can only be recognised by matching it against
            // what the installs living there actually own.
            Ok(resolved) => scan_packages(pkg_dir).iter().any(|info| {
                owned_bins(&info.install_dir, &info.aliases)
                    .iter()
                    .any(|bin| bin.target.as_ref().is_some_and(|t| *t == resolved))
            }),
            // Broken link into our own tree: a leftover of ours, so reclaiming
            // it is right. Broken and pointing elsewhere stays untouched — we
            // cannot show it is not something the user is repairing.
            Err(_) => false,
        }
    } else {
        // A regular file is one of ours only when its embedded target points
        // back into the global package dir.
        //
        // The presence of a shim shape proves nothing about who wrote it: npm,
        // pnpm and yarn all emit `%~dp0`-relative `.cmd` wrappers of the same
        // form, so testing for the marker alone would adopt every one of them
        // as ours and overwrite it — the precise failure this guard exists to
        // stop. Only where the target RESOLVES distinguishes them.
        let Ok(content) = std::fs::read_to_string(link) else {
            return false;
        };
        let rel = aube_linker::parse_posix_shim_target(&content)
            .map(str::to_string)
            .or_else(|| aube_linker::parse_win_shim_target(&content));
        let Some(rel) = rel else {
            return false;
        };
        let resolved = aube_linker::normalize_path(&bin_dir.join(rel.replace('\\', "/")));
        resolved.starts_with(&pkg_lex) || resolved.starts_with(&pkg_canon)
    }
}

/// After a global install lands, link each resolved dependency's bins
/// into `<bin_dir>`. Bins are extracted from each package's `package.json`
/// inside `<install_dir>/node_modules/<alias>/`. Returns the list of bin
/// names that were linked — callers use this list to undo the links on
/// `aube remove -g`.
///
/// A name already held by a foreign file is skipped with a warning rather
/// than overwritten, and rather than failing the whole install: the other
/// packages in the same command still have to land, and the occupant is the
/// user's to remove.
pub fn link_bins(
    install_dir: &Path,
    bin_dir: &Path,
    pkg_dir: &Path,
    aliases: &[String],
    shim_opts: aube_linker::BinShimOptions,
) -> miette::Result<Vec<String>> {
    std::fs::create_dir_all(bin_dir)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to create bin dir {}", bin_dir.display()))?;
    let modules = super::project_modules_dir(install_dir);
    let mut linked = Vec::new();
    for alias in aliases {
        let alias_dir = modules.join(alias);
        let manifest_path = alias_dir.join("package.json");
        let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(bin_field) = json.get("bin") else {
            continue;
        };
        let bins: Vec<(String, String)> = match bin_field {
            serde_json::Value::String(path) => {
                let name = alias.rsplit('/').next().unwrap_or(alias).to_string();
                vec![(name, path.clone())]
            }
            serde_json::Value::Object(map) => map
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect(),
            _ => continue,
        };
        for (name, rel) in bins {
            if aube_linker::validate_bin_name(&name).is_err()
                || aube_linker::validate_bin_target(&rel).is_err()
            {
                continue;
            }
            if !bin_slot_is_writable(bin_dir, pkg_dir, &name) {
                eprintln!(
                    "warning: not linking {name} — {} already exists and was not \
                     created by this tool; remove it to link {name}",
                    bin_dir.join(&name).display()
                );
                continue;
            }
            let target = alias_dir.join(&rel);
            aube_linker::create_bin_shim(bin_dir, &name, &target, shim_opts)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to create bin shim for {name}"))?;
            linked.push(name);
        }
    }
    Ok(linked)
}

/// Remove bin entries we own. A bin that was overwritten by a later
/// `aube add -g` belongs to that later install, so we leave it alone.
///
/// Ownership is decided two ways, and the first is what makes an isolated
/// install work at all: a symlinked bin resolves THROUGH
/// `node_modules/<alias>` into the shared content store, so it lands
/// outside `install_dir` and a containment test alone never matches — the
/// bins then leak as dangling links every `remove -g`. Comparing the
/// resolved link against the target [`owned_bins`] captured from this
/// install's own manifest is what identifies it. Containment stays as the
/// second test for the layouts that do keep the target inside the install.
///
/// Both sides are canonicalized. On macOS, temp dirs like `/var/folders/...`
/// are symlinks to `/private/var/folders/...`; without canonicalizing both
/// the comparison always returns false and the bins leak.
///
/// Two installs providing the same package at the same version resolve to
/// the same store path, so target equality cannot tell their bins apart and
/// either `remove -g` reclaims the shared link. That is deliberate: the
/// links are byte-identical, and re-running `add -g` restores the survivor's.
pub fn unlink_bins(install_dir: &Path, bin_dir: &Path, bins: &[OwnedBin]) {
    #[cfg(unix)]
    {
        let install_canon = std::fs::canonicalize(install_dir).ok();
        // Lex-normalized `install_dir` is the ownership anchor for the two
        // cases that cannot be canonicalized: a regular-file shim
        // (`preferSymlinkedExecutables=false`), whose `$basedir/<rel>` target
        // would resolve through the project's symlinks into the shared virtual
        // store, and a dangling link, whose target no longer exists at all.
        let install_lex = aube_linker::normalize_path(install_dir);
        for bin in bins {
            let link = bin_dir.join(&bin.name);
            // A scoped name (`@scope/foo`) puts the link a directory below
            // `bin_dir`, and `create_bin_shim` anchors both the symlink
            // target and the shim's `$basedir` on that deeper directory.
            // Resolving from `bin_dir` instead lands one level too shallow
            // and the bin silently survives the unlink.
            let link_parent = link.parent().unwrap_or(bin_dir);
            match std::fs::read_link(&link) {
                Ok(target) => {
                    let absolute = if target.is_absolute() {
                        target
                    } else {
                        link_parent.join(target)
                    };
                    // The literal target, checked before resolving: a bin
                    // whose path textually sits inside this install is ours
                    // even when canonicalizing escapes into the shared
                    // virtual store and `bin.target` was never captured.
                    let lex = aube_linker::normalize_path(&absolute);
                    match std::fs::canonicalize(&absolute) {
                        Ok(resolved) => {
                            let ours = bin.target.as_ref().is_some_and(|t| *t == resolved)
                                || lex.starts_with(&install_lex)
                                || install_canon
                                    .as_ref()
                                    .is_some_and(|canon| resolved.starts_with(canon));
                            if ours {
                                let _ = std::fs::remove_file(&link);
                            }
                        }
                        // An unresolvable link is dangling — its target tree is
                        // already gone. Reclaim it when the literal target sits
                        // inside this install, which is what clears the
                        // `<bin> -> global-<embedder>/<removed>` strays a
                        // previous leak left behind.
                        Err(_) => {
                            if lex.starts_with(&install_lex)
                                || install_canon
                                    .as_ref()
                                    .is_some_and(|canon| lex.starts_with(canon))
                            {
                                let _ = std::fs::remove_file(&link);
                            }
                        }
                    }
                }
                Err(_) => {
                    // Regular-file shim (`preferSymlinkedExecutables=false`):
                    // read the `# aube-bin-shim` marker line generated
                    // alongside the script body to recover the
                    // `$basedir`-relative target, then lex-normalize from
                    // the link's own directory to match the shim's
                    // string-level resolution semantics (`$basedir` is
                    // `dirname "$0"`). Canonicalizing here would
                    // follow the install's symlinks into the shared
                    // virtual store, so the ownership check has to
                    // stay textual.
                    let Some(content) = std::fs::read_to_string(&link).ok() else {
                        continue;
                    };
                    let Some(rel) = aube_linker::parse_posix_shim_target(&content) else {
                        continue;
                    };
                    let resolved = aube_linker::normalize_path(&link_parent.join(rel));
                    if resolved.starts_with(&install_lex)
                        || install_canon
                            .as_ref()
                            .is_some_and(|canon| resolved.starts_with(canon))
                    {
                        let _ = std::fs::remove_file(&link);
                    }
                }
            }
        }
    }
    #[cfg(windows)]
    {
        // On Windows, bins are cmd-shim wrapper scripts whose embedded target
        // is the surface path under `install_dir` (`relative_bin_target`), not
        // a store path — so containment alone still decides ownership here and
        // the store-resolution problem the Unix arm handles cannot arise.
        let Ok(install_canon) = std::fs::canonicalize(install_dir) else {
            return;
        };
        for bin in bins {
            let name = &bin.name;
            let cmd_path = bin_dir.join(format!("{name}.cmd"));
            let Ok(content) = std::fs::read_to_string(&cmd_path) else {
                continue;
            };
            // One parse, shared with the link path — see
            // `aube_linker::parse_win_shim_target`. Inlining a second copy here
            // is what let the reader drift from the writer before: this is the
            // DELETE path, so a stale parse removes the wrong binary or leaves
            // a live one behind, and the round-trip test would stay green.
            let owned = aube_linker::parse_win_shim_target(&content);
            if let Some(rel) = owned {
                let resolved = bin_dir.join(&rel);
                if let Ok(resolved) = std::fs::canonicalize(&resolved)
                    && !resolved.starts_with(&install_canon)
                {
                    continue; // owned by a different global install
                }
                // Remove if owned or target no longer exists (stale shim)
            }
            aube_linker::remove_bin_shim(bin_dir, name);
        }
    }
}

/// One bin an install owns: the name it occupies in `<bin_dir>`, plus the
/// canonical path that bin is expected to resolve to.
#[derive(Debug, Clone)]
pub struct OwnedBin {
    pub name: String,
    /// `None` when the target could not be canonicalized at capture time —
    /// [`unlink_bins`] then falls back to its containment tests.
    pub target: Option<PathBuf>,
}

/// Enumerate the bins every alias in an install dir owns, resolving each
/// target as it goes.
///
/// **Call this BEFORE mutating or deleting the install.** The target is the
/// only evidence of ownership that survives an isolated layout, and it can
/// only be read while the install is intact: `aube update -g` unlinks stale
/// bins *after* re-installing, by which point the manifest no longer lists
/// them and the old files are gone.
pub fn owned_bins(install_dir: &Path, aliases: &[String]) -> Vec<OwnedBin> {
    let modules = super::project_modules_dir(install_dir);
    let mut out = Vec::new();
    for alias in aliases {
        let pkg_dir = modules.join(alias);
        let manifest_path = pkg_dir.join("package.json");
        let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(bin_field) = json.get("bin") else {
            continue;
        };
        let bins: Vec<(String, String)> = match bin_field {
            serde_json::Value::String(rel) => {
                let name = alias.rsplit('/').next().unwrap_or(alias).to_string();
                vec![(name, rel.clone())]
            }
            serde_json::Value::Object(map) => map
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect(),
            _ => continue,
        };
        for (name, rel) in bins {
            out.push(OwnedBin {
                name,
                target: std::fs::canonicalize(pkg_dir.join(&rel)).ok(),
            });
        }
    }
    out
}

/// Delete a global package: remove its bins, its hash pointer, and the
/// physical install directory.
///
/// Both sides of the containment check are canonicalized. `info.install_dir`
/// already comes out of `scan_packages` in canonical form, but `layout.pkg_dir`
/// may still be in whatever shape `GlobalLayout::resolve()` produced (on
/// macOS that's typically an un-canonicalized `/var/folders/...` path
/// that's actually a symlink to `/private/var/folders/...`). Without
/// normalizing here, `starts_with` silently returns false and the
/// physical install dir leaks.
///
/// `keep_bins` names bins a CALLER has already re-linked to something it owns,
/// and it exists because `add -g` commits the new install before tearing down
/// the priors it replaces. Two installs of the same package at the same
/// version share one content-store path, so this package's recorded target
/// still matches the link the new install just wrote — without the exclusion,
/// dropping a prior would delete the live bin. Pass an empty set to remove
/// every bin this package owns.
pub fn remove_package(
    info: &GlobalPackageInfo,
    layout: &GlobalLayout,
    keep_bins: &std::collections::BTreeSet<&str>,
) -> miette::Result<()> {
    let bins: Vec<OwnedBin> = owned_bins(&info.install_dir, &info.aliases)
        .into_iter()
        .filter(|bin| !keep_bins.contains(bin.name.as_str()))
        .collect();
    unlink_bins(&info.install_dir, &layout.bin_dir, &bins);

    // Remove the hash pointer first. A missing pointer is fine (the
    // caller may have already cleaned it up), but permission denied or
    // similar means the package is still findable and we must not
    // report success. `super::remove_existing` handles the Windows
    // directory-junction case where `remove_file` fails with
    // `Access is denied`; we created the pointer via `create_dir_link`
    // (NTFS junction), so a plain `remove_file` here would leak it.
    let hash_ptr = hash_link(&layout.pkg_dir, &info.hash);
    super::remove_existing(&hash_ptr)?;

    // `crate::dirs::canonicalize` so `pkg_canon` is comparable with the
    // `info.install_dir` `scan_packages` produced — both must be in the
    // same Windows form (no `\\?\` prefix) or the `starts_with` check
    // fails and the install dir leaks.
    let pkg_canon =
        crate::dirs::canonicalize(&layout.pkg_dir).unwrap_or_else(|_| layout.pkg_dir.clone());
    if info.install_dir.starts_with(&pkg_canon) {
        match std::fs::remove_dir_all(&info.install_dir) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).into_diagnostic().wrap_err_with(|| {
                    format!(
                        "failed to remove install dir {}",
                        info.install_dir.display()
                    )
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_stable_across_alias_order() {
        let regs: BTreeMap<String, String> = [(
            "default".to_string(),
            "https://registry.npmjs.org/".to_string(),
        )]
        .into_iter()
        .collect();
        let a = cache_key(&["lodash".into(), "chalk".into()], &regs);
        let b = cache_key(&["chalk".into(), "lodash".into()], &regs);
        assert_eq!(a, b);
    }

    #[test]
    fn bin_dir_on_path_matches_a_listed_entry() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = std::env::join_paths(["/usr/bin".as_ref(), bin.as_os_str()]).unwrap();
        assert!(bin_dir_on_path(&bin, Some(&path)));
    }

    #[test]
    fn bin_dir_on_path_rejects_an_absent_entry() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = std::env::join_paths(["/usr/bin"]).unwrap();
        assert!(!bin_dir_on_path(&bin, Some(&path)));
    }

    /// An unset `PATH` means the bin is unreachable, so `add -g` must still
    /// warn — the check can't quietly pass because it has nothing to search.
    #[test]
    fn bin_dir_on_path_is_false_when_path_is_unset() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!bin_dir_on_path(dir.path(), None));
        assert!(!bin_dir_on_path(dir.path(), Some(std::ffi::OsStr::new(""))));
    }

    #[test]
    fn cache_key_changes_with_aliases() {
        let regs = BTreeMap::new();
        let a = cache_key(&["lodash".into()], &regs);
        let b = cache_key(&["chalk".into()], &regs);
        assert_ne!(a, b);
    }

    /// A scoped bin lives at `<bin_dir>/@scope/foo`, so `create_bin_shim`
    /// anchors its relative target one directory deeper than a bare name.
    /// Ownership detection has to undo that from the same anchor, or it
    /// resolves above the real target and leaves the bin behind on
    /// `remove -g`. Both link shapes carry a relative target, so both
    /// arms of the ownership check are exercised.
    #[cfg(unix)]
    #[test]
    fn unlink_bins_removes_a_scoped_bin_it_owns() {
        for prefer_symlink in [None, Some(false)] {
            let dir = tempfile::tempdir().unwrap();
            let install_dir = dir.path().join("install");
            let bin_dir = dir.path().join("bin");
            let pkg_bin = install_dir.join("node_modules/pkg/bin");
            std::fs::create_dir_all(&pkg_bin).unwrap();
            std::fs::create_dir_all(&bin_dir).unwrap();
            let target = pkg_bin.join("foo.js");
            std::fs::write(&target, b"#!/usr/bin/env node\n").unwrap();

            let names = ["bare".to_string(), "@scope/foo".to_string()];
            for name in &names {
                aube_linker::create_bin_shim(
                    &bin_dir,
                    name,
                    &target,
                    aube_linker::BinShimOptions {
                        prefer_symlinked_executables: prefer_symlink,
                        ..Default::default()
                    },
                )
                .unwrap();
            }

            let bins: Vec<OwnedBin> = names
                .iter()
                .map(|name| OwnedBin {
                    name: name.clone(),
                    target: std::fs::canonicalize(&target).ok(),
                })
                .collect();
            unlink_bins(&install_dir, &bin_dir, &bins);

            for name in &names {
                assert!(
                    bin_dir.join(name).symlink_metadata().is_err(),
                    "bin {name:?} is owned by this install and must be removed \
                     (prefer_symlinked_executables={prefer_symlink:?})"
                );
            }
        }
    }

    /// The layout a real isolated install produces: `node_modules/<alias>` is a
    /// symlink into a content store OUTSIDE the install dir, so the bin link
    /// resolves clean out of `install_dir`. Containment alone cannot recognise
    /// that bin as ours — which is what left every `remove -g` bin behind in
    /// the global bin dir as a dangling link.
    ///
    /// Ownership must come from the target `owned_bins` captured off this
    /// install's own manifest, so the assertion below is what fails when that
    /// evidence is dropped.
    #[cfg(unix)]
    #[test]
    fn unlink_bins_removes_a_bin_that_resolves_into_the_content_store() {
        for prefer_symlink in [None, Some(false)] {
            let dir = tempfile::tempdir().unwrap();
            let install_dir = dir.path().join("install");
            let bin_dir = dir.path().join("bin");
            let store_pkg = dir.path().join("store/pkg@1.0.0/node_modules/pkg");
            std::fs::create_dir_all(&store_pkg).unwrap();
            std::fs::create_dir_all(install_dir.join("node_modules")).unwrap();
            std::fs::create_dir_all(&bin_dir).unwrap();
            std::fs::write(store_pkg.join("cli.js"), b"#!/usr/bin/env node\n").unwrap();
            std::fs::write(
                store_pkg.join("package.json"),
                br#"{"name":"pkg","version":"1.0.0","bin":{"pkg":"cli.js"}}"#,
            )
            .unwrap();
            std::os::unix::fs::symlink(&store_pkg, install_dir.join("node_modules/pkg")).unwrap();

            let bins = owned_bins(&install_dir, &["pkg".to_string()]);
            assert_eq!(
                bins.len(),
                1,
                "owned_bins must read the manifest through the store symlink"
            );
            let install_canon = std::fs::canonicalize(&install_dir).unwrap();
            assert!(
                bins[0]
                    .target
                    .as_ref()
                    .is_some_and(|t| !t.starts_with(&install_canon)),
                "the fixture is only meaningful if the target resolves OUTSIDE \
                 install_dir — otherwise containment would pass on its own"
            );

            aube_linker::create_bin_shim(
                &bin_dir,
                "pkg",
                &install_dir.join("node_modules/pkg/cli.js"),
                aube_linker::BinShimOptions {
                    prefer_symlinked_executables: prefer_symlink,
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(
                bin_dir.join("pkg").symlink_metadata().is_ok(),
                "positive control: the bin must exist before we unlink it"
            );

            unlink_bins(&install_dir, &bin_dir, &bins);

            assert!(
                bin_dir.join("pkg").symlink_metadata().is_err(),
                "bin resolving into the content store is still ours and must be \
                 removed (prefer_symlinked_executables={prefer_symlink:?})"
            );
        }
    }

    /// The global bin dir is shared with every other tool that installs there,
    /// so an occupied slot is only ours to take when we can show we put it
    /// there. A foreign file must survive — silently replacing one is how a
    /// global install eats another tool's binary.
    ///
    /// The store-shape case is the one that needs the package scan: the link
    /// resolves out of `pkg_dir` into the content store, so it looks exactly as
    /// foreign as a stranger's symlink until it is matched against what the
    /// installs actually own.
    #[cfg(unix)]
    #[test]
    fn bin_slot_is_writable_only_when_the_occupant_is_ours() {
        let dir = tempfile::tempdir().unwrap();
        let pkg_dir = dir.path().join("global-aube");
        let bin_dir = dir.path().join("bin");
        let install_dir = pkg_dir.join("1234-abcd");
        let store_pkg = dir.path().join("store/pkg@1.0.0/node_modules/pkg");
        std::fs::create_dir_all(&store_pkg).unwrap();
        std::fs::create_dir_all(install_dir.join("node_modules")).unwrap();
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(store_pkg.join("cli.js"), b"#!/usr/bin/env node\n").unwrap();
        std::fs::write(
            store_pkg.join("package.json"),
            br#"{"name":"pkg","version":"1.0.0","bin":{"pkg":"cli.js"}}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(&store_pkg, install_dir.join("node_modules/pkg")).unwrap();
        // `scan_packages` only sees an install through its hash pointer, and
        // the manifest is where it reads the alias back.
        std::fs::write(
            install_dir.join("package.json"),
            br#"{"name":"aube-global","dependencies":{"pkg":"1.0.0"}}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(&install_dir, pkg_dir.join("deadbeef")).unwrap();

        assert!(
            bin_slot_is_writable(&bin_dir, &pkg_dir, "pkg"),
            "an empty slot is free"
        );

        std::fs::write(bin_dir.join("pkg"), b"#!/bin/sh\necho not ours\n").unwrap();
        assert!(
            !bin_slot_is_writable(&bin_dir, &pkg_dir, "pkg"),
            "a foreign regular file must not be overwritten"
        );

        std::fs::remove_file(bin_dir.join("pkg")).unwrap();
        aube_linker::create_bin_shim(
            &bin_dir,
            "pkg",
            &install_dir.join("node_modules/pkg/cli.js"),
            aube_linker::BinShimOptions::default(),
        )
        .unwrap();
        assert!(
            bin_slot_is_writable(&bin_dir, &pkg_dir, "pkg"),
            "our own prior link is ours to replace on a re-add"
        );
    }

    /// The same policy on EVERY platform, including the one it keeps breaking on.
    ///
    /// The store-shape test above needs `std::os::unix::fs::symlink` for its
    /// fixture, so `#[cfg(unix)]` compiles it away on the `windows-latest` leg of
    /// `.github/workflows/aube-parity.yml` — leaving the Windows arm of the guard
    /// with no test that composes it with a real occupant, which is how two
    /// consecutive Windows-only defects reached this function. Neither case here
    /// builds a symlink by hand — the writer makes whatever its platform uses —
    /// so both run everywhere.
    ///
    /// Each assertion pins one of those defects. Consulting only the
    /// extensionless path on Windows reads a foreign `pkg.cmd` slot as empty and
    /// fails the second; demanding every `win_shim_paths` entry prove itself
    /// rejects the shims this writer just emitted and fails the third.
    #[test]
    fn the_slot_policy_holds_for_plain_files_on_every_platform() {
        let dir = tempfile::tempdir().unwrap();
        let pkg_dir = dir.path().join("global-aube");
        let bin_dir = dir.path().join("bin");
        let install_dir = pkg_dir.join("1234-abcd/node_modules/pkg");
        std::fs::create_dir_all(&install_dir).unwrap();
        std::fs::create_dir_all(&bin_dir).unwrap();
        let target = install_dir.join("cli.js");
        std::fs::write(&target, b"#!/usr/bin/env node\n").unwrap();

        assert!(
            bin_slot_is_writable(&bin_dir, &pkg_dir, "pkg"),
            "an empty slot is free"
        );

        // A stranger's entry, in whichever path this platform actually consults.
        let foreign = if cfg!(windows) {
            bin_dir.join("pkg.cmd")
        } else {
            bin_dir.join("pkg")
        };
        std::fs::write(&foreign, b"@echo not ours\n").unwrap();
        assert!(
            !bin_slot_is_writable(&bin_dir, &pkg_dir, "pkg"),
            "a foreign file must not be overwritten"
        );
        std::fs::remove_file(&foreign).unwrap();

        // Ours, written by the production writer rather than a hand-built
        // string, so the guard is read against what actually gets emitted.
        aube_linker::create_bin_shim(
            &bin_dir,
            "pkg",
            &target,
            aube_linker::BinShimOptions::default(),
        )
        .unwrap();
        assert!(
            bin_slot_is_writable(&bin_dir, &pkg_dir, "pkg"),
            "a shim this writer just emitted is ours to replace on a re-add"
        );
    }

    /// `add -g` links the new install's bins BEFORE tearing down the priors it
    /// replaces. A prior holding the same package+version resolves to the same
    /// content-store path as the new one, so its recorded target matches the
    /// live link — and dropping it would delete the bin the user just
    /// installed. `keep_bins` is what prevents that.
    ///
    /// Both `keep_bins` values run against an identical fixture, so the pair is
    /// its own control: an ignored `keep_bins` fails the first case, and a
    /// broken ownership check fails the second. Either way the install dir must
    /// go — skipping a bin is not skipping the removal.
    #[cfg(unix)]
    #[test]
    fn remove_package_keeps_only_the_bins_the_replacing_install_relinked() {
        for keep in [
            ["pkg"]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::new(),
        ] {
            let survives = keep.contains("pkg");
            let dir = tempfile::tempdir().unwrap();
            let pkg_dir = dir.path().join("global-aube");
            let bin_dir = dir.path().join("bin");
            let install_dir = pkg_dir.join("1234-abcd");
            let store_pkg = dir.path().join("store/pkg@1.0.0/node_modules/pkg");
            std::fs::create_dir_all(&store_pkg).unwrap();
            std::fs::create_dir_all(install_dir.join("node_modules")).unwrap();
            std::fs::create_dir_all(&bin_dir).unwrap();
            std::fs::write(store_pkg.join("cli.js"), b"#!/usr/bin/env node\n").unwrap();
            std::fs::write(
                store_pkg.join("package.json"),
                br#"{"name":"pkg","version":"1.0.0","bin":{"pkg":"cli.js"}}"#,
            )
            .unwrap();
            std::os::unix::fs::symlink(&store_pkg, install_dir.join("node_modules/pkg")).unwrap();

            aube_linker::create_bin_shim(
                &bin_dir,
                "pkg",
                &install_dir.join("node_modules/pkg/cli.js"),
                aube_linker::BinShimOptions::default(),
            )
            .unwrap();

            let info = GlobalPackageInfo {
                hash: "deadbeef".to_string(),
                install_dir: std::fs::canonicalize(&install_dir).unwrap(),
                aliases: vec!["pkg".to_string()],
            };
            let layout = GlobalLayout {
                bin_dir: bin_dir.clone(),
                pkg_dir: pkg_dir.clone(),
            };
            remove_package(&info, &layout, &keep).unwrap();

            assert_eq!(
                bin_dir.join("pkg").symlink_metadata().is_ok(),
                survives,
                "with keep_bins={keep:?} the bin should {}",
                if survives { "survive" } else { "be removed" }
            );
            assert!(
                !install_dir.exists(),
                "the physical install dir must be removed either way \
                 (keep_bins={keep:?})"
            );
        }
    }

    /// A bin a LATER install took over keeps its new owner's target, so the
    /// earlier install must leave it alone. This is the property the ownership
    /// check exists to protect, and the one a blanket "remove by name" breaks.
    #[cfg(unix)]
    #[test]
    fn unlink_bins_leaves_a_bin_another_install_took_over() {
        let dir = tempfile::tempdir().unwrap();
        let ours = dir.path().join("ours");
        let theirs = dir.path().join("theirs");
        let bin_dir = dir.path().join("bin");
        for base in [&ours, &theirs] {
            std::fs::create_dir_all(base.join("node_modules/pkg")).unwrap();
            std::fs::write(base.join("node_modules/pkg/cli.js"), b"#!/bin/sh\n").unwrap();
        }
        std::fs::create_dir_all(&bin_dir).unwrap();

        // The link on disk belongs to `theirs`.
        aube_linker::create_bin_shim(
            &bin_dir,
            "pkg",
            &theirs.join("node_modules/pkg/cli.js"),
            aube_linker::BinShimOptions::default(),
        )
        .unwrap();

        let bins = vec![OwnedBin {
            name: "pkg".to_string(),
            target: std::fs::canonicalize(ours.join("node_modules/pkg/cli.js")).ok(),
        }];
        unlink_bins(&ours, &bin_dir, &bins);

        assert!(
            bin_dir.join("pkg").symlink_metadata().is_ok(),
            "the bin belongs to a later install and must survive"
        );
    }
}

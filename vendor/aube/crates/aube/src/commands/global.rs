//! Global install layout — `aube add -g`, `aube remove -g`, `aube list -g`.
//!
//! Modeled on pnpm v11's per-install-dir layout:
//!
//! ```text
//! <global_bin>/                    # on PATH; bins symlink into here
//! ├── some-bin        -> <pkg_dir>/<install>/node_modules/.bin/some-bin
//! └── global-aube/                 # <pkg_dir>: one subdir per global package
//!     ├── <pid>-<ts>/              # physical install dir (normal aube project)
//!     │   ├── package.json
//!     │   └── node_modules/
//!     └── <hash>           -> <pid>-<ts>  # stable pointer keyed on aliases
//! ```
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
/// `bin_dir` is the directory the user is expected to have on `$PATH` —
/// it's where bin symlinks live. `pkg_dir` is where the per-install
/// directories and hash pointers live; it's an aube-specific subdir so we
/// never step on a sibling pnpm install.
#[derive(Debug, Clone)]
pub struct GlobalLayout {
    pub bin_dir: PathBuf,
    pub pkg_dir: PathBuf,
}

impl GlobalLayout {
    pub fn resolve() -> miette::Result<Self> {
        let cwd = std::env::current_dir().unwrap_or_default();

        // `bin_dir` and `pkg_dir` are independent: `globalBinDir` controls
        // where bin symlinks go (on PATH), `globalDir` controls where
        // package installs live. Neither inherits from the other — both
        // fall back to the default home (<PREFIX>_HOME → PNPM_HOME → platform).
        let (setting_bin, setting_pkg) = super::with_settings_ctx(&cwd, |ctx| {
            let bin = aube_settings::resolved::global_bin_dir(ctx)
                .and_then(|raw| super::expand_setting_path(&raw, &cwd));
            let pkg = aube_settings::resolved::global_dir(ctx)
                .and_then(|raw| super::expand_setting_path(&raw, &cwd));
            (bin, pkg)
        });

        let bin_dir = setting_bin.map_or_else(resolve_home, Ok)?;
        // Package-install subdir named after the active embedder so we never
        // step on a sibling pnpm install. Standalone aube → `global-aube`.
        let pkg_subdir = format!("global-{}", aube_util::embedder().name);
        let pkg_dir = setting_pkg.map_or_else(
            || resolve_home().map(|h| h.join(&pkg_subdir)),
            |p| Ok(p.join(&pkg_subdir)),
        )?;

        Ok(Self { bin_dir, pkg_dir })
    }
}

/// Resolve the PATH-visible root. Honors the branded `<PREFIX>_HOME`
/// (standalone aube → `AUBE_HOME`), then `PNPM_HOME` (so existing pnpm users
/// already have the right dir on PATH), then a platform-specific pnpm-style
/// default. An embedder with no `env_prefix` skips the branded var.
fn resolve_home() -> miette::Result<PathBuf> {
    if let Some(prefix) = aube_util::embedder().env_prefix
        && let Ok(v) = std::env::var(format!("{prefix}_HOME"))
        && !v.is_empty()
    {
        return Ok(PathBuf::from(v));
    }
    if let Ok(v) = std::env::var("PNPM_HOME")
        && !v.is_empty()
    {
        return Ok(PathBuf::from(v));
    }
    platform_default()
}

/// Resolve the global prefix root. This is distinct from `globalBinDir`:
/// users may point global bin symlinks somewhere else while the prefix
/// itself still comes from `AUBE_HOME` / `PNPM_HOME` / the platform default.
pub fn prefix_dir() -> miette::Result<PathBuf> {
    resolve_home()
}

// Linux plus every other Unix (FreeBSD, Android/Termux, …): pnpm
// special-cases only macOS (`~/Library/pnpm`), while Windows has its own
// arm below. Scoped to `unix` so a non-Unix, non-Windows target doesn't
// silently inherit the XDG/HOME logic — it gets a compile error instead,
// which is the signal we'd want before shipping such a build.
#[cfg(all(unix, not(target_os = "macos")))]
fn platform_default() -> miette::Result<PathBuf> {
    if let Some(xdg) = aube_util::env::xdg_data_home() {
        return Ok(xdg.join("pnpm"));
    }
    let home = aube_util::env::home_dir()
        .ok_or_else(|| miette!("HOME is not set; can't locate global directory"))?;
    Ok(home.join(".local/share/pnpm"))
}

#[cfg(target_os = "macos")]
fn platform_default() -> miette::Result<PathBuf> {
    let home = std::env::var("HOME")
        .map_err(|_| miette!("HOME is not set; can't locate global directory"))?;
    Ok(PathBuf::from(home).join("Library/pnpm"))
}

#[cfg(target_os = "windows")]
fn platform_default() -> miette::Result<PathBuf> {
    let local = std::env::var("LOCALAPPDATA")
        .map_err(|_| miette!("LOCALAPPDATA is not set; can't locate global directory"))?;
    Ok(PathBuf::from(local).join("pnpm"))
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
    let link = bin_dir.join(name);
    let Ok(meta) = link.symlink_metadata() else {
        return true; // nothing there
    };
    let pkg_canon = std::fs::canonicalize(pkg_dir).unwrap_or_else(|_| pkg_dir.to_path_buf());

    if meta.file_type().is_symlink() {
        let Ok(raw) = std::fs::read_link(&link) else {
            return false;
        };
        let absolute = if raw.is_absolute() {
            raw
        } else {
            link.parent().unwrap_or(bin_dir).join(raw)
        };
        // Surface shape: the link points straight into the global pkg dir.
        if aube_linker::normalize_path(&absolute).starts_with(&pkg_canon) {
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
        // A regular file is one of our shims only when it carries the marker
        // `create_bin_shim` writes. Any other script in the slot belongs to
        // somebody else.
        match std::fs::read_to_string(&link) {
            Ok(content) => aube_linker::parse_posix_shim_target(&content).is_some(),
            Err(_) => false,
        }
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
                    match std::fs::canonicalize(&absolute) {
                        Ok(resolved) => {
                            let ours = bin.target.as_ref().is_some_and(|t| *t == resolved)
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
                            let lex = aube_linker::normalize_path(&absolute);
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
            // The .cmd shim embeds the target as `"%~dp0\<rel_path>"`.
            // Two shapes:
            //   - node shim: extract from the ELSE branch (`prog "%~dp0\
            //     <rel>" %*`), skipping the IF-branch `"%~dp0\node.exe"`.
            //   - direct-exec shim for a native bin (#394): the whole file
            //     is `@"%~dp0\<rel>" %*` — a line that STARTS with `@"%~dp0\`,
            //     which the node shim never produces. Its `<rel>` typically
            //     ends in `.exe`, so it must be matched by shape, not by the
            //     `.exe"` filter below (which would drop it and skip the
            //     ownership check, over-removing another install's bin).
            let owned = content
                .lines()
                .filter_map(|line| {
                    let line = line.trim();
                    if let Some(after) = line.strip_prefix("@\"%~dp0\\") {
                        let end = after.find('"')?;
                        return Some(after[..end].to_string());
                    }
                    // Match the fallback line: `prog "%~dp0\<path>" %*`
                    // Skip lines containing `.exe"` (those are the IF branch).
                    if line.contains("%~dp0\\") && !line.contains(".exe\"") {
                        let start = line.find("%~dp0\\")?;
                        let after = &line[start + 6..]; // skip `%~dp0\`
                        let end = after.find('"')?;
                        Some(after[..end].to_string())
                    } else {
                        None
                    }
                })
                .next();
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

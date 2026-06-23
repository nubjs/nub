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

#[cfg(target_os = "linux")]
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

/// After a global install lands, link each resolved dependency's bins
/// into `<bin_dir>`. Bins are extracted from each package's `package.json`
/// inside `<install_dir>/node_modules/<alias>/`. Returns the list of bin
/// names that were linked — callers use this list to undo the links on
/// `aube remove -g`.
pub fn link_bins(
    install_dir: &Path,
    bin_dir: &Path,
    aliases: &[String],
    shim_opts: aube_linker::BinShimOptions,
) -> miette::Result<Vec<String>> {
    std::fs::create_dir_all(bin_dir)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to create bin dir {}", bin_dir.display()))?;
    let modules = super::project_modules_dir(install_dir);
    let mut linked = Vec::new();
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
            let target = pkg_dir.join(&rel);
            aube_linker::create_bin_shim(bin_dir, &name, &target, shim_opts)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to create bin shim for {name}"))?;
            linked.push(name);
        }
    }
    Ok(linked)
}

/// Remove bin symlinks we own. Only unlinks entries whose symlink target
/// points inside `install_dir` — any bin that was overwritten by a later
/// `aube add -g` is owned by that later install, so we leave it alone.
///
/// Both the target and `install_dir` are canonicalized before the
/// `starts_with` check. On macOS, temp dirs like `/var/folders/...` are
/// actually symlinks to `/private/var/folders/...`; without canonicalizing
/// both sides the comparison always returns false and the bins leak.
pub fn unlink_bins(install_dir: &Path, bin_dir: &Path, bin_names: &[String]) {
    #[cfg(unix)]
    {
        let install_canon = std::fs::canonicalize(install_dir).ok();
        // Lex-normalized `install_dir` is the fallback ownership anchor
        // for regular-file shims (`preferSymlinkedExecutables=false`),
        // where we can't canonicalize the shim's `$basedir/<rel>` target
        // without following the project's symlinks into the shared
        // virtual store.
        let install_lex = aube_linker::normalize_path(install_dir);
        for name in bin_names {
            let link = bin_dir.join(name);
            match std::fs::read_link(&link) {
                Ok(target) => {
                    // Symlink bin: fully resolve and check against
                    // `install_canon`. Matches the pre-settings behavior.
                    let absolute = if target.is_absolute() {
                        target
                    } else {
                        bin_dir.join(target)
                    };
                    let Some(install_canon) = install_canon.as_ref() else {
                        continue;
                    };
                    let Some(resolved) = std::fs::canonicalize(&absolute).ok() else {
                        continue;
                    };
                    if resolved.starts_with(install_canon) {
                        let _ = std::fs::remove_file(&link);
                    }
                }
                Err(_) => {
                    // Regular-file shim (`preferSymlinkedExecutables=false`):
                    // read the `# aube-bin-shim` marker line generated
                    // alongside the script body to recover the
                    // `$basedir`-relative target, then lex-normalize from
                    // `bin_dir` to match the shim's string-level
                    // resolution semantics. Canonicalizing here would
                    // follow the install's symlinks into the shared
                    // virtual store, so the ownership check has to
                    // stay textual.
                    let Some(content) = std::fs::read_to_string(&link).ok() else {
                        continue;
                    };
                    let Some(rel) = aube_linker::parse_posix_shim_target(&content) else {
                        continue;
                    };
                    let resolved = aube_linker::normalize_path(&bin_dir.join(rel));
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
        // On Windows, bins are cmd-shim wrapper scripts. Parse the .cmd
        // shim to extract the embedded relative target path and verify
        // it resolves into install_dir before removing — same ownership
        // semantics as the Unix read_link check.
        let Ok(install_canon) = std::fs::canonicalize(install_dir) else {
            return;
        };
        for name in bin_names {
            let cmd_path = bin_dir.join(format!("{name}.cmd"));
            let Ok(content) = std::fs::read_to_string(&cmd_path) else {
                continue;
            };
            // The .cmd shim embeds the target as `"%~dp0\<rel_path>"`.
            // Extract the relative path from the ELSE branch (the one
            // without `.exe`), which looks like:
            //   prog "%~dp0\<rel_target>" %*
            let owned = content
                .lines()
                .filter_map(|line| {
                    let line = line.trim();
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

/// Enumerate bin names for every alias in an install dir. Used by the
/// remove path to know which symlinks to clean up.
pub fn bin_names_for(install_dir: &Path, aliases: &[String]) -> Vec<String> {
    let modules = super::project_modules_dir(install_dir);
    let mut out = Vec::new();
    for alias in aliases {
        let manifest_path = modules.join(alias).join("package.json");
        let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(bin_field) = json.get("bin") else {
            continue;
        };
        match bin_field {
            serde_json::Value::String(_) => {
                out.push(alias.rsplit('/').next().unwrap_or(alias).to_string());
            }
            serde_json::Value::Object(map) => {
                for name in map.keys() {
                    out.push(name.clone());
                }
            }
            _ => {}
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
pub fn remove_package(info: &GlobalPackageInfo, layout: &GlobalLayout) -> miette::Result<()> {
    let bins = bin_names_for(&info.install_dir, &info.aliases);
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
}

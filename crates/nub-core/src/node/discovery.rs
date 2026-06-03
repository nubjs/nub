//! Node binary discovery: pin-file walk-up, PATH probe, nvm scan.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use camino::Utf8PathBuf;
use thiserror::Error;

use super::version::{NodeVersion, VersionPin};

/// A resolved Node binary: its path on disk and its parsed version.
/// `pin_source` records where the version was pinned (e.g. `.nvmrc`,
/// `.node-version`) when discovery walked up and found a pin file; it
/// is `None` when the version came from the shell PATH alone, so the
/// hard-error message can reference the pin source cleanly when it's
/// known.
#[derive(Debug, Clone)]
pub struct ResolvedNode {
    pub path: Utf8PathBuf,
    pub version: NodeVersion,
    pub pin_source: Option<String>,
}

impl ResolvedNode {
    pub fn fallback() -> Self {
        Self {
            path: Utf8PathBuf::from("node"),
            version: NodeVersion::new(22, 15, 0),
            pin_source: None,
        }
    }
}

#[derive(Error, Debug)]
pub enum DiscoveryError {
    #[error(
        "pinned Node version {pin} not found\n\
         \x20\x20Active shell Node: {shell_version} (does not satisfy pin)\n\
         \x20\x20Install with: nvm install {pin}\n\
         \x20\x20Or run in compat mode: nub run --node <script>"
    )]
    PinnedNotFound { pin: String, shell_version: String },

    #[error("no Node binary found on PATH")]
    NoNodeOnPath,

    /// The discovered Node is older than `NodeVersion::MIN_SUPPORTED`
    /// (18.19.0). No hook API exists below this floor that can carry
    /// Nub's feature surface, so Nub refuses to run. Canonical wording
    /// per `wiki/research/supported-node-versions.md` line 52.
    /// Replaces the prior `TooOld` variant, which gated on the 22.15
    /// fast-path floor — that boundary is now a tier classifier
    /// (sync vs. async hook registration), not an error.
    #[error("{}", format_unsupported(.version, .pin_source.as_deref()))]
    Unsupported {
        version: NodeVersion,
        pin_source: Option<String>,
    },

    #[error("failed to detect Node version: {0}")]
    VersionDetection(String),

    /// A pinned version wasn't on PATH / in nub's store / in nvm, and the
    /// download+install from nodejs.org failed. Names the version + pin source +
    /// the underlying reason so the user can act (network/proxy, or pre-install).
    #[error(
        "failed to provision Node {version} (pinned via {pin_source}): {reason}\n\
         \x20\x20Check your network / proxy, or pre-install Node {version} so it's on PATH."
    )]
    ProvisionFailed {
        version: String,
        pin_source: String,
        reason: String,
    },
}

/// Format the `Unsupported` error text. Centralized so the canonical
/// wording (per `wiki/research/supported-node-versions.md` line 52)
/// lives in one place; tests pin to the output of this function.
fn format_unsupported(version: &NodeVersion, pin_source: Option<&str>) -> String {
    match pin_source {
        Some(src) => format!(
            "Nub requires Node 18.19 or newer for runtime augmentation. \
             This project pins Node {version} via {src}. \
             To run it: update the pin to 18.19+ (Nub will run it in compatibility mode), \
             or run plain `node` directly for this project."
        ),
        None => "Nub requires Node 18.19 or newer for runtime augmentation. \
             To run it: upgrade Node to 18.19+ (Nub will run it in compatibility mode), \
             or run plain `node` directly for this project."
            .to_string(),
    }
}

/// Discover the Node binary to use, following the algorithm in
/// `wiki/runtime/node-version-discovery.md`.
///
/// 1. Walk up from `cwd` looking for `.node-version` / `.nvmrc`.
/// 2. If no pin: use `node` on PATH.
/// 3. If pinned: PATH node satisfies → nub's own download store
///    (`~/.cache/nub/node/<version>/`) → nvm scan → error. (The download +
///    install step that populates the store, replacing the error, is the next
///    provisioning sub-item — see `wiki/runtime/node-version-management.md`.)
///
/// The hard floor (Node 18.19) is **not** enforced here — call
/// [`check_min_version`] afterwards. Discovery deliberately stays
/// floor-agnostic so callers like `nub --version` (which only need
/// the binary path) don't trip the version gate.
pub fn discover_node(cwd: &Path) -> Result<ResolvedNode, DiscoveryError> {
    // NODE_EXECUTABLE — the sole version-management override surface
    // (node-version-management.md). An absolute path bypasses pin-file reading,
    // cache, nvm, and download: use that binary directly. Its version is still
    // detected, so the floor check + tier dispatch apply (a Node-16 NODE_EXECUTABLE
    // hard-errors exactly like a Node-16 pin). Brand-compliant: Node doesn't claim
    // the NODE_EXECUTABLE name, so piggybacking on NODE_* is the prescribed hatch.
    if let Some(node) = node_executable_override()? {
        return Ok(node);
    }

    let pin = walk_up_for_pin(cwd);

    match pin {
        None => {
            // No pin file — use whatever node is on PATH.
            shell_path_node(None)
        }
        Some((pin_str, parsed_pin, pin_source)) => {
            // Try shell PATH first (covers fnm/Volta/mise auto-switch).
            if let Ok(node) = shell_path_node(Some(pin_source.clone())) {
                if node.version.satisfies(&parsed_pin) {
                    return Ok(node);
                }
                // PATH node doesn't satisfy — try nub's own download store, then nvm.
                if let Some(mut node) = nub_store_node(&parsed_pin) {
                    node.pin_source = Some(pin_source.clone());
                    return Ok(node);
                }
                if let Some(mut node) = scan_nvm(&parsed_pin) {
                    node.pin_source = Some(pin_source);
                    return Ok(node);
                }
                return Err(DiscoveryError::PinnedNotFound {
                    pin: pin_str,
                    shell_version: format!("v{}", node.version),
                });
            }
            // No node on PATH at all — try nub's own store, then nvm.
            if let Some(mut node) = nub_store_node(&parsed_pin) {
                node.pin_source = Some(pin_source.clone());
                return Ok(node);
            }
            if let Some(mut node) = scan_nvm(&parsed_pin) {
                node.pin_source = Some(pin_source);
                return Ok(node);
            }
            Err(DiscoveryError::NoNodeOnPath)
        }
    }
}

/// [`discover_node`], but when a pinned version can't be satisfied from PATH /
/// nub's store / nvm, DOWNLOAD + install it from nodejs.org (uv-style, silent)
/// and use it. This is the provisioning fire point — call it ONLY from
/// `nub <file>` and the hijack-descendant `node` handler, never from
/// `nub run` / `nub exec` (which keep plain [`discover_node`]), per
/// `wiki/runtime/node-version-management.md` §"Where the version logic fires".
///
/// Exact pins provision the named version directly; range pins (`22`, `22.13`)
/// and aliases (`latest`, `lts`, `lts/<codename>`) resolve to a concrete version
/// against nodejs.org's `index.json` (cached) first. (`rc/<major>` lives on a
/// different mirror — not yet resolved; it surfaces a clear ProvisionFailed.)
pub fn discover_or_provision_node(cwd: &Path) -> Result<ResolvedNode, DiscoveryError> {
    // Fast path: PATH / nub's store / nvm already satisfy the pin (or there's no
    // pin). Aliases never satisfy a concrete check, so they always fall through.
    let discover_err = match discover_node(cwd) {
        Ok(node) => return Ok(node),
        Err(e @ (DiscoveryError::PinnedNotFound { .. } | DiscoveryError::NoNodeOnPath)) => e,
        Err(other) => return Err(other),
    };
    let Some((raw, pin, pin_source)) = walk_up_for_pin(cwd) else {
        return Err(discover_err); // no pin → nothing to provision
    };

    let fail = |reason: String| DiscoveryError::ProvisionFailed {
        version: raw.clone(),
        pin_source: pin_source.clone(),
        reason,
    };
    let host = crate::version_management::HostTarget::detect()
        .ok_or_else(|| fail("this host is not a platform nodejs.org publishes".to_string()))?;
    let store_root = cache_dir()
        .ok_or_else(|| fail("could not locate a cache directory (no $HOME / $XDG_CACHE_HOME)".to_string()))?;

    // Resolve to a concrete version. Exact is already concrete; everything else
    // resolves against the (cached) dist index.
    let concrete = match &pin {
        VersionPin::Exact(version) => version.clone(),
        _ => {
            let mirror = crate::version_management::resolve_mirror_base(&host);
            let index = crate::version_management::node_index::load_index(&store_root, &mirror)
                .map_err(|e| fail(format!("could not fetch the Node release index: {e:#}")))?;
            crate::version_management::node_index::resolve_spec(&raw, &index)
                .ok_or_else(|| fail("no published Node version matches this pin".to_string()))?
        }
    };

    // The resolved concrete may already be on PATH or in nub's store (e.g. an
    // alias that resolved to the active version) — use it without downloading.
    let concrete_pin = VersionPin::Exact(concrete.clone());
    if let Some(mut node) = nub_store_node(&concrete_pin) {
        node.pin_source = Some(pin_source);
        return Ok(node);
    }
    if let Ok(node) = shell_path_node(Some(pin_source.clone())) {
        if node.version == concrete {
            return Ok(node);
        }
    }

    // Download + install it.
    let version_dir = crate::version_management::provision_node(&concrete, &host, &store_root)
        .map_err(|e| fail(format!("{e:#}")))?;
    let bin = store_node_binary(&version_dir)
        .ok_or_else(|| fail("installed, but no node binary was found in the extracted tree".to_string()))?;
    Ok(ResolvedNode {
        path: bin,
        version: concrete,
        pin_source: Some(pin_source),
    })
}

/// Enforce the hard floor: Node 18.19.0. Below that, Nub cannot
/// deliver its feature surface (no hook API capable of carrying
/// it exists pre-18.19; see
/// `wiki/research/supported-node-versions.md`). At or above 18.19,
/// the spawn path proceeds and the JS preload picks the
/// hook-registration shape based on the version tier (sync
/// `registerHooks` at 22.15+, async `register()` at 18.19-22.14).
///
/// Name kept as `check_min_version` to minimize churn at call sites;
/// the semantics changed (floor moved from 22.15 to 18.19) but the
/// shape and signature did not.
pub fn check_min_version(node: &ResolvedNode) -> Result<(), DiscoveryError> {
    if node.version.is_supported() {
        Ok(())
    } else {
        Err(DiscoveryError::Unsupported {
            version: node.version.clone(),
            pin_source: node.pin_source.clone(),
        })
    }
}

/// Walk up from `cwd` looking for a pin file. Returns the raw pin string, parsed
/// pin, and the filename that produced it (`.node-version` or `.nvmrc`) for
/// user-facing messages. Bounded by $HOME, filesystem root, and 16 ancestors.
///
/// Precedence within a directory is `.node-version` BEFORE `.nvmrc`, per
/// `wiki/runtime/node-version-management.md` §"Resolution order" (1. `.node-version`,
/// 2. `.nvmrc`). `.node-version` is the tool-agnostic standard, so it wins when a
/// project carries both. (`package.json#engines.node` is precedence #3 — handled
/// separately via [`engines_disagreement_warning`]; an `engines.node`-only
/// resolution still routes through the download path, not yet wired.)
pub fn walk_up_for_pin(cwd: &Path) -> Option<(String, VersionPin, String)> {
    let home = dirs_next::home_dir();
    let mut dir = cwd.to_path_buf();
    let max_depth = 16;

    for _ in 0..max_depth {
        for filename in &[".node-version", ".nvmrc"] {
            let pin_path = dir.join(filename);
            if let Ok(content) = fs::read_to_string(&pin_path) {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    if let Ok(pin) = trimmed.parse::<VersionPin>() {
                        tracing::debug!(path = %pin_path.display(), pin = trimmed, "found pin file");
                        return Some((trimmed.to_string(), pin, (*filename).to_string()));
                    }
                    tracing::debug!(
                        path = %pin_path.display(),
                        content = trimmed,
                        "pin file found but unparseable — skipping"
                    );
                }
            }
        }

        // Stop at home dir or filesystem root.
        if home.as_deref() == Some(&dir) || !dir.pop() {
            break;
        }
    }

    None
}

/// Read `package.json#engines.node` (precedence #3, a semver *range*) from the
/// nearest `package.json` walking up from `cwd`. Returns `(range, source_label)`,
/// or `None` when the nearest `package.json` has no `engines.node`. The walk stops
/// at the first `package.json` found — that is the project boundary; an
/// `engines.node` in a grandparent belongs to a different project.
fn read_engines_node(cwd: &Path) -> Option<(String, String)> {
    let home = dirs_next::home_dir();
    let mut dir = cwd.to_path_buf();

    for _ in 0..16 {
        let pkg_path = dir.join("package.json");
        if let Ok(content) = fs::read_to_string(&pkg_path) {
            // Nearest package.json = project boundary; whether or not it carries
            // engines.node, we don't look past it.
            let range = serde_json::from_str::<serde_json::Value>(&content)
                .ok()
                .as_ref()
                .and_then(|json| json.get("engines"))
                .and_then(|engines| engines.get("node"))
                .and_then(|node| node.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            return range.map(|r| (r, "package.json#engines.node".to_string()));
        }
        if home.as_deref() == Some(&dir) || !dir.pop() {
            break;
        }
    }

    None
}

/// When a project carries BOTH a pin file (`.node-version`/`.nvmrc`) and a
/// `package.json#engines.node` range, and the pinned version does NOT satisfy
/// that range, return a warning naming both sources — a project misconfiguration
/// the user should see (`wiki/runtime/node-version-management.md`: "If a pin file
/// and `engines.node` disagree, warn"). Returns `None` when there is no pin file,
/// no `engines.node`, the range is unparseable (be conservative — don't cry wolf
/// on node-semver syntax the `semver` crate can't model), or they agree.
///
/// `node` is the already-resolved result of [`discover_node`]; its `version` IS
/// the pinned version when `pin_source` is set, so no re-resolution is needed.
pub fn engines_disagreement_warning(cwd: &Path, node: &ResolvedNode) -> Option<String> {
    // Only a pin-file resolution can "disagree" with engines — an engines-only
    // project has nothing to contradict.
    let pin_source = node.pin_source.as_deref()?;
    let (range, engines_source) = read_engines_node(cwd)?;
    let req = semver::VersionReq::parse(&range).ok()?;
    if req.matches(&node.version.0) {
        return None;
    }
    Some(format!(
        "Warning: Node {} is pinned via {pin_source}, but {engines_source} requires \"{range}\". \
         The pin wins; update the pin or the engines range so they agree.",
        node.version
    ))
}

/// Resolve `node` from the shell PATH and detect its version.
/// `pin_source` is threaded through so the resulting `ResolvedNode`
/// carries the pin filename when one was found by the walk-up.
fn shell_path_node(pin_source: Option<String>) -> Result<ResolvedNode, DiscoveryError> {
    let node_path = which_node()?;
    let version = detect_version(&node_path)?;
    let utf8_path = Utf8PathBuf::try_from(node_path)
        .map_err(|e| DiscoveryError::VersionDetection(e.to_string()))?;
    Ok(ResolvedNode {
        path: utf8_path,
        version,
        pin_source,
    })
}

/// Find `node` on PATH, skipping nub's own PATH shim directories.
fn which_node() -> Result<PathBuf, DiscoveryError> {
    let path_var = env::var_os("PATH").unwrap_or_default();

    for dir in env::split_paths(&path_var) {
        // Skip our own PATH shim directories.
        if let Some(name) = dir.file_name() {
            if name.to_string_lossy().starts_with("nub-node-shim-") {
                continue;
            }
        }

        let candidate = dir.join("node");
        if candidate.is_file() {
            return Ok(candidate);
        }
        #[cfg(windows)]
        {
            let candidate_exe = dir.join("node.exe");
            if candidate_exe.is_file() {
                return Ok(candidate_exe);
            }
        }
    }
    Err(DiscoveryError::NoNodeOnPath)
}

/// Run `node --version` and parse the output, with a disk cache
/// keyed on the binary's path + mtime to avoid spawning on repeat calls.
fn detect_version(node_path: &Path) -> Result<NodeVersion, DiscoveryError> {
    if let Some(cached) = read_version_cache(node_path) {
        return Ok(cached);
    }

    let output = Command::new(node_path)
        .arg("--version")
        .output()
        .map_err(|e| DiscoveryError::VersionDetection(format!("{node_path:?}: {e}")))?;

    if !output.status.success() {
        return Err(DiscoveryError::VersionDetection(format!(
            "{node_path:?} --version exited with {}",
            output.status
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let version = stdout
        .trim()
        .parse::<NodeVersion>()
        .map_err(|e| DiscoveryError::VersionDetection(e.to_string()))?;

    write_version_cache(node_path, &version);
    Ok(version)
}

/// Resolve the `NODE_EXECUTABLE` override, if set. Split from the env read so the
/// resolution is unit-testable without mutating the process environment.
fn node_executable_from(raw: Option<std::ffi::OsString>) -> Result<Option<ResolvedNode>, DiscoveryError> {
    let Some(raw) = raw else { return Ok(None) };
    if raw.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(raw);
    // Detect the version (spawns `<path> --version`, mtime-cached). A bad path /
    // non-Node binary surfaces a clear VersionDetection error.
    let version = detect_version(&path)?;
    let utf8_path = Utf8PathBuf::try_from(path)
        .map_err(|e| DiscoveryError::VersionDetection(e.to_string()))?;
    Ok(Some(ResolvedNode {
        path: utf8_path,
        version,
        // Name the override as the source so the floor error attributes it.
        pin_source: Some("NODE_EXECUTABLE".to_string()),
    }))
}

fn node_executable_override() -> Result<Option<ResolvedNode>, DiscoveryError> {
    node_executable_from(env::var_os("NODE_EXECUTABLE"))
}

/// nub's cache root (`$XDG_CACHE_HOME/nub` or `~/.cache/nub`). Public so the
/// `nub node` command group can locate the store + index cache without
/// reimplementing the path logic.
pub fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var("XDG_CACHE_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| dirs_next::home_dir().map(|h| h.join(".cache")))?;
    Some(base.join("nub"))
}

/// nub's own Node download store (`<cache_dir>/node/`), where each subdirectory
/// name IS the concrete installed version. Public for the `nub node` command
/// group (`ls`/`uninstall`/`install` all key off this dir).
pub fn node_store_dir() -> Option<PathBuf> {
    Some(cache_dir()?.join("node"))
}

fn read_version_cache(node_path: &Path) -> Option<NodeVersion> {
    let cache = cache_dir()?.join("node-discovery.json");
    let content = fs::read_to_string(&cache).ok()?;
    let data: serde_json::Value = serde_json::from_str(&content).ok()?;
    let key = node_path.to_string_lossy();
    let entry = data.get(key.as_ref())?;
    let cached_mtime = entry.get("mtime")?.as_u64()?;
    let cached_version = entry.get("version")?.as_str()?;

    let actual_mtime = fs::metadata(node_path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();

    if cached_mtime == actual_mtime {
        cached_version.parse().ok()
    } else {
        None
    }
}

fn write_version_cache(node_path: &Path, version: &NodeVersion) {
    let Some(dir) = cache_dir() else { return };
    let _ = fs::create_dir_all(&dir);
    let cache = dir.join("node-discovery.json");

    let mut data: serde_json::Value = fs::read_to_string(&cache)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let mtime = fs::metadata(node_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let key = node_path.to_string_lossy().to_string();
    data[key] = serde_json::json!({
        "version": version.to_string(),
        "mtime": mtime,
    });

    let _ = fs::write(
        &cache,
        serde_json::to_string_pretty(&data).unwrap_or_default(),
    );
}

/// Scan the nvm install directory for a version matching the pin.
fn scan_nvm(pin: &VersionPin) -> Option<ResolvedNode> {
    let nvm_dir = nvm_dir()?;
    let versions_dir = nvm_dir.join("versions").join("node");

    let entries = fs::read_dir(&versions_dir).ok()?;
    let mut candidates: Vec<(NodeVersion, PathBuf)> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name();
            let name_str = name.to_str()?;
            let version = name_str.parse::<NodeVersion>().ok()?;
            let bin = entry.path().join("bin").join("node");
            if bin.is_file() {
                Some((version, bin))
            } else {
                None
            }
        })
        .filter(|(v, _)| v.satisfies(pin))
        .collect();

    // Pick the highest matching version.
    candidates.sort_by_key(|c| std::cmp::Reverse(c.0.clone()));

    candidates.into_iter().next().and_then(|(version, path)| {
        let utf8_path = Utf8PathBuf::try_from(path).ok()?;
        Some(ResolvedNode {
            path: utf8_path,
            version,
            // Caller (`discover_node`) overwrites this with the pin
            // filename when it had one; left `None` here so this
            // helper stays usable in isolation.
            pin_source: None,
        })
    })
}

/// The `node` binary inside one of nub's stock-dist version directories:
/// `bin/node` on unix, `node.exe` at the dir root on Windows (the layout
/// `nodejs.org/dist` tarballs extract to).
fn store_node_binary(version_dir: &Path) -> Option<Utf8PathBuf> {
    [version_dir.join("bin").join("node"), version_dir.join("node.exe")]
        .into_iter()
        .find(|p| p.is_file())
        .and_then(|p| Utf8PathBuf::try_from(p).ok())
}

/// Look up a Node satisfying `pin` in nub's own download store
/// (`~/.cache/nub/node/<version>/`, where the directory name IS the concrete
/// version — `wiki/runtime/node-version-management.md` §"State 1: Cache hit").
/// On a hit the spawn is silent (no notice). Returns the highest cached version
/// satisfying the pin. Parameterized over `store` so it's testable without
/// mutating the process env (XDG_CACHE_HOME); `nub_store_node` is the wrapper.
fn nub_store_node_in(store: &Path, pin: &VersionPin) -> Option<ResolvedNode> {
    let mut candidates: Vec<(NodeVersion, Utf8PathBuf)> = fs::read_dir(store)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let version = entry.file_name().to_str()?.parse::<NodeVersion>().ok()?;
            let bin = store_node_binary(&entry.path())?;
            Some((version, bin))
        })
        .filter(|(v, _)| v.satisfies(pin))
        .collect();

    // Highest matching version wins (mirrors scan_nvm).
    candidates.sort_by_key(|c| std::cmp::Reverse(c.0.clone()));
    candidates
        .into_iter()
        .next()
        .map(|(version, path)| ResolvedNode {
            path,
            version,
            // Caller overwrites with the pin filename; left None for isolation.
            pin_source: None,
        })
}

/// `nub_store_node_in` against nub's real store at `~/.cache/nub/node/`.
fn nub_store_node(pin: &VersionPin) -> Option<ResolvedNode> {
    nub_store_node_in(&cache_dir()?.join("node"), pin)
}

/// Resolve the nvm install directory.
fn nvm_dir() -> Option<PathBuf> {
    // $NVM_DIR if set, otherwise ~/.nvm
    if let Some(dir) = env::var_os("NVM_DIR") {
        let path = PathBuf::from(dir);
        if path.is_dir() {
            return Some(path);
        }
    }
    let home = dirs_next::home_dir()?;
    let default = home.join(".nvm");
    if default.is_dir() {
        Some(default)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_node_finds_something() {
        // This test requires node on PATH. Skip gracefully if not present.
        match which_node() {
            Ok(path) => assert!(path.is_file()),
            Err(DiscoveryError::NoNodeOnPath) => {
                eprintln!("skipping: no node on PATH");
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn detect_version_works() {
        if let Ok(path) = which_node() {
            let version = detect_version(&path).unwrap();
            assert!(version.major() >= 18, "expected Node 18+, got {version}");
        }
    }

    #[test]
    fn walk_up_returns_none_for_tmp() {
        // /tmp typically has no .nvmrc
        let pin = walk_up_for_pin(Path::new("/tmp"));
        assert!(pin.is_none());
    }

    /// A unique temp dir for resolution tests (no tempfile dev-dep). Created under
    /// the system temp dir, which is NOT under $HOME on macOS (/var/folders) or
    /// Linux (/tmp), so the walk-up can't reach a stray pin file up the tree — and
    /// the test files live directly in `dir`, found before any walk.
    fn resolution_tmpdir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nub-disc-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn node_version_file_wins_over_nvmrc() {
        // Spec precedence (node-version-management.md §"Resolution order"):
        // .node-version (#1) beats .nvmrc (#2) in the same directory.
        let dir = resolution_tmpdir("prec");
        std::fs::write(dir.join(".node-version"), "20.11.0\n").unwrap();
        std::fs::write(dir.join(".nvmrc"), "18.19.0\n").unwrap();
        let (raw, _pin, source) = walk_up_for_pin(&dir).expect("a pin file");
        assert_eq!(source, ".node-version", ".node-version must win over .nvmrc");
        assert_eq!(raw, "20.11.0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_engines_node_from_nearest_package_json() {
        let dir = resolution_tmpdir("eng");
        std::fs::write(dir.join("package.json"), r#"{"engines":{"node":">=20"}}"#).unwrap();
        let (range, source) = read_engines_node(&dir).expect("engines.node range");
        assert_eq!(range, ">=20");
        assert!(source.contains("engines.node"), "source label names engines.node: {source}");
        // A package.json without engines.node is the project boundary → None, not a
        // walk into ancestors.
        let dir2 = resolution_tmpdir("noeng");
        std::fs::write(dir2.join("package.json"), r#"{"name":"x"}"#).unwrap();
        assert!(read_engines_node(&dir2).is_none());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn engines_disagreement_warns_when_pin_violates_engines() {
        let dir = resolution_tmpdir("disagree");
        std::fs::write(dir.join("package.json"), r#"{"engines":{"node":">=20"}}"#).unwrap();
        let node = ResolvedNode {
            path: Utf8PathBuf::from("/x/node"),
            version: NodeVersion::new(18, 19, 0),
            pin_source: Some(".nvmrc".to_string()),
        };
        let warning = engines_disagreement_warning(&dir, &node).expect("a disagreement warning");
        assert!(
            warning.contains("18.19.0") && warning.contains(".nvmrc") && warning.contains(">=20"),
            "warning must name the pinned version, the pin source, and the engines range: {warning}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn node_executable_override_uses_the_given_binary() {
        // Use whatever real Node is on PATH as the override target.
        let Ok(node_path) = which_node() else {
            eprintln!("skipping: no node on PATH");
            return;
        };
        let resolved = node_executable_from(Some(node_path.clone().into_os_string()))
            .unwrap()
            .expect("an explicit NODE_EXECUTABLE resolves to that binary");
        assert_eq!(resolved.pin_source.as_deref(), Some("NODE_EXECUTABLE"));
        assert_eq!(resolved.path.as_std_path(), node_path.as_path());
        assert!(resolved.version.major() >= 18);
        // Unset / empty → no override (falls through to normal resolution).
        assert!(node_executable_from(None).unwrap().is_none());
        assert!(node_executable_from(Some(std::ffi::OsString::new())).unwrap().is_none());
        // A bad path is a clear error, not a silent fall-through.
        assert!(node_executable_from(Some("/no/such/node".into())).is_err());
    }

    #[test]
    fn nub_store_finds_highest_satisfying_cached_version() {
        // nub's store layout: ~/.cache/nub/node/<version>/bin/node (dir = version).
        let store = resolution_tmpdir("store");
        for v in ["20.11.0", "22.13.0", "22.15.0"] {
            let bin = store.join(v).join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join("node"), "").unwrap();
        }
        // Exact pin → that exact cached version.
        let exact = nub_store_node_in(&store, &"22.13.0".parse::<VersionPin>().unwrap())
            .expect("cached 22.13.0");
        assert_eq!(exact.version, NodeVersion::new(22, 13, 0));
        assert!(exact.path.as_str().contains("22.13.0"));
        // Range pin (major 22) → highest matching cached version.
        let major = nub_store_node_in(&store, &"22".parse::<VersionPin>().unwrap())
            .expect("a cached 22.x");
        assert_eq!(major.version, NodeVersion::new(22, 15, 0), "highest matching wins");
        // Not cached → None (falls through to nvm / download).
        assert!(nub_store_node_in(&store, &"18.19.0".parse::<VersionPin>().unwrap()).is_none());
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn engines_disagreement_silent_when_satisfied_or_unpinned() {
        let dir = resolution_tmpdir("agree");
        std::fs::write(dir.join("package.json"), r#"{"engines":{"node":">=18"}}"#).unwrap();
        // Pin satisfies the range → no warning.
        let satisfied = ResolvedNode {
            path: Utf8PathBuf::from("/x/node"),
            version: NodeVersion::new(20, 11, 0),
            pin_source: Some(".node-version".to_string()),
        };
        assert!(engines_disagreement_warning(&dir, &satisfied).is_none());
        // No pin file (PATH-resolved) — engines alone has nothing to disagree with.
        let unpinned = ResolvedNode {
            path: Utf8PathBuf::from("/x/node"),
            version: NodeVersion::new(16, 0, 0),
            pin_source: None,
        };
        assert!(engines_disagreement_warning(&dir, &unpinned).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsupported_error_with_pin_source_matches_canonical_wording() {
        // Canonical wording per the v0.1-anneal binding brief
        // (and wiki/research/supported-node-versions.md). Exact-string
        // assertion — any rewording must update this test deliberately.
        let err = DiscoveryError::Unsupported {
            version: NodeVersion::new(16, 10, 0),
            pin_source: Some(".nvmrc".to_string()),
        };
        let msg = format!("{err}");
        let expected = "Nub requires Node 18.19 or newer for runtime augmentation. \
                        This project pins Node 16.10.0 via .nvmrc. \
                        To run it: update the pin to 18.19+ (Nub will run it in compatibility mode), \
                        or run plain `node` directly for this project.";
        assert_eq!(msg, expected);
    }

    #[test]
    fn unsupported_error_without_pin_source_omits_project_clause() {
        // When deferring to whatever Node is on PATH (no pin file
        // discovered), the message must NOT claim the project is using
        // any particular Node — the project hasn't said anything about
        // Node version, so the message should just state the
        // requirement + the user's action. Colin 2026-05-29.
        let err = DiscoveryError::Unsupported {
            version: NodeVersion::new(18, 18, 2),
            pin_source: None,
        };
        let msg = format!("{err}");
        let expected = "Nub requires Node 18.19 or newer for runtime augmentation. \
                        To run it: upgrade Node to 18.19+ (Nub will run it in compatibility mode), \
                        or run plain `node` directly for this project.";
        assert_eq!(msg, expected);
        assert!(!msg.contains("This project"));
        assert!(!msg.contains(" via "));
    }

    #[test]
    fn provision_failed_error_names_version_source_reason_and_suggestion() {
        // The graceful-failure contract (Plumbing): a pin that can't be fetched
        // must name the version, the pin source, the underlying reason, and offer
        // a way forward.
        let err = DiscoveryError::ProvisionFailed {
            version: "22.99.99".to_string(),
            pin_source: ".node-version".to_string(),
            reason: "HTTP status client error (404 Not Found)".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("22.99.99"), "names the version: {msg}");
        assert!(msg.contains(".node-version"), "names the pin source: {msg}");
        assert!(msg.contains("404 Not Found"), "includes the reason: {msg}");
        assert!(msg.contains("pre-install"), "offers a way forward: {msg}");
    }

    #[test]
    fn check_min_version_accepts_18_19() {
        let node = ResolvedNode {
            path: Utf8PathBuf::from("/usr/bin/node"),
            version: NodeVersion::new(18, 19, 0),
            pin_source: None,
        };
        assert!(check_min_version(&node).is_ok());
    }

    #[test]
    fn check_min_version_accepts_22_14_compat_tier() {
        // 22.14 is below MIN_AUGMENTED but at/above MIN_SUPPORTED —
        // it runs in compatibility mode, not refused.
        let node = ResolvedNode {
            path: Utf8PathBuf::from("/usr/bin/node"),
            version: NodeVersion::new(22, 14, 5),
            pin_source: None,
        };
        assert!(check_min_version(&node).is_ok());
    }

    #[test]
    fn check_min_version_rejects_18_18() {
        let node = ResolvedNode {
            path: Utf8PathBuf::from("/usr/bin/node"),
            version: NodeVersion::new(18, 18, 2),
            pin_source: Some(".nvmrc".to_string()),
        };
        match check_min_version(&node) {
            Err(DiscoveryError::Unsupported {
                version,
                pin_source,
            }) => {
                assert_eq!(version, NodeVersion::new(18, 18, 2));
                assert_eq!(pin_source.as_deref(), Some(".nvmrc"));
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn discover_node_returns_something() {
        // Basic smoke test — requires node on PATH.
        let cwd = env::current_dir().unwrap();
        match discover_node(&cwd) {
            Ok(node) => {
                assert!(!node.path.as_str().is_empty());
                assert!(node.version.major() >= 18);
            }
            Err(DiscoveryError::NoNodeOnPath) => {
                eprintln!("skipping: no node on PATH");
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}

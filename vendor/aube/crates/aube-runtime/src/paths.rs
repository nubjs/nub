//! On-disk layout for aube-managed Node installs and caches. Follows
//! the same XDG / `%LOCALAPPDATA%` / home-fallback convention as
//! `aube-store/src/dirs.rs` (kept separate so this crate doesn't pull
//! in the CAS machinery for three path joins).
//!
//! ```text
//! $XDG_DATA_HOME/<data_namespace>/nodejs/
//! ├── 24.1.0/              # native layout: unix bin/node, win node.exe
//! ├── .downloads/          # in-flight archive downloads
//! ├── .tmp/                # extraction staging (rename source)
//! └── .locks/              # per-version fslock files
//! ```

use std::path::PathBuf;

/// Root of aube-managed Node installs. `AUBE_RUNTIME_DIR` overrides
/// for tests and unusual setups. The data namespace comes from the
/// active embedder (standalone aube → `"aube"`).
pub fn runtime_dir() -> Option<PathBuf> {
    if let Some(dir) = aube_util::env::embedder_env("RUNTIME_DIR")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    let ns = aube_util::embedder().data_namespace;
    #[cfg(windows)]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        return Some(PathBuf::from(local).join(ns).join("nodejs"));
    }
    let data_home = match aube_util::env::xdg_data_home() {
        Some(xdg) => xdg,
        None => aube_util::env::home_dir()?.join(".local/share"),
    };
    Some(data_home.join(ns).join("nodejs"))
}

/// The install dir for an exact version: `<runtime_dir>/24.1.0/`.
pub fn install_dir(version: &node_semver::Version) -> Option<PathBuf> {
    runtime_dir().map(|d| d.join(version.to_string()))
}

pub(crate) fn downloads_dir() -> Option<PathBuf> {
    runtime_dir().map(|d| d.join(".downloads"))
}

pub(crate) fn staging_dir() -> Option<PathBuf> {
    runtime_dir().map(|d| d.join(".tmp"))
}

pub(crate) fn locks_dir() -> Option<PathBuf> {
    runtime_dir().map(|d| d.join(".locks"))
}

/// Cache directory shared with the rest of aube, mirroring `aube-store`'s
/// `cache_dir`. Uses the active embedder's `cache_namespace` (standalone aube →
/// `"aube"`) under `$XDG_CACHE_HOME`, `%LOCALAPPDATA%`, or `~/.cache`.
pub(crate) fn cache_dir() -> Option<PathBuf> {
    let ns = aube_util::embedder().cache_namespace;
    if let Some(xdg) = aube_util::env::xdg_cache_home() {
        return Some(xdg.join(ns));
    }
    #[cfg(windows)]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        return Some(PathBuf::from(local).join(ns));
    }
    aube_util::env::home_dir().map(|h| h.join(".cache").join(ns))
}

/// Disk cache for the dist index, segmented by mirror origin so two
/// mirrors never serve each other's entries:
/// `$XDG_CACHE_HOME/aube/node-index/origin-<sha256-16>/index.json`.
pub(crate) fn index_cache_path(mirror_base: &str) -> Option<PathBuf> {
    cache_dir().map(|d| {
        d.join("node-index")
            .join(origin_segment(mirror_base))
            .join("index.json")
    })
}

/// Disk cache for a release's SHASUMS256.txt (immutable once
/// published — cached forever):
/// `$XDG_CACHE_HOME/aube/node-shasums/origin-<sha256-16>/v24.1.0.txt`.
pub(crate) fn shasums_cache_path(
    mirror_base: &str,
    version: &node_semver::Version,
) -> Option<PathBuf> {
    cache_dir().map(|d| {
        d.join("node-shasums")
            .join(origin_segment(mirror_base))
            .join(format!("v{version}.txt"))
    })
}

fn origin_segment(mirror_base: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(mirror_base.as_bytes());
    format!("origin-{}", hex::encode(&digest[..8]))
}

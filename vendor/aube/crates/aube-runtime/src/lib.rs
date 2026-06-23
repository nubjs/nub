//! Node.js runtime management for aube: resolve a project's requested
//! Node version (devEngines.runtime / `.node-version` / `.nvmrc`),
//! discover satisfying installs (PATH, mise, aube's own runtime dir),
//! and download missing versions — delegating to mise by default when
//! it's present so mise users keep one Node store.
//!
//! This crate is policy-light plumbing: settings resolution, manifest
//! parsing, PATH injection, and lockfile recording live in the CLI
//! crate. Nothing here prints; progress flows through
//! [`DownloadProgress`] and diagnostics through `tracing`.

mod discover;
mod error;
mod extract;
mod http;
mod index;
mod installer;
mod mise;
mod paths;
mod platform;
mod progress;
mod resolver;
mod self_install;
mod shasums;
mod sources;
mod spec;

pub use discover::{
    InstallOrigin, InstalledNode, list_installed, mise_node_installs_dir, node_on_path,
    probe_path_node,
};
pub use error::Error;
pub use mise::mise_on_path;
pub use paths::{install_dir, runtime_dir};
pub use platform::Platform;
pub use progress::{DownloadProgress, InstallPhase, NoopProgress};
pub use resolver::{NodeRuntime, Resolution, ResolvedFrom};
pub use self_install::{
    InstalledAube, available_aube_versions, find_installed_aube, install_aube, list_installed_aube,
    release_target_triple, self_dir,
};
pub use shasums::{sha256_from_sri, sri_sha256};
pub use sources::{effective_request, find_version_file};
pub use spec::{NodeRequest, NodeSpec, RequestSource};

use std::collections::BTreeMap;

/// Default download base, matching pnpm's runtime resolver (the
/// `/download/release` tree mirrors `/dist` and is what pnpm records
/// in lockfiles).
pub const DEFAULT_MIRROR_BASE: &str = "https://nodejs.org/download/release";

/// musl builds aren't published on the official mirror; pnpm and mise
/// both source them from unofficial-builds. Used only when no custom
/// mirror is configured.
pub const UNOFFICIAL_BASE: &str = "https://unofficial-builds.nodejs.org/download/release";

/// Who installs a missing runtime (the `runtimeInstaller` setting).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InstallerMode {
    /// Delegate to `mise install` when mise is on PATH, else download.
    #[default]
    Auto,
    /// Always delegate; fail if mise is missing.
    Mise,
    /// Never delegate.
    Aube,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkMode {
    #[default]
    Online,
    /// Serve caches regardless of staleness; never touch the network.
    Offline,
}

/// Configuration for a [`NodeRuntime`].
#[derive(Debug, Clone, Default)]
pub struct RuntimeConfig {
    pub installer: InstallerMode,
    /// `nodeDownloadMirrors.release` — replaces the official base for
    /// index, checksums, and artifacts when set.
    pub mirror: Option<String>,
    pub network: NetworkMode,
    /// Extra request retries after the first attempt (default 2).
    pub retries: u32,
}

impl RuntimeConfig {
    pub fn new() -> Self {
        RuntimeConfig {
            retries: 2,
            ..Default::default()
        }
    }

    /// The base URL for the index and (non-musl) artifacts, without a
    /// trailing slash.
    pub(crate) fn mirror_base(&self) -> String {
        match &self.mirror {
            Some(m) => m.trim_end_matches('/').to_string(),
            None => DEFAULT_MIRROR_BASE.to_string(),
        }
    }

    /// The base URL artifacts and checksums are fetched from for
    /// `platform`: the configured mirror always wins (a corporate
    /// mirror may host musl artifacts); otherwise musl platforms
    /// route to unofficial-builds.
    pub(crate) fn artifact_base(&self, platform: &Platform) -> String {
        if self.mirror.is_none() && platform.libc.as_deref() == Some("musl") {
            UNOFFICIAL_BASE.to_string()
        } else {
            self.mirror_base()
        }
    }
}

/// An exact resolved version plus its per-platform artifacts —
/// the interchange shape for lockfile pins (the CLI maps this onto
/// `aube_lockfile::RuntimePin`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedNode {
    pub version: node_semver::Version,
    pub variants: Vec<PinnedVariant>,
}

impl PinnedNode {
    pub fn variant_for(&self, os: &str, cpu: &str, libc: Option<&str>) -> Option<&PinnedVariant> {
        self.variants
            .iter()
            .find(|v| v.os == os && v.cpu == cpu && v.libc.as_deref() == libc)
    }
}

/// One platform's artifact in a [`PinnedNode`]. Vocabulary matches
/// pnpm's lockfile (`os: win32`, `archive: tarball|zip`,
/// `integrity: sha256-<base64>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedVariant {
    pub os: String,
    pub cpu: String,
    pub libc: Option<String>,
    pub archive: String,
    pub url: String,
    pub integrity_sri: String,
    pub bin: BTreeMap<String, String>,
    pub prefix: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_bases() {
        let mut cfg = RuntimeConfig::new();
        assert_eq!(cfg.mirror_base(), DEFAULT_MIRROR_BASE);
        cfg.mirror = Some("https://npmmirror.com/mirrors/node/".to_string());
        assert_eq!(cfg.mirror_base(), "https://npmmirror.com/mirrors/node");

        let musl = Platform {
            os: "linux".into(),
            cpu: "x64".into(),
            libc: Some("musl".into()),
        };
        // Custom mirror wins even for musl.
        assert_eq!(
            cfg.artifact_base(&musl),
            "https://npmmirror.com/mirrors/node"
        );
        cfg.mirror = None;
        assert_eq!(cfg.artifact_base(&musl), UNOFFICIAL_BASE);
        let glibc = Platform {
            os: "linux".into(),
            cpu: "x64".into(),
            libc: None,
        };
        assert_eq!(cfg.artifact_base(&glibc), DEFAULT_MIRROR_BASE);
    }
}

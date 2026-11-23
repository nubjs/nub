//! The embedder's incumbent-PM detection, injected.
//!
//! Which package manager a project belongs to is decided in the CLI, by the same
//! declaration and lockfile functions the embedded engine uses
//! (`aube_lockfile::declared_package_manager`, the config-surface walk, the
//! declared pnpm major). nub-core cannot depend on the engine, and a second
//! detector here drifts from that one on ranges and multi-tool `devEngines`
//! arrays, so nub-core asks through this probe instead. The CLI registers it once
//! at startup, before anything reads project state.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::workspace::npmrc::MemberNpmrc;

/// The questions nub-core asks about a project's incumbent.
#[derive(Clone, Copy)]
pub struct IncumbentProbe {
    /// The ONE package manager `dir/package.json` itself declares, or `None`
    /// (absent, or a `devEngines.packageManager` array naming several tools).
    pub declared_pm_name: fn(&Path) -> Option<String>,
    /// Whether a member's own `.npmrc` layers over the workspace root's.
    pub member_npmrc: fn(&Path) -> MemberNpmrc,
    /// The user-level `.npmrc` the engine reads (`NPM_CONFIG_USERCONFIG` or
    /// `~/.npmrc`), so the package-manager download reads the same file.
    pub user_npmrc: fn() -> Option<PathBuf>,
    /// The registry-client settings the environment sets, as `.npmrc` entries
    /// lowest precedence first, by the engine's rules (`npm_config_*` in either
    /// case, and the incumbent's own variables): the default `registry` and the
    /// auth that goes with it.
    pub env_config: fn() -> Vec<(String, String)>,
    /// The integrity `<dir>/pnpm-lock.yaml` records for the pinned pnpm at a
    /// version (its `packageManagerDependencies`), as an SRI string.
    pub pnpm_lockfile_integrity: fn(&Path, &str) -> Option<String>,
    /// The pnpm version `<dir>/pnpm-lock.yaml` records in its
    /// `packageManagerDependencies`.
    pub pnpm_lockfile_version: fn(&Path) -> Option<String>,
}

static PROBE: OnceLock<IncumbentProbe> = OnceLock::new();

/// Install the embedder's probe. Set-once; a second call is ignored.
pub fn register(probe: IncumbentProbe) {
    let _ = PROBE.set(probe);
}

/// See [`IncumbentProbe::declared_pm_name`]. Unregistered, nothing is declared,
/// so the pnpm-named gates fall back to lockfile presence alone.
pub(crate) fn declared_pm_name(dir: &Path) -> Option<String> {
    PROBE.get().and_then(|p| (p.declared_pm_name)(dir))
}

/// See [`IncumbentProbe::member_npmrc`]. Unregistered, the dominant v10 model.
pub(crate) fn member_npmrc(dir: &Path) -> MemberNpmrc {
    PROBE
        .get()
        .map_or(MemberNpmrc::Layered, |p| (p.member_npmrc)(dir))
}

/// See [`IncumbentProbe::user_npmrc`]. Unregistered, `~/.npmrc`.
pub(crate) fn user_npmrc() -> Option<PathBuf> {
    match PROBE.get() {
        Some(p) => (p.user_npmrc)(),
        None => dirs_next::home_dir().map(|h| h.join(".npmrc")),
    }
}

/// See [`IncumbentProbe::env_config`]. Unregistered, `npm_config_registry`.
pub(crate) fn env_config() -> Vec<(String, String)> {
    match PROBE.get() {
        Some(p) => (p.env_config)(),
        None => std::env::var("npm_config_registry")
            .ok()
            .map(|r| ("registry".to_string(), r))
            .into_iter()
            .collect(),
    }
}

/// See [`IncumbentProbe::pnpm_lockfile_integrity`]. Unregistered, `None`.
pub(crate) fn pnpm_lockfile_integrity(dir: &Path, version: &str) -> Option<String> {
    PROBE
        .get()
        .and_then(|p| (p.pnpm_lockfile_integrity)(dir, version))
}

/// See [`IncumbentProbe::pnpm_lockfile_version`]. Unregistered, `None`.
pub(crate) fn pnpm_lockfile_version(dir: &Path) -> Option<String> {
    PROBE.get().and_then(|p| (p.pnpm_lockfile_version)(dir))
}

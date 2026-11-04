//! The embedder's incumbent-PM detection, injected.
//!
//! Which package manager a project belongs to is decided in the CLI, by the same
//! declaration and lockfile functions the embedded engine uses
//! (`aube_lockfile::declared_package_manager`, the config-surface walk, the
//! declared pnpm major). nub-core cannot depend on the engine, and a second
//! detector here drifts from that one on ranges and multi-tool `devEngines`
//! arrays, so nub-core asks through this probe instead. The CLI registers it once
//! at startup, before anything reads project state.

use std::path::Path;
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

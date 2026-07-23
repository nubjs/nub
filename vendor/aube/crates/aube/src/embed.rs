//! Stable in-process API for hosts that embed aube's package manager.
//!
//! Call [`initialize`] once during process startup, then use [`install`] or
//! [`add`] from an async context. Each invocation carries its own output and
//! cancellation state through [`InstallControl`]. Installs in unrelated
//! projects may run concurrently; operations targeting the same workspace
//! are serialized by the project lock.

use std::path::{Path, PathBuf};

pub use crate::commands::add::AddToProjectOptions;
pub use crate::commands::install::{
    DepSelection, FrozenMode, InstallControl, InstallEvent, InstallOutputLevel, InstallOutputMode,
    InstallPhase, InstallProgressSnapshot, InstallReporter,
};
pub use aube_registry::NetworkMode;
pub use aube_util::{AUBE, Embedder as Host};

/// Options for an in-process install.
///
/// Use [`InstallOptions::new`] so the project directory is always explicit.
/// The facade intentionally exposes a smaller, stable set than the internal
/// command options used by aube's CLI.
#[derive(Debug, Clone)]
pub struct InstallOptions {
    /// Project or workspace member whose dependencies should be installed.
    pub project_dir: PathBuf,
    /// How an existing lockfile should be treated relative to the manifest.
    pub frozen_mode: FrozenMode,
    /// Dependency sections to materialize.
    pub dep_selection: DepSelection,
    /// Skip all root and dependency lifecycle scripts.
    pub ignore_scripts: bool,
    /// Run the root package's lifecycle scripts when scripts are enabled.
    pub run_root_lifecycle: bool,
    /// Resolve and report changes without writing them.
    pub dry_run: bool,
    /// Update only the lockfile without materializing `node_modules`.
    pub lockfile_only: bool,
    /// Ignore install freshness state and re-resolve/relink the project.
    pub force: bool,
    /// Registry network behavior for this invocation.
    pub network_mode: NetworkMode,
    /// Require an existing lockfile when using frozen mode.
    pub strict_no_lockfile: bool,
    /// Allow dependency lifecycle scripts without the normal allowlist.
    pub dangerously_allow_all_builds: bool,
    /// Force a live transitive OSV check even when resolution reused every
    /// version from the existing lockfile.
    pub osv_transitive_check: bool,
    /// Invocation-scoped output, progress reporting, and cancellation.
    pub control: InstallControl,
    /// Directory containing the `node` executable to run lifecycle scripts on.
    /// Set this when the host manages its own node runtime (e.g. mise) so
    /// scripts spawn on it and find it on PATH; `None` uses aube's own runtime
    /// resolution / PATH fallback.
    pub node_bin_dir: Option<PathBuf>,
}

impl InstallOptions {
    /// Construct an install with normal non-CI lockfile behavior.
    pub fn new(project_dir: impl Into<PathBuf>) -> Self {
        Self {
            project_dir: project_dir.into(),
            frozen_mode: FrozenMode::Prefer,
            dep_selection: DepSelection::All,
            ignore_scripts: false,
            run_root_lifecycle: true,
            dry_run: false,
            lockfile_only: false,
            force: false,
            network_mode: NetworkMode::Online,
            strict_no_lockfile: false,
            dangerously_allow_all_builds: false,
            osv_transitive_check: false,
            control: InstallControl::default(),
            node_bin_dir: None,
        }
    }
}

/// Result type returned by the embedding API.
pub type Result<T> = miette::Result<T>;

/// Register the host identity and its user-overridable setting defaults.
///
/// Both registrations are process-global and first-write-wins. Call this once,
/// before starting any aube operation. A process that does not call this
/// function uses standalone aube's [`AUBE`] profile and built-in setting
/// defaults.
///
/// Setting defaults use canonical setting names and their string forms. They
/// have the lowest precedence, below environment variables, project files,
/// user configuration, and explicit command options.
pub fn initialize(host: &'static Host, setting_defaults: Vec<(String, String)>) {
    aube_util::set_embedder(host);
    aube_settings::set_embedder_defaults(setting_defaults);
}

/// Return the process's active host profile.
pub fn host() -> &'static Host {
    aube_util::embedder()
}

/// Install the dependencies declared by a project.
pub async fn install(options: InstallOptions) -> Result<()> {
    let mut command_options =
        crate::commands::install::InstallOptions::with_mode(options.frozen_mode);
    command_options.project_dir = Some(options.project_dir);
    command_options.dep_selection = options.dep_selection;
    command_options.ignore_scripts = options.ignore_scripts;
    command_options.skip_root_lifecycle = !options.run_root_lifecycle;
    command_options.dry_run = options.dry_run;
    command_options.lockfile_only = options.lockfile_only;
    command_options.force = options.force;
    command_options.network_mode = options.network_mode;
    command_options.strict_no_lockfile = options.strict_no_lockfile;
    command_options.dangerously_allow_all_builds = options.dangerously_allow_all_builds;
    command_options.osv_transitive_check = options.osv_transitive_check;
    command_options.control = options.control;
    command_options.embedder_node_bin_dir = options.node_bin_dir;
    crate::commands::install::run(command_options).await
}

/// Add packages to a project's manifest and install the resulting graph.
///
/// The project lock spans both manifest mutation and installation, so another
/// in-process operation cannot observe the intermediate manifest state.
/// Cancellation restores the manifest and lockfile to their pre-call state.
/// Other install errors preserve the manifest change for a later retry,
/// matching CLI `add` behavior.
pub async fn add(
    project_dir: &Path,
    packages: &[String],
    options: AddToProjectOptions,
) -> Result<()> {
    crate::commands::add::add_to_project(project_dir, packages, options).await
}

/// Extract a stable `ERR_AUBE_*` identifier from a failed operation.
pub fn error_code(error: &miette::Report) -> Option<String> {
    error.code().map(|code| code.to_string())
}

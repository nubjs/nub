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
    InstallPhase, InstallProgressSnapshot, InstallPrompt, InstallPromptFuture,
    InstallPromptHandler, InstallReporter,
};
pub use crate::runtime::{EmbedderRuntime, set_embedder_runtime};
pub use aube_manifest::{Error as ManifestError, PackageJson, Workspaces};
pub use aube_registry::NetworkMode;
pub use aube_util::{AUBE, Embedder as Host};
pub use aube_workspace::{WorkspaceBoundary, WorkspaceDiscoveryOptions};

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
    /// How the host wants Node invoked for lifecycle scripts. Use
    /// [`EmbedderRuntime::selector`] when the host merely manages a Node
    /// toolchain (e.g. mise), or [`EmbedderRuntime::wrapper`] to interpose on
    /// Node (instrumenting runtime, transpiling loader, sandbox). Takes
    /// precedence over any process-wide [`set_embedder_runtime`]; `None` falls
    /// back to that, else aube's own runtime resolution / PATH fallback.
    pub runtime: Option<EmbedderRuntime>,
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
            runtime: None,
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

/// Whether `project_dir` declares a Node workspace.
///
/// This recognizes Aube and pnpm workspace YAML plus the npm/Yarn/Bun
/// `package.json#workspaces` field. Invalid or unrelated package manifests are
/// treated as non-workspaces, making this suitable for repository-wide
/// provider probing.
pub fn is_workspace_project_root(project_dir: &Path) -> bool {
    aube_workspace::is_workspace_project_root(project_dir)
}

/// Resolve the project or workspace root an install started at `start` will
/// use. Hosts that preflight before calling [`install`] must use this exact
/// resolver so validation targets the same file-backed project configuration
/// and `node_modules` tree as the later install.
pub fn resolve_install_root(start: &Path) -> Result<PathBuf> {
    crate::dirs::workspace_or_project_root_from(start)
}

/// Discover packages declared by a Node workspace.
///
/// Use [`WorkspaceBoundary::ConfinedToRoot`] when `project_dir` is a security
/// or repository boundary chosen by the host. The default preserves
/// package-manager compatibility with parent-relative pnpm workspace globs.
/// Confined discovery returns canonical package paths; other modes preserve
/// the package-manager-facing lexical paths.
pub fn discover_workspace_packages(
    project_dir: &Path,
    options: WorkspaceDiscoveryOptions,
) -> Result<Vec<PathBuf>> {
    aube_workspace::find_workspace_packages_with_options(project_dir, options)
        .map_err(miette::Report::new)
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
    command_options.embedder_runtime = options.runtime;
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

/// Run a package's script (`package.json` `scripts.<name>`) in `project_dir`,
/// the in-process equivalent of `aube run <script> -- <args>`. Runs pre/post
/// hooks and returns the script's exit code (`None` when nothing ran).
///
/// The project is resolved from `project_dir` (walking up to the nearest
/// `package.json`), never the process cwd, so concurrent calls in different
/// projects don't race. Every spawn honors `runtime` when given, else the
/// process-wide [`set_embedder_runtime`] — pass a per-call runtime when it
/// varies per invocation (e.g. a fresh shim dir per command), since the
/// process-wide registration is set-once.
pub async fn run(
    project_dir: &Path,
    script: &str,
    args: Vec<String>,
    runtime: Option<EmbedderRuntime>,
) -> Result<Option<i32>> {
    crate::runtime::with_embedder_runtime(
        runtime,
        crate::commands::run::run_script_in(
            project_dir.to_path_buf(),
            script,
            &args,
            false,
            false,
            &aube_workspace::selector::EffectiveFilter::default(),
        ),
    )
    .await
}

/// Run a project-local binary (`node_modules/.bin/<bin>`) in `project_dir`,
/// the in-process equivalent of `aube exec <bin> -- <args>`. Returns the
/// binary's exit code. The project is resolved from `project_dir`, not the
/// process cwd; every spawn honors `runtime` when given, else the
/// process-wide [`set_embedder_runtime`].
pub async fn exec(
    project_dir: &Path,
    bin: &str,
    args: Vec<String>,
    runtime: Option<EmbedderRuntime>,
) -> Result<Option<i32>> {
    let exec_args = crate::commands::exec::ExecArgs {
        bin: bin.to_string(),
        args,
        ..Default::default()
    };
    crate::runtime::with_embedder_runtime(
        runtime,
        crate::commands::exec::run_in(
            exec_args,
            aube_workspace::selector::EffectiveFilter::default(),
            Some(project_dir.to_path_buf()),
        ),
    )
    .await
}

/// Install one or more packages into a throwaway project and run a binary
/// from them, the in-process equivalent of `aube dlx`. `params` is the
/// command followed by its arguments; `packages` overrides the inferred
/// install target (the `-p` flag), empty to infer from the command.
///
/// The transient install runs in its own scratch project; `project_dir`
/// roots runtime resolution and the local-`.bin` fast path. Every spawn
/// honors `runtime` when given, else the process-wide
/// [`set_embedder_runtime`].
///
/// Unlike [`run`] / [`exec`], `dlx` changes the process working directory
/// (into its scratch project) for the duration of the transient install —
/// inherent to how dlx works. A host should not run `dlx` concurrently
/// with other cwd-sensitive work in the same process.
pub async fn dlx(
    project_dir: &Path,
    params: Vec<String>,
    packages: Vec<String>,
    runtime: Option<EmbedderRuntime>,
) -> Result<Option<i32>> {
    let args = crate::commands::dlx::DlxArgs {
        params,
        package: packages,
        ..Default::default()
    };
    crate::runtime::with_embedder_runtime(
        runtime,
        crate::commands::dlx::run_in(
            args,
            Some(project_dir.to_path_buf()),
            std::collections::BTreeMap::new(),
        ),
    )
    .await
}

/// Run Node as a supervised child in `project_dir`, the in-process
/// equivalent of `aube node -- <args>`. Unlike the CLI, this never
/// image-replaces the host process; it returns Node's exit code. Runtime is
/// resolved from `project_dir`; the spawn honors `runtime` when given —
/// a wrapper's `NODE`/`NODE_OPTIONS` included — else the process-wide
/// [`set_embedder_runtime`].
pub async fn node(
    project_dir: &Path,
    args: Vec<std::ffi::OsString>,
    runtime: Option<EmbedderRuntime>,
) -> Result<Option<i32>> {
    crate::runtime::with_embedder_runtime(
        runtime,
        crate::commands::node::run_spawn(args, Some(project_dir.to_path_buf())),
    )
    .await
}

/// Extract a stable `ERR_AUBE_*` identifier from a failed operation.
pub fn error_code(error: &miette::Report) -> Option<String> {
    error.code().map(|code| code.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifest_types_keep_tolerant_parsing() {
        let manifest: PackageJson = serde_json::from_str(
            r#"{
                "name": "app",
                "workspaces": "packages/*",
                "dependencies": null,
                "devDependencies": {"local": "workspace:*", "junk": false}
            }"#,
        )
        .unwrap();

        assert_eq!(manifest.name.as_deref(), Some("app"));
        assert_eq!(
            manifest.workspaces.as_ref().map(|workspaces| workspaces
                .patterns()
                .iter()
                .map(String::as_str)
                .collect()),
            Some(vec!["packages/*"])
        );
        assert!(manifest.dependencies.is_empty());
        assert_eq!(
            manifest.dev_dependencies.get("local").map(String::as_str),
            Some("workspace:*")
        );
        assert!(!manifest.dev_dependencies.contains_key("junk"));
    }
}

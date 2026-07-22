//! `aube ci` / `aube clean-install`: strict install for CI environments.
//!
//! Matches `pnpm ci` / `npm ci`:
//!   1. A lockfile must be present and in sync with `package.json` — drift is
//!      a hard error. Internally this reuses `install::FrozenMode::Frozen`.
//!   2. `node_modules/` is deleted first, guaranteeing a clean install
//!      independent of whatever state was left by a previous run.
//!   3. Root lifecycle scripts (`preinstall`, `install`, `postinstall`,
//!      `prepare`) run unless `--ignore-scripts` is set. Dependency
//!      scripts run only if the project's `allowBuilds` allowlist
//!      permits them (same semantics as `aube install`);
//!      `--dangerously-allow-all-builds` is always off under `aube ci`.
//!
//! The global virtual store (`~/.cache/aube/virtual-store/`) and the content-
//! addressable store (`$XDG_DATA_HOME/aube/store/`) are intentionally **not**
//! deleted — those are caches, not per-project state, and wiping them would
//! defeat the point of a CI cache layer.

use super::install;
use clap::Args;

#[derive(Debug, Args)]
pub struct CiArgs {
    /// Skip lifecycle scripts (no-op; aube already skips by default)
    #[arg(long)]
    pub ignore_scripts: bool,
    /// Skip optionalDependencies; don't install optional native modules
    #[arg(long)]
    pub no_optional: bool,
    #[command(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[command(flatten)]
    pub virtual_store: crate::cli_args::VirtualStoreArgs,
}

pub async fn run(args: CiArgs) -> miette::Result<()> {
    args.network.install_overrides();
    args.lockfile.install_overrides();
    args.virtual_store.install_overrides();
    let CiArgs {
        ignore_scripts,
        no_optional,
        lockfile: _,
        network: _,
        virtual_store: _,
    } = args;
    let cwd = crate::dirs::workspace_or_project_root()?;
    let lock = super::take_project_lock(&cwd)?;

    let nm = super::project_modules_dir(&cwd);
    // `symlink_metadata` instead of `exists` so we notice (and delete) a
    // symlink without following it. `remove_existing` then handles symlinks
    // via `remove_file` rather than `remove_dir_all` — otherwise rmtree on a
    // symlink-to-dir would recursively wipe the symlink's *target* (which
    // could be anywhere on disk) and then fail to remove the symlink itself.
    if nm.symlink_metadata().is_ok() {
        eprintln!("Removing existing node_modules...");
        super::remove_existing(&nm)?;
    }

    // Strict frozen install. Any drift is an error, no lockfile is an error.
    // Propagate --ignore-scripts so root lifecycle hooks are skipped.
    let opts = install::InstallOptions {
        control: install::InstallControl::default(),
        embedder_node_bin_dir: None,
        project_dir: None,
        mode: install::FrozenMode::Frozen,
        dep_selection: install::DepSelection::from_flags(false, false, no_optional),
        ignore_pnpmfile: false,
        pnpmfile: None,
        global_pnpmfile: None,
        ignore_scripts,
        dry_run: false,
        lockfile_only: false,
        merge_git_branch_lockfiles: false,
        dangerously_allow_all_builds: false,
        network_mode: aube_registry::NetworkMode::Online,
        minimum_release_age_override: None,
        strict_no_lockfile: true,
        force: false,
        cli_flags: vec![(
            "dangerously-allow-all-builds".to_string(),
            "false".to_string(),
        )],
        env_snapshot: aube_settings::values::capture_env(),
        git_prepare_depth: 0,
        inherited_build_policy: None,
        build_policy_override: None,
        workspace_filter: aube_workspace::selector::EffectiveFilter::default(),
        skip_root_lifecycle: false,
        // `aube ci` is the canonical "lockfile is law" path —
        // strict frozen, no drift allowed. Don't force the live
        // API; install::run's fresh-resolution detection and the
        // `advisoryCheckEveryInstall` setting still apply.
        osv_transitive_check: false,
    };
    install::run_with_project_lock(opts, &lock).await
}

use super::{FrozenMode, InstallOptions};
use crate::state;
use miette::miette;
use std::path::{Path, PathBuf};

pub(super) fn resolve_project_cwd(opts: &InstallOptions) -> miette::Result<PathBuf> {
    if let Some(project_dir) = &opts.project_dir {
        return Ok(project_dir.clone());
    }
    // `workspace_or_project_root` gives us workspace-first precedence:
    // `aube install` from inside a workspace member installs against
    // the workspace root, so members don't get their own lockfile or
    // virtual store. Yaml-only roots install with a synthesized empty
    // manifest later in the pipeline.
    crate::dirs::workspace_or_project_root()
}

pub(super) fn apply_force_state_reset(cwd: &Path, opts: &InstallOptions) -> miette::Result<()> {
    if !opts.force {
        return Ok(());
    }
    state::remove_state(cwd).map_err(|e| miette!("--force: failed to remove install state: {e}"))
}

pub(super) fn modules_cache_sweep_is_default(cwd: &Path) -> bool {
    super::super::with_settings_ctx(cwd, |ctx| {
        aube_settings::resolved::modules_cache_max_age(ctx) == 10080
    })
}

pub(super) fn try_install_fast_path(
    cwd: &Path,
    opts: &InstallOptions,
    mode: FrozenMode,
    modules_cache_sweep_default: bool,
) -> miette::Result<Option<usize>> {
    let dangerously_allow_all_builds = resolve_dangerously_allow_all_builds(cwd, opts);
    if !install_fast_path_eligible(
        cwd,
        opts,
        mode,
        modules_cache_sweep_default,
        dangerously_allow_all_builds,
    ) {
        return Ok(None);
    }
    opts.control.check_cancelled()?;
    let total = state::read_state_package_content_hashes(cwd)
        .map(|packages| packages.len())
        .or_else(|| {
            let manifest = super::super::load_manifest_or_default(cwd).ok()?;
            aube_lockfile::parse_lockfile_with_kind(cwd, &manifest)
                .ok()
                .map(|(graph, _)| graph.packages.len())
        })
        .unwrap_or_default();
    Ok(Some(total))
}

fn resolve_dangerously_allow_all_builds(cwd: &Path, opts: &InstallOptions) -> bool {
    let files = super::super::FileSources::load(cwd);
    let raw_workspace = aube_manifest::workspace::load_raw(cwd).unwrap_or_default();
    let ctx = files.ctx(&raw_workspace, &opts.env_snapshot, &opts.cli_flags);
    aube_settings::resolved::dangerously_allow_all_builds(&ctx)
}

fn install_fast_path_eligible(
    cwd: &Path,
    opts: &InstallOptions,
    mode: FrozenMode,
    modules_cache_sweep_default: bool,
    dangerously_allow_all_builds: bool,
) -> bool {
    let preconditions_met = matches!(mode, FrozenMode::Frozen | FrozenMode::Prefer)
        && !opts.force
        && !opts.lockfile_only
        && !opts.dep_selection.is_filtered()
        && !opts.merge_git_branch_lockfiles
        && !opts.strict_no_lockfile
        && !dangerously_allow_all_builds
        && opts.workspace_filter.is_empty()
        && modules_cache_sweep_default;
    if !preconditions_met {
        return false;
    }
    if paranoid_requires_full_pipeline(cwd, opts) {
        return false;
    }
    // Surface *why* the warm path was missed at debug level — the state
    // freshness reason is otherwise discarded here (only `.is_none()` is
    // consulted), leaving `aube install -v` silent on repeat-install loops
    // that originate from state drift rather than lockfile drift.
    match state::check_needs_install_with_flags(cwd, &opts.cli_flags) {
        None => compatibility_metadata_is_current(cwd, opts),
        Some(reason) => {
            tracing::debug!("install warm path skipped: {reason}");
            false
        }
    }
}

fn compatibility_metadata_is_current(cwd: &Path, opts: &InstallOptions) -> bool {
    let Some(layout) = state::read_state_layout(cwd) else {
        return false;
    };
    let modules_dir_name = super::super::resolve_modules_dir_name_for_cwd(cwd);
    let aube_dir = super::super::resolve_virtual_store_dir_for_cwd(cwd);
    let mut legacy_vite_patches_current = true;
    let expected = match layout.linker {
        state::InstallLayoutMode::Hoisted => Some(aube_dir),
        state::InstallLayoutMode::Isolated => {
            let files = super::super::FileSources::load(cwd);
            let raw_workspace = aube_manifest::workspace::load_raw(cwd).unwrap_or_default();
            let ctx = files.ctx(&raw_workspace, &opts.env_snapshot, &opts.cli_flags);
            let global_virtual_store = super::super::global_virtual_store_dir_with_ctx(cwd, &ctx);
            match super::gvs::detect_existing_global_virtual_store(
                cwd,
                &aube_dir,
                &modules_dir_name,
                &global_virtual_store,
            ) {
                Some(true) => {
                    legacy_vite_patches_current =
                        super::gvs::legacy_vite_patches_are_current(&aube_dir);
                    if layout.gvs_nested_links.is_none() {
                        tracing::debug!(
                            "install warm path skipped: install state predates global virtual store link tracking"
                        );
                        return false;
                    }
                    if !state::gvs_nested_links_are_current(cwd, &layout) {
                        tracing::debug!(
                            "install warm path skipped: global virtual store links are stale"
                        );
                        return false;
                    }
                    Some(global_virtual_store)
                }
                Some(false) => Some(aube_dir),
                None => {
                    tracing::debug!(
                        "install warm path skipped: unable to detect virtual-store layout"
                    );
                    return false;
                }
            }
        }
    };
    if !legacy_vite_patches_current {
        tracing::debug!("install warm path skipped: legacy Vite patch is missing");
        return false;
    }
    let metadata_current = super::gvs::modules_metadata_is_current(
        cwd,
        layout.direct_entries.keys().map(String::as_str),
        &modules_dir_name,
        expected.as_deref(),
    );
    if !metadata_current {
        tracing::debug!("install warm path skipped: .modules.yaml metadata is stale");
    }
    metadata_current
}

fn paranoid_requires_full_pipeline(cwd: &Path, opts: &InstallOptions) -> bool {
    if opts.network_mode == aube_registry::NetworkMode::Offline {
        return false;
    }
    let files = crate::commands::FileSources::load(cwd);
    let raw_workspace = aube_manifest::workspace::load_raw(cwd).unwrap_or_default();
    let ctx = files.ctx(&raw_workspace, &opts.env_snapshot, &opts.cli_flags);
    // `paranoid` bundles strict advisory checks and store-integrity gates,
    // so it always takes the full pipeline. A locked package is already a
    // trust decision, so trustPolicy alone does not invalidate the fast path.
    aube_settings::resolved::paranoid(&ctx)
}

pub(super) fn emit_up_to_date(cwd: &Path) {
    super::unreviewed_builds::emit_warning(&super::unreviewed_builds::from_state(cwd));
    super::print_already_up_to_date();
}

pub(super) fn merge_branch_lockfiles_if_needed(
    cwd: &Path,
    manifest: &aube_manifest::PackageJson,
    settings_ctx: &aube_settings::ResolveCtx<'_>,
    lockfile_enabled: bool,
    force_merge: bool,
) -> miette::Result<()> {
    if !lockfile_enabled {
        return Ok(());
    }

    let patterns = aube_settings::resolved::merge_git_branch_lockfiles_branch_pattern(settings_ctx)
        .unwrap_or_default();
    let should_merge = force_merge || aube_lockfile::merge::current_branch_matches(cwd, &patterns);
    if !should_merge {
        return Ok(());
    }

    match aube_lockfile::merge_branch_lockfiles(cwd, manifest) {
        Ok(report) => {
            if !report.merged_files.is_empty() {
                let filenames: Vec<String> = report
                    .merged_files
                    .iter()
                    .filter_map(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .map(|s| s.to_string())
                    })
                    .collect();
                tracing::info!(
                    "merged {} branch lockfile(s) into aube-lock.yaml: {}",
                    report.merged_files.len(),
                    filenames.join(", ")
                );
                if !report.conflicts.is_empty() {
                    super::control::output(
                        super::InstallOutputLevel::Warning,
                        None,
                        format!(
                            "{} conflict(s) resolved during branch-lockfile merge:",
                            report.conflicts.len()
                        ),
                    );
                    for c in &report.conflicts {
                        super::control::output(
                            super::InstallOutputLevel::Warning,
                            None,
                            format!("  {c}"),
                        );
                    }
                }
            } else {
                tracing::debug!(
                    "branch-lockfile merge triggered but no aube-lock.*.yaml files were found"
                );
            }
            Ok(())
        }
        Err(err) => Err(miette!("failed to merge branch lockfiles: {err}")),
    }
}

pub(super) fn warn_accepted_noop_install_settings(settings_ctx: &aube_settings::ResolveCtx<'_>) {
    if super::settings::resolve_use_running_store_server(settings_ctx) {
        super::control::output(
            super::InstallOutputLevel::Warning,
            None,
            "aube has no store server; useRunningStoreServer=true is accepted but has no effect",
        );
    }
    if !super::settings::resolve_symlink(settings_ctx) {
        super::control::output(
            super::InstallOutputLevel::Warning,
            None,
            "aube's isolated layout requires symlinks; symlink=false is accepted but has no effect",
        );
    }
}

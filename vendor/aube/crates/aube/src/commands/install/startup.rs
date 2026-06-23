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
) -> bool {
    if restore_missing_lockfile_fast_path_eligible(cwd, opts, mode, modules_cache_sweep_default) {
        emit_up_to_date(cwd);
        return true;
    }

    if !install_fast_path_eligible(cwd, opts, mode, modules_cache_sweep_default) {
        return false;
    }
    emit_up_to_date(cwd);
    true
}

fn install_fast_path_eligible(
    cwd: &Path,
    opts: &InstallOptions,
    mode: FrozenMode,
    modules_cache_sweep_default: bool,
) -> bool {
    let preconditions_met = matches!(mode, FrozenMode::Frozen | FrozenMode::Prefer)
        && !opts.force
        && !opts.lockfile_only
        && !opts.dep_selection.is_filtered()
        && !opts.merge_git_branch_lockfiles
        && !opts.strict_no_lockfile
        && !opts.dangerously_allow_all_builds
        && opts.workspace_filter.is_empty()
        && modules_cache_sweep_default;
    if !preconditions_met {
        return false;
    }
    // Surface *why* the warm path was missed at debug level — the state
    // freshness reason is otherwise discarded here (only `.is_none()` is
    // consulted), leaving `aube install -v` silent on repeat-install loops
    // that originate from state drift rather than lockfile drift.
    match state::check_needs_install_with_flags(cwd, &opts.cli_flags) {
        None => true,
        Some(reason) => {
            tracing::debug!("install warm path skipped: {reason}");
            false
        }
    }
}

fn restore_missing_lockfile_fast_path_eligible(
    cwd: &Path,
    opts: &InstallOptions,
    mode: FrozenMode,
    modules_cache_sweep_default: bool,
) -> bool {
    matches!(mode, FrozenMode::No)
        && !opts.force
        && !opts.lockfile_only
        && !opts.dep_selection.is_filtered()
        && !opts.merge_git_branch_lockfiles
        && !opts.strict_no_lockfile
        && !opts.dangerously_allow_all_builds
        && opts.workspace_filter.is_empty()
        && modules_cache_sweep_default
        && state::restore_missing_lockfile_if_fresh(cwd, &opts.cli_flags)
}

fn emit_up_to_date(cwd: &Path) {
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
                    crate::progress::safe_eprintln(&format!(
                        "warn: {} conflict(s) resolved during branch-lockfile merge:",
                        report.conflicts.len()
                    ));
                    for c in &report.conflicts {
                        crate::progress::safe_eprintln(&format!("warn:   {c}"));
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
        eprintln!(
            "warning: aube has no store server; useRunningStoreServer=true is accepted but has no effect"
        );
    }
    if !super::settings::resolve_symlink(settings_ctx) {
        eprintln!(
            "warning: aube's isolated layout requires symlinks; symlink=false is accepted but has no effect"
        );
    }
}

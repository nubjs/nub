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
    emit_up_to_date(cwd);
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
    // Trust re-validation only blocks the short-circuit under embedders that opt
    // into re-validating an already-satisfied tree (`warm_trust_revalidate`,
    // aube's default). When it does NOT (nub), the gate is skipped here and the
    // warm path is decided purely by `check_needs_install` below: only a
    // fully-satisfied no-op (`None`) short-circuits, and any real work
    // (`Some(reason)`) falls through to the full pipeline where the trust check
    // fires during resolution. So skipping this gate never lets an install that
    // installs anything bypass the trust posture — it only drops the redundant
    // re-validation of an unchanged, on-disk, previously-validated tree.
    if aube_util::embedder().warm_trust_revalidate && trust_policy_requires_validation(cwd, opts) {
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

fn trust_policy_requires_validation(cwd: &Path, opts: &InstallOptions) -> bool {
    if opts.network_mode == aube_registry::NetworkMode::Offline {
        return false;
    }
    let files = crate::commands::FileSources::load(cwd);
    let raw_workspace = aube_manifest::workspace::load_raw(cwd).unwrap_or_default();
    let ctx = files.ctx(&raw_workspace, &opts.env_snapshot, &opts.cli_flags);
    aube_settings::resolved::paranoid(&ctx)
        || matches!(
            aube_settings::resolved::trust_policy(&ctx),
            aube_settings::resolved::TrustPolicy::NoDowngrade
        )
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

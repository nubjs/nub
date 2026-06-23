use crate::state;
use miette::miette;
use std::path::Path;

pub(super) fn resolve_global_virtual_store_override(
    settings_ctx: &aube_settings::ResolveCtx<'_>,
    manifests: &[(String, aube_manifest::PackageJson)],
    env_snapshot: &[(String, String)],
) -> Option<bool> {
    let explicit = aube_settings::resolved::enable_global_virtual_store(settings_ctx);
    explicit.or_else(|| {
        let triggers =
            aube_settings::resolved::disable_global_virtual_store_for_packages(settings_ctx);
        let triggered_by = super::settings::find_gvs_incompatible_trigger(manifests, &triggers);
        let ci_mode = env_snapshot.iter().any(|(k, _)| k == "CI");
        let virtual_store_only_setting = aube_settings::resolved::virtual_store_only(settings_ctx);
        if let Some(name) = triggered_by
            && !ci_mode
            && !virtual_store_only_setting
        {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_GVS_INCOMPATIBLE,
                "`{name}` isn't compatible with aube's global virtual store — \
                 installing per-project instead. Install still succeeds; repeat \
                 installs of this project just won't share materialized packages \
                 across projects. Fixing this requires an upstream change in \
                 `{name}` itself (please file it with that project, not aube). \
                 To silence this warning, run `aube config set \
                 enableGlobalVirtualStore false --location project` — or set \
                 `disableGlobalVirtualStoreForPackages=[]` to opt out of this \
                 auto-detection entirely. \
                 Details: https://aube.jdx.dev/package-manager/global-virtual-store"
            );
            Some(false)
        } else {
            None
        }
    })
}

pub(super) fn planned_global_virtual_store(
    use_global_virtual_store_override: Option<bool>,
    env_snapshot: &[(String, String)],
) -> bool {
    use_global_virtual_store_override
        .unwrap_or_else(|| !env_snapshot.iter().any(|(k, _)| k == "CI"))
}

/// The global virtual store mode the linker will *actually* materialize.
///
/// `planned_global_virtual_store` is the requested mode (override, else
/// `!CI`), but the linker forces per-project materialization whenever the
/// hidden hoist tree is enabled (`hoist=true`, the default) or the layout
/// is `hoisted` — see `Linker::link_all`/`link_workspace`, both of which
/// fall back to `without_global_virtual_store()` under those conditions.
/// `reset_on_mode_change` compares this against the existing `.aube/` tree,
/// so it MUST predict the same value the linker writes; using the raw
/// `planned_gvs` instead made every non-fast-path install on a default
/// (`hoist=true`) project see a spurious `disabled → enabled` transition
/// and wipe `node_modules` (issue #71 — `aube add` in a workspace member,
/// which always bypasses the fast path).
pub(super) fn effective_global_virtual_store(
    planned_gvs: bool,
    hoist: bool,
    node_linker: aube_linker::NodeLinker,
) -> bool {
    planned_gvs && !hoist && matches!(node_linker, aube_linker::NodeLinker::Isolated)
}

pub(super) fn reset_on_mode_change(
    cwd: &Path,
    aube_dir: &Path,
    modules_dir_name: &str,
    planned_gvs: bool,
) -> miette::Result<()> {
    let Some(existing_gvs) = super::settings::detect_aube_dir_gvs_mode(aube_dir) else {
        return Ok(());
    };
    if existing_gvs == planned_gvs {
        return Ok(());
    }

    let from = if existing_gvs { "enabled" } else { "disabled" };
    let to = if planned_gvs { "enabled" } else { "disabled" };
    let modules_dir_path = cwd.join(modules_dir_name);
    tracing::warn!(
        code = aube_codes::warnings::WARN_AUBE_GVS_MODE_CHANGED,
        "global virtual store {from} → {to}; removing {} and reinstalling from scratch",
        modules_dir_path.display()
    );
    remove_dir_all_if_exists(&modules_dir_path).map_err(|e| {
        miette!(
            "global virtual store transition: failed to remove {}: {e}",
            modules_dir_path.display()
        )
    })?;
    if !aube_dir.starts_with(&modules_dir_path) {
        remove_dir_all_if_exists(aube_dir).map_err(|e| {
            miette!(
                "global virtual store transition: failed to remove {}: {e}",
                aube_dir.display()
            )
        })?;
    }
    state::remove_state(cwd).map_err(|e| {
        miette!("global virtual store transition: failed to remove install state: {e}")
    })
}

fn remove_dir_all_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_linker::NodeLinker;

    // Regression for issue #71: the mode-change check must predict the
    // *effective* layout, which the linker forces to per-project whenever
    // the hidden hoist tree is on (the default) or the layout is hoisted.
    // Before the fix, `reset_on_mode_change` was fed the raw `planned_gvs`
    // (`true` off-CI), so a default project — whose linker materializes
    // per-project (`false`) — saw a spurious `disabled → enabled` flip on
    // every non-fast-path install (e.g. `add` in a workspace member),
    // wiping node_modules each time.

    #[test]
    fn default_project_predicts_per_project_so_no_spurious_reset() {
        // hoist=true (default), isolated (default), GVS requested on (off-CI):
        // the linker writes per-project, so the effective mode is `false`.
        assert!(!effective_global_virtual_store(true, true, NodeLinker::Isolated));
    }

    #[test]
    fn gvs_active_only_when_hoist_off_and_isolated() {
        assert!(effective_global_virtual_store(true, false, NodeLinker::Isolated));
    }

    #[test]
    fn hoisted_layout_never_uses_global_virtual_store() {
        assert!(!effective_global_virtual_store(true, false, NodeLinker::Hoisted));
        assert!(!effective_global_virtual_store(true, true, NodeLinker::Hoisted));
    }

    #[test]
    fn requested_off_stays_off_regardless_of_hoist() {
        assert!(!effective_global_virtual_store(false, false, NodeLinker::Isolated));
        assert!(!effective_global_virtual_store(false, true, NodeLinker::Isolated));
    }
}

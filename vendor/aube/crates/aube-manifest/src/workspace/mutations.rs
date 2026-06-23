//! Domain-specific workspace-config mutations.
//!
//! `allowBuilds` and `patchedDependencies` edits route through
//! `config_write_target` (workspace yaml when one exists,
//! `package.json#pnpm.<key>` otherwise) so a project that adopted
//! the workspace yaml keeps its comments + structure when aube
//! mutates these maps.

use super::config::{ConfigWriteTarget, config_write_target};
use super::edits::{
    add_to_pnpm_only_built_dependencies, edit_setting_map, edit_workspace_yaml,
    set_pnpm_allow_builds_entries, workspace_yaml_submap,
};
use std::path::{Path, PathBuf};

/// Force-write `names` in the project's `allowBuilds` map. Routes
/// through [`config_write_target`]: workspace yaml when one exists,
/// otherwise `package.json` (`pnpm.allowBuilds` / `aube.allowBuilds`
/// for a namespaced tool, top-level for a manifest-root embedder).
/// Returns the file that was written. Used by `aube approve-builds`
/// and the `--allow-build=<pkg>` / `--deny-build=<pkg>` CLI flags —
/// entries are forcibly set, overwriting any prior value.
///
/// Manifest-root embedder routing: an embedder whose `manifest_namespace`
/// is `""` writes the setting at the *top level* of `package.json` via
/// [`edit_setting_map`]. The read side honors that top-level (neutral,
/// un-branded) key on *every* surface, so under nub identity AND under an
/// npm/bun/yarn incumbent (`read_branded_pnpm_config` off) the approval takes
/// effect — `approve-builds` heals on those surfaces.
///
/// The one surface where a bare top-level write is NOT the right target is the
/// embedder's *pnpm-compat* surface (`read_branded_pnpm_config` on): there the
/// reader also consults the `pnpm.*` namespace and the workspace yaml, and to
/// match real pnpm 10.x — and to keep the approval where a pnpm user expects it
/// — the write lands under `pnpm.*` (see below). The top-level key would still
/// be *read* there, but writing under `pnpm.*` is the more faithful placement.
///
/// On that pnpm-compat surface the write lands in `package.json` under
/// `pnpm.*` — the surface the reader honors — *without* creating a
/// `pnpm-workspace.yaml` where none exists (a fresh yaml file is noisy).
/// Approvals (`allow=true`) go to `pnpm.onlyBuiltDependencies` (pnpm's
/// canonical allowlist array, which real pnpm 10.x also reads from
/// `package.json`); denials (`allow=false`) go to the `pnpm.allowBuilds`
/// map, since the array form carries no per-entry boolean. An *existing*
/// `pnpm-workspace.yaml` still wins (the `WorkspaceYaml` arm) — we append
/// there to keep all workspace config in one place.
pub fn set_allow_builds(
    project_dir: &Path,
    names: &[String],
    allow: bool,
) -> Result<PathBuf, crate::Error> {
    match config_write_target(project_dir) {
        ConfigWriteTarget::WorkspaceYaml(path) => write_allow_builds_yaml(&path, names, allow),
        ConfigWriteTarget::PackageJson if root_write_unread_but_yaml_read() => {
            if allow {
                add_to_pnpm_only_built_dependencies(project_dir, names)?;
            } else {
                // No array slot for a denial — record `false` in the nested
                // `pnpm.allowBuilds` map, which both nub and pnpm 10.x read.
                set_pnpm_allow_builds_entries(project_dir, names, false)?;
            }
            Ok(project_dir.join("package.json"))
        }
        ConfigWriteTarget::PackageJson => {
            edit_setting_map(project_dir, "allowBuilds", |map| {
                for name in names {
                    map.insert(name.clone(), serde_json::Value::Bool(allow));
                }
            })?;
            Ok(project_dir.join("package.json"))
        }
    }
}

/// Whether a `package.json`-target `allowBuilds` write would land at the
/// *top level* of the manifest yet be unread there, while the workspace
/// yaml *would* be read — the manifest-root-embedder + pnpm-compat-surface
/// combination that makes a top-level write a silent no-op. True only when:
/// the embedder is manifest-root (`manifest_namespace == ""`, so
/// [`edit_setting_map`] writes at top level), the read side does *not* read
/// the top-level key (`read_manifest_root_config` is off), and it *does* read
/// the pnpm workspace yaml (`read_branded_pnpm_config` is on). For a
/// namespaced tool (standalone aube writes `aube.allowBuilds`, which its own
/// reader consults) this is false and the `package.json` path is taken
/// unchanged.
fn root_write_unread_but_yaml_read() -> bool {
    let ctx = aube_util::engine_context();
    aube_util::embedder().manifest_namespace.is_empty()
        && !ctx.read_manifest_root_config
        && ctx.read_branded_pnpm_config
}

/// Force-approve `names` in the project's `allowBuilds` map.
pub fn add_to_allow_builds(project_dir: &Path, names: &[String]) -> Result<PathBuf, crate::Error> {
    set_allow_builds(project_dir, names, true)
}

/// Canonical placeholder string pnpm writes for unreviewed `allowBuilds`
/// entries. Aube never writes it (we leave the manifest alone and rely
/// on the warning + `aube approve-builds` flow instead), but pnpm-managed
/// projects swapping to aube can carry these strings in their existing
/// configs. The read-side in `aube-scripts::policy` recognizes this exact
/// value and treats it as "skip without warning" rather than emitting
/// an `UnsupportedValue` warning for every install.
pub const ALLOW_BUILDS_REVIEW_PLACEHOLDER: &str = "set this to true or false";

/// Insert or replace a single `patchedDependencies` entry in the
/// workspace yaml at `path`. Creates the file (and the
/// `patchedDependencies` mapping) if needed. The shared
/// [`edit_workspace_yaml`] helper skips the rewrite when the closure
/// produces no structural change, so an idempotent re-record after
/// editing the patch file leaves yaml comments intact.
pub fn upsert_workspace_patched_dependency(
    path: &Path,
    key: &str,
    rel_patch_path: &str,
) -> Result<PathBuf, crate::Error> {
    edit_workspace_yaml(path, |map| {
        let pd_map = workspace_yaml_submap(map, "patchedDependencies", path)?;
        pd_map.insert(
            yaml_serde::Value::String(key.to_string()),
            yaml_serde::Value::String(rel_patch_path.to_string()),
        );
        Ok(())
    })
}

/// Drop a `patchedDependencies` entry from the workspace yaml at
/// `path`. Returns `Ok(true)` when the entry was removed (and the
/// file was rewritten). When the removal empties
/// `patchedDependencies` we drop the key from the document so we
/// don't leave a `patchedDependencies: {}` stub behind.
pub fn remove_workspace_patched_dependency(path: &Path, key: &str) -> Result<bool, crate::Error> {
    let mut existed = false;
    edit_workspace_yaml(path, |map| {
        let pd_map = workspace_yaml_submap(map, "patchedDependencies", path)?;
        existed = pd_map.shift_remove(key).is_some();
        if pd_map.is_empty() {
            map.shift_remove("patchedDependencies");
        }
        Ok(())
    })?;
    Ok(existed)
}

fn write_allow_builds_yaml(
    path: &Path,
    names: &[String],
    allow: bool,
) -> Result<PathBuf, crate::Error> {
    edit_workspace_yaml(path, |map| {
        let allow_builds = workspace_yaml_submap(map, "allowBuilds", path)?;
        for name in names {
            let key = yaml_serde::Value::String(name.clone());
            allow_builds.insert(key, yaml_serde::Value::Bool(allow));
        }
        Ok(())
    })
}

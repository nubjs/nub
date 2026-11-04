use crate::state;
use miette::miette;
use std::path::{Path, PathBuf};

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
            // The notice is unactionable by the end user (the only fix is an
            // upstream change in `{name}`), so an embedder that owns its own UX
            // (`gvs_incompatible_warning = false`) demotes it to `debug` —
            // silent at default verbosity, reachable when the engine log level
            // is raised. The per-project fallback (`Some(false)`) is identical
            // regardless; only the notice's level differs. The level can't be a
            // runtime value in `tracing::event!`, so the message is built once
            // and the macro selected by branch.
            let msg = format!(
                "`{name}` isn't compatible with aube's global virtual store — \
                 installing per-project instead. Install still succeeds; repeat \
                 installs of this project just won't share materialized packages \
                 across projects. Fixing this requires an upstream change in \
                 `{name}` itself (please file it with that project, not aube). \
                 To silence this warning, run `aube config set \
                 enableGlobalVirtualStore false --local` — or set \
                 `disableGlobalVirtualStoreForPackages=[]` to opt out of this \
                 auto-detection entirely. \
                 Details: https://aube.sh/package-manager/global-virtual-store"
            );
            let code = aube_codes::warnings::WARN_AUBE_GVS_INCOMPATIBLE;
            if aube_util::embedder().gvs_incompatible_warning {
                tracing::warn!(code, "{msg}");
            } else {
                tracing::debug!(code, "{msg}");
            }
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

/// How the isolated-layout `.aube/<dep_path>` entries are materialized on disk —
/// a flat three-valued decision.
///
/// Makes the "hidden hoist tree inside the SHARED global store" state
/// UNREPRESENTABLE (aube issue #6: a bare-name alias inside the shared store is
/// cross-project-mutable, so a live shared store must never carry one): there is
/// simply no `Symlink`-with-hidden-tree variant. Only the project-local disk
/// materialization carries the hidden tree, split into two flat sibling variants
/// (with / without) rather than the former `Disk { hidden_tree: bool }` nesting.
/// This replaces the former loose `(effective_gvs, build_hidden_tree)` bool pair,
/// whose `(true, true)` combination was a representable contradiction.
///
/// The write METHOD ([`aube_linker::LinkStrategy`] = `package-import-method`)
/// and the LAYOUT ([`aube_linker::NodeLinker`] = `node-linker`) are separate,
/// already-orthogonal axes resolved independently — this type is only the
/// symlink-vs-disk × hidden-tree decision the `gvs_over_default_hoist` +
/// `hoist` + `enableGlobalVirtualStore` tangle used to fold into two bools.
///
/// (Orthogonal to this type, under GVS the linker still builds a COLLECTIVE
/// project-local hidden hoist tree over the disk-materialized ejected SUBSET —
/// see [`aube_linker::Linker::link_hidden_hoist`]. That tree is gated on the
/// ejected set, not on this enum, and is #6-safe for the same project-local
/// reason; the [`Symlink`](Self::Symlink) "no hidden tree" here is about the
/// pnpm-parity whole-graph tree, which the shared store cannot carry.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Materialization {
    /// Symlink into the shared global virtual store; files live machine-global.
    /// The whole-graph hidden hoist tree can't live in a shared store, so there
    /// is none of it under this variant.
    Symlink,
    /// Project-local real directories (reflink/hardlink/copy from the CAS) WITH
    /// the pnpm-parity `node_modules/.<store>/node_modules/` hidden hoist tree.
    DiskWithHiddenTree,
    /// Project-local real directories WITHOUT the hidden hoist tree.
    DiskNoHiddenTree,
}

impl Materialization {
    /// Resolve the materialization from the coupling inputs, folding the former
    /// `effective_global_virtual_store` + `build_hidden_tree` computation into
    /// one sound value.
    ///
    /// The shared store engages only on the ISOLATED layout with the requested
    /// mode on and nothing vetoing it: upstream (`gvs_over_default_hoist ==
    /// false`) ANY resolved `hoist=true` (the default) vetoes it; under the
    /// embedder profile (`true`) only an EXPLICITLY-set `hoist=true` does, so a
    /// DEFAULT hoist lets the store engage while the hidden hoist tree is built
    /// wherever it does not (CI, per-project, an incompatible-package trigger,
    /// an explicit opt-out, dlx). The `hoisted` layout discards `.aube`, so it
    /// never uses the shared store. `reset_on_mode_change` compares this
    /// against the existing `.aube/` tree, so it MUST predict the same value
    /// the linker writes (issue #71): a mismatch wipes `node_modules` on every
    /// non-fast-path install.
    pub(super) fn resolve(
        gvs_over_default_hoist: bool,
        planned_gvs: bool,
        resolved_hoist: bool,
        hoist_explicit: Option<bool>,
        node_linker: aube_linker::NodeLinker,
    ) -> Self {
        let isolated = matches!(node_linker, aube_linker::NodeLinker::Isolated);
        let hoist_vetoes_store = if gvs_over_default_hoist {
            hoist_explicit.unwrap_or(false)
        } else {
            resolved_hoist
        };
        if isolated && planned_gvs && !hoist_vetoes_store {
            Self::Symlink
        } else if resolved_hoist {
            Self::DiskWithHiddenTree
        } else {
            Self::DiskNoHiddenTree
        }
    }

    /// Whether the linker materializes `.aube/<dep>` as symlinks into the
    /// shared global virtual store (the former `effective_gvs`).
    pub(super) fn uses_shared_store(self) -> bool {
        matches!(self, Self::Symlink)
    }

    /// Whether the isolated linker builds the pnpm-parity whole-graph hidden hoist
    /// tree. Never true under [`Symlink`](Self::Symlink) — the constraint that
    /// motivates the type.
    pub(super) fn build_hidden_tree(self) -> bool {
        matches!(self, Self::DiskWithHiddenTree)
    }
}

/// Reject the contradictory explicit request `enableGlobalVirtualStore=true`
/// together with a layout that structurally excludes the shared store — an
/// explicit `hoist=true` (which needs the project-local hidden tree) or
/// `node-linker=hoisted` (which discards `.aube` entirely). The old coupling
/// resolved this SILENTLY by dropping the shared-store request (hoist/hoisted
/// won); a two-sided explicit request is genuinely contradictory, so it is a
/// loud error at the config boundary instead. Codeless miette, matching the
/// adjacent `node-linker=pnp` rejection in `resolve_node_linker`. A DEFAULT
/// `hoist=true` is not a conflict — under the embedder profile it lets the
/// store engage — so only `hoist_explicit == Some(true)` triggers this.
pub(super) fn reject_gvs_layout_contradiction(
    enable_gvs_explicit: Option<bool>,
    hoist_explicit: Option<bool>,
    node_linker: aube_linker::NodeLinker,
) -> miette::Result<()> {
    if enable_gvs_explicit != Some(true) {
        return Ok(());
    }
    let conflicting = if matches!(node_linker, aube_linker::NodeLinker::Hoisted) {
        "node-linker=hoisted"
    } else if hoist_explicit == Some(true) {
        "hoist=true"
    } else {
        return Ok(());
    };
    Err(miette!(
        "enableGlobalVirtualStore=true conflicts with {conflicting}: the shared \
         global virtual store needs the isolated layout with no hidden hoist tree \
         (a shared-store bare-name alias would be cross-project-mutable state). \
         Set one, not both."
    ))
}

/// The global-virtual-store decision the fetch-pipelined materializer
/// (`spawn_gvs_prewarm`) must use so its per-package materialize lands
/// exactly where the link phase later reads it.
///
/// For the isolated linker this is the *effective* mode
/// ([`Materialization::uses_shared_store`]), which folds in `hoist`: under the
/// default `hoist=true` layout `Linker::link_all` recurses to
/// `without_global_virtual_store` and materializes per-project, so the
/// prewarm must too. Feeding the prewarm the raw (hoist-unaware) override
/// instead made it populate the shared global store while the link phase
/// re-materialized every package per-project off the fetch tail — wasted
/// work and a serial materialize the pipeline was meant to hide.
///
/// The hoisted linker discards `.aube` regardless, so it keeps the raw
/// override unchanged: forcing its prewarm per-project would only strand
/// orphan `.aube/<dep_path>` dirs the hoisted sweep doesn't reclaim.
pub(super) fn prewarm_global_virtual_store_override(
    node_linker: aube_linker::NodeLinker,
    effective_gvs: bool,
    raw_override: Option<bool>,
) -> Option<bool> {
    if matches!(node_linker, aube_linker::NodeLinker::Isolated) {
        Some(effective_gvs)
    } else {
        raw_override
    }
}

/// Write the pnpm-compatible metadata Vite 8.1+ uses to allow files
/// served from a virtual store outside the workspace root.
///
/// `.modules.yaml` is not aube's install-state file — `.aube-state`
/// remains authoritative. This is a narrow compatibility surface, and
/// preserving unknown keys lets aube coexist with metadata left by
/// pnpm or other tools.
pub(super) fn write_modules_metadata(
    workspace_root: &Path,
    graph: &aube_lockfile::LockfileGraph,
    modules_dir_name: &str,
    virtual_store_dir: &Path,
) -> std::io::Result<()> {
    let virtual_store_dir = virtual_store_dir.to_string_lossy().into_owned();
    for importer_path in graph
        .importers
        .keys()
        .filter(|path| aube_linker::is_physical_importer(path))
    {
        let project_dir =
            super::workspace::importer_project_dir(workspace_root, importer_path.as_str());
        let metadata_path = project_dir.join(modules_dir_name).join(".modules.yaml");
        let bytes = match std::fs::read(&metadata_path) {
            Ok(bytes) => upsert_virtual_store_dir(&bytes, &virtual_store_dir)?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let mut metadata = serde_json::Map::new();
                metadata.insert(
                    "virtualStoreDir".to_string(),
                    serde_json::Value::String(virtual_store_dir.clone()),
                );
                serde_json::to_vec_pretty(&metadata)?
            }
            Err(err) => return Err(err),
        };
        aube_util::fs_atomic::atomic_write(&metadata_path, &bytes)?;
    }
    Ok(())
}

fn upsert_virtual_store_dir(existing: &[u8], virtual_store_dir: &str) -> std::io::Result<Vec<u8>> {
    if let Ok(mut metadata) =
        serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(existing)
    {
        metadata.insert(
            "virtualStoreDir".to_string(),
            serde_json::Value::String(virtual_store_dir.to_string()),
        );
        return serde_json::to_vec_pretty(&metadata).map_err(std::io::Error::other);
    }

    let source = std::str::from_utf8(existing)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut parsed: yaml_serde::Value = yaml_serde::from_str(source)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if !parsed.is_mapping() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "node_modules/.modules.yaml must contain a mapping",
        ));
    }

    let quoted = serde_json::to_string(virtual_store_dir).map_err(std::io::Error::other)?;
    let flow_mapping = source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .find_map(|line| {
            if line.starts_with('%') || line == "---" || line == "..." {
                return None;
            }
            let content = line
                .strip_prefix("---")
                .map(str::trim_start)
                .unwrap_or(line);
            (!content.is_empty()).then(|| content.starts_with('{'))
        })
        .unwrap_or(false);
    let block_scalar = source.lines().any(|line| {
        let line = line.trim_end();
        !line.chars().next().is_some_and(char::is_whitespace)
            && line.split_once(':').is_some_and(|(key, value)| {
                key.trim_matches([' ', '"', '\'']) == "virtualStoreDir"
                    && matches!(value.trim_start().chars().next(), Some('|' | '>'))
            })
    });
    let explicit_document_end = source.lines().any(|line| line.trim() == "...");
    if flow_mapping || block_scalar || explicit_document_end {
        let Some(mapping) = parsed.as_mapping_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "node_modules/.modules.yaml must contain a mapping",
            ));
        };
        mapping.insert(
            yaml_serde::Value::String("virtualStoreDir".to_string()),
            yaml_serde::Value::String(virtual_store_dir.to_string()),
        );
        return yaml_serde::to_string(&parsed)
            .map(String::into_bytes)
            .map_err(std::io::Error::other);
    }

    let replacement = format!("virtualStoreDir: {quoted}");
    let mut output = String::with_capacity(source.len() + replacement.len() + 1);
    let mut replaced = false;
    for segment in source.split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let is_top_level_key = !line.chars().next().is_some_and(char::is_whitespace)
            && line
                .split_once(':')
                .is_some_and(|(key, _)| key.trim_matches([' ', '"', '\'']) == "virtualStoreDir");
        if is_top_level_key {
            output.push_str(&replacement);
            if segment.ends_with("\r\n") {
                output.push_str("\r\n");
            } else if segment.ends_with('\n') {
                output.push('\n');
            }
            replaced = true;
        } else {
            output.push_str(segment);
        }
    }
    if !replaced {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&replacement);
        output.push('\n');
    }
    Ok(output.into_bytes())
}

fn metadata_virtual_store_dir(bytes: &[u8]) -> Option<String> {
    if let Ok(metadata) =
        serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(bytes)
    {
        return metadata
            .get("virtualStoreDir")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
    }
    let source = std::str::from_utf8(bytes).ok()?;
    let metadata: yaml_serde::Value = yaml_serde::from_str(source).ok()?;
    metadata
        .as_mapping()?
        .get(yaml_serde::Value::String("virtualStoreDir".to_string()))?
        .as_str()
        .map(str::to_string)
}

pub(super) fn modules_metadata_is_current<'a>(
    workspace_root: &Path,
    importer_paths: impl IntoIterator<Item = &'a str>,
    modules_dir_name: &str,
    expected_virtual_store_dir: Option<&Path>,
) -> bool {
    importer_paths
        .into_iter()
        .filter(|path| aube_linker::is_physical_importer(path))
        .all(|importer_path| {
            let project_dir = super::workspace::importer_project_dir(workspace_root, importer_path);
            let metadata_path = project_dir.join(modules_dir_name).join(".modules.yaml");
            let Some(actual) = std::fs::read(metadata_path)
                .ok()
                .and_then(|bytes| metadata_virtual_store_dir(&bytes))
            else {
                tracing::debug!(
                    "install warm path skipped: {} has no virtualStoreDir",
                    project_dir
                        .join(modules_dir_name)
                        .join(".modules.yaml")
                        .display()
                );
                return false;
            };
            let matches = expected_virtual_store_dir
                .is_none_or(|expected| actual.as_str() == expected.to_string_lossy().as_ref());
            if !matches {
                tracing::debug!(
                    "install warm path skipped: {} has virtualStoreDir={actual}, expected {}",
                    project_dir
                        .join(modules_dir_name)
                        .join(".modules.yaml")
                        .display(),
                    expected_virtual_store_dir
                        .map_or_else(|| "<any>".into(), |path| path.to_string_lossy())
                );
            }
            matches
        })
}

pub(crate) fn detect_existing_global_virtual_store(
    workspace_root: &Path,
    aube_dir: &Path,
    modules_dir_name: &str,
    global_virtual_store: &Path,
) -> Option<bool> {
    let mut existing_gvs = super::settings::detect_aube_dir_gvs_mode(aube_dir)?;
    if !existing_gvs {
        let metadata_path = workspace_root.join(modules_dir_name).join(".modules.yaml");
        existing_gvs = std::fs::read(metadata_path)
            .ok()
            .and_then(|bytes| metadata_virtual_store_dir(&bytes))
            .map(PathBuf::from)
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    workspace_root.join(modules_dir_name).join(path)
                }
            })
            .is_some_and(|path| {
                let actual = std::fs::canonicalize(&path).unwrap_or(path);
                let expected = std::fs::canonicalize(global_virtual_store)
                    .unwrap_or_else(|_| global_virtual_store.to_path_buf());
                actual == expected
            });
    }
    Some(existing_gvs)
}

fn legacy_vite_dep_paths(
    graph: &aube_lockfile::LockfileGraph,
) -> std::collections::BTreeSet<String> {
    graph
        .packages
        .iter()
        .filter(|(_, pkg)| {
            pkg.registry_name() == "vite" && vite_needs_modules_metadata_backport(&pkg.version)
        })
        .map(|(dep_path, _)| dep_path.clone())
        .collect()
}

/// Select each legacy Vite package plus every package ancestor needed
/// to make the root/importer resolution path reach that local copy.
///
/// Other branches of the graph remain in the GVS. Dependencies of a
/// selected ancestor that are not themselves selected still resolve
/// through the ordinary local `.aube` symlinks into the shared store.
pub(super) fn legacy_vite_project_local_closure(
    graph: &aube_lockfile::LockfileGraph,
) -> std::collections::BTreeSet<String> {
    let mut selected = legacy_vite_dep_paths(graph);
    if selected.is_empty() {
        return selected;
    }
    loop {
        let mut added = false;
        for (parent_path, parent) in &graph.packages {
            if selected.contains(parent_path) {
                continue;
            }
            let reaches_selected = parent.dependencies.iter().any(|(name, tail)| {
                aube_lockfile::resolve_dep_edge(name, tail, |key| graph.packages.contains_key(key))
                    .is_some_and(|child| selected.contains(&child))
            });
            if reaches_selected {
                selected.insert(parent_path.clone());
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    selected
}

fn vite_needs_modules_metadata_backport(version: &str) -> bool {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let mut parts = core.split('.');
    let Some(major) = parts.next().and_then(|part| part.parse::<u32>().ok()) else {
        return false;
    };
    if major != 8 {
        return major < 8;
    }
    parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0)
        < 1
}

const VITE_COMPAT_PREPEND: &str = "import{readFileSync as __aubeRfs}from\"node:fs\";\
import{join as __aubeJoin,isAbsolute as __aubeIsAbs,resolve as __aubeResolve}from\"node:path\";\n";
const VITE_COMPAT_ANCHORS: &[&str] = &[
    "let allowDirs = server.fs.allow;",
    "let allowDirs = server.fs?.allow;",
    "let allowDirs = (_a = server.fs) === null || _a === void 0 ? void 0 : _a.allow;",
];
const VITE_COMPAT_MARKER: &str = "__aubeRfs(__aubeJoin";

/// Backport Vite 8.1's `.modules.yaml` allow-list sniff into the
/// project-local legacy Vite copies.
///
/// The linker may hardlink package files from the CAS. Atomic sibling
/// replacement is therefore load-bearing: truncating the installed
/// file in place would mutate the shared CAS inode.
pub(super) fn patch_legacy_vite_copies(
    aube_dir: &Path,
    graph: &aube_lockfile::LockfileGraph,
    virtual_store_dir_max_length: usize,
    global_virtual_store: &Path,
    modules_dir_name: &str,
) -> std::io::Result<usize> {
    let global_virtual_store = std::fs::canonicalize(global_virtual_store)
        .unwrap_or_else(|_| global_virtual_store.to_path_buf());
    let mut patched = 0;
    for dep_path in legacy_vite_dep_paths(graph) {
        let Some(pkg) = graph.packages.get(&dep_path) else {
            continue;
        };
        let entry = aube_lockfile::dep_path_filename::dep_path_to_filename(
            &dep_path,
            virtual_store_dir_max_length,
        );
        let vite_dir = aube_dir.join(entry).join("node_modules").join(&pkg.name);
        let real_vite_dir = match std::fs::canonicalize(&vite_dir) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if real_vite_dir.starts_with(&global_virtual_store) {
            return Err(std::io::Error::other(format!(
                "refusing to patch shared Vite package at {}",
                real_vite_dir.display()
            )));
        }
        let dist_node = real_vite_dir.join("dist").join("node");
        if !dist_node.is_dir() {
            continue;
        }
        let mut files = Vec::new();
        collect_vite_js(&dist_node, &mut files, 0)?;
        for file in files {
            patched += usize::from(patch_vite_file(&file, modules_dir_name)?);
        }
    }
    Ok(patched)
}

pub(super) fn legacy_vite_patches_are_current(aube_dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(aube_dir) else {
        return false;
    };
    let mut inspected = std::collections::BTreeSet::new();
    for entry in entries.flatten() {
        let modules_dir = entry.path().join("node_modules");
        let Ok(packages) = std::fs::read_dir(modules_dir) else {
            continue;
        };
        for package in packages.flatten() {
            let package_dir = package.path();
            let Ok(bytes) = std::fs::read(package_dir.join("package.json")) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                return false;
            };
            if manifest.get("name").and_then(serde_json::Value::as_str) != Some("vite") {
                continue;
            }
            let Some(version) = manifest.get("version").and_then(serde_json::Value::as_str) else {
                return false;
            };
            if !vite_needs_modules_metadata_backport(version) {
                continue;
            }
            let real_package_dir =
                std::fs::canonicalize(&package_dir).unwrap_or_else(|_| package_dir.clone());
            if !inspected.insert(real_package_dir.clone()) {
                continue;
            }
            let mut files = Vec::new();
            if collect_vite_js(&real_package_dir.join("dist").join("node"), &mut files, 0).is_err()
            {
                return false;
            }
            for file in files {
                let Ok(source) = std::fs::read_to_string(file) else {
                    return false;
                };
                if !source.contains(VITE_COMPAT_MARKER)
                    && VITE_COMPAT_ANCHORS
                        .iter()
                        .any(|anchor| source.contains(anchor))
                {
                    return false;
                }
            }
        }
    }
    true
}

fn collect_vite_js(
    dir: &Path,
    out: &mut Vec<std::path::PathBuf>,
    depth: usize,
) -> std::io::Result<()> {
    if depth > 8 {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_vite_js(&path, out, depth + 1)?;
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "js") {
            out.push(path);
        }
    }
    Ok(())
}

fn patch_vite_file(file: &Path, modules_dir_name: &str) -> std::io::Result<bool> {
    let source = std::fs::read_to_string(file)?;
    if source.contains(VITE_COMPAT_MARKER) {
        return Ok(false);
    }
    let Some(anchor) = VITE_COMPAT_ANCHORS
        .iter()
        .find(|anchor| source.contains(**anchor))
    else {
        return Ok(false);
    };
    let modules_dir = serde_json::to_string(modules_dir_name).map_err(std::io::Error::other)?;
    let insert = format!(
        ";const __awr=searchForWorkspaceRoot(root);\
if(!allowDirs)allowDirs=[__awr];\
try{{const __ac=__aubeRfs(__aubeJoin(__awr,{modules_dir},\".modules.yaml\"),\"utf-8\");\
let __av;try{{__av=JSON.parse(__ac).virtualStoreDir;}}catch{{const __am=__ac.match(/^virtualStoreDir:\\s*(.+?)\\s*(?:#.*)?$/m);\
if(__am)try{{__av=JSON.parse(__am[1]);}}catch{{__av=__am[1];}}}}\
if(__av){{if(__aubeIsAbs(__av))allowDirs.push(__av);\
else if(__av.startsWith(\"..\"))allowDirs.push(__aubeResolve(__aubeJoin(__awr,{modules_dir}),__av));}}}}catch{{void 0;}}"
    );
    let patched = source.replacen(anchor, &format!("{anchor}{insert}"), 1);
    let patched = format!("{VITE_COMPAT_PREPEND}{patched}");
    aube_util::fs_atomic::atomic_write(file, patched.as_bytes())?;
    Ok(true)
}

pub(super) fn reset_on_mode_change(
    cwd: &Path,
    aube_dir: &Path,
    modules_dir_name: &str,
    planned_gvs: bool,
    settings_ctx: &aube_settings::ResolveCtx<'_>,
) -> miette::Result<()> {
    let global_virtual_store =
        crate::commands::global_virtual_store_dir_with_ctx(cwd, settings_ctx);
    let Some(existing_gvs) = detect_existing_global_virtual_store(
        cwd,
        aube_dir,
        modules_dir_name,
        &global_virtual_store,
    ) else {
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
    use aube_lockfile::{DepType, DirectDep, LockedPackage};

    // Regression for issue #71: the mode-change check must predict the
    // *effective* layout, which the linker forces to per-project whenever
    // the hidden hoist tree is on (the default) or the layout is hoisted.
    // Before the fix, `reset_on_mode_change` was fed the raw `planned_gvs`
    // (`true` off-CI), so a default project — whose linker materializes
    // per-project (`false`) — saw a spurious `disabled → enabled` flip on
    // every non-fast-path install (e.g. `add` in a workspace member),
    // wiping node_modules each time.

    // The stock (upstream) coupling: `gvs_over_default_hoist == false`, where a
    // default hoist=true still vetoes the shared store. Passing the flag
    // explicitly keeps these hermetic (no reliance on a process-global embedder).

    #[test]
    fn default_project_predicts_per_project_so_no_spurious_reset() {
        // hoist=true (default), isolated (default), store requested on (off-CI):
        // the linker writes per-project, so the effective mode is `false`.
        assert!(
            !Materialization::resolve(false, true, true, None, NodeLinker::Isolated)
                .uses_shared_store()
        );
    }

    #[test]
    fn store_active_only_when_hoist_off_and_isolated() {
        assert!(
            Materialization::resolve(false, true, false, Some(false), NodeLinker::Isolated)
                .uses_shared_store()
        );
    }

    #[test]
    fn hoisted_layout_never_uses_shared_store() {
        assert!(
            !Materialization::resolve(false, true, false, Some(false), NodeLinker::Hoisted)
                .uses_shared_store()
        );
        assert!(
            !Materialization::resolve(false, true, true, None, NodeLinker::Hoisted)
                .uses_shared_store()
        );
    }

    #[test]
    fn requested_off_stays_off_regardless_of_hoist() {
        assert!(
            !Materialization::resolve(false, false, false, Some(false), NodeLinker::Isolated)
                .uses_shared_store()
        );
        assert!(
            !Materialization::resolve(false, false, true, None, NodeLinker::Isolated)
                .uses_shared_store()
        );
    }

    // The `gvs_over_default_hoist` (nub) coupling, exercised through
    // `Materialization::resolve` with the flag passed explicitly (no
    // process-global embedder registration). Only an EXPLICIT hoist=true vetoes
    // the shared store. The asserted `uses_shared_store()` values are identical
    // to the former `effective_gvs_impl` outputs — behavior is preserved; the
    // enum only makes the illegal shared-store-plus-hidden-tree state
    // unrepresentable.

    #[test]
    fn flag_default_hoist_lets_store_engage() {
        // hoist resolves to the default `true` but was never explicitly set:
        // the shared store engages off-CI on the isolated layout.
        assert_eq!(
            Materialization::resolve(true, true, true, None, NodeLinker::Isolated),
            Materialization::Symlink
        );
    }

    #[test]
    fn flag_explicit_hoist_true_vetoes_store() {
        // An explicit hoist=true (e.g. nub's injected-deps embedder push) keeps
        // per-project + hidden tree, even off-CI.
        assert_eq!(
            Materialization::resolve(true, true, true, Some(true), NodeLinker::Isolated),
            Materialization::DiskWithHiddenTree
        );
    }

    #[test]
    fn flag_explicit_hoist_false_keeps_store_on() {
        assert_eq!(
            Materialization::resolve(true, true, false, Some(false), NodeLinker::Isolated),
            Materialization::Symlink
        );
    }

    #[test]
    fn flag_ci_and_hoisted_stay_off() {
        // planned off (CI) ⇒ off regardless of hoist explicitness.
        assert!(
            !Materialization::resolve(true, false, true, None, NodeLinker::Isolated)
                .uses_shared_store()
        );
        // hoisted layout never engages the shared store.
        assert!(
            !Materialization::resolve(true, true, true, None, NodeLinker::Hoisted)
                .uses_shared_store()
        );
    }

    #[test]
    fn hidden_tree_is_built_only_when_store_is_off() {
        // Default hoist, store on ⇒ no hidden tree (can't live in a shared store).
        assert!(
            !Materialization::resolve(true, true, true, None, NodeLinker::Isolated)
                .build_hidden_tree()
        );
        // Default hoist, store off (CI / per-project) ⇒ hidden tree built.
        assert!(
            Materialization::resolve(true, false, true, None, NodeLinker::Isolated)
                .build_hidden_tree()
        );
        // Explicit hoist=false ⇒ never a hidden tree.
        assert!(
            !Materialization::resolve(true, false, false, Some(false), NodeLinker::Isolated)
                .build_hidden_tree()
        );
    }

    #[test]
    fn symlink_never_builds_a_hidden_tree() {
        // The soundness invariant the type enforces: whenever the shared store
        // is live, no hidden tree — across every combination of inputs.
        for gvs_over_default_hoist in [false, true] {
            for planned_gvs in [false, true] {
                for resolved_hoist in [false, true] {
                    for hoist_explicit in [None, Some(false), Some(true)] {
                        for node_linker in [NodeLinker::Isolated, NodeLinker::Hoisted] {
                            let m = Materialization::resolve(
                                gvs_over_default_hoist,
                                planned_gvs,
                                resolved_hoist,
                                hoist_explicit,
                                node_linker,
                            );
                            assert!(
                                !(m.uses_shared_store() && m.build_hidden_tree()),
                                "shared store must never carry a hidden tree: {m:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    // The previously-SILENT contradiction (explicit enableGlobalVirtualStore=true
    // + a layout that structurally excludes the shared store) is now a loud
    // error at the config boundary.

    #[test]
    fn explicit_store_plus_explicit_hoist_is_a_loud_error() {
        assert!(
            reject_gvs_layout_contradiction(Some(true), Some(true), NodeLinker::Isolated).is_err()
        );
    }

    #[test]
    fn explicit_store_plus_hoisted_layout_is_a_loud_error() {
        assert!(reject_gvs_layout_contradiction(Some(true), None, NodeLinker::Hoisted).is_err());
    }

    #[test]
    fn store_with_default_hoist_or_no_explicit_request_is_not_a_conflict() {
        // Default hoist (None) + explicit store request: no conflict — the
        // profile lets a default hoist engage the store.
        assert!(reject_gvs_layout_contradiction(Some(true), None, NodeLinker::Isolated).is_ok());
        // No explicit store request: never a conflict, whatever the layout.
        assert!(reject_gvs_layout_contradiction(None, Some(true), NodeLinker::Isolated).is_ok());
        assert!(
            reject_gvs_layout_contradiction(Some(false), Some(true), NodeLinker::Hoisted).is_ok()
        );
    }

    // The prewarm materializer must mirror the link phase's *effective*
    // decision for the isolated linker so its per-package work lands where
    // `link_all` reads it. Under default isolated (`hoist=true`,
    // effective=false) it must route per-project; under GVS (`hoist=false`,
    // effective=true) it stays on the shared store.
    #[test]
    fn prewarm_isolated_mirrors_effective_gvs() {
        // default isolated (hoist=true) → effective false → per-project
        assert_eq!(
            prewarm_global_virtual_store_override(NodeLinker::Isolated, false, None),
            Some(false)
        );
        // GVS (hoist=false) → effective true → shared store
        assert_eq!(
            prewarm_global_virtual_store_override(NodeLinker::Isolated, true, None),
            Some(true)
        );
        // An explicit raw override is overridden by the effective decision:
        // the link phase ignores it once hoist forces per-project.
        assert_eq!(
            prewarm_global_virtual_store_override(NodeLinker::Isolated, false, Some(true)),
            Some(false)
        );
    }

    // The hoisted linker discards `.aube`, so its prewarm decision is left
    // as-is (the raw override) — never forced per-project, which would
    // strand orphan `.aube/<dep_path>` dirs the hoisted sweep can't reclaim.
    #[test]
    fn prewarm_hoisted_passes_raw_override_through() {
        assert_eq!(
            prewarm_global_virtual_store_override(NodeLinker::Hoisted, false, Some(true)),
            Some(true)
        );
        assert_eq!(
            prewarm_global_virtual_store_override(NodeLinker::Hoisted, false, None),
            None
        );
    }

    #[test]
    fn writes_vite_metadata_for_each_importer_and_preserves_unknown_keys() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("node_modules"))
            .expect("root node_modules should be created");
        std::fs::create_dir_all(root.join("packages/app/node_modules"))
            .expect("member node_modules should be created");
        std::fs::write(
            root.join("node_modules/.modules.yaml"),
            br#"{"packageManager":"pnpm@11.0.0","virtualStoreDir":".pnpm"}"#,
        )
        .expect("existing metadata should be written");
        std::fs::write(
            root.join("packages/app/node_modules/.modules.yaml"),
            "layoutVersion: 5\n# preserve this comment\nvirtualStoreDir: .pnpm\nhoistPattern:\n  - '*'\n",
        )
        .expect("existing pnpm YAML should be written");

        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.importers.insert(".".to_string(), Default::default());
        graph
            .importers
            .insert("packages/app".to_string(), Default::default());
        let synthetic_importer = "packages/app/node_modules/vite@5.4.21";
        graph
            .importers
            .insert(synthetic_importer.to_string(), Default::default());
        let store = root.join("shared/virtual-store");

        write_modules_metadata(root, &graph, "node_modules", &store)
            .expect("metadata should be written");

        for project in [root.to_path_buf(), root.join("packages/app")] {
            let bytes = std::fs::read(project.join("node_modules/.modules.yaml"))
                .expect("metadata should exist");
            assert_eq!(
                metadata_virtual_store_dir(&bytes).as_deref(),
                Some(store.to_string_lossy().as_ref())
            );
        }
        let root_metadata: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join("node_modules/.modules.yaml"))
                .expect("root metadata should exist"),
        )
        .expect("root metadata should parse");
        assert_eq!(root_metadata["packageManager"], "pnpm@11.0.0");
        let member_metadata =
            std::fs::read_to_string(root.join("packages/app/node_modules/.modules.yaml"))
                .expect("member metadata should be readable");
        assert!(member_metadata.contains("layoutVersion: 5"));
        assert!(member_metadata.contains("# preserve this comment"));
        assert!(member_metadata.contains("hoistPattern:\n  - '*'"));
        assert_eq!(
            metadata_virtual_store_dir(member_metadata.as_bytes()).as_deref(),
            Some(store.to_string_lossy().as_ref())
        );
        assert!(
            !root
                .join(synthetic_importer)
                .join("node_modules/.modules.yaml")
                .exists(),
            "synthetic peer-context importers must not get filesystem metadata"
        );
        assert!(modules_metadata_is_current(
            root,
            graph.importers.keys().map(String::as_str),
            "node_modules",
            Some(&store)
        ));
        std::fs::remove_file(root.join("packages/app/node_modules/.modules.yaml"))
            .expect("member metadata should be removed");
        assert!(!modules_metadata_is_current(
            root,
            graph.importers.keys().map(String::as_str),
            "node_modules",
            Some(&store)
        ));
    }

    #[test]
    fn rewrites_flow_style_yaml_as_a_valid_mapping() {
        for existing in [
            "{virtualStoreDir: .pnpm, layoutVersion: 5}\n",
            "%YAML 1.2\n---\n{virtualStoreDir: .pnpm, layoutVersion: 5}\n",
        ] {
            let updated = upsert_virtual_store_dir(existing.as_bytes(), "/shared/store")
                .expect("metadata should update");
            let parsed: yaml_serde::Value = yaml_serde::from_slice(&updated)
                .expect("updated metadata should remain valid YAML");
            let mapping = parsed
                .as_mapping()
                .expect("updated metadata should remain a mapping");
            assert_eq!(
                mapping
                    .get(yaml_serde::Value::String("virtualStoreDir".to_string()))
                    .and_then(yaml_serde::Value::as_str),
                Some("/shared/store")
            );
            assert_eq!(
                mapping
                    .get(yaml_serde::Value::String("layoutVersion".to_string()))
                    .and_then(yaml_serde::Value::as_i64),
                Some(5)
            );
        }
    }

    #[test]
    fn rewrites_block_scalar_yaml_without_leaving_scalar_lines() {
        let existing = b"virtualStoreDir: |-\n  .pnpm\nlayoutVersion: 5\nhoistPattern:\n  - '*'\n";
        let updated =
            upsert_virtual_store_dir(existing, "/shared/store").expect("metadata should update");
        let parsed: yaml_serde::Value =
            yaml_serde::from_slice(&updated).expect("updated metadata should remain valid YAML");
        let mapping = parsed
            .as_mapping()
            .expect("updated metadata should remain a mapping");
        assert_eq!(
            mapping
                .get(yaml_serde::Value::String("virtualStoreDir".to_string()))
                .and_then(yaml_serde::Value::as_str),
            Some("/shared/store")
        );
        assert_eq!(
            mapping
                .get(yaml_serde::Value::String("layoutVersion".to_string()))
                .and_then(yaml_serde::Value::as_i64),
            Some(5)
        );
    }

    #[test]
    fn inserts_metadata_before_an_explicit_document_end() {
        let existing = b"layoutVersion: 5\nhoistPattern:\n  - '*'\n...\n";
        let updated =
            upsert_virtual_store_dir(existing, "/shared/store").expect("metadata should update");
        let parsed: yaml_serde::Value =
            yaml_serde::from_slice(&updated).expect("updated metadata should remain valid YAML");
        let mapping = parsed
            .as_mapping()
            .expect("updated metadata should remain a mapping");
        assert_eq!(
            mapping
                .get(yaml_serde::Value::String("virtualStoreDir".to_string()))
                .and_then(yaml_serde::Value::as_str),
            Some("/shared/store")
        );
    }

    #[test]
    fn metadata_disambiguates_gvs_trees_with_only_local_packages() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let root = tmp.path();
        let aube_dir = root.join("node_modules/.aube");
        std::fs::create_dir_all(aube_dir.join("vendor-dir@9.9.9"))
            .expect("local package should be created");
        let shared_store = root.join("shared/virtual-store");
        std::fs::write(
            root.join("node_modules/.modules.yaml"),
            format!(
                "virtualStoreDir: {}\n",
                serde_json::to_string(&shared_store.to_string_lossy())
                    .expect("store path should serialize")
            ),
        )
        .expect("metadata should be written");

        assert_eq!(
            detect_existing_global_virtual_store(root, &aube_dir, "node_modules", &shared_store),
            Some(true)
        );
        std::fs::write(
            root.join("node_modules/.modules.yaml"),
            "virtualStoreDir: .pnpm\n",
        )
        .expect("pnpm metadata should be written");
        assert_eq!(
            detect_existing_global_virtual_store(root, &aube_dir, "node_modules", &shared_store),
            Some(false),
            "pnpm's relative local store must not imply aube GVS mode"
        );
    }

    #[test]
    fn legacy_direct_vite_is_selected_but_modern_vite_is_not() {
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.importers.insert(
            ".".to_string(),
            vec![
                DirectDep {
                    name: "vite".to_string(),
                    dep_path: "vite@7.3.6".to_string(),
                    dep_type: DepType::Dev,
                    specifier: Some("^7.0.0".to_string()),
                },
                DirectDep {
                    name: "vite".to_string(),
                    dep_path: "vite@8.1.0".to_string(),
                    dep_type: DepType::Dev,
                    specifier: Some("^8.1.0".to_string()),
                },
            ],
        );
        for version in ["7.3.6", "8.1.0"] {
            let dep_path = format!("vite@{version}");
            graph.packages.insert(
                dep_path.clone(),
                LockedPackage {
                    name: "vite".to_string(),
                    version: version.to_string(),
                    dep_path,
                    ..Default::default()
                },
            );
        }

        assert_eq!(
            legacy_vite_dep_paths(&graph),
            ["vite@7.3.6".to_string()].into_iter().collect()
        );
    }

    #[test]
    fn legacy_vite_local_closure_includes_framework_ancestors_only() {
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "vitepress".to_string(),
                dep_path: "vitepress@1.6.4".to_string(),
                dep_type: DepType::Dev,
                specifier: Some("^1.6.0".to_string()),
            }],
        );
        graph.packages.insert(
            "vite@5.4.21".to_string(),
            LockedPackage {
                name: "vite".to_string(),
                version: "5.4.21".to_string(),
                dep_path: "vite@5.4.21".to_string(),
                ..Default::default()
            },
        );
        graph.packages.insert(
            "vitepress@1.6.4".to_string(),
            LockedPackage {
                name: "vitepress".to_string(),
                version: "1.6.4".to_string(),
                dep_path: "vitepress@1.6.4".to_string(),
                dependencies: [("vite".to_string(), "5.4.21".to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        );
        graph.packages.insert(
            "unrelated@1.0.0".to_string(),
            LockedPackage {
                name: "unrelated".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "unrelated@1.0.0".to_string(),
                ..Default::default()
            },
        );

        assert_eq!(
            legacy_vite_project_local_closure(&graph),
            ["vite@5.4.21".to_string(), "vitepress@1.6.4".to_string()]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn legacy_vite_patch_is_anchored_idempotent_and_breaks_cas_hardlinks() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let aube_dir = tmp.path().join("project/node_modules/.aube");
        let vite_dir = aube_dir.join("vite@7.3.6/node_modules/vite");
        let chunks = vite_dir.join("dist/node/chunks");
        let global = tmp.path().join("global-virtual-store");
        std::fs::create_dir_all(&chunks).expect("Vite chunks should be created");
        std::fs::create_dir_all(&global).expect("global store should be created");
        std::fs::write(
            vite_dir.join("package.json"),
            r#"{"name":"vite","version":"7.3.6"}"#,
        )
        .expect("Vite manifest should be written");

        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "vite".to_string(),
                dep_path: "vite@7.3.6".to_string(),
                dep_type: DepType::Dev,
                specifier: Some("^7.0.0".to_string()),
            }],
        );
        graph.packages.insert(
            "vite@7.3.6".to_string(),
            LockedPackage {
                name: "vite".to_string(),
                version: "7.3.6".to_string(),
                dep_path: "vite@7.3.6".to_string(),
                ..Default::default()
            },
        );

        let cas = tmp.path().join("cas.js");
        let chunk = chunks.join("config.js");
        let source = "let allowDirs = server.fs.allow;\n";
        std::fs::write(&cas, source).expect("CAS stand-in should be written");
        std::fs::hard_link(&cas, &chunk).expect("installed chunk should hardlink to CAS stand-in");

        let patched = patch_legacy_vite_copies(
            &aube_dir,
            &graph,
            aube_lockfile::dep_path_filename::DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH,
            &global,
            "vendor_modules",
        )
        .expect("legacy Vite should be patched");
        assert_eq!(patched, 1);
        let output = std::fs::read_to_string(&chunk).expect("patched chunk should be readable");
        assert!(output.starts_with(VITE_COMPAT_PREPEND));
        assert!(output.contains(VITE_COMPAT_MARKER));
        assert!(output.contains(r#"__aubeJoin(__awr,"vendor_modules",".modules.yaml")"#));
        assert!(!output.contains(r#"__aubeJoin(__awr,"node_modules",".modules.yaml")"#));
        assert_eq!(
            std::fs::read_to_string(&cas).expect("CAS stand-in should be readable"),
            source,
            "atomic replacement must not mutate the shared hardlink inode"
        );

        let second = patch_legacy_vite_copies(
            &aube_dir,
            &graph,
            aube_lockfile::dep_path_filename::DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH,
            &global,
            "vendor_modules",
        )
        .expect("second patch pass should succeed");
        assert_eq!(second, 0);
        assert!(legacy_vite_patches_are_current(&aube_dir));

        std::fs::write(&chunk, source).expect("unpatched Vite chunk should be restored");
        assert!(!legacy_vite_patches_are_current(&aube_dir));

        std::fs::write(&chunk, "export const untouched = true;\n")
            .expect("anchorless Vite chunk should be written");
        assert!(
            legacy_vite_patches_are_current(&aube_dir),
            "an anchorless bundle is intentionally skipped by the patcher"
        );
    }

    #[test]
    fn legacy_vite_patch_matches_vite_two_through_eight_bundles() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        for (index, anchor) in VITE_COMPAT_ANCHORS.iter().enumerate() {
            let chunk = tmp.path().join(format!("chunk-{index}.js"));
            std::fs::write(&chunk, format!("{anchor}\n"))
                .expect("synthetic Vite chunk should be written");
            assert!(patch_vite_file(&chunk, "node_modules").expect("matching anchor should patch"));
            let output = std::fs::read_to_string(&chunk).expect("patched chunk should be readable");
            assert!(output.contains(VITE_COMPAT_MARKER));
        }
    }
}

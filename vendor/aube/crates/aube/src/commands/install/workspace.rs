use crate::commands::workspace_importer_path;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

pub(super) struct WorkspaceInstallPlan {
    pub workspace_packages: Vec<PathBuf>,
    pub has_workspace: bool,
    pub is_workspace_project: bool,
    pub link_all_workspace_importers: bool,
    pub manifests: Vec<(String, aube_manifest::PackageJson)>,
    pub ws_package_versions: HashMap<String, String>,
    pub ws_dirs: BTreeMap<String, PathBuf>,
    pub lifecycle_manifests: Vec<(String, aube_manifest::PackageJson)>,
}

pub(super) fn discover_workspace_plan(
    cwd: &Path,
    root_manifest: &aube_manifest::PackageJson,
    settings_ctx: &aube_settings::ResolveCtx<'_>,
    workspace_filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<WorkspaceInstallPlan> {
    let workspace_packages = aube_workspace::find_workspace_packages(cwd)
        .into_diagnostic()
        .wrap_err("failed to discover workspace packages")?;
    let recursive_install = aube_settings::resolved::recursive_install(settings_ctx);
    let has_workspace = !workspace_packages.is_empty();
    // Distinct from `has_workspace`: `is_workspace_project` stays
    // true when every workspace sub-package was just removed from
    // disk but the workspace yaml / `workspaces` field is still in
    // place. The lockfile drift check needs this stronger signal so
    // it still prunes orphan importer entries on the all-packages-
    // gone boundary, where `manifests` collapses to `[(".", root)]`
    // and looks indistinguishable from a non-workspace install.
    let is_workspace_project = aube_workspace::is_workspace_project_root(cwd);
    let link_all_workspace_importers =
        has_workspace && (recursive_install || !workspace_filter.is_empty());

    let mut manifests = vec![(".".to_string(), root_manifest.clone())];
    let mut ws_package_versions = HashMap::new();
    let mut ws_dirs = BTreeMap::new();

    // Include the root package itself as a workspace target so
    // sub-packages can use `workspace:*` to depend on it. The
    // directory entry is needed for the linker to create symlinks
    // into child packages' node_modules.
    if let Some(ref name) = root_manifest.name {
        let version = root_manifest.version.as_deref().unwrap_or("0.0.0");
        ws_package_versions.insert(name.clone(), version.to_string());
        ws_dirs.insert(name.clone(), cwd.to_path_buf());
    }

    if has_workspace {
        let project_name = root_manifest.name.as_deref().unwrap_or("(unnamed)");
        tracing::debug!(
            "Workspace: {} packages for {project_name}",
            workspace_packages.len()
        );
        for pkg_dir in &workspace_packages {
            let pkg_manifest = aube_manifest::PackageJson::from_path(&pkg_dir.join("package.json"))
                .map_err(miette::Report::new)
                .wrap_err_with(|| format!("failed to read {}/package.json", pkg_dir.display()))?;

            // Importer key uses forward slash. pnpm lockfile convention
            // is always `/`. `pathdiff` lets workspace globs reach into
            // parent trees while still writing relative importer keys.
            let rel_path = pathdiff::diff_paths(pkg_dir, cwd)
                .unwrap_or_else(|| pkg_dir.clone())
                .to_string_lossy()
                .replace('\\', "/");

            if let Some(ref name) = pkg_manifest.name {
                // pnpm accepts workspace members without versions. Use
                // "0.0.0" so workspace protocol and bare `*` links can
                // still resolve locally while specific ranges fail when
                // they should.
                let version = pkg_manifest.version.as_deref().unwrap_or("0.0.0");
                ws_package_versions.insert(name.clone(), version.to_string());
                ws_dirs.insert(name.clone(), pkg_dir.clone());
                tracing::debug!("  {name}@{version} ({rel_path})");
            }

            // `pnpm-workspace.yaml: packages: ["."]` expands to the
            // root itself; skip the empty relative path because `"."`
            // is already seeded above.
            if !rel_path.is_empty() {
                manifests.push((rel_path, pkg_manifest));
            }
        }
    }

    let lifecycle_manifests = if has_workspace && link_all_workspace_importers {
        order_lifecycle_manifests(
            manifests
                .iter()
                .filter(|(importer, _)| aube_linker::is_physical_importer(importer))
                .cloned()
                .collect(),
        )
    } else {
        vec![(".".to_string(), root_manifest.clone())]
    };

    Ok(WorkspaceInstallPlan {
        workspace_packages,
        has_workspace,
        is_workspace_project,
        link_all_workspace_importers,
        manifests,
        ws_package_versions,
        ws_dirs,
        lifecycle_manifests,
    })
}

pub(super) fn filter_graph_to_workspace_selection(
    workspace_root: &std::path::Path,
    workspace_packages: &[std::path::PathBuf],
    graph: &aube_lockfile::LockfileGraph,
    filters: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<aube_lockfile::LockfileGraph> {
    let selected = aube_workspace::selector::select_workspace_packages(
        workspace_root,
        workspace_packages,
        filters,
    )
    .map_err(|e| miette!("invalid --filter selector: {e}"))?;
    if selected.is_empty() {
        return Err(miette!(
            "{}: filter {filters:?} did not match any workspace package",
            aube_util::cmd("install")
        ));
    }
    let mut keep_importers = std::collections::BTreeSet::new();
    if graph.importers.contains_key(".") {
        keep_importers.insert(".".to_string());
    }
    for pkg in selected {
        keep_importers.insert(workspace_importer_path(workspace_root, &pkg.dir)?);
    }
    let importers: std::collections::BTreeMap<String, Vec<aube_lockfile::DirectDep>> = graph
        .importers
        .iter()
        .filter(|(importer, _)| keep_importers.contains(*importer))
        .map(|(importer, deps)| (importer.clone(), deps.clone()))
        .collect();
    let filtered = aube_lockfile::LockfileGraph {
        importers,
        ..graph.clone()
    };
    Ok(filtered.filter_deps(|_| true))
}

/// The set of importer keys whose per-project lockfiles a filtered
/// install should (re)write, or `None` for an unfiltered install (write
/// every importer).
///
/// pnpm parity: `pnpm --filter <pkg> install` under
/// `sharedWorkspaceLockfile=false` only writes the selected project(s)'
/// lockfiles — it does not create or rewrite the workspace root's
/// lockfile (nor unrelated members') as a side effect of a scoped
/// command. The link pass still keeps the root importer for its own
/// dependencies (see [`filter_graph_to_workspace_selection`]), but the
/// lockfile write must stay scoped to exactly what the selector matched,
/// so the root's lockfile is only written when the root itself is
/// selected (`select_workspace_packages` adds it only on a matching
/// filter). Keys use the same `.`/relative form as the importer keys in
/// `manifests` (see [`crate::commands::workspace_importer_path`]).
pub(super) fn per_project_write_selection(
    workspace_root: &Path,
    workspace_packages: &[PathBuf],
    workspace_filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<Option<std::collections::BTreeSet<String>>> {
    if workspace_filter.is_empty() {
        return Ok(None);
    }
    let selected = aube_workspace::selector::select_workspace_packages(
        workspace_root,
        workspace_packages,
        workspace_filter,
    )
    .map_err(|e| miette!("invalid --filter selector: {e}"))?;
    let mut keys = std::collections::BTreeSet::new();
    for pkg in &selected {
        keys.insert(workspace_importer_path(workspace_root, &pkg.dir)?);
    }
    Ok(Some(keys))
}

pub(super) fn importer_project_dir(
    workspace_root: &std::path::Path,
    importer_path: &str,
) -> std::path::PathBuf {
    if importer_path == "." {
        workspace_root.to_path_buf()
    } else {
        // Lexically collapse `..` from the join so a parent-relative
        // importer key (`../sibling`, written by `find_workspace_packages`
        // when `pnpm-workspace.yaml#packages` uses `../**`) lands at
        // the actual sibling directory rather than `<root>/../sibling`.
        // Downstream consumers — `pathdiff` for symlink targets and
        // `strip_prefix` for ancestor checks — give wrong results
        // against an unnormalized path with embedded `..` segments.
        aube_util::path::normalize_lexical(&workspace_root.join(importer_path))
    }
}

/// Reconstruct missing workspace-member importer entries from each
/// member's own per-project lockfile, folding them into `graph`.
///
/// Under `sharedWorkspaceLockfile=false` the warm/fresh install path
/// parses only the *current project's* lockfile, so the graph it hands
/// the linker carries a single `.` importer. That relinks the root, but
/// the linker also needs every member's importer entry (and the packages
/// reachable from it) to relink members. Without this, a member whose
/// `node_modules` was deleted or left incomplete is never repaired: the
/// install links the root, finds nothing else to do, and reports
/// "Already up to date" while the member stays broken.
///
/// Each member ships its own lockfile (written by
/// [`write_per_project_lockfiles`]) whose sole importer is `.`. Parse it,
/// re-key that importer to the member's path, and merge its package
/// closure in. Members already present in `graph` (the cold/resolve path
/// produces every importer) are skipped, so this is a no-op there.
/// Best-effort: a member without a parseable lockfile is skipped — a
/// genuinely new member busts the warm path through its manifest hash and
/// gets a full resolve instead.
pub(super) fn merge_member_lockfile_graphs(
    workspace_root: &std::path::Path,
    graph: &mut aube_lockfile::LockfileGraph,
    manifests: &[(String, aube_manifest::PackageJson)],
) {
    for (importer_path, manifest) in manifests {
        if importer_path == "." || graph.importers.contains_key(importer_path) {
            continue;
        }
        let member_dir = importer_project_dir(workspace_root, importer_path);
        let member_graph = match aube_lockfile::parse_lockfile(&member_dir, manifest) {
            Ok(member_graph) => member_graph,
            Err(e) => {
                tracing::debug!(
                    "sharedWorkspaceLockfile=false: skipping member {importer_path} graph merge: {e}"
                );
                continue;
            }
        };
        if let Some(deps) = member_graph.importers.get(".") {
            graph.importers.insert(importer_path.clone(), deps.clone());
        }
        for (dep_path, pkg) in member_graph.packages {
            graph.packages.entry(dep_path).or_insert(pkg);
        }
    }
}

pub(super) fn order_lifecycle_manifests(
    manifests: Vec<(String, aube_manifest::PackageJson)>,
) -> Vec<(String, aube_manifest::PackageJson)> {
    if manifests.len() < 2 {
        return manifests;
    }

    let importer_index: std::collections::HashMap<&str, usize> = manifests
        .iter()
        .enumerate()
        .map(|(idx, (importer, _))| (importer.as_str(), idx))
        .collect();
    let workspace_name_to_importer: std::collections::HashMap<&str, &str> = manifests
        .iter()
        .filter_map(|(importer, manifest)| {
            manifest
                .name
                .as_deref()
                .map(|name| (name, importer.as_str()))
        })
        .collect();

    let mut edges = vec![Vec::<usize>::new(); manifests.len()];
    let mut indegree = vec![0usize; manifests.len()];
    for (dependent_idx, (dependent_importer, manifest)) in manifests.iter().enumerate() {
        for dep_name in manifest
            .dependencies
            .keys()
            .chain(manifest.dev_dependencies.keys())
            .chain(manifest.optional_dependencies.keys())
        {
            let Some(dependency_importer) = workspace_name_to_importer.get(dep_name.as_str())
            else {
                continue;
            };
            if *dependency_importer == dependent_importer {
                continue;
            }
            let Some(&dependency_idx) = importer_index.get(dependency_importer) else {
                continue;
            };
            if !edges[dependency_idx].contains(&dependent_idx) {
                edges[dependency_idx].push(dependent_idx);
                indegree[dependent_idx] += 1;
            }
        }
    }

    let mut ready: std::collections::VecDeque<usize> = indegree
        .iter()
        .enumerate()
        .filter_map(|(idx, degree)| (*degree == 0).then_some(idx))
        .collect();
    let mut ordered = Vec::with_capacity(manifests.len());
    let mut emitted = vec![false; manifests.len()];
    while let Some(idx) = ready.pop_front() {
        if emitted[idx] {
            continue;
        }
        emitted[idx] = true;
        ordered.push(idx);
        for &dependent_idx in &edges[idx] {
            indegree[dependent_idx] -= 1;
            if indegree[dependent_idx] == 0 {
                ready.push_back(dependent_idx);
            }
        }
    }
    for (idx, is_emitted) in emitted.iter().enumerate() {
        if !is_emitted {
            ordered.push(idx);
        }
    }

    let mut manifests = manifests
        .into_iter()
        .map(Some)
        .collect::<Vec<Option<(String, aube_manifest::PackageJson)>>>();
    ordered
        .into_iter()
        .filter_map(|idx| manifests[idx].take())
        .collect()
}

/// Write one lockfile per workspace importer when
/// `sharedWorkspaceLockfile=false` is set, the workspace root
/// included. Each lockfile contains only that importer's own deps
/// (remapped to `.`) plus the transitive closure reachable from them.
///
/// pnpm under `sharedWorkspaceLockfile=false` writes a separate
/// lockfile for every project, the workspace root included when the
/// root is itself a project — i.e. it ships a `package.json`. So the
/// root's lockfile (importer `.`) is written here too, at the workspace
/// root, even when the root declares no dependencies (its lockfile then
/// just records an empty `.` importer). A config-only root that carries
/// only a `pnpm-workspace.yaml` and no `package.json` is not a project,
/// so its synthetic `.` importer is skipped.
///
/// Each project's existing lockfile format is preserved: a package that
/// already ships a `pnpm-lock.yaml` (or any other supported lockfile)
/// keeps getting that file rewritten in place instead of gaining a
/// surprise `aube-lock.yaml` next to it. This mirrors the single-project
/// write path ([`aube_lockfile::write_lockfile_preserving_existing`]) and
/// pnpm's own `sharedWorkspaceLockfile=false` behavior, where each
/// member keeps its own `pnpm-lock.yaml`. `fallback_kind` is only used
/// for projects (root or member) that have no lockfile yet (the
/// workspace default, derived from the root's lockfile format or
/// aube's own when none exists).
///
/// Importers without a corresponding manifest entry are skipped — the
/// resolver should never produce one, but defensive skipping keeps a
/// stale graph entry from triggering a write into a directory that
/// doesn't exist on disk.
///
/// `write_selection` scopes the write to a subset of importer keys for
/// filtered installs (`aube install --filter <pkg>`); `None` writes every
/// importer. See [`per_project_write_selection`] for how the selection is
/// derived and why a filtered command must not touch the root lockfile.
pub(super) fn write_per_project_lockfiles(
    workspace_root: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    workspace_manifests: &[(String, aube_manifest::PackageJson)],
    fallback_kind: aube_lockfile::LockfileKind,
    write_selection: Option<&std::collections::BTreeSet<String>>,
) -> miette::Result<()> {
    use miette::IntoDiagnostic;
    for (importer_path, pkg_manifest) in workspace_manifests {
        // Filtered install: only (re)write the selected importers'
        // lockfiles. Without this an `aube install --filter <member>`
        // would rewrite the root and every unrelated member lockfile as
        // a side effect, diverging from pnpm (which touches only the
        // selected project's lockfile).
        if let Some(selection) = write_selection
            && !selection.contains(importer_path)
        {
            tracing::debug!(
                "sharedWorkspaceLockfile=false: skipping {importer_path} (not in --filter selection)"
            );
            continue;
        }
        // The workspace root (`.`) gets its own lockfile only when it is
        // itself a project — i.e. it ships a package.json. pnpm under
        // sharedWorkspaceLockfile=false writes the root project's lockfile
        // (even when it declares no dependencies), but writes nothing for a
        // config-only root that carries just a pnpm-workspace.yaml and no
        // package.json. Mirror that: skip the synthetic `.` importer when
        // the root isn't a project.
        if importer_path == "." && !workspace_root.join("package.json").exists() {
            tracing::debug!(
                "sharedWorkspaceLockfile=false: skipping root importer (workspace root has no package.json)"
            );
            continue;
        }
        let Some(subset) = graph.subset_to_importer(importer_path, |_| true) else {
            tracing::debug!(
                "sharedWorkspaceLockfile=false: skipping {importer_path} (no graph importer entry)"
            );
            continue;
        };
        // The root importer (`.`) writes its lockfile at the workspace
        // root itself; members nest under their relative path. Reuse
        // `importer_project_dir` so a parent-relative importer key
        // (`../sibling`, produced when `pnpm-workspace.yaml#packages`
        // uses `../**`) is lexically normalized rather than left as
        // `<root>/../sibling`.
        let pkg_dir = importer_project_dir(workspace_root, importer_path);
        // Honor the member's own on-disk lockfile format; only members
        // without one fall back to the workspace default. Without this,
        // a `pnpm-lock.yaml`-based member gets a redundant `aube-lock.yaml`
        // written alongside its (preserved) pnpm lockfile.
        let write_kind =
            aube_lockfile::detect_existing_lockfile_kind(&pkg_dir).unwrap_or(fallback_kind);
        let written = aube_lockfile::write_lockfile_as(&pkg_dir, &subset, pkg_manifest, write_kind)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to write per-project lockfile at {importer_path}"))?;
        tracing::debug!(
            "sharedWorkspaceLockfile=false: wrote {} for importer {importer_path}",
            written
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| written.display().to_string())
        );
    }
    Ok(())
}

pub(super) fn filter_graph_to_importers<const N: usize>(
    graph: &aube_lockfile::LockfileGraph,
    keep_importers: [&str; N],
) -> aube_lockfile::LockfileGraph {
    let keep_importers: std::collections::BTreeSet<&str> = keep_importers.into_iter().collect();
    let importers: std::collections::BTreeMap<String, Vec<aube_lockfile::DirectDep>> = graph
        .importers
        .iter()
        .filter(|(importer, _)| keep_importers.contains(importer.as_str()))
        .map(|(importer, deps)| (importer.clone(), deps.clone()))
        .collect();
    let filtered = aube_lockfile::LockfileGraph {
        importers,
        ..graph.clone()
    };
    filtered.filter_deps(|_| true)
}

#[cfg(test)]
mod lifecycle_manifest_order_tests {
    use super::order_lifecycle_manifests;

    #[test]
    fn lifecycle_manifests_follow_workspace_dependency_order() {
        let ordered = order_lifecycle_manifests(vec![
            (".".to_string(), named_manifest("root")),
            (
                "packages/app".to_string(),
                manifest_with_dep("app", "@scope/lib"),
            ),
            ("packages/lib".to_string(), named_manifest("@scope/lib")),
        ]);
        let importers = ordered
            .iter()
            .map(|(importer, _)| importer.as_str())
            .collect::<Vec<_>>();

        assert_eq!(importers, [".", "packages/lib", "packages/app"]);
    }

    fn named_manifest(name: &str) -> aube_manifest::PackageJson {
        aube_manifest::PackageJson {
            name: Some(name.to_string()),
            ..Default::default()
        }
    }

    fn manifest_with_dep(name: &str, dep: &str) -> aube_manifest::PackageJson {
        let mut manifest = named_manifest(name);
        manifest
            .dependencies
            .insert(dep.to_string(), "workspace:*".to_string());
        manifest
    }
}

#[cfg(test)]
mod per_project_lockfile_tests {
    use super::write_per_project_lockfiles;
    use aube_lockfile::{DirectDep, LockfileGraph, LockfileKind};
    use std::collections::BTreeMap;

    fn graph_with_importers(importers: &[&str]) -> LockfileGraph {
        let importers: BTreeMap<String, Vec<DirectDep>> = importers
            .iter()
            .map(|importer| ((*importer).to_string(), Vec::new()))
            .collect();
        LockfileGraph {
            importers,
            ..Default::default()
        }
    }

    fn manifest(name: &str) -> aube_manifest::PackageJson {
        aube_manifest::PackageJson {
            name: Some(name.to_string()),
            version: Some("1.0.0".to_string()),
            ..Default::default()
        }
    }

    /// `sharedWorkspaceLockfile=false` must preserve each member's own
    /// lockfile format: a member that already ships `pnpm-lock.yaml` keeps
    /// getting that file rewritten — no surprise `aube-lock.yaml` lands
    /// beside it — while a member with no lockfile yet gets the workspace
    /// default. Regression for per-project installs writing `aube-lock.yaml`
    /// onto pnpm-based members.
    #[test]
    fn preserves_existing_member_lockfile_format() {
        let root = tempfile::tempdir().unwrap();
        let lib_dir = root.path().join("packages/lib");
        let app_dir = root.path().join("packages/app");
        std::fs::create_dir_all(&lib_dir).unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();

        // `lib` already uses pnpm; `app` has no lockfile yet.
        std::fs::write(lib_dir.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();

        let graph = graph_with_importers(&["packages/lib", "packages/app"]);
        let manifests = vec![
            (".".to_string(), manifest("root")),
            ("packages/lib".to_string(), manifest("@test/lib")),
            ("packages/app".to_string(), manifest("@test/app")),
        ];

        // Fallback kind is the workspace default (aube's own) — what an
        // install with no root lockfile would pass.
        write_per_project_lockfiles(root.path(), &graph, &manifests, LockfileKind::Aube, None)
            .unwrap();

        // `lib`: pnpm lockfile is rewritten in place, no aube-lock.yaml.
        assert!(
            lib_dir.join("pnpm-lock.yaml").exists(),
            "existing pnpm-lock.yaml must be preserved"
        );
        assert!(
            !lib_dir.join("aube-lock.yaml").exists(),
            "no aube-lock.yaml must be created next to an existing pnpm-lock.yaml"
        );

        // `app`: no prior lockfile, so the workspace default (Aube) is used.
        assert!(
            app_dir.join("aube-lock.yaml").exists(),
            "a member without a lockfile falls back to the workspace default"
        );
        assert!(!app_dir.join("pnpm-lock.yaml").exists());
    }

    /// The fallback kind is honored for members with no lockfile: when the
    /// workspace default is pnpm (e.g. the root carries `pnpm-lock.yaml`),
    /// fresh members get `pnpm-lock.yaml`, not `aube-lock.yaml`.
    #[test]
    fn fallback_kind_used_when_member_has_no_lockfile() {
        let root = tempfile::tempdir().unwrap();
        let pkg_dir = root.path().join("packages/fresh");
        std::fs::create_dir_all(&pkg_dir).unwrap();

        let graph = graph_with_importers(&["packages/fresh"]);
        let manifests = vec![
            (".".to_string(), manifest("root")),
            ("packages/fresh".to_string(), manifest("@test/fresh")),
        ];

        write_per_project_lockfiles(root.path(), &graph, &manifests, LockfileKind::Pnpm, None)
            .unwrap();

        assert!(
            pkg_dir.join("pnpm-lock.yaml").exists(),
            "fallback kind (pnpm) must be used for a member with no lockfile"
        );
        assert!(!pkg_dir.join("aube-lock.yaml").exists());
    }

    /// A filtered install (`aube install --filter <pkg>`) passes a
    /// `write_selection` so only the selected importers' lockfiles are
    /// (re)written. The workspace root and unrelated members must be left
    /// untouched, matching pnpm's scoped-command behavior. Regression for
    /// the per-project write iterating every importer regardless of filter.
    #[test]
    fn write_selection_scopes_filtered_install_to_selected_importers() {
        let root = tempfile::tempdir().unwrap();
        let lib_dir = root.path().join("packages/lib");
        let app_dir = root.path().join("packages/app");
        std::fs::create_dir_all(&lib_dir).unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();

        // Root is a project too, so without scoping it would also get a
        // lockfile — the selection must keep it out.
        std::fs::write(
            root.path().join("package.json"),
            r#"{ "name": "root", "version": "1.0.0" }"#,
        )
        .unwrap();

        let graph = graph_with_importers(&[".", "packages/lib", "packages/app"]);
        let manifests = vec![
            (".".to_string(), manifest("root")),
            ("packages/lib".to_string(), manifest("@test/lib")),
            ("packages/app".to_string(), manifest("@test/app")),
        ];

        let selection: std::collections::BTreeSet<String> =
            std::iter::once("packages/lib".to_string()).collect();
        write_per_project_lockfiles(
            root.path(),
            &graph,
            &manifests,
            LockfileKind::Aube,
            Some(&selection),
        )
        .unwrap();

        assert!(
            lib_dir.join("aube-lock.yaml").exists(),
            "the selected member's lockfile must be written"
        );
        assert!(
            !app_dir.join("aube-lock.yaml").exists(),
            "an unselected member's lockfile must not be written"
        );
        assert!(
            !root.path().join("aube-lock.yaml").exists(),
            "the workspace root's lockfile must not be written by a filtered install"
        );
    }

    /// Under `sharedWorkspaceLockfile=false`, pnpm writes the root
    /// project's OWN lockfile (importer `.`) at the workspace root when the
    /// root is itself a project (it ships a package.json), alongside each
    /// member's. Regression for the root lockfile disappearing once aube
    /// switched to per-project lockfiles: a root project must keep getting
    /// its own lockfile, with its format preserved (no surprise
    /// `aube-lock.yaml` beside an existing `pnpm-lock.yaml`).
    #[test]
    fn writes_root_importer_lockfile_when_root_is_a_project() {
        let root = tempfile::tempdir().unwrap();
        let lib_dir = root.path().join("packages/lib");
        std::fs::create_dir_all(&lib_dir).unwrap();

        // Root is a project: it ships a package.json and already uses pnpm,
        // so its lockfile format must be preserved.
        std::fs::write(
            root.path().join("package.json"),
            r#"{ "name": "root", "version": "1.0.0" }"#,
        )
        .unwrap();
        std::fs::write(
            root.path().join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n",
        )
        .unwrap();

        let graph = graph_with_importers(&[".", "packages/lib"]);
        let manifests = vec![
            (".".to_string(), manifest("root")),
            ("packages/lib".to_string(), manifest("@test/lib")),
        ];

        write_per_project_lockfiles(root.path(), &graph, &manifests, LockfileKind::Aube, None)
            .unwrap();

        // Root: its own pnpm-lock.yaml is (re)written at the workspace
        // root, format preserved — no aube-lock.yaml beside it.
        let root_lock = root.path().join("pnpm-lock.yaml");
        assert!(
            root_lock.exists(),
            "root pnpm-lock.yaml must be written under sharedWorkspaceLockfile=false"
        );
        assert!(
            !root.path().join("aube-lock.yaml").exists(),
            "no aube-lock.yaml beside the root's preserved pnpm-lock.yaml"
        );
        let root_contents = std::fs::read_to_string(&root_lock).unwrap();
        assert!(
            root_contents.contains("importers:"),
            "root lockfile must hold its own importer, got:\n{root_contents}"
        );

        // Member still gets its own lockfile (fallback kind, no prior lockfile).
        assert!(lib_dir.join("aube-lock.yaml").exists());
    }

    /// A config-only workspace root (a `pnpm-workspace.yaml` but no
    /// `package.json`) is not a project, so pnpm writes no root lockfile —
    /// only the members get one. aube must match: skip the synthetic `.`
    /// importer when the root has no package.json, even though the graph
    /// and manifests still carry a `.` entry for it.
    #[test]
    fn skips_root_importer_when_root_is_not_a_project() {
        let root = tempfile::tempdir().unwrap();
        let lib_dir = root.path().join("packages/lib");
        std::fs::create_dir_all(&lib_dir).unwrap();
        // Deliberately no package.json at the workspace root.

        let graph = graph_with_importers(&[".", "packages/lib"]);
        let manifests = vec![
            (".".to_string(), manifest("root")),
            ("packages/lib".to_string(), manifest("@test/lib")),
        ];

        write_per_project_lockfiles(root.path(), &graph, &manifests, LockfileKind::Aube, None)
            .unwrap();

        // No root lockfile of any kind — the root isn't a project.
        assert!(
            !root.path().join("aube-lock.yaml").exists(),
            "config-only root (no package.json) must not get a lockfile"
        );
        assert!(!root.path().join("pnpm-lock.yaml").exists());
        // Members still get theirs.
        assert!(lib_dir.join("aube-lock.yaml").exists());
    }
}

#[cfg(test)]
mod member_graph_merge_tests {
    use super::{merge_member_lockfile_graphs, write_per_project_lockfiles};
    use aube_lockfile::{DepType, DirectDep, LockedPackage, LockfileGraph, LockfileKind};
    use std::collections::BTreeMap;

    fn manifest(name: &str) -> aube_manifest::PackageJson {
        aube_manifest::PackageJson {
            name: Some(name.to_string()),
            version: Some("1.0.0".to_string()),
            ..Default::default()
        }
    }

    /// Under `sharedWorkspaceLockfile=false` the warm/fresh path loads only
    /// the root importer, so member importers (and the packages reachable
    /// from them) must be folded back in from each member's own per-project
    /// lockfile before linking — otherwise members never get relinked.
    /// Round-trips through [`write_per_project_lockfiles`] so the test
    /// exercises the real on-disk member lockfile, not a hand-rolled graph.
    #[test]
    fn merges_member_importers_and_packages_from_per_project_lockfiles() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("packages/app")).unwrap();

        let mut importers = BTreeMap::new();
        importers.insert(".".to_string(), Vec::new());
        importers.insert(
            "packages/app".to_string(),
            vec![DirectDep {
                name: "is-odd".to_string(),
                dep_path: "is-odd@3.0.1".to_string(),
                dep_type: DepType::Production,
                specifier: Some("3.0.1".to_string()),
            }],
        );
        let mut packages = BTreeMap::new();
        packages.insert(
            "is-odd@3.0.1".to_string(),
            LockedPackage {
                name: "is-odd".to_string(),
                version: "3.0.1".to_string(),
                dep_path: "is-odd@3.0.1".to_string(),
                ..Default::default()
            },
        );
        let full = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        let manifests = vec![
            (".".to_string(), manifest("root")),
            ("packages/app".to_string(), manifest("@test/app")),
        ];
        write_per_project_lockfiles(root.path(), &full, &manifests, LockfileKind::Aube, None)
            .unwrap();

        // Simulate the warm/fresh path: only the root importer is loaded.
        let mut root_only = LockfileGraph {
            importers: BTreeMap::from([(".".to_string(), Vec::new())]),
            ..Default::default()
        };
        merge_member_lockfile_graphs(root.path(), &mut root_only, &manifests);

        let app_deps = root_only
            .importers
            .get("packages/app")
            .expect("member importer must be reconstructed from its per-project lockfile");
        assert_eq!(app_deps.len(), 1);
        assert_eq!(app_deps[0].name, "is-odd");
        assert!(
            root_only.packages.contains_key("is-odd@3.0.1"),
            "member package closure must be merged in"
        );
    }

    /// The cold/resolve path already produces every importer, so a member
    /// that is already present must be left untouched (and a missing
    /// member lockfile must not panic the merge).
    #[test]
    fn merge_leaves_present_importers_untouched() {
        let root = tempfile::tempdir().unwrap();
        let mut graph = LockfileGraph {
            importers: BTreeMap::from([
                (".".to_string(), Vec::new()),
                ("packages/app".to_string(), Vec::new()),
            ]),
            ..Default::default()
        };
        let before = graph.importers.len();
        let manifests = vec![
            (".".to_string(), manifest("root")),
            ("packages/app".to_string(), manifest("@test/app")),
        ];
        merge_member_lockfile_graphs(root.path(), &mut graph, &manifests);
        assert_eq!(graph.importers.len(), before);
    }
}

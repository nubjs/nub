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
    let selected =
        crate::commands::select_projects(workspace_root, workspace_packages, filters, "install")?;
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
    let selected = crate::commands::select_projects(
        workspace_root,
        workspace_packages,
        workspace_filter,
        "install",
    )?;
    let mut keys = std::collections::BTreeSet::new();
    for pkg in &selected {
        keys.insert(workspace_importer_path(workspace_root, &pkg.dir)?);
    }
    Ok(Some(keys))
}

/// The importers an install links, recorded in the install state as
/// [`crate::state::InstallState::workspace_selection`]: the root and the
/// selection for a `--filter` install
/// ([`filter_graph_to_workspace_selection`]), the root alone for an
/// unfiltered one under `recursiveInstall=false`, and `None` when it
/// links every importer.
pub(super) fn linked_workspace_selection(
    per_project_write_selection: Option<&std::collections::BTreeSet<String>>,
    links_every_importer: bool,
) -> Option<std::collections::BTreeSet<String>> {
    match per_project_write_selection {
        Some(selected) => {
            let mut linked = selected.clone();
            linked.insert(".".to_string());
            Some(linked)
        }
        None if links_every_importer => None,
        None => Some(std::collections::BTreeSet::from([".".to_string()])),
    }
}

/// The importers whose own lifecycle hooks this install runs, in
/// `lifecycle_manifests` (dependency) order. A caller's explicit set wins.
/// Otherwise a `--filter` narrows them to the selected members plus the
/// root, whose hooks pnpm runs under any filter; with neither, every linked
/// importer runs its hooks.
pub(super) fn project_hook_importers(
    lifecycle_manifests: &[(String, aube_manifest::PackageJson)],
    requested: Option<&std::collections::BTreeSet<String>>,
    filter_selection: Option<&std::collections::BTreeSet<String>>,
) -> Vec<(String, aube_manifest::PackageJson)> {
    lifecycle_manifests
        .iter()
        .filter(|(importer, _)| match (requested, filter_selection) {
            (Some(requested), _) => requested.contains(importer),
            (None, Some(selected)) => importer == "." || selected.contains(importer),
            (None, None) => true,
        })
        .cloned()
        .collect()
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

/// A local path the workspace graph holds relative to the workspace root,
/// re-expressed relative to the member at `importer_path`. Worked out on the
/// real directories, so a member outside the root (`../app`) gets a path that
/// still names the same target.
fn root_to_member(workspace_root: &Path, importer_path: &str, path: &Path) -> PathBuf {
    let root = aube_util::path::normalize_lexical(workspace_root);
    let target = aube_util::path::normalize_lexical(&root.join(path));
    relative_or_original(&target, &importer_project_dir(&root, importer_path), path)
}

/// The inverse of [`root_to_member`].
fn member_to_root(workspace_root: &Path, importer_path: &str, path: &Path) -> PathBuf {
    let root = aube_util::path::normalize_lexical(workspace_root);
    let target =
        aube_util::path::normalize_lexical(&importer_project_dir(&root, importer_path).join(path));
    relative_or_original(&target, &root, path)
}

fn relative_or_original(target: &Path, base: &Path, original: &Path) -> PathBuf {
    if original.is_absolute() {
        return original.to_path_buf();
    }
    match pathdiff::diff_paths(target, base) {
        Some(rel) if rel.as_os_str().is_empty() => PathBuf::from("."),
        Some(rel) => rel,
        None => original.to_path_buf(),
    }
}

/// The graph a per-project lockfile records for `importer_path`, in that
/// project's frame as pnpm writes it under `sharedWorkspaceLockfile=false`:
/// a sibling member is `link:<path from the project>`, and every `file:` /
/// `link:` path is relative to the project rather than the workspace root.
/// A bare subset knows neither: its sibling importers are gone, so a member
/// dependency serialized as a registry version pnpm cannot find.
fn project_lockfile_graph(
    workspace_root: &Path,
    graph: &aube_lockfile::LockfileGraph,
    importer_path: &str,
    workspace_manifests: &[(String, aube_manifest::PackageJson)],
) -> Option<aube_lockfile::LockfileGraph> {
    let mut subset = graph.subset_to_importer(importer_path, |_| true)?;
    // A runtime pin belongs to the project whose manifest declares it.
    if importer_path != "." {
        let manifest = workspace_manifests
            .iter()
            .find(|(path, _)| path == importer_path)
            .map(|(_, manifest)| manifest);
        let pins = |name: &str| {
            manifest
                .and_then(|manifest| manifest.dev_engines.as_ref())
                .is_some_and(|engines| engines.runtime.iter().any(|r| r.name == name))
        };
        subset.runtimes.retain(|name, _| pins(name));
    }
    // Same `name@version` match the shared-lockfile writer uses to spot a
    // workspace member, including its `0.0.0` default for a versionless one.
    let member_dirs: BTreeMap<String, &str> = workspace_manifests
        .iter()
        .filter(|(path, _)| path != ".")
        .filter_map(|(path, manifest)| {
            let name = manifest.name.as_deref()?;
            let version = manifest.version.as_deref().unwrap_or("0.0.0");
            Some((format!("{name}@{version}"), path.as_str()))
        })
        .collect();
    let mut links = Vec::new();
    for dep in subset.importers.values_mut().flatten() {
        if subset.packages.contains_key(&dep.dep_path) {
            continue;
        }
        let Some(member_dir) = member_dirs.get(&dep.dep_path) else {
            continue;
        };
        let local = aube_lockfile::LocalSource::Link(PathBuf::from(member_dir));
        dep.dep_path = local.dep_path(&dep.name);
        links.push(aube_lockfile::LockedPackage {
            name: dep.name.clone(),
            version: "0.0.0".to_string(),
            dep_path: dep.dep_path.clone(),
            local_source: Some(local),
            ..Default::default()
        });
    }
    for pkg in links {
        subset.packages.insert(pkg.dep_path.clone(), pkg);
    }
    if importer_path != "." {
        subset.rebase_local_paths(|path| root_to_member(workspace_root, importer_path, path));
    }
    Some(subset)
}

/// A member's per-project lockfile, moved into the workspace-root frame the
/// resolver and linker use — the inverse of [`project_lockfile_graph`].
///
/// Earlier versions wrote a member lockfile with every `file:` / `link:` path
/// relative to the workspace root. Such a lockfile is recognised by a direct
/// dep whose version is its specifier resolved from the member (the test the
/// pnpm reader applies to a shared lockfile), and is then read as it is.
pub(crate) fn parse_member_lockfile(
    workspace_root: &Path,
    importer_path: &str,
    manifest: &aube_manifest::PackageJson,
) -> Result<aube_lockfile::LockfileGraph, aube_lockfile::Error> {
    use aube_util::path::normalize_lexical;
    let member_dir = importer_project_dir(workspace_root, importer_path);
    let mut graph = aube_lockfile::parse_lockfile(&member_dir, manifest)?;
    if importer_path == "." {
        return Ok(graph);
    }
    let root_frame = graph.importers.values().flatten().any(|dep| {
        let Some(spec) = dep.specifier.as_deref().and_then(|spec| {
            spec.strip_prefix("file:")
                .or_else(|| spec.strip_prefix("link:"))
        }) else {
            return false;
        };
        let Some(version) = graph
            .packages
            .get(&dep.dep_path)
            .and_then(|pkg| pkg.local_source.as_ref())
            .and_then(|local| local.path())
        else {
            return false;
        };
        let version = normalize_lexical(version);
        normalize_lexical(Path::new(spec)) != version
            && member_to_root(workspace_root, importer_path, Path::new(spec)) == version
    });
    if !root_frame {
        graph.rebase_local_paths(|path| member_to_root(workspace_root, importer_path, path));
    }
    Ok(graph)
}

/// The workspace root when `project_dir` is a member of a workspace that
/// keeps one lockfile per project; `None` for the root itself, a shared
/// lockfile, or no workspace. Install runs at that root, so it holds the
/// member's store, install state and settings.
pub(crate) fn unshared_lockfile_workspace_root(project_dir: &Path) -> Option<PathBuf> {
    let root = crate::dirs::find_workspace_root(project_dir)?;
    let shared = crate::commands::with_settings_ctx(
        &root,
        aube_settings::resolved::shared_workspace_lockfile,
    );
    (root != project_dir && !shared).then_some(root)
}

/// The lockfile of the project at `project_dir`, in the frame its installation
/// used: a member of a workspace with per-project lockfiles reads through
/// [`parse_member_lockfile`], so a local package's key matches the one its
/// store entry was materialized under, and its path is relative to the
/// workspace root [`unshared_lockfile_workspace_root`] names.
pub(crate) fn parse_project_lockfile(
    project_dir: &Path,
    manifest: &aube_manifest::PackageJson,
) -> Result<aube_lockfile::LockfileGraph, aube_lockfile::Error> {
    if let Some(root) = unshared_lockfile_workspace_root(project_dir)
        && let Ok(importer_path) = workspace_importer_path(&root, project_dir)
    {
        return parse_member_lockfile(&root, &importer_path, manifest);
    }
    aube_lockfile::parse_lockfile(project_dir, manifest)
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
/// genuinely new member busts the warm path (via `state::new_workspace_member`,
/// or `member_lockfiles_stale` under `sharedWorkspaceLockfile=false`) and gets
/// a full resolve instead, so this merge only ever runs on the warm path for
/// members already present at the last install.
///
/// pnpm records a catalog entry only in the lockfile of a project that uses
/// it, so the member's catalogs are folded into the graph the drift checks
/// read. A member whose catalog range or runtime pin no longer holds is left
/// out, which makes the graph stale against its manifest, and the first such
/// disagreement is returned for a frozen install to report.
pub(super) fn merge_member_lockfile_graphs(
    workspace_root: &std::path::Path,
    graph: &mut aube_lockfile::LockfileGraph,
    manifests: &[(String, aube_manifest::PackageJson)],
) -> Option<String> {
    let mut mismatch = None;
    for (importer_path, manifest) in manifests {
        if importer_path == "." || graph.importers.contains_key(importer_path) {
            continue;
        }
        let member_graph = match parse_member_lockfile(workspace_root, importer_path, manifest) {
            Ok(member_graph) => member_graph,
            Err(e) => {
                tracing::debug!(
                    "sharedWorkspaceLockfile=false: skipping member {importer_path} graph merge: {e}"
                );
                continue;
            }
        };
        if let Some(reason) = member_config_mismatch(graph, &member_graph, manifest) {
            tracing::debug!(
                "sharedWorkspaceLockfile=false: skipping member {importer_path} graph merge: {reason}"
            );
            mismatch.get_or_insert(format!("{importer_path}: {reason}"));
            continue;
        }
        for (catalog, entries) in member_graph.catalogs {
            let merged = graph.catalogs.entry(catalog).or_default();
            for (name, entry) in entries {
                merged.entry(name).or_insert(entry);
            }
        }
        if let Some(deps) = member_graph.importers.get(".") {
            graph.importers.insert(importer_path.clone(), deps.clone());
        }
        for (dep_path, pkg) in member_graph.packages {
            graph.packages.entry(dep_path).or_insert(pkg);
        }
    }
    mismatch
}

/// Why a member lockfile's recorded config no longer holds, if it doesn't.
///
/// pnpm scopes the rest of this config to each project: a member's `.npmrc`,
/// `pnpm` manifest field and `.pnpmfile.cjs` give it its own settings,
/// overrides, checksums, ignored optionals and patches, so they may differ
/// from the root's. A catalog has one definition, so two ranges for an entry
/// mean one is stale, and a runtime pin answers to its own project's manifest.
fn member_config_mismatch(
    graph: &aube_lockfile::LockfileGraph,
    member: &aube_lockfile::LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> Option<String> {
    for (catalog, entries) in &member.catalogs {
        for (name, entry) in entries {
            // Projects resolve on their own, so only the range must agree.
            if let Some(merged) = graph.catalogs.get(catalog).and_then(|c| c.get(name))
                && merged.specifier != entry.specifier
            {
                return Some(format!(
                    "lockfile records catalogs.{catalog}.{name} as {}, another workspace lockfile as {}",
                    entry.specifier, merged.specifier
                ));
            }
        }
    }
    // Earlier writers copied the root's pin into every member's lockfile.
    let own = aube_lockfile::LockfileGraph {
        runtimes: member
            .runtimes
            .iter()
            .filter(|(name, pin)| graph.runtimes.get(*name) != Some(*pin))
            .map(|(name, pin)| (name.clone(), pin.clone()))
            .collect(),
        ..Default::default()
    };
    match own.check_runtimes_drift(manifest) {
        aube_lockfile::DriftStatus::Stale { reason } => Some(reason),
        aube_lockfile::DriftStatus::Fresh => None,
    }
}

pub(crate) fn order_lifecycle_manifests(
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
/// write path ([`aube_lockfile::write_lockfile_as`]) and
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
    write_per_project_lockfiles_with(
        workspace_root,
        graph,
        workspace_manifests,
        fallback_kind,
        write_selection,
        false,
    )
}

/// [`write_per_project_lockfiles`], where `rewrite` writes each member's file
/// even when its resolved graph is unchanged (`--fix-lockfile`).
pub(super) fn write_per_project_lockfiles_with(
    workspace_root: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    workspace_manifests: &[(String, aube_manifest::PackageJson)],
    fallback_kind: aube_lockfile::LockfileKind,
    write_selection: Option<&std::collections::BTreeSet<String>>,
    rewrite: bool,
) -> miette::Result<()> {
    use miette::IntoDiagnostic;
    let write = if rewrite {
        aube_lockfile::rewrite_lockfile_as
    } else {
        aube_lockfile::write_lockfile_as
    };
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
        let Some(subset) =
            project_lockfile_graph(workspace_root, graph, importer_path, workspace_manifests)
        else {
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
        let written = write(&pkg_dir, &subset, pkg_manifest, write_kind)
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

    /// A member's lockfile records its paths from the member, as pnpm 10
    /// writes it: a sibling member as `link:../a`, a `file:` dep relative to
    /// the member. A bare subset wrote the sibling as a registry `1.0.0` that
    /// `pnpm install --frozen-lockfile` rejects, and `file:` root-relative.
    /// Reading it back must return the root-relative graph the linker uses.
    #[test]
    fn member_lockfile_records_paths_from_the_member_and_reads_back_root_relative() {
        use aube_lockfile::LocalSource;
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("packages/a")).unwrap();
        std::fs::create_dir_all(root.path().join("packages/b")).unwrap();

        let ldir = LocalSource::Directory("ldir".into());
        let ldir_key = ldir.dep_path("ldir");
        let dep = |name: &str, dep_path: &str, specifier: &str| DirectDep {
            name: name.to_string(),
            dep_path: dep_path.to_string(),
            dep_type: DepType::Production,
            specifier: Some(specifier.to_string()),
        };
        let full = LockfileGraph {
            importers: BTreeMap::from([
                (".".to_string(), Vec::new()),
                ("packages/a".to_string(), Vec::new()),
                (
                    "packages/b".to_string(),
                    vec![
                        dep("a", "a@1.0.0", "workspace:*"),
                        dep("ldir", &ldir_key, "file:../../ldir"),
                    ],
                ),
            ]),
            packages: BTreeMap::from([(
                ldir_key.clone(),
                LockedPackage {
                    name: "ldir".to_string(),
                    version: "1.0.0".to_string(),
                    dep_path: ldir_key.clone(),
                    local_source: Some(ldir),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let manifests = vec![
            (".".to_string(), manifest("root")),
            ("packages/a".to_string(), manifest("a")),
            ("packages/b".to_string(), manifest("b")),
        ];
        write_per_project_lockfiles(root.path(), &full, &manifests, LockfileKind::Pnpm, None)
            .unwrap();

        let written =
            std::fs::read_to_string(root.path().join("packages/b/pnpm-lock.yaml")).unwrap();
        for line in ["version: link:../a", "version: file:../../ldir"] {
            assert!(
                written.contains(line),
                "member lockfile lacks `{line}`:\n{written}"
            );
        }

        let mut root_only = LockfileGraph {
            importers: BTreeMap::from([(".".to_string(), Vec::new())]),
            ..Default::default()
        };
        merge_member_lockfile_graphs(root.path(), &mut root_only, &manifests);
        let b = &root_only.importers["packages/b"];
        let local = |name: &str| {
            let dep = b.iter().find(|d| d.name == name).unwrap();
            root_only.packages[&dep.dep_path].local_source.clone()
        };
        assert_eq!(local("a"), Some(LocalSource::Link("packages/a".into())));
        assert_eq!(
            b.iter().find(|d| d.name == "ldir").unwrap().dep_path,
            ldir_key,
            "a `file:` dep must read back under its root-relative key"
        );
    }

    fn local_dir(name: &str, path: &str) -> (String, LockedPackage) {
        let local = aube_lockfile::LocalSource::Directory(path.into());
        let key = local.dep_path(name);
        let pkg = LockedPackage {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            dep_path: key.clone(),
            local_source: Some(local),
            ..Default::default()
        };
        (key, pkg)
    }

    fn file_dep(name: &str, key: &str, specifier: &str) -> DirectDep {
        DirectDep {
            name: name.to_string(),
            dep_path: key.to_string(),
            dep_type: DepType::Production,
            specifier: Some(specifier.to_string()),
        }
    }

    /// A member outside the workspace root (`../app`) still names the same
    /// target: the path is worked out between the real directories, not by
    /// stripping components off two root-relative strings.
    #[test]
    fn parent_relative_member_lockfile_round_trips_its_local_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(tmp.path().join("app")).unwrap();
        let (key, pkg) = local_dir("dep", "vendor/dep");
        let full = LockfileGraph {
            importers: BTreeMap::from([
                (".".to_string(), Vec::new()),
                (
                    "../app".to_string(),
                    vec![file_dep("dep", &key, "file:../workspace/vendor/dep")],
                ),
            ]),
            packages: BTreeMap::from([(key.clone(), pkg)]),
            ..Default::default()
        };
        let manifests = vec![
            (".".to_string(), manifest("root")),
            ("../app".to_string(), manifest("app")),
        ];
        write_per_project_lockfiles(&root, &full, &manifests, LockfileKind::Pnpm, None).unwrap();
        let written = std::fs::read_to_string(tmp.path().join("app/pnpm-lock.yaml")).unwrap();
        assert!(
            written.contains("version: file:../workspace/vendor/dep"),
            "the member lockfile must point at the same directory:\n{written}"
        );

        let mut root_only = LockfileGraph {
            importers: BTreeMap::from([(".".to_string(), Vec::new())]),
            ..Default::default()
        };
        merge_member_lockfile_graphs(&root, &mut root_only, &manifests);
        assert_eq!(root_only.importers["../app"][0].dep_path, key);
    }

    /// A member lockfile written before paths moved into the member's frame
    /// is read as written, transitive local deps included: rebasing only the
    /// ones the importer names would move `vendor/other` under the member.
    #[test]
    fn root_relative_member_lockfile_keeps_its_transitive_local_paths() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("packages/app")).unwrap();
        let (other_key, other) = local_dir("other", "vendor/other");
        let (ldir_key, mut ldir) = local_dir("ldir", "vendor/ldir");
        ldir.dependencies.insert(
            "other".to_string(),
            other_key.strip_prefix("other@").unwrap().to_string(),
        );
        let full = LockfileGraph {
            importers: BTreeMap::from([(
                "packages/app".to_string(),
                vec![file_dep("ldir", &ldir_key, "file:../../vendor/ldir")],
            )]),
            packages: BTreeMap::from([(ldir_key.clone(), ldir), (other_key.clone(), other)]),
            ..Default::default()
        };
        // The earlier writer: a bare subset, paths left root-relative.
        let legacy = full.subset_to_importer("packages/app", |_| true).unwrap();
        aube_lockfile::write_lockfile_as(
            &root.path().join("packages/app"),
            &legacy,
            &manifest("app"),
            LockfileKind::Pnpm,
        )
        .unwrap();

        let manifests = vec![("packages/app".to_string(), manifest("app"))];
        let mut root_only = LockfileGraph::default();
        merge_member_lockfile_graphs(root.path(), &mut root_only, &manifests);
        assert_eq!(root_only.importers["packages/app"][0].dep_path, ldir_key);
        let paths: Vec<_> = root_only
            .packages
            .values()
            .filter_map(|pkg| pkg.local_source.as_ref()?.path().map(|p| p.to_path_buf()))
            .collect();
        assert!(
            paths.contains(&"vendor/other".into()),
            "the transitive local dep must stay at vendor/other, got {paths:?}"
        );
    }

    const PNPM_HEADER: &str = "lockfileVersion: '9.0'\n\nsettings:\n  autoInstallPeers: true\n  excludeLinksFromLockfile: false\n\n";

    fn write_member_lockfile(root: &std::path::Path, member: &str, body: &str) {
        let dir = root.join(member);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pnpm-lock.yaml"), format!("{PNPM_HEADER}{body}")).unwrap();
    }

    fn catalog_member_lockfile(range: &str) -> String {
        format!(
            "catalogs:\n  default:\n    post-only:\n      specifier: {range}\n      version: 1.0.0\n\n\
             importers:\n\n  .:\n    dependencies:\n      post-only:\n        specifier: 'catalog:'\n        version: 1.0.0\n\n\
             packages:\n\n  post-only@1.0.0:\n    resolution: {{integrity: sha512-AAAA}}\n\n\
             snapshots:\n\n  post-only@1.0.0: {{}}\n"
        )
    }

    /// pnpm records a catalog entry only in the lockfile of a project that
    /// uses it, so a member's catalogs must reach the freshness check, and two
    /// ranges for one entry leave the later member out. Everything else pnpm
    /// scopes to the project may differ from the root (`packages/c`).
    #[test]
    fn member_lockfile_config_reaches_the_freshness_check() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("pnpm-lock.yaml"),
            format!("{PNPM_HEADER}importers:\n\n  .: {{}}\n"),
        )
        .unwrap();
        write_member_lockfile(
            root.path(),
            "packages/a",
            &catalog_member_lockfile("^1.0.0"),
        );
        write_member_lockfile(
            root.path(),
            "packages/b",
            &catalog_member_lockfile("^2.0.0"),
        );
        write_member_lockfile(
            root.path(),
            "packages/c",
            "overrides:\n  post-only: 1.0.0\n\npackageExtensionsChecksum: sha256-AAAA\n\npnpmfileChecksum: sha256-BBBB\n\nimporters:\n\n  .: {}\n\nignoredOptionalDependencies:\n  - fsevents\n",
        );
        let manifests = vec![
            (".".to_string(), manifest("root")),
            ("packages/a".to_string(), manifest("a")),
            ("packages/b".to_string(), manifest("b")),
            ("packages/c".to_string(), manifest("c")),
        ];
        let mut graph = aube_lockfile::parse_lockfile(root.path(), &manifests[0].1).unwrap();
        let mismatch = merge_member_lockfile_graphs(root.path(), &mut graph, &manifests);

        let catalog = |range: &str| {
            BTreeMap::from([(
                "default".to_string(),
                BTreeMap::from([("post-only".to_string(), range.to_string())]),
            )])
        };
        assert!(graph.importers.contains_key("packages/a"));
        assert!(matches!(
            graph.check_catalogs_drift(&catalog("^1.0.0")),
            aube_lockfile::DriftStatus::Fresh
        ));
        assert!(
            matches!(
                graph.check_catalogs_drift(&catalog("^2.0.0")),
                aube_lockfile::DriftStatus::Stale { .. }
            ),
            "a catalog range only a member lockfile records must be checked"
        );
        let mismatch = mismatch.expect("packages/b records another catalog range");
        assert!(mismatch.starts_with("packages/b: "), "{mismatch}");
        assert!(!graph.importers.contains_key("packages/b"));
        assert!(
            graph.importers.contains_key("packages/c"),
            "a member's own overrides, checksums and ignored optionals may differ from the root's"
        );
    }

    fn runtime_manifest(version: &str) -> aube_manifest::PackageJson {
        serde_json::from_str(&format!(
            r#"{{"name":"a","version":"1.0.0","devEngines":{{"runtime":{{"name":"node","version":"{version}"}}}}}}"#
        ))
        .unwrap()
    }

    /// A runtime pin answers to its own project's manifest: a member's pin may
    /// be absent from the root's lockfile, but not from the member's manifest.
    /// The writer keeps it there too, leaving the root's pin out of a member.
    #[test]
    fn member_runtime_pin_answers_to_its_own_manifest() {
        let root = tempfile::tempdir().unwrap();
        let lockfile = "importers:\n\n  .:\n    devDependencies:\n      node:\n        specifier: runtime:24.4.0\n        version: runtime:24.4.0\n";
        for member in ["packages/a", "packages/b"] {
            write_member_lockfile(root.path(), member, lockfile);
        }
        let manifests = vec![
            ("packages/a".to_string(), runtime_manifest("24.4.0")),
            ("packages/b".to_string(), manifest("b")),
        ];
        let mut graph = LockfileGraph::default();
        let mismatch = merge_member_lockfile_graphs(root.path(), &mut graph, &manifests)
            .expect("packages/b no longer pins the runtime");
        assert!(mismatch.starts_with("packages/b: "), "{mismatch}");
        assert!(graph.importers.contains_key("packages/a"));
        assert!(graph.runtimes.is_empty());

        let pinned =
            aube_lockfile::parse_lockfile(&root.path().join("packages/a"), &manifests[0].1)
                .unwrap();
        let full = LockfileGraph {
            importers: BTreeMap::from([("packages/b".to_string(), Vec::new())]),
            runtimes: pinned.runtimes,
            ..Default::default()
        };
        write_per_project_lockfiles(
            root.path(),
            &full,
            &manifests[1..],
            LockfileKind::Pnpm,
            None,
        )
        .unwrap();
        let written =
            std::fs::read_to_string(root.path().join("packages/b/pnpm-lock.yaml")).unwrap();
        assert!(!written.contains("runtime:"), "{written}");
    }

    /// pnpm keys a transitive-only local package by its spelling from the
    /// lockfile (`foo@file:../lib`), which names a different directory in
    /// each member: both must survive the merge as separate packages.
    #[test]
    fn members_spelling_a_transitive_local_alike_keep_their_own_packages() {
        let root = tempfile::tempdir().unwrap();
        let body = "importers:\n\n  .:\n    dependencies:\n      parent:\n        specifier: file:../parent\n        version: file:../parent\n\n\
                    packages:\n\n  foo@file:../lib:\n    resolution: {directory: ../lib, type: directory}\n\n  \
                    parent@file:../parent:\n    resolution: {directory: ../parent, type: directory}\n\n\
                    snapshots:\n\n  foo@file:../lib: {}\n\n  parent@file:../parent:\n    dependencies:\n      foo: file:../lib\n";
        let mut manifests = Vec::new();
        for group in ["a", "b"] {
            let member = format!("groups/{group}/app");
            write_member_lockfile(root.path(), &member, body);
            manifests.push((member, manifest(&format!("app-{group}"))));
        }
        let mut graph = LockfileGraph::default();
        assert_eq!(
            merge_member_lockfile_graphs(root.path(), &mut graph, &manifests),
            None
        );

        for group in ["a", "b"] {
            let parent =
                &graph.packages[&graph.importers[&format!("groups/{group}/app")][0].dep_path];
            let foo = &graph.packages[&format!("foo@{}", parent.dependencies["foo"])];
            assert_eq!(
                foo.local_source.as_ref().and_then(|l| l.path()),
                Some(std::path::Path::new(&format!("groups/{group}/lib"))),
                "groups/{group}/app's foo"
            );
        }
    }
}

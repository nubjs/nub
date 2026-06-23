use crate::ResolveTask;
use aube_lockfile::{DepType, DirectDep};
use aube_manifest::PackageJson;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Seed the BFS queue with direct deps from every importer manifest.
///
/// When a package is declared in more than one section
/// (`dependencies` + `devDependencies`, etc.) we keep only the
/// highest-priority entry — `dependencies` > `devDependencies` >
/// `optionalDependencies` > auto-installed `peerDependencies` —
/// matching pnpm, which silently drops the lower-priority duplicates
/// on resolve. Without this the same name gets pushed into the
/// importer's `DirectDep` list twice (once per section), and the
/// linker's parallel step 2 races to create the same
/// `node_modules/<name>` symlink from two tasks, producing an
/// `EEXIST` on the loser.
pub(super) fn seed_direct_deps(
    manifests: &[(String, PackageJson)],
    ignored_optional_dependencies: &BTreeSet<String>,
    auto_install_peers: bool,
    queue: &mut VecDeque<ResolveTask>,
    importers: &mut BTreeMap<String, Vec<DirectDep>>,
) {
    for (importer_path, manifest) in manifests {
        importers.insert(importer_path.clone(), Vec::new());

        for (name, range) in &manifest.dependencies {
            queue.push_back(ResolveTask::root(
                name.clone(),
                range.clone(),
                DepType::Production,
                importer_path.clone(),
            ));
        }
        for (name, range) in &manifest.dev_dependencies {
            if manifest.dependencies.contains_key(name) {
                continue;
            }
            queue.push_back(ResolveTask::root(
                name.clone(),
                range.clone(),
                DepType::Dev,
                importer_path.clone(),
            ));
        }
        for (name, range) in &manifest.optional_dependencies {
            if ignored_optional_dependencies.contains(name) {
                tracing::debug!(
                    "ignoring optional dependency {name} (pnpm.ignoredOptionalDependencies)"
                );
                continue;
            }
            if manifest.dependencies.contains_key(name)
                || manifest.dev_dependencies.contains_key(name)
            {
                continue;
            }
            queue.push_back(ResolveTask::root(
                name.clone(),
                range.clone(),
                DepType::Optional,
                importer_path.clone(),
            ));
        }
        if auto_install_peers {
            for (name, range) in manifest.non_optional_peer_dependencies() {
                if manifest.dependencies.contains_key(name)
                    || manifest.dev_dependencies.contains_key(name)
                    || manifest.optional_dependencies.contains_key(name)
                {
                    continue;
                }
                queue.push_back(ResolveTask::root(
                    name.clone(),
                    range.clone(),
                    DepType::Production,
                    importer_path.clone(),
                ));
            }
        }
    }
}

use super::install;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::HashSet;

#[derive(Debug, Clone, usage_rs::Args)]
pub struct RemoveArgs {
    /// Package(s) to remove
    pub packages: Vec<String>,
    /// Remove only from devDependencies
    #[usage(short = 'D', long)]
    pub save_dev: bool,
    /// Remove from the global install directory instead of the project
    #[usage(short = 'g', long)]
    pub global: bool,
    /// Skip root lifecycle scripts during the chained reinstall
    #[usage(long)]
    pub ignore_scripts: bool,
    /// Remove the dependency from the workspace root's `package.json`.
    ///
    /// Applies regardless of the current working directory: walks up
    /// from cwd looking for `aube-workspace.yaml`, `pnpm-workspace.yaml`,
    /// or a `package.json` with a `workspaces` field and runs the
    /// remove against that directory. Takes precedence over `--filter`
    /// when both are supplied (same as `add --workspace`).
    #[usage(short = 'w', long, conflicts = "--global")]
    pub workspace: bool,
    #[usage(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[usage(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[usage(flatten)]
    pub virtual_store: crate::cli_args::VirtualStoreArgs,
}

pub async fn run(
    args: RemoveArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    args.network.install_overrides();
    args.lockfile.install_overrides();
    args.virtual_store.install_overrides();
    let packages = &args.packages[..];
    if packages.is_empty() {
        return Err(miette!("no packages specified"));
    }

    if !filter.is_empty() && !args.global && !args.workspace {
        return run_filtered(args, &filter).await;
    }

    if args.global {
        return run_global(packages);
    }

    // `--workspace` / `-w`: redirect the remove at the workspace root
    // before anything reads `dirs::cwd()`.
    if args.workspace {
        let start = std::env::current_dir()
            .into_diagnostic()
            .wrap_err("failed to read current dir")?;
        let root = super::find_workspace_root(&start).wrap_err("--workspace")?;
        if root != start {
            std::env::set_current_dir(&root)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to chdir into {}", root.display()))?;
        }
        crate::dirs::set_cwd(&root)?;
    }

    let cwd = crate::dirs::project_root()?;
    let lock = super::take_install_project_lock(&cwd)?;
    let manifest_path = cwd.join("package.json");

    let mut manifest = super::load_manifest(&manifest_path)?;

    for name in packages {
        let removed = if args.save_dev {
            manifest.dev_dependencies.remove(name).is_some()
        } else {
            // Strip from every section. `--save-peer` previously
            // wrote to both peerDependencies and devDependencies so
            // both need clearing on a full uninstall.
            let from_deps = manifest.dependencies.remove(name).is_some();
            let from_dev = manifest.dev_dependencies.remove(name).is_some();
            let from_optional = manifest.optional_dependencies.remove(name).is_some();
            let from_peer = manifest.peer_dependencies.remove(name).is_some();
            from_deps || from_dev || from_optional || from_peer
        };

        // Also prune sidecar metadata so a later `aube add <name>`
        // does not silently inherit the old entries. Main concern is
        // pnpm.allowBuilds. If user removes a build-script package
        // then later adds a malicious package with the same name
        // (typo-squat, name reclaim), the old allowBuilds entry
        // would auto-approve its postinstall. Same hazard, lower
        // risk, for overrides and resolutions which just leave dead
        // rewrite rules around. Matches pnpm remove behavior.
        prune_sidecar_entries(&mut manifest, name);

        if !removed {
            let section = if args.save_dev {
                "a devDependency"
            } else {
                "a dependency"
            };
            return Err(miette!("package '{name}' is not {section}"));
        }
    }

    // Build and validate resolver configuration before persisting the
    // manifest mutation. A malformed policy must leave package.json and
    // the installed graph in their original, consistent state.
    let existing = aube_lockfile::parse_lockfile(&cwd, &manifest).ok();
    let workspace_catalogs = super::load_workspace_catalogs(&cwd)?;
    // Built for its validation alone — the removal path never resolves
    // standalone, so a malformed policy still fails before package.json
    // is touched.
    let _resolver = super::build_resolver(&cwd, &manifest, workspace_catalogs.clone())?;

    // Write updated package.json atomically. Crash mid-write would
    // otherwise truncate the user manifest, worst-case aube failure
    // mode. Tempfile + persist keeps the swap atomic.
    //
    // We mutate the parsed JSON object in place rather than going
    // through `sync_manifest_dep_sections`. The latter rebuilds each
    // dep section from `BTreeMap`, which would alphabetize the keys
    // and reshuffle the user's manifest as a side-effect of removing
    // an unrelated entry. `aube remove` must only touch the names the
    // user named — surrounding entries stay in their original on-disk
    // order. (`aube add` keeps using the BTreeMap path because it
    // both inserts and is expected to land new entries in a stable
    // sorted spot.)
    let dep_sections: &[&str] = if args.save_dev {
        &["devDependencies"]
    } else {
        &[
            "dependencies",
            "devDependencies",
            "optionalDependencies",
            "peerDependencies",
        ]
    };
    super::update_manifest_json_object(&manifest_path, |obj| {
        for section_key in dep_sections {
            let Some(section) = obj.get_mut(*section_key).and_then(|v| v.as_object_mut()) else {
                continue;
            };
            for name in packages {
                // shift_remove rather than remove: serde_json's `Map`
                // is an `IndexMap` under the `preserve_order` feature
                // and the default `remove` is `swap_remove`, which
                // would scramble the surviving keys. shift_remove
                // keeps every other entry in its on-disk position.
                section.shift_remove(name);
            }
            if section.is_empty() {
                obj.shift_remove(*section_key);
            }
        }
        for name in packages {
            prune_sidecar_entries_json(obj, name);
        }
        Ok(())
    })?;
    for name in packages {
        eprintln!("  - {name}");
    }
    eprintln!("Updated package.json");

    // Removing a direct dependency normally needs no registry work: trim
    // that importer's roots and garbage-collect packages that are no longer
    // reachable. Anything that path declines — a shared-workspace graph, a
    // surviving package that took a peer from the removed one, patch or
    // catalog drift — falls through to the install pipeline below.
    let workspace_config = aube_manifest::WorkspaceConfig::load(&cwd)
        .map_err(miette::Report::new)
        .wrap_err("failed to load workspace config")?;
    let lockfile_kind = aube_lockfile::detect_existing_lockfile_kind(&cwd)
        .unwrap_or(aube_lockfile::LockfileKind::Aube);
    let patch_status = existing
        .as_ref()
        .map(|graph| install::check_patch_drift(&cwd, graph, lockfile_kind))
        .transpose()?;
    let mut graph = existing
        .as_ref()
        .and_then(|graph| prune_removed_dependencies(graph, &manifest, packages))
        .filter(|graph| {
            matches!(patch_status, Some(aube_lockfile::DriftStatus::Fresh))
                && matches!(
                    graph.check_drift_for_kind(
                        &manifest,
                        &workspace_config.overrides,
                        &workspace_config.ignored_optional_dependencies,
                        &workspace_catalogs,
                        lockfile_kind,
                    ),
                    aube_lockfile::DriftStatus::Fresh
                )
                && matches!(
                    graph.check_catalogs_drift(&workspace_catalogs),
                    aube_lockfile::DriftStatus::Fresh
                )
        });
    let used_lockfile_prune = graph.is_some();
    if let Some(graph) = graph.as_mut() {
        eprintln!("Pruned lockfile to {} packages", graph.packages.len());
        install::finalize_lockfile_graph(&cwd, graph, &manifest, false, None).await?;
        super::write_and_log_lockfile(&cwd, graph, &manifest)?;
    }

    // Without the fast path, re-resolve + relink through the install
    // pipeline, the same way `add` chains into `install::run` after mutating
    // the manifest. The pipeline is the only path that seeds the resolver
    // with the local workspace packages (`discover_workspace_plan` →
    // `resolve_workspace` with `ws_package_versions`), so a sibling dep
    // declared `workspace:*` resolves to its local copy instead of being
    // looked up on the registry. Resolving standalone here passes a single
    // `.` importer and an empty workspace map, which makes any surviving
    // `workspace:*` dependency fail with ERR_AUBE_NO_MATCHING_VERSION — and
    // leaves the manifest edited but the lockfile stale.
    //
    // `Fix` (vs `Prefer`) re-resolves while seeding from the existing
    // lockfile, so unchanged specs keep their pinned versions and only
    // the removed entry (and anything that depended on it) drops out —
    // matching `add`'s post-mutation contract. `with_mode()` already
    // skips the root lifecycle hooks (chained-call contract).
    let mode = if used_lockfile_prune {
        install::FrozenMode::Frozen
    } else {
        super::chained_frozen_mode(install::FrozenMode::Fix)
    };
    let mut opts = install::InstallOptions::with_mode(mode);
    opts.ignore_scripts = args.ignore_scripts;
    if let Some(graph) = graph.as_ref() {
        // A verified index means every referenced CAS shard exists. Keep the
        // normal remove path offline only when the entire retained registry
        // graph is ready locally; otherwise let one online install repair it.
        // Starting in the right mode avoids rerunning lifecycle side effects.
        let store = super::open_store(&cwd)?;
        let fully_cached = graph.packages.values().all(|pkg| {
            pkg.local_source.as_ref().map_or_else(
                || {
                    store
                        .load_index_verified(
                            pkg.registry_name(),
                            &pkg.version,
                            pkg.integrity.as_deref(),
                        )
                        .is_some()
                },
                // file/link/portal/exec sources can be rematerialized from
                // disk. Git and URL sources use their own remote caches and
                // may need the network when those caches are cold.
                |local| local.path().is_some(),
            )
        });
        opts.network_mode = if fully_cached {
            aube_registry::NetworkMode::Offline
        } else {
            aube_registry::NetworkMode::Online
        };
    }
    install::run_with_project_lock(opts, &lock).await?;

    Ok(())
}

/// Remove direct roots from a single-importer lockfile without consulting the
/// registry. Shared workspace graphs stay on the resolver path because the
/// current command may represent a member importer rather than `.`.
///
/// A removed package that supplied a peer to a surviving package also forces
/// resolution: peer-context suffixes encode the provider version, so merely
/// pruning the root could leave a stale contextualized dep path behind.
fn prune_removed_dependencies(
    graph: &aube_lockfile::LockfileGraph,
    manifest: &aube_manifest::PackageJson,
    packages: &[String],
) -> Option<aube_lockfile::LockfileGraph> {
    if graph.importers.len() != 1 || !graph.importers.contains_key(".") {
        return None;
    }
    let removed: HashSet<&str> = packages.iter().map(String::as_str).collect();
    // Removing a manifest override changes resolution intent, not just graph
    // reachability. Let the resolver rebuild that case so the lockfile header
    // cannot retain an override that package.json no longer declares.
    if graph
        .overrides
        .keys()
        .any(|selector| removed.contains(selector.as_str()))
    {
        return None;
    }
    let mut pruned = graph.filter_deps(|dep| {
        !removed.contains(dep.name.as_str()) || manifest_direct_dep(manifest, &dep.name).is_some()
    });
    // `--save-dev` can reveal a lower-priority declaration of the same name.
    // Retain its locked package but rewrite the importer metadata to match the
    // surviving manifest section and specifier.
    for dep in pruned.importers.get_mut(".").into_iter().flatten() {
        if removed.contains(dep.name.as_str())
            && let Some((dep_type, specifier)) = manifest_direct_dep(manifest, &dep.name)
        {
            let range = specifier.strip_prefix("workspace:").unwrap_or(specifier);
            let range = range.strip_prefix("npm:").map_or(range, |alias| {
                alias.rsplit_once('@').map_or("", |(_, range)| range)
            });
            let locked_version = pruned.packages.get(&dep.dep_path).map(|pkg| &pkg.version);
            let compatible = locked_version
                .and_then(|version| node_semver::Version::parse(version).ok())
                .zip(node_semver::Range::parse(range).ok())
                .is_some_and(|(version, range)| version.satisfies(&range));
            if !compatible {
                return None;
            }
            dep.dep_type = dep_type;
            dep.specifier = Some(specifier.to_string());
        }
    }
    if pruned.packages.values().any(|pkg| {
        pkg.peer_dependencies
            .keys()
            .any(|name| removed.contains(name.as_str()))
    }) {
        return None;
    }
    Some(pruned)
}

fn manifest_direct_dep<'a>(
    manifest: &'a aube_manifest::PackageJson,
    name: &str,
) -> Option<(aube_lockfile::DepType, &'a str)> {
    manifest
        .dependencies
        .get(name)
        .map(|range| (aube_lockfile::DepType::Production, range.as_str()))
        .or_else(|| {
            manifest
                .dev_dependencies
                .get(name)
                .map(|range| (aube_lockfile::DepType::Dev, range.as_str()))
        })
        .or_else(|| {
            manifest
                .optional_dependencies
                .get(name)
                .map(|range| (aube_lockfile::DepType::Optional, range.as_str()))
        })
}

async fn run_filtered(
    args: RemoveArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    let cwd = crate::dirs::cwd()?;
    let (_root, matched) = super::select_workspace_packages(&cwd, filter, "remove")?;
    let result = async {
        for pkg in matched {
            // Match pnpm's recursive-remove semantics: silently skip
            // projects that don't declare any of the named packages,
            // and per-project narrow the package list to just the
            // ones present so a partial overlap (e.g. `aube -r remove
            // pkg1 pkg2` against a project that only declares `pkg1`)
            // doesn't trip the strict "package is not a dependency"
            // error in `run` after the first mutation has already
            // landed. The single-project (`aube remove`) path keeps
            // the strict per-package error so an isolated typo in
            // one shell still fails fast.
            let present = manifest_present_deps(&pkg.manifest, &args.packages, args.save_dev);
            if present.is_empty() {
                continue;
            }
            super::retarget_cwd(&pkg.dir)?;
            let mut narrowed = args.clone();
            narrowed.packages = present;
            Box::pin(run(
                narrowed,
                aube_workspace::selector::EffectiveFilter::default(),
            ))
            .await?;
        }
        Ok(())
    }
    .await;
    super::finish_filtered_workspace(&cwd, result)
}

fn manifest_present_deps(
    manifest: &aube_manifest::PackageJson,
    packages: &[String],
    save_dev: bool,
) -> Vec<String> {
    packages
        .iter()
        .filter(|name| {
            if save_dev {
                manifest.dev_dependencies.contains_key(*name)
            } else {
                manifest.dependencies.contains_key(*name)
                    || manifest.dev_dependencies.contains_key(*name)
                    || manifest.optional_dependencies.contains_key(*name)
                    || manifest.peer_dependencies.contains_key(*name)
            }
        })
        .cloned()
        .collect()
}

/// `aube remove -g <pkg>...` — delete globally-installed packages and
/// unlink their bins. Each named package is looked up in the global pkg
/// dir; if found, the whole install (hash symlink + physical dir + bins)
/// is removed atomically.
fn run_global(packages: &[String]) -> miette::Result<()> {
    let layout = super::global::GlobalLayout::resolve()?;

    let mut any_removed = false;
    for name in packages {
        match super::global::find_package(&layout.pkg_dir, name) {
            Some(info) => {
                // Nothing to keep: `remove -g` is not replacing this package,
                // so every bin it owns should go.
                super::global::remove_package(&info, &layout, &std::collections::BTreeSet::new())?;
                eprintln!("Removed global {name}");
                any_removed = true;
            }
            None => {
                eprintln!("Not globally installed: {name}");
            }
        }
    }
    if !any_removed {
        return Err(miette!("no matching global packages were removed"));
    }
    Ok(())
}

fn prune_sidecar_entries_json(obj: &mut serde_json::Map<String, serde_json::Value>, name: &str) {
    // shift_remove (not remove → swap_remove) keeps the surrounding
    // keys in their original on-disk position. Same rationale as the
    // dep-section pruning above: `remove` must not reshuffle the user's
    // manifest as a side effect.
    // Compat namespaces plus the embedder's own (standalone aube →
    // ["pnpm", "aube"]); an embedder with no namespace ("") skips it.
    let id = aube_util::embedder();
    let ns_keys = id
        .compatible_names
        .iter()
        .copied()
        .chain(std::iter::once(id.manifest_namespace).filter(|ns| !ns.is_empty()));
    for ns_key in ns_keys {
        let remove_ns = if let Some(ns) = obj.get_mut(ns_key).and_then(|v| v.as_object_mut()) {
            for map_key in ["allowBuilds", "overrides", "peerDependencyRules"] {
                if let Some(inner) = ns.get_mut(map_key).and_then(|v| v.as_object_mut()) {
                    inner.shift_remove(name);
                    if inner.is_empty() {
                        ns.shift_remove(map_key);
                    }
                }
            }
            for arr_key in [
                "onlyBuiltDependencies",
                "neverBuiltDependencies",
                "trustedDependencies",
            ] {
                if let Some(arr) = ns.get_mut(arr_key).and_then(|v| v.as_array_mut()) {
                    arr.retain(|entry| match entry.as_str() {
                        Some(s) => s.rsplit_once('@').map(|(base, _)| base).unwrap_or(s) != name,
                        None => true,
                    });
                    if arr.is_empty() {
                        ns.shift_remove(arr_key);
                    }
                }
            }
            ns.is_empty()
        } else {
            false
        };
        if remove_ns {
            obj.shift_remove(ns_key);
        }
    }

    for top_key in ["overrides", "resolutions"] {
        let remove_top = if let Some(top) = obj.get_mut(top_key).and_then(|v| v.as_object_mut()) {
            top.shift_remove(name);
            top.is_empty()
        } else {
            false
        };
        if remove_top {
            obj.shift_remove(top_key);
        }
    }

    // The manifest-root build allowlist is keyed by package name OR by a
    // pinned `name@<spec>` form, so a removed dep must lose every key that
    // targets it — otherwise the grant outlives the dependency and silently
    // re-applies when that version comes back. npm's `approve-scripts` writes
    // the pinned form by DEFAULT, so an exact-name removal would miss the
    // common case entirely.
    let remove_allow = if let Some(top) = obj
        .get_mut(aube_manifest::ROOT_ALLOW_SCRIPTS_KEY)
        .and_then(|v| v.as_object_mut())
    {
        top.retain(|key, _| !allow_key_targets(key, name));
        top.is_empty()
    } else {
        false
    };
    if remove_allow {
        obj.shift_remove(aube_manifest::ROOT_ALLOW_SCRIPTS_KEY);
    }
}

/// Whether a build-allowlist key targets the package `name`: either the bare
/// name, or any `name@<spec>` pin (`pkg@1.2.3`, `pkg@1 || 2`, `pkg@file:./x`,
/// `pkg@git+https://…`).
///
/// Anchored on the `@` separator rather than a bare prefix test, which is what
/// keeps `is-odd` from matching `is-odd-2`, and splitting from the LEFT rather
/// than the right, which is what keeps a scoped bare name (`@scope/pkg`, whose
/// only `@` is its first character) from being read as a pin.
fn allow_key_targets(key: &str, name: &str) -> bool {
    key == name
        || key
            .strip_prefix(name)
            .is_some_and(|rest| rest.starts_with('@'))
}

/// Prune aube/pnpm sidecar metadata entries that reference `name`.
/// Covers pnpm.allowBuilds, pnpm.onlyBuiltDependencies,
/// pnpm.neverBuiltDependencies, pnpm.overrides, aube.* mirrors,
/// top-level overrides, yarn resolutions, and the manifest-root build
/// allowlist. Also removes the whole
/// namespace block if its last entry was the one we just dropped.
/// Safe no-op if the manifest has none of these fields.
fn prune_sidecar_entries(manifest: &mut aube_manifest::PackageJson, name: &str) {
    // Namespaced allowlists, overrides, denylists: the compat namespaces
    // plus the embedder's own (standalone aube → pnpm.* / aube.*); an
    // embedder with no namespace ("") skips it.
    let id = aube_util::embedder();
    let ns_keys = id
        .compatible_names
        .iter()
        .copied()
        .chain(std::iter::once(id.manifest_namespace).filter(|ns| !ns.is_empty()));
    for ns_key in ns_keys {
        let Some(ns) = manifest.extra.get_mut(ns_key) else {
            continue;
        };
        let Some(obj) = ns.as_object_mut() else {
            continue;
        };
        // Map-shape fields: key is package name.
        for map_key in ["allowBuilds", "overrides", "peerDependencyRules"] {
            if let Some(inner) = obj.get_mut(map_key).and_then(|v| v.as_object_mut()) {
                inner.remove(name);
                // peerDependencyRules has nested allowedVersions,
                // ignoreMissing. Only clean the outer pkg-keyed
                // entries, deeper structures are author-controlled.
                if inner.is_empty() {
                    obj.remove(map_key);
                }
            }
        }
        // Array-shape fields: whole entries match name or name@ver.
        for arr_key in [
            "onlyBuiltDependencies",
            "neverBuiltDependencies",
            "trustedDependencies",
        ] {
            if let Some(arr) = obj.get_mut(arr_key).and_then(|v| v.as_array_mut()) {
                arr.retain(|entry| match entry.as_str() {
                    Some(s) => {
                        // "pkg" stays only if it is not our name.
                        // "pkg@range" stays only if pkg is not ours.
                        let base = s.rsplit_once('@').map(|(a, _)| a).unwrap_or(s);
                        base != name
                    }
                    None => true,
                });
                if arr.is_empty() {
                    obj.remove(arr_key);
                }
            }
        }
        // Drop the whole pnpm/aube block if we emptied it completely.
        if obj.is_empty() {
            manifest.extra.remove(ns_key);
        }
    }
    // Top-level `overrides` (npm + pnpm both accept it here).
    if let Some(top) = manifest
        .extra
        .get_mut("overrides")
        .and_then(|v| v.as_object_mut())
    {
        top.remove(name);
        if top.is_empty() {
            manifest.extra.remove("overrides");
        }
    }
    // yarn `resolutions` at top level.
    if let Some(top) = manifest
        .extra
        .get_mut("resolutions")
        .and_then(|v| v.as_object_mut())
    {
        top.remove(name);
        if top.is_empty() {
            manifest.extra.remove("resolutions");
        }
    }
    // The manifest-root build allowlist, for the same reason as above: a grant
    // must not outlive the dependency it was written for. Pinned `name@<spec>`
    // keys count — npm writes those by default.
    if let Some(top) = manifest
        .extra
        .get_mut(aube_manifest::ROOT_ALLOW_SCRIPTS_KEY)
        .and_then(|v| v.as_object_mut())
    {
        top.retain(|key, _| !allow_key_targets(key, name));
        if top.is_empty() {
            manifest.extra.remove(aube_manifest::ROOT_ALLOW_SCRIPTS_KEY);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::prune_removed_dependencies;
    use aube_lockfile::{DepType, DirectDep, LockedPackage, LockfileGraph};
    use serde_json::Value;

    /// The allowlist key grammar, exercised on the shapes that actually occur.
    /// The two rows that matter are the last two: a same-prefix sibling must
    /// survive (`is-odd` never prunes `is-odd-2`), and a scoped bare name must
    /// not be mistaken for a pin just because it contains an `@`.
    #[test]
    fn allow_key_matching_is_anchored_on_the_pin_separator() {
        for (key, name, targets) in [
            ("canvas", "canvas", true),
            ("canvas@2.11.0", "canvas", true),
            ("esbuild@0.19.0 || 0.20.0", "esbuild", true),
            ("buildy@file:./buildy-1.0.0.tgz", "buildy", true),
            ("pkg@git+https://example.com/pkg.git", "pkg", true),
            ("@scope/pkg", "@scope/pkg", true),
            ("@scope/pkg@1.0.0", "@scope/pkg", true),
            ("is-odd-2", "is-odd", false),
            ("is-odd-2@1.0.0", "is-odd", false),
            ("@scope/pkg-2@1.0.0", "@scope/pkg", false),
            ("other", "canvas", false),
        ] {
            assert_eq!(
                super::allow_key_targets(key, name),
                targets,
                "key {key:?} vs name {name:?}"
            );
        }
    }

    fn direct(name: &str, version: &str, dep_type: DepType) -> DirectDep {
        DirectDep {
            name: name.to_string(),
            dep_path: format!("{name}@{version}"),
            dep_type,
            specifier: Some(version.to_string()),
        }
    }

    fn locked(name: &str, version: &str) -> LockedPackage {
        LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            dep_path: format!("{name}@{version}"),
            ..Default::default()
        }
    }

    #[test]
    fn lockfile_prune_keeps_only_reachable_packages() {
        let mut graph = LockfileGraph::default();
        graph.importers.insert(
            ".".to_string(),
            vec![
                direct("remove-me", "1.0.0", DepType::Production),
                direct("keep-me", "1.0.0", DepType::Production),
            ],
        );
        let mut removed = locked("remove-me", "1.0.0");
        removed
            .dependencies
            .insert("orphan".to_string(), "1.0.0".to_string());
        graph.packages.insert(removed.dep_path.clone(), removed);
        for pkg in [locked("orphan", "1.0.0"), locked("keep-me", "1.0.0")] {
            graph.packages.insert(pkg.dep_path.clone(), pkg);
        }

        let manifest = aube_manifest::PackageJson::default();
        let pruned = prune_removed_dependencies(&graph, &manifest, &["remove-me".to_string()])
            .expect("single-importer graph without affected peers should prune");
        assert_eq!(pruned.importers["."].len(), 1);
        assert!(pruned.packages.contains_key("keep-me@1.0.0"));
        assert!(!pruned.packages.contains_key("remove-me@1.0.0"));
        assert!(!pruned.packages.contains_key("orphan@1.0.0"));
    }

    #[test]
    fn lockfile_prune_falls_back_when_removed_dep_supplied_peer() {
        let mut graph = LockfileGraph::default();
        graph.importers.insert(
            ".".to_string(),
            vec![
                direct("react", "18.0.0", DepType::Production),
                direct("plugin", "1.0.0", DepType::Production),
            ],
        );
        let mut plugin = locked("plugin", "1.0.0");
        plugin
            .peer_dependencies
            .insert("react".to_string(), "^18".to_string());
        for pkg in [locked("react", "18.0.0"), plugin] {
            graph.packages.insert(pkg.dep_path.clone(), pkg);
        }

        let manifest = aube_manifest::PackageJson::default();
        assert!(prune_removed_dependencies(&graph, &manifest, &["react".to_string()]).is_none());
    }

    #[test]
    fn lockfile_prune_falls_back_when_removed_dep_had_override() {
        let mut graph = LockfileGraph::default();
        graph.importers.insert(
            ".".to_string(),
            vec![direct("remove-me", "1.0.0", DepType::Production)],
        );
        graph
            .overrides
            .insert("remove-me".to_string(), "1.0.0".to_string());

        let manifest = aube_manifest::PackageJson::default();
        assert!(
            prune_removed_dependencies(&graph, &manifest, &["remove-me".to_string()]).is_none()
        );
    }

    #[test]
    fn lockfile_prune_retypes_a_surviving_overlapping_declaration() {
        let mut graph = LockfileGraph::default();
        graph.importers.insert(
            ".".to_string(),
            vec![direct("shared", "1.0.0", DepType::Dev)],
        );
        graph
            .packages
            .insert("shared@1.0.0".to_string(), locked("shared", "1.0.0"));
        let mut manifest = aube_manifest::PackageJson::default();
        manifest
            .optional_dependencies
            .insert("shared".to_string(), "^1.0.0".to_string());

        let pruned = prune_removed_dependencies(&graph, &manifest, &["shared".to_string()])
            .expect("surviving optional declaration should stay locked");
        let dep = &pruned.importers["."][0];
        assert_eq!(dep.dep_type, DepType::Optional);
        assert_eq!(dep.specifier.as_deref(), Some("^1.0.0"));
        assert!(pruned.packages.contains_key("shared@1.0.0"));
    }

    #[test]
    fn lockfile_prune_rejects_an_incompatible_overlapping_declaration() {
        let mut graph = LockfileGraph::default();
        graph.importers.insert(
            ".".to_string(),
            vec![direct("shared", "1.0.0", DepType::Dev)],
        );
        graph
            .packages
            .insert("shared@1.0.0".to_string(), locked("shared", "1.0.0"));
        let mut manifest = aube_manifest::PackageJson::default();
        manifest
            .dependencies
            .insert("shared".to_string(), "^2.0.0".to_string());

        assert!(prune_removed_dependencies(&graph, &manifest, &["shared".to_string()]).is_none());
    }

    fn collect_section_order(raw: &str, section: &str) -> Vec<String> {
        let v: Value = serde_json::from_str(raw).unwrap();
        let obj = v.as_object().unwrap().get(section).unwrap();
        obj.as_object().unwrap().keys().cloned().collect()
    }

    /// Regression: `aube remove` previously rebuilt every dep section
    /// from `BTreeMap`, alphabetizing the surviving entries even
    /// though the user only asked to drop one name. This test exercises
    /// the in-place pruning path used by `update_manifest_json_object`
    /// to confirm the surrounding keys stay in their original order.
    #[test]
    fn remove_preserves_dep_order_in_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.json");
        std::fs::write(
            &path,
            r#"{
  "name": "example",
  "dependencies": {
    "zod": "^3.22.0",
    "axios": "^1.6.0",
    "lodash": "^4.17.21",
    "react": "^18.2.0"
  }
}
"#,
        )
        .unwrap();

        crate::commands::update_manifest_json_object(&path, |obj| {
            let dep_sections: &[&str] = &[
                "dependencies",
                "devDependencies",
                "optionalDependencies",
                "peerDependencies",
            ];
            for section_key in dep_sections {
                let Some(section) = obj.get_mut(*section_key).and_then(|v| v.as_object_mut())
                else {
                    continue;
                };
                section.shift_remove("axios");
                if section.is_empty() {
                    obj.shift_remove(*section_key);
                }
            }
            Ok(())
        })
        .unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            collect_section_order(&written, "dependencies"),
            ["zod", "lodash", "react"],
            "remove must keep on-disk order — got:\n{written}"
        );
    }
}

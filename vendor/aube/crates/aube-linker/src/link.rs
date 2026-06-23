use tracing::trace;

use crate::patches::{
    applied_patches_sidecar_name, current_patch_hashes, read_applied_patches,
    wipe_changed_patched_entries, write_applied_patches,
};
use crate::pool::with_link_pool;
use crate::sweep::{
    EntryState, classify_entry_state, is_physical_importer, mkdirp, remove_hidden_hoist_tree,
    sweep_dead_hidden_hoist_entries, sweep_stale_tmp_dirs, sweep_stale_top_level_entries,
    try_remove_entry,
};
use crate::{Error, HoistedPlacements, LinkStats, Linker, NodeLinker, hoisted, sys};
use aube_lockfile::{LocalSource, LockfileGraph};
use aube_store::PackageIndex;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

impl Linker {
    fn without_global_virtual_store(&self) -> Self {
        Self {
            virtual_store: self.virtual_store.clone(),
            store: self.store.clone(),
            use_global_virtual_store: false,
            strategy: self.strategy,
            patches: self.patches.clone(),
            hashes: self.hashes.clone(),
            virtual_store_dir_max_length: self.virtual_store_dir_max_length,
            shamefully_hoist: self.shamefully_hoist,
            public_hoist_patterns: self.public_hoist_patterns.clone(),
            public_hoist_negations: self.public_hoist_negations.clone(),
            hoist: self.hoist,
            hoist_patterns: self.hoist_patterns.clone(),
            hoist_negations: self.hoist_negations.clone(),
            hoist_workspace_packages: self.hoist_workspace_packages,
            hoisting_limits: self.hoisting_limits,
            dedupe_direct_deps: self.dedupe_direct_deps,
            node_linker: self.node_linker,
            modules_dir_name: self.modules_dir_name.clone(),
            aube_dir_override: self.aube_dir_override.clone(),
            link_concurrency: self.link_concurrency,
            virtual_store_only: self.virtual_store_only,
        }
    }

    /// Link all packages into node_modules for the given project.
    pub fn link_all(
        &self,
        project_dir: &Path,
        graph: &LockfileGraph,
        package_indices: &BTreeMap<String, PackageIndex>,
    ) -> Result<LinkStats, Error> {
        if matches!(self.node_linker, NodeLinker::Hoisted) {
            let mut stats = LinkStats::default();
            let mut placements = HoistedPlacements::default();
            hoisted::link_hoisted_importer(
                self,
                hoisted::HoistedImporterDirs {
                    root: project_dir,
                    importer: project_dir,
                },
                graph.root_deps(),
                graph,
                package_indices,
                &mut stats,
                &mut placements,
            )?;
            // Hoisted mode doesn't use the isolated `.aube/` virtual
            // store, so a hidden hoist tree under `.aube/node_modules/`
            // has no consumer. If a previous isolated install left one
            // behind, sweep it — hoisted's top-level cleanup preserves
            // dotfiles, so it wouldn't be removed otherwise, and a
            // stale tree would keep satisfying phantom deps for any
            // leftover `.aube/<dep_path>/` directories until their
            // eventual cleanup. Honors `virtualStoreDir`.
            let _ = crate::remove_dir_all_with_retry(
                &self.aube_dir_for(project_dir).join("node_modules"),
            );
            stats.hoisted_placements = Some(placements);
            return Ok(stats);
        }

        if self.use_global_virtual_store && self.hoist {
            remove_hidden_hoist_tree(&self.virtual_store.join("node_modules"));
            return self.without_global_virtual_store().link_all(
                project_dir,
                graph,
                package_indices,
            );
        }

        let nm = project_dir.join(&self.modules_dir_name);
        let aube_dir = self.aube_dir_for(project_dir);

        mkdirp(&aube_dir)?;

        // Reclaim space from prior aborted installs. A crash or
        // Ctrl+C between materialize_into and the atomic rename
        // leaves `.tmp-<pid>-*` dirs in the virtual store. Sweep
        // them now so the current install starts clean.
        sweep_stale_tmp_dirs(&aube_dir);

        // Clean up stale top-level entries not in the current graph.
        // With shamefully_hoist, every package name in the graph is
        // also a legitimate top-level entry, so fold those into the
        // preserve set before sweeping. Scoped packages live under
        // `node_modules/@scope/<pkg>`, but `read_dir` on `node_modules`
        // yields the bare `@scope` directory — so we build a second
        // set of scope prefixes and preserve any entry that matches.
        let mut root_dep_names: std::collections::HashSet<&str> =
            graph.root_deps().iter().map(|d| d.name.as_str()).collect();
        if self.shamefully_hoist {
            for pkg in graph.packages.values() {
                root_dep_names.insert(pkg.name.as_str());
            }
        } else if !self.public_hoist_patterns.is_empty() {
            for pkg in graph.packages.values() {
                if pkg.local_source.is_none() && self.public_hoist_matches(&pkg.name) {
                    root_dep_names.insert(pkg.name.as_str());
                }
            }
        }
        // Preserve the virtual-store leaf name when `aube_dir` sits
        // directly under `nm`. With the default `.aube` the dotfile
        // check inside the sweep covers it, but a user who sets
        // `virtualStoreDir=node_modules/vstore` would otherwise see
        // the sweep delete the freshly-`mkdirp`d virtual store on
        // every install because `vstore` isn't a dotfile and isn't
        // in `root_dep_names`.
        let aube_dir_leaf: Option<std::ffi::OsString> = if aube_dir.parent() == Some(nm.as_path()) {
            aube_dir.file_name().map(|s| s.to_owned())
        } else {
            None
        };
        sweep_stale_top_level_entries(&nm, &root_dep_names, aube_dir_leaf.as_deref());

        let mut stats = LinkStats::default();

        // Reconcile previously-applied patches against the current
        // `self.patches` set. Without graph hashes (CI / no-global-store
        // mode) the `.aube/<dep_path>` directory name doesn't change
        // when a patch is added or removed, so the simple "exists?
        // skip!" check would otherwise leave stale patched bytes in
        // place after `aube patch-remove` or fail to apply a brand new
        // patch after `aube patch-commit`. We track the per-`(name,
        // version)` patch fingerprint in a sidecar file under
        // `node_modules/` and wipe the matching `.aube/<dep_path>`
        // entries whenever the fingerprint changes.
        let prev_applied = read_applied_patches(&nm);
        let curr_applied = current_patch_hashes(&self.patches);
        if !self.use_global_virtual_store {
            wipe_changed_patched_entries(
                &aube_dir,
                graph,
                &prev_applied,
                &curr_applied,
                self.virtual_store_dir_max_length,
            );
        }

        let nested_link_targets = build_nested_link_targets(project_dir, graph);

        // Step 1: Populate .aube virtual store
        //
        // `file:` / `link:` / `portal:` / `exec:` sources resolve
        // against a path inside the project, so they never go into the
        // shared global virtual store — we materialize them straight
        // into per-project `.aube/` below. `link:` entries don't need
        // any `.aube/` entry at all; their top-level symlink points
        // directly at the target.
        //
        // Git and remote-tarball sources are the exception: they're
        // pinned to immutable content and shared across projects like
        // registry packages. Under the global virtual store they must
        // be materialized into the shared store (and have their
        // `.aube/<dep_path>` entry symlinked at it) so a registry
        // dependent that also lives in the shared store doesn't get a
        // dangling sibling symlink to them.
        for (dep_path, pkg) in &graph.packages {
            let Some(ref local) = pkg.local_source else {
                continue;
            };
            if matches!(local, LocalSource::Link(_)) {
                continue;
            }
            if self.use_global_virtual_store && local.is_globally_shareable() {
                // A git / remote-tarball dep shared globally must be
                // materialized into its content-addressed GVS path so a
                // registry dependent's sibling symlink resolves. The
                // package index *defines* that path — the content
                // fingerprint is folded into the graph hash — so a
                // missing index is not a skippable cache hit the way a
                // per-project `file:` dep can be; silently continuing
                // would leave the GVS entry absent and dangle every
                // dependent (`require()` throwing `Cannot find module`).
                // Fail loudly, exactly like the registry pass below.
                //
                // There is deliberately no `store.load_index` fallback
                // here (unlike the registry pass): git/tarball indices
                // are never persisted by `(name, version, integrity)`,
                // because a prepared tree and its `--ignore-scripts` raw
                // checkout share that coordinate and the content
                // fingerprint is precisely what keeps them on separate
                // GVS paths. The fetch driver guarantees the index is
                // present by always re-importing source deps.
                let index = package_indices
                    .get(dep_path)
                    .ok_or_else(|| Error::MissingPackageIndex(dep_path.to_string()))?;
                self.ensure_shared_local_in_global_store(
                    &aube_dir,
                    dep_path,
                    pkg,
                    index,
                    &mut stats,
                    nested_link_targets.as_ref(),
                )?;
                continue;
            }
            let Some(index) = package_indices.get(dep_path) else {
                continue;
            };
            let aube_entry = aube_dir.join(dep_path);
            if !aube_entry.exists() {
                self.materialize_into(
                    &aube_dir,
                    dep_path,
                    pkg,
                    index,
                    &mut stats,
                    false,
                    nested_link_targets.as_ref(),
                )?;
            } else {
                stats.packages_cached += 1;
            }
        }

        if self.use_global_virtual_store {
            use rayon::prelude::*;
            use rustc_hash::FxHashSet;

            // Pre-create every parent directory (`aube_dir` itself plus
            // one entry per unique `@scope/`) once so the per-package
            // par_iter below does not pay 1.4k `create_dir_all` stat
            // syscalls. The set is tiny (1-5 entries on a typical
            // graph) so the serial pre-pass is dwarfed by the wins
            // inside the par_iter that no longer needs the inner
            // `mkdirp(parent)` call.
            let mut step1_parents: FxHashSet<PathBuf> = FxHashSet::default();
            for (dep_path, pkg) in &graph.packages {
                if pkg.local_source.is_some() {
                    continue;
                }
                let entry = aube_dir.join(self.aube_dir_entry_name(dep_path));
                if let Some(parent) = entry.parent() {
                    step1_parents.insert(parent.to_path_buf());
                }
            }
            for parent in &step1_parents {
                mkdirp(parent)?;
            }

            let link_parallelism = self.link_parallelism();
            let step1_timer = std::time::Instant::now();
            let step1_results: Vec<Result<LinkStats, Error>> =
                with_link_pool(link_parallelism, || {
                    graph
                        .packages
                        .par_iter()
                        .filter_map(|(dep_path, pkg)| {
                            if pkg.local_source.is_some() {
                                return None;
                            }
                            Some((dep_path, pkg))
                        })
                        .map(|(dep_path, pkg)| {
                            let mut local_stats = LinkStats::default();
                            let local_aube_entry =
                                aube_dir.join(self.aube_dir_entry_name(dep_path));
                            let global_entry =
                                self.virtual_store.join(self.virtual_store_subdir(dep_path));

                            // Single readlink classifies the entry into one of
                            // three states and drives the whole per-package
                            // decision tree below. Avoids the double-check
                            // (`read_link` then `exists`) the previous version
                            // did and eliminates the unconditional
                            // `remove_dir`/`remove_file` pair on cold installs,
                            // which strace showed as ~1.4k ENOENT syscalls per
                            // install on the medium fixture.
                            let state = classify_entry_state(&local_aube_entry, &global_entry);

                            if matches!(state, EntryState::Fresh) {
                                local_stats.packages_cached += 1;
                                return Ok(local_stats);
                            }

                            // Symlink is stale or missing — need the package
                            // index to (re)materialize. The install driver
                            // omits `package_indices` entries for packages on
                            // the fast path; load from the store on demand if
                            // this one slipped through. This keeps the
                            // fast-path safe against graph-hash changes that
                            // invalidate the symlink target (patches, engine
                            // bumps, `allowBuilds` flips).
                            let owned_index;
                            let index = match package_indices.get(dep_path) {
                                Some(idx) => idx,
                                None => {
                                    owned_index = self
                                        .store
                                        .load_index(
                                            pkg.registry_name(),
                                            &pkg.version,
                                            pkg.integrity.as_deref(),
                                        )
                                        .ok_or_else(|| {
                                            Error::MissingPackageIndex(dep_path.to_string())
                                        })?;
                                    &owned_index
                                }
                            };
                            self.ensure_in_virtual_store(
                                dep_path,
                                pkg,
                                index,
                                &mut local_stats,
                                nested_link_targets.as_ref(),
                            )?;

                            // Only pay the `remove_dir`/`remove_file` syscalls
                            // when we actually have something to remove.
                            // On Windows, `.aube/<dep_path>` is an NTFS
                            // junction (created via `sys::create_dir_link`);
                            // `remove_file` can't unlink those, so try
                            // `remove_dir` first and fall back to
                            // `remove_file` for the unix case (where
                            // `symlink` produces a file-style link).
                            if matches!(state, EntryState::Stale) {
                                let _ = std::fs::remove_dir(&local_aube_entry)
                                    .or_else(|_| std::fs::remove_file(&local_aube_entry));
                            }
                            // Parent dirs were pre-created above the
                            // par_iter; no per-package `mkdirp` here.
                            sys::create_dir_link(&global_entry, &local_aube_entry)
                                .map_err(|e| Error::Io(local_aube_entry.clone(), e))?;
                            Ok(local_stats)
                        })
                        .collect()
                });

            for result in step1_results {
                let local_stats = result?;
                stats.packages_linked += local_stats.packages_linked;
                stats.packages_cached += local_stats.packages_cached;
                stats.files_linked += local_stats.files_linked;
            }
            tracing::debug!("link:step1 (gvs populate) {:.1?}", step1_timer.elapsed());
        } else {
            use rayon::prelude::*;

            // `wipe_changed_patched_entries` above already removed any
            // `.aube/<dep_path>` whose patch fingerprint changed since
            // the last install, so the existence check below will fall
            // through to `materialize_into` for those packages and
            // pick up the current patch state. In per-project mode the
            // dep paths are already isolated, so we can materialize
            // them independently on the same rayon pool the gvs path
            // uses instead of rebuilding the whole tree serially.
            let link_parallelism = self.link_parallelism();
            let step1_results: Vec<Result<LinkStats, Error>> =
                with_link_pool(link_parallelism, || {
                    graph
                        .packages
                        .par_iter()
                        .filter_map(|(dep_path, pkg)| {
                            if pkg.local_source.is_some() {
                                return None;
                            }
                            Some((dep_path, pkg))
                        })
                        .map(|(dep_path, pkg)| {
                            let mut local_stats = LinkStats::default();
                            let aube_entry = aube_dir.join(self.aube_dir_entry_name(dep_path));
                            if aube_entry.exists() {
                                // Already in place from a previous run —
                                // count as cached. `install.rs`
                                // deliberately omits this dep_path from
                                // `package_indices` on the fast path, so
                                // do the existence check first.
                                local_stats.packages_cached += 1;
                                return Ok(local_stats);
                            }
                            // Entry missing — load the index. Fast path in
                            // `install.rs` skips `load_index` when
                            // `aube_entry` already exists; lazy-load here
                            // for the case where a patch / allowBuilds
                            // change invalidated the entry since.
                            let owned_index;
                            let index = match package_indices.get(dep_path) {
                                Some(idx) => idx,
                                None => {
                                    owned_index = self
                                        .store
                                        .load_index(
                                            pkg.registry_name(),
                                            &pkg.version,
                                            pkg.integrity.as_deref(),
                                        )
                                        .ok_or_else(|| {
                                            Error::MissingPackageIndex(dep_path.to_string())
                                        })?;
                                    &owned_index
                                }
                            };
                            self.materialize_into(
                                &aube_dir,
                                dep_path,
                                pkg,
                                index,
                                &mut local_stats,
                                false,
                                nested_link_targets.as_ref(),
                            )?;
                            Ok(local_stats)
                        })
                        .collect()
                });

            for result in step1_results {
                let local_stats = result?;
                stats.packages_linked += local_stats.packages_linked;
                stats.packages_cached += local_stats.packages_cached;
                stats.files_linked += local_stats.files_linked;
            }
        }

        // `virtualStoreOnly=true` skips Steps 2 + 3 — the
        // user-visible top-level `node_modules/<name>` symlinks and
        // the hoisting passes that target the same directory — but
        // Step 4 (the hidden `.aube/node_modules/` hoist) still runs
        // because that tree lives *inside* the virtual store and
        // packages walking up for undeclared deps need it. Anything
        // that walks the user-visible root tree (bin linking,
        // lifecycle scripts, the state sidecar) is the install
        // driver's responsibility to skip in this mode.
        if self.virtual_store_only {
            self.link_hidden_hoist(&aube_dir, graph)?;
            if let Err(e) = write_applied_patches(&nm, &curr_applied) {
                let sidecar = applied_patches_sidecar_name();
                tracing::error!(
                    code = aube_codes::errors::ERR_AUBE_PATCHES_TRACKING_WRITE,
                    "failed to write {sidecar}: {e}. next install may miss stale patched entries"
                );
            }
            return Ok(stats);
        }

        // Step 2: Create top-level entries as symlinks into .aube.
        // The .aube/<dep_path>/node_modules/ directory already contains the
        // package and sibling symlinks to its direct deps (set up by
        // materialize_into / ensure_in_virtual_store), so a single symlink at
        // node_modules/<name> gives Node everything it needs to resolve
        // transitive deps via its normal directory walk.
        use rayon::prelude::*;

        let root_deps: Vec<_> = graph.root_deps().to_vec();
        let link_parallelism = self.link_parallelism();
        let step2_timer = std::time::Instant::now();
        let results: Vec<Result<bool, Error>> = with_link_pool(link_parallelism, || {
            root_deps
                .par_iter()
                .map(|dep| {
                    crate::validate_package_link_name(&dep.name)?;
                    let target_dir = nm.join(&dep.name);

                    // `link:` direct deps point at the on-disk target with
                    // a plain symlink, bypassing `.aube/` entirely.
                    if let Some(pkg) = graph.packages.get(&dep.dep_path)
                        && let Some(LocalSource::Link(rel)) = pkg.local_source.as_ref()
                    {
                        let abs_target = project_dir.join(rel);
                        let link_parent = target_dir.parent().unwrap_or(&nm);
                        let rel_target =
                            pathdiff::diff_paths(&abs_target, link_parent).unwrap_or(abs_target);
                        if reconcile_top_level_link(&target_dir, &rel_target)? {
                            return Ok(false);
                        }
                        if let Some(parent) = target_dir.parent() {
                            mkdirp(parent)?;
                        }
                        sys::create_dir_link(&rel_target, &target_dir)
                            .map_err(|e| Error::Io(target_dir.clone(), e))?;
                        return Ok(true);
                    }

                    // Verify the source actually exists in .aube before symlinking
                    let source_dir = aube_dir
                        .join(self.aube_dir_entry_name(&dep.dep_path))
                        .join("node_modules")
                        .join(&dep.name);
                    if !source_dir.exists() {
                        return Ok(false);
                    }

                    // Symlink target is relative to node_modules/<name>'s parent.
                    // For non-scoped packages the parent is node_modules/, but for
                    // scoped packages (e.g. @scope/name) it is node_modules/@scope/,
                    // so we must compute the relative path dynamically.
                    let link_parent = target_dir.parent().unwrap_or(&nm);
                    let rel_target = pathdiff::diff_paths(&source_dir, link_parent)
                        .unwrap_or_else(|| source_dir.clone());
                    // Target-aware reconcile: a version upgrade keeps the
                    // old `node_modules/<name>` symlink but it now points
                    // at a stale `.aube/<old-dep-path>`; we need to
                    // rewrite it to the new `.aube/<new-dep-path>`.
                    if reconcile_top_level_link(&target_dir, &rel_target)? {
                        return Ok(false);
                    }
                    if let Some(parent) = target_dir.parent() {
                        mkdirp(parent)?;
                    }

                    sys::create_dir_link(&rel_target, &target_dir)
                        .map_err(|e| Error::Io(target_dir.clone(), e))?;

                    trace!("top-level: {}", dep.name);
                    Ok(true)
                })
                .collect()
        });

        for result in results {
            if result? {
                stats.top_level_linked += 1;
            }
        }
        tracing::debug!(
            "link:step2 (top-level symlinks) {:.1?}",
            step2_timer.elapsed()
        );

        // Step 3: public-hoist-pattern matches get surfaced to the
        // root first, then shamefully_hoist (if enabled) sweeps up
        // everything else. Both use first-write-wins so direct deps
        // keep their symlinks and the pattern-matched names take
        // precedence over the bulk hoist.
        if !self.public_hoist_patterns.is_empty() {
            self.hoist_remaining_into(
                &nm,
                &aube_dir,
                graph,
                &mut stats,
                "public-hoist",
                &|name| self.public_hoist_matches(name),
            )?;
        }
        if self.shamefully_hoist {
            self.hoist_remaining_into(&nm, &aube_dir, graph, &mut stats, "hoist", &|_| true)?;
        }

        // Step 4: populate (or sweep) the hidden modules tree under
        // `.aube/node_modules/`. This runs regardless of the root
        // hoist passes above — it targets a different consumer
        // (packages inside the virtual store walking up for
        // undeclared deps) and wouldn't interact with the
        // root-level symlinks even on name clashes.
        self.link_hidden_hoist(&aube_dir, graph)?;

        if let Err(e) = write_applied_patches(&nm, &curr_applied) {
            let sidecar = applied_patches_sidecar_name();
            tracing::error!(
                code = aube_codes::errors::ERR_AUBE_PATCHES_TRACKING_WRITE,
                "failed to write {sidecar}: {e}. next install may miss stale patched entries"
            );
        }
        Ok(stats)
    }

    /// Hoisted-mode workspace linker. Runs the per-importer
    /// hoisted planner once per importer in the graph, accumulating
    /// stats + placements into a single `LinkStats`. Each importer
    /// gets its own independent flat tree (no shared root
    /// virtual-store like the isolated layout), matching npm
    /// workspaces and what hoisted-mode toolchains expect: a
    /// self-contained `node_modules/` under every importer.
    fn link_workspace_hoisted(
        &self,
        root_dir: &Path,
        graph: &LockfileGraph,
        package_indices: &BTreeMap<String, PackageIndex>,
        workspace_dirs: &BTreeMap<String, PathBuf>,
    ) -> Result<LinkStats, Error> {
        let mut stats = LinkStats::default();
        let mut placements = HoistedPlacements::default();
        for (importer_path, deps) in &graph.importers {
            if !is_physical_importer(importer_path) {
                continue;
            }
            let importer_dir = if importer_path == "." {
                root_dir.to_path_buf()
            } else {
                // Collapse `..` segments lexically — a parent-relative
                // importer key (`../sibling`, possible when
                // `pnpm-workspace.yaml#packages` uses `../**`) needs
                // to land at the actual sibling dir before
                // `pathdiff`/`strip_prefix` see it.
                aube_util::path::normalize_lexical(&root_dir.join(importer_path))
            };
            // Workspace deps resolve through `workspace_dirs` rather
            // than going through the placement tree, so the hoisted
            // planner shouldn't try to copy their contents. Filter
            // them out of the seed set — we'll symlink them in a
            // post-pass below.
            //
            // Same gating as the isolated mode below: the resolver
            // omits a `LockedPackage` for workspace-resolved siblings,
            // so a name match plus a missing package entry is the
            // signal that the resolver picked the sibling. When the
            // resolved package IS in `graph.packages`, the resolver
            // pinned a registry version and the dep should follow the
            // normal hoisted-placement path (otherwise the post-pass
            // would silently substitute the local copy).
            let planner_deps: Vec<aube_lockfile::DirectDep> = deps
                .iter()
                .filter(|d| {
                    !workspace_dirs.contains_key(&d.name)
                        || graph.packages.contains_key(&d.dep_path)
                })
                .cloned()
                .collect();
            hoisted::link_hoisted_importer(
                self,
                hoisted::HoistedImporterDirs {
                    root: root_dir,
                    importer: &importer_dir,
                },
                &planner_deps,
                graph,
                package_indices,
                &mut stats,
                &mut placements,
            )?;

            // Drop workspace deps in as symlinks, same as isolated mode.
            let nm = importer_dir.join(&self.modules_dir_name);
            if !self.hoist_workspace_packages {
                continue;
            }
            for dep in deps {
                crate::validate_package_link_name(&dep.name)?;
                let Some(ws_dir) = workspace_dirs.get(&dep.name) else {
                    continue;
                };
                // See planner_deps gating above: skip deps the
                // resolver actually pinned to a registry version.
                if graph.packages.contains_key(&dep.dep_path) {
                    continue;
                }
                let link_path = nm.join(&dep.name);
                if let Some(parent) = link_path.parent() {
                    mkdirp(parent)?;
                }
                try_remove_entry(&link_path);
                let link_parent = link_path.parent().unwrap_or(&nm);
                let target = pathdiff::diff_paths(ws_dir, link_parent).unwrap_or(ws_dir.clone());
                sys::create_dir_link(&target, &link_path)
                    .map_err(|e| Error::Io(link_path.clone(), e))?;
                stats.top_level_linked += 1;
            }
        }
        // Same rationale as the non-workspace hoisted path: sweep any
        // `.aube/node_modules/` left behind by a prior isolated
        // install so hoisted's dotfile-preserving cleanup doesn't
        // leak a stale hidden tree. Honors `virtualStoreDir`.
        let _ = crate::remove_dir_all_with_retry(&self.aube_dir_for(root_dir).join("node_modules"));
        stats.hoisted_placements = Some(placements);
        Ok(stats)
    }

    /// Link all packages for a workspace (multiple importers).
    ///
    /// Creates the shared `.aube/` virtual store at root, then for each workspace
    /// package creates `node_modules/` with its direct deps linked from the root `.aube/`.
    /// Workspace packages that depend on each other get symlinks to the package directory.
    pub fn link_workspace(
        &self,
        root_dir: &Path,
        graph: &LockfileGraph,
        package_indices: &BTreeMap<String, PackageIndex>,
        workspace_dirs: &BTreeMap<String, PathBuf>,
    ) -> Result<LinkStats, Error> {
        if matches!(self.node_linker, NodeLinker::Hoisted) {
            return self.link_workspace_hoisted(root_dir, graph, package_indices, workspace_dirs);
        }
        if self.use_global_virtual_store && self.hoist {
            remove_hidden_hoist_tree(&self.virtual_store.join("node_modules"));
            return self.without_global_virtual_store().link_workspace(
                root_dir,
                graph,
                package_indices,
                workspace_dirs,
            );
        }

        let root_nm = root_dir.join(&self.modules_dir_name);
        let aube_dir = self.aube_dir_for(root_dir);

        mkdirp(&aube_dir)?;
        mkdirp(&root_nm)?;

        let mut stats = LinkStats::default();

        // Patch reconciliation. Mirrors `link_all`'s logic: wipe
        // `.aube/<dep_path>` for any package whose patch fingerprint
        // changed between the previous and current install. Only
        // applies to per-project (non-gvs) mode because the gvs path
        // already folds patches into the hashed `.aube/<dep_path>`
        // name via `with_graph_hashes`.
        let prev_applied = read_applied_patches(&root_nm);
        let curr_applied = current_patch_hashes(&self.patches);
        if !self.use_global_virtual_store {
            wipe_changed_patched_entries(
                &aube_dir,
                graph,
                &prev_applied,
                &curr_applied,
                self.virtual_store_dir_max_length,
            );
        }

        let nested_link_targets = build_nested_link_targets(root_dir, graph);

        // Step 1a: Materialize local (`file:` dir/tarball, `portal:`,
        // `exec:`) packages straight into the shared per-project
        // `.aube/`. They never participate in the global virtual store
        // since their source resolves against a path inside the
        // project. `link:` deps get no `.aube/` entry at all — step 2
        // symlinks directly to the target.
        //
        // Git and remote-tarball sources are content-pinned and shared
        // across projects like registry packages, so under the global
        // virtual store they're materialized into the shared store with
        // a `.aube/<dep_path>` symlink pointing at it — otherwise a
        // registry dependent in the shared store would get a dangling
        // sibling symlink to them.
        for (dep_path, pkg) in &graph.packages {
            let Some(ref local) = pkg.local_source else {
                continue;
            };
            if matches!(local, LocalSource::Link(_)) {
                continue;
            }
            if self.use_global_virtual_store && local.is_globally_shareable() {
                // See the matching block in `link_isolated`: a globally
                // shared git/tarball dep's index defines its
                // content-addressed GVS path, so a missing index must
                // fail loudly rather than dangle every dependent. No
                // `load_index` fallback — those indices aren't persisted
                // by coordinate (prepared vs raw checkout would collide).
                let index = package_indices
                    .get(dep_path)
                    .ok_or_else(|| Error::MissingPackageIndex(dep_path.to_string()))?;
                self.ensure_shared_local_in_global_store(
                    &aube_dir,
                    dep_path,
                    pkg,
                    index,
                    &mut stats,
                    nested_link_targets.as_ref(),
                )?;
                continue;
            }
            let Some(index) = package_indices.get(dep_path) else {
                continue;
            };
            let aube_entry = aube_dir.join(self.aube_dir_entry_name(dep_path));
            if aube_entry.exists() {
                stats.packages_cached += 1;
                continue;
            }
            self.materialize_into(
                &aube_dir,
                dep_path,
                pkg,
                index,
                &mut stats,
                false,
                nested_link_targets.as_ref(),
            )?;
        }

        // Step 1b: Populate shared .aube virtual store at root for
        // registry packages. Mirrors `link_all`'s parallel +
        // Fresh/Missing/Stale state machine so warm re-runs are a
        // `readlink` per package instead of a recreate per package.
        if self.use_global_virtual_store {
            use rayon::prelude::*;
            use rustc_hash::FxHashSet;

            // Pre-create every parent directory (`aube_dir` itself plus
            // one entry per unique `@scope/`) once so the per-package
            // par_iter below does not pay 1.4k `create_dir_all` stat
            // syscalls. The set is tiny (1-5 entries on a typical
            // graph) so the serial pre-pass is dwarfed by the wins
            // inside the par_iter that no longer needs the inner
            // `mkdirp(parent)` call.
            let mut step1_parents: FxHashSet<PathBuf> = FxHashSet::default();
            for (dep_path, pkg) in &graph.packages {
                if pkg.local_source.is_some() {
                    continue;
                }
                let entry = aube_dir.join(self.aube_dir_entry_name(dep_path));
                if let Some(parent) = entry.parent() {
                    step1_parents.insert(parent.to_path_buf());
                }
            }
            for parent in &step1_parents {
                mkdirp(parent)?;
            }

            let link_parallelism = self.link_parallelism();
            let step1_timer = std::time::Instant::now();
            let step1_results: Vec<Result<LinkStats, Error>> =
                with_link_pool(link_parallelism, || {
                    graph
                        .packages
                        .par_iter()
                        .filter_map(|(dep_path, pkg)| {
                            if pkg.local_source.is_some() {
                                return None;
                            }
                            Some((dep_path, pkg))
                        })
                        .map(|(dep_path, pkg)| {
                            let mut local_stats = LinkStats::default();
                            let local_aube_entry =
                                aube_dir.join(self.aube_dir_entry_name(dep_path));
                            let global_entry =
                                self.virtual_store.join(self.virtual_store_subdir(dep_path));

                            let state = classify_entry_state(&local_aube_entry, &global_entry);

                            if matches!(state, EntryState::Fresh) {
                                local_stats.packages_cached += 1;
                                return Ok(local_stats);
                            }

                            let owned_index;
                            let index = match package_indices.get(dep_path) {
                                Some(idx) => idx,
                                None => {
                                    owned_index = self
                                        .store
                                        .load_index(
                                            pkg.registry_name(),
                                            &pkg.version,
                                            pkg.integrity.as_deref(),
                                        )
                                        .ok_or_else(|| {
                                            Error::MissingPackageIndex(dep_path.to_string())
                                        })?;
                                    &owned_index
                                }
                            };
                            self.ensure_in_virtual_store(
                                dep_path,
                                pkg,
                                index,
                                &mut local_stats,
                                nested_link_targets.as_ref(),
                            )?;

                            if matches!(state, EntryState::Stale) {
                                let _ = std::fs::remove_dir(&local_aube_entry)
                                    .or_else(|_| std::fs::remove_file(&local_aube_entry));
                            }
                            // Parent dirs were pre-created above the
                            // par_iter; no per-package `mkdirp` here.
                            sys::create_dir_link(&global_entry, &local_aube_entry)
                                .map_err(|e| Error::Io(local_aube_entry.clone(), e))?;
                            Ok(local_stats)
                        })
                        .collect()
                });

            for result in step1_results {
                let local_stats = result?;
                stats.packages_linked += local_stats.packages_linked;
                stats.packages_cached += local_stats.packages_cached;
                stats.files_linked += local_stats.files_linked;
            }
            tracing::debug!(
                "link_workspace:step1 (gvs populate) {:.1?}",
                step1_timer.elapsed()
            );
        } else {
            use rayon::prelude::*;

            let link_parallelism = self.link_parallelism();
            let step1_results: Vec<Result<LinkStats, Error>> =
                with_link_pool(link_parallelism, || {
                    graph
                        .packages
                        .par_iter()
                        .filter_map(|(dep_path, pkg)| {
                            if pkg.local_source.is_some() {
                                return None;
                            }
                            Some((dep_path, pkg))
                        })
                        .map(|(dep_path, pkg)| {
                            let mut local_stats = LinkStats::default();
                            let aube_entry = aube_dir.join(self.aube_dir_entry_name(dep_path));
                            if aube_entry.exists() {
                                local_stats.packages_cached += 1;
                                return Ok(local_stats);
                            }
                            let owned_index;
                            let index = match package_indices.get(dep_path) {
                                Some(idx) => idx,
                                None => {
                                    owned_index = self
                                        .store
                                        .load_index(
                                            pkg.registry_name(),
                                            &pkg.version,
                                            pkg.integrity.as_deref(),
                                        )
                                        .ok_or_else(|| {
                                            Error::MissingPackageIndex(dep_path.to_string())
                                        })?;
                                    &owned_index
                                }
                            };
                            self.materialize_into(
                                &aube_dir,
                                dep_path,
                                pkg,
                                index,
                                &mut local_stats,
                                false,
                                nested_link_targets.as_ref(),
                            )?;
                            Ok(local_stats)
                        })
                        .collect()
                });

            for result in step1_results {
                let local_stats = result?;
                stats.packages_linked += local_stats.packages_linked;
                stats.packages_cached += local_stats.packages_cached;
                stats.files_linked += local_stats.files_linked;
            }
        }

        // `virtualStoreOnly=true` skips per-importer node_modules
        // population and the root-level hoisting passes, but the
        // hidden `.aube/node_modules/` hoist (Step 4 below) still
        // runs because it lives *inside* the virtual store. Bin
        // linking and lifecycle scripts for the top-level importers
        // are the install driver's responsibility to skip in this
        // mode.
        if self.virtual_store_only {
            // Sweep root_nm of any user-visible entries a prior
            // (non-virtualStoreOnly) install left behind. With the
            // default `virtualStoreDir`, `.aube/` lives directly
            // under `root_nm` and must be preserved. Custom
            // `virtualStoreDir` overrides put `.aube/` outside the
            // sweep zone already.
            let aube_dir_leaf: Option<std::ffi::OsString> =
                if aube_dir.parent() == Some(root_nm.as_path()) {
                    aube_dir.file_name().map(|s| s.to_owned())
                } else {
                    None
                };
            if let Ok(entries) = std::fs::read_dir(&root_nm) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.starts_with('.') {
                        continue;
                    }
                    if aube_dir_leaf.as_deref() == Some(name.as_os_str()) {
                        continue;
                    }
                    try_remove_entry(&entry.path());
                }
            }
            self.link_hidden_hoist(&aube_dir, graph)?;
            if let Err(e) = write_applied_patches(&root_nm, &curr_applied) {
                let sidecar = applied_patches_sidecar_name();
                tracing::error!(
                    code = aube_codes::errors::ERR_AUBE_PATCHES_TRACKING_WRITE,
                    "failed to write {sidecar}: {e}. next install may miss stale patched entries"
                );
            }
            return Ok(stats);
        }

        // Precompute root importer's direct deps keyed by name so the
        // per-importer loop below can short-circuit on `dedupeDirectDeps`
        // without walking the root's dep list for every child entry.
        // Empty when the root has no direct deps (lockfile-only workspaces)
        // or when `dedupeDirectDeps=false` — skipping the build on the
        // common path avoids an allocation the per-dep check would
        // never consult.
        let root_deps_by_name: std::collections::HashMap<&str, &aube_lockfile::DirectDep> =
            if self.dedupe_direct_deps {
                graph
                    .importers
                    .get(".")
                    .map(|deps| deps.iter().map(|d| (d.name.as_str(), d)).collect())
                    .unwrap_or_default()
            } else {
                std::collections::HashMap::new()
            };

        // Step 2a: Per-importer setup — ensure each importer's
        // `node_modules/` exists and sweep entries no longer in that
        // importer's direct deps. Cheap serial work (workspace
        // importers count is small; the expensive symlink syscalls
        // run in parallel below). For the root importer we also
        // expand the preserve set with `shamefullyHoist` /
        // `publicHoistPattern` matches so the hoist passes that run
        // after Step 2 don't redo work they'd have preserved.
        let aube_dir_leaf_root: Option<std::ffi::OsString> =
            if aube_dir.parent() == Some(root_nm.as_path()) {
                aube_dir.file_name().map(|s| s.to_owned())
            } else {
                None
            };

        for (importer_path, deps) in &graph.importers {
            if !is_physical_importer(importer_path) {
                continue;
            }
            let nm = if importer_path == "." {
                root_nm.clone()
            } else {
                // Same lexical-normalization rationale as the hoisted
                // path above: a `../sibling` importer key has to land
                // at the actual sibling's `node_modules` rather than
                // `<root>/../sibling/node_modules`, otherwise
                // `pathdiff` produces a symlink target with the wrong
                // depth (one extra `..` per uncollapsed segment).
                aube_util::path::normalize_lexical(
                    &root_dir.join(importer_path).join(&self.modules_dir_name),
                )
            };
            if importer_path != "." {
                mkdirp(&nm)?;
            }

            let mut preserve: std::collections::HashSet<&str> =
                deps.iter().map(|d| d.name.as_str()).collect();
            if importer_path == "." {
                if self.shamefully_hoist {
                    for pkg in graph.packages.values() {
                        preserve.insert(pkg.name.as_str());
                    }
                } else if !self.public_hoist_patterns.is_empty() {
                    for pkg in graph.packages.values() {
                        if pkg.local_source.is_none() && self.public_hoist_matches(&pkg.name) {
                            preserve.insert(pkg.name.as_str());
                        }
                    }
                }
            }
            let aube_leaf_here = if importer_path == "." {
                aube_dir_leaf_root.as_deref()
            } else {
                None
            };
            sweep_stale_top_level_entries(&nm, &preserve, aube_leaf_here);
        }

        // Step 2b: Create top-level symlinks in parallel.
        // Flatten (importer, dep) pairs so every symlink syscall
        // runs through the rayon pool — 3k+ serial
        // `create_dir_link` calls was the second-biggest slice of
        // the workspace install phase before this change.
        use rayon::prelude::*;

        #[derive(Clone)]
        struct Step2Task<'a> {
            importer_path: &'a str,
            nm: PathBuf,
            dep: &'a aube_lockfile::DirectDep,
        }
        let tasks: Vec<Step2Task<'_>> = graph
            .importers
            .iter()
            .filter(|(importer_path, _)| is_physical_importer(importer_path))
            .flat_map(|(importer_path, deps)| {
                let nm = if importer_path == "." {
                    root_nm.clone()
                } else {
                    // Same lexical-normalization rationale as
                    // `link_workspace_hoisted` above: parent-relative
                    // importer keys must collapse before `pathdiff`
                    // computes the top-level symlink target.
                    aube_util::path::normalize_lexical(
                        &root_dir.join(importer_path).join(&self.modules_dir_name),
                    )
                };
                deps.iter().map(move |dep| Step2Task {
                    importer_path: importer_path.as_str(),
                    nm: nm.clone(),
                    dep,
                })
            })
            .collect();

        let link_parallelism = self.link_parallelism();
        let step2_timer = std::time::Instant::now();
        let step2_results: Vec<Result<bool, Error>> = with_link_pool(link_parallelism, || {
            tasks
                .par_iter()
                .map(|task| {
                    let Step2Task {
                        importer_path,
                        nm,
                        dep,
                    } = task;

                    // `dedupeDirectDeps`: non-root importer dep
                    // already covered by the root symlink +
                    // parent-directory walk.
                    if self.dedupe_direct_deps
                        && *importer_path != "."
                        && let Some(root_dep) = root_deps_by_name.get(dep.name.as_str())
                        && root_dep.dep_path == dep.dep_path
                    {
                        return Ok(false);
                    }

                    crate::validate_package_link_name(&dep.name)?;
                    let link_path = nm.join(&dep.name);

                    // Workspace dep (`workspace:` protocol or bare
                    // semver that satisfies the sibling's version):
                    // link straight into the sibling package dir.
                    //
                    // Gate on the resolver's decision, not just the
                    // name match. The resolver omits a `LockedPackage`
                    // entry for workspace-resolved siblings (the
                    // `workspace_packages` branch in resolve.rs only
                    // pushes a `DirectDep`, never inserts into
                    // `resolved`), so a `dep_path` with no package
                    // entry means "resolver picked the sibling". When
                    // the package IS in `graph.packages`, the resolver
                    // pinned a registry version — even if a sibling
                    // shares the name, the user's spec didn't
                    // satisfy it (e.g. `is-positive: "2.0.0"` with a
                    // workspace sibling at `3.0.0`). Falling through
                    // to the registry branch in that case prevents the
                    // linker from silently substituting an
                    // incompatible local copy for the resolved
                    // version recorded in the lockfile.
                    if workspace_dirs.contains_key(&dep.name)
                        && !graph.packages.contains_key(&dep.dep_path)
                    {
                        let ws_dir = &workspace_dirs[&dep.name];
                        if !self.hoist_workspace_packages {
                            return Ok(false);
                        }
                        let link_parent = link_path.parent().unwrap_or(nm);
                        let rel_target =
                            pathdiff::diff_paths(ws_dir, link_parent).unwrap_or(ws_dir.clone());
                        if reconcile_top_level_link(&link_path, &rel_target)? {
                            return Ok(false);
                        }
                        if let Some(parent) = link_path.parent() {
                            mkdirp(parent)?;
                        }
                        sys::create_dir_link(&rel_target, &link_path)
                            .map_err(|e| Error::Io(link_path.clone(), e))?;
                        return Ok(true);
                    }

                    // `link:` dep — absolute path relative to `root_dir`.
                    if let Some(locked) = graph.packages.get(&dep.dep_path)
                        && let Some(LocalSource::Link(rel)) = locked.local_source.as_ref()
                    {
                        let abs_target = root_dir.join(rel);
                        let link_parent = link_path.parent().unwrap_or(nm);
                        let rel_target =
                            pathdiff::diff_paths(&abs_target, link_parent).unwrap_or(abs_target);
                        if reconcile_top_level_link(&link_path, &rel_target)? {
                            return Ok(false);
                        }
                        if let Some(parent) = link_path.parent() {
                            mkdirp(parent)?;
                        }
                        sys::create_dir_link(&rel_target, &link_path)
                            .map_err(|e| Error::Io(link_path.clone(), e))?;
                        return Ok(true);
                    }

                    // Regular registry dep — symlink to the root
                    // `.aube/<dep_path>/node_modules/<name>`.
                    let source_dir = aube_dir
                        .join(self.aube_dir_entry_name(&dep.dep_path))
                        .join("node_modules")
                        .join(&dep.name);
                    if !source_dir.exists() {
                        return Ok(false);
                    }
                    let link_parent = link_path.parent().unwrap_or(nm);
                    let rel_target = pathdiff::diff_paths(&source_dir, link_parent)
                        .unwrap_or_else(|| source_dir.clone());
                    if reconcile_top_level_link(&link_path, &rel_target)? {
                        return Ok(false);
                    }
                    if let Some(parent) = link_path.parent() {
                        mkdirp(parent)?;
                    }
                    sys::create_dir_link(&rel_target, &link_path)
                        .map_err(|e| Error::Io(link_path.clone(), e))?;
                    trace!("workspace top-level: {} -> {}", dep.name, importer_path);
                    Ok(true)
                })
                .collect()
        });
        for result in step2_results {
            if result? {
                stats.top_level_linked += 1;
            }
        }
        tracing::debug!(
            "link_workspace:step2 (top-level symlinks) {:.1?}",
            step2_timer.elapsed()
        );

        // Hoisting passes run against the *root* importer only —
        // pnpm never hoists into nested workspace packages. Run the
        // selective public-hoist-pattern first so matched names take
        // precedence, then `shamefully_hoist` sweeps up everything
        // else.
        if !self.public_hoist_patterns.is_empty() {
            self.hoist_remaining_into(
                &root_nm,
                &aube_dir,
                graph,
                &mut stats,
                "workspace public-hoist",
                &|name| self.public_hoist_matches(name),
            )?;
        }
        if self.shamefully_hoist {
            self.hoist_remaining_into(
                &root_nm,
                &aube_dir,
                graph,
                &mut stats,
                "workspace hoist",
                &|_| true,
            )?;
        }

        // Hidden hoist is shared across importers, so a single sweep
        // here is sufficient for the whole workspace.
        self.link_hidden_hoist(&aube_dir, graph)?;

        if let Err(e) = write_applied_patches(&root_nm, &curr_applied) {
            let sidecar = applied_patches_sidecar_name();
            tracing::error!(
                code = aube_codes::errors::ERR_AUBE_PATCHES_TRACKING_WRITE,
                "failed to write {sidecar}: {e}. next install may miss stale patched entries"
            );
        }
        Ok(stats)
    }

    /// Populate (or sweep) the project-local hidden modules directory at
    /// `aube_dir/node_modules/<name>`. When `self.hoist` is enabled,
    /// walks every non-local package in the graph and creates a symlink
    /// for names that match `hoist_patterns` into each corresponding
    /// virtual-store package entry.
    /// When disabled, wipes the directory so previously-hoisted
    /// symlinks don't keep resolving through Node's parent walk.
    ///
    /// Unlike `hoist_remaining_into`, this writes into a private
    /// sibling of `.aube/<dep_path>/` rather than the visible root
    /// `node_modules/`. Packages inside the virtual store (e.g.
    /// `.aube/react@18/node_modules/react/`) walk up through
    /// `.aube/node_modules/` during require resolution, which is the
    /// only consumer of these links — nothing inside the user's own
    /// `node_modules/<name>` view is affected. In GVS mode, the shared
    /// virtual store must not expose unversioned hidden-hoist aliases:
    /// those aliases are project-specific and would otherwise be mutable
    /// cross-project state. Remove any stale shared mirror left by older
    /// linkers and keep the only hidden-hoist tree project-local.
    fn link_hidden_hoist(&self, aube_dir: &Path, graph: &LockfileGraph) -> Result<(), Error> {
        self.link_hidden_hoist_at(aube_dir, aube_dir, graph, false, true)?;
        if self.use_global_virtual_store {
            remove_hidden_hoist_tree(&self.virtual_store.join("node_modules"));
        }
        Ok(())
    }

    fn link_hidden_hoist_at(
        &self,
        hidden_root: &Path,
        source_root: &Path,
        graph: &LockfileGraph,
        use_hashed_subdirs: bool,
        sweep_stale_entries: bool,
    ) -> Result<(), Error> {
        let hidden = hidden_root.join("node_modules");
        // FxHashSet over the borrowed name (lives for the lockfile graph
        // lifetime) drops the SipHash overhead and the per-insert
        // `String` clone the `HashSet<String>` version forced.
        let mut claimed: rustc_hash::FxHashSet<&str> = rustc_hash::FxHashSet::default();
        let mut packages = Vec::new();
        if self.hoist {
            for direct in graph.root_deps() {
                let Some(pkg) = graph.packages.get(&direct.dep_path) else {
                    continue;
                };
                if pkg.local_source.is_some() || !self.hoist_matches(&pkg.name) {
                    continue;
                }
                if claimed.insert(pkg.name.as_str()) {
                    packages.push((direct.dep_path.as_str(), pkg));
                }
            }
            for (dep_path, pkg) in &graph.packages {
                if pkg.local_source.is_some() || !self.hoist_matches(&pkg.name) {
                    continue;
                }
                // Root direct dependencies reserve their package names
                // before the deterministic transitive sweep. That matches
                // pnpm's undeclared-import fallback when the app and a
                // transitive dependency bring different versions of the
                // same package into the graph.
                if claimed.insert(pkg.name.as_str()) {
                    packages.push((dep_path.as_str(), pkg));
                }
            }
        }

        if !self.hoist {
            // Previous install may have populated this tree with
            // hoist=true. Drop entries so Node doesn't keep resolving
            // phantom deps through the stale symlinks. Project-local
            // hidden hoist owns the whole tree and can remove it in
            // one shot; the shared GVS mirror only reclaims broken
            // entries because live links may belong to another project.
            if sweep_stale_entries {
                remove_hidden_hoist_tree(&hidden);
            } else {
                sweep_dead_hidden_hoist_entries(&hidden);
            }
            return Ok(());
        }
        // Wipe before repopulating so a dependency removed from the
        // graph (or a pattern that no longer matches) doesn't linger.
        // The shared GVS hidden hoist only prunes broken entries:
        // removing live cross-project links would make the directory
        // last-writer-wins for sequential installs.
        if sweep_stale_entries {
            remove_hidden_hoist_tree(&hidden);
        } else {
            sweep_dead_hidden_hoist_entries(&hidden);
        }
        for (dep_path, pkg) in packages {
            let source_subdir = if use_hashed_subdirs {
                self.virtual_store_subdir(dep_path)
            } else {
                self.aube_dir_entry_name(dep_path)
            };
            let source_dir = source_root
                .join(source_subdir)
                .join("node_modules")
                .join(&pkg.name);
            if !source_dir.exists() {
                continue;
            }
            let target_dir = hidden.join(&pkg.name);
            if let Some(parent) = target_dir.parent() {
                mkdirp(parent)?;
            }
            let link_parent = target_dir.parent().unwrap_or(&hidden);
            let rel_target = pathdiff::diff_paths(&source_dir, link_parent)
                .unwrap_or_else(|| source_dir.clone());
            if reconcile_top_level_link(&target_dir, &rel_target)? {
                continue;
            }
            sys::create_dir_link(&rel_target, &target_dir)
                .map_err(|e| Error::Io(target_dir.clone(), e))?;
            trace!("hidden-hoist: {}", pkg.name);
            // Intentionally not counted in `stats.top_level_linked`.
            // That counter reflects the user-visible root
            // `node_modules/<name>` entries; hidden-hoist symlinks
            // live under `.aube/node_modules/` and are only reached
            // via Node's parent-directory walk from inside the
            // virtual store, not from the user's own code.
        }
        Ok(())
    }

    /// Shared `shamefully_hoist` implementation. For every non-local
    /// package in the graph, create a symlink at `nm/<pkg.name>`
    /// pointing at the matching `.aube/<dep_path>/node_modules/<pkg.name>`
    /// entry.
    ///
    /// Two separate "first-write-wins" protections apply:
    ///
    /// - **Direct deps always win over hoisted transitives.** Names
    ///   that appear in `graph.root_deps()` were placed (or
    ///   deliberately skipped) by Step 2 and must never be overwritten
    ///   by a hoist pass — that would silently swap `node_modules/foo`
    ///   from the version the user pinned to whatever transitive
    ///   happened to sort first.
    /// - **Within the hoist pass, BTree iteration order is the
    ///   tiebreaker across versions.** The `claimed` set records
    ///   names we already hoisted this call so a later iteration with
    ///   the same name (different `dep_path`) doesn't clobber the
    ///   first winner.
    ///
    /// For everything else the caller gets a *target-aware* reconcile:
    /// an existing symlink at `nm/<name>` that points at the version
    /// this iteration wants is kept; one pointing at a stale
    /// `.aube/<old-dep-path>/` (leftover from a prior install whose
    /// hoisted version has since changed) is replaced. The old
    /// plain-`exists?` check here kept stale entries because the
    /// surrounding linker used to wipe `nm` unconditionally — now that
    /// we sweep surgically, hoist has to cope with partial priors.
    ///
    /// `trace_label` distinguishes the `link_all` vs `link_workspace`
    /// callers in `-v` output.
    fn hoist_remaining_into(
        &self,
        nm: &Path,
        aube_dir: &Path,
        graph: &LockfileGraph,
        stats: &mut LinkStats,
        trace_label: &str,
        select: &dyn Fn(&str) -> bool,
    ) -> Result<(), Error> {
        // Root direct-dep names. Populated from the importer map
        // rather than an opaque "touched by Step 2" signal so a direct
        // dep that *failed* to place (missing `source_dir.exists()`,
        // workspace toggle, etc.) still reserves its slot — pnpm
        // doesn't hoist over a direct dep even when the direct dep
        // couldn't be installed.
        let direct_dep_names: std::collections::HashSet<&str> =
            graph.root_deps().iter().map(|d| d.name.as_str()).collect();

        // FxHashSet over the borrowed name (lives for the lockfile graph
        // lifetime) drops the SipHash overhead and the per-insert
        // `String` clone the `HashSet<String>` version forced.
        let mut claimed: rustc_hash::FxHashSet<&str> = rustc_hash::FxHashSet::default();

        for (dep_path, pkg) in &graph.packages {
            if pkg.local_source.is_some() {
                continue;
            }
            if !select(&pkg.name) {
                continue;
            }
            // Direct deps always win over hoisting.
            if direct_dep_names.contains(pkg.name.as_str()) {
                continue;
            }
            // First-writer-wins within the hoist pass: if an earlier
            // iteration already hoisted this name, later iterations
            // with the same name don't overwrite it.
            if !claimed.insert(pkg.name.as_str()) {
                continue;
            }
            let source_dir = aube_dir
                .join(self.aube_dir_entry_name(dep_path))
                .join("node_modules")
                .join(&pkg.name);
            if !source_dir.exists() {
                // Don't remove `name` from `claimed` — another
                // iteration for the same name would also find its
                // `source_dir` missing (the `.aube` populate phase
                // runs before hoist for every package), and leaving
                // the name claimed preserves the existing symlink
                // (whatever it points at) instead of repeatedly
                // probing for a materialization that isn't coming.
                continue;
            }
            let target_dir = nm.join(&pkg.name);
            let link_parent = target_dir.parent().unwrap_or(nm);
            let rel_target = pathdiff::diff_paths(&source_dir, link_parent)
                .unwrap_or_else(|| source_dir.clone());
            if reconcile_top_level_link(&target_dir, &rel_target)? {
                continue;
            }
            if let Some(parent) = target_dir.parent() {
                mkdirp(parent)?;
            }
            sys::create_dir_link(&rel_target, &target_dir)
                .map_err(|e| Error::Io(target_dir.clone(), e))?;
            trace!("{trace_label}: {}", pkg.name);
            stats.top_level_linked += 1;
        }
        Ok(())
    }
}

/// Decide whether an existing `node_modules/<name>` entry can be left
/// alone, or must be removed so the caller can recreate it.
///
/// Returns `Ok(true)` when a live entry is present and should be
/// preserved. Returns `Ok(false)` when nothing is there (or a broken
/// link was reclaimed) and the caller should proceed to create the
/// entry. `symlink_metadata().is_ok()` on its own treats a dangling
/// symlink — whose `.aube/<dep_path>/...` target has been deleted — as
/// "already in place", which silently leaves the project unresolvable.
///
/// `sys::create_dir_link` produces a Unix symlink on Unix and an NTFS
/// junction on Windows. A junction's `file_type().is_symlink()` is
/// `false`, so we trust the `symlink_metadata().is_ok() && !exists()`
/// pair to identify "something is at `path` but its target is gone",
/// and use the same `remove_dir().or_else(remove_file())` fallback
/// used elsewhere in this file to unlink both shapes.
/// Reconcile a top-level `node_modules/<name>` entry against the
/// expected symlink target. Compares the link's *target* — a version
/// upgrade that leaves `.aube/<old-dep-path>/` resolvable on disk is
/// correctly classified as stale instead of silently keeping the old
/// symlink.
///
/// - `Ok(true)`  – existing entry is a symlink pointing at
///   `expected_target`; caller skips creation.
/// - `Ok(false)` – no entry exists, or a stale entry (wrong target,
///   dangling symlink, regular directory) has been best-effort
///   removed; caller should proceed to create the symlink.
///
/// Unix and Windows use different comparison strategies because
/// `create_dir_link` writes the target differently on each platform:
/// Unix preserves the relative target bytes-for-bytes as a POSIX
/// symlink, Windows normalizes to an absolute path before calling
/// `junction::create`. A plain `read_link == expected` check that
/// works on Unix would miss every warm run on Windows.
fn reconcile_top_level_link(link_path: &Path, expected_target: &Path) -> Result<bool, Error> {
    #[cfg(windows)]
    {
        // NTFS junctions store normalized absolute targets
        // (sometimes `\\?\`-prefixed), so comparing against the
        // relative `pathdiff::diff_paths` output the callers compute
        // would never match. Compare the canonical forms instead: if
        // the junction resolves to the same directory
        // `expected_target` points at, the link is fresh. Anything
        // else (dangling, wrong target, not a reparse point) falls
        // through to a best-effort reclaim.
        //
        // Canonicalize is ~5 syscalls on NTFS (open reparse, read
        // reparse data, close, query attrs ×2). With ~1000 top-level
        // links per warm install that's 5000 syscalls just for
        // expected_abs. Cache canonical forms keyed by the absolute
        // path so a second call to the same target returns
        // immediately.
        use std::sync::OnceLock;
        static CANON_CACHE: OnceLock<
            std::sync::RwLock<std::collections::HashMap<PathBuf, PathBuf>>,
        > = OnceLock::new();
        fn cached_canonicalize(p: &Path) -> std::io::Result<PathBuf> {
            let map = CANON_CACHE.get_or_init(Default::default);
            if let Some(hit) = map.read().expect("canon cache poisoned").get(p) {
                return Ok(hit.clone());
            }
            let canon = p.canonicalize()?;
            map.write()
                .expect("canon cache poisoned")
                .insert(p.to_path_buf(), canon.clone());
            Ok(canon)
        }
        let expected_abs = if expected_target.is_absolute() {
            expected_target.to_path_buf()
        } else {
            let parent = link_path.parent().unwrap_or_else(|| Path::new(""));
            parent.join(expected_target)
        };
        if let Ok(link_canon) = cached_canonicalize(link_path)
            && let Ok(exp_canon) = cached_canonicalize(&expected_abs)
            && link_canon == exp_canon
        {
            return Ok(true);
        }
        if link_path.symlink_metadata().is_err() {
            return Ok(false);
        }
        match std::fs::remove_dir(link_path).or_else(|_| std::fs::remove_file(link_path)) {
            Ok(()) => Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Error::Io(link_path.to_path_buf(), e)),
        }
    }
    #[cfg(not(windows))]
    {
        match std::fs::read_link(link_path) {
            Ok(existing) if existing == expected_target => Ok(true),
            Ok(_) => {
                // Wrong target — remove the stale symlink so the
                // caller's `create_dir_link` below doesn't EEXIST.
                let _ = std::fs::remove_dir(link_path).or_else(|_| std::fs::remove_file(link_path));
                Ok(false)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => {
                // `read_link` failed with EINVAL (entry exists but
                // isn't a symlink — e.g. a regular directory left by
                // a prior hoisted install) or another error.
                // Best-effort reclaim so the create call lands on a
                // clean slot.
                let _ =
                    std::fs::remove_dir_all(link_path).or_else(|_| std::fs::remove_file(link_path));
                Ok(false)
            }
        }
    }
}

/// Build a `dep_path → absolute on-disk target` map for every
/// `LocalSource::Link` in the graph. Returned `None` when the graph
/// has no link entries (vast majority of installs), so the materialize
/// hot path can short-circuit without a per-dep lookup.
pub fn build_nested_link_targets(
    project_dir: &Path,
    graph: &LockfileGraph,
) -> Option<BTreeMap<String, PathBuf>> {
    let map: BTreeMap<String, PathBuf> = graph
        .packages
        .iter()
        .filter_map(|(dp, pkg)| match pkg.local_source.as_ref() {
            Some(LocalSource::Link(rel)) => Some((dp.clone(), project_dir.join(rel))),
            _ => None,
        })
        .collect();
    if map.is_empty() { None } else { Some(map) }
}

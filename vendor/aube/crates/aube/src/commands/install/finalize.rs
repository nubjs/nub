use super::dep_selection::DepSelection;
use super::lifecycle::{
    JailBuildPolicy, run_dep_lifecycle_scripts, run_root_lifecycle, unreviewed_dep_builds,
};
use super::side_effects_cache::{SideEffectsCacheConfig, side_effects_cache_root};
use super::summary::{print_direct_dependency_summary, should_print_human_install_summary};
use super::sweep::sweep_orphaned_aube_entries;
use super::workspace::importer_project_dir;
use super::{InstallPhaseTimings, delta, unreviewed_builds};
use crate::state;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct FinalizePhaseInput<'a> {
    pub(super) cwd: &'a std::path::Path,
    pub(super) settings_ctx: &'a aube_settings::ResolveCtx<'a>,
    pub(super) store: &'a aube_store::Store,
    pub(super) graph: &'a aube_lockfile::LockfileGraph,
    pub(super) graph_for_link: &'a aube_lockfile::LockfileGraph,
    pub(super) manifests: &'a [(String, aube_manifest::PackageJson)],
    pub(super) lifecycle_manifests: &'a [(String, aube_manifest::PackageJson)],
    pub(super) direct_dep_info: &'a std::collections::HashMap<String, aube_resolver::DirectDepInfo>,
    pub(super) deprecations:
        &'a std::sync::Arc<std::sync::Mutex<Vec<crate::deprecations::DeprecationRecord>>>,
    pub(super) build_policy: &'a aube_scripts::BuildPolicy,
    pub(super) default_trust_floor: &'a super::default_trust::DefaultTrustFloor,
    pub(super) jail_policy: &'a JailBuildPolicy,
    pub(super) stats: &'a aube_linker::LinkStats,
    pub(super) node_linker: aube_linker::NodeLinker,
    pub(super) virtual_store_only: bool,
    pub(super) current_leaf_hashes: Option<BTreeMap<String, String>>,
    pub(super) current_subtree_hashes: Option<BTreeMap<String, String>>,
    pub(super) patch_hashes: BTreeMap<String, String>,
    pub(super) modules_dir_name: &'a str,
    pub(super) aube_dir: &'a std::path::Path,
    pub(super) virtual_store_dir_max_length: usize,
    pub(super) child_concurrency: usize,
    pub(super) side_effects_cache_setting: bool,
    pub(super) side_effects_cache_readonly_setting: bool,
    pub(super) strict_dep_builds_setting: bool,
    pub(super) ignore_scripts: bool,
    pub(super) skip_root_lifecycle: bool,
    pub(super) workspace_filter_empty: bool,
    pub(super) dep_selection: DepSelection,
    pub(super) cli_flags: &'a [(String, String)],
    pub(super) cached_count: usize,
    pub(super) fetch_count: usize,
    pub(super) start: std::time::Instant,
    pub(super) prog_ref: Option<&'a crate::progress::InstallProgress>,
    pub(super) phase_timings: &'a mut InstallPhaseTimings,
}

fn dep_build_policy_hash(
    build_policy: &aube_scripts::BuildPolicy,
    default_trust_floor: &super::default_trust::DefaultTrustFloor,
) -> String {
    let policy = build_policy.fingerprint();
    let floor = default_trust_floor.fingerprint();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"dep-build-policy-v1");
    hasher.update(&(policy.len() as u64).to_le_bytes());
    hasher.update(policy.as_bytes());
    hasher.update(&(floor.len() as u64).to_le_bytes());
    hasher.update(floor.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn lifecycle_delta_filter(
    cwd: &std::path::Path,
    graph_for_link: &aube_lockfile::LockfileGraph,
    patch_hashes: &BTreeMap<String, String>,
    build_policy: &aube_scripts::BuildPolicy,
    default_trust_floor: &super::default_trust::DefaultTrustFloor,
    dep_build_policy_hash: &str,
    virtual_store_only: bool,
) -> Option<BTreeSet<String>> {
    if virtual_store_only {
        return None;
    }
    let prior_policy_hash = state::read_state_dep_build_policy_hash(cwd)?;
    if prior_policy_hash != dep_build_policy_hash {
        tracing::debug!("delta: dep build policy changed; running full eligible build scan");
        return None;
    }
    let prior_leaf_hashes = state::read_state_package_content_hashes(cwd)?;
    let prior_subtree_hashes = state::read_state_subtree_hashes(cwd)?;
    let (current_leaf_hashes, current_subtree_hashes) =
        delta::compute_leaf_and_subtree_hashes(graph_for_link, patch_hashes, cwd);
    let plan = delta::diff(&prior_leaf_hashes, &current_leaf_hashes);
    let mut selected = plan.touched_set().clone();
    selected.extend(delta::changed_subtree_roots(
        &prior_subtree_hashes,
        &current_subtree_hashes,
    ));
    let prior_unreviewed: BTreeSet<String> = state::read_state_unreviewed_builds(cwd)
        .into_iter()
        .collect();
    select_previously_unreviewed_now_allowed(
        &mut selected,
        graph_for_link,
        build_policy,
        default_trust_floor,
        &prior_unreviewed,
    );
    tracing::debug!(
        "delta: dep lifecycle selected {} package(s) (leaf touched {}, graph total {})",
        selected.len(),
        plan.touched(),
        current_leaf_hashes.len(),
    );
    Some(selected)
}

fn select_previously_unreviewed_now_allowed(
    selected: &mut BTreeSet<String>,
    graph_for_link: &aube_lockfile::LockfileGraph,
    build_policy: &aube_scripts::BuildPolicy,
    default_trust_floor: &super::default_trust::DefaultTrustFloor,
    prior_unreviewed: &BTreeSet<String>,
) {
    if prior_unreviewed.is_empty() {
        return;
    }
    for (dep_path, pkg) in &graph_for_link.packages {
        if !prior_unreviewed.contains(&pkg.spec_key()) {
            continue;
        }
        if matches!(
            super::default_trust::decide_with_floor(
                build_policy,
                default_trust_floor,
                pkg,
                &graph_for_link.times,
            ),
            aube_scripts::AllowDecision::Allow
        ) {
            selected.insert(dep_path.clone());
        }
    }
}

pub(super) async fn run_finalize_phase(input: FinalizePhaseInput<'_>) -> miette::Result<()> {
    let FinalizePhaseInput {
        cwd,
        settings_ctx,
        store,
        graph,
        graph_for_link,
        manifests,
        lifecycle_manifests,
        direct_dep_info,
        deprecations,
        build_policy,
        default_trust_floor,
        jail_policy,
        stats,
        node_linker,
        virtual_store_only,
        current_leaf_hashes,
        current_subtree_hashes,
        patch_hashes,
        modules_dir_name,
        aube_dir,
        virtual_store_dir_max_length,
        child_concurrency,
        side_effects_cache_setting,
        side_effects_cache_readonly_setting,
        strict_dep_builds_setting,
        ignore_scripts,
        skip_root_lifecycle,
        workspace_filter_empty,
        dep_selection,
        cli_flags,
        cached_count,
        fetch_count,
        start,
        prog_ref,
        phase_timings,
    } = input;

    let placements_ref = stats.hoisted_placements.as_ref();

    // Tear down the progress display before running post-link lifecycle
    // scripts or printing the final summary — scripts write directly to
    // stdout/stderr and would collide with an active progress bar.
    //
    // Skip the CI-mode framed summary on a no-op install: `print_install_summary`
    // below will print the "Already up to date" branded line, and we don't
    // want CI users to see both the framed `[ ✓ … resolved N · reused N ]`
    // block and the branded line as redundant twins.
    let install_is_noop = stats.packages_linked == 0 && stats.top_level_linked == 0;
    if let Some(p) = prog_ref {
        p.finish(!install_is_noop);
    }

    if !ignore_scripts && strict_dep_builds_setting && !virtual_store_only {
        let unreviewed = unreviewed_dep_builds(
            aube_dir,
            graph_for_link,
            build_policy,
            default_trust_floor,
            virtual_store_dir_max_length,
            placements_ref,
        )?;
        if !unreviewed.is_empty() {
            return Err(miette!(
                "dependencies with build scripts must be reviewed before install:\n{}\nhelp: add the package(s) to `allowBuilds` with `true`/`false`, or set `strictDepBuilds=false`",
                unreviewed
                    .into_iter()
                    .map(|b| format!("  - {}", b.spec_key))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
    }

    let filtered_install = !workspace_filter_empty || dep_selection.is_filtered();
    let dep_build_policy_hash = dep_build_policy_hash(build_policy, default_trust_floor);
    let lifecycle_delta_filter = if ignore_scripts {
        None
    } else {
        lifecycle_delta_filter(
            cwd,
            graph_for_link,
            &patch_hashes,
            build_policy,
            default_trust_floor,
            &dep_build_policy_hash,
            virtual_store_only,
        )
    };

    // 7a. Dependency lifecycle scripts (allowBuilds).
    //     Every dep that the `BuildPolicy` explicitly allows runs its
    //     `preinstall` / `install` / `postinstall` scripts from inside
    //     its linked directory under `node_modules/.aube`. Reuses the
    //     already-constructed `build_policy` from above. Skipped
    //     entirely under `--ignore-scripts` (pnpm parity) and when the
    //     policy has no allow rules at all (fast path: no config, no
    //     cost). A failing dep script fails the whole install —
    //     matching pnpm's fail-fast default. No cross-project
    //     collision warning here: step 6a content-addresses the
    //     global store so two projects resolving the same
    //     `(dep-graph, engine)` share a safe directory and divergent
    //     resolutions land at distinct paths.
    // The `defaultTrust` floor can allow builds even when the policy
    // itself has no allow rules, so it keeps the phase alive too.
    if super::default_trust::dep_build_scripts_may_run(
        ignore_scripts,
        build_policy.has_any_allow_rule(),
        default_trust_floor.may_allow_any(),
    ) && !virtual_store_only
    {
        let phase_start = std::time::Instant::now();
        let side_effects_cache_root =
            side_effects_cache_setting.then(|| side_effects_cache_root(store));
        let side_effects_cache = side_effects_cache_root
            .as_deref()
            .map(|root| {
                if side_effects_cache_readonly_setting {
                    SideEffectsCacheConfig::RestoreOnly(root)
                } else {
                    SideEffectsCacheConfig::RestoreAndSave(root)
                }
            })
            .unwrap_or(SideEffectsCacheConfig::Disabled);
        let ran = run_dep_lifecycle_scripts(
            cwd,
            modules_dir_name,
            aube_dir,
            graph_for_link,
            build_policy,
            default_trust_floor,
            virtual_store_dir_max_length,
            child_concurrency,
            placements_ref,
            side_effects_cache,
            jail_policy,
            lifecycle_delta_filter.as_ref(),
            None,
        )
        .await?;
        if ran > 0 {
            tracing::debug!("allowBuilds: ran {ran} dep lifecycle script(s)");
        }
        phase_timings.record("dep_lifecycle", phase_start.elapsed());
    }

    // 7b. Post-link root lifecycle hooks: install → postinstall → prepare.
    //     npm and pnpm run these in this order after deps are linked so the
    //     scripts can use anything they depend on. Skipped with --ignore-scripts
    //     and under `virtualStoreOnly` — scripts typically resolve
    //     binaries via `node_modules/.bin`, which doesn't exist in
    //     that mode.
    //     A hook that's not defined in package.json is a silent no-op.
    //     A hook that exits non-zero fails the install (fail-fast, matching pnpm).
    if !ignore_scripts && !virtual_store_only && !skip_root_lifecycle {
        let phase_start = std::time::Instant::now();
        for (importer_path, importer_manifest) in lifecycle_manifests {
            let project_dir = importer_project_dir(cwd, importer_path);
            for hook in [
                aube_scripts::LifecycleHook::Install,
                aube_scripts::LifecycleHook::PostInstall,
                aube_scripts::LifecycleHook::Prepare,
            ] {
                run_root_lifecycle(&project_dir, modules_dir_name, importer_manifest, hook).await?;
            }
        }
        phase_timings.record("root_lifecycle", phase_start.elapsed());
    }

    // 8. Write state file for auto-install tracking.
    //    Record whether this was a --prod install so ensure_installed knows
    //    to re-install the full graph before running dev tooling.
    //    Skipped under `virtualStoreOnly` — the state sidecar is
    //    keyed off a materialized node_modules tree that doesn't
    //    exist, and writing it would lie on the next auto-install
    //    freshness check. Same skip when a workspace filter scoped the
    //    run to a subset of importers. State hash is derived from full
    //    manifest + lockfile inputs, so writing it after a partial
    //    materialize would let the next unfiltered `aube install` hit
    //    the warm path while unfiltered importers are still empty.
    //    Observed via `aube add <pkg> --filter <ws>` leaving the new
    //    dep unmaterialized.
    // Walk the linked graph once for the unreviewed-builds set; reused
    // by both the state writer (so warm-path repeats keep nudging) and
    // the post-install warning emission below. The walk does a stat per
    // package, so collapsing two callers into one cuts the linker-tail
    // cost on large graphs roughly in half.
    let unreviewed_builds = if !ignore_scripts && !strict_dep_builds_setting && !virtual_store_only
    {
        unreviewed_dep_builds(
            aube_dir,
            graph_for_link,
            build_policy,
            default_trust_floor,
            virtual_store_dir_max_length,
            placements_ref,
        )?
    } else {
        Vec::new()
    };

    if !virtual_store_only && !filtered_install {
        let phase_start = std::time::Instant::now();
        // Fingerprint every package in the final graph so the next
        // install can diff and skip unchanged entries. Missing or
        // stale fingerprints fall back to a full install on the
        // read side. Safe for older readers that ignore the field.
        // When the early pass already produced the leaf map for
        // `graph_for_link`, reuse it here as long as it covers every
        // dep_path in the writeback `graph`. Filtered installs short-
        // circuit before this branch so the two graphs are normally
        // identical, but verifying every key keeps the reuse safe
        // against any future code path that diverges them. Any miss
        // falls back to a fresh compute over `graph`.
        let package_content_hashes = current_leaf_hashes
            .filter(|leaf| {
                leaf.len() == graph.packages.len()
                    && graph.packages.keys().all(|k| leaf.contains_key(k))
            })
            .unwrap_or_else(|| delta::compute_package_hashes(graph, &patch_hashes, cwd));
        let package_subtree_hashes = current_subtree_hashes.unwrap_or_else(|| {
            delta::compute_subtree_hashes_from_leaf(graph, &package_content_hashes)
        });
        let graph_lthash = hex::encode(delta::lthash_of(&package_content_hashes).digest());
        let package_json_hashes = state::collect_package_json_hashes_from_manifests(cwd, manifests);
        // Diff against the previous install. Logs delta counts at
        // debug so `-v` installs surface what actually moved. A
        // later pass feeds the plan into fetch and link as a
        // pre-filter.
        if let Some(prior) = state::read_state_package_content_hashes(cwd) {
            let plan = delta::diff(&prior, &package_content_hashes);
            if !plan.is_empty() {
                // Touched set built once. Doubles as a membership
                // probe so future wiring exercises the same shape
                // of predicate shipped in production.
                let touched = plan.touched_set();
                tracing::debug!(
                    "delta: +{} ~{} -{} ({} touched vs {} total, should_touch(first-added)={})",
                    plan.added.len(),
                    plan.changed.len(),
                    plan.removed.len(),
                    touched.len(),
                    package_content_hashes.len(),
                    plan.added.first().is_some_and(|dp| plan.should_touch(dp)),
                );
            }
            // Incremental LtHash self-check. Start from the prior
            // accumulator, apply the observed delta, confirm the
            // result matches a from-scratch hash of the new graph.
            // Cheap sanity on the homomorphic add/remove ops. The
            // future causal scheduler needs these two to stay in
            // lockstep with the full recompute.
            if let Some(prior_lthash_hex) = state::read_state_graph_lthash(cwd)
                && let Ok(prior_bytes) = hex::decode(&prior_lthash_hex)
                && prior_bytes.len() == 32
            {
                let mut incr = delta::lthash_of(&prior);
                for dp in &plan.removed {
                    if let Some(fp) = prior.get(dp) {
                        incr.remove(fp);
                    }
                }
                for dp in &plan.added {
                    if let Some(fp) = package_content_hashes.get(dp) {
                        incr.add(fp);
                    }
                }
                for dp in &plan.changed {
                    if let Some(old_fp) = prior.get(dp) {
                        incr.remove(old_fp);
                    }
                    if let Some(new_fp) = package_content_hashes.get(dp) {
                        incr.add(new_fp);
                    }
                }
                if hex::encode(incr.digest()) != graph_lthash {
                    // Real bug signal, not routine noise. `debug`
                    // hides it behind `-v` so CI would silently
                    // ship broken homomorphic bookkeeping.
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_LTHASH_MISMATCH,
                        "lthash: incremental/full mismatch, homomorphic invariant broken"
                    );
                }
            }
        }
        // LtHash diagnostic. One 32-byte compare proves graph
        // equivalence with the last install. Beats the map diff
        // when both sides are known good.
        if let Some(prior_lthash) = state::read_state_graph_lthash(cwd)
            && prior_lthash != graph_lthash
        {
            tracing::debug!(
                "lthash: graph content digest changed ({}..{} -> {}..{})",
                &prior_lthash[..8.min(prior_lthash.len())],
                &prior_lthash[prior_lthash.len().saturating_sub(8)..],
                &graph_lthash[..8],
                &graph_lthash[graph_lthash.len() - 8..],
            );
        }
        // Merkle subtree diagnostic. How many subtree roots moved
        // vs how many leaves moved. Fewer roots means tighter
        // re-link scope once the delta linker lands.
        if let Some(prior_subtrees) = state::read_state_subtree_hashes(cwd) {
            let changed_subtrees = package_subtree_hashes
                .iter()
                .filter(|(k, v)| prior_subtrees.get(*k).is_none_or(|old| old != *v))
                .count();
            if changed_subtrees > 0 {
                tracing::debug!(
                    "merkle: {} subtree hashes changed of {}",
                    changed_subtrees,
                    package_subtree_hashes.len()
                );
            }
        }
        // Persist the unreviewed-builds set so the warm-path
        // short-circuit can re-emit the warning on repeat installs.
        // Computed once above and reused here. The state file
        // carries only spec_keys; per-package suspicion data is
        // re-derived from the live tree on each install rather
        // than persisted, since the regex set evolves with aube.
        let unreviewed_builds_for_state: Vec<String> = unreviewed_builds
            .iter()
            .map(|b| b.spec_key.clone())
            .collect();
        state::write_state(
            cwd,
            state::WriteStateInput {
                section_filtered: dep_selection.prod_or_dev_axis(),
                package_json_hashes,
                cli_flags,
                package_content_hashes,
                graph_lthash,
                package_subtree_hashes,
                dep_build_policy_hash: if ignore_scripts {
                    String::new()
                } else {
                    dep_build_policy_hash
                },
                layout: state::WriteStateLayout {
                    graph: graph_for_link,
                    node_linker,
                    modules_dir_name,
                    aube_dir,
                    virtual_store_dir_max_length,
                    placements: placements_ref,
                },
                unreviewed_builds: unreviewed_builds_for_state,
            },
        )
        .into_diagnostic()
        .wrap_err("failed to write install state")?;
        phase_timings.record("state", phase_start.elapsed());
    }

    // 8a. Sweep orphaned `.aube/<dep_path>` entries older than
    //     `modulesCacheMaxAge`. The "in use" set is built from the
    //     **unfiltered** `graph`, not `graph_for_link`, so that a
    //     `--prod` / `--dev` / `--no-optional` / `--filter` install
    //     doesn't treat the deps it skipped this run as orphans —
    //     a subsequent full install would otherwise have to re-fetch
    //     them. Runs best-effort: I/O errors are logged and swallowed
    //     so a partial sweep never fails an install that otherwise
    //     succeeded.
    let modules_cache_max_age_minutes =
        aube_settings::resolved::modules_cache_max_age(settings_ctx);
    if modules_cache_max_age_minutes > 0 && !virtual_store_only {
        let phase_start = std::time::Instant::now();
        let removed = sweep_orphaned_aube_entries(
            aube_dir,
            graph,
            virtual_store_dir_max_length,
            std::time::Duration::from_secs(modules_cache_max_age_minutes.saturating_mul(60)),
        );
        if removed > 0 {
            tracing::debug!("modulesCacheMaxAge: swept {removed} orphaned .aube entry/entries");
        }
        phase_timings.record("sweep", phase_start.elapsed());
    }

    let elapsed = start.elapsed();
    tracing::debug!(
        "Done in {:.0?}: {} packages ({} cached), {} files linked, {} top-level",
        elapsed,
        stats.packages_linked + stats.packages_cached,
        stats.packages_cached,
        stats.files_linked,
        stats.top_level_linked
    );
    phase_timings.write(
        cwd,
        elapsed,
        graph_for_link.packages.len(),
        cached_count,
        fetch_count,
    );

    if stats.packages_linked == 0
        && stats.packages_cached == 0
        && graph_for_link
            .packages
            .values()
            .any(|p| p.local_source.is_none())
    {
        return Err(miette!("no packages were linked — something went wrong"));
    }

    // Deprecation warnings, gated by the `deprecationWarnings` setting.
    // Prune to packages still in the finalized graph so we don't warn
    // on platform-mismatched optionals that `filter_graph` trimmed,
    // then dedupe across peer-context dep_path variants.
    {
        let mut records = std::mem::take(&mut *deprecations.lock().unwrap());
        crate::deprecations::retain_in_graph(&mut records, graph_for_link);
        let records = crate::deprecations::dedupe(records);
        if !records.is_empty() {
            let mode = aube_settings::resolved::deprecation_warnings(settings_ctx);
            crate::deprecations::render_install_warnings(&records, graph_for_link, mode);
        }
    }

    // Final user-facing output. When linking did real work, print the
    // direct deps that now exist at the top level before the green
    // `✓ installed N packages in Xs` line. Text modes such as `-v` and
    // `--reporter=append-only` skip the progress object but still get the
    // dependency summary; silent and ndjson stay machine-clean.
    if !install_is_noop && should_print_human_install_summary() {
        print_direct_dependency_summary(graph_for_link, manifests, direct_dep_info);
    }
    if let Some(p) = prog_ref {
        p.print_install_summary(
            stats.packages_linked,
            stats.top_level_linked,
            graph_for_link.packages.len(),
            elapsed,
        );
    }

    // Surface packages whose build scripts were skipped because they're
    // not on the `allowBuilds` / `onlyBuiltDependencies` allowlist. Without
    // this, a fresh install of a project that depends on native bindings
    // (`better-sqlite3`, `esbuild`, napi-rs packages, etc.) looks like it
    // succeeded but leaves those packages unbuilt — the failure only
    // surfaces later when something tries to `require` the binding.
    // Skipped under `--ignore-scripts`, `virtualStoreOnly`, and
    // `strictDepBuilds=true` (the strict path already errored above).
    if !unreviewed_builds.is_empty() {
        unreviewed_builds::emit_warning(&unreviewed_builds);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::{LockedPackage, LockfileGraph};

    fn policy() -> aube_scripts::BuildPolicy {
        let (policy, warnings) =
            aube_scripts::BuildPolicy::from_config(&BTreeMap::new(), &[], &[], false);
        assert!(
            warnings.is_empty(),
            "unexpected policy warnings: {warnings:?}"
        );
        policy
    }

    #[test]
    fn lifecycle_delta_selects_prior_unreviewed_now_allowed_by_default_trust() {
        let mut graph = LockfileGraph::default();
        let pkg = LockedPackage {
            name: "esbuild".into(),
            version: "1.0.0".into(),
            dep_path: "esbuild@1.0.0".into(),
            integrity: Some("sha512-esbuild".into()),
            ..Default::default()
        };
        graph
            .times
            .insert("esbuild@1.0.0".into(), "2000-01-01T00:00:00.000Z".into());
        graph.packages.insert(pkg.dep_path.clone(), pkg);

        let mut selected = BTreeSet::new();
        let prior_unreviewed = BTreeSet::from(["esbuild@1.0.0".to_string()]);
        select_previously_unreviewed_now_allowed(
            &mut selected,
            &graph,
            &policy(),
            &super::super::default_trust::DefaultTrustFloor::test_enabled(
                "2100-01-01T00:00:00.000Z",
            ),
            &prior_unreviewed,
        );

        assert_eq!(selected, BTreeSet::from(["esbuild@1.0.0".to_string()]));
    }
}

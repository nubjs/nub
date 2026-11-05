use super::run::load_manifest;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::HashSet;

/// `aube rebuild [<pkg>...]` — without args, re-run install / postinstall
/// work for dependency packages allowed by the active `allowBuilds` /
/// `onlyBuiltDependencies` policy. The projects' own lifecycle hooks do not
/// run: pnpm 12's `rebuild` builds dependencies only.
///
/// With one or more package names, run lifecycle scripts only for the
/// named deps. Match is by graph `name`, which is the in-tree alias when
/// one is configured (so `aube rebuild my-alias` works for a manifest
/// entry like `"my-alias": "npm:real-pkg@1.0"`, matching pnpm). Naming a
/// package does not approve it: as in pnpm 12, an unapproved package is
/// reported as an ignored build — `ERR_AUBE_IGNORED_BUILDS` under the
/// default `strictDepBuilds`, a warning otherwise.
///
/// Unlike the other lifecycle shortcuts, `rebuild` intentionally does not
/// auto-install: `aube install` already runs these same scripts after
/// linking, so triggering an install here would double-run every script
/// on a stale tree. Users who actually want a fresh install should run
/// `aube install`.
#[derive(Debug, usage_rs::Args)]
pub struct RebuildArgs {
    /// Optional package names. When supplied, only matching deps'
    /// scripts run. Match is by graph `name`, not by `dep_path`. The
    /// active `allowBuilds` policy still decides which of them may run.
    #[usage(arg, name = "PACKAGE")]
    pub packages: Vec<String>,
}

pub async fn run(
    args: RebuildArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    if !filter.is_empty() {
        return run_filtered(&args, &filter).await;
    }
    rebuild_project(&args.packages, None).await
}

/// Rebuild the project at the current directory. `importers` narrows the
/// rebuild to those workspace projects' dependencies, as a filtered
/// `pnpm rebuild` does; `None` takes every dependency in the lockfile.
async fn rebuild_project(packages: &[String], importers: Option<&[String]>) -> miette::Result<()> {
    let selected = if packages.is_empty() {
        None
    } else {
        Some(packages.iter().cloned().collect::<HashSet<String>>())
    };

    let project_dir = crate::dirs::project_root()?;
    let project_manifest = load_manifest(&project_dir)?;
    // Under `sharedWorkspaceLockfile=false` a member keeps its own lockfile,
    // but install still links every member into the workspace root's virtual
    // store and takes build approvals from the root. So a member's graph comes
    // from its own lockfile and everything else — store, settings, policy,
    // the project the scripts run for — from the root, as install did.
    let cwd = super::install::unshared_lockfile_workspace_root(&project_dir)
        .unwrap_or_else(|| project_dir.clone());
    // A root holding only `pnpm-workspace.yaml` is valid, as install accepts.
    let manifest = if cwd == project_dir {
        project_manifest.clone()
    } else {
        super::load_manifest_or_default(&cwd)?
    };
    let files = crate::commands::FileSources::load(&cwd);
    let (workspace, raw_workspace) = aube_manifest::workspace::load_both(&cwd)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    let env_snapshot = aube_settings::values::capture_env();
    let settings_ctx = files.ctx(&raw_workspace, &env_snapshot, &[]);
    super::configure_script_settings(&cwd, &settings_ctx, Some("rebuild"));

    let graph = match super::install::parse_project_lockfile(&project_dir, &project_manifest) {
        Ok(graph) => Some(graph),
        Err(aube_lockfile::Error::NotFound(_)) => None,
        Err(e) => return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    };

    // Selective rebuild needs a graph to match names against. Without
    // the lockfile the unmatched-name check below never runs, and the
    // command would exit Ok with no scripts run and no diagnostic —
    // invisible in CI.
    if selected.is_some() && graph.is_none() {
        return Err(miette!(
            "no lockfile found at {} — run `{}` before targeting specific packages",
            project_dir.display(),
            aube_util::cmd("install")
        ));
    }

    let modules_dir_name = aube_settings::resolved::modules_dir(&settings_ctx);
    let aube_dir = super::resolve_virtual_store_dir(&settings_ctx, &cwd);
    if let Some(graph) = graph {
        if let Some(selected) = selected.as_ref() {
            let known: HashSet<&str> = graph.packages.values().map(|p| p.name.as_str()).collect();
            let unmatched: Vec<&str> = selected
                .iter()
                .filter(|n| !known.contains(n.as_str()))
                .map(String::as_str)
                .collect();
            if !unmatched.is_empty() {
                let mut sorted = unmatched;
                sorted.sort_unstable();
                return Err(miette!(
                    "no installed dependency matches: {}",
                    sorted.join(", ")
                ));
            }
        }

        // The tree the last install linked: its `--prod` / `--dev` /
        // `--no-optional` selection drops the same edges here, so a package
        // installed only for an unselected project stays out.
        let closure = importers.map(|importers| {
            let selection = crate::state::read_dep_selection(&cwd);
            let installed = graph.filter_deps(|dep| selection.keeps(dep));
            importers_dep_closure(&installed, importers)
        });

        let policy_manifests =
            super::install::build_policy_manifests(&cwd, &manifest, &settings_ctx)?;
        let (policy, warnings) = super::install::build_policy_from_manifest_sources(
            policy_manifests.iter(),
            &workspace,
            aube_settings::resolved::dangerously_allow_all_builds(&settings_ctx),
        );
        for warning in warnings {
            eprintln!("warn: {warning}");
        }

        // The generated accessor already reads `nodeLinker` from
        // `raw_workspace`, which is the same map `workspace.node_linker`
        // is parsed out of — no need for a separate fallback on the
        // typed struct field.
        let node_linker_setting = aube_settings::resolved::node_linker(&settings_ctx);
        let hoisting_limits = crate::commands::settings_hoisting_limits_to_linker(
            aube_settings::resolved::hoisting_limits(&settings_ctx),
        );
        let hoisted_placements = match node_linker_setting {
            aube_settings::resolved::NodeLinker::Pnp => {
                return Err(miette!(
                    "node-linker=pnp is not supported by aube; use `isolated` (default) or `hoisted`"
                ));
            }
            aube_settings::resolved::NodeLinker::Hoisted => {
                Some(match crate::state::read_hoisted_placements(&cwd) {
                    Some(placements) => placements,
                    None => aube_linker::HoistedPlacements::from_graph(
                        &cwd,
                        &graph,
                        &modules_dir_name,
                        hoisting_limits,
                    )?,
                })
            }
            aube_settings::resolved::NodeLinker::Isolated => None,
        };

        if policy.has_any_allow_rule() {
            let child_concurrency =
                aube_settings::resolved::child_concurrency(&settings_ctx) as usize;
            let (jail_policy, jail_policy_warnings) =
                super::install::JailBuildPolicy::from_settings(&settings_ctx, &workspace);
            for warning in jail_policy_warnings {
                eprintln!("warn: {warning}");
            }
            let side_effects_cache_root =
                if aube_settings::resolved::side_effects_cache(&settings_ctx) {
                    let store = super::open_store(&cwd)?;
                    Some(super::install::side_effects_cache_root(&store))
                } else {
                    None
                };
            let node_version = crate::engines::build_node_version();
            // Re-emit per-dep `.bin/` shims so a rebuild on a tree
            // that pre-dates the transitive-bin fix still lands them
            // on PATH for the lifecycle scripts. `link_dep_bins`
            // is idempotent, so re-running on an already-wired tree
            // is a no-op.
            let isolated = !matches!(
                node_linker_setting,
                aube_settings::resolved::NodeLinker::Hoisted
            );
            let canonicalize_package_dir = cfg!(windows)
                && isolated
                && super::install::detect_existing_global_virtual_store(
                    &cwd,
                    &aube_dir,
                    &modules_dir_name,
                    &super::global_virtual_store_dir(&cwd),
                )
                .unwrap_or(false);
            let prefer_symlinked_executables =
                aube_settings::resolved::prefer_symlinked_executables(&settings_ctx)
                    .or(isolated.then_some(false));
            let hidden_modules_dir = aube_dir.join("node_modules");
            let shim_opts = aube_linker::BinShimOptions {
                extend_node_path: aube_settings::resolved::extend_node_path(&settings_ctx),
                prefer_symlinked_executables,
                hidden_modules_dir: isolated.then_some(hidden_modules_dir.as_path()),
            };
            let mut pkg_json_cache = super::install::PkgJsonCache::new();
            let mut managed_bin_links = super::install::ManagedBinLinks::capturing();
            super::install::link_dep_bins(super::install::LinkDepBinsInput {
                aube_dir: &aube_dir,
                graph: &graph,
                virtual_store_dir_max_length: super::resolve_virtual_store_dir_max_length(
                    &settings_ctx,
                ),
                placements: hoisted_placements.as_ref(),
                shim_opts,
                cache: &mut pkg_json_cache,
                managed: &mut managed_bin_links,
                preserved: None,
            })?;
            super::install::run_dep_lifecycle_scripts(
                &cwd,
                &modules_dir_name,
                &aube_dir,
                &graph,
                &policy,
                // No defaultTrust floor on rebuild: no advisory gate
                // runs here.
                &super::install::DefaultTrustFloor::disabled(),
                super::resolve_virtual_store_dir_max_length(&settings_ctx),
                canonicalize_package_dir,
                child_concurrency,
                hoisted_placements.as_ref(),
                side_effects_cache_root
                    .as_deref()
                    .map(|root| {
                        // `rebuild` means "run scripts again"; readonly
                        // cache may not write, but it must not restore and
                        // skip the script work either.
                        if aube_settings::resolved::side_effects_cache_readonly(&settings_ctx) {
                            super::install::SideEffectsCacheConfig::Disabled
                        } else {
                            super::install::SideEffectsCacheConfig::SaveOnlyOverwrite(
                                super::install::SideEffectsCacheLocation {
                                    root,
                                    node_version: node_version.as_deref(),
                                },
                            )
                        }
                    })
                    .unwrap_or(super::install::SideEffectsCacheConfig::Disabled),
                &jail_policy,
                closure.as_ref(),
                selected.as_ref(),
            )
            .await?;
            let preserved = super::install::remove_managed_bin_links(&managed_bin_links)?;
            let mut refreshed_pkg_json_cache = super::install::PkgJsonCache::new();
            let mut refreshed_bin_links = super::install::ManagedBinLinks::default();
            super::install::link_dep_bins(super::install::LinkDepBinsInput {
                aube_dir: &aube_dir,
                graph: &graph,
                virtual_store_dir_max_length: super::resolve_virtual_store_dir_max_length(
                    &settings_ctx,
                ),
                placements: hoisted_placements.as_ref(),
                shim_opts,
                cache: &mut refreshed_pkg_json_cache,
                managed: &mut refreshed_bin_links,
                preserved: Some(&preserved),
            })?;
            super::install::remove_unclaimed_preserved_bin_links(
                &managed_bin_links,
                &preserved,
                &refreshed_bin_links,
            )?;
        }
        return super::install::report_unreviewed_rebuilds(
            &cwd,
            &aube_dir,
            &graph,
            &policy,
            super::resolve_virtual_store_dir_max_length(&settings_ctx),
            hoisted_placements.as_ref(),
            closure.as_ref(),
            selected.as_ref(),
            aube_settings::resolved::strict_dep_builds(&settings_ctx)
                || aube_settings::resolved::paranoid(&settings_ctx),
        );
    }

    Ok(())
}

async fn run_filtered(
    args: &RebuildArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    let cwd = crate::dirs::cwd()?;
    let (root, matched) = super::select_workspace_packages(&cwd, filter, "rebuild")?;
    if matched.is_empty() {
        return super::no_projects_matched(&root, filter, super::NoMatchReport::Quiet);
    }
    let result = async {
        // A shared workspace lockfile: the members have none of their own,
        // so rebuild the selected projects' dependencies from the root, in
        // one pass, as pnpm does.
        if let Some(importers) = root_lockfile_importers(&root, &matched)? {
            super::retarget_cwd(&root)?;
            return rebuild_project(&args.packages, Some(&importers)).await;
        }
        let mut ignored_builds = None;
        for pkg in matched {
            super::retarget_cwd(&pkg.dir)?;
            super::install::defer_ignored_builds(
                Box::pin(rebuild_project(&args.packages, None)).await,
                &mut ignored_builds,
            )?;
        }
        ignored_builds.map_or(Ok(()), Err)
    }
    .await;
    super::finish_filtered_workspace(&cwd, result)
}

/// The selected projects' importer keys, when the workspace root's
/// lockfile records every one of them.
fn root_lockfile_importers(
    root: &std::path::Path,
    matched: &[aube_workspace::selector::SelectedPackage],
) -> miette::Result<Option<Vec<String>>> {
    let Ok(manifest) = load_manifest(root) else {
        return Ok(None);
    };
    let graph = match aube_lockfile::parse_lockfile(root, &manifest) {
        Ok(graph) => graph,
        Err(aube_lockfile::Error::NotFound(_)) => return Ok(None),
        Err(e) => return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    };
    let importers = matched
        .iter()
        .map(|pkg| super::workspace_importer_path(root, &pkg.dir))
        .collect::<miette::Result<Vec<_>>>()?;
    Ok(importers
        .iter()
        .all(|importer| graph.importers.contains_key(importer))
        .then_some(importers))
}

fn importers_dep_closure(
    graph: &aube_lockfile::LockfileGraph,
    importers: &[String],
) -> std::collections::BTreeSet<String> {
    let direct = importers
        .iter()
        .filter_map(|importer| graph.importers.get(importer))
        .flatten()
        .map(|dep| dep.dep_path.clone())
        .collect();
    super::dep_closure_from(graph, direct, false)
        .into_keys()
        .collect()
}

use super::run::load_manifest;
use aube_scripts::LifecycleHook;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::HashSet;

/// `aube rebuild [<pkg>...]` — without args, re-run the root package's
/// preinstall hook, then install / postinstall work for dependency
/// packages allowed by the active `allowBuilds` /
/// `onlyBuiltDependencies` policy, then the root package's install /
/// postinstall / prepare lifecycle hooks.
///
/// With one or more package names, run lifecycle scripts only for the
/// named deps and skip the root hooks. Match is by graph `name`, which
/// is the in-tree alias when one is configured (so `aube rebuild
/// my-alias` works for a manifest entry like
/// `"my-alias": "npm:real-pkg@1.0"`, matching pnpm).
///
/// Unlike the other lifecycle shortcuts, `rebuild` intentionally does not
/// auto-install: `aube install` already runs these same four hooks after
/// linking, so triggering an install here would double-run every script
/// on a stale tree. Users who actually want a fresh install should run
/// `aube install`.
#[derive(Debug, Args)]
pub struct RebuildArgs {
    /// Optional package names. When supplied, only matching deps'
    /// scripts run; the root lifecycle hooks (preinstall, install,
    /// postinstall, prepare) are skipped. Match is by graph `name`,
    /// not by `dep_path`. The active `allowBuilds` /
    /// `onlyBuiltDependencies` policy is bypassed for the named
    /// deps — naming the package is the explicit opt-in.
    #[arg(value_name = "PACKAGE")]
    pub packages: Vec<String>,
}

pub async fn run(
    args: RebuildArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    if !filter.is_empty() {
        return run_filtered(&args, &filter).await;
    }
    let selected = if args.packages.is_empty() {
        None
    } else {
        Some(args.packages.iter().cloned().collect::<HashSet<String>>())
    };

    let cwd = crate::dirs::project_root()?;
    let manifest = load_manifest(&cwd)?;
    let files = crate::commands::FileSources::load(&cwd);
    let (workspace, raw_workspace) = aube_manifest::workspace::load_both(&cwd)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    let env_snapshot = aube_settings::values::capture_env();
    let settings_ctx = files.ctx(&raw_workspace, &env_snapshot, &[]);
    super::configure_script_settings(&cwd, &settings_ctx, Some("rebuild"));

    let graph = match aube_lockfile::parse_lockfile(&cwd, &manifest) {
        Ok(graph) => Some(graph),
        Err(aube_lockfile::Error::NotFound(_)) => None,
        Err(e) => return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    };

    // Selective rebuild needs a graph to match names against. Without
    // the lockfile the unmatched-name check below never runs, root
    // hooks are skipped (selected.is_some()), and the command would
    // exit Ok with no scripts run and no diagnostic — invisible in CI.
    if selected.is_some() && graph.is_none() {
        return Err(miette!(
            "no lockfile found at {} — run `{}` before targeting specific packages",
            cwd.display(),
            aube_util::cmd("install")
        ));
    }

    let modules_dir_name = aube_settings::resolved::modules_dir(&settings_ctx);
    let aube_dir = super::resolve_virtual_store_dir(&settings_ctx, &cwd);
    if selected.is_none() {
        aube_scripts::run_root_hook(
            &cwd,
            &modules_dir_name,
            &manifest,
            LifecycleHook::PreInstall,
        )
        .await
        .map_err(|e| miette!("{}", e))?;
    }

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

        let (policy, warnings) =
            super::install::build_policy_from_sources(&manifest, &workspace, false);
        for warning in warnings {
            eprintln!("warn: {warning}");
        }

        if selected.is_some() || policy.has_any_allow_rule() {
            let child_concurrency =
                aube_settings::resolved::child_concurrency(&settings_ctx) as usize;
            let (jail_policy, jail_policy_warnings) =
                super::install::JailBuildPolicy::from_settings(&settings_ctx, &workspace);
            for warning in jail_policy_warnings {
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
                    Some(aube_linker::HoistedPlacements::from_graph(
                        &cwd,
                        &graph,
                        &modules_dir_name,
                        hoisting_limits,
                    )?)
                }
                aube_settings::resolved::NodeLinker::Isolated => None,
            };
            let side_effects_cache_root =
                if aube_settings::resolved::side_effects_cache(&settings_ctx) {
                    let store = super::open_store(&cwd)?;
                    Some(super::install::side_effects_cache_root(&store))
                } else {
                    None
                };
            // Re-emit per-dep `.bin/` shims so a rebuild on a tree
            // that pre-dates the transitive-bin fix still lands them
            // on PATH for the lifecycle scripts. `link_bins_for_dep`
            // is idempotent, so re-running on an already-wired tree
            // is a no-op.
            let isolated = !matches!(
                node_linker_setting,
                aube_settings::resolved::NodeLinker::Hoisted
            );
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
            super::install::link_dep_bins(
                &aube_dir,
                &graph,
                super::resolve_virtual_store_dir_max_length(&settings_ctx),
                hoisted_placements.as_ref(),
                shim_opts,
                &mut pkg_json_cache,
            )?;
            super::install::run_dep_lifecycle_scripts(
                &cwd,
                &modules_dir_name,
                &aube_dir,
                &graph,
                &policy,
                // No defaultTrust floor on rebuild: no advisory gate
                // runs here, and `aube rebuild <name>` already
                // bypasses the policy for explicitly named packages.
                &super::install::DefaultTrustFloor::disabled(),
                super::resolve_virtual_store_dir_max_length(&settings_ctx),
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
                            super::install::SideEffectsCacheConfig::SaveOnlyOverwrite(root)
                        }
                    })
                    .unwrap_or(super::install::SideEffectsCacheConfig::Disabled),
                &jail_policy,
                None,
                selected.as_ref(),
            )
            .await?;
        }
    }

    if selected.is_none() {
        for hook in [
            LifecycleHook::Install,
            LifecycleHook::PostInstall,
            LifecycleHook::Prepare,
        ] {
            aube_scripts::run_root_hook(&cwd, &modules_dir_name, &manifest, hook)
                .await
                .map_err(|e| miette!("{}", e))?;
        }
    }

    Ok(())
}

async fn run_filtered(
    args: &RebuildArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    let cwd = crate::dirs::cwd()?;
    let (_root, matched) = super::select_workspace_packages(&cwd, filter, "rebuild")?;
    let result = async {
        for pkg in matched {
            super::retarget_cwd(&pkg.dir)?;
            Box::pin(run(
                RebuildArgs {
                    packages: args.packages.clone(),
                },
                aube_workspace::selector::EffectiveFilter::default(),
            ))
            .await?;
        }
        Ok(())
    }
    .await;
    super::finish_filtered_workspace(&cwd, result)
}

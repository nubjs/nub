use crate::progress::InstallProgress;
use aube_lockfile::{DriftStatus, LockfileGraph, LockfileKind};
use miette::{Context, IntoDiagnostic, miette};
use std::collections::HashMap;
use std::path::Path;

use super::frozen::FrozenMode;
use super::lockfile_dir::{parse_lockfile_dir_remapped_with_kind, write_lockfile_dir_remapped};
use super::settings::{
    ResolverConfigInputs, configure_resolver, maybe_cleanup_unused_catalogs,
    stamp_pnpm_config_checksums,
};
use super::workspace::write_per_project_lockfiles;

pub(super) type ParsedLockfile = Option<(LockfileGraph, LockfileKind)>;

pub(super) fn pre_parse_lockfile(
    lockfile_enabled: bool,
    mode: FrozenMode,
    lockfile_dir: &Path,
    lockfile_importer_key: &str,
    manifest: &aube_manifest::PackageJson,
) -> miette::Result<ParsedLockfile> {
    if !lockfile_enabled || !matches!(mode, FrozenMode::Fix | FrozenMode::Prefer) {
        return Ok(None);
    }
    match parse_lockfile_dir_remapped_with_kind(lockfile_dir, lockfile_importer_key, manifest) {
        Ok(parsed) => Ok(Some(parsed)),
        Err(aube_lockfile::Error::NotFound(_)) => Ok(None),
        Err(e) if active_lockfile_has_conflict_markers(lockfile_dir) => {
            warn_lockfile_conflict_markers(lockfile_dir, &e);
            Ok(None)
        }
        Err(e) => Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    }
}

pub(super) struct LockfileOnlyInput<'a> {
    pub cwd: &'a Path,
    pub mode: FrozenMode,
    pub lockfile_dir: &'a Path,
    pub lockfile_importer_key: &'a str,
    pub manifest: &'a aube_manifest::PackageJson,
    pub manifests: &'a [(String, aube_manifest::PackageJson)],
    pub per_project_write_selection: Option<&'a std::collections::BTreeSet<String>>,
    pub ws_config: &'a aube_manifest::workspace::WorkspaceConfig,
    pub workspace_catalogs: &'a crate::commands::CatalogMap,
    pub settings_ctx: &'a aube_settings::ResolveCtx<'a>,
    pub lockfile_pre_parse: Option<&'a (LockfileGraph, LockfileKind)>,
    pub lockfile_conflict_marker_warning_emitted: bool,
    pub existing_for_resolver: Option<&'a LockfileGraph>,
    pub source_kind_before: Option<LockfileKind>,
    pub lockfile_enabled: bool,
    pub lockfile_include_tarball_url: bool,
    pub shared_workspace_lockfile: bool,
    pub has_workspace: bool,
    pub is_workspace_project: bool,
    pub ignore_pnpmfile: bool,
    pub network_mode: aube_registry::NetworkMode,
    pub global_pnpmfile: Option<&'a Path>,
    pub pnpmfile: Option<&'a Path>,
    pub minimum_release_age_override: Option<u64>,
    pub ws_package_versions: &'a HashMap<String, String>,
    pub ignore_scripts: bool,
    pub prog_ref: Option<&'a InstallProgress>,
}

pub(super) async fn run_lockfile_only(input: LockfileOnlyInput<'_>) -> miette::Result<()> {
    let LockfileOnlyInput {
        cwd,
        mode,
        lockfile_dir,
        lockfile_importer_key,
        manifest,
        manifests,
        per_project_write_selection,
        ws_config,
        workspace_catalogs,
        settings_ctx,
        lockfile_pre_parse,
        lockfile_conflict_marker_warning_emitted,
        existing_for_resolver,
        source_kind_before,
        lockfile_enabled,
        lockfile_include_tarball_url,
        shared_workspace_lockfile,
        has_workspace,
        is_workspace_project,
        ignore_pnpmfile,
        network_mode,
        global_pnpmfile,
        pnpmfile,
        minimum_release_age_override,
        ws_package_versions,
        ignore_scripts,
        prog_ref,
    } = input;

    // `--no-frozen-lockfile` means "always re-resolve", so skip the
    // freshness check entirely in that mode. Otherwise (Prefer, Fix,
    // or CI's auto-Frozen) a fresh lockfile is a no-op — for Fix
    // specifically, "fresh" means "nothing to fix."
    let force_resolve = matches!(mode, FrozenMode::No);
    // Reuse the up-front pre-parse when we already have it so we
    // don't read and parse the same lockfile twice on
    // `--lockfile-only`. The borrowed form is all the freshness
    // check needs — `existing_for_resolver` still points at the
    // same graph for the resolver call below.
    let parsed_owned;
    let parsed: Result<(&LockfileGraph, LockfileKind), &aube_lockfile::Error> =
        if let Some((g, k)) = lockfile_pre_parse {
            Ok((g, *k))
        } else {
            parsed_owned = parse_lockfile_dir_remapped_with_kind(
                lockfile_dir,
                lockfile_importer_key,
                manifest,
            );
            match &parsed_owned {
                Ok((g, k)) => Ok((g, *k)),
                Err(e) => Err(e),
            }
        };
    if let Err(e) = parsed
        && !matches!(e, aube_lockfile::Error::NotFound(_))
    {
        if active_lockfile_has_conflict_markers(lockfile_dir) {
            if !lockfile_conflict_marker_warning_emitted {
                warn_lockfile_conflict_markers(lockfile_dir, e);
            }
        } else {
            // Can't hand &Error to miette::Report since Diagnostic
            // is only implemented on owned Error. Re-parse once to
            // get an owned Error value for the Diagnostic path.
            // Slightly wasteful on the error branch, install is
            // about to abort anyway so speed does not matter here.
            // What matters: keeping the source span and miette help
            // text instead of flattening to one line via `{e}`.
            match parse_lockfile_dir_remapped_with_kind(
                lockfile_dir,
                lockfile_importer_key,
                manifest,
            ) {
                Ok(_) => {
                    // Race: second parse succeeded while first failed.
                    // Surface the observed error text as a best
                    // effort flat message. Extremely unlikely.
                    return Err(miette!("failed to parse lockfile: {e}"));
                }
                Err(owned) => {
                    return Err(miette::Report::new(owned)).wrap_err("failed to parse lockfile");
                }
            }
        }
    }
    // Patch-config drift (declared patchedDependencies the lockfile
    // doesn't record, or an edited patch file) re-resolves like any
    // other drift — same rule as pnpm's lockfile-config mismatch.
    let (effective_patch_paths, effective_patch_hashes) =
        crate::patches::effective_patch_config(cwd)?;
    let fresh = !force_resolve
        && matches!(
            parsed,
            Ok((g, k))
                if matches!(
                    g.check_drift_workspace_for_kind(
                        manifests,
                        &ws_config.overrides,
                        &ws_config.ignored_optional_dependencies,
                        workspace_catalogs,
                        is_workspace_project,
                        k,
                    ),
                    DriftStatus::Fresh,
                )
                    && matches!(g.check_catalogs_drift(workspace_catalogs), DriftStatus::Fresh)
                    && matches!(
                        g.check_patched_dependencies_drift(
                            k,
                            &effective_patch_paths,
                            &effective_patch_hashes
                        ),
                        DriftStatus::Fresh
                    )
        );
    if fresh {
        tracing::debug!("--lockfile-only: lockfile already up to date");
        if let Some(p) = prog_ref {
            p.finish(true);
        }
        eprintln!("Lockfile is up to date, resolution step is skipped");
        return Ok(());
    }
    if let Some(p) = prog_ref {
        p.set_phase("resolving");
    }
    let client =
        std::sync::Arc::new(crate::commands::make_client(cwd).with_network_mode(network_mode));
    let pnpmfile_paths = if ignore_pnpmfile {
        Vec::new()
    } else {
        crate::pnpmfile::ordered_paths(
            crate::pnpmfile::detect_global(cwd, global_pnpmfile).as_deref(),
            crate::pnpmfile::detect(cwd, pnpmfile, ws_config.pnpmfile_path.as_deref()).as_deref(),
        )
    };
    crate::commands::run_pnpmfile_pre_resolution(&pnpmfile_paths, cwd, existing_for_resolver)
        .await?;
    let (read_package_host, read_package_forwarders) =
        match crate::pnpmfile::ReadPackageHostChain::spawn(&pnpmfile_paths, cwd)
            .await
            .wrap_err("failed to start pnpmfile readPackage host")?
        {
            Some((h, f)) => (Some(h), f),
            None => (None, Vec::new()),
        };
    let read_package_hook: Option<Box<dyn aube_resolver::ReadPackageHook>> =
        read_package_host.map(|h| Box::new(h) as Box<dyn aube_resolver::ReadPackageHook>);
    let mut resolver = configure_resolver(
        aube_resolver::Resolver::new(client.clone()),
        cwd,
        manifest,
        ResolverConfigInputs {
            settings_ctx,
            workspace_config: ws_config,
            workspace_catalogs,
            minimum_release_age_override,
            // `lockfile=false` collapses to `None` so the resolver
            // doesn't waste a fetch widening a lockfile that will
            // never be written. With lockfiles enabled, a missing
            // `source_kind_before` means "we'll create the default
            // aube-lock.yaml", so the aube-native wide default
            // applies.
            target_lockfile_kind: lockfile_enabled.then(|| {
                source_kind_before
                    .unwrap_or_else(|| crate::commands::default_lockfile_kind(settings_ctx))
            }),
            cache_full_packuments: true,
            ignore_scripts,
        },
        read_package_hook,
    );
    let mut graph = if has_workspace {
        resolver
            .resolve_workspace(manifests, existing_for_resolver, ws_package_versions)
            .await
    } else {
        resolver.resolve(manifest, existing_for_resolver).await
    }
    .map_err(miette::Report::new)
    .wrap_err("failed to resolve dependencies")?;
    drop(resolver);
    // Drain the readPackage stderr forwarders so every `ctx.log`
    // record they captured during resolve flushes to stdout before
    // afterAllResolved emits its own pnpm:hook records — keeps
    // resolve-time logs strictly ahead of post-resolve logs in the
    // ndjson stream.
    crate::pnpmfile::ReadPackageHostChain::drain_forwarders(read_package_forwarders).await;
    crate::pnpmfile::run_after_all_resolved_chain(&pnpmfile_paths, cwd, &mut graph).await?;
    // Same patch-config recording as the main install branch — keeps
    // `--lockfile-only` output byte-identical to a full install's.
    crate::patches::record_patches_on_graph(cwd, &mut graph)?;
    // Same tarball-URL population pass as the main fetch branch —
    // keeps `--lockfile-only` and regular installs byte-identical.
    // Reuses the resolver's `client` (already built above) to avoid
    // re-walking `.npmrc` and rebuilding the rustls client just to
    // construct registry URLs.
    if lockfile_include_tarball_url {
        let lo_client = client.as_ref();
        graph.settings.lockfile_include_tarball_url = true;
        for pkg in graph.packages.values_mut() {
            if pkg.local_source.is_some() {
                continue;
            }
            // Preserve any URL the parser already captured from an
            // aliased `resolved:` field — deriving from
            // `(registry_name, version)` would also work for
            // aliases but skips a redundant allocation.
            if pkg.tarball_url.is_none() {
                pkg.tarball_url = Some(lo_client.tarball_url(pkg.registry_name(), &pkg.version));
            }
        }
    }
    let lo_write_kind =
        source_kind_before.unwrap_or_else(|| crate::commands::default_lockfile_kind(settings_ctx));
    // Same runtime-pin recording as the main install path.
    crate::runtime::refresh_lockfile_pin(
        &mut graph,
        manifest,
        crate::runtime::RuntimeSettings::from_ctx(settings_ctx),
        lo_write_kind,
    )
    .await?;
    // Mirror the streaming path: stamp pnpm's config checksums (pnpm-lock
    // only) before the graph hits disk so `--lockfile-only` and a regular
    // install stay byte-identical.
    let lo_local_pnpmfile = if ignore_pnpmfile {
        None
    } else {
        crate::pnpmfile::detect(cwd, pnpmfile, ws_config.pnpmfile_path.as_deref())
    };
    stamp_pnpm_config_checksums(
        &mut graph,
        lo_write_kind,
        manifest,
        settings_ctx,
        lo_local_pnpmfile.as_deref(),
    )
    .await;
    // Annotate the resolved graph with pnpm-parity snapshot metadata
    // (`optional: true`, `transitivePeerDependencies`) so `--lockfile-only`
    // output stays byte-identical to a regular install.
    crate::commands::prepare_resolved_graph_for_lockfile_write(&mut graph);
    // Same `time:`-persistence gate as the regular install write path:
    // pnpm writes a top-level `time:` block only under time-based mode,
    // so a Highest-mode `--lockfile-only` run stays `time:`-free even
    // with `minimumReleaseAge` keeping publish times in memory.
    let persist_times = super::settings::resolve_resolution_mode(settings_ctx)
        == aube_resolver::ResolutionMode::TimeBased;
    let write_graph = super::lockfile_dir::lockfile_graph_for_write(&graph, persist_times);
    if shared_workspace_lockfile || !has_workspace {
        let lo_written = write_lockfile_dir_remapped(
            lockfile_dir,
            lockfile_importer_key,
            &write_graph,
            manifest,
            lo_write_kind,
        )
        .into_diagnostic()
        .wrap_err("failed to write lockfile")?;
        tracing::debug!(
            "--lockfile-only: wrote {}",
            lo_written
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| lo_written.display().to_string())
        );
    } else {
        write_per_project_lockfiles(
            cwd,
            &write_graph,
            manifests,
            lo_write_kind,
            per_project_write_selection,
        )?;
    }
    // Prune unused catalog entries *after* the lockfile hits disk —
    // same ordering as the main install path below, so a
    // workspace-yaml write error can't block the command's
    // primary output.
    maybe_cleanup_unused_catalogs(cwd, settings_ctx, workspace_catalogs, &graph.catalogs)?;
    if let Some(p) = prog_ref {
        p.finish(true);
    }
    eprintln!(
        "Lockfile written ({} packages); skipped node_modules linking",
        graph.packages.len()
    );
    Ok(())
}

pub(super) struct SelectLockfileInput<'a> {
    pub lockfile_enabled: bool,
    pub mode: FrozenMode,
    pub cwd: &'a Path,
    pub lockfile_dir: &'a Path,
    pub lockfile_importer_key: &'a str,
    pub manifest: &'a aube_manifest::PackageJson,
    pub manifests: &'a [(String, aube_manifest::PackageJson)],
    pub ws_config: &'a aube_manifest::workspace::WorkspaceConfig,
    pub workspace_catalogs: &'a crate::commands::CatalogMap,
    pub is_workspace_project: bool,
    pub lockfile_pre_parse: Option<&'a (LockfileGraph, LockfileKind)>,
}

pub(super) fn select_lockfile_result(
    input: SelectLockfileInput<'_>,
) -> miette::Result<Result<(LockfileGraph, LockfileKind), aube_lockfile::Error>> {
    let SelectLockfileInput {
        lockfile_enabled,
        mode,
        cwd,
        lockfile_dir,
        lockfile_importer_key,
        manifest,
        manifests,
        ws_config,
        workspace_catalogs,
        is_workspace_project,
        lockfile_pre_parse,
    } = input;
    if !lockfile_enabled {
        tracing::debug!("lockfile=false: skipping lockfile parse, re-resolving");
        return Ok(Err(aube_lockfile::Error::NotFound(cwd.to_path_buf())));
    }
    match mode {
        // Always re-resolve.
        FrozenMode::No => Ok(Err(aube_lockfile::Error::NotFound(cwd.to_path_buf()))),
        // Always fall through to re-resolve; `existing_for_resolver`
        // carries the current lockfile (if any) so the resolver
        // reuses locked versions for unchanged specs and only
        // re-resolves entries whose spec drifted.
        FrozenMode::Fix => Ok(Err(aube_lockfile::Error::NotFound(cwd.to_path_buf()))),
        FrozenMode::Frozen => {
            // Use the lockfile, but error out on any drift across all workspace importers.
            let parsed = parse_lockfile_dir_remapped_with_kind(
                lockfile_dir,
                lockfile_importer_key,
                manifest,
            );
            if let Ok((ref graph, kind)) = parsed {
                if let DriftStatus::Stale { reason } =
                    graph.check_catalogs_drift(workspace_catalogs)
                {
                    return Err(miette!(
                        code = aube_codes::errors::ERR_AUBE_OUTDATED_LOCKFILE,
                        "lockfile is out of date with pnpm-workspace.yaml: {reason}\n\
                         help: run without --frozen-lockfile to update the lockfile"
                    ));
                }
                if let DriftStatus::Stale { reason } = graph.check_drift_workspace_for_kind(
                    manifests,
                    &ws_config.overrides,
                    &ws_config.ignored_optional_dependencies,
                    workspace_catalogs,
                    is_workspace_project,
                    kind,
                ) {
                    return Err(miette!(
                        code = aube_codes::errors::ERR_AUBE_OUTDATED_LOCKFILE,
                        "lockfile is out of date with package.json: {reason}\n\
                         help: run without --frozen-lockfile to update the lockfile, \
                         or run `{} --no-frozen-lockfile` to regenerate it",
                        aube_util::cmd("install")
                    ));
                }
                // Same hard-fail for patch-config drift — pnpm rejects
                // this as ERR_PNPM_LOCKFILE_CONFIG_MISMATCH.
                let (effective_patch_paths, effective_patch_hashes) =
                    crate::patches::effective_patch_config(cwd)?;
                if let DriftStatus::Stale { reason } = graph.check_patched_dependencies_drift(
                    kind,
                    &effective_patch_paths,
                    &effective_patch_hashes,
                ) {
                    return Err(miette!(
                        code = aube_codes::errors::ERR_AUBE_OUTDATED_LOCKFILE,
                        "lockfile is out of date with the project's patch configuration: {reason}\n\
                         help: run without --frozen-lockfile to update the lockfile"
                    ));
                }
            }
            Ok(parsed)
        }
        FrozenMode::Prefer => {
            // Use the lockfile when fresh, otherwise pretend there isn't one
            // so the existing "no lockfile → resolve" branch handles it.
            // Reuse `lockfile_pre_parse` instead of parsing the same file
            // a second time — on Prefer-fresh we clone the graph so the
            // borrow held by `existing_for_resolver` keeps pointing at
            // the original (unused on the fresh path, but safe to leave).
            match lockfile_pre_parse {
                Some((graph, kind)) => {
                    if let DriftStatus::Stale { reason } =
                        graph.check_catalogs_drift(workspace_catalogs)
                    {
                        tracing::debug!(
                            "Lockfile out of date with workspace catalogs ({reason}), re-resolving..."
                        );
                        Ok(Err(aube_lockfile::Error::NotFound(cwd.to_path_buf())))
                    } else {
                        let (effective_patch_paths, effective_patch_hashes) =
                            crate::patches::effective_patch_config(cwd)?;
                        let drift = match graph.check_drift_workspace_for_kind(
                            manifests,
                            &ws_config.overrides,
                            &ws_config.ignored_optional_dependencies,
                            workspace_catalogs,
                            is_workspace_project,
                            *kind,
                        ) {
                            DriftStatus::Fresh => graph.check_patched_dependencies_drift(
                                *kind,
                                &effective_patch_paths,
                                &effective_patch_hashes,
                            ),
                            stale => stale,
                        };
                        match drift {
                            DriftStatus::Fresh => Ok(Ok((graph.clone(), *kind))),
                            DriftStatus::Stale { reason } => {
                                tracing::debug!("Lockfile out of date ({reason}), re-resolving...");
                                Ok(Err(aube_lockfile::Error::NotFound(cwd.to_path_buf())))
                            }
                        }
                    }
                }
                None => Ok(Err(aube_lockfile::Error::NotFound(cwd.to_path_buf()))),
            }
        }
    }
}

fn active_lockfile_has_conflict_markers(lockfile_dir: &Path) -> bool {
    aube_lockfile::active_lockfile_has_conflict_markers(lockfile_dir)
}

fn warn_lockfile_conflict_markers(lockfile_dir: &Path, err: &aube_lockfile::Error) {
    tracing::warn!(
        code = aube_codes::warnings::WARN_AUBE_LOCKFILE_CONFLICT_MARKERS,
        "lockfile in {} contains Git conflict markers; regenerating from package.json ({err})",
        lockfile_dir.display()
    );
}

pub(super) fn apply_lockfile_graph_platform_rules(
    mut graph: LockfileGraph,
    kind: LockfileKind,
    manifest: &aube_manifest::PackageJson,
    ws_config: &aube_manifest::workspace::WorkspaceConfig,
    settings_ctx: &aube_settings::ResolveCtx<'_>,
) -> miette::Result<LockfileGraph> {
    // Drop optional deps that don't match the current platform
    // (or are in `pnpm.ignoredOptionalDependencies`) before we
    // start fetching tarballs. The resolver's inline filter
    // never runs on the lockfile-happy path, so this pass is
    // what makes cross-platform lockfile installs work.
    let (sup_os, sup_cpu, sup_libc) =
        super::settings::effective_supported_architectures(manifest, ws_config, settings_ctx);
    let supported_architectures = aube_resolver::SupportedArchitectures {
        os: sup_os,
        cpu: sup_cpu,
        libc: sup_libc,
        ..Default::default()
    };
    let ignored_optional_deps =
        aube_manifest::effective_ignored_optional_dependencies(manifest, ws_config);
    // npm/bun lockfiles serialize a flat, pre-hoisted tree
    // with no peer context — they rely on Node's upward
    // `node_modules/` walk to find peer deps, which the
    // isolated virtual store breaks. Fresh resolves flow
    // through `Resolver::resolve_workspace`, which runs
    // `hoist_auto_installed_peers` + `apply_peer_contexts` on
    // its way out; the lockfile path has to replicate those
    // two steps explicitly or peer-dependent packages
    // (e.g. `@tanstack/devtools-vite` peering on `vite`)
    // install with no sibling peer link and die at runtime
    // with `Cannot find package`.
    //
    // `aube-lock.yaml` / `pnpm-lock.yaml` already carry
    // peer-context suffixes and peer edges merged into
    // `dependencies`, so we skip them — re-running the pass
    // would double-suffix every key.
    //
    // Yarn v1 has the same flat shape but is intentionally
    // omitted: real-world `yarn.lock` files don't record
    // `peerDependencies` per entry (yarn 1.22 emits them
    // only on the workspace root), so running the pass would
    // be a no-op. Making yarn v1 imports peer-correct needs
    // a packument fetch on the import path to graft peer
    // ranges back onto each `LockedPackage` — a deeper
    // change than this match arm.
    //
    // The hoist must run *before* `filter_graph`: bun records
    // peer-only-installed packages (e.g. `@mui/material` when
    // the importer only depends on `@textea/json-viewer`, which
    // peers on MUI) in its packages map, but our bun parser
    // doesn't merge those into the consumer's `dependencies`
    // map. `filter_graph`'s GC walk only follows `dependencies`,
    // so without the hoist running first it prunes every
    // peer-only package as unreachable — and a post-prune hoist
    // has nothing left to promote.
    let needs_peer_pass = matches!(
        kind,
        LockfileKind::Npm | LockfileKind::NpmShrinkwrap | LockfileKind::Bun
    );
    // Time the hoist on its own, then `filter_graph` runs untimed
    // (it's not part of the peer pass), then apply is timed below.
    // Snapshotting `pkgs_before` after `filter_graph` keeps the
    // logged delta a pure measure of `apply_peer_contexts`'s
    // additions, not filter_graph's prunes.
    let mut hoist_elapsed: Option<std::time::Duration> = None;
    let mut auto_installed_peers = aube_resolver::AutoInstalledPeers::new();
    if needs_peer_pass {
        let hoist_start = std::time::Instant::now();
        (graph, auto_installed_peers) = aube_resolver::hoist_auto_installed_peers(graph);
        hoist_elapsed = Some(hoist_start.elapsed());
    }
    aube_resolver::platform::filter_graph(
        &mut graph,
        &supported_architectures,
        &ignored_optional_deps,
    );
    if let Some(hoist_elapsed) = hoist_elapsed {
        let peer_options = aube_resolver::PeerContextOptions {
            dedupe_peer_dependents: super::settings::resolve_dedupe_peer_dependents(settings_ctx),
            dedupe_peers: super::settings::resolve_dedupe_peers(settings_ctx),
            resolve_from_workspace_root: super::settings::resolve_peers_from_workspace_root(
                settings_ctx,
            ),
            peers_suffix_max_length: super::settings::resolve_peers_suffix_max_length(settings_ctx),
        };
        let pkgs_before = graph.packages.len();
        let apply_start = std::time::Instant::now();
        graph = aube_resolver::apply_peer_contexts(graph, &peer_options)
            .map_err(|e| miette!("peer-context pass failed: {e}"))?;
        // The hoisted importer entries were scaffolding for the two
        // passes above (peer-context scopes + filter_graph
        // reachability); pnpm never surfaces auto-installed peers as
        // importer deps, so drop them before linking/serialization.
        aube_resolver::remove_auto_installed_peers(&mut graph, &auto_installed_peers);
        tracing::debug!(
            "peer-context pass (lockfile={:?}) {} → {} packages in {:.1?}",
            kind,
            pkgs_before,
            graph.packages.len(),
            hoist_elapsed + apply_start.elapsed()
        );
    }
    Ok(graph)
}

pub(super) fn lockfile_source_label(kind: LockfileKind) -> &'static str {
    match kind {
        LockfileKind::Aube => "Lockfile",
        LockfileKind::Pnpm => "pnpm-lock.yaml",
        LockfileKind::Yarn | LockfileKind::YarnBerry => "yarn.lock",
        LockfileKind::Npm => "package-lock.json",
        LockfileKind::NpmShrinkwrap => "npm-shrinkwrap.json",
        LockfileKind::Bun => "bun.lock",
    }
}

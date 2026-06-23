use super::make_client;
use crate::progress::InstallProgress;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::BTreeMap;
use std::io::Write;

mod advisory;
mod args;
mod bin_linking;
mod critical_path;
mod default_trust;
mod delta;
mod dep_selection;
mod fetch;
mod finalize;
mod frozen;
mod git_prepare;
mod gvs;
mod layout;
mod lifecycle;
mod link;
mod lockfile_dir;
mod materialize;
// `pub` (not `pub(crate)`) so an embedder whose `current_exe()` is the binary
// the lazy node-gyp shim re-execs can reach `ensure_cached` /
// `print_bootstrapped_binary` to service the bootstrap re-entry. Standalone
// aube reaches them the same way; widening visibility changes no behavior.
pub mod node_gyp_bootstrap;
mod resolve;
pub(crate) mod settings;
mod side_effects_cache;
mod startup;
mod summary;
mod sweep;
mod unreviewed_builds;
mod workspace;

use advisory::resolve_osv_routing_settings;
pub use args::{InstallArgs, InstallOptions};
pub(crate) use bin_linking::{PkgJsonCache, link_dep_bins, materialized_pkg_dir};
pub(crate) use default_trust::DefaultTrustFloor;
pub use dep_selection::DepSelection;
pub(super) use fetch::fetch_packages;
use fetch::{
    fetch_packages_with_root, import_local_source, remap_indices_to_contextualized,
    version_from_dep_path,
};
pub use frozen::{FrozenMode, FrozenOverride, GlobalVirtualStoreFlags};
pub(crate) use lifecycle::{
    JailBuildPolicy, build_policy_from_manifest_sources, build_policy_from_sources,
    run_dep_lifecycle_scripts,
};
use lifecycle::{
    resolve_link_strategy, run_import_on_blocking, run_root_lifecycle, validate_required_scripts,
};
use lockfile_dir::{
    parse_lockfile_dir_remapped, parse_lockfile_dir_remapped_with_kind, write_lockfile_dir_remapped,
};
use materialize::{
    GvsPrewarmInputs, combine_install_pipeline_errors, materialize_channel, spawn_gvs_prewarm,
};
pub(crate) use settings::PeerDependencyRules;
pub(crate) use settings::{ResolverConfigInputs, configure_resolver, finalize_lockfile_graph};
pub(crate) use side_effects_cache::{SideEffectsCacheConfig, side_effects_cache_root};

use settings::{
    check_unmet_peers, default_streaming_network_concurrency, maybe_cleanup_unused_catalogs,
    resolve_git_shallow_hosts, resolve_link_concurrency, resolve_network_concurrency,
    resolve_side_effects_cache, resolve_side_effects_cache_readonly,
    resolve_strict_peer_dependencies, resolve_strict_store_pkg_content_check,
    resolve_verify_store_integrity,
};
use startup::{
    apply_force_state_reset, merge_branch_lockfiles_if_needed, modules_cache_sweep_is_default,
    resolve_project_cwd, try_install_fast_path, warn_accepted_noop_install_settings,
};
use summary::print_already_up_to_date;
use workspace::{
    discover_workspace_plan, filter_graph_to_importers, filter_graph_to_workspace_selection,
    importer_project_dir, merge_member_lockfile_graphs, per_project_write_selection,
    write_per_project_lockfiles,
};

/// Process-global toggle for the warm-relink store verification depth.
///
/// Default `true` == upstream behavior: the two warm-relink classifier
/// sites (`fetch::fetch_packages_with_root` and the GVS prewarm loop in
/// this module) call [`aube_store::Store::load_index_verified`], which
/// stats *every* file recorded in a package's cached index on a cache
/// hit (~150 ms on a 1.4k-package warm install). That full stat guards
/// only against external drift of the local CAS — a Docker BuildKit
/// cache mount that covers `index/` but not `files/`, a foreign sync
/// tool, a manual `rm` inside the store — and even then only partially:
/// a stale entry simply re-fetches the tarball cleanly, never silent
/// corruption (the index is the store's completeness marker; aube
/// publishes to the CAS atomically — O_TMPFILE+linkat / O_CREAT|O_EXCL
/// per file, index written LAST, a torn index is parse-rejected).
///
/// When set `false`, the warm-relink sites use the cheap
/// [`aube_store::Store::load_index`] path instead (parse the index +
/// stat only the first file per package — enough to catch the common
/// crash-residue class of a wiped CAS shard). An embedder that trusts
/// the atomically-published store (nub, Bun's model) sets
/// `warm_store_verify = false` on its embedder profile to skip the
/// per-file stat sweep.
///
/// **This is independent of import-time integrity.** It does NOT touch
/// download/tarball SHA-512 verification, the `verifyStoreIntegrity`
/// setting, or `strict-store-integrity` — those stay on regardless.
/// Only the local-cache warm-relink stat depth relaxes.
///
/// Sourced from the compile-time embedder profile (a fixed posture, not
/// per-project). Defaults to `true` (upstream) when an embedder never
/// relaxed it.
pub(crate) fn warm_store_verify() -> bool {
    aube_util::embedder().warm_store_verify
}

/// Load a cached package index for a warm-relink classifier site,
/// choosing stat depth per the process-global [`warm_store_verify`]
/// flag: full per-file verify by default (upstream), first-file-only
/// when an embedder opted into fast-trust. Independent of import-time
/// SRI / `verifyStoreIntegrity`, which is enforced elsewhere on fetch.
pub(crate) fn warm_load_index(
    store: &aube_store::Store,
    name: &str,
    version: &str,
    integrity: Option<&str>,
) -> Option<aube_store::PackageIndex> {
    let verify = warm_store_verify();
    // Log the selected depth exactly once per process so a `-v` warm
    // install can confirm which path is active without per-package noise.
    static LOGGED: std::sync::Once = std::sync::Once::new();
    LOGGED.call_once(|| {
        tracing::debug!(
            verify,
            "warm-relink store verification: {}",
            if verify {
                "full (stat every cached file)"
            } else {
                "fast (first-file stat only)"
            }
        );
    });
    if verify {
        store.load_index_verified(name, version, integrity)
    } else {
        store.load_index(name, version, integrity)
    }
}

#[derive(Default)]
struct InstallPhaseTimings {
    path: Option<std::path::PathBuf>,
    phases_ms: BTreeMap<&'static str, u128>,
    /// Last kernel snapshot, captured immediately after the previous
    /// phase recorded. The next [`record`] call diffs against this and
    /// emits a `kernel.<phase>` event with the per-phase user/sys CPU,
    /// peak RSS, and page fault deltas.
    last_kernel_snap: Option<aube_util::diag_kernel::KernelSnapshot>,
}

impl InstallPhaseTimings {
    fn from_env() -> Self {
        Self {
            path: aube_util::env::embedder_env("BENCH_PHASES_FILE").map(std::path::PathBuf::from),
            phases_ms: BTreeMap::new(),
            last_kernel_snap: aube_util::diag_kernel::snapshot(),
        }
    }

    fn record(&mut self, phase: &'static str, elapsed: std::time::Duration) {
        if self.path.is_some() {
            self.phases_ms.insert(phase, elapsed.as_millis());
        }
        aube_util::diag::event(
            aube_util::diag::Category::InstallPhase,
            phase,
            elapsed,
            None,
        );
        // When kernel sampling is on, emit a per-phase kernel delta so
        // user/sys CPU split, page fault counts, and peak RSS land in
        // the trace alongside the wall-time phase event.
        if aube_util::diag_kernel::enabled()
            && let Some(after) = aube_util::diag_kernel::snapshot()
        {
            if let Some(before) = self.last_kernel_snap.take() {
                aube_util::diag_kernel::emit_phase_delta(phase, before, after);
            }
            self.last_kernel_snap = Some(after);
        }
    }

    fn write(
        &self,
        cwd: &std::path::Path,
        total: std::time::Duration,
        packages: usize,
        cached: usize,
        fetched: usize,
    ) {
        let Some(path) = &self.path else {
            return;
        };
        let payload = serde_json::json!({
            "cwd": cwd,
            "scenario": aube_util::env::embedder_env("BENCH_SCENARIO")
                .and_then(|s| s.into_string().ok()),
            "total_ms": total.as_millis(),
            "packages": packages,
            "cached": cached,
            "fetched": fetched,
            "phases_ms": self.phases_ms,
        });
        let Ok(line) = serde_json::to_string(&payload) else {
            return;
        };
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(mut file) => {
                let _ = writeln!(file, "{line}");
            }
            Err(e) => tracing::debug!("failed to write install phase timings: {e}"),
        }
    }
}

pub async fn run(opts: InstallOptions) -> miette::Result<()> {
    let mode = opts.mode;
    let cwd = resolve_project_cwd(&opts)?;
    let _lock = super::take_project_lock(&cwd)?;
    let start = std::time::Instant::now();
    let mut phase_timings = InstallPhaseTimings::from_env();
    aube_util::diag::spawn_concurrency_sampler();
    aube_util::diag::instant(aube_util::diag::Category::Install, "begin", None);
    let _diag_install = aube_util::diag::Span::new(aube_util::diag::Category::Install, "total");

    apply_force_state_reset(&cwd, &opts)?;
    if try_install_fast_path(&cwd, &opts, mode, modules_cache_sweep_is_default(&cwd)) {
        return Ok(());
    }

    // Yaml-only workspace roots (`pnpm-workspace.yaml` only, no root
    // `package.json`) install with a synthesized empty manifest so
    // every workspace member is installed without the root carrying
    // any deps or scripts itself. The synthesized manifest naturally
    // skips root lifecycle hooks, has no required-scripts to validate,
    // and threads through the rest of the pipeline as a manifest with
    // no direct deps would.
    let manifest = super::load_manifest_or_default(&cwd)?;
    let project_name = manifest.name.as_deref().unwrap_or("(unnamed)");

    // Load the workspace yaml *once* — both as the typed
    // `WorkspaceConfig` (used below for `allow_builds_raw` and
    // friends) and as a raw `BTreeMap` (used by
    // `aube_settings::resolved::*` for metadata-driven lookups).
    // Errors propagate here rather than silently defaulting later,
    // so a malformed workspace file surfaces before we start
    // resolving the dep graph. Also load `.npmrc` entries once so
    // the same borrow feeds both the resolve-time settings and the
    // later engine-check settings.
    let files = crate::commands::FileSources::load(&cwd);
    let (ws_config_shared, raw_workspace) = aube_manifest::workspace::load_both(&cwd)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    // Catalog discovery walks up for the workspace yaml and also pulls
    // from package.json's `workspaces.catalog` / `pnpm.catalog`, so
    // `aube install` run from a monorepo subpackage still sees the root
    // workspace's catalog. See `discover_catalogs` for the precedence
    // order.
    let workspace_catalogs = super::discover_catalogs(&cwd)?;
    let settings_ctx = files.ctx(&raw_workspace, &opts.env_snapshot, &opts.cli_flags);
    // Resolve the project's Node runtime before anything can spawn
    // node: the root `preinstall` hooks below must already run on the
    // switched runtime, and the virtual-store keys downstream fold
    // the node major in. The lockfile pin (when recorded) wins over
    // the manifest range, and `--offline` blocks runtime downloads
    // the same way it blocks registry fetches.
    let mut runtime_settings = crate::runtime::RuntimeSettings::from_ctx(&settings_ctx);
    if opts.network_mode == aube_registry::NetworkMode::Offline {
        runtime_settings.network = aube_runtime::NetworkMode::Offline;
    }
    crate::runtime::ensure(
        &cwd,
        Some(&manifest),
        runtime_settings,
        crate::runtime::lockfile_node_pin(&cwd, &manifest).as_ref(),
    )
    .await?;
    super::configure_script_settings(&cwd, &settings_ctx, Some("install"));

    let layout::InstallLayoutConfig {
        lockfile_dir,
        lockfile_importer_key,
        modules_dir_name,
        aube_dir,
        lockfile_enabled,
        shared_workspace_lockfile,
        lockfile_only_effective,
        lockfile_include_tarball_url,
    } = layout::resolve_install_layout(
        &cwd,
        &manifest,
        &settings_ctx,
        opts.lockfile_only,
        opts.strict_no_lockfile,
    )?;

    merge_branch_lockfiles_if_needed(
        &cwd,
        &manifest,
        &settings_ctx,
        lockfile_enabled,
        opts.merge_git_branch_lockfiles,
    )?;

    // Resolve the install-wide networking / integrity knobs once up
    // front so every downstream fetch site (the lockfile path, the
    // streaming-resolver path, and the forthcoming `aube fetch`
    // bridge) reads the same values. `network_concurrency_setting`
    // stays `Option<usize>` so each site can apply the dynamic
    // built-in fallback when the setting is absent.
    //
    // `sideEffectsCache` controls whether allowlisted dependency
    // lifecycle scripts can reuse a previously-cached post-build
    // package directory. It still respects aube's security model:
    // packages that are not allowed by BuildPolicy never run scripts
    // and never populate the side-effects cache.
    let network_concurrency_setting = resolve_network_concurrency(&settings_ctx);
    let link_concurrency_setting = resolve_link_concurrency(&settings_ctx);
    let verify_store_integrity_setting = resolve_verify_store_integrity(&settings_ctx);
    let strict_store_integrity_setting = settings::resolve_strict_store_integrity(&settings_ctx);
    let strict_store_pkg_content_check_setting =
        resolve_strict_store_pkg_content_check(&settings_ctx);
    let side_effects_cache_setting = resolve_side_effects_cache(&settings_ctx);
    let side_effects_cache_readonly_setting = resolve_side_effects_cache_readonly(&settings_ctx);
    // `paranoid=true` forces unreviewed dep build scripts to error
    // instead of being silently skipped.
    let strict_dep_builds_setting = aube_settings::resolved::strict_dep_builds(&settings_ctx)
        || aube_settings::resolved::paranoid(&settings_ctx);
    let required_scripts =
        aube_settings::resolved::required_scripts(&settings_ctx).unwrap_or_default();
    validate_required_scripts(&cwd, &manifest, &required_scripts)?;
    warn_accepted_noop_install_settings(&settings_ctx);
    // `dlxCacheMaxAge` has no consumer yet (aube `dlx` uses a
    // tempdir per invocation) but resolving it here keeps the value
    // exercised through the same `ResolveCtx` the rest of the install
    // uses, so a future persistent-dlx-cache change can pick it up
    // without revisiting the resolver wiring.
    let _ = aube_settings::resolved::dlx_cache_max_age(&settings_ctx);
    tracing::debug!(
        "settings: network-concurrency={:?}, link-concurrency={:?}, verify-store-integrity={}, strict-store-pkg-content-check={}, side-effects-cache={}, side-effects-cache-readonly={}, strict-dep-builds={}",
        network_concurrency_setting,
        link_concurrency_setting,
        verify_store_integrity_setting,
        strict_store_pkg_content_check_setting,
        side_effects_cache_setting,
        side_effects_cache_readonly_setting,
        strict_dep_builds_setting,
    );

    // Resolve once for the whole install: both the fetch phase's
    // `AlreadyLinked` fast path and the linker's `aube_dir_entry_name`
    // need to encode `dep_path` into the same `.aube/<name>` filename.
    // Pinning the value here and threading it through both call sites
    // keeps them in lockstep, and the same resolved cap is re-read by
    // `aube list` / `aube why` / `aube patch` / `aube rebuild` so the
    // read-side encoding agrees with what the linker actually wrote.
    let virtual_store_dir_max_length = super::resolve_virtual_store_dir_max_length(&settings_ctx);

    let workspace_plan =
        discover_workspace_plan(&cwd, &manifest, &settings_ctx, &opts.workspace_filter)?;
    let workspace_packages = workspace_plan.workspace_packages;
    let has_workspace = workspace_plan.has_workspace;
    let is_workspace_project = workspace_plan.is_workspace_project;
    let link_all_workspace_importers = workspace_plan.link_all_workspace_importers;
    let manifests = workspace_plan.manifests;
    let ws_package_versions = workspace_plan.ws_package_versions;
    let ws_dirs = workspace_plan.ws_dirs;
    let lifecycle_manifests = workspace_plan.lifecycle_manifests;
    let default_trust_enabled = aube_settings::resolved::default_trust(&settings_ctx);
    // Importer keys whose per-project lockfiles a filtered install may
    // (re)write. `None` for an unfiltered install (write every importer).
    // Computed once and shared by the `--lockfile-only` short-circuit and
    // the streaming-install write so both paths stay scoped identically.
    let per_project_write_selection =
        per_project_write_selection(&cwd, &workspace_packages, &opts.workspace_filter)?;
    let (build_policy, policy_warnings) =
        if let Some(override_policy) = opts.build_policy_override.as_deref() {
            (override_policy.clone(), Vec::new())
        } else {
            // With the `defaultTrust` floor active the documented
            // precedence puts explicit `allowBuilds` entries above
            // `dangerouslyAllowAllBuilds` in both directions, so the
            // allow-all posture composes via `allow_all_except_denied`
            // (explicit `false` survives). Without the floor, the
            // pnpm-parity short-circuit (allow-all drops the map)
            // stays untouched.
            let compose_allow_all = opts.dangerously_allow_all_builds && default_trust_enabled;
            let (mut build_policy, policy_warnings) = build_policy_from_manifest_sources(
                lifecycle_manifests.iter().map(|(_, manifest)| manifest),
                &ws_config_shared,
                opts.dangerously_allow_all_builds && !compose_allow_all,
            );
            if compose_allow_all {
                build_policy = build_policy.allow_all_except_denied();
            }
            if let Some(inherited) = opts.inherited_build_policy.as_deref() {
                build_policy.merge(inherited);
            }
            (build_policy, policy_warnings)
        };
    let inherited_build_policy_for_git_prepare = Some(std::sync::Arc::new(build_policy.clone()));

    // 1b. Project `preinstall` lifecycle hooks.
    //     Workspace installs run the hook for every physical importer
    //     that will be linked, matching pnpm's recursive install
    //     behavior. Runs before the progress UI starts so script output
    //     cannot collide with the progress display.
    if !opts.ignore_scripts && !lockfile_only_effective && !opts.skip_root_lifecycle {
        let phase_start = std::time::Instant::now();
        for (importer_path, importer_manifest) in &lifecycle_manifests {
            let project_dir = importer_project_dir(&cwd, importer_path);
            run_root_lifecycle(
                &project_dir,
                &modules_dir_name,
                importer_manifest,
                aube_scripts::LifecycleHook::PreInstall,
            )
            .await?;
        }
        phase_timings.record("root_preinstall", phase_start.elapsed());
    }
    // Progress UI. `None` on non-TTY stderr, in text mode (e.g. `-v`), or
    // when progress output is otherwise disabled. A normal install produces
    // *no* output other than the bar itself — everything else is tracing at
    // debug level, visible with `aube -v install`. Must be constructed after
    // any lifecycle script that writes to stderr.
    let prog = InstallProgress::try_new();
    let prog_ref = prog.as_ref();

    let use_global_virtual_store_override =
        gvs::resolve_global_virtual_store_override(&settings_ctx, &manifests, &opts.env_snapshot);

    // Remember which lockfile format the project currently uses so
    // every downstream write site (the `--lockfile-only` short-circuit
    // below *and* the re-resolve branch further down) can preserve it
    // instead of quietly converting the project to another filename.
    // Declaration-aware: when no lockfile exists yet but package.json
    // declares a package manager, the declared tool's format is the
    // write target (pin-over-inference), and a declaration that
    // contradicts the on-disk lockfiles — or several tools' lockfiles
    // with no declaration — fails the install here with a structured
    // error before anything is resolved or written. Must happen before
    // the `--lockfile-only` block so that path doesn't bypass the
    // format-preserving write logic. Skipped when `lockfile=false` —
    // no lockfile is read and no format is preserved, so the install
    // always writes nothing (see below).
    let source_kind_before = if lockfile_enabled {
        crate::commands::resolve_lockfile_kind_for_write(&lockfile_dir)?
    } else {
        None
    };

    // Hand any parseable lockfile to the resolver as `existing` so
    // unchanged specs reuse their already-pinned versions and only
    // entries whose spec actually drifted get re-resolved. Without
    // this, `aube install` after any manifest edit re-resolves every
    // transitive against the latest packument and silently bumps
    // versions that the previous lockfile had pinned (e.g.
    // `electron-to-chromium@1.5.344` → `1.5.343`), which is the
    // opposite of what pnpm/bun's default `install` does.
    //
    // Scope:
    //   - Fix: existing behavior (`--fix-lockfile`).
    //   - Prefer: default mode; the bug above lives here.
    //   - Frozen: short-circuits to the lockfile-as-truth branch and
    //     never calls the resolver, so parsing is wasted work.
    //   - No (`--no-frozen-lockfile`): kept as fresh-resolve so users
    //     who reach for that flag to bump transitives still get a
    //     fresh pass. Matching pnpm's "lockfile may drift but locked
    //     versions are still preferred" semantics is a separate
    //     decision and would change observable behavior on this path.
    //
    // We parse once and keep both the graph and its kind so the
    // `--lockfile-only` block below can reuse the same result for its
    // freshness check instead of re-reading + re-parsing the same file.
    //
    // Hard-fail on a real parse error: the prior in-arm parse in
    // `FrozenMode::Prefer` propagated parse errors out of
    // `lockfile_result`, and silently swallowing them here would leave
    // a corrupt lockfile masquerading as "no lockfile" and trigger a
    // full re-resolve without surfacing the actionable diagnostic.
    // `NotFound` is the one error we treat as expected — it just means
    // the lockfile is absent, which the downstream arms already handle.
    let lockfile_pre_parse = resolve::pre_parse_lockfile(
        lockfile_enabled,
        mode,
        &lockfile_dir,
        &lockfile_importer_key,
        &manifest,
    )?;
    let lockfile_conflict_marker_warning_emitted = lockfile_pre_parse.is_none()
        && lockfile_enabled
        && matches!(mode, FrozenMode::Fix | FrozenMode::Prefer)
        && aube_lockfile::active_lockfile_has_conflict_markers(&lockfile_dir);
    let existing_for_resolver: Option<&aube_lockfile::LockfileGraph> =
        lockfile_pre_parse.as_ref().map(|(g, _)| g);

    // `--lockfile-only` short-circuit. Resolves (or reuses a fresh
    // lockfile), writes the new lockfile, and exits before any tarball
    // fetch / link / lifecycle work. Runs *before* the FrozenMode
    // match so it bypasses drift hard-errors entirely — pnpm's
    // `--lockfile-only` regenerates regardless of frozen mode, and
    // we'd otherwise be preempted by the auto-CI Frozen default.
    // `enableModulesDir=false` follows the same short-circuit so
    // projects that persistently disable node_modules materialization
    // share the exact same control flow.
    if lockfile_only_effective {
        resolve::run_lockfile_only(resolve::LockfileOnlyInput {
            cwd: &cwd,
            mode,
            lockfile_dir: &lockfile_dir,
            lockfile_importer_key: &lockfile_importer_key,
            manifest: &manifest,
            manifests: &manifests,
            per_project_write_selection: per_project_write_selection.as_ref(),
            ws_config: &ws_config_shared,
            workspace_catalogs: &workspace_catalogs,
            settings_ctx: &settings_ctx,
            lockfile_pre_parse: lockfile_pre_parse.as_ref(),
            lockfile_conflict_marker_warning_emitted,
            existing_for_resolver,
            source_kind_before,
            lockfile_enabled,
            lockfile_include_tarball_url,
            shared_workspace_lockfile,
            has_workspace,
            is_workspace_project,
            ignore_pnpmfile: opts.ignore_pnpmfile,
            network_mode: opts.network_mode,
            global_pnpmfile: opts.global_pnpmfile.as_deref(),
            pnpmfile: opts.pnpmfile.as_deref(),
            minimum_release_age_override: opts.minimum_release_age_override,
            ws_package_versions: &ws_package_versions,
            ignore_scripts: opts.ignore_scripts,
            prog_ref,
        })
        .await?;
        return Ok(());
    }

    let planned_gvs =
        gvs::planned_global_virtual_store(use_global_virtual_store_override, &opts.env_snapshot);
    // The mode-change check must compare the existing `.aube/` tree against
    // what the linker will *actually* write, not the raw requested mode:
    // the linker forces per-project materialization when the hidden hoist
    // tree is on (`hoist=true`, the default) or the layout is `hoisted`.
    // Predicting the raw `planned_gvs` instead made every non-fast-path
    // install on a default project see a spurious `disabled → enabled`
    // transition and wipe `node_modules` (issue #71). `resolve_node_linker`
    // is the same resolver the link phase uses, so the two stay in lockstep.
    let effective_gvs = gvs::effective_global_virtual_store(
        planned_gvs,
        aube_settings::resolved::hoist(&settings_ctx),
        link::resolve_node_linker(&settings_ctx)?,
    );
    gvs::reset_on_mode_change(&cwd, &aube_dir, &modules_dir_name, effective_gvs)?;

    // 3. Parse or resolve lockfile, streaming tarball fetches during resolution
    let phase_start = std::time::Instant::now();
    let store = std::sync::Arc::new(super::open_store(&cwd)?);
    // Pre-create all 256 two-char shard directories in the CAS root.
    // `import_bytes` is called once per stored file (~7.5k for a medium
    // install) and previously did `mkdirp(parent)` per call — a stat
    // syscall that was the #1 hotspot in a dtrace/fs_usage profile.
    // With the shard tree pre-created, every `import_bytes` skips the
    // mkdirp entirely and lets its `create_new` open handle the
    // existence check atomically. Best-effort: a failure here is not
    // fatal because `import_bytes` retains the slow-path mkdirp
    // fallback when shards are missing.
    if let Err(e) = store.ensure_shards_exist() {
        tracing::debug!("ensure_shards_exist failed (slow path will cover): {e}");
    }
    // macOS fast-path gate: take an exclusive `try_lock` on
    // `<store>/v1/.install.lock`. If we get it, no other aube install is
    // running against this store right now, so the CAS write path can
    // skip the tempfile + persist_noclobber dance and write straight to
    // the final content-addressed path (`Store::enable_fast_path`). The
    // guard is held in `_store_lock` for the rest of this `run` call;
    // dropping it at function exit releases the lock. Contention falls
    // back to the safe tempfile path — concurrent installers still
    // proceed, just at the existing speed.
    //
    // Linux is unaffected: `create_cas_file` always uses O_TMPFILE+linkat
    // there, which is already atomic-by-construction and faster than
    // both options. Windows keeps the tempfile path; the fast-path branch
    // in `aube-store` is unix-only (`OpenOptionsExt::mode`), so gating
    // the lock acquisition on macOS too avoids opening a lock file that
    // nothing would consult.
    #[cfg(target_os = "macos")]
    let _store_lock = {
        let lock_dir = store
            .root()
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| store.root().to_path_buf());
        let _ = std::fs::create_dir_all(&lock_dir);
        let lock_path = lock_dir.join(".install.lock");
        match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
        {
            Ok(file) => match file.try_lock() {
                Ok(()) => {
                    store.enable_fast_path();
                    tracing::debug!("CAS fast path enabled (exclusive store lock acquired)");
                    Some(file)
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    tracing::debug!(
                        "another aube install is using this store; staying on tempfile path"
                    );
                    None
                }
                Err(std::fs::TryLockError::Error(e)) => {
                    tracing::debug!("store lock probe failed ({e}); staying on tempfile path");
                    None
                }
            },
            Err(e) => {
                tracing::debug!(
                    "could not open store lock at {} ({e}); staying on tempfile path",
                    lock_path.display()
                );
                None
            }
        }
    };

    let lockfile_result = resolve::select_lockfile_result(resolve::SelectLockfileInput {
        lockfile_enabled,
        mode,
        cwd: &cwd,
        lockfile_dir: &lockfile_dir,
        lockfile_importer_key: &lockfile_importer_key,
        manifest: &manifest,
        manifests: &manifests,
        ws_config: &ws_config_shared,
        workspace_catalogs: &workspace_catalogs,
        is_workspace_project,
        lockfile_pre_parse: lockfile_pre_parse.as_ref(),
    })?;

    // Deprecation messages from freshly-resolved packages. Only the
    // no-lockfile branch below populates this; the lockfile-reuse branch
    // has no packument in hand. Rendered right before the install summary
    // once `filter_graph` has culled dropped packages.
    let deprecations: std::sync::Arc<
        std::sync::Mutex<Vec<crate::deprecations::DeprecationRecord>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    // Per-direct-dep packument snapshot rendered inline by the install
    // summary printer (`+ name@version  deprecated · latest …`). Only
    // populated by the resolve-from-packuments branch — the frozen
    // lockfile reuse path has no cache to read from, so badges silently
    // degrade to empty rather than triggering extra network.
    let mut direct_dep_info: std::collections::HashMap<String, aube_resolver::DirectDepInfo> =
        std::collections::HashMap::new();

    // Captures the prewarm task's `compute_graph_hashes` output so the
    // link phase can reuse it instead of recomputing the same 4-pass
    // BLAKE3 walk over `graph.packages`. Populated by the no-lockfile
    // branch when the prewarm task uses GVS; left `None` on the
    // frozen-lockfile path or when the prewarm short-circuits.
    let mut prewarm_graph_hashes: Option<std::sync::Arc<aube_lockfile::graph_hash::GraphHashes>> =
        None;
    // Whether the post-resolve OSV routing actually covered this
    // install (assigned by both branches below). Input to the
    // `defaultTrust` floor.
    let osv_gate_active;
    // Whether this install's graph inherits resolution-time vetting
    // from an unchanged lockfile (frozen install / `aube ci` /
    // teammate clone, or a re-resolve that reproduced the locked
    // picks). The `defaultTrust` floor uses this to trust its
    // allowlist without a per-install OSV run — see
    // `wiki/commands/pm/supply-chain-posture.md` Decision 2.
    let lockfile_vetted;
    let (graph, package_indices, cached_count, fetch_count) = match lockfile_result {
        Ok((mut graph, kind)) => {
            // Under `sharedWorkspaceLockfile=false` the project's own
            // lockfile only carries the `.` importer, so the reuse path
            // would hand the linker a root-only graph and never relink
            // members (a deleted/incomplete member `node_modules` would
            // be reported "up to date" yet stay broken). Fold every
            // member's per-project lockfile back in so the linker sees
            // all importers. No-op for shared lockfiles, non-workspace
            // projects, and the cold resolve path (which already
            // produces every importer).
            if !shared_workspace_lockfile && has_workspace {
                merge_member_lockfile_graphs(&cwd, &mut graph, &manifests);
            }
            let graph = resolve::apply_lockfile_graph_platform_rules(
                graph,
                kind,
                &manifest,
                &ws_config_shared,
                &settings_ctx,
            )?;
            let source_label = resolve::lockfile_source_label(kind);
            tracing::debug!(
                "{source_label}: {} packages for {project_name}",
                graph.packages.len()
            );
            tracing::debug!(
                "phase:resolve (from lockfile) {:.1?}",
                phase_start.elapsed()
            );
            phase_timings.record("resolve", phase_start.elapsed());

            // Lockfile path: the total is known upfront, so seed the overall
            // bar with the full package count and enter the fetch phase.
            if let Some(p) = prog_ref {
                p.set_total(graph.packages.len());
                p.set_phase("fetching");
            }
            // Seed the chain index for diagnostic enrichment on the
            // lockfile fast path. Same effect as the resolve-fresh
            // branch above — error wrappers in `dep_chain` now know
            // each package's ancestor path.
            crate::dep_chain::set_active(&graph);
            aube_registry::slow_metadata::flush_summary();

            // Post-resolve OSV `MAL-*` routing — lockfile-found
            // branch. `fresh_resolution = false` here because the
            // graph came from the lockfile and we never ran the
            // resolver, so the router falls through to the mirror
            // backend unless `osv_transitive_check` or
            // `advisoryCheckEveryInstall` forces the live API.
            // Same helper as the no-lockfile branch — kept here so
            // `aube ci`, `aube install --frozen-lockfile`, and
            // every frozen reinstall actually run the routing
            // (previously skipped, surfaced by review).
            //
            // Scheduling: the gate is fired as a concurrent task that
            // overlaps the tarball-download phase below, then `await`ed
            // at line `lock_osv_gate_active = …` BEFORE any build /
            // lifecycle script runs (those live in
            // `run_finalize_phase`, well after fetch + link). The set
            // of packages queried and the gating decision are
            // identical to the prior serial-before-fetch call — only
            // the `await` point moved later, hiding the OSV round-trip
            // behind the download tail. Downloading a tarball the gate
            // later flags is harmless; it is never *executed* before
            // the gate clears because the `?` on the awaited result
            // aborts the whole install before the finalize/build phase.
            let osv_settings = resolve_osv_routing_settings(&cwd);
            let lock_osv_cwd = cwd.clone();
            let lock_osv_graph = graph.clone();
            let lock_osv_transitive_check = opts.osv_transitive_check;
            // Single-task `JoinSet` (abort-on-drop): a few fallible `?`
            // sites sit between here and the gate `await` below
            // (link-strategy / patch loading, the fetch). If any
            // early-returns, the install is aborting before the build
            // phase, and dropping the set cancels the in-flight probe so
            // it doesn't keep doing network I/O post-error. On the
            // normal path the verdict is consumed via `join_next()`
            // below, strictly before any build / lifecycle script.
            let mut lock_osv_set: tokio::task::JoinSet<miette::Result<bool>> =
                tokio::task::JoinSet::new();
            lock_osv_set.spawn(async move {
                super::add_supply_chain::run_post_resolve_osv_routing(
                    &lock_osv_cwd,
                    &lock_osv_graph,
                    /*fresh_resolution=*/ false,
                    lock_osv_transitive_check,
                    osv_settings.advisory_check,
                    osv_settings.advisory_check_on_install,
                    osv_settings.advisory_bloom_check,
                    osv_settings.advisory_check_every_install,
                )
                .await
            });
            // Graph came straight from the lockfile (frozen reinstall /
            // `aube ci` / clone) — it carries the advisory + cooling
            // vetting performed when the lockfile was written, so the
            // `defaultTrust` floor inherits it rather than requiring a
            // (correctly skipped) per-install OSV run.
            lockfile_vetted = true;

            // Check index cache, fetch missing tarballs. Tarball client
            // is lazy because eager construction costs ~20ms even when
            // no request gets sent, dominating no-op install time.
            //
            // Pipeline GVS materialization into the fetch tail. Same
            // shape as the no-lockfile branch. Channel feeds a
            // concurrent materializer that reflinks into GVS, hiding
            // link-step-1 cost behind the fetch tail.
            let phase_start = std::time::Instant::now();
            let network_mode = opts.network_mode;
            let cwd_for_client = cwd.clone();

            let lock_node_version = crate::engines::effective_node_version(
                aube_settings::resolved::node_version(&settings_ctx).as_deref(),
            );
            let lock_build_policy = std::sync::Arc::new(build_policy.clone());
            let lock_strategy = resolve_link_strategy(&cwd, &settings_ctx, planned_gvs)?;
            let (lock_patches, lock_patch_hashes) =
                crate::patches::load_patches_for_linker(&cwd, &graph.patched_dependencies)?;
            let (lock_materialize_tx, lock_materialize_rx) = materialize_channel();
            let lock_prewarm_inputs = GvsPrewarmInputs {
                graph: std::sync::Arc::new(graph.clone()),
                store: store.clone(),
                cwd: cwd.clone(),
                virtual_store_dir_max_length,
                link_strategy: lock_strategy,
                link_concurrency: link_concurrency_setting,
                patches: lock_patches,
                patch_hashes: lock_patch_hashes,
                node_version: lock_node_version,
                build_policy: lock_build_policy,
                use_global_virtual_store_override,
                virtual_store_dir: aube_dir.clone(),
            };
            let lock_materialize_handle =
                spawn_gvs_prewarm(lock_prewarm_inputs, lock_materialize_rx);

            let fetch_result = fetch_packages_with_root(
                &graph.packages,
                &store,
                || {
                    std::sync::Arc::new(
                        make_client(&cwd_for_client).with_network_mode(network_mode),
                    )
                },
                prog_ref,
                &cwd,
                &aube_dir,
                Some(lock_materialize_tx),
                // Workspace installs disable the `AlreadyLinked` fast path
                // under the upstream default (full warm-store-verify): the
                // historical rationale was that `link_workspace` would
                // invalidate the classification. That no longer holds —
                // `link_workspace` preserves `.aube/<dep_path>` virtual-store
                // entries across warm re-runs (Step 1b's Fresh/Missing/Stale
                // state machine readlinks them; it does not wipe `.aube/`).
                // So when an embedder opts into fast-trust
                // (`warm_store_verify = false` on the embedder profile) we re-enable the shortcut
                // for workspaces too, skipping a serial per-package
                // `load_index` inside the linker. Default (verify on) keeps
                // upstream behavior exactly.
                /*skip_already_linked_shortcut=*/
                has_workspace && warm_store_verify(),
                virtual_store_dir_max_length,
                opts.ignore_scripts,
                network_concurrency_setting,
                verify_store_integrity_setting,
                strict_store_integrity_setting,
                strict_store_pkg_content_check_setting,
                opts.git_prepare_depth,
                inherited_build_policy_for_git_prepare.clone(),
                resolve_git_shallow_hosts(&settings_ctx),
            )
            .await;
            // Don't abort the materializer on fetch err: the failing
            // fetch task drops its `tx`, so the materializer's `rx`
            // closes and it exits naturally. Awaiting first lets a real
            // materializer error (the likely root cause of a generic
            // "materializer task exited..." fetch err) surface instead.
            let (indices, cached, fetched) = match fetch_result {
                Ok(t) => t,
                Err(e) => {
                    // Fetch failed: the install is aborting, so no build
                    // script will run. Dropping `lock_osv_set` aborts the
                    // in-flight OSV probe so it doesn't keep doing network
                    // I/O after the CLI has errored.
                    drop(lock_osv_set);
                    return Err(combine_install_pipeline_errors(lock_materialize_handle, e).await);
                }
            };
            // Materializer stats roll into link via GVS-already-linked
            // fast path. Errors abort install.
            let _ = lock_materialize_handle.await.into_diagnostic()??;
            // Gate: consume the OSV verdict that ran concurrently with
            // the download phase above. This `await` is strictly before
            // the link + finalize phases, so a `MAL-*` finding aborts
            // the install (via `?`) before any dependency build /
            // lifecycle script can execute — the security posture is
            // unchanged, only the OSV round-trip now overlaps downloads
            // instead of serializing ahead of them. `join_next()` is
            // `Some` (exactly one spawned task); inner `?` = OSV finding
            // / required-check failure, outer `?` = task-join panic.
            osv_gate_active = match lock_osv_set.join_next().await {
                Some(joined) => joined.into_diagnostic()??,
                None => unreachable!("OSV JoinSet had exactly one spawned task"),
            };
            tracing::debug!(
                "phase:fetch {:.1?} ({fetched} packages)",
                phase_start.elapsed()
            );
            phase_timings.record("fetch", phase_start.elapsed());

            (graph, indices, cached, fetched)
        }
        Err(aube_lockfile::Error::NotFound(_))
            if !(matches!(mode, FrozenMode::Frozen) && opts.strict_no_lockfile) =>
        {
            // No lockfile — resolve + fetch tarballs concurrently
            tracing::debug!("No lockfile found, resolving dependencies for {project_name}...");
            if let Some(p) = prog_ref {
                // Seed the resolving-phase denominator floor from any
                // existing lockfile on disk. In FrozenMode::Fix /
                // Prefer we already parsed it into
                // `existing_for_resolver`; in FrozenMode::No the
                // pre-parse is skipped (we always re-resolve), so peek
                // the disk lockfile inline. The cost is one extra
                // parse on the fresh-resolve path, dwarfed by the
                // resolve itself — and the resulting estimate lets
                // the resolving bar show real progress instead of an
                // empty placeholder.
                let lockfile_estimate =
                    existing_for_resolver.map(|g| g.packages.len()).or_else(|| {
                        parse_lockfile_dir_remapped_with_kind(
                            &lockfile_dir,
                            &lockfile_importer_key,
                            &manifest,
                        )
                        .ok()
                        .map(|(g, _)| g.packages.len())
                    });
                if let Some(n) = lockfile_estimate {
                    p.set_total_floor(n);
                }
                p.set_phase("resolving");
            }
            // Resolve node version + build policy up front so the
            // GVS-prewarm materializer (spawned below the resolver
            // await) can compute the same graph hashes the link phase
            // will. Keeping a single source of truth avoids any
            // subdir-name drift between prewarm and link step 1.
            let node_version_for_prewarm = crate::engines::effective_node_version(
                aube_settings::resolved::node_version(&settings_ctx).as_deref(),
            );
            let build_policy_for_prewarm = std::sync::Arc::new(build_policy.clone());
            let client =
                std::sync::Arc::new(make_client(&cwd).with_network_mode(opts.network_mode));
            // Speculative TLS + TCP + HTTP/2 handshake. Fires while the
            // rest of this function builds the resolver, parses the
            // manifest, and reads the lockfile. By the time the
            // resolver requests its first packument the connection
            // pool is already warm, hiding ~50-150 ms of handshake on
            // cold installs. `AUBE_DISABLE_SPECULATIVE_TLS=1` opts
            // out.
            client.prewarm_connection();
            let tarball_client = client.clone();

            // Set up streaming resolver with disk-backed packument cache.
            // Resolver options are applied via `configure_resolver` so the
            // `--lockfile-only` short-circuit produces an identical lockfile.
            // `AUBE_CONCURRENCY` is an emergency override for users on slow
            // private registries (Artifactory, Nexus) where the default
            // 128 in-flight tarballs trigger 429/503 throttling. Honored
            // ahead of `network_concurrency_setting` so the env var wins
            // over npmrc + workspace yaml.
            let env_concurrency =
                aube_util::concurrency::parse_concurrency_env().map(|n| n as usize);
            let fetch_network_concurrency = env_concurrency
                .or(network_concurrency_setting)
                .unwrap_or_else(default_streaming_network_concurrency);
            // Channel capacity is decoupled from fetch concurrency: the
            // mpsc just buffers ResolvedPackage handoffs so the BFS
            // never blocks on send() while the fetch coordinator is
            // mid-tarball. Sized to absorb deep-tree bursts without
            // backpressure on graphs into the tens of thousands of
            // packages; fetch parallelism is still gated by
            // `fetch_network_concurrency` downstream.
            let stream_capacity = fetch_network_concurrency.saturating_mul(16).max(1024);
            let (resolver, mut resolved_rx) =
                aube_resolver::Resolver::with_stream_capacity(client, stream_capacity);
            let pnpmfile_paths = if opts.ignore_pnpmfile {
                Vec::new()
            } else {
                crate::pnpmfile::ordered_paths(
                    crate::pnpmfile::detect_global(&cwd, opts.global_pnpmfile.as_deref())
                        .as_deref(),
                    crate::pnpmfile::detect(
                        &cwd,
                        opts.pnpmfile.as_deref(),
                        ws_config_shared.pnpmfile_path.as_deref(),
                    )
                    .as_deref(),
                )
            };
            super::run_pnpmfile_pre_resolution(&pnpmfile_paths, &cwd, existing_for_resolver)
                .await?;
            let (read_package_host, read_package_forwarders) =
                match crate::pnpmfile::ReadPackageHostChain::spawn(&pnpmfile_paths, &cwd)
                    .await
                    .wrap_err("failed to start pnpmfile readPackage host")?
                {
                    Some((h, f)) => (Some(h), f),
                    None => (None, Vec::new()),
                };
            let read_package_hook: Option<Box<dyn aube_resolver::ReadPackageHook>> =
                read_package_host.map(|h| Box::new(h) as Box<dyn aube_resolver::ReadPackageHook>);
            let mut resolver = configure_resolver(
                resolver,
                &cwd,
                &manifest,
                ResolverConfigInputs {
                    settings_ctx: &settings_ctx,
                    workspace_config: &ws_config_shared,
                    workspace_catalogs: &workspace_catalogs,
                    minimum_release_age_override: opts.minimum_release_age_override,
                    // Same disambiguation as the `--lockfile-only` path:
                    // `None` only when no lockfile will be written, so
                    // widening to every common platform doesn't happen
                    // just to be discarded.
                    target_lockfile_kind: lockfile_enabled.then(|| {
                        source_kind_before
                            .unwrap_or_else(|| super::default_lockfile_kind(&settings_ctx))
                    }),
                    cache_full_packuments: true,
                    ignore_scripts: opts.ignore_scripts,
                },
                read_package_hook,
            );

            // Spawn the tarball fetch coordinator — it starts fetching as
            // packages arrive from the resolver, overlapping network I/O.
            // Clone the registry client up front so the post-fetch
            // lockfile-write step (below) can still use it to derive
            // tarball URLs when `lockfileIncludeTarballUrl=true` — the
            // `tokio::spawn` below moves one clone into the fetch
            // coordinator's task.
            let post_fetch_client = tarball_client.clone();
            let fetch_store = store.clone();
            let fetch_progress = prog.clone();
            let fetch_project_root = cwd.clone();
            let fetch_local_client = tarball_client.clone();
            let fetch_ignore_scripts = opts.ignore_scripts;
            let fetch_git_prepare_depth = opts.git_prepare_depth;
            let fetch_inherited_build_policy = inherited_build_policy_for_git_prepare.clone();
            let fetch_verify_integrity = verify_store_integrity_setting;
            let fetch_strict_integrity = strict_store_integrity_setting;
            let fetch_strict_pkg_content_check = strict_store_pkg_content_check_setting;
            let fetch_git_shallow_hosts = resolve_git_shallow_hosts(&settings_ctx);
            // Host-side platform filter for the streaming fetch. The
            // resolver widens its graph filter for aube-lock.yaml so
            // the committed lockfile carries native optionals for every
            // common platform, but that widening mustn't make us
            // download every foreign-platform tarball up front — most
            // of them will disappear when `filter_graph` trims optional
            // edges below, and only a vanishingly rare broken-package
            // shape (required dep with platform constraints) actually
            // needs the fetch. A post-resolve catch-up pass picks up
            // those stragglers from the finalized graph; here we just
            // defer. `filter_graph` keys off the same narrow manifest
            // set, so a deferred package that survives the trim is
            // exactly one the catch-up must fetch.
            let (fetch_sup_os, fetch_sup_cpu, fetch_sup_libc) =
                settings::effective_supported_architectures(
                    &manifest,
                    &ws_config_shared,
                    &settings_ctx,
                );
            let fetch_supported_arch = aube_resolver::SupportedArchitectures {
                os: fetch_sup_os,
                cpu: fetch_sup_cpu,
                libc: fetch_sup_libc,
                ..Default::default()
            };
            // Each imported (dep_path, index) feeds the GVS-prewarm
            // materializer running concurrently with the rest of fetch.
            /*
             * Materialize channel sized from the cross run learned
             * recommendation when available, falling back to the
             * static default. Tokio mpsc cap is fixed at
             * construction so the only knob we can turn here is
             * the initial size for this process. Bounds 256 to
             * 16384 cap RAM and floor progress.
             */
            let (materialize_tx, materialize_rx) = materialize_channel();
            // Clone the shared deprecations accumulator into the
            // spawned task. The install command reads it back after
            // `filter_graph` prunes the post-resolve graph.
            let fetch_deprecations_tx = deprecations.clone();
            let fetch_handle = tokio::spawn(async move {
                /*
                 * Adaptive tarball concurrency. Loaded from the
                 * cross run persistent store when available so the
                 * limiter starts where a previous run converged
                 * instead of cold ramping from the ceiling. Falls
                 * back to seed 256 (h2 stream cap) on first ever
                 * run. Floor 4 keeps progress under continuous
                 * 429 / 503. Persisted back at end of fetch phase
                 * so the next invocation benefits.
                 */
                // Honor user-configured `networkConcurrency` (or
                // `AUBE_NETWORK_CONCURRENCY` env override) as the
                // seed. Adaptive grow/shrink still operate around
                // it. Floor 4 keeps progress under continuous
                // throttling regardless of seed.
                let tarball_seed = fetch_network_concurrency.max(4);
                let tarball_max = tarball_seed.max(256);
                let persistent = aube_util::adaptive::global_persistent_state();
                let semaphore = match persistent.as_ref() {
                    Some(state) => aube_util::adaptive::AdaptiveLimit::from_persistent(
                        state,
                        "tarball:default",
                        tarball_seed,
                        4,
                        tarball_max,
                    ),
                    None => aube_util::adaptive::AdaptiveLimit::new(tarball_seed, 4, tarball_max),
                };
                let semaphore_for_persist = std::sync::Arc::clone(&semaphore);
                let persistent_for_save = persistent.clone();
                // Hoist env-driven flags out of the per-tarball loop.
                let streaming_sha512_enabled =
                    aube_util::env::embedder_env("DISABLE_STREAMING_SHA512").is_none();
                let tarball_stream_enabled =
                    aube_util::env::embedder_env("DISABLE_TARBALL_STREAM").is_none();
                // JoinSet over bare Vec<JoinHandle>. If the first
                // fetch errors and we return via `?`, a plain Vec
                // drops the remaining JoinHandles which detaches the
                // tasks. They keep fetching tarballs and writing
                // to the CAS while the CLI has already errored.
                // JoinSet aborts every outstanding task on drop,
                // matches the pattern ensure_dep_scripts uses.
                let mut handles: tokio::task::JoinSet<
                    miette::Result<(String, aube_store::PackageIndex)>,
                > = tokio::task::JoinSet::new();
                let mut indices: BTreeMap<String, aube_store::PackageIndex> = BTreeMap::new();
                let mut cached_count = 0usize;
                // Drives the resolving-phase denominator estimate.
                // `received + pkg.pending` is a non-strict lower bound
                // on the final resolved-package count; raising it via
                // `set_total_floor` makes the bar fill as the
                // BFS-frontier high-water mark grows. Tracked locally
                // because the resolver's view is per-send, not a
                // single shared atomic.
                let mut resolved_received: usize = 0;

                while let Some(pkg) = resolved_rx.recv().await {
                    if let Some(ref msg) = pkg.deprecated {
                        fetch_deprecations_tx.lock().unwrap().push(
                            crate::deprecations::DeprecationRecord {
                                name: pkg.name.clone(),
                                version: pkg.version.clone(),
                                dep_path: pkg.dep_path.clone(),
                                message: msg.clone(),
                            },
                        );
                    }
                    // Each resolved package bumps the overall denominator by
                    // one. Cached packages are immediately credited against
                    // the numerator; missing ones get a transient child row.
                    //
                    // Bumping the denominator *before* the platform-deferred
                    // skip below is intentional: the catch-up pass (after
                    // `filter_graph`) credits surviving deferred packages
                    // against the numerator, and skipping the increment
                    // here would let the numerator overrun the denominator
                    // (the historical "2/1 packages" display bug). The
                    // overcount on dropped optionals is reconciled by a
                    // single `set_total(graph.packages.len())` after
                    // `filter_graph` runs.
                    resolved_received += 1;
                    if let Some(p) = fetch_progress.as_ref() {
                        p.inc_total(1);
                        // Raise the resolving-phase denominator floor
                        // toward the resolver's current frontier so
                        // the bar fills against a meaningful target
                        // instead of an empty placeholder. Stamping
                        // the frontier on each `ResolvedPackage`
                        // keeps the protocol shape unchanged.
                        p.set_total_floor(resolved_received + pkg.pending);
                        if let Some(sz) = pkg.unpacked_size {
                            p.inc_estimated_bytes(&pkg.dep_path, sz);
                        }
                    }

                    // Defer platform-mismatched registry packages to
                    // the post-filter_graph catch-up pass: almost all
                    // of them are optional natives that `filter_graph`
                    // is about to drop, so fetching up front would just
                    // waste bandwidth. Local `file:`/`link:` deps
                    // always fetch here — they carry empty platform
                    // arrays and `is_supported` treats them as
                    // unconstrained.
                    if pkg.local_source.is_none()
                        && !aube_resolver::is_supported(
                            &pkg.os,
                            &pkg.cpu,
                            &pkg.libc,
                            &fetch_supported_arch,
                        )
                    {
                        tracing::debug!(
                            "deferring tarball fetch for {}@{}: platform mismatch (catch-up will cover survivors)",
                            pkg.name,
                            pkg.version
                        );
                        continue;
                    }

                    // Local (`file:` / `link:`) deps materialize from
                    // disk, not the registry — short-circuit the
                    // tarball pipeline.
                    if let Some(ref local) = pkg.local_source {
                        match import_local_source(
                            &fetch_store,
                            &fetch_project_root,
                            local,
                            Some(&fetch_local_client),
                            fetch_ignore_scripts,
                            fetch_git_prepare_depth,
                            fetch_inherited_build_policy.clone(),
                            &fetch_git_shallow_hosts,
                            &pkg.name,
                            &pkg.version,
                        )
                        .await
                        {
                            Ok(Some(index)) => {
                                // Send failure means the materializer
                                // task died. Bail now instead of
                                // continuing to import tarballs into a
                                // half-wired virtual store.
                                materialize_tx
                                    .send((pkg.dep_path.clone(), index.clone()))
                                    .await
                                    .map_err(|_| {
                                        miette!("materializer task exited before fetch finished")
                                    })?;
                                indices.insert(pkg.dep_path, index);
                                cached_count += 1;
                                if let Some(p) = fetch_progress.as_ref() {
                                    p.inc_reused(1);
                                }
                            }
                            Ok(None) => {
                                if let Some(p) = fetch_progress.as_ref() {
                                    p.inc_reused(1);
                                }
                            }
                            Err(e) => return Err(e),
                        }
                        continue;
                    }

                    // Check index cache first. `registry_name()` is
                    // the real package name on the registry — equal
                    // to `name` for the common case, and the alias's
                    // real target for npm-alias entries (where the
                    // alias-qualified name would miss the cache and
                    // later 404 the tarball fetch). Integrity is part
                    // of the cache key so a github-sourced tarball
                    // under the same (name, version) can't return the
                    // registry-cached file list.
                    //
                    // Stat depth follows the warm-store-verify seam:
                    // full per-file verify by default (upstream), or
                    // first-file-only under an embedder that opted into
                    // fast-trust via `warm_store_verify = false` on the embedder profile.
                    // Either way a stale index drops here and re-fetches
                    // the tarball cleanly instead of letting the
                    // materializer die later with
                    // `ERR_AUBE_MISSING_STORE_FILE`. Independent of
                    // import-time SRI / `verifyStoreIntegrity`.
                    let pkg_registry_name = pkg.registry_name().to_string();
                    if let Some(index) = warm_load_index(
                        &fetch_store,
                        &pkg_registry_name,
                        &pkg.version,
                        pkg.integrity.as_deref(),
                    ) {
                        materialize_tx
                            .send((pkg.dep_path.clone(), index.clone()))
                            .await
                            .map_err(|_| {
                                miette!("materializer task exited before fetch finished")
                            })?;
                        indices.insert(pkg.dep_path, index);
                        cached_count += 1;
                        if let Some(p) = fetch_progress.as_ref() {
                            p.inc_reused(1);
                        }
                        continue;
                    }

                    let sem = semaphore.clone();
                    let store = fetch_store.clone();
                    let client = tarball_client.clone();
                    let row = fetch_progress
                        .as_ref()
                        .map(|p| p.start_fetch(&pkg.name, &pkg.version));
                    let bytes_progress = fetch_progress.clone();

                    handles.spawn(async move {
                        let _row = row;
                        let _diag_tar = aube_util::diag::Span::new(aube_util::diag::Category::Fetch, "tarball")
                            .with_meta_fn(|| format!(r#"{{"name":{},"version":{}}}"#,
                                aube_util::diag::jstr(&pkg.name), aube_util::diag::jstr(&pkg.version)));
                        let _diag_tar_inflight = aube_util::diag::inflight(aube_util::diag::Slot::Tar);
                        let permit_wait = std::time::Instant::now();
                        let permit = sem.acquire().await;
                        let permit_wait_ms = permit_wait.elapsed();
                        let pkg_id_for_diag = format!("{}@{}", pkg.name, pkg.version);
                        if permit_wait_ms.as_millis() > 1 {
                            aube_util::diag::event_lazy(aube_util::diag::Category::Fetch, "tarball_permit_wait", permit_wait_ms, || format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(&pkg.name)));
                        }
                        aube_util::diag::attribute_wait(
                            aube_util::diag::Slot::Tar,
                            &pkg_id_for_diag,
                            permit_wait_ms,
                        );
                        let _tar_holder = aube_util::diag::register_holder(
                            aube_util::diag::Slot::Tar,
                            &pkg_id_for_diag,
                        );
                        let url = pkg.tarball_url.clone().unwrap_or_else(|| {
                            client.tarball_url(&pkg_registry_name, &pkg.version)
                        });

                        tracing::trace!("Fetching {}@{}", pkg.name, pkg.version);

                        let pkg_display_name = pkg.name.clone();
                        let pkg_version = pkg.version.clone();
                        let dep_path = pkg.dep_path.clone();
                        let integrity = pkg.integrity.clone();

                        let stream_eligible = tarball_stream_enabled
                            && integrity
                                .as_deref()
                                .is_none_or(|s| s.starts_with("sha512-"));
                        aube_util::diag::instant_lazy(aube_util::diag::Category::Fetch, "tarball_path", || format!(r#"{{"streaming":{},"name":{}}}"#, stream_eligible, aube_util::diag::jstr(&pkg.name)));
                        if stream_eligible {
                            let streamed = crate::commands::install::lifecycle::fetch_and_import_tarball_streaming(
                                &client,
                                &store,
                                &url,
                                &pkg_display_name,
                                &pkg_registry_name,
                                &pkg_version,
                                integrity.as_deref(),
                                fetch_verify_integrity,
                                fetch_strict_integrity,
                                fetch_strict_pkg_content_check,
                            )
                            .await;
                            let (index, bytes_len) = match streamed {
                                Ok(v) => {
                                    permit.record_success();
                                    v
                                }
                                Err(e) => {
                                    if e.is_throttle {
                                        permit.record_throttle();
                                    } else {
                                        permit.record_cancelled();
                                    }
                                    return Err(e.into());
                                }
                            };
                            if let Some(p) = bytes_progress.as_ref() {
                                p.inc_downloaded_bytes(bytes_len);
                            }
                            return Ok::<_, miette::Report>((dep_path, index));
                        }

                        let fetch_outcome = if streaming_sha512_enabled {
                            client
                                .fetch_tarball_bytes_streaming_sha512(&url)
                                .await
                                .map(|(b, d)| (b, Some(d)))
                                .map_err(|e| {
                                    let throttled = e.is_throttle();
                                    (
                                        miette!(
                                            "failed to fetch {}@{}: {e}{}",
                                            pkg.name,
                                            pkg.version,
                                            crate::dep_chain::format_chain_for(&pkg.name, &pkg.version)
                                        ),
                                        throttled,
                                    )
                                })
                        } else {
                            client.fetch_tarball_bytes(&url).await.map(|b| (b, None)).map_err(|e| {
                                let throttled = e.is_throttle();
                                (
                                    miette!(
                                        "failed to fetch {}@{}: {e}{}",
                                        pkg.name,
                                        pkg.version,
                                        crate::dep_chain::format_chain_for(&pkg.name, &pkg.version)
                                    ),
                                    throttled,
                                )
                            })
                        };
                        let (bytes, streamed_digest) = match fetch_outcome {
                            Ok(v) => {
                                permit.record_success();
                                v
                            }
                            Err((report, throttled)) => {
                                if throttled {
                                    permit.record_throttle();
                                } else {
                                    permit.record_cancelled();
                                }
                                return Err(report);
                            }
                        };
                        if let Some(p) = bytes_progress.as_ref() {
                            p.inc_downloaded_bytes(bytes.len() as u64);
                        }

                        let (index, _) = run_import_on_blocking(
                            store,
                            bytes,
                            streamed_digest,
                            pkg_display_name,
                            pkg_registry_name,
                            pkg_version,
                            integrity,
                            fetch_verify_integrity,
                            fetch_strict_integrity,
                            fetch_strict_pkg_content_check,
                        )
                        .await?;

                        Ok::<_, miette::Report>((dep_path, index))
                    });
                }

                // Collect all fetch results via JoinSet. Drop on
                // error aborts outstanding siblings.
                let fetch_count = handles.len();
                while let Some(joined) = handles.join_next().await {
                    let (dep_path, index) = joined.into_diagnostic()??;
                    materialize_tx
                        .send((dep_path.clone(), index.clone()))
                        .await
                        .map_err(|_| miette!("materializer task exited before fetch finished"))?;
                    indices.insert(dep_path, index);
                }
                // Explicitly drop the materialize sender so the
                // materializer consumer sees the channel close and
                // exits its receive loop.
                drop(materialize_tx);
                if let Some(state) = persistent_for_save.as_ref() {
                    semaphore_for_persist.persist(state, "tarball:default");
                }
                Ok::<_, miette::Report>((indices, cached_count, fetch_count))
            });

            // Run resolution (this streams packages to the fetch coordinator).
            // `existing_for_resolver` is `Some` when Fix / Prefer parsed a
            // lockfile cleanly; the resolver reuses already-pinned versions
            // for unchanged specs and only re-resolves entries whose spec
            // drifted. `No` mode (`--no-frozen-lockfile`) intentionally
            // stays at `None` so the user gets the fresh resolve they
            // asked for.
            aube_util::diag::instant(aube_util::diag::Category::Install, "resolve_begin", None);
            let _diag_resolve =
                aube_util::diag::Span::new(aube_util::diag::Category::Install, "phase_resolve");
            let resolve_result = if has_workspace {
                resolver
                    .resolve_workspace(&manifests, existing_for_resolver, &ws_package_versions)
                    .await
            } else {
                resolver.resolve(&manifest, existing_for_resolver).await
            }
            .map_err(miette::Report::new)
            .wrap_err("failed to resolve dependencies");

            if resolve_result.is_err() {
                fetch_handle.abort();
                return resolve_result.map(|_| unreachable!());
            }
            let mut graph = resolve_result.unwrap();
            // Snapshot per-direct-dep packument facts before dropping the
            // resolver — its `cache` field owns the only copy and the
            // install summary printer runs much later, well after the
            // channel-closing drop below.
            direct_dep_info = resolver.direct_dep_info(&graph);
            // Drop the resolver to close the channel, signaling the fetch
            // coordinator to finish, then drain the readPackage stderr
            // forwarders so every `ctx.log` record from resolve flushes
            // to stdout before afterAllResolved emits its own pnpm:hook
            // records. Doing this in the order drop → drain → hook keeps
            // resolve-time logs strictly ahead of afterAllResolved-time
            // logs in the ndjson stream.
            drop(resolver);
            crate::pnpmfile::ReadPackageHostChain::drain_forwarders(read_package_forwarders).await;
            crate::pnpmfile::run_after_all_resolved_chain(&pnpmfile_paths, &cwd, &mut graph)
                .await?;
            // Record the project's patch configuration (manifest /
            // workspace-yaml `patchedDependencies` + sha256 of each
            // patch file) on the graph before anything downstream
            // clones it — the lockfile writers need it to emit pnpm
            // 10's `{hash, path}` block and `(patch_hash=…)` suffixes
            // / bun's `patchedDependencies` block, without which the
            // real PMs reject or silently unpatch a frozen install.
            crate::patches::record_patches_on_graph(&cwd, &mut graph)?;
            // Overlay per-package metadata the resolver can't recover
            // from abbreviated (corgi) packuments — `license`,
            // `funding_url`, bun's `configVersion` — from the
            // existing lockfile when one was on disk. Without this,
            // `aube install --no-frozen-lockfile` drops those fields
            // on every re-resolve even though the resolved versions
            // didn't change, which churns the lockfile diff against
            // formats (npm, bun) that preserve them.
            // Reuse the pre-parsed lockfile when the resolver already
            // loaded it for seeding (Fix/Prefer modes). Skips a second
            // YAML parse pass over the same 5-50 KB file.
            if let Some((prior, _)) = lockfile_pre_parse.as_ref() {
                graph.overlay_metadata_from(prior);
            } else if let Ok(prior) =
                parse_lockfile_dir_remapped(&lockfile_dir, &lockfile_importer_key, &manifest)
            {
                graph.overlay_metadata_from(&prior);
            }
            tracing::debug!("Resolved {} packages", graph.packages.len());
            // Seed the chain index for diagnostic enrichment. Any
            // post-resolver error wrapping `(name, version)` via
            // `crate::dep_chain::format_chain_for` now sees a
            // chain back to the importer.
            crate::dep_chain::set_active(&graph);
            aube_registry::slow_metadata::flush_summary();

            // Post-resolve OSV `MAL-*` routing — no-lockfile /
            // re-resolve branch. The lockfile-found branch has the
            // parallel call before its own fetch so both paths
            // run through the same router. See
            // `add_supply_chain::run_post_resolve_osv_routing` for
            // the decision table. Fires before the pluggable
            // scanner so a confirmed-malicious advisory aborts
            // without spawning the scanner.
            let prior_lockfile = lockfile_pre_parse.as_ref().map(|(g, _)| g);
            let fresh_resolution =
                super::add_supply_chain::lockfile_has_new_picks(&cwd, prior_lockfile, &graph);
            let osv_settings = resolve_osv_routing_settings(&cwd);
            // Fire the OSV gate as a concurrent task that overlaps the
            // tail of the in-flight tarball downloads (`fetch_handle`,
            // spawned during resolution above and still draining here),
            // then `await` it just before the fetch await below —
            // strictly before link + finalize, so the gate still aborts
            // the install before any dependency build / lifecycle script
            // runs. Set of packages queried + the gating decision are
            // unchanged; only the `await` point moved past the download
            // tail. A flagged tarball may finish downloading, but it is
            // never executed before the gate clears (the `?` on the
            // awaited verdict aborts ahead of the finalize/build phase).
            let fresh_osv_cwd = cwd.clone();
            let fresh_osv_graph = graph.clone();
            let fresh_osv_transitive_check = opts.osv_transitive_check;
            // Single-task `JoinSet` rather than a bare `tokio::spawn`
            // so the OSV probe is aborted-on-drop: between here and the
            // gate `await` below sit several fallible `?` sites (the
            // security scanner, patch/link-strategy loading, the
            // fetch-join). If any of them early-returns, the install is
            // aborting before the build phase anyway, and the `JoinSet`
            // drop cancels the in-flight probe so no detached task keeps
            // doing network I/O after the CLI has errored. The verdict
            // is consumed via `join_next()` on the success path below.
            let mut fresh_osv_set: tokio::task::JoinSet<miette::Result<bool>> =
                tokio::task::JoinSet::new();
            fresh_osv_set.spawn(async move {
                super::add_supply_chain::run_post_resolve_osv_routing(
                    &fresh_osv_cwd,
                    &fresh_osv_graph,
                    fresh_resolution,
                    fresh_osv_transitive_check,
                    osv_settings.advisory_check,
                    osv_settings.advisory_check_on_install,
                    osv_settings.advisory_bloom_check,
                    osv_settings.advisory_check_every_install,
                )
                .await
            });
            // The resolver ran, but when it reproduced the locked picks
            // (`fresh_resolution == false`) the graph still matches what
            // the lockfile vetted, so the floor may inherit that vetting.
            // A graph with new picks is covered by this install's OSV
            // gate (live path) instead, not by inheritance.
            lockfile_vetted = !fresh_resolution;

            // Bun-compatible security scanner runs against the
            // *resolved* graph — full transitive set with concrete
            // versions, matching Bun's contract. Fires before fetch
            // so a `fatal` advisory aborts without wasting bandwidth
            // on tarball downloads. Fail-closed on any subprocess
            // failure (see `commands::security_scanner`); empty
            // `securityScanner` (the default) short-circuits to a
            // no-op without spawning `node`.
            let scanner = super::with_settings_ctx(&cwd, aube_settings::resolved::security_scanner);
            if !scanner.is_empty() {
                let scanner_packages =
                    super::security_scanner::resolved_packages_for_scanner(&graph);
                super::security_scanner::run_scanner(&scanner, &cwd, &scanner_packages).await?;
            }

            if let Some(p) = prog_ref {
                p.set_phase("fetching");
            }
            tracing::debug!("phase:resolve (fresh) {:.1?}", phase_start.elapsed());
            phase_timings.record("resolve", phase_start.elapsed());
            drop(_diag_resolve);
            aube_util::diag::instant(aube_util::diag::Category::Install, "resolve_end", None);

            // fetch_handle streams imported (dep_path, index) tuples
            // into the materializer, which reflinks each into
            // ~/.cache/aube/virtual-store. Used to run serially after
            // fetch as link step 1. Now overlaps with in-flight
            // downloads and post-resolve bookkeeping. Link step 1
            // below hits pkg_nm_dir.exists() fast path and only writes
            // the per-project .aube/<dep_path> symlink.
            let materialize_phase_start = std::time::Instant::now();
            let materialize_graph_arc = std::sync::Arc::new(graph.clone());
            let materialize_strategy = resolve_link_strategy(&cwd, &settings_ctx, planned_gvs)?;
            let (materialize_patches, materialize_patch_hashes) =
                crate::patches::load_patches_for_linker(&cwd, &graph.patched_dependencies)?;
            let materialize_inputs = GvsPrewarmInputs {
                graph: materialize_graph_arc.clone(),
                store: store.clone(),
                cwd: cwd.clone(),
                virtual_store_dir_max_length,
                link_strategy: materialize_strategy,
                link_concurrency: link_concurrency_setting,
                patches: materialize_patches,
                patch_hashes: materialize_patch_hashes,
                node_version: node_version_for_prewarm.clone(),
                build_policy: build_policy_for_prewarm.clone(),
                use_global_virtual_store_override,
                virtual_store_dir: aube_dir.clone(),
            };
            aube_util::diag::instant(
                aube_util::diag::Category::Install,
                "materialize_spawn",
                None,
            );
            let materialize_handle = spawn_gvs_prewarm(materialize_inputs, materialize_rx);

            // On fetch err, await the materializer (don't abort): the
            // failing fetch task drops its `tx`, so the materializer's
            // `rx` closes and it exits naturally. Awaiting first lets a
            // real materializer error (the likely root cause of a
            // generic "materializer task exited..." fetch err) surface
            // instead.
            let _diag_fetch_wait =
                aube_util::diag::Span::new(aube_util::diag::Category::Install, "phase_fetch_await");
            let fetch_phase_start = std::time::Instant::now();
            let fetch_result = match fetch_handle.await.into_diagnostic()? {
                Ok(v) => v,
                Err(e) => {
                    // Fetch failed → install is aborting, no build script
                    // will run. Dropping `fresh_osv_set` aborts the
                    // in-flight OSV probe so it doesn't keep doing
                    // network I/O after the CLI has errored.
                    drop(fresh_osv_set);
                    return Err(combine_install_pipeline_errors(materialize_handle, e).await);
                }
            };
            // Gate: consume the OSV verdict that ran concurrently with
            // the download tail. Strictly before link + finalize, so a
            // `MAL-*` finding aborts (via `?`) before any dependency
            // build / lifecycle script can execute. Posture unchanged —
            // only the `await` point moved past the fetch tail. The
            // `join_next()` Option is `Some` (we spawned exactly one
            // task); inner `?` surfaces an OSV finding / required-check
            // failure, outer `?` a task-join panic.
            osv_gate_active = match fresh_osv_set.join_next().await {
                Some(joined) => joined.into_diagnostic()??,
                None => unreachable!("OSV JoinSet had exactly one spawned task"),
            };
            let (canonical_indices, mut cached, mut fetched) = fetch_result;
            tracing::debug!(
                "phase:fetch {:.1?} ({fetched} packages, {cached} cached)",
                fetch_phase_start.elapsed()
            );
            phase_timings.record("fetch", fetch_phase_start.elapsed());
            drop(_diag_fetch_wait);
            aube_util::diag::instant(aube_util::diag::Category::Install, "fetch_await_end", None);
            // Drain the materializer; its stats get rolled into the
            // final link stats below. Errors abort the install just like
            // a failing link phase would.
            let _diag_mat_wait = aube_util::diag::Span::new(
                aube_util::diag::Category::Install,
                "phase_materialize_await",
            );
            let (prewarm_stats, prewarm_hashes_from_task) =
                materialize_handle.await.into_diagnostic()??;
            drop(_diag_mat_wait);
            aube_util::diag::instant(
                aube_util::diag::Category::Install,
                "materialize_await_end",
                None,
            );
            prewarm_graph_hashes = prewarm_hashes_from_task;
            tracing::debug!(
                "phase:prewarm-gvs {:.1?} ({} packages, {} files)",
                materialize_phase_start.elapsed(),
                prewarm_stats.packages_linked,
                prewarm_stats.files_linked,
            );
            phase_timings.record("prewarm_gvs", materialize_phase_start.elapsed());

            // The fetch coordinator streamed `ResolvedPackage`s from the
            // resolver's *first pass*, which uses canonical `name@version`
            // dep_paths. After the resolver's peer-context post-pass, the
            // graph has contextualized dep_paths — same underlying files,
            // but the indices map needs to be re-keyed to match so the
            // linker can find each variant by the dep_path on its
            // `LockedPackage`. Multiple contextualized variants of the
            // same canonical package share a single set of files, so
            // cloning the PackageIndex is cheap relative to re-extraction.
            let mut indices = remap_indices_to_contextualized(&canonical_indices, &graph);

            // Write the lockfile in whatever format the project was
            // already using. If no lockfile existed, create aube's
            // default `aube-lock.yaml`. Skipped entirely when
            // `lockfile=false`.
            if lockfile_enabled {
                // When `lockfileIncludeTarballUrl=true`, record the
                // registry tarball URL on every registry-sourced
                // package so the writer can embed it in
                // `resolution.tarball:`. The client's `tarball_url`
                // helper honors per-scope registry overrides read
                // from `.npmrc`, so a `@mycorp:registry=...` override
                // still routes scoped packages through the right host.
                // Non-registry packages (local_source Some) already
                // carry their own URL and are left alone.
                if lockfile_include_tarball_url {
                    graph.settings.lockfile_include_tarball_url = true;
                    for pkg in graph.packages.values_mut() {
                        if pkg.local_source.is_some() {
                            continue;
                        }
                        // Preserve any URL already present — the npm
                        // lockfile reader stashes the `resolved:` URL
                        // for aliased entries at parse time because
                        // `(alias, version)` doesn't resolve against
                        // the registry.
                        if pkg.tarball_url.is_none() {
                            pkg.tarball_url = Some(
                                post_fetch_client.tarball_url(pkg.registry_name(), &pkg.version),
                            );
                        }
                    }
                }
                let write_kind = source_kind_before
                    .unwrap_or_else(|| super::default_lockfile_kind(&settings_ctx));
                // Record/refresh the devEngines runtime pin before the
                // graph hits disk (pnpm 10.14+ parity).
                crate::runtime::refresh_lockfile_pin(
                    &mut graph,
                    &manifest,
                    crate::runtime::RuntimeSettings::from_ctx(&settings_ctx),
                    write_kind,
                )
                .await?;
                // Record pnpm's config checksums (pnpm-lock.yaml only) so
                // the written lockfile carries the same drift markers pnpm
                // would. Resolve the local pnpmfile here where `opts` /
                // `ws_config_shared` live; the helper skips non-pnpm formats.
                let local_pnpmfile = if opts.ignore_pnpmfile {
                    None
                } else {
                    crate::pnpmfile::detect(
                        &cwd,
                        opts.pnpmfile.as_deref(),
                        ws_config_shared.pnpmfile_path.as_deref(),
                    )
                };
                settings::stamp_pnpm_config_checksums(
                    &mut graph,
                    write_kind,
                    &manifest,
                    &settings_ctx,
                    local_pnpmfile.as_deref(),
                )
                .await;
                // Annotate the full (pre-host-filter) graph with pnpm-parity
                // snapshot metadata (`optional: true`, `transitivePeerDependencies`)
                // before the write and before the host-only `filter_graph` below.
                crate::commands::prepare_resolved_graph_for_lockfile_write(&mut graph);
                // pnpm persists a top-level `time:` block only under
                // `resolution-mode=time-based`; in every other mode the
                // lockfile stays `time:`-free even when the resolver kept
                // publish times in memory for `minimumReleaseAge` /
                // `trustPolicy` / the `defaultTrust` floor. Strip them on
                // the writer's view (a clone) WITHOUT mutating the shared
                // `graph` — the floor clones `graph` further down and
                // still needs `graph.times`.
                let persist_times = settings::resolve_resolution_mode(&settings_ctx)
                    == aube_resolver::ResolutionMode::TimeBased;
                let write_graph = lockfile_dir::lockfile_graph_for_write(&graph, persist_times);
                if shared_workspace_lockfile || !has_workspace {
                    let written_path = write_lockfile_dir_remapped(
                        &lockfile_dir,
                        &lockfile_importer_key,
                        &write_graph,
                        &manifest,
                        write_kind,
                    )
                    .into_diagnostic()
                    .wrap_err("failed to write lockfile")?;
                    // Log the basename (matches the format resolve.bats and
                    // similar tests assert against — e.g. "Wrote aube-lock.yaml").
                    tracing::debug!(
                        "Wrote {}",
                        written_path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| written_path.display().to_string())
                    );
                } else {
                    write_per_project_lockfiles(
                        &cwd,
                        &write_graph,
                        &manifests,
                        write_kind,
                        per_project_write_selection.as_ref(),
                    )?;
                }
            } else {
                tracing::debug!("lockfile=false: skipping lockfile write");
            }

            // Trim the in-memory graph down to host-installable optionals
            // before it reaches the linker. When the resolver widened its
            // platform filter for aube-lock.yaml, the graph (and now the
            // lockfile) carries native packages for every major platform;
            // `node_modules` must still only get the host's. Mirrors the
            // filter pass the lockfile-happy branch above runs against a
            // parsed lockfile. A no-op when the manifest didn't trigger
            // widening (graph was already host-only).
            let (sup_os, sup_cpu, sup_libc) = settings::effective_supported_architectures(
                &manifest,
                &ws_config_shared,
                &settings_ctx,
            );
            let install_supported_architectures = aube_resolver::SupportedArchitectures {
                os: sup_os,
                cpu: sup_cpu,
                libc: sup_libc,
                ..Default::default()
            };
            let install_ignored_optional = aube_manifest::effective_ignored_optional_dependencies(
                &manifest,
                &ws_config_shared,
            );
            aube_resolver::platform::filter_graph(
                &mut graph,
                &install_supported_architectures,
                &install_ignored_optional,
            );

            // Reconcile the progress denominator and the running
            // estimated-download total. The streaming pass bumped
            // `inc_total` once per *resolved* package and recorded
            // each `unpacked_size`; `filter_graph` just dropped the
            // platform-mismatched optionals, so both totals overcount
            // by the culled entries (the historical "stays at 90%"
            // and over-inflated `~X MB` segments). Resetting against
            // the surviving graph produces a stable cur/total ratio
            // and a size estimate that reflects only what will
            // actually install.
            if let Some(p) = prog_ref {
                p.set_total(graph.packages.len());
                p.reconcile_estimated_bytes(graph.packages.keys());
            }

            // Catch-up fetch: the streaming coordinator deferred
            // platform-mismatched registry tarballs on the assumption
            // `filter_graph` would drop them. Anything still in
            // `graph.packages` without a store index is a survivor
            // (i.e. reached via a non-optional edge) and needs its
            // tarball before the linker runs. In practice this set is
            // usually empty: platform-constrained packages are almost
            // always `optionalDependencies`, and `filter_graph` culls
            // those. The rare non-empty case is a broken package that
            // declares `os`/`cpu` without marking itself optional — we
            // still install it with a warning, matching pnpm's
            // `packageIsInstallable` behavior.
            let missing_packages: BTreeMap<String, aube_lockfile::LockedPackage> = graph
                .packages
                .iter()
                // Only non-local registry tarballs are ever deferred by
                // the streaming platform-skip above (it fires solely for
                // `local_source.is_none()`), so the catch-up must scope to
                // those. Local `file:`/`link:` deps already ran their
                // `import_local_source` + `inc_reused` up front; link-only
                // deps legitimately leave no `indices` entry, so a plain
                // `!indices.contains_key` filter would re-import them and
                // double-credit `reused` (reused > resolved →
                // WARN_AUBE_PROGRESS_OVERFLOW).
                .filter(|(dep_path, pkg)| {
                    !indices.contains_key(*dep_path) && pkg.local_source.is_none()
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !missing_packages.is_empty() {
                tracing::debug!(
                    "catch-up fetch for {} package(s) deferred by the streaming filter but kept by filter_graph",
                    missing_packages.len()
                );
                let catchup_start = std::time::Instant::now();
                let cwd_for_catchup_client = cwd.clone();
                let catchup_network_mode = opts.network_mode;
                let (catchup_indices, catchup_cached, catchup_fetched) = fetch_packages_with_root(
                    &missing_packages,
                    &store,
                    || {
                        std::sync::Arc::new(
                            make_client(&cwd_for_catchup_client)
                                .with_network_mode(catchup_network_mode),
                        )
                    },
                    prog_ref,
                    &cwd,
                    &aube_dir,
                    /*materialize_tx=*/ None,
                    // Same warm-store-verify gate as the primary fetch
                    // above: re-enable the workspace `AlreadyLinked`
                    // shortcut only under fast-trust; default keeps the
                    // upstream `has_workspace` value.
                    /*skip_already_linked_shortcut=*/
                    has_workspace && warm_store_verify(),
                    virtual_store_dir_max_length,
                    opts.ignore_scripts,
                    network_concurrency_setting,
                    verify_store_integrity_setting,
                    strict_store_integrity_setting,
                    strict_store_pkg_content_check_setting,
                    opts.git_prepare_depth,
                    inherited_build_policy_for_git_prepare.clone(),
                    resolve_git_shallow_hosts(&settings_ctx),
                )
                .await?;
                indices.extend(catchup_indices);
                cached += catchup_cached;
                fetched += catchup_fetched;
                phase_timings.record("catchup_fetch", catchup_start.elapsed());
            }

            (graph, indices, cached, fetched)
        }
        Err(aube_lockfile::Error::NotFound(_)) => {
            // Reachable when mode == Frozen, strict_no_lockfile == true,
            // and no lockfile is on disk. Today that's `aube ci` /
            // `aube clean-install`, which match `npm ci` semantics.
            return Err(miette!(
                "no lockfile found and --frozen-lockfile is set\n\
                 help: commit pnpm-lock.yaml to your repository, or run \
                 `{} --no-frozen-lockfile` to generate one",
                aube_util::cmd("install")
            ));
        }
        Err(e) => {
            return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile");
        }
    };

    tracing::debug!("Packages: {cached_count} cached, {fetch_count} fetched");

    // `cleanupUnusedCatalogs` (gated by the setting) rewrites
    // `aube-workspace.yaml` / `pnpm-workspace.yaml` to drop entries no
    // importer references. Runs once after we have the final graph so
    // the same helper covers both lockfile-read and fresh-resolve
    // paths (the `--lockfile-only` short-circuit above already handled
    // its own return). Pruning is independent of the lockfile write
    // below since the resolver already recorded the used subset in
    // `graph.catalogs`.
    maybe_cleanup_unused_catalogs(&cwd, &settings_ctx, &workspace_catalogs, &graph.catalogs)?;

    // 5a. Under `strict-peer-dependencies=true`, scan the resolved
    //     graph for unmet required peers and fail the install with the
    //     list. Default (strict=false) is silent, matching bun/npm/yarn
    //     — the previous pnpm-style warn-on-every-mismatch default
    //     produced a lot of noise on real-world trees and buried the
    //     genuinely actionable ones. Optional peers
    //     (peerDependenciesMeta.optional) are skipped either way, and
    //     `peerDependencyRules` escape hatches filter out matches
    //     before the strict check fires.
    //
    //     The `PeerDependencyRules::resolve` call is gated on strict
    //     because it reads across package.json / .npmrc /
    //     pnpm-workspace.yaml to build the three escape-hatch lists —
    //     allocation + file-source iteration nobody consumes on the
    //     silent default path.
    if resolve_strict_peer_dependencies(&settings_ctx) {
        let peer_rules = PeerDependencyRules::resolve(&manifest, &settings_ctx);
        check_unmet_peers(&graph, &peer_rules)?;
    }

    // 5b. Apply --prod / --dev / --no-optional filters. Drops the corresponding
    //     direct dep roots from every importer and prunes transitive packages
    //     only reachable through them. The filtered graph is what gets passed
    //     to the linker, so node_modules won't contain the excluded deps.
    //     The lockfile on disk is untouched.
    let mut graph_for_link = if opts.dep_selection.is_filtered() {
        let before = graph.packages.len();
        let sel = opts.dep_selection;
        let filtered = graph.filter_deps(|d| {
            if sel.prod_only() && d.dep_type == aube_lockfile::DepType::Dev {
                return false;
            }
            if sel.dev_only() && d.dep_type != aube_lockfile::DepType::Dev {
                return false;
            }
            if sel.skip_optional() && d.dep_type == aube_lockfile::DepType::Optional {
                return false;
            }
            true
        });
        let dropped = before - filtered.packages.len();
        if dropped > 0 {
            tracing::debug!("{}: skipping {dropped} packages", sel.label());
        }
        filtered
    } else {
        graph.clone()
    };
    if !opts.workspace_filter.is_empty() {
        graph_for_link = filter_graph_to_workspace_selection(
            &cwd,
            &workspace_packages,
            &graph_for_link,
            &opts.workspace_filter,
        )?;
    } else if has_workspace && !link_all_workspace_importers {
        graph_for_link = filter_graph_to_importers(&graph_for_link, ["."]);
    }

    // 5c. Validate root + dependency `engines.node` constraints against
    //     the current Node version. Runs against `graph_for_link` so
    //     `--prod` / `--no-optional` excluded packages don't trip
    //     `engine-strict`: a dev-only dep pinning Node >=20 should not
    //     block a Node 18 production install. Defaults to warning on
    //     mismatch; fails the install when `engine-strict` is set in
    //     `.npmrc`. Packages with unparseable versions or ranges are
    //     treated as "no opinion" so malformed fields or unusual Node
    //     builds don't block installs.
    // 5c. Resolve node version, build policy, and validate engines.
    //     All three go through the `settings_ctx` loaded once at the
    //     top of `run`, so there's a single `.npmrc` read and a
    //     single workspace-yaml parse for the whole install.
    let engine_strict = aube_settings::resolved::engine_strict(&settings_ctx);
    // `childConcurrency` caps how many dep lifecycle scripts run in
    // parallel during the post-link allowBuilds phase. Matches pnpm's
    // default of 5 when unset. Zero gets clamped up to 1 inside
    // `run_dep_lifecycle_scripts` so a malformed config can't wedge
    // the install.
    let child_concurrency = aube_settings::resolved::child_concurrency(&settings_ctx) as usize;
    let (jail_policy, jail_policy_warnings) =
        JailBuildPolicy::from_settings(&settings_ctx, &ws_config_shared);
    let node_version_override = aube_settings::resolved::node_version(&settings_ctx);
    let node_version = crate::engines::effective_node_version(node_version_override.as_deref());
    crate::engines::run_checks(
        &aube_dir,
        &manifest,
        &manifests,
        &graph_for_link,
        &package_indices,
        node_version.as_deref(),
        engine_strict,
        virtual_store_dir_max_length,
        aube_util::embedder().self_engines_check,
    )?;

    // Emit policy-config warnings regardless of `--ignore-scripts`.
    // User wants to know about typos in `allowBuilds` even if scripts
    // will not run, otherwise they reenable scripts later and wonder
    // why nothing runs. Bar is active here (set_phase=linking comes
    // soon, set_phase=fetching already ran). Raw eprintln smears
    // output across bar frames. Route through safe_eprintln which
    // pauses the bar and holds the terminal lock for atomic output.
    for w in &policy_warnings {
        crate::progress::safe_eprintln(&format!("warn: {w}"));
    }
    for w in &jail_policy_warnings {
        crate::progress::safe_eprintln(&format!("warn: {w}"));
    }

    // Built here (before linking) — the link phase needs it too: the
    // `defaultTrust` floor can authorize a package's build scripts even
    // with no explicit allow rule, and those scripts must see their own
    // deps' bins on PATH, so the per-dep `.bin` linking pass
    // (`link_dep_bins`) has to fire whenever scripts *might* run, not
    // only when an allow rule exists. Same gate the lifecycle phase
    // uses in `finalize.rs`. `from_settings` is pure (reads settings,
    // no I/O), so constructing it early is free.
    let default_trust_floor = default_trust::DefaultTrustFloor::from_settings(
        &settings_ctx,
        opts.minimum_release_age_override,
        osv_gate_active,
        lockfile_vetted,
    );
    let link::LinkPhaseOutput {
        stats,
        node_linker,
        virtual_store_only,
        current_leaf_hashes,
        current_subtree_hashes,
        patch_hashes,
    } = link::run_link_phase(link::LinkPhaseInput {
        cwd: &cwd,
        settings_ctx: &settings_ctx,
        store: store.as_ref(),
        graph_for_link: &graph_for_link,
        package_indices: &package_indices,
        ws_dirs: &ws_dirs,
        manifests: &manifests,
        manifest: &manifest,
        build_policy: &build_policy,
        node_version: node_version.as_deref(),
        prewarm_graph_hashes: prewarm_graph_hashes.as_ref(),
        aube_dir: &aube_dir,
        modules_dir_name: &modules_dir_name,
        virtual_store_dir_max_length,
        link_concurrency_setting,
        use_global_virtual_store_override,
        planned_gvs,
        has_workspace,
        dep_selection_filtered: opts.dep_selection.is_filtered(),
        workspace_filter_empty: opts.workspace_filter.is_empty(),
        ignore_scripts: opts.ignore_scripts,
        floor_may_allow_any: default_trust_floor.may_allow_any(),
        prog_ref,
        phase_timings: &mut phase_timings,
    })?;
    finalize::run_finalize_phase(finalize::FinalizePhaseInput {
        cwd: &cwd,
        settings_ctx: &settings_ctx,
        store: store.as_ref(),
        graph: &graph,
        graph_for_link: &graph_for_link,
        manifests: &manifests,
        lifecycle_manifests: &lifecycle_manifests,
        direct_dep_info: &direct_dep_info,
        deprecations: &deprecations,
        build_policy: &build_policy,
        default_trust_floor: &default_trust_floor,
        jail_policy: &jail_policy,
        stats: &stats,
        node_linker,
        virtual_store_only,
        current_leaf_hashes,
        current_subtree_hashes,
        patch_hashes,
        modules_dir_name: &modules_dir_name,
        aube_dir: &aube_dir,
        virtual_store_dir_max_length,
        child_concurrency,
        side_effects_cache_setting,
        side_effects_cache_readonly_setting,
        strict_dep_builds_setting,
        ignore_scripts: opts.ignore_scripts,
        skip_root_lifecycle: opts.skip_root_lifecycle,
        workspace_filter_empty: opts.workspace_filter.is_empty(),
        dep_selection: opts.dep_selection,
        cli_flags: &opts.cli_flags,
        cached_count,
        fetch_count,
        start,
        prog_ref,
        phase_timings: &mut phase_timings,
    })
    .await?;
    Ok(())
}

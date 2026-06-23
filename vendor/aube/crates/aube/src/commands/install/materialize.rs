use miette::{Context, IntoDiagnostic, miette};

// Inputs for the GVS-prewarm materializer task. Built once before
// fetch starts, moved into spawned task.
#[allow(clippy::too_many_arguments)]
pub(super) struct GvsPrewarmInputs {
    pub graph: std::sync::Arc<aube_lockfile::LockfileGraph>,
    pub store: std::sync::Arc<aube_store::Store>,
    pub cwd: std::path::PathBuf,
    pub virtual_store_dir_max_length: usize,
    pub link_strategy: aube_linker::LinkStrategy,
    pub link_concurrency: Option<usize>,
    pub patches: aube_linker::Patches,
    pub patch_hashes: std::collections::BTreeMap<String, String>,
    pub node_version: Option<String>,
    pub build_policy: std::sync::Arc<aube_scripts::BuildPolicy>,
    pub use_global_virtual_store_override: Option<bool>,
    /// The RESOLVED per-project virtual store dir (the same value the
    /// link phase applies via `with_aube_dir_override`). Threaded here
    /// so the non-GVS streaming materializer lands in the directory the
    /// user (or embedder) configured via `virtualStoreDir` — deriving it
    /// from the probe linker's built-in `<modulesDir>/.aube` fallback
    /// instead materialized a SECOND store next to the configured one.
    pub virtual_store_dir: std::path::PathBuf,
}

/// Initial capacity for the (canonical_key, PackageIndex) channel
/// that feeds the GVS-prewarm materializer. Bounded so RSS on a
/// huge graph stays sane while a slow filesystem (Defender,
/// network share) backs up the materializer; backpressure only
/// kicks in under real producer/consumer skew.
///
/// Tokio mpsc capacity is fixed at construction, so a bigger
/// learned-from-prior-run value couldn't be applied to the
/// current channel anyway. A static cap keeps the construction
/// path obvious without dragging cross-run telemetry through the
/// hot send/recv loops for marginal gain.
pub(super) const MATERIALIZE_CHANNEL_CAPACITY: usize = 2048;

pub(super) type MaterializeChannel = (
    tokio::sync::mpsc::Sender<(String, aube_store::PackageIndex)>,
    tokio::sync::mpsc::Receiver<(String, aube_store::PackageIndex)>,
);

pub(super) type MaterializeJoinHandle = tokio::task::JoinHandle<
    miette::Result<(
        aube_linker::LinkStats,
        Option<std::sync::Arc<aube_lockfile::graph_hash::GraphHashes>>,
    )>,
>;

pub(super) fn materialize_channel() -> MaterializeChannel {
    let (tx, rx) = tokio::sync::mpsc::channel(MATERIALIZE_CHANNEL_CAPACITY);
    aube_util::diag::register_channel("materialize", &tx, MATERIALIZE_CHANNEL_CAPACITY);
    (tx, rx)
}

/// Spawn the GVS-prewarm consumer with the given inputs and rx.
/// Centralizes the JoinHandle type + tokio::spawn boilerplate.
pub(super) fn spawn_gvs_prewarm(
    inputs: GvsPrewarmInputs,
    rx: tokio::sync::mpsc::Receiver<(String, aube_store::PackageIndex)>,
) -> MaterializeJoinHandle {
    tokio::spawn(run_gvs_prewarm_materializer(inputs, rx))
}

/// When the fetch task fails the surfaced error is often just
/// "materializer task exited before fetch finished" — a symptom of the
/// materializer dying first (its `rx` was dropped, fetch's `tx.send`
/// returned Err). Await the materializer (don't abort) so its real
/// error joins the chain. The returned report shows both errors:
///
/// * top message = how the pipeline aborted (the fetch error)
/// * source chain = why the materializer task failed (root cause)
///
/// If the materializer succeeded the fetch error was the real one and
/// is returned unchanged.
pub(super) async fn combine_install_pipeline_errors(
    materialize_handle: MaterializeJoinHandle,
    fetch_err: miette::Report,
) -> miette::Report {
    let mat_err = match materialize_handle.await {
        Ok(Ok(_)) => return fetch_err,
        Ok(Err(err)) => err,
        Err(join_err) => Err::<(), _>(join_err)
            .into_diagnostic()
            .unwrap_err()
            .wrap_err("materializer task panicked"),
    };
    mat_err.wrap_err(format!("install aborted: {fetch_err}"))
}

// Consumes (canonical_key, index) from rx as tarballs land in the CAS,
// reflinks each into the global virtual store. Returns aggregate
// LinkStats plus computed graph hashes so the caller's link phase
// reuses them. Hides 30-200ms of GVS reflinks behind the in-flight
// download tail. Non-GVS installs drain rx as a no-op consumer.
pub(super) async fn run_gvs_prewarm_materializer(
    inputs: GvsPrewarmInputs,
    materialize_rx: tokio::sync::mpsc::Receiver<(String, aube_store::PackageIndex)>,
) -> miette::Result<(
    aube_linker::LinkStats,
    Option<std::sync::Arc<aube_lockfile::graph_hash::GraphHashes>>,
)> {
    let GvsPrewarmInputs {
        graph,
        store,
        cwd,
        virtual_store_dir_max_length,
        link_strategy,
        link_concurrency,
        patches,
        patch_hashes,
        node_version,
        build_policy,
        use_global_virtual_store_override,
        virtual_store_dir,
    } = inputs;

    let engine = node_version
        .as_deref()
        .map(aube_lockfile::graph_hash::engine_name_default);

    // Build a probe linker without graph_hashes to check GVS mode
    // first. compute_graph_hashes_with_patches walks every package
    // BLAKE3-style, expensive on huge graphs. Skip it when GVS is
    // off so per-project installs and cold CI (CI=true gates GVS)
    // don't pay for hashes nothing reads.
    let mut probe = aube_linker::Linker::new(store.as_ref(), link_strategy)
        .with_virtual_store_dir_max_length(virtual_store_dir_max_length)
        .with_aube_dir_override(virtual_store_dir);
    if let Some(enabled) = use_global_virtual_store_override {
        probe = probe.with_use_global_virtual_store(enabled);
    }
    if !probe.uses_global_virtual_store() {
        return run_aube_dir_materializer(probe, graph, cwd, link_concurrency, materialize_rx)
            .await;
    }

    // Hash compute walks every package BLAKE3-style. spawn_blocking
    // pushes it off the tokio worker so the canonical_to_contextualized
    // build below + nested_link_targets walk + first materialize_rx
    // recv keep making progress in parallel. compute_graph_hashes_with_patches
    // is CPU-bound and was previously blocking the executor.
    let graph_for_hash = graph.clone();
    let build_policy_for_hash = build_policy.clone();
    let engine_for_hash = engine.clone();
    let patch_hashes_for_hash = patch_hashes.clone();
    aube_util::diag::instant(
        aube_util::diag::Category::Materialize,
        "hash_compute_spawn",
        None,
    );
    let hash_handle = tokio::task::spawn_blocking(move || {
        let _diag = aube_util::diag::Span::new(
            aube_util::diag::Category::Materialize,
            "graph_hash_compute",
        );
        let allow = |pkg: &aube_lockfile::LockedPackage| {
            matches!(
                build_policy_for_hash.decide_package(
                    pkg.registry_name(),
                    &pkg.version,
                    pkg.source_approval_key().as_deref(),
                ),
                aube_scripts::AllowDecision::Allow
            )
        };
        let patch_hash_fn = |name: &str, version: &str| -> Option<String> {
            let key = format!("{name}@{version}");
            patch_hashes_for_hash.get(&key).cloned()
        };
        aube_lockfile::graph_hash::compute_graph_hashes_with_patches(
            &graph_for_hash,
            &allow,
            engine_for_hash.as_ref(),
            &patch_hash_fn,
        )
    });

    let nested_link_targets =
        aube_linker::build_nested_link_targets(&cwd, &graph).map(std::sync::Arc::new);

    // Channel emits `pkg.dep_path` (canonical on resolver first-pass,
    // contextualized on post-pass). When the received key is canonical
    // and fans out to one-or-more peer-contextualized variants in the
    // graph, this map points canonical -> {contextualized}. Identity
    // entries (canonical == dep_path) skip the map and fall back to a
    // direct graph lookup at receive time.
    let mut canonical_to_contextualized: aube_util::collections::FxMap<
        String,
        aube_util::collections::FxSet<String>,
    > = aube_util::collections::FxMap::default();
    for (dep_path, pkg) in &graph.packages {
        if pkg.local_source.is_some() {
            continue;
        }
        let canonical = pkg.spec_key();
        if canonical != *dep_path {
            canonical_to_contextualized
                .entry(canonical)
                .or_default()
                .insert(dep_path.clone());
        }
    }

    // Packages whose graph hash folds in a content fingerprint — every
    // globally-shareable source dep (git / remote tarball) plus all of
    // its transitive ancestors. Prewarm runs before fetch and so hashes
    // them content-less, while the link phase hashes them content-ful;
    // materializing them here would strand a duplicate content-less
    // cohort in the global store whose source leaves (skipped at receive
    // time because their trees aren't fetched yet) dangle — a
    // duplicate-singleton "Cannot find module" class of bug. Defer the
    // whole set to the link phase, which materializes every node at its
    // final content-ful path.
    let content_affected = aube_lockfile::graph_hash::content_affected_dep_paths(&graph);

    let _diag_hash_wait =
        aube_util::diag::Span::new(aube_util::diag::Category::Materialize, "hash_await");
    let graph_hashes = hash_handle
        .await
        .into_diagnostic()
        .wrap_err("graph_hash compute task failed")?;
    drop(_diag_hash_wait);
    aube_util::diag::instant(
        aube_util::diag::Category::Materialize,
        "drain_rx_begin",
        None,
    );
    let graph_hashes_arc = std::sync::Arc::new(graph_hashes);
    let mut linker = probe.with_graph_hashes((*graph_hashes_arc).clone());
    if !patches.is_empty() {
        linker = linker.with_patches(patches);
    }
    let linker = std::sync::Arc::new(linker);

    /*
     * Adaptive linker parallelism. The signal is the same as the
     * network limiter for ceiling/throttle behavior, but with
     * CUSUM-driven shrink disabled. Per-package
     * `ensure_in_virtual_store` wall on storage-bound workloads
     * has high intrinsic variance (Defender scans, NTFS
     * cold-cache, COW reflink fall-through to copy) that has no
     * upstream queue to relieve — treating rising RTT as
     * backpressure was observed to collapse the limit from seed
     * 16 down to 12 on Windows, queueing 1195 packages behind a
     * 12-permit cap (mean 2456ms permit_wait).
     *
     * `record_throttle` shrink remains active for real IO errors.
     * `link_concurrency` setting is a *seed* (when set); default
     * seed clamps `default_linker_parallelism()` into [16, 48].
     * Floor 8 prevents pathological collapse under throttle
     * cascades; ceiling 64 caps concurrent open file descriptors.
     */
    let permit_seed = link_concurrency.unwrap_or_else(aube_linker::default_linker_parallelism);
    let linker_persistent = aube_util::adaptive::global_persistent_state();
    let sem = match linker_persistent.as_ref() {
        Some(state) => aube_util::adaptive::AdaptiveLimit::from_persistent(
            state,
            "linker_prewarm:default",
            permit_seed.clamp(16, 48),
            8,
            64,
        ),
        None => aube_util::adaptive::AdaptiveLimit::new(permit_seed.clamp(16, 48), 8, 64),
    };
    sem.disable_cusum_shrink();
    let linker_sem_for_persist = std::sync::Arc::clone(&sem);
    let linker_persistent_for_save = linker_persistent.clone();
    let mut in_flight: Vec<tokio::task::JoinHandle<miette::Result<aube_linker::LinkStats>>> =
        Vec::new();
    let mut rx = materialize_rx;
    while let Some((key, index)) = rx.recv().await {
        // canonical_to_contextualized only stores entries where
        // canonical != dep_path. Identity packages (the common case)
        // fall through to a direct graph lookup. Without this fallback
        // the lockfile path — which emits dep_path == canonical for
        // every non-peer-suffixed package — would silently skip
        // materialize for every identity entry.
        let dep_paths: Vec<String> = if let Some(set) = canonical_to_contextualized.get(&key) {
            set.iter().cloned().collect()
        } else if graph.packages.contains_key(&key) {
            vec![key.clone()]
        } else {
            continue;
        };
        let index = std::sync::Arc::new(index);
        for dep_path in dep_paths {
            let Some(pkg) = graph.packages.get(&dep_path).cloned() else {
                continue;
            };
            if pkg.local_source.is_some() {
                continue;
            }
            if content_affected.contains(&dep_path) {
                continue;
            }
            let linker = linker.clone();
            let sem = sem.clone();
            let index = index.clone();
            let nested_link_targets = nested_link_targets.clone();
            in_flight.push(tokio::spawn(async move {
                let _diag_pkg =
                    aube_util::diag::Span::new(aube_util::diag::Category::Materialize, "package")
                        .with_meta_fn(|| {
                            format!(r#"{{"dep_path":{}}}"#, aube_util::diag::jstr(&dep_path))
                        });
                let _diag_pkg_inflight = aube_util::diag::inflight(aube_util::diag::Slot::Imp);
                let permit_wait = std::time::Instant::now();
                let permit = sem.acquire().await;
                let permit_wait_ms = permit_wait.elapsed();
                if permit_wait_ms.as_millis() > 1 {
                    aube_util::diag::event_lazy(
                        aube_util::diag::Category::Materialize,
                        "permit_wait",
                        permit_wait_ms,
                        || format!(r#"{{"dep_path":{}}}"#, aube_util::diag::jstr(&dep_path)),
                    );
                }
                let dep_path_for_err = dep_path.clone();
                let outcome = tokio::task::spawn_blocking(move || -> miette::Result<_> {
                    let _diag_blk = aube_util::diag::Span::new(
                        aube_util::diag::Category::Materialize,
                        "package_blocking",
                    );
                    let mut stats = aube_linker::LinkStats::default();
                    linker
                        .ensure_in_virtual_store(
                            &dep_path,
                            &pkg,
                            &index,
                            &mut stats,
                            nested_link_targets.as_deref(),
                        )
                        .map_err(|e| miette!("prewarm GVS for {dep_path_for_err}: {e}"))?;
                    Ok(stats)
                })
                .await
                .into_diagnostic()?;
                match &outcome {
                    Ok(_) => permit.record_success(),
                    Err(_) => permit.record_cancelled(),
                }
                outcome
            }));
        }
    }
    let mut total = aube_linker::LinkStats::default();
    for handle in in_flight {
        let s = handle.await.into_diagnostic()??;
        total.packages_linked += s.packages_linked;
        total.packages_cached += s.packages_cached;
        total.files_linked += s.files_linked;
    }
    if let Some(state) = linker_persistent_for_save.as_ref() {
        linker_sem_for_persist.persist(state, "linker_prewarm:default");
    }
    Ok((total, Some(graph_hashes_arc)))
}

/// Per-project materializer: pipelines the link work into the fetch
/// phase under non-GVS mode. Each (canonical_key, PackageIndex) the
/// fetch coordinator emits triggers a `materialize_into` against the
/// per-project `.aube/<dep_path>/`, so by the time fetch finishes
/// the dedicated link phase only has to create the top-level
/// `node_modules/<name>` symlinks.
async fn run_aube_dir_materializer(
    linker: aube_linker::Linker,
    graph: std::sync::Arc<aube_lockfile::LockfileGraph>,
    cwd: std::path::PathBuf,
    link_concurrency: Option<usize>,
    materialize_rx: tokio::sync::mpsc::Receiver<(String, aube_store::PackageIndex)>,
) -> miette::Result<(
    aube_linker::LinkStats,
    Option<std::sync::Arc<aube_lockfile::graph_hash::GraphHashes>>,
)> {
    let aube_dir = std::sync::Arc::new(linker.aube_dir_for(&cwd));
    aube_linker::mkdirp(&aube_dir).map_err(|e| miette!("create {}: {e}", aube_dir.display()))?;
    let nested_link_targets =
        aube_linker::build_nested_link_targets(&cwd, &graph).map(std::sync::Arc::new);

    // Channel emits `pkg.dep_path` (canonical on the resolver's
    // first-pass packages, contextualized on post-pass). When the
    // received key is a canonical that fans out to one-or-more
    // peer-contextualized variants in the graph, this map points
    // canonical -> {contextualized dep_paths}. Identity entries
    // (canonical == dep_path) are skipped because the receive loop
    // falls back to a direct graph lookup for those.
    let mut canonical_to_contextualized: aube_util::collections::FxMap<
        String,
        aube_util::collections::FxSet<String>,
    > = aube_util::collections::FxMap::default();
    for (dep_path, pkg) in &graph.packages {
        if pkg.local_source.is_some() {
            continue;
        }
        let canonical = pkg.spec_key();
        if canonical != *dep_path {
            canonical_to_contextualized
                .entry(canonical)
                .or_default()
                .insert(dep_path.clone());
        }
    }

    let linker = std::sync::Arc::new(linker);
    /*
     * Adaptive per-project materialize parallelism. Same gradient
     * controller as the prewarm path, with CUSUM-driven shrink
     * disabled for the same reason: per-package `ensure_in_aube_dir`
     * wall is filesystem-bound (Defender, NTFS cold-cache, COW
     * fall-through) and rising RTT here is intrinsic noise rather
     * than upstream backpressure. `record_throttle` shrink remains
     * active for real IO errors. Floor 8 prevents pathological
     * collapse under throttle cascades; persisted under
     * `linker_per_project:default` so the next process resumes
     * the converged operating point.
     */
    let permit_seed = link_concurrency.unwrap_or_else(aube_linker::default_linker_parallelism);
    let perproj_persistent = aube_util::adaptive::global_persistent_state();
    let sem = match perproj_persistent.as_ref() {
        Some(state) => aube_util::adaptive::AdaptiveLimit::from_persistent(
            state,
            "linker_per_project:default",
            permit_seed.clamp(16, 48),
            8,
            64,
        ),
        None => aube_util::adaptive::AdaptiveLimit::new(permit_seed.clamp(16, 48), 8, 64),
    };
    sem.disable_cusum_shrink();
    let perproj_sem_for_persist = std::sync::Arc::clone(&sem);
    let perproj_persistent_for_save = perproj_persistent.clone();
    // JoinSet aborts in-flight tasks if we early-return on error,
    // so a failed materialize doesn't leave orphan tasks racing
    // disk writes against the install driver's cleanup.
    let mut in_flight: tokio::task::JoinSet<miette::Result<aube_linker::LinkStats>> =
        tokio::task::JoinSet::new();
    let mut total = aube_linker::LinkStats::default();
    let mut rx = materialize_rx;
    while let Some((key, index)) = rx.recv().await {
        // Surface task failures while still receiving so a sustained
        // I/O error aborts before we queue hundreds more.
        while let Some(joined) = in_flight.try_join_next() {
            let s = joined.into_diagnostic()??;
            total.packages_linked += s.packages_linked;
            total.packages_cached += s.packages_cached;
            total.files_linked += s.files_linked;
        }

        let dep_paths: Vec<String> = if let Some(set) = canonical_to_contextualized.get(&key) {
            set.iter().cloned().collect()
        } else if graph.packages.contains_key(&key) {
            vec![key.clone()]
        } else {
            continue;
        };
        let index = std::sync::Arc::new(index);
        for dep_path in dep_paths {
            let Some(pkg) = graph.packages.get(&dep_path).cloned() else {
                continue;
            };
            if pkg.local_source.is_some() {
                continue;
            }
            let linker = linker.clone();
            let sem = sem.clone();
            let index = index.clone();
            let aube_dir = aube_dir.clone();
            let nested_link_targets = nested_link_targets.clone();
            in_flight.spawn(async move {
                let permit = sem.acquire().await;
                let dep_path_for_err = dep_path.clone();
                let outcome = tokio::task::spawn_blocking(move || -> miette::Result<_> {
                    let mut stats = aube_linker::LinkStats::default();
                    linker
                        .ensure_in_aube_dir(
                            &aube_dir,
                            &dep_path,
                            &pkg,
                            &index,
                            &mut stats,
                            nested_link_targets.as_deref(),
                        )
                        .map_err(|e| miette!("materialize {dep_path_for_err}: {e}"))?;
                    Ok(stats)
                })
                .await
                .into_diagnostic()?;
                match &outcome {
                    Ok(_) => permit.record_success(),
                    Err(_) => permit.record_cancelled(),
                }
                outcome
            });
        }
    }
    while let Some(joined) = in_flight.join_next().await {
        let s = joined.into_diagnostic()??;
        total.packages_linked += s.packages_linked;
        total.packages_cached += s.packages_cached;
        total.files_linked += s.files_linked;
    }
    if let Some(state) = perproj_persistent_for_save.as_ref() {
        perproj_sem_for_persist.persist(state, "linker_per_project:default");
    }
    Ok((total, None))
}

#[cfg(test)]
mod combine_pipeline_errors_tests {
    use super::combine_install_pipeline_errors;
    use miette::miette;

    fn fmt_chain(report: &miette::Report) -> String {
        let mut out = report.to_string();
        let mut src = report.source();
        while let Some(e) = src {
            out.push_str(" :: ");
            out.push_str(&e.to_string());
            src = e.source();
        }
        out
    }

    #[tokio::test]
    async fn returns_fetch_err_when_materializer_succeeded() {
        let handle = tokio::spawn(async {
            Ok((
                aube_linker::LinkStats::default(),
                None::<std::sync::Arc<aube_lockfile::graph_hash::GraphHashes>>,
            ))
        });
        let fetch_err = miette!("network down: timed out fetching foo@1.0");
        let combined = combine_install_pipeline_errors(handle, fetch_err).await;
        assert!(
            combined.to_string().contains("network down"),
            "got: {}",
            combined
        );
    }

    #[tokio::test]
    async fn nests_both_errors_when_materializer_failed() {
        let handle = tokio::spawn(async {
            Err::<
                (
                    aube_linker::LinkStats,
                    Option<std::sync::Arc<aube_lockfile::graph_hash::GraphHashes>>,
                ),
                _,
            >(miette!("materialize foo@1.0: permission denied"))
        });
        // The fetch task surfaces the channel-closed symptom.
        let fetch_err = miette!("materializer task exited before fetch finished");
        let combined = combine_install_pipeline_errors(handle, fetch_err).await;
        let chain = fmt_chain(&combined);
        // Both errors visible: fetch's symptom on top, materializer's
        // root cause in the chain below.
        assert!(
            chain.contains("materializer task exited before fetch finished"),
            "fetch err missing from chain: {chain}"
        );
        assert!(
            chain.contains("permission denied"),
            "materializer err missing from chain: {chain}"
        );
    }
}

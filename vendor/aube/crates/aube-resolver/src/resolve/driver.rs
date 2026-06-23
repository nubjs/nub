//! BFS resolution driver.
//!
//! Bundles the per-resolution accumulators that used to live as
//! function-local variables in `Resolver::resolve_workspace`. The
//! driver borrows the parent `Resolver` for its mutable cache + read
//! hook plus all read-only configuration, while owning the state that
//! gets walked / mutated during the BFS itself: the queue, the
//! `resolved` graph, the time-based cutoff bookkeeping, the catalog
//! picks, the fetch scheduler, etc.
//!
//! Splitting this out turns `resolve_workspace` from a 2,000-line
//! function-with-macros into a thin orchestrator that constructs a
//! `ResolveDriver` and calls `run`. The per-task body still lives in
//! one method (`process_task`) for now — the per-branch dispatch
//! inside it (preprocess → local-source → workspace-link →
//! sibling-dedupe → lockfile-reuse → fetch-and-pick) is staged for a
//! later refactor.

use super::fetch::FetchScheduler;
use super::seed::seed_direct_deps;
use super::vulnerable::{is_vulnerable, prefer_non_vulnerable_pick};
use crate::local_source::{
    dep_path_for, is_non_registry_specifier, read_local_manifest, rebase_local,
    resolve_exec_manifest, resolve_git_source, resolve_remote_tarball, should_block_exotic_subdep,
};
use crate::package_ext::{
    apply_package_extensions, apply_package_extensions_to_deps, pick_override_spec,
};
use crate::semver_util::{PickResult, Regime, classify_regime, pick_version, version_satisfies};
use crate::{
    Error, ExoticSubdepDetails, FxHashMap, FxHashSet, ResolutionMode, ResolveTask, ResolvedPackage,
    Resolver, error, is_deprecation_allowed, is_supported,
};
use aube_lockfile::{
    DepType, DirectDep, LocalSource, LockedPackage, LockfileGraph, git_commits_match,
};
use aube_manifest::PackageJson;
use aube_util::adaptive::{AdaptiveLimit, PersistentState};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) struct ResolveDriver<'a> {
    resolver: &'a mut Resolver,
    existing: Option<&'a LockfileGraph>,
    workspace_packages: &'a HashMap<String, String>,

    /// Borrowed set of names present in the existing lockfile. Used as
    /// a prefetch gate so packuments that will hit the lockfile-reuse
    /// path never burn a tokio spawn. Strictly an optimization — the
    /// wait-for-fetch loop calls `ensure_fetch` unconditionally.
    existing_names: FxHashSet<&'a str>,
    /// Per-importer set of declared dep names (`dependencies` ∪
    /// `devDependencies` ∪ `optionalDependencies` ∪ synthesized
    /// non-optional `peerDependencies`). Consulted by the peer-dep
    /// enqueue path to suppress `auto-install-peers` when the importer
    /// (or root, under `resolve-peers-from-workspace-root`) has already
    /// declared the peer.
    importer_declared_dep_names: BTreeMap<String, BTreeSet<String>>,

    /// Locked packages keyed by dep_path. The output graph's packages
    /// field.
    resolved: BTreeMap<String, LockedPackage>,
    /// `name → resolved versions seen this run`. Drives sibling dedupe.
    /// Pre-sized for typical monorepo (5000-dep graphs take one grow).
    resolved_versions: FxHashMap<String, Vec<String>>,
    /// Importer paths → direct dep records, populated as root tasks
    /// resolve.
    importers: BTreeMap<String, Vec<DirectDep>>,
    /// BFS queue of pending tasks. Seeded with the direct deps from
    /// every importer; the per-task body pushes transitives as it
    /// resolves.
    queue: VecDeque<ResolveTask>,
    /// dep_paths whose package has been written into `resolved` and
    /// whose transitives have been enqueued. The set holds `Arc<str>`
    /// so frequent re-checks against the same dep_path don't trigger
    /// allocations.
    visited: FxHashSet<Arc<str>>,
    /// Round-tripped to the lockfile's top-level `time:` block.
    /// Populated opportunistically from whatever packuments we fetch:
    /// empty when the metadata omits `time` (corgi from npmjs.org in
    /// default mode), filled otherwise.
    resolved_times: BTreeMap<String, String>,
    /// Per-importer record of optionals dropped on this run (platform
    /// mismatch or `pnpm.ignoredOptionalDependencies`).
    skipped_optional_dependencies: BTreeMap<String, BTreeMap<String, String>>,
    /// Packument fetches that failed (registry 404, network error, etc.),
    /// keyed by package name. Errors are stored here instead of
    /// propagated from `join_next` so a failed fetch for one package
    /// doesn't crash the wrong task. Checked after the fetch-wait loop
    /// to decide skip (optional) vs propagate (required).
    failed_fetches: FxHashMap<String, Error>,
    /// Catalog picks gathered as the BFS rewrites `catalog:` task
    /// ranges. Outer key: catalog name. Inner: package name → spec.
    catalog_picks: BTreeMap<String, BTreeMap<String, String>>,
    /// Transitives parked while the TimeBased cutoff is still pending
    /// (i.e. wave 0 of direct deps hasn't finished). Re-enqueued in
    /// FIFO order once the cutoff fires.
    deferred_transitives: Vec<ResolveTask>,

    /// ISO-8601 UTC cutoff string used by the version picker. Seeded
    /// from `minimum_release_age` for supply-chain mitigation;
    /// extended by the TimeBased cutoff once wave 0 resolves.
    published_by: Option<String>,
    /// Direct deps still awaiting a terminal outcome (resolved,
    /// dropped, or filtered). Drops to zero once wave 0 completes and
    /// triggers the TimeBased cutoff computation.
    direct_deps_pending: usize,
    /// True when the TimeBased cutoff hasn't been computed yet. While
    /// true, transitives that reach the version-pick step are parked
    /// in `deferred_transitives`.
    cutoff_pending: bool,
    /// True when the resolver needs the packument's `time:` map (and
    /// must therefore take the full-packument path).
    needs_time: bool,

    packument_fetch_count: u32,
    packument_fetch_time: Duration,
    lockfile_reuse_count: u32,

    fetcher: FetchScheduler,
    /// (Persistent-state arc, semaphore arc) — the cross-run
    /// concurrency limiter state. `None` when the cross-run store
    /// isn't available; `Some` keeps the handles alive so we can
    /// persist the converged operating point at the end of `run`.
    packument_persist_handle: Option<(Arc<PersistentState>, Arc<AdaptiveLimit>)>,
}

impl<'a> ResolveDriver<'a> {
    pub(crate) fn new(
        resolver: &'a mut Resolver,
        manifests: &[(String, PackageJson)],
        existing: Option<&'a LockfileGraph>,
        workspace_packages: &'a HashMap<String, String>,
    ) -> Self {
        let mut queue: VecDeque<ResolveTask> = VecDeque::with_capacity(512);
        let mut importers: BTreeMap<String, Vec<DirectDep>> = BTreeMap::new();
        let importer_declared_dep_names: BTreeMap<String, BTreeSet<String>> = manifests
            .iter()
            .map(|(importer_path, manifest)| {
                let mut names: BTreeSet<String> = manifest
                    .dependencies
                    .keys()
                    .chain(manifest.dev_dependencies.keys())
                    .chain(manifest.optional_dependencies.keys())
                    .cloned()
                    .collect();
                if resolver.auto_install_peers {
                    names.extend(
                        manifest
                            .non_optional_peer_dependencies()
                            .map(|(name, _)| name.clone()),
                    );
                }
                (importer_path.clone(), names)
            })
            .collect();
        // ISO-8601 UTC cutoff string. npm's registry `time` map uses
        // `Z`-suffixed UTC timestamps throughout, which sort
        // lexicographically — so a raw `String` doubles as a
        // comparable instant without pulling in a date library.
        let published_by: Option<String> = resolver
            .minimum_release_age
            .as_ref()
            .and_then(|m| m.cutoff());
        if let Some(c) = published_by.as_deref() {
            tracing::debug!("minimumReleaseAge cutoff: {}", c);
        }

        seed_direct_deps(
            manifests,
            &resolver.ignored_optional_dependencies,
            resolver.auto_install_peers,
            &mut queue,
            &mut importers,
        );

        // Adaptive packument concurrency. Loaded from the cross-run
        // persistent store when available so the limiter resumes the
        // converged operating point of the previous run instead of
        // cold ramping. Falls back to seed 256 (h2 stream cap) on a
        // fresh install. Floor 4 keeps progress under continuous
        // throttling. User-configured `networkConcurrency` is honored
        // as the seed.
        let packument_seed = resolver.packument_network_concurrency.unwrap_or(256).max(4);
        let packument_max = packument_seed.max(256);
        let persistent = aube_util::adaptive::global_persistent_state();
        let shared_semaphore = match persistent.as_ref() {
            Some(state) => AdaptiveLimit::from_persistent(
                state,
                "packument:default",
                packument_seed,
                4,
                packument_max,
            ),
            None => AdaptiveLimit::new(packument_seed, 4, packument_max),
        };
        let packument_persist_handle = persistent
            .as_ref()
            .map(|p| (Arc::clone(p), Arc::clone(&shared_semaphore)));

        // Time-based mode and `minimumReleaseAge` both need the
        // packument's `time:` map. `registry-supports-time-field=true`
        // lets the cheaper abbreviated path stay on the hot path.
        let needs_time = (resolver.resolution_mode == ResolutionMode::TimeBased
            || resolver.minimum_release_age.is_some()
            || resolver.dependency_policy.trust_policy == crate::TrustPolicy::NoDowngrade)
            && !resolver.registry_supports_time_field;

        let direct_deps_pending = queue.len();
        let cutoff_pending = resolver.resolution_mode == ResolutionMode::TimeBased;
        let fetcher = FetchScheduler::new(resolver, shared_semaphore, needs_time);

        let existing_names: FxHashSet<&'a str> = existing
            .map(|g| g.packages.values().map(|p| p.name.as_str()).collect())
            .unwrap_or_default();

        Self {
            resolver,
            existing,
            workspace_packages,
            existing_names,
            importer_declared_dep_names,
            resolved: BTreeMap::new(),
            resolved_versions: FxHashMap::with_capacity_and_hasher(1024, Default::default()),
            importers,
            queue,
            visited: FxHashSet::with_capacity_and_hasher(2048, Default::default()),
            resolved_times: BTreeMap::new(),
            skipped_optional_dependencies: BTreeMap::new(),
            failed_fetches: FxHashMap::default(),
            catalog_picks: BTreeMap::new(),
            deferred_transitives: Vec::new(),
            published_by,
            direct_deps_pending,
            cutoff_pending,
            needs_time,
            packument_fetch_count: 0,
            packument_fetch_time: Duration::ZERO,
            lockfile_reuse_count: 0,
            fetcher,
            packument_persist_handle,
        }
    }

    pub(crate) async fn run(mut self) -> Result<LockfileGraph, Error> {
        let resolve_start = Instant::now();
        self.seed_initial_prefetches();
        self.bfs_loop().await?;
        self.fetcher.drain().await;

        let resolve_elapsed = resolve_start.elapsed();
        tracing::debug!(
            "resolver: {:.1?} total, {} packuments fetched ({:.1?} wall), {} reused from lockfile, {} packages resolved",
            resolve_elapsed,
            self.packument_fetch_count,
            self.packument_fetch_time,
            self.lockfile_reuse_count,
            self.resolved.len()
        );
        let resolved_count = self.resolved.len();
        let lockfile_reuse_count = self.lockfile_reuse_count;
        let packument_fetch_count = self.packument_fetch_count;
        aube_util::diag::instant_lazy(aube_util::diag::Category::Resolver, "decision_mix", || {
            format!(
                r#"{{"resolved":{},"lockfile_reused":{},"packuments_fetched":{}}}"#,
                resolved_count, lockfile_reuse_count, packument_fetch_count
            )
        });

        let contextualized = self.resolver.finalize_resolved_graph(
            self.importers,
            self.resolved,
            &self.resolved_versions,
            self.resolved_times,
            self.skipped_optional_dependencies,
            self.catalog_picks,
        )?;
        if let Some((state, sem)) = self.packument_persist_handle {
            sem.persist(&state, "packument:default");
        }
        Ok(contextualized)
    }

    /// Fire prefetches for every seeded root dep up front, so their
    /// packuments are already in flight by the time the first task is
    /// popped.
    fn seed_initial_prefetches(&mut self) {
        for task in self.queue.iter() {
            if !self.resolver.is_prefetchable(
                task.name.as_str(),
                task.range.as_str(),
                self.workspace_packages,
            ) {
                continue;
            }
            if self.existing_names.contains(task.name.as_str()) {
                continue;
            }
            if !self.resolver.cache.contains_key(task.name.as_str()) {
                self.fetcher.ensure_fetch(task.name.as_str());
            }
        }
    }

    /// Decrement the pending-directs counter when a root task reaches
    /// a terminal state. Used by the TimeBased cutoff trigger at the
    /// top of the outer loop.
    fn note_root_done(&mut self) {
        if self.direct_deps_pending > 0 {
            self.direct_deps_pending -= 1;
        }
    }

    /// Spawn a packument fetch via the scheduler if one isn't already
    /// running for `name` and the packument isn't already cached.
    ///
    /// Gated *only* on in-flight + cache — callers that want to skip
    /// prefetching names already covered by the lockfile check
    /// `existing_names` explicitly before invoking this.
    fn ensure_fetch(&mut self, name: &str) {
        if !self.resolver.cache.contains_key(name) && !self.failed_fetches.contains_key(name) {
            self.fetcher.ensure_fetch(name);
        }
    }

    /// Outer BFS loop. Pops tasks until the queue drains, with a
    /// TimeBased-cutoff trigger at the top that fires once wave 0
    /// completes.
    async fn bfs_loop(&mut self) -> Result<(), Error> {
        loop {
            // TimeBased cutoff trigger. Fires the first time
            // `direct_deps_pending` hits zero with the cutoff still
            // pending — at which point every direct dep has been
            // version-picked (or terminated in preprocessing),
            // `resolved_times` holds their publish times, and we can
            // derive the max to seed `published_by` for the
            // transitives we deferred.
            if self.cutoff_pending && self.direct_deps_pending == 0 {
                let direct_dep_paths: FxHashSet<&String> = self
                    .importers
                    .values()
                    .flat_map(|deps| deps.iter().map(|d| &d.dep_path))
                    .collect();
                let mut max_time: Option<&String> = None;
                for (dep_path, t) in self.resolved_times.iter() {
                    if !direct_dep_paths.contains(dep_path) {
                        continue;
                    }
                    if max_time.map(|m| t > m).unwrap_or(true) {
                        max_time = Some(t);
                    }
                }
                if let Some(existing_graph) = self.existing {
                    for (dep_path, t) in &existing_graph.times {
                        if !direct_dep_paths.contains(dep_path) {
                            continue;
                        }
                        if max_time.map(|m| t > m).unwrap_or(true) {
                            max_time = Some(t);
                        }
                    }
                }
                if let Some(m) = max_time {
                    tracing::debug!("time-based resolution cutoff: {}", m);
                    self.published_by = Some(match self.published_by.take() {
                        Some(existing) if existing.as_str() < m.as_str() => existing,
                        _ => m.clone(),
                    });
                }
                self.cutoff_pending = false;
                self.queue.extend(self.deferred_transitives.drain(..));
            }

            let Some(task) = self.queue.pop_front() else {
                if !self.deferred_transitives.is_empty() {
                    return Err(Error::Registry(
                        "(resolver)".to_string(),
                        format!(
                            "{} transitives still deferred when resolve completed",
                            self.deferred_transitives.len()
                        ),
                    ));
                }
                return Ok(());
            };

            self.process_task(task).await?;
        }
    }
}

impl<'a> ResolveDriver<'a> {
    /// Decide whether a primer-seeded `Found` pick must be refetched
    /// live before we trust it (the always-on pick-site freshness gate).
    /// Only consulted when the pick came from the bundled primer (checked
    /// by the caller).
    ///
    /// Returns `true` (refetch) when the pick is at the live frontier
    /// (`Current`) and the offline seed is stale for the active cutoff,
    /// or when a `SoftFrozen` pick coincides with `trustPolicy=NoDowngrade`
    /// (the sparse-seed fail-open hazard). Returns `false` (accept the
    /// offline pick) for frozen picks whose history is settled. See the
    /// big comment on the matching arm in the pick loop for the full
    /// rationale.
    fn primer_pick_needs_refetch(
        &self,
        packument: &aube_registry::Packument,
        picked_version: &str,
        cutoff_for_pkg: Option<&str>,
    ) -> bool {
        match classify_regime(packument, picked_version) {
            // Live edge: refetch only if the offline seed predates the
            // active cutoff (the staleness the legacy gate keyed on,
            // now scoped to just the frontier pick that can actually be
            // wrong). No cutoff active → nothing can be stale → accept.
            Regime::Current => cutoff_for_pkg.is_some_and(|c| !crate::primer::covers_cutoff(c)),
            // Settled history below a newer major: trustworthy for the
            // version pick, BUT the no-downgrade check needs the older
            // trusted neighbors the truncated seed may have dropped —
            // so refetch when that policy is active. Otherwise accept.
            Regime::SoftFrozen => {
                self.resolver.dependency_policy.trust_policy == crate::TrustPolicy::NoDowngrade
            }
            // Settled history within the same major: a refetch could
            // never surface a newer satisfying version, and the seed's
            // window covers the relevant neighbors. Always accept.
            Regime::HardFrozen => false,
        }
    }

    /// Drive a single task through preprocess → local-source → workspace-link → sibling-dedupe → lockfile-reuse → fetch-and-pick.
    ///
    /// Returns `Ok(())` whether the task settled on a version, was
    /// dropped by an override, or was deferred for the TimeBased
    /// cutoff. `Err(_)` propagates to the BFS loop and ends the
    /// resolve.
    #[allow(clippy::too_many_lines)]
    async fn process_task(&mut self, mut task: ResolveTask) -> Result<(), Error> {
        if !self.preprocess_task(&mut task)? {
            return Ok(());
        }

        // A TRANSITIVE `link:`/`portal:` spec whose name is a workspace
        // member is a workspace link (pnpm serializes a peer satisfied by
        // a member this way, e.g. a registry parent recording
        // `vue@link:packages/vue`). Bind it to the local member BEFORE the
        // non-registry dispatch — otherwise it reaches
        // `handle_local_source_task` and the default-on exotic-subdep
        // guard wrongly refuses a first-party workspace package. Gated to
        // NON-root tasks: a root-declared `link:` is the user's explicit
        // local-path intent, so it keeps flowing through the local-source
        // path even when the name happens to match a member. Links to a
        // NON-member also fall through to the local-source path below.
        if !task.is_root
            && (task.range.starts_with("link:") || task.range.starts_with("portal:"))
            && self.workspace_packages.contains_key(&task.name)
            && self.try_workspace_link(&task)
        {
            return Ok(());
        }

        if is_non_registry_specifier(&task.range) {
            return self.handle_local_source_task(task).await;
        }

        if self.try_workspace_link(&task) {
            return Ok(());
        }

        if self.try_sibling_dedupe(&task) {
            return Ok(());
        }

        if self.try_lockfile_reuse(&task).await {
            return Ok(());
        }

        // Packument not in cache. Spawn its fetch if one
        // isn't already running, then wait for packument
        // fetches to land until this task's packument is
        // available. Other fetches that happen to complete
        // while we're waiting get cached opportunistically,
        // which is exactly what lets the pipeline overlap
        // network and CPU: by the time a later task is
        // popped its packument is usually already sitting
        // in the cache because it landed while an earlier
        // task was being waited on.
        let wait_start = std::time::Instant::now();
        // Cache is keyed by the *registry* name — for aliased
        // tasks `task.name` is the user-facing alias (e.g.
        // `h3-v2`), which would never hit. `registry_name()`
        // returns the alias-resolved target (`h3`) on
        // aliased tasks and `task.name` otherwise.
        let fetch_name = task.registry_name().to_string();
        let _diag_task_wait =
            aube_util::diag::Span::new(aube_util::diag::Category::Resolver, "task_wait_packument")
                .with_meta_fn(|| format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(&fetch_name)));
        while !self.resolver.cache.contains_key(&fetch_name)
            && !self.failed_fetches.contains_key(&fetch_name)
        {
            self.ensure_fetch(&fetch_name);
            match self.fetcher.join_next().await {
                Some(Ok(Ok((name, packument, from_primer)))) => {
                    self.fetcher.release_in_flight(&name);
                    if from_primer {
                        self.fetcher.note_primer_seeded(name.clone());
                    }
                    self.resolver.cache.insert(name, packument);
                    self.packument_fetch_count += 1;
                }
                Some(Ok(Err(e))) => {
                    // Store failed fetches in the side table instead
                    // of propagating immediately. pnpm parity.
                    let name = match &e {
                        crate::Error::Registry(n, _) => n.clone(),
                        _ => return Err(e),
                    };
                    self.fetcher.release_in_flight(&name);
                    self.failed_fetches.insert(name, e);
                }
                Some(Err(join_err)) => {
                    return Err(Error::Registry("(join)".to_string(), join_err.to_string()));
                }
                None => {
                    // join_next returns None only when the JoinSet is
                    // empty. ensure_fetch above guarantees at least one
                    // task is in flight if the cache still doesn't
                    // hold this name, so None means the spawn failed
                    // silently. Surface it.
                    return Err(Error::Registry(
                        fetch_name.clone(),
                        "packument fetch disappeared before completing".to_string(),
                    ));
                }
            }
        }
        self.packument_fetch_time += wait_start.elapsed();

        // Post-loop: if this task's packument fetch failed, decide
        // whether to skip (optional) or propagate (required).
        // For optional deps the error stays in `failed_fetches` so
        // sibling tasks that share the same transitive optional dep
        // don't re-fetch and re-fail for each importer.
        if task.dep_type == DepType::Optional && self.failed_fetches.contains_key(&fetch_name) {
            tracing::debug!(
                "skipping optional dep {}@{}: registry fetch failed",
                task.name,
                task.range,
            );
            if task.is_root {
                self.note_root_done();
            }
            return Ok(());
        }
        if let Some(e) = self.failed_fetches.remove(&fetch_name) {
            return Err(e);
        }

        // TimeBased wave-0 gate. Transitives that reach
        // the version-pick step while the cutoff is still
        // unknown must wait until the direct deps have
        // been picked and the cutoff has been derived;
        // otherwise they'd pick against a `None` cutoff
        // and miss the filter. In `Highest` mode (the
        // default), `cutoff_pending` starts false and this
        // is a no-op.
        if self.cutoff_pending && !task.is_root {
            self.deferred_transitives.push(task);
            return Ok(());
        }

        // Version-pick + transitive enqueue. Was a separate
        // sub-loop over `processed_batch` in the old wave
        // code; here it's inline as the tail of the per-task
        // pipeline now that we know the packument is in
        // cache. `registry_name()` is the cache key for
        // aliased tasks (cache is populated under the real
        // registry name), so use the same accessor here.
        // Find locked version
        let locked_version = self.existing.and_then(|g| {
            g.packages
                .values()
                .find(|p| p.name == task.name && version_satisfies(&p.version, &task.range))
                .map(|p| p.version.as_str())
                .filter(|v| {
                    !is_vulnerable(task.registry_name(), v, &self.resolver.vulnerable_ranges)
                })
        });

        // Direct deps in time-based mode pick the lowest
        // satisfying version; everything else (transitives,
        // and all picks in Highest mode) picks highest.
        let pick_lowest =
            self.resolver.resolution_mode == ResolutionMode::TimeBased && task.is_root;
        // Apply the cutoff unless this package is on the
        // minimumReleaseAge exclude list. The exclude list only
        // suppresses the *minimumReleaseAge* leg, not the
        // time-based-mode leg — but since we collapse both
        // into the same `published_by` string at this point,
        // we have to skip the cutoff entirely for excluded
        // names. Acceptable: time-based mode and exclude
        // lists aren't expected to coexist in the wild.
        let cutoff_for_pkg = match self.resolver.minimum_release_age.as_ref() {
            Some(mra) if mra.exclude.contains(&task.name) => None,
            _ => self.published_by.as_deref(),
        };
        // Strict semantics in two cases:
        //   - `minimumReleaseAgeStrict=true` (the user opted in
        //     to hard failures), or
        //   - the cutoff comes from `--resolution-mode=time-based`
        //     alone, with no `minimumReleaseAge` configured. The
        //     time-based cutoff is intended as a hard wall — if
        //     no version fits, the *correct* fix is for the user
        //     to update the lockfile, not for the resolver to
        //     silently pick a different version.
        let strict = match self.resolver.minimum_release_age.as_ref() {
            Some(m) => m.strict,
            None => true,
        };
        let registry_name = task.registry_name().to_string();
        let selected_pick = loop {
            let packument = self.resolver.cache.get(&registry_name).ok_or_else(|| {
                Error::Registry(registry_name.clone(), "packument not in cache".to_string())
            })?;
            let pick = pick_version(
                packument,
                &task.range,
                locked_version,
                pick_lowest,
                cutoff_for_pkg,
                strict,
            );
            match pick {
                // A primer-seeded pick that satisfies the range still
                // needs a live full-packument fetch when `needs_time`
                // is on and the seed carries no publish time for the
                // picked version: the bundled primer's `time` data is
                // sparse, and without it the graph's `time:` map (and
                // every consumer of it — the `defaultTrust` floor, the
                // lockfile round-trip) silently drops the entry. The
                // refetch only fires once per package (it consumes the
                // primer-seeded flag), so a registry whose full
                // packument also lacks the time is not retried.
                PickResult::Found(meta)
                    if self.needs_time
                        && !packument.time.contains_key(&meta.version)
                        && self.fetcher.take_primer_seeded(&registry_name) =>
                {
                    let fetch_start = std::time::Instant::now();
                    let live = match self.resolver.packument_full_cache_dir.as_ref() {
                        Some(dir) => {
                            self.resolver
                                .client
                                .fetch_packument_with_time_cached(&registry_name, dir)
                                .await
                        }
                        None => self.resolver.client.fetch_packument(&registry_name).await,
                    }
                    .map_err(|e| Error::Registry(registry_name.clone(), e.to_string()))?;
                    self.packument_fetch_time += fetch_start.elapsed();
                    self.packument_fetch_count += 1;
                    self.resolver.cache.insert(registry_name.clone(), live);
                }
                // Pick-site freshness gate — the always-on correctness
                // layer beneath the primer TTL. Only the top-level TTL
                // (`primer_within_ttl`, unlimited by default) decides
                // whether the primer is consulted at all; once a name is
                // primer-seeded, *this* arm decides per-pick whether the
                // offline pick must be refetched live, keyed on the
                // picked version's regime.
                //
                // This is the fix for the cold-install regression: the
                // legacy fetch-time `covers_cutoff` gate keyed freshness
                // on the primer *build date*, so once the moving
                // `published_by` cutoff overtook it (~24h post-build),
                // every primer hit was suppressed and a cold install
                // went all-network. The regime of the *picked version*
                // is the right key instead:
                //
                //  - FROZEN (a higher minor in the same major exists →
                //    HardFrozen; only a higher major exists →
                //    SoftFrozen): the slice we picked from is immutable
                //    history — a refetch could never surface a *newer*
                //    satisfying version than what we already hold, so we
                //    serve the offline primer pick indefinitely.
                //    Cooling is NOT bypassed: the age cutoff was already
                //    applied inside `pick_version` against the primer's
                //    own `time` map, so a `minimumReleaseAge` floor still
                //    holds. This is a correctness fix, not a security
                //    weakening.
                //
                //  - CURRENT (the pick sits at the visible frontier —
                //    nothing newer in the packument we hold): a newer
                //    publish could exist upstream that the offline seed
                //    can't see, so we KEEP the freshness gate — if the
                //    seed is stale (`covers_cutoff` false for the active
                //    cutoff) we refetch live before trusting it.
                //
                //  - SOFT-FROZEN + trustPolicy=NoDowngrade: conservative
                //    posture-preserving exception. `check_no_downgrade`
                //    scans *older* versions of the packument for stronger
                //    trust evidence; on a truncated primer seed an older
                //    trusted version may be ABSENT, so the check
                //    silently fails open. A HardFrozen pick is deep
                //    enough in settled history that the seed's window
                //    still covers the relevant neighbors, but a
                //    SoftFrozen pick (a maintenance line under a newer
                //    major) is exactly where the seed is most likely to
                //    have dropped the older trusted release — so we
                //    refetch rather than trust the sparse offline seed.
                //    Security posture is never weakened by the new path.
                PickResult::Found(meta)
                    if self.fetcher.is_primer_seeded(&registry_name)
                        && self.primer_pick_needs_refetch(
                            packument,
                            &meta.version,
                            cutoff_for_pkg,
                        ) =>
                {
                    // Consume the seed flag (one refetch per package,
                    // matching the other heal arms) and fetch live.
                    self.fetcher.take_primer_seeded(&registry_name);
                    let fetch_start = std::time::Instant::now();
                    let live = if self.needs_time {
                        match self.resolver.packument_full_cache_dir.as_ref() {
                            Some(dir) => {
                                self.resolver
                                    .client
                                    .fetch_packument_with_time_cached(&registry_name, dir)
                                    .await
                            }
                            None => self.resolver.client.fetch_packument(&registry_name).await,
                        }
                    } else {
                        match self.resolver.client.fetch_packument(&registry_name).await {
                            Ok(live) => {
                                if let Some(dir) = self.resolver.packument_cache_dir.as_ref() {
                                    self.resolver.client.replace_packument_cache(
                                        &registry_name,
                                        dir,
                                        &live,
                                    );
                                }
                                Ok(live)
                            }
                            Err(err) => Err(err),
                        }
                    }
                    .map_err(|e| Error::Registry(registry_name.clone(), e.to_string()))?;
                    self.packument_fetch_time += fetch_start.elapsed();
                    self.packument_fetch_count += 1;
                    self.resolver.cache.insert(registry_name.clone(), live);
                }
                PickResult::Found(meta) => break meta.clone(),
                PickResult::AgeGated | PickResult::NoMatch
                    if self.fetcher.take_primer_seeded(&registry_name) =>
                {
                    let fetch_start = std::time::Instant::now();
                    let live = if self.needs_time {
                        match self.resolver.packument_full_cache_dir.as_ref() {
                            Some(dir) => {
                                self.resolver
                                    .client
                                    .fetch_packument_with_time_cached(&registry_name, dir)
                                    .await
                            }
                            None => self.resolver.client.fetch_packument(&registry_name).await,
                        }
                    } else {
                        match self.resolver.client.fetch_packument(&registry_name).await {
                            Ok(live) => {
                                if let Some(dir) = self.resolver.packument_cache_dir.as_ref() {
                                    self.resolver.client.replace_packument_cache(
                                        &registry_name,
                                        dir,
                                        &live,
                                    );
                                }
                                Ok(live)
                            }
                            Err(err) => Err(err),
                        }
                    }
                    .map_err(|e| Error::Registry(registry_name.clone(), e.to_string()))?;
                    self.packument_fetch_time += fetch_start.elapsed();
                    self.packument_fetch_count += 1;
                    self.resolver.cache.insert(registry_name.clone(), live);
                }
                // Only surface `AgeGate` when the cutoff actually
                // came from `minimumReleaseAge`. When it came from
                // `--resolution-mode=time-based` alone, the user
                // never opted into the supply-chain age gate, so
                // the failure should report as a plain no-match
                // instead of a misleading "older than 0 minutes".
                PickResult::AgeGated => match self.resolver.minimum_release_age.as_ref() {
                    Some(mra) => {
                        return Err(Error::AgeGate(Box::new(error::build_age_gate(
                            &task,
                            packument,
                            mra.minutes,
                        ))));
                    }
                    None => {
                        return Err(Error::NoMatch(Box::new(error::build_no_match(
                            &task, packument,
                        ))));
                    }
                },
                PickResult::NoMatch => {
                    return Err(Error::NoMatch(Box::new(error::build_no_match(
                        &task, packument,
                    ))));
                }
            }
        };
        let packument = self.resolver.cache.get(&registry_name).ok_or_else(|| {
            Error::Registry(registry_name.clone(), "packument not in cache".to_string())
        })?;
        let picked_ref = prefer_non_vulnerable_pick(
            task.registry_name(),
            packument,
            &task.range,
            &selected_pick,
            pick_lowest,
            cutoff_for_pkg,
            &self.resolver.vulnerable_ranges,
        );
        // Trust-policy enforcement runs *before* any other
        // post-pick processing (mirrors pnpm's placement
        // immediately after `pickPackage`). Skip when policy is
        // off so the off-by-default case is a single enum
        // compare. The check needs the live packument's `time`
        // map and all version metadata, both of which are still
        // in scope here from L1191.
        if self.resolver.dependency_policy.trust_policy == crate::TrustPolicy::NoDowngrade {
            crate::trust::check_no_downgrade(
                packument,
                &picked_ref.version,
                picked_ref,
                &self.resolver.dependency_policy.trust_policy_exclude,
                self.resolver.dependency_policy.trust_policy_ignore_after,
            )
            .map_err(|e| match e {
                crate::trust::TrustCheckError::Downgrade(d) => Error::TrustDowngrade(Box::new(d)),
                crate::trust::TrustCheckError::MissingTime(d) => {
                    Error::TrustCheckMissingTime(Box::new(d))
                }
            })?;
        }

        // Clone the picked metadata into an owned value so we can
        // both run the `readPackage` hook (which needs a
        // disjoint `&mut self` borrow) and, later, mutate the
        // resolver's own caches without holding a borrow into
        // `self.cache`. Also grab the publish-time entry now,
        // for the same reason.
        let mut picked_owned = picked_ref.clone();
        let picked_publish_time = packument.time.get(&picked_ref.version).cloned();
        // Skip the readPackage hook entirely for a `(name, version)`
        // pair we've already fully processed via a prior task. The
        // mutated dep maps only drive the transitive enqueue below,
        // and that block is short-circuited by the `visited` guard
        // later in this iteration — so running the hook here would
        // just burn an IPC round-trip whose result is discarded.
        let prehook_dep_path = dep_path_for(&task.name, &picked_ref.version);
        let already_visited = self.visited.contains(prehook_dep_path.as_str());

        if !already_visited {
            apply_package_extensions(
                &mut picked_owned,
                &self.resolver.dependency_policy.package_extensions,
            );
        }

        // readPackage hook. Runs at most once per version-picked
        // package, before transitive enqueue. We honor edits to
        // the four dep maps and warn on (then discard) edits to
        // name/version/dist/platform/`hasInstallScript` — pnpm
        // tolerates readPackage returning a hollowed-out
        // object, so we restore those fields from the original
        // packument entry after the call.
        if !already_visited && let Some(hook) = self.resolver.read_package_hook.as_mut() {
            let before_name = picked_owned.name.clone();
            let before_version = picked_owned.version.clone();
            let before_dist = picked_owned.dist.clone();
            let before_os = picked_owned.os.clone();
            let before_cpu = picked_owned.cpu.clone();
            let before_libc = picked_owned.libc.clone();
            let before_bundled = picked_owned.bundled_dependencies.clone();
            let before_has_install_script = picked_owned.has_install_script;
            let before_deprecated = picked_owned.deprecated.clone();
            let input = picked_owned.clone();
            let mut after = hook.read_package(input).await.map_err(|e| {
                Error::Registry(before_name.clone(), format!("readPackage hook: {e}"))
            })?;
            if after.name != before_name || after.version != before_version {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_HOOK_IDENTITY_REWRITTEN,
                    "[pnpmfile] readPackage rewrote {}@{} identity to {}@{}; \
                             aube ignores identity edits",
                    before_name,
                    before_version,
                    after.name,
                    after.version,
                );
            }
            after.name = before_name;
            after.version = before_version;
            after.dist = before_dist;
            after.os = before_os;
            after.cpu = before_cpu;
            after.libc = before_libc;
            after.bundled_dependencies = before_bundled;
            after.has_install_script = before_has_install_script;
            after.deprecated = before_deprecated;
            picked_owned = after;
        }
        let version_meta = &picked_owned;

        // Optional deps that don't match the host platform get
        // silently dropped — pnpm parity. Required deps with a
        // bad platform still get installed; the warning matches
        // pnpm's `packageIsInstallable` behavior.
        let platform_ok = is_supported(
            &version_meta.os,
            &version_meta.cpu,
            &version_meta.libc,
            &self.resolver.supported_architectures,
        );
        if !platform_ok {
            if task.dep_type == DepType::Optional {
                tracing::debug!(
                    "skipping optional dep {}@{}: unsupported platform (os={:?} cpu={:?} libc={:?})",
                    task.name,
                    version_meta.version,
                    version_meta.os,
                    version_meta.cpu,
                    version_meta.libc
                );
                if task.is_root
                    && let Some(spec) = task.original_specifier.as_ref()
                {
                    self.skipped_optional_dependencies
                        .entry(task.importer.clone())
                        .or_default()
                        .insert(task.name.clone(), spec.clone());
                }
                if task.is_root {
                    self.note_root_done();
                }
                return Ok(());
            }
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_UNSUPPORTED_PLATFORM_INSTALL,
                "required dep {}@{} declares unsupported platform (os={:?} cpu={:?} libc={:?}); installing anyway",
                task.name,
                version_meta.version,
                version_meta.os,
                version_meta.cpu,
                version_meta.libc
            );
        }

        let version = version_meta.version.clone();
        let dep_path = dep_path_for(&task.name, &version);

        // Record the picked version's publish time so (a) the
        // time-based cutoff computation at the end of wave 0 can
        // derive `published_by` from the directs, (b) the lockfile
        // write emits a `time:` block under time-based mode, and (c)
        // the embedder's `defaultTrust` floor can read `graph.times`
        // for its cooling-window gate. Gated on
        // `should_keep_in_memory_times()` — time-based OR
        // `minimumReleaseAge` OR `trustPolicy=no-downgrade` — because
        // all three need the in-memory publish dates during the
        // resolve. Lockfile-`time:` persistence is gated separately at
        // the write site (`persist_times`), so a Highest-mode install
        // with `minimumReleaseAge` keeps `graph.times` populated yet
        // still writes a `time:`-free lockfile (pnpm parity).
        //
        // Fall back to the prior lockfile's time when the
        // packument doesn't carry one — `aube update` filters
        // direct deps out of `existing.packages` to force a
        // fresh resolve, so the lockfile-reuse fallback further
        // up doesn't fire for them. Without this fallback the
        // resolver-fetched corgi (no time) would silently drop
        // the dep's `time:` entry on every update, even when
        // the version didn't change. Reported in discussion
        // #345 (mrazauskas).
        if self.resolver.should_keep_in_memory_times() {
            if let Some(t) = picked_publish_time.as_ref() {
                self.resolved_times.insert(dep_path.clone(), t.clone());
            } else if let Some(g) = self.existing
                && let Some(t) = g.times.get(&dep_path)
            {
                self.resolved_times.insert(dep_path.clone(), t.clone());
            }
        }

        // Record root dep
        if task.is_root
            && let Some(deps) = self.importers.get_mut(&task.importer)
        {
            deps.push(DirectDep {
                name: task.name.clone(),
                dep_path: dep_path.clone(),
                dep_type: task.dep_type,
                specifier: task.original_specifier.clone(),
            });
        }

        // Wire parent
        if let Some(ref parent_dp) = task.parent
            && let Some(parent_pkg) = self.resolved.get_mut(parent_dp)
        {
            parent_pkg
                .dependencies
                .insert(task.name.clone(), version.clone());
            if task.dep_type == DepType::Optional {
                parent_pkg
                    .optional_dependencies
                    .insert(task.name.clone(), version.clone());
            }
        }

        // Skip if already fully processed this exact version
        if self.visited.contains(dep_path.as_str()) {
            if task.is_root {
                self.note_root_done();
            }
            return Ok(());
        }
        self.visited.insert(std::sync::Arc::from(dep_path.as_str()));

        tracing::trace!("resolved {}@{}", task.name, version);

        // Forward a deprecation message to the install command,
        // subject to `allowedDeprecatedVersions` suppression.
        // User-facing rendering is the CLI's job — doing it here
        // would fire per resolved version with no way for the
        // caller to batch or filter direct-vs-transitive.
        let deprecated_msg: Option<Arc<str>> = version_meta.deprecated.as_deref().and_then(|msg| {
            let suppressed = is_deprecation_allowed(
                &task.name,
                &version,
                &self.resolver.dependency_policy.allowed_deprecated_versions,
            );
            (!suppressed).then(|| Arc::<str>::from(msg))
        });

        // Track this version
        self.resolved_versions
            .entry(task.name.clone())
            .or_default()
            .push(version.clone());

        let integrity = version_meta.dist.as_ref().and_then(|d| d.integrity.clone());
        // Always stash the registry tarball URL on the locked
        // package. pnpm / yarn writers gate emission on
        // `lockfile_include_tarball_url` (so the pnpm
        // round-trip stays byte-identical for projects that
        // opted out); the npm writer emits `resolved:` on
        // every package entry unconditionally, which is what
        // npm itself writes. Carrying the URL on every
        // LockedPackage lets both policies work without a
        // second packument fetch at write time.
        let tarball_url = version_meta.dist.as_ref().map(|d| d.tarball.clone());
        let registry_git_hosted = tarball_url
            .as_deref()
            .is_some_and(|url| url.contains("://npm.pkg.github.com/"));

        // Stream this resolved package for early tarball fetching.
        // `alias_of` mirrors what the LockedPackage below
        // will carry — the streaming fetch consumer in
        // install.rs uses it to derive the real tarball URL
        // for aliased packages where `name` alone (`h3-v2`)
        // would 404.
        if let Some(ref tx) = self.resolver.resolved_tx {
            let pending =
                self.queue.len() + self.fetcher.in_flight_count() + self.deferred_transitives.len();
            let _ = tx
                .send(ResolvedPackage {
                    dep_path: dep_path.clone(),
                    name: task.name.clone(),
                    version: version.clone(),
                    integrity: integrity.clone(),
                    tarball_url: tarball_url.clone(),
                    alias_of: task.real_name.clone(),
                    local_source: None,
                    os: version_meta.os.iter().cloned().collect(),
                    cpu: version_meta.cpu.iter().cloned().collect(),
                    libc: version_meta.libc.iter().cloned().collect(),
                    deprecated: deprecated_msg.clone(),
                    unpacked_size: version_meta.dist.as_ref().and_then(|d| d.unpacked_size),
                    pending,
                })
                .await;
        }

        // Capture the declared peer deps now so the post-pass can compute
        // each consumer's peer context without re-reading the packument.
        // pnpm records a `peerDependencies: { x: '*' }` entry for every
        // `peerDependenciesMeta` key a package ships without an explicit
        // range (debug's optional `supports-color`, typescript-eslint's
        // optional `typescript`, …). Synthesize the same `*` so peer
        // context resolves these exactly like pnpm: an optional peer that
        // a real ancestor / the workspace root provides (typescript) gets
        // a dep-path suffix, while one nothing on the path provides
        // (supports-color) is left unresolved and surfaces under
        // `transitivePeerDependencies`. The optional-peer branch in
        // `visit_peer_context` is what keeps the graph-wide scan from
        // binding the latter to an unrelated copy in the tree.
        let mut peer_deps = version_meta.peer_dependencies.clone();
        for name in version_meta.peer_dependencies_meta.keys() {
            peer_deps
                .entry(name.clone())
                .or_insert_with(|| "*".to_string());
        }
        let peer_meta: BTreeMap<String, aube_lockfile::PeerDepMeta> = version_meta
            .peer_dependencies_meta
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    aube_lockfile::PeerDepMeta {
                        optional: v.optional,
                    },
                )
            })
            .collect();
        // `bundledDependencies` names are shipped inside the
        // tarball itself and must not be resolved from the
        // registry. If we did enqueue them, we'd fetch a
        // (possibly different) version and plant a sibling
        // symlink inside `.aube/<parent>@ver/node_modules/`
        // that would shadow the bundled copy during Node's
        // directory walk. Compute the skip set once here and
        // store the names on the LockedPackage so restore
        // (from lockfile, skipping this code path) also
        // knows to avoid the sibling symlinks — see the
        // `.dependencies` write-through downstream.
        let bundled_names: FxHashSet<String> = version_meta
            .bundled_dependencies
            .as_ref()
            .map(|b| {
                b.names(&version_meta.dependencies)
                    .into_iter()
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        self.resolved.insert(
            dep_path.clone(),
            LockedPackage {
                name: task.name.clone(),
                version: version.clone(),
                integrity,
                dependencies: BTreeMap::new(),
                optional_dependencies: BTreeMap::new(),
                peer_dependencies: peer_deps,
                peer_dependencies_meta: peer_meta,
                dep_path: dep_path.clone(),
                local_source: None,
                os: version_meta.os.iter().cloned().collect(),
                cpu: version_meta.cpu.iter().cloned().collect(),
                libc: version_meta.libc.iter().cloned().collect(),
                bundled_dependencies: {
                    let mut v: Vec<String> = bundled_names.iter().cloned().collect();
                    v.sort();
                    v
                },
                tarball_url,
                registry_git_hosted,
                // `name` is the alias for npm-aliased tasks
                // (`"h3-v2": "npm:h3@..."` → name = "h3-v2"),
                // so stash the real registry name here. The
                // lockfile writer + installer consult
                // `alias_of` whenever they need to hit the
                // registry, matching how the npm-lockfile
                // reader populates this field.
                alias_of: task.real_name.clone(),
                yarn_checksum: None,
                engines: version_meta.engines.clone(),
                // Rehydrate a string-form bin (`"bin": "cli.js"`)
                // into `{<package_name>: "cli.js"}` — registry
                // packuments leave the name off, expecting
                // consumers to default it to the package name.
                // Doing it here keeps bun's per-entry meta
                // byte-identical to bun's own output without
                // pushing the fixup into every writer.
                bin: {
                    let mut m = version_meta.bin.clone();
                    if let Some(path) = m.remove("") {
                        // String-form `bin` in a packument
                        // (`"bin": "cli.js"`) is implicitly
                        // named after the real registry
                        // package — not the alias. For an
                        // aliased dep (`"h3-v2": "npm:h3@…"`)
                        // the bun writer must emit the bin
                        // under `h3`, not `h3-v2`, or the
                        // map drifts against bun's own
                        // output (and the shim install path
                        // creates the wrong binary name).
                        let bin_name = task.real_name.as_deref().unwrap_or(&task.name).to_string();
                        m.insert(bin_name, path);
                    }
                    m
                },
                // Declared ranges straight from the packument's
                // `dependencies` / `optionalDependencies`. Fed
                // back out by npm / yarn / bun writers so
                // nested package entries keep the original
                // specifiers instead of collapsing to pins.
                declared_dependencies: {
                    let mut m = version_meta.dependencies.clone();
                    for (k, v) in &version_meta.optional_dependencies {
                        m.insert(k.clone(), v.clone());
                    }
                    m
                },
                license: version_meta.license.clone(),
                funding_url: version_meta.funding_url.clone(),
                optional: false,
                transitive_peer_dependencies: Vec::new(),
                // Record the registry's deprecation reason so the
                // pnpm/aube writers can emit the `deprecated:` field
                // pnpm keeps on package entries. Stored on the generic
                // meta map rather than a typed slot to match how bun
                // round-trips it. Uses the raw packument message, not
                // `deprecated_msg`: that one is gated by
                // `allowedDeprecatedVersions`, which only silences the
                // install warning — pnpm still records the field.
                extra_meta: version_meta
                    .deprecated
                    .as_deref()
                    .map(|msg| {
                        BTreeMap::from([(
                            "deprecated".to_string(),
                            serde_json::Value::String(msg.to_string()),
                        )])
                    })
                    .unwrap_or_default(),
                // npm's typed per-entry verbatim flags. `has_install_script`
                // and `deprecated` come straight off the packument so a
                // fresh resolve (no prior lockfile) still emits npm's
                // exact `package-lock.json` shape — closing the round-trip
                // churn the survey caught on every `nub add`. `inBundle`
                // and `hasShrinkwrap` aren't recoverable from a packument
                // (they're placement / tarball-shipped properties), so they
                // default to false here and survive only when carried in
                // from a parsed npm lockfile.
                has_install_script: version_meta.has_install_script,
                has_shrinkwrap: false,
                in_bundle: false,
                deprecated: version_meta.deprecated.clone(),
            },
        );

        // Enqueue transitive deps. Kick off a background
        // packument fetch the instant we discover the dep
        // name — so by the time the task is popped off the
        // queue below, its packument is usually already in
        // flight (and often already in cache). This is where
        // the pipeline overlaps fetches with CPU work without
        // any explicit wave barrier.
        //
        // Compute the child ancestor chain once — the same
        // frame (this package's name + resolved version)
        // applies to every dep / optionalDep / peer we enqueue
        // below.
        let mut child_ancestors = task.ancestors.clone();
        child_ancestors.push((task.name.clone(), version.clone()));

        for (dep_name, dep_range) in &version_meta.dependencies {
            if bundled_names.contains(dep_name) {
                continue;
            }
            if self.resolver.dependency_policy.block_exotic_subdeps
                && is_non_registry_specifier(dep_range)
            {
                return Err(Error::Registry(
                    dep_name.clone(),
                    format!(
                        "uses exotic specifier \"{dep_range}\" which is blocked \
                                 by blockExoticSubdeps (declared by {})",
                        task.name
                    ),
                ));
            }
            if !self.existing_names.contains(dep_name.as_str())
                && self.resolver.is_prefetchable(
                    dep_name.as_str(),
                    dep_range.as_str(),
                    self.workspace_packages,
                )
            {
                self.ensure_fetch(dep_name);
            }
            self.queue.push_back(ResolveTask::transitive(
                dep_name.clone(),
                dep_range.clone(),
                DepType::Production,
                dep_path.clone(),
                task.importer.clone(),
                child_ancestors.clone(),
            ));
        }

        for (dep_name, dep_range) in &version_meta.optional_dependencies {
            if bundled_names.contains(dep_name) {
                continue;
            }
            if self
                .resolver
                .ignored_optional_dependencies
                .contains(dep_name)
            {
                continue;
            }
            if self.resolver.dependency_policy.block_exotic_subdeps
                && is_non_registry_specifier(dep_range)
            {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_EXOTIC_SUBDEP_SKIPPED,
                    "skipping optional dependency {dep_name} of {} — \
                             exotic specifier \"{dep_range}\" blocked by blockExoticSubdeps",
                    task.name
                );
                continue;
            }
            if !self.existing_names.contains(dep_name.as_str())
                && self.resolver.is_prefetchable(
                    dep_name.as_str(),
                    dep_range.as_str(),
                    self.workspace_packages,
                )
            {
                self.ensure_fetch(dep_name);
            }
            self.queue.push_back(ResolveTask::transitive(
                dep_name.clone(),
                dep_range.clone(),
                DepType::Optional,
                dep_path.clone(),
                task.importer.clone(),
                child_ancestors.clone(),
            ));
        }

        // Peer dependencies: enqueue only required peers that
        // are truly missing from the importer/root scope. The
        // post-pass below (`apply_peer_contexts`) computes
        // which version each consumer sees, via ancestor
        // scope, and assigns peer-suffixed dep_paths.
        //
        // pnpm's `auto-install-peers=true` fills in missing
        // required peers, but it does not install optional peer
        // alternatives that the user did not ask for, and it
        // does not install a second compatible peer when the
        // importer already declares that peer name at an
        // incompatible version. In the latter case pnpm keeps
        // the user's direct dependency and reports an unmet
        // peer warning.
        //
        // When `auto-install-peers=false`, we skip enqueueing
        // peers entirely. Users are on the hook for adding
        // them to `package.json` themselves. Unmet peers still
        // surface as warnings via `detect_unmet_peers` after
        // resolve — in fact more so, since nothing gets
        // auto-installed.
        //
        // Skip peers that are already declared as regular or
        // optional deps of the same package — those already have a
        // task queued via the loops above, and duplicating would
        // just burn a queue slot.
        if self.resolver.auto_install_peers {
            for (dep_name, dep_range) in &version_meta.peer_dependencies {
                let peer_optional = version_meta
                    .peer_dependencies_meta
                    .get(dep_name)
                    .map(|m| m.optional)
                    .unwrap_or(false);
                // Optional peers are opt-in integrations, not
                // auto-install candidates. Users who need one must
                // declare it in their own manifest so the normal dep
                // loops above resolve it explicitly.
                if peer_optional {
                    continue;
                }
                let importer_declares_peer = self
                    .importer_declared_dep_names
                    .get(&task.importer)
                    .is_some_and(|names| names.contains(dep_name));
                let root_declares_peer = self.resolver.resolve_peers_from_workspace_root
                    && task.importer != "."
                    && self
                        .importer_declared_dep_names
                        .get(".")
                        .is_some_and(|names| names.contains(dep_name));
                let peer_dep_is_ancestor = task.ancestors.iter().any(|(name, _)| name == dep_name);
                if importer_declares_peer || root_declares_peer || peer_dep_is_ancestor {
                    continue;
                }
                if version_meta.dependencies.contains_key(dep_name)
                    || version_meta.optional_dependencies.contains_key(dep_name)
                    || bundled_names.contains(dep_name)
                {
                    continue;
                }
                if self.resolver.dependency_policy.block_exotic_subdeps
                    && is_non_registry_specifier(dep_range)
                {
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_EXOTIC_SUBDEP_SKIPPED,
                        "skipping peer dependency {dep_name} of {} — \
                                 exotic specifier \"{dep_range}\" blocked \
                                 by blockExoticSubdeps",
                        task.name
                    );
                    continue;
                }
                if !self.existing_names.contains(dep_name.as_str())
                    && self.resolver.is_prefetchable(
                        dep_name.as_str(),
                        dep_range.as_str(),
                        self.workspace_packages,
                    )
                {
                    self.ensure_fetch(dep_name);
                }
                self.queue.push_back(ResolveTask::transitive(
                    dep_name.clone(),
                    dep_range.clone(),
                    DepType::Production,
                    dep_path.clone(),
                    task.importer.clone(),
                    child_ancestors.clone(),
                ));
            }
        }

        // Root task just completed its full version-pick
        // path. Decrement the pending-directs counter so
        // the TimeBased cutoff trigger at the top of the
        // outer loop can fire once wave 0 is resolved.
        if task.is_root {
            self.note_root_done();
        }
        Ok(())
    }

    /// Resolve a `file:` / `link:` / `git:` / remote-tarball task.
    ///
    /// Anchors `link:`/`file:` paths against the importer for root
    /// deps and against the parent package's source root for
    /// transitives (and the project root for override-substituted
    /// paths, since overrides are declared at the root). Git +
    /// remote-tarball specs anchor on nothing. Transitive
    /// `link:`/`file:` from a registry-hosted parent errors out —
    /// there's no on-disk path to resolve against.
    ///
    /// Side effects: wires the importer + parent edges, inserts the
    /// package into `resolved`, streams to the early-fetch consumer,
    /// and enqueues the local package's transitives (except for
    /// `link:`, whose transitives are the target's responsibility).
    async fn handle_local_source_task(&mut self, task: ResolveTask) -> Result<(), Error> {
        // Root-declared `pnpm.overrides` opts the user into the
        // rewritten `link:`/`file:` target by name, so they bypass
        // the exotic-subdep block — otherwise an override aimed at a
        // transitive of a registry package would always lose to the
        // default-on guard.
        if !task.range_from_override
            && should_block_exotic_subdep(
                &task,
                &self.resolved,
                self.resolver.dependency_policy.block_exotic_subdeps,
            )
        {
            return Err(Error::BlockedExoticSubdep(Box::new(ExoticSubdepDetails {
                name: task.name.clone(),
                spec: task.range.clone(),
                parent: task
                    .parent
                    .clone()
                    .unwrap_or_else(|| "<unknown>".to_string()),
                ancestors: task.ancestors.clone(),
                importer: task.importer.clone(),
            })));
        }
        // Pull the parent's on-disk package root, when the parent
        // is a directory-backed source. `exec:` stores the
        // generator script path, not the generated package
        // directory, so it cannot safely anchor relative transitive
        // local specifiers.
        let parent_source_root: Option<std::path::PathBuf> = (!task.is_root)
            .then(|| {
                task.parent
                    .as_ref()
                    .and_then(|dp| self.resolved.get(dp))
                    .and_then(|pkg| pkg.local_source.as_ref())
                    .and_then(|src| match src {
                        LocalSource::Directory(p)
                        | LocalSource::Link(p)
                        | LocalSource::Portal(p) => Some(self.resolver.project_root.join(p)),
                        _ => None,
                    })
            })
            .flatten();
        // Override-substituted link:/file: paths are
        // project-root-relative regardless of where the consumer
        // lives — pin them at the root before any importer/parent
        // fallback wins.
        let importer_root = if task.range_from_override {
            self.resolver.project_root.clone()
        } else {
            parent_source_root.clone().unwrap_or_else(|| {
                if task.importer == "." {
                    self.resolver.project_root.clone()
                } else {
                    self.resolver.project_root.join(&task.importer)
                }
            })
        };
        let Some(raw_local) = LocalSource::parse(&task.range, &importer_root) else {
            return Err(Error::Registry(
                task.name.clone(),
                format!("unparseable local specifier: {}", task.range),
            ));
        };
        // Git and remote-tarball specifiers don't reference a path,
        // so they pass through regardless of parent shape.
        // `link:`/`file:` transitives only resolve when we either
        // (a) located a parent source root or (b) inherited the
        // path from a project-root-anchored override.
        if !task.is_root
            && parent_source_root.is_none()
            && !task.range_from_override
            && matches!(
                raw_local,
                LocalSource::Directory(_)
                    | LocalSource::Tarball(_)
                    | LocalSource::Link(_)
                    | LocalSource::Portal(_)
                    | LocalSource::Exec(_)
            )
        {
            return Err(Error::Registry(
                task.name.clone(),
                format!(
                    "transitive local specifier {} cannot be resolved without the parent package source root",
                    task.range
                ),
            ));
        }
        let (mut local, real_version, mut target_deps, integrity) = if let LocalSource::Git(ref g) =
            raw_local
        {
            let shallow = aube_store::git_host_in_list(&g.url, &self.resolver.git_shallow_hosts);
            let (resolved_local, version, deps, integrity) =
                resolve_git_source(&task.name, g, shallow, Some(self.resolver.client.as_ref()))
                    .await
                    .map_err(|e| {
                        Error::Registry(
                            task.name.clone(),
                            format!("git resolve {}: {e}", task.range),
                        )
                    })?;
            let integrity = integrity.or_else(|| {
                existing_local_source_integrity(
                    self.existing,
                    &task.name,
                    &version,
                    &resolved_local,
                )
            });
            (resolved_local, version, deps, integrity)
        } else if let LocalSource::RemoteTarball(ref t) = raw_local {
            let (resolved_local, version, deps) =
                resolve_remote_tarball(&task.name, t, self.resolver.client.as_ref())
                    .await
                    .map_err(|e| {
                        Error::Registry(
                            task.name.clone(),
                            format!("remote tarball {}: {e}", task.range),
                        )
                    })?;
            let integrity = match &resolved_local {
                LocalSource::RemoteTarball(tarball) if !tarball.integrity.is_empty() => {
                    Some(tarball.integrity.clone())
                }
                _ => None,
            };
            (resolved_local, version, deps, integrity)
        } else {
            // Rewrite the path to be relative to the project root so
            // every downstream consumer can resolve it with a single
            // `project_root.join(rel)`.
            let local = rebase_local(&raw_local, &importer_root, &self.resolver.project_root);
            let (version, deps) = if matches!(local, LocalSource::Exec(_)) {
                if self.resolver.ignore_scripts {
                    return Err(Error::Registry(
                        task.name.clone(),
                        format!(
                            "{} requires executing its generator, but scripts are disabled",
                            local.specifier()
                        ),
                    ));
                }
                resolve_exec_manifest(&task.name, &local, &self.resolver.project_root).await?
            } else {
                let (_target_name, version, deps) = read_local_manifest(&raw_local, &importer_root)
                    .unwrap_or_else(|_| (task.name.clone(), "0.0.0".to_string(), BTreeMap::new()));
                (version, deps)
            };
            (local, version, deps, None)
        };
        attach_integrity_to_git_source(&mut local, integrity.as_deref());
        // Apply `packageExtensions` to non-registry packages too. The
        // registry path applies them to the picked VersionMetadata; git /
        // remote-tarball / directory packages resolve through this path
        // with a flat dependency map, so without this an extension
        // targeting a git dep (e.g. a connector the package require()s at
        // runtime) is dropped and never linked as a sibling under the
        // global virtual store.
        apply_package_extensions_to_deps(
            &task.name,
            &real_version,
            &mut target_deps,
            &self.resolver.dependency_policy.package_extensions,
        );
        let dep_path = local.dep_path(&task.name);
        let linked_name = task.name.clone();

        if task.is_root
            && let Some(deps) = self.importers.get_mut(&task.importer)
        {
            deps.push(DirectDep {
                name: task.name.clone(),
                dep_path: dep_path.clone(),
                dep_type: task.dep_type,
                specifier: task.original_specifier.clone(),
            });
        }

        // Wire parent → this exotic transitive. Without this, the
        // parent snapshot's `dependencies` map omits the
        // git/url/file subdep entirely, so the linker never creates
        // the sibling symlink inside the parent's node_modules and
        // the package fails to resolve at runtime. The value is the
        // dep_path tail (e.g. `git+<hash>`) so the linker can
        // reconstruct the full dep_path by concatenating
        // `{name}@{value}`.
        if let Some(ref parent_dp) = task.parent
            && let Some(parent_pkg) = self.resolved.get_mut(parent_dp)
        {
            // `local.dep_path(name)` always returns `{name}@{tail}`;
            // if that invariant ever breaks we'd silently store a
            // malformed dep value that the pnpm writer would emit
            // as-is.
            let name_prefix = format!("{}@", task.name);
            debug_assert!(
                dep_path.starts_with(&name_prefix),
                "local.dep_path returned {dep_path:?} without expected prefix {name_prefix:?}"
            );
            let dep_tail = dep_path
                .strip_prefix(&name_prefix)
                .unwrap_or(&dep_path)
                .to_string();
            parent_pkg
                .dependencies
                .insert(task.name.clone(), dep_tail.clone());
            if task.dep_type == DepType::Optional {
                parent_pkg
                    .optional_dependencies
                    .insert(task.name.clone(), dep_tail);
            }
        }

        if self.visited.insert(std::sync::Arc::from(dep_path.as_str())) {
            self.resolved.insert(
                dep_path.clone(),
                LockedPackage {
                    name: linked_name.clone(),
                    version: real_version.clone(),
                    integrity: integrity.clone(),
                    dep_path: dep_path.clone(),
                    local_source: Some(local.clone()),
                    ..Default::default()
                },
            );
            if let Some(ref tx) = self.resolver.resolved_tx {
                let pending = self.queue.len()
                    + self.fetcher.in_flight_count()
                    + self.deferred_transitives.len();
                let _ = tx
                    .send(ResolvedPackage {
                        dep_path: dep_path.clone(),
                        name: linked_name.clone(),
                        version: real_version.clone(),
                        integrity: integrity.clone(),
                        tarball_url: None,
                        // local_source deps aren't aliased —
                        // `file:`/`link:` specifiers go through the
                        // local-source branch, not the `npm:`
                        // rewrite.
                        alias_of: None,
                        local_source: Some(local.clone()),
                        // Local `file:`/`link:` packages never carry
                        // npm-style platform constraints — they're
                        // whatever the user points at, so the fetch
                        // coordinator treats them as unconstrained
                        // (always fetch).
                        os: aube_lockfile::PlatformList::new(),
                        cpu: aube_lockfile::PlatformList::new(),
                        libc: aube_lockfile::PlatformList::new(),
                        deprecated: None,
                        unpacked_size: None,
                        pending,
                    })
                    .await;
            }
            // Enqueue transitive deps of the local package
            // (directories, tarballs, portals, and exec outputs —
            // `link:` deps are fully the target's responsibility).
            if !matches!(local, LocalSource::Link(_)) {
                let mut child_ancestors = task.ancestors.clone();
                child_ancestors.push((linked_name.clone(), real_version.clone()));
                for (child_name, child_range) in target_deps {
                    self.queue.push_back(ResolveTask::transitive(
                        child_name,
                        child_range,
                        DepType::Production,
                        dep_path.clone(),
                        task.importer.clone(),
                        child_ancestors.clone(),
                    ));
                }
            }
        }
        if task.is_root {
            self.note_root_done();
        }
        Ok(())
    }

    /// Apply catalog, override, and `npm:`/`jsr:` alias rewrites
    /// in-place on `task`.
    ///
    /// Runs a small fixed-point loop (capped at 2 iterations) over
    /// override → npm-alias → jsr-alias, since two interleavings need
    /// to work together: (1) an override whose value is itself an
    /// `npm:` alias, and (2) an alias-declared dep whose override
    /// targets the real package. After one alias rewrite the name is
    /// canonical, so two iterations is enough.
    ///
    /// Returns `Ok(true)` to continue processing, `Ok(false)` when an
    /// override of `"-"` dropped the dep entirely (caller should skip
    /// to the next task), or `Err(_)` for malformed `jsr:` specs.
    fn preprocess_task(&mut self, task: &mut ResolveTask) -> Result<bool, Error> {
        // Catalog protocol: rewrite `catalog:` / `catalog:<name>` to
        // the workspace catalog's actual range *before* the override
        // loop, so overrides can still target a catalog dep by bare
        // name. The original `catalog:...` text stays in
        // `original_specifier` for the lockfile importer.
        if let Some((catalog_name, real_range)) = self
            .resolver
            .resolve_catalog_spec(&task.name, &task.range)?
        {
            tracing::trace!("catalog: {} {} -> {}", task.name, task.range, real_range);
            self.catalog_picks
                .entry(catalog_name)
                .or_default()
                .insert(task.name.clone(), real_range.clone());
            task.range = real_range;
        }

        for _ in 0..2 {
            let mut changed = false;
            if let Some(override_spec) = pick_override_spec(
                &self.resolver.override_rules,
                &task.name,
                &task.range,
                &task.ancestors,
            ) {
                // pnpm's removal marker: an override value of `"-"`
                // drops the dep edge entirely. Skip before
                // catalog/alias rewrites so `-` never reaches the
                // registry resolver.
                if override_spec == "-" {
                    tracing::trace!("override: {}@{} -> dropped", task.name, task.range);
                    if task.is_root {
                        self.note_root_done();
                    }
                    return Ok(false);
                }
                // An override may itself point at a catalog entry
                // (e.g. `"overrides": {"foo": "catalog:"}`). The
                // catalog pre-pass above already ran against the
                // original range, so resolve the indirection here
                // before assigning — otherwise `catalog:` leaks
                // through to the registry resolver.
                let (effective_spec, pending_pick) = match self
                    .resolver
                    .resolve_catalog_spec(&task.name, &override_spec)?
                {
                    Some((catalog_name, real_range)) => {
                        (real_range.clone(), Some((catalog_name, real_range)))
                    }
                    None => (override_spec, None),
                };
                if task.range != effective_spec {
                    if let Some((catalog_name, real_range)) = pending_pick {
                        self.catalog_picks
                            .entry(catalog_name)
                            .or_default()
                            .insert(task.name.clone(), real_range);
                    }
                    tracing::trace!(
                        "override: {}@{} -> {}",
                        task.name,
                        task.range,
                        effective_spec
                    );
                    // Overrides are declared at the project root, so
                    // a substituted `link:`/`file:` path is
                    // project-root-relative — mark the task so the
                    // local-source branch anchors it correctly.
                    if is_non_registry_specifier(&effective_spec) {
                        task.range_from_override = true;
                    }
                    task.range = effective_spec;
                    // If the override replaced the spec with a bare
                    // range (not itself an `npm:`/`jsr:` alias), it's
                    // targeting `task.name` — implicitly undoing any
                    // prior alias rewrite. The alias pass below
                    // picks up a new target on the next iteration if
                    // the override's value is itself an alias.
                    if task.real_name.is_some()
                        && !task.range.starts_with("npm:")
                        && !task.range.starts_with("jsr:")
                    {
                        task.real_name = None;
                    }
                    changed = true;
                }
            }
            if let Some(rest) = task.range.strip_prefix("npm:")
                && let Some(at_idx) = rest.rfind('@')
            {
                let real_name = rest[..at_idx].to_string();
                let real_range = rest[at_idx + 1..].to_string();
                // Keep `task.name` as the user-facing alias; stash
                // the registry name on `real_name`. Only packument /
                // tarball fetch sites (via `task.registry_name()`)
                // hit the real package.
                if task.real_name.as_deref() != Some(real_name.as_str()) || real_range != task.range
                {
                    tracing::trace!("npm alias: {} -> {}@{}", task.name, real_name, real_range);
                    task.real_name = Some(real_name);
                    task.range = real_range;
                    changed = true;
                }
            }
            // `jsr:<range>` and `jsr:<@scope/name>[@<range>]` both
            // land here. JSR's npm-compat endpoint serves every
            // package under `@jsr/<scope>__<name>`; keep `task.name`
            // as the JSR-facing identity and stash the npm-compat
            // name in `real_name`. Only registry IO should see
            // `@jsr/...`.
            if let Some(rest) = task.range.strip_prefix("jsr:") {
                let (jsr_name_raw, jsr_range) = if let Some(body) = rest.strip_prefix('@') {
                    match body.rfind('@') {
                        Some(rel_at) => {
                            // Indices are relative to `body`; add 1
                            // for the `@` we just stripped so we
                            // can slice against the original `rest`.
                            let at_idx = rel_at + 1;
                            (rest[..at_idx].to_string(), rest[at_idx + 1..].to_string())
                        }
                        None => (rest.to_string(), "latest".to_string()),
                    }
                } else {
                    // Bare range form — the manifest key carries the
                    // JSR name (e.g. `"@std/collections": "jsr:^1"`).
                    (task.name.clone(), rest.to_string())
                };
                match aube_registry::jsr::jsr_to_npm_name(&jsr_name_raw) {
                    Some(npm_name) => {
                        if task.real_name.as_deref() != Some(npm_name.as_str())
                            || jsr_range != task.range
                        {
                            tracing::trace!("jsr: {} -> {}@{}", task.name, npm_name, jsr_range);
                            task.real_name = Some(npm_name);
                            task.range = jsr_range;
                            changed = true;
                        }
                    }
                    None => {
                        return Err(Error::Registry(
                            task.name.clone(),
                            format!(
                                "invalid jsr: spec `{}` — expected `jsr:@scope/name[@range]`",
                                task.range,
                            ),
                        ));
                    }
                }
            }
            if !changed {
                break;
            }
        }
        Ok(true)
    }

    /// Wire `task` into the resolver graph as a reuse of an
    /// already-known `version`.
    ///
    /// Updates the importer's direct-dep list (when `task.is_root`),
    /// records the dep edge on the parent package's `dependencies`
    /// map (plus `optional_dependencies` when applicable), and bumps
    /// the pending-directs counter. Used by both the workspace-link
    /// and sibling-dedupe branches, which differ only in where they
    /// source the `version` from.
    fn link_to_existing_version(&mut self, task: &ResolveTask, version: &str) {
        let dep_path = dep_path_for(&task.name, version);
        if task.is_root
            && let Some(deps) = self.importers.get_mut(&task.importer)
        {
            deps.push(DirectDep {
                name: task.name.clone(),
                dep_path: dep_path.clone(),
                dep_type: task.dep_type,
                specifier: task.original_specifier.clone(),
            });
        }
        if let Some(ref parent_dp) = task.parent
            && let Some(parent_pkg) = self.resolved.get_mut(parent_dp)
        {
            parent_pkg
                .dependencies
                .insert(task.name.clone(), version.to_string());
            if task.dep_type == DepType::Optional {
                parent_pkg
                    .optional_dependencies
                    .insert(task.name.clone(), version.to_string());
            }
        }
        if task.is_root {
            self.note_root_done();
        }
    }

    /// Try to resolve `task` against the workspace.
    ///
    /// Three cases link rather than going to the registry: an explicit
    /// `workspace:` protocol (range accepted unconditionally for
    /// `*`/`^`/`~`/`""`, range-checked otherwise); a `link:`/`portal:`
    /// spec whose name is a workspace member (pnpm's serialization of a
    /// workspace peer — e.g. `@vitejs/plugin-vue`'s `vue` peer recorded
    /// as `vue@link:packages/vue` when the importer declares `vue` as a
    /// workspace dep); and a bare semver range whose name matches a
    /// workspace package whose version satisfies the range (yarn-v1 /
    /// npm / bun default). Returns true when the task was wired to the
    /// local workspace copy.
    fn try_workspace_link(&mut self, task: &ResolveTask) -> bool {
        let Some(ws_version) = self.workspace_packages.get(&task.name) else {
            return false;
        };
        let matches = match task.range.strip_prefix("workspace:") {
            // workspace:*, workspace:^, workspace:~ bind to whatever
            // local version is. pnpm's "don't pin me, just track
            // local" sigils.
            Some("" | "*" | "^" | "~") => true,
            // workspace:<range> must still satisfy the local version.
            Some(rest) => version_satisfies(ws_version, rest),
            // `link:`/`portal:` whose name is a workspace member is a
            // workspace link, not an untrusted exotic dep. pnpm records
            // a peer satisfied by a workspace member this way (e.g.
            // `vue@link:packages/vue`); the workspace IS the trust
            // boundary, so bind to the local member by name regardless
            // of the path tail. The `is_member` guard above (name must
            // be in `workspace_packages`) keeps a `link:` to a
            // NON-member out of this branch — those still flow to
            // `handle_local_source_task` and the exotic-subdep guard.
            None if task.range.starts_with("link:") || task.range.starts_with("portal:") => true,
            // Bare semver paths. Special-case `*`/`""` so a workspace
            // with a placeholder version like `0.0.0-0` (common in
            // changesets-managed repos) still links instead of falling
            // through to the registry.
            None if task.range.is_empty() || task.range == "*" => true,
            None => version_satisfies(ws_version, &task.range),
        };
        if !matches {
            return false;
        }
        let ws_version = ws_version.clone();
        self.link_to_existing_version(task, &ws_version);
        true
    }

    /// Try to resolve `task` against an entry in the existing
    /// lockfile.
    ///
    /// Runs after sibling dedupe — these are the two "free" paths
    /// that avoid registry IO. When the lockfile carries a satisfying
    /// non-vulnerable entry, this:
    ///   1. drops optional deps whose platform doesn't fit the host
    ///      (so frozen installs work on a different machine than
    ///      where the lockfile was written),
    ///   2. wires up the importer + parent edges,
    ///   3. streams the resolved package to the early-fetch
    ///      consumer (`resolved_tx`),
    ///   4. re-inserts the locked package into `resolved` (carrying
    ///      peer deps forward so the post-pass sees them without a
    ///      packument refetch),
    ///   5. enqueues the locked package's transitives (stripping any
    ///      peer-context suffix and any `name@` prefix yarn/bun
    ///      writers carry).
    ///
    /// Returns true when a lockfile entry handled the task (whether
    /// fully resolved or dropped as a platform-mismatched optional).
    async fn try_lockfile_reuse(&mut self, task: &ResolveTask) -> bool {
        let Some(locked_pkg) = self.existing.and_then(|g| {
            g.packages.values().find(|p| {
                p.name == task.name
                    && version_satisfies(&p.version, &task.range)
                    && !is_vulnerable(
                        task.registry_name(),
                        &p.version,
                        &self.resolver.vulnerable_ranges,
                    )
            })
        }) else {
            return false;
        };
        // Drop optional deps whose platform constraints don't match
        // the active host / supported set. Handles frozen/lockfile
        // installs on a different machine than the one that wrote
        // the lockfile.
        if task.dep_type == DepType::Optional
            && !is_supported(
                &locked_pkg.os,
                &locked_pkg.cpu,
                &locked_pkg.libc,
                &self.resolver.supported_architectures,
            )
        {
            tracing::debug!(
                "skipping optional dep {}@{}: platform mismatch",
                task.name,
                locked_pkg.version
            );
            if task.is_root
                && let Some(spec) = task.original_specifier.as_ref()
            {
                self.skipped_optional_dependencies
                    .entry(task.importer.clone())
                    .or_default()
                    .insert(task.name.clone(), spec.clone());
            }
            if task.is_root {
                self.note_root_done();
            }
            return true;
        }
        let version = locked_pkg.version.clone();
        let dep_path = dep_path_for(&task.name, &version);

        if task.is_root
            && let Some(deps) = self.importers.get_mut(&task.importer)
        {
            deps.push(DirectDep {
                name: task.name.clone(),
                dep_path: dep_path.clone(),
                dep_type: task.dep_type,
                specifier: task.original_specifier.clone(),
            });
        }
        if let Some(ref parent_dp) = task.parent
            && let Some(parent_pkg) = self.resolved.get_mut(parent_dp)
        {
            parent_pkg
                .dependencies
                .insert(task.name.clone(), version.clone());
            if task.dep_type == DepType::Optional {
                parent_pkg
                    .optional_dependencies
                    .insert(task.name.clone(), version.clone());
            }
        }
        if self.visited.insert(std::sync::Arc::from(dep_path.as_str())) {
            self.resolved_versions
                .entry(task.name.clone())
                .or_default()
                .push(version.clone());

            // Carry any round-tripped publish time forward so (a) the
            // cutoff computation at the end of wave 0 can see reused
            // directs alongside freshly-resolved ones and (b) the
            // next lockfile write preserves the existing `time:`
            // entry even when this install reuses the locked version
            // without re-fetching a packument.
            if self.resolver.should_keep_in_memory_times()
                && let Some(g) = self.existing
                && let Some(t) = g.times.get(&dep_path)
            {
                self.resolved_times.insert(dep_path.clone(), t.clone());
            }

            if let Some(ref tx) = self.resolver.resolved_tx {
                let pending = self.queue.len()
                    + self.fetcher.in_flight_count()
                    + self.deferred_transitives.len();
                let _ = tx
                    .send(ResolvedPackage {
                        dep_path: dep_path.clone(),
                        name: task.name.clone(),
                        version: version.clone(),
                        integrity: locked_pkg.integrity.clone(),
                        tarball_url: locked_pkg.tarball_url.clone(),
                        // Carry the alias identity through the reuse
                        // path — the existing `locked_pkg` already
                        // records it if the lockfile held an aliased
                        // entry, so the streaming fetch still hits
                        // the real registry name.
                        alias_of: locked_pkg.alias_of.clone(),
                        local_source: locked_pkg.local_source.clone(),
                        os: locked_pkg.os.clone(),
                        cpu: locked_pkg.cpu.clone(),
                        libc: locked_pkg.libc.clone(),
                        // Lockfile reuse skips the packument fetch, so
                        // we have no deprecation message to forward
                        // here. `aube deprecations` re-queries
                        // packuments live for the after-the-fact view.
                        deprecated: None,
                        // Same reasoning: lockfile reuse doesn't
                        // refetch the packument and LockedPackage
                        // doesn't carry size metadata, so the
                        // size-estimate segment stays absent.
                        unpacked_size: None,
                        pending,
                    })
                    .await;
            }

            // Carry declared peer deps forward from the existing
            // lockfile so subsequent peer-context computation sees
            // them without a re-fetch.
            self.resolved.insert(
                dep_path.clone(),
                LockedPackage {
                    name: task.name.clone(),
                    version: version.clone(),
                    integrity: locked_pkg.integrity.clone(),
                    dependencies: BTreeMap::new(),
                    optional_dependencies: BTreeMap::new(),
                    peer_dependencies: locked_pkg.peer_dependencies.clone(),
                    peer_dependencies_meta: locked_pkg.peer_dependencies_meta.clone(),
                    dep_path: dep_path.clone(),
                    local_source: locked_pkg.local_source.clone(),
                    os: locked_pkg.os.clone(),
                    cpu: locked_pkg.cpu.clone(),
                    libc: locked_pkg.libc.clone(),
                    bundled_dependencies: locked_pkg.bundled_dependencies.clone(),
                    optional: locked_pkg.optional,
                    transitive_peer_dependencies: locked_pkg.transitive_peer_dependencies.clone(),
                    tarball_url: locked_pkg.tarball_url.clone(),
                    registry_git_hosted: locked_pkg.registry_git_hosted,
                    alias_of: locked_pkg.alias_of.clone(),
                    yarn_checksum: locked_pkg.yarn_checksum.clone(),
                    engines: locked_pkg.engines.clone(),
                    bin: locked_pkg.bin.clone(),
                    declared_dependencies: locked_pkg.declared_dependencies.clone(),
                    license: locked_pkg.license.clone(),
                    funding_url: locked_pkg.funding_url.clone(),
                    extra_meta: locked_pkg.extra_meta.clone(),
                    has_install_script: locked_pkg.has_install_script,
                    has_shrinkwrap: locked_pkg.has_shrinkwrap,
                    in_bundle: locked_pkg.in_bundle,
                    deprecated: locked_pkg.deprecated.clone(),
                },
            );

            // Enqueue transitive deps from the locked package. Strip
            // any peer-context suffix off the version before treating
            // it as a semver range — a locked `"18.2.0(react@18.2.0)"`
            // tail should match against packuments as just `18.2.0`.
            // Also strip a leading `name@` if present: bun/yarn
            // parsers store transitive deps in `name@version` (full
            // dep_path) form, while pnpm stores bare versions. Without
            // the strip, a yarn/bun-locked `is-odd` would emit a
            // transitive task for is-number with range
            // `"is-number@6.0.0"`, which doesn't parse as semver. The
            // lockfile already omitted bundled dep edges on write, so
            // iterating `locked_pkg.dependencies` naturally skips them.
            let mut child_ancestors = task.ancestors.clone();
            child_ancestors.push((task.name.clone(), version.clone()));
            for (dep_name, dep_version) in &locked_pkg.dependencies {
                let prefix = format!("{dep_name}@");
                let stripped = dep_version.strip_prefix(&prefix).unwrap_or(dep_version);
                let canonical_version = stripped.split('(').next().unwrap_or(stripped).to_string();
                let dep_type = if locked_pkg.optional_dependencies.contains_key(dep_name) {
                    DepType::Optional
                } else {
                    DepType::Production
                };
                self.queue.push_back(ResolveTask::transitive(
                    dep_name.clone(),
                    canonical_version,
                    dep_type,
                    dep_path.clone(),
                    task.importer.clone(),
                    child_ancestors.clone(),
                ));
            }
        }
        self.lockfile_reuse_count += 1;
        if task.is_root {
            self.note_root_done();
        }
        true
    }

    /// Try to resolve `task` against a version another task already
    /// settled on this run.
    ///
    /// In the wave-based code this was a post-fetch check; in the
    /// pipelined loop it runs up-front so dedupable tasks never block
    /// on a fetch or a lockfile scan. Returns true when a satisfying,
    /// non-vulnerable sibling version was found and wired in.
    fn try_sibling_dedupe(&mut self, task: &ResolveTask) -> bool {
        let Some(matched_ver) = self.resolved_versions.get(&task.name).and_then(|versions| {
            versions
                .iter()
                .find(|v| {
                    version_satisfies(v, &task.range)
                        && !is_vulnerable(task.registry_name(), v, &self.resolver.vulnerable_ranges)
                })
                .cloned()
        }) else {
            return false;
        };
        self.link_to_existing_version(task, &matched_ver);
        true
    }
}

fn existing_local_source_integrity(
    existing: Option<&LockfileGraph>,
    name: &str,
    version: &str,
    local: &LocalSource,
) -> Option<String> {
    existing
        .and_then(|g| {
            g.packages.values().find(|pkg| {
                pkg.name == name
                    && pkg.local_source.as_ref().is_some_and(|old| {
                        local_sources_match_for_integrity(old, local)
                            && (pkg.version == version
                                || matches!(
                                    (old, local),
                                    (LocalSource::Git(_), LocalSource::Git(_))
                                ) && pkg.version == "0.0.0")
                    })
            })
        })
        .and_then(|pkg| pkg.integrity.clone())
}

fn local_sources_match_for_integrity(old: &LocalSource, new: &LocalSource) -> bool {
    match (old, new) {
        (LocalSource::Git(old), LocalSource::Git(new)) => {
            git_commits_match(&old.resolved, &new.resolved) && old.subpath == new.subpath
        }
        _ => old == new,
    }
}

fn attach_integrity_to_git_source(local: &mut LocalSource, integrity: Option<&str>) {
    if let LocalSource::Git(git) = local
        && git.integrity.is_none()
    {
        git.integrity = integrity.map(str::to_string);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::{GitSource, LockedPackage};

    #[test]
    fn existing_local_source_integrity_matches_resolved_git_commit() {
        let source = LocalSource::Git(GitSource {
            url: "git+https://github.com/acme/dep.git".to_string(),
            committish: Some("main".to_string()),
            resolved: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            integrity: None,
            subpath: None,
        });
        let graph = LockfileGraph {
            packages: BTreeMap::from([(
                "dep@git+https://github.com/acme/dep.git#abcdef0123456789abcdef0123456789abcdef01"
                    .to_string(),
                LockedPackage {
                    name: "dep".to_string(),
                    version: "1.0.0".to_string(),
                    integrity: Some("sha512-old".to_string()),
                    local_source: Some(source.clone()),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };

        assert_eq!(
            existing_local_source_integrity(Some(&graph), "dep", "1.0.0", &source).as_deref(),
            Some("sha512-old")
        );

        let changed_commit = LocalSource::Git(GitSource {
            resolved: "1111111111111111111111111111111111111111".to_string(),
            ..match source {
                LocalSource::Git(g) => g,
                _ => unreachable!(),
            }
        });
        assert!(
            existing_local_source_integrity(Some(&graph), "dep", "1.0.0", &changed_commit)
                .is_none()
        );
    }

    #[test]
    fn existing_local_source_integrity_matches_git_by_resolved_commit() {
        let old_source = LocalSource::Git(GitSource {
            url: "git+ssh://git@github.com/acme/dep.git".to_string(),
            committish: None,
            resolved: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            integrity: None,
            subpath: Some("packages/dep".to_string()),
        });
        let graph = LockfileGraph {
            packages: BTreeMap::from([(
                "dep@git+ssh://git@github.com/acme/dep.git#abcdef0123456789abcdef0123456789abcdef01"
                    .to_string(),
                LockedPackage {
                    name: "dep".to_string(),
                    version: "1.0.0".to_string(),
                    integrity: Some("sha512-old".to_string()),
                    local_source: Some(old_source),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let resolved_source = LocalSource::Git(GitSource {
            url: "https://github.com/acme/dep.git".to_string(),
            committish: Some("main".to_string()),
            resolved: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            integrity: None,
            subpath: Some("packages/dep".to_string()),
        });

        assert_eq!(
            existing_local_source_integrity(Some(&graph), "dep", "1.0.0", &resolved_source)
                .as_deref(),
            Some("sha512-old")
        );
    }

    #[test]
    fn existing_local_source_integrity_matches_git_abbrev_and_placeholder_version() {
        let old_source = LocalSource::Git(GitSource {
            url: "git+ssh://git@github.com/acme/dep.git".to_string(),
            committish: None,
            resolved: "abcdef0".to_string(),
            integrity: None,
            subpath: None,
        });
        let graph = LockfileGraph {
            packages: BTreeMap::from([(
                "dep@git+ssh://git@github.com/acme/dep.git#abcdef0".to_string(),
                LockedPackage {
                    name: "dep".to_string(),
                    version: "0.0.0".to_string(),
                    integrity: Some("sha512-old".to_string()),
                    local_source: Some(old_source),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let resolved_source = LocalSource::Git(GitSource {
            url: "https://github.com/acme/dep.git".to_string(),
            committish: Some("main".to_string()),
            resolved: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            integrity: None,
            subpath: None,
        });

        assert_eq!(
            existing_local_source_integrity(Some(&graph), "dep", "1.0.0", &resolved_source)
                .as_deref(),
            Some("sha512-old")
        );
    }

    #[test]
    fn attach_integrity_to_git_source_fills_missing_git_integrity() {
        let mut source = LocalSource::Git(GitSource {
            url: "https://github.com/acme/dep.git".to_string(),
            committish: Some("main".to_string()),
            resolved: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            integrity: None,
            subpath: None,
        });

        attach_integrity_to_git_source(&mut source, Some("sha512-old"));

        let LocalSource::Git(git) = source else {
            unreachable!();
        };
        assert_eq!(git.integrity.as_deref(), Some("sha512-old"));
    }
}

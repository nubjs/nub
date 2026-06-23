use crate::{Error, FxHashSet, Resolver};
use aube_registry::Packument;
use aube_registry::client::RegistryClient;
use aube_util::adaptive::AdaptiveLimit;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::task::JoinSet;

/// Spawns and tracks in-flight packument fetches.
///
/// Owns the `JoinSet` of running fetch tasks plus the bookkeeping the
/// resolver needs to dedupe spawns (`in_flight_names`) and to know
/// which packuments came from the bundled primer
/// (`primer_seeded_names`, so range misses against the primer's
/// capped history can trigger a live refetch before reporting
/// `ERR_AUBE_NO_MATCHING_VERSION`).
///
/// Pre-clones the immutable Resolver bits the spawn body needs so
/// `ensure_fetch` doesn't need a `&Resolver` borrow at call time —
/// keeping it compatible with the BFS loop's `&mut self.resolver.cache`
/// access pattern.
pub(super) struct FetchScheduler {
    in_flight: JoinSet<Result<(String, Packument, bool), Error>>,
    in_flight_names: FxHashSet<String>,
    primer_seeded_names: FxHashSet<String>,
    sem: Arc<AdaptiveLimit>,
    client: Arc<RegistryClient>,
    cache_dir: Option<PathBuf>,
    full_cache_dir: Option<PathBuf>,
    force_metadata_primer: bool,
    needs_time: bool,
}

pub(super) type FetchOutcome =
    Option<Result<Result<(String, Packument, bool), Error>, tokio::task::JoinError>>;

impl FetchScheduler {
    pub(super) fn new(resolver: &Resolver, sem: Arc<AdaptiveLimit>, needs_time: bool) -> Self {
        Self {
            in_flight: JoinSet::new(),
            in_flight_names: FxHashSet::default(),
            primer_seeded_names: FxHashSet::default(),
            sem,
            client: resolver.client.clone(),
            cache_dir: resolver.packument_cache_dir.clone(),
            full_cache_dir: resolver.packument_full_cache_dir.clone(),
            force_metadata_primer: resolver.force_metadata_primer,
            needs_time,
        }
    }

    pub(super) fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Spawn a fetch for `name` unless one is already running for it.
    ///
    /// The caller is responsible for the resolver-cache gate — passing
    /// a name that's already in the cache wastes a spawn but is
    /// otherwise harmless.
    pub(super) fn ensure_fetch(&mut self, name: &str) {
        if self.in_flight_names.contains(name) {
            return;
        }
        self.in_flight_names.insert(name.to_string());
        // Top-level TTL gate: while the binary is within the primer's TTL
        // (unlimited by default), always let the primer serve at fetch
        // time. The freshness decision lives at the version-pick site,
        // which keys it on the picked version's *regime* (not the build
        // date) — a frozen pick is served offline, a live-edge pick
        // refetches when stale (see `primer_pick_needs_refetch` + the
        // `PickResult::Found` arm in driver.rs). Once the binary ages past
        // a finite TTL, `primer_within_ttl()` is false and the primer is
        // skipped entirely (all-network resolve). The old fetch-site
        // build-date gate (`covers_cutoff`) is no longer the seeding
        // decision — that build-date keying was the self-disable bug; it
        // survives only as the per-regime staleness signal at the pick
        // site.
        let primer_covers_cutoff = crate::primer::primer_within_ttl();
        self.in_flight.spawn(fetch_one_packument(FetchInputs {
            name: name.to_string(),
            client: self.client.clone(),
            cache_dir: self.cache_dir.clone(),
            full_cache_dir: self.full_cache_dir.clone(),
            primer_covers_cutoff,
            force_metadata_primer: self.force_metadata_primer,
            sem: self.sem.clone(),
            needs_time: self.needs_time,
        }));
    }

    /// Wait for the next in-flight fetch to complete.
    pub(super) async fn join_next(&mut self) -> FetchOutcome {
        self.in_flight.join_next().await
    }

    pub(super) fn release_in_flight(&mut self, name: &str) {
        self.in_flight_names.remove(name);
    }

    pub(super) fn note_primer_seeded(&mut self, name: String) {
        self.primer_seeded_names.insert(name);
    }

    /// Returns true if `name` was marked as primer-seeded, removing it.
    pub(super) fn take_primer_seeded(&mut self, name: &str) -> bool {
        self.primer_seeded_names.remove(name)
    }

    /// Non-consuming peek: is `name` currently flagged as primer-seeded?
    /// The pick-site freshness gate uses this to *classify* a pick
    /// before deciding whether to consume the flag and refetch (frozen
    /// picks are accepted as-is, so they must not eagerly clear it).
    pub(super) fn is_primer_seeded(&self, name: &str) -> bool {
        self.primer_seeded_names.contains(name)
    }

    pub(super) async fn drain(&mut self) {
        while self.in_flight.join_next().await.is_some() {}
    }
}

/// Inputs the packument-fetch task needs once it's spawned.
///
/// All fields are owned/`Arc`-cloned so the future can be moved into
/// the resolver's `JoinSet` without borrowing the outer scope.
struct FetchInputs {
    name: String,
    client: Arc<RegistryClient>,
    cache_dir: Option<PathBuf>,
    full_cache_dir: Option<PathBuf>,
    /// Precomputed from the resolver's `minimum_release_age` exclude
    /// list and `published_by` cutoff — if false, the primer is
    /// bypassed even when it would otherwise be eligible.
    primer_covers_cutoff: bool,
    /// `force_metadata_primer` from the resolver: when true, use the
    /// primer even for non-default registries (and rewrite tarball URLs
    /// to the active registry).
    force_metadata_primer: bool,
    sem: Arc<AdaptiveLimit>,
    /// True when the caller needs the packument's `time:` map and
    /// must therefore use the full-packument path.
    needs_time: bool,
}

/// Body of the per-packument fetch task spawned by the resolver.
///
/// Returns `(name, packument, from_primer)` — `from_primer` is true
/// when the result came from the bundled metadata primer (only its
/// capped slice of high-traffic histories), so the caller knows a
/// range miss must trigger a live registry refetch before reporting
/// `ERR_AUBE_NO_MATCHING_VERSION`.
async fn fetch_one_packument(inputs: FetchInputs) -> Result<(String, Packument, bool), Error> {
    let FetchInputs {
        name,
        client,
        cache_dir,
        full_cache_dir,
        primer_covers_cutoff,
        force_metadata_primer,
        sem,
        needs_time,
    } = inputs;
    let _diag_span =
        aube_util::diag::Span::new(aube_util::diag::Category::Resolver, "packument_fetch")
            .with_meta_fn(|| format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(&name)));
    let _diag_inflight = aube_util::diag::inflight(aube_util::diag::Slot::Pack);
    let permit_wait = std::time::Instant::now();
    let permit = sem.acquire().await;
    let permit_wait_ms = permit_wait.elapsed();
    if permit_wait_ms.as_millis() > 1 {
        aube_util::diag::event_lazy(
            aube_util::diag::Category::Resolver,
            "packument_permit_wait",
            permit_wait_ms,
            || format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(&name)),
        );
    }
    aube_util::diag::attribute_wait(aube_util::diag::Slot::Pack, &name, permit_wait_ms);
    let _holder_guard = aube_util::diag::register_holder(aube_util::diag::Slot::Pack, &name);
    let mut cached = if needs_time {
        match full_cache_dir.as_ref() {
            Some(dir) => client.cached_full_packument_lookup(&name, dir),
            None => Default::default(),
        }
    } else if let Some(ref dir) = cache_dir {
        client.cached_packument_lookup(&name, dir)
    } else {
        Default::default()
    };
    if let Some(packument) = cached.packument.take() {
        aube_util::diag::instant_lazy(
            aube_util::diag::Category::Resolver,
            "packument_disk_hit",
            || format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(&name)),
        );
        permit.record_cancelled();
        return Ok((name, packument, false));
    }
    let use_metadata_primer = (force_metadata_primer
        || client.uses_default_npm_registry_for(&name))
        && primer_covers_cutoff;
    if use_metadata_primer
        && !cached.stale
        && let Some(seed) = crate::primer::get(&name)
    {
        let mut packument = seed.packument();
        if force_metadata_primer {
            for version in packument.versions.values_mut() {
                let tarball = client.tarball_url(&version.name, &version.version);
                version.dist = version.dist.take().map(|mut dist| {
                    dist.tarball = tarball;
                    dist
                });
            }
        }
        if needs_time {
            if let Some(dir) = full_cache_dir.as_ref() {
                // Deliberately seed WITHOUT the primer's ETag /
                // Last-Modified. The bundled primer is a *truncated*
                // slice (newest `version_cap` versions) of the full
                // packument, but it carries the registry's real
                // validators for the complete document. Writing them
                // into the full-packument cache would let a later
                // range-miss refetch (driver's `NoMatch` heal under
                // `minimumReleaseAge`) send `If-None-Match`, get a
                // `304 Not Modified`, and resurrect the *truncated*
                // body as if it were authoritative — so a range like
                // `^5.1.0` against a high-churn package whose newest
                // `version_cap` publishes are all newer (e.g.
                // `eslint-plugin-react-hooks`, 2600+ versions) would
                // fail to resolve a version that plainly exists.
                // Dropping the validators forces that heal to be an
                // honest unconditional full GET. The common
                // primer-is-sufficient path never refetches, so it is
                // unaffected.
                client.seed_full_packument_cache(&name, dir, &packument, None, None, false);
            }
        } else if let Some(dir) = cache_dir.as_ref() {
            client.seed_packument_cache(
                &name,
                dir,
                &packument,
                seed.etag.as_deref(),
                seed.last_modified.as_deref(),
                false,
            );
        }
        aube_util::diag::instant_lazy(
            aube_util::diag::Category::Resolver,
            "packument_primer_hit",
            || format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(&name)),
        );
        permit.record_cancelled();
        return Ok((name, packument, true));
    }
    let fetch_outcome = if needs_time {
        match full_cache_dir.as_ref() {
            Some(dir) => {
                client
                    .fetch_packument_with_time_cached_after_lookup(&name, dir, cached)
                    .await
            }
            None => client.fetch_packument(&name).await,
        }
    } else if let Some(ref dir) = cache_dir {
        client
            .fetch_packument_cached_after_lookup(&name, dir, cached)
            .await
    } else {
        client.fetch_packument(&name).await
    };
    let packument = match fetch_outcome {
        Ok(p) => {
            permit.record_success();
            p
        }
        Err(e) => {
            if e.is_throttle() {
                permit.record_throttle();
            } else {
                permit.record_cancelled();
            }
            return Err(Error::Registry(name.clone(), e.to_string()));
        }
    };
    aube_util::diag::instant_lazy(
        aube_util::diag::Category::Resolver,
        "packument_network_hit",
        || format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(&name)),
    );
    Ok((name, packument, false))
}

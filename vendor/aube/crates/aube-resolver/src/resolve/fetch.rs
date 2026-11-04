use crate::{Error, FxHashMap, FxHashSet, Resolver};
use aube_registry::client::RegistryClient;
use aube_registry::{Packument, VersionTrustMetadata};
use aube_util::adaptive::AdaptiveLimit;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use tokio::task::JoinSet;

/// Bound synchronous cache reads and JSON parsing independently from registry
/// capacity. The permit moves into `spawn_blocking` so cancellation cannot
/// release it while the blocking task is still running.
static PACKUMENT_CACHE_IO: LazyLock<Arc<tokio::sync::Semaphore>> = LazyLock::new(|| {
    let concurrency = std::thread::available_parallelism().map_or(4, |n| n.get().clamp(4, 32));
    Arc::new(tokio::sync::Semaphore::new(concurrency))
});

/// Spawns and tracks in-flight packument fetches.
///
/// Owns the `JoinSet` of running fetch tasks plus the bookkeeping the
/// resolver needs to dedupe spawns (`active_fetches`) and to know
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
    in_flight: JoinSet<(FetchKey, Result<FetchResult, Error>)>,
    active_fetches: FxHashSet<FetchKey>,
    task_keys: FxHashMap<tokio::task::Id, FetchKey>,
    primer_seeded_names: FxHashMap<String, PrimerSeed>,
    sem: Arc<AdaptiveLimit>,
    client: Arc<RegistryClient>,
    cache_dir: Option<PathBuf>,
    full_cache_dir: Option<PathBuf>,
    force_metadata_primer: bool,
    needs_time: bool,
}

/// What the fetch task knows about a packument it served from the primer.
#[derive(Clone, Copy)]
pub(super) struct PrimerSeed {
    /// The seed was age-pruned (`Seed::sparse`): old versions that are
    /// neither the highest of their `major.minor` line nor a dist-tag
    /// target are missing, so a pick from it can sit below a version the
    /// registry holds (`semver_util::sparse_pick_needs_refetch`).
    pub(super) sparse: bool,
}

pub(super) type TrustHistory = std::collections::BTreeMap<String, VersionTrustMetadata>;
pub(super) type FetchResult = (String, Packument, FetchSource, Option<TrustHistory>);
pub(super) type FetchOutcome =
    Option<Result<(FetchKey, Result<FetchResult, Error>), tokio::task::JoinError>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum FetchKey {
    Full(String),
    Exact(String, String),
}

/// Where a landed packument came from. `Primer` carries the seed's
/// sparseness so the pick site can tell an age-pruned history — whose pick
/// may sit below a version the registry holds — from a dense one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FetchSource {
    Disk,
    Primer { sparse: bool },
    Network,
    Exact,
}

/// Top-level TTL gate: while the binary is within the primer's TTL
/// (unlimited by default), always let the primer serve at fetch time. The
/// freshness decision lives at the version-pick site, which keys it on the
/// picked version's *regime* (not the build date) — a frozen pick is served
/// offline, a live-edge pick refetches when stale (see
/// `primer_pick_needs_refetch` + the `PickResult::Found` arm in driver.rs).
/// Once the binary ages past a finite TTL this is false and the primer is
/// skipped entirely (all-network resolve). Upstream's fetch-site build-date
/// gate (`covers_cutoff`) is no longer the seeding decision — that build-date
/// keying was the self-disable bug; it survives only as the per-regime
/// staleness signal at the pick site.
fn primer_covers_cutoff() -> bool {
    crate::primer::primer_within_ttl()
}

impl FetchScheduler {
    pub(super) fn new(resolver: &Resolver, sem: Arc<AdaptiveLimit>, needs_time: bool) -> Self {
        Self {
            in_flight: JoinSet::new(),
            active_fetches: FxHashSet::default(),
            task_keys: FxHashMap::default(),
            primer_seeded_names: FxHashMap::default(),
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

    /// Whether an exact request for any version of `name` is still running.
    /// A successful exact response can supersede a retained full-fetch error
    /// by proving the package is reachable and requiring a fresh full fetch.
    pub(super) fn has_active_exact_fetch(&self, name: &str) -> bool {
        self.active_fetches
            .iter()
            .any(|key| matches!(key, FetchKey::Exact(active_name, _) if active_name == name))
    }

    /// Spawn a full fetch for `name` unless one was already scheduled.
    ///
    /// The caller is responsible for the resolver-cache gate — passing
    /// a name that's already in the cache wastes a spawn but is
    /// otherwise harmless.
    pub(super) fn ensure_fetch(&mut self, name: &str, force_refresh: bool) {
        let key = FetchKey::Full(name.to_string());
        if !self.active_fetches.insert(key.clone()) {
            return;
        }
        let primer_covers_cutoff = primer_covers_cutoff();
        let inputs = FetchInputs {
            name: name.to_string(),
            client: self.client.clone(),
            cache_dir: self.cache_dir.clone(),
            full_cache_dir: self.full_cache_dir.clone(),
            primer_covers_cutoff,
            force_metadata_primer: self.force_metadata_primer,
            sem: self.sem.clone(),
            needs_time: self.needs_time,
            force_refresh,
        };
        let task_key = key.clone();
        let handle = self.in_flight.spawn(async move {
            let result = fetch_one_packument(inputs).await;
            (task_key, result)
        });
        self.task_keys.insert(handle.id(), key);
    }

    /// Fetch an exact optional dependency without retaining every historical
    /// version's dependency metadata. The full document is decoded into its
    /// publish-time/trust subset so policy checks keep their semantics.
    pub(super) fn ensure_exact_optional_fetch(&mut self, name: &str, version: &str) {
        let key = FetchKey::Exact(name.to_string(), version.to_string());
        if !self.active_fetches.insert(key.clone()) {
            return;
        }
        let primer_covers_cutoff = primer_covers_cutoff();
        let inputs = FetchInputs {
            name: name.to_string(),
            client: self.client.clone(),
            cache_dir: self.cache_dir.clone(),
            full_cache_dir: self.full_cache_dir.clone(),
            primer_covers_cutoff,
            force_metadata_primer: self.force_metadata_primer,
            sem: self.sem.clone(),
            needs_time: self.needs_time,
            force_refresh: false,
        };
        let version = version.to_string();
        let task_key = key.clone();
        let handle = self.in_flight.spawn(async move {
            let result = fetch_exact_optional_packument(inputs, version).await;
            (task_key, result)
        });
        self.task_keys.insert(handle.id(), key);
    }

    /// Wait for the next in-flight fetch to complete.
    pub(super) async fn join_next(&mut self) -> FetchOutcome {
        match self.in_flight.join_next_with_id().await {
            Some(Ok((id, outcome))) => {
                self.task_keys.remove(&id);
                self.active_fetches.remove(&outcome.0);
                Some(Ok(outcome))
            }
            Some(Err(error)) => {
                if let Some(key) = self.task_keys.remove(&error.id()) {
                    self.active_fetches.remove(&key);
                }
                Some(Err(error))
            }
            None => None,
        }
    }

    pub(super) fn note_primer_seeded(&mut self, name: String, seed: PrimerSeed) {
        self.primer_seeded_names.insert(name, seed);
    }

    /// Returns true if `name` was marked as primer-seeded, removing it.
    pub(super) fn take_primer_seeded(&mut self, name: &str) -> bool {
        self.primer_seeded_names.remove(name).is_some()
    }

    /// Non-consuming peek: is `name` currently flagged as primer-seeded?
    /// The pick-site freshness gate uses this to *classify* a pick
    /// before deciding whether to consume the flag and refetch (frozen
    /// picks are accepted as-is, so they must not eagerly clear it).
    pub(super) fn is_primer_seeded(&self, name: &str) -> bool {
        self.primer_seeded_names.contains_key(name)
    }

    /// Non-consuming peek: was `name` seeded from an age-pruned primer
    /// entry? False for a dense seed and for a name not primer-seeded.
    pub(super) fn primer_seed_is_sparse(&self, name: &str) -> bool {
        self.primer_seeded_names
            .get(name)
            .is_some_and(|seed| seed.sparse)
    }

    pub(super) async fn drain(&mut self) {
        while self.join_next().await.is_some() {}
    }
}

/// Inputs the packument-fetch task needs once it's spawned.
///
/// All fields are owned/`Arc`-cloned so the future can be moved into
/// the resolver's `JoinSet` without borrowing the outer scope.
#[derive(Clone)]
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
    /// Ignore an apparently fresh full-packument disk entry because a newer
    /// compact response proved that it is incomplete.
    force_refresh: bool,
}

/// Body of the per-packument fetch task spawned by the resolver.
///
/// Returns the result source so callers can distinguish incomplete local
/// metadata from a live registry response. [`FetchSource::Primer`] means the
/// bundled primer served it — only its capped slice of high-traffic
/// histories — so the caller knows a range miss must trigger a live registry
/// refetch before reporting `ERR_AUBE_NO_MATCHING_VERSION`.
async fn fetch_one_packument(inputs: FetchInputs) -> Result<FetchResult, Error> {
    let FetchInputs {
        name,
        client,
        cache_dir,
        full_cache_dir,
        primer_covers_cutoff,
        force_metadata_primer,
        sem,
        needs_time,
        force_refresh,
    } = inputs;
    let _diag_span =
        aube_util::diag::Span::new(aube_util::diag::Category::Resolver, "packument_fetch")
            .with_meta_fn(|| format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(&name)));
    let _diag_inflight = aube_util::diag::inflight(aube_util::diag::Slot::Pack);
    let cache_lookup_dir = if needs_time {
        full_cache_dir.clone()
    } else {
        cache_dir.clone()
    };
    let mut cached = if let Some(cache_lookup_dir) = cache_lookup_dir {
        let cache_io_permit = Arc::clone(&PACKUMENT_CACHE_IO)
            .acquire_owned()
            .await
            .map_err(|e| Error::Registry(name.clone(), e.to_string()))?;
        let lookup_client = Arc::clone(&client);
        let lookup_name = name.clone();
        tokio::task::spawn_blocking(move || {
            let _cache_io_permit = cache_io_permit;
            if force_refresh && needs_time {
                lookup_client.invalidate_full_packument_cache(&lookup_name, &cache_lookup_dir);
            }
            if needs_time {
                lookup_client.cached_full_packument_lookup(&lookup_name, &cache_lookup_dir)
            } else {
                lookup_client.cached_packument_lookup(&lookup_name, &cache_lookup_dir)
            }
        })
        .await
        .map_err(|e| Error::Registry(name.clone(), format!("packument cache lookup: {e}")))?
    } else {
        Default::default()
    };
    if let Some(packument) = cached.packument.take() {
        aube_util::diag::instant_lazy(
            aube_util::diag::Category::Resolver,
            "packument_disk_hit",
            || {
                format!(
                    r#"{{"name":{},"versions":{}}}"#,
                    aube_util::diag::jstr(&name),
                    packument.versions.len()
                )
            },
        );
        return Ok((name, packument, FetchSource::Disk, None));
    }
    let use_metadata_primer = !force_refresh
        && (force_metadata_primer || client.uses_default_npm_registry_for(&name))
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
        // Deliberately seed WITHOUT the primer's ETag / Last-Modified, on
        // BOTH cache tiers. The bundled primer is a *truncated* slice
        // (newest `version_cap` versions) of the packument, but it carries
        // the registry's real validators for the complete document.
        // Writing them into a packument cache would let a later range-miss
        // refetch (driver's `NoMatch` heal) send `If-None-Match`, get a
        // `304 Not Modified`, and resurrect the *truncated* body as if it
        // were authoritative — so a range like `^5.1.0` against a
        // high-churn package whose newest `version_cap` publishes are all
        // newer (e.g. `eslint-plugin-react-hooks`, 2600+ versions) would
        // fail to resolve a version that plainly exists. Dropping the
        // validators forces that heal to be an honest unconditional GET.
        // The common primer-is-sufficient path never refetches, so it is
        // unaffected.
        //
        // The abbreviated tier needs this as much as the full tier: it is
        // the one in play whenever the age gate is off or
        // `registry-supports-time-field` is set, and it was seeded WITH the
        // validators until this was fixed.
        if needs_time {
            if let Some(dir) = full_cache_dir.as_ref() {
                client.seed_full_packument_cache(&name, dir, &packument, None, None, false);
            }
        } else if let Some(dir) = cache_dir.as_ref() {
            client.seed_packument_cache(&name, dir, &packument, None, None, false);
        }
        aube_util::diag::instant_lazy(
            aube_util::diag::Category::Resolver,
            "packument_primer_hit",
            || {
                format!(
                    r#"{{"name":{},"versions":{}}}"#,
                    aube_util::diag::jstr(&name),
                    packument.versions.len()
                )
            },
        );
        return Ok((
            name,
            packument,
            FetchSource::Primer {
                sparse: seed.sparse,
            },
            None,
        ));
    }
    // The adaptive limit models registry capacity. Local metadata does not
    // consume that capacity and must not queue behind slow HTTP requests.
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
    let fetch_outcome = if needs_time {
        match full_cache_dir.as_ref() {
            Some(dir) => {
                client
                    .fetch_packument_with_time_cached_after_lookup(&name, dir, cached)
                    .await
            }
            // No full-packument disk cache (update's dist-tag freshness
            // rule) still needs the `time` map: the corgi fallback here
            // silently disabled the `minimumReleaseAge` gate, because a
            // version with no publish time bypasses the cutoff at the
            // pick site.
            None => client.fetch_packument_with_time(&name).await,
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
        || {
            format!(
                r#"{{"name":{},"versions":{}}}"#,
                aube_util::diag::jstr(&name),
                packument.versions.len()
            )
        },
    );
    Ok((name, packument, FetchSource::Network, None))
}

async fn fetch_exact_optional_packument(
    inputs: FetchInputs,
    version: String,
) -> Result<FetchResult, Error> {
    let permit = inputs.sem.acquire().await;
    let name = inputs.name.clone();
    let fetched = inputs
        .client
        .fetch_exact_version_packument(&name, &version)
        .await;
    match fetched {
        Ok(exact) => {
            permit.record_success();
            let mut versions = std::collections::BTreeMap::new();
            versions.insert(version, exact.metadata);
            Ok((
                name.clone(),
                Packument {
                    name,
                    modified: None,
                    versions,
                    dist_tags: std::collections::BTreeMap::new(),
                    time: exact.history.time,
                },
                FetchSource::Exact,
                Some(exact.history.versions),
            ))
        }
        Err(err) => {
            tracing::debug!(
                "compact exact metadata fetch failed for optional dep {name}@{version}; falling back to full packument: {err}"
            );
            permit.record_cancelled();
            let fallback_inputs = inputs.clone();
            let fallback = fetch_one_packument(inputs).await?;
            if fallback.1.versions.contains_key(&version) {
                Ok(fallback)
            } else if matches!(fallback.2, FetchSource::Disk | FetchSource::Primer { .. }) {
                let mut refresh_inputs = fallback_inputs;
                refresh_inputs.force_refresh = true;
                let refreshed = fetch_one_packument(refresh_inputs).await?;
                if refreshed.1.versions.contains_key(&version) {
                    Ok(refreshed)
                } else {
                    Err(Error::Registry(
                        name,
                        format!("version {version} is missing from the full packument"),
                    ))
                }
            } else {
                Err(Error::Registry(
                    name,
                    format!("version {version} is missing from the full packument"),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn serve_registry_responses(
        bodies: Vec<Vec<u8>>,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let registry = format!("http://{}/", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let server = {
            let requests = Arc::clone(&requests);
            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let request = requests.fetch_add(1, Ordering::Relaxed);
                    let body = bodies[request.min(bodies.len() - 1)].clone();
                    tokio::spawn(async move {
                        let mut buf = [0_u8; 2048];
                        let _ = socket.read(&mut buf).await;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        );
                        socket.write_all(response.as_bytes()).await.unwrap();
                        socket.write_all(&body).await.unwrap();
                    });
                }
            })
        };
        (registry, requests, server)
    }

    async fn serve_registry(
        body: Vec<u8>,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        serve_registry_responses(vec![body]).await
    }

    #[tokio::test]
    async fn completed_full_fetch_can_force_refresh_a_fresh_disk_entry() {
        let Some(name) = crate::primer::popular_package_names()
            .lines()
            .find(|name| crate::primer::get(name).is_some())
        else {
            return;
        };
        let stale = crate::primer::get(name).unwrap().packument();
        let Some(mut new_metadata) = stale.versions.values().next().cloned() else {
            return;
        };
        let new_version = "9999.0.0";
        new_metadata.version = new_version.to_string();
        let mut fresh = stale.clone();
        fresh.versions.insert(new_version.to_string(), new_metadata);
        let stale_len = stale.versions.len();
        let (registry, requests, server) =
            serve_registry(serde_json::to_vec(&fresh).unwrap()).await;

        let cache = tempfile::tempdir().unwrap();
        let client = Arc::new(RegistryClient::new(&registry));
        client.seed_full_packument_cache(name, cache.path(), &stale, None, None, true);
        let resolver = Resolver::new(Arc::clone(&client))
            .with_packument_full_cache(cache.path().to_path_buf())
            .with_force_metadata_primer(true);
        let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), true);

        scheduler.ensure_fetch(name, false);
        let Some(Ok((FetchKey::Full(_), Ok((_, first, _, _))))) = scheduler.join_next().await
        else {
            panic!("cached full fetch did not complete");
        };
        assert_eq!(first.versions.len(), stale_len);
        assert_eq!(requests.load(Ordering::Relaxed), 0);

        scheduler.ensure_fetch(name, true);
        let Some(Ok((FetchKey::Full(_), Ok((_, refreshed, _, _))))) = scheduler.join_next().await
        else {
            panic!("forced full refresh did not restart");
        };
        assert!(refreshed.versions.contains_key(new_version));
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        server.abort();
    }

    #[tokio::test]
    async fn disk_metadata_does_not_wait_for_a_network_permit() {
        let Some(name) = crate::primer::popular_package_names()
            .lines()
            .find(|name| crate::primer::get(name).is_some())
        else {
            return;
        };
        let cache = tempfile::tempdir().unwrap();
        let client = Arc::new(RegistryClient::new("https://registry.npmjs.org"));
        client.seed_packument_cache(
            name,
            cache.path(),
            &crate::primer::get(name).unwrap().packument(),
            None,
            None,
            true,
        );
        let limiter = AdaptiveLimit::new(1, 1, 1);
        let held_network_permit = limiter.acquire().await;
        let resolver = Resolver::new(client).with_packument_cache(cache.path().to_path_buf());
        let mut scheduler = FetchScheduler::new(&resolver, limiter, false);

        scheduler.ensure_fetch(name, false);
        let outcome =
            tokio::time::timeout(std::time::Duration::from_secs(1), scheduler.join_next())
                .await
                .expect("disk lookup queued behind the network permit");
        let Some(Ok((FetchKey::Full(_), Ok((_, _, source, _))))) = outcome else {
            panic!("disk fetch did not complete");
        };
        assert_eq!(source, FetchSource::Disk);
        drop(held_network_permit);
    }

    #[tokio::test]
    async fn primer_metadata_does_not_wait_for_a_network_permit() {
        let Some(name) = crate::primer::popular_package_names()
            .lines()
            .find(|name| crate::primer::get(name).is_some())
        else {
            return;
        };
        let limiter = AdaptiveLimit::new(1, 1, 1);
        let held_network_permit = limiter.acquire().await;
        let resolver = Resolver::new(Arc::new(RegistryClient::new("https://registry.npmjs.org")))
            .with_force_metadata_primer(true);
        let mut scheduler = FetchScheduler::new(&resolver, limiter, false);

        scheduler.ensure_fetch(name, false);
        let outcome =
            tokio::time::timeout(std::time::Duration::from_secs(1), scheduler.join_next())
                .await
                .expect("primer lookup queued behind the network permit");
        let Some(Ok((FetchKey::Full(_), Ok((_, _, source, _))))) = outcome else {
            panic!("primer fetch did not complete");
        };
        assert!(matches!(source, FetchSource::Primer { .. }));
        drop(held_network_permit);
    }

    #[tokio::test]
    async fn missing_exact_version_fallback_returns_a_terminal_error() {
        let body = serde_json::to_vec(&serde_json::json!({
            "name": "shared",
            "versions": {
                "1.0.0": { "name": "shared", "version": "1.0.0" }
            },
            "dist-tags": { "latest": "1.0.0" },
            "time": { "1.0.0": "2024-01-01T00:00:00.000Z" }
        }))
        .unwrap();
        let (registry, requests, server) = serve_registry(body).await;
        let resolver = Resolver::new(Arc::new(RegistryClient::new(&registry)));
        let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), true);

        scheduler.ensure_exact_optional_fetch("shared", "2.0.0");
        let Some(Ok((FetchKey::Exact(_, version), Err(Error::Registry(_, message))))) =
            scheduler.join_next().await
        else {
            panic!("missing exact version did not return a terminal registry error");
        };
        assert_eq!(version, "2.0.0");
        assert!(message.contains("version 2.0.0 is missing"));
        assert_eq!(requests.load(Ordering::Relaxed), 2);
        server.abort();
    }

    #[tokio::test]
    async fn exact_fallback_refreshes_an_incomplete_disk_entry() {
        let incomplete = serde_json::from_value(serde_json::json!({
            "name": "shared",
            "versions": {
                "1.0.0": { "name": "shared", "version": "1.0.0" }
            },
            "dist-tags": { "latest": "1.0.0" },
            "time": { "1.0.0": "2024-01-01T00:00:00.000Z" }
        }))
        .unwrap();
        let refreshed = serde_json::to_vec(&serde_json::json!({
            "name": "shared",
            "versions": {
                "1.0.0": { "name": "shared", "version": "1.0.0" },
                "2.0.0": { "name": "shared", "version": "2.0.0" }
            },
            "dist-tags": { "latest": "2.0.0" },
            "time": {
                "1.0.0": "2024-01-01T00:00:00.000Z",
                "2.0.0": "2024-02-01T00:00:00.000Z"
            }
        }))
        .unwrap();
        let compact_miss = serde_json::to_vec(&incomplete).unwrap();
        let (registry, requests, server) =
            serve_registry_responses(vec![compact_miss, refreshed]).await;
        let cache = tempfile::tempdir().unwrap();
        let client = Arc::new(RegistryClient::new(&registry));
        client.seed_full_packument_cache("shared", cache.path(), &incomplete, None, None, true);
        let resolver = Resolver::new(client).with_packument_full_cache(cache.path().to_path_buf());
        let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), true);

        scheduler.ensure_exact_optional_fetch("shared", "2.0.0");
        let Some(Ok((FetchKey::Exact(_, _), Ok((_, packument, source, _))))) =
            scheduler.join_next().await
        else {
            panic!("forced exact fallback refresh did not complete");
        };
        assert!(packument.versions.contains_key("2.0.0"));
        assert_eq!(source, FetchSource::Network);
        assert_eq!(requests.load(Ordering::Relaxed), 2);
        server.abort();
    }

    async fn panic_fetch(key: FetchKey) -> (FetchKey, Result<FetchResult, Error>) {
        let _ = key;
        panic!("simulated fetch panic");
    }

    #[tokio::test]
    async fn join_error_releases_the_active_fetch_key() {
        let resolver = Resolver::new(Arc::new(RegistryClient::new("http://127.0.0.1:0")));
        let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), false);
        let key = FetchKey::Full("shared".to_string());
        scheduler.active_fetches.insert(key.clone());
        let handle = scheduler.in_flight.spawn(panic_fetch(key.clone()));
        scheduler.task_keys.insert(handle.id(), key.clone());

        assert!(matches!(scheduler.join_next().await, Some(Err(_))));
        assert!(!scheduler.active_fetches.contains(&key));
        assert!(scheduler.task_keys.is_empty());
    }

    #[test]
    fn active_exact_fetch_is_reported_by_package_name() {
        let resolver = Resolver::new(Arc::new(RegistryClient::new("http://127.0.0.1:0")));
        let mut scheduler = FetchScheduler::new(&resolver, AdaptiveLimit::new(1, 1, 1), false);
        scheduler
            .active_fetches
            .insert(FetchKey::Exact("shared".to_string(), "1.0.0".to_string()));

        assert!(scheduler.has_active_exact_fetch("shared"));
        assert!(!scheduler.has_active_exact_fetch("other"));
    }
}

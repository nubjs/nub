use super::body::{check_body_cap, is_retriable_status, retry_after_from};
use super::cache::*;
use super::{
    PACKUMENT_ACCEPT, PACKUMENT_FULL_ACCEPT, RegistryClient, force_full_packument,
    parse_full_response,
};
use crate::{Error, NetworkMode, Packument};
use std::path::{Path, PathBuf};

impl RegistryClient {
    pub fn cached_packument_lookup(&self, name: &str, cache_dir: &Path) -> CachedPackumentLookup {
        let registry_url = self.config.registry_for(name).to_string();
        let Some(cache_path) = packument_cache_path(cache_dir, name, &registry_url) else {
            return CachedPackumentLookup::default();
        };
        let Some(cached) = read_cached_packument(&cache_path) else {
            return CachedPackumentLookup::default();
        };
        if self.trust_cached_packument(cached.fetched_at, cached.max_age_secs) {
            return CachedPackumentLookup {
                packument: Some(cached.packument),
                stale: false,
                cached: None,
            };
        }
        CachedPackumentLookup {
            packument: None,
            stale: true,
            cached: Some(CachedPackumentLookupEntry::Abbreviated(cached)),
        }
    }

    pub fn cached_full_packument_lookup(
        &self,
        name: &str,
        cache_dir: &Path,
    ) -> CachedPackumentLookup {
        let registry_url = self.config.registry_for(name).to_string();
        let Some(cache_path) = packument_full_cache_path(cache_dir, name, &registry_url) else {
            return CachedPackumentLookup::default();
        };
        read_cached_full_packument_typed_lookup(&cache_path, self.force_cache())
    }

    pub fn seed_packument_cache(
        &self,
        name: &str,
        cache_dir: &Path,
        packument: &Packument,
        etag: Option<&str>,
        last_modified: Option<&str>,
        fresh: bool,
    ) {
        let registry_url = self.config.registry_for(name);
        let Some(cache_path) = packument_cache_path(cache_dir, name, registry_url) else {
            return;
        };
        if cache_path.exists() {
            return;
        }
        let cached = CachedPackument {
            etag: etag.map(str::to_owned),
            last_modified: last_modified.map(str::to_owned),
            fetched_at: if fresh { now_secs() } else { 0 },
            max_age_secs: (!fresh).then_some(0),
            packument: packument.clone(),
        };
        if let Err(e) = write_cached_packument(&cache_path, &cached) {
            tracing::debug!(
                "failed to seed packument cache {} from bundled primer: {e}",
                cache_path.display()
            );
        }
    }

    pub fn replace_packument_cache(&self, name: &str, cache_dir: &Path, packument: &Packument) {
        let registry_url = self.config.registry_for(name);
        let Some(cache_path) = packument_cache_path(cache_dir, name, registry_url) else {
            return;
        };
        let cached = CachedPackument {
            etag: None,
            last_modified: None,
            fetched_at: now_secs(),
            max_age_secs: None,
            packument: packument.clone(),
        };
        if let Err(e) = write_cached_packument(&cache_path, &cached) {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_PACKUMENT_CACHE_WRITE,
                "failed to write packument cache {}: {e}",
                cache_path.display()
            );
        }
    }

    pub fn seed_full_packument_cache(
        &self,
        name: &str,
        cache_dir: &Path,
        packument: &Packument,
        etag: Option<&str>,
        last_modified: Option<&str>,
        fresh: bool,
    ) {
        let registry_url = self.config.registry_for(name);
        let Some(cache_path) = packument_full_cache_path(cache_dir, name, registry_url) else {
            return;
        };
        if cache_path.exists() {
            return;
        }
        let Ok(packument) = serde_json::to_value(packument) else {
            return;
        };
        let fetched_at = if fresh { now_secs() } else { 0 };
        let max_age_secs = (!fresh).then_some(0);
        if let Err(e) = write_cached_full_packument(
            &cache_path,
            etag,
            last_modified,
            fetched_at,
            max_age_secs,
            &packument,
        ) {
            tracing::debug!(
                "failed to seed full packument cache {} from bundled primer: {e}",
                cache_path.display()
            );
        }
    }
    pub async fn fetch_packument_full_cached(
        &self,
        name: &str,
        cache_dir: &Path,
    ) -> Result<serde_json::Value, Error> {
        let registry_url = self.config.registry_for(name).to_string();
        let cache_path = packument_full_cache_path(cache_dir, name, &registry_url)
            .ok_or_else(|| Error::InvalidName(name.to_string()))?;
        let cached = read_cached_full_packument(&cache_path);

        // --prefer-offline / --offline: trust any cached copy regardless of age.
        // --offline additionally forbids falling back to the network on a miss.
        let force_cache = self.force_cache();
        if let Some(c) = cached.as_ref()
            && (force_cache || cached_is_fresh(c.fetched_at, c.max_age_secs))
        {
            return Ok(cached.unwrap().packument);
        }
        if self.network_mode == NetworkMode::Offline {
            return Err(Error::Offline(format!("packument for {name}")));
        }

        // Single-flight: same shape as `fetch_packument_cached_with_entry`.
        // See that method's comment for the why. Keyed `full:<registry>:<name>`
        // so the full and corgi paths don't serialize through each other.
        // Released before any retry backoff sleep so waiters don't pay
        // a serialized recovery cost when the winner hits transient errors.
        let (url, registry_url) = self.packument_url(name);
        let sf_key = format!("full:{registry_url}:{name}");
        let sf_mutex = self.packument_singleflight_mutex(sf_key);
        let mut sf_guard = Some(sf_mutex.lock().await);
        let cached = match read_cached_full_packument(&cache_path) {
            Some(c) if force_cache || cached_is_fresh(c.fetched_at, c.max_age_secs) => {
                return Ok(c.packument);
            }
            recheck => recheck.or(cached),
        };
        let started = std::time::Instant::now();

        // Rebuild the conditional request on each retry. Held in a
        // closure so the revalidation headers are consistent across
        // attempts — a 503 retry with stale `If-None-Match` would be
        // a caching bug.
        let cached_ref = cached.as_ref();
        let label = format!("packument {name}");
        let max_attempts = self.fetch_policy.retries.saturating_add(1);
        for attempt in 0..max_attempts {
            let is_last = attempt + 1 >= max_attempts;
            match {
                let mut req = self
                    .authed_get_for_package(&url, registry_url, name)
                    .header("Accept", PACKUMENT_FULL_ACCEPT)
                    // RFC 9218: packument metadata is resolver-blocking,
                    // mark Critical so H2-aware origins prioritize it
                    // ahead of pending tarball frames.
                    .header(
                        "Priority",
                        aube_util::http::priority::header_value(
                            aube_util::http::priority::Urgency::Critical,
                            false,
                        ),
                    );
                if let Some(c) = cached_ref {
                    if let Some(ref etag) = c.etag {
                        req = req.header("If-None-Match", etag);
                    }
                    if let Some(ref lm) = c.last_modified {
                        req = req.header("If-Modified-Since", lm);
                    }
                }
                req
            }
            .send()
            .await
            {
                Ok(resp) if is_retriable_status(resp.status()) && !is_last => {
                    let wait = retry_after_from(&resp)
                        .unwrap_or_else(|| self.fetch_policy.backoff_for_attempt(attempt + 1));
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        status = resp.status().as_u16(),
                        label,
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSIENT,
                        "retrying HTTP request after transient failure",
                    );
                    drop(sf_guard.take());
                    tokio::time::sleep(wait).await;
                }
                Ok(resp) => {
                    if resp.status() == reqwest::StatusCode::NOT_FOUND {
                        self.maybe_record_slow_metadata(&label, started);
                        return Err(Error::NotFound(name.to_string()));
                    }

                    if resp.status() == reqwest::StatusCode::NOT_MODIFIED
                        && let Some(c) = cached.as_ref()
                    {
                        let revalidated_max_age =
                            parse_cache_control_max_age(&resp).or(c.max_age_secs);
                        if let Err(e) = write_cached_full_packument(
                            &cache_path,
                            c.etag.as_deref(),
                            c.last_modified.as_deref(),
                            now_secs(),
                            revalidated_max_age,
                            &c.packument,
                        ) {
                            tracing::warn!(
                                code = aube_codes::warnings::WARN_AUBE_PACKUMENT_CACHE_WRITE,
                                "failed to write packument cache {}: {e}",
                                cache_path.display()
                            );
                        }
                        self.maybe_record_slow_metadata(&label, started);
                        return Ok(c.packument.clone());
                    }

                    let (etag, last_modified) = extract_cache_headers(&resp);
                    let max_age_secs = parse_cache_control_max_age(&resp);
                    let resp = resp.error_for_status()?;
                    check_body_cap(&resp, self.fetch_policy.packument_max_bytes, &label)?;
                    match parse_full_response::<serde_json::Value>(resp).await {
                        Ok(packument) => {
                            if let Err(e) = write_cached_full_packument(
                                &cache_path,
                                etag.as_deref(),
                                last_modified.as_deref(),
                                now_secs(),
                                max_age_secs,
                                &packument,
                            ) {
                                tracing::warn!(
                                    code = aube_codes::warnings::WARN_AUBE_PACKUMENT_CACHE_WRITE,
                                    "failed to write packument cache {}: {e}",
                                    cache_path.display()
                                );
                            }
                            self.maybe_record_slow_metadata(&label, started);
                            return Ok(packument);
                        }
                        Err(err) if !is_last => {
                            let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                            tracing::warn!(
                                    attempt = attempt + 1,
                                    max_attempts,
                                    backoff_ms = wait.as_millis() as u64,
                                    error = %err,
                                    label,
                                    code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_BODY_DECODE,
                            "retrying HTTP request after response body decode error",
                                );
                            drop(sf_guard.take());
                            tokio::time::sleep(wait).await;
                        }
                        Err(err) => return Err(err),
                    }
                }
                Err(err) if !is_last => {
                    let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        error = %err,
                        label,
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSPORT,
                        "retrying HTTP request after transport error",
                    );
                    drop(sf_guard.take());
                    tokio::time::sleep(wait).await;
                }
                Err(err) => return Err(err.into()),
            }
        }
        unreachable!("retry loop exited without returning; max_attempts was {max_attempts}")
    }

    /// Fetch the full (non-corgi) packument for a package and parse it
    /// into [`Packument`]. Unlike [`Self::fetch_packument_cached`], the
    /// result includes the `time` map — needed for
    /// `--resolution-mode=time-based`. Shares on-disk cache layout with
    /// [`Self::fetch_packument_full_cached`] so callers pay one network
    /// fetch for both the `aube view`-style full JSON and the time map.
    ///
    /// Hot path on warm cache: reads the cache file once and uses
    /// `sonic-rs` to deserialize the wrapper directly into the typed
    /// [`Packument`] shape in a single pass. This avoids the older
    /// `serde_json::Value` + `serde_json::from_value` round-trip, which
    /// walked the cached JSON twice on every resolver read.
    pub async fn fetch_packument_with_time_cached(
        &self,
        name: &str,
        cache_dir: &Path,
    ) -> Result<Packument, Error> {
        // Fast path: try the warm-cache read first. Matches the
        // freshness window logic in `fetch_packument_full_cached`
        // exactly so the two APIs share revalidation behavior.
        let registry_url = self.config.registry_for(name).to_string();
        let cache_path = packument_full_cache_path(cache_dir, name, &registry_url)
            .ok_or_else(|| Error::InvalidName(name.to_string()))?;
        let force_cache = self.force_cache();
        if let Some(packument) = read_cached_full_packument_typed(&cache_path, force_cache) {
            return Ok(packument);
        }

        // Slow path: full value round-trip covers revalidation + fresh
        // network fetches + all the ETag bookkeeping.
        // `fetch_packument_full_cached` is the single source of truth
        // for those branches; we just re-parse its `Value` into
        // `Packument` here. The one `from_value` walk this still pays
        // is amortized across the network round-trip so it doesn't
        // show up in steady-state resolves.
        let value = self.fetch_packument_full_cached(name, cache_dir).await?;
        let packument: Packument = serde_json::from_value(value)
            .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
        Ok(packument)
    }

    pub async fn fetch_packument_with_time_cached_after_lookup(
        &self,
        name: &str,
        cache_dir: &Path,
        lookup: CachedPackumentLookup,
    ) -> Result<Packument, Error> {
        match lookup.cached {
            Some(CachedPackumentLookupEntry::Full(cached)) => {
                self.revalidate_full_packument_typed(name, cache_dir, cached)
                    .await
            }
            _ => self.fetch_packument_with_time_cached(name, cache_dir).await,
        }
    }

    pub(super) async fn revalidate_full_packument_typed(
        &self,
        name: &str,
        cache_dir: &Path,
        cached: CachedFullPackumentTyped,
    ) -> Result<Packument, Error> {
        let force_cache = self.force_cache();
        if force_cache || cached_is_fresh(cached.fetched_at, cached.max_age_secs) {
            return Ok(cached.packument);
        }
        if self.network_mode == NetworkMode::Offline {
            return Err(Error::Offline(format!("packument for {name}")));
        }

        let registry_url = self.config.registry_for(name).to_string();
        let cache_path = packument_full_cache_path(cache_dir, name, &registry_url)
            .ok_or_else(|| Error::InvalidName(name.to_string()))?;
        let (url, registry_url) = self.packument_url(name);

        // Single-flight: see `fetch_packument_cached_with_entry`.
        // Coalesce concurrent revalidations for the same name into one
        // network conditional-GET; later waiters re-read the warm cache.
        // Released before any retry backoff sleep so waiters don't pay
        // a serialized recovery cost when the winner hits transient errors.
        let sf_key = format!("full:{registry_url}:{name}");
        let sf_mutex = self.packument_singleflight_mutex(sf_key);
        let mut sf_guard = Some(sf_mutex.lock().await);
        if let Some(refreshed) = read_cached_full_packument_typed(&cache_path, force_cache) {
            return Ok(refreshed);
        }

        let label = format!("packument {name}");
        let started = std::time::Instant::now();
        let max_attempts = self.fetch_policy.retries.saturating_add(1);

        for attempt in 0..max_attempts {
            let is_last = attempt + 1 >= max_attempts;
            match {
                let mut req = self
                    .authed_get_for_package(&url, registry_url, name)
                    .header("Accept", PACKUMENT_FULL_ACCEPT);
                if let Some(ref etag) = cached.etag {
                    req = req.header("If-None-Match", etag);
                }
                if let Some(ref lm) = cached.last_modified {
                    req = req.header("If-Modified-Since", lm);
                }
                req
            }
            .send()
            .await
            {
                Ok(resp) if is_retriable_status(resp.status()) && !is_last => {
                    let wait = retry_after_from(&resp)
                        .unwrap_or_else(|| self.fetch_policy.backoff_for_attempt(attempt + 1));
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        status = resp.status().as_u16(),
                        label,
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSIENT,
                        "retrying HTTP request after transient failure",
                    );
                    drop(sf_guard.take());
                    tokio::time::sleep(wait).await;
                }
                Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {
                    self.maybe_record_slow_metadata(&label, started);
                    return Err(Error::NotFound(name.to_string()));
                }
                Ok(resp) if resp.status() == reqwest::StatusCode::NOT_MODIFIED => {
                    let revalidated_max_age =
                        parse_cache_control_max_age(&resp).or(cached.max_age_secs);
                    let to_cache = if let Some(to_cache) = read_cached_full_packument(&cache_path) {
                        to_cache
                    } else {
                        let packument = serde_json::to_value(&cached.packument).map_err(|e| {
                            Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
                        })?;
                        CachedFullPackument {
                            etag: cached.etag.clone(),
                            last_modified: cached.last_modified.clone(),
                            fetched_at: cached.fetched_at,
                            max_age_secs: cached.max_age_secs,
                            packument,
                        }
                    };
                    if let Err(e) = write_cached_full_packument(
                        &cache_path,
                        to_cache.etag.as_deref(),
                        to_cache.last_modified.as_deref(),
                        now_secs(),
                        revalidated_max_age,
                        &to_cache.packument,
                    ) {
                        tracing::warn!(
                            code = aube_codes::warnings::WARN_AUBE_PACKUMENT_CACHE_WRITE,
                            "failed to write packument cache {}: {e}",
                            cache_path.display()
                        );
                    }
                    self.maybe_record_slow_metadata(&label, started);
                    return Ok(cached.packument);
                }
                Ok(resp) => {
                    let (etag, last_modified) = extract_cache_headers(&resp);
                    let max_age_secs = parse_cache_control_max_age(&resp);
                    let resp = resp.error_for_status()?;
                    check_body_cap(&resp, self.fetch_policy.packument_max_bytes, &label)?;
                    match parse_full_response::<serde_json::Value>(resp).await {
                        Ok(value) => {
                            if let Err(e) = write_cached_full_packument(
                                &cache_path,
                                etag.as_deref(),
                                last_modified.as_deref(),
                                now_secs(),
                                max_age_secs,
                                &value,
                            ) {
                                tracing::warn!(
                                    code = aube_codes::warnings::WARN_AUBE_PACKUMENT_CACHE_WRITE,
                                    "failed to write packument cache {}: {e}",
                                    cache_path.display()
                                );
                            }
                            let packument: Packument =
                                serde_json::from_value(value).map_err(|e| {
                                    Error::Io(std::io::Error::new(
                                        std::io::ErrorKind::InvalidData,
                                        e,
                                    ))
                                })?;
                            self.maybe_record_slow_metadata(&label, started);
                            return Ok(packument);
                        }
                        Err(err) if !is_last => {
                            let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                            tracing::warn!(
                                    attempt = attempt + 1,
                                    max_attempts,
                                    backoff_ms = wait.as_millis() as u64,
                                    error = %err,
                                    label,
                                    code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_BODY_DECODE,
                            "retrying HTTP request after response body decode error",
                                );
                            drop(sf_guard.take());
                            tokio::time::sleep(wait).await;
                        }
                        Err(err) => return Err(err),
                    }
                }
                Err(err) if !is_last => {
                    let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        error = %err,
                        label,
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSPORT,
                        "retrying HTTP request after transport error",
                    );
                    drop(sf_guard.take());
                    tokio::time::sleep(wait).await;
                }
                Err(err) => return Err(err.into()),
            }
        }
        unreachable!("retry loop exited without returning; max_attempts was {max_attempts}")
    }

    /// Fetch the abbreviated packument for a package (corgi format).
    pub async fn fetch_packument(&self, name: &str) -> Result<Packument, Error> {
        if self.network_mode == NetworkMode::Offline {
            return Err(Error::Offline(format!("packument for {name}")));
        }
        let (url, registry_url) = self.packument_url(name);
        let label = format!("packument {name}");
        let _diag_full =
            aube_util::diag::Span::new(aube_util::diag::Category::Registry, "fetch_packument")
                .with_meta_fn(|| format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(name)));
        let max_attempts = self.fetch_policy.retries.saturating_add(1);
        let started = std::time::Instant::now();
        for attempt in 0..max_attempts {
            let is_last = attempt + 1 >= max_attempts;
            let _diag_attempt = aube_util::diag::Span::new(
                aube_util::diag::Category::Registry,
                "packument_http_attempt",
            )
            .with_meta_fn(|| {
                format!(
                    r#"{{"name":{},"attempt":{}}}"#,
                    aube_util::diag::jstr(name),
                    attempt + 1
                )
            });
            let _attempt_send_t0 = std::time::Instant::now();
            match {
                let req = self
                    .authed_get_for_package(&url, registry_url, name)
                    // RFC 9218: packument metadata is resolver-blocking,
                    // mark Critical so Cloudflare/Fastly H2 schedulers
                    // prioritize it ahead of pending tarball frames on
                    // the shared connection.
                    .header(
                        "Priority",
                        aube_util::http::priority::header_value(
                            aube_util::http::priority::Urgency::Critical,
                            false,
                        ),
                    );
                if force_full_packument() {
                    req
                } else {
                    req.header("Accept", PACKUMENT_ACCEPT)
                }
            }
            .send()
            .await
            {
                Ok(resp) if is_retriable_status(resp.status()) && !is_last => {
                    let wait = retry_after_from(&resp)
                        .unwrap_or_else(|| self.fetch_policy.backoff_for_attempt(attempt + 1));
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        status = resp.status().as_u16(),
                        label,
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSIENT,
                        "retrying HTTP request after transient failure",
                    );
                    tokio::time::sleep(wait).await;
                }
                Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {
                    self.maybe_record_slow_metadata(&label, started);
                    return Err(Error::NotFound(name.to_string()));
                }
                Ok(resp) => {
                    aube_util::diag::event_lazy(
                        aube_util::diag::Category::Registry,
                        "packument_first_byte",
                        _attempt_send_t0.elapsed(),
                        || {
                            format!(
                                r#"{{"name":{},"status":{}}}"#,
                                aube_util::diag::jstr(name),
                                resp.status().as_u16()
                            )
                        },
                    );
                    let _diag_parse = aube_util::diag::Span::new(
                        aube_util::diag::Category::Registry,
                        "packument_body_parse",
                    )
                    .with_meta_fn(|| format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(name)));
                    let resp = resp.error_for_status()?;
                    check_body_cap(&resp, self.fetch_policy.packument_max_bytes, &label)?;
                    match parse_full_response::<Packument>(resp).await {
                        Ok(packument) => {
                            drop(_diag_parse);
                            self.maybe_record_slow_metadata(&label, started);
                            return Ok(packument);
                        }
                        Err(err) if !is_last => {
                            let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                            tracing::warn!(
                                    attempt = attempt + 1,
                                    max_attempts,
                                    backoff_ms = wait.as_millis() as u64,
                                    error = %err,
                                    label,
                                    code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_BODY_DECODE,
                            "retrying HTTP request after response body decode error",
                                );
                            tokio::time::sleep(wait).await;
                        }
                        Err(err) => return Err(err),
                    }
                }
                Err(err) if !is_last => {
                    let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        error = %err,
                        label,
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSPORT,
                        "retrying HTTP request after transport error",
                    );
                    tokio::time::sleep(wait).await;
                }
                Err(err) => return Err(err.into()),
            }
        }
        unreachable!("retry loop exited without returning; max_attempts was {max_attempts}")
    }

    /// Fetch a packument using a disk-backed cache:
    ///   - If a cached entry exists and is younger than PACKUMENT_TTL_SECS, return it
    ///     immediately (no network).
    ///   - Otherwise, send a conditional request with If-None-Match/If-Modified-Since.
    ///     On 304, refresh the cache timestamp and return the cached body.
    ///   - On 200, write the new packument to disk.
    pub async fn fetch_packument_cached(
        &self,
        name: &str,
        cache_dir: &Path,
    ) -> Result<Packument, Error> {
        let registry_url = self.config.registry_for(name).to_string();
        let cache_path = packument_cache_path(cache_dir, name, &registry_url)
            .ok_or_else(|| Error::InvalidName(name.to_string()))?;
        let cached = read_cached_packument(&cache_path);
        self.fetch_packument_cached_with_entry(name, cache_path, cached)
            .await
    }

    pub async fn fetch_packument_cached_after_lookup(
        &self,
        name: &str,
        cache_dir: &Path,
        lookup: CachedPackumentLookup,
    ) -> Result<Packument, Error> {
        let registry_url = self.config.registry_for(name).to_string();
        let cache_path = packument_cache_path(cache_dir, name, &registry_url)
            .ok_or_else(|| Error::InvalidName(name.to_string()))?;
        let cached = match lookup.cached {
            Some(CachedPackumentLookupEntry::Abbreviated(cached)) => Some(cached),
            _ => read_cached_packument(&cache_path),
        };
        self.fetch_packument_cached_with_entry(name, cache_path, cached)
            .await
    }

    pub(super) async fn fetch_packument_cached_with_entry(
        &self,
        name: &str,
        cache_path: PathBuf,
        cached: Option<CachedPackument>,
    ) -> Result<Packument, Error> {
        // Fast path: trust the cache if it's still fresh.
        // Move out of the wrapper to avoid cloning the Packument.
        // --prefer-offline / --offline extend "fresh" to "any cached entry"
        // so we skip revalidation and, for --offline, the network entirely.
        let force_cache = self.force_cache();
        if let Some(c) = cached.as_ref()
            && (force_cache || cached_is_fresh(c.fetched_at, c.max_age_secs))
        {
            return Ok(cached.unwrap().packument);
        }
        if self.network_mode == NetworkMode::Offline {
            return Err(Error::Offline(format!("packument for {name}")));
        }

        // Single-flight: when a pre-resolver speculative prefetch and
        // the resolver's BFS both ask for the same name within the
        // same install, the first one to land here does the network
        // fetch + cache write. The second one blocks on the per-name
        // tokio Mutex and re-reads the (now warm) disk cache on
        // wake-up, skipping the duplicate GET entirely. Keyed by
        // `corgi:<registry>:<name>` so corgi and full caches stay
        // independent. Drops the std lock immediately — only the
        // tokio Mutex is held across the network await.
        //
        // Released before any retry backoff sleep so a winner stuck
        // in exponential backoff against a flaky registry doesn't
        // serialize the recovery of N concurrent waiters behind it.
        let (url, registry_url) = self.packument_url(name);
        let sf_key = format!("corgi:{registry_url}:{name}");
        let sf_mutex = self.packument_singleflight_mutex(sf_key);
        let mut sf_guard = Some(sf_mutex.lock().await);
        // Re-read the cache under the lock — another task may have
        // populated it while we waited. Costs one disk read per
        // coalesced caller but saves a full HTTP round-trip.
        let cached = match read_cached_packument(&cache_path) {
            Some(c) if force_cache || cached_is_fresh(c.fetched_at, c.max_age_secs) => {
                return Ok(c.packument);
            }
            recheck => recheck.or(cached),
        };

        // Normally we ask for the abbreviated (corgi) response so we
        // get a smaller payload. See `force_full_packument()` for why
        // this escape hatch exists — it is strictly a BATS/fixture
        // workaround, never a user-facing tunable.
        //
        // Revalidation headers are rebuilt per attempt (same contract
        // as `fetch_packument_full_cached`) so retries on 503 keep
        // using the correct `If-None-Match` / `If-Modified-Since`
        // without silently stripping cache hints.
        let cached_ref = cached.as_ref();
        let label = format!("packument {name}");
        let max_attempts = self.fetch_policy.retries.saturating_add(1);
        let started = std::time::Instant::now();
        for attempt in 0..max_attempts {
            let is_last = attempt + 1 >= max_attempts;
            match {
                let mut req = self
                    .authed_get_for_package(&url, registry_url, name)
                    .header(
                        "Priority",
                        aube_util::http::priority::header_value(
                            aube_util::http::priority::Urgency::Critical,
                            false,
                        ),
                    );
                if !force_full_packument() {
                    req = req.header("Accept", PACKUMENT_ACCEPT);
                }
                if let Some(c) = cached_ref {
                    if let Some(ref etag) = c.etag {
                        req = req.header("If-None-Match", etag);
                    }
                    if let Some(ref lm) = c.last_modified {
                        req = req.header("If-Modified-Since", lm);
                    }
                }
                req
            }
            .send()
            .await
            {
                Ok(resp) if is_retriable_status(resp.status()) && !is_last => {
                    let wait = retry_after_from(&resp)
                        .unwrap_or_else(|| self.fetch_policy.backoff_for_attempt(attempt + 1));
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        status = resp.status().as_u16(),
                        label,
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSIENT,
                        "retrying HTTP request after transient failure",
                    );
                    drop(sf_guard.take());
                    tokio::time::sleep(wait).await;
                }
                Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {
                    self.maybe_record_slow_metadata(&label, started);
                    return Err(Error::NotFound(name.to_string()));
                }
                Ok(resp)
                    if resp.status() == reqwest::StatusCode::NOT_MODIFIED && cached.is_some() =>
                {
                    let c = cached.as_ref().unwrap();
                    let revalidated_max_age = parse_cache_control_max_age(&resp).or(c.max_age_secs);
                    let to_cache = CachedPackument {
                        etag: c.etag.clone(),
                        last_modified: c.last_modified.clone(),
                        fetched_at: now_secs(),
                        max_age_secs: revalidated_max_age,
                        packument: c.packument.clone(),
                    };
                    if let Err(e) = write_cached_packument(&cache_path, &to_cache) {
                        tracing::warn!(
                            code = aube_codes::warnings::WARN_AUBE_PACKUMENT_CACHE_WRITE,
                            "failed to write packument cache {}: {e}",
                            cache_path.display()
                        );
                    }
                    self.maybe_record_slow_metadata(&label, started);
                    return Ok(c.packument.clone());
                }
                Ok(resp) => {
                    let (etag, last_modified) = extract_cache_headers(&resp);
                    let max_age_secs = parse_cache_control_max_age(&resp);

                    let resp = resp.error_for_status()?;
                    check_body_cap(&resp, self.fetch_policy.packument_max_bytes, &label)?;
                    match parse_full_response::<Packument>(resp).await {
                        Ok(packument) => {
                            let to_cache = CachedPackument {
                                etag,
                                last_modified,
                                fetched_at: now_secs(),
                                max_age_secs,
                                packument: packument.clone(),
                            };
                            if let Err(e) = write_cached_packument(&cache_path, &to_cache) {
                                tracing::warn!(
                                    code = aube_codes::warnings::WARN_AUBE_PACKUMENT_CACHE_WRITE,
                                    "failed to write packument cache {}: {e}",
                                    cache_path.display()
                                );
                            }
                            self.maybe_record_slow_metadata(&label, started);
                            return Ok(packument);
                        }
                        Err(err) if !is_last => {
                            let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                            tracing::warn!(
                                    attempt = attempt + 1,
                                    max_attempts,
                                    backoff_ms = wait.as_millis() as u64,
                                    error = %err,
                                    label,
                                    code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_BODY_DECODE,
                            "retrying HTTP request after response body decode error",
                                );
                            drop(sf_guard.take());
                            tokio::time::sleep(wait).await;
                        }
                        Err(err) => return Err(err),
                    }
                }
                Err(err) if !is_last => {
                    let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        error = %err,
                        label,
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSPORT,
                        "retrying HTTP request after transport error",
                    );
                    drop(sf_guard.take());
                    tokio::time::sleep(wait).await;
                }
                Err(err) => return Err(err.into()),
            }
        }
        unreachable!("retry loop exited without returning; max_attempts was {max_attempts}")
    }
}

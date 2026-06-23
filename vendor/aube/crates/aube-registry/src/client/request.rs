use super::body::{
    TIMEOUT_RETRY_CAP, check_body_cap, is_retriable_status, read_body_capped,
    read_body_capped_streaming_sha512, retry_after_from,
};
use super::cache::cached_is_fresh;
use super::{RegistryClient, encoded_name};
use crate::{Error, NetworkMode};

impl RegistryClient {
    pub(super) fn registry_url_for(&self, name: &str) -> &str {
        self.config.registry_for(name)
    }

    pub(super) fn force_cache(&self) -> bool {
        matches!(
            self.network_mode,
            NetworkMode::PreferOffline | NetworkMode::Offline
        )
    }

    pub(super) fn trust_cached_packument(
        &self,
        fetched_at: u64,
        max_age_secs: Option<u64>,
    ) -> bool {
        self.force_cache() || cached_is_fresh(fetched_at, max_age_secs)
    }

    /// Build `{registry}/{encoded_name}` — the packument route. Scoped
    /// packages have their `/` encoded as `%2F` so intermediate proxies
    /// that route on path segments (Artifactory's npm remote is the
    /// known offender) don't reject the request with 406. npm-cli and
    /// pnpm encode the same way.
    pub(super) fn packument_url(&self, name: &str) -> (String, &str) {
        let registry_url = self.registry_url_for(name);
        let url = format!(
            "{}/{}",
            registry_url.trim_end_matches('/'),
            encoded_name(name),
        );
        (url, registry_url)
    }

    pub(super) fn authed_get_for_package(
        &self,
        url: &str,
        registry_url: &str,
        package_name: &str,
    ) -> reqwest::RequestBuilder {
        self.authed_request_for_package(reqwest::Method::GET, url, registry_url, package_name)
    }

    /// Build an HTTP request using this registry's configured TLS client
    /// and auth fallback order: bearer token, tokenHelper, then basic auth.
    pub fn authed_request(
        &self,
        method: reqwest::Method,
        url: &str,
        registry_url: &str,
    ) -> reqwest::RequestBuilder {
        self.authed(
            self.http_for(registry_url).request(method, url),
            registry_url,
        )
    }

    pub fn authed_request_for_package(
        &self,
        method: reqwest::Method,
        url: &str,
        registry_url: &str,
        package_name: &str,
    ) -> reqwest::RequestBuilder {
        self.authed_for_package(
            self.http_for_package(registry_url, package_name)
                .request(method, url),
            registry_url,
            package_name,
        )
    }

    /// Build an HTTP request using the TLS/proxy client selected for this
    /// registry, but leave authentication to the caller. Publish uses this
    /// for npm Trusted Publishing exchange tokens so an old `.npmrc` token
    /// cannot be sent alongside the short-lived OIDC-derived bearer token.
    pub fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        registry_url: &str,
    ) -> reqwest::RequestBuilder {
        self.http_for(registry_url).request(method, url)
    }

    pub fn has_resolved_auth_for(&self, registry_url: &str) -> bool {
        self.registry_auth_token_for(registry_url).is_some()
            || self.config.basic_auth_for(registry_url).is_some()
    }

    pub fn has_resolved_auth_for_package(&self, registry_url: &str, package_name: &str) -> bool {
        self.registry_auth_token_for_package(registry_url, Some(package_name))
            .is_some()
            || self
                .config
                .basic_auth_for_package(registry_url, package_name)
                .is_some()
    }

    /// Attach auth headers to any `RequestBuilder` keyed off the registry
    /// that owns `registry_url`. Shared between the GET helpers and the
    /// dist-tag / deprecate PUT calls so every write request picks up the
    /// same token/basic-auth resolution as reads. Future token-type
    /// changes (e.g. web-flow refresh) only have to be made here.
    pub(super) fn authed(
        &self,
        req: reqwest::RequestBuilder,
        registry_url: &str,
    ) -> reqwest::RequestBuilder {
        if let Some(token) = self.registry_auth_token_for(registry_url) {
            req.bearer_auth(token)
        } else if let Some(auth) = self.config.basic_auth_for(registry_url) {
            req.header("Authorization", format!("Basic {auth}"))
        } else {
            req
        }
    }

    fn registry_auth_token_for(&self, registry_url: &str) -> Option<String> {
        self.registry_auth_token_for_package(registry_url, None)
    }

    pub(super) fn authed_for_package(
        &self,
        req: reqwest::RequestBuilder,
        registry_url: &str,
        package_name: &str,
    ) -> reqwest::RequestBuilder {
        if let Some(token) = self.registry_auth_token_for_package(registry_url, Some(package_name))
        {
            req.bearer_auth(token)
        } else if let Some(auth) = self
            .config
            .basic_auth_for_package(registry_url, package_name)
        {
            req.header("Authorization", format!("Basic {auth}"))
        } else {
            req
        }
    }

    fn registry_auth_token_for_package(
        &self,
        registry_url: &str,
        package_name: Option<&str>,
    ) -> Option<String> {
        let cache_key = match package_name {
            Some(name) => format!("{registry_url}\0{name}"),
            None => registry_url.to_string(),
        };
        // Fast path: memoized result. Hit on the second-and-later
        // request to the same registry URL within one process.
        if let Ok(cache) = self.auth_token_by_url.lock()
            && let Some(cached) = cache.get(&cache_key)
        {
            return cached.clone();
        }
        let auth_config = match package_name {
            Some(name) => self.config.registry_config_for_package(registry_url, name),
            None => self.config.registry_config_for(registry_url),
        };
        let resolved = if let Some(auth) = auth_config {
            if let Some(token) = auth.auth_token.as_ref() {
                Some(token.to_string())
            } else if let Some(helper) = auth.token_helper.as_deref() {
                self.cached_token_helper_result(helper)
            } else {
                None
            }
        } else {
            None
        };
        if let Ok(mut cache) = self.auth_token_by_url.lock() {
            cache.insert(cache_key, resolved.clone());
        }
        resolved
    }

    /// Cache key is the helper command itself, not the registry URL:
    /// `run_token_helper` spawns the helper as a subprocess that returns
    /// a token determined entirely by the command, with no URL input.
    /// Keying by URL would defeat the cache for tarball fetches (each
    /// tarball has a unique path) and re-spawn the helper hundreds of
    /// times during a large install.
    fn cached_token_helper_result(&self, helper: &str) -> Option<String> {
        {
            let cache = self.token_helper_cache.lock().ok()?;
            if let Some(token) = cache.get(helper) {
                return token.clone();
            }
        }
        let token = crate::config::run_token_helper(helper);
        if let Ok(mut cache) = self.token_helper_cache.lock() {
            cache.insert(helper.to_string(), token.clone());
        }
        token
    }

    pub(super) fn http_for(&self, registry_url: &str) -> &reqwest::Client {
        let uri_key = crate::config::registry_uri_key_pub(registry_url);
        crate::config::lookup_by_uri_prefix(&self.http_by_uri, &uri_key).unwrap_or(&self.http)
    }

    pub(super) fn http_for_package(
        &self,
        registry_url: &str,
        package_name: &str,
    ) -> &reqwest::Client {
        if let Some((prefix, scope, _)) = self
            .config
            .scoped_tls_config_for_package(registry_url, package_name)
            && let Some(client) = self
                .http_by_uri_scope
                .get(prefix)
                .and_then(|by_scope| by_scope.get(scope))
        {
            return client;
        }
        self.http_for(registry_url)
    }

    /// Pick the right HTTP client for tarball body downloads. The
    /// default registry uses the dedicated h1 client. Per-uri
    /// authed registries (corporate Artifactory, GitHub Packages)
    /// fall through to their h2 client because they're rare and
    /// keeping a parallel h1 map for them is not worth the
    /// complexity until measurement shows it matters.
    pub(super) fn http_tarball_for(&self, registry_url: &str) -> &reqwest::Client {
        let uri_key = crate::config::registry_uri_key_pub(registry_url);
        crate::config::lookup_by_uri_prefix(&self.http_by_uri, &uri_key)
            .unwrap_or(&self.http_tarball)
    }

    pub(super) fn http_tarball_for_package(
        &self,
        registry_url: &str,
        package_name: &str,
    ) -> &reqwest::Client {
        if let Some((prefix, scope, _)) = self
            .config
            .scoped_tls_config_for_package(registry_url, package_name)
            && let Some(client) = self
                .http_by_uri_scope
                .get(prefix)
                .and_then(|by_scope| by_scope.get(scope))
        {
            return client;
        }
        self.http_tarball_for(registry_url)
    }

    /// Authed RequestBuilder routed through the tarball-specific
    /// client. Mirrors [`Self::authed_get`] but picks
    /// [`Self::http_tarball_for`] instead of [`Self::http_for`].
    pub(super) fn authed_tarball_get(
        &self,
        url: &str,
        registry_url: &str,
    ) -> reqwest::RequestBuilder {
        if let Some(package_name) = package_name_from_tarball_url(url) {
            let req = self
                .http_tarball_for_package(registry_url, &package_name)
                .request(reqwest::Method::GET, url);
            // Default path: resolve auth against the tarball's own URL. A
            // tarball on the same origin as the configured registry picks
            // up its credentials; a tarball on a *different* origin (a
            // separate CDN) resolves to nothing and is sent
            // unauthenticated — npm's default.
            if self.has_resolved_auth_for_package(url, &package_name) {
                return self.authed_for_package(req, url, &package_name);
            }
            // `always-auth` widening: the per-URL lookup found nothing, but
            // the package's home registry has `always-auth` set, so attach
            // that registry's credentials even though the tarball lives on
            // a different origin. Keyed off the home registry URL so the
            // existing prefix lookup resolves the configured token.
            let home_registry = self.config.registry_for(&package_name);
            if self.config.always_auth_for(home_registry) {
                return self.authed_for_package(req, home_registry, &package_name);
            }
            self.authed_for_package(req, url, &package_name)
        } else {
            let req = self
                .http_tarball_for(registry_url)
                .request(reqwest::Method::GET, url);
            if self.has_resolved_auth_for(url) || !self.config.always_auth_for(registry_url) {
                self.authed(req, url)
            } else {
                self.authed(req, registry_url)
            }
        }
    }

    /// Same as [`Self::send_with_retry`] but also returns wall-clock
    /// elapsed from the first `.send()` to the returned response. Used
    /// by metadata call sites to compare against `fetchWarnTimeoutMs`
    /// without double-timing the retry backoff from caller code.
    pub(super) async fn send_with_retry_timed<F>(
        &self,
        build: F,
    ) -> Result<(reqwest::Response, std::time::Duration), reqwest::Error>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let started = std::time::Instant::now();
        let max_attempts = self.fetch_policy.retries.saturating_add(1);
        for attempt in 0..max_attempts {
            let is_last = attempt + 1 >= max_attempts;
            match build().send().await {
                Ok(resp) => {
                    let status = resp.status();
                    // Retry on 5xx server errors and 429 rate-limit.
                    // Everything else — 2xx/3xx successes and 4xx
                    // client errors the caller needs to see (404,
                    // 401, 403) — is returned verbatim.
                    if !is_retriable_status(status) || is_last {
                        return Ok((resp, started.elapsed()));
                    }
                    // 429 may carry a `Retry-After` header; honor it
                    // (seconds form) so a rate-limited registry gets
                    // the wait it asked for instead of our default
                    // exponential backoff. `make-fetch-happen` does
                    // the same. HTTP-date form is rare for npm and
                    // `chrono` isn't a dep — parse as u64 seconds or
                    // fall back to the computed backoff.
                    let wait = retry_after_from(&resp)
                        .unwrap_or_else(|| self.fetch_policy.backoff_for_attempt(attempt + 1));
                    drop(resp);
                    // Surfaces at WARN so users see retry activity in
                    // the install output. The final failure still
                    // propagates up as a user-facing error if every
                    // attempt fails.
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        status = status.as_u16(),
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSIENT,
                        "retrying HTTP request after transient failure",
                    );
                    tokio::time::sleep(wait).await;
                }
                Err(e) => {
                    if is_last {
                        return Err(e);
                    }
                    let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        error = %e,
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSPORT,
                        "retrying HTTP request after transport error",
                    );
                    tokio::time::sleep(wait).await;
                }
            }
        }
        // `FetchPolicy::retries` is `u32`, so `max_attempts =
        // retries + 1` is always ≥ 1 and the loop runs at least once;
        // every path inside the loop either returns or continues. An
        // exit past this point is a structural bug, not a runtime
        // input the caller can provoke.
        unreachable!("retry loop exited without returning; max_attempts was {max_attempts}")
    }

    /// Metadata-request wrapper around [`Self::send_with_retry_timed`]
    /// that records a slow-metadata entry when total wall-clock
    /// (including any retry backoff) exceeds `fetchWarnTimeoutMs`. `0`
    /// disables the recording, matching pnpm's convention and the
    /// default in `settings.toml`.
    ///
    /// Per-event detail goes into [`crate::slow_metadata`], not the
    /// log stream — the install pipeline emits one summary warning
    /// after resolve via [`crate::slow_metadata::flush_summary`].
    ///
    /// Not used by tarball downloads — `fetchMinSpeedKiBps` is the
    /// tarball-side observability knob, and the two warnings are
    /// semantically distinct (headers latency vs. body throughput).
    pub(super) async fn send_metadata_with_retry<F>(
        &self,
        label: &str,
        build: F,
    ) -> Result<reqwest::Response, reqwest::Error>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let (resp, elapsed) = self.send_with_retry_timed(build).await?;
        let threshold = self.fetch_policy.warn_timeout_ms;
        let elapsed_ms = elapsed.as_millis() as u64;
        if threshold > 0 && elapsed_ms > threshold {
            crate::slow_metadata::record(label, elapsed_ms, threshold);
        }
        Ok(resp)
    }

    pub(super) fn maybe_record_slow_metadata(&self, label: &str, started: std::time::Instant) {
        let threshold = self.fetch_policy.warn_timeout_ms;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if threshold > 0 && elapsed_ms > threshold {
            crate::slow_metadata::record(label, elapsed_ms, threshold);
        }
    }

    /// Streaming variant of `retry_bytes_body_read`. Returns the body
    /// bytes along with a SHA-512 digest computed incrementally during
    /// the chunk read loop. Same retry semantics as the buffered path.
    /// Used by `fetch_tarball_bytes_streaming_sha512` so callers can
    /// skip the post-buffer hash pass.
    pub(super) async fn retry_bytes_body_read_streaming_sha512<F>(
        &self,
        label: &str,
        cap: u64,
        build: F,
    ) -> Result<(bytes::Bytes, [u8; 64], std::time::Duration), Error>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let max_attempts = self.fetch_policy.retries.saturating_add(1);
        let mut timeout_retries: u32 = 0;
        for attempt in 0..max_attempts {
            let is_last = attempt + 1 >= max_attempts;
            match build().send().await {
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
                Ok(resp) => {
                    let resp = resp.error_for_status()?;
                    check_body_cap(&resp, cap, label)?;
                    let started = std::time::Instant::now();
                    match read_body_capped_streaming_sha512(resp, cap, label).await {
                        Ok((bytes, sha512)) => return Ok((bytes, sha512, started.elapsed())),
                        Err(err) if !is_last => {
                            let is_timeout = matches!(&err, Error::Http(e) if e.is_timeout());
                            if is_timeout && timeout_retries >= TIMEOUT_RETRY_CAP {
                                return Err(err);
                            }
                            if is_timeout {
                                timeout_retries += 1;
                            }
                            let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                            tracing::warn!(
                                attempt = attempt + 1,
                                max_attempts,
                                backoff_ms = wait.as_millis() as u64,
                                error = %err,
                                label,
                                code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_BODY_READ,
                                "retrying HTTP request after response body read error",
                            );
                            tokio::time::sleep(wait).await;
                        }
                        Err(err) => return Err(err),
                    }
                }
                Err(err) if !is_last => {
                    if err.is_timeout() {
                        if timeout_retries >= TIMEOUT_RETRY_CAP {
                            return Err(Error::Http(err));
                        }
                        timeout_retries += 1;
                    }
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
                Err(err) => return Err(Error::Http(err)),
            }
        }
        unreachable!("retry loop exited without returning; max_attempts was {max_attempts}")
    }

    pub(super) async fn retry_bytes_body_read<F>(
        &self,
        label: &str,
        cap: u64,
        build: F,
    ) -> Result<(bytes::Bytes, std::time::Duration), Error>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let max_attempts = self.fetch_policy.retries.saturating_add(1);
        let mut timeout_retries: u32 = 0;
        for attempt in 0..max_attempts {
            let is_last = attempt + 1 >= max_attempts;
            match build().send().await {
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
                Ok(resp) => {
                    let resp = resp.error_for_status()?;
                    check_body_cap(&resp, cap, label)?;
                    let started = std::time::Instant::now();
                    match read_body_capped(resp, cap, label).await {
                        Ok(bytes) => return Ok((bytes, started.elapsed())),
                        Err(err) if !is_last => {
                            let is_timeout = matches!(&err, Error::Http(e) if e.is_timeout());
                            if is_timeout && timeout_retries >= TIMEOUT_RETRY_CAP {
                                return Err(err);
                            }
                            if is_timeout {
                                timeout_retries += 1;
                            }
                            let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                            tracing::warn!(
                                attempt = attempt + 1,
                                max_attempts,
                                backoff_ms = wait.as_millis() as u64,
                                error = %err,
                                label,
                                code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_BODY_READ,
                                "retrying HTTP request after response body read error",
                            );
                            tokio::time::sleep(wait).await;
                        }
                        Err(err) => return Err(err),
                    }
                }
                Err(err) if !is_last => {
                    if err.is_timeout() {
                        if timeout_retries >= TIMEOUT_RETRY_CAP {
                            return Err(Error::Http(err));
                        }
                        timeout_retries += 1;
                    }
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
                Err(err) => return Err(Error::Http(err)),
            }
        }
        unreachable!("retry loop exited without returning; max_attempts was {max_attempts}")
    }
}

fn package_name_from_tarball_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let segments: Vec<&str> = parsed.path_segments()?.collect();
    if let Some(dash_idx) = segments.iter().position(|segment| *segment == "-")
        && dash_idx >= 1
    {
        let name = segments[dash_idx - 1];
        if let Some((scope, name)) = name.split_once("%2F").or_else(|| name.split_once("%2f"))
            && scope.starts_with('@')
        {
            return Some(format!("{scope}/{name}"));
        }
        if dash_idx >= 2 && segments[dash_idx - 2].starts_with('@') {
            return Some(format!("{}/{}", segments[dash_idx - 2], name));
        }
        return Some(name.to_string());
    }
    for (idx, segment) in segments.iter().enumerate() {
        if let Some((scope, name)) = segment
            .split_once("%2F")
            .or_else(|| segment.split_once("%2f"))
            && scope.starts_with('@')
        {
            return Some(format!("{scope}/{name}"));
        }
        if segment.starts_with('@') {
            let name = segments.get(idx + 1)?;
            return Some(format!("{segment}/{name}"));
        }
    }
    segments.first().map(|segment| (*segment).to_string())
}

#[cfg(test)]
mod tests {
    use super::{RegistryClient, package_name_from_tarball_url};
    use crate::config::{AuthConfig, NpmConfig};

    #[test]
    fn package_name_from_tarball_url_handles_scoped_paths() {
        assert_eq!(
            package_name_from_tarball_url("https://registry.example.com/@org/pkg/-/pkg-1.0.0.tgz")
                .as_deref(),
            Some("@org/pkg")
        );
        assert_eq!(
            package_name_from_tarball_url(
                "https://registry.example.com/@org%2Fpkg/-/pkg-1.0.0.tgz"
            )
            .as_deref(),
            Some("@org/pkg")
        );
        assert_eq!(
            package_name_from_tarball_url(
                "https://registry.example.com/npm/@org/pkg/-/pkg-1.0.0.tgz"
            )
            .as_deref(),
            Some("@org/pkg")
        );
        assert_eq!(
            package_name_from_tarball_url("https://registry.example.com/lodash/-/lodash-1.0.0.tgz")
                .as_deref(),
            Some("lodash")
        );
        assert_eq!(
            package_name_from_tarball_url(
                "https://registry.example.com/npm/lodash/-/lodash-1.0.0.tgz"
            )
            .as_deref(),
            Some("lodash")
        );
    }

    #[test]
    fn http_for_package_uses_scoped_tls_client() {
        let mut config = NpmConfig {
            registry: "https://registry.example.com/".to_string(),
            ..Default::default()
        };
        let mut scoped = AuthConfig::default();
        scoped.tls.cafile = Some(std::path::PathBuf::from("org-a-ca.pem"));
        config
            .scoped_auth_by_uri
            .entry("//registry.example.com/".to_string())
            .or_default()
            .insert("@org-a".to_string(), scoped);
        let client = RegistryClient::from_config(config);

        let default_client = client.http_for("https://registry.example.com/") as *const _;
        let org_client =
            client.http_for_package("https://registry.example.com/", "@org-a/pkg") as *const _;
        let other_client =
            client.http_for_package("https://registry.example.com/", "@org-b/pkg") as *const _;

        assert_ne!(org_client, default_client);
        assert_eq!(other_client, default_client);
    }

    #[test]
    fn tarball_request_uses_scoped_auth_for_path_registry() {
        let mut config = NpmConfig::default();
        let registry_auth = AuthConfig {
            auth_token: Some("registry-token".to_string()),
            ..Default::default()
        };
        config
            .auth_by_uri
            .insert("//registry.example.com/".to_string(), registry_auth);

        let scoped_auth = AuthConfig {
            auth_token: Some("scoped-token".to_string()),
            ..Default::default()
        };
        config
            .scoped_auth_by_uri
            .entry("//registry.example.com/npm".to_string())
            .or_default()
            .insert("@myorg".to_string(), scoped_auth);
        let client = RegistryClient::from_config(config);

        let req = client
            .authed_tarball_get(
                "https://registry.example.com/npm/@myorg/pkg/-/pkg-1.0.0.tgz",
                "https://registry.example.com/npm/@myorg/pkg/-/pkg-1.0.0.tgz",
            )
            .build()
            .unwrap();
        assert_eq!(
            req.headers().get(reqwest::header::AUTHORIZATION),
            Some(&reqwest::header::HeaderValue::from_static(
                "Bearer scoped-token"
            )),
        );

        let req = client
            .authed_tarball_get(
                "https://registry.example.com/npm-release/@myorg/pkg/-/pkg-1.0.0.tgz",
                "https://registry.example.com/npm-release/@myorg/pkg/-/pkg-1.0.0.tgz",
            )
            .build()
            .unwrap();
        assert_eq!(
            req.headers().get(reqwest::header::AUTHORIZATION),
            Some(&reqwest::header::HeaderValue::from_static(
                "Bearer registry-token"
            )),
        );
    }

    #[test]
    fn cross_host_tarball_is_unauthenticated_by_default() {
        // A tarball on a different origin than the configured registry is
        // sent without credentials unless `always-auth` is set — npm's
        // default, and the behavior `always-auth` exists to override.
        let mut config = NpmConfig {
            registry: "https://registry.example.com/".to_string(),
            ..Default::default()
        };
        config.auth_by_uri.insert(
            "//registry.example.com/".to_string(),
            AuthConfig {
                auth_token: Some("registry-token".to_string()),
                ..Default::default()
            },
        );
        let client = RegistryClient::from_config(config);

        let req = client
            .authed_tarball_get(
                "https://cdn.example.net/lodash/-/lodash-1.0.0.tgz",
                "https://cdn.example.net/lodash/-/lodash-1.0.0.tgz",
            )
            .build()
            .unwrap();
        assert!(
            req.headers().get(reqwest::header::AUTHORIZATION).is_none(),
            "cross-host tarball must not leak the registry token by default"
        );
    }

    #[test]
    fn always_auth_attaches_registry_token_to_cross_host_tarball() {
        // With `always-auth` set for the home registry, its token is
        // attached even to a tarball hosted on a different origin.
        let mut config = NpmConfig {
            registry: "https://registry.example.com/".to_string(),
            ..Default::default()
        };
        config.auth_by_uri.insert(
            "//registry.example.com/".to_string(),
            AuthConfig {
                auth_token: Some("registry-token".to_string()),
                always_auth: true,
                ..Default::default()
            },
        );
        let client = RegistryClient::from_config(config);

        let req = client
            .authed_tarball_get(
                "https://cdn.example.net/lodash/-/lodash-1.0.0.tgz",
                "https://cdn.example.net/lodash/-/lodash-1.0.0.tgz",
            )
            .build()
            .unwrap();
        assert_eq!(
            req.headers().get(reqwest::header::AUTHORIZATION),
            Some(&reqwest::header::HeaderValue::from_static(
                "Bearer registry-token"
            )),
            "always-auth must attach the home registry's token cross-host"
        );
    }
}

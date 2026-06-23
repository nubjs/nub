use super::{RegistryClient, build_http_client, build_http_tarball_client};
use crate::NetworkMode;
use crate::config::{FetchPolicy, NpmConfig};
use std::collections::BTreeMap;
use std::sync::Mutex;

impl RegistryClient {
    pub fn new(registry_url: &str) -> Self {
        // `NpmConfig::load` folds proxy env vars into the config so
        // that `from_config` can later call `.no_proxy()` on the
        // reqwest builder and still honor them. This constructor
        // skips `load` (it has no `.npmrc` to read), so call
        // `apply_proxy_env` directly — otherwise disabling reqwest's
        // auto-detection would silently strip `HTTPS_PROXY` /
        // `HTTP_PROXY` support from every caller that uses
        // `RegistryClient::new` or `::default`.
        let mut config = NpmConfig {
            registry: crate::config::normalize_registry_url_pub(registry_url),
            ..Default::default()
        };
        config.apply_proxy_env();
        Self::from_config(config)
    }

    /// Build a client with the default [`FetchPolicy`]. Callers that
    /// have already resolved a [`ResolveCtx`] should prefer
    /// [`Self::from_config_with_policy`] so env / workspace-yaml /
    /// `.npmrc` overrides to the `fetch*` settings take effect.
    pub fn from_config(config: NpmConfig) -> Self {
        Self::from_config_with_policy(config, FetchPolicy::default())
    }

    /// Build a client with an explicit [`FetchPolicy`]. This is the
    /// primary constructor used by `aube::commands::make_client`,
    /// which resolves the policy from the full settings precedence
    /// chain before calling in.
    pub fn from_config_with_policy(config: NpmConfig, fetch_policy: FetchPolicy) -> Self {
        let http = build_http_client(&config, None, &fetch_policy);
        let http_tarball = build_http_tarball_client(&config, None, &fetch_policy);
        let mut http_by_uri = BTreeMap::new();
        for (uri, registry) in &config.auth_by_uri {
            if !registry.has_tls_material() {
                continue;
            }
            http_by_uri.insert(
                uri.clone(),
                build_http_client(&config, Some(registry), &fetch_policy),
            );
        }
        let mut http_by_uri_scope = BTreeMap::new();
        for (uri, by_scope) in &config.scoped_auth_by_uri {
            for (scope, registry) in by_scope {
                if !registry.has_tls_material() {
                    continue;
                }
                http_by_uri_scope
                    .entry(uri.clone())
                    .or_insert_with(BTreeMap::new)
                    .insert(
                        scope.clone(),
                        build_http_client(&config, Some(registry), &fetch_policy),
                    );
            }
        }

        Self {
            http,
            http_by_uri,
            http_by_uri_scope,
            http_tarball,
            token_helper_cache: Mutex::new(BTreeMap::new()),
            auth_token_by_url: Mutex::new(BTreeMap::new()),
            packument_in_flight: Mutex::new(aube_util::collections::FxMap::default()),
            config,
            network_mode: NetworkMode::Online,
            fetch_policy,
        }
    }

    /// Return (and lazily insert) the per-name mutex from
    /// `packument_in_flight`. Held in a `Mutex<FxMap>`: the std lock
    /// is only held for the find-or-insert, not for the actual network
    /// fetch — that's gated by the returned tokio `Mutex`. Callers
    /// pass a `key` distinct per cache variant (corgi vs full) per
    /// registry URL so concurrent fetches of the same name against
    /// different caches don't serialize through each other.
    pub(super) fn packument_singleflight_mutex(
        &self,
        key: String,
    ) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        let mut map = self
            .packument_in_flight
            .lock()
            .expect("packument_in_flight mutex poisoned");
        map.entry(key)
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Force this client into a given network mode (online, prefer-offline,
    /// offline). Consumed by `install` when the user passes `--offline` or
    /// `--prefer-offline`.
    pub fn with_network_mode(mut self, mode: NetworkMode) -> Self {
        self.network_mode = mode;
        self
    }

    /// Fire-and-forget HEAD request against the configured registry to
    /// warm the TLS + TCP + HTTP/2 handshake before the resolver starts
    /// requesting packuments. Saves one round-trip on cold installs
    /// (~50-150 ms on a 50 ms-RTT path) by overlapping the handshake
    /// with manifest parsing.
    ///
    /// `AUBE_DISABLE_SPECULATIVE_TLS=1` skips the prewarm. Wrong
    /// registry, network failure, or auth rejection are all silently
    /// dropped: the response is discarded; subsequent real requests
    /// take the standard path.
    pub fn prewarm_connection(&self) {
        if matches!(self.network_mode, NetworkMode::Offline) {
            return;
        }
        // HEAD on every distinct registry root the install may touch:
        // the default registry, every scoped registry from `.npmrc`
        // (`@org:registry=...`), and every per-uri auth registry that
        // owns its own pool. Prewarming only the default registry
        // forces the first scoped/auth-uri packument to pay the full
        // TLS+TCP+ALPN cost on the cold path.
        //
        // `aube_util::http::prewarm` honors `AUBE_DISABLE_SPECULATIVE_TLS=1`.
        let mut targets: Vec<(reqwest::Client, String)> =
            vec![(self.http.clone(), self.config.registry.clone())];
        // Lowercase + trim trailing `/` so `Registry.NPMjs.org` and
        // `https://registry.npmjs.org/` collapse to the same prewarm
        // target. URL hosts are case-insensitive per RFC 3986 §3.2.2.
        let normalize = |u: &str| u.trim_end_matches('/').to_ascii_lowercase();
        for url in self.config.scoped_registries.values() {
            let trimmed = normalize(url);
            if !targets.iter().any(|(_, u)| normalize(u) == trimmed) {
                let client = self.http_for(url).clone();
                targets.push((client, url.clone()));
            }
        }
        // The HEAD requests below populate hickory-dns's in-process
        // cache as a side effect of issuing the request. A separate
        // `tokio::net::lookup_host` preresolve would only warm the
        // OS-level resolver (getaddrinfo), which reqwest's hickory
        // path does not consult. So the prewarm itself is the DNS
        // warm-up; no extra lookup needed.
        aube_util::http::prewarm::spawn_head(targets);
    }

    pub fn network_mode(&self) -> NetworkMode {
        self.network_mode
    }

    pub fn uses_default_npm_registry_for(&self, name: &str) -> bool {
        self.registry_url_for(name).trim_end_matches('/') == "https://registry.npmjs.org"
    }
}

impl Default for RegistryClient {
    fn default() -> Self {
        Self::new("https://registry.npmjs.org")
    }
}

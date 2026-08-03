use super::{
    LazyHttpClient, RegistryClient, build_http_client, build_http_tarball_client,
    load_node_extra_ca_certs,
};
use crate::NetworkMode;
use crate::config::{AuthConfig, FetchPolicy, NpmConfig};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

fn lazy_http_client(
    config: NpmConfig,
    registry_config: Option<AuthConfig>,
    fetch_policy: FetchPolicy,
    extra_ca_certs: Arc<OnceLock<Vec<reqwest::Certificate>>>,
    tarball: bool,
) -> LazyHttpClient {
    LazyHttpClient::new(move || {
        let extra_ca_certs = extra_ca_certs.get_or_init(load_node_extra_ca_certs);
        let client = if tarball {
            build_http_tarball_client(
                &config,
                registry_config.as_ref(),
                &fetch_policy,
                extra_ca_certs,
            )
        } else {
            build_http_client(
                &config,
                registry_config.as_ref(),
                &fetch_policy,
                extra_ca_certs,
            )
        };
        client.map_err(Into::into)
    })
}
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
        let mut config = NpmConfig::default();
        // `new` predates fallible command setup and remains convenient for
        // library callers. Preserve malformed explicit input rather than
        // silently retaining an empty/default registry; a request will fail
        // locally instead of being redirected to npmjs.
        config.registry = crate::config::normalize_registry_url_pub(registry_url)
            .unwrap_or_else(|| registry_url.trim().to_string());
        config.apply_proxy_env();
        Self::from_config(config)
    }

    /// Build a client with the default [`FetchPolicy`]. Callers that
    /// have already resolved a `ResolveCtx` should prefer
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
        // The root bundle is also lazy: a cache-only/offline resolution must
        // not touch certificate files or initialize a TLS backend. Once a
        // route does need a client, every route shares this parsed bundle.
        let extra_ca_certs = Arc::new(OnceLock::new());
        let http = lazy_http_client(
            config.clone(),
            None,
            fetch_policy,
            Arc::clone(&extra_ca_certs),
            false,
        );
        Self::from_config_with_http(config, fetch_policy, http, extra_ca_certs)
    }

    fn from_config_with_http(
        config: NpmConfig,
        fetch_policy: FetchPolicy,
        http: LazyHttpClient,
        extra_ca_certs: Arc<OnceLock<Vec<reqwest::Certificate>>>,
    ) -> Self {
        let http_tarball = lazy_http_client(
            config.clone(),
            None,
            fetch_policy,
            Arc::clone(&extra_ca_certs),
            true,
        );
        let mut http_by_uri = BTreeMap::new();
        for (uri, registry) in &config.auth_by_uri {
            if registry.has_tls_material() {
                http_by_uri.insert(
                    uri.clone(),
                    lazy_http_client(
                        config.clone(),
                        Some(registry.clone()),
                        fetch_policy,
                        Arc::clone(&extra_ca_certs),
                        false,
                    ),
                );
            }
        }
        let mut http_by_uri_scope = BTreeMap::new();
        for (uri, by_scope) in &config.scoped_auth_by_uri {
            for (scope, registry) in by_scope {
                if registry.has_tls_material() {
                    http_by_uri_scope.entry(uri.clone()).or_insert_with(BTreeMap::new).insert(
                        scope.clone(),
                        lazy_http_client(
                            config.clone(),
                            Some(registry.clone()),
                            fetch_policy,
                            Arc::clone(&extra_ca_certs),
                            false,
                        ),
                    );
                }
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
            named_routes: std::sync::RwLock::new(BTreeMap::new()),
        }
    }

    #[cfg(test)]
    pub(super) fn from_config_with_http_factory(
        config: NpmConfig,
        factory: impl FnOnce() -> Result<reqwest::Client, crate::Error> + Send + 'static,
    ) -> Self {
        let fetch_policy = FetchPolicy::default();
        let extra_ca_certs = Arc::new(OnceLock::new());
        Self::from_config_with_http(
            config,
            fetch_policy,
            LazyHttpClient::new(factory),
            extra_ca_certs,
        )
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

    /// Fire-and-forget HEAD request against already-initialized registry
    /// clients. Construction remains demand-driven: a cache-only or offline
    /// resolve must not open a TLS factory merely for speculative warming.
    ///
    /// `AUBE_DISABLE_SPECULATIVE_TLS=1` skips the prewarm. Wrong registry,
    /// network failure, or auth rejection are all silently dropped: the
    /// response is discarded; subsequent real requests take the standard path.
    pub fn prewarm_connection(&self) {
        if matches!(self.network_mode, NetworkMode::Offline) {
            return;
        }
        let Some(default) = self.http.initialized() else {
            return;
        };
        // `aube_util::http::prewarm` honors `AUBE_DISABLE_SPECULATIVE_TLS=1`.
        let mut targets = vec![(default.clone(), self.config.registry.clone())];
        // Lowercase + trim trailing `/` so `Registry.NPMjs.org` and
        // `https://registry.npmjs.org/` collapse to the same prewarm target.
        let normalize = |u: &str| u.trim_end_matches('/').to_ascii_lowercase();
        for url in self.config.scoped_registries.values() {
            let trimmed = normalize(url);
            if targets.iter().any(|(_, target)| normalize(target) == trimmed) {
                continue;
            }
            let client = crate::config::registry_uri_key_pub(url)
                .and_then(|uri_key| {
                    crate::config::lookup_by_uri_prefix(&self.http_by_uri, &uri_key)
                })
                .and_then(LazyHttpClient::initialized);
            if let Some(client) = client {
                targets.push((client.clone(), url.clone()));
            }
        }
        // The HEAD requests below populate hickory-dns's in-process cache as
        // a side effect of issuing the request; no extra resolver warm-up is
        // needed.
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

#[cfg(test)]
mod tests {
    use super::RegistryClient;
    use crate::config::NpmConfig;
    use crate::{Error, Packument};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn packument() -> Packument {
        Packument {
            name: "demo".to_owned(),
            modified: None,
            versions: BTreeMap::new(),
            dist_tags: BTreeMap::new(),
            time: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn warm_packument_cache_does_not_initialize_http_client() {
        let calls = Arc::new(AtomicUsize::new(0));
        let factory_calls = Arc::clone(&calls);
        let client = RegistryClient::from_config_with_http_factory(
            NpmConfig {
                registry: "https://registry.example.test/".to_owned(),
                ..Default::default()
            },
            move || {
                factory_calls.fetch_add(1, Ordering::SeqCst);
                Err(Error::Io(std::io::Error::other("test HTTP factory failure")))
            },
        );
        let cache = tempfile::tempdir().expect("cache tempdir");
        client.seed_packument_cache("demo", cache.path(), &packument(), None, None, true);

        assert_eq!(
            client
                .fetch_packument_cached("demo", cache.path())
                .await
                .expect("fresh cache result")
                .name,
            "demo"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn remote_fetch_initializes_once_and_preserves_factory_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let factory_calls = Arc::clone(&calls);
        let client = RegistryClient::from_config_with_http_factory(
            NpmConfig {
                registry: "https://registry.example.test/".to_owned(),
                ..Default::default()
            },
            move || {
                factory_calls.fetch_add(1, Ordering::SeqCst);
                Err(Error::Io(std::io::Error::other("test HTTP factory failure")))
            },
        );

        for _ in 0..2 {
            let error = client.fetch_packument("demo").await.expect_err("factory failure");
            let Error::HttpClientInitialization(source) = error else {
                panic!("expected retained client factory error");
            };
            assert!(source.to_string().contains("test HTTP factory failure"));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

use crate::NetworkMode;
use crate::config::{FetchPolicy, NpmConfig};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

mod access;
mod body;
mod cache;
mod dist_tags;
mod endpoints;
mod http;
mod lifecycle;
mod npm_verbs;
mod packument;
mod parse;
mod request;
mod tarball;

#[cfg(test)]
mod retry_tests;
#[cfg(test)]
mod seed_tests;
#[cfg(test)]
mod slow_tarball_tests;

pub use cache::CachedPackumentLookup;
use dist_tags::*;
pub use endpoints::PackageSearchResult;
use http::*;
pub use npm_verbs::{Owner, TokenInfo};
use parse::parse_full_response;

/// Accept header for packument requests. `vnd.npm.install-v1+json` is the
/// abbreviated (corgi) format npmjs emits for installs; the `application/json`
/// fallback covers registries (Verdaccio, older Artifactory, private mirrors)
/// whose proxy layer normalizes Accept and would otherwise return 406 on the
/// corgi-only form. `*/*` keeps us compatible with anything that strips the
/// fancy media types entirely. Same shape npm-cli / pnpm send.
const PACKUMENT_ACCEPT: &str =
    "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8, */*";

/// Accept header for the full (non-corgi) packument route used by `aube view`
/// and mutating commands. Adds `*/*` as a fallback for the same reason as
/// `PACKUMENT_ACCEPT` — some proxies won't serve JSON unless it's in the list.
const PACKUMENT_FULL_ACCEPT: &str = "application/json; q=1.0, */*";

// Packument and tarball body caps are configurable via the
// `packumentMaxBytes` / `tarballMaxBytes` settings. Defaults live in
// `FetchPolicy::default()`; setting either to `0` disables the cap.
// These are hardening knobs against hostile or misconfigured
// registries streaming runaway bodies into the resolver.

/// Hard cap for the `/-/npm/v1/security/advisories/bulk` response. The
/// body scales with the number of distinct `<name>@<version>` pairs in
/// the request, which is bounded by the lockfile. 256 MiB gives an
/// extremely generous upper bound for monorepos with tens of thousands
/// of locked versions.
const AUDIT_BODY_CAP: u64 = 256 << 20;

type HttpClientFactory = Box<dyn FnOnce() -> Result<reqwest::Client, crate::Error> + Send>;

/// Defers HTTP/TLS setup until a request actually needs this client. Both a
/// successful client and a construction error are cached, so concurrent
/// callers neither rebuild TLS state nor turn one bad configuration into a
/// factory storm.
pub(super) struct LazyHttpClient {
    result: OnceLock<Result<reqwest::Client, Arc<crate::Error>>>,
    factory: Mutex<Option<HttpClientFactory>>,
}

impl LazyHttpClient {
    pub(super) fn new(
        factory: impl FnOnce() -> Result<reqwest::Client, crate::Error> + Send + 'static,
    ) -> Self {
        Self {
            result: OnceLock::new(),
            factory: Mutex::new(Some(Box::new(factory))),
        }
    }

    pub(super) fn get(&self) -> Result<&reqwest::Client, crate::Error> {
        let result = self.result.get_or_init(|| {
            let factory = self
                .factory
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            match factory {
                Some(factory) => factory().map_err(Arc::new),
                None => Err(Arc::new(crate::Error::HttpClientFactoryUnavailable)),
            }
        });
        result
            .as_ref()
            .map_err(|error| crate::Error::HttpClientInitialization(Arc::clone(error)))
    }

    pub(super) fn initialized(&self) -> Option<&reqwest::Client> {
        self.result.get().and_then(|result| result.as_ref().ok())
    }

    #[cfg(test)]
    pub(super) fn ready(client: reqwest::Client) -> Self {
        let result = OnceLock::new();
        let _ = result.set(Ok(client));
        Self {
            result,
            factory: Mutex::new(None),
        }
    }
}

/// Client for interacting with the npm registry.
pub struct RegistryClient {
    http: LazyHttpClient,
    http_by_uri: BTreeMap<String, LazyHttpClient>,
    http_by_uri_scope: BTreeMap<String, BTreeMap<String, LazyHttpClient>>,
    /// HTTP/1.1-only client used for tarball body downloads. See
    /// [`build_http_tarball_client`] for the rationale (h2 stream
    /// queueing on a single connection vs h1's parallel TCP per
    /// request). All metadata (packument, dist-tag, deprecate)
    /// stays on `http` so h2 multiplexing + header compression
    /// still apply where they help.
    http_tarball: LazyHttpClient,
    token_helper_cache: Mutex<BTreeMap<String, Option<String>>>,
    /// Memoized result of `registry_auth_token_for(url)`. Without this,
    /// every authed request walks `auth_by_uri` for a longest-prefix
    /// match against `registry_url`. On a 2000-package install that's
    /// 2000 × O(N_uris × strcmp) wasted lookups. The token is fixed
    /// for the lifetime of the process (helpers are already memoized
    /// in `token_helper_cache`), so per-URL caching is safe.
    auth_token_by_url: Mutex<BTreeMap<String, Option<String>>>,
    /// Single-flight gate for concurrent packument fetches. Keyed by
    /// `<variant>:<registry_url>:<name>` so corgi and full lookups
    /// against the same name are independently coalesced. The first
    /// task to acquire the per-key tokio Mutex does the real network
    /// fetch + cache write; later tasks block on the same mutex and
    /// re-read the (now warm) disk cache on wake-up, skipping the
    /// duplicate GET. Without this, a pre-resolver speculative
    /// prefetch races against the resolver's own BFS fetches for the
    /// same name and we pay 2× bandwidth + 2× server load on every
    /// overlap. Entries are never removed — the lock itself is tiny
    /// (`Arc<tokio::sync::Mutex<()>>`) and bounded by the dep graph
    /// size for the install duration.
    packument_in_flight:
        Mutex<aube_util::collections::FxMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    config: NpmConfig,
    network_mode: NetworkMode,
    fetch_policy: FetchPolicy,
    /// pnpm `namedRegistries` routing: package name → the aliased registry
    /// URL its `<alias>:<spec>` resolved to. Consulted FIRST by
    /// [`RegistryClient::registry_url_for`] so a named-registry package fetches
    /// (and caches) against its aliased host instead of the default registry.
    /// Populated by the resolver's `preprocess_task` as it rewrites each
    /// `<alias>:` spec. Interior-mutable + request-scoped; empty under any
    /// non-pnpm posture, which makes `registry_url_for` behave exactly as
    /// before (falls straight through to `config.registry_for`). `RwLock`
    /// rather than `Mutex` because the map is read on every fetch but written
    /// only once per named-registry spec.
    named_routes: std::sync::RwLock<BTreeMap<String, String>>,
}

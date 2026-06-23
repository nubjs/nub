use crate::config::{FetchPolicy, NpmConfig};
use std::path::Path;

/// Add inline PEM strings and a PEM-bundle file to a reqwest client
/// builder as additional trust roots. Shared between the top-level
/// (unscoped) and per-registry cert paths so both go through the same
/// parse + warn pipeline.
fn apply_extra_root_certs(
    mut builder: reqwest::ClientBuilder,
    ca: &[String],
    cafile: Option<&Path>,
    scope: &str,
) -> reqwest::ClientBuilder {
    for pem in ca {
        match reqwest::Certificate::from_pem(pem.as_bytes()) {
            Ok(cert) => builder = builder.add_root_certificate(cert),
            Err(e) => tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_INVALID_CA,
                "ignoring invalid {scope} ca: {e}"
            ),
        }
    }
    if let Some(cafile) = cafile {
        match std::fs::read(cafile) {
            Ok(bytes) => match reqwest::Certificate::from_pem_bundle(&bytes) {
                Ok(certs) => {
                    for cert in certs {
                        builder = builder.add_root_certificate(cert);
                    }
                }
                Err(e) => tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_INVALID_CAFILE,
                    "ignoring invalid {scope} cafile {}: {e}",
                    cafile.display()
                ),
            },
            Err(e) => tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_UNREADABLE_CAFILE,
                "ignoring unreadable {scope} cafile {}: {e}",
                cafile.display()
            ),
        }
    }
    builder
}

pub(super) fn build_http_client(
    config: &NpmConfig,
    registry_config: Option<&crate::config::AuthConfig>,
    fetch_policy: &FetchPolicy,
    extra_ca_certs: &[reqwest::Certificate],
) -> reqwest::Client {
    build_http_client_inner(config, registry_config, fetch_policy, extra_ca_certs, false)
}

/// Load the PEM bundle named by `NODE_EXTRA_CA_CERTS` as extra trust
/// roots, to be added to every client built from one config.
///
/// `NODE_EXTRA_CA_CERTS` is the standard Node convention for appending
/// a CA to the default trust store; npm/pnpm inherit it transitively
/// because they run on Node, and aube reads it explicitly so native
/// installs stay compatible with corporate MITM proxies and self-signed
/// registries configured the Node way.
///
/// Loaded once per [`RegistryClient`] construction (see
/// `from_config_with_policy`) rather than inside each builder — one
/// `RegistryClient` builds the default client, the tarball client, and
/// one client per per-registry / scoped override, so reading and
/// parsing the bundle per builder would repeat the work N times. The
/// returned certs are additive and lowest-priority: applied before the
/// `.npmrc` `ca` / `cafile` roots, on top of the OS / webpki roots
/// (ordering is immaterial — `add_root_certificate` forms a union). A
/// missing, unreadable, or malformed file is warned and yields an empty
/// list, never fatal — matching the `.npmrc cafile` handling.
pub(super) fn load_node_extra_ca_certs() -> Vec<reqwest::Certificate> {
    let Some(path) = std::env::var_os("NODE_EXTRA_CA_CERTS").filter(|value| !value.is_empty())
    else {
        return Vec::new();
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_UNREADABLE_CAFILE,
                "ignoring unreadable NODE_EXTRA_CA_CERTS {}: {e}",
                Path::new(&path).display()
            );
            return Vec::new();
        }
    };
    match reqwest::Certificate::from_pem_bundle(&bytes) {
        Ok(certs) => certs,
        Err(e) => {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_INVALID_CAFILE,
                "ignoring invalid NODE_EXTRA_CA_CERTS {}: {e}",
                Path::new(&path).display()
            );
            Vec::new()
        }
    }
}

/// HTTP/1.1-only variant for tarball downloads. Tarballs are large
/// opaque blobs where h2 multiplexing buys nothing: there are no
/// compressible headers, and a single slow tarball stream causes
/// head-of-line blocking for every other in-flight tarball on the
/// same h2 connection. npm's CDN advertises
/// `SETTINGS_MAX_CONCURRENT_STREAMS` ≈ 100-128, so a 256-permit
/// tarball semaphore over a single h2 connection queues 128+
/// requests inside hyper waiting for streams. A diag-trace cold
/// install observed `tarball_stream_open` mean 565ms (n=1230,
/// 3242ms on critical path) — that's server-side h2 stream
/// queueing, not TLS or network.
///
/// Switching to h1 lets reqwest's connection pool open as many
/// parallel TCP connections to `registry.npmjs.org` as we have
/// in-flight tarball requests (capped by `pool_max_idle_per_host`),
/// matching what npm/pnpm/yarn already do for the same reason.
/// Packument requests stay on the h2 client because gzip+brotli
/// header compression and request multiplexing are real wins for
/// thousands of small JSON payloads.
pub(super) fn build_http_tarball_client(
    config: &NpmConfig,
    registry_config: Option<&crate::config::AuthConfig>,
    fetch_policy: &FetchPolicy,
    extra_ca_certs: &[reqwest::Certificate],
) -> reqwest::Client {
    build_http_client_inner(config, registry_config, fetch_policy, extra_ca_certs, true)
}

fn build_http_client_inner(
    config: &NpmConfig,
    registry_config: Option<&crate::config::AuthConfig>,
    fetch_policy: &FetchPolicy,
    extra_ca_certs: &[reqwest::Certificate],
    for_tarball: bool,
) -> reqwest::Client {
    // `maxsockets` (when set) overrides the default pool size. pnpm
    // documents this as "concurrent connections per origin"; reqwest
    // doesn't expose a hard cap, but `pool_max_idle_per_host` is the
    // closest knob and is what downstream users actually care about.
    let pool_max_idle = config.max_sockets.unwrap_or(64);
    // CDN edge cache hit rate keys partly off the User-Agent header.
    // Hardcoded `0.1.0` lands in cold buckets on Cloudflare/Fastly. Use
    // the real workspace version + an OS/arch tail in the same shape
    // pnpm and npm send so the registry recognises us.
    static UA: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let user_agent = UA.get_or_init(|| {
        format!(
            "{} ({} {})",
            aube_util::embedder().user_agent,
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    });
    let mut builder = aube_util::http::with_webpki_root_fallback(reqwest::Client::builder())
        .user_agent(user_agent)
        // Wire-level decompression for packument JSON. Tarball
        // requests explicitly send `Accept-Encoding: identity`
        // (tarballs are already gzip on the payload), so this only
        // affects metadata calls. Popular packuments (`react`,
        // `webpack`, `next`) drop 3-5x on the wire when gzipped.
        .gzip(true)
        .brotli(true)
        .zstd(true)
        // `fetchTimeout` — applied to the whole response (headers +
        // body) via reqwest's single-knob timeout. pnpm / npm expose
        // this as `fetch-timeout` in `.npmrc`; the default matches
        // npm's 60s. Without this override reqwest would use its
        // built-in 30s default, which is tighter than pnpm's.
        .timeout(std::time::Duration::from_millis(fetch_policy.timeout_ms))
        // Bigger connection pool so concurrent fetches don't queue on a small set of conns.
        // HTTP/2 (when negotiated via ALPN, which npm registry supports) multiplexes many
        // requests over a single connection so this mostly matters for fallback HTTP/1.1.
        .pool_max_idle_per_host(pool_max_idle)
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .tcp_nodelay(true);
    if !for_tarball {
        builder = builder
            .http2_keep_alive_interval(std::time::Duration::from_secs(30))
            .http2_keep_alive_timeout(std::time::Duration::from_secs(20))
            .http2_keep_alive_while_idle(true)
            .http2_adaptive_window(true)
            .http2_initial_stream_window_size(Some(16 * 1024 * 1024))
            .http2_initial_connection_window_size(Some(16 * 1024 * 1024))
            .http2_max_frame_size(Some(16 * 1024 * 1024 - 1));
    } else {
        builder = builder.http1_only();
    }
    builder = builder
        .tcp_keepalive(std::time::Duration::from_secs(60))
        // In-process DNS caching via hickory-dns. The system resolver
        // does not cache and uses a thread pool for `getaddrinfo`,
        // which serializes the first cold lookup per origin. hickory
        // resolves async + caches for the process lifetime.
        .hickory_dns(true)
        // `strict-ssl=false` disables cert validation entirely. This
        // is a security hole on purpose: corporate registries should
        // prefer per-registry `ca` / `cafile` so validation stays on.
        .danger_accept_invalid_certs(!config.strict_ssl)
        // rustls already defaults to TLS 1.2+, but pinning the floor
        // here makes the policy explicit so a future default-loosening
        // upstream does not silently re-enable TLS 1.1 for aube.
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        // Block https to http downgrades on redirect. reqwest already
        // strips Authorization on cross-host redirects as of 0.12, so
        // this policy only adds the scheme guard. A 302 from a good
        // registry to `http://evil/` would otherwise leak whatever
        // header survived into cleartext.
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("too many redirects");
            }
            if let Some(prev) = attempt.previous().last()
                && prev.scheme() == "https"
                && attempt.url().scheme() != "https"
            {
                return attempt.stop();
            }
            attempt.follow()
        }))
        // Disable reqwest's built-in `system-proxy` auto-detection
        // before installing any explicit proxies. Without this, the
        // builder would silently read `HTTP(S)_PROXY` / `NO_PROXY`
        // from the environment *on top of* the values we already
        // pulled into `NpmConfig`, so a `.npmrc` that overrides an
        // env-var proxy would be ignored for one scheme and honored
        // for the other, and `noproxy` bypasses would only apply to
        // the manually-configured proxies. `NpmConfig::load` now
        // folds the env vars into the config itself, so this crate
        // is the single source of truth for proxy state.
        .no_proxy();

    if let Some(ip) = config.local_address {
        builder = builder.local_address(Some(ip));
    }

    let no_proxy = config
        .no_proxy
        .as_deref()
        .and_then(reqwest::NoProxy::from_string);

    if let Some(ref url) = config.https_proxy {
        match reqwest::Proxy::https(url) {
            Ok(mut p) => {
                if let Some(ref np) = no_proxy {
                    p = p.no_proxy(Some(np.clone()));
                }
                builder = builder.proxy(p);
            }
            Err(e) => tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_INVALID_HTTPS_PROXY,
                "ignoring https-proxy {url:?}: {e}"
            ),
        }
    }
    if let Some(ref url) = config.http_proxy {
        match reqwest::Proxy::http(url) {
            Ok(mut p) => {
                if let Some(ref np) = no_proxy {
                    p = p.no_proxy(Some(np.clone()));
                }
                builder = builder.proxy(p);
            }
            Err(e) => tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_INVALID_HTTP_PROXY,
                "ignoring http-proxy {url:?}: {e}"
            ),
        }
    }

    // `NODE_EXTRA_CA_CERTS` roots, loaded once per `RegistryClient`
    // construction (see `load_node_extra_ca_certs`) and shared across
    // every client this config builds. Additive and lowest-priority:
    // applied before the `.npmrc` `ca` / `cafile` roots; ordering is
    // immaterial since `add_root_certificate` forms a union.
    for cert in extra_ca_certs {
        builder = builder.add_root_certificate(cert.clone());
    }

    // Top-level `cafile` / `ca` (unscoped npmrc keys) apply to every
    // client built from this config, matching npm/pnpm semantics.
    builder = apply_extra_root_certs(builder, &config.ca, config.cafile.as_deref(), "top-level");

    if let Some(registry_config) = registry_config {
        builder = apply_extra_root_certs(
            builder,
            &registry_config.tls.ca,
            registry_config.tls.cafile.as_deref(),
            "per-registry",
        );
        if let (Some(cert), Some(key)) = (&registry_config.tls.cert, &registry_config.tls.key) {
            let mut pem = Vec::with_capacity(cert.len() + key.len() + 1);
            pem.extend_from_slice(cert.as_bytes());
            if !cert.ends_with('\n') {
                pem.push(b'\n');
            }
            pem.extend_from_slice(key.as_bytes());
            match reqwest::Identity::from_pem(&pem) {
                Ok(identity) => builder = builder.identity(identity),
                Err(e) => tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_INVALID_CLIENT_CERT,
                    "ignoring invalid per-registry client cert/key: {e}"
                ),
            }
        }
    }

    builder.build().expect("failed to build HTTP client")
}

/// BATS-fixture escape hatch: ask the registry for the unabbreviated
/// packument instead of the corgi (`application/vnd.npm.install-v1+json`)
/// shape. Our Verdaccio-backed fixture strips `bundledDependencies`
/// when it projects stored packuments to corgi, so the
/// `test/bundled_dependencies.bats` suite sets this to exercise the
/// resolver's bundled-skip path end-to-end. Production registries
/// include `bundleDependencies` in corgi per the npm spec, so the
/// default path stays cheap.
///
/// The name is deliberately `AUBE_INTERNAL_*` so nothing outside the
/// test harness grows a habit of relying on it, and we require the
/// exact literal `"1"` (not just any non-empty value) so an inherited
/// or accidentally-set empty value won't silently balloon registry
/// traffic on end-user machines.
pub(super) fn force_full_packument() -> bool {
    aube_util::env::embedder_env("INTERNAL_FORCE_FULL_PACKUMENT")
        .as_deref()
        .is_some_and(|v| v == "1")
}

#[cfg(test)]
mod tests {
    use super::{build_http_client, load_node_extra_ca_certs};
    use crate::config::{FetchPolicy, NpmConfig};
    use std::ffi::{OsStr, OsString};

    /// A minimal self-signed certificate. Lives in
    /// `tests/fixtures/test-ca.pem` so the base64 body stays out of the
    /// spellchecker dictionary. Regenerate with:
    ///
    /// ```text
    /// openssl req -x509 -newkey rsa:2048 -nodes -days 36500 \
    ///     -subj '/CN=aube-test' -keyout /dev/null \
    ///     -out crates/aube-registry/tests/fixtures/test-ca.pem
    /// ```
    ///
    /// The private key is discarded — nothing pins a specific issuer or
    /// fingerprint, the fixture only needs to be a valid X.509 PEM.
    const TEST_CA_FIXTURE: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test-ca.pem");

    /// RAII guard for `NODE_EXTRA_CA_CERTS`: serializes env-mutating
    /// tests on a process-wide lock and restores the variable's prior
    /// value (or absence) on drop — *including on panic*. The prior
    /// value is snapshotted once at acquisition, before any mutation,
    /// so a `.expect()` failure mid-test can never leak the test's
    /// value into a sibling test or re-capture a stale value on the
    /// next run (the failure mode of a manual restore-at-the-end).
    struct EnvVarGuard {
        prior: Option<OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvVarGuard {
        fn acquire() -> Self {
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let lock = LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            EnvVarGuard {
                prior: std::env::var_os("NODE_EXTRA_CA_CERTS"),
                _lock: lock,
            }
        }

        fn set(&self, value: impl AsRef<OsStr>) {
            // SAFETY: the guard holds the process-wide lock, so no other
            // guard-using test mutates the environment concurrently.
            unsafe { std::env::set_var("NODE_EXTRA_CA_CERTS", value) };
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: the lock is still held; restore the pre-test value
            // (or remove it if it was unset) even when the test panicked.
            unsafe {
                match &self.prior {
                    Some(value) => std::env::set_var("NODE_EXTRA_CA_CERTS", value),
                    None => std::env::remove_var("NODE_EXTRA_CA_CERTS"),
                }
            }
        }
    }

    /// `load_node_extra_ca_certs` parses a valid bundle into trust
    /// roots, and treats a missing / unreadable / malformed file as an
    /// empty list (warned, never fatal) so client construction always
    /// succeeds. `build_http_client` accepts whatever it returns.
    #[test]
    fn node_extra_ca_certs_is_loaded_and_failures_are_non_fatal() {
        let env = EnvVarGuard::acquire();

        // Empty value: nothing to add.
        env.set("");
        assert!(load_node_extra_ca_certs().is_empty());

        // A valid PEM bundle parses into one trust root, and a client
        // built with it succeeds (`build_http_client` ends in
        // `.expect(...)`, so a build failure would panic the test).
        env.set(TEST_CA_FIXTURE);
        let certs = load_node_extra_ca_certs();
        assert_eq!(certs.len(), 1);
        drop(build_http_client(
            &NpmConfig::default(),
            None,
            &FetchPolicy::default(),
            &certs,
        ));

        // A readable file that isn't valid PEM: warned, ignored → empty.
        let bad =
            std::env::temp_dir().join(format!("aube-node-extra-ca-{}.pem", std::process::id()));
        std::fs::write(&bad, b"not a certificate").expect("write temp ca bundle");
        env.set(&bad);
        assert!(load_node_extra_ca_certs().is_empty());
        let _ = std::fs::remove_file(&bad);

        // A nonexistent file: unreadable, warned, ignored → empty.
        env.set("/aube/does-not-exist.pem");
        assert!(load_node_extra_ca_certs().is_empty());
        // `env` restores NODE_EXTRA_CA_CERTS on drop, even on panic.
    }
}

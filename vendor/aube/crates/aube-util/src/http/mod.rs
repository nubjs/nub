//! HTTP client helpers reused across aube crates.
//!
//! The npm registry path is dominated by cold TCP+TLS handshakes,
//! per-origin DNS lookups, and per-request priority noise. Each helper
//! here addresses one of those costs without owning a `reqwest::Client`
//! itself — call sites keep their builders and pass them in.
//!
//! Killswitch convention follows aube-util: every optimization that
//! defaults ON ships an `AUBE_DISABLE_*` env var. Each killswitch is
//! named in the doc comment of the function reading it so cargo doc
//! enumerates them.

pub mod prewarm;
pub mod priority;
pub mod race;
pub mod resolve;
pub mod ticket_cache;

/// Add Mozilla's baked-in root bundle as extra trust roots while keeping
/// reqwest's rustls-platform-verifier OS trust store active.
///
/// reqwest 0.13 can merge extra roots with the platform verifier on Unix
/// (except Android) and Windows. On other targets, leave the builder alone
/// so client construction does not fail at runtime.
pub fn with_webpki_root_fallback(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    #[cfg(any(all(unix, not(target_os = "android")), target_os = "windows"))]
    {
        let certs = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|cert| {
                reqwest::Certificate::from_der(cert.as_ref())
                    // webpki-root-certs is generated as valid DER; failure means the dependency is corrupt.
                    .expect("webpki root certificate must be valid DER")
            })
            .collect::<Vec<_>>();
        builder.tls_certs_merge(certs)
    }

    #[cfg(not(any(all(unix, not(target_os = "android")), target_os = "windows")))]
    {
        builder
    }
}

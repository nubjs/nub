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
/// (except Android) and Windows. On Android the platform verifier resolves
/// the OS trust store through JNI and aborts without a JVM (`ndk-context:
/// android context was not initialized`) — the Termux/CLI case, where no
/// app context exists — so there the webpki roots become the ONLY trust
/// roots (`tls_certs_only`), bypassing the verifier entirely. Hickory DNS
/// is disabled on Android for the same reason: reqwest's `hickory-dns`
/// feature defaults the resolver on for every client, and hickory reads
/// Android's DNS config through the same JNI surface. On other targets,
/// leave the builder alone so client construction does not fail at runtime.
///
/// Compiled to a no-op when the `rustls` feature is not enabled — reqwest's
/// `Certificate`/`tls_certs_merge` APIs only exist with a TLS backend, and
/// this crate leaves that choice to the final binary (the aube binary selects
/// rustls via aube-registry's defaults).
pub fn with_webpki_root_fallback(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    #[cfg(all(feature = "rustls", any(unix, target_os = "windows")))]
    let certs = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|cert| {
            reqwest::Certificate::from_der(cert.as_ref())
                // webpki-root-certs is generated as valid DER; failure means the dependency is corrupt.
                .expect("webpki root certificate must be valid DER")
        })
        .collect::<Vec<_>>();

    #[cfg(all(
        feature = "rustls",
        any(all(unix, not(target_os = "android")), target_os = "windows")
    ))]
    {
        builder.tls_certs_merge(certs)
    }

    #[cfg(all(feature = "rustls", target_os = "android"))]
    {
        builder.tls_certs_only(certs).no_hickory_dns()
    }

    #[cfg(not(all(feature = "rustls", any(unix, target_os = "windows"))))]
    {
        builder
    }
}

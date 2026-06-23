//! Thin HTTP layer for nodejs.org (and mirrors): a shared client and
//! a retrying GET. Deliberately not `aube-registry`'s `RegistryClient`
//! — that type drags in npm auth/packument machinery irrelevant to
//! dist downloads, and keeping this crate off it keeps the compile
//! graph parallel. Retry semantics mirror
//! `aube-registry/src/client/tarball.rs`: request-level retries with
//! exponential backoff on transport errors, 5xx, and 429; `Retry-After`
//! honored when present.

use crate::error::Error;
use std::time::Duration;

pub(crate) struct Http {
    client: reqwest::Client,
    retries: u32,
}

pub(crate) struct HttpResponse {
    pub(crate) etag: Option<String>,
    pub(crate) last_modified: Option<String>,
    pub(crate) body: Option<reqwest::Response>,
}

impl Http {
    pub(crate) fn new(retries: u32) -> Self {
        static UA: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let user_agent = UA.get_or_init(|| {
            format!(
                "aube/{} ({} {})",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        });
        let client = aube_util::http::with_webpki_root_fallback(reqwest::Client::builder())
            .user_agent(user_agent)
            // Archives are already compressed; metadata (index.json)
            // benefits from gzip. reqwest handles per-request override
            // via Accept-Encoding when we stream archives.
            .gzip(true)
            .timeout(Duration::from_secs(120))
            .tcp_nodelay(true)
            .build()
            .expect("reqwest client construction cannot fail with static config");
        Http { client, retries }
    }

    /// GET `url`, optionally conditional (`If-None-Match` /
    /// `If-Modified-Since`). Returns the open response for the caller
    /// to consume; 304 yields `body: None`.
    pub(crate) async fn get(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
        identity_encoding: bool,
    ) -> Result<HttpResponse, Error> {
        self.get_with_bearer(url, etag, last_modified, identity_encoding, None)
            .await
    }

    /// [`Self::get`] with an optional bearer token (GitHub API calls
    /// attach `GITHUB_TOKEN` to dodge the 60/hr unauthenticated
    /// per-IP limit on shared CI/NAT addresses).
    pub(crate) async fn get_with_bearer(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
        identity_encoding: bool,
        bearer: Option<&str>,
    ) -> Result<HttpResponse, Error> {
        validate_url(url)?;
        let mut attempt = 0u32;
        loop {
            let mut req = self.client.get(url);
            if let Some(token) = bearer {
                req = req.bearer_auth(token);
            }
            if let Some(tag) = etag {
                req = req.header(reqwest::header::IF_NONE_MATCH, tag);
            }
            if let Some(lm) = last_modified {
                req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
            }
            if identity_encoding {
                req = req.header(reqwest::header::ACCEPT_ENCODING, "identity");
            }
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status == reqwest::StatusCode::NOT_MODIFIED {
                        return Ok(HttpResponse {
                            etag: header_string(&resp, reqwest::header::ETAG),
                            last_modified: header_string(&resp, reqwest::header::LAST_MODIFIED),
                            body: None,
                        });
                    }
                    if status.is_success() {
                        return Ok(HttpResponse {
                            etag: header_string(&resp, reqwest::header::ETAG),
                            last_modified: header_string(&resp, reqwest::header::LAST_MODIFIED),
                            body: Some(resp),
                        });
                    }
                    let retriable = status.is_server_error()
                        || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
                    if !retriable || attempt >= self.retries {
                        return Err(Error::DownloadFailed {
                            url: url.to_string(),
                            reason: format!("HTTP {status}"),
                        });
                    }
                    let wait = retry_after(&resp).unwrap_or_else(|| backoff(attempt));
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSIENT,
                        url,
                        status = %status,
                        backoff_ms = wait.as_millis() as u64,
                        "retrying node dist fetch"
                    );
                    tokio::time::sleep(wait).await;
                }
                Err(e) => {
                    if attempt >= self.retries {
                        return Err(Error::DownloadFailed {
                            url: url.to_string(),
                            reason: e.to_string(),
                        });
                    }
                    let wait = backoff(attempt);
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSPORT,
                        url,
                        error = %e,
                        backoff_ms = wait.as_millis() as u64,
                        "retrying node dist fetch after transport error"
                    );
                    tokio::time::sleep(wait).await;
                }
            }
            attempt += 1;
        }
    }
}

fn header_string(resp: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let secs: u64 = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs.min(30)))
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500u64.saturating_mul(1 << attempt.min(4)))
}

/// Same belt-and-suspenders scheme check as
/// `aube-registry`'s `validate_tarball_url`.
fn validate_url(url: &str) -> Result<(), Error> {
    if url.starts_with("https://") || url.starts_with("http://") {
        Ok(())
    } else {
        Err(Error::DownloadFailed {
            url: url.to_string(),
            reason: "only http(s) URLs are allowed".to_string(),
        })
    }
}

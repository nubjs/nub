use super::RegistryClient;
use super::body::{
    TIMEOUT_RETRY_CAP, check_body_cap, is_retriable_status, retry_after_from, warn_slow_tarball,
};
use crate::{Error, NetworkMode};

fn validate_tarball_url(client: &RegistryClient, url: &str) -> Result<(), Error> {
    // Refuse non-http(s) tarball URLs at the aube boundary so
    // attacker-controlled `dist.tarball` from a hostile mirror
    // cannot reach `file:///` (local file disclosure) or the
    // ssh / git transports inside reqwest. Belt-and-suspenders
    // against transport-layer regressions.
    let safe_url = aube_util::url::redact_url(url);
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| Error::Io(std::io::Error::other(format!("invalid tarball url: {e}"))))?;
    match parsed.scheme() {
        "https" | "http" => {}
        scheme => {
            return Err(Error::Io(std::io::Error::other(format!(
                "tarball {safe_url}: refusing scheme {scheme:?}",
            ))));
        }
    }
    if client.network_mode == NetworkMode::Offline {
        return Err(Error::Offline(format!("tarball {safe_url}")));
    }
    Ok(())
}

impl RegistryClient {
    /// Download a tarball and return the bytes.
    ///
    /// Emits a `fetchMinSpeedKiBps` warning when the end-to-end average
    /// throughput of the body read falls below the configured threshold.
    /// Average (not instantaneous) speed because the call path is a
    /// single `resp.bytes().await?` — we keep the eager-read model and
    /// still give operators a signal for flaky links. `fetchWarnTimeoutMs`
    /// does *not* fire here: that one is scoped to metadata requests
    /// per its pnpm documentation, and the tarball-specific analogue
    /// is the min-speed warning.
    pub async fn fetch_tarball_bytes(&self, url: &str) -> Result<bytes::Bytes, Error> {
        validate_tarball_url(self, url)?;
        // Tarball URLs may point to any registry, try to match auth.
        // Pass the full tarball URL through so longest-prefix matching
        // in `registry_config_for` can find path-scoped auth entries
        // (e.g. `//host/artifactory/npm/`). Tarballs are already gzip
        // archives, so ask intermediaries not to wrap them in HTTP
        // content encoding that can fail independently of the payload.
        // Retries cover transient 5xx / 429 / connection errors; see
        // [`Self::send_with_retry`].
        let (bytes, body_elapsed) = self
            .retry_bytes_body_read(url, self.fetch_policy.tarball_max_bytes, || {
                self.authed_tarball_get(url, url)
                    .header(reqwest::header::ACCEPT_ENCODING, "identity")
            })
            .await?;
        warn_slow_tarball(
            self.fetch_policy.min_speed_kibps,
            url,
            bytes.len(),
            body_elapsed,
        );
        Ok(bytes)
    }

    /// Streaming variant of `fetch_tarball_bytes`. Returns the body
    /// bytes plus the SHA-512 of the on-the-wire payload, computed
    /// incrementally during the chunk read loop. Callers that already
    /// know the lockfile-pinned `integrity` field can compare against
    /// this digest directly and skip the second hash pass that
    /// `aube_store::verify_integrity` would otherwise do over the
    /// owned `Bytes`.
    ///
    /// `AUBE_DISABLE_STREAMING_SHA512=1` is the killswitch: callers
    /// can short-circuit to `fetch_tarball_bytes` and re-hash on the
    /// import side. The killswitch lives in the caller (so it can pick
    /// the buffered path without still paying the streaming cost), not
    /// in this method.
    pub async fn fetch_tarball_bytes_streaming_sha512(
        &self,
        url: &str,
    ) -> Result<(bytes::Bytes, [u8; 64]), Error> {
        let _diag = aube_util::diag::Span::new(
            aube_util::diag::Category::Registry,
            "tarball_buffered_with_sha512",
        )
        .with_meta_fn(|| {
            format!(
                r#"{{"url":{}}}"#,
                aube_util::diag::jstr(&aube_util::url::redact_url(url))
            )
        });
        validate_tarball_url(self, url)?;
        let (bytes, sha512, body_elapsed) = self
            .retry_bytes_body_read_streaming_sha512(
                url,
                self.fetch_policy.tarball_max_bytes,
                || {
                    self.authed_tarball_get(url, url)
                        .header(reqwest::header::ACCEPT_ENCODING, "identity")
                },
            )
            .await?;
        warn_slow_tarball(
            self.fetch_policy.min_speed_kibps,
            url,
            bytes.len(),
            body_elapsed,
        );
        Ok((bytes, sha512))
    }

    /// Start a streaming tarball fetch. Returns the live reqwest
    /// Response so the caller can pull `chunk()` futures and pipe them
    /// through gz+tar+CAS without buffering the full body.
    ///
    /// Retries the *initial* request on transient failures (5xx, 429,
    /// connection errors) using `fetch_policy.retries` attempts with
    /// exponential backoff. Once chunks start flowing the caller owns
    /// stream-level errors — restarting mid-body would require
    /// unwinding partial CAS writes — so the caller should fall back
    /// to `fetch_tarball_bytes_streaming_sha512` (which retries the
    /// full body cleanly via a buffered fetch) if a mid-stream error
    /// needs another attempt.
    pub async fn start_tarball_stream(&self, url: &str) -> Result<reqwest::Response, Error> {
        let _diag =
            aube_util::diag::Span::new(aube_util::diag::Category::Registry, "tarball_stream_open")
                .with_meta_fn(|| {
                    format!(
                        r#"{{"url":{}}}"#,
                        aube_util::diag::jstr(&aube_util::url::redact_url(url))
                    )
                });
        validate_tarball_url(self, url)?;
        let safe_url = aube_util::url::redact_url(url);
        let label = format!("tarball {safe_url}");
        let max_attempts = self.fetch_policy.retries.saturating_add(1);
        let mut timeout_retries: u32 = 0;
        for attempt in 0..max_attempts {
            let is_last = attempt + 1 >= max_attempts;
            let result = self
                .authed_tarball_get(url, url)
                .header(reqwest::header::ACCEPT_ENCODING, "identity")
                .send()
                .await;
            match result {
                Ok(resp) if is_retriable_status(resp.status()) && !is_last => {
                    let wait = retry_after_from(&resp)
                        .unwrap_or_else(|| self.fetch_policy.backoff_for_attempt(attempt + 1));
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        status = resp.status().as_u16(),
                        label = label.as_str(),
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSIENT,
                        "retrying HTTP request after transient failure",
                    );
                    drop(resp);
                    tokio::time::sleep(wait).await;
                }
                Ok(resp) => {
                    let resp = resp.error_for_status()?;
                    check_body_cap(&resp, self.fetch_policy.tarball_max_bytes, "tarball")?;
                    return Ok(resp);
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
                        label = label.as_str(),
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSPORT,
                        "retrying HTTP request after transport error",
                    );
                    tokio::time::sleep(wait).await;
                }
                Err(err) => return Err(Error::Http(err)),
            }
        }
        // FetchPolicy::retries is `u32`, so `max_attempts =
        // retries + 1` is always ≥ 1 and the loop runs at least once;
        // every path inside the loop either returns or continues. An
        // exit past this point is a structural bug, not a runtime
        // input the caller can provoke.
        unreachable!("retry loop exited without returning; max_attempts was {max_attempts}")
    }

    /// Tarball body cap, surfaced so the streaming caller can enforce
    /// it as chunks arrive (Content-Length pre-check happens in
    /// start_tarball_stream but chunked-encoding bodies need ongoing
    /// total tracking).
    pub fn tarball_max_bytes(&self) -> u64 {
        self.fetch_policy.tarball_max_bytes
    }
}

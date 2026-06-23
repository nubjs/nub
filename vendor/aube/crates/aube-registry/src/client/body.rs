use crate::Error;

pub(super) fn is_retriable_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

/// Stream-and-count read of a response body that enforces `cap` even
/// when the server omits `Content-Length` (chunked transfer encoding).
/// `check_body_cap` only inspects the precheck header; this function
/// is the runtime gate that closes the chunked-bypass primitive.
pub(super) async fn read_body_capped(
    mut resp: reqwest::Response,
    cap: u64,
    label: &str,
) -> Result<bytes::Bytes, Error> {
    if cap == 0 {
        return Ok(resp.bytes().await?);
    }
    const STREAM_INITIAL: usize = 64 * 1024;
    let initial = resp
        .content_length()
        .map(|len| len.min(cap) as usize)
        .unwrap_or(STREAM_INITIAL);
    let mut buf = bytes::BytesMut::with_capacity(initial);
    while let Some(chunk) = resp.chunk().await? {
        if (buf.len() as u64).saturating_add(chunk.len() as u64) > cap {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{label}: response body exceeds cap {cap}"),
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

/// `read_body_capped` plus a streaming SHA-512 of every byte the
/// registry delivered. Used by the tarball fetch path to skip the
/// post-buffer integrity hash (~7 ms / 5 MB tarball, ~50-120 ms /
/// 1000-pkg cold install). `Accept-Encoding: identity` is set on
/// the tarball request so reqwest cannot transparently decompress
/// underneath us, which means the streamed digest covers the same
/// bytes `aube_store::verify_integrity` checks against the lockfile
/// `integrity` field. Non-tarball callers go through the buffered
/// `read_body_capped` and skip the per-chunk hash work.
pub(super) async fn read_body_capped_streaming_sha512(
    mut resp: reqwest::Response,
    cap: u64,
    label: &str,
) -> Result<(bytes::Bytes, [u8; 64]), Error> {
    use sha2::Digest;
    const STREAM_INITIAL: usize = 64 * 1024;
    // Pre-size from Content-Length when present, capped at `cap`
    // when set, falling back to a 64 KiB scratch when neither is
    // available so chunked-encoding bodies don't pay BytesMut's
    // doubling-grow tax all the way up to `cap`.
    let initial = match (resp.content_length(), cap) {
        (Some(len), 0) => len as usize,
        (Some(len), cap) => len.min(cap) as usize,
        (None, _) => STREAM_INITIAL,
    };
    let mut buf = bytes::BytesMut::with_capacity(initial);
    let mut hasher = sha2::Sha512::new();
    while let Some(chunk) = resp.chunk().await? {
        if cap > 0 && (buf.len() as u64).saturating_add(chunk.len() as u64) > cap {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{label}: response body exceeds cap {cap}"),
            )));
        }
        hasher.update(&chunk);
        buf.extend_from_slice(&chunk);
    }
    let mut digest = [0u8; 64];
    digest.copy_from_slice(&hasher.finalize()[..]);
    Ok((buf.freeze(), digest))
}

/// Refuse a response whose declared `Content-Length` exceeds `cap`
/// before reading the body. A hostile registry (or MITM on a
/// compromised mirror) could otherwise stream gigabytes into the
/// resolver and OOM the install. Servers that omit `Content-Length`
/// still reach the capped read helpers, where chunked bodies are
/// bounded while streaming.
///
/// A `cap` of `0` disables the check entirely — an escape hatch for
/// users who need to pull packuments that exceed the default (e.g.
/// packages with very long release histories) and accept the DoS
/// exposure on the trusted-registry side.
pub(super) fn check_body_cap(resp: &reqwest::Response, cap: u64, label: &str) -> Result<(), Error> {
    if cap == 0 {
        return Ok(());
    }
    if let Some(len) = resp.content_length()
        && len > cap
    {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{label}: response Content-Length {len} exceeds cap {cap}"),
        )));
    }
    Ok(())
}

/// Emit a `fetchMinSpeedKiBps` warning if the tarball downloaded slower
/// than the configured threshold. `threshold_kibps == 0` disables the
/// warning (pnpm convention). Transfers that completed in one second
/// or less are skipped: for small/fast responses the TCP/TLS handshake
/// and TTFB dominate the "average" throughput, producing spurious
/// warnings that don't reflect network health. This matches pnpm's
/// `elapsedSec > 1` gate in its tarball fetcher.
pub(super) fn warn_slow_tarball(
    threshold_kibps: u64,
    url: &str,
    len: usize,
    elapsed: std::time::Duration,
) {
    if threshold_kibps == 0 {
        return;
    }
    if len == 0 || elapsed <= std::time::Duration::from_secs(1) {
        return;
    }
    let elapsed_ms = elapsed.as_millis() as u64;
    // speed (KiB/s) = bytes / 1024 / seconds = bytes * 1000 / elapsed_ms / 1024
    let kibps = ((len as u64).saturating_mul(1000)) / elapsed_ms / 1024;
    if kibps < threshold_kibps {
        let safe_url = aube_util::url::redact_url(url);
        tracing::warn!(
            kibps,
            threshold_kibps,
            bytes = len,
            elapsed_ms,
            url = %safe_url,
            code = aube_codes::warnings::WARN_AUBE_SLOW_TARBALL,
            "slow tarball download fell below fetchMinSpeedKiBps",
        );
    }
}

/// Parse the `Retry-After` response header as a number of seconds.
/// Per RFC 7231, this header can also be an HTTP-date, but the `Date`
/// format is rare in practice for npm-style registries and `chrono`
/// isn't a dep — callers fall back to the computed exponential
/// backoff if the header is missing, unparseable, or in date form.
/// `RETRY_AFTER_CAP_SECS` clamps the parsed value so a hostile
/// registry can't park an install for hours or years by returning
/// `Retry-After: 999999999`.
pub(super) fn retry_after_from(resp: &reqwest::Response) -> Option<std::time::Duration> {
    let raw = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    Some(std::time::Duration::from_secs(
        secs.min(RETRY_AFTER_CAP_SECS),
    ))
}

/// Upper bound on `Retry-After` we are willing to honour. 60 seconds
/// is well above any real npm-style rate-limit cooldown and keeps the
/// total retry budget bounded even when a server hands us a bogus
/// value.
const RETRY_AFTER_CAP_SECS: u64 = 60;

/// Maximum number of timeout-shaped retries before we surface the
/// error to the caller, regardless of `fetchRetries`. A timeout has
/// already cost us `fetchTimeout` of wall-clock; retrying many more
/// times compounds the user-visible hang without much chance of
/// recovery on the same upstream. One retry is enough to absorb a
/// fluke; beyond that, fail fast and let the caller decide.
///
/// Counted separately from the global retry counter inside the retry
/// loop so a non-timeout failure (e.g. a 503 on the first attempt)
/// never consumes the timeout budget.
pub(super) const TIMEOUT_RETRY_CAP: u32 = 1;

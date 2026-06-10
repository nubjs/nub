//! HTTP download + SHA-256 checksum verification for Node provisioning.
//!
//! Trust model (ratified 2026-05-30 — see
//! `wiki/research/node-provisioning-implementation.md` and the spec's Decisions
//! log): HTTPS authenticates that `SHASUMS256.txt` came from nodejs.org; the
//! SHA-256 inside it authenticates the tarball. No GPG gate in v0.1. Verification
//! is mandatory and fail-closed — a missing entry or a mismatch is an error, and
//! callers must verify BEFORE extracting (executables landing on disk).
//!
//! PM provisioning reuses this transport for the npm registry, which (unlike
//! nodejs.org) may be a private mirror requiring an `Authorization` header — the
//! `_auth`-bearing [`fetch_text_auth`] / [`download_to_file_auth`] variants carry
//! it (the credential-free `fetch_text` / `download_to_file` delegate with no
//! auth, so the Node path is untouched). Attempt failures retry a bounded
//! number of times ([`MAX_ATTEMPTS`]); 4xx and integrity failures fail fast.

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// An HTTP `Authorization` credential for a private registry. `Bearer` carries a
/// token (`.npmrc` `_authToken` / `COREPACK_NPM_TOKEN`); `Basic` carries the
/// already-base64-encoded `user:pass` (or the verbatim `.npmrc` `_auth`, which is
/// itself that base64). The value is rendered straight into the header, so the
/// caller owns the encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    Bearer(String),
    Basic(String),
}

impl Auth {
    /// The full `Authorization` header value (`Bearer <tok>` / `Basic <b64>`).
    fn header_value(&self) -> String {
        match self {
            Auth::Bearer(tok) => format!("Bearer {tok}"),
            Auth::Basic(b64) => format!("Basic {b64}"),
        }
    }
}

/// Bounded retry for transient transport failures (connection reset mid-stream,
/// 5xx, timeout). 4xx and integrity mismatches are NOT transient and fail on the
/// first attempt. Three total attempts with brief linear backoff is enough to
/// ride out a flaky proxy / a registry hiccup without turning a hard failure into
/// a multi-minute hang.
const MAX_ATTEMPTS: u32 = 3;
const RETRY_BACKOFF: Duration = Duration::from_millis(400);

/// Blocking HTTP client: rustls (no OpenSSL), native roots so corporate MITM CAs
/// keep working, and `HTTP(S)_PROXY` / `NO_PROXY` honored for free by reqwest.
fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("nub/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(600))
        .build()
        .context("building HTTP client")
}

/// Attach the optional `Authorization` header to a request builder.
fn with_auth(
    req: reqwest::blocking::RequestBuilder,
    auth: Option<&Auth>,
) -> reqwest::blocking::RequestBuilder {
    match auth {
        Some(a) => req.header(reqwest::header::AUTHORIZATION, a.header_value()),
        None => req,
    }
}

/// Whether a transport error is worth retrying: a timeout or a
/// connection-level fault (reset / refused / DNS blip) is transient; a TLS or
/// redirect-policy error is not. `error_for_status` produces a `status()`-bearing
/// error for non-2xx — those are classified by [`status_is_transient`], not here.
fn err_is_transient(err: &reqwest::Error) -> bool {
    if err.is_timeout() || err.is_connect() {
        return true;
    }
    // A body-read fault mid-stream (peer closed the connection) surfaces as a
    // generic request error with no status — treat it as transient so a dropped
    // tarball stream retries rather than aborting the install.
    err.status().is_none() && !err.is_builder() && !err.is_redirect()
}

/// 5xx (and 429) are transient; every other status (notably 4xx auth/not-found)
/// is a hard failure that must not retry.
fn status_is_transient(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

/// Brief linear backoff between attempts (no jitter — this is a single-process
/// one-shot provision, not a thundering-herd client).
fn backoff(attempt: u32) {
    std::thread::sleep(RETRY_BACKOFF * attempt);
}

/// GET a small text resource (e.g. `SHASUMS256.txt`), fail-closed on non-2xx.
pub fn fetch_text(url: &str) -> Result<String> {
    fetch_text_auth(url, None)
}

/// [`fetch_text`] with an optional private-registry `Authorization` header and the
/// bounded transient-failure retry.
pub fn fetch_text_auth(url: &str, auth: Option<&Auth>) -> Result<String> {
    let client = client()?;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let last = attempt >= MAX_ATTEMPTS;
        match try_fetch_text(&client, url, auth) {
            Ok(body) => return Ok(body),
            Err(e) if !last && e.transient => {
                backoff(attempt);
                continue;
            }
            Err(e) => return Err(e.error).with_context(|| format!("GET {url}")),
        }
    }
}

/// One `fetch_text` attempt; the [`Attempt`] wrapper tells the retry loop
/// whether the failure is worth another try.
fn try_fetch_text(
    client: &reqwest::blocking::Client,
    url: &str,
    auth: Option<&Auth>,
) -> std::result::Result<String, Attempt> {
    let resp = with_auth(client.get(url), auth)
        .send()
        .map_err(Attempt::from_send)?;
    let resp = resp.error_for_status().map_err(Attempt::from_status)?;
    resp.text()
        .map_err(|e| Attempt::transient(anyhow::Error::new(e)))
}

/// A failed attempt plus whether the retry loop should try again.
struct Attempt {
    error: anyhow::Error,
    transient: bool,
}

impl Attempt {
    fn transient(error: anyhow::Error) -> Self {
        Self {
            error,
            transient: true,
        }
    }
    fn fatal(error: anyhow::Error) -> Self {
        Self {
            error,
            transient: false,
        }
    }
    /// A send/connect/timeout failure (no HTTP status yet).
    fn from_send(err: reqwest::Error) -> Self {
        let transient = err_is_transient(&err);
        Self {
            error: anyhow::Error::new(err),
            transient,
        }
    }
    /// A non-2xx status from `error_for_status`: only 5xx/429 retry.
    fn from_status(err: reqwest::Error) -> Self {
        let transient = err.status().map(status_is_transient).unwrap_or(false);
        Self {
            error: anyhow::Error::new(err),
            transient,
        }
    }
}

/// Stream `url` into `dest` (not buffered in memory — tarballs are tens of MB),
/// returning the SHA-256 (lowercase hex) of the bytes written. `progress` is
/// called as chunks arrive with `(bytes_so_far, total_len_if_known)` so callers
/// can render a stderr progress line.
pub fn download_to_file(
    url: &str,
    dest: &Path,
    progress: impl FnMut(u64, Option<u64>),
) -> Result<String> {
    download_to_file_auth(url, dest, None, progress)
}

/// [`download_to_file`] with an optional private-registry `Authorization` header
/// and bounded transient-failure retry. A mid-stream connection drop, a 5xx, or a
/// timeout retries from scratch (the dest file is truncated on each attempt, so a
/// partial body never corrupts the result); a 4xx fails fast. The `progress`
/// callback may fire on a doomed attempt — callers gate their announce line on a
/// latch (see `provision`), so a retried download announces at most once.
pub fn download_to_file_auth(
    url: &str,
    dest: &Path,
    auth: Option<&Auth>,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<String> {
    let client = client()?;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let last = attempt >= MAX_ATTEMPTS;
        match try_download(&client, url, dest, auth, &mut progress) {
            Ok(sha) => return Ok(sha),
            Err(e) if !last && e.transient => {
                backoff(attempt);
                continue;
            }
            Err(e) => return Err(e.error).with_context(|| format!("downloading {url}")),
        }
    }
}

/// One streamed-download attempt. A non-2xx status or a connect failure before
/// the first byte classifies via [`Attempt`]; a fault *during* the body stream
/// is treated as transient (peer reset mid-tarball is the canonical retryable
/// case). The dest file is (re)created at the top, so a retry starts clean.
fn try_download(
    client: &reqwest::blocking::Client,
    url: &str,
    dest: &Path,
    auth: Option<&Auth>,
    progress: &mut impl FnMut(u64, Option<u64>),
) -> std::result::Result<String, Attempt> {
    let resp = with_auth(client.get(url), auth)
        .send()
        .map_err(Attempt::from_send)?;
    let mut resp = resp.error_for_status().map_err(Attempt::from_status)?;
    let total = resp.content_length();

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut file = std::fs::File::create(dest)
        .with_context(|| format!("create {}", dest.display()))
        .map_err(Attempt::fatal)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut written = 0u64;
    loop {
        // A read fault here is a mid-stream transport drop → retry.
        let n = resp.read(&mut buf).map_err(|e| {
            Attempt::transient(anyhow::Error::new(e).context("reading response body"))
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        // A local write failure is NOT transient (disk full / read-only) — fail fast.
        file.write_all(&buf[..n])
            .with_context(|| format!("writing {}", dest.display()))
            .map_err(Attempt::fatal)?;
        written += n as u64;
        progress(written, total);
    }
    file.flush().ok();
    Ok(hex_lower(&hasher.finalize()))
}

/// SHA-256 (lowercase hex) of a file already on disk.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Find the expected SHA-256 for `filename` in a `SHASUMS256.txt` body. Each line
/// is `<64-hex>  <filename>` (sha256sum format — two spaces). Returns the
/// lowercase hex, or `None` when the file isn't listed.
pub fn checksum_for(shasums: &str, filename: &str) -> Option<String> {
    shasums.lines().find_map(|line| {
        let (hash, rest) = line.split_once(char::is_whitespace)?;
        let name = rest.trim_start(); // collapse the leading space(s) before the name
        let valid = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        (valid && name == filename).then(|| hash.to_ascii_lowercase())
    })
}

/// Verify a downloaded artifact's SHA-256 against `SHASUMS256.txt`. Fail-closed:
/// errors when `filename` isn't listed or the hashes differ.
pub fn verify_checksum(actual_sha256_hex: &str, shasums: &str, filename: &str) -> Result<()> {
    let expected = checksum_for(shasums, filename)
        .with_context(|| format!("{filename} is not listed in SHASUMS256.txt — refusing"))?;
    if actual_sha256_hex.eq_ignore_ascii_case(&expected) {
        Ok(())
    } else {
        bail!("checksum mismatch for {filename}: expected {expected}, got {actual_sha256_hex}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_renders_the_authorization_header_value() {
        assert_eq!(
            Auth::Bearer("abc123".into()).header_value(),
            "Bearer abc123"
        );
        // Basic carries the already-encoded credential verbatim.
        assert_eq!(
            Auth::Basic("dXNlcjpwYXNz".into()).header_value(),
            "Basic dXNlcjpwYXNz"
        );
    }

    #[test]
    fn only_5xx_and_429_status_codes_retry() {
        use reqwest::StatusCode;
        // Server faults and rate-limit are transient.
        assert!(status_is_transient(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(status_is_transient(StatusCode::BAD_GATEWAY));
        assert!(status_is_transient(StatusCode::SERVICE_UNAVAILABLE));
        assert!(status_is_transient(StatusCode::TOO_MANY_REQUESTS));
        // Auth/not-found/client errors must fail fast — retrying a 401 just
        // wastes three round-trips against an auth-required mirror.
        assert!(!status_is_transient(StatusCode::UNAUTHORIZED));
        assert!(!status_is_transient(StatusCode::FORBIDDEN));
        assert!(!status_is_transient(StatusCode::NOT_FOUND));
        assert!(!status_is_transient(StatusCode::OK));
    }

    // A realistic SHASUMS256.txt slice (two-space separator, real format).
    const SHASUMS: &str = "\
0000000000000000000000000000000000000000000000000000000000000001  node-v22.13.0-linux-x64.tar.xz
abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789  node-v22.13.0-darwin-arm64.tar.xz
not-a-valid-hash  node-v22.13.0-win-x64.zip
";

    #[test]
    fn checksum_for_finds_the_exact_filename() {
        assert_eq!(
            checksum_for(SHASUMS, "node-v22.13.0-darwin-arm64.tar.xz").as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
        );
        // Not listed → None.
        assert!(checksum_for(SHASUMS, "node-v22.13.0-linux-arm64.tar.xz").is_none());
        // A malformed hash line is ignored, not accepted.
        assert!(checksum_for(SHASUMS, "node-v22.13.0-win-x64.zip").is_none());
        // No partial/prefix matches.
        assert!(checksum_for(SHASUMS, "node-v22.13.0-darwin-arm64").is_none());
    }

    #[test]
    fn verify_checksum_is_fail_closed() {
        // Match (case-insensitive) → ok.
        assert!(
            verify_checksum(
                "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789",
                SHASUMS,
                "node-v22.13.0-darwin-arm64.tar.xz"
            )
            .is_ok()
        );
        // Mismatch → error.
        assert!(verify_checksum("dead", SHASUMS, "node-v22.13.0-darwin-arm64.tar.xz").is_err());
        // Not listed → error (never silently pass).
        assert!(verify_checksum("whatever", SHASUMS, "node-v22.13.0-linux-arm64.tar.xz").is_err());
    }

    #[test]
    fn sha256_file_matches_known_vector() {
        // SHA-256("abc") — the canonical NIST vector.
        let dir = std::env::temp_dir().join(format!("nub-dl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("abc.txt");
        std::fs::write(&f, "abc").unwrap();
        assert_eq!(
            sha256_file(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real-network end-to-end: fetch the actual SHASUMS256.txt for a known
    /// version and confirm `checksum_for` extracts a valid 64-hex for this host's
    /// darwin-arm64 tarball. `#[ignore]` — network, run manually / in the matrix:
    ///   cargo test -p nub-core --lib version_management::download -- --ignored
    #[test]
    #[ignore = "network: fetches real nodejs.org SHASUMS256.txt"]
    fn fetch_real_shasums_and_parse() {
        let body = fetch_text("https://nodejs.org/dist/v22.13.0/SHASUMS256.txt").unwrap();
        let sum = checksum_for(&body, "node-v22.13.0-darwin-arm64.tar.xz")
            .expect("darwin-arm64 listed in real SHASUMS256.txt");
        assert_eq!(sum.len(), 64);
        assert!(sum.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    /// Full real-network flow against THIS host's actual dist tarball: build the
    /// artifact URLs (dogfooding the arch/URL module), stream-download, and
    /// confirm `download_to_file`'s SHA-256 verifies against the real
    /// SHASUMS256.txt — the verify-before-extract gate, end-to-end. ~25 MB,
    /// `#[ignore]`, run manually.
    #[test]
    #[ignore = "network: downloads a real Node tarball (~25MB)"]
    fn download_real_tarball_and_verify() {
        use crate::version_management::{HostTarget, node_artifact, resolve_mirror_base};
        let host = HostTarget::detect().expect("a published host");
        let ver: crate::node::version::NodeVersion = "22.13.0".parse().unwrap();
        let art = node_artifact(&ver, &host, &resolve_mirror_base(&host));
        let shasums = fetch_text(&art.shasums_url).unwrap();
        let dir = std::env::temp_dir().join(format!("nub-dl-real-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join(&art.tarball_filename);
        let sha = download_to_file(&art.tarball_url, &dest, |_, _| {}).unwrap();
        verify_checksum(&sha, &shasums, &art.tarball_filename)
            .expect("real tarball must verify against real SHASUMS256.txt");
        // The streamed hash must equal a fresh hash of the written file.
        assert_eq!(sha256_file(&dest).unwrap(), sha);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! HTTP download + SHA-256 checksum verification for Node provisioning.
//!
//! Trust model (ratified 2026-05-30 — see
//! `wiki/research/node-provisioning-implementation.md` and the spec's Decisions
//! log): HTTPS authenticates that `SHASUMS256.txt` came from nodejs.org; the
//! SHA-256 inside it authenticates the tarball. No GPG gate in v0.1. Verification
//! is mandatory and fail-closed — a missing entry or a mismatch is an error, and
//! callers must verify BEFORE extracting (executables landing on disk).

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

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

/// GET a small text resource (e.g. `SHASUMS256.txt`), fail-closed on non-2xx.
pub fn fetch_text(url: &str) -> Result<String> {
    let resp = client()?
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?;
    resp.text().with_context(|| format!("reading {url}"))
}

/// Stream `url` into `dest` (not buffered in memory — tarballs are tens of MB),
/// returning the SHA-256 (lowercase hex) of the bytes written. `progress` is
/// called as chunks arrive with `(bytes_so_far, total_len_if_known)` so callers
/// can render a stderr progress line.
pub fn download_to_file(
    url: &str,
    dest: &Path,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<String> {
    let mut resp = client()?
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?;
    let total = resp.content_length();

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut file =
        std::fs::File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut written = 0u64;
    loop {
        let n = resp.read(&mut buf).context("reading response body")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .with_context(|| format!("writing {}", dest.display()))?;
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
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
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
        assert!(verify_checksum(
            "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789",
            SHASUMS,
            "node-v22.13.0-darwin-arm64.tar.xz"
        )
        .is_ok());
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
        use crate::version_management::{node_artifact, resolve_mirror_base, HostTarget};
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

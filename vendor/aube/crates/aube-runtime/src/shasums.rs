//! `SHASUMS256.txt` fetch + parse. A release's checksum file is
//! immutable once published, so it caches forever (no TTL, no
//! revalidation).

use crate::error::Error;
use crate::http::Http;
use crate::paths;
use crate::{NetworkMode, RuntimeConfig};
use std::collections::BTreeMap;

/// Parsed checksum file: artifact filename → SHA-256 (raw bytes).
pub(crate) struct Shasums {
    entries: BTreeMap<String, [u8; 32]>,
}

impl Shasums {
    pub(crate) fn for_file(&self, filename: &str) -> Option<&[u8; 32]> {
        self.entries.get(filename)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&String, &[u8; 32])> {
        self.entries.iter()
    }

    fn parse(text: &str) -> Shasums {
        let mut entries = BTreeMap::new();
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let (Some(hash), Some(name)) = (parts.next(), parts.next()) else {
                continue;
            };
            let Ok(bytes) = hex::decode(hash) else {
                continue;
            };
            let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) else {
                continue;
            };
            entries.insert(name.to_string(), arr);
        }
        Shasums { entries }
    }
}

/// Convert a raw SHA-256 into the SRI form lockfiles use
/// (`sha256-<base64>`).
pub fn sri_sha256(digest: &[u8; 32]) -> String {
    use base64::Engine;
    format!(
        "sha256-{}",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

/// Parse an SRI `sha256-<base64>` string back into raw bytes.
pub fn sha256_from_sri(sri: &str) -> Option<[u8; 32]> {
    use base64::Engine;
    let b64 = sri.strip_prefix("sha256-")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    <[u8; 32]>::try_from(bytes.as_slice()).ok()
}

/// Load `SHASUMS256.txt` for `version` from `base` (a mirror base
/// URL), via the forever-cache.
pub(crate) async fn load_shasums(
    http: &Http,
    cfg: &RuntimeConfig,
    base: &str,
    version: &node_semver::Version,
) -> Result<Shasums, Error> {
    let cache_path = paths::shasums_cache_path(base, version);
    if let Some(text) = cache_path
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok())
    {
        let parsed = Shasums::parse(&text);
        if !parsed.entries.is_empty() {
            return Ok(parsed);
        }
        // A truncated/corrupted cache entry would otherwise fail every
        // install until manually deleted — refetch and overwrite.
        tracing::debug!(version = %version, "ignoring unparseable shasums cache entry");
    }
    if cfg.network == NetworkMode::Offline {
        return Err(Error::Offline {
            what: format!("checksums for Node.js v{version}"),
        });
    }
    let url = format!("{base}/v{version}/SHASUMS256.txt");
    let resp = http.get(&url, None, None, false).await?;
    let body = resp.body.ok_or_else(|| Error::DownloadFailed {
        url: url.clone(),
        reason: "unexpected empty response".to_string(),
    })?;
    let text = body.text().await.map_err(|e| Error::DownloadFailed {
        url: url.clone(),
        reason: e.to_string(),
    })?;
    let parsed = Shasums::parse(&text);
    if parsed.entries.is_empty() {
        return Err(Error::DownloadFailed {
            url,
            reason: "SHASUMS256.txt contained no parseable entries".to_string(),
        });
    }
    if let Some(path) = cache_path
        && let Some(parent) = path.parent()
    {
        let _ = std::fs::create_dir_all(parent);
        if let Err(e) = aube_util::fs_atomic::atomic_write(&path, text.as_bytes()) {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_CACHE_WRITE_FAILED,
                path = %path.display(),
                error = %e,
                "failed to write node shasums cache"
            );
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shasums_format() {
        let text = "\
0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  node-v22.1.0-darwin-arm64.tar.gz
fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210  node-v22.1.0-win-x64.zip
garbage line
deadbeef  too-short-hash.tar.gz
";
        let s = Shasums::parse(text);
        assert_eq!(s.entries.len(), 2);
        assert!(s.for_file("node-v22.1.0-darwin-arm64.tar.gz").is_some());
        assert!(s.for_file("too-short-hash.tar.gz").is_none());
    }

    #[test]
    fn sri_round_trip() {
        let digest = [7u8; 32];
        let sri = sri_sha256(&digest);
        assert!(sri.starts_with("sha256-"));
        assert_eq!(sha256_from_sri(&sri), Some(digest));
        assert_eq!(sha256_from_sri("sha512-AAAA"), None);
        assert_eq!(sha256_from_sri("sha256-notbase64!!!"), None);
    }
}

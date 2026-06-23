//! The nodejs.org dist index (`{mirror}/index.json`): fetch, disk
//! cache with ETag/Last-Modified revalidation (mirroring
//! `aube-registry/src/client/cache.rs`'s packument cache), and
//! version selection against a [`NodeSpec`].

use crate::error::Error;
use crate::http::Http;
use crate::paths;
use crate::platform::Platform;
use crate::spec::NodeSpec;
use crate::{NetworkMode, RuntimeConfig};
use serde::{Deserialize, Serialize};

/// One release line from index.json (newest first).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    /// `"v22.1.0"` in the raw feed; exposed parsed.
    #[serde(with = "v_prefixed_version")]
    pub version: node_semver::Version,
    /// `false`, or the LTS codename (`"Jod"`).
    #[serde(default)]
    pub lts: LtsField,
    /// Artifact availability tokens (`"osx-arm64-tar"`, `"win-x64-zip"`, …).
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LtsField {
    #[default]
    NotLts,
    Flag(bool),
    Codename(String),
}

impl LtsField {
    pub fn codename(&self) -> Option<&str> {
        match self {
            LtsField::Codename(name) => Some(name),
            _ => None,
        }
    }

    pub fn is_lts(&self) -> bool {
        // The official feed uses `false` or a codename string, but a
        // mirror emitting a bare `lts: true` still means LTS.
        matches!(self, LtsField::Codename(_) | LtsField::Flag(true))
    }
}

mod v_prefixed_version {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &node_semver::Version, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&format!("v{v}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<node_semver::Version, D::Error> {
        let raw = String::deserialize(de)?;
        node_semver::Version::parse(raw.trim_start_matches('v'))
            .map_err(|e| serde::de::Error::custom(format!("bad version {raw:?}: {e}")))
    }
}

/// Wrapper persisted to disk — same shape as the packument cache:
/// validators + fetch time + payload.
#[derive(Serialize, Deserialize)]
struct CachedIndex {
    etag: Option<String>,
    last_modified: Option<String>,
    fetched_at: u64,
    entries: Vec<IndexEntry>,
}

const INDEX_TTL_SECS: u64 = 30 * 60;

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Load the dist index for `cfg`'s mirror, serving the disk cache
/// when fresh, revalidating with ETag/If-Modified-Since when stale,
/// and serving any cache at all in offline mode.
pub(crate) async fn load_index(http: &Http, cfg: &RuntimeConfig) -> Result<Vec<IndexEntry>, Error> {
    let base = cfg.mirror_base();
    let cache_path = paths::index_cache_path(&base);
    let cached: Option<CachedIndex> = cache_path
        .as_deref()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());

    if let Some(ref c) = cached {
        let age = now_epoch().saturating_sub(c.fetched_at);
        if age < INDEX_TTL_SECS || cfg.network == NetworkMode::Offline {
            return Ok(c.entries.clone());
        }
    } else if cfg.network == NetworkMode::Offline {
        return Err(Error::Offline {
            what: "the Node.js release index".to_string(),
        });
    }

    let url = format!("{base}/index.json");
    let resp = match http
        .get(
            &url,
            cached.as_ref().and_then(|c| c.etag.as_deref()),
            cached.as_ref().and_then(|c| c.last_modified.as_deref()),
            false,
        )
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            // Network trouble with a (stale) cache on disk: serve the
            // cache. A 30-minute-old index is far better than failing
            // `aubr test` outright on a flaky connection.
            if let Some(c) = cached {
                tracing::debug!(error = %e, "node index refresh failed; serving stale cache");
                return Ok(c.entries);
            }
            return Err(e);
        }
    };

    let (entries, etag, last_modified) = match resp.body {
        None => {
            // 304 — refresh the cache timestamp so the TTL restarts.
            let c = cached.expect("304 implies a conditional request from cache");
            (
                c.entries,
                resp.etag.or(c.etag),
                resp.last_modified.or(c.last_modified),
            )
        }
        Some(body) => {
            let bytes = body.bytes().await.map_err(|e| Error::DownloadFailed {
                url: url.clone(),
                reason: e.to_string(),
            })?;
            let entries: Vec<IndexEntry> =
                serde_json::from_slice(&bytes).map_err(|e| Error::DownloadFailed {
                    url: url.clone(),
                    reason: format!("invalid index.json: {e}"),
                })?;
            (entries, resp.etag, resp.last_modified)
        }
    };

    if let Some(path) = cache_path {
        let wrapper = CachedIndex {
            etag,
            last_modified,
            fetched_at: now_epoch(),
            entries: entries.clone(),
        };
        if let Ok(bytes) = serde_json::to_vec(&wrapper)
            && let Some(parent) = path.parent()
        {
            let _ = std::fs::create_dir_all(parent);
            if let Err(e) = aube_util::fs_atomic::atomic_write(&path, &bytes) {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_CACHE_WRITE_FAILED,
                    path = %path.display(),
                    error = %e,
                    "failed to write node index cache"
                );
            }
        }
    }
    Ok(entries)
}

/// Pick the best entry satisfying `spec`, requiring the platform's
/// artifact token in `files[]` (the feed marks which builds exist per
/// release). Entries are newest-first; "best" is the highest
/// satisfying version.
///
/// musl note: official index entries never list musl tokens — musl
/// artifacts live on unofficial-builds with availability tracked by
/// its own index. For musl platforms the `files[]` gate uses the bare
/// linux token (see [`Platform::index_files_token`]); the checksum
/// fetch is the authoritative existence check.
pub(crate) fn select<'a>(
    entries: &'a [IndexEntry],
    spec: &NodeSpec,
    platform: &Platform,
) -> Option<&'a IndexEntry> {
    let token = platform.index_files_token();
    let available = |e: &IndexEntry| e.files.is_empty() || e.files.iter().any(|f| f == &token);
    let mut candidates: Vec<&IndexEntry> = entries
        .iter()
        .filter(|e| available(e))
        .filter(|e| match spec {
            NodeSpec::Exact(v) => &e.version == v,
            NodeSpec::Range(r) => e.version.satisfies(r),
            NodeSpec::Lts => e.lts.is_lts(),
            NodeSpec::Latest => true,
            NodeSpec::LtsCodename(name) => e
                .lts
                .codename()
                .is_some_and(|c| c.eq_ignore_ascii_case(name)),
        })
        .collect();
    candidates.sort_by(|a, b| a.version.cmp(&b.version));
    candidates.pop()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(version: &str, lts: Option<&str>, files: &[&str]) -> IndexEntry {
        IndexEntry {
            version: version.parse().unwrap(),
            lts: match lts {
                Some(name) => LtsField::Codename(name.to_string()),
                None => LtsField::Flag(false),
            },
            files: files.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn plat() -> Platform {
        Platform {
            os: "darwin".into(),
            cpu: "arm64".into(),
            libc: None,
        }
    }

    fn feed() -> Vec<IndexEntry> {
        vec![
            entry("26.1.0", None, &["osx-arm64-tar", "win-x64-zip"]),
            entry("24.4.1", Some("Krypton"), &["osx-arm64-tar", "win-x64-zip"]),
            entry("24.4.0", Some("Krypton"), &["osx-arm64-tar"]),
            entry("22.11.0", Some("Jod"), &["osx-arm64-tar"]),
            entry("0.12.18", None, &[]),
        ]
    }

    #[test]
    fn selects_highest_in_range() {
        let f = feed();
        let spec = NodeSpec::parse("^24").unwrap();
        assert_eq!(
            select(&f, &spec, &plat()).unwrap().version.to_string(),
            "24.4.1"
        );
    }

    #[test]
    fn selects_latest_and_lts() {
        let f = feed();
        assert_eq!(
            select(&f, &NodeSpec::Latest, &plat())
                .unwrap()
                .version
                .to_string(),
            "26.1.0"
        );
        assert_eq!(
            select(&f, &NodeSpec::Lts, &plat())
                .unwrap()
                .version
                .to_string(),
            "24.4.1"
        );
    }

    #[test]
    fn selects_codename_case_insensitively() {
        let f = feed();
        let spec = NodeSpec::LtsCodename("jod".into());
        assert_eq!(
            select(&f, &spec, &plat()).unwrap().version.to_string(),
            "22.11.0"
        );
    }

    #[test]
    fn respects_files_availability() {
        let f = feed();
        let win = Platform {
            os: "win32".into(),
            cpu: "x64".into(),
            libc: None,
        };
        // 24.4.0 has no win build; the LTS pick for windows is 24.4.1.
        let spec = NodeSpec::parse("24.4").unwrap();
        assert_eq!(
            select(&f, &spec, &win).unwrap().version.to_string(),
            "24.4.1"
        );
    }

    #[test]
    fn no_match_returns_none() {
        let f = feed();
        assert!(select(&f, &NodeSpec::parse("^99").unwrap(), &plat()).is_none());
        assert!(select(&f, &NodeSpec::LtsCodename("argon".into()), &plat()).is_none());
    }

    #[test]
    fn raw_feed_shape_parses() {
        let raw = r#"[
            {"version": "v26.1.0", "date": "2026-06-01", "files": ["osx-arm64-tar"], "npm": "11.16.0", "lts": false, "security": false},
            {"version": "v24.4.1", "date": "2026-05-20", "files": ["osx-arm64-tar"], "lts": "Krypton", "security": true}
        ]"#;
        let entries: Vec<IndexEntry> = serde_json::from_str(raw).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(!entries[0].lts.is_lts());
        assert_eq!(entries[1].lts.codename(), Some("Krypton"));
    }
}

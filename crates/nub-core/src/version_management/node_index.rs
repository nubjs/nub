//! Resolve `.nvmrc` / `.node-version` aliases (`latest`, `lts`, `lts/<codename>`,
//! bare `<major>` / `<major.minor>`) to a concrete version against nodejs.org's
//! `index.json`. Structure ported MIT-clean from pacquet's `resolve_node_version`
//! (`.repos/pnpm/pacquet/crates/engine-runtime-node-resolver/`).
//!
//! The index is cached on disk with a short TTL so alias resolution doesn't hit
//! the network on every invocation. `resolve_spec` is pure (index in, version
//! out) so it's fully unit-tested; only `fetch_index` / `load_index` touch the
//! network + disk.

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::Deserialize;

use super::download;
use crate::node::version::NodeVersion;

/// Cached index is refetched after this long (a few hours — new releases are
/// infrequent and a stale index only delays seeing a brand-new patch).
const INDEX_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// One row of nodejs.org's `index.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub version: NodeVersion,
    /// The LTS codename (e.g. `Jod`, `Iron`) when this is an LTS release; `None`
    /// for a non-LTS / current-line release.
    pub lts: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawEntry {
    version: String,
    #[serde(default)]
    lts: serde_json::Value,
}

/// Parse the `index.json` body into entries (pure — the unit-test seam). Bad rows
/// (unparseable version) are skipped rather than failing the whole index.
pub fn parse_index(body: &str) -> Result<Vec<IndexEntry>> {
    let raw: Vec<RawEntry> = serde_json::from_str(body).context("decoding Node index.json")?;
    Ok(raw
        .into_iter()
        .filter_map(|entry| {
            let v = entry.version.strip_prefix('v').unwrap_or(&entry.version);
            let version: NodeVersion = v.parse().ok()?;
            let lts = match entry.lts {
                serde_json::Value::String(name) => Some(name),
                _ => None, // `false`
            };
            Some(IndexEntry { version, lts })
        })
        .collect())
}

/// Fetch + parse the index from `mirror_base` (e.g. `https://nodejs.org/dist`).
pub fn fetch_index(mirror_base: &str) -> Result<Vec<IndexEntry>> {
    let url = format!("{}/index.json", mirror_base.trim_end_matches('/'));
    let body = download::fetch_text(&url).with_context(|| format!("fetching {url}"))?;
    parse_index(&body)
}

/// Load the index, preferring a fresh on-disk cache
/// (`<cache_root>/node-index.json`, refetched after `INDEX_TTL`). On a fetch
/// failure but a stale-cache hit, fall back to the stale cache (offline-tolerant).
pub fn load_index(cache_root: &Path, mirror_base: &str) -> Result<Vec<IndexEntry>> {
    let cache = cache_root.join("node-index.json");
    if let Ok(meta) = std::fs::metadata(&cache) {
        if let Ok(modified) = meta.modified() {
            let fresh = SystemTime::now()
                .duration_since(modified)
                .map(|age| age < INDEX_TTL)
                .unwrap_or(false);
            if fresh {
                if let Ok(body) = std::fs::read_to_string(&cache) {
                    if let Ok(index) = parse_index(&body) {
                        return Ok(index);
                    }
                }
            }
        }
    }

    // Cache stale/absent — refetch and rewrite.
    let url = format!("{}/index.json", mirror_base.trim_end_matches('/'));
    match download::fetch_text(&url) {
        Ok(body) => {
            let index = parse_index(&body)?;
            std::fs::create_dir_all(cache_root).ok();
            std::fs::write(&cache, &body).ok();
            Ok(index)
        }
        Err(fetch_err) => {
            // Offline but we have *a* cache (even if stale)? Use it rather than fail.
            if let Ok(body) = std::fs::read_to_string(&cache) {
                if let Ok(index) = parse_index(&body) {
                    return Ok(index);
                }
            }
            Err(fetch_err).with_context(|| format!("fetching {url} (and no usable cache)"))
        }
    }
}

/// Resolve a `.nvmrc` / `.node-version` selector to a concrete version against
/// `index`. Handles `latest`/`node`, `lts`/`lts/*`, `lts/<codename>`, bare
/// `<major>` and `<major.minor>`, and an exact `[v]X.Y.Z`. Returns `None` when
/// nothing matches. (`rc/<major>` lives on a different mirror — handled by the
/// caller passing the rc index; this picks the newest entry there too.)
pub fn resolve_spec(spec: &str, index: &[IndexEntry]) -> Option<NodeVersion> {
    let spec = spec.trim();
    let lower = spec.to_ascii_lowercase();

    if lower == "latest" || lower == "node" {
        return index.iter().map(|e| e.version.clone()).max();
    }
    if lower == "lts" || lower == "lts/*" {
        return index
            .iter()
            .filter(|e| e.lts.is_some())
            .map(|e| e.version.clone())
            .max();
    }
    if let Some(codename) = lower.strip_prefix("lts/") {
        return index
            .iter()
            .filter(|e| {
                e.lts
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(codename))
            })
            .map(|e| e.version.clone())
            .max();
    }

    // Numeric: exact / major / major.minor (tolerate a leading `v`).
    let numeric = lower.strip_prefix('v').unwrap_or(&lower);
    let parts: Vec<&str> = numeric.split('.').collect();
    match parts.as_slice() {
        [maj, min, _pat] if all_digits(maj) && all_digits(min) => {
            // Exact pin — match it in the index (so a typo'd nonexistent version
            // resolves to None rather than a doomed download).
            let want: NodeVersion = numeric.parse().ok()?;
            index
                .iter()
                .find(|e| e.version == want)
                .map(|e| e.version.clone())
        }
        [maj, min] if all_digits(maj) && all_digits(min) => {
            let (maj, min): (u64, u64) = (maj.parse().ok()?, min.parse().ok()?);
            index
                .iter()
                .filter(|e| e.version.0.major == maj && e.version.0.minor == min)
                .map(|e| e.version.clone())
                .max()
        }
        [maj] if all_digits(maj) => {
            let maj: u64 = maj.parse().ok()?;
            index
                .iter()
                .filter(|e| e.version.0.major == maj)
                .map(|e| e.version.clone())
                .max()
        }
        _ => None,
    }
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Resolve an already-parsed semver range (`devEngines.runtime` /
/// `engines.node` style) to the newest version in `index` satisfying it —
/// the resolution rule for range pins per
/// `wiki/runtime/node-version-management.md` §"Resolution order".
/// `alternatives` carries node-semver `||` branches (OR semantics — the shape
/// `VersionPin::Range` holds); a plain range is a one-element slice.
pub fn resolve_range(
    alternatives: &[semver::VersionReq],
    index: &[IndexEntry],
) -> Option<NodeVersion> {
    index
        .iter()
        .filter(|e| alternatives.iter().any(|req| req.matches(&e.version.0)))
        .map(|e| e.version.clone())
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A small fixture index in real index.json shape (newest first, mixed LTS).
    const FIXTURE: &str = r#"[
        {"version":"v23.5.0","lts":false},
        {"version":"v22.13.0","lts":"Jod"},
        {"version":"v22.12.0","lts":"Jod"},
        {"version":"v20.18.1","lts":"Iron"},
        {"version":"v20.18.0","lts":"Iron"},
        {"version":"v18.20.5","lts":"Hydrogen"},
        {"version":"not-a-version","lts":false}
    ]"#;

    fn idx() -> Vec<IndexEntry> {
        parse_index(FIXTURE).unwrap()
    }

    #[test]
    fn parse_skips_bad_rows_and_strips_v_and_decodes_lts() {
        let index = idx();
        assert_eq!(index.len(), 6, "the unparseable row is dropped");
        assert_eq!(index[0].version, NodeVersion::new(23, 5, 0));
        assert_eq!(index[0].lts, None); // false → None
        assert_eq!(index[1].lts.as_deref(), Some("Jod"));
    }

    #[test]
    fn resolves_latest_and_lts_aliases() {
        let index = idx();
        assert_eq!(
            resolve_spec("latest", &index),
            Some(NodeVersion::new(23, 5, 0))
        );
        assert_eq!(
            resolve_spec("node", &index),
            Some(NodeVersion::new(23, 5, 0))
        );
        // Newest LTS overall — 22.13.0 (Jod) beats 20.x (Iron).
        assert_eq!(
            resolve_spec("lts", &index),
            Some(NodeVersion::new(22, 13, 0))
        );
        assert_eq!(
            resolve_spec("lts/*", &index),
            Some(NodeVersion::new(22, 13, 0))
        );
    }

    #[test]
    fn resolves_lts_codename_case_insensitively() {
        let index = idx();
        assert_eq!(
            resolve_spec("lts/iron", &index),
            Some(NodeVersion::new(20, 18, 1))
        );
        assert_eq!(
            resolve_spec("lts/Iron", &index),
            Some(NodeVersion::new(20, 18, 1))
        );
        assert_eq!(
            resolve_spec("lts/jod", &index),
            Some(NodeVersion::new(22, 13, 0))
        );
        assert_eq!(resolve_spec("lts/nonexistent", &index), None);
    }

    #[test]
    fn resolves_numeric_major_minor_exact() {
        let index = idx();
        // Major → highest matching.
        assert_eq!(
            resolve_spec("22", &index),
            Some(NodeVersion::new(22, 13, 0))
        );
        assert_eq!(
            resolve_spec("20", &index),
            Some(NodeVersion::new(20, 18, 1))
        );
        // Major.minor → highest patch.
        assert_eq!(
            resolve_spec("22.12", &index),
            Some(NodeVersion::new(22, 12, 0))
        );
        // Exact (must be present in the index; leading v tolerated).
        assert_eq!(
            resolve_spec("v22.13.0", &index),
            Some(NodeVersion::new(22, 13, 0))
        );
        assert_eq!(
            resolve_spec("22.13.0", &index),
            Some(NodeVersion::new(22, 13, 0))
        );
        // Exact-but-not-published → None (don't attempt a doomed download).
        assert_eq!(resolve_spec("22.13.99", &index), None);
        // Nonexistent major → None.
        assert_eq!(resolve_spec("99", &index), None);
    }

    #[test]
    fn resolve_range_picks_newest_satisfying() {
        let index = idx();
        let req = semver::VersionReq::parse(">=20, <23").unwrap();
        assert_eq!(
            resolve_range(std::slice::from_ref(&req), &index),
            Some(NodeVersion::new(22, 13, 0)),
            "newest in-range release wins, not merely any match"
        );
        // `||` alternatives: newest across ALL branches — 22.13.0 (from >=22)
        // beats 18.20.5 (from ^18) even though both branches match something.
        let or = vec![
            semver::VersionReq::parse("^18").unwrap(),
            semver::VersionReq::parse(">=22, <23").unwrap(),
        ];
        assert_eq!(
            resolve_range(&or, &index),
            Some(NodeVersion::new(22, 13, 0)),
            "the newest match across || branches wins"
        );
        // Unsatisfiable range → None (surfaces as ProvisionFailed upstream).
        let none = semver::VersionReq::parse(">=99").unwrap();
        assert_eq!(resolve_range(std::slice::from_ref(&none), &index), None);
    }

    /// Real-network: resolve common aliases against the live index.
    /// `#[ignore]` — run manually / in the matrix.
    #[test]
    #[ignore = "network: fetches the real nodejs.org index.json"]
    fn resolve_against_real_index() {
        let index = fetch_index("https://nodejs.org/dist").unwrap();
        assert!(!index.is_empty());
        let latest = resolve_spec("latest", &index).expect("latest");
        let lts = resolve_spec("lts", &index).expect("lts");
        let major22 = resolve_spec("22", &index).expect("a 22.x");
        // latest >= lts, and 22 resolves into the 22 line.
        assert!(latest >= lts);
        assert_eq!(major22.0.major, 22);
    }
}

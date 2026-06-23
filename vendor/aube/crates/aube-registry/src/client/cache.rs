use crate::Packument;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Disk-cached packument with revalidation metadata.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct CachedPackument {
    pub(super) etag: Option<String>,
    pub(super) last_modified: Option<String>,
    /// Unix epoch seconds when this entry was written
    pub(super) fetched_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) max_age_secs: Option<u64>,
    pub(super) packument: Packument,
}

/// Disk-cached *full* (non-corgi) packument. Stored as raw JSON so we
/// preserve fields the resolver doesn't parse (`description`, `repository`,
/// `license`, `keywords`, `maintainers`, ...), for use by human-facing
/// commands like `aube view`.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct CachedFullPackument {
    pub(super) etag: Option<String>,
    pub(super) last_modified: Option<String>,
    pub(super) fetched_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) max_age_secs: Option<u64>,
    pub(super) packument: serde_json::Value,
}

#[derive(Debug, Default)]
pub struct CachedPackumentLookup {
    pub packument: Option<Packument>,
    pub stale: bool,
    pub(super) cached: Option<CachedPackumentLookupEntry>,
}

#[derive(Debug)]
pub(super) enum CachedPackumentLookupEntry {
    Abbreviated(CachedPackument),
    Full(CachedFullPackumentTyped),
}

#[derive(Debug)]
pub(super) struct CachedFullPackumentTyped {
    pub(super) etag: Option<String>,
    pub(super) last_modified: Option<String>,
    pub(super) fetched_at: u64,
    pub(super) max_age_secs: Option<u64>,
    pub(super) packument: Packument,
}

/// How long to trust a cached packument before revalidating with the registry.
/// Trust cached packuments for 30 minutes before revalidating. This keeps
/// repeated installs in a long-lived dev session from devolving into hundreds
/// of conditional metadata requests once the cache is just over pnpm's 5-minute
/// default staleness window.
const PACKUMENT_TTL_SECS: u64 = 1800;

pub(super) fn cached_is_fresh(fetched_at: u64, max_age_secs: Option<u64>) -> bool {
    let age = now_secs().saturating_sub(fetched_at);
    let budget = max_age_secs.unwrap_or(PACKUMENT_TTL_SECS);
    age < budget
}

pub(super) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pull ETag + Last-Modified off a response as owned strings.
pub(super) fn extract_cache_headers(resp: &reqwest::Response) -> (Option<String>, Option<String>) {
    let headers = resp.headers();
    let grab = |name: reqwest::header::HeaderName| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    };
    (
        grab(reqwest::header::ETAG),
        grab(reqwest::header::LAST_MODIFIED),
    )
}

pub(super) fn parse_cache_control_max_age(resp: &reqwest::Response) -> Option<u64> {
    let raw = resp
        .headers()
        .get(reqwest::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())?;
    let mut max_age = None;
    let mut s_maxage = None;
    let mut force_revalidate = false;
    for directive in raw.split(',').map(str::trim) {
        let directive_lc = directive.to_ascii_lowercase();
        match directive_lc.as_str() {
            "no-store" | "no-cache" | "private" => force_revalidate = true,
            _ => {}
        }
        if let Some(val) = directive_lc.strip_prefix("s-maxage=") {
            s_maxage = val.parse::<u64>().ok();
        } else if let Some(val) = directive_lc.strip_prefix("max-age=") {
            max_age = val.parse::<u64>().ok();
        }
    }
    if force_revalidate {
        return Some(0);
    }
    s_maxage.or(max_age)
}

pub(super) fn packument_cache_path(
    cache_dir: &Path,
    name: &str,
    registry_url: &str,
) -> Option<PathBuf> {
    // `name` is derived from registry responses and user-written
    // manifests. `replace('/', "__")` alone would let `../../evil`
    // escape the cache directory and turn a first resolve into an
    // arbitrary-file-write primitive. Delegate to the store's
    // shared validator so the grammar never drifts across crates.
    let safe_name = aube_store::validate_and_encode_name(name)?;
    // Partition by registry origin: a packument fetched against
    // registry A must never be returned to a request that resolves
    // to registry B (CVE-2018-7167 class). Hash the URL so port,
    // trailing-slash, and scheme variants share the same bucket only
    // when literally identical bytes were configured.
    let origin = registry_origin_segment(registry_url);
    Some(cache_dir.join(origin).join(format!("{safe_name}.json")))
}

fn registry_origin_segment(registry_url: &str) -> String {
    let digest = blake3::hash(registry_url.as_bytes()).to_hex();
    format!("origin-{}", &digest.as_str()[..16])
}

pub(super) fn read_cached_packument(path: &Path) -> Option<CachedPackument> {
    // sonic-rs is faster than serde_json on packument-shape JSON and,
    // unlike simd-json, takes an immutable `&[u8]` so the file content
    // doesn't need to be kept mutable for the parse to be zero-copy.
    let content = std::fs::read(path).ok()?;
    sonic_rs::from_slice(&content).ok()
}

pub(super) fn write_cached_packument(path: &Path, cached: &CachedPackument) -> std::io::Result<()> {
    // sonic-rs serializer for symmetry with the read path; output
    // format doesn't need to match anything external (cache file we
    // own), so we trade serde_json's stable formatting for a small
    // throughput win on the cold-install metadata phase.
    let json = sonic_rs::to_vec(cached).map_err(std::io::Error::other)?;
    aube_util::fs_atomic::atomic_write(path, &json)
}

pub(super) fn packument_full_cache_path(
    cache_dir: &Path,
    name: &str,
    registry_url: &str,
) -> Option<PathBuf> {
    let safe_name = aube_store::validate_and_encode_name(name)?;
    let origin = registry_origin_segment(registry_url);
    Some(cache_dir.join(origin).join(format!("{safe_name}.json")))
}

pub(super) fn read_cached_full_packument(path: &Path) -> Option<CachedFullPackument> {
    let content = std::fs::read(path).ok()?;
    sonic_rs::from_slice(&content).ok()
}

/// Typed fast-path read used by `fetch_packument_with_time_cached`
/// in the warm-cache branch. Reads the file once and uses `sonic-rs`
/// to deserialize the cached wrapper directly into a tiny typed struct
/// holding `fetched_at` plus a fully-typed [`Packument`].
///
/// Returns a missing lookup on file/parse errors, and a stale lookup
/// when revalidation is needed, so callers can decide whether a primer
/// fallback is safe without reading the cache a second time.
pub(super) fn read_cached_full_packument_typed_lookup(
    path: &Path,
    force_cache: bool,
) -> CachedPackumentLookup {
    #[derive(Deserialize)]
    struct Typed {
        etag: Option<String>,
        last_modified: Option<String>,
        fetched_at: u64,
        #[serde(default)]
        max_age_secs: Option<u64>,
        packument: Packument,
    }

    let Ok(content) = std::fs::read(path) else {
        return CachedPackumentLookup::default();
    };
    let Ok(typed) = sonic_rs::from_slice::<Typed>(&content) else {
        return CachedPackumentLookup::default();
    };
    let typed = CachedFullPackumentTyped {
        etag: typed.etag,
        last_modified: typed.last_modified,
        fetched_at: typed.fetched_at,
        max_age_secs: typed.max_age_secs,
        packument: typed.packument,
    };
    if !force_cache && !cached_is_fresh(typed.fetched_at, typed.max_age_secs) {
        return CachedPackumentLookup {
            packument: None,
            stale: true,
            cached: Some(CachedPackumentLookupEntry::Full(typed)),
        };
    }
    CachedPackumentLookup {
        packument: Some(typed.packument),
        stale: false,
        cached: None,
    }
}

pub(super) fn read_cached_full_packument_typed(
    path: &Path,
    force_cache: bool,
) -> Option<Packument> {
    read_cached_full_packument_typed_lookup(path, force_cache).packument
}

pub(super) fn write_cached_full_packument(
    path: &Path,
    etag: Option<&str>,
    last_modified: Option<&str>,
    fetched_at: u64,
    max_age_secs: Option<u64>,
    packument: &serde_json::Value,
) -> std::io::Result<()> {
    // Serialize through a borrow struct so popular packuments don't pay
    // a multi-MB `serde_json::Value::clone` per write. The owned
    // `CachedFullPackument` is still used by the read path; the writer
    // just doesn't need ownership.
    #[derive(Serialize)]
    struct CachedFullPackumentRef<'a> {
        etag: Option<&'a str>,
        last_modified: Option<&'a str>,
        fetched_at: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_age_secs: Option<u64>,
        packument: &'a serde_json::Value,
    }
    let json = sonic_rs::to_vec(&CachedFullPackumentRef {
        etag,
        last_modified,
        fetched_at,
        max_age_secs,
        packument,
    })
    .map_err(std::io::Error::other)?;
    aube_util::fs_atomic::atomic_write(path, &json)
}

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

/// Serialize a cache entry that reaches a `serde_json::Value`.
///
/// **serde_json, never sonic-rs.** A `serde_json::Value` may only be handed to
/// serde_json's own serializer. With `serde_json/arbitrary_precision` in the
/// feature graph, `Number`'s `Serialize` emits
/// `serialize_struct("$serde_json::private::Number", 1)` and relies on the
/// serializer to intercept that token — serde_json does, everything else writes
/// it out literally. sonic-rs therefore turns every number in the document into
/// `{"$serde_json::private::Number":"27353961"}` on disk, and the next read
/// fails the typed parse with `invalid type: map, expected u64`.
///
/// This is neither hypothetical nor opt-in. Cargo unifies features per build,
/// so one dependency anywhere in the binary turns it on for the whole graph
/// (`nub compile` pulls `rolldown_common`, which requires it). The packument
/// cache is shared by every build on the machine, so such a build poisons it
/// for the released binary — and the damage is self-perpetuating, because a
/// poisoned entry reads back as a plain map and ETag revalidation rewrites it
/// verbatim, so it never heals.
///
/// Only the WRITE side is affected: deserializing into a `Value` has no
/// equivalent hazard, so the read path stays on sonic-rs, which is where the
/// throughput actually matters.
fn to_cache_bytes<T: Serialize>(entry: &T) -> std::io::Result<Vec<u8>> {
    serde_json::to_vec(entry).map_err(std::io::Error::other)
}

pub(super) fn write_cached_packument(path: &Path, cached: &CachedPackument) -> std::io::Result<()> {
    // [`Packument`] reaches `serde_json::Value` through
    // `dist.attestations.provenance`, `_npmUser.trustedPublisher` and
    // `approver`. Those three are trust-policy evidence, so a corrupted one
    // degrades a security decision silently rather than failing a parse.
    let json = to_cache_bytes(cached)?;
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
    let json = to_cache_bytes(&CachedFullPackumentRef {
        etag,
        last_modified,
        fetched_at,
        max_age_secs,
        packument,
    })?;
    aube_util::fs_atomic::atomic_write(path, &json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::value::RawValue;

    #[derive(Serialize)]
    struct Entry<'a> {
        packument: &'a RawValue,
    }

    /// The cache writer must be a serializer that honors serde_json's private
    /// tokens, because the entries it writes reach `serde_json::Value`.
    ///
    /// Exercised through `RawValue` rather than through the number token that
    /// actually broke: serde_json emits the number token only under
    /// `arbitrary_precision`, and intercepts it under the same `cfg`, so in a
    /// default-feature build neither half exists and no assertion can tell the
    /// serializers apart. `raw_value` is on by default, which makes it the
    /// testable instance of the same invariant — a serializer that mishandles
    /// one mishandles the other. The number-token failure itself is only
    /// reproducible in a build that unifies `arbitrary_precision` in, and
    /// enabling that here would change the code under test relative to the code
    /// that ships.
    #[test]
    fn cache_writer_honors_serde_json_private_tokens() {
        let raw = RawValue::from_string(r#"{"unpackedSize":27353961}"#.to_string())
            .expect("fixture must be valid JSON");

        let written = String::from_utf8(
            to_cache_bytes(&Entry { packument: &raw }).expect("serializing must not fail"),
        )
        .expect("cache bytes must be UTF-8");
        assert_eq!(
            written, r#"{"packument":{"unpackedSize":27353961}}"#,
            "writer must inline the raw JSON, not leak a private token into the cache file"
        );

        // Failing control. Without it the assertion above would also pass on a
        // serializer that never had to intercept anything, and this test would
        // silently stop guarding the choice of serializer. sonic-rs is the one
        // the writer used to call.
        let via_sonic =
            String::from_utf8(sonic_rs::to_vec(&Entry { packument: &raw }).expect("control ran"))
                .expect("control bytes must be UTF-8");
        assert!(
            via_sonic.contains("$serde_json::private::"),
            "control is broken: sonic-rs was expected to leak a token, got {via_sonic}"
        );
    }
}

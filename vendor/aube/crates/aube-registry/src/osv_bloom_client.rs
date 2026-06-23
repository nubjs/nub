//! Compact bloom-filter prefilter for OSV `MAL-*` advisories.
//!
//! Fetches `endevco/osv-bloom`'s Pages-hosted `filter.bin` (sub-MB)
//! and decodes it on demand. `probe_lockfile` returns the subset of
//! `(name, version)` pairs that *probably* land on a malicious
//! advisory; the caller escalates each to the live OSV API for a
//! precise (name, version) confirmation. Bloom false positives turn
//! into one extra live-API round trip per FP, not a wrong-decision —
//! the live API is the source of truth.
//!
//! Companion to [`crate::osv_mirror`]:
//! - **`osv_mirror`** downloads OSV's full ~200 MB npm zip and keeps
//!   a name-only `HashMap` index. Default-off because the cold-start
//!   download is large.
//! - **`osv_bloom_client`** downloads a ~380 KB bloom filter
//!   regenerated every 10 minutes by `endevco/osv-bloom` and probes
//!   `(name, semver-major-bucket)` against it. Small enough that a
//!   future PR can flip the install-time gate default on, with the
//!   live API as the escalation oracle on hits.
//!
//! The wire format is documented in
//! <https://github.com/endevco/osv-bloom#wire-format-v1>. This module
//! reads format version 1; any other version aborts the decode and
//! the caller treats it as a refresh failure.
//!
//! Cache layout under `$XDG_CACHE_HOME/aube/osv-bloom/`:
//! - `filter.bin` — the bloom binary fetched verbatim from upstream.
//! - `manifest.json` — upstream's metadata sidecar, used to short-
//!   circuit the `filter.bin` download when the underlying set
//!   digest is unchanged.
//! - `state.json` — local-only book-keeping (fetched-at, prior
//!   set-digest). Separate from upstream's `manifest.json` so we
//!   don't have to spread aube-specific fields across upstream's
//!   schema.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// Upstream URLs. `endevco/osv-bloom` deploys the refreshed pair
/// to GitHub Pages every 10 minutes via an orphan-commit to its
/// `gh-pages` branch — no binary history accumulates in git and
/// Pages serves both files with strong ETags from the GitHub CDN.
const FILTER_URL: &str = "https://endevco.github.io/osv-bloom/filter.bin";
const MANIFEST_URL: &str = "https://endevco.github.io/osv-bloom/manifest.json";

/// Subdirectory under `$XDG_CACHE_HOME/aube/`. Sibling to `osv/` so
/// the two checks can coexist without colliding.
const SUBDIR: &str = "osv-bloom";
const FILTER_FILENAME: &str = "filter.bin";
const MANIFEST_FILENAME: &str = "manifest.json";
const STATE_FILENAME: &str = "state.json";

const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Default freshness budget. The upstream refresh cadence is every
/// 10 minutes; 15 minutes here leaves a half-tick of slack so a
/// concurrent CI matrix doesn't all stampede the CDN at once.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(15 * 60);

/// Wire-format magic — first 4 bytes of every valid `filter.bin`.
/// Must match `osv_bloom::MAGIC` upstream; format-version is checked
/// independently so a bumped seed/format reliably refuses to decode
/// here until aube is updated.
const MAGIC: &[u8; 4] = b"OSVB";
const FORMAT_VERSION: u32 = 1;
const HEADER_LEN: usize = 64;

/// Wildcard bucket sentinel — advisories with `introduced: "0"` or
/// no parseable version info get bucketed under `"*"` so we
/// conservatively flag every version of the named package.
const WILDCARD_BUCKET: &str = "*";

#[derive(Debug, thiserror::Error)]
pub enum BloomError {
    #[error("OSV bloom HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("OSV bloom returned non-success status: {0}")]
    Status(reqwest::StatusCode),
    #[error("OSV bloom I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("OSV bloom JSON decode error: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("OSV bloom binary format error: {0}")]
    BadFormat(&'static str),
    /// SHA-256 of the bloom bytes does not match
    /// `manifest.filter_sha256`. Either the download is corrupt or
    /// the on-disk pair was tampered with — refuse to trust either
    /// rather than risk a structurally-valid filter with bits
    /// silently cleared.
    #[error("OSV bloom filter SHA-256 mismatch: expected {expected}, computed {actual} ({origin})")]
    Integrity {
        expected: String,
        actual: String,
        /// Names the call site so the recovery action is obvious:
        /// `"downloaded"` → re-fetch from upstream;
        /// `"on-disk cache"` → blow away and re-fetch.
        origin: &'static str,
    },
    #[error("OSV bloom not yet initialized — call refresh_if_stale first")]
    NotInitialized,
}

/// Upstream `manifest.json` shape — only the fields we read. The
/// upstream file is documented at
/// <https://github.com/endevco/osv-bloom#consume>; everything we
/// don't touch (advisory_count, target_fpr, etc.) is ignored on
/// deserialize so upstream can add fields without breaking us.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct UpstreamManifest {
    pub format_version: u32,
    pub set_digest_sha256: String,
    pub filter_sha256: String,
    pub bloom_byte_len: u64,
    pub entry_count: u32,
    pub built_at_unix: u64,
}

/// Local book-keeping persisted next to the cached filter. RFC-3339
/// `fetched_at` so the file is greppable on disk.
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
struct LocalState {
    #[serde(default)]
    fetched_at: Option<String>,
    /// Last upstream `set_digest_sha256` we saw. Comparing against
    /// the freshly fetched manifest avoids re-downloading the bloom
    /// when the underlying entry set hasn't moved.
    #[serde(default)]
    set_digest_sha256: Option<String>,
}

/// Decoded bloom filter. Cheap to clone if you need to share probe
/// access across tasks — the bitset is held by `Box<[u8]>` and the
/// rest is fixed-size.
#[derive(Debug, Clone)]
pub struct Bloom {
    m: u64,
    k: u32,
    seed: [u8; 32],
    bits: Box<[u8]>,
}

impl Bloom {
    /// Decode the wire format. Bails on bad magic, format-version
    /// mismatch, header truncation, or bitset truncation — every
    /// case maps onto [`BloomError::BadFormat`] so the caller can
    /// treat them uniformly as "the cached file is corrupt, refetch".
    pub fn decode(bytes: &[u8]) -> Result<Self, BloomError> {
        if bytes.len() < HEADER_LEN {
            return Err(BloomError::BadFormat("buffer shorter than 64-byte header"));
        }
        if &bytes[0..4] != MAGIC {
            return Err(BloomError::BadFormat("bad magic"));
        }
        let format_version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        if format_version != FORMAT_VERSION {
            return Err(BloomError::BadFormat(
                "unsupported format version (aube understands v1 only)",
            ));
        }
        let m = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        let k = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
        // bytes[20..24] is n (entry count) — informational, ignored here.
        // bytes[24..32] is built_at — also informational.
        if m == 0 || m % 8 != 0 || k == 0 || k > 32 {
            return Err(BloomError::BadFormat("nonsensical bloom params"));
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&bytes[32..64]);
        let needed = (m / 8) as usize;
        let got = bytes.len() - HEADER_LEN;
        if got < needed {
            return Err(BloomError::BadFormat("bitset truncated"));
        }
        Ok(Self {
            m,
            k,
            seed,
            bits: bytes[HEADER_LEN..HEADER_LEN + needed]
                .to_vec()
                .into_boxed_slice(),
        })
    }

    /// True if the key *might* be present (real hit or bloom false
    /// positive). False is conclusive: the upstream entry set
    /// definitely doesn't contain this `(name, bucket)`.
    pub fn contains(&self, name: &str, bucket: &str) -> bool {
        let (h1, h2) = self.hash(name, bucket);
        for i in 0..self.k {
            let idx = (h1.wrapping_add((i as u64).wrapping_mul(h2)) % self.m) as usize;
            let byte = self.bits[idx / 8];
            let mask = 1u8 << (idx % 8);
            if byte & mask == 0 {
                return false;
            }
        }
        true
    }

    /// Keyed-BLAKE3 double hash matching the upstream insert path.
    /// `name || 0x00 || bucket` is the exact key encoding the
    /// builder uses; changing it here without changing the upstream
    /// (and bumping `FORMAT_VERSION`) would silently miss every
    /// real hit.
    fn hash(&self, name: &str, bucket: &str) -> (u64, u64) {
        let mut hasher = blake3::Hasher::new_keyed(&self.seed);
        hasher.update(name.as_bytes());
        hasher.update(&[0u8]);
        hasher.update(bucket.as_bytes());
        let digest = hasher.finalize();
        let bytes = digest.as_bytes();
        let h1 = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let h2 = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        (h1, h2)
    }
}

/// Bucket a concrete semver as the upstream builder does. Mirrors
/// `osv_bloom::bucket` — keep in sync with the published wire format.
fn bucket_of(version: &str) -> Option<String> {
    let v = node_semver::Version::parse(version).ok()?;
    if v.major == 0 {
        Some(format!("0.{}", v.minor))
    } else {
        Some(v.major.to_string())
    }
}

/// Cached-on-disk bloom handle.
///
/// `open` is synchronous (path resolution only). [`Self::refresh_if_stale`]
/// performs the conditional network I/O and seeds the in-memory bloom.
/// [`Self::probe_lockfile`] is synchronous against the cached bloom.
#[derive(Debug)]
pub struct OsvBloomClient {
    root: PathBuf,
    /// In-memory bloom. `Mutex<Option<Bloom>>` rather than `OnceCell`
    /// because [`Self::refresh_if_stale_from`] needs to *replace*
    /// the bloom when a fresh download lands — pre-seeded
    /// last-known-good data has to give way to the new filter, and
    /// `OnceCell::set` rejects the second write. Locking cost is
    /// negligible: probes happen once per install on the order of
    /// thousands of entries and complete in microseconds.
    bloom: Mutex<Option<Bloom>>,
}

impl OsvBloomClient {
    pub fn open(cache_dir: &Path) -> Self {
        Self {
            root: cache_dir.join(SUBDIR),
            bloom: Mutex::new(None),
        }
    }

    pub fn filter_path(&self) -> PathBuf {
        self.root.join(FILTER_FILENAME)
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join(MANIFEST_FILENAME)
    }

    fn state_path(&self) -> PathBuf {
        self.root.join(STATE_FILENAME)
    }

    /// Refresh if the local copy is older than `max_age`, missing,
    /// or upstream's `set_digest_sha256` no longer matches what we
    /// last saw. A successful return seeds the in-memory bloom from
    /// disk so subsequent [`Self::probe_lockfile`] calls are cheap.
    ///
    /// On any error past the initial decode, the in-memory bloom is
    /// still seeded with whatever the on-disk copy held going in —
    /// the caller under the `On` policy can proceed against the
    /// previously cached data rather than silently skipping the gate.
    pub async fn refresh_if_stale(
        &self,
        client: &reqwest::Client,
        max_age: Duration,
    ) -> Result<(), BloomError> {
        self.refresh_if_stale_from(client, FILTER_URL, MANIFEST_URL, max_age)
            .await
    }

    async fn refresh_if_stale_from(
        &self,
        client: &reqwest::Client,
        filter_url: &str,
        manifest_url: &str,
        max_age: Duration,
    ) -> Result<(), BloomError> {
        std::fs::create_dir_all(&self.root)?;
        // Seed the in-memory bloom from the cached pair *before* the
        // network round-trip. If the refresh below errors under the
        // `On` policy, the caller's `probe_lockfile` still has
        // last-known-good data to probe against — matching the
        // contract documented on `BloomError` and
        // `WARN_AUBE_OSV_BLOOM_REFRESH_FAILED`. A missing or
        // mismatched on-disk pair is *not* an error here; the
        // network refresh is the authoritative recovery path.
        let _ = self.try_load_from_disk();
        let state = self.load_state();
        if !is_stale(&state, max_age) && self.is_loaded() {
            return Ok(());
        }

        let manifest = fetch_manifest(client, manifest_url).await?;
        if manifest.format_version != FORMAT_VERSION {
            return Err(BloomError::BadFormat(
                "upstream manifest format_version does not match this build",
            ));
        }

        let needs_filter_download = state.set_digest_sha256.as_deref()
            != Some(manifest.set_digest_sha256.as_str())
            || !self.filter_path().exists()
            || !self.is_loaded();

        if needs_filter_download {
            let bytes = fetch_filter(client, filter_url).await?;
            // Verify against `manifest.filter_sha256` BEFORE decode.
            // `Bloom::decode` only checks structural integrity; a
            // tampered filter with selected bits cleared (e.g. for
            // known-malicious packages) would decode cleanly and
            // silently suppress probe hits. SHA-256 over the raw
            // bytes is the only thing tying the bitset to the
            // upstream-attested set digest.
            verify_filter_sha256(&bytes, &manifest.filter_sha256, "downloaded")?;
            // Decode before persisting so a corrupt download doesn't
            // poison the on-disk cache. `Bloom::decode` is cheap.
            let bloom = Bloom::decode(&bytes)?;
            atomic_write(&self.filter_path(), &bytes)?;
            // Replace any pre-seeded last-known-good bloom with the
            // freshly downloaded one. Mutex (not `OnceCell`) is
            // specifically what makes this assignment work.
            *self.bloom.lock().expect("bloom mutex poisoned") = Some(bloom);
        }

        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        atomic_write(&self.manifest_path(), &manifest_bytes)?;
        let new_state = LocalState {
            fetched_at: Some(now_rfc3339()),
            set_digest_sha256: Some(manifest.set_digest_sha256.clone()),
        };
        let state_bytes = serde_json::to_vec_pretty(&new_state)?;
        atomic_write(&self.state_path(), &state_bytes)?;
        Ok(())
    }

    pub async fn refresh_if_stale_default(
        &self,
        client: &reqwest::Client,
    ) -> Result<(), BloomError> {
        self.refresh_if_stale(client, DEFAULT_MAX_AGE).await
    }

    pub fn is_loaded(&self) -> bool {
        self.bloom.lock().expect("bloom mutex poisoned").is_some()
    }

    /// Best-effort load of the on-disk `(filter.bin, manifest.json)`
    /// pair into memory. Verifies that `filter.bin`'s SHA-256
    /// matches the cached `manifest.filter_sha256` — a desynced
    /// pair (partial write, concurrent process, manual tamper)
    /// fails closed and the caller falls back to the network
    /// refresh path. Missing files are treated as "nothing to load"
    /// rather than an error; SHA mismatch / decode failure
    /// propagates so the caller can decide to log and continue.
    fn try_load_from_disk(&self) -> Result<(), BloomError> {
        let manifest_path = self.manifest_path();
        let filter_path = self.filter_path();
        if !manifest_path.exists() || !filter_path.exists() {
            return Ok(());
        }
        let manifest_bytes = std::fs::read(&manifest_path)?;
        let manifest: UpstreamManifest = serde_json::from_slice(&manifest_bytes)?;
        let filter_bytes = std::fs::read(&filter_path)?;
        verify_filter_sha256(&filter_bytes, &manifest.filter_sha256, "on-disk cache")?;
        let bloom = Bloom::decode(&filter_bytes)?;
        *self.bloom.lock().expect("bloom mutex poisoned") = Some(bloom);
        Ok(())
    }

    /// For each `(name, version)` pair, probe `(name, major-bucket)`
    /// against the bloom. Pairs whose version doesn't parse — or
    /// that hit the wildcard bucket — are returned conservatively
    /// so the live-API escalation can confirm or clear them.
    /// Returns `(name, version)` pairs (not names) so the
    /// escalation can ask the live API the version-specific
    /// question; a name-only collapse would treat
    /// `ansi-regex@6.2.1` (the Sep 2025 worm) as forever-malicious
    /// for every release of the package. Dedup is per-pair, so a
    /// transitive graph pinning `lodash` at two versions yields at
    /// most two live-API probes.
    pub fn probe_lockfile(
        &self,
        pkgs: &[(String, String)],
    ) -> Result<Vec<(String, String)>, BloomError> {
        let guard = self.bloom.lock().expect("bloom mutex poisoned");
        let bloom = guard.as_ref().ok_or(BloomError::NotInitialized)?;
        let mut hits = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (name, version) in pkgs {
            let trigger = match bucket_of(version) {
                Some(bucket) => {
                    bloom.contains(name, &bucket) || bloom.contains(name, WILDCARD_BUCKET)
                }
                // Lockfile carries something we can't parse as semver
                // (e.g. a git URL pinning, a workspace alias surviving
                // upstream filters). Conservatively flag — the live
                // API will give the real answer.
                None => true,
            };
            if trigger && seen.insert((name.clone(), version.clone())) {
                hits.push((name.clone(), version.clone()));
            }
        }
        Ok(hits)
    }

    pub fn build_client() -> Result<reqwest::Client, BloomError> {
        Ok(
            aube_util::http::with_webpki_root_fallback(reqwest::Client::builder())
                .timeout(FETCH_TIMEOUT)
                .build()?,
        )
    }

    fn load_state(&self) -> LocalState {
        let Ok(bytes) = std::fs::read(self.state_path()) else {
            return LocalState::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }
}

/// Compare `actual_bytes`' SHA-256 against the hex-encoded
/// `expected_hex` from the manifest. Hex-encoding mismatches are
/// surfaced via [`BloomError::Integrity`] with `source` naming the
/// site so failures point at the right recovery action ("downloaded"
/// → re-fetch, "on-disk cache" → blow away and re-fetch).
fn verify_filter_sha256(
    actual_bytes: &[u8],
    expected_hex: &str,
    origin: &'static str,
) -> Result<(), BloomError> {
    let mut hasher = Sha256::new();
    hasher.update(actual_bytes);
    let actual_hex = hex_encode(&hasher.finalize());
    // Case-insensitive compare so a manifest written with uppercase
    // hex still validates.
    if !actual_hex.eq_ignore_ascii_case(expected_hex) {
        return Err(BloomError::Integrity {
            expected: expected_hex.to_lowercase(),
            actual: actual_hex,
            origin,
        });
    }
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

async fn fetch_manifest(
    client: &reqwest::Client,
    url: &str,
) -> Result<UpstreamManifest, BloomError> {
    let resp = client.get(url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(BloomError::Status(status));
    }
    let bytes = resp.bytes().await?;
    let manifest: UpstreamManifest = serde_json::from_slice(&bytes)?;
    Ok(manifest)
}

async fn fetch_filter(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, BloomError> {
    let resp = client.get(url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(BloomError::Status(status));
    }
    Ok(resp.bytes().await?.to_vec())
}

fn is_stale(state: &LocalState, max_age: Duration) -> bool {
    let Some(ts) = state.fetched_at.as_deref().and_then(parse_rfc3339) else {
        return true;
    };
    SystemTime::now()
        .duration_since(ts)
        .map(|elapsed| elapsed > max_age)
        .unwrap_or(true)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), BloomError> {
    aube_util::fs_atomic::atomic_write(path, bytes)
        .map_err(|e| BloomError::Io(std::io::Error::other(e.to_string())))
}

fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_rfc3339(secs)
}

fn format_rfc3339(unix_seconds: u64) -> String {
    let (y, mo, d, h, mi, s) = unix_to_ymdhms(unix_seconds);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    // Minimal "YYYY-MM-DDTHH:MM:SSZ" parser; we never read anything else.
    if s.len() < 20 || s.as_bytes()[10] != b'T' || !s.ends_with('Z') {
        return None;
    }
    let year: i64 = s[0..4].parse().ok()?;
    let month: u32 = s[5..7].parse().ok()?;
    let day: u32 = s[8..10].parse().ok()?;
    let hour: u32 = s[11..13].parse().ok()?;
    let min: u32 = s[14..16].parse().ok()?;
    let sec: u32 = s[17..19].parse().ok()?;
    let secs = ymdhms_to_unix(year, month, day, hour, min, sec)?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

fn ymdhms_to_unix(year: i64, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> Option<u64> {
    if !(1..=12).contains(&month) || day == 0 || hour > 23 || min > 59 || sec > 59 {
        return None;
    }
    let dim = [
        31,
        if is_leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    // Reject e.g. `2024-02-30` — otherwise the field overflows
    // silently into March and we'd accept a manifest written with
    // a malformed `fetched_at` as fresh.
    if day > dim[(month - 1) as usize] {
        return None;
    }
    let mut days: i64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    for &md in dim.iter().take((month - 1) as usize) {
        days += md as i64;
    }
    days += (day - 1) as i64;
    Some(days as u64 * 86400 + hour as u64 * 3600 + min as u64 * 60 + sec as u64)
}

fn unix_to_ymdhms(mut secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let s = (secs % 60) as u32;
    secs /= 60;
    let mi = (secs % 60) as u32;
    secs /= 60;
    let h = (secs % 24) as u32;
    let mut days = (secs / 24) as i64;
    let mut year: i64 = 1970;
    loop {
        let dy = if is_leap(year) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        year += 1;
    }
    let dim = [
        31,
        if is_leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1u32;
    for &md in &dim {
        if days < md as i64 {
            break;
        }
        days -= md as i64;
        month += 1;
    }
    (year, month, days as u32 + 1, h, mi, s)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic bloom matching the upstream wire format so
    /// decode/probe tests don't need a real download. Mirrors what
    /// `osv-bloom-build` does — keep in sync if `FORMAT_VERSION` ever
    /// bumps.
    fn synth_filter(entries: &[(&str, &str)]) -> Vec<u8> {
        let seed = *blake3::hash(b"osv-bloom v1 deterministic seed").as_bytes();
        // 4096-bit / 7-hash params — large enough to keep FPR well
        // below the test surface for the entry count we throw at it.
        let m: u64 = 4096;
        let k: u32 = 7;
        let mut bits = vec![0u8; (m / 8) as usize];
        for (name, bucket) in entries {
            let mut hasher = blake3::Hasher::new_keyed(&seed);
            hasher.update(name.as_bytes());
            hasher.update(&[0u8]);
            hasher.update(bucket.as_bytes());
            let digest = hasher.finalize();
            let h1 = u64::from_le_bytes(digest.as_bytes()[0..8].try_into().unwrap());
            let h2 = u64::from_le_bytes(digest.as_bytes()[8..16].try_into().unwrap());
            for i in 0..k {
                let idx = (h1.wrapping_add((i as u64).wrapping_mul(h2)) % m) as usize;
                bits[idx / 8] |= 1u8 << (idx % 8);
            }
        }
        let mut out = Vec::with_capacity(HEADER_LEN + bits.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&m.to_le_bytes());
        out.extend_from_slice(&k.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes()); // built_at, unused on decode
        out.extend_from_slice(&seed);
        out.extend_from_slice(&bits);
        out
    }

    #[test]
    fn decode_roundtrips_synth_filter() {
        let bytes = synth_filter(&[("evil-pkg", "1"), ("evil-pkg", "2"), ("other", "0.3")]);
        let bloom = Bloom::decode(&bytes).expect("decode");
        assert!(bloom.contains("evil-pkg", "1"));
        assert!(bloom.contains("evil-pkg", "2"));
        assert!(bloom.contains("other", "0.3"));
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let mut bytes = synth_filter(&[("a", "1")]);
        bytes[0] = b'X';
        assert!(matches!(
            Bloom::decode(&bytes),
            Err(BloomError::BadFormat("bad magic"))
        ));
    }

    #[test]
    fn decode_rejects_wrong_format_version() {
        let mut bytes = synth_filter(&[("a", "1")]);
        bytes[4..8].copy_from_slice(&999u32.to_le_bytes());
        assert!(matches!(
            Bloom::decode(&bytes),
            Err(BloomError::BadFormat(_))
        ));
    }

    #[test]
    fn decode_rejects_truncated_bitset() {
        let bytes = synth_filter(&[("a", "1")]);
        let truncated = &bytes[..bytes.len() - 1];
        assert!(matches!(
            Bloom::decode(truncated),
            Err(BloomError::BadFormat(_))
        ));
    }

    #[test]
    fn bucket_of_handles_one_x_and_zero_x() {
        assert_eq!(bucket_of("1.2.3").as_deref(), Some("1"));
        assert_eq!(bucket_of("0.3.7").as_deref(), Some("0.3"));
        assert_eq!(bucket_of("0.0.1").as_deref(), Some("0.0"));
        assert_eq!(bucket_of("2.0.0-beta.1").as_deref(), Some("2"));
        assert_eq!(bucket_of("not-a-version"), None);
    }

    #[test]
    fn probe_lockfile_returns_only_bloom_hits() {
        let bytes = synth_filter(&[("evil", "1"), ("evil", "2")]);
        let client = OsvBloomClient {
            root: PathBuf::from("/tmp/osv-bloom-test"),
            bloom: Mutex::new(Some(Bloom::decode(&bytes).unwrap())),
        };
        let pkgs = vec![
            ("evil".to_string(), "1.4.0".to_string()),
            ("evil".to_string(), "3.0.0".to_string()),
            ("safe".to_string(), "1.0.0".to_string()),
        ];
        let hits = client.probe_lockfile(&pkgs).expect("probe");
        // "evil" 1.x is in the filter, "evil" 3.x is not (different
        // bucket, no wildcard), "safe" is not. Only the version
        // that actually trips the bloom is escalated.
        assert_eq!(hits, vec![("evil".to_string(), "1.4.0".to_string())]);
    }

    #[test]
    fn probe_lockfile_flags_wildcard_entries() {
        let bytes = synth_filter(&[("worm", WILDCARD_BUCKET)]);
        let client = OsvBloomClient {
            root: PathBuf::from("/tmp/osv-bloom-test"),
            bloom: Mutex::new(Some(Bloom::decode(&bytes).unwrap())),
        };
        let pkgs = vec![("worm".to_string(), "5.4.2".to_string())];
        let hits = client.probe_lockfile(&pkgs).expect("probe");
        assert_eq!(hits, vec![("worm".to_string(), "5.4.2".to_string())]);
    }

    #[test]
    fn probe_lockfile_flags_unparseable_versions_conservatively() {
        let bytes = synth_filter(&[("evil", "1")]);
        let client = OsvBloomClient {
            root: PathBuf::from("/tmp/osv-bloom-test"),
            bloom: Mutex::new(Some(Bloom::decode(&bytes).unwrap())),
        };
        let pkgs = vec![("safe-but-weird".to_string(), "git+ssh://x".to_string())];
        let hits = client.probe_lockfile(&pkgs).expect("probe");
        // Unparseable version → conservatively flagged; live API
        // will confirm or clear.
        assert_eq!(
            hits,
            vec![("safe-but-weird".to_string(), "git+ssh://x".to_string())]
        );
    }

    #[test]
    fn probe_lockfile_without_refresh_returns_not_initialized() {
        let client = OsvBloomClient {
            root: PathBuf::from("/tmp/osv-bloom-test"),
            bloom: Mutex::new(None),
        };
        assert!(matches!(
            client.probe_lockfile(&[("x".to_string(), "1.0.0".to_string())]),
            Err(BloomError::NotInitialized)
        ));
    }

    #[test]
    fn is_stale_treats_missing_fetched_at_as_stale() {
        let s = LocalState::default();
        assert!(is_stale(&s, Duration::from_secs(60)));
    }

    #[test]
    fn is_stale_treats_recent_fetched_at_as_fresh() {
        let s = LocalState {
            fetched_at: Some(now_rfc3339()),
            set_digest_sha256: None,
        };
        assert!(!is_stale(&s, Duration::from_secs(60)));
    }

    #[test]
    fn rfc3339_roundtrip_at_known_timestamp() {
        // 2024-02-29T12:34:56Z — exercise the leap-day path.
        let s = "2024-02-29T12:34:56Z";
        let t = parse_rfc3339(s).expect("parse");
        let elapsed = t
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("post-epoch");
        assert_eq!(format_rfc3339(elapsed.as_secs()), s);
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        hex_encode(&h.finalize())
    }

    fn synth_manifest(filter_bytes: &[u8], set_digest: &str) -> Vec<u8> {
        let manifest = UpstreamManifest {
            format_version: 1,
            set_digest_sha256: set_digest.into(),
            filter_sha256: sha256_hex(filter_bytes),
            bloom_byte_len: filter_bytes.len() as u64,
            entry_count: 1,
            built_at_unix: 0,
        };
        serde_json::to_vec(&manifest).expect("manifest json")
    }

    #[test]
    fn verify_filter_sha256_accepts_matching_hash() {
        let bytes = b"hello world";
        verify_filter_sha256(bytes, &sha256_hex(bytes), "test").expect("hash matches");
    }

    #[test]
    fn verify_filter_sha256_rejects_mismatched_hash() {
        let bytes = b"hello world";
        let result = verify_filter_sha256(bytes, "00".repeat(32).as_str(), "downloaded");
        match result {
            Err(BloomError::Integrity {
                origin, expected, ..
            }) => {
                assert_eq!(origin, "downloaded");
                assert_eq!(expected, "0".repeat(64));
            }
            other => panic!("expected Integrity error, got {other:?}"),
        }
    }

    #[test]
    fn verify_filter_sha256_is_case_insensitive() {
        let bytes = b"hello world";
        let upper = sha256_hex(bytes).to_uppercase();
        verify_filter_sha256(bytes, &upper, "test").expect("uppercase hex matches");
    }

    #[tokio::test]
    async fn refresh_seeds_bloom_from_mock_endpoints() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let client = OsvBloomClient::open(tmp.path());
        let mock = wiremock::MockServer::start().await;

        let filter_bytes = synth_filter(&[("evil", "1")]);
        let manifest_json = synth_manifest(&filter_bytes, "deadbeef");

        wiremock::Mock::given(wiremock::matchers::path("/manifest.json"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_bytes(manifest_json)
                    .insert_header("content-type", "application/json"),
            )
            .mount(&mock)
            .await;
        wiremock::Mock::given(wiremock::matchers::path("/filter.bin"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(filter_bytes))
            .mount(&mock)
            .await;

        let http = reqwest::Client::new();
        client
            .refresh_if_stale_from(
                &http,
                &format!("{}/filter.bin", mock.uri()),
                &format!("{}/manifest.json", mock.uri()),
                Duration::from_secs(0),
            )
            .await
            .expect("refresh");

        let hits = client
            .probe_lockfile(&[("evil".to_string(), "1.4.0".to_string())])
            .expect("probe");
        assert_eq!(hits, vec![("evil".to_string(), "1.4.0".to_string())]);
        assert!(client.filter_path().exists());
        assert!(client.manifest_path().exists());
    }

    #[tokio::test]
    async fn refresh_skips_filter_download_when_set_digest_unchanged() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let client = OsvBloomClient::open(tmp.path());
        let mock = wiremock::MockServer::start().await;

        let filter_bytes = synth_filter(&[("evil", "1")]);
        let manifest_json = synth_manifest(&filter_bytes, "stable-digest");

        wiremock::Mock::given(wiremock::matchers::path("/manifest.json"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_bytes(manifest_json.clone()),
            )
            .mount(&mock)
            .await;
        // The filter mock is expectation-only: if we hit it on the
        // second pass the test fails via expect().
        wiremock::Mock::given(wiremock::matchers::path("/filter.bin"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(filter_bytes))
            .expect(1)
            .mount(&mock)
            .await;

        let http = reqwest::Client::new();
        let filter_url = format!("{}/filter.bin", mock.uri());
        let manifest_url = format!("{}/manifest.json", mock.uri());

        client
            .refresh_if_stale_from(&http, &filter_url, &manifest_url, Duration::from_secs(0))
            .await
            .expect("first refresh");
        // A second refresh against the same digest must not re-fetch
        // the filter. Force the freshness check with max_age=0.
        client
            .refresh_if_stale_from(&http, &filter_url, &manifest_url, Duration::from_secs(0))
            .await
            .expect("second refresh");
    }

    /// Tampered download: manifest declares one SHA but the filter
    /// bytes hash to another. Must surface as
    /// `BloomError::Integrity` rather than silently writing the
    /// poisoned bytes to disk and treating it as a valid bloom.
    #[tokio::test]
    async fn refresh_rejects_download_with_mismatched_sha() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let client = OsvBloomClient::open(tmp.path());
        let mock = wiremock::MockServer::start().await;

        let real_bytes = synth_filter(&[("evil", "1")]);
        // Build the manifest from a *different* synth filter so its
        // declared SHA-256 won't match what we actually serve.
        let other_bytes = synth_filter(&[("safe", "1")]);
        let manifest_json = synth_manifest(&other_bytes, "deadbeef");

        wiremock::Mock::given(wiremock::matchers::path("/manifest.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(manifest_json))
            .mount(&mock)
            .await;
        wiremock::Mock::given(wiremock::matchers::path("/filter.bin"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(real_bytes))
            .mount(&mock)
            .await;

        let http = reqwest::Client::new();
        let err = client
            .refresh_if_stale_from(
                &http,
                &format!("{}/filter.bin", mock.uri()),
                &format!("{}/manifest.json", mock.uri()),
                Duration::from_secs(0),
            )
            .await
            .expect_err("must reject mismatched SHA");
        assert!(
            matches!(err, BloomError::Integrity { origin, .. } if origin == "downloaded"),
            "expected Integrity{{origin=\"downloaded\"}}, got {err:?}"
        );
        // Mismatched download must not poison the on-disk cache.
        assert!(!client.filter_path().exists());
    }

    /// On-disk pair survives a network failure: pre-seed before the
    /// network call means a refresh failure under the `On` policy
    /// still leaves a valid bloom in memory for `probe_lockfile`.
    #[tokio::test]
    async fn refresh_falls_back_to_cached_filter_on_network_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");

        // Pre-populate the cache directory as if a successful prior
        // refresh had landed.
        let filter_bytes = synth_filter(&[("evil", "1")]);
        let manifest_json = synth_manifest(&filter_bytes, "prior-digest");
        let root = tmp.path().join(SUBDIR);
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::write(root.join(FILTER_FILENAME), &filter_bytes).expect("write filter");
        std::fs::write(root.join(MANIFEST_FILENAME), &manifest_json).expect("write manifest");

        let client = OsvBloomClient::open(tmp.path());
        let mock = wiremock::MockServer::start().await;
        // 503 on the manifest URL → refresh fails outright.
        wiremock::Mock::given(wiremock::matchers::path("/manifest.json"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .mount(&mock)
            .await;
        wiremock::Mock::given(wiremock::matchers::path("/filter.bin"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .mount(&mock)
            .await;

        let http = reqwest::Client::new();
        let err = client
            .refresh_if_stale_from(
                &http,
                &format!("{}/filter.bin", mock.uri()),
                &format!("{}/manifest.json", mock.uri()),
                Duration::from_secs(0),
            )
            .await
            .expect_err("503 → refresh error");
        assert!(
            matches!(err, BloomError::Status(_)),
            "expected Status error, got {err:?}"
        );

        // Despite the refresh failure, the pre-seed should have
        // landed and `probe_lockfile` returns the cached hits.
        assert!(client.is_loaded(), "cached filter must be in memory");
        let hits = client
            .probe_lockfile(&[("evil".to_string(), "1.4.0".to_string())])
            .expect("probe against cached bloom");
        assert_eq!(hits, vec![("evil".to_string(), "1.4.0".to_string())]);
    }

    /// Desynced disk: filter.bin's bytes hash to something other than
    /// what manifest.json claims. The on-disk pre-seed must reject
    /// the pair (`Integrity` error from `try_load_from_disk`) rather
    /// than poisoning memory with a bloom whose provenance we can't
    /// vouch for.
    #[tokio::test]
    async fn try_load_from_disk_rejects_desynced_sha() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let filter_bytes = synth_filter(&[("evil", "1")]);
        // Manifest claims a different filter's hash.
        let other_bytes = synth_filter(&[("safe", "1")]);
        let manifest_json = synth_manifest(&other_bytes, "any");
        let root = tmp.path().join(SUBDIR);
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::write(root.join(FILTER_FILENAME), &filter_bytes).expect("write filter");
        std::fs::write(root.join(MANIFEST_FILENAME), &manifest_json).expect("write manifest");
        let client = OsvBloomClient::open(tmp.path());
        let err = client.try_load_from_disk().expect_err("desynced");
        assert!(
            matches!(err, BloomError::Integrity { origin, .. } if origin == "on-disk cache"),
            "expected Integrity{{origin=\"on-disk cache\"}}, got {err:?}"
        );
        assert!(
            !client.is_loaded(),
            "must not seed memory with un-vouched bytes"
        );
    }

    #[test]
    fn rfc3339_rejects_feb_30() {
        assert!(parse_rfc3339("2024-02-30T00:00:00Z").is_none());
    }

    #[test]
    fn rfc3339_rejects_apr_31() {
        assert!(parse_rfc3339("2024-04-31T00:00:00Z").is_none());
    }
}

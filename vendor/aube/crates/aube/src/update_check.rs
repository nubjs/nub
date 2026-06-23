//! Update notifier.
//!
//! Invoked from `aube doctor` and the `aube --version` / `-V` flag —
//! the version-asking commands. Fetches `https://aube.jdx.dev/VERSION`
//! and prints a one-line notice on stderr if the advertised version is
//! newer than the running binary. The result is cached under
//! `<cacheDir>/update-check.json` so only the first run in any 24h
//! window touches the network.
//!
//! Failures (DNS, timeout, non-200, unparseable) are swallowed silently
//! — a hiccup on the update server must never disturb command output.
//! The fetch also short-circuits when `CI` / `AUBE_NO_UPDATE_CHECK` is
//! set or when the `updateNotifier` setting is `false`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const VERSION_URL: &str = "https://aube.jdx.dev/VERSION";
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
const FETCH_TIMEOUT: Duration = Duration::from_millis(1500);
/// Hard cap on the bytes we'll read from the VERSION endpoint. The
/// file is expected to be a single semver (≈ 10 bytes); anything
/// meaningfully larger is either misconfigured or hostile, and we'd
/// rather not route it into the semver parser.
const MAX_VERSION_LEN: usize = 64;

#[derive(serde::Serialize, serde::Deserialize)]
struct CacheEntry {
    checked_at: u64,
    latest: String,
}

/// Run the update check and print a notice if a newer version exists.
pub async fn check_and_notify(cwd: &Path) {
    // An embedder that owns its own upgrade path sets `self_update_enabled`
    // false; aube's update notifier (and its `aube.jdx.dev` endpoints) must
    // never run in that case. Standalone aube has it true.
    if !aube_util::embedder().self_update_enabled {
        return;
    }
    if !should_check() {
        return;
    }
    let enabled = crate::commands::with_settings_ctx(cwd, aube_settings::resolved::update_notifier);
    if !enabled {
        return;
    }
    let id = aube_util::embedder();
    let current = id.version;
    let Some(latest) = latest_version(cwd).await else {
        return;
    };
    if !is_newer(&latest, current) {
        return;
    }
    eprintln!();
    eprintln!("  {} {latest} is available (current: {current})", id.name);
    eprintln!("  upgrade: https://aube.jdx.dev");
}

fn should_check() -> bool {
    if aube_util::env::is_ci() {
        return false;
    }
    if aube_util::env::embedder_env("NO_UPDATE_CHECK").is_some() {
        return false;
    }
    true
}

async fn latest_version(cwd: &Path) -> Option<String> {
    let cache = cache_path(cwd);
    let now = unix_now();
    if let Some(entry) = read_cache(&cache)
        && now.saturating_sub(entry.checked_at) < CHECK_INTERVAL_SECS
    {
        return Some(entry.latest);
    }
    let latest = fetch_latest().await?;
    // Write-through cache: record the successful fetch even when the
    // advertised version isn't newer, so the next install within 24h
    // doesn't re-hit the network just to discover the same "no update"
    // answer.
    let _ = write_cache(
        &cache,
        &CacheEntry {
            checked_at: now,
            latest: latest.clone(),
        },
    );
    Some(latest)
}

async fn fetch_latest() -> Option<String> {
    let client = aube_util::http::with_webpki_root_fallback(reqwest::Client::builder())
        .timeout(FETCH_TIMEOUT)
        .build()
        .ok()?;
    let resp = client.get(VERSION_URL).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    // Reject an advertised oversize body *before* reading it — a
    // misconfigured or hostile server could otherwise push several MB
    // into memory before the trailing length check fires. The 1.5s
    // timeout already bounds wall-clock exposure, so this check is
    // defense-in-depth for the buffer-size dimension.
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_VERSION_LEN as u64)
    {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    if bytes.len() > MAX_VERSION_LEN {
        return None;
    }
    let text = std::str::from_utf8(&bytes).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

fn cache_path(cwd: &Path) -> PathBuf {
    crate::commands::resolved_cache_dir(cwd).join("update-check.json")
}

fn read_cache(path: &Path) -> Option<CacheEntry> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_cache(path: &Path, entry: &CacheEntry) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(entry).map_err(std::io::Error::other)?;
    aube_util::fs_atomic::atomic_write(path, &bytes)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_newer(latest: &str, current: &str) -> bool {
    match (
        node_semver::Version::parse(latest),
        node_semver::Version::parse(current),
    ) {
        (Ok(l), Ok(c)) => l > c,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_compares_semver() {
        assert!(is_newer("1.2.3", "1.2.2"));
        assert!(is_newer("2.0.0", "1.99.99"));
        assert!(!is_newer("1.2.3", "1.2.3"));
        assert!(!is_newer("1.0.0", "1.0.1"));
    }

    #[test]
    fn is_newer_rejects_unparseable() {
        assert!(!is_newer("not-a-version", "1.0.0"));
        assert!(!is_newer("1.0.0", ""));
    }
}

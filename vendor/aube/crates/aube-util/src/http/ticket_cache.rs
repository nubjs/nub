//! Cross-invocation TLS session ticket cache.
//!
//! rustls 0.23+ exposes `ClientSessionStore` for caching session
//! tickets in-memory; the default impl is per-process and dies with
//! the CLI. Persisting tickets on disk lets the second `aube install`
//! invocation skip the full TLS handshake and resume against the
//! cached session, saving 1 RTT (~50-150 ms per origin) on cold
//! invocations after the first one. No PM in the npm-CM-space ships
//! this — npm/pnpm/yarn/bun/vlt all start with an empty session
//! store every invocation.
//!
//! Format: serde-json blob at `$XDG_CACHE_HOME/aube/tls-tickets.json`
//! containing per-host entries `(server_name, port) -> TicketEntry`.
//! Each entry holds the rustls ticket bytes plus the SPKI fingerprint
//! observed at ticket-acquire time. The rustls wiring layer compares
//! the live cert's SPKI fingerprint against `spki_fp` and calls
//! `invalidate(host, port)` on mismatch so a rotated cert never
//! silently downgrades to a stale resumption. Entries past `MAX_AGE`
//! (24 h) are pruned at load.
//!
//! On Unix the on-disk file is created with mode 0600 so ticket bytes
//! are not world-readable on multi-user hosts.
//!
//! `AUBE_DISABLE_TLS_TICKET_CACHE=1` skips load + save; rustls falls
//! back to its per-process in-memory store.
//!
//! The rustls `ClientSessionStore` trait wiring lives at the
//! `aube-registry` integration site so `aube-util` keeps zero rustls
//! dependency. This module ships the on-disk format, the in-memory
//! map, and the load/save/expire/invalidate APIs the wiring layer
//! reads.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Tickets older than this are pruned at load. Matches the typical
/// session-ticket-lifetime hint Cloudflare/Fastly send (~24 h).
pub const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

const FORMAT_MAGIC: &str = "aube-tls-tickets/v1";

/// Returns true when the on-disk ticket cache is disabled.
#[inline]
pub fn is_disabled() -> bool {
    crate::env::embedder_env("DISABLE_TLS_TICKET_CACHE").is_some()
}

/// One serialized ticket entry. `ticket` is opaque to this module —
/// the rustls `ClientSessionStore` wiring layer encodes/decodes it.
/// `spki_fp` binds the ticket to the cert observed when it was
/// acquired so a rotated cert force-invalidates the resumption.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketEntry {
    /// Opaque rustls ticket bytes.
    pub ticket: Vec<u8>,
    /// SHA-256 over the server's SubjectPublicKeyInfo at ticket-acquire time.
    pub spki_fp: [u8; 32],
    /// Wall-clock (UNIX seconds) when the ticket was stored. Used for `MAX_AGE` pruning.
    pub stored_at_unix_secs: u64,
}

/// Storage key — `(host, port)`. Lowercased host, normalized port.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HostPort {
    pub host: String,
    pub port: u16,
}

impl HostPort {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into().to_ascii_lowercase(),
            port,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct OnDisk {
    /// Format magic; bumping invalidates the whole file.
    magic: String,
    /// Per-host tickets serialized as a flat list because serde_json
    /// refuses non-string map keys (`HostPort` is a struct). Vec lets
    /// rustls' multi-ticket convention through (most servers issue 2
    /// NewSessionTicket frames per handshake).
    entries: Vec<(HostPort, Vec<TicketEntry>)>,
}

/// In-memory ticket cache. Backed by an on-disk JSON blob; load and
/// save are explicit so the rustls wiring layer can drive them at
/// install start / install end.
#[derive(Debug)]
pub struct TicketCache {
    path: PathBuf,
    inner: RwLock<HashMap<HostPort, Vec<TicketEntry>>>,
    /// Serializes file reads/writes against concurrent open() calls
    /// in the same process; cross-process is best-effort (last-writer
    /// wins, idempotent payload).
    file_lock: Mutex<()>,
}

impl TicketCache {
    /// Open the cache at the canonical path under
    /// `XDG_CACHE_HOME/aube/tls-tickets.json`. Caller responsible for
    /// `XDG_CACHE_HOME` resolution; pass an explicit path here.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let inner = if is_disabled() {
            HashMap::new()
        } else {
            load_from_disk(&path).unwrap_or_default()
        };
        Self {
            path,
            inner: RwLock::new(inner),
            file_lock: Mutex::new(()),
        }
    }

    /// Look up cached tickets for `(host, port)`. Stale entries beyond
    /// `MAX_AGE` are filtered transparently; callers receive only
    /// fresh tickets.
    pub fn get(&self, host: &str, port: u16) -> Vec<TicketEntry> {
        if is_disabled() {
            return Vec::new();
        }
        let key = HostPort::new(host, port);
        let now = unix_now();
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        inner
            .get(&key)
            .map(|tickets| {
                tickets
                    .iter()
                    .filter(|t| now.saturating_sub(t.stored_at_unix_secs) < MAX_AGE.as_secs())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Store a fresh ticket for `(host, port)`. Multiple tickets per
    /// origin are kept (rustls servers typically issue 2 per
    /// handshake); `prune_max_per_host` caps the queue.
    pub fn put(&self, host: &str, port: u16, entry: TicketEntry) {
        if is_disabled() {
            return;
        }
        const MAX_PER_HOST: usize = 4;
        let key = HostPort::new(host, port);
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let bucket = inner.entry(key).or_default();
        bucket.push(entry);
        if bucket.len() > MAX_PER_HOST {
            let drop = bucket.len() - MAX_PER_HOST;
            bucket.drain(..drop);
        }
    }

    /// Evict every ticket for `(host, port)`. Called when a TLS
    /// handshake observes a cert whose SPKI fingerprint does not
    /// match the cached entry — the cert rotated, so the ticket is
    /// stale.
    pub fn invalidate(&self, host: &str, port: u16) {
        let key = HostPort::new(host, port);
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        inner.remove(&key);
    }

    /// Persist the in-memory cache to disk. Atomic via
    /// `aube_util::fs_atomic::atomic_write`.
    pub fn save(&self) -> std::io::Result<()> {
        if is_disabled() {
            return Ok(());
        }
        let _guard = self.file_lock.lock().unwrap_or_else(|e| e.into_inner());
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let payload = OnDisk {
            magic: FORMAT_MAGIC.to_string(),
            entries: inner.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        };
        let bytes = serde_json::to_vec(&payload).map_err(std::io::Error::other)?;
        crate::fs_atomic::atomic_write(&self.path, &bytes)?;
        // Tighten POSIX perms after the atomic rename so ticket bytes
        // are not world-readable. Windows inherits the parent ACL,
        // which already restricts %LOCALAPPDATA% to the user; nothing
        // to do there.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Total ticket count across all hosts (for diagnostics).
    pub fn len(&self) -> usize {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        inner.values().map(|v| v.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn load_from_disk(path: &Path) -> Option<HashMap<HostPort, Vec<TicketEntry>>> {
    let bytes = std::fs::read(path).ok()?;
    let payload: OnDisk = serde_json::from_slice(&bytes).ok()?;
    if payload.magic != FORMAT_MAGIC {
        return None;
    }
    let now = unix_now();
    let map: HashMap<HostPort, Vec<TicketEntry>> = payload
        .entries
        .into_iter()
        .filter_map(|(k, v)| {
            let fresh: Vec<TicketEntry> = v
                .into_iter()
                .filter(|t| now.saturating_sub(t.stored_at_unix_secs) < MAX_AGE.as_secs())
                .collect();
            if fresh.is_empty() {
                None
            } else {
                Some((k, fresh))
            }
        })
        .collect();
    Some(map)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn entry(label: u8) -> TicketEntry {
        TicketEntry {
            ticket: vec![label, label + 1, label + 2],
            spki_fp: [label; 32],
            stored_at_unix_secs: unix_now(),
        }
    }

    #[test]
    fn roundtrip_persists_across_open() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tickets.json");
        {
            let cache = TicketCache::open(&path);
            cache.put("registry.npmjs.org", 443, entry(1));
            cache.save().unwrap();
        }
        let reopened = TicketCache::open(&path);
        let tickets = reopened.get("registry.npmjs.org", 443);
        assert_eq!(tickets.len(), 1);
        assert_eq!(tickets[0].ticket, vec![1, 2, 3]);
    }

    #[test]
    fn host_port_lowercases() {
        let a = HostPort::new("Registry.NPMJS.ORG", 443);
        let b = HostPort::new("registry.npmjs.org", 443);
        assert_eq!(a, b);
    }

    #[test]
    fn invalidate_removes_all_for_host() {
        let dir = tempdir().unwrap();
        let cache = TicketCache::open(dir.path().join("tickets.json"));
        cache.put("a.example", 443, entry(1));
        cache.put("a.example", 443, entry(2));
        assert_eq!(cache.len(), 2);
        cache.invalidate("a.example", 443);
        assert!(cache.is_empty());
    }

    #[test]
    fn max_per_host_evicts_oldest() {
        let dir = tempdir().unwrap();
        let cache = TicketCache::open(dir.path().join("tickets.json"));
        for i in 0..6u8 {
            cache.put("a.example", 443, entry(i));
        }
        let kept = cache.get("a.example", 443);
        assert_eq!(kept.len(), 4, "MAX_PER_HOST = 4");
        // Oldest two (label 0, 1) should be gone.
        assert!(kept.iter().all(|t| t.ticket[0] >= 2));
    }

    #[test]
    fn stale_entries_filtered_at_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tickets.json");
        {
            let cache = TicketCache::open(&path);
            let mut stale = entry(9);
            stale.stored_at_unix_secs = 0;
            cache.put("a.example", 443, stale);
            cache.save().unwrap();
        }
        let reopened = TicketCache::open(&path);
        assert!(reopened.get("a.example", 443).is_empty());
    }

    /// Panic-safe cleanup so a failed assertion inside the killswitch
    /// test doesn't leave `AUBE_DISABLE_TLS_TICKET_CACHE=1` set —
    /// `RUST_TEST_THREADS=1` serializes the suite but doesn't reset
    /// process env between tests, so a leaked killswitch would still
    /// poison subsequent tests in the same binary.
    struct EnvVarGuard {
        key: &'static str,
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: tests run serially via RUST_TEST_THREADS=1; no
            // other thread is mid-setenv when this guard drops.
            unsafe { std::env::remove_var(self.key) };
        }
    }

    #[test]
    fn killswitch_short_circuits() {
        // SAFETY: tests run serially via RUST_TEST_THREADS=1; no
        // other thread is reading the env while we mutate it.
        unsafe { std::env::set_var("AUBE_DISABLE_TLS_TICKET_CACHE", "1") };
        let _cleanup = EnvVarGuard {
            key: "AUBE_DISABLE_TLS_TICKET_CACHE",
        };
        let dir = tempdir().unwrap();
        let cache = TicketCache::open(dir.path().join("tickets.json"));
        cache.put("a.example", 443, entry(1));
        assert!(cache.get("a.example", 443).is_empty());
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = tempdir().unwrap();
        let cache = TicketCache::open(dir.path().join("nonexistent.json"));
        assert!(cache.is_empty());
    }

    #[test]
    fn corrupt_magic_loads_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tickets.json");
        std::fs::write(&path, br#"{"magic":"wrong","entries":[]}"#).unwrap();
        let cache = TicketCache::open(&path);
        assert!(cache.is_empty());
    }
}

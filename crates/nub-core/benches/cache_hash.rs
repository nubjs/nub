//! Baseline benches for the transpile-cache SHA-256 hot path.
//!
//! The warm-hit path in `crates/nub-native/src/cache.rs` does TWO full SHA-256
//! passes over file-sized inputs on every cache lookup:
//!   1. `cache_key` — sha256 over the key preimage, which INCLUDES the full
//!      source text (the cache FILENAME).
//!   2. `integrity` — sha256 over the stored body (transpiled code) on read,
//!      to self-heal corrupt entries.
//!
//! That native code returns napi-bridged types, so it cannot be linked into a
//! bench executable (same constraint as `test = false` on nub-native — the
//! `napi_*` symbols resolve only inside Node at dlopen). These benches
//! therefore reproduce the EXACT hashing work — same preimage layout, same
//! `to_hex` lowercasing, same `[..16]` integrity truncation — over a realistic
//! medium source so the SHA-256 cost itself is measured faithfully. The figures
//! are the per-pass cost; the warm-hit path pays roughly the sum of both.

use criterion::{Criterion, criterion_group, criterion_main};
use sha2::{Digest, Sha256};

// Mirrors cache.rs constants/layout exactly (kept in sync by hand — these are
// the literal byte preimage components).
const NUB_VERSION: &str = "0.0.0-bench";
const CACHE_SCHEMA: &str = "3";
const INTEGRITY_LEN: usize = 16;

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// sha256(NUB_VERSION \0 SCHEMA \0 source \0 ext \0 tsconfig_hash \0 pkg_type).
fn cache_key(source: &str, ext: &str, tsconfig_hash: &str, pkg_type: &str) -> String {
    let mut h = Sha256::new();
    h.update(NUB_VERSION.as_bytes());
    h.update(b"\0");
    h.update(CACHE_SCHEMA.as_bytes());
    h.update(b"\0");
    h.update(source.as_bytes());
    h.update(b"\0");
    h.update(ext.as_bytes());
    h.update(b"\0");
    h.update(tsconfig_hash.as_bytes());
    h.update(b"\0");
    h.update(pkg_type.as_bytes());
    to_hex(&h.finalize())
}

/// sha256(body)[..16] — the integrity prefix re-hashed on every warm read.
fn integrity(body: &str) -> String {
    let mut h = Sha256::new();
    h.update(body.as_bytes());
    let full = to_hex(&h.finalize());
    full[..INTEGRITY_LEN].to_string()
}

// A realistic medium TS source (~3 KB) — the kind of file the cache key hashes.
const MEDIUM_SOURCE: &str = include_str!("fixtures/medium.ts");

fn bench_cache_hash(c: &mut Criterion) {
    let tsconfig_hash = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2";

    c.bench_function("cache/key_hash/medium", |b| {
        b.iter(|| {
            cache_key(
                std::hint::black_box(MEDIUM_SOURCE),
                std::hint::black_box("ts"),
                std::hint::black_box(tsconfig_hash),
                std::hint::black_box("module"),
            )
        });
    });

    // The integrity re-hash on the warm path runs over the transpiled body,
    // which is the same order of magnitude as the source.
    let body = format!("m{MEDIUM_SOURCE}");
    c.bench_function("cache/integrity_hash/medium", |b| {
        b.iter(|| integrity(std::hint::black_box(&body)));
    });
}

criterion_group!(benches, bench_cache_hash);
criterion_main!(benches);

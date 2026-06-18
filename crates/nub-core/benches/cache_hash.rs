//! Benches for the transpile-cache hot path.
//!
//! The warm-hit path in `crates/nub-native/src/cache.rs` does TWO full hash
//! passes over file-sized inputs on every cache lookup:
//!   1. `cache_key` — hash over the key preimage, which INCLUDES the full
//!      source text (the cache FILENAME).
//!   2. `integrity` — hash over the stored body (transpiled code) on read,
//!      to self-heal corrupt entries.
//!
//! That native code returns napi-bridged types, so it cannot be linked into a
//! bench executable (same constraint as `test = false` on nub-native — the
//! `napi_*` symbols resolve only inside Node at dlopen). These benches
//! therefore reproduce the EXACT hashing work — same preimage layout, same
//! `to_hex` lowercasing, same `[..16]` integrity truncation — over a realistic
//! medium source so the hash cost itself is measured faithfully. The figures
//! are the per-pass cost; the warm-hit path pays roughly the sum of both.
//!
//! Each pass is benched under BOTH SHA-256 (the prior algo) and blake3 (the
//! current algo, schema v4) so the speedup is measured directly. cache.rs ships
//! blake3 only; SHA-256 stays here purely as the baseline to compare against.

use criterion::{Criterion, criterion_group, criterion_main};
use sha2::{Digest, Sha256};

// Mirrors cache.rs constants/layout exactly (kept in sync by hand — these are
// the literal byte preimage components).
const NUB_VERSION: &str = "0.0.0-bench";
const CACHE_SCHEMA: &str = "4";
const INTEGRITY_LEN: usize = 16;
// A representative exe-hash component (cache.rs folds blake3(current_exe) into
// the key preimage; here a fixed 64-hex stand-in keeps the preimage layout faithful).
const EXE_HASH: &str = "f00dcafef00dcafef00dcafef00dcafef00dcafef00dcafef00dcafef00dcafe0";

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

// --- SHA-256 (baseline) ---

fn cache_key_sha(source: &str, ext: &str, tsconfig_hash: &str, pkg_type: &str) -> String {
    let mut h = Sha256::new();
    h.update(NUB_VERSION.as_bytes());
    h.update(b"\0");
    h.update(CACHE_SCHEMA.as_bytes());
    h.update(b"\0");
    h.update(EXE_HASH.as_bytes());
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

fn integrity_sha(body: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(body);
    let full = to_hex(&h.finalize());
    full[..INTEGRITY_LEN].to_string()
}

// --- blake3 (current, schema v4) ---

fn cache_key_blake3(source: &str, ext: &str, tsconfig_hash: &str, pkg_type: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(NUB_VERSION.as_bytes());
    h.update(b"\0");
    h.update(CACHE_SCHEMA.as_bytes());
    h.update(b"\0");
    h.update(EXE_HASH.as_bytes());
    h.update(b"\0");
    h.update(source.as_bytes());
    h.update(b"\0");
    h.update(ext.as_bytes());
    h.update(b"\0");
    h.update(tsconfig_hash.as_bytes());
    h.update(b"\0");
    h.update(pkg_type.as_bytes());
    // 32-byte digest, 64-hex — same on-disk filename shape as the sha256 key.
    h.finalize().to_hex().to_string()
}

fn integrity_blake3(body: &[u8]) -> String {
    blake3::hash(body).to_hex()[..INTEGRITY_LEN].to_string()
}

// A realistic medium TS source (~3 KB) — the kind of file the cache key hashes.
const MEDIUM_SOURCE: &str = include_str!("fixtures/medium.ts");

fn bench_cache_hash(c: &mut Criterion) {
    let tsconfig_hash = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2";

    // --- key_hash: SHA-256 vs blake3 ---
    c.bench_function("cache/key_hash/sha256/medium", |b| {
        b.iter(|| {
            cache_key_sha(
                std::hint::black_box(MEDIUM_SOURCE),
                std::hint::black_box("ts"),
                std::hint::black_box(tsconfig_hash),
                std::hint::black_box("module"),
            )
        });
    });
    c.bench_function("cache/key_hash/blake3/medium", |b| {
        b.iter(|| {
            cache_key_blake3(
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
    let body_bytes = body.as_bytes();
    c.bench_function("cache/integrity_hash/sha256/medium", |b| {
        b.iter(|| integrity_sha(std::hint::black_box(body_bytes)));
    });
    c.bench_function("cache/integrity_hash/blake3/medium", |b| {
        b.iter(|| integrity_blake3(std::hint::black_box(body_bytes)));
    });
}

criterion_group!(benches, bench_cache_hash);
criterion_main!(benches);

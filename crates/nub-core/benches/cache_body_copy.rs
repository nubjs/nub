//! Bench for the warm-hit body-copy reduction in `crates/nub-native/src/cache.rs`.
//!
//! On every warm cache hit (the hottest steady-state transpile path) the OLD
//! `cache_get` copied the body THREE times: (1) `read_to_string` read the whole
//! on-disk file into an owned String, (2) `body.to_string()` re-alloc'd the
//! `[INTEGRITY_LEN..]` body slice, (3) the caller's `body[1..].to_string()`
//! re-alloc'd again minus the format byte. The fix returns the read buffer as the
//! final `code` by draining the integrity prefix + format byte off the front IN
//! PLACE (no re-alloc), collapsing the chain to ONE allocation (the file read).
//!
//! Like `cache_hash.rs`, this bench reproduces the EXACT read logic rather than
//! linking nub-native (whose napi_* symbols resolve only inside Node at dlopen —
//! the `test = false` constraint). The on-disk format mirrored here is
//! `[16-hex integrity = blake3(body)[..16]][body]`, `body = format_byte + code`.
//! A real ~100 KB cached entry is written to a temp file once; each iteration
//! reads + verifies + extracts the final code, so the allocation cost the path
//! actually pays is measured faithfully.

use std::io::Write;

use criterion::{Criterion, criterion_group, criterion_main};

const INTEGRITY_LEN: usize = 16;
const FORMAT_BYTE: u8 = b'm';

fn integrity(body: &[u8]) -> String {
    blake3::hash(body).to_hex()[..INTEGRITY_LEN].to_string()
}

/// OLD path: read → `body.to_string()` → caller `body[1..].to_string()` (3 copies).
fn cache_get_old(path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    if raw.len() < INTEGRITY_LEN {
        return None;
    }
    let body = &raw[INTEGRITY_LEN..];
    if raw[..INTEGRITY_LEN] != integrity(body.as_bytes()) {
        return None;
    }
    let body = body.to_string(); // copy 2
    Some(body[1..].to_string()) // copy 3
}

/// NEW path: read → verify over the slice → `drain` the prefix + format byte in
/// place, reusing the read buffer as the final code (1 copy).
fn cache_get_new(path: &std::path::Path) -> Option<(u8, String)> {
    let mut raw = std::fs::read_to_string(path).ok()?;
    if raw.len() < INTEGRITY_LEN + 1 {
        return None;
    }
    let body = &raw[INTEGRITY_LEN..];
    if raw[..INTEGRITY_LEN] != integrity(body.as_bytes()) {
        return None;
    }
    let format_byte = raw.as_bytes()[INTEGRITY_LEN];
    raw.drain(..INTEGRITY_LEN + 1);
    Some((format_byte, raw))
}

/// The OLD post-read body extraction in isolation: given the freshly-read `raw`
/// buffer (the integrity check already passed), `body.to_string()` then the
/// caller's `body[1..].to_string()` — TWO extra allocations + memcpys.
fn extract_old(raw: &str) -> String {
    let body = raw[INTEGRITY_LEN..].to_string();
    body[1..].to_string()
}

/// The NEW post-read body extraction in isolation: `drain` the prefix + format
/// byte off the owned `raw` buffer in place — ZERO extra allocations, reusing the
/// read buffer as the final code.
fn extract_new(mut raw: String) -> (u8, String) {
    let format_byte = raw.as_bytes()[INTEGRITY_LEN];
    raw.drain(..INTEGRITY_LEN + 1);
    (format_byte, raw)
}

/// Write a cache entry with a ~100 KB code body to a unique temp file.
fn write_entry() -> std::path::PathBuf {
    // ~100 KB of representative transpiled-looking text.
    let code: String = "const value = compute(left, right);\n".repeat(2900);
    let mut body = String::with_capacity(code.len() + 1);
    body.push(FORMAT_BYTE as char);
    body.push_str(&code);
    let contents = format!("{}{}", integrity(body.as_bytes()), body);

    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("nub-bench-cache-{pid}.entry"));
    let mut f = std::fs::File::create(&path).expect("create bench cache entry");
    f.write_all(contents.as_bytes()).expect("write bench entry");
    path
}

fn bench_cache_body_copy(c: &mut Criterion) {
    let path = write_entry();

    // Sanity: both paths extract byte-identical final code.
    let old = cache_get_old(&path).expect("old path hit");
    let (fb, new) = cache_get_new(&path).expect("new path hit");
    assert_eq!(fb, FORMAT_BYTE, "format byte preserved");
    assert_eq!(old, new, "old and new extraction must be byte-identical");

    // End-to-end warm hit (read + verify + extract). Both arms pay the same
    // unchanged file read + blake3 integrity hash, so the delta here is the
    // extraction-copy reduction net of that fixed cost.
    c.bench_function("cache/get/old_3copy/100kb", |b| {
        b.iter(|| cache_get_old(std::hint::black_box(&path)));
    });
    c.bench_function("cache/get/new_1copy/100kb", |b| {
        b.iter(|| cache_get_new(std::hint::black_box(&path)));
    });

    // Post-read extraction in isolation — the work that actually changed. The
    // owned `raw` buffer is cloned in UNTIMED `iter_batched` setup so neither arm
    // pays for it; the measured region is exactly the extraction. OLD = the two
    // body-sized `to_string` allocations + memcpys; NEW = the in-place `drain`
    // (zero extra allocation, an in-buffer shift). This is where the 3→1 (here
    // 2→0 net of the shared read) reduction shows cleanly, free of the read +
    // integrity-hash cost that dominates the end-to-end arms.
    use criterion::BatchSize;
    let raw = std::fs::read_to_string(&path).expect("read raw for extract bench");
    c.bench_function("cache/extract/old_2copy/100kb", |b| {
        b.iter_batched(
            || raw.clone(),
            |owned| extract_old(std::hint::black_box(&owned)),
            BatchSize::SmallInput,
        );
    });
    c.bench_function("cache/extract/new_0copy/100kb", |b| {
        b.iter_batched(
            || raw.clone(),
            |owned| extract_new(std::hint::black_box(owned)),
            BatchSize::SmallInput,
        );
    });

    let _ = std::fs::remove_file(&path);
}

criterion_group!(benches, bench_cache_body_copy);
criterion_main!(benches);

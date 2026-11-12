# Benchmarks

Criterion micro-benchmarks for nub's measured hot paths. Run them before and after any perf change so improvements (and regressions) are demonstrated, not assumed.

## Running

```sh
cargo bench -p nub-core                      # all nub-core benches
cargo bench -p nub-core --bench cache_hash   # one bench target
cargo bench -p nub-core -- topological       # filter by name
```

Criterion records each run under `target/criterion/`; a second run reports the delta vs the stored baseline. HTML reports land in `target/criterion/report/` (install `gnuplot` for line plots, otherwise plotters is used).

## What each bench measures

All live beside this README.

### `cache_hash`

The transpile-cache hashing in `crates/nub-native/src/cache.rs`. The warm-hit path does two full hash passes per lookup: `cache_key` (hashes the key preimage, which includes the entire source text) and `integrity` (re-hashes the stored body to self-heal corruption). Each pass runs under blake3, which `cache.rs` ships, and SHA-256, kept as the baseline, over a realistic ~3 KB TS source (`fixtures/medium.ts`).

- `cache/key_hash/{sha256,blake3}/medium` — one cache-key derivation.
- `cache/integrity_hash/{sha256,blake3}/medium` — one integrity re-hash.

The native cache code returns napi-bridged types, so it cannot link into a bench executable (the `napi_*` symbols resolve only inside Node at dlopen — the same constraint that sets `test = false` on nub-native). The benches reproduce the hashing instead. The key preimage is the cache-schema 4 layout; the current key has a few more short components, which leave the cost dominated by the source text.

### `cache_body_copy`

The warm-hit read in `crates/nub-native/src/cache.rs`: reading a ~100 KB entry, verifying its integrity prefix, and extracting the code, comparing the earlier three-copy extraction with the in-place `drain`.

- `cache/get/{old_3copy,new_1copy}/100kb` — read, verify and extract end to end.
- `cache/extract/{old_2copy,new_0copy}/100kb` — the extraction alone.

### `workspace_filter`

The workspace topo-sort in `crates/nub-core/src/workspace/filter.rs`, run on every `nub run -r` / filtered invocation. The fixture is a synthetic 200-package DAG where each package depends on its three lower-indexed neighbors — a deep, wide layered-monorepo shape that forces many topo waves.

- `workspace/build_dep_graph/200` — parse manifests into the index→deps adjacency.
- `workspace/topological_chunks/200` — Kahn's-algorithm level chunking.

## Not benched

The oxc transpiler (`crates/nub-native/src/transform.rs`) is not benched in-process: `transform` returns napi-bridged types (`OxcError`, `oxc_sourcemap::napi::SourceMap`), so it cannot link into a bench executable. Measuring transpile throughput needs an end-to-end harness that runs through the built Node addon.

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

All live under `crates/nub-core/benches/`.

### `cache_hash`

The transpile-cache SHA-256 work in `crates/nub-native/src/cache.rs`. The warm-hit path does two full SHA-256 passes per lookup: `cache_key` (hashes the key preimage, which includes the entire source text) and `integrity` (re-hashes the stored body to self-heal corruption). Both are reproduced byte-for-byte over a realistic ~3 KB TS source (`fixtures/medium.ts`).

- `cache/key_hash/medium` — one cache-key derivation.
- `cache/integrity_hash/medium` — one integrity re-hash.

The native cache code returns napi-bridged types, so it cannot link into a bench executable (the `napi_*` symbols resolve only inside Node at dlopen — the same constraint that sets `test = false` on nub-native). The benches mirror the exact hashing layout instead; keep them in sync with `cache.rs` if the preimage changes.

### `workspace_filter`

The workspace topo-sort in `crates/nub-core/src/workspace/filter.rs`, run on every `nub run -r` / filtered invocation. The fixture is a synthetic 200-package DAG where each package depends on its three lower-indexed neighbors — a deep, wide layered-monorepo shape that forces many topo waves.

- `workspace/build_dep_graph/200` — parse manifests into the index→deps adjacency.
- `workspace/topological_chunks/200` — Kahn's-algorithm level chunking (the per-wave `remaining`-set rescan).

## Not benched

The oxc transpiler (`crates/nub-native/src/transform.rs`) is not benched in-process: `transform` returns napi-bridged types (`OxcError`, `oxc_sourcemap::napi::SourceMap`), so it cannot link into a bench executable. Measuring transpile throughput needs an end-to-end harness that runs through the built Node addon.

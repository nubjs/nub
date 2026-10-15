# Index remapping benchmark

## What this measures

When a package is installed into several places (the package itself plus peer-dependent
copies), the engine copies the package's file index once per place. This benchmark
times just that copy step — called the "index remap" — for one package of a given
size, placed a given number of times.

The change under test: instead of copying the index for every place, move it into the
last place that needs it and clone it only for the extra peer places. The public store
API and the on-disk cache format do not change. This applies the same change as
[aube PR 1650](https://github.com/aubepkg/aube/pull/1650), inspired by
[upm's avoidance of expanded index copies](https://github.com/unjs/upm/blob/16ad722f616e5ddb7b55f67440d75b4d1a1c2c09/src/link.ts#L611).

## Results

Per-remap medians, ten paired runs (base and candidate alternate so neither side
keeps warmer caches):

| Files | Placements | Base per remap | Candidate per remap | Reduction |
| ---: | ---: | ---: | ---: | ---: |
| 16 | 1 | 1.366 µs | 0.307 µs | 77.6% |
| 16 | 8 | 10.323 µs | 9.644 µs | 6.6% |
| 1,024 | 1 | 69.412 µs | 0.691 µs | 99.0% |
| 1,024 | 8 | 1,647.011 µs | 1,411.378 µs | 14.3% |
| 10,000 | 1 | 1,844.580 µs | 0.963 µs | 99.9% |
| 10,000 | 8 | 17,207.299 µs | 14,742.369 µs | 14.3% |

How to read this: the win is biggest when a package is placed exactly once (the
common case) — the copy disappears. With eight peer placements the extra clones are
real work, so the win is smaller. These are per-package remapping microbenchmarks,
not whole-install timings; frozen installs that skip fresh resolution see no change.
For the 1,024-file/eight-placement case the full batch takes 158.40–160.98 ms before
and 136.39–137.60 ms after. The single-package fixture does not measure grouping
overhead across a large graph. Raw batch timings are in
[results.json](results.json).

## Environment

[Remote validation and measurement run](https://github.com/jdalton/nub/actions/runs/36455148125).
Host: Linux 6.17.0-1022-azure, AMD EPYC 7763 (4 visible CPUs), Rust 1.98.1, release
builds with LTO off and 16 codegen units, base `5fafc732`, candidate
`127913b89dc87b07cf9be617678b7036d86be9a8`.

## Reproduce

Build the candidate and base engines in **separate checkouts with separate Cargo
target directories** — if they share a target dir, Cargo can hand the candidate an
executable built from the base, which silently corrupts the comparison.

1. In the candidate checkout, from `vendor/aube/`:

   ```sh
   unset CARGO_TARGET_DIR
   CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
     mise exec -- cargo build --locked --release -p aube --example index_remap
   cp target/release/examples/index_remap /tmp/index-candidate
   ```

2. In a second checkout at the base revision, copy the candidate's example in, then
   extract the production function the example imports into a module (this is the
   only manual step — the function lives inside `fetch.rs` at the base revision):

   ```sh
   cp $CANDIDATE_CHECKOUT/vendor/aube/crates/aube/examples/index_remap.rs \
     crates/aube/examples/index_remap.rs
   ```

   ```python
   from pathlib import Path
   root = Path("crates/aube")
   source = (root / "src/commands/install/fetch.rs").read_text()
   start = source.index("/// Re-key a canonical-indexed")
   end = source.index("\n#[cfg(test)]", start)
   (root / "src/commands/install/index_remap.rs").write_text(
       "use std::collections::BTreeMap;\n" + source[start:end])
   p = root / "examples/index_remap.rs"
   p.write_text(p.read_text().replace(
       "remap_indices_to_contextualized(input,",
       "remap_indices_to_contextualized(&input,"))
   ```

   Build the same way and copy the executable to `/tmp/index-base`.

3. From the candidate checkout run `compare.py`. It drives both executables with the
   same file count, placements and iterations and prints the table above. It collects
   timings only — it does not diff outputs across builds. The example itself asserts
   per run that every placement is present with the expected file count and size.
   No network or filesystem placement happens inside the timed operation.

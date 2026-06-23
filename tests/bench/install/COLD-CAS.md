# Cold-CAS install benchmark

Measures the un-benched install pipeline — fetch tarball → gzip-decode → tar-unpack → CAS-store write → link into `node_modules` — with the content-addressed store reset to **cold** before each timed run. This is the path the link-side roadmap items target:

- **OPP-2** — eager clonedir tree-build on macOS (the `simple` multi-dep fixture).
- **OPP-5** — CAS rayon chunk size for fat packages (the `fat` fixture: `next` + `typescript` + `@swc/core`, whose tarballs unpack to thousands of files each).

It is the gate for measuring those changes A/B: the harness can reset the store to cold between runs, and separately measures a warm-store run for contrast.

The script is `cold-cas.sh`; it sits alongside the older `run.sh` / `run-warm-gvs.sh` matrices and shares their fixtures + idioms.

## Quick run

```bash
cargo build --release
bash tests/bench/install/cold-cas.sh
```

Options:

```bash
--fixture <name>   # simple | fat (default: both)
--cold-only        # skip the warm contrast leg
--warm-only        # skip the cold leg
--runs <n>         # timed runs per leg (default: cold 5, warm 8)
--no-hermetic      # install against the public npm registry instead of Verdaccio
--no-rss           # skip the peak-RSS measurement pass
--save             # write JSON under results/ (default: a temp dir)
```

## What "cold-CAS" means

The CAS store lives at `$XDG_DATA_HOME/nub/store/v1/files` and the packument/index cache at `$XDG_CACHE_HOME/nub/pm` (the namespaces come from nub's embedder profile; the store path itself is aube's `dirs::store_dir()`). A cold run points both `XDG_DATA_HOME` and `XDG_CACHE_HOME` at fresh empty directories, so the install pays the full fetch+decode+unpack+link cost rather than relinking from a warm store.

hyperfine's `--prepare` wipes and re-creates those directories before **each** timed run, so every iteration starts fully cold. `--prepare` is excluded from the measured wall-clock.

The warm contrast leg populates the store + cache once, then reuses them; only `node_modules` is wiped between runs. It measures the relink-against-warm-store path.

GVS is pinned **off** for both legs (`CI=1`), so the timed path is the materialized fetch+decode+unpack+link path the CAS work lives on. With GVS on, a warm reinstall would relink from the global virtual store and hide the unpack cost the cold measurement is about.

## Hermetic registry

The harness sources `vendor/aube/benchmarks/hermetic.bash`, which brings up a no-uplink Verdaccio on port 4874 (`BENCH_VERDACCIO_PORT`) serving a one-time-warmed local storage so every tarball fetch hits localhost rather than npmjs — the numbers measure nub's code path, not CDN jitter. nub is pointed at it with `nub install --registry "$BENCH_REGISTRY_URL"`.

The first hermetic run warms the Verdaccio storage from npmjs (a one-time network fetch into `~/.cache/aube-bench/registry/`) and installs Verdaccio globally if it is not on `PATH`. Subsequent runs are fully offline. Wipe `~/.cache/aube-bench/registry/` to force a re-warm. The registry warm is scoped to `BENCH_TOOLS=aube,pnpm,npm` (the pnpm-family tarballs nub needs); the harness then pulls each fixture's exact tarballs via uplink before the timed runs, so the warm does not need the slower yarn/deno/bun legs. Override `BENCH_TOOLS` in the environment for the full warm set.

To avoid colliding with another benchmark sharing the default port 4874 + `~/.cache/aube-bench` (e.g. a sibling run of aube's own `bench.sh`), give this harness a private registry:

```bash
BENCH_VERDACCIO_PORT=4894 \
BENCH_HERMETIC_CACHE="$TMPDIR/coldcas-registry" \
  bash tests/bench/install/cold-cas.sh
```

Pass `--no-hermetic` to skip Verdaccio and install against the public npm registry. Those numbers carry CDN jitter and are labelled as such in the output.

## What's measured

- **Wall-clock** — hyperfine, median ± σ over N runs, exported as JSON.
- **Peak RSS** — a separate single install under `/usr/bin/time` (`-l` on macOS, `-v` on GNU/Linux). hyperfine does not capture RSS, so this is its own pass. macOS reports both "maximum resident set size" and "peak memory footprint"; this harness parses the former. Peak RSS matters for OPP-5's chunk-size memory tradeoff (a larger chunk size trades RSS for throughput).

Report median and σ. σ-overlap between A/B runs is a tie, not a win.

## Fixtures

| Fixture | Shape | Targets |
|---------|-------|---------|
| `simple` | ~435 packages, single project (express/react/typescript/vite/…) | OPP-2 — many small packages, clonedir tree-build |
| `fat` | `next` + `typescript` + `@swc/core` | OPP-5 — packages whose tarballs unpack to thousands of files |

`simple` is shared with `run.sh`. `fat` is new for this benchmark.

Lockfiles are committed (`pnpm-lock.yaml`) so resolution is deterministic across runs. Regenerate the `fat` lockfile after editing its `package.json`:

```bash
cd tests/bench/install/fixtures/fat
rm -rf node_modules pnpm-lock.yaml
pnpm install --no-frozen-lockfile
rm -rf node_modules
```

## Results

By default the script writes JSON to a temp directory. Pass `--save` to update checked-in JSON under `tests/bench/install/results/` (`coldcas-<fixture>-<ts>.json`, `warmcas-<fixture>-<ts>.json`).

## Caveats

- Cold-reset correctness depends on `XDG_DATA_HOME` + `XDG_CACHE_HOME` fully controlling the store + cache. nub's store/cache resolution honors both (`aube-store/src/dirs.rs`, `nub-cli/src/pm_engine`); a stray pre-existing `~/.cache/nub` or `~/.local/share/nub` is NOT consulted because the timed command overrides both env vars.
- RSS is the peak of a single representative install, not an average — it is the right metric for the chunk-size memory ceiling but is noisier run-to-run than the wall-clock median.
- Run on a quiet machine. Install timings are sensitive to filesystem load, CPU contention, Spotlight indexing, and concurrent builds.
- The default registry warm is scoped to `aube,pnpm,npm` because the full aube warm set also runs yarn/deno/bun legs, and a host that routes `yarn`/`pnpm` through an interactive package-manager shim can stall those legs. nub only needs the pnpm-family tarballs, so the scoped warm is both faster and more robust.

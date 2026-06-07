# Yarn PnP test harness — how this feature is iterated on

This directory is the working system for developing and regression-testing nub's Yarn Plug'n'Play support. It exists because PnP behavior cannot be faithfully unit-tested in Rust: the `.pnp.cjs` `_resolveFilename` patch, the `pnpapi` builtin, zip-stored packages, and Node's per-version ESM format-detection quirks only reproduce against a real `yarn install`, exercised by a real nub binary, on a real Node. The design rationale and the decision record live in `wiki/research/pnp-preload-feasibility.md`; this README documents the *iteration loop* so a future agent can reproduce it in minutes instead of rediscovering it.

## The loop

1. `make-fixture.sh [dest]` builds a reproducible Yarn 4 PnP project (default `/tmp/nub-pnp-fixture`). It installs three deps chosen because they resolve through paths that fail *independently*: `lodash` (CJS, required and imported), `chalk@5` (pure ESM, `"type":"module"`, zip-stored), and `cowsay` (CJS package shipping a bin, zip-stored).
2. `run-pnp-matrix.sh [nub-binary] [node-bin-dir ...]` runs every scenario against the fixture for each Node version. With no node dirs it sweeps `~/.nvm/versions/node/*`; pass explicit bin dirs to target specific versions (or a container's `/usr/local/bin`).
3. `docker-matrix.sh [version ...]` does the same on Linux, one container per Node version (see `Dockerfile.pnp`), building a Linux nub once and layering it onto each `node:<ver>` base.

The fast inner loop during development is: edit `runtime/*.cjs|mjs` (read at spawn — no rebuild needed for JS-only changes), then `run-pnp-matrix.sh target/debug/nub <dir1> <dir2> …`. Rust changes need `cargo build` first. The Docker leg is for Linux/floor confirmation before relying on a result, per the repo's "use Docker instead of declaring things untestable" rule in `AGENTS.md`.

## Why version-switching via PATH

nub discovers its Node from `PATH`, so `PATH="<nvm-version>/bin:$PATH" nub …` is the cheapest way to drive nub onto a specific Node and flip between the **fast tier** (Node 22.15+, sync `module.registerHooks`) and the **compat tier** (18.19–22.14, async loader-worker via `module.register`). Those two tiers take different PnP code paths and broke differently, so every scenario must be checked on both. The dev box runs a single modern Node (often 26), which masks every compat-tier and floor-only defect — version-switching (and Docker) is how you stop trusting a green run on one Node.

## The scenarios, and what each guards

| Scenario | Path exercised |
| --- | --- |
| `cjs` | `require()` of a CJS PnP dep — `--require .pnp.cjs` + the fast-tier `_resolveFilename` PnP branch |
| `esm-cjsdep` | `import` of a CJS dep from ESM — the CJS-from-ESM sub-loader (where PnP's fs patch is live) |
| `esm-puredep` | `import` of a pure-ESM **zip-stored** dep — the resolve hook must emit an explicit `format`, or Node ≤20.11 mis-tags it CJS → `ERR_REQUIRE_ESM` |
| `run` | `nub run` of a script using a PnP dep — the `compute_augmentation_env` NODE_OPTIONS path |
| `nubx` | `nubx <bin>` for a zip-stored bin — the `pnpapi` bin-resolver plus running a zip-internal entry |
| `node-off` | `nub --node` must **not** resolve the dep — proves augmentation (and PnP) is fully disabled |

## Known gap, asserted not failed

`nubx <bin>` where the bin's script is still inside a `.yarn/cache/*.zip` works only on the **fast tier** (Node 22.15+). On the compat tier the `--import` preload routes the entry through the ESM loader, whose existence check bypasses PnP's `fs` patch, so a zip-internal entry path can't be read. The matrix marks this `~(known-compat-gap)` rather than failing — if a future change makes it pass on compat Node it flips to a visible `✓(gap-now-fixed!)`. Workarounds today: run `nubx` on Node 22.15+, or `yarn unplug <pkg>`. Everything else passes across the whole supported range (18.19+).

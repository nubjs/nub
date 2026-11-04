# Package install benchmarks

Wall-clock comparison of `nub install` vs `pnpm install`, `bun install`, and `npm ci` on frozen lockfiles. The primary measurement is the warm install: warm CAS store + lockfile present, `node_modules` wiped, then a full offline reinstall.

## Quick run

```bash
cd /path/to/nub
cargo build --release -p nub-cli
bash tests/bench/install/run-warm-gvs.sh
```

For a single nub/bun/pnpm/npm warm-install table on one fixture (default `tanstack-start`): `bash tests/bench/install/run-4way.sh`.

For the older fixture matrix:

```bash
bash tests/bench/install/run.sh --fixture t3 --warm-only
bash tests/bench/install/run.sh --materialized
```

## Warm install — GVS eligibility

`nub install`'s warm-install speed comes from its global virtual store. `node_modules` stays project-local, but with GVS on the inner package under `.store/` is hardlinked from a shared store instead of materialized per project. A warm reinstall becomes a relink against an already-materialized store.

The main harness exercises both sides of the compatibility split:

| Fixture | Script | What it measures |
|---------|--------|------------------|
| GVS eligible | `run-warm-gvs.sh --fixture gvs-eligible` | `nub install` warm-install time vs pnpm where GVS stays on. |
| GVS ineligible | `run-warm-gvs.sh --fixture gvs-ineligible` | A `next` project where GVS auto-disables and nub is roughly pnpm parity. |

Nub's trigger list is `next` and `react-native`. `vite`, `vitepress`, and `@sveltejs/kit` are not triggers in Nub.

```bash
NUB=/path/to/target/release/nub bash tests/bench/install/run-warm-gvs.sh
NUB=/path/to/target/release/nub bash tests/bench/install/run-warm-gvs.sh --fixture gvs-eligible --runs 12 --warmup 3
```

## Published numbers (ubuntu-latest CI)

The warm-install numbers on the homepage, the `introducing-nub` blog post, and the install docs come from a dedicated 3-bar harness, `run-hardlink-vs-gvs.sh`, run on a near-idle GitHub Actions `ubuntu-latest` runner. The harness lives on the `bench-adhoc` branch (a [`ci-adhoc-test`](../../../.claude/skills/ci-adhoc-test/SKILL.md)-style branch-scoped probe, not merged to `main`) alongside its workflow: [`.github/workflows/bench-adhoc.yml`](https://github.com/nubjs/nub/blob/bench-adhoc/.github/workflows/bench-adhoc.yml) and [`run-hardlink-vs-gvs.sh`](https://github.com/nubjs/nub/blob/bench-adhoc/tests/bench/install/run-hardlink-vs-gvs.sh).

Protocol: `large` fixture (1,168 packages, 81,398 files), warm CAS store + frozen lockfile, `node_modules` wiped between runs via hyperfine `--prepare` (untimed), no network. `hyperfine -N`, 25 runs / 6 warmup. Tool versions: bun 1.3.14, pnpm 10.34.4, npm on Node 24, nub 0.4.2.

| tool / mode | mean ± σ |
|---|---|
| npm | 12.945 s ± 0.269 s |
| pnpm | 3.453 s ± 0.395 s |
| bun (flat hoisted, per-file hardlink) | 1.896 s ± 0.166 s |
| nub — hoisted, hardlink (`--node-linker hoisted`) | 1.461 s ± 0.207 s |
| nub — default (GVS, O(packages) symlink relink) | 346.1 ms ± 10.5 ms |

Ratios: nub-default is 5.48× faster than bun and 4.22× faster than nub-hoisted; nub-hoisted is 1.30× faster than bun.

The three bars tell a deliberate story, in order: bun is the baseline; `nub --node-linker hoisted` reproduces bun's own layout and per-file hardlink syscall, so that comparison is same-regime, not a different algorithm; the default GVS row then relinks one symlink per package instead of one hardlink per file, which is where the O(packages)-vs-O(files) win comes from. **Linux only, deliberately** — Linux's `auto` package-import-method resolves to a hardlink for both bun and nub, so the two tools share one primitive and the comparison is clean. On macOS `auto` resolves to APFS clonefile for both, which muddies the hardlink-vs-GVS story.

Source: [GitHub Actions run 28922376170](https://github.com/nubjs/nub/actions/runs/28922376170), `hardlink-vs-gvs` job, `bench-adhoc` branch. No hyperfine `--export-json` output exists for this run — `run-hardlink-vs-gvs.sh` doesn't pass `--export-json` — so there is no results JSON to check in for this table; it's transcribed from the job's `hyperfine` log output.

## Older install matrix

The older `run.sh` matrix covers frozen/offline warm and cold installs across these fixtures.

| Fixture | Packages | Description |
|---------|----------|-------------|
| `simple` | ~342 | Single-package project: express, react, typescript, vite, lodash, axios, zod, … |
| `monorepo` | ~407 | Four-workspace monorepo using `workspace:*`; npm is skipped. |
| `t3` | ~222 | Bun's create-t3-app benchmark fixture (Next — GVS auto-disables). |
| `tanstack-start` | ~313 | Real TanStack Start (Vite + React) starter; GVS stays on. See `fixtures/tanstack-start/README.md`. |
| `large` | ~1168 | React + MUI + webpack + Babel + TypeScript + ESLint. |

```bash
bash tests/bench/install/run.sh
bash tests/bench/install/run.sh --fixture t3 --warm-only
bash tests/bench/install/run.sh --cold-only
```

## Results

By default, scripts write JSON to a temp directory. Pass `--save` to update checked-in JSON under `tests/bench/install/results/`.

`run.sh`, `run-warm-gvs.sh`, and `cold-cas.sh` stamp each file with a top-level `env` object: `platform`, `arch`, `timestamp_utc`, the 1-minute `load_1min` at measurement time, and a `versions` map holding the version of each tool that ran.

## Methodology notes

The timed warm command starts with no `node_modules`. The harness uses hyperfine `--prepare` to rename `node_modules` aside before each timed run, then reaps it in the background. Teardown is not timed.

The CAS store and GVS are never cleared between warm runs. Clearing them would make the run cold, not warm.

Report median and σ. σ-overlap between tools is a tie, not a win.

## Fixtures

Fixtures live under `tests/bench/install/fixtures/`. Lockfiles are committed so each tool resolves consistently, and each harness strips the foreign lockfiles from a tool's isolated workdir so every tool installs from its OWN lockfile:

- `nub.lock` for Nub — its native lockfile, so the nub leg exercises nub's own install path, not the `pnpm-lock.yaml` compat-read
- `pnpm-lock.yaml` for pnpm
- `bun.lock` for Bun
- `package-lock.json` for npm where supported

Regenerate the nub, pnpm, and Bun lockfiles:

```bash
bash tests/bench/install/gen-fixtures.sh
```

Regenerate npm lockfiles:

```bash
for f in simple t3 large; do ( cd tests/bench/install/fixtures/$f && npm install --package-lock-only --ignore-scripts ); done
```

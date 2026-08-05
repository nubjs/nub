---
name: benchmarking
description: Comparative install-benchmarking methodology for nub vs npm/pnpm/bun — cold/warm protocol, genuine-cold cache isolation, load-robust measurement, and the anti-juicing honesty bar. Invoke (via the Skill tool) whenever you need to benchmark `nub install` against another package manager, produce or update the homepage/blog install numbers, or verify a perf claim before it ships. Encodes the hard-won gotchas: time setup OUTSIDE the measurement (hyperfine `--prepare`), the cache lives on DISK so env-var isolation is NOT trustworthy (bun ignores `BUN_INSTALL_CACHE_DIR`/`$HOME` — wipe the real path), VERIFY every cold is genuine via an offline-fails check, and only measure wall-clock on a quiet machine (gate on low load) with file counts as a load-independent cross-check. Pairs with `pm-perf-tracing` for the internal Rust phase decomposition.
metadata:
  internal: true
---

# benchmarking

Honest, reproducible **comparative install benchmarks** of `nub install` against npm / pnpm / bun — the external, wall-clock-and-file-count method. For decomposing where the time goes INSIDE a single nub install, use [`pm-perf-tracing`](../pm-perf-tracing/SKILL.md) instead.

Applies to: benchmarking against another PM, refreshing homepage/blog install numbers, verifying a perf claim before it ships. **A single non-genuine cell discredits the whole table.**

## The tool: hyperfine

`/opt/homebrew/bin/hyperfine` is the canonical timer. The load-bearing flag is `--prepare`, which runs setup BEFORE each timed run, untimed. **Never time setup.**

```sh
# COLD: empty the tool's REAL cache + wipe node_modules before each run (untimed), then time the install.
hyperfine --warmup 0 --runs 5 \
  --prepare 'rm -rf node_modules && d="$(bun pm cache)" && rm -rf "${d:?}"' \
  'bun install --ignore-scripts'

# WARM-RELINK: cache populated, wipe ONLY node_modules before each run.
hyperfine --warmup 1 --runs 5 \
  --prepare 'rm -rf node_modules' \
  'nub install --ignore-scripts'

# WARM-SAT: node_modules already present (idempotency path) — no prepare wipe.
hyperfine --warmup 1 --runs 5 'nub install --ignore-scripts'
```

Prefer median + spread (min–max) over the mean — contention skews the mean.

## The cold / warm protocol

`node_modules` is deleted before EVERY timed run, cold and warm, always in `--prepare`. The cold/warm axis differs only in global-cache state:

- **cold** — the tool's real cache is EMPTY (genuine download).
- **warm-relink** — cache populated, `node_modules` wiped (the link-from-store path; the homepage number).
- **warm-sat** — `node_modules` already present; a separate, clearly-labeled scenario, not the headline.

## GENUINE-COLD per tool — the cache is on DISK

A "cold" run is only cold if the tool's real on-disk cache is gone. Setting a cache-dir env var does not guarantee that — **bun ignores `BUN_INSTALL_CACHE_DIR` and `$HOME`**, resolving its cache via the OS passwd home. You must wipe the disk path.

| tool | real cache path | clear command for cold |
|---|---|---|
| nub | its store (`NUB_CACHE_DIR` + `XDG_DATA_HOME`/`XDG_CACHE_HOME`) | `rm -rf "$NUB_CACHE_DIR" "$XDG_DATA_HOME" "$XDG_CACHE_HOME"` |
| npm | `~/.npm/_cacache` (or `--cache <dir>`) | `rm -rf <cache>` (the `--cache` dir you pass) |
| pnpm | `pnpm store path` (or `--store-dir <dir>`) | `rm -rf <store>` (or `pnpm store prune`) |
| **bun** | **`bun pm cache`** = the real `~/.bun/install/cache` | **`rm -rf "$(bun pm cache)"`** — env vars do NOT relocate it |

## VERIFY each cold is genuine — the offline-fails check

After wiping a cache, an `--offline` install MUST FAIL. A pass means the cache wasn't cleared (wrong disk path), and any "cold" number from it is a warm-link artifact. Run this for every tool before trusting a cold number:

| tool | expected after cache wipe | genuine? |
|---|---|---|
| nub | `rc≠0`, "not available in the local cache" | FAIL → genuine |
| pnpm | `rc=1`, `ERR_PNPM_NO_OFFLINE_TARBALL` | FAIL → genuine |
| npm | `rc≠0`, cache-miss error | FAIL → genuine |
| bun | if `bun install --offline` SUCCEEDS → cache NOT cleared → bun cold is NOT genuine |

A true bun cold needs a clean container, or wiping the user's real cache (destructive — don't, unless in Docker).

## Apples-to-apples isolation

- Each tool gets its OWN cache dir.
- `--ignore-scripts` for ALL tools (or `--allow-scripts` for all — same on both sides).
- Identical fixture, identical lockfile throughout (use a `--frozen-lockfile`/`--frozen` equivalent where the tool offers one).
- **Interleave tool order round-robin** (nub → npm → bun → pnpm, repeat) — never all-of-one-then-the-other — so drift in host load hits every tool equally.

## Load discipline — measure only on a quiet machine

- **Check the load average before measuring and only proceed when the machine is quiet.** A dedicated quiet box or a CI runner near zero load is ideal for anything that will be published.
- **On a shared dev host, WAIT for load to fall below a threshold — don't measure above it.** A practical gate is a 1-minute load average under ~40 (pick a threshold the machine actually reaches; below ~5 is ideal on a dedicated box). Poll, wait, then run; abort and retry if it spikes mid-run.
- **Report the load that held during the run** alongside the numbers.
- **File counts and store-entry counts are exact and load-independent** — a robust cross-check, and the clearest way to tell the dedup story. They complement a properly-measured time, not replace it.

## File-count forensics (the load-independent crux)

```sh
find node_modules -type f -o -type l | wc -l          # total materialized entries
ls -d node_modules/**/core-js node_modules/core-js    # physical copies of a duplicated dep (dedup story)
```

Reach for this first — exact, reproducible, immune to load.

## The honesty bar (anti-juicing)

- VERIFY every cold is genuine (offline-fails check) BEFORE citing it.
- **NEVER compare one tool's genuine-cold to another's warm-link** — the exact misleading comparison the offline check exists to prevent.
- Report what was actually measured, caveats included (which colds are genuine, host load, sample size).
- The homepage cites the WARM number because it is the honest, reproducible one.
- A single non-genuine cell discredits the whole table. When in doubt, exclude the cell and say why.

## Process hygiene (this runs on the maintainer's machine)

Short-lived install/measurement processes reap themselves. The hazard is a **long-lived process a bench starts and forgets**:

- **Never leave a dev server running.** If a bench starts one, tear it down in the same run (`trap '…kill…' EXIT INT TERM`).
- **Docker: `docker run --rm`, and confirm `docker ps` is empty when done.**

## Reference template

`/tmp/cs-bench-final.sh` is a working 4-tool harness (nub / npm / bun / pnpm) — per-tool isolated cache dirs, `--ignore-scripts` for all, interleaved round-robin order, `node_modules` wiped before every run, median/spread helpers. Read its structure before hand-rolling a new one; adapt the cache paths and fixture, keep the protocol.

## Internal decomposition

When a comparative number raises "WHY is nub's phase X slow?", switch to [`pm-perf-tracing`](../pm-perf-tracing/SKILL.md): `RUST_LOG=debug nub install` for the `phase:resolve/fetch/link` split, and the gated `AUBE_DIAG_FILE` per-file linker strategy tally.

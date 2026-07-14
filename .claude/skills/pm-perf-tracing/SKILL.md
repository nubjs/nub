---
name: pm-perf-tracing
description: Performance-trace Nub package-manager installs using the existing phase timings, structured diagnostics, and sampling-profiler workflow. Use when an install or package-manager operation is unexpectedly slow and the bottleneck needs to be localized before changing code.
metadata:
  internal: true
---

# pm-perf-tracing

How to performance-trace the nub package manager (install/resolve/fetch/**link**) and find where the time actually goes — the method that cracked the hoisted-linker slowness (10.8s → root-caused to a per-file copy loop). Reach for this any time a `nub install` / PM operation is "mysteriously slow" — do NOT reverse-engineer from source; the instrumentation already exists, turn it on.

## The two layers that already exist

1. **Phase timings — works under nub TODAY.** `RUST_LOG=debug nub install` emits `phase:<name> <elapsed>` lines on stderr: `phase:resolve`, `phase:fetch (N packages)`, `phase:link (N files)`, `phase:link_bins`. This alone tells you the coarse split (is it network/resolve, or linking?). The aube engine wires these through the `tracing` crate.

2. **Rich diagnostics layer — native under nub via `NUB_DIAG_*`.** `aube_util::diag` emits structured JSONL to `NUB_DIAG_FILE=<path>` — including, for the linker, one event per file naming the strategy used (`link_clonedir`, `link_reflink`, `link_macos_small_copy`, `link_copy`, `link_hardlink`). This is the load-independent crux for a link-perf question. The full knob set (all off by default, zero cost when unset):

   - `NUB_DIAG_FILE=<path>` — JSONL events to a file.
   - `NUB_DIAG_PRINT=1` — live per-span lines to stderr.
   - `NUB_DIAG_SUMMARY=1` — end-of-run aggregate table.
   - `NUB_DIAG_CRITPATH=1` — retain records for the critical-path / lifecycle / starvation analyzers.
   - `NUB_DIAG_THRESHOLD_MS=<n>` — filter live prints below `<n>` ms.
   - `NUB_DIAG_KERNEL=1` — `getrusage` kernel deltas around phases.
   - `NUB_BENCH_PHASES_FILE=<path>` — per-run phase-timing JSON for the bench harness.

```sh
cargo build -p nub-cli --profile fast        # dev binary at <worktree>-target/fast/nub
NUB=<worktree>-target/fast/nub
NUB_DIAG_FILE=/tmp/d.jsonl NUB_DIAG_SUMMARY=1 "$NUB" install --offline
grep -o '"name":"link_[a-z_]*"' /tmp/d.jsonl | sort | uniq -c     # per-strategy tally
```

(The diag layer reads `NUB_DIAG_*` under the nub embedder profile via the `diag_env_prefix` hook — no source edit needed. `AUBE_DIAG_*` is NOT read under nub. The `dev-tracing-telemetry` thread / `wiki/research/dev-tracing-telemetry.md` tracks the optional chrome-trace/flamegraph export layer on top.)

## The measurement discipline (load-independent — this host is permanently contended)

The dev box runs load ~30–50 and never goes quiet, so **absolute wall-clock is untrustworthy**. Measure things contention can't ruin:

- **Verified-clean warm loop:** `rm -rf node_modules` and *assert it's gone*, warm store already populated, `--offline` (proves zero network: `phase:fetch` shows `0 packages`), and check **rc=0** on every run (a timing from an errored install — e.g. npm's `rm: Directory not empty` purge failures → rc=254 — is garbage).
- **Strategy tally, not seconds:** "75,079/76,167 files took `link_macos_small_copy`, 0 took `link_clonedir`" is a fact regardless of load. That's what proves a design gap.
- **Back-to-back A/B on the same box, report the RATIO:** e.g. hoisted vs `--node-linker isolated` on the same fixture, same load window → the relative delta (≈26×) is robust even when both absolutes are inflated.
- For a real clean wall-clock number, hand it to a quiet machine / CI runner — never block on this box settling.

## Layout matters — always check which linker path runs

nub mirrors the incumbent layout: an npm/yarn/bun lockfile → **hoisted** layout (`link.rs` → `hoisted::link_hoisted_importer`); nub-identity / `--node-linker isolated` → **isolated** layout (`materialize_into`, the only path with the whole-dir `clonefile(2)` fast path). They have completely different perf characteristics. When a perf question is about linking, run BOTH (`--node-linker isolated` vs default) and diff — that A/B is what localized the hoisted-linker gap.

## When spans aren't enough — sampling profiler

Span/JSONL instrumentation can distort a syscall-bound, parallel pass (observer effect). For "where inside the link phase do the syscalls go" use a sampling profiler on a **release** build: `samply record -- <NUB> install --offline` (macOS/Linux), or `cargo flamegraph`. Spans tell you *which phase/strategy*; the sampler tells you *which syscalls/functions* dominate.

## Fixtures

- `/tmp/coffee2-demo` — CoffeeScript 2.0.1, npm `lockfileVersion:1`, 519 pkgs / ~76k hoisted files. The canonical heavy-hoisted-layout fixture.
- A minimal hoisted repro: a `package.json` + `package-lock.json` with `webpack@3.6.0` + `underscore` (no `node_modules`).

## The one-liner to remember

`RUST_LOG=debug nub install` for the phase split; if it's `phase:link`, set `NUB_DIAG_FILE=…` for the per-file strategy tally; A/B against `--node-linker isolated`; judge by strategy-tally + ratio, never the contended absolute.

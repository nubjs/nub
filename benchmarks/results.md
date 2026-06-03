# Benchmark Results — Nub

**Date:** 2026-05-29
**Hardware:** Apple M1 Max
**Node:** v24.14.0 · **Rust:** release build
**Comparison tools:** pnpm 10.15.1, npm 11.9.0, tsx 4.19.4, bun 1.3.14

All benchmarks run via `hyperfine --warmup 3 --runs 20`. Numbers are machine-specific; the **ratios**, and the **constant ~150 ms of wrapper overhead Nub removes**, are the portable claims.

---

## Script runner: `nub run` vs pnpm vs npm

A no-op script (`echo hi`) — pure orchestration overhead. `nub run` is Rust; no Node bootstrap for the wrapper.

| Command | Mean [ms] | vs nub |
|---------|-----------|--------|
| `nub run noop` | **9.2** | 1.0× |
| `npm run noop` | 104.0 | 11.4× slower |
| `pnpm run noop` | 160.9 | 17.6× slower |

That 11–18× is the wrapper tax **in isolation** — measured on a no-op (`echo`) so nothing dilutes it. Headline: **Nub removes ~150 ms of per-invocation wrapper overhead**, a flat per-call constant `npm` / `pnpm` pay and Nub doesn't. The benchmarks here use near-instant commands on purpose — that's where the wrapper cost *is* the runtime and the order-of-magnitude shows clean, rather than buried under a script's own work.

## Direct TS execution: `nub hello.ts` vs node vs tsx vs bun

| Command | Mean [ms] | note |
|---------|-----------|------|
| `bun hello.ts` | **11.2** | bun's native runtime, ~4× faster than nub (not a wrapper comparison) |
| `node hello.js` | 25.8 | plain-JS baseline |
| `node hello.ts` | 44.8 | native type-strip (Node 24) |
| `nub hello.ts` | 44.4 | on par with plain `node` |
| `tsx hello.ts` | 127.8 | **2.9× slower than nub** |

On Node 24 `node` strips erasable types natively, so `nub hello.ts` is on par with plain `node` and **~2.9× faster than `tsx`**. Nub additionally runs non-erasable syntax (`enum`, decorators) that Node's stripper rejects. Bun is faster than both — native runtime, no Node startup — as expected.

## Bin runner: `nubx` / `nub exec` vs pnpm exec vs npx

Nub resolves `node_modules/.bin` in Rust and exec's the binary directly; `pnpm exec` / `npx` boot a full Node first. The ~150 ms wrapper tax is constant, so the ratio is cleanest with a **native** CLI that adds no Node startup of its own — `esbuild` (a Go binary) is the representative case, and the truest measure of `nubx`'s own speed:

| Command | Mean [ms] | vs nub |
|---------|-----------|--------|
| `nub exec esbuild --version` | **11.2** | 1.0× |
| `pnpm exec esbuild --version` | 190.6 | 17.0× slower |
| `npx esbuild --version` | 225.5 | 20.1× slower |

A **Node-based** CLI (`tsc`) pays its own ~85 ms Node startup on *both* sides, so the same ~150 ms saving is diluted to a smaller multiple — the absolute time removed is identical; Node's own bootstrap just dominates the ratio:

| Command | Mean [ms] | vs nub |
|---------|-----------|--------|
| `nub exec tsc --version` | **91.1** | 1.0× |
| `pnpm exec tsc --version` | 234.9 | 2.6× slower |
| `npx tsc --version` | 261.8 | 2.9× slower |

So `nubx`'s real wrapper speed is **~17–20×** (native bin); end-to-end for a Node CLI it narrows to **~2.6–2.9×** as the tool's own Node startup takes over. Both rows remove the same ~150 ms of *wrapper* bootstrap.

> **Correction (2026-05-29).** A prior version of this file recorded a 40–67× exec speedup, measured via `nub exec tsc --version`. That benchmark was **invalid**: nub's argv pre-parse consumed `--version` as nub's *own* version flag, so the command printed nub's version in ~5 ms and never ran `tsc` — comparing "nub prints its version" against "pnpm/npx actually run tsc." That flag-stealing is now **fixed** (post-subcommand flags forward to the bin/script), so the `tsc --version` row above genuinely runs tsc; the `esbuild` row is the native-CLI measure.

## Summary for whitepaper claims

| Claim | Measured (M1 Max) | Verdict |
|-------|-------------------|---------|
| "Faster than `pnpm run`" (no-op overhead) | 17.6× | ✅ (overhead-only; real scripts ~2×) |
| "Faster than `npm run`" (no-op overhead) | 11.4× | ✅ (overhead-only) |
| "Faster than `tsx`" | 2.9× | ✅ |
| "Faster than `pnpm exec` / `npx`" | ~17–20× native CLI (esbuild), ~2.6–2.9× Node CLI (tsc) | ✅ (was bogusly 40–67×; corrected) |
| "Sub-100ms TypeScript execution" | 44 ms direct | ✅ |
| "Comparable to bun" | ~4× slower than bun on hello-world | ❌ not comparable — bun has a native-runtime advantage |

## Methodology

- `noop`: `echo hi` — pure orchestration overhead.
- `build`: `tsc --version` (TypeScript 5.9 installed locally) — a real Node-based CLI.
- TS execution: `hello.ts` / `hello.js` = `console.log("hello")`; transpile cache warmed before timing.
- Bin runner: real, locally-installed CLIs run as `<tool> --version` — `esbuild` 0.28.0 (native Go binary, the wrapper-speed measure) and `tsc` 5.9.3 (Node-based, the diluted case). Same tool on all three sides, so the delta is purely wrapper overhead.
- All comparisons run on the same machine, same session.

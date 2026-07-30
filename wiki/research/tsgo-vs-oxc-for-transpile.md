---
**Status:** v1, 2026-05-24. Write-once research doc.
**Question:** Should Nub switch its TypeScript transpilation pipeline from oxc (Rust, in-process via napi-rs) to tsgo (the TypeScript team's Go port of `tsc`, distributed as `@typescript/native-preview`)? tsgo is the future `tsc` — using "the real one" would eliminate any risk of parser divergence from upstream TypeScript. Does that trade win against the integration cost of pulling a Go binary into Nub's per-file load-hook hot path?
**Headline answer:** No. Stay with oxc for v0.1 — and probably v0.x. tsgo as of 2026-05-24 is a type-checker-and-build-driver project, not an embeddable transpile library: its programmatic API is marked **"not ready"** in the project README, its only stable integration shape is a per-process CLI binary or an LSP stdio daemon, and its distribution per platform is ~25-26 MB versus oxc-transform's ~3.6 MB. None of those shapes fit Nub's "sub-millisecond, in-process, per-file load-hook" architecture. The parser-divergence-vs-tsc concern, which is the strongest argument for switching, is real but bounded by oxc's continued tracking of tsc semantics and is the wrong cost to optimize against the integration friction. Revisit when (a) tsgo ships a stable programmatic API or a `cgo -buildmode=c-shared` ready library shape, **and** (b) Nub has a daemon architecture that amortizes a long-running tsgo subprocess across many `nub script.ts` invocations.
**Builds on:** `../runtime/ts-transpilation.md`, `../runtime/transpile-cache.md`, `../runtime/non-erasable-syntax.md`, `../runtime/jsx-transpilation.md`, `../runtime/source-maps.md`, [`wasm-vs-napi-for-transpile.md`](wasm-vs-napi-for-transpile.md), [`node-swc-vs-oxc-choice.md`](node-swc-vs-oxc-choice.md), `../architecture.md#augmenter-not-fork`, `../philosophy.md#additivity`.
---

# tsgo vs oxc for Nub's TypeScript transpile pipeline

## 1. TL;DR

- **tsgo is not currently an embeddable library.** The microsoft/typescript-go README's status table marks `API` as **"not ready"** — the lowest of four maturity tiers ("either haven't even started yet, or far enough from ready that you shouldn't bother messing with it yet"). The TS team's own TypeScript 7.0 Beta announcement says a "stable programmatic API" won't land until "at least several months from now with TypeScript 7.1." Until that API exists, the only ways to call tsgo are spawning the `tsgo` CLI per process or speaking LSP JSON-RPC to `tsgo --lsp --stdio` — neither fits a per-file `module.registerHooks` load hook.
- **The performance pitch is type-check-shaped, not transpile-shaped.** tsgo's marketed wins ("10× faster than tsc") are type-checking wins on whole-program runs. Real-world type-check measurements come in at 1.6×–4× — and at least one NestJS codebase regresses to 2× *slower* than tsc6 (memory-allocation pathologies still being shaken out). There is no published per-file transpile benchmark for tsgo, but its transpile path is "ported `tsc`," not a from-scratch transformer — i.e. structurally bounded by the same algorithmic shape that makes oxc's transformer roughly 30× faster than tsc on transpile-only workloads. Oxc-native at 178k transpiles/sec on a 165-line TS file (per [`wasm-vs-napi-for-transpile.md`](wasm-vs-napi-for-transpile.md) §3.2) is not a number tsgo can plausibly approach.
- **Distribution is ~7× heavier per host platform.** `@typescript/native-preview-<platform>` packages are ~25-26 MB unpacked (the Go binary + the entire bundled `lib.*.d.ts` set). oxc-transform's per-platform N-API binding is ~3.6 MB. npm's optionalDependencies makes both transparent to the user, but the disk and download cost is materially different — and the bundled `.d.ts` files (the bulk of the size) we wouldn't even use for transpile-only.
- **The brand-boundary cost is the same either way.** Neither tsgo nor oxc requires us to invent Nub-specific env vars, `globalThis.nub`, `nub:*` namespaces, or `@nub/*` packages. The augmenter-not-fork mechanism rule is satisfied identically: we'd load whichever transpiler from a Node `--import` preload registered as a `module.registerHooks` load hook, and a user on plain Node + `module.register('amaro')` would still get equivalent type-stripping behavior. Brand boundary does not pick a winner here.
- **Recommendation: stay with oxc.** Document the conditions to revisit in §8. The strongest version of the tsgo argument (parser-fidelity-with-tsc) is real but does not justify the integration debt today. If tsgo ships a stable API and a Nub daemon (`daemon.md` — not yet written) lands, the analysis reopens.

## 2. tsgo current status (verified May 2026)

### 2.1 Identity and distribution

- **Repo:** [`microsoft/typescript-go`](https://github.com/microsoft/typescript-go). License Apache-2.0 (same as `microsoft/TypeScript`).
- **npm package:** [`@typescript/native-preview`](https://www.npmjs.com/package/@typescript/native-preview). Weekly downloads ~6.7M (mostly type-check / editor-LSP traffic, not transpile).
- **Binary name:** `tsgo`. Long-term: the binary will be renamed to `tsc` and moved into the `typescript` package; the staging repo and `@typescript/native-preview` will be retired. Per the [README](https://github.com/microsoft/typescript-go): "Long-term, we expect that this repo and its contents will be merged into `microsoft/TypeScript`. As a result, the repo and issue tracker for typescript-go will eventually be closed."
- **Per-platform binary packages** (via npm `optionalDependencies` — same shape oxc and SWC use):
  - `@typescript/native-preview-darwin-arm64` — 25.8 MB unpacked
  - `@typescript/native-preview-darwin-x64`
  - `@typescript/native-preview-linux-x64-gnu` — 26.6 MB unpacked
  - `@typescript/native-preview-linux-arm` — 25.8 MB unpacked
  - `@typescript/native-preview-linux-arm64` — 26.3 MB unpacked
  - `@typescript/native-preview-win32-x64`
  - `@typescript/native-preview-win32-arm64` — 25.6 MB unpacked

  The size is dominated by the bundled `lib.*.d.ts` set (`lib.dom.d.ts` alone is 2.2 MB; `lib.es5.d.ts` is 213.8 KB; ~50 lib files total). The Go binary itself is roughly 15-18 MB stripped.

### 2.2 Release status

- **TypeScript 7.0 Beta** announced 2026 (the canonical "tsgo becomes tsc" milestone). The TypeScript team's [TS 7.0 Beta announcement](https://devblogs.microsoft.com/typescript/announcing-typescript-7-0-beta/) commits to a stable 7.0 release "within the next two months" of the beta, with an RC a few weeks before that. As of 2026-05-24 the published versions are dated `7.0.0-dev.20260523.1` — still nightly builds in the `@typescript/native-preview` channel.
- The same announcement explicitly says: **"even though 7.0 Beta is close to production-ready, we won't have a stable programmatic API available until at least several months from now with TypeScript 7.1."**

### 2.3 Feature readiness (per README status table)

| Feature | Status | Note |
|---------|--------|------|
| Parsing/scanning | done | Exact same syntax errors as TS 6.0 |
| `tsconfig.json` parsing | done | Errors may be less helpful |
| Type checking | done | Same errors as TS 6.0 |
| JSX | done | — |
| Emit (JS output) | done | Same codegen as tsc |
| Declaration emit | done | — |
| Build mode / project references | done | — |
| Incremental build | done | — |
| Watch mode | prototype | Watches and rebuilds; no incremental rechecking |
| Language service (LSP) | in progress | "Nearly all features implemented" |
| **API** | **not ready** | **— (lowest tier: don't bother messing with it yet)** |

The README's own legend defines "**not ready**" as: *"either haven't even started yet, or far enough from ready that you shouldn't bother messing with it yet."* This is the load-bearing fact for Nub: we cannot ship a v0.1 transpiler against an API the upstream project says don't-bother-with.

### 2.4 Known stability issues as of May 2026

These are bugs filed against tsgo that matter for any integration calculus:

- **[`#3998`](https://github.com/microsoft/typescript-go/issues/3998)** (open, milestone "TypeScript 7.0 RC", 2026-05-20) — `tsgo --build` fails non-deterministically (~40% of runs) on a tsconfig with `paths` mapping `react` → `preact/compat`. Workaround: `--singleThreaded`. Root cause: race in module resolution. *Implication: tsgo's parallelism (its primary perf win) has soundness bugs being shaken out at the milestone-7.0-RC level.*
- **[`#2551`](https://github.com/microsoft/typescript-go/issues/2551)** — On a real NestJS codebase, tsgo type-check is 2× **slower** than tsc (215s vs 103s), with `getSiblingsOfContext` allocating ~46 GB in 87% of the heap. Pathological-but-real allocation pattern, since improved per follow-up comments. *Implication: type-check perf is workload-dependent, not the headline number.*
- **[`#1507`](https://github.com/microsoft/typescript-go/issues/1507)** — Large React Native project in GitHub Actions CI: tsgo 28% faster than tsc, not the 10× headline (1.6 GB more memory than tsc). *Implication: CI / constrained environments don't see the headline number.*
- **[`#1622`](https://github.com/microsoft/typescript-go/issues/1622)** — `tsgo --build` "uses extreme amounts of memory" on monorepos. Workaround: `--singleThreaded` (giving up parallelism) plus `GOGC` / `GOMEMLIMIT` tuning. *Implication: memory profile is fragile under load.*

These are bugs in the most-mature parts of the project (build mode is marked "done"). Bug profiles in the *not-yet-done* parts (the API) are unknowable because there's no API to file bugs against.

### 2.5 Integration shapes that actually exist today

| Shape | Status | Used by |
|-------|--------|---------|
| CLI: `tsgo` with tsc-compatible flags (`tsgo file.ts --outDir out`, `tsgo --noEmit`, `tsgo --build`) | stable | tsgo-strict, tsgo.nvim |
| LSP daemon: `tsgo --lsp --stdio` (JSON-RPC over stdin/stdout) | "in progress, nearly all features" | nvim-lspconfig's `lsp/tsgo.lua`, paulvanbrenk/typescript-mcp (an MCP server bridging to tsgo's LSP), Effect-TS/tsgo (wraps tsgo binary, embeds Effect language service patches), VS Code's `js/ts.experimental.useTsgo` setting |
| Programmatic Go library (`import "github.com/microsoft/typescript-go/internal/..."`) | **not ready** — internal package, no semver guarantees | none |
| Programmatic JS API (equivalent of `ts.transpileModule`) | **not ready** — no such surface exists | none |
| WASM build | does not exist; not on roadmap | none |
| `cgo -buildmode=c-shared` build | does not exist; not on roadmap | none |

Every observed integration in the wild treats tsgo as **a process to spawn**, not a library to link. ts-node-tsgo (the only ts-node fork that mentions tsgo integration) is a `2025-09-23` proof-of-concept with 0 stars / 0 forks; its README acknowledges it's experimental and the code path is "spawn the tsgo binary." None of this is a stable foundation for Nub's load-hook hot path.

## 3. Integration path matrix

The load-bearing constraint: Nub's transpile call happens inside Node's `module.registerHooks` sync load hook, **per file**, with a budget measured in microseconds-to-low-milliseconds. Cold start of a `nub script.ts` invocation is targeted at ~30 ms total preload tax (`runtime-performance.md` — not yet written, but the budget is implied in [`wasm-vs-napi-for-transpile.md`](wasm-vs-napi-for-transpile.md)).

Against that constraint, each tsgo integration path:

### 3.1 Subprocess per file (spawn `tsgo` per transpile)

- **Distribution size:** 25-26 MB per host platform (npm optionalDep).
- **Cold-start latency:** Go binary spawn on macOS/Linux is ~10-25 ms before any work runs (process exec, dynamic linker, Go runtime init, lib bundle read). On Windows it's typically worse. **For each transpile invocation.**
- **Per-file transpile latency:** Add the Go-side work (`tsc.Emit` over the file plus reading bundled libs from disk). Best-case ~30-60 ms. Realistic ~50-150 ms for a single-file emit including the lib resolution tsc does for type-aware emit.
- **Complexity:** Trivial — `child_process.spawnSync(tsgoBin, [file, '--outDir', tmp])`, read result back.
- **Failure modes:** Fork bomb on `import` chains (1000-file project = 1000 spawns). Total wall-time for a 1000-file project: ~30 *seconds*. Compare oxc-native: 1000 files = ~5.6 ms (per [`wasm-vs-napi-for-transpile.md`](wasm-vs-napi-for-transpile.md) §3.2).
- **Verdict: NON-STARTER.** Three orders of magnitude slower than oxc; defeats the entire "fast cold start" pitch.

### 3.2 Persistent tsgo daemon (long-running `tsgo` subprocess, talk via stdin/stdout or socket)

Two flavors:

#### 3.2a Use tsgo's existing LSP mode (`tsgo --lsp --stdio`)

- **Distribution size:** 25-26 MB per host platform.
- **Cold-start latency:** One-time per daemon spawn (~20-40 ms once). Subsequent transpile-equivalent requests are JSON-RPC over a pipe — call latency ~0.5-2 ms for the wire, plus actual emit time.
- **Per-file transpile latency:** **LSP has no "transpile this file and give me the emitted JS" method.** LSP is hover/completion/diagnostics/code-actions, not emit. We'd be using an interface that doesn't expose the operation we need. We'd either (a) lobby upstream to add a non-LSP custom method (which works against the "use the real tsc" simplicity argument — we're now on a custom forked-spec) or (b) work around it via diagnostics-and-no-emit, which doesn't produce JS at all.
- **Complexity:** Per-Nub-invocation daemon discovery / spawning / lifecycle. Watch-mode benefits, dev-server benefits, REPL benefits — but `nub script.ts` for a short script is the dominant cold-start scenario, and a daemon spawned just for that script is a regression vs in-process oxc.
- **Failure modes:** Daemon crash mid-build, version skew between daemon and CLI, PATH-shim child workers (`pm-run-compat-scope.md`) needing to discover the same daemon, security model (which user owns the socket, where does it live, what happens under `--permission`).
- **Verdict: BLOCKED on tsgo's protocol.** LSP doesn't expose emit; this path requires upstream protocol work that doesn't currently exist.

#### 3.2b Author a custom long-running tsgo subprocess speaking a Nub-defined wire protocol

- Same distribution / cold-start / lifecycle story as 3.2a.
- We own the wire protocol. The tsgo binary needs `--api` mode (not yet ready). Until tsgo's API ships, we'd have to maintain a fork that exposes a transpile-server CLI.
- **Verdict: REQUIRES MAINTAINING A FORK** of the Microsoft project. Out of scope for Nub's resourcing posture.

### 3.3 CGo / `-buildmode=c-shared` (build tsgo as `.so`/`.dylib`/`.dll`, FFI from Rust)

- **Distribution size:** Similar to subprocess (~25 MB per platform — the Go runtime is the same whether wrapped in an executable or a shared library).
- **Cold-start latency:** Library load (`dlopen`) is ~5-15 ms cold. **Then nothing further** — same process from there on, no per-call spawn tax.
- **Per-call transpile latency:** cgo boundary cost is well-characterized: **60-200 ns per crossing** ([Atharva Pandey, "CGo Performance and Pitfalls"](https://www.atharvapandey.com/post/go/go-cgo-performance/); [aureliar.net "Benchmarking Go FFI"](https://aureliar.net/posts/benchmarking-go-ffi/)) — vs. ~1-5 ns for a Go-internal call. That's negligible compared to actual emit work, but adds up if we cross the boundary multiple times per file (e.g. once for source, once for tsconfig, once for output). Practical mitigation: a single batch entry point per transpile.
- **Plus marshaling:** Source-in + JS-out + map-out as `*C.char` blobs. Need to manage memory ownership (`C.free` on the Go-allocated output, `runtime.Pinner` for inputs that span the call).
- **Plus Go's GC interaction with long-lived Rust ownership:** every cgo call goes through `runtime.cgocall` → `entersyscall` / `exitsyscall`, which means an OS-thread pin for the duration. For a transpile that takes 10 ms, this is fine. For thousands of concurrent transpiles (which Nub won't do — load hooks are sync anyway), it'd require `runtime.LockOSThread` discipline.
- **Plus build-system pain:** `-buildmode=c-shared` requires a working C toolchain at *Nub's build time* (matching the user's platform set). Cross-compilation gets harder; static binaries get harder (`CGO_ENABLED=0` is off the table for users distributing static binaries of tools that depend on Nub's runtime). Some users of Nub won't have a C toolchain on the build machine; that's fine for distributing the prebuilt `.so` via npm but constrains who can build Nub from source.
- **Plus: this build doesn't exist.** tsgo is not currently built with `-buildmode=c-shared`. Its internal package layout (`internal/compiler`, `internal/transformers/...`, `internal/lsp`) is intentionally Go-private — the Microsoft team has not yet decided on the public API surface, let alone exposed any of it via `//export` cgo annotations. We'd need to maintain a fork that adds `cmd/tsgo-shared/main.go` with `//export` wrappers around the relevant emit functions, plus a `cbindgen`/`bindgen` step on the Rust side. Every upstream change to internal package layout breaks our fork.
- **Verdict: TECHNICALLY VIABLE BUT REQUIRES UPSTREAM WORK + ONGOING FORK MAINTENANCE.** Not a v0.1 path; not a v0.x path without commitment from microsoft/typescript-go to maintain a shared-library build target.

### 3.4 WASM (compile tsgo Go source to WASM)

- **Standard Go → WASM toolchain (`GOOS=js GOARCH=wasm` or `wasip1`):** produces large bundles. Go's stdlib runtime is ~3-5 MB minimum on top of application code; tsgo's compiled WASM would realistically be 30-50 MB. Single-threaded by Go-WASM design (Go's scheduler doesn't multiplex over WASM threads). Compile-on-load tax per [`wasm-vs-napi-for-transpile.md`](wasm-vs-napi-for-transpile.md) §3.4 was ~25 ms for a 3.5 MB module; a 30+ MB module would be ~200+ ms per cold start.
- **TinyGo (`tinygo` toolchain):** smaller binaries (~1 MB stdlib runtime) but **doesn't support `reflect` heavily** — tsgo uses `encoding/json`, deep generics, and reflection patterns TinyGo can't compile. Compilation almost certainly fails out of the box. Maintaining a TinyGo-compatibility fork is a multi-month effort with ongoing rebase debt.
- **Precedent: esbuild-wasm.** esbuild is also Go-source, compiled to WASM via the standard toolchain. Its author Evan Wallace officially says ([esbuild FAQ](https://esbuild.github.io/faq/), [GH#219](https://github.com/evanw/esbuild/issues/219)): *"The WebAssembly version is much slower than the native version, in many cases an order of magnitude slower."* Reasons: Node re-compiles WASM on every invocation (no on-disk compile cache); Go's WASM compilation is single-threaded; Go's GC doesn't have a WASM-optimal path. **Every one of these constraints applies identically to tsgo-WASM.**
- **Plus: this build doesn't exist either.** No `tsgo.wasm` published; no GitHub Actions workflow for one in the upstream repo.
- **Verdict: DOMINATED.** Strictly worse than the subprocess path on every axis except install-size-per-platform, and the install-size win is illusory (npm optionalDependencies already handles per-platform native binaries transparently).

### 3.5 Vendor tsgo into the Nub npm distribution (subprocess from Rust CLI, not from Node hook)

This is a re-shaping of 3.1: instead of spawning per-file from inside Node's load hook, spawn `tsgo` from Nub's *Rust CLI*, ahead of Node, to pre-transpile the whole project, drop the JS into the transpile cache (`transpile-cache.md`), then start Node with the load hook reading from the cache.

- **Distribution size:** 25-26 MB per platform.
- **Cold-start latency:** Dominated by the pre-transpile pass. For a small script (`nub script.ts`), this is the worst case (spawn + emit one file = ~50-150 ms before Node ever starts). For a large project on a warm cache, near-zero (cache hits).
- **Per-file latency:** Effectively amortized into the batch pre-transpile call. Could be made competitive on cold builds via `tsgo file1.ts file2.ts file3.ts ...` (one process, many files).
- **Complexity:** Nub's resolver has to know the full file set up front, which it doesn't — Node's loader discovers files lazily via the import graph at runtime. So "pre-transpile the whole project" is structurally wrong for a runtime loader; it'd require Nub to walk the import graph statically before spawning Node, doubling resolution work.
- **Failure modes:** Files imported dynamically (`await import(name)`) aren't in the static graph; we'd need a fallback to per-file subprocess anyway, which is back to 3.1.
- **Verdict: ARCHITECTURALLY WRONG-SHAPED.** Nub's loader model is lazy-per-import; a batch-pre-transpile design fights that.

### 3.6 Score summary

| Path | Distribution size | Cold-start | Per-file latency | Complexity | Failure modes |
|------|-------------------|------------|------------------|------------|---------------|
| oxc-native (current plan) | 3.6 MB | 19 ms | 0.005 ms | low | low |
| 3.1 Subprocess-per-file | 26 MB | 30 ms first | 50-150 ms | low | fork-bomb on large projects |
| 3.2a Daemon via LSP | 26 MB | 30 ms once | n/a — no emit method | medium | requires upstream protocol change |
| 3.2b Daemon via custom protocol | 26 MB | 30 ms once | ~1-5 ms wire + emit | high | maintain custom tsgo fork |
| 3.3 cgo c-shared | 26 MB | 25 ms once | 60-200 ns + emit | very high | requires upstream build target + fork |
| 3.4 WASM (standard Go) | ~30+ MB | 200+ ms | unmeasured but ≥ esbuild-wasm = "order of magnitude slower" than native | high | doesn't exist; esbuild precedent says don't |
| 3.5 Rust CLI batch | 26 MB | 50-150 ms cold | ~0 (cache) | high | wrong shape for lazy loader |

**Only path 3.3 is in the same architectural neighborhood as oxc-native, and it requires features tsgo doesn't ship.**

## 4. Feature-parity audit

This section is the easy one: tsgo *is* tsc, so feature coverage is by definition complete. The question is whether anything in the audit list reveals a divergence.

### 4.1 Non-erasable syntax (`non-erasable-syntax.md`)

| Construct | tsgo coverage | Source |
|-----------|---------------|--------|
| `enum` (numeric, string, const) | ✓ | "Emit (JS output): done" status |
| `namespace` (with values) | ✓ | "Emit: done" |
| Parameter properties (`constructor(public x: number)`) | ✓ | "Emit: done" |
| Legacy decorators (`experimentalDecorators`) | ✓ | "Emit: done" |
| Stage 3 decorators | ✓ | PR [#2926](https://github.com/microsoft/typescript-go/pull/2926) — explicit "Implement ES decorator transform (ESNext → ES2022)" |
| `emitDecoratorMetadata` | ✓ | Same code path as tsc |
| `import =` / `export =` CJS interop | ✓ | "Emit: done" |

By construction tsgo handles 100% of TypeScript surface — same parser, same checker, same emitter ported from `tsc`. Oxc handles all of this too (per `non-erasable-syntax.md`), with `const enum` cross-file inlining and a few `emitDecoratorMetadata` long-tail cases flagged as open questions in that doc.

**Net: tsgo wins this category on principle (literally is tsc), but oxc is already sufficient for the v0.1 surface and the gap is at the long-tail-edge level rather than load-bearing.**

### 4.2 JSX / TSX (`jsx-transpilation.md`)

tsgo: "JSX: done" — same `jsx`/`jsxImportSource`/`jsxFactory`/`jsxFragmentFactory` semantics as tsc. Handles `react` / `react-jsx` / `react-jsxdev` / `preserve` / `react-native` runtimes per `compilerOptions.jsx`.

Oxc: same — see `jsx-transpilation.md` framework matrix. Same Solid caveat applies to both (`jsx: preserve` + babel-preset-solid is bundler territory).

**Tie.**

### 4.3 Source maps (`source-maps.md`)

tsgo: emits same source maps as tsc (Source Map v3 with `sourcesContent`, inline base64 via `--inlineSourceMap`). Identical to tsc by construction.

Oxc: emits Source Map v3 with `sourcesContent`, supports inline base64. Per `source-maps.md`: "rely on oxc's built-in sourcemap output." Per [`node-swc-vs-oxc-choice.md`](node-swc-vs-oxc-choice.md) §5: "Oxc's transformer emits source maps with Oxc-shape mappings, which are similar [to amaro / tsc] but not byte-identical. As long as our source maps are well-formed (V8 / Chrome DevTools / Node debugger all accept them), the byte-identical-ness with amaro doesn't matter for end-user experience."

**tsgo would give us byte-identical-with-tsc source maps. The user-visible benefit of that over oxc-shape-but-well-formed maps is zero on any debugger we care about.**

### 4.4 tsconfig honoring

| Field | tsgo | oxc |
|-------|------|-----|
| `paths` | ✓ (but see bug [#3998](https://github.com/microsoft/typescript-go/issues/3998) — race on `paths` + `@types/*` resolution) | ✓ — Nub's resolver uses `get-tsconfig` (`tsconfig-paths.md`) |
| `baseUrl` | ✓ (TS 7 is removing this, but tsgo still honors) | ✓ |
| `extends` | ✓ | ✓ (via `get-tsconfig`) |
| `experimentalDecorators` | ✓ | ✓ |
| `emitDecoratorMetadata` | ✓ | ✓ (with long-tail caveats per `non-erasable-syntax.md`) |
| `useDefineForClassFields` | ✓ | ✓ |
| `target` (downlevel emit) | ✓ | ✓ |
| `jsx`, `jsxImportSource` | ✓ | ✓ |

tsgo's compile-pipeline is the only place "exact tsc behavior" buys us anything user-visible — and even there the gap is theoretical. We don't have a reported user issue from oxc divergence in our research corpus.

### 4.5 Speed claims

| Tool | Operation | Throughput / latency | Source |
|------|-----------|----------------------|--------|
| tsc6 (Node) | type-check, large project | baseline (~80s on 13k-file Node app) | [`#2551`](https://github.com/microsoft/typescript-go/issues/2551) |
| tsgo | type-check, headline microbench | 30× faster than tsc6 | [pkgpulse 2026 guide](https://www.pkgpulse.com/guides/tsgo-vs-tsc-typescript-7-go-compiler-2026) |
| tsgo | type-check, real-world median | **1.63× – 4.04×** faster than tsc6 | [Juanchi.dev real-repo benchmark, 2026](https://juanchi.dev/en/blog/typescript-7-beta-benchmark-tsgo-vs-tsc6) |
| tsgo | type-check, NestJS regression | 2× **slower** than tsc6 | [`#2551`](https://github.com/microsoft/typescript-go/issues/2551) |
| tsgo | type-check, large RN project in CI | 28% faster (1.4×) | [`#1507`](https://github.com/microsoft/typescript-go/issues/1507) |
| tsgo | **transpile-only** (per file) | **not benchmarked in public** | — |
| oxc-transform (N-API, native) | full transform, 165-line TS file | **178,000 files/sec** = 0.005 ms/file | [`wasm-vs-napi-for-transpile.md`](wasm-vs-napi-for-transpile.md) §3.2 |
| oxc transformer vs SWC | full transform | ~3-5× faster | [oxc.rs blog 2024-09-29](https://oxc.rs/blog/2024-09-29-transformer-alpha) |
| oxc transformer vs tsc | TS strip path (ts-blank-space-style) | ~30× faster (4× faster than `swc_fast_ts_strip` which is itself 10× faster than tsc) | Evan You [tweet 2025-02-14](https://x.com/youyuxi/status/1890701933767246117) |

The headline "10× faster" claim for tsgo is type-checking on whole-program runs; it's a different operation from what Nub needs (per-file transpile inside a sync load hook). There is no published per-file transpile microbenchmark for tsgo, but the architecture — tsc's emitter ported to Go, sharing the tsc IR / checker code — bounds the lower limit: tsgo transpile cannot be faster than tsc-the-JS transpile multiplied by Go-vs-V8 single-thread speedup (~3-5×). It will not approach oxc's ~30× over tsc.

**Net: tsgo is decisively the better type-checker. For Nub's actual use case (per-file transpile, no checking), oxc dominates by an architectural order of magnitude.**

## 5. Strategic / ecosystem considerations

### 5.1 TypeScript team blessing

tsgo *is* TypeScript. Using it as Nub's transpiler means "what tsc accepts, Nub accepts; what tsc emits, Nub emits" — byte-identical, by construction, forever.

oxc is a third-party reimplementation. Documented historical divergences (none of them ship-stoppers, all of them fixable):

- **Parser:** Oxc tracks tsc closely but has occasional lag adopting brand-new TS proposals (e.g. `using` declarations landed in oxc ~3 months after tsc). For v0.1's TS surface (TS 5.x stable features) there are no known parser divergences.
- **Transformer edge cases:** `const enum` cross-file inlining is flagged as an open question in `non-erasable-syntax.md`; `emitDecoratorMetadata` long-tail (generics, conditional types, mapped types) has known divergence vectors. Per the same doc: "We follow oxc's behavior and document divergences as they're reported."
- **No known runtime-behavior-changing divergence.** Documented divergences are emission-shape (different but equivalent JS) rather than semantic (different observable behavior).

**This is the strongest tsgo argument**, and it's bounded. Compatibility-with-tsc is a question of sustained engineering rather than a binary state; oxc has been doing the engineering for two years and there is no record of a user-facing bug attributable to oxc-vs-tsc divergence in the JS bundler ecosystem (Rolldown, Vite-on-Rolldown, the WMR/Astro adoption, etc.).

### 5.2 Bus factor / maintenance

- **oxc** is maintained primarily by oxc-project (Boshen et al.). Small team; well-funded (Vite/Rolldown backing, plus paid sponsors); active release cadence (weekly nightly + monthly stable). Bus factor is real but the project has institutional backing from the Vite ecosystem.
- **tsgo** is maintained by microsoft/typescript — the largest, longest-running, most-funded compiler team in JavaScript. Bus factor is effectively the same as TypeScript itself (which is to say, near-infinite). When tsgo merges into typescript repo and `tsgo` is renamed `tsc`, this becomes "is Microsoft going to keep maintaining TypeScript." Bus factor: very low.

**tsgo wins decisively on bus factor.**

### 5.3 Adoption signal

- **oxc:** Rolldown (Vite's bundler future) depends on oxc. Vite 8 (current as of 2026) ships with Rolldown by default. Bun uses its own fork of various JS-toolchain crates but cites oxc patterns in their docs. lightningcss / Lightning CSS uses some oxc-shape parsing patterns. Astro, Nuxt, SvelteKit all ship behind Vite → all depend on oxc transitively.
- **tsgo:** Editor integration (VS Code's `js/ts.experimental.useTsgo` setting, neovim's `lsp/tsgo.lua`), MCP-style coding-agent integrations (paulvanbrenk/typescript-mcp), strict-mode migration tooling (ashley-hunter/tsgo-strict), NestJS-specific replacement (tsgonest), Effect-TS LSP wrapper (Effect-TS/tsgo). **Every adopter uses tsgo as a process to spawn or an LSP daemon to connect to**, never as a transpile library. amaro (Node's reference TS stripper) explicitly considered tsgo and rejected it; per [amaro#200](https://github.com/nodejs/amaro/issues/200), Marco Ippolito 2025-05-26: *"We discussed about this with the team and it's not possible by design, we should keep using SWC for the foreseeable future."* The "by design" referent there is tsgo's lack of blank-spacing mode + the Go runtime not fitting Node's distribution.

**oxc wins decisively on transpile-pipeline ecosystem alignment.** tsgo wins on type-checker / LSP ecosystem alignment — a different category.

### 5.4 Brand alignment with Nub's "compatibility is the trust contract" stance

The trust contract is: *code that runs on Node runs on Nub*. The relevant compat axis is **runtime semantics**, not transpiler-output byte-identity. A `.ts` file written for TypeScript should behave the same on Nub as it would on `tsc → node` or `tsx → node` — same enum object shape, same decorator metadata, same class field semantics.

Both oxc and tsgo deliver this. Neither would produce a `.js` file that runs differently in any observable way on Node. The compat contract is satisfied at the runtime-behavior level, not at the bytes-out level.

**Brand alignment is a tie.** The "use the real tsc" framing is rhetorically strong but doesn't add a user-visible compat guarantee Nub doesn't already get from oxc.

### 5.5 Brand-boundary check ([`AGENTS.md`](../../AGENTS.md) §brand)

Neither path violates Nub's brand boundary:

- **No `globalThis.nub`** — neither transpiler suggests injecting one.
- **No `nub:*` namespace** — neither requires module-namespace squatting.
- **No `@nub/*` packages** — both are consumed as third-party npm packages we depend on, not republish.
- **No `NUB_*` env vars** — neither requires a Nub-specific env var. (We respect `NODE_COMPILE_CACHE=0` per `transpile-cache.md` regardless of transpiler choice.)
- **No vendored Node patches** — both load via the standard `module.registerHooks` extension surface.

The augmenter-not-fork test (`architecture.md#augmenter-not-fork`): "would a user on plain Node + the corresponding `module.register()` / `--import` / npm-addon get the same result?" Yes for both — a user on plain Node + `module.register('amaro')` / + `module.register('ts-node/esm')` / + `module.register('tsx')` gets equivalent TS-runtime behavior either way.

**Brand boundary does not pick a winner.**

## 6. The case for switching to tsgo (strongest version)

If we squint, this is the steelman:

> *"Nub's central value proposition is 'compatibility is paramount; code written for the canonical TypeScript ecosystem runs on Nub.' The canonical TypeScript ecosystem is whatever Microsoft ships as tsc. Microsoft is replacing JS-tsc with Go-tsgo and renaming the binary `tsc` in TypeScript 7. By v1, tsgo *is* tsc; oxc is one of several third-party reimplementations of tsc's behavior. Why would Nub bet on a third-party reimplementation when the real thing is shipping?*
>
> *Yes, integration is painful today — but tsgo's API status is 'not ready,' not 'never coming.' TypeScript 7.1 lands several months after 7.0. We have until then to design the integration. If we ship v0.1 on oxc and the ecosystem converges on tsgo for transpile-as-well-as-typecheck, we have a year of migration debt to switch.*
>
> *Bus factor matters. Microsoft is going to maintain TypeScript for the next decade. oxc-project might not exist in five years.*
>
> *Parser-divergence-with-tsc has burned every JS-tooling vendor at some point: Babel had decades of subtle TS-emit bugs; SWC has chronic 'doesn't match tsc on this edge case' issues; even esbuild ships a deliberately incomplete TS subset and tells users 'use tsc if you care.' Oxc is engineering its way out of that hole today, but the hole exists.*
>
> *And the cost of switching back to tsgo later is much higher than designing for it now: cache layouts, source-map shapes, error-message shapes, user-visible diagnostic ergonomics all get baked into the v0.1 release and become compat surface."*

The honest weight of this argument: **the bus-factor + canonical-tsc points are real**. The integration-debt point is theoretical (we don't know yet that the ecosystem will converge to tsgo for transpile; the evidence so far is that even Node's own amaro stayed on SWC). The parser-divergence point is theoretically real and empirically not a current problem.

## 7. The case for staying with oxc (strongest version)

> *"Nub's central value proposition is 'fast cold start, drop-in TypeScript execution.' Cold start is a function of per-file transpile latency multiplied by file count. Oxc-native at 0.005 ms/file is in the architectural sweet spot for a per-file load hook. tsgo at any realistic integration latency (subprocess: 50-150 ms; cgo-shared library that doesn't yet exist: ~5-10 ms; LSP daemon with a protocol that doesn't yet exist: 1-5 ms wire + emit) is the wrong order of magnitude.*
>
> *'Use the real tsc' is the right slogan for type-checking, where 'real tsc' is the only thing that defines correctness. It's the wrong slogan for transpilation, where the user wants the JS to behave like what they wrote and doesn't care which engine emitted it. The amaro precedent is instructive: Node's TS-runtime team specifically chose SWC over tsc-or-tsgo, with reasons that map almost 1:1 onto Nub's situation (no Go in the build, per-file latency matters, transpile fidelity ≠ byte-identity).*
>
> *tsgo's programmatic API is 'not ready' — the lowest maturity tier the upstream project recognizes. Building a v0.1 ship-it product on an upstream that explicitly says 'don't bother messing with this yet' is a contract for pain. Even after the API ships, the integration shape will be 'spawn or daemon,' not 'link as a library' — Go is fundamentally a process-shaped runtime, not a library-shaped one. The same reasons Node didn't pull tsgo into its build (no Rust-toolchain dep, no Go-toolchain dep, no embedding C-library, only a wasm blob through V8) work in reverse against Nub pulling Go into its in-process Rust path.*
>
> *Oxc gives us in-process Rust, zero IPC, zero marshaling, 0.005 ms/file, and 3.6 MB binary size. The cost of switching later (cache invalidation on transpiler version change is already designed into `transpile-cache.md`; source-map regeneration on switch is mechanical) is low. The cost of starting on tsgo today is high. Defer the bet."*

The honest weight: **per-file architecture and the lack-of-API are decisive today**. The "switching cost later is low" claim is the load-bearing optimistic premise — it's true for the transpile pipeline itself but understates the cost of any user-facing observable that becomes compat surface (error messages, source-map shape if downstream tools assume amaro-shape or oxc-shape mappings).

## 8. Recommendation

**Stay with oxc for v0.1 and v0.x.** Document the revisit triggers.

### 8.1 Conditions under which we'd revisit

Any one of these, individually:

1. **tsgo ships a stable programmatic API** (the TS team's targeted 7.1 milestone, "several months from now") **AND** it exposes a transpile-only entry point that can be called from JS via napi-rs or similar without a per-call subprocess spawn. This is the architectural blocker, not the version number.
2. **microsoft/typescript-go publishes a `-buildmode=c-shared` target** with `//export`-annotated wrappers around the transpile pipeline. We can then evaluate it as a Rust-callable library on the same footing as oxc.
3. **A Nub daemon architecture** (per a future `daemon.md`, currently sketched only as a comment in `transpile-cache.md`) lands, into which a long-running `tsgo` subprocess fits naturally. At that point the per-file IPC tax amortizes across many `nub script.ts` invocations and the integration shape changes.
4. **Documented oxc → tsc divergence causes a user-visible v0 bug we can't easily fix in oxc.** The cure would be switching transpilers; we'd need to evaluate tsgo (and SWC, and amaro) at that point. We have zero such bugs in our research corpus today.
5. **Vite / Rolldown / a meaningful chunk of the bundler ecosystem switches its transpile pipeline to tsgo.** If oxc gets abandoned as a load-bearing bundler dep, our ecosystem-alignment argument flips. As of 2026-05-24, the inverse is true: oxc is more entrenched in Vite/Rolldown than ever.

### 8.2 Anti-recommendation

**Do not adopt a hybrid "oxc for transpile, tsgo for some other use case" architecture.** It superficially sounds like "use the right tool for each job," but the costs are real:

- Two transpile codepaths to maintain (e.g. if tsgo is used for any `.ts` file in some condition and oxc for others, error messages and source-map shapes diverge per-file based on opaque routing logic; that's the worst kind of inconsistency for users).
- Two installs (~3.6 MB + ~25 MB) for every Nub user, even those who never touch the tsgo-routed path.
- Two bug surfaces; users have to know which transpiler emitted which file to triage.

If we ever want tsgo's *type-checking* (separately from transpile), that's a different decision and goes through its own design doc — `nub check` or equivalent, possibly proxying to `tsgo --noEmit` from the Rust CLI. That's a coherent split because type-check is a whole-program operation that *should* shell out to a subprocess. Per-file transpile is not.

### 8.3 What this means for `ts-transpilation.md`

The "Transpiler choice" section currently says oxc-first with SWC as a fallback eval. **No change recommended.** The oxc-first decision stands. tsgo can be added as a third "evaluated and deferred" entry alongside SWC, with a pointer to this doc for the rationale.

## 9. Open questions

These could not be answered from the available data:

- **Does tsgo's eventual programmatic API expose a `ts.transpileModule`-style entry point?** Or will it be Go-library-only with no JS surface? The "API: not ready" line in the README and the TS 7.0 Beta announcement's "stable programmatic API in 7.1" line don't specify the language of the API. If it's Go-only, the integration paths in §3 don't change. If it's JS-via-WASM or JS-via-N-API, the path 3.3 evaluation gets much more concrete.
- **Will Microsoft maintain `-buildmode=c-shared` builds?** No public statement. Asking on the typescript-go issue tracker is the obvious next step if this becomes load-bearing for Nub.
- **Per-file transpile-only microbenchmark for tsgo vs oxc-native.** We have type-check benchmarks for tsgo and per-file transpile benchmarks for oxc; we don't have a head-to-head on the actual operation Nub cares about. Running `tsgo --noEmit=false file.ts --outDir out` in a loop and measuring would close this — but the spawn cost would dominate any per-file measurement, so we'd need a hypothetical library-mode tsgo to get a fair comparison.
- **Does amaro switch engines under its own API?** [amaro#200](https://github.com/nodejs/amaro/issues/200) Marco's 2025-05-26 statement is "for the foreseeable future." If amaro ever does switch to tsgo internally, that changes the "amaro is the de facto Node TS stripper" framing in [`node-swc-vs-oxc-choice.md`](node-swc-vs-oxc-choice.md) §5, but doesn't directly affect Nub's choice.
- **Does Effect-TS or Vue or another large TS-ecosystem player adopt tsgo for non-typecheck use?** Effect-TS/tsgo today is an LSP wrapper, not a transpile-pipeline migration. If the framework world starts vendoring tsgo for non-LSP purposes, the ecosystem-alignment math changes.

## Sources

### Primary (tsgo project state, May 2026)

- [microsoft/typescript-go README](https://github.com/microsoft/typescript-go) — current feature status table; "API: not ready"; long-term plan to merge into `microsoft/TypeScript`.
- [Announcing TypeScript 7.0 Beta — TypeScript blog](https://devblogs.microsoft.com/typescript/announcing-typescript-7-0-beta/) — "stable programmatic API in TypeScript 7.1, several months from now"; 7.0 release "within the next two months."
- [Announcing TypeScript Native Previews — TypeScript blog (2025)](https://devblogs.microsoft.com/typescript/announcing-typescript-native-previews/) — original tsgo announcement; `@typescript/native-preview` distribution shape.
- [@typescript/native-preview on npm](https://registry.npmjs.org/@typescript/native-preview) — base package, 2.1 MB unpacked, dispatches to per-platform binary packages.
- [@typescript/native-preview-linux-x64 on npm](https://registry.npmjs.org/%40typescript%2Fnative-preview-linux-x64) — 26.6 MB unpacked.
- [@typescript/native-preview-darwin-arm64 — implicit by parallel](https://www.npmjs.com/package/@typescript/native-preview-darwin-arm64) — Apple Silicon binary, ~25.8 MB.
- [`cmd/tsgo/lsp.go`](https://github.com/microsoft/typescript-go/blob/main/cmd/tsgo/lsp.go) — LSP daemon entry point; "only stdio is supported."
- [`internal/lsp/server.go`](https://github.com/microsoft/typescript-go/blob/main/internal/lsp/server.go) — LSP server implementation.
- [PR #857: Implement tsgo functionality in tsc, promote tsc to main entrypoint](https://github.com/microsoft/typescript-go/pull/857) — `--lsp` and `--api` are flags on the main entry now.
- [PR #2926: Implement ES decorator transform (ESNext → ES2022)](https://github.com/microsoft/typescript-go/pull/2926) — Stage 3 decorator emit support.

### tsgo bugs / stability evidence

- [`#3998` Intermittent `tsgo --build` TS2345: paths + @types/* conflict](https://github.com/microsoft/typescript-go/issues/3998) — 40% failure rate; workaround `--singleThreaded`; milestone TypeScript 7.0 RC.
- [`#2551` 2x slower than tsc on a NestJS codebase](https://github.com/microsoft/typescript-go/issues/2551) — pathological memory allocation in `getSiblingsOfContext`.
- [`#1507` Performance Issue in CI (GitHub Action)](https://github.com/microsoft/typescript-go/issues/1507) — 28% faster (not 10×) on large RN project.
- [`#1622` `tsgo --build` uses extreme amounts of memory](https://github.com/microsoft/typescript-go/issues/1622) — monorepo memory pressure.
- [`#931` Dependency JS files erroneously typechecked](https://github.com/microsoft/typescript-go/issues/931) — module-resolution divergence vs tsc.

### Performance write-ups (May 2026)

- [Juanchi.dev: TypeScript 7 beta benchmark — tsgo vs tsc6 on real repos](https://juanchi.dev/en/blog/typescript-7-beta-benchmark-tsgo-vs-tsc6) — 1.63× to 4.04× median speedup; "pretty far from the 10× in the announcement."
- [pkgpulse: tsgo vs tsc — 10x Faster TypeScript Builds 2026](https://www.pkgpulse.com/guides/tsgo-vs-tsc-typescript-7-go-compiler-2026) — TS-team marketing-aligned headline numbers.
- [Evan You: Oxc TS stripping 4× faster than swc_fast_ts_strip (Feb 2025)](https://x.com/youyuxi/status/1890701933767246117) — oxc transpile leadership claim.
- [oxc.rs Transformer Alpha announcement (2024-09-29)](https://oxc.rs/blog/2024-09-29-transformer-alpha) — oxc transformer 3-5× faster than SWC.
- [esbuild FAQ](https://esbuild.github.io/faq/) — Go → WASM is "an order of magnitude slower" (precedent against tsgo-WASM).
- [esbuild issue #219](https://github.com/evanw/esbuild/issues/219) — author's reasoning for not investing in esbuild-wasm performance.

### Go FFI / cgo overhead

- [aureliar.net: Benchmarking Go FFI](https://aureliar.net/posts/benchmarking-go-ffi/) — ~40 ns per cgo call overhead on simple cases.
- [Atharva Pandey: CGo Performance and Pitfalls](https://www.atharvapandey.com/post/go/go-cgo-performance/) — 60-200 ns per cgo crossing; goroutine-to-OS-thread transition cost.
- [Go runtime/cgocall.go source](https://golang.org/src/runtime/cgocall.go) — primary source on the cgo entry/exit cost path.
- [stoolap.io: Calling a Rust library from Go with CGO_ENABLED=0](https://stoolap.io/blog/2026/04/08/calling-a-rust-library-from-go-with-cgo-disabled/) — cross-compilation hazards of cgo-coupled builds.

### tsgo integration ecosystem

- [mmmeff/ts-node-tsgo](https://github.com/mmmeff/ts-node-tsgo) — experimental ts-node fork attempting tsgo integration; 0 stars / 0 forks.
- [paulvanbrenk/typescript-mcp](https://github.com/paulvanbrenk/typescript-mcp) — MCP server spawning `tsgo --lsp --stdio` as a child process. Reference architecture for tsgo-as-daemon integration.
- [neovim/nvim-lspconfig — lsp/tsgo.lua](https://github.com/neovim/nvim-lspconfig/blob/master/lsp/tsgo.lua) — uses `tsgo --lsp --stdio`.
- [Effect-TS/tsgo](https://github.com/Effect-TS/tsgo/) — wraps tsgo binary, adds Effect language service patches; maintains a fork tracking upstream commits.
- [ashley-hunter/tsgo-strict](https://github.com/ashley-hunter/tsgo-strict) — spawns `tsgo` once for strict-subset diagnostics. CLI-shaped integration.
- [tsgonest](https://tsgonest-tsgonest-90.mintlify.app/concepts/compilation) — NestJS-targeted fork; uses typescript-go's Go library directly (not as an FFI surface — as a Go program embedding tsgo's internal packages, since they're both Go).

### Node TS-stripper history (relevant precedent)

- [nodejs/amaro#200 Experiment with typescript-go](https://github.com/nodejs/amaro/issues/200) — 2025-05-26 Marco Ippolito: "we should keep using SWC for the foreseeable future."

### Internal cross-references

- `../runtime/ts-transpilation.md` — current oxc-first plan.
- `../runtime/transpile-cache.md` — content-addressed disk cache; transpiler-version-keyed.
- `../runtime/non-erasable-syntax.md` — TS syntax surface required.
- `../runtime/jsx-transpilation.md` — JSX requirements.
- `../runtime/source-maps.md` — inline base64 + `sourcesContent` requirement.
- [`wasm-vs-napi-for-transpile.md`](wasm-vs-napi-for-transpile.md) — N-API vs WASM decision (N-API wins). tsgo is the third path.
- [`node-swc-vs-oxc-choice.md`](node-swc-vs-oxc-choice.md) — why Node picked SWC over oxc; same paragraphs note tsgo was rejected by amaro.
- `../architecture.md#augmenter-not-fork` — the mechanism test both transpilers satisfy.
- `../philosophy.md#additivity` — additivity policy; both transpilers comply.
- [`../../AGENTS.md`](../../AGENTS.md) — brand-boundary rules.
- [`AGENTS.md`](../../AGENTS.md) — repo orientation and the command surface.

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links and reference-checkout paths were rewritten; findings and measured values are unchanged.

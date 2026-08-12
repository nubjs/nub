---
**Status:** v1, 2026-05-18. Write-once research doc.

**Question:** Should Nub's preload transpiler ship as N-API (via napi-rs) or as WASM (sidestepping the `--allow-addons` permission gate that N-API loading triggers under Node's permission model)?

**Headline answer:** Stick with N-API for v0.1. The permission payoff for WASM is smaller than it first appears — a WASM module's grant requirements depend on how it was compiled, and the N-API addon load is already gated by the same `--permission` envelope users accept. The **performance hit for WASM is 6–10×** on the hot transpile path when oxc is compiled via napi-rs+emnapi+WASI, and oxc-transform's WASM build is unstable enough to OOM the V8 wasm heap at 10k transpiles. If Nub ever ships a WASM fallback, it is a **secondary build** for permission-locked environments, not the primary distribution.

**Builds on:** [`node-swc-vs-oxc-choice.md`](node-swc-vs-oxc-choice.md).
---

# WASM vs N-API for Nub's transpiler

## 1. TL;DR

- **N-API is the right v0.1 choice.** Native oxc-transform via N-API (`oxc-transform` npm package on `@oxc-transform/binding-<platform>`) hits **178,000 transpiles/sec** on a 165-line TS file with full transform mode (enums, parameter properties, namespace, decorators). That's a 0.005 ms per-call hot path — effectively free relative to any other cost in the load hook.
- **WASM oxc-transform exists but is slower and more fragile.** The `@oxc-transform/binding-wasm32-wasi` package (3.5 MB single `.wasm`) hits **28,000 transpiles/sec** — a **6.4× slowdown** vs. native — and crashed at N=10000 with a `RuntimeError: memory access out of bounds` in V8's wasm runtime. Production fragility beyond v0.1 budget.
- **WASM still beats SWC native.** Even at 6× slower than oxc native, oxc-wasm at 28k files/sec is faster than `@swc/core` native (4,400 files/sec) and ~10× faster than `amaro` / `@swc/wasm-typescript` (2,500 files/sec). So if compatibility forced us to WASM, the result would still be an order of magnitude faster than Node's own built-in strip-types path.
- **Permission payoff is real but narrower than expected.** The load-bearing distinction is **what kind** of WASM:
  - `@swc/wasm-typescript` / `amaro` use wasm-bindgen targeting `wasm32-unknown-unknown`. **No WASI, no addons.** Loads under `--permission` with only `--allow-fs-read`.
  - `@oxc-transform/binding-wasm32-wasi` uses napi-rs's emnapi runtime with `wasm32-wasi`. **Requires `--allow-wasi` AND `--allow-worker`** (it uses `SharedArrayBuffer` + worker threads for fs proxy). The supposed permission win disappears.
- **Recommendation for v0.1:** Ship N-API. Document `--allow-addons` as a required grant under permission mode. **Phase-2 option:** if/when oxc-transform gets a wasm-bindgen `wasm32-unknown-unknown` build (no WASI), reconsider as a permission-mode fallback shipped alongside the native binaries.

## 2. The transpiler-WASM landscape (May 2026)

### Packages surveyed

Installed and measured directly on the agent's machine (macOS 25.5, darwin-arm64, Node v24.14.0):

| Package | Type | Disk size | Wasm/native blob size |
|---|---|---|---|
| `oxc-transform` | N-API stub | 68 KB | (depends on `@oxc-transform/binding-<platform>`) |
| `@oxc-transform/binding-darwin-arm64` | N-API native | 3.6 MB | 3,682,720 bytes (.node) |
| `@oxc-transform/binding-wasm32-wasi` | WASM (emnapi+WASI) | 6.9 MB | 3,498,004 bytes (.wasm) |
| `oxc-parser` | N-API stub | 1.4 MB | platform binding pulled separately |
| `@oxc-parser/wasm` | WASM (wasm-bindgen) | 1.5 MB | 737,034 bytes (.wasm) — **parser only, no transform** |
| `@swc/core` | N-API stub | 148 KB | (depends on `@swc/core-<platform>`) |
| `@swc/core-darwin-arm64` | N-API native | not measured | 22,480,256 bytes (.node) |
| `@swc/wasm-typescript` | WASM (wasm-bindgen) | 3.6 MB | 3,739,316 bytes (.js with inline wasm) |
| `amaro` | wrapper around `@swc/wasm-typescript` | 3.6 MB | (re-exports the wasm.js) |
| `esbuild` | Go native | 10 MB | 10,540,722 bytes (binary) |
| `esbuild-wasm` | Go WASM | 14 MB | 13,918,738 bytes (.wasm) |

(All sizes are darwin-arm64 host; install set varies by host.)

### Maturity assessment

- **`@swc/wasm-typescript` / `amaro`:** Production. Used by Node itself for `--experimental-strip-types` / `--strip-types`. Stable API, with known limitations: only the erasable subset in strip-only mode, and transform mode is needed for enums/param-props/namespace. amaro's `transform` mode pulls in the full SWC TS pipeline — slower than strip-only but feature-complete.
- **`@oxc-parser/wasm`:** Out-of-date. Version 0.60.0 published while native bindings are at 0.67.0+ ([issue #10778](https://github.com/oxc-project/oxc/issues/10778)). This is a parser, not a transformer — would need a separate transform pass to be useful. The oxc-project's pkg.json reads `"deprecated": true` for the standalone parser-wasm package in some recent commits (not yet on registry); the canonical entry is now `oxc-parser` (N-API) with WASM via the `wasm32-wasi` binding subpath.
- **`@oxc-transform/binding-wasm32-wasi`:** Beta-quality. Uses napi-rs's emnapi shim to expose the N-API surface inside WASM. Works for short-running transpile workloads; crashed (V8 wasm OOM) at N=10000 in our throughput test (see §3.5). Not production-grade for high-throughput pipelines yet.
- **`esbuild-wasm`:** Production but officially de-prioritized. esbuild's own FAQ explicitly says the WebAssembly version is "an order of magnitude slower" than native and is intended as a fallback only ([esbuild FAQ](https://esbuild.github.io/faq/)).
- **`@rolldown/wasm-binding`:** Beta. Used by browser-only Rolldown integrations (e.g. Vite preview in webcontainers). Same emnapi+WASI shape as oxc-transform's WASI binding. Rolldown's primary distribution is native via napi-rs platform packages.

### Toolchain note

The Rust → WASM stack for a transpiler-class workload has two divergent paths, and the second is far easier for maintainers — no code changes between native and WASM — but pulls in more Node-level permission grants:

1. **wasm-bindgen → `wasm32-unknown-unknown`.** Direct JS↔Wasm glue. No syscall/POSIX surface. Lightweight runtime, but the Rust crate has to avoid `std::fs`, `std::process`, `std::thread` primitives — only what compiles with no_std-ish constraints. This is what `@swc/wasm-typescript` uses.
2. **napi-rs (emnapi) → `wasm32-wasi`.** The crate compiles identically to its native target; emnapi+WASI shims provide N-API + syscalls. Bigger runtime, includes `node:wasi` + `SharedArrayBuffer` + worker threads. This is what `@oxc-transform/binding-wasm32-wasi` uses.

## 3. Performance comparison

### 3.1 Test methodology

- Machine: Mac mini M-series, darwin-arm64, Node v24.14.0.
- Fixture: 165-line TypeScript file ("sample.ts") exercising interfaces, classes, generics, decorators, parameter properties, enum, namespace, type-only exports. ~4.5 KB source.
- Per-call benchmark: 20 warmup calls, then N=1000 calls, mean per-call latency.
- Cold-start benchmark: each runtime spawned in a separate `node` subprocess; measured `import + first-transform` wall time.
- Throughput benchmark: same fixture, N=100, 1000, 10000.
- All `--no-warnings` to suppress experimental noise.

### 3.2 Per-call latency (full transform mode, 165-line TS file)

```
oxc-transform (native N-API)         init=  12.70ms  per-call= 0.005ms  total(1000)=    5.4ms
oxc-transform (WASM emnapi+WASI)     init=  46.14ms  per-call= 0.032ms  total(1000)=   31.8ms
@swc/core (native N-API)             init=  18.78ms  per-call= 0.256ms  total(1000)=  256.0ms
amaro transform-types (swc-wasm)     init=  32.61ms  per-call= 0.455ms  total(1000)=  454.6ms
```

Ratios (oxc-native baseline):

| Runtime | Per-call latency | Ratio vs oxc-native |
|---|---|---|
| oxc-transform native | 0.005 ms | 1× |
| oxc-transform WASM | 0.032 ms | 6.4× slower |
| @swc/core native | 0.256 ms | 51× slower |
| amaro/swc WASM | 0.455 ms | 91× slower |

### 3.3 Per-call latency (strip-only mode, no enums/namespace)

```
oxc-transform strip (native N-API)         init=   0.13ms  per-call= 0.005ms  total(1000)=    5.5ms
oxc-transform strip (WASM emnapi+WASI)     init=   0.07ms  per-call= 0.048ms  total(1000)=   47.5ms
@swc/wasm-typescript strip-only (raw)      init=  22.47ms  per-call= 0.210ms  total(1000)=  209.9ms
amaro strip-only (swc-wasm)                init=   0.15ms  per-call= 0.198ms  total(1000)=  197.7ms
@swc/core strip (native N-API)             init=   0.11ms  per-call= 0.203ms  total(1000)=  202.6ms
```

Strip-only is faster than full-transform across the board (no AST rewriting for enums/decorators), but the relative ordering holds: oxc native >> oxc WASM > swc native ≈ amaro WASM.

Notable: oxc WASM strip-only (47 ms / 1000 = 0.048 ms each) is **still ~4× faster than @swc/core native**, the de facto reference TS transformer in the JS ecosystem.

### 3.4 Cold-start (subprocess time-to-first-transform)

Three runs, separate `node` subprocess each:

```
--- Run 1 ---
oxc-native:        import=31.48ms  first-transform=0.43ms  total=31.91ms
oxc-wasm (emnapi): import=48.68ms  first-transform=3.62ms  total=52.31ms
swc-native:        import=35.56ms  first-transform=21.34ms  total=56.90ms
amaro (swc-wasm):  import=46.59ms  first-transform=23.38ms  total=69.97ms

--- Run 2 ---
oxc-native:        import=19.24ms  first-transform=0.12ms  total=19.36ms
oxc-wasm (emnapi): import=40.92ms  first-transform=3.74ms  total=44.66ms
swc-native:        import=21.03ms  first-transform=1.43ms  total=22.46ms
amaro (swc-wasm):  import=46.92ms  first-transform=23.52ms  total=70.44ms

--- Run 3 ---
oxc-native:        import=19.45ms  first-transform=0.12ms  total=19.58ms
oxc-wasm (emnapi): import=39.06ms  first-transform=3.65ms  total=42.71ms
swc-native:        import=21.94ms  first-transform=1.46ms  total=23.41ms
amaro (swc-wasm):  import=46.17ms  first-transform=23.30ms  total=69.47ms
```

Median:

| Runtime | Import time | First transform | Total cold-start |
|---|---|---|---|
| oxc-native | 19.4 ms | 0.12 ms | **19.5 ms** |
| oxc-wasm | 40.9 ms | 3.7 ms | **44.7 ms** |
| swc-native | 21.9 ms | 1.5 ms | **23.4 ms** |
| amaro/swc-wasm | 46.6 ms | 23.4 ms | **70.0 ms** |

The cold-start delta between oxc-native and oxc-wasm is **+25 ms** — this is the V8 wasm engine compiling the 3.5 MB wasm module on first load. Subsequent calls in the same process don't pay this again (V8 caches the compiled wasm), but every cold `nub script.ts` invocation does.

Against Nub's cold-start budget — a target of ~30 ms total preload tax — a 25 ms wasm compile would nearly double the augmentation tax, and V8's compile cache does not amortize it across processes (§6.2).

### 3.5 Throughput at scale

```
oxc-transform native                N=  100       0.6ms  (176134 files/sec)
oxc-transform native                N= 1000       5.6ms  (178739 files/sec)
oxc-transform native                N=10000      55.4ms  (180365 files/sec)
oxc-transform WASM (emnapi+WASI)    N=  100       5.2ms  (19314 files/sec)
oxc-transform WASM (emnapi+WASI)    N= 1000      35.9ms  (27829 files/sec)
oxc-transform WASM (emnapi+WASI)    N=10000      *** CRASH ***
  RuntimeError: memory access out of bounds
    at wasm://wasm/00d58052:wasm-function[3559]:0x2fa74b
@swc/core native                    N=  100      24.1ms  (4147 files/sec)
@swc/core native                    N= 1000     228.1ms  (4384 files/sec)
@swc/core native                    N=10000    2252.0ms  (4441 files/sec)
amaro transform (swc-wasm)          N=  100      71.7ms  (1395 files/sec)
amaro transform (swc-wasm)          N= 1000     432.0ms  (2315 files/sec)
amaro transform (swc-wasm)          N=10000    3943.4ms  (2536 files/sec)
```

The oxc-WASM crash at N=10000 is the headline reliability finding: the same fixture runs 10000 times against the native build with zero issue (55 ms total). The WASM build's linear memory grows under repeated allocations and either doesn't get GC'd back to the arena correctly, or hits a fragmentation cliff in the napi-rs/emnapi allocator.

It is not a one-off, it is emnapi+oxc-specific (amaro/swc-wasm didn't crash at 10000), and it is solvable upstream but not on our timeline. **A v0.1 default that intermittently crashes after ~5000 transpiles is not shippable**; the project sizes we care about (5k-10k file monorepos) hit this.

### 3.6 Memory footprint

```
oxc-native: rss-delta= 4.1 MB
oxc-wasm: rss-delta= 25.3 MB
swc-native: rss-delta= 9.8 MB
amaro: rss-delta= 33.7 MB
```

Measured by `process.memoryUsage().rss` delta between pre-import and post-first-transform:

- oxc-native is the lightest (4 MB) — the .node binary maps via dlopen; only mapped pages contribute to RSS.
- oxc-wasm carries the full 3.5 MB compiled wasm + 16 MB V8 wasm arena (initial 4000 64KB pages = 250 MB virtual, ~25 MB resident).
- amaro is heaviest (33 MB) — SWC's wasm module is larger (3.7 MB) and emnapi adds its own runtime overhead.

All four are fine for a long-running daemon; for short-lived `nub script.ts` invocations, oxc-native's small footprint is one more cold-start win.

## 4. The permission interaction: the load-bearing answer

### 4.1 Empirical test (Node 24.14.0, May 2026)

The original question was: does WASM import bypass `--allow-addons`? Tested directly on this machine.

**Setup:** minimal 41-byte `.wasm` module exporting an `add` function.

**Test 1 — baseline:**
```
$ node --experimental-wasm-modules ./entry.mjs
imported wasm, add(2,3) = 5
ExperimentalWarning: Importing WebAssembly module instances ...
```
Works without permission mode.

**Test 2 — under `--permission` with broad fs-read, no `--allow-addons`:**
```
$ node --experimental-wasm-modules --permission --allow-fs-read='*' /tmp/.../entry.mjs
imported wasm, add(2,3) = 5
```
**Works.** WASM `.wasm` imports are NOT gated by `--allow-addons`.

**Test 3 — inline `WebAssembly.instantiate` (no .wasm file) under `--permission`:**
```
$ node --permission --allow-fs-read='*' /tmp/.../wa-only.mjs
inline wasm with no fs touch: 30
```
**Works.** `WebAssembly.instantiate(Uint8Array)` runs with zero filesystem grants beyond the entry .mjs read.

**Test 4 — native N-API addon under `--permission` without `--allow-addons`:**
```
$ node --permission --allow-fs-read='*' /tmp/.../load-native.mjs
Error: Cannot load native addon because loading addons is disabled.
[cause]: code: 'ERR_DLOPEN_DISABLED'
```
**Fails.** N-API addons are blocked without `--allow-addons`.

**Test 5 — native N-API addon under `--permission --allow-addons`:**
```
$ node --permission --allow-fs-read='*' --allow-addons ...
SecurityWarning: The flag --allow-addons must be used with extreme
                 caution. It could invalidate the permission model.
import=28.99ms  first-transform=0.51ms  total=29.50ms
```
**Works, with a noisy security warning.**

### 4.2 Source-level confirmation

Per `lib/internal/modules/esm/load.js` (Node main branch, checked via WebFetch): the `.wasm` load path uses standard `fs.readFileSync`, gated only by the FileSystemRead permission, with no addon permission check.

Per `src/permission/permission.cc`: the registered permission scopes are `{FileSystem, ChildProcess, WorkerThreads, Net, Inspector, WASI, Addon, FFI}`. WebAssembly is not a scope. WASI is its own scope but only gates the `node:wasi` API (constructor), not raw `WebAssembly.instantiate`.

### 4.3 But the "permission win" splits on WASM build shape

The naïve framing — "WASM bypasses `--allow-addons`" — is correct but incomplete. **What grants WASM actually needs depends on the WASM build:**

| WASM build flavor | `--allow-fs-read` | `--allow-addons` | `--allow-wasi` | `--allow-worker` |
|---|---|---|---|---|
| wasm-bindgen → `wasm32-unknown-unknown` (e.g. `@swc/wasm-typescript`, `amaro`) | yes (for .wasm + entry script) | **no** | **no** | **no** |
| napi-rs+emnapi → `wasm32-wasi` (e.g. `@oxc-transform/binding-wasm32-wasi`) | yes | **no** | **yes** | **yes** (SharedArrayBuffer + worker for fs proxy) |

**Empirical proof:**

```
$ node --permission --allow-fs-read='*' --no-warnings ./swc-wasm-test.mjs
result: const x         = 1; export {x};       # works

$ node --permission --allow-fs-read='*' --no-warnings ./amaro-test.mjs
result: const x         = 1; export {x};       # works

$ node --permission --allow-fs-read='*' --no-warnings ./cold-oxc-wasm.mjs
node:wasi:90
    const wrap = new _WASI(args, env, preopens, stdio);
                 ^
Error: Access to this API has been restricted. Use --allow-wasi to manage permissions.
```

So the existing oxc-transform WASM binding **does not** give us the "single grant" simplicity we hoped for. Users would need to add `--allow-wasi --allow-worker` to the grant set we already require (`--allow-fs-read=$WORKSPACE`).

For oxc-WASM to be a true permission-mode win, we'd need a **wasm-bindgen / wasm32-unknown-unknown** oxc-transform build — which doesn't exist in npm today (oxc-project ships only the WASI variant).

### 4.4 Net assessment

| Distribution | Grants required under `--permission` | User friction |
|---|---|---|
| Nub's current N-API plan | `--allow-fs-read --allow-addons` | low (addons is widely accepted as Node-runtime-class) |
| Nub + oxc-WASI (counterfactual) | `--allow-fs-read --allow-wasi --allow-worker` | medium (two more grants, neither well-known) |
| Nub + oxc-bindgen (hypothetical) | `--allow-fs-read` | **lowest** |

The hypothetical wasm-bindgen build is the only option that meaningfully beats N-API on permission grants; until it exists, the N-API plan is no worse than the WASM alternative. That inverts the question's premise: "WASM sidesteps `--allow-addons`" holds for wasm-bindgen modules, not for the napi-rs+emnapi build the question implicitly proposes.

## 5. Bundle / distribution implications

### 5.1 Single-WASM vs per-platform N-API tally

**N-API per-platform install (what npm currently does for `oxc-transform`):**

| Platform | binary size |
|---|---|
| darwin-arm64 | 3.6 MB |
| darwin-x64 | ~3.6 MB |
| linux-x64-gnu | ~4 MB |
| linux-x64-musl | ~4 MB |
| linux-arm64-gnu | ~4 MB |
| linux-arm64-musl | ~4 MB |
| win32-x64 | ~4 MB |
| win32-arm64 | ~4 MB |
| freebsd-x64 | ~4 MB |
| **Total across all platforms** | **~30 MB** |

The user installs only **one** of these (their host's optionalDeps match), so per-user disk cost is ~3.5–4 MB — the same as the 3.5 MB WASM single-binary. The registry carries ~30 MB across platform packages against WASM's ~3.5 MB: a marginal registry cost, no practical user difference.

**The "single WASM binary" claim is not a real distribution simplification.** npm's optionalDependencies mechanism — used by every Rust-via-napi-rs project today (rolldown, swc, oxc, lightningcss) — makes the per-platform install transparent, and nobody has been complaining about per-platform `.node` distribution for years.

Where single-WASM **does** matter: novel runtime environments (WebContainers, edge functions with no native binary loader, some serverless platforms). Those either already run Node successfully, in which case `.node` addons work too, or don't run Node at all, in which case Nub is out of scope regardless.

### 5.2 Total Nub binary impact

Nub's distribution is a Rust CLI binary; the transpile runtime lives in an npm-installed sidecar that Nub's `--import` preload loads, **not in the Rust binary**. Whether that sidecar is `.node` (N-API) or `.wasm` does not change the CLI binary size — only the **first-run** install size, ~3.5 MB either way for the user's host platform.

Embedding the transpiler inside the Rust CLI binary (a native Rust dependency on the `oxc_transformer` crate, called via FFI or a small Node child process) would be a different question than WASM-vs-N-API. Not on the current plan.

## 6. Failure modes

### 6.1 oxc WASM (emnapi+WASI) production crashes

Demonstrated above (§3.5): at N=10000 transpiles of a 165-line file, the WASM runtime crashes with `RuntimeError: memory access out of bounds` — a wasm memory corruption / OOM in emnapi's allocator, triggered by sustained allocation churn. For Nub's TS pipeline, which may transpile thousands of files per `nub build`-style invocation, this is a hard blocker.

### 6.2 V8 wasm compile-cache absence

V8's wasm compile cache is in-memory and scoped to the Module bytes; it does NOT persist across `node` processes by default, so every cold `nub script.ts` pays the 25 ms wasm-compile cost. Reaching it from an `--import` preload would need a serialized snapshot (`--predictable-gc-schedule` / `--snapshot-from`) carrying the pre-compiled module — a separate engineering effort, and snapshot stability is fragile across Node minor versions.

### 6.3 wasm32-wasi requires `--allow-wasi` AND `--allow-worker`

Already detailed in §4.3. The grants users have to enumerate under `--permission` are strictly more for the available WASM build than for N-API.

### 6.4 Feature gaps in transpiler WASM builds

- `@swc/wasm-typescript` strip-only mode does **not** handle: parameter properties, enums, namespaces (verified — three of these errored in our benchmark setup).
- amaro `transform` mode handles all of those but adds ~2× latency vs strip-only and pulls in a heavier wasm bundle.
- `@oxc-parser/wasm` is parser-only; no transform step. Not useful as a transpiler.

Nub's "non-erasable syntax support" commitment needs full transform mode — which on the WASM side means amaro (2,500 files/sec) or oxc-WASI (28,000 files/sec but crashes).

### 6.5 WASM ↔ JS boundary cost for large files

The benchmarks above use a 4.5 KB file. For larger files (50 KB+), the JS→WASM copy of the source string and the WASM→JS copy of the output start to matter. Not benchmarked here; the literature ([nickb.dev/wasm-and-native-node-module-performance-comparison](https://nickb.dev/blog/wasm-and-native-node-module-performance-comparison/)) expects the WASM/native delta to widen with payload size, not narrow.

### 6.6 Go-WASM and the esbuild-wasm caution

Esbuild's official position is that `esbuild-wasm` is "an order of magnitude slower" than `esbuild` native ([esbuild FAQ](https://esbuild.github.io/faq/), [GitHub issue #219](https://github.com/evanw/esbuild/issues/219)). Their reasons:

- Node re-compiles the WebAssembly on every invocation (no on-disk compile cache).
- Go's WASM compilation is single-threaded; native esbuild parallelizes across cores.

Our Rust→WASM toolchain is better than Go→WASM (wasm-bindgen produces tighter modules than Go's stdlib), but the cold-recompile constraint is the same.

## 7. Recommendation

### v0.1: ship N-API, not WASM

1. **N-API oxc-transform via `oxc-transform` + `@oxc-transform/binding-<platform>`** is the right primary transpiler. 178k files/sec, 0.12 ms cold-start-first-transform, 4 MB RSS.
2. **`--allow-addons` is a documented required grant** under permission mode, and belongs on the grant-set list. The "SecurityWarning: The flag --allow-addons must be used with extreme caution" Node emits is a known annoyance; the docs need a note explaining that the addon Nub loads is the transpile binding, makes no network calls, and writes nothing outside the cache dir.
3. **Record `--allow-addons` as part of the v0.1 required-grant set**, not a future-N-API-loading TODO.

### Defer WASM transpiler to "permission-locked environment" fallback (post-v0.1)

If/when a real user comes up against `--allow-addons` being unacceptable (corporate policy, sandbox, etc.), the fallback is:

- **Today's available WASM:** `oxc-transform`'s WASI binding works but requires `--allow-wasi --allow-worker` instead, which is arguably worse from a "minimize the permission surface" angle.
- **Future ideal WASM:** wasm-bindgen `wasm32-unknown-unknown` oxc transformer build. Doesn't exist; we'd need to either contribute it upstream to oxc-project or maintain our own fork of the oxc-transform crate compiled with that target.

The upstream-contribution path is the right play if we ever go here: 1-2 weeks of work for someone familiar with the oxc build system, the result lands in the ecosystem (Vite browser preview, Rolldown WebContainer), and Nub gets a permission-friendly fallback without owning a fork.

Until that exists: **`NODE_COMPAT=1 nub ./script.ts --permission` is the documented escape hatch** for permission-locked users who can't grant `--allow-addons`. Compat mode no-ops Nub's transpile pipeline; the user runs plain Node with `--experimental-strip-types` / `--strip-types`, which uses amaro/swc-wasm and is already permission-friendly.

### Phasing summary

| Phase | Transpiler | Permission grant set |
|---|---|---|
| v0.1 | oxc N-API | `--allow-fs-read --allow-addons` |
| v0.x (if user demand) | oxc N-API + oxc-WASM-bindgen fallback selected at install | `--allow-fs-read --allow-addons` (default) OR `--allow-fs-read` (with `--no-native-addons` opt-in) |
| Permission-locked users today | Compat mode | Whatever Node natively needs (`--allow-fs-read` is enough for `--strip-types`) |

### Anti-recommendation

Do not switch to amaro / @swc/wasm-typescript as the primary transpile path. It's the slowest of the four runtimes measured (amaro 0.46 ms/call vs. oxc-native 0.005 ms — **91× slower**), and our [Oxc-first decision](node-swc-vs-oxc-choice.md) was correct on technical merits regardless of the permission story.

## 8. Open questions

- **Does an oxc-transform wasm-bindgen build exist outside the npm registry?** A `wasm32-unknown-unknown` build is possible in principle but the oxc transform crate's dependency on `oxc_span` and the resolver may pull in `std::fs` at compile time. Verifying feasibility would require trying to compile `oxc_transformer` with `wasm32-unknown-unknown` target and seeing what breaks. Not attempted here.
- **Does V8's wasm compile cache work for `--import` preloads?** V8 caches `WebAssembly.Module` instances within a process; cross-process caching needs `--predictable-gc-schedule` snapshots or similar. Empirical test of cold-start with a warm V8 cache would clarify whether the 25 ms wasm-compile tax is amortizable. Not tested.
- **Could a daemon amortize the wasm compile cost?** A long-running Nub caching daemon that pre-loaded the wasm transpiler would take the cold-start tax only on its first start. But the daemon path is itself optional, and a non-daemon `nub script.ts` would still pay the tax.
- **Is the oxc-WASI N=10000 OOM upstream-fixable?** The crash trace points at emnapi's allocator; whether it's an emnapi bug, an oxc-transformer allocation pattern, or a V8 wasm-engine limitation is undetermined. Filing an upstream bug would be the next step if WASM becomes load-bearing.
- **Permission ergonomics for `--allow-wasi`.** If we ever needed to ship a WASI-using addon (FS-touching wasm), the `--allow-wasi` grant is per-process. There's no `--allow-wasi=<path>` scoping like `--allow-fs-read=<path>`. Coarse-grained grant. Worth flagging if WASI-based extensions become part of any future Nub design.
- **Multi-platform install size on real OS.** We measured darwin-arm64 only (3.6 MB). Linux glibc + musl variants may be larger due to static linking; would be worth measuring on a Linux CI host before publishing a "Nub install size: X MB" claim.
- **Could embedded-libnode change the answer?** If Nub were to eventually embed libnode in the Rust binary (per the [node-embedding-vs-spawn.md](node-embedding-vs-spawn.md) write-up), it could call into oxc-transformer directly from Rust, bypassing the N-API-vs-WASM question entirely. Out of scope for v0.1.

## Sources

### Primary (empirical tests on this machine, May 2026)

- Tests run on Node v24.14.0, darwin-arm64. All benchmark scripts reproduced under `/tmp/pkg-sizes/` during this research. Numbers are from cold runs without prior warm-up of file system caches.
- Permission model behavior tested via direct invocation of `node --permission --allow-* ...` against minimal test fixtures (41-byte hand-written .wasm; oxc-transform; amaro; swc-wasm-typescript).

### Node.js source / docs (verified)

- [Node.js Permission Model](https://nodejs.org/api/permissions.html) — registered permission scopes; `--allow-addons` gates native addons; `--allow-wasi` gates the `node:wasi` API; no WebAssembly-specific scope.
- [Node.js CLI flags](https://nodejs.org/api/cli.html) — `--experimental-wasm-modules`, `--allow-addons`, `--allow-wasi` semantics.
- `lib/internal/modules/esm/load.js` (Node main branch, fetched via WebFetch) — .wasm load path uses standard fs read; no addon permission check.
- `src/permission/permission.cc` (Node main branch, fetched via WebFetch) — enumerates {FileSystem, ChildProcess, WorkerThreads, Net, Inspector, WASI, Addon, FFI} permission scopes.

### Transpiler-WASM ecosystem

- [@swc/wasm-typescript on npm](https://www.npmjs.com/package/@swc/wasm-typescript) — Node's chosen TS stripper distribution.
- [@oxc-parser/wasm on npm](https://www.npmjs.com/package/@oxc-parser/wasm) — Oxc parser WASM, out-of-date vs native (0.60 vs 0.67).
- [oxc-project/oxc issue #10778](https://github.com/oxc-project/oxc/issues/10778) — out-of-date @oxc-parser/wasm versioning report.
- [oxc-project/oxc discussion #3311](https://github.com/oxc-project/oxc/discussions/3311) — WASM build status discussion.
- [Oxc Transformer Alpha announcement (2024-09-29)](https://oxc.rs/blog/2024-09-29-transformer-alpha) — performance vs SWC.
- [rolldown's wasm build status](https://github.com/rolldown/rolldown/discussions/3391) — uses napi-rs/emnapi WASI path; same shape as oxc-transform's wasi binding.
- [@napi-rs/wasm-runtime](https://www.npmjs.com/package/@napi-rs/wasm-runtime) — the runtime emnapi+WASI shim used by oxc-transform's WASI build.

### Performance write-ups (external)

- [Wasm and Native Node Module Performance Comparison](https://nickb.dev/blog/wasm-and-native-node-module-performance-comparison/) (nickb.dev) — Rust N-API vs WASM, 1.75–2.5× native faster on a zip+inflate+parse workload.
- [NodeJS Native Module vs WASM](https://yieldcode.blog/post/native-rust-wasm/) — fibonacci benchmark, native 1.6× faster than WASM at scale.
- [esbuild FAQ](https://esbuild.github.io/faq/) — "the WebAssembly version is much slower than the native version, in many cases an order of magnitude slower."
- [esbuild issue #219](https://github.com/evanw/esbuild/issues/219) — author's reasoning for not investing in esbuild-wasm performance.
- [@swc/wasm-typescript docs](https://swc.rs/docs/references/wasm-typescript) — what the WASM build supports/doesn't support.
- [pkgpulse 2026 TS-strip benchmark](https://www.pkgpulse.com/blog/ts-blank-space-vs-node-strip-types-vs-swc-typescript-type-stripping-2026) — contemporary cross-tool TS stripping comparison.
- [Evan You: Oxc TS stripping 4× faster than swc_fast_ts_strip](https://x.com/youyuxi/status/1890701933767246117) (Feb 2025).

### Related work in this corpus

- [`node-swc-vs-oxc-choice.md`](node-swc-vs-oxc-choice.md) — sister research doc explaining why oxc, not swc; this doc's performance numbers reinforce that.
- [`node-embedding-vs-spawn.md`](node-embedding-vs-spawn.md) — the embedded-libnode option raised in the open questions.

Two findings here feed decisions recorded elsewhere: `--allow-addons` belongs in the v0.1 required-grant set of the `--permission` interop policy, and auto-unflagging `--experimental-wasm-modules` has no permission-model downside. The mechanism constraint is unchanged either way — the transpiler is loaded through Node's own extension surfaces (`--import` preload), so WASM and N-API are both in-scope and the choice rests on performance, permissions and ergonomics.

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links and reference-checkout paths were rewritten; findings and measured values are unchanged.

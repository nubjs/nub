# Cold Start: where Node spends its 12–20 ms

> Research target: source-grounded breakdown of why `node hello.js` takes ~15 ms warm on macOS arm64 while `bun hello.js` finishes in <5 ms. Locally measured on Node v24.14.0 / Bun 1.3.9. Goal: name the costs, name the upstream work, decide what Nub should do.

## Local measurements (Apple Silicon, macOS 15)

Hyperfine, `--shell=none`, 100 runs after a 10-run warmup:

| Invocation | Mean | Min … Max |
|---|---|---|
| `node -e ''`            | 26.7 ms | 25.3 … 28.9 |
| `node hello.cjs`        | 27.8 ms | 26.3 … 35.1 |
| `node hello.mjs`        | 29.4 ms | 28.1 … 32.4 |
| `node import-fs.mjs`    | 31.7 ms | 29.2 … 110.0 |
| `bun -e ''`             |  4.4 ms |  3.8 … 5.4  |
| `bun hello.cjs`         | 11.3 ms |  9.7 … 17.4 |
| `bun hello.mjs`         | 10.8 ms |  9.7 … 12.6 |
| `bun import-fs.mjs`     | 17.0 ms | 15.7 … 19.4 |

Cold first-run (fs cache evicted) is dramatically worse: Node went to **137 ms** on the very first invocation before warmup. That cold case is what feels "perceptibly slow" — the warm case is borderline. Both matter, but the warm case dominates day-to-day developer experience.

Note that `node -e ''` does not load a script file; the ~1 ms it shaves vs `node hello.cjs` is roughly the fopen+stat+read+parse of an empty script. `--no-warnings` and `--disable-warning` made no measurable difference, so the per-warning install cost is below noise.

## TL;DR

A warm `node ./hello.js` on macOS arm64 today spends its ~15–27 ms roughly like this (apportioned from flamegraphs and `--no-node-snapshot` A/B numbers in [nodejs/performance#180][perf180]):

| Phase | Approx. share | Notes |
|---|---|---|
| `dyld` + image load + global ctors (incl. weak-symbol fixups) | 4–6 ms | macOS-specific; was 8 ms worse before [#56275][pr56275] (already landed in v24) |
| `node::InitializeOncePerProcess` (OpenSSL, cppgc, ncrypto, ICU register) | 3–4 ms | dominated by `OPENSSL_init_crypto` and `cppgc::InitializeProcess` |
| `NodeMainInstance` ctor (Isolate + 4 context snapshots deserialize) | 3–4 ms | this is the V8 startup snapshot fast path |
| Bootstrap JS (`internal/process/pre_execution.js`, ESM loader init, source-map cache, diagnostics_channel, etc.) | 2–3 ms | a lot of "setup X" calls that run even when unused |
| Compile + run user file, CJS loader, module resolution | 0.5–1 ms | the actual work |
| `__cxa_atexit` / teardown (counted by hyperfine) | 1–2 ms | non-trivial on macOS |

Takeaways:

1. **Most of the 15 ms is not "Node bootstrap" in the JS sense.** It's `dyld`, libc++ template fixups, OpenSSL one-time init, V8 isolate construction, and snapshot deserialization — all C++ that runs before a single line of `lib/internal/bootstrap/node.js` executes.
2. **The snapshot is doing its job.** Without it, empty-script startup on Linux is ~53 ms vs ~18 ms with it (the 2021 builtins-snapshot work, ~3× speedup, per the PR description on [#27321][pr27321]). The remaining 15 ms is what's left _after_ the biggest already-fixed lever.
3. **Bun's headline win is mostly that it links statically (no dyld weak-symbol fixups), uses JSC instead of V8 (smaller engine init, smaller snapshot footprint), and skips OpenSSL on macOS in favor of BoringSSL/Apple frameworks.** That is, the gap is mostly _under_ Node's main(), not above it.

## Sources of cost, itemized

### 1. `dyld` and global C++ constructors (macOS-only tax)

This is the single biggest macOS-specific cost and the one with a proper post-mortem in the PR description on [#56275][pr56275].

Between v20 and v23, macOS startup regressed from ~19 ms to ~30 ms. Root cause: V8's upgrade 11.3 → 11.8 added a huge number of templated `StaticCallInterfaceDescriptor` instantiations. Without `-fvisibility=hidden` on the V8 build, those templates produced weak symbols that `dyld` resolved at process start. From the bench in [nodejs/performance#180][perf180]:

> "DYLD_PRINT_BINDINGS=1 ./node --version 2>&1 | grep 'looking for weak-def symbol' | wc -l: 7317" versus the fixed build: "1755"

Fix: add `-fvisibility=hidden` (plus `BUILDING_V8_SHARED`) to V8's gypfiles. Result: **2.33× faster startup on macOS arm64 (28.9 ms → 12.4 ms), binary 10 MB smaller (118 → 108 MB)**, landed in [#56275][pr56275] (Dec 2024, in v23.7/v22.13). V8's own `node-ci` fork did not have the regression because Chromium's build always sets `-fvisibility=hidden`; that contrast is how the regression was located.

**This fix is in v24, so our local 27 ms baseline already reflects it.** The 16 ms reclaim is not on the table for Nub — it's our baseline too.

Even with the fix in, `dyld` is still the largest single contributor on macOS. From `otool -L node`:

```
/System/Library/Frameworks/CoreFoundation.framework/.../CoreFoundation
/usr/lib/libSystem.B.dylib
/usr/lib/libc++.1.dylib
```

A proposal to remove CoreFoundation ([#44715][pr44715]) was closed unactioned because ICU pulls it in. From Daniel Lemire in [#180][perf180]:

> "One maybe significant difference between bun and node is that node depends on Core Foundation whereas bun does not appear to do so... So I am back at my theory: this is a case where dynamic linking and rebasing is expensive under macOS for some reason."

There is no equivalent post-mortem for Linux, where startup stayed roughly flat across versions (Lemire's measurements on Linux i5: node 17.9 → 21.8 ms across the same versions — actually _improving_).

### 2. `node::InitializeOncePerProcess` (OpenSSL, cppgc, ICU)

Source: [`src/node.cc`][nodecc] `InitializeOncePerProcessInternal`. The flamegraph diff in [#180][perf180] (billywhizz) put this at **~30% of v22 wall time** (6.5 ms of the 16 ms regression). Sub-costs:

- `OPENSSL_init_crypto` — ~6.5% of total. Building with `./configure --without-ssl` drops about 3.5 ms in A/B (35.4 → 26.9 ms). Node forks [quictls/openssl][quictls] and links it statically; OpenSSL v3's provider model is heavier than 1.1 was.
- `cppgc::InitializeProcess` — ~2.5%, added between v20 and v22 when V8's cppgc became a hard dependency. No tracking issue for removal.
- `ncrypto::CSPRNG` — ~2.5%, the seed gather for `crypto.randomUUID` etc. (Separately, [#59550][pr59550] moved _system CA_ loading off-thread — saved ~48 ms on first TLS context, 57 → 8.5 ms — but this is a TLS warmup fix, not a startup fix.)
- ICU registration — `--without-intl` saves ~30 MB binary but "didn't seem to make a difference" to startup ([#180][perf180]).

### 3. V8 isolate construction + snapshot deserialize

Source: `node::NodeMainInstance::NodeMainInstance` constructor. The flamegraph attributes ~26% of wall time to it in both fast and slow builds — i.e. constant, not regressed. So roughly **3.5 ms** at v23 macOS.

What gets deserialized is documented in [`tools/snapshot/README.md`][snapREADME]: one isolate snapshot plus **four** context snapshots:

> 1. The default context snapshot ... 2. The vm context snapshot ... 3. The base context snapshot ... 4. The main context snapshot ... captures initializations done by `node::CommonEnvironmentSetup::CreateForSnapshotting()`, most notably `node::CreateEnvironment()`, which runs the following scripts via `node::Realm::RunBootstrapping()` for the main context as a principal realm, so that at runtime, these scripts do not need to be run. Instead only the context initialized by them is deserialized at runtime. 1. `internal/bootstrap/realm` 2. `internal/bootstrap/node` 3. `internal/bootstrap/web/exposed-wildcard` 4. `internal/bootstrap/web/exposed-window-or-worker` 5. `internal/bootstrap/switches/is_main_thread` 6. `internal/bootstrap/switches/does_own_process_state`

That's the only JS that doesn't run at runtime in a default build. As of Feb 2026, the ESM loader joined them via [#61769][pr61769]; before that it was being initialized from scratch on every start.

Snapshot compression was disabled by default in [#45716][pr45716] — +2.7 MB binary in exchange for **9–18% faster startup**, validating that decompression was on the hot path.

### 4. Bootstrap JS that still runs on every start

The post-snapshot JS bootstrap lives in [`lib/internal/process/pre_execution.js`][preExec] (26 KB). It is literally a sequential `setup…()` chain:

```
patchProcessObject     setupTraceCategoryState     setupInspectorHooks
setupNetworkInspection setupNavigator              setupWarningHandler
setupFFI               setupSQLite                 setupStreamIter
setupQuic              setupWebStorage             setupWebsocket
setupEventsource       setupCodeCoverage           setupDebugEnv
initializeReport       setupDiagnosticsChannel     initializePermission
initializeSourceMapsHandlers                       initializeDeprecations
initializeConfigFileSupport                        initializeDns
setupStacktracePrinterOnSigint                     initializeReportSignalHandlers
initializeHeapSnapshotSignalHandlers               setupChildProcessIpcChannel
initializeClusterIPC                               initializeExtensionFormatMap
setupVmModules                                     initializeModuleLoaders
setupHttpProxy
```

Every one runs on every hello-world. [#45659][pr45659] ("bootstrap: lazy load non-essential modules", merged Dec 2022) was the big project to push this back. From the PR description:

> "It turns out that even with startup snapshots, there is a non-trivial overhead for loading internal modules. This patch makes the loading of the non-essential modules lazy again."

Result: **~17% faster basic startup, ~37% faster worker startup**, at the cost of 5–10% on apps that need every builtin. The PR primarily recovered a regression rather than going below v14.

Follow-ups still landing in 2025–26: [#62267][pr62267] lazy source-map cache in the CJS loader; [#59517][pr59517] lazy `internal/tty` in tests; [#56980][pr56980] lazy modules in test runner; [#57307][pr57307] `fs.getLazy`; [#59473][pr59473] simdjson for `--snapshot-config`. Pattern: each PR shaves micro/single-digit milliseconds; nobody has the lever to drop 5+ ms in one PR anymore.

### 5. Module resolution and the actual user script

Almost free. From the discussion in [#180][perf180]:

> "in recent versions of Node.js no internal JS code is compiled at all when executing a CJS script, that's because we moved the internal JS code compilation into build time and serialized the bytecode into the snapshot."

For ESM (`hello.mjs`), the loader is now snapshotted as of [#61769][pr61769] (Feb 2026) but only just merged. From the PR description:

> "empty/minimal CJS startup is now slightly slower in worker but other metrics get a slight boost (because they all incur ESM loader initialization). In reality ESM loading is likely to happen at some point in the lifetime of an application especially with the growing adoption of ESM and `require(esm)`."

This explains the ~2 ms gap between `hello.cjs` (27.8 ms) and `hello.mjs` (29.4 ms) in our local bench — and that gap should narrow once #61769 propagates.

### 6. Teardown (counted by hyperfine)

billywhizz, [#180][perf180]: "the small traces on left and right of the graph are system code being run when tearing down the process and take ~6% of time in the fast instance and ~12% of time in the slow instance" — roughly **1–2 ms** of `__cxa_atexit`/`Isolate::Delete`. Hyperfine includes this; it is not strictly "startup" but is in every benchmark quoted here.

## What Node has already done

Timeline of landmark startup work, all from [#35711][issue35711] ("Tracking issue: snapshot integration in Node.js core", open since 2020):

| When | PR | What |
|---|---|---|
| v12.5 (2019) | [#27321][pr27321] / [#28181][pr28181] | First V8 startup snapshot landed |
| 2021 | `tools/js2c.py` | Internal JS encoded into C++ arrays at build time, V8 code cache pre-compiled into `node_code_cache.cc`, bootstrap context serialized into `node_snapshot.cc`. "Node.js does not need to execute this part of the bootstrap at all." |
| v19.6 | [#42466][pr42466] | Build-time user-land snapshot via `--node-snapshot-main`; foundation for SEA |
| v19.6 | [#45659][pr45659] | Bootstrap lazy-loads non-essential modules — **+17% startup** |
| v19.6 | [#45716][pr45716] | Disable snapshot compression by default — **+9–18% startup, +2.7 MB binary** |
| v23.7 / v22.13 | [#56275][pr56275] | `-fvisibility=hidden` for V8 on macOS — **+2.33× macOS startup, –10 MB binary** |
| 2025 | [#59550][pr59550] | System-CA load off the main thread — first TLS context 57 → 8.5 ms |
| 2026-02 | [#61769][pr61769] | ESM loader baked into the built-in snapshot |
| ongoing | many | `getLazy()` retrofits across `fs`, source-map cache, test runner, internal/tty |

For runtime user-land snapshotting, [#44014][issue44014] is the open tracking issue. It is gated on V8 not supporting a long list of types in run-time-built snapshots; the build-time path is what powers SEA (Single Executable Applications). The integration story for packagers is still cumbersome ([#42566][pr42566]).

## What's still on the table upstream

Open and unresolved:

- **Run-time snapshots for arbitrary user code** — [#44014][issue44014], open since 2022. Blocked on V8 supporting more embedder types outside build-time snapshots.
- **Macro-level OpenSSL init cost** — no tracking issue; the `OPENSSL_init_crypto` line is the largest single C++ frame still visible. Bun avoids it by using BoringSSL.
- **`cppgc::InitializeProcess`** — added by V8 upgrade; no Node-side fix being worked.
- **Config-file initialization** ([#53787][issue53787]) is adjacent: loading a config file by default "adds overhead to the startup to probe the file system." Same problem will face any package.json-based hooks config; the lean is on a field _inside package.json_ rather than a new dotfile.
- **CoreFoundation dependency on macOS** — [#44715][pr44715] proposed removal; closed without action because ICU pulls it in.
- **Single-pass bootstrap JS** — `pre_execution.js` is still a long imperative chain. Each `setup…` adds μs; together they're a couple of ms. There are `TODO: move this to vm.js?` markers in the source suggesting more lazification is wanted.
- **Run-time options parsing in JS** — `refreshRuntimeOptions()` runs every start. [#59473][pr59473] moved snapshot-config parsing to simdjson; the full options system is still JS.

## Why hasn't Node done this already?

The natural pushback on the levers below is "if any of these were easy, Node would have shipped them years ago." Honest answer, lever by lever:

1. **Some they did.** `-fvisibility=hidden` shipped in [#56275][pr56275] (Dec 2024). The 16 ms reclaim is _already in our v24 baseline_, not a future win for Nub. Conflating "Node hasn't done this" with "Nub should do this" is the common error to avoid in this doc.

2. **Most of the rest, Node is doing — slowly.** The `getLazy()` PR train ([#45659][pr45659] and follow-ups) has been clawing back `pre_execution.js` overhead for three years and is maybe halfway. Each `setup{Inspector, Permission, DiagnosticsChannel, …}` call has subtle ordering guarantees: `process.on('warning')` listeners installed by user code must fire if a warning is emitted by another setup; permission model must be live before any fs/net access; diagnostics channels must precede async_hooks. Each step is a bug magnet that has to land behind tests and a release cycle. They can't take the cut in one swing; a from-scratch runtime can, in exchange for accepting compat risk on userland that introspects globals before touching them.

3. **Some they can't do without breaking promises we don't owe.**
   - **Static linking**: Debian/Fedora packaging policy forbids it — distros want to swap OpenSSL for CVEs without rebuilding Node. Bun ships static because it's distributed direct from `bun.sh/install`. Nub can ship static for the same reason — we have no distro relationship to maintain.
   - **Narrower snapshot**: Node's snapshot is shared across `node`, `--eval`, `vm.Script`, workers. Specializing it means either binary bloat (multiple snapshots) or build-at-install — and the productized "build-at-install snapshot" already exists as SEA, which is opt-in. Changing the default breaks embedders.
   - **Lazy OpenSSL init**: tracked off and on, stalled on FIPS mode (must be configured pre-crypto), eager `globalThis.crypto` (Web Crypto is a spec-visible global), and OpenSSL thread callbacks needing to be installed before any worker spawns. Not impossible, but the Node team has chosen predictability over ~3 ms.

4. **Their userbase doesn't feel the pain we feel.** Node's revenue-generating workloads are long-running servers where 15 ms of startup amortizes to zero. The cohort that perceives "node is slow" is developers running CLIs on macOS — and that cohort has weaker pull in TSC discussions than "don't break our deployed install base."

5. **Governance overhead.** Every change needs TSC sign-off and a deprecation path. A from-scratch project with no users gets to take the cut in one PR.

**This is the structural opening for Nub.** The cold-start work validates a thesis Nub should lean on broadly: _Node is doing the right work, in the order their constraints permit. Nub's value-prop is shipping that work eagerly._ We don't have 25 M deployed apps to keep compatible, no TSC, no distro packaging contract, no FIPS-mode customers. We can take the entire `getLazy()` train in one cut, ship static, lazy-init OpenSSL, and narrow the snapshot — all on day one.

## Maintainer commentary (verbatim, third-party angles)

From Daniel Lemire (TSC, perf-focused), in [#180][perf180]:

> "10 ms is quite a large effect. Especially if you account for the fact that bun can print 'hello' in 5 ms."

> "Bun is 58 MB and it runs in about 7 ms on my mac with the same benchmark. One maybe significant difference between bun and node is that node depends on Core Foundation whereas bun does not appear to do so..."

> "So I am back at my theory: this is a case where dynamic linking and rebasing is expensive under macOS for some reason."

From billywhizz (independent runtime author of `lo` / `just-js`), [#180][perf180], after the M3 Max flamegraph diff:

> "most of the overhead in InitializeOncePerProcessInternal seems to be coming from OpenSSL initialization. Most of the overhead in NodeMainInstance::NodeMainInstance constructor seems to be coming from v8 snapshot initialization."

> "bun is crazy fast for a micro bench like this. i think is more to do with JSC than anything. i have tried building a minimal runtime on v8 for macos and best i can do is ~15 ms, which is roughly same as deno. while bun on same hardware is ~7 ms. on linux the situation is the opposite ime — bun/JSC is almost 2x slower than a minimal v8 runtime."

That last point matters: **JSC vs V8 is a big lever on macOS specifically**, not in general. JSC is not a free lunch for cross-platform.

From Geoffrey Booth (ESM lead), on config-file proposals in [#53787][issue53787]:

> "The recent .env file support was meant as a bridge to this; that effort got us the ability to parse JSON files without needing to start V8."

From isaacs (npm originator), [#53787][issue53787]:

> "We all hate json. No comments, excessive quoting, no multi line strings, no trailing commas, etc. But: it's specified very clearly (unlike ini, which is not specified at all); it's built into the language; It's FAST, like, omg wow, much faster than yaml or toml, not even close."

## Implications for Nub

Given a Node-compat surface and Node-compat semantics, the levers in priority order. Effort is rough sizing; compat risk is rated explicitly. **All numbers are against the v24 baseline** (i.e. they do _not_ double-count savings already in #56275).

### Priority 1: Static link everything we can (small effort, zero compat risk)

Even with `-fvisibility=hidden` collected upstream, Node still ships as a dynamically-linked binary loading libc++ / CoreFoundation / etc. Each dynamic dep contributes dyld fixup work. A statically-linked Nub binary skips it. Bun's edge here is mostly structural. **Estimated saving: 1–2 ms macOS. Effort: small (build config). Risk: none, beyond the distro-packaging question (which doesn't apply to Nub's direct-download distribution model).**

### Priority 2: No OpenSSL on the hot path (medium effort, low compat risk)

`OPENSSL_init_crypto` is ~3.5 ms by `--without-ssl` A/B. Options, best to worst from a compat angle:

1. **Lazy-init crypto on first use** of `node:crypto` / `node:tls` / `globalThis.crypto`. Node can't easily do this because their CSPRNG is touched in `InitializeOncePerProcess` and Web Crypto is a spec-visible global. A from-scratch runtime can install a Web Crypto _facade_ that defers backing init until first call. **Saving: 2–3 ms cold. Effort: medium. Risk: low — the only observable change is `process.versions.openssl` reading lazily.**
2. Use BoringSSL (Bun's choice) or rustls; both have cheaper init. Effort jumps because then OpenSSL-shaped APIs (`crypto.createHash` etc.) need back-paving on top.

### Priority 3: One snapshot, not four (medium effort, no compat risk)

Node has four context snapshots (default / vm / base / main) per [tools/snapshot/README.md][snapREADME]. The vm and base snapshots only matter when `vm.createContext()` or workers are used; for `nub run hello.js` they are dead weight in the isolate's view. A Nub snapshot that deserializes only what the current invocation needs (essentially: main context + the parsed `package.json` + resolved entry path) is a narrower deserialize. **Saving: 0.5–1 ms. Effort: medium. Risk: none if vm/worker remain on-demand.**

### Priority 4: Don't run `pre_execution.js` (large effort, medium compat risk)

The `setup{Inspector,Navigator,Warning,FFI,SQLite,Stream,Quic,WebStorage, Websocket,Eventsource,CodeCoverage,DiagnosticsChannel,Permission,Dns, …}` parade in [`pre_execution.js`][preExec] is ~2 ms of pure overhead. Each item exists because _someone, somewhere_ depends on the side effect being visible by the time user code runs.

The compatible play: replicate each setup as a getter on the relevant global / module namespace, install once at snapshot build, never run imperatively at start. Node has been doing this gradually ([#45659][pr45659] and the long `getLazy` PR train); we can start there and go further because we don't carry their legacy `process.binding` shape. **Saving: 1–2 ms. Effort: large (every setup needs auditing for side-effect timing). Risk: medium — code that introspects globals before touching them could observe lazy getters.**

### Priority 5: cppgc deferral (small effort, low compat risk)

`cppgc::InitializeProcess` accounts for ~2.5% per billywhizz. If nothing in the user's first tick allocates a cppgc-managed object (true for `hello.js`), the init can run on a background thread or on first allocation. Nub can decide this; Node cannot trivially because its bindings register early. **Saving: ~0.4 ms. Effort: small. Risk: low.**

### Priority 6: Skip CoreFoundation on macOS (medium effort, low compat risk)

[#44715][pr44715] was closed unactioned because ICU pulls CoreFoundation in. A from-scratch runtime can either (a) ship ICU's data file separately and use the small-ICU build, restoring `Intl` lazily via dlopen on first use, or (b) use Apple's `NSLocale` directly on macOS for `Intl`. **Saving: probably 0.5–1 ms macOS. Effort: medium. Risk: low if `Intl` semantics stay identical (this is a known minefield; the win may not be worth the test burden).**

### Out of scope for Nub v1

- **JSC instead of V8.** Switching engines is what gives Bun the rest of its win on macOS, but shipping a non-V8 runtime is a multi-year commitment and a compat landmine (Maglev vs FTL, Atomics quirks, addon ABI). And billywhizz's data shows V8 actually beating JSC on _Linux_ for this benchmark — JSC is not unambiguously faster.
- **Daemonize.** Already settled in `wiki/research/daemon.md`. Bun reaches <5 ms without one.
- **Strip OpenSSL entirely.** `node:crypto` compat is non-negotiable. Lazy-init it, don't drop it.
- **Disable the V8 startup snapshot.** That costs ~35 ms. Keep it; just narrow ours.
- **Run-time user-land snapshots.** V8 doesn't support enough embedder types yet ([#44014][issue44014], open 4 years). Build-time is fine for SEA-equivalent later.

### What the math says

Stack-ranked savings on macOS arm64 from the v24 baseline measured above (27 ms warm `node hello.cjs`):

```
  v24 baseline                                   27.0 ms
- static link (libc++, no dyld penalty)           1.5 ms
- lazy OpenSSL init                               2.5 ms
- single narrower snapshot                        0.8 ms
- lazy pre_execution.js                           1.5 ms
- lazy cppgc                                      0.4 ms
- skip CoreFoundation / lazy ICU                  0.5 ms
                                                 ───────
  realistic compat-preserving target            ~20 ms
```

That is **~1.35× speedup** by aggressively executing every lever Node hasn't gotten to yet — i.e. a meaningful but not dramatic gap. Closing the rest to Bun's <5 ms requires leaving V8, which is out of scope for v1.

This is honest: the earlier 5.5 ms target double-counted savings already in v24. The real headline is not "we'll be 3× faster than Node" — it's "we'll be 30–40% faster on cold start, which is perceptible, _and_ we accumulate further upstream wins as Node lands them, without governance lag."

The bigger latency story for Nub is not `nub hello.js` (where Node is already merely-slow, not unusable) but the longer call chains users actually run — e.g. package-manager script runners that re-spawn Node processes. That's outside this doc's scope and tracked separately.

## Sources

[perf180]: https://github.com/nodejs/performance/issues/180 [pr56275]: https://github.com/nodejs/node/pull/56275 [pr45659]: https://github.com/nodejs/node/pull/45659 [pr45716]: https://github.com/nodejs/node/pull/45716 [pr42466]: https://github.com/nodejs/node/pull/42466 [pr59550]: https://github.com/nodejs/node/pull/59550 [pr61769]: https://github.com/nodejs/node/pull/61769 [pr59473]: https://github.com/nodejs/node/pull/59473 [pr27321]: https://github.com/nodejs/node/pull/27321 [pr28181]: https://github.com/nodejs/node/pull/28181 [pr44715]: https://github.com/nodejs/node/issues/44715 [pr62267]: https://github.com/nodejs/node/pull/62267 [pr59517]: https://github.com/nodejs/node/pull/59517 [pr56980]: https://github.com/nodejs/node/pull/56980 [pr57307]: https://github.com/nodejs/node/pull/57307 [pr42566]: https://github.com/nodejs/node/issues/42566 [issue35711]: https://github.com/nodejs/node/issues/35711 [issue44014]: https://github.com/nodejs/node/issues/44014 [issue53787]: https://github.com/nodejs/node/issues/53787 [snapREADME]: https://github.com/nodejs/node/blob/main/tools/snapshot/README.md [preExec]: https://github.com/nodejs/node/blob/main/lib/internal/process/pre_execution.js [nodecc]: https://github.com/nodejs/node/blob/main/src/node.cc [quictls]: https://github.com/quictls/openssl

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links and reference-checkout paths were rewritten; findings and measured values are unchanged.

---
**Status:** v1, 2026-05-16. Drafted to answer the question "how
much of Node's resolution can Nub do in Rust before V8 boots."
Companion to [`module-resolution.md`](module-resolution.md), which
covers the TS-extensionless slice in the JS hook layer; this doc
zooms out and looks at the *entire* algorithm.
**Builds on:** [`module-resolution.md`](module-resolution.md),
[`tsconfig-paths.md`](tsconfig-paths.md),
[`pnpm-specific-behavior.md`](pnpm-specific-behavior.md),
[`cold-start.md`](cold-start.md).
**Sibling:** [`augmentation-layers.md`](augmentation-layers.md)
(where in the lifecycle a Rust resolver could be wired).
**Informs:** `PLAN.md` — Pre-processing model,
`commands/run.md` (entry-point pre-resolution).
---

# Research: how much of Node's resolution can Nub do in Rust pre-V8?

Working write-up. Conclusions are current best read.

## Question

Nub owns the process from `nub run hello.js` through V8 boot. Node itself doesn't get a shot at resolution until its ESM/CJS loaders are alive in JS. **What fraction of `require.resolve` / ESM `PACKAGE_RESOLVE` can Nub answer from Rust against a cold V8?** And which parts genuinely need a running JS engine?

Motivation: every layer pushed pre-V8 is a cold-start win (see [`cold-start.md`](cold-start.md): Node spends ~15–27 ms of warm start *before* the user file is touched). It's also a compat-surface liability — Node's resolver changes, exports semantics drift, and a Rust mirror has to keep up.

## TL;DR

- ~85% of the resolution algorithm is purely declarative and filesystem-driven. The Rust ecosystem (`oxc_resolver`, `enhanced-resolve` port, Bun's resolver) already implements it.
- The ~15% that *requires* JS-runtime state: registered loader hooks (`module.registerHooks` / `module.register`), `require.cache` mutation, `require.extensions` monkey-patches, runtime-mutable `--conditions` (these are mostly static but observable via JS), `vm.Module` synthetic modules, and `import.meta.resolve` from inside user code.
- The honest split: **Nub can resolve the entry point and all reachable static imports in Rust with high confidence; it cannot resolve dynamic `import()` of computed specifiers, anything behind a user-registered loader hook, or anything that depends on side effects of already-executed JS.**
- Recommendation: ship `oxc_resolver` as the pre-V8 resolver for the entry point + a cache prewarm pass. Defer in-process resolution to the JS hook (which itself delegates to a shared Rust core via N-API). Do not attempt a full Rust replacement of Node's in-process resolver — the compat surface is hostile and the win is marginal past the entry point.

## Where the algorithm splits: pure vs JS-dependent

Annotated against `lib/internal/modules/esm/resolve.js` (per the node docs and the WHATWG-style algorithm at [nodejs.org/api/esm.html#resolution-algorithm](https://nodejs.org/api/esm.html#resolution-algorithm)) and `lib/internal/modules/cjs/loader.js`.

### Pure / filesystem-only (trivially Rust-able)

These compute a result from `(specifier, parentURL, cwd, fs, set-of-conditions)` with no observation of JS-runtime state past process start:

- **CJS candidate probing.** Walk `node_modules` up the parent chain; read each candidate's `package.json` `main` / `exports`; probe extensions in `[.js, .json, .node]` order. The classic `require.resolve` shape. Implemented end-to-end in `oxc_resolver`, `enhanced-resolve`, `node-resolve` crate, swc's loader, Bun's resolver, and `@vercel/nft`.
- **`PACKAGE_RESOLVE` for bare specifiers** — walk-up loop, scope detection, `package.json` parse.
- **`PACKAGE_EXPORTS_RESOLVE` / `PACKAGE_IMPORTS_RESOLVE`**, given a fixed condition set. The conditions array is static at process start under normal Nub usage (`["node", "import"]` or `["node", "require"]` plus any `--conditions=` flag values). No user JS mutates it after boot.
- **`PATTERN_KEY_COMPARE`** and the wildcard-specificity sort — pure string logic.
- **`PACKAGE_TARGET_RESOLVE`** — string interpolation + path validation; the `invalidSegmentRegEx` checks are pure regex.
- **`LOOKUP_PACKAGE_SCOPE` / `READ_PACKAGE_JSON`** — pure fs + JSON parse.
- **`ESM_FILE_FORMAT`** for `.mjs` / `.cjs` / `.json` / `.wasm` / `.node` — extension lookup + nearest-`package.json` `"type"`. Pure.
- **Symlink realpath / `preserveSymlinks` toggle.** A `realpath` syscall; relevant for pnpm (see below). Pure.
- **`tsconfig.json` `paths` resolution.** Pure once tsconfig (including `extends` and `references`) is loaded. `oxc_resolver` ships this.
- **Conditional exports evaluation** against a fixed condition set. Pure declarative.

### Mixed (Rust-able with a JS escape hatch)

- **`ESM_FILE_FORMAT` for `.js`** when there's no `"type"` in the nearest `package.json` and Node has to syntax-detect. The detection itself (look for `import` / `export` / top-level `await` / `import.meta`) is a parse pass; doable in Rust with oxc or `es-module-lexer-rs`, but it's "running a parser," not "running JS." Bun does this in Zig.
- **`--conditions=` and `--experimental-network-imports` style flags.** Static after Node boot. Rust can read its own CLI.
- **`require.resolve(specifier, { paths })`** — same algorithm, different starting directory. Rust-able.

### Genuinely JS-runtime-dependent (must defer)

These read or mutate state that lives inside V8:

- **User-registered loader hooks** via `module.registerHooks()` (sync, on-thread, added in v23 — see [nodejs/node#56241](https://github.com/nodejs/node/issues/56241)) or async off-thread `module.register()`. The hook *is* user JS. By definition Rust can't run it without V8. Anything routed through such a hook bypasses the static algorithm entirely. Examples in the wild: `tsx`, `ts-node`, `@swc-node/register`, `@cloudflare/unenv`, `import-meta-resolve`'s polyfills, Vitest's transform hook.
- **`require.extensions[".ts"] = ...`** monkey-patches — legacy but still load-bearing for `ts-node/register` in CJS contexts and a handful of CoffeeScript/etc. relics. Pure JS state.
- **`require.cache` mutation.** Users (and frameworks like Jest) delete entries to force re-load. The cache itself is a JS object. Rust pre-resolution can populate a parallel cache, but authoritative state is JS-side.
- **`Module.paths` / `module.paths`** — read at resolution time; user JS can mutate `process.env.NODE_PATH` mid-run (rare but legal pre-`require`).
- **`import.meta.resolve()`** inside user code — synchronous, but it's a call from a running V8 isolate. Rust can be the implementation behind it via N-API; it can't pre-compute it.
- **Dynamic `import(expr)`** with a non-literal specifier. Statically un-analyzable in the general case. Same for `require(expr)`. Pre-warming requires conservative escape analysis (Bun's `--smol` and Rolldown both punt here).
- **`vm.SourceTextModule` / `vm.SyntheticModule`** linker callbacks. Pure user JS.
- **Self-referential / monkey-patched `Module._resolveFilename`.** Legacy CJS extension point still in active use by Yarn PnP (`.pnp.cjs` registers a `_resolveFilename` override), Sentry, the Next.js dev server, OpenTelemetry instrumentation, etc. Once one of these runs, the static algorithm is no longer authoritative.

## What the Rust ecosystem already gives us

Survey, ordered by relevance to Nub:

### `oxc_resolver` (oxc-project)

Most production-ready candidate. Powering Rolldown, Rspack, parts of Vite (per the Vite/Rolldown blog), Knip, swc-node, Nova ([oxc.rs benchmarks](https://oxc.rs/docs/guide/benchmarks)). Implements ESM + CJS algorithms, `exports` / `imports` with conditions, `mainFields`, `browser` field, tsconfig `paths` (with `extends` and `references`), Yarn PnP, symlink toggle. Active — ~100 releases; current open issues are tsconfig-alignment edge cases (#1086 tsconfig project-references priority, #1075 `rootDirs` under `extends`, #1115 regex restrictions). **Explicitly unimplemented** per its README: `descriptionFiles`, `cachePredicate`, `cacheWithContext`, plugin system, unsafe caching. None of those are needed for Nub-as-runtime.

Compat caveat: it ports `enhanced-resolve`'s behavior, not Node's directly. Most of the time these agree; the disagreements live in edge cases around `mainFields` precedence and `browser` field remapping. For Nub's "runtime, not bundler" stance, the relevant fields are `exports` / `imports` / `main` / `type` — these are well-covered.

### `enhanced-resolve` (webpack, JS — reference only)

Original. Authoritative for the webpack-flavored algorithm; Node's own algorithm is the upstream. Worth diffing against when oxc shows a deviation.

### `node-resolve` crate

Older, less maintained, narrower than oxc_resolver. Skip.

### swc's `swc_ecma_loader`

Bundled with swc. Per [`module-resolution.md`](module-resolution.md#non-goals) ("not tracked against Node's resolver and has its own quirks — don't reuse it"). Skip.

### Bun's resolver (Zig)

Instructive parallel. Lives at `bun/src/resolver/resolver.rs` (~6.6k lines) and `package_json.rs` (~3.3k lines). Bun resolves *everything* in Zig before JSC ever sees it — the entire dep graph of the entry point is walked, parsed, cached, and handed to JSC as pre-resolved native pointers. This is feasible because Bun owns the engine integration end-to-end; the resolver and the module loader share data structures. We can't replicate that depth — we hand resolved paths to Node's loader, which then redoes some work — but we can match Bun at the per-import fs cost level (per [`module-resolution.md`](module-resolution.md#differential-analysis-vs-bun)).

### `@vercel/nft` (Node File Trace)

Static-analysis tool: traces all files a Node app *might* load. Useful as a reference for "what static pre-resolution can catch" (answer: a lot, including conservative dynamic `require()` heuristics). It explicitly tolerates being wrong — it overshoots, because it's used for deployment bundling. Not directly reusable as a runtime resolver but the heuristics (e.g. `require(\`./${x}\`)` expanded to `require('./*')`) inform the "transitive prewarm" design below.

### Turbopack's resolver

Internal to turbo-tooling. Not separately published. Architecturally similar to oxc_resolver. Skip.

## Is there a Node-resolution conformance suite to run them against?

**Not really, no.** Node's own ESM/CJS resolution coverage lives inside its `test/es-module/` and `test/parallel/test-module-*` trees and is glued to the rest of Node's test harness — extracting it as a standalone conformance suite isn't done. `oxc_resolver` ports tests from `enhanced-resolve`, `tsconfig-paths`, and `parcel-resolver`; those are the de facto standard but they're *not* Node's tests. There's been periodic discussion (per various `nodejs/loaders` threads) about extracting a shared spec-conformance suite; nothing has shipped. **Practical answer for Nub:** stand up a small differential harness that runs `node --print "require.resolve('x')"` and our Rust resolver against the same specifier-set across a curated fixture set (express, next, vite, nestjs, tRPC, prisma, drizzle, vitest, the pnpm/yarn/npm install shapes of each). That's what oxc-project does and it's the only honest baseline.

## The specific case: `nub run hello.js`

The user's question, concretized. What happens at each layer?

### Resolving the entry point itself

`hello.js` (or `.ts`, `.mjs`, etc.) — trivially Rust-resolvable. Stat + extension family probe + nearest-`package.json` for `type`. This is happening *before* V8 boots regardless; Node itself does this in C++ before its JS bootstrap. We just do it earlier.

### Resolving the entry point's immediate imports

`require("./foo")` or `import "./foo.js"` from inside `hello.js`. **Resolvable in Rust pre-V8 with high confidence**, by doing one extra step: parse the entry with `oxc_parser` or `es-module-lexer-rs`, extract its static import list, run each through `oxc_resolver`. Cost: one parse pass + N stats. For a typical entry: ~1–5 ms.

What this *gives* us:
- Pre-warm the on-disk resolution cache for the imports we know will be hit.
- Pre-warm the source-file cache (we already need to read these for transform; doing it during resolution piggybacks the page cache).
- Pre-detect ESM vs CJS so we can dodge Node's syntax-detection cost.

What it *does not* give us:
- The ability to skip Node's own resolver. Node will still call its loader hooks for each import. Our gain is amortizing fs cost, not removing dispatch.

### Resolving the transitive graph

Same trick recursively. Stops being a clear win past depth ~2–3 because:
1. Dynamic `import(x)` with non-literal `x` short-circuits the analysis.
2. Some packages use `require()` in init code (e.g. `dotenv` reads files at require-time), and we don't want to chase side effects.
3. Time budget — past a few ms we're spending more than we save.

`@vercel/nft`'s heuristics suggest depth ~3 + conservative dynamic expansion captures ~85% of the eventual graph. **Recommendation:** prewarm depth 1 by default, depth 3 behind `--prewarm=deep`, nothing past that.

### Sharp edge: any of those packages has a `_resolveFilename`
**override or a `module.register` hook.** If `hello.js` does `require('ts-node/register')` on its first line, the entry-point prewarm of subsequent imports may produce a *different* result than Node will. Mitigation: cache by specifier+parent, but mark results as "pre-V8 prewarm" and let the in-process JS hook (which sees the actual Node state) override on miss/disagreement. Don't return pre-V8 results back into Node as authoritative — feed them through the hook so registered customizations win. This is the same trust-contract concern as the [`augmentation-layers.md`](augmentation-layers.md) bundle-then-exec sharp edge.

## Exports field: static or not?

Mostly static. The condition set is process-fixed:
- `node` (always present in a Node-compat runtime)
- `import` vs `require` (per-call-site, determined statically by the calling format)
- `default` (fallback)
- `--conditions=foo` adds to the set; static at boot.
- `node-addons`, `module-sync`, etc. — all process-flag-driven, static.

**Pathological cases that exist but are rare:**
- A package that gates `exports` on a custom condition the user forgot to declare (`"my-bundler"`), then expects a tool's registered hook to inject it. Bundler-specific, doesn't fire in runtime usage.
- `imports` subpath aliases (`#internal/foo`) — these are *just as statically resolvable* as `exports`. Per [nodejs.org/api/packages.html#subpath-imports](https://nodejs.org/api/packages.html#subpath-imports). `oxc_resolver` supports them.
- Wildcard `exports` patterns with `*` — supported by Node and oxc_resolver per the spec's `PATTERN_KEY_COMPARE` ordering.

The one place the static algorithm is genuinely insufficient is **Yarn PnP** (`.pnp.cjs` is itself a registered resolver), which is JS. `oxc_resolver` handles the lookup table directly without running the .pnp.cjs, which is a clean shortcut Nub can adopt.

## pnpm: the symlink question

The standard pnpm layout: `node_modules/foo` is a symlink to `node_modules/.pnpm/foo@1.0.0/node_modules/foo`. When `foo` does `require('bar')`, Node by default `realpath`s `foo` first, so the search starts in `.pnpm/foo@1.0.0/node_modules/` — which is where pnpm placed `bar`. **Resolution Just Works** as long as the resolver follows symlinks (i.e., doesn't pass `--preserve-symlinks`).

The trap: `--preserve-symlinks` (per `pnpm/pnpm#244`, `pnpm/pnpm#496`) breaks the entire pnpm layout because the node_modules walk-up from the symlinked location can't see the hoisted `.pnpm/` peers. **Nub must default to following symlinks**; `oxc_resolver`'s `symlinks: true` default matches Node's default and is correct for pnpm.

Per [`pnpm-specific-behavior.md`](pnpm-specific-behavior.md) §1.1 (nodeLinker `isolated` / `hoisted` / `pnp`): all three layouts resolve correctly *as long as symlinks are followed*. No pnpm-specific code path needed in the resolver — just the right default.

Subtle case: a workspace package whose `package.json` lives at the symlink target (`.pnpm/foo@1.0.0/node_modules/foo/package.json`) but whose nearest-scope walk from a parent in the consuming app should resolve via the symlinked path. `oxc_resolver` does this correctly; verify in the differential harness on a pnpm-shaped fixture.

## tsconfig `paths`

Covered in depth in [`tsconfig-paths.md`](tsconfig-paths.md). Short version: `oxc_resolver` handles it (including `extends` and `references`, with one known bug around `rootDirs` normalization under `extends` — issue #1075). Pure-Rust, no JS needed. The cost is upfront tsconfig-load + a small per-resolve trie lookup.

## Recommendation for Nub

Concrete proposal, with cost trade-offs called out.

### Layer 1: Entry-point resolution in Rust pre-V8. **Do it.**

Cost: zero compat surface — Node does this in C++ anyway, we're just moving it earlier in the same process.

Win: tiny (~100 μs saved), but it unblocks layer 2.

### Layer 2: Entry-point immediate-import prewarm in Rust pre-V8. **Do it.**

Mechanism: parse entry with `oxc_parser`, extract static imports, run each through `oxc_resolver`, populate the persistent resolution cache and the source-file cache.

Cost:
- ~1–5 ms parse + N resolves on cold start (subset of the resolution work Node will do anyway in JS — strict win as long as N ≤ ~50).
- Compat surface: zero, as long as we *don't* return these results back to Node as authoritative. They're cache prewarm only; the JS hook re-asks and (with cache hit) gets the same answer.

Win: cold-start budget claws back the ~1.25 ms of "bare specifier via Node's JS resolver" cost flagged in [`module-resolution.md`](module-resolution.md#what-nub-pays-per-import).

### Layer 3: Transitive prewarm depth 2–3. **Behind a flag.**

`--prewarm=deep` for production / `nub build`-style flows. Off by default for `nub run`.

Cost: 5–20 ms parse + resolve; compat risk if we follow into a package that should have been transformed first by a registered hook. Mitigate by treating prewarm results as cache-only and invalidating on hook registration.

Win: in heavy import graphs (think NestJS), can prewarm 100+ resolutions before V8's loader runs.

### Layer 4: In-process resolution in Rust via N-API. **Do it,
**but as a delegate of the JS hook**, not as a replacement.

This is already the [`module-resolution.md`](module-resolution.md) plan. The JS hook calls into Rust (`oxc_resolver` as the engine for the bare-specifier branch); Rust returns a result; JS hands it to Node with `shortCircuit: true`. Critically: when other hooks are registered, they get to run first (or after, per registration order). We don't *replace* Node's resolver — we feed it answers.

Cost: per-import N-API overhead (~230 ns, see [`rust-from-js.md`](rust-from-js.md)). Compat surface: we now depend on `oxc_resolver`'s Node-compat, but the JS hook still falls back to `nextResolve` for anything Rust returns `null` on. This is the right escape hatch.

Win: Bun-parity per-import resolution speed once the cache is warm.

### Layer 5: Full Node-loader replacement in Rust. **Don't.**

This means: Rust serves all resolution requests authoritatively, bypassing Node's `lib/internal/modules/esm/` entirely. Bun does this. We can't, because:
- `module.registerHooks()` and `require.extensions` are public APIs we have to honor.
- `require.cache` is observable and mutable.
- Yarn PnP's `_resolveFilename` injection happens in JS and we'd have to either re-execute `.pnp.cjs` in Rust (absurd) or honor the JS-side override (defeats the point).
- Compat surface: the algorithm drifts. Every release of Node, a PR description has resolution behavior tweaks. Mirroring is a maintenance treadmill.

Per the reversibility filter: this one is irreversible — every layer past 4 burns compat trust that we can't get back.

## Edge cases that will bite (be honest)

1. **`ts-node/register` and friends.** They install `require.extensions` shims that bypass our static analysis. Anything we prewarmed for a `.ts` file under their watch is suspect. Detect their presence (parse for `require('ts-node/register')` literally in the entry) and skip prewarm.
2. **Yarn PnP without `.pnp.cjs` parse-on-startup.** `oxc_resolver` handles `.pnp.data.json` directly; older PnP versions only ship the `.cjs`. We'll need a fallback that defers to Node.
3. **Workspaces with circular symlinks.** pnpm in some monorepo shapes creates cycles. `realpath` resolves them but be ready for `EMLINK` / `ELOOP` from the syscall.
4. **`exports` with conditions we don't know about.** A package ships `"exports": { "rsc": "./rsc.js", "default": "./node.js" }` under Next.js's RSC condition. We don't have `"rsc"` in our condition set; we'll resolve to `"default"`. Correct behavior, but worth documenting that the runtime condition set is `["node", "import"|"require", "default"]` plus any `--conditions=`.
5. **`package.json` `"type": "module"` retroactively changing a `.js` file's format.** Our cache key must include the nearest-scope `package.json` mtime/inode, not just the file path.
6. **`process.dlopen` and `.node` files.** Static algorithm resolves them; Node's native-addon loader runs them. No conflict but worth noting.
7. **`pnpm patch` and `patch-package`.** Patched files have different content than what's in the registry but the resolution shape is identical. Our cache invalidation has to track `node_modules/.pnpm/` mtime as a project signature input.
8. **`module-sync` condition** (Node v22.10+, see Node docs). Used for sync ESM/CJS interop. Static; `oxc_resolver` supports custom conditions, so we pass it through.

## Sources

- Node ESM resolution algorithm: [nodejs.org/api/esm.html#resolution-algorithm](https://nodejs.org/api/esm.html#resolution-algorithm).
- Node module API (`module.register`, `module.registerHooks`): [nodejs.org/api/module.html](https://nodejs.org/api/module.html).
- `module.registerHooks()` tracking: [nodejs/node#56241](https://github.com/nodejs/node/issues/56241).
- Node ESM resolver source: `lib/internal/modules/esm/resolve.js` in nodejs/node.
- Node CJS loader source: `lib/internal/modules/cjs/loader.js`.
- `oxc_resolver` repo: [github.com/oxc-project/oxc-resolver](https://github.com/oxc-project/oxc-resolver).
- `oxc_resolver` open compat issues sampled: #1115 (regex restrictions), #1086 (tsconfig references priority), #1075 (`rootDirs` under `extends`), #1011 (find_tsconfig errors), #852 (canonicalize cache).
- Bun resolver: `bun/src/resolver/resolver.rs` (~6.6k LOC), `bun/src/resolver/package_json.rs` (~3.3k LOC); exports resolution at `package_json.rs:2377–2502`.
- `@vercel/nft`: [github.com/vercel/nft](https://github.com/vercel/nft).
- pnpm symlinked layout: [pnpm.io/symlinked-node-modules-structure](https://pnpm.io/symlinked-node-modules-structure).
- pnpm `--preserve-symlinks` history: [pnpm/pnpm#244](https://github.com/pnpm/pnpm/issues/244), [pnpm/pnpm#496](https://github.com/pnpm/pnpm/issues/496).
- WHATWG-ish subpath imports spec: [nodejs.org/api/packages.html#subpath-imports](https://nodejs.org/api/packages.html#subpath-imports).
- oxc benchmarks: [oxc.rs/docs/guide/benchmarks](https://oxc.rs/docs/guide/benchmarks).

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Links to internal planning documents were removed and reference-checkout paths rewritten; findings, tables and measured values are unchanged.

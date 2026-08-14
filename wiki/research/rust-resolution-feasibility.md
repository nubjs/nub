# Research: how much of Node's resolution can Nub do in Rust pre-V8?

**Status:** v1, 2026-05-16. Companion to [[research/module-resolution]], which covers the TS-extensionless slice in the JS hook layer; this doc looks at the entire algorithm.

**Builds on:** [[research/module-resolution]], [[research/tsconfig-paths]], [[research/pnpm-specific-behavior]], [[research/cold-start]]. **Sibling:** [[research/augmentation-layers]], on where in the lifecycle a Rust resolver could be wired.

## Question

Nub owns the process from `nub run hello.js` through V8 boot, and Node gets no shot at resolution until its ESM/CJS loaders are alive in JS.

**What fraction of `require.resolve` / ESM `PACKAGE_RESOLVE` can Nub answer from Rust against a cold V8?** Which parts need a running JS engine?

Every layer pushed pre-V8 is a cold-start win — per [[research/cold-start]], Node spends ~15–27 ms of warm start before the user file is touched — and also a compat-surface liability, because Node's resolver changes, exports semantics drift, and a Rust mirror has to keep up.

## TL;DR

About 85% of Node's resolution algorithm is pure filesystem logic that the Rust ecosystem already implements. The rest needs live V8 state, which puts the practical boundary at the entry point plus a cache prewarm pass.

- ~85% of the resolution algorithm is purely declarative and filesystem-driven. The Rust ecosystem (`oxc_resolver`, `enhanced-resolve` port, Bun's resolver) already implements it.
- The ~15% that *requires* JS-runtime state: registered loader hooks (`module.registerHooks` / `module.register`), `require.cache` mutation, `require.extensions` monkey-patches, runtime-mutable `--conditions` (these are mostly static but observable via JS), `vm.Module` synthetic modules, and `import.meta.resolve` from inside user code.
- The split: **Nub can resolve the entry point and all reachable static imports in Rust with high confidence; it cannot resolve dynamic `import()` of computed specifiers, anything behind a user-registered loader hook, or anything that depends on side effects of already-executed JS.**
- Recommendation: ship `oxc_resolver` as the pre-V8 resolver for the entry point plus a cache prewarm pass, and defer in-process resolution to the JS hook, which itself delegates to a shared Rust core via N-API. Do not attempt a full Rust replacement of Node's in-process resolver — the compat surface is hostile and the win is marginal past the entry point.

## Where the algorithm splits: pure vs JS-dependent

Annotated against `lib/internal/modules/esm/resolve.js` (per the Node docs and the WHATWG-style algorithm at [nodejs.org/api/esm.html#resolution-algorithm](https://nodejs.org/api/esm.html#resolution-algorithm)) and `lib/internal/modules/cjs/loader.js`.

### Pure / filesystem-only (trivially Rust-able)

These compute a result from `(specifier, parentURL, cwd, fs, set-of-conditions)` with no observation of JS-runtime state past process start:

- **CJS candidate probing.** Walk `node_modules` up the parent chain, read each candidate's `package.json` `main` / `exports`, and probe extensions in `[.js, .json, .node]` order — the classic `require.resolve` shape. Implemented end-to-end in `oxc_resolver`, `enhanced-resolve`, the `node-resolve` crate, swc's loader, Bun's resolver, and `@vercel/nft`.
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

Three cases Rust can still answer, each needing either a parse pass or a read of Nub's own command line rather than V8 state.

- **`ESM_FILE_FORMAT` for `.js`** when the nearest `package.json` has no `"type"` and Node has to syntax-detect. The detection — looking for `import` / `export` / top-level `await` / `import.meta` — is a parse pass, doable in Rust with oxc or `es-module-lexer-rs`. Bun does this in Zig.
- **`--conditions=` and `--experimental-network-imports` style flags.** Static after Node boot. Rust can read its own CLI.
- **`require.resolve(specifier, { paths })`** — same algorithm, different starting directory. Rust-able.

### Genuinely JS-runtime-dependent (must defer)

These read or mutate state that lives inside V8:

- **User-registered loader hooks** via `module.registerHooks()` (sync, on-thread, added in v23 — see [nodejs/node#56241](https://github.com/nodejs/node/issues/56241)) or async off-thread `module.register()`. The hook is user JS, so Rust cannot run it without V8, and anything routed through such a hook bypasses the static algorithm entirely. In the wild: `tsx`, `ts-node`, `@swc-node/register`, `@cloudflare/unenv`, `import-meta-resolve`'s polyfills, Vitest's transform hook.
- **`require.extensions[".ts"] = ...`** monkey-patches — legacy, but still load-bearing for `ts-node/register` in CJS contexts and a handful of CoffeeScript-era relics. Pure JS state.
- **`require.cache` mutation.** Users and frameworks like Jest delete entries to force a re-load. The cache is a JS object; Rust pre-resolution can populate a parallel cache, but the authoritative state is JS-side.
- **`Module.paths` / `module.paths`** — read at resolution time, and user JS can mutate `process.env.NODE_PATH` mid-run (rare but legal pre-`require`).
- **`import.meta.resolve()`** inside user code — synchronous, but a call from a running V8 isolate. Rust can be the implementation behind it via N-API; it cannot pre-compute it.
- **Dynamic `import(expr)`** with a non-literal specifier, and likewise `require(expr)` — statically un-analyzable in the general case. Prewarming requires conservative escape analysis, which Bun's `--smol` and Rolldown both punt on.
- **`vm.SourceTextModule` / `vm.SyntheticModule`** linker callbacks. Pure user JS.
- **Self-referential / monkey-patched `Module._resolveFilename`.** Legacy CJS extension point still in active use by Yarn PnP (`.pnp.cjs` registers a `_resolveFilename` override), Sentry, the Next.js dev server, and OpenTelemetry instrumentation. Once one of these runs, the static algorithm is no longer authoritative.

## What the Rust ecosystem already gives us

Ordered by relevance to Nub.

### `oxc_resolver` (oxc-project)

The most production-ready candidate, powering Rolldown, Rspack, parts of Vite (per the Vite/Rolldown blog), Knip, swc-node, and Nova ([oxc.rs benchmarks](https://oxc.rs/docs/guide/benchmarks)).

It implements the ESM and CJS algorithms, `exports` / `imports` with conditions, `mainFields`, the `browser` field, tsconfig `paths` (with `extends` and `references`), Yarn PnP, and the symlink toggle.

Active at ~100 releases; the current open issues are tsconfig-alignment edge cases (#1086 tsconfig project-references priority, #1075 `rootDirs` under `extends`, #1115 regex restrictions). Explicitly unimplemented per its README: `descriptionFiles`, `cachePredicate`, `cacheWithContext`, the plugin system, and unsafe caching — none of which Nub-as-runtime needs.

Compat caveat: it ports `enhanced-resolve`'s behavior, not Node's directly. These mostly agree; the disagreements live in edge cases around `mainFields` precedence and `browser`-field remapping. For a runtime rather than a bundler, the relevant fields are `exports` / `imports` / `main` / `type`, which are well covered.

### `enhanced-resolve` (webpack, JS — reference only)

Authoritative for the webpack-flavored algorithm, with Node's own algorithm upstream of it. Worth diffing against when oxc shows a deviation.

### `node-resolve` crate

Older, less maintained, narrower than `oxc_resolver`. Skip.

### swc's `swc_ecma_loader`

Bundled with swc. Per [[research/module-resolution#Non-goals|`module-resolution.md`]], it is not tracked against Node's resolver and has its own quirks. Skip.

### Bun's resolver (Zig)

An instructive parallel, at `bun/src/resolver/resolver.rs` (~6.6k lines) and `package_json.rs` (~3.3k lines).

Bun resolves everything in Zig before JSC sees it: the entire dep graph of the entry point is walked, parsed, cached, and handed to JSC as pre-resolved native pointers. That works because Bun owns the engine integration end-to-end and the resolver and module loader share data structures. Nub cannot replicate that depth — it hands resolved paths to Node's loader, which redoes some work — but it can match Bun at the per-import fs cost level (per [[research/module-resolution#Differential analysis vs Bun|`module-resolution.md`]]).

### `@vercel/nft` (Node File Trace)

A static-analysis tool that traces every file a Node app might load, and a useful reference for what static pre-resolution can catch — a lot, including conservative dynamic `require()` heuristics.

It tolerates being wrong and overshoots, because it targets deployment bundling. Not reusable as a runtime resolver, but its heuristics (`require(\`./${x}\`)` expanded to `require('./*')`) inform the transitive-prewarm design below.

### Turbopack's resolver

Internal to turbo-tooling, not separately published, architecturally similar to `oxc_resolver`. Skip.

## Is there a Node-resolution conformance suite to run them against?

Not really. Node's own ESM/CJS resolution coverage lives in its `test/es-module/` and `test/parallel/test-module-*` trees, glued to the rest of Node's test harness, and nobody has extracted it as a standalone conformance suite.

`oxc_resolver` ports tests from `enhanced-resolve`, `tsconfig-paths`, and `parcel-resolver`, which are the de facto standard but are not Node's tests. Extracting a shared spec-conformance suite has been discussed periodically in `nodejs/loaders` threads; nothing has shipped.

The practical answer for Nub: a small differential harness running `node --print "require.resolve('x')"` and the Rust resolver over the same specifier set across a curated fixture set (express, next, vite, nestjs, tRPC, prisma, drizzle, vitest, in the pnpm/yarn/npm install shapes of each). That is what oxc-project does, and it is the only honest baseline.

## The specific case: `nub run hello.js`

Three scopes, in increasing risk order: the entry file, its immediate static imports, and the transitive graph. The first two are safe pre-V8 work; the third stops paying off past depth 3.

### Resolving the entry point itself

Trivially Rust-resolvable for `hello.js` or any `.ts` / `.mjs` variant: stat, extension-family probe, nearest-`package.json` for `type`.

This happens before V8 boots regardless — Node does it in C++ before its JS bootstrap — so Nub only does it earlier.

### Resolving the entry point's immediate imports

Static imports written literally in the entry file are resolvable before V8 boots; only computed specifiers escape.

For a `require("./foo")` or `import "./foo.js"` inside `hello.js`, resolvable in Rust pre-V8 with high confidence via one extra step: parse the entry with `oxc_parser` or `es-module-lexer-rs`, extract its static import list, and run each through `oxc_resolver`. Cost is one parse pass plus N stats, ~1–5 ms for a typical entry.

What it gives:
- A prewarmed on-disk resolution cache for the imports that will be hit.
- A prewarmed source-file cache — those files have to be read for transform anyway, so reading them during resolution piggybacks the page cache.
- Pre-detected ESM vs CJS, dodging Node's syntax-detection cost.

What it does not give: the ability to skip Node's own resolver. Node still calls its loader hooks for each import, so the gain is amortized fs cost, not removed dispatch.

### Resolving the transitive graph

The same trick recursively, which stops being a clear win past depth ~2–3 because:

1. Dynamic `import(x)` with non-literal `x` short-circuits the analysis.
2. Some packages `require()` in init code (`dotenv` reads files at require-time), and chasing side effects is not wanted.
3. Past a few ms the time budget spends more than it saves.

`@vercel/nft`'s heuristics suggest depth ~3 plus conservative dynamic expansion captures ~85% of the eventual graph. **Recommendation:** prewarm depth 1 by default, depth 3 behind `--prewarm=deep`, nothing past that.

### Sharp edge: a prewarmed package with a `_resolveFilename` override or a `module.register` hook

If `hello.js` calls `require('ts-node/register')` on its first line, the entry-point prewarm of subsequent imports may produce a different result than Node will.

Mitigation: cache by specifier+parent, mark results as pre-V8 prewarm, and let the in-process JS hook — which sees the actual Node state — override on miss or disagreement. Never return pre-V8 results into Node as authoritative; feed them through the hook so registered customizations win. This is the same trust-contract concern as the [[research/augmentation-layers]] bundle-then-exec sharp edge.

## Exports field: static or not?

Mostly static. The condition set is process-fixed:
- `node` (always present in a Node-compat runtime)
- `import` vs `require` (per-call-site, determined statically by the calling format)
- `default` (fallback)
- `--conditions=foo` adds to the set; static at boot.
- `node-addons`, `module-sync`, etc. — all process-flag-driven, static.

**Pathological cases that exist but are rare:**
- A package that gates `exports` on a custom condition the user forgot to declare (`"my-bundler"`), then expects a tool's registered hook to inject it. Bundler-specific; it does not fire in runtime usage.
- `imports` subpath aliases (`#internal/foo`), which are as statically resolvable as `exports` per [nodejs.org/api/packages.html#subpath-imports](https://nodejs.org/api/packages.html#subpath-imports). `oxc_resolver` supports them.
- Wildcard `exports` patterns with `*` — supported by Node and `oxc_resolver` per the spec's `PATTERN_KEY_COMPARE` ordering.

The one place the static algorithm is insufficient is Yarn PnP, where `.pnp.cjs` is itself a registered resolver written in JS. `oxc_resolver` reads the lookup table directly without running `.pnp.cjs`, a clean shortcut Nub can adopt.

## pnpm: the symlink question

In the standard pnpm layout, `node_modules/foo` is a symlink to `node_modules/.pnpm/foo@1.0.0/node_modules/foo`.

When `foo` does `require('bar')`, Node `realpath`s `foo` first by default, so the search starts in `.pnpm/foo@1.0.0/node_modules/`, where pnpm placed `bar`. Resolution works as long as the resolver follows symlinks — that is, as long as `--preserve-symlinks` is not passed.

The trap is `--preserve-symlinks` (per `pnpm/pnpm#244`, `pnpm/pnpm#496`), which breaks the entire pnpm layout because the `node_modules` walk-up from the symlinked location cannot see the hoisted `.pnpm/` peers. Nub must default to following symlinks; `oxc_resolver`'s `symlinks: true` default matches Node's and is correct for pnpm.

Per [[research/pnpm-specific-behavior]] §1.1, all three nodeLinker layouts (`isolated` / `hoisted` / `pnp`) resolve correctly as long as symlinks are followed. The resolver needs no pnpm-specific code path, only the right default.

Subtle case: a workspace package whose `package.json` lives at the symlink target (`.pnpm/foo@1.0.0/node_modules/foo/package.json`) but whose nearest-scope walk from a parent in the consuming app should resolve via the symlinked path. `oxc_resolver` does this correctly; verify it in the differential harness on a pnpm-shaped fixture.

## tsconfig `paths`

Covered in depth in [[research/tsconfig-paths]]. `oxc_resolver` handles it, including `extends` and `references`, with one known bug around `rootDirs` normalization under `extends` (issue #1075).

Pure Rust, no JS needed; the cost is an upfront tsconfig load plus a small per-resolve trie lookup.

## Recommendation for Nub

Five layers, ordered by how deep into Node's loader each one reaches. Layers 1 and 2 ship, layer 3 goes behind a flag, layer 4 delegates from the JS hook, and layer 5 is refused.

### Layer 1: Entry-point resolution in Rust pre-V8. **Do it.**

Cost: zero compat surface — Node does this in C++ anyway, so it only moves earlier in the same process.

Win: tiny (~100 μs saved), but it unblocks layer 2.

### Layer 2: Entry-point immediate-import prewarm in Rust pre-V8. **Do it.**

Mechanism: parse the entry with `oxc_parser`, extract static imports, run each through `oxc_resolver`, and populate the persistent resolution cache and the source-file cache.

Cost:
- ~1–5 ms parse plus N resolves on cold start — a subset of the resolution work Node does in JS anyway, so a strict win as long as N ≤ ~50.
- Zero compat surface, as long as the results are never returned to Node as authoritative. They are cache prewarm only; the JS hook re-asks and, on a cache hit, gets the same answer.

Win: claws back the ~1.25 ms of bare-specifier-via-Node's-JS-resolver cost flagged in [[research/module-resolution#Differential analysis vs Bun#What Nub pays per import|`module-resolution.md`]].

### Layer 3: Transitive prewarm depth 2–3. **Behind a flag.**

`--prewarm=deep` for production flows, off by default for `nub run`.

Cost: 5–20 ms parse plus resolve, and compat risk if the walk follows into a package that a registered hook should have transformed first. Mitigate by treating prewarm results as cache-only and invalidating on hook registration.

Win: in heavy import graphs such as NestJS, 100+ resolutions prewarmed before V8's loader runs.

### Layer 4: In-process resolution in Rust via N-API. **Do it, as a delegate of the JS hook rather than a replacement.**

This is already the [[research/module-resolution]] plan: the JS hook calls into Rust with `oxc_resolver` as the engine for the bare-specifier branch, Rust returns a result, and JS hands it to Node with `shortCircuit: true`.

When other hooks are registered they still run, before or after per registration order. Node's resolver is fed answers, not replaced.

Cost: per-import N-API overhead (~230 ns, see [[research/rust-from-js]]), plus a dependency on `oxc_resolver`'s Node compat — though the JS hook still falls back to `nextResolve` for anything Rust returns `null` on, which is the right escape hatch.

Win: Bun-parity per-import resolution speed once the cache is warm.

### Layer 5: Full Node-loader replacement in Rust. **Don't.**

Rust would serve all resolution requests authoritatively, bypassing Node's `lib/internal/modules/esm/` entirely, as Bun does. Nub cannot, because:

- `module.registerHooks()` and `require.extensions` are public APIs Nub has to honor.
- `require.cache` is observable and mutable.
- Yarn PnP's `_resolveFilename` injection happens in JS, leaving only re-executing `.pnp.cjs` in Rust (absurd) or honoring the JS-side override (which defeats the point).
- The algorithm drifts. Node ships resolution-behavior tweaks every release, so mirroring is a maintenance treadmill.

Per the reversibility filter this one is irreversible: every layer past 4 burns compat trust that cannot be regained.

## Edge cases that will bite

Eight cases where the static algorithm is right but the state around it is not — mostly cache keys, and JS-side resolver overrides that land after the prewarm.

1. **`ts-node/register` and friends.** They install `require.extensions` shims that bypass static analysis, so anything prewarmed for a `.ts` file under their watch is suspect. Detect them by parsing for a literal `require('ts-node/register')` in the entry and skip prewarm.
2. **Yarn PnP without `.pnp.cjs` parse-on-startup.** `oxc_resolver` handles `.pnp.data.json` directly, but older PnP versions ship only the `.cjs`, so a fallback that defers to Node is needed.
3. **Workspaces with circular symlinks.** pnpm creates cycles in some monorepo shapes. `realpath` resolves them, but be ready for `EMLINK` / `ELOOP` from the syscall.
4. **`exports` with unknown conditions.** A package shipping `"exports": { "rsc": "./rsc.js", "default": "./node.js" }` under Next.js's RSC condition resolves to `"default"` without `"rsc"` in the condition set. That is correct behavior, but document that the runtime condition set is `["node", "import"|"require", "default"]` plus any `--conditions=`.
5. **`package.json` `"type": "module"` retroactively changing a `.js` file's format.** The cache key must include the nearest-scope `package.json` mtime/inode, not just the file path.
6. **`process.dlopen` and `.node` files.** The static algorithm resolves them and Node's native-addon loader runs them. No conflict.
7. **`pnpm patch` and `patch-package`.** Patched files differ in content from the registry copy but have an identical resolution shape, so cache invalidation has to take `node_modules/.pnpm/` mtime as a project-signature input.
8. **`module-sync` condition** (Node v22.10+), used for sync ESM/CJS interop. Static, and `oxc_resolver` supports custom conditions, so it passes through.

## Sources

Node's own resolution algorithm and loader sources, the `oxc_resolver` repo with the compat issues sampled from it, Bun's Zig resolver, and the pnpm symlink-layout history.

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

Every revision to this document, with the date and what changed.

- 2026-07-30 — Migrated from the internal research corpus. Links to internal planning documents were removed and reference-checkout paths rewritten; findings, tables and measured values are unchanged.

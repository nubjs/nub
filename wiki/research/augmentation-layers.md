# Research: augmentation layers (bundler vs loader hooks) and Rust-from-JS interop

**Status:** v2, 2026-05-16. Initial claims verified the same day, then revised after reading the tsx source directly. Informs the pre-processing model, the `nub build` bundler, and package replacement by name.

## Question

Where in the pipeline does Nub augment Node, and how do augmentations get implemented? Two sub-questions:

1. **Augmentation surface.** Should Nub pre-process user code at the *bundler* layer (statically link Rolldown, bundle the program, pass the artifact to Node) or at the *loader-hook* layer (`module.registerHooks()` intercepts each module as Node imports it, transformed per-file)? The plan currently commits to the latter; this doc revisits that choice.
2. **Implementation language for new APIs.** If Nub adds globals, built-in modules, or new specifier namespaces, can their implementations live in Rust and still be callable from JS at reasonable overhead — or does anything in JS-land have to be written in JS?

## Premise check: what do Bun and tsx actually do?

The motivating intuition — "Bun and tsx bundle with esbuild or their own bundler before passing the result to Node" — is the wrong mental model, and "tsx just transpiles per file" is incomplete. The accurate picture, from the tsx source:

- **tsx ships two hooks, not one.** A resolve hook (744 lines in `src/esm/hook/resolve.ts`) and a load hook (420 lines in `src/esm/hook/load.ts`), plus a parallel CJS path (`src/cjs/api/module-resolve-filename/`, `src/cjs/api/module-extensions.ts`). The resolve hook does the heavy lifting — extension probing for `.ts/.tsx/.jsx` candidates, directory-import → `index.{ts,tsx,jsx,js}` resolution, tsconfig `paths` alias rewriting, `.js → .ts` swap in exports/imports maps. The load hook then calls esbuild's `transformSync` per file, and cached output is content-hashed. Architectural ancestor: `@esbuild-kit/{esm,cjs}-loader`. Detailed breakdown: [[research/tsx-architecture]].
- The implication for Nub: **resolution and transform are both first-class augmentation surfaces inside the loader-hook layer** — interception happens *before* Node's default resolver runs (resolve hook) and *between resolution and parse* (load hook). esbuild's `transform` API has no plugin system, but the resolve hook is itself the plugin point.
- **Bun** transpiles per module on first import, inside its runtime — the transpiler is invoked from JavaScriptCore's module-loader callback, not as a pre-pass. Bun's docs state "every file is transpiled on the fly … before being executed." Bun's *bundler* (`bun build` / `Bun.build`) is a separate code path for deploy artifacts, not for running scripts.
- **esbuild-runner** is the one widely-known bundle-before-exec precedent. The community moved off it toward per-file tools (tsx, swc-node, ts-node) because the bundled-runtime model hits the module-identity sharp edges below.

So Nub's planned design (sync `module.registerHooks()` plus a content-addressed cache) is the same shape as both tsx and Bun. Bundle-before-exec is the road less travelled.

## Augmentation layer A: bundle-then-exec (Rolldown statically linked)

What this would build: `nub run script.ts` invokes a Rust binary that calls vendored Rolldown to produce a single bundle for the entry plus its graph, writes it to a cache, and spawns `node` on the bundled artifact.

**What it buys:**

- Virtual modules and custom specifier resolution via Rolldown plugins (`nub:foo`, `~/...`, asset imports).
- Whole-program transforms — constant folding across module boundaries, dead-code elimination of unused exports, tree-shaking before Node sees the code.
- Shared output: `nub build` and `nub run` would use one pipeline.
- Custom Rust-side plugins inside the vendored Rolldown — pure-Rust augmentation without an N-API hop per transform.

**What it costs:**

- **Module identity breaks in subtle, painful ways.** Bundling collapses N modules into one artifact, while anything resolved *outside* the bundle (Node builtins, native `.node` addons, externalized `node_modules`, dynamic `import(<computed>)`) keeps its own instance. Classes from a bundled copy of `lib-x` fail `instanceof` against the externally-resolved copy; module-level singletons diverge (the dual-package hazard); `require.cache` keys diverge, breaking HMR, mocking libraries and test isolation; `import.meta.url` becomes the bundle URL, breaking `new URL('./asset', import.meta.url)`; native addons can never be bundled. The mitigation is to mark `node_modules` external — at which point only user code is bundled, which a per-file hook does just as well, and the bundler layer stops earning its keep.
- **Cold-start cost is whole-program, not just touched files.** Per-file hooks transform `O(modules actually imported)`; a bundler always parses the full graph rooted at the entry. For a server with conditional code paths, that is wasted work.
- **Node's resolver semantics are lost** inside the bundle. Conditional exports, package exports maps, subpath imports and `node:`-prefix handling all go through Rolldown's resolver, which is rollup-compatible but not Node-identical.
- **The debugger and source-map story is more complex.** The artifact is one file with sections per source; debuggers handle it, but there is more rope.
- **Rolldown's Rust plugin trait is not a stable public API.** The team's published position is napi-rs first: the committed contract is the Rollup-compatible JS plugin interface, mirrored across napi and wasi backends. Built-in plugins live behind an internal `Plugin` trait that is explicitly unstable, so embedding Rolldown as a Rust crate and writing plugins against that trait means pinning a commit and absorbing churn. Confirmed by the rolldown maintainers.

## Augmentation layer B: per-file loader hooks (current plan)

Sync `module.registerHooks()` (Node 24.13.1+) intercepts each module as Node resolves and loads it. Rust transformers (swc, lightningcss) run via napi-rs; a content-hash disk cache short-circuits known files.

**What it buys:**

- Cold start scales with **touched** files, not repo size.
- Node's resolver is preserved — conditional exports, subpath imports, `node:*` builtins and native addons behave exactly as in stock Node. Compatibility is the trust contract, and this path defends it by default.
- Module identity is preserved end-to-end: singletons, `instanceof` across packages, `require.cache`, `import.meta.url` all native.
- A unified `require` / `import` path. The sync hooks API, unlike the older async `register()`, was designed so one resolve/load pair covers CJS and ESM, closing async `register()`'s historical CJS gaps.
- **A recent Node fix unlocks `node:*` interception via sync hooks** (commit `2d560e4` / PR #58004 — `require('node:zlib')` previously skipped the sync chain), giving a hook over the `node:*` namespace from outside the runtime. The fix shipped in the 24.x line; worth a v1 sanity check against the pinned Node floor.
- Hook code is a `--import nub-hooks.mjs` prelude — easy to reason about, easy to opt out of for debugging.

**What it costs:**

- No whole-program transforms. Cross-module dead-code elimination or constant propagation at run time is out of reach on this layer — that is `nub build`'s job.
- One hook per import, so import-heavy startup paths pay N×(hook cost). The content-addressed cache makes the per-hit cost stat-bound; a cold cache pays the full transform.
- The resolve hook can claim arbitrary `<scheme>:<rest>` specifiers (`nub:foo`), matching virtual modules in a bundler plugin, and bare specifiers and `node:*` are interceptable too — so a new specifier namespace is not a bundler-only capability.
- Hooks are **not** inherited by worker threads unless re-preloaded. The spawn pipeline controls the prelude, so this is solvable, but worth remembering when Workers land.

## Augmentation layer C: stock Node, no pre-processing (`nub run script.js`)

For JS needing no transformation, both layers above add zero overhead.

## Implementation language: how does Rust reach JS at run time?

Moved to its own write-up: [[research/rust-from-js]].

The architectural summary: N-API addons via napi-rs are the default Rust-from-JS path on stock Node, with a ~26 ns floor per trivial call and ~230 ns returning objects. **Surfaces must be coarse-grained** — one call per operation, never per token or byte. Bun-style sub-microsecond globals are unreachable without modifying the Node runtime, which is out of scope (see [[research/forking-node]]). WASM and Rust sidecars are special-case options.

## Recommendation

Hooks stay the default execution pipeline and the bundler stays off the run path. Every augmentation the bundler was proposed for is reachable through a resolve hook or an `--import` prelude instead.

1. **Keep the loader-hook layer as the default execution pipeline.** It matches what Bun and tsx do, defends Node compatibility automatically, and avoids the module-identity sharp edges of bundle-then-exec.
2. **Rolldown stays scoped to `nub build` and explicit bundle commands**, off the `nub run` hot path. The "bundler-level augmentation for free" framing over-promises: most of the wanted augmentation is reachable via resolve hooks (virtual specifiers, package replacement) or `--import` preludes (globals).
3. **New globals and built-in modules are implementable in Rust**, via a small N-API prelude addon loaded by `--import`. Keep the surface coarse-grained, with the JS side calling into Rust in chunks rather than per-byte hot loops. V8 Fast API is on the horizon but unreliable to plan around.
4. **New module specifier namespaces** — `nub:foo` and the like, currently off the table under the brand boundary — are served by the resolve hook returning a synthetic URL whose load hook returns Rust-transformed JS source, or thin JS re-exporting the N-API prelude. No bundler needed.
5. **Package replacement by name** is also a resolve-hook job: when the resolver sees `swc` / `tsx` / `lightningcss`, redirect to the built-in implementation. No bundler layer required.

## Open follow-ups

Six items the recommendation does not settle: two measurements, two design questions about the N-API surface and worker inheritance, one upstream dependency to track, and one deferral.

- **Cache-hit micro-benchmark.** Confirm the sync hook plus cache path is sub-ms per intercepted file under realistic load, measured against tsx and ts-node baselines.
- **Worker-thread hook inheritance** — the concrete UX for users spawning Workers from a Nub-run script. Likely "Nub spawns with `--import` and Workers inherit `execArgv` by default"; needs verification.
- **Interception of `node:*` via sync hooks** is fixed in Node 24.x; verify against the pinned floor (24.13.1+) and add a CI smoke test.
- **N-API surface design.** The concrete first APIs (hash, file read, Rust-side path resolution) and a written-down coarse-grained-only convention.
- **Rolldown Rust plugin trait stability.** Track whether the rolldown team publishes a stable Rust plugin contract during 2026. If it lands, `nub build`'s Rust-plugin story improves, but it does not change the runtime-pipeline call.
- **Workers, embedding API.** Out of scope today; revisit after v1 ships.

## Sources verified (2026-05-16)

The primary sources behind the claims above — tsx's and Bun's transpile models, the hooks API's documented capabilities, the N-API call-cost benchmarks, and rolldown's plugin model — each checked on the date in this heading.

- tsx architecture and esbuild `transform`-only constraint: `npmjs.com/package/tsx`, `esbuild.github.io/api/#transform`.
- Bun on-demand transpile model: `bun.com/docs/runtime/typescript`, `github.com/oven-sh/bun/blob/main/src/resolver/resolver.zig`.
- `module.registerHooks()` capabilities incl. `node:*` fix: `nodejs.org/api/module.html`, `github.com/nodejs/node/commit/2d560e42fa`, `github.com/nodejs/node/issues/56241`.
- N-API call-cost benchmarks: `github.com/Brooooooklyn/rust-to-nodejs-overhead-benchmark`, `github.com/nodejs/node/pull/21072`, `github.com/napi-rs/napi-rs/issues/1973`.
- Rolldown plugin model (napi-rs-first, Rust trait internal): `github.com/rolldown/rolldown`, `deepwiki.com/rolldown/rolldown`.
- Bundle-then-exec precedents: `github.com/folke/esbuild-runner`, `github.com/privatenumber/ts-runtime-comparison`.
- Module identity / dual-package hazard: `nodejs.org/api/packages.html#dual-package-hazard`.

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.

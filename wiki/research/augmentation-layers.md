# Research: augmentation layers (bundler vs loader hooks) and Rust-from-JS interop

**Status:** v2, 2026-05-16. Sub-agent verified initial claims same day; revised after reading the tsx source directly (`tsx/`). **Informs:** `PLAN.md` — Pre-processing model, `PLAN.md` — Bundler `nub build`, `PLAN.md` — Package replacement by name.

## Question

Where in the pipeline does Nub augment Node, and how do augmentations get implemented?

Two distinct sub-questions:

1. **Augmentation surface.** Should Nub pre-process user code at the *bundler* layer (statically link Rolldown, bundle the program, pass the artifact to Node) or at the *loader-hook* layer (`module.registerHooks()` intercepts each module as Node imports it, transformed per-file)? The plan currently commits to the latter; this doc revisits that choice.
2. **Implementation language for new APIs.** If Nub adds globals, built-in modules, or new specifier namespaces, can their implementations live in Rust and still be callable from JS at reasonable overhead — or does anything in JS-land have to be written in JS?

## Premise check: what do Bun and tsx actually do?

The motivating intuition was "Bun and tsx bundle with esbuild/their own bundler before passing the result to Node." That's the wrong mental model — but the framing "tsx just transpiles per file" was also incomplete. The accurate picture, after reading `tsx/`:

- **tsx ships two hooks, not one.** A resolve hook (744 lines in `src/esm/hook/resolve.ts`) and a load hook (420 lines in `src/esm/hook/load.ts`), plus a parallel CJS path (`src/cjs/api/module-resolve-filename/`, `src/cjs/api/module-extensions.ts`). The resolve hook is where the heavy lifting happens — extension probing for `.ts/.tsx/.jsx` candidates, directory-import → `index.{ts,tsx,jsx,js}` resolution, tsconfig `paths` alias rewriting, `.js → .ts` swap in exports/imports maps. The load hook then calls esbuild's `transformSync` per file. Cached output is content-hashed. Architectural ancestor: `@esbuild-kit/{esm,cjs}-loader`. Detailed breakdown lives in [`tsx-architecture.md`](tsx-architecture.md).
- The practical implication for Nub: **resolution + transform are both first-class augmentation surfaces inside the loader-hook layer.** "Where in the pipeline can we intercept?" — both *before* Node's default resolver runs (resolve hook) and *between resolution and parse* (load hook). esbuild's `transform` API has no plugin system, but the resolve hook is itself the plugin point.
- **Bun** transpiles per module on first import, inside its runtime — its transpiler is invoked from JavaScriptCore's module-loader callback, not as a pre-pass. Bun's own docs state "every file is transpiled on the fly … before being executed." Bun's *bundler* (`bun build` / `Bun.build`) is a separate code path used for producing deploy artifacts, not for running scripts.
- **esbuild-runner** is the one widely-known "bundle before exec" precedent. The community moved off it toward per-file tools (tsx, swc-node, ts-node) specifically because the bundled-runtime model hits module-identity sharp edges (below).

In other words, Nub's currently-planned design (sync `module.registerHooks()` + content-addressed cache) is the same shape as both tsx and Bun, *not* a deviation. The bundle-before-exec idea is actually the road less travelled.

## Augmentation layer A: bundle-then-exec (Rolldown statically linked)

What we'd build: `nub run script.ts` invokes a Rust binary that calls vendored Rolldown to produce a single bundle for the entry + its graph, writes it to a cache, and spawns `node` on the bundled artifact.

**What this buys us:**

- Virtual modules / custom specifier resolution via Rolldown plugins (`nub:foo`, `~/...`, asset imports).
- Whole-program transforms (constant folding across module boundaries, dead code elimination of unused exports, tree-shaking before Node even sees the code).
- Trivial single-file output reuse — `nub build` and `nub run` would share the same pipeline.
- Custom Rust-side plugins inside our vendored Rolldown — pure-Rust augmentation without an N-API hop per transform.

**What it costs:**

- **Module identity is broken in subtle, painful ways.** Bundling collapses N modules into one artifact. Anything resolved *outside* the bundle (Node builtins, native `.node` addons, externalized `node_modules`, dynamic `import(<computed>)`) keeps its own instance. Classes from a bundled copy of `lib-x` fail `instanceof` against the externally-resolved copy; module-level singletons diverge ("dual package hazard"); `require.cache` keys diverge (breaks HMR, mocking libs, test isolation); `import.meta.url` becomes the bundle URL (breaks `new URL('./asset', import.meta.url)` patterns); native addons can never be bundled. Mitigation is to mark `node_modules` external — but at that point you're only bundling user code, which a per-file hook does just as well, and the bundler layer stops earning its keep.
- **Cold start cost is whole-program, not just touched files.** Per-file hooks transform `O(modules actually imported)`. A bundler always parses the full graph rooted at the entry. For a server starting up with conditional code paths, this is wasted work.
- **Loses Node's resolver semantics** for everything inside the bundle. Conditional exports, package exports maps, subpath imports, `node:`-prefix handling — all of it now goes through Rolldown's resolver, which is rollup-compatible but not Node-identical. Edge cases bite.
- **Debugger / source-map story is more complex.** The artifact is one file with sections per source; debuggers handle this fine but there's more rope.
- **Rolldown's Rust plugin trait is not a stable public API.** The team's published position is "napi-rs first" — the contract they commit to is the Rollup-compatible JS plugin interface, mirrored across napi and wasi backends. Built-in plugins live behind an internal `Plugin` trait, but it's explicitly unstable. Embedding Rolldown as a Rust crate and writing plugins against the internal trait means pinning a commit and absorbing churn. Confirmed by the rolldown maintainers and reflected in our existing rolldown-embedding research.

## Augmentation layer B: per-file loader hooks (current plan)

`module.registerHooks()` (sync, Node 24.13.1+) intercepts each module as Node resolves and loads it. Rust transformers (swc, lightningcss) run via napi-rs; content-hash disk cache short-circuits known files.

**What this buys us:**

- Cold start scales with **touched** files, not repo size.
- Preserves Node's resolver — conditional exports, subpath imports, `node:*` builtins, native addons all behave exactly as in stock Node. Compatibility is the trust contract, and this path defends it by default.
- Module identity is preserved end-to-end. Singletons, `instanceof` across packages, `require.cache`, `import.meta.url` — all native.
- Unified `require` / `import` path. The sync hooks API (vs the older async `register()`) was specifically designed so a single resolve/load pair covers CJS and ESM. The historical CJS gaps of async `register()` go away.
- **Recent Node fix unlocks `node:*` interception via sync hooks** (Node commit `2d560e4` / PR #58004 — `require('node:zlib')` was previously skipping the sync chain). We get a hook over the `node:*` namespace from outside the runtime. Worth a v1 sanity check against our pinned Node floor; the fix shipped in the 24.x line.
- Hook code is just a `--import nub-hooks.mjs` prelude — easy to reason about, easy to opt out for debugging.

**What it costs / doesn't give us:**

- No whole-program transforms. If we ever want cross-module dead-code elimination or constant propagation at run time, this layer can't do it. (Probably fine — that's `nub build`'s job.)
- We get one hook per import, which for very import-heavy startup paths adds N×(hook cost) overhead. The content-addressed cache makes the per-hit cost stat-bound; cold cache pays the full transform.
- Resolve-hook can claim arbitrary `<scheme>:<rest>` specifiers (e.g. `nub:foo`) — same expressiveness as virtual modules in a bundler plugin. Bare specifiers and `node:*` are also interceptable. So the "new specifier namespace" capability isn't unique to the bundler layer.
- Gotcha: hooks are **not** inherited by worker threads unless re-preloaded. Our spawn pipeline already controls the prelude, so this is solvable but worth remembering when we add Workers later.

## Augmentation layer C: stock Node, no pre-processing (`nub run script.js`)

For JS that doesn't need any transformation, both layers above should add zero overhead. The plan already commits to this.

## Implementation language: how does Rust reach JS at run time?

Moved to its own write-up: [`rust-from-js.md`](rust-from-js.md).

TL;DR for the architectural decision here: N-API addons via napi-rs are the default Rust-from-JS path on stock Node. ~26 ns floor per trivial call, ~230 ns returning objects. **Surfaces must be coarse-grained** — one call per operation, never per token/byte. Bun-style sub-microsecond globals are not achievable without modifying the Node runtime, and modifying the runtime is out of scope (see [`forking-node.md`](forking-node.md)). WASM and Rust sidecars are special-case options. See the dedicated doc for benchmarks, distribution, and design rules.

## Recommendation

1. **Keep the loader-hook layer as the default execution pipeline.** It matches what Bun and tsx actually do, defends Node compatibility automatically, and avoids the module-identity sharp edges of bundle-then-exec.
2. **Rolldown stays scoped to `nub build` and explicit bundle commands.** Don't put it on the `nub run` hot path. The "we get bundler-level augmentation for free" framing turns out to over-promise: most of the augmentation we want is already reachable via resolve hooks (virtual specifiers, package replacement) or `--import` preludes (globals).
3. **New globals and built-in modules: yes, implementable in Rust, via a small N-API "prelude" addon loaded by `--import`.** Keep the surface coarse-grained — design APIs so the JS side calls into Rust in chunks, not in per-byte hot loops. V8 Fast API is on the horizon but unreliable to plan around.
4. **New module specifier namespaces** (e.g. `nub:foo` if/when we decide to introduce them — currently off the table per the [no-nub-global feedback memory](../../../.claude/projects/-Users-colinmcd94-Documents-projects-nub/memory/feedback-no-nub-global.md)) are served by the resolve hook returning a synthetic URL whose load hook returns Rust-transformed JS source (or thin JS that re-exports the N-API prelude). The bundler is not needed.
5. **Package replacement by name** is also a resolve-hook job: when the resolver sees `swc` / `tsx` / `lightningcss`, redirect to our built-in implementation. This is the cleanest version of what PLAN.md describes; we don't need the bundler layer for it.

## Open follow-ups

- **Cache-hit micro-benchmark.** Confirm the sync hook + cache path is sub-ms per intercepted file under realistic load. Build the prototype, measure against tsx and ts-node baselines.
- **Worker-thread hook inheritance** — concrete UX for users spawning Workers from a Nub-run script. Likely just "Nub spawns with `--import` and Workers inherit `execArgv` by default"; needs verification.
- **`node:*` interception via sync hooks** is fixed in Node 24.x but verify against our pinned floor (24.13.1+). Add a CI smoke test once it lands.
- **N-API surface design.** Concrete first APIs (hash, file read, Rust-side path resolution?) and a coarse-grained-only convention written down somewhere visible. Possibly its own research doc once we have candidates.
- **Rolldown Rust plugin trait stability.** Track whether the rolldown team publishes a stable Rust plugin contract during
  2026. If it lands, `nub build`'s Rust-plugin story improves
meaningfully, but it doesn't change the runtime-pipeline call.
- **Workers, embedding API.** Out of scope today; revisit after v1 ships.

## Sources verified (2026-05-16)

- tsx architecture and esbuild `transform`-only constraint: `npmjs.com/package/tsx`, `esbuild.github.io/api/#transform`.
- Bun on-demand transpile model: `bun.com/docs/runtime/typescript`, `github.com/oven-sh/bun/blob/main/src/resolver/resolver.zig`.
- `module.registerHooks()` capabilities incl. `node:*` fix: `nodejs.org/api/module.html`, `github.com/nodejs/node/commit/2d560e42fa`, `github.com/nodejs/node/issues/56241`.
- N-API call-cost benchmarks: `github.com/Brooooooklyn/rust-to-nodejs-overhead-benchmark`, `github.com/nodejs/node/pull/21072`, `github.com/napi-rs/napi-rs/issues/1973`.
- Rolldown plugin model (napi-rs-first, Rust trait internal): `github.com/rolldown/rolldown`, `deepwiki.com/rolldown/rolldown`.
- Bundle-then-exec precedents: `github.com/folke/esbuild-runner`, `github.com/privatenumber/ts-runtime-comparison`.
- Module identity / dual-package hazard: `nodejs.org/api/packages.html#dual-package-hazard`.

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.

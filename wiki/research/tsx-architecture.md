# Research: tsx architecture — what we should learn from it

**Status:** v1, 2026-05-16. Reviewed by reading `tsx/` HEAD directly (cloned `privatenumber/tsx`, ~1.8k LOC across ESM/CJS hooks). **Informs:** `PLAN.md` — Pre-processing model, `PLAN.md` — Execution entry points, `PLAN.md` — Package replacement by name. **Related:** [`augmentation-layers.md`](augmentation-layers.md) — corrected its "tsx is just per-file transpile" framing based on this read.

## Why tsx matters

tsx is the most-installed Node TypeScript runner (>10M downloads/wk as of mid-2026). It is the project Nub's "drop-in `node` for TS" positioning is most directly competing with, and it has solved several real problems we're going to hit. Most of the architecture below transfers directly.

## High-level shape

```
  tsx script.ts
        │
        ▼
  spawns ─► node                                       (pinned via process.execPath)
              --require .../preflight.cjs              (pre-bootstrap; worker hooks, signals)
              --require .../patch-repl.cjs             (only if REPL invocation)
              --import file://.../loader.mjs           (the actual loader)
              script.ts                                (user code, unmodified)
```

That's it. `tsx` itself is a Node CLI that **spawns** Node — same pattern Nub has committed to in Execution entry points (bundle-and-spawn, not embed). Source: `src/run.ts:38-62`.

Older Node (pre-`module.register`) gets `--loader` instead of `--import` — gated on `isFeatureSupported(moduleRegister)`. Worth noting: tsx supports Node back to ~18, so it juggles multiple hook APIs. Nub targets Node ≥ 24 and gets to skip all the legacy.

## Two parallel hook layers

tsx ships **separate ESM and CJS hooks**, because CJS pre-dates `module.register` and even today some CJS interop paths benefit from the classic monkey-patching pattern.

### ESM hooks (`src/esm/hook/`)

Registered via `module.register('./loader.mjs', ...)`. Two hook functions:

- **`resolve`** — 744 lines in `src/esm/hook/resolve.ts`. This is where the heavy lifting happens; details below.
- **`load`** — 420 lines in `src/esm/hook/load.ts`. Calls esbuild's `transformSync` per file, attaches inline source maps, detects ESM vs CJS via `es-module-lexer`, rewrites dynamic `import()` if needed.

Both run synchronously and in-realm (since they go through the sync hooks API on supported Node versions).

### CJS hooks (`src/cjs/api/`)

The classic pattern:

- **`Module._resolveFilename` monkey-patched** (`src/cjs/api/register.ts:84`). Wrapped with tsx's resolver so `require('./foo')` does the same extension-probing dance the ESM resolve hook does.
- **`Module._extensions['.ts']` etc. installed** (`src/cjs/api/module-extensions.ts`). For each TypeScript extension, a function that reads the file, runs `transformSync`, evaluates the result via `module._compile`. Source maps attached.

For Nub, the relevant fact is: with **Node ≥ 24's `module.registerHooks()`** (sync), the resolve+load pair covers *both* `require` and `import` from one place. We don't need a separate CJS path. This is a real simplification tsx itself can't yet enjoy because of its broader Node-version support.

## What the ESM resolve hook actually does

Reading `src/esm/hook/resolve.ts` end-to-end, the responsibilities in order of how they fire on a typical `import './foo'`:

1. **Namespace gate.** tsx supports a `namespace` option for libraries that want their own isolated hook instance (multiple tsx registrations can coexist without colliding — `src/esm/api/scoped-import.ts`). Outside scope for Nub v1 but worth knowing the pattern exists.
2. **tsconfig path-alias rewriting** (`resolveTsPaths`, `src/esm/hook/resolve.ts:422-460`). Reads parsed `tsconfig.json`, applies `paths` mappings (e.g. `@/components/Foo` → `/abs/path/src/components/Foo`) via `get-tsconfig`'s `resolvePathAlias`. Skips for `node_modules` and for specifiers that already include query params.
3. **Directory-import handling** (`resolveDirectory`, lines 294-356). `./foo/` and `./foo` (when the actual filesystem shows a directory) become `./foo/index` candidates.
4. **Extension probing** (`resolveBase` → `resolveExtensions` → `mapTsExtensions`). The candidate-list strategy is the single most useful piece to copy. From `src/utils/map-ts-extensions.ts`:

   ```
   .js  ⇄  .ts, .tsx, .js, .jsx            (swap table)
   .jsx ⇄  .tsx, .ts, .jsx, .js
   .cjs ⇄  .cts
   .mjs ⇄  .mts

   extensionless local files: .ts, .tsx, .jsx, .js, .json
   extensionless deps:        .js, .json, .ts, .tsx, .jsx
                              ↑ deps prefer .js because the
                                published .js is more likely to
                                "behave correctly" than the
                                source .ts that ships alongside
                                (per the esbuild 0.20.0 release
                                notes that tsx cites).
   ```

The hook loops the candidate list calling `nextResolve(candidate)` and catching `ERR_MODULE_NOT_FOUND` / `ERR_PACKAGE_PATH_NOT_EXPORTED`, accepting the first one that resolves.
5. **`.js → .ts` swap inside exports/imports maps.** When the straightforward resolve fails because a package's `exports` field points to `./dist/foo.js` and that file doesn't exist on disk (because the package author is shipping `.ts` sources), tsx pulls the missing-path from the error, swaps `.js` → `.ts`, and retries. This is how it transparently runs packages that haven't been built.

The pattern is: **don't reimplement Node's resolver. Generate a candidate list, delegate to `nextResolve` for each candidate, accept the first that works.** Cheap, correct, doesn't drift from Node's semantics.

### Implications for Nub

- All of (1)–(5) are things `nub run` needs to do too. We can copy the candidate-list pattern directly. The tsconfig handling can share `get-tsconfig` (MIT, well-maintained) rather than reinvent.
- The `namespace` pattern is interesting if we ever want `module.registerHooks()` to coexist with user-installed hooks (e.g., user has tsx *and* Nub's hooks active). Worth keeping filed under "later."
- We can extend the candidate list to include Nub's package-replacement targets at the same layer — if user wrote `import "tsx"`, the resolver intercepts and routes to our prelude-loaded built-in. Mechanism, [per `augmentation-layers.md`](augmentation-layers.md#augmentation-layer-b-per-file-loader-hooks-current-plan), is the resolve hook.

## What the ESM load hook does

`src/esm/hook/load.ts`:

1. **Format detection.** Uses `es-module-lexer` to peek at the source and decide ESM vs CJS for ambiguous files (e.g., a `.ts` without a `package.json` `type`). Records this in the format string returned to Node (`module-typescript` vs `commonjs-typescript`).
2. **Transform.** Calls `transformSync` from `src/utils/transform/index.ts`, which wraps esbuild's `transformSync`. esbuild's `transform` API has **no plugin system** (only `build` does) — so this is genuinely per-file AST-level transform with no extension points.
3. **Dynamic-`import` rewriting.** Some `import(...)` calls in transpiled output need rewriting to pass through tsx's loader again (`transformDynamicImport`). This is the kind of subtle thing you only hit if you've shipped a TS runner before; worth re-discovering carefully when we get there.
4. **Inline source maps.** `inlineSourceMap` adds the source map as a base64 data URL comment. Combined with `process.setSourceMapsEnabled(true)` at register time, this means Node remaps stack traces transparently.

### Implications for Nub

- esbuild's `transform`-only constraint is a real limitation for tsx. Nub avoids it by calling **swc** directly (with full plugin surface available if we ever want it).
- The format-detection step matters. Without it, an ambiguous `.ts` file imported with `require()` from a package that uses CJS would fail. Plan to use es-module-lexer (or swc's own detection — needs investigation).
- Dynamic `import()` rewriting is a sharp edge to remember when we bench against tsx on dynamic-heavy code.
- `process.setSourceMapsEnabled(true)` + inline maps is the whole source-map story. Don't overthink it.

## Caching

`TMPDIR/tsx-<process-uid>/<content-hash>.<ext>`. Each transformed file is keyed by content + transform options. Read-through cache on every load. Confirmed by reading `src/utils/transform/index.ts` paths but the disk layout is documented in tsx's README and the test fixtures (`tests/fixtures/`).

Nub's plan already commits to a content-addressed cache; the difference is `~/.cache/nub/<hash>` rather than `TMPDIR`. Pick of location matters for CI reuse: `~/.cache` survives across runs, `TMPDIR` doesn't on most CI runners. We probably want `~/.cache` as default with `TMPDIR` as a fallback when `XDG_CACHE_HOME` is unset and `HOME` is non-writable (containers).

## IPC pipe for `--watch`

`src/utils/ipc/{client,server}.ts`. When tsx is invoked as `tsx watch`, the parent process opens a Unix socket / named pipe at a path derived from `ppid`. The loader inside the child process opens a client connection on startup. On every `load`, the child sends `{ type: "load", url }` to the parent, which feeds the file watcher with the actual dep graph.

This is the right shape for "watch what was actually imported" (vs. watching the filesystem blindly). For `nub --watch` / `nub dev`, copy this exactly. Pipe path derivation by `ppid` is a small detail but solves the "how does the loader know what to connect to" question without env-var plumbing.

## Worker thread inheritance

`src/preflight.cjs` is `--require`d before everything else and is responsible for ensuring that worker threads spawned from the user code also get tsx's hooks. This is the patch for the [worker-hooks-not-inherited gotcha noted in `augmentation-layers.md`](augmentation-layers.md#augmentation-layer-b-per-file-loader-hooks-current-plan).

The mechanism: `preflight.cjs` patches `worker_threads.Worker` options to inject the same `--import` / `--require` flags into the worker's `execArgv`. Result: workers transparently inherit the loader without the user thinking about it.

Nub needs to ship this. It's a small file but easy to forget.

## Things tsx does that we should **not** copy

- **Hand-rolled `Module._resolveFilename` patching.** A legacy pattern tsx keeps for old-Node support. Nub targets Node ≥ 24 and gets `module.registerHooks()` sync for both CJS and ESM in one call. Skip the legacy.
- **esbuild as the transformer.** esbuild's `transform` API lacks plugins; we get the full swc/oxc surface area instead. Same shape, more flexibility.
- **Loader fallback for old Node.** Nub's Target version section pins Node ≥ 24. Don't pay the maintenance for older API paths.

## Things tsx does that we should copy directly

1. Bundle-and-spawn pattern with `--require preflight.cjs --import loader.mjs <argv>`.
2. Candidate-list extension probing via repeated `nextResolve` calls.
3. `get-tsconfig` for tsconfig.json parsing & path-alias resolution (or vendor it Rust-side via a swc-adjacent crate — worth evaluating, but starting from `get-tsconfig` is fine for v1).
4. Format detection via `es-module-lexer` (or swc's own detection).
5. `process.setSourceMapsEnabled(true)` + inline source maps in the load hook output.
6. Content-addressed disk cache. (Already in the plan.)
7. IPC pipe parent↔child for `--watch` mode's "what files actually got loaded" feedback.
8. Worker-thread `execArgv` patching via a `--require`d preflight.
9. The `.js → .ts` swap in exports/imports maps trick — invisibly handles unbuilt packages.
10. Dependency-vs-local extension priority (deps prefer `.js`, local prefers `.ts`).

## Things to investigate further

- **How tsx handles the new `node:*` exception** for the sync resolve hook ([fixed in Node 24 per `augmentation-layers.md`](augmentation-layers.md#augmentation-layer-b-per-file-loader-hooks-current-plan)). tsx's resolve hook doesn't explicitly skip `node:*` — does Node do that for it, or does tsx need to handle it explicitly with the new sync hooks?
- **swc vs esbuild for our use case.** tsx uses esbuild because it's the de facto fast-transform tool for JS land. swc is comparable in speed and gives us more (plugins, Rust-native embedding). Confirm with a micro-bench on our typical TS files.
- **Source-map fidelity** on combined transforms (TS → JS + bundler passes for `nub build`). tsx only stacks one transform; Nub potentially stacks more.
- **Test plumbing.** tsx's `tests/` directory is a goldmine of edge cases (decorators, namespaces, `.cts` from ESM, etc.). Worth mining for our test plan once `nub run` is implementable.

## Sources

- tsx source: `tsx/` (cloned from `github.com/privatenumber/tsx`)
- Files read in detail: `src/run.ts`, `src/esm/hook/{resolve,load,initialize}.ts`, `src/cjs/api/{register,module-extensions,require}.ts`, `src/utils/map-ts-extensions.ts`, `src/utils/ipc/client.ts`.
- esbuild `transform`-no-plugins constraint: `esbuild.github.io/api/#transform`.
- get-tsconfig: `github.com/privatenumber/get-tsconfig` (same author as tsx).

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.

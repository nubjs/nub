# Research: tsx architecture — what we should learn from it

**Status:** v1, 2026-05-16. Reviewed by reading `tsx/` HEAD directly (cloned `privatenumber/tsx`, ~1.8k LOC across ESM/CJS hooks). **Related:** [`augmentation-layers.md`](augmentation-layers.md) — this read corrected its "tsx is just per-file transpile" framing.

## Why tsx matters

tsx is the most-installed Node TypeScript runner (>10M downloads/wk as of mid-2026) and the closest existing analogue to Nub's drop-in `node`-for-TypeScript positioning. Most of the architecture below transfers directly.

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

The tsx CLI is itself a Node process that **spawns** Node — the bundle-and-spawn pattern Nub has committed to, rather than embedding (`src/run.ts:38-62`).

Older Node (pre-`module.register`) gets `--loader` instead of `--import`, gated on `isFeatureSupported(moduleRegister)`. tsx supports Node back to ~18 and so juggles multiple hook APIs; Nub targets Node ≥ 24 and skips the legacy paths.

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

With **Node ≥ 24's sync `module.registerHooks()`**, the resolve+load pair covers *both* `require` and `import` from one place, so Nub needs no separate CJS path — a simplification tsx cannot take, given its broader Node-version support.

## What the ESM resolve hook actually does

Responsibilities from `src/esm/hook/resolve.ts`, in the order they fire on a typical `import './foo'`:

1. **Namespace gate.** tsx supports a `namespace` option for libraries that want their own isolated hook instance (multiple tsx registrations can coexist without colliding — `src/esm/api/scoped-import.ts`). Outside scope for Nub v1.
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

Three takeaways: the candidate-list pattern copies directly and can reuse `get-tsconfig`, the `namespace` isolation option is deferred, and Nub's package-replacement intercept hangs off this same resolve hook.

- All of (1)–(5) are things `nub run` needs to do; the candidate-list pattern copies directly, and tsconfig handling can use `get-tsconfig` (MIT, well-maintained) rather than a reimplementation.
- The `namespace` pattern matters if `module.registerHooks()` ever needs to coexist with user-installed hooks (a user running tsx *and* Nub's hooks at once). Filed under later.
- We can extend the candidate list to include Nub's package-replacement targets at the same layer — if user wrote `import "tsx"`, the resolver intercepts and routes to our prelude-loaded built-in. Mechanism, [per `augmentation-layers.md`](augmentation-layers.md#augmentation-layer-b-per-file-loader-hooks-current-plan), is the resolve hook.

## What the ESM load hook does

From `src/esm/hook/load.ts`:

1. **Format detection.** Uses `es-module-lexer` to peek at the source and decide ESM vs CJS for ambiguous files (e.g., a `.ts` without a `package.json` `type`). Records this in the format string returned to Node (`module-typescript` vs `commonjs-typescript`).
2. **Transform.** Calls `transformSync` from `src/utils/transform/index.ts`, which wraps esbuild's `transformSync`. esbuild's `transform` API has **no plugin system** (only `build` does), so this is a per-file AST-level transform with no extension points.
3. **Dynamic-`import` rewriting.** Some `import(...)` calls in transpiled output need rewriting to pass through tsx's loader again (`transformDynamicImport`).
4. **Inline source maps.** The map goes in as a base64 data URL comment (`inlineSourceMap`). Combined with `process.setSourceMapsEnabled(true)` at register time, Node remaps stack traces transparently.

### Implications for Nub

What the load hook implies for Nub: swc instead of esbuild, format detection is mandatory, dynamic `import()` rewriting is a benchmark caveat, and inline maps carry the whole source-map story.

- esbuild's `transform`-only constraint is a real limitation for tsx. Nub avoids it by calling **swc** directly, with the full plugin surface available.
- The format-detection step matters: without it, an ambiguous `.ts` file imported with `require()` from a package that uses CJS would fail. Plan to use es-module-lexer, or swc's own detection — needs investigation.
- Dynamic `import()` rewriting is a sharp edge to remember when we bench against tsx on dynamic-heavy code.
- Inline maps plus `process.setSourceMapsEnabled(true)` is the whole source-map story.

## Caching

Cache path is `TMPDIR/tsx-<process-uid>/<content-hash>.<ext>`, each transformed file keyed by content + transform options, read through on every load.

Confirmed by reading the paths in `src/utils/transform/index.ts`; the disk layout is documented in tsx's README and the test fixtures (`tests/fixtures/`).

Nub's plan already commits to a content-addressed cache; the difference is `~/.cache/nub/<hash>` rather than `TMPDIR`. Location matters for CI reuse: `~/.cache` survives across runs, `TMPDIR` doesn't on most CI runners. We probably want `~/.cache` as default with `TMPDIR` as a fallback when `XDG_CACHE_HOME` is unset and `HOME` is non-writable (containers).

## IPC pipe for `--watch`

Source: `src/utils/ipc/{client,server}.ts`. When tsx is invoked as `tsx watch`, the parent process opens a Unix socket / named pipe at a path derived from `ppid`.

The loader inside the child process opens a client connection on startup. On every `load`, the child sends `{ type: "load", url }` to the parent, which feeds the file watcher with the actual dep graph.

This is the right shape for "watch what was actually imported" rather than watching the filesystem blindly, and `nub --watch` / `nub dev` should copy it exactly. Deriving the pipe path from `ppid` answers "how does the loader know what to connect to" without env-var plumbing.

## Worker thread inheritance

The file `src/preflight.cjs` is `--require`d before everything else, and ensures worker threads spawned from user code also get tsx's hooks — the patch for the [worker-hooks-not-inherited gotcha noted in `augmentation-layers.md`](augmentation-layers.md#augmentation-layer-b-per-file-loader-hooks-current-plan).

It patches `worker_threads.Worker` options to inject the same `--import` / `--require` flags into the worker's `execArgv`, so workers inherit the loader transparently.

Nub needs to ship this; it is a small file and easy to forget.

## Things tsx does that we should **not** copy

Three patterns Nub skips, each a concession either to old-Node support or to esbuild's limits.

- **Hand-rolled `Module._resolveFilename` patching.** A legacy pattern tsx keeps for old-Node support. Nub targets Node ≥ 24 and gets sync `module.registerHooks()` for both CJS and ESM in one call.
- **esbuild as the transformer.** esbuild's `transform` API lacks plugins; we get the full swc/oxc surface area instead.
- **Loader fallback for old Node.** Nub pins Node ≥ 24, so the older API paths are not worth the maintenance.

## Things tsx does that we should copy directly

Ten patterns to lift as they stand, from the preflight-plus-loader spawn shape to the `.js → .ts` exports-map swap.

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

Four open questions: the `node:*` exception under sync hooks, swc versus esbuild on Nub's own files, source-map fidelity across stacked transforms, and which of tsx's tests to mine.

- **How tsx handles the new `node:*` exception** for the sync resolve hook ([fixed in Node 24 per `augmentation-layers.md`](augmentation-layers.md#augmentation-layer-b-per-file-loader-hooks-current-plan)). tsx's resolve hook doesn't explicitly skip `node:*` — does Node do that for it, or does tsx need to handle it explicitly with the new sync hooks?
- **swc vs esbuild for our use case.** tsx uses esbuild, the de facto fast-transform tool in JS land; swc is comparable in speed and adds plugins and Rust-native embedding. Confirm with a micro-bench on our typical TS files.
- **Source-map fidelity** on combined transforms (TS → JS + bundler passes for `nub build`). tsx only stacks one transform; Nub potentially stacks more.
- **Test plumbing.** tsx's `tests/` directory carries edge cases worth mining for our test plan (decorators, namespaces, `.cts` from ESM) once `nub run` is implementable.

## Sources

The tsx checkout, the files read in detail, and the upstream references for esbuild's transform constraint and get-tsconfig.

- tsx source: `tsx/` (cloned from `github.com/privatenumber/tsx`)
- Files read in detail: `src/run.ts`, `src/esm/hook/{resolve,load,initialize}.ts`, `src/cjs/api/{register,module-extensions,require}.ts`, `src/utils/map-ts-extensions.ts`, `src/utils/ipc/client.ts`.
- esbuild `transform`-no-plugins constraint: `esbuild.github.io/api/#transform`.
- get-tsconfig: `github.com/privatenumber/get-tsconfig` (same author as tsx).

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.

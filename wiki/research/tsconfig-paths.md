---
**Status:** v1, 2026-05-16. Split out from [`module-resolution.md`](module-resolution.md) because the design surface is large enough on its own.
**Builds on:** [`module-resolution.md`](module-resolution.md) (extensionless probing and the resolve-hook split), [`tsx-architecture.md`](tsx-architecture.md) (tsx's path-alias implementation), [`rust-from-js.md`](rust-from-js.md) (N-API call-cost rules).
---

# Research: tsconfig path-alias resolution

## Why this matters

The `compilerOptions.paths` field in `tsconfig.json` lets TypeScript projects declare import-specifier aliases — most famously the `@/...` → `./src/...` convention popularized by Next.js, Vite, and Vue/Nuxt templates, and copied into a large share of new TS projects since ~2020. Aliases also cover monorepo local-package references (`@org/shared` → `../shared/src`) and hiding deep directory paths.

- **A checker-only feature.** tsc never emits with the alias resolved — the emitted `.js` keeps the literal alias specifier (`import "@/components/Foo"`), leaving resolution to the runtime.
- **Node has no built-in support.** Plain Node treats `import "@/components/Foo"` as a bare specifier, looks in `node_modules`, and fails.
- **Every TS runner ships its own path-alias resolver.** tsx, ts-node, swc-node, esbuild-runner, Bun, Vite (via `vite-tsconfig-paths`), Next.js (via Webpack/Turbopack plugins), `ts-jest`, and `vitest` each have an implementation. None are bug-compatible with tsc.

A TS project that uses `paths` is therefore not portable across runners by default. Making `import "@/x"` work without a build step requires the runtime to do alias resolution; without it, every Next.js- or Vite-bootstrapped project fails at the first `@/`.

## What tsc's `paths` resolution does

The behavior to match, in priority order, per the TS handbook:

1. **Read tsconfig.** Find the nearest `tsconfig.json` walking up from the source file. Resolve `extends` chains, which can transitively pull in other configs.
2. **`baseUrl`** (deprecated in TS 5.0 but still widely used). Anchors relative paths in `paths` values. Default: directory of the tsconfig that defined `paths`.
3. **Apply `paths` mappings.** Each key is a pattern with at most one `*` wildcard; each value is an array of replacement templates with at most one `*`. Patterns try in declaration order, with more-specific patterns evaluated first per the TS "longest-match" rule, and each pattern's replacements try in order.
4. **Resolve the substituted path** as a file relative to `baseUrl`, or to the tsconfig directory if no `baseUrl`. Apply the normal extensionless-probing rules (per [`module-resolution.md`](module-resolution.md#candidate-probing-dynamic-ordering-by-parent)).
5. **Fall through to ordinary resolution** if no `paths` pattern matches.

The match algorithm:

- Exact match: `"foo"` matches specifier `foo` only.
- Wildcard match: `"@/*"` matches specifier `@/components/Foo`, captures `components/Foo`, substitutes into replacements like `["./src/*", "./vendor/*"]`.
- Longest-prefix wins when multiple patterns could match. tsc sorts patterns by static-prefix length descending before matching.

Edge cases tsc handles that runners frequently get wrong:

- `extends` chains where a base config defines `paths` and an extending config doesn't override — the base's `paths` apply, but resolved relative to the **extending config's directory** if `baseUrl` isn't explicit.
- Multiple replacements per pattern: tsc tries each in order; first that resolves to an actual file wins.
- `paths` without `baseUrl` (allowed since TS 4.1): replacements are resolved relative to the tsconfig that contains them.
- TS project references: a referenced project's `paths` are not inherited by the referencing project; each project is resolved against its own tsconfig.
- Trailing-slash and case-sensitivity subtleties on Windows / macOS.

The `get-tsconfig` package (MIT, by tsx's author) is the canonical JS implementation and is battle-tested against tsc's behavior. Treat it as the reference implementation for correctness, even if logic is eventually ported to Rust.

## Prior art

- **tsx**: uses `get-tsconfig`. Reads the nearest config, applies `paths` before extension probing (`src/esm/hook/resolve.ts:resolveTsPaths`). Skips specifiers with query params and parents inside `node_modules`.
- **Bun**: implements its own resolver in Rust. Config reading lives under `src/resolver/tsconfig_json.zig`/`.rs` post-port; applies `paths` with longest-prefix matching.
- **swc-node / ts-node**: both delegate to `tsconfig-paths` (the older npm package), which lags on newer tsc semantics — TS 5.0's `paths`-without-`baseUrl` was a notable lag.
- **Vite + `vite-tsconfig-paths`**: most-used plugin in the Vite ecosystem. Wraps `tsconfig-paths`. Configurable for monorepos with multiple tsconfigs.
- **Webpack/Next.js**: uses `tsconfig-paths-webpack-plugin`, again wrapping `tsconfig-paths`.

Everyone is doing roughly the same thing; the differences are Rust vs JS implementation, caching strategy, and handling of `extends` chains and project references. tsx's choice of `get-tsconfig` as the source of truth is the conservative starting point.

## How this fits into the Nub resolve hook

The path-alias step runs **before** the candidate-probing step ([`module-resolution.md`](module-resolution.md#the-rust--js-split)). Pipeline:

1. Resolve hook receives `(specifier, parentURL)`.
2. Parent-URL gate: TS-family parent? If no, defer to Node.
3. **Path-alias step (this doc):** find nearest tsconfig, apply `paths` if the specifier matches a pattern.
4. If alias matched: the rewritten specifier is now a relative path; feed it to candidate probing.
5. If no alias matched: leave the specifier as-is, feed to candidate probing (for relative paths) or defer to `nextResolve` (for bare specifiers).

The step is per-import work, but tsconfig lookups are nest-stable per directory, so all the expensive parts can be cached.

### Caching strategy

Two caches:

1. **Tsconfig discovery cache.** Map `parent_dir → ResolvedTsconfig`, where `ResolvedTsconfig` is the fully-merged config (including resolved `extends` chain) and includes a list of file paths that were read to produce it, for invalidation.
2. **Path-alias compilation cache.** Compile each `paths` table once per `ResolvedTsconfig` into a `Vec<(prefix, suffix, replacements)>` sorted by longest static prefix. Match is then a sorted scan with prefix/suffix string ops — sub-microsecond.

Invalidation: mtime-based. On startup, stat all the tsconfig files referenced in the cache; if any have changed, invalidate the relevant `ResolvedTsconfig` entries. Cheap enough to do once at hook init; not per-import.

### Implementation language

The candidate-probing analysis in [`module-resolution.md`](module-resolution.md#how-much-of-this-should-be-rust) applies here too: **one coarse-grained napi call per resolve**, with all the path-alias logic inside that call.

- Tsconfig parsing: Rust-side. `serde_json` for JSON-with-comments (`tsconfig.json` is JSONC). The merge logic for `extends` chains is straightforward.
- Pattern matching: Rust-side. Sorted prefix/suffix scan; no regex needed because tsc's `paths` patterns have at most one `*`.
- Caching: Rust-side, in the same `LazyLock<HashMap>` as the resolution cache.

Open: vendor `get-tsconfig` and call it via napi, or port the JSONC + `extends` logic to Rust. Porting is ~200 lines; the risk is that `get-tsconfig` evolves with tsc semantics faster than a port. Start by vendoring and calling via napi, port to Rust later if profiling shows the napi hops are material. The `get-tsconfig` author also wrote tsx, so the upstream relationship is approachable.

## Differential analysis vs Bun

Per [`module-resolution.md`](module-resolution.md#differential-analysis-vs-bun), the differential surface is:

- **Bun**: parses tsconfig in Rust at first encounter, caches the parsed table on its resolver state, applies on each resolve. Single language, no boundary.
- **Nub**: parses tsconfig in Rust (in the napi-vendored resolver), caches in the same Rust-side cache, applies on each resolve via the single napi call. One napi crossing per import.

Per-import alias-resolution cost should be within ~5% of Bun. The napi hop is the only structural delta and rounds to nothing.

## Non-goals

- **TS project references** (`"references"` in tsconfig), used in large monorepos to declare build-time dependency graphs between TS packages. Path-alias resolution doesn't need to be aware of them — each TS source file has a single closest tsconfig that defines its `paths`, so each tsconfig is treated independently.
- **Watching tsconfig for live re-resolution.** If a user edits `tsconfig.json` during a long-running `nub --watch` session, the right behavior is restarting the process, which the watch layer already does. The resolver needs no separate live-reload path.
- **`tsconfig.json` `compilerOptions` beyond `paths` / `baseUrl`.** `moduleResolution`, `module`, `target` and the rest are tsc-checker concerns that don't affect runtime resolution.
- **Multiple matching `paths` patterns with different file outcomes.** Replacements are tried in order and the first that resolves wins; no ambiguity detection or warning. Matches tsx/Bun behavior.

## Open questions

- **Should `paths` resolution fire for parents *inside* `node_modules`?** tsx skips it, on the rationale that published packages shouldn't depend on the consumer's tsconfig. Probably match it, after confirming that doesn't break monorepo setups where workspace packages reference each other via aliases.
- **`extends` from an npm package** (`"extends": "@org/tsconfig/base"`) requires resolving the package via Node's resolver to find the actual tsconfig path. Adds one Node-resolver hop at tsconfig-load time. Cache the result.
- **Comment syntax in tsconfig.** `tsconfig.json` is JSONC officially, per tsc docs; not all parsers handle it. Confirm `serde_json` plus a JSONC preprocessor (or the `jsonc-parser` crate) is the right setup.
- **Performance of `extends` chain resolution.** Some real configs extend 3–4 levels deep across npm packages. Per-project one-time cost; should be sub-millisecond after first load.
- **Should `paths` parse errors surface loudly?** A malformed `tsconfig.json` is a developer-facing problem, which argues for a clear error message rather than silently failing to resolve aliases.

## Sources

- TS handbook on `paths`: [typescriptlang.org/tsconfig#paths](https://www.typescriptlang.org/tsconfig#paths).
- tsc reference implementation: `microsoft/TypeScript`, `src/compiler/moduleNameResolver.ts`.
- `get-tsconfig`: [github.com/privatenumber/get-tsconfig](https://github.com/privatenumber/get-tsconfig) (MIT).
- `tsconfig-paths` (older, still widely used): [github.com/dividab/tsconfig-paths](https://github.com/dividab/tsconfig-paths).
- tsx integration: `tsx/src/esm/hook/resolve.ts` (`resolveTsPaths`).
- Bun integration: `bun/src/resolver/tsconfig_json.rs` (post-port).
- TS 5.0 release notes on `paths` without `baseUrl`: [typescriptlang.org/docs/handbook/release-notes/typescript-5-0.html](https://www.typescriptlang.org/docs/handbook/release-notes/typescript-5-0.html).

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Links to internal planning documents were removed and reference-checkout paths rewritten; findings, tables and measured values are unchanged.

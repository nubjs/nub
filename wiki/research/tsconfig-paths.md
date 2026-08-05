---
**Status:** v1, 2026-05-16. Split out from [`module-resolution.md`](module-resolution.md)
because the design surface is large enough on its own.
**Builds on:** [`module-resolution.md`](module-resolution.md)
(extensionless probing and the resolve-hook split),
[`tsx-architecture.md`](tsx-architecture.md) (tsx's path-alias
implementation),
[`rust-from-js.md`](rust-from-js.md) (N-API call-cost rules).
**Informs:** `runtime/pre-processing.md`,
`PLAN.md` — Runtime features.
---

# Research: tsconfig path-alias resolution

Working write-up. Conclusions are current best read; future agents should feel free to revisit any of them.

## Why this matters

The `compilerOptions.paths` field in `tsconfig.json` lets TypeScript projects declare import-specifier aliases — most famously the `@/...` → `./src/...` convention popularized by Next.js, Vite, Vue/Nuxt templates, and copy-pasted into a very large fraction of new TS projects since ~2020. Aliases are also used for monorepo local-package references (`@org/shared` → `../shared/src`) and to hide deep directory paths.

The relevant facts:

- **`paths` is purely a tsc-checker feature.** tsc itself never emits with the alias resolved — the emitted `.js` keeps the literal alias specifier (`import "@/components/Foo"`), and at run time it's the runtime's job to figure out what to do with it.
- **Node has no built-in support for `paths`.** Plain Node encountering `import "@/components/Foo"` will treat it as a bare specifier and look in `node_modules`, then fail.
- **Every TS runner ships its own path-alias resolver.** tsx, ts-node, swc-node, esbuild-runner, Bun, Vite (via `vite-tsconfig-paths`), Next.js (via Webpack/Turbopack plugins), and the Jest world (`ts-jest`, `vitest`) each have an implementation. None of them are bug-compatible with tsc.

So a TS project that uses `paths` is *not portable* across runners by default. The only way to make `import "@/x"` work without a build step is for the runtime to do alias resolution. Nub's wedge is "drop-in TS execution," which means we need to ship this on day one — without it, every Next.js / Vite-bootstrapped project fails at the first `@/`.

## What tsc's `paths` resolution actually does

The behavior we need to match (in priority order, per TS handbook):

1. **Read tsconfig.** Find the nearest `tsconfig.json` walking up from the source file. Resolve `extends` chains (which can transitively pull in other configs).
2. **`baseUrl`** (deprecated in TS 5.0 but still widely used). Anchors relative paths in `paths` values. Default: directory of the tsconfig that defined `paths`.
3. **Apply `paths` mappings.** Each key is a pattern with at most one `*` wildcard; each value is an array of replacement templates with at most one `*`. Patterns try in declaration order (with the more-specific patterns evaluated first per the TS "longest-match" rule), and each pattern's replacements try in order.
4. **Resolve the substituted path** as a file relative to `baseUrl` (or to the tsconfig directory if no `baseUrl`). Apply the normal extensionless-probing rules (per [`module-resolution.md`](module-resolution.md#candidate-probing-dynamic-ordering-by-parent)).
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

The `get-tsconfig` crate (MIT, by tsx's author) is the canonical JS implementation and is fairly battle-tested against tsc's behavior. We should treat it as the reference implementation for correctness, even if we eventually port logic to Rust.

## Prior art

- **tsx**: uses `get-tsconfig`. Reads the nearest config, applies `paths` before extension probing (`src/esm/hook/resolve.ts:resolveTsPaths`). Skips for specifiers with query params and for parents inside `node_modules`.
- **Bun**: implements its own resolver in Rust. The config-reading side lives under `src/resolver/tsconfig_json.zig`/`.rs` post-port; applies `paths` similarly with longest-prefix matching.
- **swc-node / ts-node**: both delegate to `tsconfig-paths` (the older npm package), which is functional but lags on newer tsc semantics (TS 5.0's `paths`-without-`baseUrl` was a notable lag).
- **Vite + `vite-tsconfig-paths`**: most-used plugin in the Vite ecosystem. Wraps `tsconfig-paths`. Configurable for monorepos where there are multiple tsconfigs.
- **Webpack/Next.js**: uses its own resolution plugin (`tsconfig-paths-webpack-plugin`), again wrapping `tsconfig-paths`.

The takeaway from this survey: **everyone is doing roughly the same thing**; the differences are in (a) Rust vs JS implementation, (b) caching strategy, (c) handling of `extends` chains and project references. tsx's choice of `get-tsconfig` as the source of truth is the conservative move and what we should start from.

## How this fits into the Nub resolve hook

The path-alias step runs **before** the candidate-probing step ([`module-resolution.md`](module-resolution.md#the-rust--js-split)). Pipeline:

1. Resolve hook receives `(specifier, parentURL)`.
2. Parent-URL gate: TS-family parent? If no, defer to Node.
3. **Path-alias step (this doc):** find nearest tsconfig, apply `paths` if the specifier matches a pattern.
4. If alias matched: the rewritten specifier is now a relative path; feed it to candidate probing.
5. If no alias matched: leave the specifier as-is, feed to candidate probing (for relative paths) or defer to `nextResolve` (for bare specifiers).

The path-alias step is **per-import work** in the abstract, but because tsconfig lookups are nest-stable per directory, all the expensive parts can be cached.

### Caching strategy

Two caches:

1. **Tsconfig discovery cache.** Map `parent_dir → ResolvedTsconfig`, where `ResolvedTsconfig` is the fully-merged config (including resolved `extends` chain) and includes a list of file paths that were read to produce it (for invalidation).
2. **Path-alias compilation cache.** Compile each `paths` table once per `ResolvedTsconfig` into a `Vec<(prefix, suffix, replacements)>` sorted by longest static prefix. Match is then a sorted scan with prefix/suffix string ops — sub-microsecond.

Invalidation: mtime-based. On startup, stat all the tsconfig files referenced in the cache; if any have changed, invalidate the relevant `ResolvedTsconfig` entries. Cheap enough to do once at hook init; not per-import.

### Implementation language

The candidate-probing analysis in [`module-resolution.md`](module-resolution.md#how-much-of-this-should-be-rust) applies here too: **one coarse-grained napi call per resolve**, with all the path-alias logic happening inside that call. Concretely:

- Tsconfig parsing: Rust-side. `serde_json` for JSON-with-comments (`tsconfig.json` is JSONC). The merge logic for `extends` chains is straightforward.
- Pattern matching: Rust-side. Sorted prefix/suffix scan; no regex needed because tsc's `paths` patterns have at most one `*`.
- Caching: Rust-side, in the same `LazyLock<HashMap>` as the resolution cache.

There's a question of whether to vendor `get-tsconfig` (call it via napi) or port the JSONC + extends logic to Rust. Porting is ~200 lines; the risk is that `get-tsconfig` evolves with tsc semantics faster than our port. Probably start by vendoring and call via napi, port to Rust later if profiling shows the napi hops are material. The `get-tsconfig` author is the same person as tsx's author, so the upstream relationship is approachable.

## Differential analysis vs Bun

Per [`module-resolution.md`](module-resolution.md#differential-analysis-vs-bun), the differential surface is:

- **Bun**: parses tsconfig in Rust at first encounter, caches the parsed table on its resolver state, applies on each resolve. Single language, no boundary.
- **Nub**: parses tsconfig in Rust (in the napi-vendored resolver), caches in the same Rust-side cache, applies on each resolve via the single napi call. One napi crossing per import.

Per-import alias-resolution cost should be within ~5% of Bun. The napi hop is the only structural delta and rounds to nothing.

## Non-goals

- **TS project references** (`"references"` in tsconfig). Used in large monorepos to declare build-time dependency graphs between TS packages. Path-alias resolution doesn't need to be aware of them — each TS source file has a single closest tsconfig that defines its `paths`. We treat each tsconfig independently.
- **Watching tsconfig for live re-resolution.** If a user edits `tsconfig.json` while a long-running `nub --watch` session is active, the right behavior is "restart the process," which the watch layer already does. The resolver doesn't need a separate live-reload path.
- **`tsconfig.json` `compilerOptions` beyond `paths` / `baseUrl`.** `moduleResolution`, `module`, `target`, etc., are tsc-checker concerns and don't affect runtime resolution. We ignore them.
- **Multiple matching `paths` patterns with different file outcomes.** We try replacements in order and take the first that resolves; we don't try to detect ambiguity and warn. Matches tsx/Bun behavior.

## Open questions

- **Should `paths` resolution fire for parents *inside* `node_modules`?** tsx skips it (rationale: published packages shouldn't depend on the consumer's tsconfig). We should probably match. Worth confirming this doesn't break monorepo setups where workspace packages reference each other via aliases.
- **`extends` from an npm package** (`"extends": "@org/tsconfig/base"`) requires resolving the package via Node's resolver to find the actual tsconfig path. Adds one Node-resolver hop at tsconfig-load time. Cache the result.
- **Comment syntax in tsconfig.** `tsconfig.json` is JSONC officially (per tsc docs); not all parsers handle it. Confirm `serde_json` + a JSONC preprocessor (or `jsonc-parser` crate) is the right setup.
- **Performance of `extends` chain resolution.** Some real configs extend 3–4 levels deep across npm packages. Per-project one-time cost; should be sub-millisecond after first load.
- **Should we surface `paths` parse errors loudly?** A malformed `tsconfig.json` is a developer-facing problem; we probably want a clear error message rather than silently failing to resolve aliases.

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

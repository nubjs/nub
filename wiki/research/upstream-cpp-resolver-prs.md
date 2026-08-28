# Upstream PRs porting the ESM resolver to C++

**Date:** 2026-05-17 **Question:** Has anyone in `nodejs/node` attempted to port the algorithmic core of the ESM resolver (`moduleResolve` / `packageResolve` / `packageExportsResolve` / `resolvePackageTarget`) to C++?

## TL;DR

**No.** Nobody has an open or recent PR that ports the core ESM resolution algorithm to C++. All landed and in-flight C++ work on `src/node_modules.cc` is **scoped to leaf primitives**, and the algorithmic core still lives entirely in JavaScript.

The leaf primitives are `package.json` reading/caching, `findPackageJSON`, `getNearestParentPackageJSON`, `internalModuleStat`, and the legacy main resolver. The core — `moduleResolve`, `packageExportsResolve`, `resolvePackageTarget` recursion, `PATTERN_KEY_COMPARE`, pattern matching — is still entirely in `lib/internal/modules/esm/resolve.js`.

There is no tracking issue ("port ESM resolver to C++"). `nodejs/loaders` has no RFC for it. The closest open work is **maintaining** that JS resolver (Arcanis #62239 package maps; aduh95 #62080 path-normalization fix), not replacing it.

A native resolver therefore has to be written from scratch — but on a real foundation, since the `node_modules.cc` package-config cache and binding surface already exists and is actively maintained by Joyee Cheung, anonrig, and michaelsmithxyz.

## What HAS landed in C++ (the foundation)

The native work merged between 2023 and March 2026 covers the `package.json` config cache, native `import.meta` initialization, `internalModuleStat` fast paths, the legacy main resolver, and Windows path handling.

- **#50322** (closed, anonrig, 2023) — original `package_json_reader` cache move, eventually superseded
- **#48325** (merged, prior art) — `FSLegacyMainResolve` in C++
- **#54501 / #54648 / #54971 / #56629** (joyeecheung, 2024–25) — compile-cache plumbing, `module-sync` condition
- **#55777** (marco-ippolito, merged) — `import.meta.resolve` crash fix touching native side
- **#57286** (aduh95, merged Mar 2025) — `initializeImportMeta` ported to native
- **#57599** (dario-piotrowicz, merged) — `getPackageType` perf improvement
- **#58054** (anonrig, merged Apr 2025) — `internalModuleStat` V8 fast-path fix
- **#59888** (michaelsmithxyz, merged Sep 2025) — shrunk the nearest-parent package JSON cache
- **#60425** (michaelsmithxyz, merged Jan 2026) — cache *missing* `package.json` files in the C++ cache; bench shows up to 3x speedup on slow FS. Touches `src/node_modules.{cc,h}`.
- **#60575** (indutny, merged) — Win32 wide-string filename handling in the native path
- **#60603** (joyeecheung, closed Dec 2025) — moved `import.meta` initializer fully to native
- **#61548** (joyeecheung, merged Feb 2026) — "src: initial support for ESM in embedder API." Adds `ModuleData`, `ModuleFormat`, ESM-aware `LoadEnvironment`. Approved by addaleax. **Doesn't port the resolver**, but lands the C++ scaffolding for ESM execution and adds 16 lines to `src/node_modules.cc`. The "resource_name" plumbing is adjacent to what a native resolver would need.
- **#62101** (StefanStojanovic, merged Mar 2026) — long subpath import fix (Windows)

## In-flight / closely adjacent

Four open PRs sit next to a native resolver without being one: package maps, native cache invalidation, synchronous evaluate hooks, and a `vm`/loader primitives proposal.

- **#62239** (arcanis, open, Mar 2026) — `loader: implement package maps`. Adds `--experimental-package-map=<path>`. Implementation is **JS-side** (`lib/internal/modules/package_map.js`), only `node_options.{cc,h}` touched in C++. Jasnell engaged, no merge yet. Not a resolver port, but adjacent: package-manager-driven static resolution that bypasses `node_modules` walking.
- **#61767** (anonrig, open) — `module: add clearCache for CJS and ESM`. Native cache invalidation API.
- **#57139** (joyeecheung, open since Feb 2025) — synchronous module evaluate hooks. Stalled.
- **#62720** (joyeecheung, open) — "Proposal: new `vm` module primitives & loader API for ESM customization."

## Authors' recent module work (none is "port resolver to C++")

Each of the three contributors most active in Node's module system spent 2025–26 on the surrounding plumbing — perf primitives, the ESM embedder API, config and TypeScript stripping — and none filed a resolver port.

- **Yagiz Nizipli (anonrig)**: focused on perf primitives — TextEncoder, simdjson, http parser, Ada/URL, V8 fast paths. No resolver-port PR.
- **Joyee Cheung**: ESM embedder API (#61548), require(esm), compile cache, snapshot. Doing all the surrounding plumbing but has **not** filed a resolver port.
- **Marco Ippolito**: config file (`node.config.json`), TypeScript stripping, releases. Not touching the resolver.

## The reusable surface

For a native port, the C++ surface that already exists is:

- `src/node_modules.cc` + `node_modules.h` — package config cache, `findPackageJSON`, `getNearestParentPackageJSON`, `getPackageScopeConfig`, missing-file negative cache, simdjson parsing
- `src/path.cc` — URL/path normalization
- The internal binding `modules` exposed in `typings/internalBinding/modules.d.ts`

The algorithmic core has no C++ counterpart and still lives in `lib/internal/modules/esm/resolve.js`: the `moduleResolve` driver, `packageResolve` (bare specifier → package URL), the `packageExportsResolve` / `packageImportsResolve` / `resolvePackageTarget` recursion, `patternKeyCompare` and the `*` pattern semantics, and conditions matching (`node`, `import`, `require`, user-provided).

## Verdict

There is **no upstream draft to base a port on**: the algorithmic core has not been ported and no one is publicly working on it. The leaf primitives are landed and maintained, so a port inherits a non-trivial foundation.

Joyee's #61548 (ESM embedder) and michaelsmithxyz's #60425 (missing-file cache) are the most recent examples of the patterns to follow.

The absence implies either (a) the JS resolver isn't a hot enough path for Node maintainers to prioritize porting it, or (b) the spec complexity (pattern matching, conditions) makes the JS version preferred for maintainability. Both bear on whether a native port is worth attempting.

## Changelog

Records when this survey moved into the public corpus, and the `nodejs/node` snapshot date its findings still reflect.

- 2026-07-30 — Initial publication.
- 2026-08-28 — Trimmed to the measured findings and current behavior.

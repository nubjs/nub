# Upstream PRs porting the ESM resolver to C++

**Date:** 2026-05-17 **Question:** Has anyone in `nodejs/node` taken a swing at porting the BIG piece of the ESM resolver (`moduleResolve` / `packageResolve` / `packageExportsResolve` / `resolvePackageTarget`) to C++?

## TL;DR

**No.** Nobody has an open or recent PR that ports the core ESM resolution algorithm to C++. All landed and in-flight C++ work on `src/node_modules.cc` is **scoped to leaf primitives** — `package.json` reading/caching, `findPackageJSON`, `getNearestParentPackageJSON`, `internalModuleStat`, the legacy main resolver. The algorithmic core (`moduleResolve`, `packageExportsResolve`, `resolvePackageTarget` recursion, `PATTERN_KEY_COMPARE`, pattern matching) still lives entirely in `lib/internal/modules/esm/resolve.js`.

There is no tracking issue ("port ESM resolver to C++"). `nodejs/loaders` has no RFC for it. The closest open work is **maintaining** that JS resolver (Arcanis #62239 package maps; aduh95 #62080 path-normalization fix), not replacing it.

A native resolver therefore has to be written from scratch — but on a real foundation, since the `node_modules.cc` package-config cache and binding surface already exists and is actively maintained by Joyee Cheung, anonrig, and michaelsmithxyz.

## What HAS landed in C++ (the foundation)

- **#50322** (closed, anonrig, 2023) — original `package_json_reader` cache move, eventually superseded
- **#48325** (merged, prior art) — `FSLegacyMainResolve` in C++
- **#54501 / #54648 / #54971 / #56629** (joyeecheung, 2024–25) — compile-cache plumbing, `module-sync` condition
- **#55777** (marco-ippolito, merged) — `import.meta.resolve` crash fix touching native side
- **#57286** (aduh95, merged Mar 2025) — `initializeImportMeta` ported to native
- **#57599** (dario-piotrowicz, merged) — `getPackageType` perf improvement
- **#58054** (anonrig, merged Apr 2025) — `internalModuleStat` V8 fast-path fix
- **#59888** (michaelsmithxyz, merged Sep 2025) — shrunk the nearest-parent package JSON cache
- **#60425** (michaelsmithxyz, merged Jan 2026) — cache *missing* `package.json` files in the C++ cache; bench shows up to 3x speedup on slow FS. Touches `src/node_modules.{cc,h}`. **Useful to lift from.**
- **#60575** (indutny, merged) — Win32 wide-string filename handling in the native path
- **#60603** (joyeecheung, closed Dec 2025) — moved `import.meta` initializer fully to native
- **#61548** (joyeecheung, merged Feb 2026) — "src: initial support for ESM in embedder API." Adds `ModuleData`, `ModuleFormat`, ESM-aware `LoadEnvironment`. Approved by addaleax. **Doesn't port the resolver**, but lands the C++ scaffolding for ESM execution and adds 16 lines to `src/node_modules.cc`. The "resource_name" plumbing is adjacent to what a native resolver would need.
- **#62101** (StefanStojanovic, merged Mar 2026) — long subpath import fix (Windows)

## In-flight / closely adjacent

- **#62239** (arcanis, open, Mar 2026) — `loader: implement package maps`. Adds `--experimental-package-map=<path>`. Implementation is **JS-side** (`lib/internal/modules/package_map.js`), only `node_options.{cc,h}` touched in C++. Jasnell engaged, no merge yet. Not what we want, but relevant: package-manager-driven static resolution that bypasses `node_modules` walking.
- **#61767** (anonrig, open) — `module: add clearCache for CJS and ESM`. Native cache invalidation API.
- **#57139** (joyeecheung, open since Feb 2025) — synchronous module evaluate hooks. Stalled.
- **#62720** (joyeecheung, open) — "Proposal: new `vm` module primitives & loader API for ESM customization." Worth reading before committing to a native resolver design.

## Authors' recent module work (none is "port resolver to C++")

- **Yagiz Nizipli (anonrig)**: focused on perf primitives — TextEncoder, simdjson, http parser, Ada/URL, V8 fast paths. No resolver-port PR.
- **Joyee Cheung**: ESM embedder API (#61548), require(esm), compile cache, snapshot. Doing all the surrounding plumbing but has **not** filed a resolver port.
- **Marco Ippolito**: config file (`node.config.json`), TypeScript stripping, releases. Not touching the resolver.

## The reusable surface

For a native port, the C++ surface that already exists is:

- `src/node_modules.cc` + `node_modules.h` — package config cache, `findPackageJSON`, `getNearestParentPackageJSON`, `getPackageScopeConfig`, missing-file negative cache, simdjson parsing
- `src/path.cc` — URL/path normalization
- The internal binding `modules` exposed in `typings/internalBinding/modules.d.ts`

What would have to be written fresh:
- `moduleResolve` driver
- `packageResolve` (bare specifier → package URL)
- `packageExportsResolve` + `packageImportsResolve` + `resolvePackageTarget` recursion
- `patternKeyCompare` + pattern matching (the `*` glob semantics)
- Conditions matching (`node`, `import`, `require`, user-provided)

## Verdict

There is **no upstream draft to base a port on**. The big algorithmic core has not been ported and no one is publicly working on it. The leaf primitives are landed and maintained, so a port inherits a non-trivial foundation. Joyee's #61548 (ESM embedder) and michaelsmithxyz's #60425 (missing-file cache) are the most recent examples of the patterns to follow.

The absence is conspicuous — it implies either (a) the JS resolver isn't a hot enough path that Node maintainers prioritize porting it, or (b) the spec complexity (pattern matching, conditions) makes the JS version preferred for maintainability. Both bear on whether a native port is worth attempting.

## Changelog

- 2026-07-30 — Migrated from the internal research corpus; first-person framing rewritten. The upstream survey reflects `nodejs/node` as of 2026-05-17 and has not been re-run.

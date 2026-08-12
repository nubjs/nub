# Resolver-Patcher Relevance Audit (2026)

**Question:** of the canonical Node module-loading monkey-patchers, which are still live concerns in modern TypeScript stacks, and which are dead weight that Nub's native-first resolver can ignore?

**Scope:** Nub augments the user's installed Node through its own extension surfaces. The resolver fallback can either tolerate userland patches to `Module._extensions`, `Module._resolveFilename`, `Module._load`, `Module.prototype.require`, and `Module._cache`, or it can deliberately not — at the cost of breaking some packages. Knowing which patchers are alive in 2026 tells us how big that cost is.

All download numbers below are weekly counts pulled from the npm registry the week of 2026-05-10..2026-05-16. Trend numbers are 30-day rolling sums sampled from 2024-11-01, 2025-08-01, and 2026-05-01 (`https://api.npmjs.org/downloads/range/...`).

---

## 1. TL;DR table

| Library | Weekly DL (May 2026) | Trend 24→26 | Primary patch | Verdict | One-liner |
|---|---|---|---|---|---|
| `pirates` | 88.8M | 28M → 80M (3x) | `Module._extensions[ext]` + wrap `mod._compile` | **MATTERS** | Coordination layer used by `@babel/register`, `nyc`, `ts-node`; growth driven by Babel/coverage tools in every CI pipeline |
| `tsconfig-paths` | 67.0M | 33M → 72M (2.2x) | reassign `Module._resolveFilename` | **MATTERS** | Still the runtime path-alias solution for ts-node/Jest/NestJS; bundlers don't need it but server-side TS does |
| `tsx` | 58.0M | 3.9M → 35M (9x) | `Module._extensions` (CJS) + `module.register` ESM hook | **MATTERS** | The de-facto modern `ts-node` replacement; growth is the strongest signal in the set |
| `ts-node` | 44.7M | 24M → 43M (1.8x) | `require.extensions[.ts]` (= `Module._extensions`) + wrap `_compile` | **MATTERS** (legacy-heavy) | Still load-bearing for NestJS, ts-jest config loading, and `node -r ts-node/register`; no longer the default but flat-growing because of transitive use |
| `require-in-the-middle` | 40.5M | 7M → 32M (4.6x) | reassign `Module.prototype.require`, calls `Module._resolveFilename` | **MATTERS** | Hard dependency of `@opentelemetry/instrumentation`, `@sentry/node`, Datadog `dd-trace`, NewRelic; every observability stack patches require through this |
| `@babel/register` | 10.1M | 9M (flat) | wraps `pirates.addHook` (so `Module._extensions`) | **LEGACY_DEP** | Almost nobody uses it directly anymore; downloads come from older tooling and Babel-heavy monorepos pre-SWC |
| `module-alias` | 3.5M | 1.2M → 2.8M (2.3x) | reassign `Module._resolveFilename` + `Module._nodeModulePaths` | **MATTERS** (niche) | The non-TS path-alias library; still appears in plain Node services where tsconfig-paths doesn't fit |
| `proxyquire` | 1.2M | 0.59M → 1.03M (1.7x) | mutate `Module._cache`, override `require.extensions`, call `Module._load`/`_resolveFilename` | **LEGACY_DEP** | Mocha-era test mocking; Vitest/Jest provide this natively. Downloads trickle through old test suites |
| `mock-require` | 0.33M | 0.19M → 0.30M (1.6x) | reassign `Module._load`, mutates `require.cache`, uses `Module._resolveFilename` | **LEGACY_DEP** | Same niche as proxyquire but smaller. Survives because Vitest doesn't intercept `require()` |

**Every library on this list grew in raw downloads between Nov-2024 and May-2026**, and for tsx, require-in-the-middle and pirates the growth is large. What is obsolete is much narrower than the conventional "tsx replaced ts-node, Vite replaced everything else" narrative suggests.

---

## 2. Per-library audits

### 2.1 `pirates` (88.8M/wk)

- **What it does.** A coordination layer for require hooks: lets multiple tools register transforms on the same extension without clobbering each other.
- **What it patches.** `Module._extensions[ext]` — verified by reading `package/lib/index.js@4.0.7`: it captures the old loader, installs a new one that calls `mod._compile = compileWrapper`, then runs the chained loader. So pirates patches *both* `_extensions` (the registry) and per-module `_compile` (the content path). [npm](https://www.npmjs.com/package/pirates)
- **Modern relevance.** Active in 2026. Last release 2025-03-27 (v4.0.7). Downloads grew from ~28M/wk to ~80M/wk over 18 months. Pirates is the substrate `@babel/register` uses (`pirates.addHook(compile, {exts})` is the whole hook code in `@babel/register@7.29.3/lib/hook.cjs`), and `nyc`/istanbul-style coverage runners pipe their instrumentation through it. So any CI pipeline that uses Babel-as-transformer-of-record (still common in Vue 2 holdovers, RN <0.71, large React monorepos with custom Babel plugins) drags pirates in.
- **Who depends on it.** `@babel/register`, `nyc`, `babel-plugin-istanbul`'s callers, `mocha --require @babel/register`, several test runners. Not directly in Next 14+/Vite/SvelteKit graphs (those use SWC/esbuild/Rolldown), but ubiquitous in Jest+Babel CI.
- **Verdict.** **MATTERS.** Patches one of the four extension slots Nub's CJS resolver hands out.

### 2.2 `tsx` (58.0M/wk)

- **What it does.** Drop-in `node`-replacement for running TypeScript files; uses esbuild to strip types and transform ESM/CJS interop.
- **What it patches.** CJS side: `Module._extensions['.ts'/'.tsx'/'.jsx'/'.mjs'/'.js']` plus `mod._compile`. ESM side: registers a Node loader hook via `module.register()` (the modern, supported API — not a Module-internals patch). Verified via `src/cjs/api/module-extensions.ts`.
- **Modern relevance.** Highest growth of any library here: 3.9M → 35M weekly (9x in 18 months). Last release 2026-05-17 (v4.22.1). Node's built-in type stripping (stable since v25.2.0 / v24.12.0, default since v23.6) covers *erasable* TS only — no enums, no decorators, no transform-on-import-with-paths. tsx remains the universal answer for "I want to run my actual TS file." [LogRocket: tsx vs ts-node vs native](https://blog.logrocket.com/running-typescript-node-js-tsx-vs-ts-node-vs-native/), [PkgPulse 2026](https://www.pkgpulse.com/blog/tsx-vs-ts-node-vs-bun-2026).
- **Who depends on it.** Direct user-facing CLI; not typically a transitive dep. Used by developers, by `package.json` scripts in modern repos, by tools like Drizzle Kit, by `dotenvx`, by lots of monorepo "exec the TS config" patterns.
- **Verdict.** **MATTERS.** The CJS path patches `_extensions`; the ESM path uses the official `module.register` API and is something Nub should support natively anyway (it's not a patch in the monkey sense).

### 2.3 `ts-node` (44.7M/wk)

- **What it does.** Older TypeScript runner; spawns `tsc` (or a faster transformer) on each file.
- **What it patches.** `require.extensions[ext]` (an alias for `Module._extensions[ext]`) for `.ts`, `.tsx`, `.cts`, `.mts`; wraps `mod._compile` to call `service.compile(code, filename)` first; uses `Module._preloadModules` for `--require`. ESM is via the older `--loader` hook system.
- **Modern relevance.** Last release 2025-10-13 (v10.9.2), no major version in 3 years; growth from 24M → 43M weekly is real but flat-ish in % terms (1.8x vs. tsx's 9x). NestJS still uses it via `node --inspect-brk -r tsconfig-paths/register -r ts-node/register node_modules/.bin/jest` in its monorepo. ts-jest loads it. `nestjs/nest` v12 (Q3 2026) plans Vitest migration but CJS projects continue on Jest+ts-node. [InfoQ NestJS v12 Roadmap](https://www.infoq.com/news/2026/04/nestjs-12-roadmap-esm/).
- **Who depends on it.** `ts-jest` (always), Jest's TS config file loader (transitively), NestJS CLI / `@nestjs/testing`, many older boilerplates, `typeorm` migrations runner.
- **Verdict.** **MATTERS** but legacy-flavored. Patches the same `_extensions` slot pirates does. Most new projects pick tsx, but the install base is huge and not shrinking.

### 2.4 `tsconfig-paths` (67.0M/wk)

- **What it does.** Resolves `tsconfig.json` `paths` aliases at runtime in Node.
- **What it patches.** Reassigns `Module._resolveFilename`. Verified in `src/register.ts`: stores the original, replaces with a function that checks `matchPath(request)` before delegating.
- **Modern relevance.** Downloads grew 33M → 72M weekly in 18 months. Last release 2025-10-14 (v4.2.0). The conventional wisdom is "bundlers handle paths so this is dead" — but that's only true for bundled targets. Server-side TS (NestJS, ts-jest, ad-hoc `node -r ts-node/register`, migration scripts, seeders) all need *runtime* path resolution because the bundler isn't in the loop. NestJS docs and the NestJS monorepo itself use `tsconfig-paths/register`. [NestJS TS config DeepWiki](https://deepwiki.com/nestjs/nest/12.1-typescript-configuration). Next.js's transitive tree pulls it indirectly via `jest-resolve` and `ts-jest` paths.
- **Who depends on it.** `ts-jest`, `ts-node`'s ecosystem, NestJS, Jest config loading, every "we have TS paths but also run scripts with node" setup.
- **Verdict.** **MATTERS.** The single most important `_resolveFilename` patcher.

### 2.5 `module-alias` (3.5M/wk)

- **What it does.** Older non-TS alternative to TS paths: define `_moduleAliases` in `package.json`, get `require('@foo/bar')` rewritten.
- **What it patches.** Reassigns `Module._resolveFilename` *and* `Module._nodeModulePaths`. Verified in `index.js`.
- **Modern relevance.** Smaller than tsconfig-paths and likely shrinking in *share* even as raw downloads grew 1.2M → 2.8M weekly. Last release 2026-02-05 (v2.3.4). Used in plain-JS Node services and older Express boilerplates that predate TS paths. [PkgPulse comparison 2026](https://www.pkgpulse.com/blog/tsconfig-paths-vs-module-alias-vs-pathsify-2026).
- **Who depends on it.** Mostly direct adoption; not a heavy transitive presence. Some legacy Express templates, some Strapi-era code, some self-hosted apps.
- **Verdict.** **MATTERS** but niche. Same patch surface as tsconfig-paths (`_resolveFilename`) plus the unusual `_nodeModulePaths` reassignment, which is a worse interop story.

### 2.6 `require-in-the-middle` (40.5M/wk)

- **What it does.** Lets observability tools hook every `require()` call and replace/wrap exports for instrumentation.
- **What it patches.** Reassigns `Module.prototype.require` (not `Module._load` — they took the prototype path so each module has its own require closure intact). Calls `Module._resolveFilename` for filename normalization. Verified in `index.js@8.0.1`.
- **Modern relevance.** Repo lives in the `nodejs` GitHub org. Downloads grew 7M → 32M weekly (4.6x in 18 months — driven by OpenTelemetry adoption). Last release 2025-10-20 (v8.0.1). Hard dependency of:
  - `@opentelemetry/instrumentation` (all OTel auto-instrumentations) — [docs](https://opentelemetry.io/docs/languages/js/instrumentation/)
  - `@sentry/node` (via the OTel instrumentation packages it now uses post-v8) — [Sentry docs](https://docs.sentry.io/platforms/javascript/guides/node/)
  - `dd-trace` (Datadog APM)
  - NewRelic agent
- **Who depends on it.** Almost every observability/APM library in the Node ecosystem.
- **Verdict.** **MATTERS** — possibly the single most important patcher on the list for production app compatibility. If Nub breaks RITM, it breaks OpenTelemetry, Sentry, Datadog, NewRelic simultaneously.

### 2.7 `proxyquire` (1.2M/wk)

- **What it does.** Test-time dependency stubbing: `proxyquire('./module', { dep: stub })` returns a version of `./module` with `dep` swapped out.
- **What it patches.** Temporarily mutates `Module._cache` (to evict cached modules and reload with stubs), overrides `require.extensions[ext]` per file to inject a stubbed require, calls `Module._resolveFilename` and `Module._load` directly, reads `Module.globalPaths`. Verified in `lib/proxyquire.js`.
- **Modern relevance.** Last release 2023-07-15 (v2.1.3, no updates in 2+ years). Downloads grew 0.59M → 1.03M weekly — slow but real growth, from a small base. Vitest does not intercept `require()` ([discussion #3134](https://github.com/vitest-dev/vitest/discussions/3134)), so old CommonJS Mocha test suites that need require-mocking still reach for proxyquire or rewiremock. Jest's `jest.mock()` covers most use cases natively.
- **Who depends on it.** Test suites in older OSS libraries (lots of npm packages from the 2014–2019 era). Not in modern framework dependency trees.
- **Verdict.** **LEGACY_DEP.** Real usage but obsolete tooling pattern. Modern test runners provide the feature natively.

### 2.8 `mock-require` (0.33M/wk)

- **What it does.** Same idea as proxyquire — register a mock for module name `X` globally before requiring `Y`.
- **What it patches.** Reassigns `Module._load`; mutates `require.cache`; uses `Module._resolveFilename`; reads `Module.globalPaths`. Verified in `index.js`.
- **Modern relevance.** Last release 2023-06-09 (v3.0.3). Downloads grew slightly (0.19M → 0.30M weekly) — interestingly, Vitest's lack of `require()` interception is driving *some* increased adoption as an explicit workaround. But the absolute number is small.
- **Who depends on it.** Mocha test suites; small.
- **Verdict.** **LEGACY_DEP.** Bottom of the relevance pile in this audit.

### 2.9 `@babel/register` (10.1M/wk)

- **What it does.** Lets Node `require()` `.js` files that contain Babel-syntax (JSX, stage-X proposals, decorators) by transforming them on load.
- **What it patches.** Calls `pirates.addHook(compile, {exts: ...})`. Source: `package/lib/hook.cjs`. So the actual Node-internals patch (`Module._extensions[ext]`) is delegated to pirates.
- **Modern relevance.** 10M weekly downloads, but the trend is flat-to-declining once you account for ecosystem growth. SWC displaced Babel in Next.js (17x faster, default since Next 12), `@swc/jest` displaced it for Jest, and esbuild displaced it elsewhere. Last release 2026-05-12 (v7.29.3 — Babel monorepo gets fortnightly releases regardless of whether babel-register specifically changed). [Joshuakgoldberg: Jest babel→swc](https://www.joshuakgoldberg.com/blog/jest-babel-to-swc/).
- **Who depends on it.** Mocha+Babel test suites, some monorepos with custom Babel plugins, older Storybook configs. Not in Next 14+, Vite, Nuxt 3+, Astro, SvelteKit graphs.
- **Verdict.** **LEGACY_DEP.** It still installs because pirates is in everyone's tree, but `@babel/register`-the-entry-point is rarely user-facing in modern stacks.

---

## 3. The reassignment vs. content-mutation split

This grouping is the most design-relevant for a native resolver, because the two styles need different detection strategies.

### A. Slot reassignment (replace a single function on `Module`)

These rebind a Module-level function to a wrapper. Detection means "snapshot the original at boot and check on resolve."

- `tsconfig-paths` — `Module._resolveFilename = wrapper`
- `module-alias` — `Module._resolveFilename = wrapper` + `Module._nodeModulePaths = wrapper`
- `require-in-the-middle` — `Module.prototype.require = wrapper`
- `mock-require` — `Module._load = wrapper`

### B. Map mutation (mutate an entry in a per-extension dict)

These poke individual keys in `Module._extensions` (the per-extension loader map) and chain old loaders.

- `pirates` — `Module._extensions[ext] = wrapper` for each registered ext
- `@babel/register` — same, via pirates
- `ts-node` — `require.extensions[ext] = wrapper` for `.ts`/`.tsx`/`.cts`/`.mts`
- `tsx` — same, for `.ts`/`.tsx`/`.jsx`/`.mjs`/`.js` (CJS side only)

### C. Cache + content + slot, all at once (the worst kind)

- `proxyquire` — mutates `Module._cache`, overrides `require.extensions`, *calls* (doesn't reassign) `Module._load` and `Module._resolveFilename`. It temporarily corrupts state.

### Why this split matters for Nub

If Nub's native resolver runs in C/Rust, it has no automatic awareness of userland JS reassignments to `Module._resolveFilename`. Two strategies:

1. **Trampoline:** Always call out to JS `Module._resolveFilename` from the native path, so userland reassignments are honored. Cost: ~200–500ns per resolve, plus loss of native parallelism for the resolve itself.
2. **Detect and fall back:** Snapshot `Module._resolveFilename`, `Module._load`, `Module.prototype.require`, and the contents of `Module._extensions` at startup. On each resolve, do an identity check; if patched, fall back to legacy JS resolver. Cost: cheap when unpatched, full fallback when patched.

The latter is the obvious choice and pairs naturally with the additivity criterion. Map-mutation patchers (group B) are easy to detect — check whether `_extensions[ext]` identity differs from the boot snapshot for any ext Nub cares about — and slot reassignment (group A) is equally easy. The proxyquire-style transient mutation (group C) is harder, but proxyquire is `LEGACY_DEP`, so we can document it as unsupported in the native path and trampoline anyway.

---

## 4. What the modern stack actually needs

| Consumer | Resolution / transform path | Patches that matter |
|---|---|---|
| Next.js 14+ / 15 | Native paths resolution via SWC; Webpack/Turbopack handles aliases, so app code needs no `tsconfig-paths` | RITM, pulled in transitively by Sentry/OTel instrumentation |
| Vite / SvelteKit / Nuxt 3+ / Astro | Native TS, native paths, esbuild/Rolldown transforms; SSR servers on Node typically use Vite's dev server (no `_extensions` patch) | RITM, via production servers + APM |
| Remix | Bundler handles paths and TS | RITM, when used with Sentry/OTel/Datadog |
| Vitest | Vite's transform pipeline; does not intercept `require()`, no `_extensions` patch in its own loader | Nothing from this list directly, but consumer test suites still drag in proxyquire/mock-require and ts-jest + tsconfig-paths |
| Jest | Own resolver + transform layer; `ts-jest` brings `ts-node` for config and `tsconfig-paths` for path mapping, `babel-jest` brings pirates transitively via the `@babel/register` chain | pirates, ts-node, tsconfig-paths |
| NestJS | The biggest holdout for these patches in production-Node code: `node -r ts-node/register -r tsconfig-paths/register dist/main.js`, or `nest start --watch` | ts-node (`_extensions`), tsconfig-paths (`_resolveFilename`) |
| OpenTelemetry / Sentry / Datadog / NewRelic | All four RITM-first; Sentry v8+ delegates to OTel instrumentation packages, which depend on `@opentelemetry/instrumentation`, which depends on `require-in-the-middle` directly | RITM, hard |
| Plain `node script.ts` developers | tsx, and increasingly bare Node with `--experimental-strip-types`; tsx patches `_extensions` on the CJS side and registers an ESM loader on the ESM side | tsx (`_extensions` for CJS) |

### So the load-bearing set in 2026 is:

1. `require-in-the-middle` — patches `Module.prototype.require`. Touches every production app with APM/observability.
2. `tsconfig-paths` — patches `Module._resolveFilename`. Touches every NestJS + ts-jest stack.
3. `pirates` — patches `Module._extensions[ext]`. Touches every Babel/nyc-coverage CI pipeline.
4. `ts-node` — patches `Module._extensions[ext]` for `.ts`. Touches NestJS, ts-jest config loading, any `-r ts-node/register` invocation.
5. `tsx` — patches `Module._extensions[ext]` (CJS) + uses `module.register()` (ESM, modern API). Touches every modern "run TS in dev" workflow.

Five libraries. Two patch sites: `Module._resolveFilename` (slot reassignment) and `Module._extensions[ext]` (map mutation). Plus `Module.prototype.require` (one library, but the most important one).

The rest — `module-alias`, `proxyquire`, `mock-require`, and directly-invoked `@babel/register` — are real but small. Not zero, and they will surface in bug reports, but they don't shape the architecture.

---

## 5. Bottom-line for Nub

Nub's native-first resolver needs to handle **three patch sites**, not nine:

| Patch site | Why | Detection | Fallback strategy |
|---|---|---|---|
| `Module._resolveFilename` reassignment | tsconfig-paths, module-alias | Identity check vs. boot snapshot | If patched, route resolves through JS `_resolveFilename` (trampoline). Cost is real but only on patched processes. |
| `Module._extensions[ext]` mutation | pirates, ts-node, tsx, @babel/register | Per-extension identity check vs. boot snapshot for the four extensions that matter (`.js`, `.cjs`, `.mjs`, `.ts`) | If `_extensions['.ts']` (or whichever) differs from boot value, fall back to legacy Node load for that extension. Native fast path stays for unpatched extensions. |
| `Module.prototype.require` reassignment | require-in-the-middle (so: OTel, Sentry, Datadog, NewRelic) | Identity check at `Module` prototype | If patched, every `require()` must go through the JS wrapper. Nub can't bypass — the whole observability ecosystem assumes interception works. |

Everything else can be relegated to the "best-effort, fall back to legacy" bucket without meaningful collateral damage.

What Nub explicitly does *not* need to special-case:

- `proxyquire`'s temporary `Module._cache` mutation — test-time only, niche, document as fall-back-to-legacy.
- `mock-require`'s `Module._load` reassignment — same.
- `Module._nodeModulePaths` reassignment (only `module-alias`) — rare enough to ignore unless we hear about breakage.
- `Module._findPath` — *no library on this list patches it directly*. Surprising but verified.

Concrete recommendation for the resolver design: at process boot, snapshot

```
const ORIG = {
  resolveFilename: Module._resolveFilename,
  load: Module._load,
  require: Module.prototype.require,
  extensions: { '.js': Module._extensions['.js'],
                '.cjs': Module._extensions['.cjs'],
                '.mjs': Module._extensions['.mjs'],
                '.ts':  Module._extensions['.ts'] }
};
```

then on each native resolve, do a four-identity check. If all four match boot, take the native path. If any differ, fall back to JS for that resolve. This is one branch + four pointer compares per resolve in the hot path — sub-100ns overhead — and gives us correctness against every patcher in this audit, including the dead ones.

The relevant additivity claim becomes: *Nub's native resolver is observably indistinguishable from Node's when any monkey-patcher in this set is active*, because it routes around itself the moment it detects the patch. We get full ecosystem compatibility (RITM-based APMs work, ts-node works, NestJS works, tsx works) without paying their cost in the unpatched fast path.

---

## Sources

- [npm: pirates](https://www.npmjs.com/package/pirates), [npm: tsx](https://www.npmjs.com/package/tsx), [npm: ts-node](https://www.npmjs.com/package/ts-node), [npm: tsconfig-paths](https://www.npmjs.com/package/tsconfig-paths), [npm: module-alias](https://www.npmjs.com/package/module-alias), [npm: require-in-the-middle](https://www.npmjs.com/package/require-in-the-middle), [npm: proxyquire](https://www.npmjs.com/package/proxyquire), [npm: mock-require](https://www.npmjs.com/package/mock-require), [npm: @babel/register](https://www.npmjs.com/package/@babel/register)
- Source code verified locally via `npm pack` for pirates@4.0.7 (`lib/index.js` lines 96–148, addHook implementation), require-in-the-middle@8.0.1 (`index.js`, `Module.prototype.require` reassignment), @babel/register@7.29.3 (`lib/hook.cjs` calls `pirates.addHook`).
- Download trends: `https://api.npmjs.org/downloads/range/2024-01-01:2026-05-01/<pkg>`, sampled at three points.
- [Node.js TypeScript docs (v26)](https://nodejs.org/api/typescript.html) — type stripping stable in v25.2.0/v24.12.0
- [LogRocket — tsx vs ts-node vs native (2026)](https://blog.logrocket.com/running-typescript-node-js-tsx-vs-ts-node-vs-native/)
- [PkgPulse — tsx vs ts-node vs Bun (2026)](https://www.pkgpulse.com/blog/tsx-vs-ts-node-vs-bun-2026)
- [OpenTelemetry JS Instrumentation](https://opentelemetry.io/docs/languages/js/instrumentation/) — RITM as core dependency
- [Sentry Node.js docs](https://docs.sentry.io/platforms/javascript/guides/node/) — uses OTel + RITM under the hood
- [InfoQ — NestJS v12 Roadmap (April 2026)](https://www.infoq.com/news/2026/04/nestjs-12-roadmap-esm/) — NestJS migrating to Vitest+ESM but CJS path keeps ts-node
- [NestJS TS configuration (DeepWiki)](https://deepwiki.com/nestjs/nest/12.1-typescript-configuration) — `-r ts-node/register -r tsconfig-paths/register` is the canonical incantation
- [Vitest discussion #3134](https://github.com/vitest-dev/vitest/discussions/3134) — does not intercept `require()`, keeps mock-require/proxyquire alive
- [Joshua Goldberg — Switching Jest from Babel to SWC](https://www.joshuakgoldberg.com/blog/jest-babel-to-swc/)
- [Next.js Architecture: Compiler](https://nextjs.org/docs/architecture/nextjs-compiler) — SWC default since v12, 17x faster than Babel
- [PkgPulse — tsconfig-paths vs module-alias 2026](https://www.pkgpulse.com/blog/tsconfig-paths-vs-module-alias-vs-pathsify-2026)

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links and reference-checkout paths were rewritten; findings and measured values are unchanged.

# Monkey-patching of Node's CJS resolver: blast-radius estimate

Status: 2026-05-17. Inputs: npm weekly downloads (api.npmjs.org), GitHub code search, package source on GitHub `main`, Node TSC tracking issues.

## 1. TL;DR

If Nub ships a native CJS resolver that does not honor writes to `Module._findPath`, `Module._resolveFilename`, `Module._cache`, `Module._extensions`, `require.cache`, `require.extensions`, or `Module.prototype.require`, **essentially every non-trivial Node application built in the last decade will see something break or silently lose functionality.** The blast radius is dominated by three transitive vectors:

1. **`pirates`** (~88M weekly DLs) — patches `Module._extensions`; pulled in by `@babel/register`, `esbuild-register`, `@swc-node/register`, and most "register a custom file extension" workflows. Anything that runs `.ts`, `.coffee`, `.mdx`, etc. via require goes through it.
2. **`require-in-the-middle`** (~40M weekly DLs) — patches `Module._resolveFilename` and writes into `require.cache`; the universal CJS interception layer for **every** APM agent (Datadog, New Relic, Elastic, OpenTelemetry, Sentry transitively).
3. **`tsconfig-paths`** (~67M weekly DLs) + **`module-alias`** (~3.5M) + **`app-module-path`** (~3.6M) — all patch `Module._resolveFilename` / `Module._nodeModulePaths` to add path aliases. Every TS monorepo using `paths` in `tsconfig.json` at runtime touches this.

A conservative read: any production Node service that ships with an APM agent (probably >60% of commercial Node deployments), any TS app using `tsx`/`ts-node`/path aliases (the default modern stack), and anything booting through a Babel/SWC register all break or degrade silently. `module.registerHooks()` (stable since Node 22.15 / 24.x, landed by Joyee Cheung end of 2024) is the supported replacement, but as of mid-2026 **none of the load-bearing libraries above have completed migration to it**.

## 2. The big three

| Package | Weekly DLs | Patches | What it enables |
|---|---|---|---|
| `pirates` | **88,817,599** | `Module._extensions[ext]` (read + overwrite, with revert) | The substrate for every `*-register` package. `pirates.addHook(compile, { exts: [...] })` swaps `_extensions['.ts']` etc. |
| `require-in-the-middle` | **40,489,084** | `Module._resolveFilename` (overrides dispatcher), writes wrapped exports back into `require.cache` | Synchronous, in-process patching of any third-party module by name. Universal APM substrate. |
| `import-in-the-middle` | **50,803,948** | ESM loader via `module.register()` hooks + message channel | The ESM counterpart. Less Module-internals-y but still relies on Node's loader-hooks ABI. |

Source confirmations (read out of `main` on GitHub):

- `pirates/lib/index.js` lines 96–148: explicit `Module._extensions[ext] = newLoader` writes and `delete` on revert.
- `require-in-the-middle/index.js` lines 49–185: explicit comments about caching into `require.cache` "like `@babel/register`", aborts if `Module._resolveFilename` isn't a function, overrides it.
- `import-in-the-middle/index.js` uses the official `module.register()` ESM hooks API (transferable message ports) — it is **not** monkey-patching internals, it's using the supported API. It will keep working as long as Nub implements ESM loader hooks.

Dependent counts (from npm registry search): `module-alias` lists ~60k dependents, `tsconfig-paths` ~29k. The npm search index returns the same `1,803,946` number for both `*-in-the-middle` packages, which is a search-system artifact rather than a real count — but the weekly-download numbers (40M / 50M) make the actual transitive reach unambiguous: these aren't packages people install directly, they show up because someone's APM agent pulled them in.

## 3. APM landscape

Every Node APM agent in production today depends on `require-in-the-middle` and/or `import-in-the-middle`. Confirmed by reading their `package.json` files on `main`:

| Vendor | npm package | Weekly DLs | Patcher dep | Migrating to `registerHooks`? |
|---|---|---|---|---|
| Datadog | `dd-trace` | 7,541,317 | `import-in-the-middle ^3.0.1` (RITM via dd-trace/init.js chain) | No public PR; tracked under nodejs/node#56241 as a "nice to have" advocacy target |
| New Relic | `newrelic` | 1,508,018 | `require-in-the-middle ^8.0.1`, `import-in-the-middle ^3.0.1`, plus their own `@apm-js-collab/tracing-hooks ^0.7.0` | The 2025 refactor (`d4b4f11`) moved them onto `require-in-the-middle` (away from internal globalThis instrumentation); no further move yet |
| Elastic APM | `elastic-apm-node` | 335,364 | `require-in-the-middle ^8.0.0`, `import-in-the-middle 1.15.0` | Elastic owns the `require-in-the-middle` repo; no public migration PR |
| OpenTelemetry | `@opentelemetry/instrumentation` | **82,260,382** | `require-in-the-middle ^8.0.0`, `import-in-the-middle ^3.0.0` | Open issue, no committed plan |
| Sentry | `@sentry/node` | 23,500,664 | Transitive via every `@opentelemetry/instrumentation-*` package it depends on (20+ of them) | Inherits OTel's timeline |
| AppDynamics / Dynatrace | (private bundles) | n/a | Use proprietary patches plus RITM in many SKUs | Opaque |

The OpenTelemetry number is the load-bearing one: 82M weekly downloads of `@opentelemetry/instrumentation` means RITM is on the dependency graph of essentially every observability-instrumented Node service worldwide.

## 4. Direct-patcher packages

Source-confirmed patch surface (read from `main` on GitHub today):

| Package | Weekly DLs | Patches | Replaceable with `registerHooks`? |
|---|---|---|---|
| `tsx` | **58,050,982** | `Module._resolveFilename`, `Module._extensions` (`src/cjs/api/register.ts` and `module-resolve-filename/*`) | Partially. Resolution can move to a `resolve` hook; the .ts→.js extension/loader patching could move to ESM `load`. Real work for the tsx team. |
| `ts-node` | 44,686,925 | `Module._resolveFilename` + `Module._findPath` (`src/cjs-resolve-hooks.ts`) | Same — resolve hook covers it, but unmaintained today. |
| `tsconfig-paths` | **66,968,595** | `Module._resolveFilename` (`src/register.ts:103`) | Yes, cleanly — pure resolve override. |
| `jest-resolve` / `jest` | 48M / 44M | Constructs its own resolver per-test-environment; does not globally patch `Module._resolveFilename` (verified — no hits in `facebook/jest` search). Uses `require.cache` reads for module identity. | Mostly safe; will degrade if `require.cache` writes aren't observed. |
| `@babel/register` | 10,124,617 | Via `pirates` (`packages/babel-register/src/hook.ts:3`) → `Module._extensions` | Yes, via loader hooks. |
| `esbuild-register` | 13,150,545 | Via `pirates` + reads `module.Module._extensions` | Yes. |
| `@swc-node/register` | 3,054,907 | Via `pirates` | Yes. |
| `@swc/register` | 138,095 | Via `pirates` | Yes. |
| `mock-require` | 326,355 | `Module._load`, `Module._resolveFilename`, `require.cache` deletes | Resolve hook covers it; load interception is harder. |
| `proxyquire` | 1,193,314 | `Module._load`, `Module._resolveFilename`, swaps `Module._cache` wholesale | **Hard.** It literally reassigns `require.cache = Module._cache = {}` to sandbox a load. Needs a sandbox API that doesn't exist. |
| `module-alias` | 3,452,581 | `Module._nodeModulePaths`, `Module._resolveFilename`, `require.cache` deletes | Yes — resolve hook + a `clearCache` API. |
| `app-module-path` | 3,644,690 | `Module._nodeModulePaths` (`lib/index.js:5–31`) | Yes — straightforward resolve hook. |
| `esm` (legacy) | 6,255,997 | Heavy `Module._extensions` and `_resolveFilename` patching | Abandoned since 2020; users should be off it. Keep it broken. |
| `pirates` | 88,817,599 | `Module._extensions` (above) | Yes — `pirates` itself could be reimplemented on `registerHooks` and most of the register-* ecosystem would migrate "for free". This is the single highest-leverage upstream PR. |

Surprising find: **`mock-require` patches `Module._load`, not `_resolveFilename`** (line 15). That is a fourth patch target, distinct from the three that dominate the download-weighted picture.

## 5. GitHub-wide pattern hits

Raw `gh api search/code` totals (capped at 1000 by GitHub, so these are lower bounds):

| Pattern | JS files | TS files |
|---|---|---|
| `Module._findPath =` | 385 | 17 |
| `Module._resolveFilename =` | 1,280 | 340 |
| `Module.prototype.require =` | 1,440 | 505 |
| `Module._cache[` | 880 | 133 |
| `Module._extensions[` | (rate-limited) | (rate-limited) |

Distribution from sampling the top 30 hits per pattern:

- `Module._resolveFilename =` hits are dominated by **legitimate published libraries** (tsx, ts-node, tsconfig-paths, proxyquire, parcel/package-manager, electron, codesandbox/node-services, stenciljs/core, yarnpkg/berry, vscode), not test scaffolding. The "library implementing path resolution as a feature" cohort is large.
- `Module.prototype.require =` is the noisiest pattern — many hits are tutorials/blogs in Chinese-language repos rather than production code. Real offenders include Meteor's modules-runtime-hot, Rushstack's `rundown`, dremio's `dynLoader`, iron-node, fs-monkey, scrybble.
- `Module._cache[` is split roughly 50/50 between cache-busting in test runners and real production patches (Cypress packherd-require, RocketChat message-parser, jashkenas/coffeescript, electron-vite bytecode plugin, ts-node, replayio).
- `Module._findPath =` is rarer and almost exclusively patcher-libraries (ts-node, yarn pnp, fs-monkey, asar-node, require-hacker, native-ext) — but the long tail of downstream consumers is huge.

The cleaner signal isn't the raw search count, it's: **every search returns at least one item from the top-100-most-depended-on packages** (yarn berry, electron, parcel, ts-node, tsx, jest's parent project, OpenTelemetry-adjacent). There's no `Module._*` write pattern with hits exclusively in tests/scratch repos.

## 6. Upstream migration signals

What has Node upstream said:

- **`module.registerHooks()` landed Dec 2024** (Joyee Cheung, [nodejs/node tracking issue #56241](https://github.com/nodejs/node/issues/56241)). Stated motivation: "to help use cases like require-in-the-middle." Synchronous, in-thread, covers `require()`, `import()`, and `createRequire()`.
- **Issue #56241** explicitly lists, under "Nice to have," *"Advocating it to popular npm packages doing CJS monkey-patching to reduce the overall dependency of CJS loader internals in the ecosystem."* — i.e. Node TSC has not pushed migration, only made the alternative available.
- **`require.resolve` was routed through `registerHooks`** in [PR #62028](https://github.com/nodejs/node/pull/62028) so user-provided resolve hooks see `require.resolve()` calls too. Recent (late 2025).
- Joyee's [Dec 2025 "from experiment to stability" blog post](https://joyeecheung.github.io/blog/2025/12/30/require-esm-in-node-js-from-experiment-to-stability/) acknowledges the constraint: *"Given how widely the internals of the module loader have been monkey-patched and depended on, it's impossible to change it without breaking someone somewhere."* No deprecation timeline, no migration deadline.
- No public deprecation of `Module._findPath` / `Module._resolveFilename` / `Module._cache` / `Module._extensions` has been issued by Node. They remain undocumented-but-de-facto-public.

Ecosystem response:

- **None of the APM agents have migrated.** New Relic's 2025 refactor (commit `d4b4f11`) moved *to* `require-in-the-middle`, not away from it. Datadog, Elastic, OTel still depend on RITM ^8 / IITM ^3.
- **Sentry** ships RITM transitively and has been actively patching around bundler-noise issues with it (e.g. [getsentry/sentry-javascript#15209](https://github.com/getsentry/sentry-javascript/issues/15209) "Critical dependency warning with require-in-the-middle after upgrading to Sentry 8.52.0"). That issue thread is about webpack noise, not migration.
- **No public migration PR exists for `pirates`** to `registerHooks`. That's the single most-leveraged unfixed package.
- The community discussion is muted because Node hasn't threatened breakage. Everyone is waiting for someone else to move first.

## 7. Bottom-line estimate

Conservative read of "what breaks if Nub's native resolver ignores writes to `Module._*` / `require.cache`":

- **Anything booting through `tsx` or `ts-node`** (~100M combined weekly downloads of tsx + ts-node): module resolution diverges. Most TS-in-dev workflows.
- **Anything using TS path aliases at runtime via `tsconfig-paths`** (~67M weekly): aliases unresolved, app crashes at import-time.
- **Every APM-instrumented Node service**: instrumentation silently no-ops. The app keeps running, the dashboard goes dark. This is the worst failure mode — **silent loss of observability**, not a crash. Affects OTel (82M weekly), Sentry node (23M), Datadog (7.5M), New Relic (1.5M), Elastic (0.3M). Effectively all production Node-with-monitoring.
- **Anything using a `*-register` to load non-JS files** (Babel register 10M, esbuild-register 13M, swc-node 3M, plus the `pirates`-using long tail): file just won't load, hard crash at first non-JS require.
- **Mock-heavy test suites** using `proxyquire` (1.2M) or `mock-require` (0.3M): tests fail loudly. Less serious (CI catches it).
- **Module-alias / app-module-path consumers** (~60k packages): hard crash at first aliased require.

Rough ceilings:
- **~90% of modern Node TS dev workflows** are touched (tsx + ts-node + tsconfig-paths are nearly universal).
- **~70–80% of production Node services** running with any observability vendor will silently lose instrumentation.
- **~40–60% of npm packages by download volume** have at least one of {pirates, RITM, IITM, tsconfig-paths, module-alias} somewhere in their dep tree.
- **Replaceable-with-`registerHooks` fraction**: roughly 80% of the patches above. The hard holdouts are `proxyquire` (sandboxed cache), `mock-require` (`Module._load` interception), legacy `esm`, and anything doing wholesale `Module._cache` replacement.

What the numbers imply for any resolver that does not honor these writes:

- The silent-failure mode (APMs going dark) is the strongest argument for failing loudly and early. A hard error at boot when `Module._resolveFilename =` is assigned-to is safer than letting the write succeed but be ignored — the application crashes instead of running uninstrumented for a week.
- The highest-leverage single change available in the ecosystem is a `registerHooks`-based reimplementation of `pirates`, which would carry `@babel/register`, `esbuild-register`, `@swc-node/register`, and the long tail with it.
- The next-highest is `require-in-the-middle` on `registerHooks`. Elastic owns that repository.

Sources:
- [nodejs/node#56241 — module.registerHooks() tracking](https://github.com/nodejs/node/issues/56241)
- [nodejs/node#62028 — route require.resolve through registerHooks](https://github.com/nodejs/node/pull/62028)
- [Joyee Cheung, "require(esm) in Node.js: from experiment to stability" (2025-12-30)](https://joyeecheung.github.io/blog/2025/12/30/require-esm-in-node-js-from-experiment-to-stability/)
- [Joyee Cheung tweet announcing module.registerHooks landing](https://x.com/JoyeeCheung/status/1866497001396777142)
- [New Relic commit d4b4f11 — refactor to use require-in-the-middle](https://github.com/newrelic/node-newrelic/commit/d4b4f1177267dfc2e9e9216afe90180964fff823)
- [getsentry/sentry-javascript#15209 — RITM critical-dependency warning](https://github.com/getsentry/sentry-javascript/issues/15209)
- Source verified on GitHub `main` (2026-05-17): pirates/lib/index.js, require-in-the-middle/index.js, import-in-the-middle/index.js, tsconfig-paths/src/register.ts, module-alias/index.js, app-module-path/lib/index.js, mock-require/index.js, proxyquire/lib/proxyquire.js, privatenumber/tsx src/cjs/api/register.ts, TypeStrong/ts-node src/cjs-resolve-hooks.ts, dd-trace-js/package.json, getsentry/sentry-javascript packages/node/package.json, newrelic/node-newrelic/package.json, elastic/apm-agent-nodejs/package.json, open-telemetry/opentelemetry-js experimental/packages/opentelemetry-instrumentation/package.json.
- npm weekly download stats from api.npmjs.org/downloads/point/last-week (2026-05-17).

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Content unchanged apart from removing second-person address; the download figures and source confirmations are as measured on 2026-05-17 and have not been re-sampled.

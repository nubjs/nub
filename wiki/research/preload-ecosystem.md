# The Node preload ecosystem — who relies on `--require` / `--import`, and what breaks

**Status:** 2026-08-03. Survey made while integrating varlock, to find out what else in the ecosystem depends on the same mechanism.

**Grounding:** every download figure was measured against `api.npmjs.org` on 2026-08-03 (window 2026-07-27 → 2026-08-02). Behavioral claims were reproduced on Node 26.5.0 unless another version is named; source claims were read from a local shallow clone of the tool's repository or an unpacked npm tarball. Anything unverified is labelled UNVERIFIED in place.

## What counts as a preload

A module Node loads *before* the entry point, through one of four channels:

| Channel | Form | Inherited by children? |
|---|---|---|
| CJS preload | `node --require <m>` / `-r <m>` | No (argv) |
| ESM preload | `node --import <m>` (20.6+) | No (argv) |
| Environment | `NODE_OPTIONS="--require/--import <m>"` | **Yes — every descendant process AND every worker thread** |
| Loader hooks | `module.register()` / `module.registerHooks()` called *from* a preload | Follows whichever channel registered it |

The environment channel is the one that causes trouble, and the one Nub compiles `nub.jsonc` `preload:` entries into.

### The ordering law

Measured across three independent runs, and consistent with Node's own CLI documentation (*"Modules preloaded with `--require` will run before modules preloaded with `--import`"*):

- **All `--require` run before all `--import`**, regardless of channel or argv position.
- Within a phase, `NODE_OPTIONS` entries run **before** argv entries.
- Repeating the same specifier is a no-op (module cache).
- **`process.execArgv` does not contain `NODE_OPTIONS`-supplied flags.** A tool that sniffs only `execArgv` to detect a rival preload cannot see one delivered through the environment. `tsx` is the only tool found that reads both.

### Hook chaining

| System | Registration order | Runs first | Sees transformed source |
|---|---|---|---|
| `module.registerHooks` (sync) | A then B | B (outermost) | B |
| `pirates` / `Module._extensions` | P1 then P2 | P1 (innermost) | P2 |

Net law either way: **last registered gets the final word.** Async `module.register()` hooks are always *inner* to sync `registerHooks` hooks regardless of registration order, so a sync-hook transpiler always wins over an async-loader one.

## The ranking, and why downloads are the wrong axis

Two instruments were available and **both are proxies that fail in opposite directions**, so neither is quoted alone:

- **npm downloads** measure *installs*, overwhelmingly transitive. `pirates` at 80.6M/wk and `why-is-node-running` at 75.7M/wk are artifacts of being dependencies of every `*-register` package and of vitest respectively — not evidence anyone invokes their preload entry point.
- **GitHub code search** measures what humans *commit*, and is structurally blind to what tools *inject at runtime*. The tell: `"--require .pnp.cjs"` returns **10 hits**, even though Yarn PnP writes exactly that string into `NODE_OPTIONS` for every script in every PnP project on earth. Same for the Kubernetes auto-instrumentation operators — `"NODE_OPTIONS newrelicinstrumentation.js"` returns 4.

A separate instrument, the npm registry's `depends:` search, was **discarded as broken** — it returns fuzzy text matches (`csstype` reported as a dependent of `import-in-the-middle`) with absurd totals.

### Tier 1 — the giants

The highest-volume preloads, each with the entry point it is invoked through. Yarn PnP tops the list by deployed count while appearing in no download ranking at all.

| Tool | Weekly DL | Preload entry point | Notes |
|---|---|---|---|
| **Yarn PnP** | not on npm | `NODE_OPTIONS="--require <abs>/.pnp.cjs --experimental-loader file://<abs>/.pnp.loader.mjs"` | Almost certainly the largest *injected* preload deployment. Invisible to every instrument. |
| **`dotenv`** | 166,966,426 | `node -r dotenv/config` | The largest single preload entry point users actually type (189,440 code hits). **Migrating away — see below.** |
| **`jiti`** | 171,352,968 | `node --import jiti/register` | Larger than `dotenv`. Uses the **async** `module.register()`. Nuxt / ESLint flat config / unbuild pull it in. |
| **`source-map-support`** | 132,945,240 | `node -r source-map-support/register` | Idempotent; double-preload is safe. Upstream stale since 2024-08. |
| **`tsconfig-paths`** | 99,293,765 | `node -r tsconfig-paths/register` | Patches `Module._resolveFilename` only, so composes with a transpiler by design. |
| **`@opentelemetry/instrumentation`** | 93,926,350 | `--require @opentelemetry/auto-instrumentations-node/register` | See the cost table — this one is expensive. |
| **`tsx`** | 82,780,985 | `node --import tsx`, `--require tsx/cjs`, `NODE_OPTIONS='--import tsx'` | Best-engineered preload in the ecosystem; see its sentinel below. |
| **`import-in-the-middle` / `require-in-the-middle`** | 72,485,892 / 51,429,592 | registered *from* a preload | The shared substrate under every APM vendor. |

### Tier 2 — widely used, clearly preload-shaped

Eighteen further tools with a documented preload entry point, spanning 54.0M weekly downloads down to 13K.

| Tool | Weekly DL | Tool | Weekly DL |
|---|---:|---|---:|
| `@cspotcode/source-map-support` (ts-node's vendored fork) | 54.0M | `@opentelemetry/auto-instrumentations-node` | 7.9M |
| `ts-node` | 48.5M | `@swc-node/register` | 4.7M |
| `sucrase` | 47.3M | `module-alias` | 3.9M |
| `@sentry/node` | 31.4M | `newrelic` | 1.4M |
| `@esbuild-kit/esm-loader` (deprecated) | 13.9M | `elastic-apm-node` | 478K |
| `esbuild-register` | 13.0M | `varlock` | 169K |
| `dd-trace` | 9.9M | `@splunk/otel` | 37K |
| `@dotenvx/dotenvx` | 9.8M | `@instana/collector` | 33K |
| `@babel/register` | 9.8M | `@appsignal/nodejs` | 13K |

### Checked and excluded — these are *not* preloads

Categories that look like preloads and are not: polyfills, in-process caches, test-runner setup files, coverage tools that hijack a different channel, and framework `register()` hooks.

- **Polyfills as a category.** `reflect-metadata` (39.3M), `core-js` (67.3M), `regenerator-runtime` (67.8M), `cross-fetch` (36.0M), `abort-controller` (61.4M), `whatwg-fetch` (25.7M) document **no** `-r`/`--import` entry point. They are `import`-in-your-entry side-effect modules.
- **`v8-compile-cache`** (10.6M) — its own README documents in-process `require('v8-compile-cache')`. Code search: 311 hits for the `require()` form, **3** for `-r`.
- **Most "setup file" mechanisms.** `jest` `setupFiles`, `vitest` `setupFiles`, `mocha --require`, and `ava` `require` are all **in-process**, loaded through the runner's own module registry *after* the runner has booted. They are not Node preloads and cannot hook the runner itself. Jest and Vitest set no `NODE_OPTIONS` at all. (The real-preload escape hatches are `mocha --node-option`, `ava` `nodeArguments`, and `node --test --import`.)
- **`nyc`** — does not use `NODE_OPTIONS`. It uses `spawn-wrap`, which writes a fake `node` executable into a temp dir and **prepends that dir to `PATH`**. A different hijack channel entirely, and one that collides with Nub's own PATH shim rather than its `NODE_OPTIONS`.
- **`c8`** — sets only `NODE_V8_COVERAGE`.
- **`@vercel/otel`** (2.8M) — called from Next.js's `instrumentation.ts` `register()` hook. This is the framework-hook alternative to a preload, and it is why a large slice of Next.js OTel adoption never touches `NODE_OPTIONS`.

## The direction of travel: away from preloading

Three independent signals, all pointing the same way.

**1. `dotenv` — the ecosystem's single most-typed preload — is removing preloading.** On `master` (unreleased as of 2026-08-03; latest published is 17.4.2), the changelog reads *"Remove preloading. Instead use cli `dotenv run -- your-command`"*. `dotenv/config` survives as a three-line shim; the `dotenv_config_*` argv machinery is deleted.

**2. `module.register()` is runtime-deprecated.** DEP0205: doc-deprecated in Node 25.9.0 / 24.15.0, **runtime-deprecated in 26.0.0** ([nodejs/node#62401](https://github.com/nodejs/node/pull/62401)), *"will be removed in a future version."* Measured on 26.5.0 — `module.register()` emits the warning, `module.registerHooks()` is silent. This forced `tsx` onto `registerHooks` in 4.21.1. Still on the async API today, and therefore warning on Node 26: `jiti/register`, `@sentry/node`, `ts-node`'s entire ESM path.

**3. `--experimental-loader` still works on 26.5.0 but warns it "may be removed".**

The counter-signal: `node --env-file` (92,128 code hits) is absorbing the `dotenv` use case into the runtime itself.

## The env-loader category, in detail

**A preload structurally cannot set `NODE_OPTIONS`.** Node has already parsed the variable by the time any preload runs. This is [motdotla/dotenv#314](https://github.com/motdotla/dotenv/issues/314), closed unfixed in 2018. `--env-file` and every run-wrapper can; no preload ever will.

### Precedence: first-writer-wins, silently, everywhere

Every loader in this category defaults to leaving an already-set variable alone, and almost none of them says so.

| Tool | Key already in `process.env` | Diagnostic | Override |
|---|---|---|---|
| `dotenv` (preload) | file value skipped | **none** — the preload path forces `quiet: true` | `DOTENV_CONFIG_OVERRIDE=true` |
| `dotenv` (explicit `config()`) | skipped | `injected env (N)` on stderr; N drops but no key is named | same |
| `@dotenvx/dotenvx` | skipped | count drops; `--verbose` logs `KEY pre-exists` | `--overload` |
| **Node `--env-file`** | **real env wins** | none | none |
| **`@next/env`** | skipped — snapshots `process.env` first, plus a `__NEXT_PROCESSED_ENV` re-entry guard | none | internal only |
| **Vite / SvelteKit / Astro** | ambient `VITE_*` copied in last, wins | none | `envDir: false` |
| `varlock` | real `process.env` is the top of its precedence chain | validation errors are loud; a shadowed key is not | `@currentEnv` |

SvelteKit and Astro are Vite — all three import the same `loadEnv`.

### Everyone silently injects ciphertext

The sharpest instance of the precedence problem, and it is **not** specific to any one tool. Measured against a `.env` carrying a `DOTENV_PUBLIC_KEY` and an `encrypted:` value:

| | result |
|---|---|
| `dotenvx run -- node app.mjs` | decrypted correctly |
| `node -r dotenv/config app.mjs` | `encrypted:BNWf/…` verbatim, exit 0, **silent** |
| **`node --env-file=.env app.mjs`** | `encrypted:BNWf/…` verbatim, exit 0, **silent** |

Upstream dotenv on `master` is adding a warning for exactly this case. Any runtime that eagerly loads `.env` inherits the defect unless it validates the value shape.

### Two corrections to widely-held premises about `--env-file`

Both measured on Node 26.5.0:

1. **`NODE_OPTIONS` inside a `--env-file` IS parsed and applied, and the preload it names runs.** The docs say so explicitly: *"The environment variables which configure Node.js, such as `NODE_OPTIONS`, are parsed and applied."* There is no disallow-list.
2. **No variable expansion.** `A=1` followed by `B=${A}-x` yields the literal string `${A}-x`.

Missing file: `--env-file` exits **9** with `not found`; `--env-file-if-exists` continues at exit 0.

### The run-wrapper family

Outside npm, the wrapper shape dominates secret management. The popularity proxy is GitHub stars plus code-search hits for the literal command — ordinal at best, since several are SaaS products with no public CLI repo:

| Tool | Code-search hits | Command |
|---|---|---|
| 1Password | **5,640** | `op run --env-file="./prod.env" -- <cmd>` |
| Doppler | **4,840** | `doppler run -- <cmd>` |
| Infisical | 2,244 | `infisical run -- <cmd>` |
| sops | 1,034 | `sops exec-env <file> <cmd>` |
| fnox | 829 | `fnox exec -- <cmd>` |

The `op run` wrapper masks secrets in stdout/stderr by default — a capability the preload shape cannot have, since a preload can only patch in-process writers.

**The wrapper's whole cost is one extra Node boot.** Measured against the same tool's own preload: `dotenvx` 202 ms preloaded vs 283 ms wrapped. Roughly +60–80 ms buys stream redaction and `NODE_OPTIONS` control.

### The `varlock/config` entry point fork bombs the same way `auto-load` does

The varlock exports map ships `./config` as a deliberate `-r dotenv/config` drop-in.

It hangs the same way `auto-load` does: `NODE_OPTIONS="-r varlock/config" node -e '…'` times out at exit 124, while the same specifier passed as an argv `-r` exits 0.

### The schema axis is not the preload axis

The schema validators — `@t3-oss/env-core` (4.4M), `@t3-oss/env-nextjs` (3.6M), `envalid` (637K), `env-schema` (486K), `znv` (53K) — have no `bin` and no `/config` export.

They validate an already-loaded `process.env`, compose with any loader, and conflict with none. Combined they are roughly 52× varlock's downloads — where "schema for env" mindshare sits today — for a strictly smaller problem: no resolution, no secret fetching, no redaction.

Separately, **`dotenv-extended` has owned the `.env.schema` filename since 2016** and is still maintained. Any tool claiming that filename needs a content sniff, not a name match.

## Upstream will not fix the inheritance problems

Two Node issues cover preload inheritance through the environment, and both closed with no code change.

- [nodejs/node#47615](https://github.com/nodejs/node/issues/47615), *"Loaders that use childProcess.fork lead to endless recursion of processes"* — filed 2023-04-19, **auto-closed by the stale bot on 2026-07-04 with no fix**. The preload fork bomb is permanently userland's problem.
- [nodejs/node#52930](https://github.com/nodejs/node/issues/52930) — closed as a documentation fix. The inheritance behavior was judged under-documented, never wrong.

## The five hazards of the environment channel

Ranked by how often they bite. Only the first is the one people expect.

### 1. Relative and bare specifiers are fatal in an inherited `NODE_OPTIONS`

A preload specifier is resolved **from the child's cwd**, and failure is a hard exit. Measured:

```
$ cd sub && NODE_OPTIONS="--import ./pre.mjs" node app.js     # ERR_MODULE_NOT_FOUND, exit 1
$ cd elsewhere && NODE_OPTIONS="--require dd-trace/init" node app.js   # Cannot find module, exit 1
```

Every user-facing APM invocation is one of those two forms, which is why every fleet-scale injector writes an **absolute** path (`/otel-auto-instrumentation-nodejs/autoinstrumentation.js`, `/opt/init.mjs`, `/usr/lib/splunk-instrumentation/.../@splunk/otel/instrument`) and why Datadog's `serverless-init` also sets `NODE_PATH`.

### 2. Preloads multiply into worker threads, not just child processes

Measured: a `--import` in `NODE_OPTIONS` runs in every `new Worker()`. Caught live during this survey — `esbuild.transformSync` spawns a worker thread, so an esbuild-backed preload re-evaluated *itself* inside its own transform.

Launcher binaries double the count again: the `tsx` binary spawns a child `node` inheriting the environment, so a `NODE_OPTIONS` preload evaluates **twice** for `tsx app.ts`. Same for `yarn node` and `ts-node --esm`. The multiplier is (child processes) × (worker threads each).

### 3. Per-process cost, multiplied by every descendant

Median of 5 runs of `node noop.js`, real installed packages, no collector listening:

| `NODE_OPTIONS` | median | delta |
|---|---|---|
| (none) | 44 ms | — |
| `--require dd-trace/init` | 154 ms | +110 ms |
| `--import @sentry/node/preload` | 250 ms | +206 ms |
| `--require @opentelemetry/auto-instrumentations-node/register` | **9,107 ms** | **+9.1 s** |

The OTel figure was bisected, not guessed: `OTEL_SDK_DISABLED=true` → 190 ms; `OTEL_METRICS_EXPORTER=none` → 1,209 ms; `OTEL_NODE_RESOURCE_DETECTORS=none` → 8,093 ms (not the cause on its own). **About 8 s is the metrics exporter's shutdown flush against an unreachable `localhost:4318`.**

**The residual decomposes further, and it is not module load.** Re-measured 2026-08-03 on Node 26.5.0 with a positive control asserting the SDK attached, and with the OTLP endpoint pinned to an unused port (the default 4318 was contended). Turning the metrics exporter *and* the resource detectors off together drops the cost to **219 ms**, not the ~1.2 s that `OTEL_METRICS_EXPORTER=none` alone leaves. The cost is **three independent, additive terms**:

| Term | Cost | Notes |
|---|---|---|
| Attach floor — SDK + ~40 instrumentation packages loading | **~140 ms** | the only irreducible part |
| Resource detectors — cloud-metadata probes that time out off-cloud | **~1,020 ms** | `OTEL_NODE_RESOURCE_DETECTORS` |
| Metrics-exporter shutdown flush against an unreachable endpoint | **~7,150 ms** | vanishes entirely when a collector is listening |

| Configuration | No collector | Collector listening |
|---|---|---|
| baseline, no preload | 80 ms | 84 ms |
| attached, default config | 8,398 ms | 1,266 ms |
| attached + `OTEL_SDK_DISABLED=true` | 247 ms | 243 ms |
| attached + `OTEL_METRICS_EXPORTER=none` | 1,272 ms | 1,241 ms |
| attached + `OTEL_NODE_RESOURCE_DETECTORS=none` | 7,440 ms | 253 ms |
| attached + both | 219 ms | 224 ms |

Nearly all of that cost is **decidable before the process starts** — whether a collector is reachable, and whether the cloud detectors can possibly succeed — a decision a launcher can make and an in-process SDK cannot. Full decomposition, plus the ESM attach path and the runtime-level prior art: [[research/opentelemetry|OpenTelemetry on Node]].

### 4. Consumers that re-parse `NODE_OPTIONS` corrupt repeated flags

**Next.js** parses `NODE_OPTIONS` into a `Record<string, string | boolean>` keyed by option name and reformats it for every forked worker. A `Record` cannot hold two `--import`s.

Measured by running Next.js 16.2.12's own shipped `getFormattedNodeOptionsWithoutInspect`:

| Input | Next's output |
|---|---|
| `--import A --import B` | `--import="A B"` — both paths mashed into one bogus specifier |
| `--import=A --import=B` | `--import=B` — the first silently dropped |
| `--require A --require B` | `--require="A B"` |
| `--require A --import B` | round-trips correctly |

**The rule: repeated flags of the same name are destroyed; distinct names survive.**

Confirmed end-to-end on a real `next@16.3.0` build (two identical runs, stable), counting the processes each preload ran in: one `--require=` reaches every process; two `--require=` leaves the first in only a few while the second reaches all, **exit 0**; the space-separated form fails outright with `Cannot find module '<a> <b>'`. Root cause: `parseArgs({ strict: false })` with no `multiple: true`, feeding a `Record` keyed by option name — declaring the repeatable flags as arrays fixes it upstream.

Filed as [vercel/next.js#96582](https://github.com/vercel/next.js/issues/96582) with a [hosted reproduction](https://github.com/colinhacks/nextjs-node-options-repro). Two prior reports exist: [#77550](https://github.com/vercel/next.js/issues/77550) (open, covers only the crash variant) and [#67286](https://github.com/vercel/next.js/issues/67286) (the same bug, closed by a stale bot with no human response, then locked).

### 5. Package managers clobber the variable outright

How each injector treats a `NODE_OPTIONS` that is already set. npm, pnpm, Renovate and Electron destroy what is there; Yarn PnP and the fleet-scale k8s injectors append to it.

| Injector | Behavior |
|---|---|
| npm `node-options` npmrc field | **CLOBBERS** — `env.NODE_OPTIONS = cliConf['node-options']`, a bare assignment |
| pnpm `node-options` | **CLOBBERS** identically |
| Renovate (when `nodeMaxMemory` is set) | **CLOBBERS** with only `--max-old-space-size=<n>` |
| Electron (packaged apps) | **DROPS** `--require`/`--import` entirely; only `--max-http-header-size` and `--http-parser` survive |
| Yarn Berry PnP | **appends** — prepends its own tokens, strips only its own prior ones, preserves the rest |
| pnpm PnP linker | **appends** (upstream test: *"makeNodeRequireOption() preserves existing NODE_OPTIONS"*) |
| OpenTelemetry k8s Operator | **appends** |
| New Relic k8s-agents-operator | **appends** |
| Datadog k8s single-step injection | **UNVERIFIED** — injector is closed-source; docs say only that it *"Sets `NODE_OPTIONS` with `--require` or `--import`"* |
| Datadog `serverless-init` | **appends** |
| OTel Lambda layer | **appends** — `export NODE_OPTIONS="${NODE_OPTIONS} --import /opt/init.mjs"` |
| Splunk collector installer | writes host-wide systemd `DefaultEnvironment` — every Node process on the box |

Measured for npm 11.17.0, with a control:

| `.npmrc` | ambient env | what the script saw |
|---|---|---|
| *(no `node-options`)* | `NODE_OPTIONS=--title=MARKER` | `--title=MARKER` — survives |
| `node-options=--max-old-space-size=333` | `NODE_OPTIONS=--title=MARKER` | `--max-old-space-size=333` — **ambient gone** |

**Verified negatives** (these set no `NODE_OPTIONS`): official `node` Docker images, `actions/setup-node`, Jest, Vitest, Turborepo, Vite, Angular CLI, webpack-cli, `pm2` (uses node argv instead).

## How the ecosystem defends itself — the coexistence playbook

Every mature preload has converged on one of a few guards.

**Sentinel env var, not stripping.** The instinct is to strip `NODE_OPTIONS` from children. Datadog tried and backed off, with the comment: *"Not passing `NODE_OPTIONS` results in issues with yarn, which relies on NODE_OPTIONS for PnP support, hence why we deviate from the DI pattern here. To avoid infinite initialization loops, we're disabling DI and tracing in the worker."* They keep the variable and pass `DD_TRACE_ENABLED=false` instead. **You cannot strip the channel, because someone else's correctness rides on it.**

**Sentry is the exception that proves it** — it strips categorically (`execArgv: []` plus `env: { ...process.env, NODE_OPTIONS: undefined }` at three call sites, and `unset NODE_OPTIONS` in its Lambda extension) with the comment *"We don't want any Node args like `--import` to be passed to the worker"*. It can afford to because its workers are self-contained.

**Detect a rival and stand down.** `dd-trace` ships a hardcoded conflict list of ten rival agents (`@appsignal/nodejs`, `@dynatrace/oneagent`, `@instana/*`, `@sentry/node`, `elastic-apm-node`, `newrelic`, `appoptics-apm`, `atatus-nodejs`, `stackify-node-apm`, `sqreen`) and warns on collision. Its `DD_INJECTION_ENABLED` guard makes an *injected* tracer bail out entirely if the app carries its own copy at a different path.

**Read both channels before deciding your tier.** When a TypeScript preload appears before it, `tsx` downgrades from sync `registerHooks` to async `module.register()` so the entry point still evaluates. Provenance is a real regression ([tsx#795](https://github.com/privatenumber/tsx/issues/795), *"4.21.1 regression: `--import my-opentelemetry-hook.ts` causes mystery, silent exit"*, fixed in 4.22.3).

**Guard on the preload's own module identity.** Yarn's `.pnp.cjs` checks `module.parent.id === 'internal/preload'` and deletes itself from `Module._cache`, with the comment *"it might cause some issues when the file is multiple times in NODE_OPTIONS"*.

**Guard on the thread.** `elastic-apm-node`'s `start.js` checks `isMainThread` and refuses to start in a worker. A worker guard, not a subprocess guard.

## Yarn PnP and `registerHooks` — a real crash, already fixed upstream

Worth recording because it is a live trap for any runtime installing sync resolve hooks.

A **literal pass-through** `module.registerHooks({ resolve(s, c, next) { return next(s, c) } })` preloaded under Yarn PnP 4.9.2 crashes on any CommonJS `require`:

```
Error: Some options passed to require() aren't supported by PnP yet (conditions)
    at require$$0.Module._resolveFilename (.../.pnp.cjs:6381:15)
    at wrapResolveFilename (node:internal/modules/cjs/loader:1123:27)
```

Mechanism, read from both sides: `.pnp.cjs` throws on any `_resolveFilename` option key other than `paths`/`plugnplay`, and Node's hooked-CJS path always injects `conditions`. Reproduces on Node 22.15, 24.17 and 26.5. **A `load`-only hook does not trigger it** — that is the mitigation lever.

Fixed upstream in [berry#6966](https://github.com/yarnpkg/berry/pull/6966), released in **yarn 4.11.0 (2025-11-07)**, and confirmed passing there. Berry's own PR body names the trigger: *"This is more an issue when using the new `registerHook` API… Vite recently started using this API as of 7.2.0, so it now crashes."* Residual exposure is real but bounded: `packageManager` pins are sticky, so projects pinned to 4.0.0–4.10.3 (Nov 2023 – Sep 2025) still hit it.

Caveat for re-running this: PnP is active only under `yarn node` (or under a runtime that injects the `.pnp.cjs` token itself). Plain `node` never loads `.pnp.cjs`, so nothing crashes and the test proves nothing.

## Prior art: how other runtimes propagate a config-file `preload`

Bun has the same feature — `bunfig.toml` `preload = [...]` and a `--preload` flag — and its docs are silent on subprocess behavior, so it was measured directly on bun 1.3.14:

| child spawned… | preload ran in child? | `NODE_OPTIONS` / `BUN_OPTIONS` in child |
|---|---|---|
| in the project dir | **yes** | unset |
| with `cwd` outside the project (no `bunfig.toml`) | **no** | unset |

**Bun propagates `preload` by config-file rediscovery from the child's cwd, not by environment inheritance** — each process independently re-reads `bunfig.toml`.

That fixes the **leak scope** — a descendant inside the project still gets the preload, so script coverage survives; one outside the tree gets nothing — but not the **fork bomb**, since a preload that spawns a same-project process still triggers rediscovery and recurses. Recursion needs a sentinel regardless of channel.

## Consequences for Nub

Nub is both a producer and a consumer of this channel: it injects its own augmentation tokens into `NODE_OPTIONS` alongside the user's `preload:` entries.

### Most of this list is already Nub's job

Much of the ecosystem's top preloads do what Nub already does natively, with no preload at all: read against the ranking above, the survey is substantially an inventory of what Nub replaces.

| Surveyed tool | DL/wk | What Nub already ships |
|---|---|---|
| `jiti` (as a TS runner) | 171.4M | the transpiler — `registerHooks` fast tier / `module.register` compat tier |
| `dotenv` | 167.0M | the `.env` cascade in Rust, before the child spawns |
| `source-map-support` | 132.9M | `--enable-source-maps` in `ALWAYS_INJECT` ([`flags.rs:22`](../../crates/nub-core/src/node/flags.rs)) |
| `tsconfig-paths` | 99.3M | tsconfig discovery, `extends`, and the `paths` matcher, all native (`nub-native loadTsconfig`, a `get-tsconfig@4.14.0` port; `resolveTs` runs the matcher) |
| `tsx` · `ts-node` · `sucrase` · `esbuild-register` · `@swc-node/register` | 83M · 48M · 47M · 13M · 4.7M | same transpiler |
| `abort-controller` | 61.4M | `CLOBBER_MAP` rewrites the import to the global ([`transform-core.mjs:222`](../../runtime/transform-core.mjs)) — likewise `urlpattern-polyfill`, `@js-temporal/polyfill` |

The question for most of these is therefore not "should Nub integrate it" but "does Nub's native path stay correct when the tool is *also* present" — a collision matrix, not an integration decision.

### The defensive patterns are already implemented too

The survey's coexistence playbook, checked against the code:

| Pattern found in the wild | Nub's existing implementation |
|---|---|
| `tsx` reads **both** `NODE_OPTIONS` and `execArgv` to pick its tier | `shouldAutoAsyncTierAtPreload()` = `nodeHookComposeBroken() && foreignAsyncLoaderFlagPresent()` ([`preload-common.cjs:280`](../../runtime/preload-common.cjs)), reading both channels, plus the launcher's predictive argv scan |
| Sentinel env var rather than stripping `NODE_OPTIONS` (dd-trace's forced retreat) | `__NUB_ENV_OWNER_WRAPPED`, and `is_reentrant_in` keying on Nub's own token ([`spawn.rs:809`](../../crates/nub-core/src/node/spawn.rs)) |
| Absolute paths only, never bare/relative specifiers | Nub emits absolute paths and `file://` URLs |
| Append to a pre-existing `NODE_OPTIONS`, never assign | Nub appends ([`spawn.rs:1086`](../../crates/nub-core/src/node/spawn.rs)) |
| Yarn PnP needs its token installed first | PnP token pushed before Nub's own ([`spawn.rs:1098`](../../crates/nub-core/src/node/spawn.rs)) |
| `module.register()` DEP0205 on Node 26 | already wrapped and suppressed, with a comment explaining Nub cannot use `registerHooks` on the compat and `--no-experimental-require-module` paths |
| `NODE_OPTIONS` smuggled through an env file | `ENV_FILE_DENYLIST` — **stricter than Node's own `--env-file`**, which parses and applies it |
| npm/pnpm `node-options` clobbering | Nub **appends** `npm_config_node_options` to its own augmentation for lifecycle scripts ([`pm_engine/mod.rs:1763`](../../crates/nub-cli/src/pm_engine/mod.rs)) instead of assigning over it |

Measured confirmation of the last row, on the same fixture that destroys the ambient value under real npm:

| | Nub's preload survives? | user's `--max-old-space-size=333` applied? |
|---|---|---|
| `npm run` | **no** | yes |
| `nub run` | **yes** | **yes** |

**Already correct, verified by measurement:**

- Nub routes value-bearing preload/PnP flags through `NODE_OPTIONS` only, never argv, precisely because a child that rebuilds its flags by merging `process.execArgv + NODE_OPTIONS` (Next.js, `jest-worker`) would otherwise collect the same path twice.
- Nub detects a foreign async loader (`tsx`/`ts-node`, or any `--import`/`--loader`) in the child's argv and downgrades its own tier, avoiding the broken sync/async hook composition on Node 22.15–24.11.
- **Nub does not hit the Yarn PnP `conditions` crash**, despite installing a `registerHooks` resolve hook — measured against a `yarn@4.9.2` PnP project with PnP active (`process.versions.pnp = 3`), where a bare pass-through hook crashes. The mechanism is UNVERIFIED; the likely cause is that Nub installs its own `Module._resolveFilename` override on top of PnP's and resolves PnP specifiers through `pnpapi.resolveRequest` directly.

**A second consideration, from the Bun measurement.** If the `preload:` leak into every Node descendant is worth fixing, config-file rediscovery is the shape a peer runtime already ships, and it preserves the in-project script coverage that `NODE_OPTIONS` inheritance is currently load-bearing for.

## Reproduction

How to re-run the measurements above: the download endpoint, the code-search query, the Next.js flag round-trip, and the Yarn PnP crash.

- Download figures: `api.npmjs.org/downloads/point/last-week/<pkg>` — use the **bulk** comma-separated form for unscoped packages; the per-package endpoint rate-limits after roughly four calls.
- GitHub counts: `gh api -X GET search/code -f q='"<literal>"' --jq '.total_count'`. Validate against a known positive and a known negative before believing any result.
- Next.js round-trip: unpack the `next` tarball, stub the unused `commander` import out of `dist/esm/server/lib/utils.js`, and call `getFormattedNodeOptionsWithoutInspect()` with `process.env.NODE_OPTIONS` set.
- Yarn PnP crash: a project pinned to `yarn@4.9.2` with `nodeLinker: pnp`, then `yarn node --import <pass-through-resolve-hook> <cjs-file>`.

## Changelog

Every revision to this document, with the date and what changed.

- 2026-08-03 — Initial write-up.
- 2026-08-03 — Cross-referenced the whole survey against what nub already implements. Most of the top-ranked preloads turn out to be things nub replaces natively (source maps, tsconfig `paths`, TS transpilation, the `.env` cascade, the `CLOBBER_MAP` polyfills), and every defensive pattern found in the wild is already in the code (dual-channel tier detection, sentinel-not-stripping, absolute paths, append-not-assign, PnP token ordering, DEP0205 suppression, the env-file denylist). Corrected the npm/pnpm `node-options` clobber claim — nub appends rather than assigns and is immune — and recorded the inverse parity gap it exposed: `nub run` drops `node-options` entirely, where npm and pnpm apply it.
- 2026-08-03 — Added the env-loader detail: the universal silent first-writer-wins precedence, the ciphertext injection that Node's own `--env-file` shares, the two `--env-file` premise corrections, the run-wrapper family and its measured ~60–80 ms cost, `varlock/config` inheriting the `auto-load` fork bomb, and the schema-validation family being a different shape. Ranked what is worth integrating.
- 2026-08-03 — Confirmed the Next.js repeated-flag defect end-to-end on a real `next@16.3.0` build and filed it upstream as [vercel/next.js#96582](https://github.com/vercel/next.js/issues/96582) with a [hosted reproduction](https://github.com/colinhacks/nextjs-node-options-repro), plus the root-cause analysis on the existing [#77550](https://github.com/vercel/next.js/issues/77550).
- 2026-08-03 — **Correction to the OTel cost bisection.** The ~1.2 s residual left by `OTEL_METRICS_EXPORTER=none` was attributed to the module load of ~40 instrumentation packages. Re-measured with a positive control and a pinned OTLP port: it is ~1.02 s of **resource detectors** plus only ~0.14 s of module load — turning both off lands at 219 ms. Added the full no-collector/collector-listening matrix and the three-term decomposition, plus the observation that the two dominant terms are decidable before the process starts.
- 2026-08-28 — Trimmed to the measured findings and current behavior.

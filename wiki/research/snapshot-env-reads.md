# Reads of `process.env` in snapshotted JS

How a snapshotted function sees `process.env`: a read inside a function body runs against the boot-time environment, while a module-top-level read freezes the build host's value into every shipped binary.

## TL;DR

The `process.env` object is a live JS Proxy backed by a C++ `RealEnvStore` that calls `uv_os_getenv()` on **every** read.

There is no value-capture at snapshot time: when a function defined during snapshot construction later executes at process boot, the `process.env.X` lookup inside its body runs against the boot-time environment. The hazard is the **module top level** — a `const FOO = process.env.X` at top scope of a snapshotted file freezes the build-time value into the snapshot.

So the canonical Node.js pattern for env-driven runtime branches in snapshotted JS is (a) keep the read inside the function body so it happens per call, or, preferred for boot-time-only flags, (b) promote the env var to a CLI option in `src/node_options.cc::HandleEnvOptions` and read it from `getOptionValue('--node-compat')`, which routes through `internal/options.js` with a built-in "must be after bootstrap" guard. For Nub's `NODE_COMPAT` dispatcher inside `Module._findPath` the in-body read is correct and matches existing precedent: `Module._initPaths` reads `process.env.NODE_PATH` / `safeGetenv('NODE_PATH')` at call time even though the function definition is snapshotted.

## What happens to `process.env` during snapshot build

Node.js ships **two** snapshots; the rules differ:

1. **Embedded (built-in) snapshot.** Generated at `make` time by `tools/snapshot/node_mksnapshot` and baked into every `node` binary. Eagerly runs `lib/internal/bootstrap/{realm,node}.js` plus `lib/internal/bootstrap/switches/*.js`. Critically, `lib/internal/bootstrap/switches/is_main_thread.js:293-300` eagerly `require()`s `internal/modules/cjs/loader`, `internal/modules/esm/loader`, and `internal/modules/esm/utils`, so **the top level of `cjs/loader.js` runs during snapshot build** and its closures/function objects are captured in the heap blob. Bootstrap then stops; `pre_execution.js` does NOT run.
2. **User snapshot.** Built via `--build-snapshot` (SEA, custom embedders). Runs everything embedded does plus `prepareMainThreadExecution()` plus a user main script. Same backing store for `process.env`.

In both cases, `process.env` is **never** "stubbed" or "emptied." It is a JS Proxy whose handler delegates to `RealEnvStore` (`src/node_env_var.cc:41`), constructed once per process at `src/node_env_var.cc:74`:

```cpp
std::shared_ptr<KVStore> system_environment = std::make_shared<RealEnvStore>();
```

Every property access calls `RealEnvStore::Get`, which calls `uv_os_getenv` (`src/node_env_var.cc:107-126`), with no caching layer. During `node_mksnapshot` execution, `process.env.X` therefore returns whatever `X` is in the `node_mksnapshot` process's environment — the developer's or CI's shell. **Reading `process.env.X` at module top level of a snapshotted file bakes CI's value into every shipped binary.**

The Realm carries an `isBuildingSnapshot` flag (`lib/internal/v8/startup_snapshot.js:21-23`) backed by `BindingData::is_building_snapshot_buffer_` (`src/node_snapshotable.cc:1632-1656`). The flag is set to 1 during build and explicitly reset to 0 on deserialize (line 1654), so a snapshotted function checking `isBuildingSnapshot()` at call time sees the boot-time value. That is the official escape hatch, used in 20+ places in core.

For the related runtime-state-leak problem, see the comment in `lib/internal/options.js:28-34`:

> `getCLIOptionsValues()` would serialize the option values from C++ land. It would error if the values are queried before bootstrap is complete so that we don't accidentally include runtime-dependent states into a runtime-independent snapshot.

The C++ enforcement lives at `src/node_options.cc:1704-1708`:

```cpp
if (!env->has_run_bootstrapping_code()) {
  THROW_ERR_OPTIONS_BEFORE_BOOTSTRAPPING(
      isolate, "Should not query options before bootstrapping is done");
}
```

The options path is the rails for a runtime-dependent flag that must not be snapshotted.

## Precedent table

Every `NODE_*` env read in core, with the file and line, whether it happens at require time or at call time, and whether that placement is snapshot-safe.

| Env var | Where read | Eager or lazy | Snapshot-safe? |
|---|---|---|---|
| `NODE_OPTIONS` | `src/node.cc:954` (C++, before JS) via `credentials::SafeGetenv` | eager, pre-bootstrap | safe (C++ side, not snapshotted) |
| `NODE_PRESERVE_SYMLINKS`, `NODE_PRESERVE_SYMLINKS_MAIN`, `NODE_PENDING_DEPRECATION`, `NODE_REDIRECT_WARNINGS`, `NODE_USE_ENV_PROXY` | `src/node_options.cc:2187-2212` (`HandleEnvOptions`) — folded into `EnvironmentOptions` at boot, then read via `getOptionValue('--preserve-symlinks')` etc. | eager, pre-bootstrap on C++ side; lazy on JS side | safe — JS read goes through the `has_run_bootstrapping_code` guard |
| `NODE_PATH` | `lib/internal/modules/cjs/loader.js:2124` inside `Module._initPaths` (function body, called from `pre_execution.js`) | lazy — read at call time, not module top | safe — defined during snapshot but body runs post-deserialize |
| `NODE_NO_WARNINGS` | `lib/internal/process/pre_execution.js:339` inside `setupWarningHandler` | lazy — only runs from `prepareMainThreadExecution` | safe |
| `NODE_DEBUG` | `lib/internal/process/pre_execution.js:488` calls `initializeDebugEnv(process.env.NODE_DEBUG)` | eager-at-boot, lazy-at-snapshot — the read is in `pre_execution.js`, not in `debuglog.js`'s top scope | safe; the `debuglog.js` comment at lines 83-86 explicitly calls this out: "debuglogImpl depends on process.pid and process.env.NODE_DEBUG, so it needs to be called lazily in top scopes of internal modules that may be loaded before these run time states are allowed to be accessed" |
| `NODE_V8_COVERAGE` | `lib/internal/process/pre_execution.js:461-465` and `lib/internal/source_map/source_map_cache.js:159` (function body) | lazy | safe |
| `NODE_TLS_REJECT_UNAUTHORIZED` | `lib/internal/options.js:205` inside `getAllowUnauthorized()` (called per TLS handshake) | lazy | safe |
| `NODE_CHANNEL_FD`, `NODE_CHANNEL_SERIALIZATION_MODE`, `NODE_UNIQUE_ID` | `lib/internal/process/pre_execution.js:622-643` (and `internal/main/worker_thread.js:72`) | lazy, in main scripts | safe; main scripts don't run during snapshot build |
| `NODE_CLUSTER_SCHED_POLICY` | `lib/internal/cluster/primary.js:48` — **top-level read** | eager-at-require | safe **only because** `cluster` is not in the snapshotted module set; if it ever were, this would leak the build host's env |
| `NODE_UNIQUE_ID` (cluster child) | `lib/internal/cluster/child.js:38` — top-level read in object literal | eager-at-require | same — safe only because cluster is lazy-loaded |
| `NODE_COMPILE_CACHE`, `NODE_COMPILE_CACHE_PORTABLE` | `lib/internal/modules/helpers.js:483-486` inside a function body | lazy | safe |

The pattern is consistent: **anything read at module top level is dangerous unless that module is guaranteed not to be in the snapshot.** Everything in `lib/internal/modules/{cjs,esm}/**` IS in the snapshot, so all env reads there must be inside function bodies.

## The canonical pattern

The closest parallel to a resolver dispatcher is in the snapshotted `cjs/loader.js` itself, `Module._initPaths` at lines 2122-2150:

```js
Module._initPaths = function() {
  const homeDir = isWindows ? process.env.USERPROFILE : safeGetenv('HOME');
  const nodePath = isWindows ? process.env.NODE_PATH : safeGetenv('NODE_PATH');
  // ...
};
```

The `Module._initPaths` function object is captured in the snapshot (the assignment runs at snapshot build), while the `process.env.NODE_PATH` / `safeGetenv('NODE_PATH')` calls inside the body run later, when `Module._initPaths()` is invoked from `pre_execution.js`, and see the boot-time env. This works because `process.env` is a Proxy delegating to `RealEnvStore::Get(uv_os_getenv())` on every access, with no per-snapshot capture.

Note the use of `internalBinding('credentials').safeGetenv` for POSIX. That function (`src/node_credentials.cc`) suppresses env reads when the process runs with elevated privileges (suid/sgid), which matters for security-relevant resolver knobs; `process.env` has no such guard. For a `NODE_COMPAT` switch that only toggles native vs. JS resolver, `safeGetenv` is the more conservative choice.

For boot-time-only flags (read once and cached), the canonical pattern is the options route — see `--preserve-symlinks`, `--preserve-symlinks-main`, `--pending-deprecation`, etc., all of which are env-derived but reach JS through `getOptionValue('--preserve-symlinks')`. The cjs/loader uses this at lines 586, 789, 794. The advantages: (1) the C++ side reads the env once at boot and caches; (2) `internal/options.js` enforces "must be after bootstrap" via the C++ assertion at `node_options.cc:1704`; (3) the value gets surfaced to `node --help` / `getCLIOptionsInfo` for free; (4) snapshotted functions that read it transparently get the boot-time value because the JS-side cache (`optionsDict` at `internal/options.js:23,33`) is also a closure that's lazily filled the first time `getOptionValue` is called post-bootstrap.

## Anti-patterns to avoid

Four ways to get this wrong: a top-level read in a snapshotted module, a module-scope cache that cannot be invalidated, testing only under `--no-node-snapshot`, and forgetting that a write to `process.env` reaches the real environment.

1. **Top-level read in a snapshotted module:**
   ```js
   // BAD if this file is in the snapshot:
   const COMPAT = process.env.NODE_COMPAT === '1';
   Module._findPath = function(...) { if (COMPAT) ... };
   ```
   The `COMPAT` constant is evaluated during the `node_mksnapshot` run and frozen into the heap blob, so every shipped binary gets the build host's value. This is the failure mode `lib/internal/util/debuglog.js:83-86` warns about; debuglog was restructured so initialization is deferred to `setupDebugEnv` in `pre_execution.js` (line 488).

2. **Module-scope closure with lazy fill:**
   ```js
   let cached;
   const getCompat = () => cached ??= process.env.NODE_COMPAT === '1';
   ```
   Safe in itself — the first read happens post-deserialize — but the cache cannot be invalidated if the env changes later (a worker thread tweaking `process.env` before spawning, or a test harness). Acceptable for boot-time-only flags, not for anything that might change. `internal/options.js:23,33` uses exactly this pattern for the CLI options dict, but with `refreshOptions()` (line 196) as an escape hatch called from `refreshRuntimeOptions` in pre_execution.

3. **Reading under `--no-node-snapshot` and assuming it holds in production.** Running `./node --no-node-snapshot` makes `cjs/loader.js` execute at boot, so any top-level env read sees the right value in the dev loop. A shipped binary will not. Test snapshot semantics with the default flag set.

4. **Forgetting that `process.env.FOO = 'x'` writes through.** `RealEnvStore::Set` (`src/node_env_var.cc:146-159`) calls `uv_os_setenv`, so a dispatcher doing `process.env.NODE_COMPAT = '1'` (to propagate to child processes, say) mutates the real process env.

## Application to `NODE_COMPAT`

For `Module._findPath` and the equivalent ESM resolve dispatcher, ranked:

### Option A (recommended): in-body `safeGetenv` read, no caching

Read the env var inside the function body on every resolve, the same shape as the existing `safeGetenv('NODE_PATH')` read in that file. Snapshot-safe, at the cost of a mutex-guarded `uv_os_getenv` per resolution.

```js
// lib/internal/modules/cjs/loader.js, near the existing safeGetenv import
const _findPathJS = Module._findPath;
Module._findPath = function(request, paths, isMain, conditions) {
  if (safeGetenv('NODE_COMPAT') === '1') {
    return _findPathJS(request, paths, isMain, conditions);
  }
  return modulesBinding.findPath(
    request, paths, isMain ?? false,
    conditions ?? getDefaultConditions(),
  );
};
```

Pros: zero snapshot interaction; identical shape to the existing `safeGetenv('NODE_PATH')` read in the same file; honors suid/sgid drop; per-call check means env changes (tests, workers) take effect immediately. Cons: one `uv_os_getenv` per resolve. For a resolver hot path this is non-trivial — `uv_os_getenv` takes a mutex (`per_process::env_var_mutex` at `node_env_var.cc:108`) and on macOS issues a `getenv_r` libc call. For a per-resolution check this is too expensive in steady state.

### Option B (recommended for perf-sensitive paths): promote to a CLI option

Add to `src/node_options.cc::HandleEnvOptions` (line 2187):

```cpp
env_options->node_compat = opt_getter("NODE_COMPAT") == "1";
```

with a matching `--node-compat` declaration and `EnvironmentOptions::node_compat` field. Then in `cjs/loader.js`:

```js
const { getOptionValue } = require('internal/options');

const _findPathJS = Module._findPath;
Module._findPath = function(request, paths, isMain, conditions) {
  if (getOptionValue('--node-compat')) {
    return _findPathJS(request, paths, isMain, conditions);
  }
  return modulesBinding.findPath(/* ... */);
};
```

Pros: env is read exactly once at boot in C++; JS hot path is a property lookup on a cached object (`optionsDict` in `internal/options.js`); snapshot-safe by construction (the bootstrap guard in `node_options.cc:1704` aborts if anything tries to read during snapshot); discoverable as `--node-compat` flag too. Cons: cannot toggle at runtime without `refreshOptions()`; requires C++ change in addition to JS. This is exactly the rails Node uses for `NODE_PRESERVE_SYMLINKS` and is the most "house style" option.

### Option C: module-scope lazy cache (acceptable, less ideal)

Cache the result in a module-scope variable on first call: one `uv_os_getenv` per process and no C++ change, in exchange for losing the runtime toggle.

```js
let _nodeCompatCached;
function isNodeCompat() {
  return _nodeCompatCached ??= safeGetenv('NODE_COMPAT') === '1';
}
```

Pros: one `uv_os_getenv` per process; no C++ change. Cons: no runtime toggle; the cache is in a snapshotted module so the `let _nodeCompatCached;` declaration is captured but the value remains `undefined` in the snapshot (V8 serializes the binding, not a stale value, because the assignment never ran during build). Functionally fine.

**Recommendation: Option B** if `NODE_COMPAT` is conceptually a boot-time switch (likely, since it selects between two entire resolver implementations and you probably want consistent behavior for the process lifetime). **Option A** if you genuinely want per-call dynamism (worker-thread overrides, test harnesses) — accept the mutex cost. Option C if you want to ship something today without the C++ patch and add B later.

## Open uncertainties

Six items still to confirm: per-Realm options state in worker threads, code-cache invalidation, `safeGetenv` on Windows, behavior without the snapshot, when the bootstrap guard admits `getOptionValue`, and one import in the draft.

- **Worker threads vs. snapshot.** Workers are bootstrapped from the same embedded snapshot (with a per-thread Realm). `isBuildingSnapshot()` is per-Realm (`src/node_snapshotable.cc:1651`). Whether `optionsDict` in `internal/options.js` is properly per-Realm or shared across workers is not 100% obvious from a static read; the binding `internalBinding('options')` is per-Realm so it should be, but a worker-thread test of any `NODE_COMPAT` toggle is warranted.
- **Code cache invalidation.** The built-in snapshot ships with a V8 code cache (`BuildCodeCacheFromSnapshot`, `node_snapshotable.cc:1160`). Adding a `process.env.NODE_COMPAT` branch inside `Module._findPath` invalidates the code cache for that function, costing a recompile on the first call. Probably negligible.
- **`safeGetenv` semantics on Windows.** `credentials::SafeGetenv` falls back to `uv_os_getenv` on Windows (no setuid concept). The mutex cost is identical.
- **Behavior under `--no-node-snapshot`.** All three options above behave the same with or without the snapshot.
- **Whether `getOptionValue` is callable from inside `Module._findPath`.** The bootstrap-complete guard in `node_options.cc:1704` is checked on the *first* call to `getCLIOptionsValues`. Since `Module._findPath` is only invoked after `prepareMainThreadExecution`, the guard passes. If you ever need to call it earlier (e.g., from `initializeCJS`), confirm via `has_run_bootstrapping_code`.
- **`getDefaultConditions()` import.** The dispatcher draft assumes this is already in scope in `cjs/loader.js`; verify against current top-of-file imports before pasting.

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links and reference-checkout paths were rewritten; findings and measured values are unchanged.

// Nub fast-tier preload — Node 22.15+, injected via `--require` (CommonJS).
//
// WHY CJS / `--require` (not the `.mjs` `--import` the compat tier uses): the mere
// presence of `--import` forces Node to eagerly initialize the ESM loader, which
// then routes EVEN A CJS ENTRY POINT through the async ESM module-job
// (`ModuleJob.run`) instead of the synchronous `Module.runMain` CJS path. That one
// change is the root cause of a whole regression cluster (R1): top-level
// `executionAsyncId()===0` (Node: 1), extra PROMISE async-hook events, a top-level
// sync `throw` surfacing as `unhandledRejection` instead of `uncaughtException`,
// `require.main.id` `'.'`→abspath, `module.parent` `null`→`undefined`, and a
// missing-entry `ERR_MODULE_NOT_FOUND` instead of `MODULE_NOT_FOUND`. Loading this
// preload via `--require` (CJS) keeps Node on the synchronous CJS entry path and
// restores all of them, while STILL supporting `module.registerHooks` + TS
// transpile (both work from a `--require` CJS module on Node 22.15+).
//
// HARD CONSTRAINT: this file and everything it pulls in synchronously must be
// TLA-free. `require(esm)` (which loads transform-core.mjs / the polyfill ESM
// modules) rejects any module with top-level await (ERR_REQUIRE_ASYNC_MODULE), so
// transform-core.mjs, polyfills.cjs, worker-polyfill.mjs and navigator-locks.mjs
// are all TLA-free by construction. The compat tier (< 22.15), where require(esm)
// is unreliable, keeps its async `--import` preload.mjs path UNCHANGED.
//
// ROBUSTNESS TO `--no-experimental-require-module` (the require-module cluster):
// a user may set `--no-experimental-require-module` (e.g. to assert the legacy
// require(esm)→ERR_REQUIRE_ESM contract for THEIR code). That flag globally
// disables require(esm) — including for THIS `--require` CJS preload's own
// `require("./transform-core.mjs")`, which would otherwise crash the process at
// startup (ERR_REQUIRE_ESM) before any user code runs. nub's preload must survive
// that. The fix: detect when sync require(esm) is unavailable and fall back to the
// compat tier's async loader-worker hooks (`module.register("./preload-async-
// hooks.mjs")`), which loads transform-core.mjs as a STATIC ESM import inside the
// worker — a path the flag does not gate. User code still gets Node's own
// ERR_REQUIRE_ESM for ITS require(esm), exactly as the flag promises; only nub's
// preload is made robust. (See the require-module corpus cluster:
// test-cjs-esm-warn, test-disable-require-module-with-detection,
// test-esm-type-field-errors-2, parallel/test-require-mjs.)

const { createRequire } = require("node:module");
const module_ = require("node:module");

const __require = createRequire(__filename);

// Load preload-common FIRST so we can restore NODE_COMPILE_CACHE (R8) BEFORE
// transform-core.mjs is required: spawn.rs stripped that env var to keep nub's
// preload chain out of the user's V8 compile cache, and transform-core reads
// `NODE_COMPILE_CACHE === "0"` as its transpile-cache disable signal — so the
// value must be back in process.env before transform-core's module body runs.
// Restoring it in JS does NOT re-enable Node's bootstrap compile cache (already
// configured from the stripped env), so the chain below stays uncached.
const common = __require("./preload-common.cjs");
common.restoreCompileCacheEnv();

// `--no-experimental-require-module` disables require(esm) globally, so the
// transform-core require below (and the worker/locks ESM side-effect modules)
// would throw ERR_REQUIRE_ESM and abort the process before user code. Detect that
// and load via the async-register fallback instead. We probe by attempting the
// require and catching ERR_REQUIRE_ESM — robust regardless of how the flag arrived
// (CLI, NODE_OPTIONS, or a config file), and a no-op cost on the common path where
// require(esm) works.
let core = null;
let requireEsmDisabled = false;
try {
  // The transform core is the single source of truth for resolution + transpile,
  // shared verbatim with the compat tier. It's an ES module with no top-level
  // await, so `require(esm)` loads it synchronously here on Node 22.15+.
  core = __require("./transform-core.mjs");
} catch (err) {
  if (err && err.code === "ERR_REQUIRE_ESM") {
    requireEsmDisabled = true;
  } else {
    throw err;
  }
}

const { installSyncPolyfills } = __require("./polyfills.cjs");

if (!requireEsmDisabled) {
  // ── Fast tier (sync require(esm) available) ───────────────────────

  // ── Watch-mode dependency reporting + hooks ───────────────────────
  const watchReporting = common.installWatchReporting(core);

  // Best-effort bounded-cache eviction (main thread only; the core guards on it).
  // DEFERRED to setImmediate: maybeSweepCache probes `worker_threads.isMainThread`
  // and dynamic-imports cache-evict.mjs, which would otherwise pull worker_threads
  // (and its streams/worker-io transitive set) into the BOOTSTRAP module-load list
  // on every startup — a cold-start regression (test-bootstrap-modules snapshots
  // process.moduleLoadList at user code's first line). Running it one turn later
  // keeps those out of the bootstrap snapshot while preserving the once-a-day sweep.
  // unref so a purely-synchronous program still exits promptly without waiting on it.
  setImmediate(() => {
    try { core.maybeSweepCache(); } catch {}
  }).unref();

  // ── Pre-load clobbered polyfill packages BEFORE hooks register ────
  // Packages in the core's CLOBBER_MAP can't be imported after hooks register (the
  // resolve hook returns a synthetic module instead of the real package), so
  // require them now via the not-yet-hooked CJS require and stash them for the
  // polyfill installer. Temporal is deferred entirely to a lazy global (below).
  const __preloadedPolyfills = common.preloadPolyfillPackages(__require);

  // ── Hook registration (fast tier: sync, in-thread) ────────────────
  // Same realm as user code; covers `import` and (Node 24+) `require`.
  // registerHooks' require RESOLUTION is incomplete on 22.15–24, so also install
  // the main-thread CJS resolve shim (its _resolveFilename half, always on). We do
  // NOT install the classic require.extensions transpile shim on the fast tier: the
  // sync registerHooks LOAD hook already transpiles require()'d `.ts` (CJS content,
  // tsconfig paths, .tsx, extensionless — all verified), and native require(esm)
  // (>= 22.12, always present at the 22.15+ fast floor) loads ES-module `.ts`. The
  // classic require.extensions['.ts'] hook would SHADOW that native require(esm) and
  // throw a bogus ERR_REQUIRE_ESM on every ESM `.ts` entry on 22.15–22.17 (where
  // process.features.typescript is still false) — so it must stay off here. On
  // 22.18+/24+ this was already the behavior (native-TS → false); now it's uniform.
  const { resolve, load } = common.makeHooks(core, watchReporting);
  module_.registerHooks({ resolve, load });
  common.installCjsRequireHooks(core, false);
  // NOTE: no Yarn `.pnp.loader.mjs` registration. nub's own `resolve` hook already
  // routes PnP specifiers through `pnpapi.resolveRequest` (see makeHooks /
  // installCjsRequireHooks), covering both `import` and `require` of PnP deps.
  // Registering Yarn's ESM loader ON TOP of the sync `module.registerHooks` hooks
  // deadlocks ESM entry loading (silent exit) — both hook systems intercept ESM
  // resolution. The compat tier (preload-async-hooks.mjs) resolves PnP the same way.

  // ── Sync polyfills + lazy ESM-side-effect polyfills ───────────────
  installSyncPolyfills(__preloadedPolyfills);
  installLazyEsmPolyfills();

  // ── Temporal: lazy global (A37) ───────────────────────────────────
  common.installTemporalLazyGlobal(__require);

  // ── Compile-cache: re-enable for the USER's modules (R8) ──────────
  common.reenableUserCompileCache();
} else {
  // ── Fallback tier (`--no-experimental-require-module`): async hooks ─
  // The user disabled require(esm), so the in-thread sync `module.registerHooks`
  // core can't be loaded here. Register the SAME hooks the compat tier uses, run
  // in a dedicated loader worker via `module.register`; that worker imports
  // transform-core.mjs as a static ESM import (not gated by the flag). The
  // main-thread CJS require() transpile shim, which would need the core
  // synchronously in-thread, is unavailable in this mode — an honest, additive
  // degradation: the user opted out of require(esm), and nub's `.ts`-via-require()
  // transpile rides on exactly that mechanism. `import`-side TS still transpiles
  // through the registered loader-worker hooks. User require(esm) of THEIR own ES
  // modules still gets Node's native ERR_REQUIRE_ESM, exactly as the flag promises.
  const { pathToFileURL } = require("node:url");
  module_.register("./preload-async-hooks.mjs", pathToFileURL(__filename).href);

  // Sync, non-require(esm) polyfills still install (none of them require(esm)).
  // Clobbered-polyfill packages are CJS requires, unaffected by the flag.
  const __preloadedPolyfills = common.preloadPolyfillPackages(__require);
  installSyncPolyfills(__preloadedPolyfills);
  installLazyEsmPolyfills();

  // Temporal lazy global needs only `__require` (it loads a CJS package), and the
  // user's compile-cache re-enable is independent of require(esm).
  common.installTemporalLazyGlobal(__require);
  common.reenableUserCompileCache();
}

// ── Lazy ESM-side-effect polyfills (R7) ─────────────────────────────
// The two ESM side-effect polyfills — the browser-shape Worker global
// (worker-polyfill.mjs) and Web Locks (navigator-locks.mjs) — were previously
// installed EAGERLY at preload (polyfills.cjs:installEsmPolyfillsSync). That drags
// ~50 builtins into bootstrap on EVERY startup: worker-polyfill.mjs imports
// node:worker_threads, which pulls internal/streams/* (readable/writable/duplex/
// transform/pipeline/…), internal/worker, internal/worker/io,
// internal/worker/messaging, vm, net, child_process, os, etc.; navigator-locks.mjs
// pulls internal/locks + internal/navigator. None of that is needed by the common
// "run a plain file, never touch Worker or navigator.locks" case, and the eager
// load is a cold-start regression that contradicts the fast-runner premise
// (test-bootstrap-modules: moduleLoadList must match Node's bootstrap set).
//
// Replace the eager install with lazy globals:
//   • `globalThis.Worker` — a non-enumerable getter that, on first access (the
//     first `new Worker(...)`), deletes itself, requires worker-polyfill.mjs (which
//     then defines the real `globalThis.Worker`), and returns it.
//   • `navigator.locks` — a non-enumerable getter that loads navigator-locks.mjs on
//     first access (only when not native — Node 24.5+ ships it).
// In a WORKER thread, the worker-side bootstrap inside worker-polyfill.mjs (self/
// postMessage/message wiring) MUST run at startup, so we load it eagerly there.
// That costs nothing for bootstrap accounting: a worker already loaded
// worker_threads to exist, and test-bootstrap-modules measures the main thread.
function installLazyEsmPolyfills() {
  // Cheap main-thread detection that does NOT pull node:worker_threads into the
  // main-thread bootstrap (requiring it eagerly is exactly the regression we're
  // fixing): in a worker, worker_threads is already in the module-load list by the
  // time this preload runs; on the main thread it is not.
  const inWorkerThread = process.moduleLoadList.some(
    (m) => m === "NativeModule worker_threads",
  );

  const loadEsmSideEffect = (specifier) => {
    try {
      __require(specifier);
    } catch (err) {
      if (err && err.code === "ERR_REQUIRE_ESM") {
        // require(esm) disabled — load via dynamic import (not flag-gated). Async,
        // but side-effect-only; the Worker/locks polyfills are needed lazily, and
        // for a worker thread the worker-side wiring lands a tick later, which is
        // still before any user message round-trip can complete.
        import(specifier).catch(() => {});
      } else {
        throw err;
      }
    }
  };

  if (inWorkerThread) {
    // Worker-side scope bootstrap must be present synchronously where possible.
    loadEsmSideEffect("./worker-polyfill.mjs");
    if (typeof globalThis.navigator?.locks === "undefined") {
      loadEsmSideEffect("./navigator-locks.mjs");
    }
    return;
  }

  // Main thread: lazy Worker global. Defined NON-ENUMERABLE so it stays invisible
  // to `Object.keys(globalThis)` / for-in — the additive contract — matching how
  // worker-polyfill.mjs defines the real one.
  if (typeof globalThis.Worker === "undefined") {
    let installing = false;
    Object.defineProperty(globalThis, "Worker", {
      configurable: true,
      enumerable: false,
      get() {
        if (installing) return undefined;
        installing = true;
        // Drop this lazy accessor so worker-polyfill.mjs's own
        // `if (typeof globalThis.Worker === "undefined")` guard fires and defines
        // the real Worker.
        delete globalThis.Worker;
        loadEsmSideEffect("./worker-polyfill.mjs");
        return globalThis.Worker;
      },
      set(value) {
        // A user assigning their own Worker wins — replace the lazy accessor.
        Object.defineProperty(globalThis, "Worker", {
          value,
          configurable: true,
          enumerable: false,
          writable: true,
        });
      },
    });
  }

  // Main thread: lazy navigator.locks (native on Node 24.5+, absent on the 22.x
  // floor). VERSION-GATE so we never even READ `globalThis.navigator` where locks
  // is native: on Node 24.5+ the native `navigator` global is a lazy getter that,
  // on first access, eagerly realizes internal/navigator + internal/locks AND the
  // whole stream/worker-io transitive set (~30 builtins) — touching it at preload
  // would be exactly the cold-start regression test-bootstrap-modules guards
  // against, for zero benefit (locks is already there). Below 24.5 navigator is
  // present but lacks `locks`, and accessing it is cheap (one internal module), so
  // installing the lazy polyfill there is fine.
  const [navMaj, navMin] = process.versions.node.split(".").map((n) => parseInt(n, 10));
  const locksNative = navMaj > 24 || (navMaj === 24 && navMin >= 5);
  if (locksNative) return;

  const nav = globalThis.navigator;
  if (nav && typeof nav.locks === "undefined") {
    let installing = false;
    Object.defineProperty(nav, "locks", {
      configurable: true,
      enumerable: true,
      get() {
        if (installing) return undefined;
        installing = true;
        delete nav.locks;
        loadEsmSideEffect("./navigator-locks.mjs");
        return nav.locks;
      },
      set(value) {
        Object.defineProperty(nav, "locks", {
          value,
          configurable: true,
          enumerable: true,
          writable: true,
        });
      },
    });
  }
}

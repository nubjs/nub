// Shared preload machinery for BOTH tiers — CommonJS, zero top-level await.
//
// The fast tier (Node 22.15+) loads this from a `--require` CJS preload
// (preload.cjs) so Node keeps its synchronous `Module.runMain` CJS entry path
// (top-level `executionAsyncId()===1`, sync exception origin, `require.main.id`
// `'.'`, `module.parent` `null`) — all of which the old `--import` ESM preload
// broke by forcing eager ESM-loader init that routed even a CJS entry through the
// async ESM module-job (R1). The compat tier (18.19–22.14) loads this from its
// async `--import` preload.mjs and reuses the same hook/require/watch/Temporal
// logic; only hook REGISTRATION differs (sync `module.registerHooks` on the fast
// tier vs async `module.register` loader worker on compat), which each entry owns.
//
// EVERYTHING here is synchronous and import-of-transform-core is a plain
// `require()` — transform-core.mjs has no top-level await and is require(esm)-able
// on the fast tier; the compat entry passes its already-imported core bindings in
// (it imported them as ESM), so this module never require()s the core there.

const module_ = require("node:module");
const { readdirSync } = require("node:fs");
const { fileURLToPath, pathToFileURL } = require("node:url");
const { join, dirname, extname: pathExtname } = require("node:path");

// Yarn PnP API handle, fetched lazily via Node's `module.findPnpApi`. `.pnp.cjs`
// (injected by the Rust spawn layer via --require, ahead of nub's preload) sets
// `process.versions.pnp` and installs `findPnpApi`, which returns the pnpapi object
// governing a given path. Unlike a bare `require("pnpapi")` — which throws here,
// since this preload lives in nub's install dir, OUTSIDE the user's PnP tree —
// `findPnpApi` resolves by the queried path, so an out-of-tree issuer works. Being
// a plain query it never re-enters nub's resolve hooks, so there is no ordering
// constraint with `module.registerHooks` (the reason the previous abs-path require
// was load-bearing-fragile). nub resolves PnP specifiers through
// `pnpapi.resolveRequest` (its public, conditions-free resolver) in both the
// registerHooks resolve hook and the `_resolveFilename` override below. No env var
// (brand boundary); `null` when this is not a PnP run.
let __pnpApi;
function pnpApi() {
  if (__pnpApi) return __pnpApi; // cache only a SUCCESSFUL lookup (see below)
  if (!process.versions.pnp) return null;
  // `findPnpApi` matches by the queried path. A single synthesized `cwd + sep` anchor
  // can miss on Windows (drive-letter casing, 8.3 short paths, trailing separator),
  // and a transient early miss must NOT be cached sticky — otherwise every later
  // resolution falls through to PnP's `_resolveFilename`, which rejects the
  // `conditions` option Node injects under a registered hook (the intermittent
  // Windows `conditions` crash). So try several real in-tree anchors and cache only
  // on success: `argv[1]` is the user's entry file (in-tree for `nub <file>`); cwd
  // covers `nub run` / `nub exec`.
  for (const anchor of [process.argv[1], cwdIssuer(), process.cwd()]) {
    if (!anchor) continue;
    try {
      const api = module_.findPnpApi(anchor);
      if (api) return (__pnpApi = api);
    } catch {}
  }
  return null;
}

// Shared PnP ESM resolution (resolveRequest + format) + the directory-issuer
// helper, identical to the compat worker's — see runtime/pnp-util.cjs.
const { pnpResolveEsm, cwdIssuer } = require("./pnp-util.cjs");

// ── Watch-mode dependency reporting (main thread only) ──────────────
// Under `nub watch`, Node's FilesWatcher only watches files in the import graph;
// config files (tsconfig.json, package.json) and `.env*` are NOT in any graph, so
// an edit to them otherwise goes stale. Node accepts incremental
// `process.send({'watch:require': [...]})` over its WATCH_REPORT_DEPENDENCIES IPC
// at ANY point in the child's life (it adds each path to the watch set), so we
// report config paths AS the core loader discovers them. The reporters are
// injected into the core via setWatchHooks so getTsconfigForDir / getPackageType
// self-report. The flush is coalesced via setImmediate.
function installWatchReporting(core) {
  const WATCH_REPORTING =
    process.env.WATCH_REPORT_DEPENDENCIES === "1" && typeof process.send === "function";
  const watchReported = new Set();
  const watchPending = [];
  let watchFlushScheduled = false;
  function flushWatchDeps() {
    watchFlushScheduled = false;
    if (watchPending.length === 0) return;
    const batch = watchPending.splice(0, watchPending.length);
    try { process.send({ "watch:require": batch }); } catch {}
  }
  function reportWatchDep(path) {
    if (!WATCH_REPORTING || !path || watchReported.has(path)) return;
    watchReported.add(path);
    watchPending.push(path);
    if (!watchFlushScheduled) {
      watchFlushScheduled = true;
      // A scheduled immediate is drained before the loop would exit, so even a
      // script that finishes synchronously flushes its deps. (Don't unref: an
      // unref'd immediate is skipped on a synchronous exit, dropping the report.)
      setImmediate(flushWatchDeps);
    }
  }
  // Report a directory's `.env*` files (the natural watch targets). Scanned once
  // per directory, lazily.
  const watchEnvScannedDirs = new Set();
  function reportEnvFilesIn(dir) {
    if (!WATCH_REPORTING || watchEnvScannedDirs.has(dir)) return;
    watchEnvScannedDirs.add(dir);
    let entries;
    try { entries = readdirSync(dir); } catch { return; }
    for (const name of entries) {
      if (name === ".env" || name.startsWith(".env.")) reportWatchDep(join(dir, name));
    }
  }
  core.setWatchHooks({ reportDep: reportWatchDep, reportEnvDir: reportEnvFilesIn });
  return WATCH_REPORTING;
}

// ── Resolve / load hooks (sync `module.registerHooks` shape) ────────
// Returns `{ resolve, load }` closing over `core` + the watch flag. The compat
// tier does NOT use these (its hooks run async in the loader worker via
// preload-async-hooks.mjs); only the fast tier's `module.registerHooks` does.

// True once USER code registers its own `module.registerHooks` (a ts-node/tsx-style
// transpiler). nub registers exactly one hook set from the preload (the FIRST call
// after the wrap below); every later call is the user's. This lets the load hook
// tell apart a bare `'typescript'` format that a USER resolve hook set (defer — the
// user's own load hook will transpile) from the bare `'typescript'` that Node's
// NATIVE CJS loader assigns to a `.ts` entry/require in a package with no explicit
// `type` (transpile — there is no user hook to do it, and Node's strip-only mode
// can't handle enums/namespaces). See makeHooks().load.
let __userHooksRegistered = false;
function installUserHookDetector() {
  if (typeof module_.registerHooks !== "function") return;
  const orig = module_.registerHooks;
  if (orig.__nubWrapped) return;
  let seen = 0;
  const wrapped = function (...args) {
    // Call #1 is nub's own preload registration; #2+ are user hooks.
    if (seen >= 1) __userHooksRegistered = true;
    seen += 1;
    return orig.apply(this, args);
  };
  wrapped.__nubWrapped = true;
  try { module_.registerHooks = wrapped; } catch {}
}

function makeHooks(core, watchReporting) {
  installUserHookDetector();

  function resolve(specifier, context, nextResolve) {
    const r = core.resolveSpec(specifier, context.parentURL);
    if (r) return r;
    // Under Yarn PnP, delegating to `nextResolve` flows into Node's default
    // resolution, which calls PnP's patched `_resolveFilename` WITH a `conditions`
    // option PnP rejects once a customization hook is registered ("aren't supported
    // by PnP yet (conditions)"). PnP exposes its own conditions-free resolver —
    // `pnpapi.resolveRequest` — so resolve through THAT and hand Node the file URL.
    // Still "PnP does the resolution" (its public API); nub adds none of its own.
    // Returns a virtual `.zip` path for zip-stored deps, which Node reads via the
    // zipfs patch `.pnp.cjs` installed. Falls back to `nextResolve` if PnP can't
    // resolve (e.g. a builtin or a relative path PnP defers to Node).
    const pnp = pnpApi();
    if (pnp && !module_.isBuiltin(specifier) && !specifier.startsWith("node:")) {
      try {
        const res = pnpResolveEsm(pnp, specifier, context && context.parentURL);
        if (res) return res;
      } catch { /* fall through to Node's resolver */ }
    }
    return nextResolve(specifier, context);
  }

  function load(url, context, nextLoad) {
    const ext = core.extname(url);

    // Watch mode: surface this file's nearest config files (tsconfig.json,
    // package.json) + sibling `.env*` so edits to them restart the run. Done for
    // every user file (not just transpiled ones) — getTsconfigForDir/
    // getPackageType self-report via the injected watch hooks.
    if (watchReporting && url.startsWith("file:") && !core.isNodeModules(url)) {
      try {
        const dir = dirname(fileURLToPath(url));
        core.getTsconfigForDir(dir);
        core.getPackageType(dir);
      } catch {}
    }

    // A USER resolve hook (a ts-node/tsx-style transpiler registered AFTER nub's
    // own preload hook) claimed this file with the bare 'typescript' format: defer
    // to the user's own load chain. The discriminator is `__userHooksRegistered`,
    // NOT the bare format alone — Node's NATIVE CJS loader ALSO emits the bare
    // string 'typescript' for a `.ts` entry/require whose nearest package.json has
    // no explicit `type` (cjs/loader.js getFormatOfExtensionlessFile, lines ~1986),
    // and in that native case nub MUST transpile (Node's strip-only mode can't
    // handle enums/namespaces). So we only step aside when a user hook is present:
    // nub registers exactly one hook set from the preload, the user registers theirs
    // later, and registering theirs OUTERMOST (LIFO) means their load hook wraps
    // nub's — it sets format='typescript', calls nextLoad into nub, and (without this
    // guard) nub would transpile with oxc — a type-stripper, not a module-format
    // transformer — leaving `export {}` verbatim and, for a `type:commonjs` package,
    // handing Node format='commonjs' + ESM source = invalid CJS. Stepping aside lets
    // nub fall through to Node's native load, returning raw TS source back up to the
    // user's outer hook, which does the real ESM->CJS conversion, matching Node.
    // Native 'module-typescript'/'commonjs-typescript' formats still fall through to
    // nub's transpile below, so normal augmentation is unchanged.
    if (__userHooksRegistered && context && context.format === "typescript") {
      return nextLoad(url, context);
    }

    // R12: never transpile `.ts`/`.tsx`/… inside node_modules. Node itself throws
    // ERR_UNSUPPORTED_NODE_MODULES_TYPE_STRIPPING for TS under node_modules; if
    // nub transpiled it instead, that native error would never surface and nub
    // would be MORE permissive than Node. Fall through to `nextLoad` so Node's own
    // handling (and its error) applies. (The TS-parent extensionless resolution in
    // the resolve hook is intended and stays — only this load-time transpile is
    // gated.)
    if (core.TRANSPILE_EXTS.has(ext) && !core.isNodeModules(url)) {
      return core.loadTranspile(url, ext);
    }
    if (ext in core.DATA_EXTS) return core.loadData(url, ext);

    const r = nextLoad(url, context);
    // nub's sync `module.registerHooks` load hook forces the synchronous
    // module-job (ModuleJobSync.syncLink -> loadAndTranslateForImportInRequiredESM),
    // which cannot async-fetch source. When a user `--experimental-loader` resolve
    // hook sets `format` without a `source` (a pattern vanilla Node tolerates on its
    // async load path by fetching the source itself), the default load returns
    // source:null and Node's assertBufferSource throws ERR_INVALID_RETURN_PROPERTY_VALUE.
    // Backfill the source from disk so the sync path matches Node — without touching
    // nub's own resolve/transpile hooks.
    //
    // EXCEPTION — format 'commonjs' (and 'builtin') MUST keep source:null. For those
    // formats Node's ESM loader deliberately returns no source and hands the module
    // off to the NATIVE CommonJS loader (Module._load), where `require()` uses CJS
    // resolution. A CJS `.js` ENTRY, when a user `--experimental-loader` is active, is
    // routed through the ESM loader for format detection but still loads as CJS this
    // way. If we backfilled its source, the ESM loader would instead translate it via
    // its CommonJS-to-ESM wrapper, routing every inner `require()` through the ESM
    // resolve hook — so `require('assert')` would hand the bare 'assert' specifier to
    // the user's resolve hook and crash with ERR_INVALID_RETURN_PROPERTY_VALUE (the
    // shadow-realm/custom-loaders corpus failure). Only ESM-shaped formats ('module',
    // 'json', 'wasm', …) genuinely need the source on the sync path.
    if (
      r && r.source == null && r.format &&
      r.format !== "commonjs" && r.format !== "builtin" &&
      typeof url === "string" && url.startsWith("file:")
    ) {
      try {
        const { readFileSync } = require("node:fs");
        return { ...r, source: readFileSync(fileURLToPath(url)) };
      } catch { /* fall through with the original result */ }
    }
    return r;
  }

  return { resolve, load };
}

// ── CommonJS require() augmentation (BOTH tiers) ────────────────────
// `module.registerHooks`' CJS-`require()` coverage is INCOMPLETE before ~Node 24:
// on Node 22.15 a `require()` from a `.cts` parent (which Node loads via the ESM
// translator's special-require) hits native Module._resolveFilename with no
// tsconfig/extensionless handling — a `require('@alias')` or `require('./x')` of a
// `.ts` target throws MODULE_NOT_FOUND, while the same code works on Node 26 (where
// registerHooks does cover it) and on the fast `import` path. On the compat tier
// (18.19–22.14) the only hook surface is `module.register`, which intercepts the
// ESM loader ONLY — so `require()` is entirely unaugmented there. Both gaps have
// the same closure: install this main-thread CJS shim, reusing the core's canonical
// resolveCjsPath / loadTranspile (no drift). It tries nub's resolution first and
// FALLS THROUGH to native on a miss, so it is a safe no-op on the versions where
// registerHooks already covers require (Node 24+/26). Mechanism stays within the
// augmenter rules: exactly what `--require`-installing the ts-node / tsx CJS shim
// has always done.
//
// This error is surfaced ONLY on Node versions without native require(esm)
// (< 20.19 / 22.0–22.11), where require() of an ES module genuinely cannot work.
// On every require(esm)-capable Node, Node loads the ES module itself and this is
// never reached. The message is user-facing: no internal mechanism names.
function requireEsmError(filename) {
  const err = new Error(
    `Cannot require() this file — it is an ES module.\n` +
    `  ${filename}\n` +
    `It uses \`import\`/\`export\`, so it loads as an ES module, and this version of ` +
    `Node can't require() an ES module. Load it with \`import(...)\` instead, rename ` +
    `it to .cts for a CommonJS module, or upgrade Node.`,
  );
  err.code = "ERR_REQUIRE_ESM";
  return err;
}

// `withClassicTranspile` — also install the `require.extensions` (classic CommonJS
// loader) transpile hook. Needed ONLY on Node WITHOUT native require(esm)
// (< 20.19 / 22.0–22.11): there, `module.register`'s ESM-loader hooks can't reach a
// `require()`, AND an ES module simply can't be require()d, so we transpile CJS
// content classically and surface a clean error for ESM content. On require(esm)-
// capable Node we DON'T install it — registering `require.extensions['.ts']` would
// shadow Node's own native require(esm) of ES-module `.ts` files (breaking
// `require("./esm.ts")`), and the resolve shim below plus the tier's load hook
// already cover resolution + transpile.
function installCjsRequireHooks(core, withClassicTranspile) {
  const origResolveFilename = module_._resolveFilename;
  module_._resolveFilename = function (request, parent, isMain, options) {
    let resolved = null;
    try {
      const parentPath = parent && typeof parent.filename === "string" ? parent.filename : null;
      resolved = core.resolveCjsPath(request, parentPath);
    } catch { /* fall through to Node */ }
    if (resolved) {
      if (withClassicTranspile && core.requireTargetIsEsm(resolved, pathExtname(resolved))) {
        throw requireEsmError(resolved);
      }
      return resolved;
    }
    // Yarn PnP, FAST TIER ONLY. On the fast tier nub registers a sync
    // `module.registerHooks` resolve hook, which makes Node thread a `conditions`
    // option into PnP's patched `_resolveFilename` ("aren't supported by PnP yet
    // (conditions)") AND re-inject it even if we pass none — so we must resolve
    // everything ourselves via PnP's conditions-free `pnpapi.resolveRequest` and the
    // native path primitives, never falling through to PnP's `_resolveFilename`. On
    // the COMPAT tier there is no sync hook, so PnP's own `_resolveFilename` resolves
    // CJS deps fine (no conditions option) and interposing here instead recurses to
    // OOM on the Node 18 line — so the PnP branch is gated to where registerHooks
    // exists. `pnpApi()` is null off-PnP, making this doubly safe.
    // On a PnP tree + fast tier (sync `registerHooks` registered), resolve through
    // PnP's conditions-free `resolveRequest` and NEVER fall through to
    // `origResolveFilename` — that is PnP's patched `_resolveFilename`, which rejects
    // the `conditions` option Node injects under a registered hook. The gate is
    // `process.versions.pnp` (set the moment `.pnp.cjs` runs), NOT a non-null api:
    // if `findPnpApi` momentarily misses we still avoid PnP's resolver and use Node's
    // native `_findPath`, so a transient lookup miss can't leak a `conditions` crash.
    const inPnp = typeof module_.registerHooks === "function" && !!process.versions.pnp;
    if (inPnp) {
      if (module_.isBuiltin(request)) return request; // PnP defers builtins to Node
      const pnp = pnpApi();
      if (pnp) {
        try {
          const issuer = parent && typeof parent.filename === "string" ? parent.filename : cwdIssuer();
          const r = pnp.resolveRequest(request, issuer);
          if (r) return r;
        } catch { /* fall through to native path resolution below */ }
      }
      // PnP couldn't resolve it (nub's OWN vendored deps, whose issuer is nub's
      // out-of-tree install dir) or the api was momentarily unavailable. Use Node's
      // native path primitives (`_resolveLookupPaths` + `_findPath`), which PnP does
      // NOT patch — never `origResolveFilename` (PnP's, + `conditions`).
      const lookupPaths = module_._resolveLookupPaths(request, parent) || [];
      const found = module_._findPath(request, lookupPaths, isMain);
      if (found) return found;
      const err = new Error(`Cannot find module '${request}'`);
      err.code = "MODULE_NOT_FOUND";
      throw err;
    }
    return origResolveFilename.call(this, request, parent, isMain, options);
  };

  if (!withClassicTranspile) return;

  // require.extensions: transpile via the SAME loadTranspile the load hook uses —
  // target:'es2022' lowering (`using`), tsconfig, source maps, the Stage-3
  // decorator guard, and module-format detection are all identical to the fast
  // tier. The path is already a real TS file (Module._resolveFilename ran first).
  // A module-format source can't be _compile'd as CJS — same clean error as above.
  const transpileExtension = (mod, filename) => {
    const { source, format } = core.loadTranspile(pathToFileURL(filename).href, pathExtname(filename));
    if (format === "module") throw requireEsmError(filename);
    mod._compile(source, filename);
  };
  for (const ext of [".ts", ".cts", ".mts", ".tsx", ".jsx"]) {
    module_._extensions[ext] = transpileExtension;
  }
}

// ── Clobbered-polyfill preloading + Temporal lazy global ────────────
// Packages in the core's CLOBBER_MAP can't be imported after hooks register
// because the resolve hook returns a synthetic module instead of the real package.
// Load them here via CJS require (not yet hooked) and return them so the polyfill
// installer can stash them. Temporal is the exception (A37): the polyfill is ~18ms
// to load and most scripts never touch it, so we only RESOLVE its path now (cheap)
// and defer the load to a lazy global getter. Requiring it later by absolute path
// bypasses the CLOBBER_MAP resolve-hook entry, which keys on the specifier.
function preloadPolyfillPackages(reqFromRuntime) {
  const preloaded = {};
  // Feature-detect before requiring (A39): URLPattern is native on Node 24+, so
  // skip loading the polyfill there. On 22.x it's absent → load it.
  if (typeof globalThis.URLPattern === "undefined") {
    try { preloaded.urlpattern = reqFromRuntime("urlpattern-polyfill"); } catch {}
  }
  // Float16Array: native on Node 24+, absent on the 22.x floor.
  if (typeof globalThis.Float16Array === "undefined") {
    try { preloaded.float16 = reqFromRuntime("@petamoriken/float16"); } catch {}
  }
  return preloaded;
}

// Install the lazy `globalThis.Temporal` getter. The polyfill is loaded — and even
// RESOLVED — only on first access. CRITICAL ordering note (regexp one-off): the
// `require.resolve("@js-temporal/polyfill")` is deferred INTO the getter, NOT run at
// preload top level. An unconditional resolve at startup mutates the legacy
// `RegExp.$_` static (the resolved node_modules path matches an internal regex), so
// a program inspecting `RegExp.$_` on its first line would otherwise see a leaked
// path (test-startup-empty-regexp-statics). Deferring the resolve keeps `RegExp.$_`
// empty at user-code start; the cost is paid only by a program that touches Temporal.
function installTemporalLazyGlobal(reqFromRuntime) {
  if (typeof globalThis.Temporal !== "undefined") return;

  const defineTemporal = (value) =>
    Object.defineProperty(globalThis, "Temporal", {
      value,
      configurable: true,
      writable: true,
      enumerable: false,
    });
  Object.defineProperty(globalThis, "Temporal", {
    configurable: true,
    enumerable: false,
    get() {
      let temporalPath;
      try { temporalPath = reqFromRuntime.resolve("@js-temporal/polyfill"); } catch {}
      if (!temporalPath) return undefined;
      const polyfill = reqFromRuntime(temporalPath);
      // @js-temporal/polyfill exports `toTemporalInstant` as a function but does
      // NOT auto-install it on Date.prototype (you assign it yourself). Install it
      // here so that on the floor (no native Temporal) `date.toTemporalInstant()`
      // AND the package clobber's re-export of `Date.prototype.toTemporalInstant`
      // both work — matching native Node. Guarded so we never replace a native
      // implementation on a runtime that ships Temporal.
      if (
        typeof Date.prototype.toTemporalInstant !== "function" &&
        typeof polyfill.toTemporalInstant === "function"
      ) {
        Object.defineProperty(Date.prototype, "toTemporalInstant", {
          value: polyfill.toTemporalInstant,
          configurable: true,
          writable: true,
          enumerable: false,
        });
      }
      const T = polyfill.Temporal;
      defineTemporal(T);
      return T;
    },
    set: defineTemporal,
  });
}

// ── Compile-cache handling (R8) ─────────────────────────────────────
// nub injects its preload chain via `--require`, which Node loads at bootstrap.
// If the user set NODE_COMPILE_CACHE, Node would enable the V8 code cache BEFORE
// this preload runs and cache every module the chain pulls in (preload.cjs,
// transform-core.mjs, this file, polyfills.cjs, …) into the USER's dir — so a
// program reading `fs.readdirSync(NODE_COMPILE_CACHE)` would see ~9 nub entries,
// not its own 1 (program-observable; R8). spawn.rs prevents that by STRIPPING
// NODE_COMPILE_CACHE from the child env (bootstrap caches nothing) and stashing
// the original value in a sentinel file keyed on nub's PID — which is THIS child's
// `process.ppid` (nub is our direct parent). The dir travels via a sentinel file,
// never a NUB_* env var (brand boundary).
//
// Two preload steps consume it:
//   1. restoreCompileCacheEnv() runs EARLY, before transform-core.mjs is required,
//      to put the original value BACK into process.env.NODE_COMPILE_CACHE. That
//      matters because (a) transform-core reads `NODE_COMPILE_CACHE === "0"` as
//      nub's transpile-cache disable signal, and (b) user code may read the env.
//      Restoring it in JS does NOT re-trigger Node's V8 compile cache (Node
//      configures that once at bootstrap from the now-stripped env), so the
//      preload chain stays uncached. It also DELETES the sentinel (consume-once,
//      so a recycled PID can't read stale state and the file never leaks).
//   2. reenableUserCompileCache() runs LAST, after all nub modules are loaded
//      uncached and right before user code, and calls
//      `module.enableCompileCache(dir)` for a real dir so the user's OWN modules
//      cache as they always did. A value of "0" is nub's disable sentinel (Node
//      treats "0" as a literal dir named 0, but nub honors it as "no caching"),
//      so we skip enabling there.
// Best-effort throughout: a missing/unreadable sentinel or an enableCompileCache
// failure just means no user compile cache — strictly safer than the old pollution.
// `os.tmpdir()` without requiring `node:os`. Requiring os at preload pulls
// `Internal Binding os` + `NativeModule os` into process.moduleLoadList on EVERY
// startup (test-bootstrap-modules observes this) even though almost no run touches
// the compile-cache sentinel. This replica mirrors Node's libuv/os.tmpdir() env
// resolution (POSIX: TMPDIR→TMP→TEMP→/tmp; Win32: TEMP→TMP→SystemRoot/windir+\temp),
// trailing-separator-stripped, which is also what Rust's env::temp_dir() (the side
// that WRITES the sentinel in spawn.rs) resolves to — so both ends agree.
function tmpdirNoOs() {
  const env = process.env;
  if (process.platform === "win32") {
    let dir = env.TEMP || env.TMP || ((env.SystemRoot || env.windir || "") + "\\temp");
    if (dir.length > 1 && dir.endsWith("\\") && !dir.endsWith(":\\")) dir = dir.slice(0, -1);
    return dir;
  }
  let dir = env.TMPDIR || env.TMP || env.TEMP || "/tmp";
  if (dir.length > 1 && dir.endsWith("/")) dir = dir.slice(0, -1);
  return dir;
}

function compileCacheSentinelPath() {
  return join(tmpdirNoOs(), `nub-ccache-${process.ppid}`);
}

function restoreCompileCacheEnv() {
  try {
    const { readFileSync, rmSync } = require("node:fs");
    const value = readFileSync(compileCacheSentinelPath(), "utf8");
    try { rmSync(compileCacheSentinelPath()); } catch {}
    if (value) process.env.NODE_COMPILE_CACHE = value;
  } catch { /* no sentinel: env was never set, or already consumed */ }
  // Propagate the R8 strip to node grandchildren the user spawns directly (plain
  // node inheriting nub's --require preload + a live NODE_COMPILE_CACHE → it would
  // cache nub's preload chain into the user's dir). The wrap MUST preserve each
  // function's own symbols (esp. [util.promisify.custom]) — dropping them broke
  // util.promisify(child_process.*) + abort/sync-io behavior. See wrapSpawnLike.
  try { armChildProcessCompileCacheWrap(); } catch {}
}

// Arm the child_process compile-cache wrap WITHOUT eagerly requiring child_process.
//
// Eagerly `require("node:child_process")` at preload time pulls ~40 builtins into
// process.moduleLoadList on EVERY startup — net, dgram, the entire streams tree,
// spawn_sync/tty_wrap/pipe_wrap/tcp_wrap, os, vm, etc. (test-bootstrap-modules
// observes the exact list; child_process is the dominant extra-builtin source).
// A program that never spawns a child shouldn't pay that cost — and Node's own
// startup never loads child_process.
//
// So we intercept `Module._load` and apply the wrap to the child_process module the
// FIRST time USER code requires it (`require('child_process')` /
// `require('node:child_process')`), patching the returned singleton before handing
// it back. After patching once we restore the original `_load`, so steady-state
// require() has zero added overhead. If the user never requires child_process, the
// module is never loaded and the builtins stay out of the load list — matching Node.
let __cpWrapArmed = false;
function armChildProcessCompileCacheWrap() {
  if (__cpWrapArmed || __cpWrapped) return;
  __cpWrapArmed = true;
  if (typeof module_._load !== "function") return;
  const origLoad = module_._load;
  module_._load = function (request, parent, isMain) {
    const exports = origLoad.call(this, request, parent, isMain);
    if (request === "child_process" || request === "node:child_process") {
      module_._load = origLoad; // restore: one-shot, no steady-state overhead
      try { wrapChildProcessCompileCache(exports); } catch {}
    }
    return exports;
  };
}

// Monkey-patch child_process so node-targeted children the USER spawns with an
// explicit live NODE_COMPILE_CACHE get the SAME R8 treatment spawn.rs gives nub's
// own children: strip NODE_COMPILE_CACHE from the child env (so Node's bootstrap
// caches nothing of nub's inherited preload chain) and stash the original dir in a
// PID-keyed sentinel file the grandchild's restoreCompileCacheEnv() reads back via
// `process.ppid` to re-enable caching for the USER's own modules post-bootstrap.
// Brand rule: the dir travels via a sentinel file, never a NUB_* env var.
// `cp` is the already-loaded child_process exports object, passed in by the lazy
// `_load` interceptor so we never require it ourselves (which would defeat the
// deferral).
let __cpWrapped = false;
function wrapChildProcessCompileCache(cp) {
  if (__cpWrapped || !cp) return;
  __cpWrapped = true;
  const { writeFileSync } = require("node:fs");
  const { basename } = require("node:path");

  const isNodeTarget = (command) => {
    if (typeof command !== "string" || command.length === 0) return false;
    if (command === process.execPath) return true;
    const base = basename(command).toLowerCase();
    return base === "node" || base === "node.exe";
  };

  // Returns a possibly-rewritten options object with NODE_COMPILE_CACHE stripped
  // from its env, after writing the sentinel keyed on THIS process's pid (= the
  // grandchild's process.ppid). No-op unless the child env explicitly carries a
  // live (non-empty, != "0") NODE_COMPILE_CACHE — an inherited (undefined env)
  // value was already stripped from this process by nub, so nothing to do there.
  const stripFromOptions = (options) => {
    if (!options || typeof options !== "object") return options;
    const env = options.env;
    if (!env || typeof env !== "object") return options;
    const dir = env.NODE_COMPILE_CACHE;
    if (!dir || dir === "0") return options;
    try {
      writeFileSync(join(tmpdirNoOs(), `nub-ccache-${process.pid}`), String(dir));
    } catch { return options; }
    const newEnv = { ...env };
    delete newEnv.NODE_COMPILE_CACHE;
    return { ...options, env: newEnv };
  };

  // For (command, args?, options?) signatures the options object is the last arg
  // that is a non-array object; args is an optional array in between. Rewrites the
  // call in place and dispatches to the original.
  // Copy `orig`'s OWN symbols onto `wrapped` — crucially [util.promisify.custom],
  // which Node sets on execFile/exec so `util.promisify(execFile)` returns a
  // {stdout,stderr} promise. A bare wrapper without it silently changes promisify's
  // result shape (broke test-child-process-promisified / -abortController /
  // util-promisify-custom-names / sync-io-option / test-output-abort).
  const preserveSymbols = (wrapped, orig) => {
    for (const s of Object.getOwnPropertySymbols(orig)) {
      try { wrapped[s] = orig[s]; } catch { /* read-only symbol: skip */ }
    }
    return wrapped;
  };
  const wrapSpawnLike = (orig) => preserveSymbols(function (command, ...rest) {
    if (isNodeTarget(command)) {
      let optIdx = -1;
      for (let i = rest.length - 1; i >= 0; i--) {
        const a = rest[i];
        if (a && typeof a === "object" && !Array.isArray(a)) { optIdx = i; break; }
        if (typeof a === "function") continue; // execFile callback
        if (Array.isArray(a)) break; // args array — no options object present
      }
      if (optIdx >= 0) rest[optIdx] = stripFromOptions(rest[optIdx]);
    }
    return orig.call(this, command, ...rest);
  }, orig);

  cp.spawn = wrapSpawnLike(cp.spawn);
  cp.spawnSync = wrapSpawnLike(cp.spawnSync);
  cp.execFile = wrapSpawnLike(cp.execFile);
  cp.execFileSync = wrapSpawnLike(cp.execFileSync);

  // fork() always runs `process.execPath`, so it is always a node target. Its
  // signature is (modulePath, args?, options?); reuse the same options rewrite.
  const origFork = cp.fork;
  cp.fork = function (modulePath, ...rest) {
    let optIdx = -1;
    for (let i = rest.length - 1; i >= 0; i--) {
      const a = rest[i];
      if (a && typeof a === "object" && !Array.isArray(a)) { optIdx = i; break; }
      if (Array.isArray(a)) break;
    }
    if (optIdx >= 0) rest[optIdx] = stripFromOptions(rest[optIdx]);
    return origFork.call(this, modulePath, ...rest);
  };
}

function reenableUserCompileCache() {
  const dir = process.env.NODE_COMPILE_CACHE;
  // "0" is nub's disable signal (see transform-core); anything else is the user's
  // real cache dir, which we re-point Node's compile cache at for THEIR modules.
  if (!dir || dir === "0") return;
  try { module_.enableCompileCache(dir); } catch {}
}

module.exports = {
  installWatchReporting,
  makeHooks,
  installCjsRequireHooks,
  preloadPolyfillPackages,
  installTemporalLazyGlobal,
  restoreCompileCacheEnv,
  reenableUserCompileCache,
};

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
const { readdirSync, existsSync } = require("node:fs");
const { fileURLToPath, pathToFileURL } = require("node:url");
const { join, dirname, extname: pathExtname } = require("node:path");

// Internal `__NUB_*` plumbing var carrying the running binary's version (set by
// the Rust spawn layer, coupled to preload injection). Read by installVersionMarker.
const VERSION_ENV = "__NUB_VERSION";

// Whether this Node natively supports import-text (`--experimental-import-text`,
// added Node 26.5.0, #62300). Feature-DETECTED via the accepted-flag set rather
// than version-parsed — the flag is in `allowedNodeEnvironmentFlags` iff Node
// knows it (26.5+). When true, the load hook steps aside and lets Node's own
// textStrategy own `type:"text"` imports (nub injects the flag in spawn.rs), per
// the additive contract; below 26.5 nub polyfills them via `loadTextImport`.
const NATIVE_IMPORT_TEXT = process.allowedNodeEnvironmentFlags.has("--experimental-import-text");

// ── data: URL unknown-format fidelity helpers ───────────────────────
// Mirror Node's internal/modules/esm/get_format.js so nub's sync registerHooks load
// hook surfaces ERR_UNKNOWN_MODULE_FORMAT for an unsupported `data:` MIME exactly as
// plain Node does (see the load hook for why the sync tier needs this pre-check).
// `mimeToFormat`: text/application javascript -> module, application/json -> json,
// application/wasm -> wasm, anything else -> null (unknown).
function dataUrlMimeToFormat(mime) {
  if (mime == null) return null;
  if (/^\s*(text|application)\/javascript\s*(;\s*charset=utf-?8\s*)?$/i.test(mime)) return "module";
  if (mime === "application/json") return "json";
  if (mime === "application/wasm") return "wasm";
  return null;
}

// Returns true when `url` is a `data:` URL whose MIME maps to no module format —
// i.e. the case where Node would ultimately throw ERR_UNKNOWN_MODULE_FORMAT.
function unknownDataUrlFormat(url) {
  // Strip the `data:` scheme; Node parses the pathname (everything after `data:`).
  const m = /^([^/]+\/[^;,]+)(?:[^,]*?)(;base64)?,/.exec(url.slice(5));
  const mime = m ? m[1] : null;
  return dataUrlMimeToFormat(mime) === null;
}

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

// True once a USER async ESM loader is active — registered via `module.register()`
// at runtime (the tsx / ts-node/esm pattern, incl. via `--import`). On the FAST tier
// nub itself NEVER calls `module.register()` (it uses sync `module.registerHooks`),
// so on that tier any `module.register()` call is the user's. We wrap it to observe
// runtime registrations; the static CLI-flag forms (`--experimental-loader`/`--loader`
// /`--import`) are read separately from `process.execArgv` (see userAsyncLoaderActive).
let __userAsyncLoaderRegistered = false;
function installUserAsyncLoaderDetector() {
  if (typeof module_.register !== "function") return;
  const orig = module_.register;
  if (orig.__nubAsyncWrapped) return;
  const wrapped = function (...args) {
    // On the fast tier nub never calls module.register, so every call here is the
    // user's. (registerLoaderWorker — nub's only register caller — is the compat /
    // require(esm)-off path, which does not run the fast-tier sync load hook that
    // performs the commonjs-sync relabel, so a false positive there is harmless.)
    __userAsyncLoaderRegistered = true;
    return orig.apply(this, args);
  };
  wrapped.__nubAsyncWrapped = true;
  try { module_.register = wrapped; } catch {}
}

// Did the process start with a CLI flag that registers a user async ESM loader?
// `--experimental-loader` / `--loader` register a loader directly; `--import` runs a
// module that commonly calls `module.register()` (tsx, ts-node/esm). Read once from
// `process.execArgv` — these flags appear before any user code runs, so this is a
// reliable preload-time signal. Conservative on `--import`: a `--import` that does NOT
// register a loader is harmless to relabel, but presence of the flag declines the
// optimization rather than risk interop breakage (correctness over coverage).
let __cliAsyncLoaderCache;
function cliAsyncLoaderPresent() {
  if (__cliAsyncLoaderCache !== undefined) return __cliAsyncLoaderCache;
  let present = false;
  try {
    const argv = process.execArgv;
    if (Array.isArray(argv)) {
      for (const a of argv) {
        if (typeof a !== "string") continue;
        if (
          a === "--loader" || a.startsWith("--loader=") ||
          a === "--experimental-loader" || a.startsWith("--experimental-loader=") ||
          a === "--import" || a.startsWith("--import=")
        ) { present = true; break; }
      }
    }
  } catch { /* execArgv unavailable — treat as no loader */ }
  __cliAsyncLoaderCache = present;
  return present;
}

// The guard for the `commonjs-sync` relabel: is a USER async ESM loader active on the
// import-of-CJS path? Relabel ONLY when nub is the sole loader (the common case —
// next build/dev) so we never route a user loader's inner require()s through its own
// ESM resolve hook (the interop break documented at the load hook below). Either a CLI
// loader flag OR a runtime module.register() disqualifies the optimization.
function userAsyncLoaderActive() {
  return __userAsyncLoaderRegistered || cliAsyncLoaderPresent();
}

// The Node band where the async `module.register` loader's `resolveSync`/`loadSync`
// are unimplemented stubs that throw `ERR_METHOD_NOT_IMPLEMENTED`: 22.15.0 ..= 24.11.0
// (fixed in 24.11.1, nodejs/node#59666). Mirrors the Rust `node_hook_compose_broken`
// (spawn.rs) for every RELEASE version; a pre-release like `22.15.0-rc.1` reads as in-band
// here (the numeric parse ignores the `-rc` tag) where the Rust semver sorts it just below
// the 22.15.0 floor — a harmless over-selection (the async tier is always correct and the
// two signals are OR'd), never a crash. Outside this band a foreign async loader composes
// with nub's sync hooks natively, so the fast tier stays.
function nodeHookComposeBroken() {
  const p = String(process.versions.node).split(".");
  const maj = parseInt(p[0], 10) || 0;
  const min = parseInt(p[1], 10) || 0;
  const pat = parseInt(p[2], 10) || 0;
  const geFloor = maj > 22 || (maj === 22 && min >= 15);
  const leCeil = maj < 24 || (maj === 24 && (min < 11 || (min === 11 && pat === 0)));
  return geFloor && leCeil;
}

// Does a foreign async ESM loader flag (`--import` / `--loader` / `--experimental-loader`)
// ride in THIS process's own startup flags, via EITHER channel? tsx/ts-node deliver their
// loader through one of two paths, and they land in different places:
//   • re-exec argv → `process.execArgv` (tsx's bin re-execs `node --import <loader>`), and
//   • `NODE_OPTIONS="--import tsx/esm"` → `process.env.NODE_OPTIONS` (NOT hoisted into
//     execArgv — verified on Node 24.x), the common CI/shell-config delivery.
// Both must be scanned. nub's own fast-tier injection is `--require` (never `--import`/
// `--loader`) on this band, and its compat-tier `--import preload.mjs` lives below 22.15
// (outside the broken band this gates on), so any such flag here is FOREIGN.
function foreignAsyncLoaderFlagPresent() {
  if (cliAsyncLoaderPresent()) return true; // execArgv channel
  const opts = process.env.NODE_OPTIONS;
  if (typeof opts !== "string" || opts === "") return false;
  return /(?:^|\s)--(?:experimental-)?(?:import|loader)(?:=|\s|$)/.test(opts);
}

// Should nub auto-select its async loader-worker tier at PRELOAD time because a foreign
// async ESM loader (tsx/ts-node) rides in THIS process's own startup flags, on the
// broken-compose band? This is the INTRINSIC counterpart to the launcher's predictive argv
// scan (`__NUB_FORCE_ASYNC_TIER`, spawn.rs): because it reads the process's OWN flags, it
// fires regardless of how the process was launched — a nested `nub run`, a `child_process`
// spawn (Playwright's globalSetup), or a shell wrapper — all spawn shapes the launcher-side
// scan cannot see (nub#460). nub's `--require` preload runs before `--import` executes, so
// the flag is observable here before the foreign loader registers, letting nub pick the
// composable tier up front (a mid-flight sync→async swap is impossible — the loader-worker
// cannot be registered from inside another loader's registration on this band). Deliberately
// conservative: any such flag on the band takes the async tier, even the rare one that
// registers no loader — a small worker-startup cost on a shrinking Node band, never a crash.
function shouldAutoAsyncTierAtPreload() {
  return nodeHookComposeBroken() && foreignAsyncLoaderFlagPresent();
}

// ── Internal `module.register()` without the DEP0205 leak ────────────
// `module.register()` is the loader-WORKER registration surface (async ESM hooks in
// a dedicated thread). nub uses it for the compat tier (18.19–22.14, where the sync
// `module.registerHooks` doesn't exist) and for the fast tier's
// `--no-experimental-require-module` fallback (where `require(esm)` is off, so the
// in-thread sync hooks can't load transform-core.mjs synchronously). On Node 26+,
// `module.register()` emits a one-shot `[DEP0205]` DeprecationWarning steering callers
// to `module.registerHooks()` — but nub CANNOT use `registerHooks` on these paths
// (no sync surface on compat; no sync core load when require(esm) is disabled), and
// the deprecation is for nub's OWN internal call, not anything the user wrote: the
// user has no action to take, so the warning is pure noise on their stderr. Suppress
// exactly that DEP0205 emission for the duration of nub's own register() call, then
// restore `process.emitWarning` untouched, so a user's later `module.register()` (or
// any other deprecation) still warns normally. Default-preserving: only nub's
// internal call is silenced, only for DEP0205, only on the versions that emit it.
function registerLoaderWorker(specifier, parentURL, options) {
  const realEmitWarning = process.emitWarning;
  let restored = false;
  const restore = () => {
    if (restored) return;
    restored = true;
    try { process.emitWarning = realEmitWarning; } catch {}
  };
  try {
    process.emitWarning = function (warning, ...rest) {
      // Node calls emitWarning(msg, 'DeprecationWarning', 'DEP0205', ...) for the
      // module.register() deprecation. Swallow only that exact code; pass everything
      // else (including any non-DEP0205 deprecation) straight through.
      const code = typeof rest[0] === "object" && rest[0] !== null ? rest[0].code : rest[1];
      if (code === "DEP0205") return;
      return realEmitWarning.call(this, warning, ...rest);
    };
    return module_.register(specifier, parentURL, options);
  } finally {
    restore();
  }
}

// Is this error Node's "an async `module.register` loader cannot service a SYNCHRONOUS
// resolve/load" stub? On Node 22.15–~24.11 the async-hooks proxy's `resolveSync`/
// `loadSync` are stubs that unconditionally `throw new ERR_METHOD_NOT_IMPLEMENTED(...)`.
// nub's fast-tier SYNC `module.registerHooks` hooks force EVERY resolve/load onto the
// synchronous chain; when a USER async loader (e.g. @tailwindcss/node's
// esm-cache.loader.mjs under Turbopack) is ALSO registered, the chain's default step
// reaches that stub and throws — killing the build. We must detect this WITHOUT the
// `userAsyncLoaderActive()` flag: the very FIRST throwing resolution is the loader
// module's own specifier, resolved DURING `module.register` before the detector flag is
// observable, so the flag is false exactly when recovery is needed. The error code +
// message is the reliable signal. (Node 24.12+/25.2+/26 implement these methods, so the
// stub never throws there and this never fires.)
function isAsyncLoaderSyncStub(err) {
  if (!err || typeof err.message !== "string") return false;
  // Two shapes across the affected Node band: (a) the method exists but is a stub that
  // throws ERR_METHOD_NOT_IMPLEMENTED('resolveSync()'/'loadSync()') (e.g. 24.3); (b) the
  // method is ENTIRELY ABSENT, so Node's `this[#customizations].resolveSync/loadSync(...)`
  // throws a TypeError "... is not a function" (e.g. 22.16/24.11). Match both.
  if (err.code === "ERR_METHOD_NOT_IMPLEMENTED" &&
      (err.message.includes("resolveSync") || err.message.includes("loadSync"))) {
    return true;
  }
  return err instanceof TypeError &&
    (err.message.includes("resolveSync is not a function") ||
     err.message.includes("loadSync is not a function"));
}

// Resolve a specifier to a URL the registerHooks resolve chain can return, as the
// recovery path when Node's default resolve step throws the async-loader stub (see
// isAsyncLoaderSyncStub). Returns `{ url, shortCircuit }`, or null if it cannot resolve
// (caller re-throws the original error so behavior is unchanged).
//
// Uses the parent's CommonJS resolver (`createRequire().resolve`) — the only fully-sync,
// conditions-capable resolver available in this `--require` CJS preload. It honors
// `node_modules`, package `exports`, relative + absolute paths, and bare/scoped
// specifiers, but under the REQUIRE condition; a DUAL package whose `exports` differ by
// `import`/`require` gets its `require` build where Node's default ESM resolve would
// return the `import` build (latent — the in-the-wild trigger resolves non-dual internal
// modules). Builtins MUST be mapped back to `node:`: `require.resolve("fs")` returns the
// bare name `"fs"`, which must not become a bogus `file://<cwd>/fs` URL.
function resolveViaParentRequire(specifier, parentURL) {
  try {
    // An ALREADY-RESOLVED specifier needs no resolution — return it verbatim. This is the
    // common Turbopack/Tailwind trigger: Node resolves the loader module's OWN absolute
    // `file://` URL (`@tailwindcss/node/dist/esm-cache.loader.mjs`) synchronously during
    // `module.register`, with `parentURL` = `data:` (no useful base). `require.resolve`
    // cannot take a `file://` URL string, so pass these through directly. Builtins and
    // `data:` specifiers are likewise already-resolved.
    if (specifier.startsWith("file:") || specifier.startsWith("data:")) {
      return { url: specifier, shortCircuit: true };
    }
    if (specifier.startsWith("node:") || module_.isBuiltin(specifier)) {
      return { url: specifier.startsWith("node:") ? specifier : `node:${specifier}`, shortCircuit: true };
    }
    const base = parentURL && String(parentURL).startsWith("file:")
      ? String(parentURL)
      : pathToFileURL(join(process.cwd(), "noop.js")).href;
    const resolved = module_.createRequire(base).resolve(specifier);
    if (module_.isBuiltin(resolved)) {
      return { url: resolved.startsWith("node:") ? resolved : `node:${resolved}`, shortCircuit: true };
    }
    const url = resolved.startsWith("node:") || resolved.startsWith("data:")
      ? resolved
      : pathToFileURL(resolved).href;
    return { url, shortCircuit: true };
  } catch {
    return null;
  }
}

// Phantom-dependency remediation for nub's isolated layout. The isolated layout
// (the post-flip default for npm/yarn/bun incumbents, and the standing default
// for pnpm/fresh projects) exposes only DECLARED packages at the importer's
// node_modules, so an UNDECLARED transitive that resolved under a flat
// node_modules fails. When a BARE specifier hits (ERR_)MODULE_NOT_FOUND,
// `phantomDepHint` returns the one-line opt-out to append — but ONLY when the
// package is genuinely a phantom dep.
//
// Two conditions, both required, so the hint fires ONLY for a true phantom dep:
//   (1) the bare package is in the project's graph — nub names every graph
//       package `<name>@<version>` (scoped `/` → `+`) under `node_modules/.store`,
//       so a matching entry means it's installed (GVS: symlinks into the global
//       store; non-GVS: real dirs);
//   (2) the bare package is NOT reachable at any `node_modules/<pkg>` up the
//       tree — otherwise the package resolves and the MODULE_NOT_FOUND is a
//       missing SUBPATH/file inside it (`react-dom/client-typo`), not a phantom.
// A genuine typo / absent dep fails (1); a subpath miss of a declared dep fails
// (2); the hoisted opt-out leaves no `<name>@*` entries (only internal state),
// and node-suite / plain-Node trees have no `.store` at all — none get the hint.
// NOT COVERED: the compat-tier ESM loader (Node 18.19–22.14 `import`) resolves
// in preload-async-hooks.mjs and can't reach this CJS helper, so that band
// surfaces Node's standard error without the hint; the fast tier (22.15+) and
// CommonJS `require()` on every tier are covered.
function phantomDepHint(specifier, fromPath) {
  if (!specifier || !fromPath) return null;
  const c = specifier[0];
  if (c === "." || c === "/" || c === "#" || c === "\\") return null;
  if (specifier.startsWith("node:") || module_.isBuiltin(specifier)) return null;
  if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(specifier)) return null; // file:/data:/http:/C:\ …
  const parts = specifier.split("/");
  const pkg = c === "@" ? parts.slice(0, 2).join("/") : parts[0];
  if (!pkg) return null;
  let dir;
  try {
    dir = dirname(String(fromPath).startsWith("file:") ? fileURLToPath(fromPath) : fromPath);
  } catch {
    return null;
  }
  const prefix = pkg.replace(/\//g, "+") + "@";
  let inStore = false;
  for (let i = 0; i < 40 && dir; i++) {
    const nm = join(dir, "node_modules");
    // Reachable here ⇒ the bare package resolves, so this miss is a subpath/file
    // inside it, not a phantom dep — suppress. existsSync follows the isolated
    // layout's top-level symlink to the real package.
    if (existsSync(join(nm, pkg))) return null;
    if (!inStore) {
      try {
        inStore = readdirSync(join(nm, ".store")).some((e) => e.startsWith(prefix));
      } catch {
        /* no `.store` at this level */
      }
    }
    const parent = dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  // In the graph's virtual store yet never reachable up the tree → phantom dep.
  return inStore
    ? `\n\n"${pkg}" is installed but not reachable here — it's a phantom dependency (an ` +
        `undeclared transitive). nub's isolated node_modules exposes only the packages ` +
        `declared in package.json. Add "${pkg}" to your dependencies, or add ` +
        "`node-linker=hoisted` to .npmrc to use a flat node_modules."
    : null;
}

// Append `hint` to a thrown error's message AND its printed `.stack`. V8
// materializes `.stack` LAZILY from the current `.message`, so the original
// hint-free stack MUST be read before `.message` is mutated — reading it after
// would already contain the new message and the `.replace` would double the
// hint. The marker makes it idempotent if the same error passes two catch
// sites. Best-effort: a frozen/exotic error is left verbatim.
function annotateError(err, hint) {
  try {
    if (!err || typeof err !== "object" || err.__nubPhantomHinted) return;
    Object.defineProperty(err, "__nubPhantomHinted", { value: true, configurable: true });
    const oldStack = typeof err.stack === "string" ? err.stack : null;
    const oldMsg = err.message;
    err.message = oldMsg + hint;
    if (oldStack !== null) {
      err.stack = oldStack.includes(oldMsg) ? oldStack.replace(oldMsg, err.message) : oldStack + hint;
    }
  } catch {
    /* read-only error: leave verbatim */
  }
}

function makeHooks(core, watchReporting) {
  installUserHookDetector();
  installUserAsyncLoaderDetector();

  function resolve(specifier, context, nextResolve) {
    const r = core.resolveSpec(specifier, context.parentURL);
    if (r) return r;
    // Yarn PnP (ESM): PnP doesn't patch the ESM loader, so `import` of a PnP dep must
    // be resolved explicitly — through `pnpapi.resolveRequest`, passing Node's
    // `context.conditions` (the import-side set) so a DUAL package resolves to its
    // `import` build, not its `require` build. Returns a virtual `.zip` path Node
    // reads via the zipfs patch. If the api is momentarily unavailable we fall through
    // to `nextResolve`, which reaches nub's `_resolveFilename` override (delegating to
    // PnP) — so a plain dep still resolves; only a dual package's condition is lost.
    const pnp = pnpApi();
    if (pnp && !module_.isBuiltin(specifier) && !specifier.startsWith("node:")) {
      try {
        const res = pnpResolveEsm(pnp, specifier, context);
        if (res) return res;
      } catch { /* fall through to Node's resolver */ }
    }
    try {
      return nextResolve(specifier, context);
    } catch (err) {
      if (isAsyncLoaderSyncStub(err)) {
        const fallback = resolveViaParentRequire(specifier, context.parentURL);
        if (fallback) return fallback;
      }
      if (err && err.code === "ERR_MODULE_NOT_FOUND") {
        const hint = phantomDepHint(specifier, context.parentURL);
        if (hint) annotateError(err, hint);
      }
      throw err;
    }
  }

  // Recovery for the async-loader `loadSync` stub: load a `file:` module ourselves when
  // Node's default load step throws ERR_METHOD_NOT_IMPLEMENTED (see isAsyncLoaderSyncStub).
  // Reads source from disk and derives the format from extension + nearest package `type`
  // (nub's own moduleFormatFor — the same source-of-truth as the transpile path). For a
  // transpilable TS/JSX file outside node_modules, route through nub's transpiler; for a
  // data-extension, through loadData; otherwise hand back raw source with the derived
  // format. Returns null if it cannot (caller re-throws). A CJS result keeps source:null
  // and hands off to the native CommonJS loader, matching the default-load contract.
  function loadViaDisk(url, ext) {
    try {
      const path = fileURLToPath(url);
      if (core.TRANSPILE_EXTS.has(ext) && !core.isNodeModules(url)) {
        return core.loadTranspile(url, ext);
      }
      // Plain JS: transpile only when transformable; else fall through to the raw-
      // source path below (which hands CJS back as `source:null` → Node's native
      // CJS loader), byte-identical to a non-intercepted file.
      if (core.PLAIN_JS_EXTS.has(ext) && !core.isNodeModules(url)) {
        const r = core.maybeTranspilePlainJs(url, ext);
        if (r) return r;
      }
      if (ext in core.DATA_EXTS) return core.loadData(url, ext);
      const { readFileSync } = require("node:fs");
      const source = readFileSync(path);
      const pkgType = core.getPackageType(dirname(path));
      const format = core.moduleFormatFor(ext, pkgType, path, source.toString("utf8"));
      if (format === "commonjs") return { format: "commonjs", source: null, shortCircuit: true };
      return { format, source, shortCircuit: true };
    } catch {
      return null;
    }
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

    // Import Text (attribute-keyed): honor `with { type: "text" }` on ANY extension,
    // ahead of extension dispatch so `import s from "./c.yaml" with {type:"text"}`
    // returns raw text, not parsed YAML. On Node 26.5+ (NATIVE_IMPORT_TEXT) step aside
    // and let Node's own textStrategy own it — nub injects --experimental-import-text,
    // so the additive "would plain Node + the flag do the same?" test holds and users
    // get Node's exact semantics. Below 26.5 Node has no text-import support, so nub
    // polyfills via loadTextImport (placed after watch reporting so a text file gets
    // the same watch treatment, and before the extension/data dispatch so the attribute
    // wins over the `.txt`/`.yaml`/… data loaders and Node-native JSON; it shortCircuits
    // so Node's own unknown-'text'-attribute validation never runs). The compat-tier
    // hook in preload-async-hooks.mjs always polyfills — that tier tops out at Node
    // 22.14, below 26.5, so native import-text is never reachable there.
    // (Node 18.20+ parses the `with` syntax; the 18.19.x floor cannot.)
    if (context?.importAttributes?.type === "text") {
      return NATIVE_IMPORT_TEXT ? nextLoad(url, context) : core.loadTextImport(url);
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
    // Project-source plain JS (`.js`/`.mjs`/`.cjs`): transpile ONLY when it carries
    // transformable syntax. A no-op plain-JS file returns null here and falls through
    // to Node's native loader (the `nextLoad`/relabel path below) BYTE-FOR-BYTE — it
    // is never intercepted, so native CJS/ESM behavior (the relabel, require.cache,
    // the require-of-ESM-syntax-`.cjs` error) is preserved. node_modules excluded.
    if (core.PLAIN_JS_EXTS.has(ext) && !core.isNodeModules(url)) {
      const r = core.maybeTranspilePlainJs(url, ext);
      if (r) return r;
    }
    if (ext in core.DATA_EXTS) return core.loadData(url, ext);

    // Fidelity: a `data:` URL whose MIME maps to no module format (e.g.
    // `data:application/x-unknown,…`) must surface Node's ERR_UNKNOWN_MODULE_FORMAT.
    // Node's default load returns `format: null` for this, which its ASYNC loader path
    // later converts to ERR_UNKNOWN_MODULE_FORMAT in validateLoadResult. But nub's SYNC
    // `module.registerHooks` load hook routes the default step's result through
    // customization_hooks' validateLoadSloppy -> validateFormat, which accepts only a
    // string or `undefined` and throws ERR_INVALID_RETURN_PROPERTY_VALUE on `null` —
    // and it does so INSIDE the `nextLoad` call below (the validator wraps each step),
    // so nub never gets the result back to normalize it, and nub's own load-hook frame
    // leaks into the user-visible stack. Vanilla Node, having registered no hook on this
    // path, never hits that validator and throws the correct ERR_UNKNOWN_MODULE_FORMAT.
    // Return `format: undefined` (not the default step's `null`) WITHOUT calling the
    // default load: undefined passes validateFormat, then Node's own
    // #translate -> validateLoadResult sees format == null and throws the NATIVE
    // ERR_UNKNOWN_MODULE_FORMAT — byte-identical to plain Node (the `[code]` name
    // decoration, the exact message, and a stack with zero nub frames). Short-circuit
    // so the chain stops here; the empty source is never read (the throw precedes any
    // translation).
    if (typeof url === "string" && url.startsWith("data:") &&
        (!context || context.format == null) &&
        unknownDataUrlFormat(url)) {
      return { format: undefined, source: "", shortCircuit: true };
    }

    let r;
    try {
      r = nextLoad(url, context);
    } catch (err) {
      // Async-loader `loadSync` stub (the load-hook twin of the resolve recovery): with a
      // USER async `module.register` loader present, nub's sync load hook forces the default
      // step onto the synchronous chain, which calls the async-hooks `loadSync` stub →
      // ERR_METHOD_NOT_IMPLEMENTED. Recover by loading the module ourselves: read source
      // from disk and derive the format from the URL/extension (the same source-of-truth
      // nub's own transpile path uses), so the synchronous module-job gets real source
      // instead of crashing. node:/data:/non-file URLs and the no-format case fall through
      // to a re-throw (we can't synthesize those here). Re-throw anything that isn't the stub.
      if (isAsyncLoaderSyncStub(err) && typeof url === "string") {
        // Builtins: hand back the `builtin` format with no source — Node loads them
        // natively (a `node:`-scheme module the default load would have returned builtin for).
        if (url.startsWith("node:") || module_.isBuiltin(url)) {
          return { format: "builtin", source: null, shortCircuit: true };
        }
        if (url.startsWith("file:")) {
          const recovered = loadViaDisk(url, ext);
          if (recovered) return recovered;
        }
      }
      throw err;
    }

    // #18 — relabel a `commonjs` result as `commonjs-sync` for `file:` URLs ON THE
    // `import()`-OF-CJS PATH so the module (and its inner require()s) routes through
    // Node's SOUND synchronous CJS translator (loadCJSModuleWithModuleLoad), which
    // builds a real CJS `require` with `.cache`/`.extensions`. Without this, nub's
    // sync load hook makes Node pick loadCJSModuleWithSpecialRequire — a hand-rolled
    // `require` missing `.cache`/`.extensions` — so CJS that does `require.cache[...]`
    // (next's bundled `conf`) crashes with "Cannot convert undefined or null to
    // object". Version-banded in Node: broken 22.15–~25, fixed in 26 via the
    // special-require repair (#60380); the relabel is a no-op on 26.
    //
    // Two narrowing guards make this surgical — relabel ONLY the path that actually
    // hits the broken translator, so nothing else moves:
    //
    //  (1) IMPORT PATH ONLY. The bad special-require is chosen only when CJS is
    //      reached via dynamic `import()`. On the broken band Node passes that load
    //      step `context.conditions` as an ARRAY containing "import"; a plain
    //      `require()` of the same file passes an empty-object `conditions` (no
    //      "import"). Gating on the "import" condition leaves `require()` loads
    //      untouched — without this, relabeling a `require()`-loaded `.cjs` that
    //      contains ESM syntax makes Node's sync translator accept it instead of
    //      throwing "Unexpected token 'export'" (a real regression: it swallows the
    //      syntax error vanilla Node raises). On Node 26 the require path also carries
    //      an array but without "import", and the relabel is a no-op there regardless.
    //
    //  (2) NO USER LOADER/HOOK. Skip whenever a USER async ESM loader OR a USER sync
    //      `module.registerHooks` hook is active. Relabeling makes Node treat the
    //      module as ESM-translatable, re-routing inner resolution through the user's
    //      hook chain — which changes resolve-hook call shape/count (the
    //      module-hooks resolve-import-cjs contract) and breaks async-loader interop
    //      (the source-backfill EXCEPTION block below documents the same hazard). When
    //      nub is the SOLE loader (the next-build/dev common case) the path is nub's
    //      own, so the relabel is safe; otherwise leave the native-CJS handoff intact.
    if (
      r && r.format === "commonjs" &&
      typeof url === "string" && url.startsWith("file:") &&
      Array.isArray(context && context.conditions) &&
      context.conditions.includes("import") &&
      !__userHooksRegistered && !userAsyncLoaderActive()
    ) {
      return { ...r, format: "commonjs-sync" };
    }

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
    // ('commonjs-sync' is excluded for the same reason as 'commonjs' — it's the
    // sync CJS handoff the #18 relabel above produces; backfilling its source would
    // re-route inner require()s through the ESM wrapper. The relabel block early-
    // returns, so this is defense-in-depth against a future refactor.)
    if (
      r && r.source == null && r.format &&
      r.format !== "commonjs" && r.format !== "commonjs-sync" && r.format !== "builtin" &&
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
    // Yarn PnP (CJS): `.pnp.cjs` already patched THIS function (origResolveFilename)
    // to resolve from PnP's manifest, including zip-stored deps — so we just delegate
    // to it. The one snag is that a registered customization hook makes Node thread a
    // `conditions` option that PnP rejects ("aren't supported by PnP yet
    // (conditions)"), so strip it first. The require/default condition PnP then
    // applies is exactly right for `require()`. This replaces the former
    // `pnpapi.resolveRequest` reimplementation: simpler, and with no `findPnpApi` in
    // the hot path there is no lookup-miss to leak a `conditions` crash on Windows.
    //
    // GATED ON PnP (`process.versions.pnp`). Off PnP the strip is NOT a harmless
    // no-op: a user who passes custom `conditions` to require-side resolution via
    // `module.registerHooks` (Node's module-hooks custom-conditions tests) relies on
    // Node's own `_resolveFilename` honoring them, and unconditionally deleting the
    // key silently dropped their conditions — breaking module-hooks/test-module-hooks-
    // custom-conditions{,-cjs}. PnP is the only resolver that rejects `conditions`, so
    // only strip when PnP is actually active; everywhere else conditions pass through.
    if (process.versions.pnp && options && "conditions" in options) {
      options = { ...options };
      delete options.conditions;
    }
    try {
      return origResolveFilename.call(this, request, parent, isMain, options);
    } catch (e) {
      // Under PnP, an in-tree issuer requiring a dep NOT in its manifest makes PnP
      // throw. That is nub's OWN transpile helpers (e.g. `@oxc-project/runtime`),
      // injected into transpiled user code and resolved via NODE_PATH globalPaths
      // (A30). Fall back to Node's native path resolver, which PnP does NOT patch.
      // Gated to PnP so off-PnP a genuine miss surfaces Node's own error unchanged.
      if (process.versions.pnp && e && e.code === "MODULE_NOT_FOUND") {
        const lookupPaths = module_._resolveLookupPaths(request, parent) || [];
        const found = module_._findPath(request, lookupPaths, isMain);
        if (found) return found;
      }
      if (e && e.code === "MODULE_NOT_FOUND") {
        const fromPath = parent && typeof parent.filename === "string" ? parent.filename : null;
        const hint = phantomDepHint(request, fromPath);
        if (hint) annotateError(e, hint);
      }
      throw e;
    }
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

  // Project-source plain JS (`.js`/`.cjs`) routes through the SAME pipeline so
  // `using`/`v`-flag-RegExp/decorators lower uniformly on the classic require tier —
  // but ONLY when the file carries transformable syntax. THREE cases, each preserving
  // Node's native behavior where it must:
  //   (1) node_modules dep → native handler (deps are NEVER transpiled). Node has no
  //       own `_extensions['.cjs']` (only `.js`/`.json`/`.node`), and compiles `.cjs`
  //       through the same CJS path as `.js`, so the `.cjs` bail falls back to the
  //       `.js` handler (`|| nativeJs`) — without it a node_modules `.cjs` require
  //       would call `undefined` and crash.
  //   (2) project file with transformable syntax → transpile (lower it).
  //   (3) project file with NOTHING to lower → the ORIGINAL native handler, raw bytes
  //       compiled exactly as Node would. We never serve our own source for a no-op
  //       file, so require.cache / the require-of-ESM-syntax-`.cjs` SyntaxError / every
  //       native CJS behavior is byte-identical.
  // `.mjs` is ESM-only; Node registers no require.extensions handler for it, so a
  // `require()` of `.mjs` throws ERR_REQUIRE_ESM as before — we don't override it.
  const nativeJs = module_._extensions[".js"];
  for (const ext of [".js", ".cjs"]) {
    const origExtension = module_._extensions[ext] || nativeJs;
    module_._extensions[ext] = (mod, filename) => {
      if (core.isNodeModules(pathToFileURL(filename).href)) {
        return origExtension.call(module_._extensions, mod, filename); // (1)
      }
      const r = core.maybeTranspilePlainJs(pathToFileURL(filename).href, pathExtname(filename));
      if (r) {
        if (r.format === "module") throw requireEsmError(filename); // (2)
        mod._compile(r.source, filename);
        return;
      }
      return origExtension.call(module_._extensions, mod, filename); // (3)
    };
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
  // from the child's env, after writing the sentinel keyed on THIS process's pid
  // (= the grandchild's process.ppid). Two source cases, both stripped:
  //   • EXPLICIT env (options.env carries NODE_COMPILE_CACHE) — strip from it.
  //   • INHERITED env (no options.env, child inherits this process's env) — when
  //     OUR process.env carries a live NODE_COMPILE_CACHE, materialize an explicit
  //     env from process.env with it removed. This case matters now that the
  //     DEFAULT (nub-owned) cache also travels via the sentinel and gets restored
  //     into process.env: a node child the user spawns with NO explicit env would
  //     otherwise inherit it and enable the cache AT BOOTSTRAP — before any preload
  //     gate — collapsing that child's V8 coverage if it runs under
  //     --experimental-test-coverage (the test-runner coverage-width fixtures, which
  //     are spawned with inherited env). Stripping here makes every node-target
  //     child boot cache-off; its own preload re-enables the cache post-bootstrap
  //     via reenableUserCompileCache UNLESS it's collecting coverage.
  const stripFromOptions = (options) => {
    const inheritedDir = process.env.NODE_COMPILE_CACHE;
    const opts = options && typeof options === "object" ? options : {};
    const env = opts.env;
    if (env && typeof env === "object") {
      const dir = env.NODE_COMPILE_CACHE;
      if (!dir || dir === "0") return options;
      try {
        writeFileSync(join(tmpdirNoOs(), `nub-ccache-${process.pid}`), String(dir));
      } catch { return options; }
      const newEnv = { ...env };
      delete newEnv.NODE_COMPILE_CACHE;
      return { ...opts, env: newEnv };
    }
    // Inherited env path: only act when this process actually carries a live cache
    // dir (otherwise there is nothing for the child to inherit and we leave the
    // spawn's env untouched — `undefined` keeps Node's default inheritance).
    if (!inheritedDir || inheritedDir === "0") return options;
    try {
      writeFileSync(join(tmpdirNoOs(), `nub-ccache-${process.pid}`), String(inheritedDir));
    } catch { return options; }
    const newEnv = { ...process.env };
    delete newEnv.NODE_COMPILE_CACHE;
    return { ...opts, env: newEnv };
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

// True when V8 code coverage is active for THIS process — `--experimental-test-
// coverage` / a bare `--test-coverage*` flag in our own argv or execArgv, or a
// non-empty NODE_V8_COVERAGE env. A WARM compile cache makes V8 coverage imprecise
// (cached bytecode collapses/omits per-branch ranges, so a fixture's coverage
// JSON loses `functions[].ranges[1]` and the line/branch percentages drift from
// plain node). nub must therefore NOT (re)enable its compile cache for any process
// that is collecting coverage. This mirrors spawn.rs's coverage gate, but catches
// the case spawn.rs cannot see: a grandchild the USER's test code spawns directly
// (e.g. `spawnSync(execPath, [fixture], { env: { NODE_V8_COVERAGE } })`), which
// inherits nub's preload via NODE_OPTIONS but never goes through nub's Rust spawn
// path — so the gate has to live here too. (Observed against parallel/test-v8-
// coverage, test-runner-coverage-thresholds, and the test-runner coverage-width
// snapshot tests, all of which warm-cache then collect coverage in a child.)
function coverageActiveInProcess() {
  if (process.env.NODE_V8_COVERAGE) return true;
  const hasCovFlag = (a) =>
    typeof a === "string" &&
    (a === "--experimental-test-coverage" || a.startsWith("--test-coverage"));
  return (process.execArgv || []).some(hasCovFlag) || (process.argv || []).some(hasCovFlag);
}

function reenableUserCompileCache() {
  // Coverage active: leave the compile cache OFF so V8 collects precise per-branch
  // ranges (a warm cache collapses them — see coverageActiveInProcess). Two things
  // matter, because Node's test runner spawns a SEPARATE isolated child to run the
  // covered fixture and that child enables its cache at BOOTSTRAP from an inherited
  // NODE_COMPILE_CACHE — too early for any preload gate to catch:
  //   (a) don't enableCompileCache in THIS process, and
  //   (b) set NODE_DISABLE_COMPILE_CACHE=1 in our env so EVERY descendant (incl. the
  //       test runner's isolated coverage child) boots with the cache off. Node
  //       honors NODE_DISABLE_COMPILE_CACHE at bootstrap and it travels via the env,
  //       reaching children nub never spawns itself. We do NOT clear NODE_COMPILE_CACHE
  //       (user code may read it); the disable var takes precedence at bootstrap.
  // This is the JS half of the compile-cache/coverage fix; spawn.rs is the Rust half
  // (it never sets the DEFAULT cache when it can see coverage in nub's own
  // argv/NODE_OPTIONS/NODE_V8_COVERAGE).
  //
  // INTENTIONAL COVERAGE JUDGMENT CALL (b): NODE_DISABLE_COMPILE_CACHE=1 is set in
  // process.env, so it propagates SUBTREE-WIDE for the rest of this coverage session —
  // it is inherited by EVERY descendant, including non-coverage grandchildren the
  // user's test code spawns mid-run (e.g. a build step a covered test shells out to).
  // Those grandchildren therefore ALSO lose compile caching for the duration, even
  // though they aren't themselves collecting coverage. We accept this: the disable var
  // is the only mechanism that reaches the test runner's isolated coverage child
  // (which boots its cache before any preload gate can fire), and scoping it more
  // tightly than "the whole coverage subtree" isn't possible via an inherited env var.
  // The cost is bounded (no caching during one coverage session) and self-healing (a
  // grandchild spawned outside a coverage run is unaffected); surprising only to a
  // user who expects an unrelated grandchild to keep caching while a parent collects
  // coverage. Documented here and at the spawn.rs coverage branch (judgment call (a)).
  if (coverageActiveInProcess()) {
    const dir = process.env.NODE_COMPILE_CACHE;
    if (!dir || dir === "0") {
      // No explicit cache: keep coverage precise by disabling nub's default
      // subtree-wide (the only mechanism that reaches the test runner's isolated
      // coverage child, which boots its cache before any preload gate can fire).
      process.env.NODE_DISABLE_COMPILE_CACHE = "1";
      return;
    }
    // Explicit NODE_COMPILE_CACHE present: the user's choice wins over coverage
    // precision (the maintainer, 2026-06-11) — same tradeoff they'd have on plain node.
    // Narrow accepted caveat: a nub-DEFAULT dir restored from the sentinel is
    // indistinguishable from a user dir here; that combination is reachable only
    // when coverage is invisible to nub's own spawn (e.g. a c8-style grandchild
    // setting NODE_V8_COVERAGE itself). Simple + documented beats provenance
    // plumbing through the sentinel.
    try { module_.enableCompileCache(dir); } catch {}
    return;
  }
  const dir = process.env.NODE_COMPILE_CACHE;
  // "0" is nub's disable signal (see transform-core); anything else is the user's
  // real cache dir, which we re-point Node's compile cache at for THEIR modules.
  if (!dir || dir === "0") return;
  try { module_.enableCompileCache(dir); } catch {}
}

// Publish `process.versions.nub` = the running binary's version — the universal
// `process.versions.<runtime>` self-identification convention (cf. `.bun`,
// `.electron`). A read-only DETECTION marker, never an importable/callable API.
// The value comes from the Rust spawn layer's `__NUB_VERSION` (env!("CARGO_PKG_VERSION"),
// coupled to preload injection), so it always matches the running binary; under
// `--node`/`NODE_COMPAT` no preload runs and the var is unset, so the marker is
// absent (plain-Node fingerprint). Shape mirrors Node's own `process.versions`
// entries: enumerable + configurable + non-writable. Called once per main-thread
// preload entry (fast + compat), so a single mechanism covers both tiers.
function installVersionMarker() {
  const v = process.env[VERSION_ENV];
  if (!v) return;
  try {
    Object.defineProperty(process.versions, "nub", {
      value: v,
      writable: false,
      enumerable: true,
      configurable: true,
    });
  } catch {}
}

module.exports = {
  installVersionMarker,
  installWatchReporting,
  registerLoaderWorker,
  makeHooks,
  shouldAutoAsyncTierAtPreload,
  installCjsRequireHooks,
  preloadPolyfillPackages,
  installTemporalLazyGlobal,
  restoreCompileCacheEnv,
  reenableUserCompileCache,
};

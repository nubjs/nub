// Nub transform core — the single source of truth shared by both hook tiers.
//
// runtime/preload.mjs (fast path, Node 22.15+, sync `module.registerHooks`) and
// the compat-tier loader worker (Node 18.19–22.14, async `module.register` →
// runtime/preload-async-hooks.mjs) both import every resolution + transpile
// primitive from here. The tier files own only the parts that genuinely differ:
// hook registration (sync vs async signatures), polyfill preloading, the
// Temporal lazy global, watch-mode IPC, and the compat-tier CJS `require()`
// shim. EVERYTHING about how a file is resolved and transpiled — extension
// probing, the `.js`→`.ts` swap, tsconfig `paths`, module-format detection,
// transform options (including `target: 'es2022'` `using`-lowering), the
// Stage-3 decorator guard, the on-disk cache, data-format imports, package
// clobbering — lives here, so the two tiers can never drift. (They used to:
// separate copies diverged on probe order, `target` lowering, the decorator
// guard, module-format detection, the Temporal clobber's named exports, and the
// reserved-export filter — every one a real compat bug. This module is the fix.)
//
// Side effects are confined to: loading the N-API addon (data parsers + the
// in-process TS/JSX transpiler), and reading/writing the transpile cache. There is
// no top-level hook registration here — importing this module never augments the
// realm; the tier files do that.

// EVERY node: builtin this module needs is pulled in via CJS `require()` / `process
// .getBuiltinModule` (below), NOT via static ESM `import`. This is load-bearing for
// loader compatibility (R11): nub loads transform-core through `require(esm)`, and
// Node's `require(esm)` instantiates the module by walking its STATIC IMPORT graph
// through whatever ESM loader hooks are registered — including the USER's
// `--loader`/`register()` chain. Static `import get-tsconfig`/`./version.mjs`/`node:*`
// here therefore once leaked nub's entire internal graph (transform-core,
// version.mjs, get-tsconfig, their transitive node_modules deps, and the node:
// builtins) THROUGH the user's resolve/load hooks, which observed and corrupted it
// (a user load hook returning `source: 1` for version.mjs, a strict loader throwing
// on a bare specifier — see test-esm-loader-chaining, -example-loader,
// -preserve-symlinks-not-found, test-shadow-realm-custom-loaders). Verified: a CJS
// `require()` of a builtin does NOT route through the ESM loader chain, so loading
// off it bypasses the user chain entirely. As of this migration the point is
// stronger: transform-core `require()`s ZERO npm packages — the transpiler, TS/JSX
// detection, tsconfig discovery/parse, the additive TS-resolver, AND the transpile
// cache are ALL native calls into nub's own N-API addon (loaded by absolute `.node`
// path, off the loader chain), and the version.mjs text read is gone (the cache
// version is baked into the addon). So the worst historical leaks — oxc-transform's
// and then get-tsconfig's graphs pulled through the user chain — are gone by
// construction; only node: builtins remain, fetched off the chain. `process
// .getBuiltinModule` fetches node: builtins synchronously off the loader chain;
// `createRequire(import.meta.url)` resolves the (now CommonJS-only) vendored
// polyfills + the `@oxc-project/runtime` helpers from nub's distribution.
// This file keeps its `export`s (it stays an ES module) but has ZERO static
// imports — INCLUDING zero static `import` of any `node:` builtin — so `require(esm)`
// of transform-core finds no dependency graph to route through the user loader.
// This is load-bearing, not cosmetic: transform-core previously carried a static
// `import { createRequire } from "node:module"`. That import sat in transform-core's
// static graph, so when nub's fast-tier preload.cjs does `require("./transform-core
// .mjs")` (a `require(esm)`), Node instantiated transform-core by walking its static
// import graph THROUGH the user's pre-registered `--experimental-loader` /
// `module.register` chain — and a user resolve hook that rejects or rewrites
// `node:module` (e.g. the example-loader that throws on any non-`./`/`../`/URL
// specifier) then exploded nub's own load, while resolve-count loaders saw a phantom
// `node:module` hit. (Observed against es-module/test-esm-example-loader,
// -loader-chaining, -initialization, -preserve-symlinks-not-found, and
// parallel/test-shadow-realm-custom-loaders.) The earlier comment here claimed the
// `node:module` import was "never routed through a user loader hook" — that was
// FALSE for the fast-tier `require(esm)` path, and is the bug this rewrite fixes.
//
// `process.getBuiltinModule` (Node 22.3 / backported to 20.16 / 18.20.4) fetches a
// node: builtin synchronously OFF the loader chain, with no static import — so on
// the fast tier (22.15+, the only tier that loads transform-core via `require(esm)`,
// and where getBuiltinModule ALWAYS exists) there is nothing in the graph for a user
// loader to observe. On the narrow FLOOR below 22.3/20.16/18.20.4 (18.19.x,
// 20.11–20.15, 22.0–22.2) it's `undefined`; there, transform-core is loaded ONLY via
// static ESM `import` from the compat-tier entries (preload.mjs main thread /
// preload-async-hooks.mjs loader worker), both OFF any user loader chain — so the
// floor's `node:module` access cannot leak.
//
// BRAND BOUNDARY — the floor's `createRequire` is THREADED IN THROUGH MODULE SCOPE,
// not parked on `globalThis`. floor-builtin.mjs holds the lone static `import {
// createRequire } from "node:module"` and pushes the value in here via the
// `setBootstrapCreateRequire` setter below; nothing is ever written to the user's
// global object (a `globalThis.__nub*` sentinel is the same brand leak as a NUB_*
// env var — enumerable in user code AND worker realms — so it is forbidden). The
// floor's `node:module` import lives only in floor-builtin.mjs, which the fast tier
// never loads, so it never enters the fast-tier `require(esm)` graph.
//
// On the floor the threaded value isn't available at this module's top-level eval:
// the compat entry imports floor-builtin AHEAD of transform-core, but ES modules
// evaluate the importEE before the importer's body, so floor-builtin's setter call
// (made during ITS evaluation) lands AFTER transform-core's body has run. So on the
// floor every builtin is acquired LAZILY, on first hook use — by which point the
// setter has run. On the fast tier getBuiltinModule is present, so the builtins are
// acquired eagerly here at module eval (no setter, no floor path involved).
let _bootstrapCreateRequire = null;
// Called by floor-builtin.mjs (imported first by the compat entries) to hand in the
// floor's `createRequire` without any globalThis surface. NEVER called on the fast
// tier (getBuiltinModule covers it). The compat entries import floor-builtin ahead of
// transform-core, so by the time this fires transform-core's body has already
// evaluated (importEE before importer) — which is exactly why this setter also runs
// __ensureBuiltins() right now: it lands DURING floor-builtin's evaluation, before the
// entry's body and long before any hook fires, so every builtin binding is ready
// without ever consulting globalThis.
export function setBootstrapCreateRequire(fn) {
  _bootstrapCreateRequire = fn;
  __ensureBuiltins();
}

// node: builtins (lazy on the floor, eager on the fast tier — see above). On the
// floor `_bootstrapCreateRequire` is read at FETCH time (inside the thunk), never at
// definition time, so floor-builtin's setter has run before the first fetch fires.
function __getBuiltin(id) {
  if (typeof process.getBuiltinModule === "function") return process.getBuiltinModule(id);
  return _bootstrapCreateRequire(import.meta.url)(id);
}

// Builtin bindings + the native addon, populated by __ensureBuiltins(). They stay
// `let` (not `const`) because on the floor they are filled on first hook use rather
// than at module eval — see the lazy-vs-eager note above.
let createRequire, __require, module, readFileSync, writeFileSync, mkdirSync, statSync;
let fileURLToPath, pathToFileURL, join, dirname;
// Nub's N-API addon — the in-process TS/JSX transpiler (`transform`,
// `transformCached`, `detectModuleInfo`), the tsconfig reader + additive
// TS-resolver (`loadTsconfig`, `resolveTs`), AND the data-format parsers
// (`parseYaml`/`parseToml`/`parseJson5`/`parseJsonc`), all native. Loaded once
// per module instance (= once per thread: the main thread and the loader worker
// each import this module separately). It is a `.node` binary resolved by absolute
// path off this file's dir, so it never touches the ESM loader chain — the
// historical require(esm)-of-an-ESM-npm-package leak (oxc-transform, and before
// this migration get-tsconfig) is gone: transpilation, tsconfig discovery, the
// additive resolution, and the transpile cache are synchronous native calls, no JS
// package, no static-import graph to route. nub now loads ZERO npm packages
// internally, so the user ESM loader chain can never observe a nub dependency.
let nubNative = null;

// Idempotent. Acquires the node: builtins + the native addon. Runs eagerly at module
// eval on the fast tier (getBuiltinModule present); on the floor it is invoked at the
// top of every exported entry point, where the threaded createRequire is ready.
let __builtinsReady = false;
function __ensureBuiltins() {
  if (__builtinsReady) return;
  __builtinsReady = true;
  ({ createRequire } = __getBuiltin("node:module"));
  __require = createRequire(import.meta.url);
  module = __getBuiltin("node:module");
  ({ readFileSync, writeFileSync, mkdirSync, statSync } = __getBuiltin("node:fs"));
  ({ fileURLToPath, pathToFileURL } = __getBuiltin("node:url"));
  ({ join, dirname } = __getBuiltin("node:path"));
  for (const rel of ["./addons/nub-native.node", "../runtime/addons/nub-native.node"]) {
    try { nubNative = __require(fileURLToPath(new URL(rel, import.meta.url))); break; } catch {}
  }
}
// Fast tier: getBuiltinModule is present, so acquire everything now (preserves the
// original eager-at-eval behavior). The floor defers to first-use — see above.
if (typeof process.getBuiltinModule === "function") __ensureBuiltins();

// NOTE: the transpile-cache version component is no longer read here. nub's
// version is baked into the native addon at compile time (`env!("CARGO_PKG_VERSION")`
// in nub-native's cache.rs), which `make version` keeps in lockstep with
// runtime/version.mjs and Cargo.toml — so the cache key's version component lives
// natively now, and this file no longer needs to read version.mjs.

// ── Constants ───────────────────────────────────────────────────────
// TS/JSX exts ALWAYS transform (type-stripping is required), so they live in
// TRANSPILE_EXTS — the set every dispatch site checks to route a file to
// loadTranspile. Plain JS (.js/.mjs/.cjs) is DELIBERATELY NOT here: a plain-JS file
// is transpiled ONLY when it carries transformable syntax (`using`/`await using`,
// `v`-flag RegExp, decorators), and a no-op plain-JS file must take Node's OWN load
// path BYTE-FOR-BYTE — putting it in TRANSPILE_EXTS would route every `.js`/`.cjs`
// through nub's hook and change native CJS/ESM behavior (the `commonjs-sync` relabel,
// require.cache, the require-of-ESM-syntax-.cjs error). So plain JS is handled by a
// SEPARATE narrow path (`maybeTranspilePlainJs`) that fires only for transformable
// files and is a no-op (returns null) otherwise — see PLAIN_JS_EXTS below.
export const TRANSPILE_EXTS = new Set([".ts", ".tsx", ".mts", ".cts", ".jsx"]);
// Project-source plain JS. Routed to the transpiler ONLY when transformable (the
// `maybeTranspilePlainJs` gate); a no-op plain-JS file falls through to Node's
// native loader untouched, byte-identical. node_modules is excluded at the gate.
export const PLAIN_JS_EXTS = new Set([".js", ".mjs", ".cjs"]);
export const DATA_EXTS = { ".jsonc": "jsonc", ".json5": "json5", ".toml": "toml", ".yaml": "yaml", ".yml": "yaml", ".txt": "txt" };
export const TS_PARENT_EXTS = new Set([".ts", ".tsx", ".mts", ".cts"]);

// Packages resolved from Nub's distribution, not the user's.
export const VENDORED_PACKAGES = new Set(["@oxc-project/runtime"]);

// Built-in modules provided by Nub (resolved to files in this distribution).
// connect() sockets deferred per design decision — "sockets" specifier not clobbered.
export const BUILTIN_MODULES = new Map();

// Package clobbering: specifiers that resolve to a synthetic module re-exporting
// the native global instead of the userland package.
export const CLOBBER_MAP = new Map([
  // Reading globalThis.Temporal triggers the lazy getter the tier file installs,
  // which loads the polyfill by resolved path — that load is what installs
  // Date.prototype.toTemporalInstant, so Temporal MUST be read first.
  // @js-temporal/polyfill exports { Temporal, Intl, toTemporalInstant }; mirror
  // all three so `import { Temporal, Intl, toTemporalInstant } from ...` binds.
  ["@js-temporal/polyfill", () => `const T = globalThis.Temporal; export default T; export const Temporal = T; export const Intl = globalThis.Intl; export const toTemporalInstant = Date.prototype.toTemporalInstant;`],
  ["urlpattern-polyfill", () => `export const URLPattern = globalThis.URLPattern;`],
  ["abort-controller", () => `export const AbortController = globalThis.AbortController; export const AbortSignal = globalThis.AbortSignal; export default globalThis.AbortController;`],
]);

// ── Watch-mode hooks (injected by the main-thread tier) ─────────────
// `nub watch` needs config files (tsconfig.json, package.json) and `.env*` —
// which are not in any import graph — surfaced to Node's FilesWatcher. The main
// thread (preload.mjs) injects reporters; the loader worker injects nothing
// (watch IPC is main-thread only), so these default to no-ops.
let _reportDep = null;
let _reportEnvDir = null;
export function setWatchHooks({ reportDep, reportEnvDir } = {}) {
  if (reportDep) _reportDep = reportDep;
  if (reportEnvDir) _reportEnvDir = reportEnvDir;
}

// ── tsconfig + package-type caches ──────────────────────────────────
// tsconfig discovery / parse / `extends` resolution + the `paths` matcher all
// happen natively (nub-native `loadTsconfig`, the get-tsconfig@4.14.0 port). This
// JS wrapper exists only to (a) memoize per importer-dir — native ALSO memoizes,
// but a JS-side Map skips the napi boundary on a hit and lets watch-mode report
// the dep exactly once per dir — and (b) surface the resolved tsconfig path to the
// watch FilesWatcher. The returned shape exposes the transform-relevant
// `compilerOptions` slice and the `tsconfigHash` cache-key component; the `paths`
// matcher lives entirely in native (`resolveTs` runs it), so there is no JS matcher.
const tsconfigCache = new Map();
export function getTsconfigForDir(dir) {
  if (tsconfigCache.has(dir)) return tsconfigCache.get(dir);
  // { path: string|null, compilerOptions: object|null, tsconfigHash: string }
  const result = nubNative
    ? nubNative.loadTsconfig(dir)
    : { path: null, compilerOptions: null, tsconfigHash: "" };
  tsconfigCache.set(dir, result);
  if (result.path) _reportDep?.(result.path);
  return result;
}

// The NEAREST package.json's `type` decides the format of ambiguous extensions
// (.ts/.tsx/.jsx, like Node's .js). The nearest one wins even when its `type`
// is absent — Node does not skip a typeless package.json to find a typed
// ancestor — so we stop at the first package.json found. Returns "module",
// "commonjs", or undefined.
const packageTypeCache = new Map();
export function getPackageType(dir) {
  if (packageTypeCache.has(dir)) return packageTypeCache.get(dir);
  let type;
  let current = dir;
  for (;;) {
    const pkgPath = join(current, "package.json");
    if (fileExists(pkgPath)) {
      try { type = JSON.parse(readFileSync(pkgPath, "utf8")).type; } catch {}
      // Watch this package.json (a `type`/script edit should restart) and the
      // `.env*` files alongside it (the package root is where they live).
      _reportDep?.(pkgPath);
      _reportEnvDir?.(current);
      break;
    }
    const parent = dirname(current);
    if (parent === current) break;
    current = parent;
  }
  packageTypeCache.set(dir, type);
  return type;
}

// ── Filesystem helpers ──────────────────────────────────────────────
export function extname(url) {
  const path = url.includes("?") ? url.slice(0, url.indexOf("?")) : url;
  const dot = path.lastIndexOf(".");
  return dot === -1 ? "" : path.slice(dot);
}

export function isNodeModules(url) {
  return url.includes("/node_modules/") || url.includes("\\node_modules\\");
}

export function fileExists(filePath) {
  const s = statSync(filePath, { throwIfNoEntry: false });
  return s !== undefined && s.isFile();
}

function safeRequireResolve(specifier) {
  try { return __require.resolve(specifier); } catch { return null; }
}

export function barePkg(specifier) {
  return specifier.startsWith("@")
    ? specifier.split("/").slice(0, 2).join("/")
    : specifier.split("/")[0];
}

// ── Resolution ──────────────────────────────────────────────────────
// The ADDITIVE TS resolution — tsconfig `paths` aliases, `.ts/.tsx/.mts/.cts/.jsx`
// extension probing, the `.js`→`.ts` (and `.jsx→.tsx`, `.mjs→.mts`, `.cjs→.cts`)
// emit-convention swap, directory-index probing, and reading a directory's
// `package.json#main` — all happens natively now (nub-native `resolveTs`). It
// returns an absolute path for the additive cases nub owns, or `null` for
// EVERYTHING Node owns (node_modules, `exports`/`imports`, conditions, scoped/bare
// specifiers), which the resolve hooks below turn into a fall-through to Node. That
// `null` is the byte-for-byte compat boundary; reimplementing Node's resolution in
// nub is forbidden. The `node:`/`data:`/builtin guards, the nub-internal-graph
// bypass, vendored packages, and the clobber map all stay in JS and run BEFORE the
// native resolver (see resolveSpec / resolveCjsPath).
function resolveTs(specifier, parentPath) {
  if (!nubNative) return null;
  try {
    return nubNative.resolveTs(specifier, parentPath || "");
  } catch {
    return null;
  }
}

// nub's own runtime directory (this file's dir, as a file: URL prefix). Any
// resolution whose IMPORTER lives here is one of nub's internal requires — the
// preload loading transform-core, the Temporal lazy getter resolving
// @js-temporal/polyfill — and must NEVER be routed through nub's own
// clobber/vendored/tsconfig logic: those are user-code conveniences, and applying
// them to nub's internals both breaks them (e.g. the Temporal clobber re-exports
// globalThis.Temporal, which IS the getter → a require of the polyfill from the
// getter would recurse into the clobber) and amplifies the user loader chain by
// re-walking nub's internal graph through user hooks (R11). Short-circuit to native
// resolution for these.
const RUNTIME_DIR_URL = new URL(".", import.meta.url).href;

// Is this importer part of nub's own internal module graph? Such imports must
// bypass the user ESM loader chain entirely (R11). nub now loads ZERO npm packages
// internally — tsconfig, the additive resolver, the transpile cache, the
// transpiler, and module detection are ALL native nub-native calls, and the only
// remaining JS deps (@oxc-project/runtime helpers, the polyfills) are CommonJS,
// whose `require()` graph already bypasses the ESM loader chain by construction. So
// the only nub-internal ESM importer left is nub's own runtime directory (this
// file, the preload tiers, the Temporal lazy getter resolving @js-temporal/
// polyfill). The historical "nub-dependency package roots" walk — which existed
// solely to catch an ESM hop into get-tsconfig (and before that oxc-transform) — is
// gone with those packages.
function isNubInternalParent(parentURL) {
  if (!parentURL) return false;
  return String(parentURL).startsWith(RUNTIME_DIR_URL);
}

// Resolve a specifier the way both hook tiers do. Returns `{ url, shortCircuit }`
// to short-circuit Node's resolver, or `null` to fall through to `nextResolve`.
// `parentURL` is the importer (a file: URL string), or "" for the entry.
export function resolveSpec(specifier, parentURL) {
  // nub's own internal graph (importer inside nub's runtime dir OR a nub
  // dependency package): resolve natively and SHORT-CIRCUIT so nextResolve (the
  // user's loader chain) never observes nub's internals. This MUST run before the
  // node:/data:/builtin early-returns below, because those `return null` =
  // DELEGATE to the user loader — and a nub-internal `import "node:module"` (e.g.
  // from a nub-dependency ESM entry) delegated to a strict user loader is exactly
  // the R11 leak. See isNubInternalParent.
  if (isNubInternalParent(parentURL)) {
    if (specifier.startsWith("node:") || module.builtinModules.includes(specifier)) {
      const url = specifier.startsWith("node:") ? specifier : `node:${specifier}`;
      return { url, shortCircuit: true };
    }
    if (specifier.startsWith("data:")) return { url: specifier, shortCircuit: true };
    // A relative/bare import from inside nub's graph: resolve it natively from the
    // parent's own require() resolver (NOT nub's tsconfig/clobber/probe logic) and
    // short-circuit. Bare specifiers resolve from the parent package's location.
    try {
      const parentReq = createRequire(parentURL);
      const resolved = parentReq.resolve(specifier);
      return { url: pathToFileURL(resolved).href, shortCircuit: true };
    } catch {
      // Couldn't resolve from the parent (e.g. a non-file: parent): still short-
      // circuit by handing the specifier back as-is, so the user chain is bypassed.
      return null;
    }
  }

  // node: and data: protocols, and bare Node built-ins, are never ours.
  if (specifier.startsWith("node:") || specifier.startsWith("data:")) return null;
  if (module.builtinModules.includes(specifier)) return null;

  // 1. Built-in modules provided by Nub.
  if (BUILTIN_MODULES.has(specifier)) {
    return { url: BUILTIN_MODULES.get(specifier), shortCircuit: true };
  }

  // 2. Vendored packages (e.g. @oxc-project/runtime).
  const bare = barePkg(specifier);
  if (VENDORED_PACKAGES.has(bare)) {
    const resolved = safeRequireResolve(specifier);
    if (resolved) return { url: pathToFileURL(resolved).href, shortCircuit: true };
  }

  // 3. Package clobbering.
  if (CLOBBER_MAP.has(bare) && !isNodeModules(parentURL || "")) {
    return { url: `data:text/javascript,${encodeURIComponent(CLOBBER_MAP.get(bare)())}`, shortCircuit: true };
  }

  const parent = String(parentURL || "");

  // 4. The ADDITIVE TS resolution (tsconfig `paths`, extension probing, `.js`→`.ts`
  // swap, directory index/`main`) — native. `resolveTs` is handed the parent's
  // absolute FS path (or "" for a non-file: parent / the entry, where it falls back
  // to cwd, matching the old `process.cwd()` parentDir). A non-null result is an
  // additive hit nub owns; null falls through to Node's resolver (the compat
  // boundary — node_modules, `exports`, bare/scoped specifiers stay Node's).
  const parentPath = parent.startsWith("file:") ? fileURLToPath(parent) : "";
  const resolved = resolveTs(specifier, parentPath);
  if (resolved) return { url: pathToFileURL(resolved).href, shortCircuit: true };

  return null;
}

// CommonJS `require()` resolution for the compat-tier Module._resolveFilename
// patch. Returns an absolute file path for a require specifier nub should
// redirect (tsconfig `paths`, extensionless `.ts`, `.js`→`.ts` swap), or null to
// defer to Node's resolver. Mirrors resolveSpec steps 4–5 but returns a path (not
// a URL) and never handles clobber/vendored/builtin — those are import-only, and
// a clobber's data: URL can't be a require target. `parentPath` is the requiring
// file's absolute path (from the CJS parent Module), or null for the entry.
export function resolveCjsPath(request, parentPath) {
  if (request.startsWith("node:") || request.startsWith("data:") ||
      module.builtinModules.includes(request)) {
    return null;
  }
  // The SAME native additive resolver as resolveSpec, returning an absolute path
  // (not a URL). Vendored/clobber/builtin are import-only and never reach here. A
  // null result (node_modules / `exports` / a plain bare package) falls through to
  // Node's CJS resolver — the compat boundary.
  return resolveTs(request, parentPath || "");
}

// Would `require()`-ing this resolved TS file need Node's require(esm)? An
// ESM-syntax `.ts`/`.mts` (or a `.ts` in a `type: module` package) transpiles to
// ESM, which `require()` can only load via require(esm). On the compat tier that
// path is the loader-worker's CJS translator, which on Node below the #60380 fix
// crashes cryptically (`cjsCache.get(job.url)` is undefined) instead of erroring.
// The compat CJS shim calls this so it can surface a clean ERR_REQUIRE_ESM
// instead. (`.cts` is always CommonJS → false; non-transpiled extensions → false.)
export function requireTargetIsEsm(filePath, ext) {
  if (ext === ".cts") return false;
  if (ext === ".mts") return true;
  if (!TRANSPILE_EXTS.has(ext)) return false;
  let source;
  try { source = readFileSync(filePath, "utf8"); } catch { return false; }
  const pkgType = getPackageType(dirname(filePath));
  return moduleFormatFor(ext, pkgType, filePath, source) === "module";
}

// ── Module-format detection ─────────────────────────────────────────
// Both signals nub needs to read off a file's syntax — the absent-`type` module
// format and the Stage-3-decorator guard — come from ONE native call into nub's
// N-API addon (`detectModuleInfo`, the oxc parser compiled in-process). There is
// no JS parser package anymore: `oxc-parser` (ESM-only, which used to need
// `require(esm)` on the fast tier and a dynamic-`import()` `ensureParser()` dance
// on the 18.19 compat tier) is gone, and with it the whole "is require(esm)
// available here?" fork. The native call is synchronous and works identically on
// every supported Node, so there is nothing to preload and no async warm-up — the
// former `ensureParser()` export is removed (its compat-tier callers just stop
// calling it). Used only for ambiguous extensions / the decorator guard; explicit
// `type` and `.mts`/`.cts` short-circuit before the parser runs.
function detectModuleInfo(filePath, source, lang) {
  // Addon missing (should never happen in a real install): default to ESM for
  // format (the common case) and "no decorators" for the guard — the same fallback
  // the old oxc-parser-unavailable branches used.
  if (!nubNative) return { hasValueEsmSyntax: true, hasDecorators: false, transformableSyntax: false };
  try {
    return nubNative.detectModuleInfo(filePath, source, lang);
  } catch {
    // Unparseable → CJS for format + no decorators (the transpile/V8 surfaces the
    // real error), matching the old per-call catch blocks. `transformableSyntax:
    // false` is the SAFE plain-JS default — the verbatim path hands the raw bytes
    // back, so V8 surfaces the real syntax error exactly where Node would.
    return { hasValueEsmSyntax: false, hasDecorators: false, transformableSyntax: false };
  }
}

// Map a transpiled file's extension + nearest package.json "type" to the module
// format Node's loader should use. `.mts`/`.cts` are explicit; an explicit
// `type` is authoritative; otherwise (ambiguous) we detect from source syntax —
// full Node parity (`--experimental-detect-module`), so a CJS-syntax `.ts` with
// no `type` runs as CJS on nub exactly as on Node. See wiki/runtime/module-format.md.
// `.mjs`→module / `.cjs`→commonjs are explicit (mirroring `.mts`/`.cts`), so the
// plain-JS gate gets the right format without a needless detect.
export function moduleFormatFor(ext, pkgType, filePath, source) {
  if (ext === ".mts" || ext === ".mjs") return "module";
  if (ext === ".cts" || ext === ".cjs") return "commonjs";
  if (pkgType === "module") return "module";
  if (pkgType === "commonjs") return "commonjs";
  const lang = ext === ".tsx" ? "tsx" : ext === ".jsx" ? "jsx" : "ts";
  return detectModuleInfo(filePath, source, lang).hasValueEsmSyntax ? "module" : "commonjs";
}

// The Stage-3-decorator rejection diagnostic. oxc does not lower TC39 Stage 3
// decorators yet (oxc-project/oxc#9170) — it passes the `@decorator` syntax
// through verbatim with errors:[], so without this check V8 throws a bare
// `SyntaxError: Invalid or unexpected token`. See wiki/runtime/stage3-decorators.md.
function stage3DecoratorError(filePath) {
  return new Error(
    `Nub: Stage 3 decorators are not supported by the transpiler yet.\n` +
    `This is an upstream limitation in oxc (oxc-project/oxc#9170).\n` +
    `  in ${filePath}\n\n` +
    `Workarounds:\n` +
    `  1. Set "experimentalDecorators": true in tsconfig.json to use legacy decorators\n` +
    `     (the shape NestJS / TypeORM / class-validator are written against).\n` +
    `  2. Wait for Stage 3 decorator support in oxc; tracked upstream at\n` +
    `     https://github.com/oxc-project/oxc/issues/9170.\n\n` +
    `See: https://www.typescriptlang.org/tsconfig/#experimentalDecorators`,
  );
}

// Does the source contain TC39 decorator syntax (`@expr` on a class or class
// member)? Used ONLY when legacy decorators are off, to surface a clear
// diagnostic instead of oxc's verbatim passthrough → V8 SyntaxError. The cheap
// `source.includes("@")` pre-filter in the caller keeps decorator-free files off
// the native parser. The walk now happens in Rust (detectModuleInfo's AST visit).
function hasDecoratorSyntax(filePath, source, lang) {
  return detectModuleInfo(filePath, source, lang).hasDecorators;
}

// ── Transpile cache ─────────────────────────────────────────────────
// The transpile cache — `cacheGet` + transform-on-miss + post-processing
// (CJS empty-export strip, inline sourceMap, `//# sourceURL=`) + `cacheSet` — is
// ONE native call now (nub-native `transformCached`): the cache key (NUB_VERSION
// is the sole version component — a new release ships any emit change + a rebuilt
// addon), the 16-hex integrity prefix, the `c`/`m` format byte, and the atomic
// `*.tmp`-then-rename write all live in Rust, byte-identical to the old JS cache so
// warm caches survive. This JS file keeps only (a) the cache enable/disable signal
// and (b) the cache directory it passes IN, so the policy stays in JS and native
// just does the I/O against the dir nub hands it.
//
// Disable the transpile cache when (a) the permission model is active (writing a
// cache file may not be granted), or (b) the user set `NODE_COMPILE_CACHE=0` —
// Node's compile-cache disable signal, which nub honors as "no caching in this
// pipeline" (one knob for both V8's compile cache and nub's transpile cache; no
// nub-specific env var). Per wiki/runtime/transpile-cache.md (the maintainer 2026-05-18).
const CACHE_DISABLED =
  process.permission?.has !== undefined || process.env.NODE_COMPILE_CACHE === "0";
// Resolved lazily (memoized) rather than at module eval, because on the floor the
// node:path builtins it needs aren't bound until __ensureBuiltins() runs on first
// hook use. `null` = disabled / no writable dir; `undefined` cacheDirResolved means
// "not yet computed".
let cacheDir = null;
let cacheDirResolved = false;
function getCacheDir() {
  if (cacheDirResolved) return cacheDir;
  cacheDirResolved = true;
  if (CACHE_DISABLED) return cacheDir;
  __ensureBuiltins();
  const base = process.env.XDG_CACHE_HOME || (process.env.HOME ? join(process.env.HOME, ".cache") : null);
  if (base) {
    cacheDir = join(base, "nub", "transpile");
    try { mkdirSync(cacheDir, { recursive: true }); } catch { cacheDir = null; }
  }
  return cacheDir;
}

// ── Bounded-cache maintenance ───────────────────────────────────────
const CACHE_MAX_BYTES = 512 * 1024 * 1024; // 512 MiB — bounds runaway growth, not normal use
const SWEEP_INTERVAL_MS = 24 * 60 * 60 * 1000; // ≤ one sweep per day
export function maybeSweepCache() {
  __ensureBuiltins();
  const dir = getCacheDir();
  if (!dir) return;
  // Workers inherit this preload (via execArgv); only the main thread sweeps.
  try {
    if (!__require("node:worker_threads").isMainThread) return;
  } catch {
    return;
  }
  const sentinel = join(dir, ".sweep");
  const s = statSync(sentinel, { throwIfNoEntry: false });
  if (s && Date.now() - s.mtimeMs < SWEEP_INTERVAL_MS) return;
  try {
    writeFileSync(sentinel, "");
  } catch {
    return;
  }
  import("./cache-evict.mjs")
    .then((m) => m.sweepCache(dir, CACHE_MAX_BYTES))
    .catch(() => {});
}

// ── Transpile ───────────────────────────────────────────────────────
// Transpile a TS/JSX file to JS, returning `{ format, source, shortCircuit }` in
// the shape both hook tiers hand back to Node. Format is detected (not derived
// from extension alone), so a CommonJS-syntax `.ts` is reported `commonjs` — the
// fix that makes `require()` of a TS file work on the compat tier, where Node's
// CJS translator loads it via this hook and keys on the returned format.
export function loadTranspile(url, ext) {
  __ensureBuiltins();
  const filePath = fileURLToPath(url);
  const source = readFileSync(filePath, "utf8");
  const dir = dirname(filePath);
  // The transform-relevant compilerOptions slice + the byte-for-byte cache-key
  // component (`tsconfigHash`) both come from the native tsconfig reader.
  const { compilerOptions: co, tsconfigHash } = getTsconfigForDir(dir);

  // The nearest package.json `type` decides the format of an ambiguous extension
  // (.ts/.tsx/.jsx); .mts/.cts are explicit so its lookup is skipped. The chosen
  // format is folded into the cache key (and the entry's leading byte) by native.
  const pkgType = ext === ".mts" || ext === ".cts" ? undefined : getPackageType(dir);
  const format = moduleFormatFor(ext, pkgType, filePath, source);

  const lang = ext === ".tsx" ? "tsx" : ext === ".jsx" ? "jsx" : "ts";

  const opts = {
    lang,
    sourceType: format === "commonjs" ? "commonjs" : "module",
    sourcemap: true,
    // Lower syntax newer than the 22.15 floor. Critically this downlevels
    // `using`/`await using` (Explicit Resource Management) — unparseable on Node
    // 22's V8 — into the vendored `@oxc-project/runtime/helpers/usingCtx` shape,
    // which resolves via VENDORED_PACKAGES. Without a target, oxc leaves `using`
    // verbatim and Node 22 throws a SyntaxError. es2022 is the highest target
    // that still lowers `using` while leaving everything Node 22 already supports
    // (top-level await, class fields, private methods) untouched.
    target: "es2022",
    typescript: {},
    // Decorators default to OFF (Stage-3 mode), matching tsc: legacy semantics
    // and metadata are opt-in via tsconfig. See wiki/runtime/non-erasable-syntax.md.
    decorator: co?.experimentalDecorators === true
      ? { legacy: true, emitDecoratorMetadata: co?.emitDecoratorMetadata === true }
      : undefined,
  };
  if (ext === ".tsx" || ext === ".jsx") {
    opts.jsx = {
      runtime: co?.jsx === "react" ? "classic" : "automatic",
      importSource: co?.jsxImportSource || "react",
    };
    if (co?.jsxFactory) opts.jsx.pragma = co.jsxFactory;
    if (co?.jsxFragmentFactory) opts.jsx.pragmaFrag = co.jsxFragmentFactory;
  }

  // Stage-3 decorators: oxc returns errors:[] and emits the `@decorator` syntax
  // verbatim, so the result-error check below never fires and V8 throws a bare
  // SyntaxError. When legacy mode is off and decorator syntax is present, reject
  // with the documented Option-A diagnostic instead. (Cheap `source.includes("@")`
  // pre-filter keeps decorator-free files off the native parser; runs BEFORE the
  // cache so the diagnostic surfaces even on what would be a warm hit.)
  if (co?.experimentalDecorators !== true && source.includes("@") &&
      hasDecoratorSyntax(filePath, source, lang)) {
    throw stage3DecoratorError(filePath);
  }

  // cacheGet + transform-on-miss + post-process (CJS empty-export strip, inline
  // sourceMap, sourceURL append) + cacheSet — ALL native, byte-identical on-disk.
  // The cache key folds in ext + tsconfigHash + pkgType (same source, different
  // type → different format → distinct entry). `cacheDir: null/undefined` is the
  // JS enable/disable signal: native then skips all cache I/O and just transforms.
  const formatByte = format === "commonjs" ? "c" : "m";
  const result = nubNative.transformCached(
    filePath, source, opts, ext, tsconfigHash || "", pkgType || "", formatByte, getCacheDir() ?? undefined,
  );
  if (result.errors.length > 0) {
    const details = result.errors.map((e) => e.codeframe || e.message).join("\n\n");
    throw new Error(`Transpile error in ${filePath}:\n${details}`);
  }
  return { format: result.format, source: result.code, shortCircuit: true };
}

// Project-source plain JS (`.js`/`.mjs`/`.cjs`) gate. Returns a transpiled load
// result ONLY when the file carries syntax oxc lowers at nub's es2022 target
// (`using`/`await using`, a `v`-flag RegExp, or decorators); otherwise returns
// `null`, meaning "this file needs no transform — handle it with Node's OWN loader,
// exactly as a non-listed extension." This is why `.js`/`.mjs`/`.cjs` are NOT in
// TRANSPILE_EXTS: a no-op plain-JS file must take Node's native load path
// byte-for-byte (preserving the `commonjs-sync` relabel, require.cache, the
// require-of-ESM-syntax-`.cjs` error — all of which intercepting the file would
// break), and oxc would reformat it (quotes/semicolons/whitespace + a sourcemap
// footer) if we ran it through anyway. The verdict rides ONE parse (the same one
// `detectModuleInfo` does for format detection). node_modules is gated at the call
// sites (the byte-parity boundary). JSX-in-`.js` is out of scope (lang is "ts",
// which does not parse JSX); use `.jsx`.
export function maybeTranspilePlainJs(url, ext) {
  __ensureBuiltins();
  const filePath = fileURLToPath(url);
  let source;
  try {
    source = readFileSync(filePath, "utf8");
  } catch {
    // Unreadable here → let Node's loader surface its own error.
    return null;
  }
  // lang "ts" parses all JS (a TS superset) but NOT JSX — JSX-in-.js is out of scope.
  const info = detectModuleInfo(filePath, source, "ts");
  if (!info.transformableSyntax && !info.hasDecorators) {
    return null; // no-op: Node's native loader handles it, byte-identical.
  }
  // Transformable: run the SAME pipeline as TS/JSX (target es2022 lowering, tsconfig,
  // source maps, the Stage-3 decorator guard, format detection, cache). loadTranspile
  // re-reads + re-parses, but only for the rare file that actually needs lowering.
  return loadTranspile(url, ext);
}

// ── Data-format imports ─────────────────────────────────────────────
function lazyRequire(pkg) {
  try { return __require(pkg); } catch {
    throw new Error(`Nub: importing this file requires the "${pkg}" package.\nInstall it: npm install ${pkg}`);
  }
}

function stripJsonComments(text) {
  let result = "", i = 0, inString = false, escape = false;
  while (i < text.length) {
    const ch = text[i];
    if (escape) { result += ch; escape = false; i++; continue; }
    if (inString) { if (ch === "\\") escape = true; if (ch === '"') inString = false; result += ch; i++; continue; }
    if (ch === '"') { inString = true; result += ch; i++; continue; }
    if (ch === "/" && text[i + 1] === "/") { while (i < text.length && text[i] !== "\n") i++; continue; }
    if (ch === "/" && text[i + 1] === "*") { i += 2; while (i < text.length && !(text[i] === "*" && text[i + 1] === "/")) i++; i += 2; continue; }
    result += ch; i++;
  }
  return result;
}

export function loadData(url, ext) {
  const filePath = fileURLToPath(url);
  const raw = readFileSync(filePath, "utf8");
  const kind = DATA_EXTS[ext];

  if (kind === "txt") {
    return { format: "module", source: `export default ${JSON.stringify(raw)};\n`, shortCircuit: true };
  }

  let parsed;
  if (nubNative) {
    if (kind === "yaml") parsed = nubNative.parseYaml(raw);
    else if (kind === "toml") parsed = nubNative.parseToml(raw);
    else if (kind === "json5") parsed = nubNative.parseJson5(raw);
    else if (kind === "jsonc") parsed = nubNative.parseJsonc(raw);
  } else {
    if (kind === "yaml") parsed = lazyRequire("yaml").parse(raw);
    else if (kind === "toml") parsed = lazyRequire("@iarna/toml").parse(raw);
    else if (kind === "json5") parsed = lazyRequire("json5").parse(raw);
    else if (kind === "jsonc") parsed = JSON.parse(stripJsonComments(raw));
  }

  if (parsed == null) {
    return { format: "module", source: "export default undefined;\n", shortCircuit: true };
  }

  // Default export only. Data modules deliberately do NOT emit per-key named
  // exports: named imports of data are categorically un-typeable in TypeScript
  // (a `declare module "*.yaml"` wildcard has no per-key export index signature),
  // and default-only matches Node's own JSON modules. Consumers destructure the
  // default — `import cfg from "./c.yaml"; const { host } = cfg;` — which the
  // `@nubjs/types` `Record<string, unknown>` default type makes sound.
  const code = `export default ${JSON.stringify(parsed)};\n`;
  return { format: "module", source: code, shortCircuit: true };
}

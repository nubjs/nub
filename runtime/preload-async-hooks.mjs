// Nub compat-tier hooks module — Node 18.19 through 22.14.
//
// On Node 22.15+, runtime/preload.mjs registers its hooks synchronously via
// `module.registerHooks({ resolve, load })`. That API didn't exist before 22.15,
// so on 18.19..22.14 the main-thread bootstrap calls
// `module.register('./preload-async-hooks.mjs', parentURL)` instead, which loads
// THIS file into a dedicated loader worker thread and uses its async
// `resolve` / `load` exports. (CommonJS `require()` is augmented separately, on
// the main thread, by preload.mjs's installCjsRequireHooks — `module.register`
// hooks the ESM loader only.)
//
// There is NO logic of its own here: resolution + transpilation come verbatim
// from runtime/transform-core.mjs, the single source of truth shared with the
// fast path. The two tiers can no longer drift — the only difference is the
// async function signatures Node's loader-worker protocol requires (it awaits
// the returned values, so returning the core's synchronous results is fine). The
// worker injects no watch hooks (watch IPC is main-thread only), so the core's
// dependency reporters stay no-ops here, exactly as before the extraction.

// Floor bootstrap (Node < 22.3/20.16/18.20.4): threads node:module's createRequire
// into transform-core via a MODULE-SCOPE SETTER — never globalThis (brand boundary) —
// because transform-core has no process.getBuiltinModule on the floor. floor-builtin
// calls transform-core's setter during its own evaluation, so importing it FIRST
// (ESM evaluates imports in source order) means the value is threaded before any hook
// in this loader worker fires. No-op where getBuiltinModule exists. This worker runs
// OFF any user loader chain, so floor-builtin's static node:module import never leaks
// — see floor-builtin.mjs. (worker-polyfill is NOT loaded here: this is the dedicated
// loader worker, not a user realm, so it installs no browser globals.)
import "./floor-builtin.mjs";
import {
  TRANSPILE_EXTS, PLAIN_JS_EXTS, DATA_EXTS,
  extname, resolveSpec, loadTranspile, maybeTranspilePlainJs, loadData, isNodeModules,
} from "./transform-core.mjs";
import { createRequire, isBuiltin } from "node:module";
import { existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { pnpResolveEsm } from "./pnp-util.cjs";

// Yarn PnP handle for this loader worker. The worker runs in its own thread where
// `.pnp.cjs` was never --require'd, so neither the `pnpapi` builtin nor
// `module.findPnpApi` is installed here (the main-thread preload uses findPnpApi; it
// can't reach across to this realm). So bootstrap PnP for this thread directly: walk
// up from cwd to the `.pnp.cjs` Rust located and require it by absolute path — that
// returns the pnpapi object. nub then resolves PnP specifiers via
// `pnpapi.resolveRequest` (its public, conditions-free resolver), mirroring the main
// thread, so there is no need to register Yarn's `.pnp.loader.mjs` (which deadlocks
// against the fast tier's `module.registerHooks`). `null` when not a PnP run.
const __pnp = (() => {
  if (!process.versions.pnp) return null;
  const req = createRequire(import.meta.url);
  try {
    let dir = process.cwd();
    for (;;) {
      const candidate = join(dir, ".pnp.cjs");
      if (existsSync(candidate)) return req(candidate);
      const parent = dirname(dir);
      if (parent === dir) return null;
      dir = parent;
    }
  } catch { return null; }
})();

// Node calls this once per worker when the main thread invokes
// `module.register(url, parentURL, { data })`. We accept and ignore the payload
// so future main-thread → worker plumbing is non-breaking. Returning a Promise
// lets the main thread `await register(...)`.
export async function initialize(_data) {}

// ── Resolve hook ────────────────────────────────────────────────────
export async function resolve(specifier, context, nextResolve) {
  const r = resolveSpec(specifier, context.parentURL);
  if (r) return r;
  // Yarn PnP: resolve deps through PnP's own resolver — identical to the fast tier,
  // via the shared helper (resolveRequest with the import conditions + format
  // detection), so dual packages resolve to their `import` build.
  if (__pnp && !isBuiltin(specifier) && !specifier.startsWith("node:")) {
    try {
      const res = pnpResolveEsm(__pnp, specifier, context);
      if (res) return res;
    } catch { /* fall through to Node's resolver */ }
  }
  return nextResolve(specifier, context);
}

// ── Load hook ───────────────────────────────────────────────────────
export async function load(url, context, nextLoad) {
  const ext = extname(url);
  // node_modules deps are NEVER transpiled (the byte-parity boundary). This guard is
  // make-or-break now that TRANSPILE_EXTS includes `.js`/`.mjs`/`.cjs`: without it,
  // the compat tier would route every dependency `.js` through oxc. (loadTranspile's
  // own skip-gate handles the project-source no-op case; this keeps deps off the
  // pipeline entirely.) Mirrors the fast-tier sync hook's `!isNodeModules` gate.
  if (TRANSPILE_EXTS.has(ext) && !isNodeModules(url)) {
    // Module-format + decorator detection inside loadTranspile is a synchronous
    // native call (nub's addon), available on every supported Node — no parser
    // warm-up needed (the old `await ensureParser()` for the ESM-only oxc-parser
    // is gone with the package).
    return loadTranspile(url, ext);
  }
  // Project-source plain JS: transpile ONLY when it carries transformable syntax. A
  // no-op plain-JS file returns null and falls through to `nextLoad` — Node's own
  // loader handles it byte-identically, preserving every native CJS/ESM behavior.
  // node_modules excluded (the byte-parity boundary).
  if (PLAIN_JS_EXTS.has(ext) && !isNodeModules(url)) {
    const r = maybeTranspilePlainJs(url, ext);
    if (r) return r;
  }
  if (ext in DATA_EXTS) return loadData(url, ext);
  return nextLoad(url, context);
}

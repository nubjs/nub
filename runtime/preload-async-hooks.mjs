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

import {
  TRANSPILE_EXTS, DATA_EXTS,
  extname, resolveSpec, loadTranspile, loadData,
} from "./transform-core.mjs";
import { createRequire, isBuiltin } from "node:module";
import { existsSync, readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { pathToFileURL, fileURLToPath } from "node:url";

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

// Module format of a PnP-resolved file, so the resolve hook can hand Node an
// explicit `format`. WITHOUT this, Node ≤ 20.11 mis-detects a zip-stored `.js`
// file from a `"type":"module"` package as CommonJS, routes it through the CJS
// translator, and `require()`s the ESM source → ERR_REQUIRE_ESM. Node 20.19+ gets
// it right on its own, but emitting the format fixes the whole supported range
// (down to the 18.19 floor). `.mjs`/`.cjs` are unambiguous; a `.js` file inherits
// its package's `type` (read via PnP — fs is zip-patched in this worker). `null`
// lets Node decide (non-JS, or detection failed).
function pnpFormat(resolvedPath) {
  if (resolvedPath.endsWith(".mjs")) return "module";
  if (resolvedPath.endsWith(".cjs")) return "commonjs";
  if (!resolvedPath.endsWith(".js")) return null;
  try {
    const info = __pnp.getPackageInformation(__pnp.findPackageLocator(resolvedPath));
    const pj = JSON.parse(readFileSync(join(info.packageLocation, "package.json"), "utf8"));
    return pj.type === "module" ? "module" : "commonjs";
  } catch { return null; }
}

// ── Resolve hook ────────────────────────────────────────────────────
export async function resolve(specifier, context, nextResolve) {
  const r = resolveSpec(specifier, context.parentURL);
  if (r) return r;
  // Yarn PnP: resolve deps through PnP's own resolver, mirroring the fast tier.
  if (__pnp && !isBuiltin(specifier) && !specifier.startsWith("node:")) {
    try {
      const issuer = context.parentURL ? fileURLToPath(context.parentURL) : process.cwd() + "/";
      const resolved = __pnp.resolveRequest(specifier, issuer);
      if (resolved) {
        const format = pnpFormat(resolved);
        return { url: pathToFileURL(resolved).href, shortCircuit: true, ...(format && { format }) };
      }
    } catch { /* fall through to Node's resolver */ }
  }
  return nextResolve(specifier, context);
}

// ── Load hook ───────────────────────────────────────────────────────
export async function load(url, context, nextLoad) {
  const ext = extname(url);
  if (TRANSPILE_EXTS.has(ext)) {
    // Module-format + decorator detection inside loadTranspile is a synchronous
    // native call (nub's addon), available on every supported Node — no parser
    // warm-up needed (the old `await ensureParser()` for the ESM-only oxc-parser
    // is gone with the package).
    return loadTranspile(url, ext);
  }
  if (ext in DATA_EXTS) return loadData(url, ext);
  return nextLoad(url, context);
}

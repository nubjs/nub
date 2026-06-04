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

// Node calls this once per worker when the main thread invokes
// `module.register(url, parentURL, { data })`. We accept and ignore the payload
// so future main-thread → worker plumbing is non-breaking. Returning a Promise
// lets the main thread `await register(...)`.
export async function initialize(_data) {}

// ── Resolve hook ────────────────────────────────────────────────────
export async function resolve(specifier, context, nextResolve) {
  const r = resolveSpec(specifier, context.parentURL);
  return r ?? nextResolve(specifier, context);
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

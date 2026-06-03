// R8 early step for the compat tier (--import preload.mjs). Restores
// NODE_COMPILE_CACHE into process.env as a SIDE EFFECT, and must be the FIRST
// import in preload.mjs so it runs before transform-core.mjs's module body — which
// reads `NODE_COMPILE_CACHE === "0"` as nub's transpile-cache disable signal.
//
// Why a separate module instead of a statement in preload.mjs: ESM `import`s are
// hoisted and their module bodies evaluate in source order BEFORE any statement in
// the importer. transform-core.mjs is imported in preload.mjs, so the only way to
// mutate process.env ahead of transform-core's evaluation is from a module imported
// earlier in source order. The fast tier (preload.cjs, CommonJS) has no such
// hoisting and calls common.restoreCompileCacheEnv() directly instead.
//
// spawn.rs strips NODE_COMPILE_CACHE from the child env (so Node's V8 compile cache
// never caches nub's preload chain) and stashes the original value in a PID-keyed
// sentinel file; here we read + delete it and put it back. Restoring it in JS does
// not re-enable Node's bootstrap compile cache. See preload-common.cjs
// (restoreCompileCacheEnv) for the full mechanism — this file just calls it.
import { createRequire } from "node:module";

const require_ = createRequire(import.meta.url);
require_("./preload-common.cjs").restoreCompileCacheEnv();

// Nub compat-tier preload — Node 18.19–22.14, injected via `--import` (ESM).
//
// The FAST tier (Node 22.15+) is loaded separately, as a `--require` CommonJS
// preload (runtime/preload.cjs), so Node keeps its synchronous `Module.runMain`
// CJS entry path (top-level `executionAsyncId()===1`, sync exception origin,
// `require.main.id` `'.'`, `module.parent` `null`). The mere presence of an
// `--import` ESM preload forces eager ESM-loader init that routes even a CJS entry
// through the async ESM module-job (R1) — so the fast tier must NOT use `--import`.
//
// THIS file stays the compat path: on 18.19–22.14, `module.registerHooks` does not
// exist and `require(esm)` is unreliable, so hooks run async in a dedicated loader
// worker via `module.register`, and the parser is preloaded via dynamic `import()`
// while we can still `await`. That async machinery is exactly why the compat tier
// keeps `--import` — its top-level `await` is accepted here (an `--import` ESM
// module may be async), and the < 22.15 floor has no equivalent sync surface.
//
// Resolution + transpile primitives come from runtime/transform-core.mjs; the
// non-tier-specific wiring (watch IPC, the CJS require() shim, clobbered-polyfill
// preloading, the Temporal lazy global) is shared verbatim with the fast tier via
// runtime/preload-common.cjs, so the two tiers can never drift.

// MUST be first: restores NODE_COMPILE_CACHE into process.env (R8) before
// transform-core.mjs's body evaluates, since transform-core reads it as the
// transpile-cache disable signal. ESM imports evaluate in source order, so this
// side-effecting import has to precede the transform-core import. See
// compile-cache-restore.mjs.
import "./compile-cache-restore.mjs";
import module from "node:module";
import { createRequire } from "node:module";
import * as core from "./transform-core.mjs";

const __require = createRequire(import.meta.url);
const common = __require("./preload-common.cjs");
const { installSyncPolyfills } = __require("./polyfills.cjs");

// ── Tier detection ──────────────────────────────────────────────────
// This `.mjs` preload should only ever be `--import`ed for the compat tier (the
// Rust spawn path chooses `--require preload.cjs` for 22.15+). But guard anyway: if
// someone `--import`s it directly on an unsupported Node, emit a clear message and
// skip hook registration rather than throw (throwing breaks user-invoked --import
// flows). The fast-tier branch is intentionally absent here — 22.15+ goes through
// preload.cjs.
const [__major = 0, __minor = 0] = process.versions.node.split(".").map((n) => parseInt(n, 10));
const __isCompatTier = __major > 18 || (__major === 18 && __minor >= 19);
const __isFastTier = __major > 22 || (__major === 22 && __minor >= 15);

// Native TypeScript support (`process.features.typescript`). Where absent (the
// whole compat tier ≤ 22.17), Node can't load a required `.ts` on its own, so the
// classic require.extensions transpile shim is installed; where present it's
// skipped so Node's native require() of `.ts` isn't shadowed.
const __hasNativeTs = !!process.features?.typescript;

// Watch reporting + the Temporal lazy global are tier-independent.
common.installWatchReporting(core);

if (__isFastTier) {
  // Defensive only — the Rust path uses preload.cjs for 22.15+. If reached, the
  // sync registerHooks API is available; register synchronously to stay correct.
  const { resolve, load } = common.makeHooks(core, process.env.WATCH_REPORT_DEPENDENCIES === "1");
  module.registerHooks({ resolve, load });
  common.installCjsRequireHooks(core, !__hasNativeTs);
} else if (__isCompatTier) {
  // Compat path: ESM `import` hooks run in a dedicated loader worker thread.
  module.register("./preload-async-hooks.mjs", import.meta.url);
  // The main-thread CommonJS require() shim's format detection is synchronous and
  // reaches down to Node 18.19, where `require("oxc-parser")` (ESM-only) fails — so
  // preload the parser via dynamic import now, while we can still await.
  await core.ensureParser();
  // module.register() is ESM-loader-only; augment CommonJS require() on the main
  // thread too. The compat tier never has native `.ts`, so it always installs the
  // classic transpile shim (CJS transpile + clean error for ES-module require()).
  common.installCjsRequireHooks(core, !__hasNativeTs);
} else {
  process.stderr.write(
    `Nub requires Node 18.19 or newer for runtime augmentation; got ${process.versions.node}. Preload is inactive.\n`,
  );
}

// ── Clobbered-polyfill preloading + polyfills ───────────────────────
// Require the clobbered polyfill packages before the resolve hook would intercept
// them, then install the sync globals (shared with the fast tier) and the two ESM
// side-effect modules. On the compat tier `require(esm)` of the worker/navigator
// modules is unreliable, so they load via dynamic `import()` here.
const __preloadedPolyfills = common.preloadPolyfillPackages(__require);
installSyncPolyfills(__preloadedPolyfills);
if (typeof globalThis.navigator?.locks === "undefined") {
  await import("./navigator-locks.mjs");
}
await import("./worker-polyfill.mjs");

// ── Temporal: lazy global (A37) ─────────────────────────────────────
common.installTemporalLazyGlobal(__require);

// ── Compile-cache: re-enable for the USER's modules (R8) ────────────
// spawn.rs strips NODE_COMPILE_CACHE for every augmented spawn (both tiers), so
// nub's preload chain is never cached into the user's dir. Re-point the cache at
// the user's original dir (via the PID-keyed sentinel) so their own modules still
// cache. `module.enableCompileCache` only exists on Node 22.8+, so on most of the
// compat tier (< 22.8) this is a safe no-op; it benefits 22.8–22.14 users who set
// NODE_COMPILE_CACHE. See reenableUserCompileCache for the full rationale.
common.reenableUserCompileCache();

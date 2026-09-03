"use strict";
// `node --require <pkg>` — the CommonJS delivery of the full loader (ESM hooks +
// CommonJS require() augmentation). On Node 22.15+ this is strictly the better
// consumption shape: a `--require` CJS preload keeps Node's synchronous CJS entry
// path (the mere presence of `--import` forces eager async ESM-loader init that
// routes even a CJS entry through the async module-job — see preload.cjs, R1),
// which is exactly why the nub CLI injects its own fast-tier preload this way.
//
// require(esm) loads the shared ES-module arming logic synchronously (TLA-free by
// construction). Where require(esm) is unavailable — `--no-experimental-require-
// module`, or a compat-tier Node below 22.12/20.19 — fall back to registering the
// loader-worker hooks directly (preload-common is CommonJS, so that registration
// needs no require(esm)): `import`-side TS still transpiles through the worker,
// and only require()'d TS is inactive, matching the CLI preload's own degradation
// under that flag.
try {
  require("./loader-entry.mjs").arm({ esm: true, cjs: true });
} catch (err) {
  if (!err || err.code !== "ERR_REQUIRE_ESM") throw err;
  if (process.versions.electron) return;
  // The loader worker's transform-core needs the addon path in the inherited env
  // — loader-entry.mjs (which normally sets it) could not load here.
  require("./loader-platform.cjs").ensureAddonEnv(require);
  const { pathToFileURL } = require("node:url");
  const common = require("./preload-common.cjs");
  common.registerLoaderWorker(
    "./preload-async-hooks.mjs",
    pathToFileURL(__filename).href,
    // Same payload the ESM entry sends: the worker clears its own realm's
    // clobber map (see initialize in preload-async-hooks.mjs).
    { data: { standaloneLoader: true } },
  );
}

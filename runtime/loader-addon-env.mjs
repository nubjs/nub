// Standalone-loader addon plumbing — MUST evaluate before transform-core.mjs.
//
// transform-core loads the `nub-native` N-API addon at its own module evaluation
// (fast tier: eagerly, the moment the module body runs), probing a sibling
// `./addons/nub-native.node` first. Under the nub CLI that sibling always exists
// (the extracted runtime dir); in the standalone loader package the addon rides a
// per-platform npm package instead, so this module resolves it and hands the
// absolute path over via the internal `__NUB_ADDON_PATH` plumbing var — see
// ensureAddonEnv in loader-platform.cjs for the probe-order and worker-thread
// rationale.
//
// Why a separate side-effect module: ESM evaluates imports in source order, so the
// entry importing THIS file before transform-core is what guarantees the env var is
// set in time (the same ordering trick compile-cache-restore.mjs uses for
// NODE_COMPILE_CACHE). The createRequire import MUST come from `node:module`
// directly, NOT from floor-builtin.mjs: floor-builtin statically imports
// transform-core (to thread the floor's createRequire into it), so reaching
// createRequire through it would evaluate transform-core — and run its addon
// probe — before this module's body sets the env var. That ordering bug shipped
// in the first cut and only the dev tree's sibling addons/ dir masked it.
import { createRequire } from "node:module";

const __require = createRequire(import.meta.url);
__require("./loader-platform.cjs").ensureAddonEnv(__require);

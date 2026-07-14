// A foreign async ESM loader registered via `--import` — the tsx/ts-node shape that rides
// in the PROCESS's OWN execArgv (nub#460). The launcher-side argv scan can't see this when
// nub is invoked nested (`nub run` → `nub run` → tsx), via a `child_process` spawn
// (Playwright globalSetup), or behind a shell wrapper — so nub must detect the loader at
// PRELOAD time from its own execArgv (shouldAutoAsyncTierAtPreload) and take its async
// loader-worker tier. If it stays on the sync `module.registerHooks` fast tier, resolving
// this loader's own specifier reaches its unimplemented `resolveSync` stub on the broken
// band (Node 22.15–24.11) → ERR_METHOD_NOT_IMPLEMENTED, aborting the process.
import { register } from "node:module";
register("./passthrough-loader.mjs", import.meta.url);

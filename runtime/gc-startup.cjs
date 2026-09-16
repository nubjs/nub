// Main's heap has already copied its startup limit. Future isolates must see
// their own resource constraints instead of this process-global override.
const threads = require("node:worker_threads");
const key = "nub.gc-startup";
let injected = threads.getEnvironmentData(key);
if (threads.isMainThread && process.env.__NUB_GC_STARTUP) {
  injected = ["--max-semi-space-size=16", process.env.__NUB_GC_STARTUP];
  delete process.env.__NUB_GC_STARTUP;
  require("node:v8").setFlagsFromString("--max-semi-space-size=0");
  threads.setEnvironmentData(key, injected);
}
// This module also rides argv, so even a Worker with env: {} runs the hygiene.
// The ordinary augmentation preload rides NODE_OPTIONS and cannot cover that case.
if (Array.isArray(injected)) {
  process.execArgv = process.execArgv.filter(arg => !injected.includes(arg));
}

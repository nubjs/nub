// Runs on a worker thread started with no `execArgv`, so Node handed this thread the
// process's real exec argv. Two claims, and the second is the one that actually broke.
import { Worker } from "node:worker_threads";

// The flags nub hid from the main thread have to be hidden here too.
const rejected = process.execArgv.filter(
  (flag) => !process.allowedNodeEnvironmentFlags.has(flag.split("=")[0]),
);
console.log(`execargv:worker-rejected=${JSON.stringify(rejected)}`);

// A worker must be able to do what the main thread can: forward its own `execArgv` on.
// Reusing worker-child.mjs also carries the control down here — it resolves an
// `import defer` namespace, so a run that filtered the flag out of `execArgv` without
// disabling the feature still prints `execargv:worker-value=42`.
const nested = new Worker(new URL("./worker-child.mjs", import.meta.url), {
  execArgv: process.execArgv,
});
nested.on("error", (err) => {
  console.log(`execargv:nested-error=${err.code}`);
});

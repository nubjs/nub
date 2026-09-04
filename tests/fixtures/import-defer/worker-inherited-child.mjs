// Runs on a worker thread Node started from the process's real exec argv, so this thread
// received nub's argv-only flags whatever the main thread filtered. Two claims, and the
// second is the one that actually broke.
import { Worker, workerData } from "node:worker_threads";

const tag = workerData?.tag ?? "untagged";

// The flags nub hid from the main thread have to be hidden here too.
const rejected = process.execArgv.filter(
  (flag) => !process.allowedNodeEnvironmentFlags.has(flag.split("=")[0]),
);
console.log(`execargv:worker-rejected-${tag}=${JSON.stringify(rejected)}`);

// A worker must be able to do what the main thread can: forward its own `execArgv` on.
// Reusing worker-child.mjs carries the control down here too — it resolves an
// `import defer` namespace, so a run that filtered the flag out of `execArgv` without
// disabling the feature still prints `execargv:worker-value=42`.
await new Promise((resolve) => {
  const nested = new Worker(new URL("./worker-child.mjs", import.meta.url), {
    execArgv: process.execArgv,
  });
  nested.on("error", (err) => {
    console.log(`execargv:nested-error-${tag}=${err.code}`);
    resolve();
  });
  nested.on("exit", () => resolve());
});

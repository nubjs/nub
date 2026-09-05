// Regression guard from the Next.js 16 + Turbopack build break: real tooling forwards
// `process.execArgv` into a Worker, and Node once rejected a nub-injected V8 flag there
// with ERR_WORKER_INVALID_EXEC_ARGV. nub now turns `--js-defer-import-eval` on from
// inside the process instead of putting it on argv, so nothing nub adds may be left in
// execArgv, and the worker's own preload must still enable deferral.
import { Worker } from "node:worker_threads";

// Every flag left visible must be one Node would accept back in NODE_OPTIONS.
const rejected = process.execArgv.filter(
  (flag) => !process.allowedNodeEnvironmentFlags.has(flag.split("=")[0]),
);
console.log(`execargv:rejected=${JSON.stringify(rejected)}`);

// The exact pattern that broke: hand our own execArgv to a Worker.
const worker = new Worker(new URL("./worker-child.mjs", import.meta.url), {
  execArgv: process.execArgv,
});
worker.on("error", (err) => {
  console.log(`execargv:worker-error=${err.code}`);
  process.exitCode = 1;
});

// Regression guard for the Next.js 16 + Turbopack build break: nub injects
// `--js-defer-import-eval` on argv because Node refuses it in NODE_OPTIONS, and real
// tooling forwards `process.execArgv` into a Worker. Node then rejected nub's own flag
// with ERR_WORKER_INVALID_EXEC_ARGV and killed the build. nub's preload hides the flag
// from execArgv; V8 has already parsed it, so the feature must still work.
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

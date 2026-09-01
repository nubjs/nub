// The sibling guard (execargv.mjs) hands a Worker an EXPLICIT `execArgv`, so Node uses
// that already-filtered list and the thread never sees nub's argv-only flags. This one
// covers the DEFAULT shape instead: a Worker created with no `execArgv` at all, where
// Node starts the thread from the process's REAL exec argv — nub's flags included,
// whatever `process.execArgv` was filtered down to on the main thread.
//
// The worker runs nub's preload again, so it can filter them again; it just needs to be
// told to. When it was not, the flag reappeared in the worker's `execArgv` and the
// ERR_WORKER_INVALID_EXEC_ARGV break this whole mechanism exists to prevent simply moved
// one level down — into the worker pools (Jest, Vitest, tinypool, Turbopack) that are the
// single most likely thing to be forwarding `execArgv` in the first place.
import { Worker } from "node:worker_threads";

console.log(`execargv:root=${JSON.stringify(process.execArgv)}`);

const worker = new Worker(new URL("./worker-inherited-child.mjs", import.meta.url));
worker.on("error", (err) => {
  console.log(`execargv:worker-error=${err.code}`);
  process.exitCode = 1;
});

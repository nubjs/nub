// The sibling guard (execargv.mjs) hands a Worker an EXPLICIT `execArgv`, so Node uses
// that already-filtered list and the thread never meets the problem. These two cover the
// shapes where Node starts the thread from the process's REAL exec argv instead — nub's
// argv-only flags included, whatever the main thread filtered. The worker runs nub's
// preload again and has to filter them again, or the ERR_WORKER_INVALID_EXEC_ARGV break
// this mechanism exists to prevent simply moves one level down, into the worker pools
// (Jest, Vitest, tinypool, Turbopack) most likely to forward `execArgv` at all.
//
// The two differ in how the signal reaches the thread, which is the whole point:
//   - default        — the worker gets a copy of `process.env`, minus the consumed var.
//   - explicit `env` — the worker gets the object the CALLER passed, so nothing in
//                      `process.env` reaches it and only worker environment data does.
// A pool that builds `{ ...process.env, POOL_ID }` is the common real shape, and it is
// the one an env-only signal silently fails.
//
// Deliberately NOT covered: `env: {}`. Wiping the environment removes NODE_OPTIONS, so
// nub's preload does not run in that thread at all — measured — and no signal can help
// code that never executes. Such a worker has no nub augmentation of any kind.
import { Worker } from "node:worker_threads";

console.log(`execargv:root=${JSON.stringify(process.execArgv)}`);

const child = new URL("./worker-inherited-child.mjs", import.meta.url);

const run = (tag, options) =>
  new Promise((resolve, reject) => {
    const worker = new Worker(child, { workerData: { tag }, ...options });
    worker.on("error", (err) => {
      console.log(`execargv:worker-error=${err.code}`);
      process.exitCode = 1;
      resolve();
    });
    worker.on("exit", () => resolve());
    setTimeout(() => reject(new Error(`worker ${tag} timed out`)), 10000).unref();
  });

await run("default", {});
await run("explicitenv", { env: { ...process.env } });

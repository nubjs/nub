import { Worker } from "node:worker_threads";

// Regression guard: a worker that posts then idles (registers no inbound
// listener) must exit naturally. The pre-fix worker-polyfill held a ref'd
// `parentPort` listener that kept every worker's event loop alive forever. A 5s
// watchdog bounds this test — if the worker hangs, the parent's loop stays alive
// (the worker handle is ref'd), the watchdog fires, and we print "worker-hung"
// and exit non-zero so the suite fails FAST instead of blocking. On the fixed
// path the worker exits, `exit` fires, and the watchdog is cleared.
const w = new Worker(new URL("./natural-exit-worker.ts", import.meta.url));
const watchdog = setTimeout(() => {
  console.log("worker-hung");
  process.exit(3);
}, 5000);
w.on("message", (m) => console.log("main-got:" + m));
w.on("exit", (code) => {
  clearTimeout(watchdog);
  console.log("worker-exited:" + code);
});

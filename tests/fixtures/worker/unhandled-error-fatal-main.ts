// A worker whose module FAILS TO RESOLVE, with NO 'error'/onerror listener. The
// oracle (node:worker_threads.Worker, an EventEmitter) re-throws an unhandled
// 'error' on the main thread → uncaughtException → nonzero exit + printed stack.
// nub's browser-shape Worker is an EventTarget, whose dispatchEvent silently
// drops an unlistened event — so a failed worker load USED to exit 0 in total
// silence (the maintainer-found bug). The fix reproduces the oracle's fatality.
const w = new Worker(new URL("./does-not-exist.mjs", import.meta.url), {
  type: "module",
});

// If the error is swallowed, the underlying worker still emits `exit`. Use that
// event as the failure oracle so a slow worker load cannot race a fixed deadline.
w.once("exit", () => {
  console.log("swallowed-and-survived");
  process.exit(0);
});

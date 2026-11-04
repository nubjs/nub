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
//
// The oracle must not fire in the same turn as `exit`, though: the fatality is a
// deliberately-DEFERRED `process.nextTick` throw (worker-polyfill.mjs re-throws on
// a fresh tick so a user `uncaughtException` handler still sees it), and
// `process.exit` does NOT drain the tick queue. Whenever `exit` is delivered before
// that queue drains, exiting 0 here reports a false swallow — which is what made
// this test flake on CI. `setImmediate` is ordered strictly after the nextTick
// queue by the event-loop contract, so a scheduled fatality always wins, while a
// genuinely swallowed error still reaches the oracle and fails the test.
w.once("exit", () => {
  setImmediate(() => {
    console.log("swallowed-and-survived");
    process.exit(0);
  });
});

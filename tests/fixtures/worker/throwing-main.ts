// Parent spawns a worker that throws at top level. The parent's `onerror` must
// fire with the error's message exposed via the ErrorEvent shape, and the
// parent process must survive (exit 0) rather than crash with a ReferenceError
// from the polyfill's `new ErrorEvent(...)` on the < Node 26 floor.
const w = new Worker(new URL("./throwing-worker.ts", import.meta.url));

let fired = false;
w.onerror = (e: { message?: string; error?: { message?: string } }) => {
  fired = true;
  const msg = e.message ?? e.error?.message ?? "";
  console.log("parent-onerror:" + msg);
  (w as { terminate(): void }).terminate();
};

// If onerror never fires (or the parent crashes first), this timer never logs
// the success line and the test fails loudly.
setTimeout(() => {
  console.log("parent-alive:" + fired);
  process.exit(0);
}, 1000);

// Reclamation guard for the cancel-mid-suspend path — the residual leak that the
// first cancel fix introduced. When the consumer cancels while an async handler is
// suspended (Asyncify), the engine must be freed when the suspended write resumes
// (cleanup() owns all freeing; cancel() must NOT free — wasm-bindgen zeroes the
// wrapper ptr before the throwing wasm free, orphaning a live Rust object). The
// pre-fix path leaked ~every engine (+44MB/500) and HARD-CRASHED with "memory
// access out of bounds" by ~1000 cycles.
//
// This is a PURE cancel-mid-suspend loop (not interleaved with a fast-resume path
// that could mask the bug): each cycle forces the handler to actually suspend, then
// cancels, then lets the suspended write resume so reclamation can happen. It must
// (a) not crash, and (b) keep RSS FLAT across a measure window AFTER warmup — a
// per-engine WASM leak (~20KB each) over the window would balloon well past the
// bound and climb with N.
//
// Run with --expose-gc (the test harness passes it) for a stable reading.

const enc = new TextEncoder();
const rssMB = () => Math.round(process.memoryUsage().rss / 1024 / 1024);
const tick = (ms = 0) => new Promise((r) => setTimeout(r, ms));

async function cancelMidSuspendCycle() {
  const src = new Response(
    new ReadableStream({ start(c) { c.enqueue(enc.encode("<a>x</a>")); } }),
  );
  const res = new HTMLRewriter()
    .on("a", {
      async element(el) {
        // A real suspension point: the write is unwound here until this resolves.
        await tick(2);
        el.setAttribute("x", "1");
      },
    })
    .transform(src);
  const reader = res.body.getReader();
  const readP = reader.read().catch(() => {}); // kick off; handler suspends
  await tick(0); // ensure we're actually mid-suspend before cancelling
  await reader.cancel("abort").catch(() => {}); // must resolve, must not free here
  await readP; // let the suspended write RESUME → cleanup() frees the engine
  await tick(3); // give the resumed write a tick to finish freeing
}

async function loop(n) {
  for (let i = 0; i < n; i++) await cancelMidSuspendCycle();
}

if (typeof global.gc === "function") global.gc();
await loop(300); // warm up allocator pools so the measure window reflects real growth
if (typeof global.gc === "function") global.gc();
const warm = rssMB();

await loop(2000); // measure window — an unbounded leak adds ~40MB here and climbs

if (typeof global.gc === "function") global.gc();
const delta = rssMB() - warm;

console.log("CANCEL_MIDSUSPEND_NO_CRASH:", true); // reaching here = no OOB crash
console.log("CANCEL_MIDSUSPEND_DELTA_MB:", delta);
// 20MB ceiling over a 2000-cycle post-warmup window: JS churn fits comfortably
// under it; a per-engine WASM leak (~40MB+) blows past it and grows with N.
console.log("CANCEL_MIDSUSPEND_FLAT:", delta < 20);
console.log("DONE");

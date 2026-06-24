// A rejecting async handler must propagate the ORIGINAL error (not the secondary
// "recursive use of an object" the Asyncify rewind would otherwise surface), and
// must NOT leak the WASM engine — the rewind releases the held Rust borrow so the
// engine can be freed. See runtime/html-rewriter-engine/asyncify.js (nub patch).

const SENTINEL = "HANDLER_REJECTED_SENTINEL";

let message = "";
try {
  await new HTMLRewriter()
    .on("a", {
      async element() {
        await Promise.resolve();
        throw new Error(SENTINEL);
      },
    })
    .transform(new Response("<a>x</a>"))
    .text();
} catch (e) {
  message = e && e.message ? e.message : String(e);
}
// The ORIGINAL handler error must win — not a borrow/aliasing error from the engine.
console.log("REJECT_ORIGINAL:", message === SENTINEL);

// Recovery: after a rejecting transform, a fresh transform on a NEW instance must
// still work (the engine state isn't globally wedged).
const recovered = await new HTMLRewriter()
  .on("a", { element(el) { el.setAttribute("ok", "1"); } })
  .transform(new Response("<a>y</a>"))
  .text();
console.log("REJECT_RECOVERS:", recovered === `<a ok="1">y</a>`);

console.log("DONE");

// Consumer cancellation of the transformed output stream must resolve cleanly
// (reader.cancel() RESOLVES, never rejects), including when an async handler is
// suspended mid-transform — the held Rust borrow is tolerated by safeFree() and
// the engine is freed when the suspended write resumes. See runtime/html-rewriter.mjs.

// --- plain cancel (no async handler in flight) resolves ---
let plainResolved = false;
{
  const src = new Response(
    new ReadableStream({
      start(c) { c.enqueue(new TextEncoder().encode("<div>x</div>")); },
    }),
  );
  const res = new HTMLRewriter()
    .on("div", { element(el) { el.setAttribute("x", "1"); } })
    .transform(src);
  const reader = res.body.getReader();
  await reader.read();
  try {
    await reader.cancel("done");
    plainResolved = true;
  } catch {
    plainResolved = false;
  }
}
console.log("CANCEL_PLAIN_RESOLVED:", plainResolved);

// --- cancel WHILE an async handler is suspended must still resolve ---
let midResolved = false;
{
  const src = new Response(
    new ReadableStream({
      start(c) { c.enqueue(new TextEncoder().encode("<a>x</a>")); },
    }),
  );
  const res = new HTMLRewriter()
    .on("a", {
      async element(el) {
        await new Promise((r) => setTimeout(r, 50));
        el.setAttribute("x", "1");
      },
    })
    .transform(src);
  const reader = res.body.getReader();
  const readP = reader.read().catch(() => {}); // kick off; handler suspends
  await new Promise((r) => setTimeout(r, 10)); // ensure mid-suspend
  try {
    await reader.cancel("abort");
    midResolved = true;
  } catch {
    midResolved = false;
  }
  await readP;
}
console.log("CANCEL_MIDSUSPEND_RESOLVED:", midResolved);

console.log("DONE");

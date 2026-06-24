// Exercises the HTMLRewriter global end-to-end under nub. Prints `LINE: <value>`
// lines the integration test asserts against. Each line pins one contract.
// Cloudflare-exact: transform takes a Response and returns a Response. The engine
// is WASM lol-html with Asyncify, so handlers may be synchronous OR async.
import assert from "node:assert";

// Rewrite an HTML string by wrapping it in a Response and reading the result.
const rewrite = (rw, html) => rw.transform(new Response(html)).text();

// The global must be present and a constructor under nub augmentation.
assert.strictEqual(typeof HTMLRewriter, "function", "HTMLRewriter global missing");
// Additive contract: invisible to enumeration.
assert.ok(
  !Object.keys(globalThis).includes("HTMLRewriter"),
  "HTMLRewriter must be non-enumerable",
);

// --- element attribute + content mutation ---
const out1 = await rewrite(
  new HTMLRewriter()
    .on("a[href]", { element(el) { el.setAttribute("rel", "noopener"); } })
    .on("h1", { element(el) { el.setInnerContent("Hi"); } }),
  `<h1>x</h1><a href="/">link</a>`,
);
console.log("ATTR:", out1);

// --- escaped vs raw insertion ---
const out2 = await rewrite(
  new HTMLRewriter().on("p", {
    element(el) {
      el.append("<b>raw</b>", { html: true });
      el.append("<i>esc</i>");
    },
  }),
  "<p>x</p>",
);
console.log("CONTENT:", out2);

// --- remove + document end append + doctype read ---
let doctypeName = "";
const out3 = await rewrite(
  new HTMLRewriter()
    .on("script", { element(el) { el.remove(); } })
    .onDocument({
      doctype(dt) { doctypeName = String(dt.name); },
      end(end) { end.append("<!--end-->", { html: true }); },
    }),
  `<!DOCTYPE html><div>keep</div><script>evil()</script>`,
);
console.log("DOCTYPE:", doctypeName);
console.log("REMOVE:", out3);

// --- text handler ---
const out4 = await rewrite(
  new HTMLRewriter().on("span", {
    text(t) { if (t.text) t.replace(t.text.toUpperCase()); },
  }),
  "<span>hello</span>",
);
console.log("TEXT:", out4);

// --- ASYNC handler awaited mid-transform (Asyncify) ---
// The element + end handlers await a microtask and a timer before mutating; the
// engine must suspend the WASM stack across the await and resume correctly.
const outAsync = await rewrite(
  new HTMLRewriter()
    .on("a", {
      async element(el) {
        await Promise.resolve();
        await new Promise((r) => setTimeout(r, 5));
        el.setAttribute("data-async", "1");
      },
    })
    .onDocument({
      async end(end) {
        await new Promise((r) => setTimeout(r, 5));
        end.append("<!--async-end-->", { html: true });
      },
    }),
  `<a href="/">x</a>`,
);
console.log("ASYNC:", outAsync);

// --- streaming over a Response (headers preserved, content-length dropped) ---
const res = new HTMLRewriter()
  .on("title", { element(el) { el.setInnerContent("Streamed"); } })
  .transform(new Response("<title>old</title>", { headers: { "content-type": "text/html" } }));
assert.ok(res instanceof Response, "transform(Response) must return a Response");
assert.strictEqual(res.headers.get("content-type"), "text/html", "headers must carry over");
console.log("STREAM:", await res.text());

// --- non-Response input throws a TypeError ---
let badInput = false;
try {
  new HTMLRewriter().transform("<h1>x</h1>");
} catch (e) {
  badInput = e instanceof TypeError;
}
console.log("BADINPUT:", badInput);

// --- invalid selector throws synchronously at .on() ---
let badSel = false;
try {
  new HTMLRewriter().on("a + b", { element() {} });
} catch {
  badSel = true;
}
console.log("BADSEL:", badSel);

console.log("DONE");

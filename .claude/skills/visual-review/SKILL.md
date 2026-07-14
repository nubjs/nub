---
name: visual-review
description: >-
  Verify UI/layout/styling changes are correct by computing occlusion,
  clipping, and alignment from the browser's resolved paint order via the
  chrome-devtools MCP `evaluate_script` tool — instead of eyeballing a flat
  screenshot. Invoke BEFORE declaring any UI, site, or styling/layout change
  correct. Screenshots have no depth buffer, so z-index/occlusion/clip bugs are
  exactly where "just look at it" fails; the `evaluate_script` routines below
  turn those fuzzy visual judgments into deterministic measurements — including
  optical center-of-mass, which measures the glyph ink's true visual center so
  differently-sized labels can be aligned by more than eye.
metadata:
  internal: true
---

# Visual review — compute occlusion, don't perceive it

**The core insight:** a multimodal LLM reading a flat PNG has no depth buffer and no stacking-context model. Layering and clipping bugs — z-index, `overflow: hidden`, fixed/sticky overlays — are precisely the class where eyeballing a screenshot fails. The browser already resolved the paint order; that result is queryable. Use `evaluate_script` (chrome-devtools MCP) to read the browser's answer directly.

**Still take the screenshot** — geometry catches occlusion the eye misses; the eye catches font-metric and color issues geometry misses. Run both passes.

---

## Optical ≠ mathematical — equal numbers routinely look wrong

**The second core insight (codified after shipping a "balanced" button that wasn't):** a layout can be mathematically/geometrically consistent and still look *wrong*, because human perception is not a pixel ruler. Measuring `getBoundingClientRect` and confirming "padding is 14px on both sides" proves nothing about whether it *looks* balanced. NEVER declare a spacing/alignment/centering change correct from measurements alone — change it, screenshot it, and **look**, then adjust by eye until it looks right. The numbers are a starting point, not the verdict.

Recurring sources of "correct but looks wrong" — when you see these, expect to nudge *against* the math:

- **Rounded caps (pills, `rounded-full`) eat edge space.** Text/icon sitting `px-3.5` from a rounded end looks *tighter* than the same padding against a square edge, because the corner curves away from the content. A pill with symmetric padding and a leading icon looks lopsided: the text-adjacent cap needs **more** padding than the icon-adjacent one. (Fix that shipped: `pill` keeps symmetric `px`, each button adds `pr-*`/`pl-*` on its text side — Copy = icon-left so `pr-4`; Open = chevron-right so `pl-4`, the mirror.)
- **Icon ink ≠ icon box.** A `w-4` icon whose glyph is 14px and visually light (thin strokes, mass off-center — e.g. a two-square copy glyph) leaves dead space inside its box, inflating the *perceived* gap to adjacent text well beyond the measured flex `gap`.
- **Optical centering ≠ geometric centering.** A glyph can be mathematically centered in its box and ride visually high/low because the font's ink sits asymmetrically in the em (serifs and tall-ascender faces especially). Triangles/play-icons need to shift toward their visual mass, not their bbox center.

The discipline: when something "is correct" but the user (or you) sees it as off, **believe the eye and re-look at the screenshot**, don't re-cite the measurement. Geometry decides occlusion/clipping; the eye decides balance/scale/centering. Both passes, every time.

---

## The `evaluate_script` routines

Replace `'SELECTOR'` with a real CSS selector before running.

### 1 — Occlusion (the non-negotiable check)

Reports what fraction of the element is actually visible, and names anything covering it.

```js
(selector => {
  const el = document.querySelector(selector);
  if (!el) return { error: 'not found' };
  const r = el.getBoundingClientRect();
  if (r.width === 0 || r.height === 0) return { error: 'zero-size box' };
  const N = 5;                       // 5×5 = 25 sample points across the box
  let visible = 0; const coveredBy = new Set();
  for (let i = 0; i < N; i++) for (let j = 0; j < N; j++) {
    const x = r.left + (i + 0.5) / N * r.width;
    const y = r.top  + (j + 0.5) / N * r.height;
    const top = document.elementFromPoint(x, y);   // topmost painted element here
    if (top === el || el.contains(top)) visible++;
    else if (top) coveredBy.add(top.tagName.toLowerCase() +
                                (top.id ? '#' + top.id : '') +
                                (top.className ? '.' + String(top.className).split(' ')[0] : ''));
  }
  return { coverage: visible / (N * N), coveredBy: [...coveredBy] };
})('SELECTOR')
```

Reading the verdict:
- `coverage === 1` → fully visible, no occlusion.
- `coverage < 1` with a `coveredBy` entry that is not an ancestor/descendant → **occlusion bug**. The `coveredBy` array names the covering element (e.g. `nav.topbar`). This is what catches a clipped arrow behind a sticky header.

No z-index reasoning required — `elementFromPoint` returns the browser's resolved paint order directly.

### 2 — Ancestor overflow / clip

Detects clipping by an ancestor's `overflow: hidden` (a sibling overlay isn't the only way an element disappears).

```js
(selector => {
  const el = document.querySelector(selector);
  const r = el.getBoundingClientRect();
  for (let p = el.parentElement; p; p = p.parentElement) {
    const o = getComputedStyle(p).overflow;
    if (o === 'visible') continue;
    const pr = p.getBoundingClientRect();
    if (r.left < pr.left || r.top < pr.top || r.right > pr.right || r.bottom > pr.bottom)
      return { clippedBy: p.tagName + (p.id ? '#' + p.id : ''),
               overflow: o, target: r, clip: pr };
  }
  return { clipped: false };
})('SELECTOR')
```

`clipped: false` is clean. Any other return → the element is cropped by that ancestor.

### 3 — Alignment and spacing (measure, don't eyeball)

Compares two elements numerically. Use for anything that should align or sit at a fixed gap.

```js
([a, b] => {
  const A = document.querySelector(a).getBoundingClientRect();
  const B = document.querySelector(b).getBoundingClientRect();
  return {
    leftAligned: Math.abs(A.left - B.left),       // px delta; ~0 = aligned
    centerXdelta: Math.abs((A.left+A.right)/2 - (B.left+B.right)/2),
    gap: B.top - A.bottom,                          // vertical spacing between them
  };
})(['SEL_A', 'SEL_B'])
```

State verdicts in px, not vibes: "left-edge delta 2px (clean)" or "gap 28px vs expected 24px."

> ⚠️ `getBoundingClientRect` centers the **line box**, not the visible ink. For two elements at the **same** font-size this is fine. For elements at **different** font-sizes that must look centered together (a large wordmark beside small nav links, a caps badge beside body text), box-center is the wrong metric — they can be box-centered and still read as misaligned. Use routine §5 instead.

### 5 — Optical center of mass (different font-sizes / "looks off but measures equal")

This is the routine the "optical ≠ mathematical" section demands and §3 can't give you. It measures the **alpha-weighted centroid of the actual glyph ink** — the true optical center — by rasterizing each label's computed font to a canvas (no screenshot, no external image library: Canvas 2D + `getImageData` IS the raster surface). The full implementation lives next to this skill in [`optical-center.js`](optical-center.js); inline it into one `evaluate_script` call.

```js
// after inlining optical-center.js in the same evaluate_script:
opticalCenter(['.wordmark', 'a[href="/docs"]', 'a[href="/blog"]'])
//  → results:[{selector, comY, deltaFromAnchor}],  cssHint:[{selector, nudge}]
//    deltaFromAnchor ~0 = optically aligned; cssHint gives the ready-to-paste translate.
```

- **One call does measure + fix + verify.** Pass `{ apply: true }` and it nudges each non-anchor toward the anchor, **re-measures the real post-nudge DOM, and iterates** — so it converges on the true residual even when the nudge (or a wrapping element) perturbs the baseline. Returns `{ before, after, appliedTranslateY }`. This is what turns a naive "−2.3px" into the correct value once the nudge is expressed as a wrapping span. Don't hand-derive nudges across a layout change — let the loop converge, then transcribe `appliedTranslateY` to CSS.
- **`{ overlay: true }` paints the analysis onto the page** — a guide line on each label's COM (anchor solid-green, others dashed-red) with a px-delta label — so the **next screenshot self-documents** instead of making you reconcile a JSON number against a flat PNG.
- **Anchor choice matters.** A **filled** pill/badge is optically centered by its BOX, not its caps ink; anchor to a bare-glyph sibling (or accept a sub-px residual) rather than dragging text to a caps centroid.
- **Gotcha (auto-handled):** the baseline probe uses `vertical-align:baseline`, which flex/grid **ignore** — so the tool auto-descends from a flex `<a>` to the inline element that actually hosts the text. Hand it the natural selector; it finds the text host.
- **Scope:** exact for a single line of plain text. Letter-spacing, `text-shadow`, `-webkit-text-stroke`, gradient text, or arbitrary raster content aren't in the font render — for those, screenshot the element's clip box and centroid the real pixels (draw the PNG into a canvas via `Image`+`getImageData`; still no external lib), or just trust the eye.

**Integration — keep it to ONE tool call.** Steady-state usage is a single `evaluate_script`: inline the function + call it. To avoid re-inlining ~6KB every time, define it once via `navigate_page`'s `initScript` (runs on every new document) so `window.opticalCenter` is present in every later `evaluate_script` as a one-liner. The measure→apply→verify chain is NOT a series of calls — `{ apply: true }` does all three in-page and returns before/after. The only irreducible handoff is live-DOM → source CSS (the tool can't edit your source); `cssHint`/`appliedTranslateY` hand you the exact value to paste.

### Works with any browser-automation tool

`optical-center.js` is a bare, dependency-free function with JSON-in/JSON-out and no closure over outer scope — so it rides on **any** tool's evaluate primitive. The universal pattern is always the same two steps: **inject the source once (defines `window.opticalCenter`) → call it.** No MCP server needed; the overlay is drawn in-page, so every tool's own screenshot captures it.

```js
// chrome-devtools MCP — inline in one evaluate_script, or persist via initScript:
navigate_page({ url, initScript: <contents of optical-center.js> })
evaluate_script(`() => window.opticalCenter(['.wm','a[href="/docs"]'], { overlay:true })`)

// Playwright (Node):
await page.addInitScript({ path: 'optical-center.js' });     // window.opticalCenter on every doc
const r = await page.evaluate(([t,o]) => window.opticalCenter(t,o),
                              [['.wm','a[href="/docs"]'], { apply:true }]);

// Puppeteer:
await page.evaluateOnNewDocument(fs.readFileSync('optical-center.js','utf8'));
const r = await page.evaluate((t,o) => window.opticalCenter(t,o),
                              ['.wm','a[href="/docs"]'], { overlay:true });

// Selenium / WebDriver (any language):
driver.execute_script(open('optical-center.js').read())       // define it once
r = driver.execute_script("return window.opticalCenter(arguments[0], arguments[1])",
                          ['.wm', 'a[href="/docs"]'], { 'apply': True })

// DevTools console / bookmarklet: paste the file, then call opticalCenter([...]).
```

The invariants that keep it portable — **don't break these** when editing the file: no `import`/`export`/`require` in the injected source, args stay plain JSON, the return stays JSON-serializable (never hand back a DOM node), and it keeps defining a single global. Those four are exactly what let one artifact serve chrome-devtools MCP, Playwright, Puppeteer, Selenium, and a bookmarklet unchanged.

### 4 — Viewport and off-screen

An element pushed off-canvas reads out-of-viewport even when the screenshot crops it away:

```js
(selector => {
  const r = document.querySelector(selector).getBoundingClientRect();
  return {
    inViewport: r.top >= 0 && r.left >= 0 && r.bottom <= innerHeight && r.right <= innerWidth,
    rect: r,
    viewport: { w: innerWidth, h: innerHeight },
  };
})('SELECTOR')
```

---

## 7-step visual-review checklist

Run this for any change to `site/` or other rendered UI. Steps 3–4 are the non-negotiable additions that a screenshot review cannot do.

1. **Screenshot** — `take_screenshot`, full page + tight crop around the changed element. Note candidate problem elements.
2. **Console** — `list_console_messages`. A 200 response alongside a thrown error is still a broken page.
3. **Occlusion pass** — run routine §1 on the changed element AND any neighbors near fixed/sticky/absolute/overlay elements (nav bars, modals, tooltips, dropdowns, sticky headers). `coverage < 1` with a non-ancestor cover → flag it.
4. **Clip pass** — run routine §2 on the changed element.
5. **Alignment/spacing pass** — for anything that should align or sit at a fixed gap, run §3. Assert the px deltas; don't eyeball. For labels at **different font-sizes** that must look centered together, run §5 (optical center of mass) — box-center (§3) is the wrong metric there.
6. **Viewport pass** — confirm the element's box is inside `innerWidth/innerHeight` via §4.
7. **State the verdict in measurements.** "coverage 1.0, clip: false, left-edge delta 0px" is a clean bill of health. "coverage 0.62, coveredBy: nav.topbar" is a flag. Never a bare "looks great."

---

## If chrome-devtools MCP is unavailable

Say so explicitly. Reason about the stacking from the CSS (`position`, `z-index`, `overflow`, paint-order rules) — but acknowledge that this is inference, not measurement, and is less reliable for occlusion. Do not silently claim visual verification you couldn't do.

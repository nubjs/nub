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

A flat PNG has no depth buffer and no stacking-context model, so layering and clipping bugs (z-index, `overflow: hidden`, fixed/sticky overlays) are exactly where eyeballing fails. The browser already resolved the paint order; query it with `evaluate_script` (chrome-devtools MCP).

**Run both passes.** Geometry catches occlusion the eye misses; the eye catches font-metric and color issues geometry misses.

## Optical ≠ mathematical

A layout can be geometrically consistent and still look wrong. Never declare a spacing/alignment/centering change correct from measurements alone — change it, screenshot it, look, and adjust by eye. Expect to nudge *against* the math when:

- **Rounded caps (pills, `rounded-full`) eat edge space** — content at `px-3.5` from a rounded end looks tighter than against a square edge, so a pill with symmetric padding and a leading icon looks lopsided; the text-adjacent cap needs *more* padding (keep symmetric `px` on the pill, add `pr-*`/`pl-*` per button on its text side).
- **Icon ink ≠ icon box** — a `w-4` icon whose glyph is 14px and visually light leaves dead space, inflating the perceived gap beyond the measured flex `gap`.
- **Optical ≠ geometric centering** — a glyph can be box-centered and ride visually high/low because the font's ink sits asymmetrically in the em.

Geometry decides occlusion/clipping; the eye decides balance/scale/centering.

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

`coverage === 1` → no occlusion. `coverage < 1` with a `coveredBy` entry that is not an ancestor/descendant → occlusion bug; the array names the covering element. No z-index reasoning required — `elementFromPoint` returns the browser's resolved paint order.

### 2 — Ancestor overflow / clip

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

`clipped: false` is clean. Anything else → the element is cropped by that ancestor.

### 3 — Alignment and spacing

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

State verdicts in px: "left-edge delta 2px (clean)", "gap 28px vs expected 24px."

> `getBoundingClientRect` centers the **line box**, not the visible ink. Fine at the same font-size; for
> elements at **different** font-sizes that must look centered together, use §5.

### 4 — Viewport and off-screen

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

### 5 — Optical center of mass (different font-sizes)

Measures the alpha-weighted centroid of the actual glyph ink by rasterizing each label's computed font to a canvas. Implementation lives beside this skill in [`optical-center.js`](optical-center.js); inline it into one `evaluate_script` call.

```js
// after inlining optical-center.js in the same evaluate_script:
opticalCenter(['.wordmark', 'a[href="/docs"]', 'a[href="/blog"]'])
//  → results:[{selector, comY, deltaFromAnchor}],  cssHint:[{selector, nudge}]
//    deltaFromAnchor ~0 = optically aligned; cssHint gives the ready-to-paste translate.
```

- **One call does measure + fix + verify.** `{ apply: true }` nudges each non-anchor toward the anchor, re-measures the post-nudge DOM, and iterates until converged, returning `{ before, after, appliedTranslateY }`. Don't hand-derive nudges — transcribe `appliedTranslateY` to CSS.
- **`{ overlay: true }` paints the analysis onto the page** (anchor solid-green, others dashed-red, px deltas labelled) so the next screenshot self-documents.
- **Anchor choice matters** — a *filled* pill/badge is optically centered by its BOX, not its caps ink; anchor to a bare-glyph sibling.
- **Auto-handled:** the baseline probe uses `vertical-align:baseline`, which flex/grid ignore, so the tool descends from a flex `<a>` to the inline element hosting the text.
- **Scope:** exact for a single line of plain text. Letter-spacing, `text-shadow`, `-webkit-text-stroke`, gradient text, and raster content aren't in the font render — screenshot the element's clip box and centroid the real pixels instead, or trust the eye.

**Keep it to ONE tool call.** To avoid re-inlining ~6KB, define it once via `navigate_page`'s `initScript` so `window.opticalCenter` exists in every later `evaluate_script`.

### Portability

`optical-center.js` is dependency-free, JSON-in/JSON-out, no closure over outer scope, so it rides on any evaluate primitive. Pattern is always: **inject once (defines `window.opticalCenter`) → call it.**

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

**Don't break these invariants when editing the file:** no `import`/`export`/`require` in the injected source, args stay plain JSON, the return stays JSON-serializable (never a DOM node), and it keeps defining a single global.

---

## Icon-beside-text alignment — measure each glyph's INK

The most-repeated visual defect: an icon, glyph, emoji, badge, or counter beside text, riding high or low. `items-center` centers the glyph's **BOX** on the flex line; the eye aligns **INK**, and those never coincide — a digit has no descender so its ink rides high, an SVG's ink sits wherever its path falls in its viewBox, and an emoji is a bitmap whose metrics ignore your font size.

One shared nudge cannot fix a cluster. Measure each glyph, correct each glyph. Illustrative divergence for a real count-badge cluster (11.5px text, 12px icons):

| glyph | ink height | offset from the digit's ink center | correction |
| --- | --- | --- | --- |
| filled octicon | 10.8px | 1.16px low | `translateY(-0.1em)` |
| lucide stroke glyph | 9.0px | 1.29px low | `translateY(-0.112em)` |
| emoji | 16.0px | 0.26px — sub-pixel | none; a nudge only blurs the bitmap |

- **Express the correction in `em`, never `px`,** then prove it by doubling the cluster's font size and re-measuring; the residual must stay near zero.
- **Leave sub-pixel offsets alone.** Below ~0.3px you are under the device grid.
- **Re-measure after correcting.** The residual should read ~0.01px.

```js
// Baseline: an empty zero-size inline-block's bottom margin edge IS the baseline of the INLINE context
// it sits in. THE TRAP: a badge holder is usually inline-FLEX, so a probe appended there becomes a FLEX
// ITEM and reports the flex line's center — inflating a real 1.2px error into a plausible 3.5px one.
// Wrap the text node in its OWN inline span, probe INSIDE.
const baselineOfTextNode = (node) => {
  const span = document.createElement("span")
  node.parentNode.insertBefore(span, node); span.appendChild(node)
  const probe = document.createElement("span")
  probe.style.cssText = "display:inline-block;width:0;height:0;padding:0;margin:0;border:0"
  span.appendChild(probe)
  const baseline = probe.getBoundingClientRect().bottom
  const cs = getComputedStyle(span)
  const font = `${cs.fontStyle} ${cs.fontWeight} ${cs.fontSize} / ${cs.lineHeight} ${cs.fontFamily}`
  probe.remove(); span.parentNode.insertBefore(node, span); span.remove()
  return { baseline, font }
}
const inkOfText = (text, font, baseline) => {
  const c = document.createElement("canvas").getContext("2d"); c.font = font
  const m = c.measureText(text)
  return { top: baseline - m.actualBoundingBoxAscent, bottom: baseline + m.actualBoundingBoxDescent }
}
// An SVG geometry element's getBoundingClientRect IS its ink box (stroke included). Union EVERY child —
// a multi-path icon's ink is all of it, not the first path.
const inkOfSvg = (svg) => {
  const rects = [...svg.querySelectorAll("path,rect,circle,ellipse,polyline,polygon,line")]
    .map((g) => g.getBoundingClientRect())
  return { top: Math.min(...rects.map((r) => r.top)), bottom: Math.max(...rects.map((r) => r.bottom)) }
}
// → returns textInkCenter - glyphInkCenter. NEGATIVE = glyph sits BELOW the text and must be LIFTED.
```

**Distrust a suspiciously large reading.** A 3.5px error on an 11.5px font is ~30% — a broken instrument, not a misalignment. If the number claims a gross error and the picture shows a subtle one, fix the instrument.

**Mirror the real product before inventing a treatment.** If the thing exists in a product the user knows (GitHub, Linear, an existing component), drive the real one headless and read its computed values rather than designing from taste. Corollary: color belongs to an item's own state, not to its links.

---

## 7-step checklist

Run for any change to `site/` or other rendered UI. Steps 3–4 are what a screenshot review cannot do.

0. **You are the first reviewer of your own screenshot.** Capturing evidence is not reviewing it. Read the shot back and hunt for what is WRONG — a glyph riding high, one mark heavier than its neighbours, a collision at a narrow width. Capture at a scale where the detail is judgeable. Never hand the user a screenshot you have not personally critiqued.
1. **Screenshot** — `take_screenshot`, full page + tight crop around the changed element.
2. **Console** — `list_console_messages`. A 200 alongside a thrown error is still a broken page.
3. **Occlusion pass** — §1 on the changed element AND neighbors near fixed/sticky/absolute/overlay elements. `coverage < 1` with a non-ancestor cover → flag it.
4. **Clip pass** — §2 on the changed element.
5. **Alignment/spacing pass** — §3 for anything that should align or sit at a fixed gap. For labels at different font-sizes, §5. For any icon/glyph/emoji beside text, the ink measurement above, per glyph, then re-measure the residual.
6. **Viewport pass** — §4.
7. **State the verdict in measurements.** "coverage 1.0, clip: false, left-edge delta 0px" — never a bare "looks great."

## If chrome-devtools MCP is unavailable

Say so explicitly. Reason about stacking from the CSS (`position`, `z-index`, `overflow`, paint order), but label it inference, not measurement. Do not silently claim visual verification you couldn't do.

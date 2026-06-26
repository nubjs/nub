---
name: visual-review
description: >-
  Verify UI/layout/styling changes are correct by computing occlusion,
  clipping, and alignment from the browser's resolved paint order via the
  chrome-devtools MCP `evaluate_script` tool — instead of eyeballing a flat
  screenshot. Invoke BEFORE declaring any UI, site, or styling/layout change
  correct. Screenshots have no depth buffer, so z-index/occlusion/clip bugs are
  exactly where "just look at it" fails; the four `evaluate_script` routines
  below turn those fuzzy visual judgments into deterministic measurements.
---

# Visual review — compute occlusion, don't perceive it

**The core insight:** a multimodal LLM reading a flat PNG has no depth buffer and no stacking-context model. Layering and clipping bugs — z-index, `overflow: hidden`, fixed/sticky overlays — are precisely the class where eyeballing a screenshot fails. The browser already resolved the paint order; that result is queryable. Use `evaluate_script` (chrome-devtools MCP) to read the browser's answer directly.

**Still take the screenshot** — geometry catches occlusion the eye misses; the eye catches font-metric and color issues geometry misses. Run both passes.

---

## The four `evaluate_script` routines

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
5. **Alignment/spacing pass** — for anything that should align or sit at a fixed gap, run §3. Assert the px deltas; don't eyeball.
6. **Viewport pass** — confirm the element's box is inside `innerWidth/innerHeight` via §4.
7. **State the verdict in measurements.** "coverage 1.0, clip: false, left-edge delta 0px" is a clean bill of health. "coverage 0.62, coveredBy: nav.topbar" is a flag. Never a bare "looks great."

---

## If chrome-devtools MCP is unavailable

Say so explicitly. Reason about the stacking from the CSS (`position`, `z-index`, `overflow`, paint-order rules) — but acknowledge that this is inference, not measurement, and is less reliable for occlusion. Do not silently claim visual verification you couldn't do.

---
name: nub-charts
description: Build a performance chart for nubjs.com — the SVG bar figures in blog posts, docs pages and social posts (a runtime augmentation against plain node, an install or dispatch comparison, a cross-tool ranking). Use whenever asked for a graphic, chart, graph or figure showing a benchmark result, whenever adding a measured claim to a docs or blog page that deserves a picture, and whenever regenerating an existing figure with fresh numbers. Covers the house visual system (720px, the light/dark/opaque triple, the Nub palette and type), the three chart forms and which one fits, the Figure wiring and the alt/caption conventions, the benchmark provenance the numbers must have, and the layout traps that only show up once the SVG is rasterized.
---

# nubjs.com performance charts

Every performance figure on nubjs.com is a hand-generated SVG, not a charting library. They share one visual system with the homepage's `<Bench>` panel, and a new chart that does not match it looks broken next to the others.

## The six rules (maintainer, 2026-09-08 and 2026-09-12)

The first charts drawn with this skill broke the first four, the next one broke the fifth, and the threadpool figures shipped with the sixth unbroken only because the maintainer asked for the axis to go, so they come before anything else.

1. **One direction per figure.** A figure is either "higher is better" or "lower is better", never both. A throughput and a latency from the same benchmark are two figures with two stems.
2. **No footnotes and no caption line inside the image.** A block of small text under the axis reads as a disclaimer, as if the number needed excusing. The fixture and the method go in the page caption, the tweet, or the benchmark README, never in the SVG.
3. **The legend is left-aligned with the bar column, on its own line under the heading.** Everything at the top of a figure starts at the same x; a right-justified legend under left-justified text reads as misplaced.
4. **Cut the words.** A heading of two or three words, a muted note of at most a version and the direction, row labels of one or two words. The post or the page supplies the context; the figure supplies the numbers. If a label needs a sentence, the row needs a better name.
5. **The ink sits optically balanced in the frame: equal side margins, and generous ones.** The eye compares the two horizontal margins first, so the bar column starts where the longest label ends and stops where the longest trailing text meets the padding — the renderer computes both from the rows (`gutterFor`, `fitRight`), never from a fixed x, and a label is never padded to fill a gutter. The padding is 56px at the sides and 30px top and bottom (`PAD_X`, `PAD_Y`), the sides wider on purpose: a landscape frame needs more side margin than vertical margin to look evenly padded. Both halves of the rule were corrected on the same day, 2026-09-12, on one two-row chart: first *"TOO MUCH dead space on the left"* (short labels in a gutter sized for long ones, 140px of nothing on the left against 40px on the right), then, once the margins matched at 22px, *"it needs more space than that... optical balance"* — centered ink that close to the edge reads as cramped. Check it on the raster: the leftmost label and the rightmost note should sit the same, comfortable distance from their edges.
6. **A paired chart has no axis: no tick marks, no tick labels.** Every bar already carries its value, so an axis repeats the numbers, and with a scale per group its ticks land at different positions from one group to the next, which reads as misaligned. Groups are separated by spacing alone (`groupGap`), which also closed the gap between the panels of the threadpool figure that read as *"a little too much space between each chart"* (maintainer, 2026-09-12: *"just omit the x axis numbers altogether, and ticks"*). The unit therefore lives in the value label (`66 req/s`, `1,763 Mloops/s`), never in a tick.

A chart is the last step, never the first. The number comes from a benchmark under `tests/bench/` that survives the methodology in `AGENTS.md` and the `benchmarking` skill; the chart only draws it. **A figure built on numbers from a loaded machine is worse than no figure**, because it ships a claim nobody will re-check.

## Where things live

| what | where |
| --- | --- |
| published assets | [`site/public/blog/`](../../../site/public/blog/) — flat, no subdirectories, even for docs pages |
| the renderer | [`scripts/chart.mjs`](scripts/chart.mjs) next to this file |
| the `Figure` component | [`site/src/components/figure.tsx`](../../../site/src/components/figure.tsx) — `darkSrc` takes the dark rendering |
| the benchmark | `tests/bench/<family>/`, tracked, with a saved run under `results/`; the caption links to it |
| the one-off generator | your scratch directory — see "The generator is scratch, the benchmark is not" |

## The workflow

1. **Write or find the benchmark** in `tests/bench/`. It must print a machine-readable line per measurement — the runtime scripts print one `ROW {…}` per cell — so the generator never has numbers typed into it by hand. A run worth keeping is saved as `results/<date>.json` with the machine, the Node version and every per-round value; the generator reads that file. Typing numbers into the generator is not acceptable, because the figure then has no provenance.
2. **Run it on the latest Node, on a quiet machine.** A figure measured on an old Node line reads as if the win needed one; an augmentation that applies everywhere is measured on the current major (26 today), and the heading does not name a version. Only a version-gated augmentation is measured on the line it gates, and then the heading names that line, because the claim is about it. The dev Mac is never quiet; a runtime benchmark goes to an idle spot VM through `remote-build --job adhoc` (the script runs at the repo root with `NUB_BIN` set). For an install benchmark, `uptime` first and follow the `benchmarking` skill's load gate. A Windows-only behavior is measured on a real `windows-latest` runner through a dispatch-only workflow that uploads the results JSON as an artifact — `tests/bench/windows-dispatch/` and `.github/workflows/bench-windows-dispatch.yml` are the template — and the run is committed under `results/` before anything is drawn. A number remembered from a PR body, a code comment or a release post is not provenance, however well it was measured at the time: the Windows dispatch figure re-measured 95.6 → 35.8 ms as 92.9 → 19.9 ms on the current release, and only the saved run can say which is true today.
3. **Write a generator** that reads the saved run and calls `scripts/chart.mjs`. Keep it in your scratch directory.
4. **Rasterize and look at it.** `rsvg-convert -z 2 -b '#faf7f0' in.svg -o out.png`, then read the PNG. Every layout bug in "Traps" below is invisible in the SVG source and obvious in the render. Check the dark theme too, with `-b '#100f0d'`. Encode Sans is installed on the dev Mac, so the raster uses the site's face; the mono falls back to Menlo.
5. **Emit the triple** into `site/public/blog/` and wire it into the MDX.

## Trust the numbers before you draw them

A chart makes a number look settled, so check it is before you commit it to a figure.

- **Run several rounds and look at the spread before picking a statistic.** For a time, the per-cell minimum across rounds is the closest estimate of the noise floor. For a throughput under a load generator, the rounds cluster and the mean is the honest figure; state the round count in the page caption. One round is an anecdote.
- **Interleave the conditions.** Alternate node and nub on every round rather than running all of one then all of the other, so a drift in the box lands on both sides equally. The threadpool file-read question was settled only by an interleaved rerun: two back-to-back rounds read 4–8% apart, five interleaved rounds read 1.9%.
- **Watch which side of a comparison is unstable.** When the ratio swings but one series is rock-steady, the noise is entirely in the other one, and that usually names the mechanism.
- **Look for a control inside your own data.** `nub --node` in the same run is plain Node with nothing injected; if it does not match `node`, the harness is measuring something other than the augmentation. `node` with the flag set by hand is the other control: if `nub` does not match it, the augmentation is not what is being measured.
- **Say what the load generator shared.** autocannon on the same box competes with the server for cores, and a route bound by the event loop can read a couple of percent lower when the pool has more threads to schedule. Include the routes that do not gain; they are what makes the routes that do credible.
- **Ask whether the win is a ratio or a constant.** Subtract the two series per row. A roughly constant difference means a fixed cost is being skipped and the multiplier is a statement about the cheapest row; a difference that scales with the row means the work itself got faster. These are different claims and the prose must say which.
- **Check the mechanism in the source, not from the shape of the graph.** A caption built on the obvious story is the failure mode; read the code path that moved before writing it.

## The three forms

**`pairedChart`** — the same measurement under two conditions, one bar each per row: a track-colored bar for plain `node`, an ember bar for `nub`. No axis is drawn (rule 6); groups carry their own scale and unit, so two magnitudes of the same direction can share a figure without sharing a scale, and a figure never mixes directions. This is the form for a runtime augmentation, where the subject is often the LARGER number: req/s under nub against req/s under node. Sort rows by the size of the effect when the effect is the story, by a natural order (the routes as listed in the benchmark) when the spread is.

**`overlapChart`** — one measurement against another on a shared axis where the subject is the SMALLER number. The slower series is a track; the faster one is an ember bar drawn *inside* it, so the gap between them is the win. Right for a time under two conditions (cold vs warm, before vs after). **Sort by the ember bar**, not the track: ember is the subject, and a subject series that jumps around is the first thing a reader notices. Never use it for a throughput: the subject bar would cover the track and the comparison vanishes.

**`rankedChart`** — one value per row, longest first, optionally split into titled groups. This is the form for cross-tool comparisons, where the bars measure different things that have no pairing. Nub's own rows are highlighted; everything else is the track color.

Pick by whether the two numbers are the *same measurement under two conditions*. If they are and the subject is a time, overlap them; if the subject can be the larger number, pair them; if they are not the same measurement, rank them.

## The visual system

- **720px wide.** Height follows from the row count. Never widen; `Figure` scales to the content column, and a wider SVG just renders smaller.
- **Keep the padding.** The renderer leaves 30px above the heading and below the last row, and 56px at either side of the ink (`PAD_Y` and `PAD_X` in `chart.mjs`). Without it the figure reads as cropped, which is obvious the moment it sits on a page or in a tweet rather than on a preview; with less at the sides it reads as cramped even when centered (rule 5).
- **Row labels are monospace, right-aligned, in a left gutter** — `pbkdf2`, `320k awaits`, a tool name. The gutter is exactly as wide as the longest label plus its gap, and the bar column runs to wherever the longest value label or note meets the right padding: the renderer computes both from the rows (rule 5), so a figure with short labels gets a wider bar column, not an empty margin.
- **Heading, then legend, both aligned with the bar column, not with the left edge of the SVG.** Any text at the top of a chart starts at that x.
- **The heading names the thing measured**, in two or three words: `libuv threadpool`, `AsyncLocalStorage`, `Fastify + OpenTelemetry`. The comparison is what the two bars and the legend say.
- **`headingNote` carries the direction and, when it matters, the Node line** — `Node 22, lower is better`. Nothing else goes there.
- **The legend names the two conditions**, with the parameter that differs when there is one: `node, 4 threads` / `nub, 8 threads`, else just `node` / `nub`.
- **One accent color per chart, ember by default.** Ember is the subject; everything else is the track. The other four accents (`acid`, `sky`, `orchid`, `pink`) exist for a figure that sits next to a homepage section already using one of them — pass `accent`. `alt` (sky) exists only for a second nub configuration in a ranked chart.
- **The value label is bold on the subject bar, muted for the comparison.** A trailing muted note (`+23%`, `1.9× faster`, `flat`) after the subject's label is the only commentary a figure carries.

Both themes come from `THEMES` in `scripts/chart.mjs`, and every value there is a token from [`site/src/app/global.css`](../../../site/src/app/global.css): the fumadocs page and text pair, and the accent quintet in its darkened light-mode and bright dark-mode variants. Do not invent colors — a chart with an off-palette gray reads as a screenshot from somewhere else. The type is Encode Sans and Geist Mono, the site's own faces, with the same system fallbacks the CSS names.

## The light/dark/opaque triple

`writeChart()` emits three files per figure:

- `<stem>-light.svg` and `<stem>-dark.svg` — transparent, for `<Figure darkSrc>`, which swaps them on the `dark` class.
- `<stem>.svg` — light chart on an opaque `#faf7f0` background. This is for GitHub, where a transparent light chart is unreadable on the dark theme. Release notes and PR bodies use it.

Wire the pair in with `Figure` (already used in the blog posts that carry an image):

```mdx
<Figure
  src="/blog/threadpool-light.svg"
  darkSrc="/blog/threadpool-dark.svg"
  alt="..."
  caption={<>... (<a href="https://github.com/nubjs/nub/blob/main/tests/bench/runtime/threadpool.sh">benchmark</a>)</>}
/>
```

**The alt text carries the whole data table**, not a description of the picture: it names every row, both values and the multiplier, then closes on the headline. A screen reader user gets the same numbers a sighted reader does.

**The page caption says what is measured and how**, then links the benchmark file on `main`. This is where the fixture, the round count and the box go: the sentence that would have been a footnote inside the image belongs here, next to the figure rather than in it.

## Charts for social rather than the docs

Not every figure is destined for a page. When the target is a tweet or a slide, the opaque `<stem>.svg` is the one to use, rasterized at 2x — `rsvg-convert -z 2 in.svg -o out@2x.png`. It carries its own background, so it survives whatever theme the reader's client uses. The dark rendering on its page color (`-b '#100f0d'`) is the other option and matches the homepage's dark bench panel.

**The caveat lives in the copy.** The image never carried the fixture note, so whoever writes the post owns it: hand them the sentence (what was measured, how many rounds, what box, what shared it) along with the file.

**A chart with no page pointing at it does not belong in `site/public/blog/`.** That directory is for published assets. Leave a social-only figure in your scratch directory and hand over the path; unreferenced SVGs in the site tree are somebody's future cleanup.

## The generator is scratch, the benchmark is not

The benchmark and its saved run are tracked, reviewed, and linked from the caption; they are the reproducible artifact. The generator that turns one run into one SVG is a one-off — keep it in your scratch directory. `scripts/chart.mjs` exists so each generator is thirty lines of data selection rather than a copy of the layout.

## Traps

- **A callout needs an empty wedge.** The big `2.0×` numeral only works when the short rows are much shorter than the long ones, leaving the lower right of the plot empty. When every bar is long there is nowhere to put it, and it lands on top of the bars. Drop it; the per-row notes already say it.
- **A renderer collapses leading whitespace inside a `<tspan>`**, and a `&#160;` entity does not save you. To put a gap between a heading and a muted qualifier, set `dx` on the tspan — an explicit offset is the only thing that survives.
- **Legend text collides silently.** A long series label runs straight into a lengthened caption. Both faults are invisible in the source and obvious in the render, so check the render.
- **Mixed units belong in separate figures.** Two rows with different units in one group draw one of them at the wrong scale with no error, and a req/s group above an ms group is the direction mix rule 1 forbids.
- **Do not fold two comparisons into one figure.** Separate charts, separate files. To make a pair comparable instead, give both the same axis, the same rows and the same row order, and distinguish them only by the `heading`.
- **Do not pick rows to flatter the result.** Include the shape where the win is smallest, or negative; it is what makes the rest credible, and it tells the reader where the effect comes from.
- **`Figure` renders the image at the column width regardless of its aspect ratio.** A very tall chart still works, but 8–10 rows is the practical limit before the type gets small in the content column.
- **Measure the margins, do not eyeball them.** The leftmost ink is the longest label's start (`X0 − 12 − 0.6 × 12 × chars`), the rightmost is the longest note's end; both should be `PAD_X` (56px) from the frame. A one-line script over the SVG's `<rect>` and `<text>` positions settles it in seconds, and it is the check the renderer's own sizing is verified against. Then look at the raster anyway: equal margins that are too small read as cramped, and only the eye catches that.

## Regenerating an existing figure

Keep the stem and overwrite all three files, so no MDX changes. Then re-check every place the numbers appear as *prose* — the alt text, the caption, the surrounding paragraph, the summary bullets at the top of a release post, and any other page repeating the claim. A regenerated chart that contradicts the sentence next to it is the failure mode here; `grep` the multiplier across `site/content/` before you finish.

// House chart renderer for nubjs.com blog and docs figures.
//
// Three forms, all 720px wide, with a monospace label gutter on the left that is exactly as
// wide as the longest label and a bar column that ends where the longest trailing text meets
// the right padding — so the ink sits centered in the frame with the same 22px on either side
// whatever the labels are. Nothing here hardcodes where the bars start or end.
//   pairedChart  — the same measurement under two conditions, one bar each per row (node vs nub).
//                  No axis: the value labels are the numbers. Groups carry their own scale and
//                  unit, so two magnitudes can share one figure without sharing a scale.
//   overlapChart — one measurement against another on a shared axis where the subject is the
//                  SMALLER number (a time). The slower series is a track; the faster one is an
//                  ember bar drawn inside it, so the gap is the win.
//   rankedChart  — one value per row, longest first, optionally split into titled groups.
//
// writeChart() emits the light/dark/opaque triple the site expects. See SKILL.md.
//
// Every color is a token from site/src/app/global.css: the page and text pair from the
// fumadocs `--color-fd-*` set, the accents from the `--color-ember/acid/sky/orchid/pink`
// quintet (light mode carries the darkened AA variants, dark mode the bright ones). Do not
// add a color that is not in that file.

export const THEMES = {
  light: {
    text: "#1a1714",
    muted: "#6b6358",
    grid: "#e4dccb",
    track: "#efe9dd",
    bar: "#d6431f",
    alt: "#1f5fb0",
    accent: "#d6431f",
    page: "#faf7f0",
    accents: { ember: "#d6431f", acid: "#0f7a29", sky: "#1f5fb0", orchid: "#6d28d9", pink: "#bd3576" },
  },
  dark: {
    text: "#ece6d8",
    muted: "#aba297",
    grid: "#2a2620",
    track: "#2a2620",
    bar: "#ff5d3b",
    alt: "#7bb0ff",
    accent: "#ff5d3b",
    page: "#100f0d",
    accents: { ember: "#ff5d3b", acid: "#4fe173", sky: "#7bb0ff", orchid: "#c9a3ff", pink: "#ff6fb5" },
  },
};

// The site's type: Encode Sans for prose, Geist Mono for code and labels. A renderer without
// them falls through to the same system stacks the site's CSS names.
const SANS = "'Encode Sans', ui-sans-serif, system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif";
const MONO = "'Geist Mono', ui-monospace, SFMono-Regular, Menlo, monospace";

const esc = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
const pill = (x, y, w, h, fill) => `<rect x="${x}" y="${y}" width="${Math.max(w, 2).toFixed(1)}" height="${h}" rx="${Math.min(4, h / 2)}" fill="${fill}"/>`;

/** Nearest round axis ceiling at or above `max`, and the ticks to draw on it. */
export function axisFor(max) {
  const mag = 10 ** Math.floor(Math.log10(Math.max(max, 1)));
  const norm = max / mag;
  const step = (norm > 6 ? 2 : norm > 3 ? 1 : norm > 1.5 ? 0.5 : 0.25) * mag;
  const ceil = Math.ceil(max / step) * step;
  return { max: ceil, ticks: Array.from({ length: Math.round(ceil / step) + 1 }, (_, i) => i * step) };
}

/** 1,234 ns / 1.2 µs — per-call timings. */
export function fmtNs(v) {
  return v >= 10000 ? `${(v / 1000).toFixed(1)} µs` : `${Math.round(v).toLocaleString("en-US")} ns`;
}

/** 96 ms / 1.2 s — wall-clock timings. */
export function fmtMs(v) {
  return v >= 10000 ? `${(v / 1000).toFixed(1)} s` : `${Math.round(v).toLocaleString("en-US")} ms`;
}

/** 12,754 req/s — server throughput. */
export function fmtReq(v) {
  return `${Math.round(v).toLocaleString("en-US")} req/s`;
}

/** 46.0M / 152k — operation counts. */
export function fmtOps(v) {
  return v >= 1e6 ? `${(v / 1e6).toFixed(1)}M` : v >= 1e3 ? `${(v / 1e3).toFixed(0)}k` : v.toFixed(0);
}

/** A tick label: once the axis runs past four digits every tick reads in k, so 5k sits beside 10k rather than 5,000. */
function fmtTick(v, unitLabel, last, axisMax) {
  const n = axisMax >= 10000 ? `${(v / 1000).toFixed(v % 1000 ? 1 : 0)}k` : v.toLocaleString("en-US");
  return `${n}${last && unitLabel ? ` ${unitLabel}` : ""}`;
}

/** Approximate rendered width of 12px Encode Sans, for placing things that must not collide. */
const textW = (s, size = 12) => String(s).length * size * 0.54;
/** The same for the monospace labels: Geist Mono and Menlo both advance 0.6em per glyph. */
const monoW = (s, size = 12) => String(s).length * size * 0.6;

// The breathing room around the ink. Without it the figure reads as cropped, and the two
// horizontal margins are what the eye compares: a figure with 140px of nothing to the left of
// its labels and 40px to the right of its notes reads as pushed off-center however carefully
// each element is aligned (maintainer, 2026-09-12). The gutter and the bar column are sized
// from the content so that both side margins come out at PAD_X. The sides get more than the
// top and bottom on purpose: in a frame this wide, equal margins all round look tight at the
// sides, and 22px there read as cramped even once they matched (same day).
const PAD_X = 56;
const PAD_Y = 30;
/** Left edge of the bar column: the widest label, right-aligned, plus its gap, after the padding. */
const gutterFor = (labels) => PAD_X + Math.ceil(Math.max(0, ...labels.map((l) => monoW(l)))) + 12;
/**
 * Right edge of the bar column: the widest column such that every piece of text anchored to a
 * bar still ends inside the padding. Each anchor is text placed at X0 + f·(XMAX − X0) + c, where
 * f is the bar's share of the axis and c the text's offset plus width, so the binding one is
 * the row whose bar-plus-label runs longest — not necessarily the longest bar.
 */
const fitRight = (W, X0, anchors) => Math.floor(Math.min(W - PAD_X, ...anchors.filter((a) => a.f > 0).map((a) => X0 + (W - PAD_X - X0 - a.c) / a.f)));

function frame({ theme, opaque, W, H, title }) {
  const t = THEMES[theme];
  let s = `<svg xmlns="http://www.w3.org/2000/svg" width="${W}" height="${H}" viewBox="0 0 ${W} ${H}" font-family="${SANS}" font-size="13">`;
  if (opaque) s += `<rect x="0" y="0" width="${W}" height="${H}" fill="${t.page}"/>`;
  if (title) s += `<title>${esc(title)}</title>`;
  return s;
}

/** A two-swatch legend, left-aligned with the bar column on its own line under the heading. */
function legend(s, t, x, y, aLabel, bLabel, barFill) {
  s += `<rect x="${x}" y="${y - 10}" width="10" height="10" rx="2" fill="${t.track}"/><text x="${x + 14}" y="${y - 1}" fill="${t.muted}" font-size="12">${esc(aLabel)}</text>`;
  const bx = x + 14 + textW(aLabel) + 18;
  s += `<rect x="${bx.toFixed(1)}" y="${y - 10}" width="10" height="10" rx="2" fill="${barFill}"/><text x="${(bx + 14).toFixed(1)}" y="${y - 1}" fill="${t.muted}" font-size="12">${esc(bLabel)}</text>`;
  return s;
}

function heading_(s, t, X0, padY, heading, headingNote) {
  if (!heading) return s;
  return s + `<text x="${X0}" y="${padY + 16}" fill="${t.text}" font-weight="700" font-size="14">${esc(heading)}${headingNote ? `<tspan dx="9" fill="${t.muted}" font-weight="400">${esc(headingNote)}</tspan>` : ""}</text>`;
}

/**
 * groups: [{ title?, unit?, rows: [{ label, a, b, note? }] }] — `a` is the baseline (plain
 * node, drawn as a track-colored bar), `b` the subject (nub, drawn in the accent). Each group
 * has its own scale, and `unit` formats its value labels; no axis is drawn, so the unit has to
 * be in the label (`fmtReq` gives `66 req/s`). Every group in a figure reads in the SAME direction — a figure never mixes
 * "higher is better" with "lower is better"; that is two figures. Rows are drawn in the order given.
 */
export function pairedChart({ groups, heading, headingNote, aLabel = "node", bLabel = "nub", accent = "ember", title, theme = "light", opaque = false }) {
  const t = THEMES[theme];
  const barFill = t.accents[accent] ?? t.bar;
  const W = 720, rowH = 44, barH = 12, gap = 3;
  const padY = PAD_Y;
  const X0 = gutterFor(groups.flatMap((g) => g.rows.map((r) => r.label)));
  // The column runs as wide as the value labels and notes allow: a clipped "+3%" is invisible
  // in the source and the first thing a reader sees, so the row whose bar-plus-text runs
  // longest sets the edge.
  const XMAX = fitRight(W, X0, groups.flatMap((g) => {
    const unit = g.unit ?? fmtReq;
    const axis = axisFor(Math.max(...g.rows.flatMap((r) => [r.a, r.b])));
    return g.rows.flatMap((r) => [
      { f: r.a / axis.max, c: 7 + textW(unit(r.a), 11) },
      { f: r.b / axis.max, c: 7 + unit(r.b).length * 6.6 + (r.note ? 8 + textW(r.note, 11) : 0) },
    ]);
  }));
  const headH = heading ? 20 : 0;
  const legendH = aLabel && bLabel ? 22 : 0;
  // No axis: every bar carries its own value label, so ticks and tick labels only repeated the
  // numbers and, with a scale per group, at positions that did not line up from one group to
  // the next (maintainer, 2026-09-12). Groups are separated by spacing alone.
  const groupTitleH = 24, groupGap = 10;
  const top = padY + headH + legendH + 8;
  const plotH = groups.reduce((h, g) => h + (g.title ? groupTitleH : 0) + g.rows.length * rowH, 0) + (groups.length - 1) * groupGap;
  const H = top + plotH + padY - 8;

  let s = frame({ theme, opaque, W, H, title: title ?? heading });
  s = heading_(s, t, X0, padY, heading, headingNote);
  if (legendH) s = legend(s, t, X0, padY + headH + 16, aLabel, bLabel, barFill);

  let y = top;
  for (const g of groups) {
    const unit = g.unit ?? fmtReq;
    if (g.title) {
      s += `<text x="${X0}" y="${y + 12}" fill="${t.text}" font-weight="600" font-size="12">${esc(g.title)}</text>`;
      y += groupTitleH;
    }
    const axis = axisFor(Math.max(...g.rows.flatMap((r) => [r.a, r.b])));
    const sx = (v) => Math.max((v / axis.max) * (XMAX - X0), 3);
    const rowsTop = y;
    g.rows.forEach((r, i) => {
      const ry = rowsTop + i * rowH + (rowH - (2 * barH + gap)) / 2;
      s += `<text x="${X0 - 12}" y="${ry + barH + gap / 2 + 4}" text-anchor="end" fill="${t.text}" font-family="${MONO}" font-size="12">${esc(r.label)}</text>`;
      const aw = sx(r.a), bw = sx(r.b);
      s += pill(X0, ry, aw, barH, t.track);
      s += `<text x="${(X0 + aw + 7).toFixed(1)}" y="${ry + barH - 1}" fill="${t.muted}" font-size="11">${esc(unit(r.a))}</text>`;
      s += pill(X0, ry + barH + gap, bw, barH, barFill);
      const lead = unit(r.b);
      const leadX = X0 + bw + 7;
      s += `<text x="${leadX.toFixed(1)}" y="${ry + 2 * barH + gap - 1}" fill="${t.text}" font-weight="700" font-size="11">${esc(lead)}</text>`;
      if (r.note) s += `<text x="${(leadX + lead.length * 6.6 + 8).toFixed(1)}" y="${ry + 2 * barH + gap - 1}" fill="${t.muted}" font-size="11">${esc(r.note)}</text>`;
    });
    y = rowsTop + g.rows.length * rowH + groupGap;
  }
  return `${s}</svg>`;
}

/**
 * rows: [{ label, track, bar, note? }] — `track` is the slower value, `bar` the faster one.
 * Rows are drawn in the order given; sort before calling (by `bar`, so the subject series reads
 * monotone). Only for a measurement where the SMALLER number is the subject: a time, never a
 * throughput — use pairedChart for those.
 */
export function overlapChart({ rows, heading, headingNote, trackLabel, barLabel, title, axisMax, ticks, unit = fmtNs, unitLabel = "ns", accent = "ember", theme = "light", opaque = false, callout }) {
  const t = THEMES[theme];
  const barFill = t.accents[accent] ?? t.bar;
  const W = 720, rowH = 34, barH = 20;
  const padY = PAD_Y;
  const headH = heading ? 20 : 0;
  const legendH = trackLabel && barLabel ? 22 : 0;
  const top = padY + headH + legendH + 12;
  const H = top + rows.length * rowH + 34 + padY;
  const axis = axisMax ? { max: axisMax, ticks: ticks ?? axisFor(axisMax).ticks } : axisFor(Math.max(...rows.map((r) => r.track)));
  const X0 = gutterFor(rows.map((r) => r.label));
  // A note clears whichever runs longer, the track or the bold value label, so both are anchors.
  const XMAX = fitRight(W, X0, [
    { f: 1, c: textW(fmtTick(axis.max, unitLabel, true, axis.max), 11) / 2 },
    ...rows.flatMap((r) => {
      const lead = unit(r.bar);
      return [
        { f: r.bar / axis.max, c: 7 + lead.length * 7 },
        ...(r.note ? [{ f: r.track / axis.max, c: 8 + textW(r.note) }, { f: r.bar / axis.max, c: 7 + lead.length * 7 + 10 + textW(r.note) }] : []),
      ];
    }),
  ]);
  const sx = (v) => Math.max((v / axis.max) * (XMAX - X0), 3);

  let s = frame({ theme, opaque, W, H, title: title ?? heading });
  s = heading_(s, t, X0, padY, heading, headingNote);
  if (legendH) s = legend(s, t, X0, padY + headH + 16, trackLabel, barLabel, barFill);

  const axisY = top + rows.length * rowH + 6;
  // A callout needs an empty wedge to sit in, which only exists when the short rows are much
  // shorter than the long ones. Grid lines break around it so they never cross the numeral.
  const block = callout ? { x0: callout.x - 96, x1: XMAX + 6, y0: top + 128, y1: top + 244 } : null;
  for (const v of axis.ticks) {
    const x = X0 + (v / axis.max) * (XMAX - X0);
    const line = (y1, y2) => `<line x1="${x.toFixed(1)}" y1="${y1}" x2="${x.toFixed(1)}" y2="${y2}" stroke="${t.grid}" stroke-width="1"/>`;
    s += block && x > block.x0 && x < block.x1 ? line(top - 4, block.y0) + line(block.y1, axisY) : line(top - 4, axisY);
    s += `<text x="${x.toFixed(1)}" y="${axisY + 16}" text-anchor="middle" fill="${t.muted}" font-size="11">${v ? esc(fmtTick(v, unitLabel, v === axis.max, axis.max)) : "0"}</text>`;
  }

  rows.forEach((r, i) => {
    const y = top + i * rowH + (rowH - barH) / 2;
    const trackW = sx(r.track), barW = sx(r.bar);
    s += `<text x="${X0 - 12}" y="${y + 14}" text-anchor="end" fill="${t.text}" font-family="${MONO}" font-size="12">${esc(r.label)}</text>`;
    s += pill(X0, y, trackW, barH, t.track);
    s += pill(X0, y, barW, barH, barFill);
    const lead = unit(r.bar);
    const leadX = X0 + barW + 7;
    s += `<text x="${leadX.toFixed(1)}" y="${y + 14}" fill="${t.text}" font-weight="700" font-size="12">${esc(lead)}</text>`;
    if (r.note) {
      // clear both the bold label and the end of the track, whichever runs longer
      const noteX = Math.max(X0 + trackW + 8, leadX + lead.length * 7 + 10);
      s += `<text x="${noteX.toFixed(1)}" y="${y + 14}" fill="${t.muted}" font-size="12">${esc(r.note)}</text>`;
    }
  });

  if (callout) {
    s += `<text x="${callout.x}" y="${top + 150}" text-anchor="middle" fill="${t.muted}" font-size="16">${esc(callout.pre ?? "up to")}</text>`;
    s += `<text x="${callout.x}" y="${top + 208}" text-anchor="middle" fill="${barFill}" font-weight="800" font-size="62">${esc(callout.value)}</text>`;
    s += `<text x="${callout.x}" y="${top + 234}" text-anchor="middle" fill="${t.text}" font-weight="600" font-size="14">${esc(callout.post)}</text>`;
  }
  return `${s}</svg>`;
}

/**
 * groups: [{ title?, rows: [{ label, value, highlight? }] }] — one bar per row, scaled to the
 * largest value in the whole chart. `highlight: true` paints nub's bars in the accent, "alt" in
 * the sky accent (a second nub configuration); everything else is the track color.
 */
export function rankedChart({ groups, heading, title, unit = fmtOps, accent = "ember", theme = "light", opaque = false }) {
  const t = THEMES[theme];
  const barFill = t.accents[accent] ?? t.bar;
  const W = 720, rowH = 30, barH = 18, top = 44, groupGap = 44;
  const multi = groups.length > 1;
  const H = top + groups.reduce((h, g) => h + g.rows.length * rowH + (multi ? groupGap : 4), 0);
  const max = Math.max(...groups.flatMap((g) => g.rows.map((r) => r.value)));
  const X0 = gutterFor(groups.flatMap((g) => g.rows.map((r) => r.label)));
  const XMAX = fitRight(W, X0, groups.flatMap((g) => g.rows.map((r) => ({ f: r.value / max, c: 8 + textW(unit(r.value)) * (r.highlight ? 1.08 : 1) }))));

  let s = frame({ theme, opaque, W, H, title: title ?? heading });
  if (heading) s += `<text x="${X0}" y="22" fill="${t.text}" font-weight="700" font-size="16">${esc(heading)}</text>`;
  let y = top;
  for (const g of groups) {
    if (multi && g.title) {
      s += `<text x="${X0}" y="${y + 10}" fill="${t.text}" font-weight="600" font-size="12">${esc(g.title)}</text>`;
      y += 22;
    }
    for (const r of g.rows) {
      const w = Math.max((r.value / max) * (XMAX - X0), 3);
      const fill = r.highlight === true ? barFill : r.highlight === "alt" ? t.alt : t.track;
      s += `<text x="${X0 - 12}" y="${y + 13}" text-anchor="end" fill="${t.text}" font-family="${MONO}" font-size="12">${esc(r.label)}</text>`;
      s += pill(X0, y, w, barH, fill);
      s += `<text x="${(X0 + w + 8).toFixed(1)}" y="${y + 13}" fill="${r.highlight ? t.text : t.muted}" font-weight="${r.highlight ? 700 : 400}" font-size="12">${esc(unit(r.value))}</text>`;
      y += rowH;
    }
    y += multi ? groupGap - 22 : 4;
  }
  return `${s}</svg>`;
}

/**
 * Writes the triple the site expects: <stem>-light.svg and <stem>-dark.svg for <Figure darkSrc>,
 * plus an opaque <stem>.svg that stays readable on GitHub's dark theme.
 */
export function writeChart(fs, outdir, stem, render) {
  fs.writeFileSync(`${outdir}/${stem}-light.svg`, render("light", false));
  fs.writeFileSync(`${outdir}/${stem}-dark.svg`, render("dark", false));
  fs.writeFileSync(`${outdir}/${stem}.svg`, render("light", true));
  return [`${stem}-light.svg`, `${stem}-dark.svg`, `${stem}.svg`];
}

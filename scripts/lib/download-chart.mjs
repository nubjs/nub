// Renders the download stats as one self-contained HTML file — no build step, no CDN, no
// dependencies, so it opens straight from disk.
//
// The page answers two questions and nothing else: how many downloads, and how they split
// between the two channels. Bars are stacked by SOURCE (npm below, GitHub above), which is
// the only split that can be stacked at all — the npm platform packages are pulled BY an
// npm install, so stacking them would draw the same install twice.
//
// Only COMPLETE periods are drawn. A part-week bar is not a small week, it is a week that
// has not finished, and rendering one puts a fake cliff at the right edge of every chart.
// The current month is the single exception: it appears at its full PROJECTED height, with
// the forecast hatched, so the bar still spans a real month.

const PALETTE = {
  light: {
    surface: "#fcfcfb", panel: "#ffffff", text: "#0b0b0b", secondary: "#52514e",
    muted: "#78776f", grid: "#e6e4de",
    npm: "#2a78d6", github: "#eb6834", forecastInk: "#78776f",
  },
  dark: {
    surface: "#1a1a19", panel: "#212120", text: "#ffffff", secondary: "#c3c2b7",
    muted: "#94938a", grid: "#333331",
    npm: "#3987e5", github: "#d95926", forecastInk: "#94938a",
  },
};

const fmt = (n) => Number(n).toLocaleString("en-US");
// One rule for every number on a chart: mixing "15K" with "4,212" on the same axis reads
// as two different units.
const compact = (n) =>
  n >= 1e6 ? `${(n / 1e6).toFixed(1)}M`
  : n >= 1e4 ? `${Math.round(n / 1e3)}K`
  : n >= 1e3 ? `${(n / 1e3).toFixed(1)}K`
  : fmt(n);
const esc = (s) =>
  String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);

/** Axis ticks on 1/2/5 x 10^n boundaries, so labels read as round numbers. */
function niceTicks(max, target = 5) {
  if (max <= 0) return { ticks: [0], top: 1 };
  const raw = max / target;
  const mag = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 2.5, 5, 10].map((m) => m * mag).find((s) => s >= raw) ?? 10 * mag;
  const top = Math.ceil(max / step) * step;
  const ticks = [];
  for (let v = 0; v <= top + 1e-9; v += step) ticks.push(v);
  return { ticks, top };
}

/** Rounded data-end, square feet — the cap is only on the topmost segment of a stack. */
function capPath(x, y, w, h, r = 4) {
  const rad = Math.min(r, w / 2, h);
  return `M${x} ${y + h}V${y + rad}a${rad} ${rad} 0 0 1 ${rad} ${-rad}h${w - 2 * rad}a${rad} ${rad} 0 0 1 ${rad} ${rad}V${y + h}Z`;
}

const GAP = 2; // surface gap dividing stacked segments — white does the separating
const MAX_BAND = 120;

/**
 * @param buckets [{ label, tip, segments: [{ value, kind }] }] — segments stack bottom-up.
 */
function stackedChart(buckets, title) {
  const W = 1000, H = 330, m = { t: 30, r: 20, b: 46, l: 68 };
  const iw = W - m.l - m.r, ih = H - m.t - m.b;
  const totals = buckets.map((b) => b.segments.reduce((s, g) => s + g.value, 0));
  const { ticks, top } = niceTicks(Math.max(...totals, 1));
  const plotW = Math.min(iw, buckets.length * MAX_BAND);
  const band = plotW / buckets.length;
  const bw = Math.min(24, band - 2);
  const base = m.t + ih;
  const y = (v) => base - (v / top) * ih;
  const left = m.l + (iw - plotW) / 2;
  const right = left + plotW;

  const grid = ticks
    .map(
      (t) =>
        `<line class="grid" x1="${left.toFixed(1)}" y1="${y(t).toFixed(1)}" x2="${right.toFixed(1)}" y2="${y(t).toFixed(1)}"/>` +
        `<text class="tick" x="${(left - 12).toFixed(1)}" y="${(y(t) + 4).toFixed(1)}" text-anchor="end">${compact(t)}</text>`,
    )
    .join("");

  const bars = buckets
    .map((b, i) => {
      const cx = left + i * band + band / 2;
      const x = cx - bw / 2;
      let cursor = base;
      const drawn = b.segments.filter((g) => g.value > 0);
      const marks = drawn
        .map((g, si) => {
          const isTop = si === drawn.length - 1;
          const full = (g.value / top) * ih;
          // Every segment above the first gives up GAP px so the surface shows through.
          const h = Math.max(1, full - (si ? GAP : 0));
          const yTop = cursor - full + (si ? 0 : 0);
          cursor = cursor - full;
          const shape = isTop
            ? `<path d="${capPath(x, yTop + (si ? GAP : 0), bw, h)}"`
            : `<rect x="${x.toFixed(1)}" y="${(yTop + (si ? GAP : 0)).toFixed(1)}" width="${bw}" height="${h.toFixed(1)}"`;
          const paint =
            g.kind === "forecast"
              ? `class="forecast" stroke="var(--forecast-ink)" stroke-width="1"`
              : `fill="var(--${g.kind})"`;
          return `${shape} ${paint}/>`;
        })
        .join("");

      const total = totals[i];
      return (
        `<g class="col" tabindex="0" data-tip="${esc(b.tip)}">` +
        `<rect class="hit" x="${(left + i * band).toFixed(1)}" y="${m.t - 24}" width="${band.toFixed(1)}" height="${ih + 24}"/>` +
        marks +
        `<text class="value" x="${cx.toFixed(1)}" y="${(y(total) - 10).toFixed(1)}" text-anchor="middle">${compact(total)}</text>` +
        `</g>` +
        `<text class="tick" x="${cx.toFixed(1)}" y="${base + 22}" text-anchor="middle">${esc(b.label)}</text>`
      );
    })
    .join("");

  return `<svg viewBox="0 0 ${W} ${H}" role="img" aria-label="${esc(title)}">${grid}${bars}
    <line class="axis" x1="${left.toFixed(1)}" y1="${base}" x2="${right.toFixed(1)}" y2="${base}"/></svg>`;
}

/** The all-time split — one bar, both channels measured, no history required. */
function sourceBar(npmTotal, ghTotal) {
  const total = npmTotal + ghTotal;
  const W = 1000, H = 26, r = 5;
  const npmW = (npmTotal / total) * W - GAP / 2;
  const ghX = npmW + GAP;
  const ghW = W - ghX;
  const round = (x, w, left) =>
    left
      ? `M${x + r} 0H${x + w}V${H}H${x + r}A${r} ${r} 0 0 1 ${x} ${H - r}V${r}A${r} ${r} 0 0 1 ${x + r} 0Z`
      : `M${x} 0H${x + w - r}A${r} ${r} 0 0 1 ${x + w} ${r}V${H - r}A${r} ${r} 0 0 1 ${x + w - r} ${H}H${x}Z`;
  return `<svg class="sourcebar" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" role="img"
    aria-label="All-time split: ${fmt(npmTotal)} npm, ${fmt(ghTotal)} GitHub">
    <path d="${round(0, npmW, true)}" fill="var(--npm)"/>
    <path d="${round(ghX, ghW, false)}" fill="var(--github)"/></svg>`;
}

function table(rows, unit) {
  const body = rows
    .map(
      (r) =>
        `<tr><td>${esc(r.label)}</td><td class="num">${fmt(r.npm)}</td><td class="num">${r.github === null ? "—" : fmt(r.github)}</td><td class="num">${r.forecast ? fmt(r.forecast) : ""}</td><td class="num">${fmt(r.total)}</td></tr>`,
    )
    .join("");
  return `<table><thead><tr><th>${unit}</th><th class="num">npm</th><th class="num">GitHub</th><th class="num">Forecast</th><th class="num">Total</th></tr></thead><tbody>${body}</tbody></table>`;
}

export function renderChart({ summary, weeks, months, meta }) {
  const npmTotal = summary.npm?.all_time ?? 0;
  const ghTotal = summary.github?.binary_downloads ?? 0;
  const pct = (n) => ((n / (npmTotal + ghTotal)) * 100).toFixed(1);
  const ghMeasured = weeks.some((b) => b.github !== null) || months.some((b) => b.github !== null);

  const toBuckets = (rows) =>
    rows.map((r) => ({
      label: r.label,
      segments: [
        { value: r.npm, kind: "npm" },
        { value: r.github ?? 0, kind: "github" },
        { value: r.forecast ?? 0, kind: "forecast" },
      ],
      tip:
        `<b>${r.label}</b><br>${fmt(r.npm)} npm` +
        (r.github === null ? "<br>GitHub not measured" : `<br>${fmt(r.github)} GitHub`) +
        (r.forecast ? `<br>+${fmt(r.forecast)} forecast (${r.forecastNote})` : "") +
        `<br><b>${fmt(r.total)} total</b>`,
    }));

  const vars = (p) =>
    Object.entries(p)
      .map(([k, v]) => `--${k.replace(/[A-Z]/g, (c) => "-" + c.toLowerCase())}:${v}`)
      .join(";");

  const hatch = ["light", "dark"]
    .map(
      (mode) => `<pattern id="hatch-${mode}" width="7" height="7" patternUnits="userSpaceOnUse">
      <rect width="7" height="7" fill="${PALETTE[mode].panel}"/>
      <path d="M0,7 L7,0 M-1.5,1.5 L1.5,-1.5 M5.5,8.5 L8.5,5.5" stroke="${PALETTE[mode].forecastInk}" stroke-width="2" fill="none"/>
    </pattern>`,
    )
    .join("");

  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>nub downloads — ${esc(summary.generated)}</title>
<style>
:root{color-scheme:light dark;${vars(PALETTE.light)}}
@media (prefers-color-scheme:dark){:root{${vars(PALETTE.dark)}}}
*{box-sizing:border-box}
html{background:var(--surface)}
body{margin:0;padding:48px 24px 72px;background:var(--surface);color:var(--text);
  font:15px/1.55 ui-sans-serif,system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;-webkit-font-smoothing:antialiased}
main{max-width:1000px;margin:0 auto}
h1{font-size:18px;font-weight:600;margin:0 0 4px}
h2{font-size:15px;font-weight:600;margin:0 0 2px}
.sub{color:var(--secondary);margin:0 0 36px;font-size:13.5px}
.hero{font-size:68px;font-weight:600;letter-spacing:-.025em;line-height:1;margin:0 0 18px}
.sourcebar{width:100%;height:26px;display:block;margin-bottom:12px}
.split{display:flex;gap:24px;color:var(--secondary);font-size:13.5px;margin:0 0 44px;flex-wrap:wrap}
.split span{display:flex;gap:8px;align-items:center}
.swatch{width:11px;height:11px;border-radius:3px;display:inline-block;flex:none}
.swatch.npm{background:var(--npm)}
.swatch.github{background:var(--github)}
.swatch.forecast{background:repeating-linear-gradient(45deg,var(--forecast-ink) 0 2px,var(--panel) 2px 5px);border:1px solid var(--forecast-ink)}
.views{display:inline-flex;gap:2px;padding:3px;background:var(--panel);border:1px solid var(--grid);border-radius:9px;margin-bottom:20px}
.views button{appearance:none;border:0;background:transparent;color:var(--secondary);font:inherit;font-size:13px;
  padding:6px 16px;border-radius:6px;cursor:pointer}
.views button[aria-pressed=true]{background:var(--npm);color:#fff;font-weight:600}
.views button:focus-visible{outline:2px solid var(--npm);outline-offset:2px}
section{margin:0 0 44px}
.chart-note{color:var(--muted);font-size:12.5px;margin:2px 0 14px}
svg{width:100%;height:auto;display:block;overflow:visible}
.grid,.axis{stroke:var(--grid);stroke-width:1}
.tick{fill:var(--muted);font-size:11.5px;font-variant-numeric:tabular-nums}
.value{fill:var(--text);font-size:11.5px;font-weight:600;font-variant-numeric:tabular-nums}
.hit{fill:transparent}
.col{cursor:default;outline:none}
.col:hover rect:not(.hit),.col:hover path,.col:focus-visible rect:not(.hit),.col:focus-visible path{filter:brightness(1.12)}
.forecast{fill:url(#hatch-light)}
@media (prefers-color-scheme:dark){.forecast{fill:url(#hatch-dark)}}
.svg-defs{position:absolute;width:0;height:0;overflow:hidden}
[data-view]{display:none}
body[data-active=weekly] [data-view=weekly],body[data-active=monthly] [data-view=monthly]{display:block}
table{border-collapse:collapse;width:100%;font-size:13px;font-variant-numeric:tabular-nums}
th,td{text-align:left;padding:7px 12px;border-bottom:1px solid var(--grid)}
th{color:var(--secondary);font-weight:500;font-size:12px}
td.num,th.num{text-align:right}
footer{color:var(--muted);font-size:12.5px;border-top:1px solid var(--grid);padding-top:18px}
footer p{margin:0 0 8px}
code{font:12.5px/1.4 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;
  background:color-mix(in srgb,var(--grid) 55%,transparent);padding:1px 5px;border-radius:4px;white-space:nowrap}
#tip{position:fixed;pointer-events:none;opacity:0;transition:opacity .1s;background:var(--panel);
  border:1px solid var(--grid);border-radius:8px;padding:8px 11px;font-size:12.5px;
  box-shadow:0 4px 16px rgba(0,0,0,.13);z-index:9;white-space:nowrap;font-variant-numeric:tabular-nums}
</style></head><body data-active="monthly">
<svg class="svg-defs" aria-hidden="true" focusable="false"><defs>${hatch}</defs></svg>
<main>

<h1>nub downloads</h1>
<p class="sub">Through ${esc(summary.npm?.last_day ?? summary.generated)}. Snapshot taken ${esc(summary.generated)}.</p>

<p class="hero">${fmt(summary.total_downloads)}</p>
${sourceBar(npmTotal, ghTotal)}
<div class="split">
  <span><i class="swatch npm"></i>npm <code>${esc(meta)}</code> — ${fmt(npmTotal)} (${pct(npmTotal)}%)</span>
  <span><i class="swatch github"></i>GitHub releases — ${fmt(ghTotal)} (${pct(ghTotal)}%)</span>
</div>

<div class="views" role="group" aria-label="Time granularity">
  <button type="button" data-set="monthly" aria-pressed="true">Monthly</button>
  <button type="button" data-set="weekly" aria-pressed="false">Weekly</button>
</div>

<section>
  <p class="chart-note" data-view="monthly">Complete calendar months. The month in progress is drawn at its full projected height, with the forecast hatched.</p>
  <p class="chart-note" data-view="weekly">Complete UTC weeks, Monday-anchored. The week in progress is omitted rather than drawn short.</p>
  <div data-view="monthly">${stackedChart(toBuckets(months), "Monthly downloads by source")}</div>
  <div data-view="weekly">${stackedChart(toBuckets(weeks), "Weekly downloads by source")}</div>
  <div class="split" style="margin-top:14px">
    <span><i class="swatch npm"></i>npm</span>
    <span><i class="swatch github"></i>GitHub</span>
    <span><i class="swatch forecast"></i>forecast</span>
  </div>
</section>

<section>
  <div data-view="monthly">${table(months, "Month")}</div>
  <div data-view="weekly">${table(weeks, "Week")}</div>
</section>

<footer>
  <p><b>Downloads, not users.</b> An upgrade re-downloads, and npm counts CI, mirrors and bots.</p>
  <p><b>The two channels add cleanly and nothing is counted twice.</b> One <code>npm i -g ${esc(meta)}</code> pulls the meta package and one <code>@nubjs/nub-&lt;platform&gt;</code> package with it, so the platform packages are excluded — they are part of an npm install, not a third source. The curl installer, <code>nub upgrade</code> and the Homebrew tap all fetch GitHub release assets and never touch npm. GitHub counts exclude ${fmt(summary.github?.excluded?.checksum ?? 0)} <code>.sha256</code> fetches.</p>
  ${
    ghMeasured
      ? ""
      : `<p><b>The GitHub segment is missing from every bar, and that is the data, not a bug.</b> GitHub exposes cumulative counters with no history API, so a GitHub time series only exists across repeated snapshots — ${esc(String(summary.github?.snapshots ?? 0))} taken so far, none yet spanning a whole period. The all-time split above is fully measured; the per-period split fills in as snapshots accumulate.</p>`
  }
</footer>

</main><div id="tip"></div>
<script>
const tip = document.getElementById("tip");
for (const el of document.querySelectorAll("[data-tip]")) {
  const show = (ev) => {
    tip.innerHTML = el.dataset.tip;
    tip.style.opacity = 1;
    const r = tip.getBoundingClientRect(), box = el.getBoundingClientRect();
    tip.style.left = Math.min((ev ? ev.clientX : box.left) + 14, innerWidth - r.width - 10) + "px";
    tip.style.top = Math.max(8, (ev ? ev.clientY : box.top) - r.height - 12) + "px";
  };
  el.addEventListener("pointermove", show);
  el.addEventListener("focus", () => show(null));
  el.addEventListener("pointerleave", () => (tip.style.opacity = 0));
  el.addEventListener("blur", () => (tip.style.opacity = 0));
}
for (const btn of document.querySelectorAll(".views button")) {
  btn.addEventListener("click", () => {
    document.body.dataset.active = btn.dataset.set;
    for (const b of document.querySelectorAll(".views button")) b.setAttribute("aria-pressed", String(b === btn));
    tip.style.opacity = 0;
  });
}
</script></body></html>
`;
}

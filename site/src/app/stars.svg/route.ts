// `/stars.svg` — a dynamically-generated GitHub-style "Star" button for the README.
//
// Returns `image/svg+xml` replicating GitHub's dark-theme Star button: the outline star
// octicon + "Star" label + a rounded count pill, with the live nubjs/nub star count
// counting up once on render. The count-up MUST be declarative CSS (`@keyframes`): GitHub
// embeds README images as `<img>`, which is script-disabled, so no JS runs — but internal
// `<style>` animations play (the readme-typing-svg precedent). Each digit is a fixed-width
// (tabular) cell holding a vertical tape of glyphs 0..final that slides up so the final
// digit lands in the pill window, easing + freezing (`forwards`, iteration-count 1). Fixed
// cell widths keep the pill from reflowing as the digits roll.
//
// Freshness is camo-bound: GitHub proxies + caches README images (~31 days default), so
// "fresh on every load" is structurally impossible. We send a 1-hour s-maxage with a long
// stale-while-revalidate so the count is approximately current while never approaching the
// 60/hr unauthenticated GitHub API limit. The animation replays whenever a viewer's
// browser actually (re)fetches + renders the SVG — a cache-miss render, not every visit.

export const revalidate = 3600; // ISR: re-fetch the count at most hourly on the origin.

const GITHUB_API = 'https://api.github.com/repos/nubjs/nub';
const FALLBACK = 1700; // sensible floor if the API call fails — never error the image.

// GitHub dark-theme button palette.
const C = {
  btnBg: '#21262d',
  btnBorder: '#30363d',
  text: '#e6edf3',
  countBg: '#30363d',
};

// octicons star-16 (outline).
const STAR_PATH =
  'M8 .25a.75.75 0 0 1 .673.418l1.882 3.815 4.21.612a.75.75 0 0 1 .416 1.279l-3.046 2.97.719 4.192a.751.751 0 0 1-1.088.791L8 12.347l-3.766 1.98a.75.75 0 0 1-1.088-.79l.72-4.194L.818 6.374a.75.75 0 0 1 .416-1.28l4.21-.611L7.327.668A.75.75 0 0 1 8 .25Zm0 2.445L6.615 5.5a.75.75 0 0 1-.564.41l-3.097.45 2.24 2.184a.75.75 0 0 1 .216.664l-.528 3.084 2.769-1.456a.75.75 0 0 1 .698 0l2.77 1.456-.53-3.084a.75.75 0 0 1 .216-.664l2.24-2.183-3.096-.45a.75.75 0 0 1-.564-.41L8 2.694Z';

// Geometry — calibrated against GitHub's real button via chrome-devtools measurement.
const H = 30; // button height (matches GitHub's default)
const ICON = 16;
const PAD_L = 13; // left padding to star
const ICON_GAP = 6; // star -> "Star"
const LABEL_W = 25.5; // "Star" @ 12px/600 system
const LABEL_GAP = 8; // "Star" -> count pill
const PILL_PAD = 7; // horizontal padding inside the pill
const PILL_H = 17;
const PILL_PAD_R = 12; // right padding after the pill
const DIGIT_H = 20; // tape advance per glyph (> PILL_H so only one digit shows at rest)

const FONT = '600 12px -apple-system,BlinkMacSystemFont,"Segoe UI",system-ui,sans-serif';
const COUNT_FONT = '600 12px ui-monospace,"SF Mono",Menlo,monospace';

async function getStarCount(): Promise<number> {
  try {
    const res = await fetch(GITHUB_API, {
      headers: { Accept: 'application/vnd.github+json' },
      next: { revalidate: 3600 },
    });
    if (!res.ok) return FALLBACK;
    const data = (await res.json()) as { stargazers_count?: number };
    return typeof data.stargazers_count === 'number' ? data.stargazers_count : FALLBACK;
  } catch {
    return FALLBACK;
  }
}

/**
 * Count pill: a rounded-full rect with the comma-formatted number, rendered as an ODOMETER
 * that counts 0 -> count over one shared ~6.5s ease-in-out timeline, then freezes.
 *
 * Each digit position is a vertical "tape" of glyphs inside its own nested `<svg>` viewport.
 * As the global value sweeps 0..count, a position holding place value `pv` (1, 10, 100, ...)
 * advances at the values `0, pv, 2*pv, ...`, displaying `s % 10` at its s-th advance — so its
 * tape lists those digits and slides up by `steps * DIGIT_H`. All tapes share ONE eased SMIL
 * timeline (same dur + spline), so the ones column spins fast while higher columns crawl — a
 * synchronized odometer that decelerates into the final value.
 *
 * Clipping: a nested `<svg>` (default overflow:hidden) is the window, and the slide is a SMIL
 * `<animateTransform>` (in the SVG render tree, so it IS clipped to the window). This is the
 * combination that actually holds in GitHub's `<img>` SVG renderer — a CSS-`transform`
 * animation composites into a layer that ESCAPES SVG clipping (clip-path/mask alike), leaking
 * adjacent glyphs out of the pill; SMIL inside a nested-svg viewport does not. `fill="freeze"`
 * holds the final number; `calcMode="spline"` gives the ease-in-out. JS never runs in a
 * README-embedded `<img>`, so the animation must be declarative (SMIL/CSS) — SMIL here.
 *
 * "Skip intermediate values": with ease-in-out over ~6.5s the sweep blurs through the middle
 * and slows at both ends; the tape positions are exact, so it lands precisely on the count.
 * Fixed mono cell widths keep the pill from reflowing as digits roll (jitter-free).
 */
function countPill(str: string, pillX: number, count: number): { svg: string; width: number } {
  const cellW = 7.4; // fixed mono cell @12px
  const commaW = 3.8;
  let inner = 0;
  for (const ch of str) inner += ch === ',' ? commaW : cellW;
  const pillW = inner + PILL_PAD * 2;
  const pillY = (H - PILL_H) / 2;
  const baseY = pillY + PILL_H / 2 + 12 * 0.34; // optical center of the 12px cap block (for the comma)
  const baseLocal = PILL_H / 2 + 12 * 0.34; // same, but in nested-svg-local coords

  // Place value per digit column, left to right. e.g. "1,744" -> [1000, _, 100, 10, 1].
  const digitChars = str.replace(/,/g, '');
  const placeOf: number[] = [];
  for (let i = 0; i < digitChars.length; i++) {
    placeOf.push(Math.pow(10, digitChars.length - 1 - i));
  }

  // Shared SMIL ease-in-out: accelerate in, decelerate as it settles. ~6.5s, plays once, freezes.
  const anim =
    '<animateTransform attributeName="transform" type="translate" from="0 0" to="0 -TRAVEL" dur="6.5s" calcMode="spline" keyTimes="0;1" keySplines="0.7 0 0.3 1" repeatCount="1" fill="freeze"/>';

  let cx = pillX + PILL_PAD;
  let digitIdx = 0;
  const cells: string[] = [];
  for (const ch of str) {
    if (ch >= '0' && ch <= '9') {
      const pv = placeOf[digitIdx];
      const steps = Math.floor(count / pv); // how many glyph-advances this column makes
      const glyphs: string[] = [];
      for (let s = 0; s <= steps; s++) {
        // At the s-th advance of this column the global value is s*pv, so it shows s % 10.
        glyphs.push(
          `<text x="${cellW / 2}" y="${baseLocal + s * DIGIT_H}" text-anchor="middle" class="cnt">${s % 10}</text>`,
        );
      }
      const travel = steps * DIGIT_H;
      cells.push(
        `<svg x="${cx}" y="${pillY}" width="${cellW}" height="${PILL_H}"><g>${glyphs.join('')}${anim.replace('TRAVEL', String(travel))}</g></svg>`,
      );
      cx += cellW;
      digitIdx++;
    } else {
      cells.push(`<text x="${cx + commaW / 2}" y="${baseY}" text-anchor="middle" class="cnt">${ch}</text>`);
      cx += commaW;
    }
  }
  const pill = `<rect x="${pillX}" y="${pillY}" width="${pillW}" height="${PILL_H}" rx="${PILL_H / 2}" fill="${C.countBg}"/>`;
  return { svg: pill + '\n  ' + cells.join('\n  '), width: pillW };
}

function renderSvg(count: number): string {
  const countStr = count.toLocaleString('en-US');
  const iconY = (H - ICON) / 2;
  const textBaseY = H / 2 + 12 * 0.34;

  let x = PAD_L;
  const star = `<path d="${STAR_PATH}" fill="${C.text}" transform="translate(${x} ${iconY})"/>`;
  x += ICON + ICON_GAP;

  const labelX = x;
  x += LABEL_W + LABEL_GAP;

  const pillX = x;
  const { svg: pillSvg, width: pillW } = countPill(countStr, pillX, count);
  x += pillW + PILL_PAD_R;

  const width = Math.round(x);

  return `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${H}" viewBox="0 0 ${width} ${H}" role="img" aria-label="${count} GitHub stars">
  <style>
    .lbl{font:${FONT};fill:${C.text};}
    .cnt{font:${COUNT_FONT};fill:${C.text};font-variant-numeric:tabular-nums;}
  </style>
  <rect x="0.5" y="0.5" width="${width - 1}" height="${H - 1}" rx="6" fill="${C.btnBg}" stroke="${C.btnBorder}"/>
  ${star}
  <text x="${labelX}" y="${textBaseY}" class="lbl">Star</text>
  ${pillSvg}
</svg>`;
}

export async function GET() {
  const count = await getStarCount();
  const svg = renderSvg(count);
  return new Response(svg, {
    headers: {
      'Content-Type': 'image/svg+xml; charset=utf-8',
      // ~1h fresh at the edge, serve-stale-while-revalidating for a day. Keeps the count
      // approximately current without hammering the unauthenticated GitHub API.
      'Cache-Control': 'public, max-age=0, s-maxage=3600, stale-while-revalidate=86400',
    },
  });
}

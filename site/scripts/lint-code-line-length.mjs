#!/usr/bin/env node
/* Lints the RENDERED width of every fenced code line in the content tree. A
   code block scrolls horizontally the moment one line is wider than its column,
   and a scrollbar under a config example is never the right answer — so this
   flags the line while it is still being authored.

   ── Where the limits come from ────────────────────────────────────────────
   Measured in the browser against the running dev server, not guessed:

     code font      Geist Mono 12px  →  7.2px per column, exactly
     line inset     48px (16px left + 32px right, the copy-button gutter)
     docs column    594px at a 1280px viewport, the NARROWEST it ever is
     usable         594 - 48 = 546px  →  546 / 7.2 = 75.8 columns

   75 is that floor, and it governs docs and guides, which share one layout.
   The column is narrowest at exactly 1280px because that is where the
   right-hand table of contents appears while the article has not yet reached
   its 720px cap; below 1280 the table of contents is gone and the column is
   wider, above ~1340 the article caps and the column settles at 84 columns.
   Authoring to the floor is what keeps every viewport scroll-free.

   The blog has no table of contents and a wider article, so its column holds
   at 99 columns and does not dip. Its posts are a dated record rather than a
   living reference, so holding them to the docs floor would mean rewriting
   shipped transcripts to fix a scrollbar they do not have.

   To re-derive after a font, breakpoint, or padding change, run this in the
   console on a page of that area sized to 1280px wide:

     const pre = document.querySelector('pre');
     const sp = pre.querySelector('code > span');
     const c = getComputedStyle(sp);
     const pad = parseFloat(c.paddingLeft) + parseFloat(c.paddingRight);
     (pre.parentElement.clientWidth - pad) / 7.2;

   ── Why this counts columns rather than characters ────────────────────────
   A previous version counted code points, which is wrong twice over:

   1. ANSI blocks. An ```ansi fence carries escape sequences that Shiki turns
      into colour, so `\e[35m` occupies ZERO columns when rendered. Counting
      them flagged 22 lines that render well inside the column.
   2. Glyphs wider than one cell. Geist Mono has no arrow, so `→` falls back to
      a face that draws it 2.24 columns wide; the filled/hollow markers and the
      emoji fall back too. WIDE below is the measured advance of every non-ASCII
      character that appears in a fence in this tree, in columns.

   ── The `wide` opt-out ────────────────────────────────────────────────────
   Some blocks quote a transcript byte for byte, and nub wraps its own errors
   for a terminal rather than for this column — `two files would collide …` is
   one 172-column line in crates/nub-cli/src/compile/mod.rs, pinned by a test.
   Rewrapping it here would put text on the page that nub never prints. Those
   blocks take a `wide` token on the fence:

     ```console wide

   Shiki and fumadocs ignore the token, so the block renders unchanged and
   scrolls. It is ONLY for verbatim output that is not ours to rewrap — never
   for an authored example that could simply be shorter. The run prints how
   many blocks use it, so the count stays visible instead of drifting.

   Scope: fenced ``` blocks under content/. The hand-built components
   (Terminal/ShimDemo/Source) take their lines as props rather than fenced text,
   so they are not covered — keep those to the same budget by hand.

   Usage: node scripts/lint-code-line-length.mjs [--limit N]
   Exits non-zero if any line is over the limit. */

import { readFileSync, globSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const limitArg = process.argv.indexOf('--limit');
const OVERRIDE = limitArg !== -1 ? Number(process.argv[limitArg + 1]) : null;

const LIMITS = { docs: 75, guides: 75, blog: 99 };
const DEFAULT_LIMIT = 75;

/* A callout insets its body by the icon column and the panel padding — measured
   at 5.83 columns on the blog, where the only such block in the tree lives. */
const CALLOUT_INSET = 6;

function limitFor(rel) {
  if (OVERRIDE !== null) return OVERRIDE;
  const area = rel.split('/')[1];
  return LIMITS[area] ?? DEFAULT_LIMIT;
}

/* Measured advance, in columns, for every non-ASCII character used in a fence
   here. Anything absent counts as one column, which is correct for the box
   drawing, the dashes, and the arrows that do have a Geist Mono glyph. */
const WIDE = {
  '→': 2.243, '←': 2.243,
  '❌': 1.667, '✅': 1.667,
  '□': 1.355, '■': 1.355, '○': 1.355, '●': 1.355,
  '⠋': 1.139,
};

/* Only the ```ansi grammar interprets these; anywhere else the same text is
   literal and does occupy columns. */
const ANSI = /(?:\\e|\\x1b|\\u001b)\[[0-9;]*[A-Za-z]/g;

/* remark-node-version and its sibling substitute these at build time, so the
   token's own length is never what renders. The stand-ins are a digit wider
   than today's values (major 26, version 26.3.0) so the budget still holds
   after the next major rolls over. */
const TOKENS = {
  '{{NODE_MAJOR}}': '000',
  '{{NODE_VERSION}}': '000.0.0',
  '{{NUB_VERSION}}': '00.0.00',
};

function renderedColumns(line, lang) {
  let visible = lang === 'ansi' ? line.replace(ANSI, '') : line;
  for (const [token, stand] of Object.entries(TOKENS)) {
    visible = visible.split(token).join(stand);
  }
  let width = 0;
  for (const ch of visible) width += WIDE[ch] ?? 1;
  return width;
}

const files = globSync('content/**/*.{md,mdx}', { cwd: root }).sort();

let violations = 0;
let exemptBlocks = 0;
const byFile = new Map();

for (const rel of files) {
  const fileLimit = limitFor(rel);
  const lines = readFileSync(join(root, rel), 'utf8').split('\n');
  let inFence = false;
  let fenceMarker = '';
  let fenceLang = '';
  let fenceExempt = false;
  let calloutDepth = 0;
  lines.forEach((line, i) => {
    // Track <Callout>…</Callout> nesting (only meaningful outside a fence — a
    // fence body never opens a JSX component).
    if (!inFence) {
      if (/<Callout(\s|>)/.test(line)) calloutDepth++;
      if (/<\/Callout>/.test(line)) calloutDepth = Math.max(0, calloutDepth - 1);
    }
    const fence = line.match(/^(\s*)(`{3,}|~{3,})(.*)$/);
    if (fence) {
      const marker = fence[2][0];
      if (!inFence) {
        const info = fence[3].trim().split(/\s+/);
        inFence = true;
        fenceMarker = marker;
        fenceLang = (info[0] ?? '').toLowerCase();
        fenceExempt = info.slice(1).includes('wide');
        if (fenceExempt) exemptBlocks++;
        return; // the opening fence line itself isn't content
      }
      // a fence line while open: close only if the marker family matches
      if (marker === fenceMarker) {
        inFence = false;
        fenceLang = '';
        fenceExempt = false;
        return;
      }
    }
    if (!inFence || fenceExempt) return;
    const width = renderedColumns(line, fenceLang);
    const limit = calloutDepth > 0 ? fileLimit - CALLOUT_INSET : fileLimit;
    if (width > limit) {
      violations++;
      if (!byFile.has(rel)) byFile.set(rel, []);
      byFile.get(rel).push({ line: i + 1, width, limit, text: line });
    }
  });
}

const budget = OVERRIDE !== null
  ? `${OVERRIDE} columns`
  : Object.entries(LIMITS).map(([area, n]) => `${area} ${n}`).join(', ');

const exempt = exemptBlocks === 0 ? '' : `; ${exemptBlocks} block(s) marked wide`;

if (violations === 0) {
  console.log(`✓ code line-length: every fenced line renders within its column (${budget}${exempt})`);
  process.exit(0);
}

console.error(`✗ code line-length: ${violations} line(s) wider than their column (${budget})\n`);
for (const [file, rows] of byFile) {
  console.error(`  ${file}`);
  for (const r of rows) {
    const width = Number.isInteger(r.width) ? r.width : r.width.toFixed(1);
    console.error(`    ${r.line}:  ${width} cols (>${r.limit})  ${JSON.stringify(r.text)}`);
  }
  console.error('');
}
process.exit(1);

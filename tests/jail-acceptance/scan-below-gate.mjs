// What still carries an install script BELOW the population's download gate?
//
// ⛔⛔ WHY THIS EXISTS. The sweep population is drawn from the npm top-downloaded set, which is the
// packages above 100,000 weekly downloads -- the default `--threshold` in
// scripts/npm-install-script-census.ts. So the coverage figure that population reports is measured
// against the set it was drawn from: it reads 100% wherever the gate sits, and it cannot see one
// package below it. This script is the instrument that looks below, so that limitation has a
// measured size instead of being an unknown.
//
// Measured 2026-09-05 over ranks ~25,500-70,000: 720 carriers holding 19.9M weekly downloads, about
// 3.5% of the weight the population covers. Two independent runs agreed on every count.
//
// ⛔ THE CONTROL IS NOT OPTIONAL. A registry sweep of this size fails by UNDER-REPORTING -- a
// throttled or timed-out lookup looks exactly like "this package has no install script", so a broken
// run returns a small number and reads like good news. A prior scan reported 57 carriers against a
// known 87 while its own two-package control passed. This one refuses to print a result unless five
// packages known to carry install scripts are all detected, and it counts unresolved lookups
// separately rather than folding them into "no script".
//
// ⛔ EVERY fetch carries a hard timeout. Without one a single hung socket parks a worker forever and
// the whole scan stalls silently -- measured: a 44k run froze at 5,000 checked for 40+ minutes while
// the registry answered an unrelated probe in 0.3s.
//
// Usage: node scan-below-gate.mjs --out <tsv> [--from-page 256] [--to-page 700] [--rank-cache <json>]
//   Ranking comes from ecosyste.ms (a complete registry enumeration, not a relevance search);
//   script presence comes from each package's own registry `latest` manifest. Pass --rank-cache to
//   reuse a ranking between runs, which also pins the input so a re-run is comparable.

import fs from 'node:fs';

const argv = process.argv.slice(2);
const arg = (n, d) => { const i = argv.indexOf(n); return i === -1 ? d : argv[i + 1]; };
const OUT = arg('--out');
const FROM = +arg('--from-page', 256), TO = +arg('--to-page', 700), PER = 100, CONC = 4;
const RANK_CACHE = arg('--rank-cache');
if (!OUT) { console.error('scan-below-gate: --out <tsv> is required'); process.exit(2); }

const RUN_KEYS = ['preinstall', 'install', 'postinstall'];
const hooks = (s) => !!s && RUN_KEYS.some((k) => typeof s[k] === 'string' && s[k].trim());

const get = async (u) => {
  for (let a = 0; a < 4; a++) {
    try {
      const r = await fetch(u, { signal: AbortSignal.timeout(12000) });
      if (r.status === 404) return { __404: true };
      const t = await r.text();
      // An HTML body is a throttle or an error page, never a manifest. Back off; do not parse.
      if (t.trim().startsWith('<')) { await new Promise((s) => setTimeout(s, 400 * (a + 1))); continue; }
      return JSON.parse(t);
    } catch { await new Promise((s) => setTimeout(s, 400 * (a + 1))); }
  }
  return null;
};

let pkgs;
if (RANK_CACHE && fs.existsSync(RANK_CACHE)) {
  pkgs = JSON.parse(fs.readFileSync(RANK_CACHE, 'utf8'));
  console.error(`ranking from cache: ${pkgs.length}`);
} else {
  pkgs = [];
  for (let p = FROM; p <= TO; p++) {
    const j = await get(`https://packages.ecosyste.ms/api/v1/registries/npmjs.org/packages?sort=downloads&order=desc&per_page=${PER}&page=${p}`);
    if (Array.isArray(j)) for (const x of j) pkgs.push({ name: x.name, monthly: x.downloads });
    if (p % 100 === 0) console.error(`  ranked page ${p} (${pkgs.length})`);
    await new Promise((s) => setTimeout(s, 80));
  }
  if (RANK_CACHE) fs.writeFileSync(RANK_CACHE, JSON.stringify(pkgs));
  console.error(`ranking done: ${pkgs.length}`);
}

const CONTROL = ['esbuild', 'core-js', 'bufferutil', 'cpu-features', 'nodejieba'];
let ctlOk = 0;
for (const n of CONTROL) {
  const d = await get(`https://registry.npmjs.org/${encodeURIComponent(n)}/latest`);
  const hit = !!(d && hooks(d.scripts));
  console.error(`  control ${n}: ${hit}`);
  if (hit) ctlOk++;
}
if (ctlOk !== CONTROL.length) {
  console.error(`control ${ctlOk}/${CONTROL.length} — the scan would under-report; refusing to produce a result`);
  process.exit(4);
}

const out = fs.createWriteStream(OUT);
let checked = 0, carriers = 0, unresolved = 0, gone = 0, i = 0;
const worker = async () => {
  while (i < pkgs.length) {
    const s = pkgs[i++];
    const d = await get(`https://registry.npmjs.org/${encodeURIComponent(s.name).replace('%40', '@')}/latest`);
    if (d && d.__404) { gone++; continue; }
    // Counted separately, never as "no script" — that conflation is how a throttled run reads clean.
    if (!d || !d.version) { unresolved++; continue; }
    checked++;
    if (hooks(d.scripts)) { carriers++; out.write(`${s.name}\t${Math.round(s.monthly / 4.33)}\n`); }
    if (checked % 10000 === 0) console.error(`  checked ${checked}, carriers ${carriers}, unresolved ${unresolved}`);
    await new Promise((r) => setTimeout(r, 50));
  }
};
await Promise.all(Array.from({ length: CONC }, worker));
out.end();
console.log(`checked ${checked} | unresolved ${unresolved} | 404 ${gone} | carriers ${carriers} -> ${OUT}`);

// What still carries an install script BELOW the population's download gate?
//
// ⛔⛔ WHY THIS EXISTS. The sweep population is drawn from the npm top-downloaded set, which is the
// packages above 100,000 weekly downloads -- the default `--threshold` in
// scripts/npm-install-script-census.ts. So the coverage figure that population reports is measured
// against the set it was drawn from: it reads 100% wherever the gate sits, and it cannot see one
// package below it. This script is the instrument that looks below, so that limitation has a
// measured size instead of being an unknown.
//
// Measured 2026-09-05 over ranks ~25,500-70,000: 678 carriers holding 18.4M weekly downloads, about
// 2.0% of the weight the population covers.
//
// ⛔ THE CONTROL IS NOT OPTIONAL. A registry sweep of this size fails by UNDER-REPORTING -- a
// throttled or timed-out lookup looks exactly like "this package has no install script", so a broken
// run returns a small number and reads like good news. A prior scan reported 57 carriers against a
// known 87 while its own two-package control passed. This one refuses to print a result unless five
// packages known to carry install scripts are all detected, and it counts unresolved lookups
// separately rather than folding them into "no script".
//
// ⛔⛔ IT JUDGES A PACKAGE BY THE VERSION PEOPLE INSTALL, NOT BY `latest`. Downloads pile up on the
// TERMINAL release of each major line, because that is where lockfiles pin, so a package whose current
// release dropped its install script can still run one for most of its users. This scan used to read
// `latest` only and reported 720 carriers over this band; the version-aware rule reports 678. The
// totals hide how far apart the two answers are: only 575 packages are in both. 103 run a script ONLY
// on a non-latest version and were invisible before, and 145 that the latest-only run counted carry it
// on a release almost nobody installs. So the old number was not merely low -- a fifth of it was
// wrong in each direction. The rule itself is pickInstalledVersion in discover-install-scripts.mjs,
// shared so both bands are measured by one instrument rather than two that disagree.
//
// ⛔ THE COUNT IS STILL A FLOOR. Unresolved lookups are counted separately rather than folded into
// "no script", and 696 of 44,055 did not resolve on the recorded run, so a package's ABSENCE from the
// output is not evidence it never runs an install script.
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
import { pickInstalledVersion } from './discover-install-scripts.mjs';

const argv = process.argv.slice(2);
const arg = (n, d) => { const i = argv.indexOf(n); return i === -1 ? d : argv[i + 1]; };
const OUT = arg('--out');
const FROM = +arg('--from-page', 256), TO = +arg('--to-page', 700), PER = 100, CONC = 4;
const RANK_CACHE = arg('--rank-cache');
if (!OUT) { console.error('scan-below-gate: --out <tsv> is required'); process.exit(2); }

// A scoped name is one path segment, so the `/` must be escaped but the `@` must not.
const enc = (n) => encodeURIComponent(n).replace('%40', '@');
const CORGI = 'application/vnd.npm.install-v1+json';

const get = async (u, accept) => {
  for (let a = 0; a < 4; a++) {
    try {
      const r = await fetch(u, { headers: accept ? { accept } : undefined, signal: AbortSignal.timeout(12000) });
      if (r.status === 404) return { __404: true };
      // A 429 body can be valid JSON, so status has to be checked BEFORE parsing — otherwise the
      // throttle response is parsed as a manifest, `versions` is absent, and the package is silently
      // counted as unresolved-or-clean depending on which field the caller reads first.
      if (r.status === 429) {
        const wait = Math.min(30000, (Number(r.headers.get('retry-after')) || 5) * 1000);
        await new Promise((s) => setTimeout(s, wait));
        continue;
      }
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

// ⛔ THE CONTROL DOES TWO JOBS, AND THE SECOND IS THE NEW ONE. Four names prove the scan is not
// throttled into silence. `sharp` proves it is VERSION-AWARE: it carries no install script on its
// current release and one on 0.34.5, so a latest-only scan scores it a clean negative. If the sharp
// row does not come back true, this instrument has silently reverted to the method it replaced.
const CONTROL = [['esbuild', true], ['core-js', true], ['bufferutil', true], ['nodejieba', true], ['sharp', true]];
let ctlOk = 0;
for (const [n, want] of CONTROL) {
  // Exercise the SAME path the worker takes, so a revert to a latest-only read fails here rather
  // than passing a control that asks an easier question than the scan does.
  const p = await get(`https://registry.npmjs.org/${enc(n)}`, CORGI);
  const d = await get(`https://api.npmjs.org/versions/${enc(n)}/last-week`);
  const pick = p?.versions && d?.downloads ? pickInstalledVersion(p.versions, d.downloads) : null;
  const onLatest = !!p?.versions?.[p?.['dist-tags']?.latest]?.hasInstallScript;
  console.error(`  control ${n}: pick=${pick ?? 'none'} onLatest=${onLatest} (want carrier=${want})`);
  if (!!pick === want) ctlOk++;
  if (n === 'sharp' && onLatest) {
    // The premise of the sharp control has changed rather than the scan being wrong. Say so instead
    // of reporting a pass that no longer discriminates between the two methods.
    console.error('  ⛔ sharp now carries an install script on `latest`, so it no longer proves version-awareness — pick a new witness');
    process.exit(4);
  }
}
if (ctlOk !== CONTROL.length) {
  console.error(`control ${ctlOk}/${CONTROL.length} — the scan would under-report; refusing to produce a result`);
  process.exit(4);
}

const out = fs.createWriteStream(OUT);
let checked = 0, carriers = 0, unresolved = 0, gone = 0, latestBlind = 0, i = 0;
const worker = async () => {
  while (i < pkgs.length) {
    const s = pkgs[i++];
    // The abbreviated packument carries `hasInstallScript` per version, so a package that has never
    // carried a script settles in ONE request. Only the rest pay for the per-version download call,
    // which is what keeps a version-aware scan of 44,000 packages the same order of cost as a
    // latest-only one.
    const p = await get(`https://registry.npmjs.org/${enc(s.name)}`, CORGI);
    if (p && p.__404) { gone++; continue; }
    // Counted separately, never as "no script" — that conflation is how a throttled run reads clean.
    if (!p || !p.versions) { unresolved++; continue; }
    checked++;
    if (!Object.values(p.versions).some((e) => e.hasInstallScript)) {
      await new Promise((r) => setTimeout(r, 50));
      continue;
    }
    const dl = await get(`https://api.npmjs.org/versions/${enc(s.name)}/last-week`);
    const downloads = dl && !dl.__404 ? (dl.downloads ?? null) : null;
    if (!downloads || !Object.keys(downloads).length) { unresolved++; continue; }
    const pick = pickInstalledVersion(p.versions, downloads);
    if (pick) {
      carriers++;
      const latest = p['dist-tags']?.latest;
      const onLatest = !!(latest && p.versions[latest]?.hasInstallScript);
      if (!onLatest) latestBlind++;
      out.write(`${s.name}\t${Math.round(s.monthly / 4.33)}\t${onLatest ? 'latest' : 'older-version-only'}\n`);
    }
    if (checked % 10000 === 0) console.error(`  checked ${checked}, carriers ${carriers}, unresolved ${unresolved}`);
    await new Promise((r) => setTimeout(r, 50));
  }
};
await Promise.all(Array.from({ length: CONC }, worker));
out.end();
console.log(`checked ${checked} | unresolved ${unresolved} | 404 ${gone} | carriers ${carriers} -> ${OUT}`);
console.log(`of those, ${latestBlind} run a script only on a non-latest version, so a latest-only scan misses them`);

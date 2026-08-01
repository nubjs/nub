// Turn a directory of per-package run records into a v2 build-jail catalog.
//
// The catalog is COLLATED from measurements, never edited in place. One record per
// package@version means a single package can be re-measured without disturbing anything else,
// and this step is pure: it reads records and writes a catalog, so it can be re-run at any time
// and produces the same file from the same inputs.
//
// Usage:
//   node collate.mjs [--runs <dir>] [--baseline <file>] [--out <file>] [--platform <p>]
//
// `--baseline` names a JSON file carrying the `baseline` and `env` arrays. Those are NOT
// measured — they are the floor every jailed script gets — so they are authored once and merged
// in here rather than being derived from any package's record.

import fs from 'node:fs';
import path from 'node:path';

const argv = process.argv.slice(2);
const opt = (name, dflt) => (argv.includes(name) ? argv[argv.indexOf(name) + 1] : dflt);
const here = new URL('.', import.meta.url).pathname;

const RUNS = opt('--runs', path.join(here, 'results', 'runs'));
const BASELINE = opt('--baseline', path.join(here, 'baseline.json'));
const OUT = opt('--out', path.join(here, 'results', 'catalog-v2.json'));
const PLATFORM = opt('--platform', null);

// ── read ──────────────────────────────────────────────────────────────────────

const records = [];
for (const f of fs.readdirSync(RUNS).filter((f) => f.endsWith('.json')).sort()) {
  try { records.push({ file: f, ...JSON.parse(fs.readFileSync(path.join(RUNS, f), 'utf8')) }); }
  catch (e) { console.error(`  SKIP ${f}: ${e.message}`); }
}

// PROVENANCE IS A GATE, NOT A FOOTNOTE. A results directory silently mixes methodologies when
// records span harness revisions — a filter change once moved nine packages between grants with
// no binary change at all. Collating across two harnesses produces a catalog no single
// experiment ever produced, so the mix is reported and the majority hash named.
const harnessHashes = {};
for (const r of records) {
  const h = r.provenance?.harnessSha256 ?? 'unknown';
  harnessHashes[h] = (harnessHashes[h] ?? 0) + 1;
}
const platforms = new Set(records.map((r) => r.provenance?.platform).filter(Boolean));

// ── group by package ──────────────────────────────────────────────────────────

const byPackage = new Map();
const excluded = { noVerdict: [], broken: [], harnessError: [], noStatePassed: [] };

for (const r of records) {
  if (r.verdict === 'BROKEN-EVEN-WITH-EVERYTHING') { excluded.broken.push(`${r.pkg}@${r.version}`); continue; }
  if (r.verdict === 'HARNESS-ERROR') { excluded.harnessError.push(`${r.pkg}@${r.version}`); continue; }
  if (r.verdict === 'NO-STATE-PASSED') { excluded.noStatePassed.push(`${r.pkg}@${r.version}`); continue; }
  if (r.verdict !== 'MINIMUM') { excluded.noVerdict.push(`${r.pkg}@${r.version}`); continue; }
  if (!byPackage.has(r.pkg)) byPackage.set(r.pkg, []);
  byPackage.get(r.pkg).push(r);
}

/** A grant's identity for banding: two versions share a band iff their capabilities AND their
 *  declared writes are identical. Key order is normalised so two equal grants never differ by
 *  serialisation alone. */
function grantKey(r) {
  const g = r.grant ?? null;
  const norm = (x) => {
    if (x === null || typeof x !== 'object') return x;
    if (Array.isArray(x)) return x.map(norm);
    return Object.fromEntries(Object.keys(x).sort().map((k) => [k, norm(x[k])]));
  };
  return JSON.stringify({ g: norm(g), w: (r.writePaths ?? []).slice().sort() });
}

// ── build ─────────────────────────────────────────────────────────────────────

const packages = {};
const notes = [];

for (const [pkg, rs] of [...byPackage.entries()].sort()) {
  const bands = new Map();
  for (const r of rs) {
    const k = grantKey(r);
    if (!bands.has(k)) bands.set(k, []);
    bands.get(k).push(r);
  }

  // A package needing NOTHING at every measured version earns no entry at all: the base profile
  // is the default, and an empty grant is rejected by the parser rather than being a spelling of
  // "nothing". This is why most of the corpus is absent from the catalog rather than present
  // with an empty entry.
  const meaningful = [...bands.entries()].filter(([, group]) =>
    group[0].grant || (group[0].writePaths ?? []).length);
  if (!meaningful.length) continue;

  const grants = [];
  for (const [, group] of meaningful) {
    const g = { ...(group[0].grant ?? {}) };
    const wp = group[0].writePaths ?? [];
    if (wp.length) g.writePaths = wp;

    // VERSIONS ARE ONLY PINNED WHEN THE MEASUREMENTS DISAGREE. A package measured at one version
    // gets a matcher-less grant, because pinning it to that one version would silently deny every
    // other version the grant it was never measured at -- and install scripts concentrate in OLD
    // pins. Where bands genuinely differ the versions are named, and the widest band is placed
    // last so it acts as the fallback under first-match-wins.
    if (meaningful.length > 1) {
      g.versions = group.map((r) => r.version).sort().join(' || ');
    }
    if (PLATFORM) g.platforms = [PLATFORM];
    g.notes = `measured: ${group.map((r) => `${r.version}`).sort().join(', ')} -> ${group[0].state}`;

    const unmeasured = group.flatMap((r) => r.unmeasuredScopesGranted ?? []);
    if (unmeasured.length) {
      g.notes += `; widened for unmeasured scopes (${[...new Set(unmeasured)].join(', ')})`;
      notes.push(`${pkg}: widened for ${[...new Set(unmeasured)].join(', ')}`);
    }
    // A verdict that could not see the project axis is recorded, because "did not need project"
    // and "was never placed where it could try" are byte-identical outcomes.
    if (group.some((r) => r.declaresInstallScript && !r.projectAxisConclusive)) {
      g.notes += '; project axis inconclusive (package was not materialized)';
    }
    grants.push({ group, g });
  }

  // First match wins, so a matcher-less grant must be LAST or it shadows everything after it.
  grants.sort((a, b) => (a.g.versions ? 0 : 1) - (b.g.versions ? 0 : 1));
  packages[pkg] = grants.map(({ g }) => g);
}

const baseline = fs.existsSync(BASELINE)
  ? JSON.parse(fs.readFileSync(BASELINE, 'utf8'))
  : { baseline: [], env: [] };

const catalog = { packages, baseline: baseline.baseline ?? [], env: baseline.env ?? [] };
fs.mkdirSync(path.dirname(OUT), { recursive: true });
fs.writeFileSync(OUT, `${JSON.stringify(catalog, null, 2)}\n`);

// ── report ────────────────────────────────────────────────────────────────────

const grantCount = Object.values(packages).reduce((n, g) => n + g.length, 0);
console.log(`records read        ${records.length}`);
console.log(`platforms           ${[...platforms].join(', ') || '(none recorded)'}`);
const hh = Object.entries(harnessHashes).sort((a, b) => b[1] - a[1]);
console.log(`harness revisions   ${hh.map(([h, n]) => `${h}:${n}`).join('  ')}`);
if (hh.length > 1) console.log(`  ⚠ RECORDS SPAN ${hh.length} HARNESS REVISIONS — re-run the minority under ${hh[0][0]} before shipping this catalog`);
console.log(`packages with entry ${Object.keys(packages).length}`);
console.log(`grants emitted      ${grantCount}`);
console.log(`needed nothing      ${byPackage.size - Object.keys(packages).length}`);
for (const [k, v] of Object.entries(excluded)) {
  if (v.length) console.log(`excluded (${k})  ${v.length}: ${v.slice(0, 6).join(', ')}${v.length > 6 ? ' …' : ''}`);
}
for (const n of notes) console.log(`  note: ${n}`);
console.log(`\nwrote ${OUT}`);

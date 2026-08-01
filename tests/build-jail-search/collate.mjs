// Turn a directory of per-package run records into a v2 build-jail catalog.
//
// The catalog is COLLATED from measurements, never edited in place. One record per
// package@version means a single package can be re-measured without disturbing anything else,
// and this step is pure: it reads records and writes a catalog, so it can be re-run at any time
// and produces the same file from the same inputs.
//
// Usage:
//   node collate.mjs [--runs <dir>] [--baseline <file>] [--out <file>] [--platform <p>]
//                    [--only-platform <p>]   keep only records whose PROVENANCE names <p>
//
// `--baseline` names a JSON file carrying the `baseline` and `env` arrays. Those are NOT
// measured — they are the floor every jailed script gets — so they are authored once and merged
// in here rather than being derived from any package's record.

import fs from 'node:fs';
import path from 'node:path';

const argv = process.argv.slice(2);
const opt = (name, dflt) => (argv.includes(name) ? argv[argv.indexOf(name) + 1] : dflt);
const here = new URL('.', import.meta.url).pathname;

// Several --runs may be given: the catalog is reconciled from runs on different machines and
// operating systems, so merging result sets is the normal case, not an edge one.
const RUNS_DIRS = argv.reduce((acc, a, i) => (a === '--runs' ? [...acc, argv[i + 1]] : acc), []);
const RUNS = RUNS_DIRS.length ? RUNS_DIRS : [path.join(here, 'results', 'runs')];
const PLATFORM_FILTER = opt('--only-platform', null);
const BASELINE = opt('--baseline', path.join(here, 'baseline.json'));
// OUTSIDE `results/`, which is gitignored. The raw run records and per-cell logs are
// regenerable measurements and large — ~236K per package, so ~127 MB for a 550-package corpus
// on one platform and ~380 MB across three — but the COLLATED CATALOG is the deliverable and
// belongs in the repo. Defaulting it into the ignored tree would have quietly kept the one
// artefact that matters out of version control.
const OUT = opt('--out', path.join(here, 'catalog-v2.json'));
const PLATFORM = opt('--platform', null);
const OVERRIDES = opt('--overrides', path.join(here, 'overrides'));

// ── read ──────────────────────────────────────────────────────────────────────

const records = [];
/** Every record under `dir`, at any depth. Records are partitioned
 *  `<platform>/<package>/<version>/results.json`, so this walks rather than lists — and it still
 *  reads a FLAT directory, which keeps older result sets collatable. */
function walk(dir) {
  const out = [];
  let entries;
  try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return out; }
  for (const e of entries.sort((a, b) => a.name.localeCompare(b.name))) {
    const full = path.join(dir, e.name);
    if (e.isDirectory()) out.push(...walk(full));
    // ONLY `results.json`. A version directory holds the record plus every cell's log, and
    // a future artefact dropped beside them must not silently become a record.
    else if (e.name === 'results.json') out.push(full);
  }
  return out;
}

for (const [root, full] of RUNS.flatMap((d) => walk(d).map((f) => [d, f]))) {
  const f = path.relative(root, full);
  try {
    const rec = { file: f, ...JSON.parse(fs.readFileSync(full, 'utf8')) };
    // The platform is the top directory level, but the record's own provenance is the
    // authority — a file moved between directories must not silently change platform.
    if (PLATFORM_FILTER && rec.provenance?.platform !== PLATFORM_FILTER) continue;
    records.push(rec);
  } catch (e) { console.error(`  SKIP ${f}: ${e.message}`); }
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

/** The catalog names platforms `macos | linux | windows`; provenance records them as
 *  `darwin-arm64`, `linux-x64`, `win32-x64`. Map once, here, so nothing downstream has to
 *  know both vocabularies. Architecture is deliberately dropped: the grant model has no
 *  per-arch axis, and a run on arm64 speaks for the OS. */
function osOf(r) {
  const p = r.provenance?.platform ?? '';
  if (p.startsWith('darwin')) return 'macos';
  if (p.startsWith('linux')) return 'linux';
  if (p.startsWith('win')) return 'windows';
  return null;
}

// ── build ─────────────────────────────────────────────────────────────────────

const packages = {};
const notes = [];

/** Merge two grants by UNION — the wider of each axis.
 *
 *  Reconciliation across machines is where this matters. A package that probes for host tooling
 *  takes different code paths on different hosts, so one run measures narrower than another;
 *  `sharp` is the worked example, needing full disk write only on the branch that shells out to
 *  brew. The wider grant covers both branches, and the narrower one BREAKS for every user whose
 *  machine takes the richer path. Over-granting is the failure this project accepts; under-
 *  granting is the one it does not. So: never intersect. */
function unionGrant(a, b) {
  if (!a) return b;
  if (!b) return a;
  const out = { ...a };
  const widest = (x, y) => {
    if (x === 'disk' || y === 'disk') return 'disk';
    if (!x) return y;
    if (!y) return x;
    return { ...x, ...y };
  };
  if (a.write || b.write) out.write = widest(a.write, b.write);
  if (a.read || b.read) out.read = widest(a.read, b.read);
  if (a.network || b.network) out.network = true;
  // A read the widened write now covers is rejected by the parser, so drop it.
  if (out.read && out.write === 'disk') delete out.read;
  else if (out.read && typeof out.read === 'object' && typeof out.write === 'object') {
    const r = { ...out.read };
    for (const k of Object.keys(out.write)) delete r[k];
    if (Object.keys(r).length) out.read = r; else delete out.read;
  }
  const wp = [...new Set([...(a.writePaths ?? []), ...(b.writePaths ?? [])])].sort();
  if (wp.length) out.writePaths = wp.filter((d) => !wp.some((o) => o !== d && d.startsWith(`${o}/`)));
  return out;
}

for (const [pkg, rsRaw] of [...byPackage.entries()].sort()) {
  // RECONCILE FIRST: several machines may have measured the SAME platform and version. Fold
  // those into one record per (platform, version) by UNION before banding, so a host that took
  // a narrower code path cannot erase a grant a richer host proved necessary.
  const folded = new Map();
  for (const r of rsRaw) {
    const key = `${r.provenance?.platform ?? '?'}\u0000${r.version}`;
    const prev = folded.get(key);
    if (!prev) { folded.set(key, r); continue; }
    folded.set(key, {
      ...prev,
      grant: unionGrant(prev.grant, r.grant),
      writePaths: [...new Set([...(prev.writePaths ?? []), ...(r.writePaths ?? [])])].sort(),
      _mergedFrom: (prev._mergedFrom ?? 1) + 1,
    });
  }
  const rs = [...folded.values()];
  for (const r of rs) {
    if (r._mergedFrom) notes.push(`${pkg}@${r.version}: reconciled ${r._mergedFrom} runs by union`);
  }
  const allPlatforms = new Set(rs.map((r) => osOf(r)).filter(Boolean));
  const allVersions = new Set(rs.map((r) => r.version));
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
    // VERSIONS ARE PINNED ONLY WHEN THE VERSIONS ARE WHAT DIFFER. Two bands that differ by
    // PLATFORM, at the same version, must not both carry a `versions` matcher — that reads as a
    // version-specific rule and is noise. Compare the version sets across bands: pin only when
    // this band's versions are not the whole set.
    const bandVersions = [...new Set(group.map((r) => r.version))].sort();
    // A writePaths entry embedding the measured version names a directory that moves on the next
    // release, so the grant MUST carry a versions matcher even when nothing else distinguishes
    // this band — otherwise it ships matcher-less and silently stops matching.
    const pinned = [...new Set(group.flatMap((r) => r.writePathsVersionPinned ?? []))];
    if (bandVersions.length < allVersions.size || pinned.length) {
      g.versions = bandVersions.join(' || ');
    }
    if (pinned.length) {
      g.notes = `${g.notes ?? ''}; version-pinned writePaths (${pinned.join(', ')}) — re-measure on a new release`;
      notes.push(`${pkg}: writePaths pinned to ${bandVersions.join(', ')} by ${pinned.join(', ')}`);
    }
    // PER-PLATFORM MATCHERS, ONLY WHEN THE PLATFORMS DISAGREE.
    //
    // Where every measured platform reached the same grant — the common case — the entry is
    // matcher-less and one line covers all three. A `platforms` key is emitted only when this
    // band genuinely does not span every platform that measured the package, so the catalog
    // does not fill with redundant per-OS duplicates of an identical grant.
    const bandPlatforms = [...new Set(group.map((r) => osOf(r)).filter(Boolean))];
    if (bandPlatforms.length && bandPlatforms.length < allPlatforms.size) {
      g.platforms = bandPlatforms.sort();
    } else if (PLATFORM) {
      g.platforms = [PLATFORM];
    }
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

// ── overrides ─────────────────────────────────────────────────────────────────
//
// A hand-authored grant REPLACES the measured one. This is the seam for a package a sweep
// cannot answer — sharp, where the honest move is to read the source rather than infer from a
// run whose result depended on whether a download succeeded.
//
// EVERY override is reported, every run. An override that applies silently is how a catalog
// stops reflecting measurement without anyone noticing, and the whole value of this pipeline is
// that its output is derived rather than asserted.
const applied = [];
const deadWeight = [];
const rejected = [];
for (const f of (fs.existsSync(OVERRIDES) ? fs.readdirSync(OVERRIDES) : []).sort()) {
  if (!f.endsWith('.json')) continue;
  let o;
  try { o = JSON.parse(fs.readFileSync(path.join(OVERRIDES, f), 'utf8')); }
  catch (e) { rejected.push(`${f}: unparseable (${e.message})`); continue; }
  const name = o.package ?? f.replace(/\.json$/, '').replace('+', '/');
  // RATIONALE IS MANDATORY. An override without one is indistinguishable from a guess a year
  // later, and the reader has no measurement to fall back on — that is the point of the file.
  const r = o.rationale ?? {};
  const missing = ['investigator', 'evidence', 'date'].filter((k) => !r[k]);
  if (missing.length) { rejected.push(`${f}: missing rationale.${missing.join(', rationale.')}`); continue; }
  if (!Array.isArray(o.grants) || !o.grants.length) { rejected.push(`${f}: no grants`); continue; }
  // COMPARE CAPABILITIES, NOT THE WHOLE GRANT. `notes` always differs — the measured note
  // records what was observed, the override's records why a human wrote it — so comparing
  // serialised grants made this check STRUCTURALLY UNABLE TO FIRE. Verified by a fixture whose
  // override matched the measured result exactly and was still not reported.
  const caps = (gs) => JSON.stringify((gs ?? []).map(({ notes, ...rest }) => {
    const o = {};
    for (const k of Object.keys(rest).sort()) o[k] = rest[k];
    return o;
  }));
  const before = packages[name] ? caps(packages[name]) : null;
  packages[name] = o.grants;
  if (before && before === caps(o.grants)) deadWeight.push(name);
  applied.push({ name, why: r.evidence, by: r.investigator, on: r.date });
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
if (applied.length) {
  console.log(`\noverrides applied   ${applied.length}`);
  for (const a of applied) console.log(`  ${a.name}  — ${a.by}, ${a.on}: ${a.why}`);
}
for (const d of deadWeight) console.log(`  ⚠ ${d}: override MATCHES the measured result — prune it`);
for (const r of rejected) console.log(`  ⛔ REJECTED ${r}`);
console.log(`\nwrote ${OUT}`);

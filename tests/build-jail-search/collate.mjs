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
import { fileURLToPath } from 'node:url';

const argv = process.argv.slice(2);
const opt = (name, dflt) => (argv.includes(name) ? argv[argv.indexOf(name) + 1] : dflt);
// `new URL(...).pathname` yields `/D:/...` on Windows, which resolves to `D:\D:\...`.
// fileURLToPath is the only correct conversion; identical on POSIX.
const here = path.dirname(fileURLToPath(import.meta.url));

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
const excluded = {
  noVerdict: [], broken: [], harnessError: [], noStatePassed: [], refusedMalicious: [],
  brokenWithoutJailToo: [], brokenInEnvironment: [],
};

for (const r of records) {
  // ⛔ ITS OWN BUCKET. A package that fails IDENTICALLY with the jail off is not evidence about the
  // jail at all — it is a nub PM/linker or packaging bug. Counting it under `broken` inflates the
  // jail's apparent failure rate, which is the number that decides whether the jail can ship.
  // MEASURED: three @pulumi/* records were `node-pre-gyp: not found` from a missing `.bin` shim,
  // reproducing with the jail disabled.
  if (r.verdict === 'BROKEN-WITHOUT-JAIL-TOO') {
    excluded.brokenWithoutJailToo.push(`${r.pkg}@${r.version}`);
    continue;
  }
  if (r.verdict === 'BROKEN-EVEN-WITH-EVERYTHING') { excluded.broken.push(`${r.pkg}@${r.version}`); continue; }
  // ⛔ EVERY `HARNESS-*`, not just `HARNESS-ERROR`. This matched the one spelling, so `HARNESS-CRASH`
  // and `HARNESS-TIMEOUT` — the two the batch driver actually emits — fell through to `noVerdict`.
  // Instrument failures hidden in a generic bucket are the ones most worth surfacing: they mean a
  // package produced NO measurement, so coverage is overstated by exactly that count.
  if (String(r.verdict ?? '').startsWith('HARNESS-')) {
    excluded.harnessError.push(`${r.pkg}@${r.version} [${r.verdict}]`);
    continue;
  }
  // Its OWN bucket for the same reason REFUSED-MALICIOUS has one: this is a DELIBERATE, verified
  // answer — the package fails here and a reference PM fails identically — so reporting it as "no
  // verdict" invites re-investigating packages that are already correctly classified. MEASURED: 51
  // records sat in `noVerdict` on one box and most were this.
  if (r.verdict === 'BROKEN-IN-ENVIRONMENT') {
    excluded.brokenInEnvironment.push(`${r.pkg}@${r.version}`);
    continue;
  }
  if (r.verdict === 'NO-STATE-PASSED') { excluded.noStatePassed.push(`${r.pkg}@${r.version}`); continue; }
  // Its OWN bucket, not `noVerdict`. The OSV screen refusing a MAL-* package is a deliberate
  // answer and the screen working as designed — reporting it as "no verdict" invites someone to
  // go re-investigate a package that is refused on purpose. It is excluded from the catalog
  // either way (a refused package never installs, so no grant is meaningful), but the REPORT
  // should say which of those two things happened.
  if (r.verdict === 'REFUSED-MALICIOUS') { excluded.refusedMalicious.push(`${r.pkg}@${r.version}`); continue; }
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

/** Semver ordering, enough for catalog banding: numeric release triple, and a prerelease sorts
 *  BELOW its release (1.0.0-rc.1 < 1.0.0) per semver §11. Build metadata is ignored. This is a
 *  comparator, not a range parser -- the catalog's only range form is `<X`, which the Rust side
 *  resolves; here we just need to order measured versions. */
function cmpVer(a, b) {
  const split = (v) => {
    const [core, pre] = String(v).split('+')[0].split('-');
    return [core.split('.').map((n) => parseInt(n, 10) || 0), pre ?? null];
  };
  const [ac, ap] = split(a);
  const [bc, bp] = split(b);
  for (let i = 0; i < 3; i++) if ((ac[i] ?? 0) !== (bc[i] ?? 0)) return (ac[i] ?? 0) - (bc[i] ?? 0);
  if (ap === bp) return 0;
  if (ap === null) return 1;        // release outranks its own prerelease
  if (bp === null) return -1;
  return ap < bp ? -1 : 1;
}

/** Capability identity, ignoring prose. Two grants are the same grant iff this matches. */
function capsKey(g) {
  const norm = (x) => {
    if (x === null || typeof x !== 'object') return x;
    if (Array.isArray(x)) return x.slice().sort().map(norm);
    return Object.fromEntries(Object.keys(x).filter((k) => k !== 'notes').sort()
      .map((k) => [k, norm(x[k])]));
  };
  return JSON.stringify(norm(g ?? {}));
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
/** Packages whose `default` was generated from something other than npm's real `latest`. A GATE,
 *  not a note — see the comment at the assignment site. */
const staleDefaults = [];
/** Packages whose records predate the dist-tag being recorded, so latest could not be checked
 *  at all. Distinct from staleDefaults: unknown-and-unchecked, rather than known-and-wrong. */
const missingTag = [];

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

  // ── `default` + `<` BANDS ────────────────────────────────────────────────────
  //
  // `default` is generated from LATEST, and every band key is a `<` bound. That pairing is what
  // makes coverage total: bands reach DOWNWARD without limit, so every old version -- including
  // the ones too unpopular to probe -- is caught by the lowest band, while `default` covers
  // today's release and every future one. Bands nest by construction, so resolution is
  // NARROWEST-BOUND-WINS with no ordering rule and no key-order dependence.
  //
  // ⛔ NEVER emit a point version as a matcher. The first real catalog emitted `versions: 5.1.1`
  // for bcrypt, which grants 5.1.1 and leaves 5.0.0 on the base profile -- it BREAKS. That is
  // under-granting, the one direction this project rejects everywhere.
  const byVersion = new Map();
  for (const [, group] of meaningful) {
    for (const r of group) {
      const cur = byVersion.get(r.version);
      const here = { ...(r.grant ?? {}) };
      if ((r.writePaths ?? []).length) here.writePaths = r.writePaths;
      byVersion.set(r.version, cur ? unionGrant(cur, here) : here);
    }
  }
  // A version measured as needing NOTHING is absent from `meaningful` but is still evidence --
  // it bounds a band from above. Seed those as empty so the ordering below sees every version.
  for (const v of allVersions) if (!byVersion.has(v)) byVersion.set(v, {});

  const ordered = [...allVersions].sort(cmpVer);
  // LATEST: the probe's recorded dist-tag when present, else the highest measured version. The
  // mega script always probes `latest` explicitly, so the fallback only serves legacy records --
  // and if it ever picks wrong, `default` is generated from an older version and FUTURE releases
  // are under-granted, which is why the dist-tag is preferred rather than merely nice.
  const distTag = rs.map((r) => r.standing?.latestVersion).find(Boolean) ?? null;
  const tagged = distTag && ordered.includes(distTag) ? distTag : null;
  const latest = tagged ?? ordered[ordered.length - 1];
  // ⛔ `default` GENERATED FROM A NON-LATEST VERSION IS AN UNDER-GRANT RISK, so it is a GATE and
  // not a note. If the true latest needs MORE than the highest version we measured, every release
  // from that point on silently falls to a grant that is too narrow — the one direction this
  // project rejects. MEASURED on the first real corpus: the highest-measured fallback was wrong
  // for 3 of 8 packages (better-sqlite3 13.0.2 vs 12.6.0, canvas 3.2.3 vs 2.11.2, sharp 0.35.3
  // vs 0.34.4), which is what turned this from a note into a gate.
  if (distTag && !tagged) staleDefaults.push(`${pkg}: latest is ${distTag}, highest measured ${latest}`);
  else if (!distTag) missingTag.push(pkg);

  const dflt = { ...(byVersion.get(latest) ?? {}) };

  // A BAND IS WRITTEN ONLY WHERE AN OLDER VERSION NEEDS *MORE* THAN LATEST. A version needing
  // LESS gets no band at all: it falls to `default` and is harmlessly over-granted, the safe
  // direction. That is also what dissolves the INVERTED case (better-sqlite3 needs network at
  // 12.6.0 and nothing at 9.6.0), which has no clean `<`-band expression.
  //
  // A band's grant is the UNION of every measured grant BELOW its bound, unioned with `default`
  // -- so it covers the unmeasured gaps between probed versions, and can never grant less than
  // `default`. Nothing merges at resolution time, so each band must be complete on its own.
  const bandList = [];
  for (let i = 1; i < ordered.length; i++) {
    const bound = ordered[i];
    let acc = { ...dflt };
    for (let j = 0; j < i; j++) acc = unionGrant(acc, byVersion.get(ordered[j]) ?? {});
    if (capsKey(acc) === capsKey(dflt)) continue;          // needs no more than latest
    bandList.push({ bound, caps: acc, covers: ordered.slice(0, i) });
  }
  // Same grant at two bounds means the narrower is redundant (narrowest wins), so keep the
  // WIDEST bound per distinct grant.
  const widest = new Map();
  for (const b of bandList) widest.set(capsKey(b.caps), b);

  const entry = { default: dflt };
  dflt.notes = `latest measured ${latest}`;

  const versions = {};
  for (const b of [...widest.values()].sort((x, y) => cmpVer(y.bound, x.bound))) {
    b.caps.notes = `measured ${b.covers.join(', ')}; covers everything below ${b.bound}`;
    versions[`<${b.bound}`] = b.caps;
  }
  if (Object.keys(versions).length) entry.versions = versions;

  // A writePaths entry embedding the measured version names a directory that moves on the next
  // release. Under `<` bands that is no longer expressible as a matcher, so it is surfaced as a
  // re-measure note rather than silently pinning the grant to one point.
  const pinned = [...new Set(rs.flatMap((r) => r.writePathsVersionPinned ?? []))];
  if (pinned.length) {
    dflt.notes += `; version-pinned writePaths (${pinned.join(', ')}) — re-measure on a new release`;
    notes.push(`${pkg}: writePaths embed a version (${pinned.join(', ')}) — re-measure each release`);
  }
  // PER-PLATFORM: only when the platforms disagree, else one line covers all three.
  if (allPlatforms.size === 1 && PLATFORM) dflt.platforms = [PLATFORM];

  const unmeasured = [...new Set(rs.flatMap((r) => r.unmeasuredScopesGranted ?? []))];
  if (unmeasured.length) {
    dflt.notes += `; widened for unmeasured scopes (${unmeasured.join(', ')})`;
    notes.push(`${pkg}: widened for ${unmeasured.join(', ')}`);
  }
  if (rs.some((r) => r.declaresInstallScript && !r.projectAxisConclusive)) {
    dflt.notes += '; project axis inconclusive (package was not materialized)';
  }

  // A band that grants strictly LESS than `default` is a generator bug by construction -- bands
  // are unioned WITH default above, so this can only fire if that invariant is broken. Assert it
  // rather than shipping a silently narrowed old-version grant.
  for (const [k, v] of Object.entries(versions)) {
    const merged = unionGrant(v, dflt);
    if (capsKey(merged) !== capsKey(v)) {
      throw new Error(`${pkg} band ${k} grants less than default — generator invariant broken`);
    }
  }
  packages[pkg] = entry;
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
  // An override is an ENTRY -- `{default, versions?}` -- the same shape the generator emits, so a
  // human writing one never has to learn a second grammar. The legacy `grants: [...]` array is
  // rejected rather than silently coerced: its first-match-wins semantics do not survive the move
  // to `<` bands, so a quietly-converted override would resolve differently than its author read.
  if (Array.isArray(o.grants)) {
    rejected.push(`${f}: legacy 'grants' array — rewrite as { default, versions? }`);
    continue;
  }
  const ent = o.entry ?? (o.default ? { default: o.default, ...(o.versions ? { versions: o.versions } : {}) } : null);
  if (!ent?.default) { rejected.push(`${f}: no entry.default`); continue; }
  // COMPARE CAPABILITIES, NOT THE WHOLE ENTRY. `notes` always differs — the measured note records
  // what was observed, the override's records why a human wrote it — so comparing serialised
  // entries made this check STRUCTURALLY UNABLE TO FIRE. Verified by a fixture whose override
  // matched the measured result exactly and was still not reported.
  const caps = (e) => JSON.stringify({
    d: capsKey(e?.default ?? {}),
    v: Object.fromEntries(Object.entries(e?.versions ?? {}).sort()
      .map(([k, v]) => [k, capsKey(v)])),
  });
  const before = packages[name] ? caps(packages[name]) : null;
  packages[name] = ent;
  if (before && before === caps(ent)) deadWeight.push(name);
  applied.push({ name, why: r.evidence, by: r.investigator, on: r.date });
}

const baseline = fs.existsSync(BASELINE)
  ? JSON.parse(fs.readFileSync(BASELINE, 'utf8'))
  : { baseline: [], env: [] };

// ⛔ EGRESS IS GRANTED BY `packageNetwork.full`, NOT BY A PACKAGE'S `network` CAPABILITY.
//
// Emitting only `packages[].default.network` produces a catalog that grants network to NOBODY:
// `parse_package_network` (`crates/nub-sandbox/src/catalog.rs`) builds the allow table from
// `networkHosts[].fetchedBy` and `packageNetwork.full[]` and from nothing else. The SHIPPED catalog
// is the proof of shape — 210 `packageNetwork.full` entries against ZERO packages carrying a
// network capability.
//
// MEASURED, and the reason it went unseen for the whole corpus: macOS and Linux deny egress
// IN-KERNEL (Seatbelt / Landlock+seccomp) off the per-package caps, so the capability alone works
// there. Windows has no per-process network filter and uses the userland JS net gate, which reads
// only that table — so on Windows the grant was invisible and 67-87% of packages needing network
// were recorded as jail failures.
//
// Derived here rather than authored so it cannot drift from the measurements it summarises: a
// package appears iff some band of its entry actually measured a network need. A version RANGE is
// deliberately not emitted — the table's matcher is name-scoped, and a package that needed egress
// at any measured version needs it at the entry level.
const networkFull = Object.entries(packages)
  .filter(([, entry]) =>
    Object.values(entry ?? {}).some((band) => band && typeof band === 'object' && band.network === true))
  .map(([name]) => ({ package: name }))
  .sort((a, b) => (a.package < b.package ? -1 : a.package > b.package ? 1 : 0));

const catalog = {
  packages,
  ...(networkFull.length ? { packageNetwork: { full: networkFull } } : {}),
  baseline: baseline.baseline ?? [],
  env: baseline.env ?? [],
};
fs.mkdirSync(path.dirname(OUT), { recursive: true });
fs.writeFileSync(OUT, `${JSON.stringify(catalog, null, 2)}\n`);

// ── report ────────────────────────────────────────────────────────────────────

const grantCount = Object.values(packages)
  .reduce((n, e) => n + 1 + Object.keys(e.versions ?? {}).length, 0);
const bandCount = Object.values(packages)
  .reduce((n, e) => n + Object.keys(e.versions ?? {}).length, 0);
console.log(`records read        ${records.length}`);
console.log(`platforms           ${[...platforms].join(', ') || '(none recorded)'}`);
const hh = Object.entries(harnessHashes).sort((a, b) => b[1] - a[1]);
console.log(`harness revisions   ${hh.map(([h, n]) => `${h}:${n}`).join('  ')}`);
if (hh.length > 1) console.log(`  ⚠ RECORDS SPAN ${hh.length} HARNESS REVISIONS — re-run the minority under ${hh[0][0]} before shipping this catalog`);
console.log(`packages with entry ${Object.keys(packages).length}`);
console.log(`grants emitted      ${grantCount}  (${Object.keys(packages).length} default + ${bandCount} version bands)`);
console.log(`needed nothing      ${byPackage.size - Object.keys(packages).length}`);
if (staleDefaults.length) {
  console.log(`\n⚠ ${staleDefaults.length} PACKAGE(S) HAVE A STALE \`default\` — latest was never measured,`);
  console.log('  so their default grant comes from an older version and a newer release that needs');
  console.log('  MORE is silently under-granted. Probe latest and re-collate before shipping:');
  for (const s of staleDefaults) console.log(`    ${s}`);
}
if (missingTag.length) {
  console.log(`\n⚠ ${missingTag.length} package(s) predate dist-tag recording, so latest is UNCHECKED`);
  console.log(`  (assumed = highest measured): ${missingTag.join(', ')}`);
}
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

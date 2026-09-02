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
// The state space, for reconstructing a grant a record never serialised — see the backfill below.
import { STATES, grantForState } from './states.mjs';

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
    // ⛔ BACKFILL A GRANT THAT WAS NEVER SERIALISED, rather than discarding the record.
    //
    // `grantFor` returned `undefined` for every non-empty state until b5d6898f82 (an `arr[0]` read
    // of an object, left over from the retired array shape), so ~2,500 records across three
    // platforms carry `state` — a human label — and no `grant`. Everything downstream keys on
    // `grant`: `grantKey` bands on it, `capsKey` compares it, `unionGrant` widens off its axes. So
    // without this the whole existing corpus collates into one `null` band and emits a catalog with
    // no capabilities.
    //
    // The reconstruction is EXACT, not a guess. `STATES` is an exhaustive `read x write x network`
    // product and each state's `label` is built deterministically from its cost atoms
    // (`costAtoms.join(' + ')`), so a label names exactly one state, and `grantFor` maps that state
    // to its grant. That is why these records need re-COLLATION and not re-measurement — hours of
    // installs preserved.
    //
    // Only ever fills a MISSING grant; a record that has one is never touched.
    if (!rec.grant && typeof rec.state === 'string' && rec.state !== '(nothing)') {
      const st = STATES.find((s) => s.label === rec.state);
      if (st) {
        rec.grant = grantForState(st);
        rec.grantBackfilled = true;
      } else {
        console.error(`  WARN ${f}: state "${rec.state}" matches no known state — grant not recovered`);
      }
    }
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

/** Whether the band `<bound` ADMITS `version` -- the JS mirror of the predicate the jail actually
 *  resolves with, `compiler::version_scope::applies` (= `semver::VersionReq::matches`).
 *
 *  ⛔ A PRERELEASE NEVER MATCHES A PLAIN `<X` BOUND. `semver` admits a prerelease only when some
 *  comparator carries a prerelease at the SAME major.minor.patch, so a release-bounded band cannot
 *  be widened into one -- pinned Rust-side as `!applies("<0.13.0", "0.12.0-rc.1")`, alongside the
 *  three bounds an author would reach for to fix that, none of which work. Nor is there another
 *  range form to reach for: `catalog_v2` rejects any band key that is not `<`-prefixed, so the
 *  two-comparator spelling that WOULD admit a prerelease cannot be written down at all.
 *
 *  Ordering defers to `cmpVer`, so two prereleases sharing a core and a first identifier compare
 *  equal and read as NOT admitted. That direction is safe: an unadmitted measurement is absorbed
 *  into `default` below rather than dropped. */
function bandAdmits(bound, version) {
  if (cmpVer(version, bound) >= 0) return false;
  const parts = (v) => {
    const [core, ...pre] = String(v).split('+')[0].split('-');
    return { core, pre: pre.length ? pre.join('-') : null };
  };
  const b = parts(bound);
  const v = parts(version);
  if (v.pre === null) return true;
  return b.pre !== null && b.core === v.core;
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

/** …and the catalog's OVERRIDE-BLOCK key for each, which is not the same vocabulary: the block
 *  is spelled `win`, not `windows`. One map, here, so nothing downstream has to remember. */
const OS_KEY = { macos: 'macos', linux: 'linux', windows: 'win' };

/** The override block that turns `outer` into `want` for ONE operating system, or null when the
 *  two already agree.
 *
 *  ⛔ THE OUTER GRANT STAYS THE WIDEST AND THE BLOCKS NARROW IT — never the reverse. An OS nobody
 *  measured inherits `outer`, so making the outer grant the union is what keeps an unmeasured
 *  platform on the SAFE side: over-granting fails to confine, under-granting breaks the install.
 *  Inverting this (outer = intersection, blocks widen) reads equivalent and is not — it silently
 *  under-grants every platform the corpus never reached.
 *
 *  `null` WITHDRAWS a field the outer grant carries, and is the only spelling of nothing: the
 *  parser refuses `network: false` so one answer never gets two dialects. */
function osBlock(outer, want) {
  const block = {};
  for (const field of ['read', 'write', 'network', 'writePaths']) {
    const o = JSON.stringify(outer[field] ?? null);
    const w = JSON.stringify(want[field] ?? null);
    if (o === w) continue;
    block[field] = want[field] ?? null;
  }
  return Object.keys(block).length ? block : null;
}

/** Per-OS caps for one set of versions, mirroring exactly how the cross-OS grant was built for the
 *  same set — union within the OS, then unioned with that OS's `default` so a band can never grant
 *  less than the entry's own default on the same platform. Returns only the OSes that actually
 *  measured something; an absent OS gets no block and inherits the wide outer grant. */
function perOsCaps(byVersionPlat, versions, floorByOs) {
  const out = new Map();
  for (const v of versions) {
    const perOs = byVersionPlat.get(v);
    if (!perOs) continue;
    for (const [os, g] of perOs) {
      const prior = out.get(os);
      out.set(os, prior ? unionGrant(prior, g) : { ...g });
    }
  }
  if (floorByOs) for (const [os, floor] of floorByOs) {
    if (out.has(os)) out.set(os, unionGrant(out.get(os), floor));
  }
  return out;
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
  // ⛔ AND THE SAME THING KEPT PER-PLATFORM, because `byVersion` UNIONS ACROSS OSes and that union
  // is an over-grant wherever they disagree. MEASURED on the real corpus: of 1734 cross-OS
  // comparable specs, 250 diverge and 98 would take `write:"disk"` on an OS that measured NARROW —
  // `@arkweid/lefthook` is `write:{project}` on darwin AND linux but `write:"disk"` on win32, so
  // the union hands POSIX a full-disk write it demonstrably does not need. Keeping the per-OS
  // grants here is what lets the emit below narrow them back with override blocks.
  const byVersionPlat = new Map();
  for (const [, group] of meaningful) {
    for (const r of group) {
      const cur = byVersion.get(r.version);
      const here = { ...(r.grant ?? {}) };
      if ((r.writePaths ?? []).length) here.writePaths = r.writePaths;
      byVersion.set(r.version, cur ? unionGrant(cur, here) : here);

      const os = osOf(r);
      if (!os) continue;
      if (!byVersionPlat.has(r.version)) byVersionPlat.set(r.version, new Map());
      const perOs = byVersionPlat.get(r.version);
      const prior = perOs.get(os);
      // Still a UNION *within* one OS: several machines may have measured the same platform, and
      // the reconciliation rule that picks the wider grant is unchanged. Only the CROSS-OS union
      // is what this split removes.
      perOs.set(os, prior ? unionGrant(prior, here) : here);
    }
  }
  // A version measured as needing NOTHING is absent from `meaningful` but is still evidence --
  // it bounds a band from above. Seed those as empty so the ordering below sees every version.
  for (const v of allVersions) if (!byVersion.has(v)) byVersion.set(v, {});

  // ⛔ THE SAME EVIDENCE RULE PER-PLATFORM, AND MISSING IT WAS THE LARGEST SOURCE OF OVER-GRANT.
  //
  // `meaningful` drops any record whose grant is null and whose writePaths are empty, so a platform
  // that measured "needs NOTHING" never reached `byVersionPlat` and was indistinguishable from a
  // platform that never measured at all. The first must emit a block WITHDRAWING the outer grant;
  // only the second may inherit it. MEASURED before this seed: `backport@12.0.4` is grant `null` on
  // BOTH darwin and linux and `write:"disk"` on win32, and was emitted as a bare global
  // `write:"disk"` with no blocks -- handing full-disk write to two platforms that measured needing
  // nothing at all. It accounted for 32 of macOS's 34 and 26 of linux's 39 catalog disk grants.
  for (const r of rs) {
    const os = osOf(r);
    if (!os) continue;
    if (!byVersionPlat.has(r.version)) byVersionPlat.set(r.version, new Map());
    const perOs = byVersionPlat.get(r.version);
    if (!perOs.has(os)) perOs.set(os, {});
  }

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

  // ⛔ A MEASUREMENT NO BAND CAN ADMIT BELONGS TO `default`, BECAUSE THAT IS WHERE ITS VERSION
  // RESOLVES. Bands are `<`-bounded and `semver` refuses a prerelease against a release bound
  // (`bandAdmits`), so a measured prerelease normally falls through every band to `default`.
  // Folding its grant into a band instead did two wrong things at once: it handed the capability
  // to release versions nobody measured, and it withheld it from the one version that proved the
  // need. MEASURED on the shipped catalog: 17 of 139 bands rested on a version the band excludes,
  // and for `@tensorflow/tfjs-backend-wasm` the only two versions in the whole package declaring an
  // install hook were both prereleases resolving to a `default` that granted nothing, while a band
  // covering 65 hook-free releases carried the whole-home write those two needed.
  //
  // The bound candidates are unchanged, so a prerelease `latest` still bounds a band -- what it
  // may no longer do is JUSTIFY one it cannot reach.
  const unbandable = ordered.filter((v, i) => !ordered.slice(i + 1).some((b) => bandAdmits(b, v)));
  const absorbed = unbandable.filter((v) => v !== latest);
  let dflt = { ...(byVersion.get(latest) ?? {}) };
  for (const v of absorbed) dflt = unionGrant(dflt, byVersion.get(v) ?? {});

  // A BAND IS WRITTEN ONLY WHERE AN OLDER VERSION NEEDS *MORE* THAN LATEST. A version needing
  // LESS gets no band at all: it falls to `default` and is harmlessly over-granted, the safe
  // direction. That is also what dissolves the INVERTED case (better-sqlite3 needs network at
  // 12.6.0 and nothing at 9.6.0), which has no clean `<`-band expression.
  //
  // A band's grant is the UNION of every measured grant the band ADMITS, unioned with `default`
  // -- so it covers the unmeasured gaps between probed versions, and can never grant less than
  // `default`. Nothing merges at resolution time, so each band must be complete on its own.
  const bandList = [];
  for (let i = 1; i < ordered.length; i++) {
    const bound = ordered[i];
    // ⛔ ADMITTED, not merely lower. A band whose evidence all sits outside it is pure invention:
    // every version it would cover is unmeasured, and the run it cites resolves somewhere else.
    const covers = ordered.slice(0, i).filter((v) => bandAdmits(bound, v));
    if (!covers.length) continue;
    let acc = { ...dflt };
    for (const v of covers) acc = unionGrant(acc, byVersion.get(v) ?? {});
    if (capsKey(acc) === capsKey(dflt)) continue;          // needs no more than latest
    bandList.push({ bound, caps: acc, covers });
  }
  // Same grant at two bounds means the narrower is redundant (narrowest wins), so keep the
  // WIDEST bound per distinct grant.
  const widest = new Map();
  for (const b of bandList) widest.set(capsKey(b.caps), b);

  const entry = { default: dflt };

  // ── PER-OS OVERRIDE BLOCKS ────────────────────────────────────────────────────
  //
  // Everything above unions ACROSS platforms, which is correct as a floor and wrong as an answer
  // wherever the OSes actually measured differently. Narrow each measured platform back to what it
  // needs. An OS that measured nothing here gets no block and keeps the union — safe by
  // construction, and the reason the outer grant must stay the widest.
  //
  // ⛔ OVER THE SAME VERSIONS `dflt` WAS BUILT FROM, not `[latest]` alone. A block is a
  // WITHDRAWAL, so computing it from latest only would emit one cancelling the grant an absorbed
  // measurement just contributed — on every OS that measured latest, which is every OS that has
  // data. That would undo the absorption on exactly the platforms it matters for.
  const dfltPerOs = perOsCaps(byVersionPlat, [latest, ...absorbed], null);
  for (const [os, caps] of dfltPerOs) {
    const block = osBlock(dflt, caps);
    if (block) entry.default[OS_KEY[os]] = block;
  }

  dflt.notes = `latest measured ${latest}`;
  if (absorbed.length) {
    dflt.notes += `; also measured ${absorbed.join(', ')}, which no \`<\` band admits and so resolve here`;
  }

  const versions = {};
  for (const b of [...widest.values()].sort((x, y) => cmpVer(y.bound, x.bound))) {
    // The band's own per-OS caps, floored by that OS's default for the same reason the cross-OS
    // band is unioned with `default`: nothing merges at resolution time, so a band must be
    // complete on its own and can never resolve NARROWER than the entry's default on that OS.
    for (const [os, caps] of perOsCaps(byVersionPlat, b.covers, dfltPerOs)) {
      const block = osBlock(b.caps, caps);
      if (block) b.caps[OS_KEY[os]] = block;
    }
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

  // ⛔ AND EVERY MEASUREMENT MUST REACH THE GRANT ITS OWN VERSION RESOLVES TO. This is the property
  // the absorption above exists for, asserted against the FINISHED entry rather than trusted from
  // the construction: absorption reasons over the candidate bounds, but `widest` may afterwards
  // drop the one band that admitted a version, leaving it on a `default` that never took its grant.
  // Checked per OS as well as cross-OS, because a withdrawal block is where such a shortfall hides
  // — the shipped `@tensorflow/tfjs-backend-wasm` entry was narrow on macOS at exactly the version
  // it had measured wide there. Mirrors `Entry::grant_for`: narrowest admitting bound wins, else
  // `default`. Runs before `scopeToOs`, which empties the outer axes on purpose.
  const effectiveOn = (caps, os) => {
    const out = { ...caps };
    delete out.notes;
    for (const k of Object.values(OS_KEY)) delete out[k];
    for (const [f, val] of Object.entries(caps[OS_KEY[os]] ?? {})) {
      if (val === null) delete out[f]; else out[f] = val;
    }
    return out;
  };
  for (const v of ordered) {
    const hit = Object.keys(versions).filter((k) => bandAdmits(k.slice(1), v))
      .sort((a, b) => cmpVer(a.slice(1), b.slice(1)));
    const resolved = hit.length ? versions[hit[0]] : dflt;
    const where = hit.length ? hit[0] : 'default';
    for (const [os, want] of byVersionPlat.get(v) ?? []) {
      const got = effectiveOn(resolved, os);
      if (capsKey(unionGrant({ ...got }, want)) !== capsKey(got)) {
        throw new Error(`${pkg}@${v} measured ${JSON.stringify(want)} on ${os} but resolves to `
          + `${where} = ${JSON.stringify(got)} — generator invariant broken`);
      }
    }
  }

  // ONE OS MEASURED => CLAIM ONLY THAT OS. The catalog has no filter any more, so a grant's
  // capability fields apply everywhere; a measurement taken on one platform cannot speak for the
  // other two. Move the capabilities into that OS's override block and leave the outer grant
  // carrying only `notes`, which is how "nothing is known about the other two" is spelled — they
  // then resolve to the base profile. Runs LAST so the band/default invariant above still
  // compares real capability sets.
  if (allPlatforms.size === 1 && PLATFORM) {
    for (const caps of [dflt, ...Object.values(versions)]) scopeToOs(caps, PLATFORM);
  }
  packages[pkg] = entry;
}

/** Rewrite `caps` in place so its capabilities apply only on `os`. `notes` stays outer: it is
 *  free text with no capability effect, and duplicating it into the block would only make the
 *  emitted catalog noisier. */
function scopeToOs(caps, os) {
  const key = OS_KEY[os];
  if (!key) throw new Error(`--platform ${os}: expected one of ${Object.keys(OS_KEY).join(', ')}`);
  const block = {};
  for (const field of ['read', 'write', 'network', 'writePaths']) {
    if (caps[field] === undefined) continue;
    block[field] = caps[field];
    delete caps[field];
  }
  // An empty block overrides nothing and the parser rejects it, which is right: a package that
  // measured as needing nothing anywhere is expressed by having no capabilities at all.
  if (Object.keys(block).length) caps[key] = block;
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

// ⛔ THIS IS A v2 CATALOG: egress is the per-package `network: true` capability, and there is no
// `packageNetwork.full` table here. A v1 table was added at one point to make Windows grant egress;
// it worked, but it papered over a nub bug rather than fixing it — `package_network_allowed()`
// consulted only the v1 catalog, so a v2 override yielded nothing and the net gate fell back to the
// compiled-in table. Fixed in `e3cdc0e7f9` and pinned by
// `crates/nub-sandbox/tests/generated_catalog_round_trip.rs`, which asserts THIS output shape
// reaches the lookup the jail uses.
const catalog = { packages, baseline: baseline.baseline ?? [], env: baseline.env ?? [] };
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

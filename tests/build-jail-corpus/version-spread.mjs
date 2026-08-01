#!/usr/bin/env node
// version-spread.mjs — select the OLD-PINNED-VERSION sample, and score the run it produces.
//
// WHY THIS EXISTS. Every corpus run to date measured roughly-latest versions, but the
// release condition names old pins explicitly, "where install scripts concentrate". The
// one data point we had says the axis matters and cuts against us: `esbuild`'s break
// boundary is exactly 0.13.0, the release where `optionalDependencies` landed — the same
// package passes confined above it and fails below it, decided purely by version.
//
// The hypothesis that generalises esbuild: the ecosystem has migrated from self-building
// postinstalls to prebuilt `optionalDependencies`, so OLDER versions need capabilities
// (egress, a project write) that newer ones do not, and the shipped pass rate — measured
// at latest — overstates what a project that pins actually experiences.
//
// ── THE SELECTION RULE, stated so its bias is arguable ──
//
// Universe: the census's 387 install-relevant packages above the 100k weekly threshold.
// A package enters the POOL when it has at least one install-scripted version that is
//   (a) a full generation behind latest — a lower major, or a lower minor while major is 0,
//       so "old" means an architectural distance and not last month's patch;
//   (b) still pulling >= 50,000 weekly downloads, so a break there is a break someone
//       actually experiences; and
//   (c) a stable release — prereleases are excluded, because a version-scoped grant cannot
//       reach them (cargo semver admits a prerelease only when a comparator names one).
// The pool is 101 packages. The sample is the top N by OLD EXPOSURE — the summed weekly
// downloads of a package's qualifying old versions — so the budget goes where an old
// break costs the most installs.
//
// Bands, per selected package:
//   latest   the published `latest`. Measured only when it HAS an install script; when a
//            package dropped its script (13 of the pool) latest is inert by construction —
//            the jail has nothing to confine, so spending a ladder on it buys nothing that
//            the census does not already state.
//   old      the highest-download qualifying old version. Usually one generation back.
//   deep     the highest-download version in the OLDEST qualifying generation. Emitted
//            only when it differs from `old`, which is where the real age lives.
//
// BIAS, explicitly. (1) Download-weighted, so it answers "how much installed volume is
// exposed", NOT "what fraction of packages break" — a package-count rate off this sample
// is not the ecosystem's. (2) The >=50k floor deliberately excludes the truly abandoned
// tail, where breakage is likely worse and nobody is watching. (3) Ranking by old exposure
// over-selects long-lived, heavily-pinned, native-build packages; those are where install
// scripts concentrate, which is the point, but they are not a random draw. (4) `latest` is
// re-measured here rather than read from an archived corpus run, because comparing bands
// across two binaries would vary more than the one thing under test.
//
// USAGE
//   node version-spread.mjs --select [--top N]      -> writes shard-vspread.tsv
//   node version-spread.mjs --score  <matrix.ndjson>
//
// Env: CENSUS_DIR (default ../../.fray/npm-census).
import fs from 'node:fs';
import path from 'node:path';

const argv = process.argv.slice(2);
const arg = (n, d) => { const i = argv.indexOf(n); return i >= 0 ? argv[i + 1] : d; };
const HERE = path.dirname(new URL(import.meta.url).pathname);
const CENSUS = process.env.CENSUS_DIR || path.join(HERE, '../../.fray/npm-census');

// Fixture capabilities, from the corpus manifests: a package whose INPUT is absent exits 0
// having done nothing, in every cell, and the ladder then calls it NEEDS-NOTHING. The
// mapping is by package name and holds across versions.
const NEEDS = {
  prisma: 'prisma-schema', '@prisma/client': 'prisma-schema',
  nx: 'nx-json', msw: 'msw-manifest', 'vue-demi': 'vue-dep',
  lefthook: 'git-repo,lefthook-config', '@evilmartians/lefthook': 'git-repo,lefthook-config',
  '@arkweid/lefthook': 'git-repo,lefthook-config', 'simple-git-hooks': 'git-repo,lefthook-config',
  'pre-commit': 'git-repo,lefthook-config', 'pre-push': 'git-repo,lefthook-config',
  husky: 'git-repo', yorkie: 'git-repo,lefthook-config',
};

const readNdjson = (f) => fs.readFileSync(f, 'utf8').split('\n').filter((l) => l.trim()).map((l) => JSON.parse(l));
const semver = (v) => { const m = /^(\d+)\.(\d+)\.(\d+)$/.exec(v); return m ? m.slice(1).map(Number) : null; };
// A "generation" is the major, or the minor while major is 0 — the unit across which a
// package's install mechanism actually changes. esbuild's whole break boundary is a 0.x
// minor, so treating 0.x majors as one generation would erase the case that motivates this.
const generation = (v) => { const p = semver(v); return p ? (p[0] > 0 ? [p[0]] : [0, p[1]]) : null; };
const below = (a, b) => JSON.stringify(a) < JSON.stringify(b) ? cmp(a, b) < 0 : cmp(a, b) < 0;
const cmp = (a, b) => { for (let i = 0; i < Math.max(a.length, b.length); i++) { const d = (a[i] ?? -1) - (b[i] ?? -1); if (d) return d; } return 0; };

const DOWNLOAD_FLOOR = 50_000;

function pool() {
  const pkgs = new Map(readNdjson(path.join(CENSUS, 'census-packages.ndjson')).map((r) => [r.name, r]));
  const byPkg = new Map();
  for (const r of readNdjson(path.join(CENSUS, 'census-versions.ndjson'))) {
    if (!byPkg.has(r.package)) byPkg.set(r.package, []);
    byPkg.get(r.package).push(r);
  }
  const out = [];
  for (const [name, p] of pkgs) {
    if (!p.has_install_script_top_n || !p.above_threshold) continue;
    const latestGen = generation(p.latest_version);
    if (!latestGen) continue;
    const vs = byPkg.get(name) || [];
    const latestRow = vs.find((r) => r.version === p.latest_version);
    const old = vs.filter((r) => r.has_install_script && semver(r.version)
      && (r.version_weekly_downloads || 0) >= DOWNLOAD_FLOOR
      && cmp(generation(r.version), latestGen) < 0);
    if (!old.length) continue;
    old.sort((a, b) => b.version_weekly_downloads - a.version_weekly_downloads);
    const gens = [...new Set(old.map((r) => JSON.stringify(generation(r.version))))].sort((a, b) => cmp(JSON.parse(a), JSON.parse(b)));
    const oldestGen = gens[0];
    const deep = old.filter((r) => JSON.stringify(generation(r.version)) === oldestGen)
      .reduce((a, b) => (a.version_weekly_downloads >= b.version_weekly_downloads ? a : b));
    out.push({
      name, weekly: p.package_weekly_downloads, latest: p.latest_version,
      latestHasScript: !!(latestRow && latestRow.has_install_script),
      latestOptionalDeps: latestRow ? latestRow.optional_deps_count : null,
      old: old[0], deep, generationsBack: gens.length,
      oldExposure: old.reduce((s, r) => s + r.version_weekly_downloads, 0),
    });
  }
  out.sort((a, b) => b.oldExposure - a.oldExposure);
  return out;
}

if (argv.includes('--select')) {
  const top = Number(arg('--top', '24'));
  const sel = pool().slice(0, top);
  const lines = ['# name\tversion\tfixture-capabilities\t# band weekly optionalDeps',
    `# generated by version-spread.mjs --top ${top}; pool = ${pool().length} packages`];
  let rows = 0;
  for (const c of sel) {
    const need = NEEDS[c.name] || '';
    const emit = (v, band, weekly, od) => { lines.push(`${c.name}\t${v}\t${need}\t# ${band} ${weekly} od=${od}`); rows++; };
    if (c.latestHasScript) emit(c.latest, 'latest', '-', c.latestOptionalDeps);
    emit(c.old.version, 'old', c.old.version_weekly_downloads, c.old.optional_deps_count);
    if (c.deep.version !== c.old.version) emit(c.deep.version, 'deep', c.deep.version_weekly_downloads, c.deep.optional_deps_count);
  }
  const f = path.join(HERE, 'shard-vspread.tsv');
  fs.writeFileSync(f, lines.join('\n') + '\n');
  console.log(`${sel.length} packages, ${rows} rows -> ${f}`);
  console.log(`latest band skipped as inert (no install script on latest): ${sel.filter((c) => !c.latestHasScript).map((c) => c.name).join(', ')}`);
}

if (argv.includes('--score')) {
  const recs = readNdjson(arg('--score'));
  const meta = new Map(pool().map((c) => [c.name, c]));
  const band = (r) => {
    const c = meta.get(r.package); if (!c) return '?';
    return r.version === c.latest ? 'latest' : r.version === c.deep.version && c.deep.version !== c.old.version ? 'deep' : 'old';
  };
  const tally = {};
  for (const r of recs) {
    const b = band(r);
    tally[b] ??= { admissible: 0, inadmissible: 0, needsNothing: 0, needsGrant: 0, failsAtBoth: 0, unmeasurable: 0, rows: 0 };
    const t = tally[b]; t.rows++;
    if (r.verdict === 'CONTROL-FAILED' || r.verdict === 'CONTROL-NOOP') { t.inadmissible++; continue; }
    if (r.verdict === 'UNMEASURABLE' || r.verdict === 'ERROR') { t.unmeasurable++; continue; }
    t.admissible++;
    if (r.verdict === 'NEEDS-NOTHING') t.needsNothing++;
    else if (r.verdict === 'FAILS-AT-BOTH') t.failsAtBoth++;
    else t.needsGrant++;
  }
  console.log('band       rows  admis  inadm  unmeas | needs-nothing  needs-grant  fails-at-both   pass-ungranted');
  for (const b of ['latest', 'old', 'deep']) {
    const t = tally[b]; if (!t) continue;
    const rate = t.admissible ? (100 * t.needsNothing / t.admissible).toFixed(1) : '-';
    console.log(`${b.padEnd(9)} ${String(t.rows).padStart(4)} ${String(t.admissible).padStart(6)} ${String(t.inadmissible).padStart(6)} ${String(t.unmeasurable).padStart(7)} | ${String(t.needsNothing).padStart(13)} ${String(t.needsGrant).padStart(12)} ${String(t.failsAtBoth).padStart(14)}   ${rate}%`);
  }
}

if (!argv.includes('--select') && !argv.includes('--score')) {
  const p = pool();
  console.log(`pool: ${p.length} packages with an install-scripted version >=1 generation behind latest at >=${DOWNLOAD_FLOOR}/wk`);
  for (const c of p.slice(0, 40)) {
    console.log(`${c.name.padEnd(34)} latest=${c.latest.padEnd(12)}${c.latestHasScript ? 'script' : 'INERT '} old=${c.old.version.padEnd(10)} deep=${c.deep.version.padEnd(10)} exposure=${c.oldExposure.toLocaleString()}`);
  }
}

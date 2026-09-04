// Record which install-script packages have been MEASURED to work, so the coverage metric stops
// conflating "not in the catalog" with "nobody has checked".
//
// ⛔⛔ WHY THIS FILE HAS TO EXIST, AND WHY THE OBVIOUS ALTERNATIVE IS AN UNDER-GRANT. The tempting
// fix is to give every measured-and-passing package a catalog entry that grants nothing. That is
// STRICTLY TIGHTER than having no entry, because absence takes `baseline_caps()` while an empty cell
// takes nothing -- `no_cell_denies_everything_unless_its_package_runs_no_lifecycle_hook` refuses it,
// and an empty cell is licensed only for packages that run NO lifecycle hook. So absence is already
// the correct encoding for "baseline suffices", and the measurement has nowhere to live IN the
// catalog. It lives here instead.
//
// TWO WAYS A PACKAGE IS COVERED, and they are different claims:
//   baseline-measured — the jailed arm passed on every platform at the DEFAULT grant. No catalog
//                       entry is wanted; absence already grants exactly what it needs.
//   v1-curated        — it holds a grant in `curated.rs`'s v1 table. Covered, but invisible to a
//                       v2-catalog-only check, which would otherwise report it as unmeasured.
//
//   usage: node make-baseline-coverage.mjs --win <tsv> --mac <tsv> --linux <tsv> \
//            --population <tsv> --curated <curated.rs> --sha <commit> --out <tsv>
import fs from 'node:fs';

const arg = (n) => { const i = process.argv.indexOf(n); return i > 0 ? process.argv[i + 1] : null; };
const need = (n) => { const v = arg(n); if (!v) { console.error(`missing ${n}`); process.exit(2); } return v; };
const [win, mac, linux, population, curated, sha, out] =
  ['--win', '--mac', '--linux', '--population', '--curated', '--sha', '--out'].map(need);

// A sweep row's verdict, and the version it actually installed when the row records one.
//
// ⛔ THE VERSION IS `assumed:` UNLESS THE ROW ITSELF CARRIES IT. The sweep DISCOVERS its population,
// so the version in the committed population file is what a run was expected to install, not what it
// did — and those diverge (measured 21 of 180 names moved in two days, and one package's `latest`
// was a prerelease that no ordinary install resolves). Rows written before `install-script-sweep.sh`
// began recording `ver=` cannot say, and marking them keeps the guess visibly distinct from evidence.
function verdicts(file) {
  const m = new Map();
  for (const line of fs.readFileSync(file, 'utf8').trim().split('\n')) {
    const p = line.split('\t');
    if (p.length >= 2) m.set(p[0], { verdict: p[1], version: /(?:^|\t)ver=(.+)$/.exec(line)?.[1] });
  }
  // A truncated sweep would silently shrink coverage and inflate the "unmeasured" count, which is
  // the direction that looks like honest caution and is actually a broken instrument.
  if (m.size < 150) { console.error(`refusing: ${file} has ${m.size} rows`); process.exit(3); }
  return m;
}
const sweeps = { win: verdicts(win), mac: verdicts(mac), linux: verdicts(linux) };

const version = new Map();
for (const line of fs.readFileSync(population, 'utf8').trim().split('\n')) {
  const [n, v] = line.split('\t');
  if (n && v) version.set(n, v.trim());
}

// The v1 table: the package name sits on the line before `CuratedGrant {`.
const src = fs.readFileSync(curated, 'utf8').split('\n');
const v1 = new Set();
for (let i = 1; i < src.length; i++) {
  if (!src[i].includes('CuratedGrant {')) continue;
  const m = src[i - 1].match(/"([^"]+)"/);
  if (m) v1.add(m[1]);
}
// KNOWN-ANSWER CONTROL. A regex over Rust fails EMPTY, and an empty v1 set would silently drop every
// v1-covered package back into "unmeasured" while looking like a clean run.
for (const known of ['pre-commit', 'cypress', '@prisma/client']) {
  if (!v1.has(known)) { console.error(`curated.rs parse is broken: ${known} not found`); process.exit(4); }
}

const rows = [];
for (const name of [...new Set(Object.values(sweeps).flatMap((m) => [...m.keys()]))].sort()) {
  const v = Object.fromEntries(Object.entries(sweeps).map(([k, m]) => [k, m.get(name)?.verdict ?? '(absent)']));
  // Every arm that recorded a version must agree, or we cannot say which one was measured.
  const seen = [...new Set(Object.values(sweeps).map((m) => m.get(name)?.version).filter(Boolean))];
  const measured = seen.length === 1 ? seen[0] : null;
  const shown = measured ?? (version.has(name) ? `assumed:${version.get(name)}` : null);
  if (Object.values(v).every((x) => x === 'OK') && shown) {
    rows.push([name, shown, 'macos,linux,win', 'baseline-measured', sha]);
  } else if (v1.has(name)) {
    rows.push([name, version.get(name) ?? '-', '-', 'v1-curated', sha]);
  }
}
for (const name of [...v1].sort()) {
  if (!rows.some((r) => r[0] === name)) rows.push([name, version.get(name) ?? '-', '-', 'v1-curated', sha]);
}

const header = '# name\tversion\tplatforms\treason\tmeasured-at\n';
fs.writeFileSync(out, header + rows.map((r) => r.join('\t')).join('\n') + '\n');
const by = (r) => r[3];
const tally = rows.reduce((a, r) => ((a[by(r)] = (a[by(r)] || 0) + 1), a), {});
console.log(`wrote ${rows.length} rows to ${out}`);
for (const [k, n] of Object.entries(tally)) console.log(`  ${k}: ${n}`);

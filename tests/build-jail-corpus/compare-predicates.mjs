#!/usr/bin/env node
// compare-predicates.mjs — score ONE run's deltas under both the old and the new
// work-signals, so "how far the number moved" is a one-variable measurement.
//
// WHY NOT JUST DIFF THE TWO RUNS' HEADLINES. Because that comparison varies three
// things at once: the predicate, run-to-run verdict instability (~4%, measured), and
// the settle gate now extending the measurement window past `approve-builds`'
// premature return. Attributing the whole movement to the predicate would be exactly
// the "control that varied more than the one thing under test" failure.
//
// Snapshots are persisted as NDJSON and deltas as JSON precisely so verdicts are
// re-derivable without re-running, which is what makes this possible: both predicate
// sets are applied to the SAME delta, from the SAME install, on the SAME binary. The
// only difference is the rule.
//
// The old predicates are reproduced here rather than read from git history, because
// what matters is their EFFECTIVE semantics, not their spelling:
//   binary-downloader : created_any_of ["^(pkg|home|store):.*"] + min_created 1.
//                       The harness strips a leading root-prefix group from each
//                       regex before applying it, which turns that pattern into `.*`
//                       — so the whole predicate reduces to "the package's own store
//                       cell gained at least one entry".
//   self-configuring  : min_created 1 — the same thing, spelled directly.
// Both are therefore `created.length >= 1`, which is what is applied below.
//
// usage: node compare-predicates.mjs <outroot-with-per-shard-delta.json> <manifest-dir>

import fs from 'node:fs';
import path from 'node:path';

const [outroot, manifestDir = '.'] = process.argv.slice(2);
if (!outroot) {
  console.error('usage: compare-predicates.mjs <outroot> [manifest-dir]');
  process.exit(2);
}

const OLD_WEAK = new Set(['binary-downloader', 'self-configuring']);
const classes = JSON.parse(fs.readFileSync(new URL('./lib/classes.json', import.meta.url), 'utf8'));

const manifestRows = new Map();
for (const f of fs.readdirSync(manifestDir).filter((f) => /^shard-default\d+\.tsv$/.test(f))) {
  for (const line of fs.readFileSync(path.join(manifestDir, f), 'utf8').split('\n')) {
    if (!line.trim() || line.startsWith('#')) continue;
    const [pkg, version, cls] = line.split('\t');
    manifestRows.set(`${path.basename(f, '.tsv').replace(/^shard-/, '')}|${pkg}`, { pkg, version, cls });
  }
}

// Per (shard, arm): pkg -> old/new effect. Only the two rewritten classes can move, so
// only those are scored; every other class's predicate is untouched and its verdicts
// are identical by construction.
const cells = {};
for (const d of fs.readdirSync(outroot)) {
  const deltaF = path.join(outroot, d, 'delta.json');
  const verdictF = path.join(outroot, d, 'verdicts.json');
  if (!fs.existsSync(deltaF) || !fs.existsSync(verdictF)) continue;
  const m = d.match(/^(.+)-(A0|PROD|A4)$/);
  if (!m) continue;
  const [, shard, arm] = m;
  const delta = JSON.parse(fs.readFileSync(deltaF, 'utf8'));
  const verdicts = JSON.parse(fs.readFileSync(verdictF, 'utf8'));
  for (const v of verdicts.results) {
    if (!OLD_WEAK.has(v.class)) continue;
    const detail = delta.owner_detail[`${v.pkg}@${v.version}`];
    const created = detail?.created || [];
    const kind = delta.by_owner[`${v.pkg}@${v.version}`]?.kind
      ?? ((delta.installed_cells || []).includes(`${v.pkg}@${v.version}`) ? 'installed-no-delta' : 'absent');
    // The old rule, applied to the same delta.
    const oldEffect = created.length >= 1;
    const oldVerdict =
      kind === 'absent' ? 'NOT-INSTALLED'
      : kind === 'installed-no-delta' || kind === 'pm-materialisation' ? 'NEVER-RAN-ITS-REAL-PATH'
      : oldEffect ? 'DID-WORK-AND-SUCCEEDED'
      : created.length ? 'DID-WORK-AND-FAILED'
      : 'NEVER-RAN-ITS-REAL-PATH';
    cells[`${shard}|${v.pkg}`] ??= { pkg: v.pkg, class: v.class, shard };
    cells[`${shard}|${v.pkg}`][arm] = { old: oldVerdict, new: v.verdict, created: created.length, artifacts: v.artifacts };
  }
}

const S = 'DID-WORK-AND-SUCCEEDED';
const tally = { old: { ok: 0, brk: 0, inadm: 0 }, new: { ok: 0, brk: 0, inadm: 0 } };
const moved = [];
for (const c of Object.values(cells)) {
  if (!c.A0 || !c.PROD) continue;
  for (const which of ['old', 'new']) {
    if (c.A0[which] !== S) tally[which].inadm++;
    else if (c.PROD[which] === S) tally[which].ok++;
    else tally[which].brk++;
  }
  const oldCost = c.A0.old !== S ? 'inadmissible' : c.PROD.old === S ? 'OK' : 'BREAK';
  const newCost = c.A0.new !== S ? 'inadmissible' : c.PROD.new === S ? 'OK' : 'BREAK';
  if (oldCost !== newCost) moved.push({ ...c, oldCost, newCost });
}

console.log(`same-run comparison over ${Object.keys(cells).length} cells in the two rewritten classes`);
console.log(`(every other class's predicate is unchanged, so its verdicts are identical by construction)\n`);
console.log(`  OLD predicates: survives ${tally.old.ok}  breaks ${tally.old.brk}  inadmissible ${tally.old.inadm}`);
console.log(`  NEW predicates: survives ${tally.new.ok}  breaks ${tally.new.brk}  inadmissible ${tally.new.inadm}`);
console.log(`\n  rows whose verdict MOVED: ${moved.length}`);
const byMove = {};
for (const m of moved) byMove[`${m.oldCost} -> ${m.newCost}`] = (byMove[`${m.oldCost} -> ${m.newCost}`] || 0) + 1;
console.log('  ', JSON.stringify(byMove, null, 2));
console.log('\n  OK -> not-OK, with what the package actually created:');
for (const m of moved.filter((m) => m.oldCost === 'OK' && m.newCost !== 'OK')) {
  const biggest = (m.PROD.artifacts || [])[0];
  console.log(
    `    ${m.pkg.padEnd(38)} [${m.class}] ${m.oldCost}->${m.newCost}  created=${m.PROD.created}` +
      (biggest ? `  largest: ${biggest.p} ${biggest.sz}b mode=${biggest.mode}` : '  (nothing created)'),
  );
}
fs.writeFileSync(process.env.COMPARE_OUT || 'compare-predicates.json', JSON.stringify({ tally, moved }, null, 2));

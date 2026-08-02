#!/usr/bin/env node
// Summarise a sweep's records ON DISK while it runs, so a systematic fault is caught in the first
// few packages rather than after the last one.
//
// It reads `results.json` files, NOT the batch's stdout: a record is the durable artifact, stdout
// is a stream that a restart loses. Two things it deliberately surfaces loudly, because both have
// already cost a full sweep:
//
//   - ANY verdict that is not MINIMUM. A run of HARNESS-ERROR means the harness is broken, not
//     that a hundred packages are; the first one is the signal and the other ninety-nine are noise.
//   - The writePaths FREQUENCY table. An entry appearing across many unrelated packages is not a
//     property of those packages — it is the environment leaking in, and it belongs in the global
//     baseline rather than in N per-package grants.
import fs from 'node:fs';
import path from 'node:path';

const dir = process.argv[2] ?? path.join(import.meta.dirname, 'results', 'runs');
const since = Number(process.argv[3] ?? 0);

const records = [];
const walk = (d) => {
  let ents = [];
  try { ents = fs.readdirSync(d, { withFileTypes: true }); } catch { return; }
  for (const e of ents) {
    const p = path.join(d, e.name);
    if (e.isDirectory()) walk(p);
    else if (e.name === 'results.json') {
      try {
        const st = fs.statSync(p);
        if (st.mtimeMs < since) continue;
        records.push(JSON.parse(fs.readFileSync(p, 'utf8')));
      } catch {}
    }
  }
};
walk(dir);

if (!records.length) { console.log('no records yet'); process.exit(0); }

const by = (f) => records.reduce((m, r) => (m[f(r) ?? '?'] = (m[f(r) ?? '?'] ?? 0) + 1, m), {});
const tally = (o) => Object.entries(o).sort((a, b) => b[1] - a[1]);

// ⛔ COVERAGE FIRST, ALWAYS. Pass a worklist as argv[4] and this reports what was NOT measured.
// A sweep that silently dropped half its worklist once read as a finished corpus, and the
// surviving half was a biased sample — the heavy native builds are exactly the ones that fail.
const worklist = process.argv[4];
if (worklist) {
  const want = fs.readFileSync(worklist, 'utf8').split('\n').map((s) => s.trim()).filter((s) => s && !s.startsWith('#'));
  const have = new Set(records.map((r) => `${r.pkg}@${r.version}`));
  // Count only records that are IN the worklist. Counting every MINIMUM record on disk inflates
  // coverage with packages from earlier runs, which is the same class of error as reporting a
  // sweep's survivors as its result.
  const wantSet = new Set(want);
  const measured = records.filter((r) => r.verdict === 'MINIMUM' && wantSet.has(`${r.pkg}@${r.version}`));
  const miss = want.filter((w) => !have.has(w));
  const other = records.filter((r) => !wantSet.has(`${r.pkg}@${r.version}`)).length;
  console.log(`COVERAGE           ${measured.length}/${want.length} of the worklist MEASURED`
    + (other ? `   (+${other} record(s) on disk from other runs, not counted)` : ''));
  if (miss.length) {
    console.log(`  ⛔ ${miss.length} have NO RECORD AT ALL: ${miss.slice(0, 8).join(', ')}${miss.length > 8 ? ' …' : ''}`);
    console.log('  Any conclusion drawn from the rest is a BIASED SAMPLE, not the corpus.');
  }
}

console.log(`records            ${records.length}`);
for (const [k, n] of tally(by((r) => r.verdict))) console.log(`  ${k.padEnd(28)} ${n}`);

const bad = records.filter((r) => r.verdict !== 'MINIMUM');
if (bad.length) {
  console.log(`\n⚠ ${bad.length} NON-MINIMUM — check the FIRST one, not the count:`);
  for (const r of bad.slice(0, 6)) {
    console.log(`  ${r.pkg}@${r.version}  ${r.verdict}`);
    if (r.why) console.log(`     ${r.why.slice(0, 150)}`);
    if (r.failureSignature) console.log(`     sig: ${r.failureSignature.slice(0, 150)}`);
    if (r.stderrTail) console.log(`     stderr: ${r.stderrTail.trim().split('\n').slice(-2).join(' | ').slice(0, 150)}`);
  }
}

const granted = records.filter((r) => r.verdict === 'MINIMUM' && r.state && r.state !== '(nothing)');
console.log(`\nneeded a grant     ${granted.length} of ${records.filter((r) => r.verdict === 'MINIMUM').length} measured`);
for (const [k, n] of tally(by((r) => (r.verdict === 'MINIMUM' ? r.state : null))).filter(([k]) => k !== '?')) {
  console.log(`  ${k.padEnd(28)} ${n}`);
}

// FREQUENCY, not presence: one package writing a path is that package's business; forty packages
// writing the same path is the environment, and the fix is a baseline entry rather than N grants.
const wp = {};
for (const r of records) for (const e of r.writePaths ?? []) wp[e] = (wp[e] ?? 0) + 1;
if (Object.keys(wp).length) {
  console.log('\nwritePaths by frequency (>=3 across unrelated packages ⇒ consider the baseline):');
  for (const [k, n] of tally(wp)) console.log(`  ${String(n).padStart(3)}  ${k}`);
}

const conclusive = records.filter((r) => r.declaresInstallScript && r.projectAxisConclusive === false);
if (conclusive.length) {
  console.log(`\n⚠ ${conclusive.length} declared an install script but could NOT see the project axis`);
  console.log(`  (placement did not materialize them — the grant says nothing about that axis)`);
  for (const r of conclusive.slice(0, 6)) console.log(`    ${r.pkg}@${r.version}`);
}

const revs = tally(by((r) => r.provenance?.harnessSha256));
if (revs.length > 1) console.log(`\n⚠ records span ${revs.length} harness revisions: ${revs.map(([k, n]) => `${k?.slice(0, 8)}:${n}`).join('  ')}`);

#!/usr/bin/env node
// aggregate.mjs — fold every report.json into the per-package verdict table.
//
// THE VALIDITY GATE IS APPLIED HERE, NOT IN THE RUNNER. A jail-arm verdict is only
// admissible if the SAME package on the SAME shard reached its class effect with the
// jail OFF. Without that denominator, a package that never runs its real path (a
// downloader whose optional dep already resolved, a codegen step with no input)
// scores as "works under the jail" — worthless, and wrong in the reassuring
// direction. A0-failing rows are reported as INVALID-FIXTURE / NO-OP-BY-DESIGN
// rather than folded into any compatibility number.
import fs from 'node:fs';
import path from 'node:path';

const root = process.argv[2] || '/private/tmp/corpus-study/out';
const reports = [];
for (const d of fs.readdirSync(root)) {
  const f = path.join(root, d, 'report.json');
  if (fs.existsSync(f)) reports.push(JSON.parse(fs.readFileSync(f, 'utf8')));
}

// An arm whose effect was not confirmed measured something other than what it claims.
// Refuse to aggregate it rather than quietly averaging a bad run into the totals.
const bad = reports.filter((r) => r.arm_effect !== 'confirmed');
if (bad.length) {
  console.log('### REFUSED — arm effect not confirmed, these runs are INADMISSIBLE');
  for (const r of bad) console.log(`  ${r.shard}/${r.arm}/${r.platform}: ${r.arm_effect}`);
}
// A run whose lifecycle-script window never went quiet took its post-snapshot over a
// tree that was still being written, so its delta is partial and every verdict
// derived from it is unsound in the NEVER-RAN direction. Refuse it for the same
// reason an unconfirmed arm is refused.
const stillRunning = reports.filter((r) => r.arm_effect === 'confirmed' && r.settle_state === 'TIMEOUT');
if (stillRunning.length) {
  console.log('### REFUSED — the script window never closed, these runs are INADMISSIBLE');
  for (const r of stillRunning) console.log(`  ${r.shard}/${r.arm}/${r.platform}: settle TIMEOUT after ${r.settle_waited_s}s`);
}
const ok = reports.filter((r) => r.arm_effect === 'confirmed' && r.settle_state !== 'TIMEOUT');

// The reap-gap measurement, carried into every aggregate rather than needing a
// separate log dig. Six post-approve lines is the harness's own trailing output;
// anything above it is lifecycle-script output that arrived after `nub
// approve-builds` had already returned to its caller.
const HARNESS_TRAILING_LINES = 6;
const leaks = ok
  .filter((r) => (r.post_approve_log_lines || 0) > HARNESS_TRAILING_LINES)
  .map((r) => ({ shard: r.shard, arm: r.arm, platform: r.platform, leaked_lines: r.post_approve_log_lines - HARNESS_TRAILING_LINES, settled_after_s: r.settle_waited_s }));

// key: pkg -> shard -> platform -> arm -> row
const table = {};
for (const r of ok) {
  for (const v of r.verdicts) {
    const k = `${v.pkg}@${v.version}`;
    table[k] ??= { class: v.class, cells: {} };
    table[k].cells[`${r.shard}|${r.platform}|${r.arm}`] = {
      verdict: v.verdict, changed: v.changed, kind: v.kind, effect: v.effect,
      reasons: v.reasons, artifacts: v.artifacts, binary_evidence: v.binary_evidence,
      fail: r.failure_signatures_by_package[v.pkg] || null,
      // The shard's whole denied set, attached to every row on that shard. An
      // unattributed host is still the carve-out candidate a break needs explained,
      // and dropping it because no line named an owner is how the list gets short.
      shard_denied_hosts: r.denied_hosts_all || [],
      shard_denied_paths: r.denied_paths_all || [],
    };
  }
}

const rows = [];
for (const [pkg, e] of Object.entries(table)) {
  for (const shard of new Set(Object.keys(e.cells).map((c) => c.split('|')[0]))) {
    for (const plat of new Set(Object.keys(e.cells).map((c) => c.split('|')[1]))) {
      const a0 = e.cells[`${shard}|${plat}|A0`];
      const prod = e.cells[`${shard}|${plat}|PROD`];
      // A4 — the project-grant-dropped arm. This loop previously read only A0 and
      // PROD, so an A4 run contributed report files, was counted in `runs`, and then
      // vanished: the arm looked empty rather than unanalysed.
      const a4 = e.cells[`${shard}|${plat}|A4`];
      if (!a0 && !prod && !a4) continue;
      // The A0 gate. `class has no reliable work-signal` => UNSIGNALLED, which is an
      // honest "cannot be measured", NOT a pass.
      let admissible, note;
      if (!a0) { admissible = false; note = 'no A0 denominator on this shard/platform'; }
      else if (a0.verdict === 'UNSIGNALLED') { admissible = false; note = 'UNSIGNALLED class — no work-signal exists'; }
      else if (a0.verdict === 'NOT-INSTALLED') { admissible = false; note = 'not installed even with the jail off'; }
      else if (a0.verdict !== 'DID-WORK-AND-SUCCEEDED') {
        admissible = false;
        note = a0.verdict === 'NEVER-RAN-ITS-REAL-PATH'
          ? 'NO-OP-BY-DESIGN — the script does nothing on this path even unconfined'
          : 'INVALID-FIXTURE — the class effect is unreachable even unconfined';
      } else { admissible = true; note = ''; }
      const jail_cost = admissible && prod ? (prod.verdict === 'DID-WORK-AND-SUCCEEDED' ? 'OK' : `BREAK:${prod.verdict}`) : null;
      rows.push({
        pkg, class: e.class, shard, platform: plat,
        a0: a0?.verdict ?? '-', prod: prod?.verdict ?? '-', a4: a4?.verdict ?? '-',
        a0_changed: a0?.changed ?? null, prod_changed: prod?.changed ?? null, a4_changed: a4?.changed ?? null,
        admissible, note,
        // The jail's cost, only where the denominator holds.
        jail_cost,
        // A4's cost is measured against the SAME A0 denominator, not against PROD:
        // a row that already breaks under PROD tells you nothing about the project
        // grant, so `grant_cost` is only meaningful where jail_cost is OK.
        grant_cost: admissible && a4 ? (a4.verdict === 'DID-WORK-AND-SUCCEEDED' ? 'OK' : `BREAK:${a4.verdict}`) : null,
        needs_project_grant: jail_cost === 'OK' && a4 ? a4.verdict !== 'DID-WORK-AND-SUCCEEDED' : null,
        a0_binary_evidence: a0?.binary_evidence ?? null,
        prod_binary_evidence: prod?.binary_evidence ?? null,
        a0_artifacts: a0?.artifacts ?? null,
        prod_reasons: prod?.reasons ?? null,
        // What this package was denied, for the carve-out list. Package-attributed
        // first; the shard-wide union rides along for a break whose own lines named
        // no owner, and is labelled as such so the two are never conflated.
        prod_denied_hosts: prod?.fail?.denied_hosts ?? [],
        prod_denied_paths: prod?.fail?.denied_paths ?? [],
        prod_shard_denied_hosts: prod?.shard_denied_hosts ?? [],
        prod_shard_denied_paths: prod?.shard_denied_paths ?? [],
        a4_denied_paths: a4?.fail?.denied_paths ?? [],
        prod_failure_signature: prod?.fail ? Object.fromEntries(Object.entries(prod.fail).filter(([k]) => k !== 'samples')) : null,
        prod_failure_samples: prod?.fail?.samples ?? null,
      });
    }
  }
}

rows.sort((a, b) => (a.shard + a.platform + a.pkg).localeCompare(b.shard + b.platform + b.pkg));
const outFile = process.env.AGGREGATE_OUT || path.join(root, '../verdict-table.json');
fs.writeFileSync(
  outFile,
  JSON.stringify(
    {
      // The predicates THIS table was produced by, recorded alongside it. A verdict
      // table whose predicate set is not pinned cannot be compared against another one.
      predicate_spec: ok[0]?.predicate_spec ?? null,
      runs: ok.map((r) => ({
        shard: r.shard, arm: r.arm, platform: r.platform, lever: r.lever,
        arm_effect: r.arm_effect, node_gyp: r.node_gyp_identity,
        settle_state: r.settle_state, settle_waited_s: r.settle_waited_s,
        post_approve_log_lines: r.post_approve_log_lines,
        rc_install: r.rc_install, rc_script: r.rc_script,
      })),
      refused: [...bad, ...stillRunning].map((r) => ({ shard: r.shard, arm: r.arm, platform: r.platform, arm_effect: r.arm_effect, settle_state: r.settle_state })),
      reap_gap: leaks,
      denied_hosts_union: [...new Set(ok.flatMap((r) => r.denied_hosts_all || []))].sort(),
      denied_paths_union: [...new Set(ok.flatMap((r) => r.denied_paths_all || []))].sort(),
      rows,
    },
    null,
    2,
  ),
);

const pad = (s, n) => String(s).padEnd(n).slice(0, n);
console.log('\n' + pad('package', 34) + pad('class', 22) + pad('shard', 10) + pad('plat', 7) + pad('A0 (jail off)', 25) + pad('PROD (jail on)', 25) + 'admissible / note');
console.log('-'.repeat(160));
for (const r of rows) {
  console.log(pad(r.pkg, 34) + pad(r.class, 22) + pad(r.shard, 10) + pad(r.platform, 7) + pad(r.a0, 25) + pad(r.prod, 25) + (r.admissible ? (r.jail_cost === 'OK' ? 'YES  ok' : `YES  ${r.jail_cost}`) : `no   ${r.note}`));
}

const adm = rows.filter((r) => r.admissible);
const survives = adm.filter((r) => r.jail_cost === 'OK');
const breaks = adm.filter((r) => r.jail_cost && r.jail_cost !== 'OK');
console.log(`\nadmissible cells: ${adm.length} / ${rows.length}`);
console.log(`  survives the jail : ${survives.length}`);
console.log(`  BREAKS under jail : ${breaks.length}`);

// ⭐ EVERY SURVIVAL, BROKEN DOWN BY THE PREDICATE THAT PRODUCED IT, WITH THAT
// PREDICATE'S DECLARED STRENGTH. This is printed unconditionally and up front
// because its absence is what let the previous run ship: 115 of 117 survivals rested
// on predicates satisfiable by "the script created any file at all", and nothing in
// the output said so. A number whose composition is invisible cannot be sanity
// checked, so the composition is now part of the number.
const strengthOf = {};
for (const r of ok) for (const [cls, spec] of Object.entries(r.predicate_spec || {})) strengthOf[cls] = spec.strength;
const byClass = (rs) => {
  const m = {};
  for (const r of rs) m[r.class] = (m[r.class] || 0) + 1;
  return Object.entries(m).sort((a, b) => b[1] - a[1]);
};
console.log('\n  survivals by class (and the declared strength of the predicate behind each):');
for (const [cls, n] of byClass(survives)) console.log(`    ${String(n).padStart(4)}  ${cls.padEnd(24)} strength=${strengthOf[cls] ?? '?'}`);
const weak = survives.filter((r) => ['weak', 'NONE', undefined].includes(strengthOf[r.class]));
if (weak.length) {
  console.log(`\n  ⚠️  ${weak.length} of ${survives.length} survivals rest on a WEAK-or-unknown predicate.`);
  console.log('      Do not quote the headline rate until each has a predicate that names the');
  console.log("      artifact its class exists to produce. See classes.json's 2026-07-28 note.");
}
console.log('\n  breaks by class:');
for (const [cls, n] of byClass(breaks)) console.log(`    ${String(n).padStart(4)}  ${cls}`);

// ── THE CARVE-OUT DELIVERABLE ────────────────────────────────────────────────
// One line per break naming what it was denied. This, not the percentage, is what
// the study exists to produce: each host is a DOWNLOAD_HOSTS candidate and each path
// is a filesystem-grant candidate.
console.log('\n  per-break carve-out candidates (host or path):');
for (const r of breaks) {
  const hosts = r.prod_denied_hosts.length ? r.prod_denied_hosts : r.prod_shard_denied_hosts;
  const scope = r.prod_denied_hosts.length ? 'pkg' : 'shard-union';
  const paths = r.prod_denied_paths.length ? r.prod_denied_paths : [];
  console.log(
    `    ${r.pkg.padEnd(38)} ${r.platform.padEnd(6)} ${r.jail_cost}` +
      (hosts.length ? `\n        net[${scope}]: ${hosts.join(', ')}` : '') +
      (paths.length ? `\n        fs: ${paths.join(', ')}` : '') +
      (!hosts.length && !paths.length ? '\n        (no denial signature — cause unresolved)' : ''),
  );
}

// A4, analysed against the same A0 denominator rather than dropped on the floor.
const a4rows = adm.filter((r) => r.a4 !== '-');
if (a4rows.length) {
  const needs = a4rows.filter((r) => r.needs_project_grant === true);
  console.log(`\n  A4 (project read grant DROPPED): ${a4rows.length} admissible cells measured`);
  console.log(`    survive PROD but BREAK without the project grant : ${needs.length}`);
  for (const r of needs) console.log(`      ${r.pkg.padEnd(38)} ${r.platform.padEnd(6)} ${r.grant_cost}  paths: ${(r.a4_denied_paths || []).join(', ') || '(none captured)'}`);
}

if (leaks.length) {
  console.log('\n  ⚠️  lifecycle-script output arrived AFTER `nub approve-builds` returned:');
  for (const l of leaks) console.log(`      ${l.shard}/${l.arm}/${l.platform}: ${l.leaked_lines} lines, tree went quiet ${l.settled_after_s}s later`);
  console.log('      Probable nub defect: the per-package opt-out path does not reap its');
  console.log('      children. The settle gate keeps these runs measurable; it is not a fix.');
}

const inad = {};
for (const r of rows.filter((r) => !r.admissible)) inad[r.note] = (inad[r.note] || 0) + 1;
console.log('\n  inadmissible reasons:', JSON.stringify(inad, null, 2));

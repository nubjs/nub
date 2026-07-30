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
const ok = reports.filter((r) => r.arm_effect === 'confirmed');

// key: pkg -> shard -> platform -> arm -> row
const table = {};
for (const r of ok) {
  for (const v of r.verdicts) {
    const k = `${v.pkg}@${v.version}`;
    table[k] ??= { class: v.class, cells: {} };
    table[k].cells[`${r.shard}|${r.platform}|${r.arm}`] = {
      verdict: v.verdict, changed: v.changed, kind: v.kind, effect: v.effect,
      fail: r.failure_signatures_by_package[v.pkg] || null,
    };
  }
}

const rows = [];
for (const [pkg, e] of Object.entries(table)) {
  for (const shard of new Set(Object.keys(e.cells).map((c) => c.split('|')[0]))) {
    for (const plat of new Set(Object.keys(e.cells).map((c) => c.split('|')[1]))) {
      const a0 = e.cells[`${shard}|${plat}|A0`];
      const prod = e.cells[`${shard}|${plat}|PROD`];
      if (!a0 && !prod) continue;
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
      rows.push({
        pkg, class: e.class, shard, platform: plat,
        a0: a0?.verdict ?? '-', prod: prod?.verdict ?? '-',
        a0_changed: a0?.changed ?? null, prod_changed: prod?.changed ?? null,
        admissible, note,
        // The jail's cost, only where the denominator holds.
        jail_cost: admissible && prod ? (prod.verdict === 'DID-WORK-AND-SUCCEEDED' ? 'OK' : `BREAK:${prod.verdict}`) : null,
        prod_failure_signature: prod?.fail ? Object.fromEntries(Object.entries(prod.fail).filter(([k]) => k !== 'samples')) : null,
        prod_failure_samples: prod?.fail?.samples ?? null,
      });
    }
  }
}

rows.sort((a, b) => (a.shard + a.platform + a.pkg).localeCompare(b.shard + b.platform + b.pkg));
fs.writeFileSync(path.join(root, '../verdict-table.json'), JSON.stringify({ runs: ok.map((r) => ({ shard: r.shard, arm: r.arm, platform: r.platform, lever: r.lever, arm_effect: r.arm_effect, node_gyp: r.node_gyp_identity, rc_install: r.rc_install, rc_script: r.rc_script })), rows }, null, 2));

const pad = (s, n) => String(s).padEnd(n).slice(0, n);
console.log('\n' + pad('package', 34) + pad('class', 22) + pad('shard', 10) + pad('plat', 7) + pad('A0 (jail off)', 25) + pad('PROD (jail on)', 25) + 'admissible / note');
console.log('-'.repeat(160));
for (const r of rows) {
  console.log(pad(r.pkg, 34) + pad(r.class, 22) + pad(r.shard, 10) + pad(r.platform, 7) + pad(r.a0, 25) + pad(r.prod, 25) + (r.admissible ? (r.jail_cost === 'OK' ? 'YES  ok' : `YES  ${r.jail_cost}`) : `no   ${r.note}`));
}

const adm = rows.filter((r) => r.admissible);
console.log(`\nadmissible cells: ${adm.length} / ${rows.length}`);
console.log(`  survives the jail : ${adm.filter((r) => r.jail_cost === 'OK').length}`);
console.log(`  BREAKS under jail : ${adm.filter((r) => r.jail_cost && r.jail_cost !== 'OK').length}`);
const inad = {};
for (const r of rows.filter((r) => !r.admissible)) inad[r.note] = (inad[r.note] || 0) + 1;
console.log('  inadmissible reasons:', JSON.stringify(inad, null, 2));

#!/usr/bin/env node
// summarize-repeats.mjs — fold N independent passes of the same matrix into a RANGE
// and a MODAL value, per package and in total.
//
// WHY A SINGLE PASS IS NOT A RESULT. Seven repeats of one shard produced survive
// counts of {17, 18}, and three packages — cpu-features, @sitespeed.io/chromedriver,
// dprint — succeeded in exactly 1 of the 7. Roughly 4% of verdicts are unstable
// across identical runs, which has two consequences the harness must enforce rather
// than leave to the reader's discipline:
//
//   1. A point estimate is a fiction. Report the range across passes and the modal
//      value, never the number one pass happened to produce.
//   2. A single-run differential of size 1 is noise. A prior A4 finding — "one
//      package needs the project read grant" — was produced by exactly that, and its
//      own control then refuted it. So a per-package verdict CHANGE is only reported
//      here when it is consistent across passes; anything that varies within an arm
//      is emitted as `unstable` and must not be quoted as a finding.
//
// usage: node summarize-repeats.mjs <verdict-table.json> [<verdict-table.json> ...]

import fs from 'node:fs';

const files = process.argv.slice(2);
if (!files.length) {
  console.error('usage: summarize-repeats.mjs <verdict-table.json>...');
  process.exit(2);
}
const passes = files.map((f) => ({ file: f, table: JSON.parse(fs.readFileSync(f, 'utf8')) }));

const mode = (xs) => {
  const m = {};
  for (const x of xs) m[x] = (m[x] || 0) + 1;
  return Object.entries(m).sort((a, b) => b[1] - a[1] || String(a[0]).localeCompare(String(b[0])))[0];
};
const fmtRange = (xs) => {
  const lo = Math.min(...xs), hi = Math.max(...xs);
  return lo === hi ? `${lo}` : `${lo}–${hi}`;
};

// ── the headline, per platform, as a range + a mode ───────────────────────────
const platforms = [...new Set(passes.flatMap((p) => p.table.rows.map((r) => r.platform)))];
const summary = {};
for (const plat of platforms) {
  const per = passes.map(({ table }) => {
    const adm = table.rows.filter((r) => r.admissible && r.platform === plat);
    const ok = adm.filter((r) => r.jail_cost === 'OK').length;
    return { admissible: adm.length, survives: ok, breaks: adm.length - ok };
  });
  const [modeSurv, modeN] = mode(per.map((x) => x.survives));
  const [modeAdm] = mode(per.map((x) => x.admissible));
  summary[plat] = {
    passes: per.length,
    admissible_range: fmtRange(per.map((x) => x.admissible)),
    admissible_modal: Number(modeAdm),
    survives_range: fmtRange(per.map((x) => x.survives)),
    survives_modal: Number(modeSurv),
    survives_modal_seen_in: `${modeN}/${per.length} passes`,
    breaks_range: fmtRange(per.map((x) => x.breaks)),
    rate_modal: Number(modeAdm) ? `${((Number(modeSurv) / Number(modeAdm)) * 100).toFixed(1)}%` : 'n/a',
    rate_range: `${Math.min(...per.map((x) => (x.admissible ? (x.survives / x.admissible) * 100 : 0))).toFixed(1)}%–${Math.max(
      ...per.map((x) => (x.admissible ? (x.survives / x.admissible) * 100 : 0)),
    ).toFixed(1)}%`,
    per_pass: per,
  };
}

// ── per package: stable vs unstable across passes ─────────────────────────────
const byPkg = {};
for (const { table } of passes) {
  for (const r of table.rows) {
    const k = `${r.pkg}|${r.platform}`;
    byPkg[k] ??= { pkg: r.pkg, platform: r.platform, class: r.class, a0: [], prod: [], cost: [], hosts: new Set(), paths: new Set() };
    byPkg[k].a0.push(r.a0);
    byPkg[k].prod.push(r.prod);
    byPkg[k].cost.push(r.admissible ? r.jail_cost : `inadmissible:${r.note}`);
    for (const h of r.prod_denied_hosts || []) byPkg[k].hosts.add(h);
    for (const p of r.prod_denied_paths || []) byPkg[k].paths.add(p);
  }
}

const stableBreaks = [], unstable = [], stableOk = [];
for (const v of Object.values(byPkg)) {
  const uniq = [...new Set(v.cost)];
  const [modal, n] = mode(v.cost);
  const rec = {
    pkg: v.pkg, platform: v.platform, class: v.class,
    modal, seen: `${n}/${v.cost.length}`, observed: uniq,
    denied_hosts: [...v.hosts], denied_paths: [...v.paths],
  };
  if (uniq.length > 1) unstable.push(rec);
  else if (String(modal).startsWith('BREAK')) stableBreaks.push(rec);
  else if (modal === 'OK') stableOk.push(rec);
}

console.log(`passes folded: ${passes.length}`);
for (const f of files) console.log(`  ${f}`);
console.log('\n=== headline, as a range and a mode (never a point estimate) ===');
console.log(JSON.stringify(summary, null, 2));

console.log(`\n=== STABLE breaks (same verdict in every pass) — ${stableBreaks.length} ===`);
for (const r of stableBreaks) {
  console.log(`  ${r.pkg.padEnd(38)} ${r.platform.padEnd(6)} ${r.modal}`);
  if (r.denied_hosts.length) console.log(`      net: ${r.denied_hosts.join(', ')}`);
  if (r.denied_paths.length) console.log(`      fs:  ${r.denied_paths.join(', ')}`);
}

console.log(`\n=== UNSTABLE across passes — ${unstable.length} — NOT quotable as findings ===`);
for (const r of unstable) console.log(`  ${r.pkg.padEnd(38)} ${r.platform.padEnd(6)} modal=${r.modal} (${r.seen})  observed: ${r.observed.join(' | ')}`);

console.log(`\n=== STABLE ok — ${stableOk.length} ===`);

fs.writeFileSync(
  process.env.REPEATS_OUT || 'repeats-summary.json',
  JSON.stringify({ passes: files, summary, stable_breaks: stableBreaks, unstable, stable_ok_count: stableOk.length }, null, 2),
);

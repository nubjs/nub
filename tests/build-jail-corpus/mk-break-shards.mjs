#!/usr/bin/env node
// mk-break-shards.mjs — emit ONE-PACKAGE shard manifests for the packages that broke
// under the jail, so each break's denied host or path can be attributed exactly.
//
// WHY A SECOND PASS IS NEEDED AT ALL. In a 50-package shard, nub does not frame
// lifecycle output per package — the approve-builds section interleaves raw subprocess
// output with no package header and no per-script rc (verified; it is why attribution
// is done by store path rather than from the log). Most denial lines therefore name no
// package: "Error fetching release: getaddrinfo EAI_AGAIN workers.cloudflare.com" is
// the entire line. errsig.mjs has a proximity cursor for these, but proximity is a
// LEAD, not an attribution, and the carve-out list is the study's deliverable — it has
// to be right, not plausible.
//
// A one-package shard removes the ambiguity structurally: every line in the log
// belongs to the one package under test. The break set is small (order tens), so this
// costs minutes, and it is the difference between "these four hosts were denied
// somewhere in shard 3" and "this package needs this host".
//
// Capabilities are carried over from the source manifest, so a package whose real path
// needs a git repo or a Prisma schema still gets one. Without that the re-run measures
// a bail-out and reports a break that is really a missing fixture.
//
// usage: node mk-break-shards.mjs <verdict-table.json> <outdir> [shard-*.tsv ...]

import fs from 'node:fs';
import path from 'node:path';

// COMMA-SEPARATED TABLES, because the break set is not stable across passes. Roughly
// 4% of verdicts move between identical runs, and at this corpus's admissible size
// (order ten) that is most of the set: three Linux passes produced 1 stable break and
// 31 rows that varied. Taking the UNION over passes is what makes the attribution
// pass cover everything worth attributing; the per-pass modal verdict is still what
// the headline is computed from.
const [tableArg, outDir, ...manifests] = process.argv.slice(2);
if (!tableArg || !outDir) {
  console.error('usage: mk-break-shards.mjs <verdict-table.json[,more.json]> <outdir> [shard-*.tsv ...]');
  process.exit(2);
}
const tables = tableArg.split(',').map((f) => JSON.parse(fs.readFileSync(f, 'utf8')));
const table = { rows: tables.flatMap((t) => t.rows) };

// The capability + version + class rows, from whichever shard manifests were passed.
const src = new Map();
for (const m of manifests.length ? manifests : fs.readdirSync('.').filter((f) => /^shard-default\d+\.tsv$/.test(f))) {
  for (const line of fs.readFileSync(m, 'utf8').split('\n')) {
    if (!line.trim() || line.startsWith('#')) continue;
    const [pkg, version, cls, needs] = line.split('\t');
    src.set(pkg, { pkg, version, cls, needs: needs || '', shard: path.basename(m, '.tsv').replace(/^shard-/, '') });
  }
}

// Every admissible break, plus every row whose PROD arm carried a denial signature even
// where the row was inadmissible. The second group matters: a package whose A0 fails the
// tightened predicate is not measurable, but if it was DENIED A HOST that is still a
// carve-out candidate, and dropping it would shorten exactly the list this study exists
// to produce.
const wanted = new Map();
for (const r of table.rows) {
  const broke = r.admissible && r.jail_cost && r.jail_cost !== 'OK';
  const denied = (r.prod_denied_hosts?.length || 0) + (r.prod_denied_paths?.length || 0) > 0;
  if (!broke && !denied) continue;
  // The verdict table keys a row `name@version`; a manifest's first column is the
  // bare name. Matching them directly finds nothing — silently, producing zero shards
  // and an empty carve-out list that looks like "no breaks" rather than a lookup bug.
  // Scoped names carry their own @, so split on the LAST one.
  const name = r.pkg.slice(0, r.pkg.lastIndexOf('@'));
  const s = src.get(name) || src.get(r.pkg);
  if (!s) { console.error(`skip ${r.pkg}: not found in any source manifest`); continue; }
  wanted.set(name, { ...s, reason: broke ? r.jail_cost : 'denial-signature-only' });
}

fs.mkdirSync(outDir, { recursive: true });
const index = [];
let n = 0;
for (const w of wanted.values()) {
  const name = `break${String(++n).padStart(3, '0')}`;
  fs.writeFileSync(path.join(outDir, `shard-${name}.tsv`), `${w.pkg}\t${w.version}\t${w.cls}\t${w.needs}\n`);
  index.push({ shard: name, pkg: w.pkg, version: w.version, class: w.cls, needs: w.needs, from_shard: w.shard, reason: w.reason });
}
fs.writeFileSync(path.join(outDir, 'index.json'), JSON.stringify(index, null, 2));
console.log(`${index.length} single-package shards -> ${outDir}`);
for (const i of index) console.log(`  ${i.shard}  ${i.pkg}@${i.version} [${i.class}] needs=${i.needs || '-'}  (${i.reason})`);

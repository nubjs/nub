#!/usr/bin/env node
// carveouts.mjs — per package, the hosts it needs and the paths it was denied.
//
// THIS IS THE STUDY'S DELIVERABLE, and it is derived from the A0 arm rather than
// from denial messages, because denial messages are not available on both platforms:
//
//   Linux   seccomp refuses socket(2) before any address is resolved, so the failure
//           surfaces as `getaddrinfo EAI_AGAIN <host>` and the host IS in the log.
//   macOS   egress is pinned to the proxy and a non-allowlisted host is refused BY
//           THE PROXY. Measured across three full passes: ZERO host denials appear in
//           any log. The method notes macOS denial telemetry is absent, and this
//           confirms it for the network axis specifically.
//
// So asking "what was it denied?" answers the question on one platform and returns
// nothing on the other. Asking "what did it CONTACT when it was allowed to work?"
// answers it on both, from the arm that by construction did the work — the A0 arm is
// already required as the validity denominator, so this costs no extra run.
//
// A host is reported only for a package whose A0 arm actually reached its class
// effect; a package that no-ops unconfined contacts nothing worth carving out.
//
// Run this over an OUTROOT of SINGLE-PACKAGE shards (mk-break-shards.mjs) so every
// line in a log belongs to the one package under test. Over multi-package shards the
// attribution is a proximity guess and is labelled as such.
//
// usage: node carveouts.mjs <outroot> [--shard-index index.json]

import fs from 'node:fs';
import path from 'node:path';

const outroot = process.argv[2];
if (!outroot) {
  console.error('usage: carveouts.mjs <outroot> [index.json]');
  process.exit(2);
}
const indexF = process.argv[3];
const index = indexF && fs.existsSync(indexF) ? JSON.parse(fs.readFileSync(indexF, 'utf8')) : null;
const shardPkg = new Map((index || []).map((i) => [i.shard, i]));

// Every absolute URL the run touched. Deliberately broader than errsig's failure
// classifier: here we want everything contacted, not everything that failed.
const URL_RE = /https?:\/\/([A-Za-z0-9._-]+\.[A-Za-z]{2,})(?::\d+)?(\/[^\s"'`)\]]*)?/g;
// Hosts that are the package manager's own traffic, not the lifecycle script's.
// Carving these out is already implied by nub being able to install at all.
const PM_HOSTS = new Set(['registry.npmjs.org', 'nodejs.org']);

const rows = [];
for (const d of fs.readdirSync(outroot)) {
  const m = d.match(/^(.+)-(A0|PROD|A4)$/);
  if (!m) continue;
  const [, shard, arm] = m;
  const logF = path.join(outroot, d, 'run.log');
  const verdictF = path.join(outroot, d, 'verdicts.json');
  if (!fs.existsSync(logF)) continue;
  const log = fs.readFileSync(logF, 'utf8');
  const hosts = new Set();
  let mm;
  URL_RE.lastIndex = 0;
  while ((mm = URL_RE.exec(log)) !== null) if (!PM_HOSTS.has(mm[1])) hosts.add(mm[1]);
  const verdicts = fs.existsSync(verdictF) ? JSON.parse(fs.readFileSync(verdictF, 'utf8')) : null;
  const meta = shardPkg.get(shard);
  // A single-package shard has exactly one manifest row, so its verdict is the
  // package's verdict with no attribution step at all.
  const v = verdicts?.results?.length === 1 ? verdicts.results[0] : null;
  rows.push({ shard, arm, pkg: meta?.pkg ?? v?.pkg ?? shard, hosts: [...hosts].sort(), verdict: v?.verdict ?? null, single: verdicts?.results?.length === 1 });
}

const byPkg = {};
for (const r of rows) {
  byPkg[r.pkg] ??= { pkg: r.pkg, single: r.single, arms: {} };
  byPkg[r.pkg].arms[r.arm] = { hosts: r.hosts, verdict: r.verdict };
}

console.log('# Carve-out candidates, per package\n');
console.log('Hosts are those the package contacted in its UNCONFINED (A0) arm — what it');
console.log('needs — with the jailed arm shown alongside. Attribution is exact where the');
console.log('shard held one package.\n');
const needHosts = [];
for (const e of Object.values(byPkg).sort((a, b) => a.pkg.localeCompare(b.pkg))) {
  const a0 = e.arms.A0, prod = e.arms.PROD;
  if (!a0) continue;
  const broke = a0.verdict === 'DID-WORK-AND-SUCCEEDED' && prod && prod.verdict !== 'DID-WORK-AND-SUCCEEDED';
  const status = !prod ? 'no jailed arm' : broke ? 'BREAKS under the jail' : prod.verdict === a0.verdict ? 'same in both arms' : `A0=${a0.verdict} PROD=${prod.verdict}`;
  console.log(`${e.pkg}${e.single ? '' : '  (multi-package shard — attribution is approximate)'}`);
  console.log(`    ${status}`);
  console.log(`    A0 contacted   : ${a0.hosts.join(', ') || '(no external host)'}`);
  if (prod) console.log(`    PROD contacted : ${prod.hosts.join(', ') || '(none)'}`);
  console.log();
  if (broke && a0.hosts.length) needHosts.push({ pkg: e.pkg, hosts: a0.hosts });
}

const union = [...new Set(needHosts.flatMap((n) => n.hosts))].sort();
console.log('## Host union across packages that break under the jail\n');
for (const h of union) console.log(`  ${h}   <- ${needHosts.filter((n) => n.hosts.includes(h)).map((n) => n.pkg).join(', ')}`);
fs.writeFileSync(process.env.CARVEOUT_OUT || 'carveouts.json', JSON.stringify({ perPackage: byPkg, needHosts, union }, null, 2));

#!/usr/bin/env node
// aggregate-vw.mjs — pair the HOST and WORLD verdicts per package and reduce the
// WORLD failures to a small number of CLASSES with a single answer each.
//
// The compatibility rate is deliberately computed over ADMISSIBLE packages only:
// the harness's validity gate excludes any package whose class effect was absent
// in the unconfined HOST arm, because a jail-arm verdict for those is not
// interpretable (README: "otherwise it is reported as NO-OP-BY-DESIGN or
// INVALID-FIXTURE rather than folded into a compatibility number").  Both the
// admissible-only rate and the raw denominator are printed so the exclusion is
// visible rather than silent.
import fs from 'node:fs';
import path from 'node:path';

const root = process.argv[2] || '/tmp/vwc';
const rows = new Map();
for (const d of fs.readdirSync(root)) {
  const f = path.join(root, d, 'verdict.json');
  if (!fs.existsSync(f)) continue;
  let v;
  try { v = JSON.parse(fs.readFileSync(f, 'utf8')); } catch { continue; }
  if (!v.pkg) continue;
  const r = rows.get(v.pkg) || { pkg: v.pkg, class: v.class, dir: {} };
  r.dir[v.arm] = { ...v, out: path.join(root, d) };
  rows.set(v.pkg, r);
}

// Failure classification.  Deliberately COARSE and evidence-driven: each bucket
// names a property of the WORLD, not a package.  A bucket that would only ever
// hold one package is reported as such rather than turned into a design input.
const MISSING_BIN = /(^|[^\w])(git|curl|wget|unzip|cmake|pkg-config|svn|hg)\b[^\n]*(not found|No such file|ENOENT|command not found)|spawn (git|curl|wget|unzip|cmake|pkg-config) ENOENT|exec: "(git|curl|wget|unzip|cmake)"/i;
const NET = /\b(getaddrinfo|ENOTFOUND|EAI_AGAIN|ECONNREFUSED|ENETUNREACH|ETIMEDOUT|ESOCKETTIMEDOUT)\b|tunneling socket|proxy/i;
const FS_DENY = /\b(EACCES|EROFS)\b|permission denied|read-only file system/i;
const GLIBC = /GLIBC_[0-9.]+.*not found|version `GLIBCXX?_|cannot open shared object/i;
const BUILDFAIL = /gyp ERR!|make: \*\*\*|fatal error:|clang: error|C\+\+ compiler/i;

function readLog(dir) { try { return fs.readFileSync(path.join(dir, 'run.log'), 'utf8'); } catch { return ''; } }

function bucket(worldDir) {
  const log = readLog(worldDir);
  const m = log.match(MISSING_BIN);
  if (m) return { cls: 'MISSING-BINARY-IN-WORLD', hint: (m[2] || m[3] || m[0]).toLowerCase(), ev: m[0].slice(0, 160) };
  let x;
  if ((x = log.match(GLIBC))) return { cls: 'ABI-MISMATCH', hint: 'glibc', ev: x[0].slice(0, 160) };
  if ((x = log.match(NET))) return { cls: 'NETWORK', hint: 'egress', ev: x[0].slice(0, 160) };
  if ((x = log.match(FS_DENY))) return { cls: 'FS-DENIAL', hint: 'path', ev: x[0].slice(0, 160) };
  if ((x = log.match(BUILDFAIL))) return { cls: 'BUILD-FAILED', hint: 'compile', ev: x[0].slice(0, 160) };
  return { cls: 'SILENT-NO-EFFECT', hint: 'exit 0, class effect absent, no diagnostic in the log', ev: '' };
}

const SUCCESS = 'DID-WORK-AND-SUCCEEDED';
const tally = { admissible: 0, pass: 0, fail: 0, inadmissible: 0, unsignalled: 0, missing_arm: 0 };
const byClass = new Map();
const perPkg = [];

for (const r of [...rows.values()].sort((a, b) => a.pkg.localeCompare(b.pkg))) {
  const H = r.dir.HOST, W = r.dir.WORLD;
  if (!H || !W) { tally.missing_arm++; perPkg.push({ pkg: r.pkg, class: r.class, state: 'MISSING-ARM' }); continue; }
  if (H.verdict === 'UNSIGNALLED' || W.verdict === 'UNSIGNALLED') {
    tally.unsignalled++; perPkg.push({ pkg: r.pkg, class: r.class, state: 'UNSIGNALLED' }); continue;
  }
  if (H.verdict !== SUCCESS) {
    // the package never reached its real path even unconfined -> not interpretable
    tally.inadmissible++;
    perPkg.push({ pkg: r.pkg, class: r.class, state: 'INADMISSIBLE', host: H.verdict, world: W.verdict });
    continue;
  }
  tally.admissible++;
  if (W.verdict === SUCCESS) {
    tally.pass++; perPkg.push({ pkg: r.pkg, class: r.class, state: 'PASS' });
  } else {
    tally.fail++;
    const b = bucket(W.out);
    const k = `${b.cls}${b.hint ? ' / ' + b.hint : ''}`;
    if (!byClass.has(k)) byClass.set(k, []);
    byClass.get(k).push({ pkg: r.pkg, class: r.class, world: W.verdict, rc: W.rc, ev: b.ev });
    perPkg.push({ pkg: r.pkg, class: r.class, state: 'FAIL', bucket: k, world: W.verdict, rc: W.rc, ev: b.ev });
  }
}

const pct = (n, d) => (d ? ((100 * n) / d).toFixed(1) + '%' : 'n/a');
console.log('=== DENOMINATORS ===');
console.log(`packages with both arms   : ${rows.size - tally.missing_arm}`);
console.log(`UNSIGNALLED (no predicate): ${tally.unsignalled}   [classes.json marks these strength=NONE]`);
console.log(`INADMISSIBLE (HOST arm did not reach the class effect): ${tally.inadmissible}`);
console.log(`ADMISSIBLE (the compatibility denominator): ${tally.admissible}`);
console.log('');
console.log('=== COMPATIBILITY ===');
console.log(`PASS  ${tally.pass}/${tally.admissible} = ${pct(tally.pass, tally.admissible)}`);
console.log(`FAIL  ${tally.fail}/${tally.admissible} = ${pct(tally.fail, tally.admissible)}`);
console.log('');
console.log('=== FAILURE CLASSES ===');
for (const [k, v] of [...byClass.entries()].sort((a, b) => b[1].length - a[1].length)) {
  console.log(`\n${k}  n=${v.length}${v.length === 1 ? '   <-- SINGLE-PACKAGE bucket' : ''}`);
  for (const p of v) console.log(`   ${p.pkg} [${p.class}] ${p.world} rc=${p.rc}  ${p.ev.replace(/\s+/g, ' ')}`);
}
fs.writeFileSync(path.join(root, 'aggregate.json'), JSON.stringify({ tally, byClass: Object.fromEntries(byClass), perPkg }, null, 2));
console.log(`\nwrote ${path.join(root, 'aggregate.json')}`);

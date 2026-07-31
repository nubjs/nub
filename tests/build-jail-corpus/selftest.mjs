#!/usr/bin/env node
// selftest.mjs — falsify the scoring gates against synthetic runs. No nub, no network.
//
// WHY A SYNTHETIC RUN AND NOT A REAL ONE. Every gate here exists because a REAL
// corpus run passed something it should have failed, and a real run cannot be
// asked to reproduce that on demand — the false pass depended on which sibling
// package happened to fail first. A synthetic snapshot pair can state the exact
// shape and assert the exact verdict, in a second, on any machine.
//
// It drives the REAL scorers end to end (delta -> shard-verdict -> errsig ->
// aggregate). A reimplementation would drift from the thing it certifies.
//
// EVERY CASE IS A DIFFERENTIAL: each asserted failure is paired with the nearest
// input that must still PASS. A gate that rejects everything is not a gate, and
// four of the five bugs these cases encode were originally hidden by exactly that
// kind of one-sided check.
//
//   node selftest.mjs

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';

const HARNESS = path.dirname(new URL(import.meta.url).pathname);
const ROOT = fs.mkdtempSync(path.join(os.tmpdir(), 'corpus-selftest-'));

const rec = (root, rel, type, size = 0) => ({
  root, rel, type, size,
  digest: type === 'file' ? `sha256:${rel}:${size}` : type === 'link' ? `link:x` : 'dir',
  mode: 420, mtime: '1', ctime: '1', btime: '1', ino: '1', nlink: 1,
});
// A cell must PRE-EXIST for its in-window additions to read as script output
// rather than as the linker materialising a package — that is the discriminator
// `cellsPresentIn` implements, and every case here honours it.
const cell = (pkg, ver, inner, type, size) =>
  rec('home', `.cache/nub/pm/store/${pkg}@${ver}-aaaaaaaabbbbbbbb/node_modules/${inner}`, type, size);

const MANIFEST = [
  ['undersized', '1.0.0', 'binary-downloader'],
  ['fullsized', '1.0.0', 'binary-downloader'],
  ['binlink-only', '1.0.0', 'binary-downloader'],
  ['scratch-only', '1.0.0', 'binary-downloader'],
  ['silent', '1.0.0', 'binary-downloader'],
  ['hookwriter', '1.0.0', 'hook-installer'],
];

// PRE: every cell exists and is empty. POST differs per arm, and each package's
// POST is the one-line statement of what that gate is about.
const pre = [];
for (const [p, v] of MANIFEST) pre.push(cell(p, v, p, 'dir'));
pre.push(rec('proj', 'package.json', 'file', 100));

const post = (arm) => {
  const o = [...pre];
  const big = arm === 'A0' ? 50_000_000 : 200_000;
  // UNDERSIZED: clears the absolute byte floor in BOTH arms, so only the
  // CROSS-ARM ratio can catch it. Without this case the ratio gate is dead code
  // that never fires and never fails.
  o.push(cell('undersized', '1.0.0', 'undersized/bin/tool', 'file', big));
  // FULLSIZED: the control. Same class, same shape, artifact intact under the
  // jail — it must survive every gate, or the gates are just rejecting the class.
  o.push(cell('fullsized', '1.0.0', 'fullsized/bin/tool', 'file', 50_000_000));
  if (arm === 'A0') {
    o.push(cell('binlink-only', '1.0.0', 'binlink-only/bin/tool', 'file', 40_000_000));
    o.push(cell('scratch-only', '1.0.0', 'scratch-only/bin/tool', 'file', 40_000_000));
    o.push(cell('silent', '1.0.0', 'silent/bin/tool', 'file', 40_000_000));
    o.push(rec('proj', '.git/hooks/pre-commit', 'file', 200));
  } else {
    // BINLINK-ONLY: the linker's bin symlinks inside the cell, and nothing else.
    // This is what `azure-functions-core-tools`, `chromium` and `cldr-data`
    // scored a confined PASS on.
    o.push(cell('binlink-only', '1.0.0', '.bin', 'dir'));
    o.push(cell('binlink-only', '1.0.0', '.bin/rimraf', 'link'));
    // SCRATCH-ONLY: nub's own jail-home file, which exists ONLY in the confined
    // arm and satisfied `min_created: 1` by itself.
    o.push(rec('home', '.cache/nub/jail-home/scratch-only-9f2a/.cache/nub/node-discovery.json', 'file', 117));
    // SILENT: nothing at all. Either the jail blocked it or it was never
    // scheduled — which of those is true is the whole question.
    // hookwriter's project effect is absent too.
  }
  return o;
};

function runArm(outRoot, arm, log) {
  const d = path.join(outRoot, `st-${arm}`);
  fs.mkdirSync(d, { recursive: true });
  const w = (f, recs) => fs.writeFileSync(path.join(d, f), recs.map((r) => JSON.stringify(r)).join('\n') + '\n');
  w('pre.ndjson', pre);
  w('post.ndjson', post(arm));
  fs.writeFileSync(path.join(d, 'run.log'), log);
  const man = path.join(d, 'manifest.tsv');
  fs.writeFileSync(man, MANIFEST.map((r) => r.join('\t') + '\t').join('\n') + '\n');

  execFileSync(process.execPath, [path.join(HARNESS, 'lib/delta.mjs'),
    path.join(d, 'pre.ndjson'), path.join(d, 'post.ndjson'), path.join(d, 'delta.json')], { stdio: ['ignore', 'ignore', 'ignore'] });
  execFileSync(process.execPath, [path.join(HARNESS, 'lib/shard-verdict.mjs'),
    path.join(d, 'delta.json'), man, path.join(d, 'run.log'), path.join(d, 'verdicts.json')], {
    stdio: ['ignore', 'ignore', 'inherit'],
    env: { ...process.env, NONCE: 'N', SHARD: 'st', ARM: arm, RC_SCRIPT: /failed for/.test(log) ? '1' : '0', RC_INSTALL: '0', PROJ: d, STOREROOT: d, STORE_BASES: '' },
  });
  execFileSync(process.execPath, [path.join(HARNESS, 'lib/errsig.mjs'),
    path.join(d, 'run.log'), man, path.join(d, 'verdicts.json'), path.join(d, 'report.json')], {
    stdio: ['ignore', 'ignore', 'inherit'],
    env: { ...process.env, ARM_EFFECT: 'confirmed', WARN_COUNT: '0', GYP_ID: '-', PLATFORM: 'selftest', LEVER: 'none', NONCE: 'N' },
  });
}

function scenario(name, log) {
  const outRoot = path.join(ROOT, name, 'out');
  fs.mkdirSync(outRoot, { recursive: true });
  runArm(outRoot, 'A0', 'Approved 6 package(s)\n');
  runArm(outRoot, 'PROD', log);
  execFileSync(process.execPath, [path.join(HARNESS, 'aggregate.mjs'), outRoot], { stdio: ['ignore', 'ignore', 'inherit'] });
  const t = JSON.parse(fs.readFileSync(path.join(ROOT, name, 'verdict-table.json'), 'utf8'));
  return Object.fromEntries(t.rows.map((r) => [r.pkg.replace(/@[^@]+$/, ''), r]));
}

const TRUNCATED = 'Approved 6 package(s)\n  × lifecycle script postinstall failed for some-other-\n  │ package@1.0.0: script `postinstall` exited with code 1\n';
const CLEAN = 'Approved 6 package(s)\n';

let failures = 0;
const check = (label, got, want) => {
  const ok = got === want;
  if (!ok) failures++;
  console.log(`${ok ? 'ok  ' : 'FAIL'}  ${label}\n        want ${want}\n        got  ${got}`);
};

const trunc = scenario('truncated', TRUNCATED);
const clean = scenario('clean', CLEAN);

console.log('\n— artifact evidence —');
check('a confined artifact 0.4% the size of the unconfined one is a FALSE PASS',
  trunc['undersized'].jail_cost, 'BREAK:UNDERSIZED-ARTIFACT');
check('CONTROL: the same class with its artifact intact still passes',
  trunc['fullsized'].jail_cost, 'OK');
check('linker bin symlinks are not the package\'s work',
  clean['binlink-only'].jail_cost, 'BREAK:NEVER-RAN-ITS-REAL-PATH');
check('nub\'s own jail-home scratch cannot satisfy a package predicate',
  clean['scratch-only'].jail_cost, 'BREAK:NEVER-RAN-ITS-REAL-PATH');
// ASSERTED AT THE DELTA, NOT THROUGH A VERDICT. The scratch file is unattributed,
// so a per-package predicate could never see it in the first place — checking it
// only through a package verdict is a test that passes whether or not the fix
// exists. What it actually contaminated is the GLOBAL created count, so that is
// what has to be zero.
const scratchDelta = JSON.parse(fs.readFileSync(path.join(ROOT, 'clean/out/st-PROD/delta.json'), 'utf8'));
check('...and it never enters the global created count',
  scratchDelta.created.some((c) => /jail-home/.test(c.rel)), false);
// Paired with the above so the check cannot pass by the scratch file simply not
// being there: the exclusion has to have FIRED.
check('...via the delta exclusion, which is recorded rather than silent',
  scratchDelta.excluded_pm_noise.created >= 1, true);

console.log('\n— scheduling attribution —');
check('silence in a TRUNCATED window is inconclusive, not a break',
  trunc['silent'].admissible === false && /INCONCLUSIVE-SCHEDULING/.test(trunc['silent'].note), true);
check('CONTROL: the same silence in an UNTRUNCATED window IS a break',
  clean['silent'].jail_cost, 'BREAK:NEVER-RAN-ITS-REAL-PATH');
check('a truncated window does not poison a verdict backed by positive evidence',
  trunc['fullsized'].jail_cost, 'OK');

console.log('\n— effect outside the package\'s own cell —');
check('a hook installer that wrote no hook breaks',
  clean['hookwriter'].jail_cost, 'BREAK:NEVER-RAN-ITS-REAL-PATH');
check('CONTROL: its A0 arm, which DID write the hook, is the denominator',
  clean['hookwriter'].a0, 'DID-WORK-AND-SUCCEEDED');

if (!process.env.KEEP) fs.rmSync(ROOT, { recursive: true, force: true });
console.log(`\n${failures ? `${failures} FAILED` : 'all gates hold'}`);
process.exit(failures ? 1 : 0);

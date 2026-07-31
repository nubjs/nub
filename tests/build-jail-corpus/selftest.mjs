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
  // The shard SIBLING whose `.git/hooks` write satisfied every other
  // hook-installer row's project-scoped predicate. Nothing else in this file
  // needs it; its whole job is to be the row that did the work someone else got
  // scored for.
  ['noop-hook', '1.0.0', 'hook-installer'],
  ['partial-build', '1.0.0', 'binary-downloader'],
  ['late-fail-prod', '1.0.0', 'binary-downloader'],
  ['opaque-fail', '1.0.0', 'binary-downloader'],
];

// PRE: every cell exists and is empty. POST differs per arm, and each package's
// POST is the one-line statement of what that gate is about.
const pre = [];
for (const [p, v] of MANIFEST) pre.push(cell(p, v, p, 'dir'));
pre.push(rec('proj', 'package.json', 'file', 100));

// `hooksGranted` models the jail WITH a `.git/hooks` write grant: the project
// effect is present in both arms. It is the control for the no-op gate's second
// scoping — identical windows must not strip a package the jail did not break.
const post = (arm, { hooksGranted = false } = {}) => {
  const o = [...pre];
  if (hooksGranted && arm !== 'A0') o.push(rec('proj', '.git/hooks/pre-commit', 'file', 200));
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
    // hookwriter's AND noop-hook's project effect are both absent — which is
    // the whole shape of the manufactured hook-installer break: one shared
    // project delta, so both rows flip together whatever either package did.
  }
  // PARTIAL-BUILD and LATE-FAIL-PROD both produce a FULL artifact in both arms,
  // so no size-scaled gate can separate them from a clean success. The only
  // thing that distinguishes them is the exit code of their own window.
  o.push(cell('partial-build', '1.0.0', 'partial-build/build/Release/x.node', 'file', 40_000_000));
  o.push(cell('late-fail-prod', '1.0.0', 'late-fail-prod/bin/tool', 'file', 40_000_000));
  o.push(cell('opaque-fail', '1.0.0', 'opaque-fail/bin/tool', 'file', 40_000_000));
  return o;
};

// PER-PACKAGE WINDOWS, which is what the runner emits under `PER_PKG=1` and
// what both the exit-code veto and the cross-arm window comparison read. The
// shard header stays the FIRST line: `approved` is parsed from the first match,
// and the scheduling gate needs the shard's count, not a window's.
const win = (pkg, rc, body) => `--- window: ${pkg} (rc=${rc})\n${body}\n`;
const OPTOUT = (p) => `warning: ${p} build scripts are running without the build sandbox (dependenciesMeta.${p}.sandbox is false in package.json)`;

const mkLog = (arm, dir, header) => {
  const A0 = arm === 'A0';
  const w = (pkg, rc, body) => win(pkg, rc, (A0 ? OPTOUT(pkg) + '\n' : '') + body);
  return (
    header
    + w('undersized', 0, 'done')
    // The CONTROL for the unclassified fallback: it prints a problem-shaped word
    // and is perfectly healthy, so it must pick up no signature at all.
    + w('fullsized', 0, 'done (0 errors)')
    + w('binlink-only', 0, 'done')
    + w('scratch-only', 0, 'done')
    + w('silent', 0, '')
    // HOOKWRITER is the CONTROL for the no-op gate: a hook installer the jail
    // genuinely broke says so, loudly and differently from its A0 arm.
    // It prints its own path one line ahead of the denial, so the six-line
    // lookback resolves it the OLD way too — which is what lets the
    // string-vs-object assertion below isolate that fix from the window one.
    + w('hookwriter', 0, A0
      ? 'installed hook'
      : `running node_modules/hookwriter/install.js\n`
        + `Error: EPERM: operation not permitted, open '${dir}/proj/.git/hooks/pre-commit'`)
    // NOOP-HOOK prints the SAME thing with the jail on and off — the shape of
    // `yorkie` ("skipping Git hooks installation") and `simple-git-hooks`
    // ("Config was not found!"), both of which the shared project delta scored
    // as breaks off a sibling's work.
    + w('noop-hook', 0, 'setting up Git hooks\ntrying to install from sub \'node_module\' directory, skipping')
    // PARTIAL-BUILD dies mid-compile in BOTH arms having already emitted its
    // artifact — `gl@8.1.6`, which produced 5,994 object files and then hit
    // `"C++20 or later required."`.
    + w('partial-build', 1, 'fatal error: "C++20 or later required."')
    // LATE-FAIL-PROD is the mirror: clean unconfined, non-zero under the jail,
    // with the artifact present in both. Its failure line carries NO path, which
    // is the attribution case the old six-line-lookback rule could not resolve.
    + w('late-fail-prod', A0 ? 0 : 1, A0 ? 'done' : 'getaddrinfo ENOTFOUND github.com')
    // OPAQUE-FAIL fails in prose no errno classifier matches — the shape of
    // `Chromedriver could not be installed: fetch failed`, `Could not connect to
    // CDN`, `getwd: invalid argument`. It must still reach triage.
    + w('opaque-fail', A0 ? 0 : 1, A0 ? 'done' : 'Chromedriver 149 could not be installed: fetch failed')
    // OUTSIDE EVERY WINDOW — the runner's epilogue, after its own end-of-windows
    // terminator. Attribution must NOT run past a window boundary and claim this
    // for whichever package happened to go last.
    + 'per-package windows=10 worst rc=1\n'
    + 'request to https://registry.example.invalid failed, reason: ECONNREFUSED\n'
  );
};

function runArm(outRoot, arm, header, opts) {
  const d = path.join(outRoot, `st-${arm}`);
  fs.mkdirSync(d, { recursive: true });
  const log = mkLog(arm, d, header);
  const w = (f, recs) => fs.writeFileSync(path.join(d, f), recs.map((r) => JSON.stringify(r)).join('\n') + '\n');
  w('pre.ndjson', pre);
  w('post.ndjson', post(arm, opts));
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

function scenario(name, header, opts) {
  const outRoot = path.join(ROOT, name, 'out');
  fs.mkdirSync(outRoot, { recursive: true });
  runArm(outRoot, 'A0', CLEAN, opts);
  runArm(outRoot, 'PROD', header, opts);
  execFileSync(process.execPath, [path.join(HARNESS, 'aggregate.mjs'), outRoot], { stdio: ['ignore', 'ignore', 'inherit'] });
  const t = JSON.parse(fs.readFileSync(path.join(ROOT, name, 'verdict-table.json'), 'utf8'));
  return Object.fromEntries(t.rows.map((r) => [r.pkg.replace(/@[^@]+$/, ''), r]));
}

const TRUNCATED = 'Approved 10 package(s)\n  × lifecycle script postinstall failed for some-other-\n  │ package@1.0.0: script `postinstall` exited with code 1\n';
const CLEAN = 'Approved 10 package(s)\n';

let failures = 0;
const check = (label, got, want) => {
  const ok = got === want;
  if (!ok) failures++;
  console.log(`${ok ? 'ok  ' : 'FAIL'}  ${label}\n        want ${want}\n        got  ${got}`);
};

const trunc = scenario('truncated', TRUNCATED);
const clean = scenario('clean', CLEAN);
// The same shard with the project effect present under the jail too — what a
// `.git/hooks` write grant looks like. Nothing here may be scored a break.
const granted = scenario('granted', CLEAN, { hooksGranted: true });

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

console.log('\n— the shared project delta cannot attribute a break —');
// Both rows read the SAME `delta.project_paths`, so both scored an A0 HIT off
// the one hook file in the tree and both flipped to MISS under the jail. Only
// one of them was ever doing anything.
check('a package whose own window is identical in both arms is NOT a break',
  clean['noop-hook'].admissible === false && /NO-OP-BOTH-ARMS/.test(clean['noop-hook'].note), true);
check('CONTROL: the sibling the jail really did break stays a break',
  clean['hookwriter'].jail_cost, 'BREAK:NEVER-RAN-ITS-REAL-PATH');
// Paired so the check cannot pass merely because the note text matched: the
// ambiguity that motivates the gate has to be RECORDED, not inferred.
check('...and the row states how many rows shared the project delta',
  clean['noop-hook'].project_scope_shared, 2);
// THE SECOND CONTROL, and the one that caught the gate over-firing on real data:
// `msw` writes its service worker in BOTH arms, so nothing is manufactured and
// the row is a genuine survivor. An unscoped gate deleted it anyway.
check('CONTROL: identical windows do NOT strip a project-scoped row the jail passed',
  granted['noop-hook'].jail_cost, 'OK');

console.log('\n— a package\'s own exit code vetoes its own success —');
check('an arm that emitted its full artifact and then exited non-zero is not a success',
  clean['partial-build'].a0, 'DID-WORK-AND-FAILED');
check('...so the row leaves the corpus rather than becoming a false denominator',
  clean['partial-build'].admissible === false && /INVALID-FIXTURE/.test(clean['partial-build'].note), true);
check('CONTROL: the same artifact with rc=0 still scores a pass',
  clean['fullsized'].jail_cost, 'OK');
check('clean unconfined, non-zero under the jail, artifact intact in both: a BREAK',
  clean['late-fail-prod'].jail_cost, 'BREAK:DID-WORK-AND-FAILED');
check('CONTROL: its A0 arm is a valid denominator',
  clean['late-fail-prod'].a0, 'DID-WORK-AND-SUCCEEDED');

console.log('\n— failure attribution by window —');
// `getaddrinfo ENOTFOUND github.com` names no package and no path. Under the
// old path-only rule it landed in `(shard-level)` and joined to no row.
// Read off the SAMPLES rather than the signature string, so this fails for the
// attribution reason alone and not because the field's rendering changed.
check('a bare failure line inside a window attributes to that window\'s package',
  /getaddrinfo ENOTFOUND github\.com/.test(JSON.stringify(clean['late-fail-prod'].prod_failure_samples || [])), true);
// Asserted on a row that attributes the OLD way too (its denial is preceded by
// its own path), so this fails for the string reason alone rather than riding on
// the window fix above.
check('...and the signature is a STRING carrying the denial, not a counts object rendered [object Object]',
  /^[a-z:0-9 ]+ \| \[fs\] Error: EPERM/.test(clean['hookwriter'].prod_failure_signature || ''), true);
// CONTROL: attribution must not run past a window's end and claim the install
// phase for whichever package happened to go last.
const prodReport = JSON.parse(fs.readFileSync(path.join(ROOT, 'clean/out/st-PROD/report.json'), 'utf8'));
check('CONTROL: a failure line OUTSIDE every window stays shard-level',
  /ECONNREFUSED/.test(JSON.stringify(prodReport.failure_signatures_by_package['(shard-level)'] || {})), true);
// Labelled, not categorised: it must NOT have been guessed into fs or net.
check('a failure no errno rule matches still reaches triage, labelled unclassified',
  /unclassified:1 \| \[unclassified\] Chromedriver 149 could not be installed: fetch failed$/
    .test(clean['opaque-fail'].prod_failure_signature || ''), true);
check('CONTROL: a healthy window that merely says "errors" gets no signature',
  clean['fullsized'].prod_failure_signature, null);
check('...and no package row absorbs it',
  Object.entries(prodReport.failure_signatures_by_package)
    .some(([k, v]) => k !== '(shard-level)' && /ECONNREFUSED/.test(JSON.stringify(v))), false);

if (!process.env.KEEP) fs.rmSync(ROOT, { recursive: true, force: true });
console.log(`\n${failures ? `${failures} FAILED` : 'all gates hold'}`);
process.exit(failures ? 1 : 0);

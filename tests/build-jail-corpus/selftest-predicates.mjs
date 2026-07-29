#!/usr/bin/env node
// selftest-predicates.mjs — the discriminating cases each work-signal predicate must
// get right, asserted directly against shard-verdict.mjs.
//
// WHY THIS FILE EXISTS. The first full two-platform run reported 117 surviving
// verdicts on Linux; 115 of them rested on predicates satisfied by "the script
// created any file at all". Nothing failed, nothing was red, and the number reached a
// report before anyone noticed the predicate could not distinguish a downloaded
// 18 MB binary from two bytes of bookkeeping written by a script that then exited 1.
//
// A predicate is the one thing in this harness that cannot be checked by running the
// harness — a weak predicate produces a clean run and a confident wrong answer. So
// the discriminating cases are asserted here instead, and the case that motivated the
// rewrite (`wrangler-bookkeeping`, modelled on @cloudflare/wrangler@1.21.0 scoring
// DID-WORK-AND-SUCCEEDED with changed=2 while its postinstall exited 1) is the first
// one. Run this before trusting any number this harness produces.
//
// usage: node selftest-predicates.mjs

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';

const MiB = 1024 * 1024;
const HERE = path.dirname(new URL(import.meta.url).pathname);

// Each case: the files a package created in its own store cell, and the verdict the
// predicate must return. `why` is the failure mode the case pins down.
const CASES = [
  {
    name: 'wrangler-bookkeeping', cls: 'binary-downloader',
    created: [{ p: 'a/out.json', sz: 412, mode: 0o644, ty: 'file' }, { p: 'a/log.txt', sz: 88, mode: 0o644, ty: 'file' }],
    want: 'DID-WORK-AND-FAILED',
    why: 'THE REGRESSION: wrote two small files then died. Must not read as success.',
  },
  {
    name: 'real-download', cls: 'binary-downloader',
    created: [{ p: 'bin/tool', sz: 18 * MiB, mode: 0o755, ty: 'file' }],
    want: 'DID-WORK-AND-SUCCEEDED',
    why: 'The golden path: a large executable landed. Tightening must not break this.',
  },
  {
    name: 'symlink-trap', cls: 'binary-downloader',
    created: [{ p: 'bin/link', sz: 0, mode: 0o777, ty: 'link' }],
    want: 'DID-WORK-AND-FAILED',
    why: 'A symlink is mode 0777 on Linux, so an exec-bit test alone passes every link.',
  },
  {
    name: 'tiny-shim', cls: 'binary-downloader',
    created: [{ p: 'bin/sh-wrap', sz: 220, mode: 0o755, ty: 'file' }],
    want: 'DID-WORK-AND-FAILED',
    why: 'Executable but 220 bytes — a shell wrapper, not a downloaded binary.',
  },
  {
    name: 'native-addon', cls: 'binary-downloader',
    created: [{ p: 'build/Release/x.node', sz: 3 * MiB, mode: 0o644, ty: 'file' }],
    want: 'DID-WORK-AND-SUCCEEDED',
    why: 'A downloaded .node is routinely mode 0644, so extension must be a second route.',
  },
  {
    name: 'non-executable-payload', cls: 'binary-downloader',
    created: [{ p: 'bin/Azure.Functions.Cli.dat', sz: 553 * MiB, mode: 0o664, ty: 'file' }],
    want: 'DID-WORK-AND-SUCCEEDED',
    why: 'THE SECOND REGRESSION: azure-functions-core-tools@4.12.1 downloads 579,911,350 bytes '
      + 'at mode 0664. Requiring the exec bit scored that as unmeasurable — a false negative in '
      + 'the direction that hides breakage.',
  },
  {
    name: 'bin-shim-noise', cls: 'binary-downloader',
    created: [],  // delta.mjs drops node_modules/.bin, so nothing reaches the predicate
    want: 'NEVER-RAN-ITS-REAL-PATH',
    why: 'THE ROOT CAUSE: wrangler\'s changed=2 was a 0-byte .bin dir and a 280-byte PM shim for '
      + 'its rimraf dependency. With .bin excluded as PM noise the honest verdict is that its '
      + 'postinstall did nothing observable.',
  },
  {
    name: 'just-under', cls: 'binary-downloader',
    created: [{ p: 'bin/t', sz: MiB - 1, mode: 0o755, ty: 'file' }],
    want: 'DID-WORK-AND-FAILED', why: 'Floor boundary, low side.',
  },
  {
    name: 'exactly-floor', cls: 'binary-downloader',
    created: [{ p: 'bin/t', sz: MiB, mode: 0o755, ty: 'file' }],
    want: 'DID-WORK-AND-SUCCEEDED', why: 'Floor boundary, inclusive.',
  },
  {
    name: 'selfconf', cls: 'self-configuring',
    created: [{ p: 'cfg.json', sz: 120, mode: 0o644, ty: 'file' }],
    want: 'UNSIGNALLED',
    why: 'Demoted 2026-07-28. It wrote a file; that is not evidence it configured anything.',
  },
];

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'corpus-selftest-'));
const owner_detail = {}, by_owner = {};
for (const c of CASES) {
  owner_detail[`${c.name}@1.0.0`] = { created: c.created, modified: [] };
  by_owner[`${c.name}@1.0.0`] = { kind: 'script-output', created: c.created.length };
}
fs.writeFileSync(path.join(dir, 'delta.json'), JSON.stringify({
  by_owner, owner_detail, installed_cells: Object.keys(owner_detail),
  unattributed_counts: {}, unattributed_sample: { 'project-file': [] }, project_paths: [],
}));
fs.writeFileSync(path.join(dir, 'manifest.tsv'), CASES.map((c) => `${c.name}\t1.0.0\t${c.cls}`).join('\n') + '\n');
fs.writeFileSync(path.join(dir, 'run.log'), '');

execFileSync(process.execPath, [
  path.join(HERE, 'lib/shard-verdict.mjs'),
  path.join(dir, 'delta.json'), path.join(dir, 'manifest.tsv'), path.join(dir, 'run.log'), path.join(dir, 'verdicts.json'),
], { env: { ...process.env, NONCE: 'X', SHARD: 'selftest', ARM: 'PROD', RC_SCRIPT: '0', RC_INSTALL: '0', PROJ: dir, STOREROOT: dir }, stdio: 'ignore' });

const got = JSON.parse(fs.readFileSync(path.join(dir, 'verdicts.json'), 'utf8'));
let failed = 0;
for (const c of CASES) {
  const r = got.results.find((x) => x.pkg === c.name);
  const ok = r?.verdict === c.want;
  if (!ok) failed++;
  console.log(`${ok ? 'ok  ' : 'FAIL'}  ${c.name.padEnd(22)} want=${c.want.padEnd(23)} got=${r?.verdict ?? '(missing)'}`);
  if (!ok) console.log(`        ${c.why}\n        reasons: ${JSON.stringify(r?.reasons)}`);
}
fs.rmSync(dir, { recursive: true, force: true });
console.log(failed ? `\n${failed}/${CASES.length} FAILED — do not trust this harness's numbers` : `\nall ${CASES.length} predicate cases pass`);
process.exit(failed ? 1 : 0);

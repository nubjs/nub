#!/usr/bin/env node
// rescore.mjs — re-score an ALREADY-COMPLETED run against the current predicates.
//
// WHY THIS EXISTS. A corpus arm costs hours and a machine; a scoring bug costs a
// verdict. Twice now the numbers moved because the HARNESS changed, not because
// the jail did, and each time the only way to tell was to re-run — which meant
// the fix and the re-measurement arrived together and neither could control the
// other. The snapshots that bracket the lifecycle window are the whole evidence
// base and they are already on disk, so a predicate change can be replayed
// against them for free. That makes "did this number move because of my fix or
// because of scheduling noise?" a question with an answer.
//
// IT RE-INVOKES THE REAL SCORERS RATHER THAN REIMPLEMENTING THEM. A second copy
// of the verdict logic would drift from the first, and then a re-score would be
// validating a fix against a scorer that is not the one the corpus runs.
//
// WHAT AN ARCHIVE CANNOT ANSWER. `content_nonce` reads the generated file off
// disk, and a finished run's store is gone. Those rows come back UNSIGNALLED
// with an explicit UNEVALUABLE reason rather than a false NEVER-RAN.
//
//   node rescore.mjs <out-dir> <run-dir>...
//   node aggregate.mjs <out-dir>

import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';

const HARNESS = path.dirname(new URL(import.meta.url).pathname);
const [outRoot, ...runDirs] = process.argv.slice(2);
if (!outRoot || !runDirs.length) {
  console.error('usage: rescore.mjs <out-dir> <run-dir>...');
  process.exit(2);
}
fs.mkdirSync(outRoot, { recursive: true });

const read = (f) => JSON.parse(fs.readFileSync(f, 'utf8'));

for (const dir of runDirs) {
  const name = path.basename(dir.replace(/\/$/, ''));
  // `continue` here used to advance the FILE loop, not the run loop, so an
  // incomplete arm printed SKIP and then crashed the whole re-score on the very
  // file it had just reported missing. A killed or still-running arm
  // (`default5-PROD`) is the normal case for this, and it must not take the
  // other 61 with it.
  const missing = ['pre.ndjson', 'post.ndjson', 'run.log', 'verdicts.json', 'report.json']
    .filter((f) => !fs.existsSync(path.join(dir, f)));
  if (missing.length) {
    console.error(`SKIP ${name}: missing ${missing.join(', ')}`);
    continue;
  }
  const oldVerdicts = read(path.join(dir, 'verdicts.json'));
  const oldReport = read(path.join(dir, 'report.json'));
  const log = fs.readFileSync(path.join(dir, 'run.log'), 'utf8');
  const nonce = (log.match(/nonce=(\S+)/) || [])[1] || '';

  // The manifest is reconstructed from the archived verdict rows, which is the
  // only source that survives with the run: a shard TSV can be edited afterwards,
  // and an isolated run's one-line manifest is regenerated per invocation.
  const outDir = path.join(outRoot, name);
  fs.mkdirSync(outDir, { recursive: true });
  const man = path.join(outDir, 'manifest.tsv');
  fs.writeFileSync(man, oldVerdicts.results.map((r) => `${r.pkg}\t${r.version}\t${r.class}\t`).join('\n') + '\n');

  execFileSync(process.execPath, [
    path.join(HARNESS, 'lib/delta.mjs'),
    path.join(dir, 'pre.ndjson'), path.join(dir, 'post.ndjson'), path.join(outDir, 'delta.json'),
  ], { stdio: ['ignore', 'inherit', 'inherit'], maxBuffer: 1 << 28 });

  // PROJ / STORE_BASES deliberately point into the ARCHIVE, which for a finished
  // run no longer holds a store. That is the honest state, and `content_nonce`
  // reports UNEVALUABLE for it rather than inventing a miss.
  execFileSync(process.execPath, [
    path.join(HARNESS, 'lib/shard-verdict.mjs'),
    path.join(outDir, 'delta.json'), man, path.join(dir, 'run.log'), path.join(outDir, 'verdicts.json'),
  ], {
    stdio: ['ignore', 'pipe', 'inherit'],
    env: {
      ...process.env,
      // FORWARDED, because without it a re-score silently runs a DIFFERENT
      // scheduling gate than the run did: `truncatedPkgs` stays null and the
      // scorer falls back to the shard-level `rc_script && approved > 1` test,
      // whose `approved` is parsed from the first `Approved N package(s)` line —
      // always 1 under per-package windows. The re-score then reads untruncated
      // by accident rather than by construction, which is the same answer for
      // the wrong reason and would not have survived a `--all` archive.
      // Set to the empty string, never left inherited: an ambient WINDOWS_JSON
      // from the caller's shell would otherwise be applied to every run dir,
      // including ones that have no such file.
      WINDOWS_JSON: fs.existsSync(path.join(dir, 'windows.json')) ? path.join(dir, 'windows.json') : '',
      NONCE: nonce,
      SHARD: oldVerdicts.shard ?? name.replace(/-(A0|PROD).*$/, ''),
      ARM: oldVerdicts.arm ?? (/-A0/.test(name) ? 'A0' : 'PROD'),
      RC_SCRIPT: String(oldVerdicts.rc_script ?? oldReport.rc_script ?? 0),
      RC_INSTALL: String(oldVerdicts.rc_install ?? oldReport.rc_install ?? 0),
      PROJ: path.join(dir, 'proj'),
      STOREROOT: path.join(dir, 'home'),
      STORE_BASES: [path.join(dir, 'cache/store'), path.join(dir, 'proj/node_modules/.store')].join('\n'),
    },
  });

  execFileSync(process.execPath, [
    path.join(HARNESS, 'lib/errsig.mjs'),
    path.join(dir, 'run.log'), man, path.join(outDir, 'verdicts.json'), path.join(outDir, 'report.json'),
  ], {
    stdio: ['ignore', 'pipe', 'inherit'],
    env: {
      ...process.env,
      // Carried FORWARD from the original run. These are facts about the
      // execution, not about the scoring, so re-deriving them here would be
      // inventing them: the arm effect in particular is the gate that makes the
      // run admissible at all.
      ARM_EFFECT: oldReport.arm_effect,
      WARN_COUNT: String(oldReport.optout_warnings ?? 0),
      GYP_ID: oldReport.node_gyp_identity ?? '',
      PLATFORM: oldReport.platform ?? '',
      LEVER: oldReport.lever ?? 'none',
      NONCE: nonce,
    },
  });

  const nv = read(path.join(outDir, 'verdicts.json'));
  const before = {}, after = {};
  for (const r of oldVerdicts.results) before[`${r.pkg}@${r.version}`] = r.verdict;
  for (const r of nv.results) after[`${r.pkg}@${r.version}`] = r.verdict;
  const moved = Object.keys(after).filter((k) => before[k] !== after[k]);
  console.log(
    `${name}: truncated=${nv.scheduling_truncated} approved=${nv.approved_jobs} ` +
      `failed=[${(nv.failed_packages || []).join(',')}] moved=${moved.length}`,
  );
  for (const k of moved) console.log(`    ${k}: ${before[k]} -> ${after[k]}`);
}

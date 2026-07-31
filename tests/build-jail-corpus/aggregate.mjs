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
//
// TWO MORE GATES SIT BESIDE IT, EACH CLOSING A WAY A VERDICT COULD LIE, IN
// OPPOSITE DIRECTIONS:
//
//   SCHEDULING (over-reports breaks). aube stops scheduling queued lifecycle jobs
//   once any sibling fails, so in a truncated window an absent delta means either
//   "the jail blocked it" or "it never ran". A tighter jail fails earlier and skips
//   more, so the bias MANUFACTURES breaks. Those rows are INCONCLUSIVE and belong
//   in neither the numerator nor the denominator; an ISOLATED re-run supersedes
//   them, and the worklist below names exactly which packages need one.
//
//   ARTIFACT (under-reports breaks — the more dangerous direction). A verdict says
//   the class effect was present; it cannot say the effect was the right SIZE.
//   `@go-task/cli` passed while its log read `getaddrinfo ENOTFOUND github.com`
//   and its 49 MB binary went to nothing. The unconfined arm is the only honest
//   scale for "big enough", so the confined artifact is diffed against it.
import fs from 'node:fs';
import path from 'node:path';

// An order of magnitude. The measured false passes are three to nine orders down
// (601 MB -> 310 B; 49 MB -> 0; 13,296 files -> 3), so this leaves an enormous
// margin for benign variation — a downloader that legitimately fetches a slightly
// smaller build, an arm that skips one optional file — while catching every shape
// of "the fetch was refused and something else satisfied the predicate".
const ARTIFACT_RATIO = 0.1;
// Only classes whose effect IS an artifact are size-scaled. A hook installer's
// effect is a 40-byte file and a codegen client's size varies with its input;
// applying a size ratio to either would invent breaks.
const ARTIFACT_SCALED = new Set(['binary-downloader', 'native-build-prebuilt', 'native-build-gyp', 'self-configuring']);

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
      artifact: v.artifact || null, scheduling: v.scheduling ?? null,
      isolated: !!r.isolated, truncated: !!r.scheduling_truncated,
      fail: r.failure_signatures_by_package[v.pkg] || null,
      project_scoped: !!v.project_scoped, project_scope_shared: v.project_scope_shared ?? null,
      window: v.own_window || null,
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

      // THE SHARED-PROJECT-DELTA GATE. A project-scoped class (a hook installer)
      // has no per-package filesystem attribution at all: `delta.project_paths`
      // is the whole project's, so one shard member writing `.git/hooks`
      // satisfies the predicate for EVERY project-scoped row in that shard, and
      // the confined arm — which writes none — then flips all of them to MISS
      // together. That manufactured a break for `yorkie` and `simple-git-hooks`,
      // both of which are pure no-ops that print the same line with the jail on
      // and off ("skipping Git hooks installation", "Config was not found!").
      //
      // The evidence that IS per-package is the package's own lifecycle window,
      // and the test it supports is a differential rather than a heuristic: if
      // the jail changed nothing the package printed and nothing it returned,
      // the jail did not break it. Real hook installers fail loudly and
      // distinctly here — EPERM on the hook open, `Unable to read current
      // working directory`, `getwd: invalid argument` — so this refuses exactly
      // the rows whose two arms are the same run twice.
      //
      // TWO SCOPINGS, both load-bearing.
      //
      // PROJECT-ONLY EVIDENCE: a downloader can be blocked silently, with
      // identical (empty) output in both arms and the break visible only in its
      // own cell's delta, which IS attributed. Applying this there would hide
      // real breaks.
      //
      // AND ONLY WHERE THE ROW WOULD OTHERWISE BREAK. The gate exists to refuse
      // a manufactured break, not to strip a demonstrated pass. Unscoped it also
      // fired on `msw`, whose project file is created in BOTH arms — nothing was
      // manufactured there, so all the gate did was delete two survivors. This
      // is the mirror of the artifact diff's scoping one screen down, which is
      // applied only to rows that would otherwise PASS for the same reason: each
      // gate corrects the direction its evidence can actually be wrong in.
      let noop_both_arms = false;
      if (admissible && prod && a0 && a0.project_scoped && prod.project_scoped
          && prod.verdict !== 'DID-WORK-AND-SUCCEEDED'
          && a0.window && prod.window
          && a0.window.digest === prod.window.digest && a0.window.rc === prod.window.rc) {
        admissible = false;
        noop_both_arms = true;
        note = `NO-OP-BOTH-ARMS — the package's own lifecycle window is identical with the jail on and off (rc=${prod.window.rc}); its A0 "denominator" is a project-delta HIT shared with ${a0.project_scope_shared ?? '?'} rows in this shard and is not attributable to it`;
      }

      // THE SCHEDULING GATE, AND IT NEEDS BOTH ARMS TO FIRE. The manufactured
      // break has exactly one shape: the unconfined arm proves the package HAD
      // work to do, the confined arm left no trace, and the confined window was
      // truncated by some OTHER package failing first — so "the jail blocked it"
      // and "it was never scheduled" are the same observation. Nothing else is
      // affected: a package silent in BOTH arms is the ordinary no-op the A0 gate
      // already excludes, and truncation does not change that conclusion.
      let inconclusive = false;
      if (admissible && prod && prod.scheduling === 'silent-in-truncated-window') {
        admissible = false;
        inconclusive = true;
        note = 'INCONCLUSIVE-SCHEDULING — A0 proves work exists, PROD left no trace, and a sibling failed first in the PROD window; re-run isolated';
      }
      // The mirror case does not manufacture a break, it SHRINKS THE CORPUS: an
      // A0 arm that skipped a package reports it as a no-op, and the row is
      // dropped. Counted separately so the denominator's uncertainty is stated
      // rather than absorbed.
      const denominator_suspect = !!(a0 && a0.scheduling === 'silent-in-truncated-window');

      // THE ARTIFACT DIFF. Applied only to a row that would otherwise be scored a
      // PASS: a verdict that already breaks needs no falsifying, and the point is
      // to catch the reassuring answer, not to add to the alarming one.
      let jail_cost = admissible && prod ? (prod.verdict === 'DID-WORK-AND-SUCCEEDED' ? 'OK' : `BREAK:${prod.verdict}`) : null;
      let artifact_ratio = null;
      if (jail_cost === 'OK' && ARTIFACT_SCALED.has(e.class) && a0.artifact && prod.artifact) {
        const rb = a0.artifact.bytes > 0 ? prod.artifact.bytes / a0.artifact.bytes : null;
        const rf = a0.artifact.files > 0 ? prod.artifact.files / a0.artifact.files : null;
        artifact_ratio = { bytes: rb, files: rf, a0: a0.artifact, prod: prod.artifact };
        if ((rb !== null && rb < ARTIFACT_RATIO) || (rf !== null && rf < ARTIFACT_RATIO)) {
          jail_cost = 'BREAK:UNDERSIZED-ARTIFACT';
          note = `false pass: confined artifact ${prod.artifact.bytes} B / ${prod.artifact.files} files vs unconfined ${a0.artifact.bytes} B / ${a0.artifact.files} files`;
        }
      }

      // A STRING, BECAUSE THIS FIELD IS READ BY A HUMAN TRIAGING BREAKS. It used
      // to be the raw `{fs,net,build,engine}` counts object, which rendered as
      // `[object Object]` in every table it appeared in and, when it did resolve,
      // said `build:5` rather than what was actually denied. The counts are kept
      // beside it for anything that wants to aggregate them.
      const counts = prod?.fail ? Object.fromEntries(Object.entries(prod.fail).filter(([k]) => k !== 'samples')) : null;
      const sig = counts
        ? `${Object.entries(counts).map(([k, n]) => `${k}:${n}`).join(' ')}`
          + (prod.fail.samples?.length ? ` | ${prod.fail.samples[0]}` : '')
        : null;

      rows.push({
        pkg, class: e.class, shard, platform: plat, isolated: !!(prod?.isolated ?? a0?.isolated),
        a0: a0?.verdict ?? '-', prod: prod?.verdict ?? '-',
        a0_changed: a0?.changed ?? null, prod_changed: prod?.changed ?? null,
        admissible, note, jail_cost, artifact_ratio, inconclusive, denominator_suspect, noop_both_arms,
        // Per-package attribution is unavailable for these, so any count built
        // from them is an upper bound. Carried on the row rather than stated as
        // a footnote, so a reader of the table cannot miss it.
        project_scope_shared: prod?.project_scope_shared ?? a0?.project_scope_shared ?? null,
        prod_window_rc: prod?.window?.rc ?? null,
        prod_failure_signature: sig,
        prod_failure_counts: counts,
        prod_failure_samples: prod?.fail?.samples ?? null,
      });
    }
  }
}

// ISOLATION SUPERSEDES THE SCREEN. A batch row and an isolated row for the same
// package are not two samples to reconcile — the batch one was taken through a
// window that may have been truncated, and the isolated one was not. Keeping both
// in the totals double-counts, and averaging them launders the known-bad number
// into the known-good one.
const isoPkgs = new Set(rows.filter((r) => r.isolated).map((r) => `${r.pkg}|${r.platform}`));
for (const r of rows) {
  if (!r.isolated && isoPkgs.has(`${r.pkg}|${r.platform}`)) {
    r.superseded = true;
    r.admissible = false;
    r.note = 'superseded by the isolated re-run of the same package';
    r.jail_cost = null;
  }
}

rows.sort((a, b) => (a.shard + a.platform + a.pkg).localeCompare(b.shard + b.platform + b.pkg));
fs.writeFileSync(path.join(root, '../verdict-table.json'), JSON.stringify({ runs: ok.map((r) => ({ shard: r.shard, arm: r.arm, platform: r.platform, lever: r.lever, arm_effect: r.arm_effect, node_gyp: r.node_gyp_identity, rc_install: r.rc_install, rc_script: r.rc_script, isolated: r.isolated, scheduling_truncated: r.scheduling_truncated, approved_jobs: r.approved_jobs, failed_packages: r.failed_packages })), rows }, null, 2));

const pad = (s, n) => String(s).padEnd(n).slice(0, n);
console.log('\n' + pad('package', 34) + pad('class', 22) + pad('shard', 10) + pad('plat', 7) + pad('A0 (jail off)', 25) + pad('PROD (jail on)', 25) + 'admissible / note');
console.log('-'.repeat(160));
for (const r of rows) {
  console.log(pad(r.pkg, 34) + pad(r.class, 22) + pad(r.shard, 10) + pad(r.platform, 7) + pad(r.a0, 25) + pad(r.prod, 25) + (r.admissible ? (r.jail_cost === 'OK' ? 'YES  ok' : `YES  ${r.jail_cost}`) : `no   ${r.note}`));
}

const adm = rows.filter((r) => r.admissible);
const breaks = adm.filter((r) => r.jail_cost && r.jail_cost !== 'OK');
console.log(`\nadmissible cells: ${adm.length} / ${rows.length}`);
console.log(`  survives the jail : ${adm.filter((r) => r.jail_cost === 'OK').length}`);
console.log(`  BREAKS under jail : ${breaks.length}`);
for (const r of breaks) console.log(`      ${pad(r.pkg, 40)} ${pad(r.class, 24)} ${r.jail_cost}${r.isolated ? ' [isolated]' : ' [BATCH — not yet isolated]'}\n          ${r.prod_failure_signature || '(no failure signature)'}`);
// A break with no signature is a break nobody can triage, so the coverage is
// reported next to the count rather than discovered later by hand.
const sigged = breaks.filter((r) => r.prod_failure_signature).length;
console.log(`  breaks carrying a failure signature: ${sigged} / ${breaks.length}`);
const inad = {};
for (const r of rows.filter((r) => !r.admissible)) inad[r.note] = (inad[r.note] || 0) + 1;
console.log('  inadmissible reasons:', JSON.stringify(inad, null, 2));
console.log(`  denominator possibly under-counted (A0 silent in a truncated window): ${rows.filter((r) => r.denominator_suspect).length}`);

// THE WORKLIST IS THE DELIVERABLE OF A BATCH RUN, not the break count. Every line
// here is a package whose fate the screen genuinely could not determine; feed it
// to `run-isolated.sh --from-worklist` and the number that comes back means what
// it says.
const worklist = [...new Set(rows.filter((r) => !r.isolated && r.inconclusive).map((r) => r.pkg))].sort();
const wf = path.join(root, '../inconclusive.txt');
fs.writeFileSync(wf, worklist.join('\n') + (worklist.length ? '\n' : ''));
console.log(`\n  inconclusive, needs isolation: ${worklist.length} -> ${wf}`);

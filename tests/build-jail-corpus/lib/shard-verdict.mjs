#!/usr/bin/env node
// shard-verdict.mjs — per-package verdicts from ONE shard run.
//
// The shard makes the generic half of the signal cheap (one install, one window)
// but it does NOT make the verdict easier: a package whose specific input is
// still missing bails silently inside a shard exactly as it would alone. So the
// three states, and especially NEVER-RAN-ITS-REAL-PATH, matter more here, not
// less — a shared fixture makes a false pass *less likely*, never impossible.
//
// A shard-level rc is nearly useless: with hundreds of scripts, one failure sets
// it and the other 299 outcomes are invisible. Attribution therefore has to be
// per package, and it comes from the store path (lib/attribute.mjs).

import fs from 'node:fs';
import path from 'node:path';
import { isBinaryPayload, BINARY_MIN_BYTES } from './attribute.mjs';

const [deltaF, manifestF, logF, outF] = process.argv.slice(2);
const delta = JSON.parse(fs.readFileSync(deltaF, 'utf8'));
const log = fs.readFileSync(logF, 'utf8');
const nonce = process.env.NONCE || '';
const rcScript = Number(process.env.RC_SCRIPT || '0');
const classes = JSON.parse(fs.readFileSync(new URL('./classes.json', import.meta.url), 'utf8'));

const manifest = fs
  .readFileSync(manifestF, 'utf8')
  .split('\n')
  .filter((l) => l.trim() && !l.startsWith('#'))
  .map((l) => {
    const [pkg, version, cls, needs] = l.split('\t');
    return { pkg, version, cls, needs: (needs || '').split(',').filter(Boolean) };
  });

// A nonce hit is only admissible if the file was CREATED OR MODIFIED in the
// window. A content-only check false-passes on a preseeded tree — measured: a
// stale generated client copied in before the run satisfied every static content
// check while nothing had been generated. Delta-scoping is what closes that.
function nonceInOwnerDelta(ownerDetail, cellAbsBase) {
  if (!nonce || !ownerDetail) return false;
  for (const { p: rel } of [...(ownerDetail.created || []), ...(ownerDetail.modified || [])]) {
    const abs = path.join(cellAbsBase, rel);
    try {
      const st = fs.statSync(abs);
      if (!st.isFile() || st.size > 32 * 1024 * 1024) continue;
      if (fs.readFileSync(abs, 'utf8').includes(nonce)) return rel;
    } catch {}
  }
  return false;
}

const results = [];
for (const m of manifest) {
  const key = `${m.pkg}@${m.version}`;
  const owner = delta.by_owner[key];
  const detail = delta.owner_detail[key];
  const spec = classes[m.cls] || {};
  const created = (detail?.created || []);
  const changed = created.concat(detail?.modified || []);

  // PM MATERIALISATION vs SCRIPT OUTPUT: a cell that appeared wholesale during
  // the window was fetched, not written into. mtime cannot make this call —
  // tested and refuted, nub's extractor stamps mtime=now on extracted files.
  // "absent from the delta" is NOT "absent from the tree". A package whose script
  // legitimately did nothing produces no delta entries at all, and reporting that
  // as NOT-INSTALLED would hide the single most common real outcome — the modern
  // ecosystem's install scripts are overwhelmingly no-ops on their default path.
  const installed = (delta.installed_cells || []).includes(key);
  const kind = owner?.kind ?? (installed ? 'installed-no-delta' : 'absent');
  const acted = kind === 'script-output' && changed.length > 0;

  let effect = null;
  let binaryEvidence = null;
  const reasons = [];
  const P = spec.predicate || {};
  if (spec.strength === 'NONE') {
    effect = null;
    reasons.push('class has no reliable work-signal');
  } else {
    effect = true;
    if (P.created_all_of) for (const re of P.created_all_of) {
      const hit = created.some((e) => new RegExp(re.replace(/^\^pkg:/, '')).test(e.p));
      reasons.push(`all_of ${re}: ${hit ? 'HIT' : 'MISS'}`); if (!hit) effect = false;
    }
    if (P.created_any_of) {
      const hit = P.created_any_of.some((re) => created.some((e) => new RegExp(re.replace(/^\^[a-z|()]*pkg[a-z|()]*:/, '')).test(e.p)));
      reasons.push(`any_of: ${hit ? 'HIT' : 'MISS'}`); if (!hit) effect = false;
    }
    if (P.created_binary) {
      // The artifact, not its existence. `evidence` names the file that satisfied the
      // predicate (or the largest created file that failed to), so a reader can judge
      // the 1 MiB floor against what was actually on disk instead of taking it on faith.
      const min = P.created_binary.min_bytes ?? BINARY_MIN_BYTES;
      const hit = created.find((e) => isBinaryPayload(e) && e.sz >= min);
      const biggest = created.reduce((a, e) => ((e.sz ?? 0) > (a?.sz ?? -1) ? e : a), null);
      binaryEvidence = hit ?? biggest ?? null;
      reasons.push(
        hit
          ? `created_binary: HIT ${hit.p} ${hit.sz}b mode=${(hit.mode ?? 0).toString(8)}`
          : `created_binary: MISS (>=${min}b regular file, executable or native ext) — largest created: ${
              biggest ? `${biggest.p} ${biggest.sz}b mode=${(biggest.mode ?? 0).toString(8)}` : 'none'
            }`,
      );
      if (!hit) effect = false;
    }
    if (P.changed_any_of) {
      // Project-scoped predicates (hook installers) look at the project delta,
      // not the package's own cell — the effect lands outside the writer.
      // THE FULL LIST, not unattributed_sample: that field is capped at 40 entries
      // for display, and a shard's project delta runs well past 40, so a hook write
      // could fall off the end and read as MISS. A predicate must never be evaluated
      // against a display cap.
      const projPaths = delta.project_paths || (delta.unattributed_sample['project-file'] || []).map((e) => e.path);
      const hit = P.changed_any_of.some((re) => projPaths.some((p) => new RegExp(re).test(p)));
      reasons.push(`changed_any_of(project): ${hit ? 'HIT' : 'MISS'}`); if (!hit) effect = false;
    }
    if (P.min_created != null) {
      const hit = created.length >= P.min_created;
      reasons.push(`min_created ${P.min_created}: got ${created.length}`); if (!hit) effect = false;
    }
    if (P.content_nonce) {
      // The cell base must be resolved against BOTH store-shaped locations: the
      // global content-hashed CAS store under HOME, and the project virtual
      // store, which is where per-project generated output actually lands.
      // Searching only the global one reported a false NEVER-RAN on Prisma while
      // its nonce-bearing generated client sat on disk.
      const bases = [
        `${process.env.STOREROOT}/.cache/nub/pm/store`,
        `${process.env.PROJ}/node_modules/.store`,
      ];
      let hit = false;
      for (const base of bases) {
        if (!fs.existsSync(base)) continue;
        const cell = fs.readdirSync(base).find((d) => d.startsWith(`${m.pkg.replace(/\//g, '+')}@${m.version}`));
        if (!cell) continue;
        hit = nonceInOwnerDelta(detail, path.join(base, cell, 'node_modules'));
        if (hit) break;
      }
      reasons.push(`content_nonce: ${hit || 'NOT FOUND'}`); if (!hit) effect = false;
    }
  }

  let verdict;
  if (effect === null) verdict = 'UNSIGNALLED';
  else if (kind === 'absent') verdict = 'NOT-INSTALLED';
  else if (kind === 'installed-no-delta') verdict = 'NEVER-RAN-ITS-REAL-PATH';
  else if (kind === 'pm-materialisation') verdict = 'NEVER-RAN-ITS-REAL-PATH';
  else if (effect) verdict = 'DID-WORK-AND-SUCCEEDED';
  else if (acted) verdict = 'DID-WORK-AND-FAILED';
  else verdict = 'NEVER-RAN-ITS-REAL-PATH';

  // The five largest created files, always — regardless of verdict. This is what
  // makes the 1 MiB binary floor auditable rather than asserted: a reader can see
  // the actual size distribution the predicate was applied to and refute the floor
  // if the two distributions turn out not to be separated the way the class doc
  // claims. It is also the only record of WHAT a package produced, which the prior
  // harness discarded entirely (verdicts carried a count and nothing else).
  const artifacts = [...created]
    .sort((a, b) => (b.sz ?? 0) - (a.sz ?? 0))
    .slice(0, 5)
    .map((e) => ({ p: e.p, sz: e.sz, mode: (e.mode ?? 0).toString(8), ty: e.ty, binary: isBinaryPayload(e) }));

  results.push({
    pkg: m.pkg, version: m.version, class: m.cls, kind, acted,
    changed: changed.length, effect, verdict, reasons,
    artifacts,
    binary_evidence: binaryEvidence
      ? { p: binaryEvidence.p, sz: binaryEvidence.sz, mode: (binaryEvidence.mode ?? 0).toString(8), binary: isBinaryPayload(binaryEvidence) }
      : null,
  });
}

// A downloader that lands its payload outside its own store cell is unattributable
// by path, so its row goes inadmissible rather than counted (classes.json states
// this loss explicitly). Surfacing the writes here is what keeps that gap visible:
// a large executable appearing under $HOME during the window, with no owner, is a
// carve-out candidate that the verdict table alone would never show.
const binaryWritesOutsideCells = [];
for (const [kind, entries] of Object.entries(delta.unattributed_sample || {})) {
  for (const e of entries) {
    if (isBinaryPayload({ ...e, p: e.path })) binaryWritesOutsideCells.push({ kind, ...e });
  }
}

const summary = {};
for (const r of results) summary[r.verdict] = (summary[r.verdict] || 0) + 1;
const out = {
  shard: process.env.SHARD, arm: process.env.ARM,
  rc_install: Number(process.env.RC_INSTALL || 0), rc_script: rcScript,
  suppressed: (log.match(/SUPPRESSED capability: (\S+)/) || [])[1] || null,
  summary, results,
  // Which predicate produced each verdict, recorded per run rather than left to be
  // inferred from whichever classes.json the reader happens to have. A verdict table
  // whose predicates changed between runs is unreadable without this.
  predicate_spec: Object.fromEntries(
    Object.entries(classes)
      .filter(([k]) => !k.startsWith('_'))
      .map(([k, v]) => [k, { strength: v.strength, predicate: v.predicate }]),
  ),
  binary_writes_outside_cells: binaryWritesOutsideCells,
  unattributed: delta.unattributed_counts,
  interaction_candidates: (delta.unattributed_sample['cell-dependency-link'] || []).slice(0, 20),
};
fs.writeFileSync(outF, JSON.stringify(out, null, 2));
console.log(JSON.stringify({ shard: out.shard, arm: out.arm, suppressed: out.suppressed, rc_script: rcScript, summary }));
for (const r of results) console.log(`   ${r.verdict.padEnd(24)} ${r.pkg}@${r.version} [${r.class}] changed=${r.changed} kind=${r.kind}`);

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
//
// AND A SHARD-LEVEL FAILURE MAKES EVERY ABSENCE AFTER IT UNATTRIBUTABLE. aube
// stops SCHEDULING queued lifecycle jobs once any sibling fails; jobs still
// behind the semaphore return having done nothing, and a skipped package is
// byte-for-byte indistinguishable from a jailed one — both leave no delta. The
// bias is asymmetric and points the wrong way: a tighter jail fails EARLIER,
// skips MORE siblings, and manufactures MORE breaks.
//
// This file RECORDS that as a per-row `scheduling` fact and does not rule on it,
// because ruling needs BOTH arms — the manufactured break is specifically "A0
// proves work exists, PROD left no trace, PROD's window was truncated". Ruling
// here, on one arm, condemns every ordinary no-op too: it marked 315 of 345 rows
// inconclusive and threw the corpus away. `aggregate.mjs` owns the judgement.

import fs from 'node:fs';
import path from 'node:path';

const [deltaF, manifestF, logF, outF] = process.argv.slice(2);
const delta = JSON.parse(fs.readFileSync(deltaF, 'utf8'));
const log = fs.readFileSync(logF, 'utf8');
const nonce = process.env.NONCE || '';
const rcScript = Number(process.env.RC_SCRIPT || '0');
const classes = JSON.parse(fs.readFileSync(new URL('./classes.json', import.meta.url), 'utf8'));
const overrides = classes._package_predicates || {};

// Packages nub NAMED as having failed. These ran, so their verdicts stand; it is
// everything else in the same window that becomes unattributable.
// The `│` join is not cosmetic: nub renders the failure inside a box and HARD-WRAPS
// the package name across lines (`… failed for @bahmutov/add-typescript-to-` /
// `│ cypress@2.1.2: …`), so a line-oriented match silently returned NO failed
// package for a shard that plainly had one — which reads as "nothing failed here".
const failedByName = new Set(
  [...log.replace(/\n\s*│\s?/g, '').matchAll(/lifecycle script \S+ failed for (\S+?)@[^\s:]+:/g)].map((m) => m[1]),
);
// `Approved N package(s)` is the count of jobs the window actually queued —
// the right denominator for "could anything have been skipped?", and not the
// same as the manifest length, since a manifest row drags in its own
// script-bearing dependencies.
const approved = Number((log.match(/Approved (\d+) package\(s\)/) || [])[1] || '0');
// One queued job cannot be skipped by its own failure, so a single-job window is
// never truncated however it exits. That is what makes an isolated re-run able
// to answer what the batch cannot.
const schedulingTruncated = rcScript !== 0 && approved > 1;

const manifest = fs
  .readFileSync(manifestF, 'utf8')
  .split('\n')
  .filter((l) => l.trim() && !l.startsWith('#'))
  .map((l) => {
    // STRIP THE CR. A CRLF checkout puts a trailing \r on the LAST field of every row,
    // which on a three-column row is the CLASS — so `classes[cls]` missed, the row got
    // an empty spec, no predicate ran, and it scored DID-WORK-AND-SUCCEEDED on the
    // strength of having produced any output at all. Measured on windows-latest: five
    // of nine pilot rows passed with `reasons: []`.
    const [pkg, version, cls, needs] = l.replace(/\r$/, '').split('\t');
    return { pkg, version, cls, needs: (needs || '').split(',').filter(Boolean) };
  });

// A nonce hit is only admissible if the file was CREATED OR MODIFIED in the
// window. A content-only check false-passes on a preseeded tree — measured: a
// stale generated client copied in before the run satisfied every static content
// check while nothing had been generated. Delta-scoping is what closes that.
function nonceInOwnerDelta(ownerDetail, cellAbsBase) {
  if (!nonce || !ownerDetail) return false;
  for (const { path: rel } of [...(ownerDetail.created || []), ...(ownerDetail.modified || [])]) {
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
  // AN UNKNOWN CLASS IS FATAL, NEVER AN EMPTY SPEC. `|| {}` meant a row whose class did
  // not resolve ran no predicate at all and then scored on `effect = true`'s initial
  // value — a silent, unconditional PASS for the one row whose contract nothing checked.
  // Every way a class can fail to resolve (a CRLF-tainted name, a typo, a class deleted
  // from classes.json) therefore produced results that looked stronger than the truth.
  // Refusing here is what makes a green verdict mean a predicate actually ran.
  // `_`-prefixed keys are the file's own metadata (`_doc`, `_package_predicates`),
  // not classes, so a manifest naming one must fail the same way a typo does.
  if (m.cls?.startsWith('_') || !Object.prototype.hasOwnProperty.call(classes, m.cls)) {
    console.error(
      `shard-verdict: FATAL — ${m.pkg}@${m.version} names class ${JSON.stringify(m.cls)}, ` +
        `which is absent from classes.json. Known: ${Object.keys(classes).filter((k) => !k.startsWith('_')).join(', ')}. ` +
        `No verdict from this run is interpretable; fix the manifest rather than reading past it.`
    );
    process.exit(4);
  }
  const spec = classes[m.cls];
  // A PER-PACKAGE PREDICATE REPLACES THE CLASS PREDICATE OUTRIGHT. Two packages
  // are classed `codegen` while emitting FIXED content, so the class's
  // `content_nonce` — the one operator a cache cannot forge — is unsatisfiable
  // for them BY CONSTRUCTION and scored them broken however well they worked.
  // An override is the honest repair; guessing a looser class predicate would
  // have weakened the signal for every other member of the class.
  const override = overrides[key] || overrides[m.pkg] || null;
  const created = (detail?.created || []);
  const changed = created.concat(detail?.modified || []);
  const createdPaths = created.map((e) => e.path);
  const cellLinks = delta.cell_links_by_owner?.[key] || null;
  const projPaths = delta.project_paths
    || (delta.unattributed_sample?.['project-file'] || []).map((e) => e.path);

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
  const reasons = [];
  const P = override?.predicate || spec.predicate || {};
  if (override) reasons.push(`per-package predicate: ${override.why || 'see classes.json'}`);
  if (!override && spec.strength === 'NONE') {
    effect = null;
    reasons.push('class has no reliable work-signal');
  } else {
    effect = true;
    if (P.created_all_of) for (const re of P.created_all_of) {
      const hit = createdPaths.some((p) => new RegExp(re.replace(/^\^pkg:/, '')).test(p));
      reasons.push(`all_of ${re}: ${hit ? 'HIT' : 'MISS'}`); if (!hit) effect = false;
    }
    if (P.created_any_of) {
      const hit = P.created_any_of.some((re) => createdPaths.some((p) => new RegExp(re.replace(/^\^[a-z|()]*pkg[a-z|()]*:/, '')).test(p)));
      reasons.push(`any_of: ${hit ? 'HIT' : 'MISS'}`); if (!hit) effect = false;
    }
    if (P.changed_any_of) {
      // Project-scoped: the effect lands OUTSIDE the writer's own cell (a hook
      // installer's `.git/hooks`, msw's `public/mockServiceWorker.js`).
      const hit = P.changed_any_of.some((re) => projPaths.some((p) => new RegExp(re).test(p)));
      reasons.push(`changed_any_of(project): ${hit ? 'HIT' : 'MISS'}`); if (!hit) effect = false;
    }
    if (P.cell_changed_any_of) {
      // Cell-scoped: the effect lands inside the writer's OWN cell but under a
      // path no name rule can claim for it — deckgl-typings copies 22 `@types/*`
      // trees next to itself. Only entries that appeared in the WINDOW count,
      // and the linker finished before the window opened, so a hit here is the
      // script's work rather than materialisation.
      const paths = cellLinks?.paths || [];
      const hit = P.cell_changed_any_of.some((re) => paths.some((p) => new RegExp(re).test(p)));
      reasons.push(`cell_changed_any_of: ${hit ? 'HIT' : 'MISS'} (${paths.length} cell entries)`); if (!hit) effect = false;
    }
    if (P.min_created != null) {
      const hit = created.length >= P.min_created;
      reasons.push(`min_created ${P.min_created}: got ${created.length}`); if (!hit) effect = false;
    }
    // FILES AND BYTES, NOT PATHS. A count of created entries is satisfied by a
    // directory, and an empty staging directory is exactly what a downloader
    // leaves behind when its fetch is refused.
    if (P.min_created_files != null) {
      const n = owner?.created_files ?? 0;
      const hit = n >= P.min_created_files;
      reasons.push(`min_created_files ${P.min_created_files}: got ${n} ${hit ? 'HIT' : 'MISS'}`); if (!hit) effect = false;
    }
    if (P.min_created_bytes != null) {
      const n = owner?.created_bytes ?? 0;
      const hit = n >= P.min_created_bytes;
      reasons.push(`min_created_bytes ${P.min_created_bytes}: got ${n} ${hit ? 'HIT' : 'MISS'}`); if (!hit) effect = false;
    }
    if (P.content_nonce) {
      // The cell base must be resolved against BOTH store-shaped locations: the
      // global content-hashed CAS store under HOME, and the project virtual
      // store, which is where per-project generated output actually lands.
      // Searching only the global one reported a false NEVER-RAN on Prisma while
      // its nonce-bearing generated client sat on disk.
      // A base that does not exist is silently skipped, so a stale list reads as a
      // clean NEVER-RAN rather than as a lookup failure — which is why the runner
      // passes the roots it actually configured instead of this file guessing.
      // `NUB_CACHE_DIR` relocates the shared store to `<cache>/store`, so the
      // `$HOME/.cache/nub/pm/store` form below is dead whenever it is set.
      const bases = (process.env.STORE_BASES || '')
        .split('\n')
        .map((s) => s.trim())
        .filter(Boolean)
        .concat([
          `${process.env.STOREROOT}/.cache/nub/pm/store`,
          `${process.env.PROJ}/node_modules/.store`,
        ]);
      let hit = false, cellSeen = false;
      for (const base of bases) {
        if (!fs.existsSync(base)) continue;
        const cell = fs.readdirSync(base).find((d) => d.startsWith(`${m.pkg.replace(/\//g, '+')}@${m.version}`));
        if (!cell) continue;
        cellSeen = true;
        hit = nonceInOwnerDelta(detail, path.join(base, cell, 'node_modules'));
        if (hit) break;
      }
      // THE ONLY OPERATOR THAT READS THE LIVE FILESYSTEM, so it is the only one
      // that can be asked a question the evidence cannot answer. Re-scoring an
      // ARCHIVED run has snapshots but no store, and "the file is not there" is
      // then indistinguishable from "the script never wrote it" — a false break,
      // in the one class whose signal is otherwise the strongest we have. Say
      // UNEVALUABLE instead of guessing.
      if (!hit && !cellSeen) {
        effect = null;
        reasons.push('content_nonce: UNEVALUABLE — no store cell on disk (re-scoring an archived run cannot reach the generated file)');
      } else {
        reasons.push(`content_nonce: ${hit || 'NOT FOUND'}`); if (!hit) effect = false;
      }
    }
  }

  // `kind` DESCRIBES THE PACKAGE'S OWN CELL, so it may only veto CELL-SCOPED
  // evidence. A class whose effect lands somewhere else entirely — a hook
  // installer writing `.git/hooks`, msw writing `public/mockServiceWorker.js`,
  // deckgl-typings copying `@types/*` beside itself — touches nothing under its
  // own name, so `installed-no-delta` is its NORMAL state and vetoing on it
  // scored a working package as never having run. This surfaced only once the
  // `.bin` symlinks were correctly disowned: before that, linker scaffolding
  // gave every such package a spurious own-cell delta, and the veto never fired.
  // Two bugs cancelling is not two bugs fixed.
  const externalOnly = (P.changed_any_of || P.cell_changed_any_of)
    && !P.created_all_of && !P.created_any_of && P.min_created == null
    && P.min_created_files == null && P.min_created_bytes == null && !P.content_nonce;

  let verdict;
  if (effect === null) verdict = 'UNSIGNALLED';
  else if (kind === 'absent') verdict = 'NOT-INSTALLED';
  else if (effect && externalOnly) verdict = 'DID-WORK-AND-SUCCEEDED';
  else if (kind === 'installed-no-delta') verdict = 'NEVER-RAN-ITS-REAL-PATH';
  else if (kind === 'pm-materialisation') verdict = 'NEVER-RAN-ITS-REAL-PATH';
  else if (effect) verdict = 'DID-WORK-AND-SUCCEEDED';
  else if (acted) verdict = 'DID-WORK-AND-FAILED';
  else verdict = 'NEVER-RAN-ITS-REAL-PATH';

  // SCHEDULING EVIDENCE IS RECORDED HERE AND JUDGED IN THE AGGREGATE, because
  // deciding it needs BOTH arms. `silent-in-truncated-window` says only "this
  // package left no trace in a window where a sibling failed first" — which is
  // the ordinary no-op outcome for most of the ecosystem, and is a manufactured
  // break ONLY where the other arm proves the package had work to do. Ruling
  // here, on one arm, condemned 315 of 345 rows and threw the corpus away.
  let scheduling = 'ran';
  if (failedByName.has(m.pkg)) scheduling = 'named-failed';
  else if (verdict === 'NEVER-RAN-ITS-REAL-PATH') scheduling = schedulingTruncated ? 'silent-in-truncated-window' : 'silent';

  results.push({
    pkg: m.pkg, version: m.version, class: m.cls, kind, acted,
    changed: changed.length, effect, verdict, scheduling, reasons,
    // Carried per row so the aggregate can diff the confined artifact against the
    // unconfined one. A pass is only as good as what it produced, and the two
    // arms are the only scale on which "big enough" means anything.
    artifact: {
      files: owner?.created_files ?? 0,
      bytes: owner?.created_bytes ?? 0,
      largest_file: owner?.largest_file ?? null,
      largest_file_bytes: owner?.largest_file_bytes ?? 0,
      cell_link_files: cellLinks?.files ?? 0,
      cell_link_bytes: cellLinks?.bytes ?? 0,
    },
  });
}

const summary = {};
for (const r of results) summary[r.verdict] = (summary[r.verdict] || 0) + 1;
const out = {
  shard: process.env.SHARD, arm: process.env.ARM,
  rc_install: Number(process.env.RC_INSTALL || 0), rc_script: rcScript,
  suppressed: (log.match(/SUPPRESSED capability: (\S+)/) || [])[1] || null,
  scheduling_truncated: schedulingTruncated,
  approved_jobs: approved,
  // A one-row manifest is the ISOLATED measurement, and the aggregate lets it
  // supersede whatever the batch screen said about the same package.
  manifest_rows: manifest.length,
  isolated: manifest.length === 1,
  failed_packages: [...failedByName],
  summary, results,
  unattributed: delta.unattributed_counts,
  interaction_candidates: (delta.unattributed_sample['cell-dependency-link'] || []).slice(0, 20),
};
fs.writeFileSync(outF, JSON.stringify(out, null, 2));
console.log(JSON.stringify({ shard: out.shard, arm: out.arm, suppressed: out.suppressed, rc_script: rcScript, summary }));
for (const r of results) console.log(`   ${r.verdict.padEnd(24)} ${r.pkg}@${r.version} [${r.class}] changed=${r.changed} kind=${r.kind}`);

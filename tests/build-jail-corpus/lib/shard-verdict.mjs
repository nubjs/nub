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
  for (const rel of [...(ownerDetail.created || []), ...(ownerDetail.modified || [])]) {
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
  if (!Object.prototype.hasOwnProperty.call(classes, m.cls)) {
    console.error(
      `shard-verdict: FATAL — ${m.pkg}@${m.version} names class ${JSON.stringify(m.cls)}, ` +
        `which is absent from classes.json. Known: ${Object.keys(classes).join(', ')}. ` +
        `No verdict from this run is interpretable; fix the manifest rather than reading past it.`
    );
    process.exit(4);
  }
  const spec = classes[m.cls];
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
  const reasons = [];
  const P = spec.predicate || {};
  if (spec.strength === 'NONE') {
    effect = null;
    reasons.push('class has no reliable work-signal');
  } else {
    effect = true;
    if (P.created_all_of) for (const re of P.created_all_of) {
      const hit = created.some((p) => new RegExp(re.replace(/^\^pkg:/, '')).test(p));
      reasons.push(`all_of ${re}: ${hit ? 'HIT' : 'MISS'}`); if (!hit) effect = false;
    }
    if (P.created_any_of) {
      const hit = P.created_any_of.some((re) => created.some((p) => new RegExp(re.replace(/^\^[a-z|()]*pkg[a-z|()]*:/, '')).test(p)));
      reasons.push(`any_of: ${hit ? 'HIT' : 'MISS'}`); if (!hit) effect = false;
    }
    if (P.changed_any_of) {
      // Project-scoped predicates (hook installers) look at the project delta,
      // not the package's own cell — the effect lands outside the writer.
      const projPaths = (delta.unattributed_sample['project-file'] || []).map((e) => e.path);
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

  results.push({ pkg: m.pkg, version: m.version, class: m.cls, kind, acted, changed: changed.length, effect, verdict, reasons });
}

const summary = {};
for (const r of results) summary[r.verdict] = (summary[r.verdict] || 0) + 1;
const out = {
  shard: process.env.SHARD, arm: process.env.ARM,
  rc_install: Number(process.env.RC_INSTALL || 0), rc_script: rcScript,
  suppressed: (log.match(/SUPPRESSED capability: (\S+)/) || [])[1] || null,
  summary, results,
  unattributed: delta.unattributed_counts,
  interaction_candidates: (delta.unattributed_sample['cell-dependency-link'] || []).slice(0, 20),
};
fs.writeFileSync(outF, JSON.stringify(out, null, 2));
console.log(JSON.stringify({ shard: out.shard, arm: out.arm, suppressed: out.suppressed, rc_script: rcScript, summary }));
for (const r of results) console.log(`   ${r.verdict.padEnd(24)} ${r.pkg}@${r.version} [${r.class}] changed=${r.changed} kind=${r.kind}`);

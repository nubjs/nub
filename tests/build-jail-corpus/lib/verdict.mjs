#!/usr/bin/env node
// verdict.mjs — apply the per-class work predicate and emit the verdict.
//
// THE VERDICT IS THREE STATES PLUS A VALIDITY GATE, and the gate is the part
// that makes the three trustworthy:
//
//   INVALID-FIXTURE          the package does not produce its class effect even
//                            UNCONFINED (arm A0). The fixture is wrong, so every
//                            jail-arm result for this package is uninterpretable
//                            and is excluded from all numbers. Prisma-with-no-
//                            schema lands here and can therefore never contribute
//                            a reassuring "works under the jail".
//   DID-WORK-AND-SUCCEEDED   the class effect is present in the window's delta.
//   DID-WORK-AND-FAILED      it acted and was stopped, or it exited non-zero.
//                            The rc=0-but-effect-missing cell is the silent
//                            degradation class and is called out separately.
//   NEVER-RAN-ITS-REAL-PATH  rc=0, nothing changed anywhere, no class effect.
//
// Ordering note: rc!=0 is evaluated BEFORE the effect test, because a script
// that generates its artifact and then crashes has still failed, and calling it
// a success because the artifact landed would hide a real break.

import fs from 'node:fs';

const [deltaF, logF, className, classesF, ...rest] = process.argv.slice(2);
const rc = Number(process.env.RUN_RC ?? '0');
const nonce = process.env.RUN_NONCE || '';
const a0Effect = process.env.A0_CLASS_EFFECT; // 'true' | 'false' | undefined (this run IS A0)

const delta = JSON.parse(fs.readFileSync(deltaF, 'utf8'));
const classes = JSON.parse(fs.readFileSync(classesF, 'utf8'));
const spec = classes[className];
if (!spec) {
  console.error(`verdict: unknown class ${className}`);
  process.exit(2);
}
const log = fs.existsSync(logF) ? fs.readFileSync(logF, 'utf8') : '';

const createdKeys = delta.created.map((c) => `${c.root}:${c.rel}`);
const changedKeys = createdKeys.concat(delta.modified.map((c) => `${c.root}:${c.rel}`));

const reasons = [];
function evalPredicate(pred) {
  if (!pred || Object.keys(pred).length === 0) return null; // unsignalled class
  let ok = true;
  if (pred.created_all_of) {
    for (const re of pred.created_all_of) {
      const hit = createdKeys.some((k) => new RegExp(re).test(k));
      reasons.push(`created_all_of ${re}: ${hit ? 'HIT' : 'MISS'}`);
      if (!hit) ok = false;
    }
  }
  if (pred.created_any_of) {
    const hit = pred.created_any_of.some((re) => createdKeys.some((k) => new RegExp(re).test(k)));
    reasons.push(`created_any_of: ${hit ? 'HIT' : 'MISS'}`);
    if (!hit) ok = false;
  }
  if (pred.changed_any_of) {
    const hit = pred.changed_any_of.some((re) => changedKeys.some((k) => new RegExp(re).test(k)));
    reasons.push(`changed_any_of: ${hit ? 'HIT' : 'MISS'}`);
    if (!hit) ok = false;
  }
  if (pred.min_created != null) {
    const hit = delta.counts.created >= pred.min_created;
    reasons.push(`min_created ${pred.min_created}: got ${delta.counts.created} ${hit ? 'HIT' : 'MISS'}`);
    if (!hit) ok = false;
  }
  if (pred.content_nonce) {
    if (!nonce) {
      reasons.push('content_nonce: NO RUN_NONCE SET — predicate cannot be evaluated');
      return null;
    }
    const cands = delta.created.concat(delta.modified).filter((c) => new RegExp(pred.content_nonce.where).test(`${c.root}:${c.rel}`));
    // The nonce test reads the file at its recorded absolute location; ROOTS_JSON
    // maps each root name back to its absolute base so the check never guesses a
    // path (a relative fallback is how a probe silently reads the wrong file).
    const roots = JSON.parse(process.env.ROOTS_JSON || '{}');
    let found = null;
    for (const c of cands) {
      const base = roots[c.root];
      if (!base) continue;
      const abs = `${base}/${c.rel}`;
      try {
        const st = fs.statSync(abs);
        if (!st.isFile() || st.size > 64 * 1024 * 1024) continue;
        if (fs.readFileSync(abs, 'utf8').includes(nonce)) { found = `${c.root}:${c.rel}`; break; }
      } catch {}
    }
    reasons.push(`content_nonce "${nonce}" in ${cands.length} candidates: ${found ? 'FOUND in ' + found : 'NOT FOUND'}`);
    if (!found) ok = false;
  }
  if (pred.log_match) {
    const hit = new RegExp(pred.log_match).test(log);
    reasons.push(`log_match: ${hit ? 'HIT' : 'MISS'}`);
    if (!hit) ok = false;
  }
  return ok;
}

const classEffect = spec.strength === 'NONE' ? null : evalPredicate(spec.predicate);
const acted = delta.acted;

let verdict, note = '';
if (spec.strength === 'NONE' || classEffect === null) {
  verdict = 'UNSIGNALLED';
  note = 'class has no reliable work-signal; see classes.json rationale';
} else if (a0Effect === 'false') {
  verdict = 'INVALID-FIXTURE';
  note = 'the class effect was absent in the A0 unconfined baseline — the fixture never reached the real path, so no jail-arm conclusion is admissible';
} else if (rc !== 0) {
  verdict = 'DID-WORK-AND-FAILED';
  note = `non-zero exit ${rc}${classEffect ? ' (class effect partially present)' : ''}`;
} else if (classEffect) {
  verdict = 'DID-WORK-AND-SUCCEEDED';
} else if (acted) {
  verdict = 'DID-WORK-AND-FAILED';
  note = 'SILENT DEGRADATION: exit 0 and the filesystem changed, but the class effect is absent';
} else {
  verdict = 'NEVER-RAN-ITS-REAL-PATH';
  note = 'exit 0 and nothing changed in any observed root';
}

const out = {
  class: className, strength: spec.strength, rc, acted,
  class_effect: classEffect, a0_class_effect: a0Effect ?? null,
  verdict, note, reasons, counts: delta.counts, by_root: delta.by_root,
};
console.log(JSON.stringify(out, null, 2));
if (rest[0]) fs.writeFileSync(rest[0], JSON.stringify(out, null, 2));

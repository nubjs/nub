#!/usr/bin/env node
// windows.mjs — per-package attribution from the runner's own window markers.
//
// WHY THIS IS THE ATTRIBUTION SOURCE. `PER_PKG=1` drives one `approve-builds
// <name>` per pending package and brackets each one with
// `--- window: <name> (rc=N)`, so the log already carries an exact, parsed
// package boundary. Everything that tried to RE-DERIVE that boundary downstream
// guessed, and guessed badly: errsig required a store path inside the error line
// itself, which most lifecycle failures do not have (`getaddrinfo ENOTFOUND
// github.com` names nothing), so 61 of 291 classified lines on one shard fell
// into a `(shard-level)` bucket that joins to no row and only 3 of 10 break rows
// carried a signature at all.
//
// The marker's rc belongs to the NAMED package, not to a set: `approve-builds
// <name>` approves that name only. The one window that can hold two jobs holds
// two VERSIONS of the same name, which is still the same package.
//
// A `--all` run emits no markers. Every function here then reports "no windows",
// and callers fall back to their previous behaviour rather than inventing one.

import crypto from 'node:crypto';

const MARKER = /^--- window: (\S+) \(rc=(-?\d+)\)/;
// THE LAST WINDOW NEEDS AN EXPLICIT END or it swallows the whole epilogue — the
// arm-effect assertion, the node-gyp identity probe, the attribution control —
// and attributes every classifiable line in it to whichever package happened to
// run last. The runner already prints a terminator; this matches it rather than
// assuming the log ends at the final window.
const END = /^(per-package windows=|approve rc=|=== ARM EFFECT ===)/;

// Segments over the LINE array, so a caller can attribute by line index without
// re-splitting or re-joining the log.
export function parseWindows(lines) {
  const segs = [];
  for (let i = 0; i < lines.length; i++) {
    if (segs.length && END.test(lines[i]) && segs[segs.length - 1].to > i) {
      segs[segs.length - 1].to = i;
      continue;
    }
    const m = lines[i].match(MARKER);
    if (!m) continue;
    if (segs.length) segs[segs.length - 1].to = Math.min(segs[segs.length - 1].to, i);
    segs.push({ pkg: m[1], rc: Number(m[2]), from: i + 1, to: lines.length });
  }
  return segs;
}

export function ownerAt(segs, i) {
  for (const s of segs) if (i >= s.from && i < s.to) return s.pkg;
  return null;
}

// pkg -> { rc, text }. A name appearing twice (it should not) is concatenated
// rather than silently dropped, and its rc is the worst of the two.
export function windowsByPkg(lines, segs) {
  const by = new Map();
  for (const s of segs) {
    const text = lines.slice(s.from, s.to).join('\n');
    const prev = by.get(s.pkg);
    by.set(s.pkg, prev ? { rc: prev.rc || s.rc, text: `${prev.text}\n${text}` } : { rc: s.rc, text });
  }
  return by;
}

// NORMALISATION EXISTS TO MAKE THE TWO ARMS COMPARABLE, AND NOTHING MORE. Only
// differences the arm itself manufactures are removed:
//   - the opt-out warning, which A0 prints BY CONSTRUCTION and PROD cannot;
//   - the run directory, which carries the arm name in its path;
//   - the per-run nonce;
//   - wall-clock stamps.
// Everything else is left alone deliberately. Over-normalising here would hide a
// real break (the two arms would read as identical when the jail had in fact
// changed the outcome), so the bias is set the other way: an unrecognised
// difference keeps the arms DISTINCT, and the caller keeps its break.
export function normaliseWindow(text, { outDir, nonce } = {}) {
  let t = text
    .split('\n')
    .filter((l) => !/running without the build sandbox/.test(l))
    .map((l) => l.replace(/\s+$/, ''))
    .join('\n');
  if (outDir) t = t.split(outDir).join('<RUN>');
  if (nonce) t = t.split(nonce).join('<NONCE>');
  t = t
    .replace(/\d{4}[-/]\d{2}[-/]\d{2}[ T]\d{2}:\d{2}:\d{2}(\.\d+)?Z?/g, '<TS>')
    .replace(/\b\d{2}:\d{2}:\d{2}\b/g, '<TS>');
  return t.replace(/^\n+|\n+$/g, '');
}

export const digest = (s) => crypto.createHash('sha256').update(s).digest('hex').slice(0, 16);

// The run's own output directory, needed to strip the arm name out of every
// absolute path the scripts print. Recorded in the provenance block by
// run-shard.sh; older runs predate that line and fall back to the caller's PROJ,
// which is correct for an archive that was never moved. A MOVED archive simply
// fails to normalise, which leaves the two arms distinct — the safe direction.
export function outDirFromLog(log, projEnv) {
  const m = log.match(/^out=(.+)$/m);
  if (m) return m[1].trim();
  return projEnv ? projEnv.replace(/[\\/]proj[\\/]?$/, '') : null;
}

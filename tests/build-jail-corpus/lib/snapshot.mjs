#!/usr/bin/env node
// snapshot.mjs — record the observable state of a set of roots at one instant.
//
// DESIGN NOTE (this is the load-bearing part of the whole method):
// The work-signal is a SET DIFFERENCE over two snapshots that bracket the
// lifecycle-script window, NOT a timestamp test and NOT an artifact-presence
// test. Both of those were measured to be unsound here:
//   * artifact PRESENCE false-positives whenever a shared CAS store persists an
//     artifact across runs — the file is there because a previous run made it.
//   * ctime > T0 false-positives on a merely-MATERIALISED file: measured on
//     APFS, both hardlink and clonefile bump the ctime of the target past the
//     fence while its mtime stays at the archive value.
//   * mtime > T0 false-NEGATIVES on anything a script extracts from a tarball,
//     because bsdtar restores mtime AND birthtime from the archive (measured;
//     only ctime moves).
// A path-set difference plus a content digest is immune to all three, so
// timestamps are recorded as corroborators and are never the primary test.
//
// Symlinks are lstat'd, never followed: node_modules is dense with links into
// the store, and following them would double-count the store root and make the
// walk quadratic. The store is snapshotted as its own root instead.

import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';

const HASH_MAX = 1024 * 1024; // digest files under 1 MiB; larger use size+ino identity
const SKIP_DIR_NAMES = new Set(['.git/objects', '.git/logs']); // huge + irrelevant

function digest(abs, size) {
  if (size > HASH_MAX) return `big:${size}`;
  try {
    return 'sha256:' + crypto.createHash('sha256').update(fs.readFileSync(abs)).digest('hex').slice(0, 32);
  } catch (e) {
    return `unreadable:${e.code}`;
  }
}

function walk(root, rootName, out, budget) {
  const stack = [root];
  while (stack.length) {
    if (budget.n > budget.max) {
      out.push({ root: rootName, rel: '__TRUNCATED__', type: 'meta' });
      return;
    }
    const dir = stack.pop();
    let ents;
    try {
      ents = fs.readdirSync(dir, { withFileTypes: true });
    } catch (e) {
      out.push({ root: rootName, rel: path.relative(root, dir) || '.', type: 'dir-unreadable', err: e.code });
      continue;
    }
    for (const ent of ents) {
      const abs = path.join(dir, ent.name);
      // POSIX SEPARATORS ARE THE WIRE FORMAT, and this line is what makes the probe
      // measurable on Windows at all. `path.relative` yields backslashes there, while
      // every downstream consumer matches on `/`: the store-cell and project-cell
      // regexes in attribute.mjs, and every class predicate in classes.json
      // (`^proj:\.git/hooks/`, `^pkg:(prebuilds|build/Release)/`, …). With backslashes
      // nothing matched, so the whole Windows corpus attributed to nobody — 12,244
      // entries landed as unattributed `home`, `installed_cells` came back EMPTY, and
      // all 338 packages reported NOT-INSTALLED while being installed perfectly well.
      // That reads as a catastrophic result rather than as an unparsed path, and it
      // survived a green job, so normalise at the source rather than per consumer.
      const rel = path.relative(root, abs).split(path.sep).join('/');
      if ([...SKIP_DIR_NAMES].some((s) => rel === s || rel.startsWith(s + '/'))) continue;
      budget.n++;
      let st;
      try {
        // `bigint: true` is required for the *Ns fields to exist at all; without
        // it Node exposes only float milliseconds, which cannot round-trip a
        // nanosecond timestamp and silently loses the sub-ms ordering a fast
        // script needs.
        st = fs.lstatSync(abs, { bigint: true });
      } catch (e) {
        out.push({ root: rootName, rel, type: 'gone', err: e.code });
        continue;
      }
      const base = {
        root: rootName,
        rel,
        mode: Number(st.mode & 0o7777n),
        mtime: String(st.mtimeNs),
        ctime: String(st.ctimeNs),
        btime: String(st.birthtimeNs),
        ino: String(st.ino),
        nlink: Number(st.nlink),
      };
      if (st.isSymbolicLink()) {
        let target = null;
        try {
          target = fs.readlinkSync(abs);
        } catch {}
        out.push({ ...base, type: 'link', target, size: 0, digest: `link:${target}` });
      } else if (st.isDirectory()) {
        out.push({ ...base, type: 'dir', size: 0, digest: 'dir' });
        stack.push(abs);
      } else if (st.isFile()) {
        out.push({ ...base, type: 'file', size: Number(st.size), digest: digest(abs, Number(st.size)) });
      } else {
        out.push({ ...base, type: 'other', size: 0, digest: 'other' });
      }
    }
  }
}

// argv: <outfile> <name>=<absdir> [<name>=<absdir> ...]
const [outFile, ...rootArgs] = process.argv.slice(2);
if (!outFile || !rootArgs.length) {
  console.error('usage: snapshot.mjs <outfile.ndjson> <name>=<absdir> ...');
  process.exit(2);
}
const records = [];
const budget = { n: 0, max: Number(process.env.SNAP_MAX_ENTRIES || 400000) };
for (const arg of rootArgs) {
  const i = arg.indexOf('=');
  const name = arg.slice(0, i);
  const dir = arg.slice(i + 1);
  if (!path.isAbsolute(dir)) {
    // A relative root would resolve against cwd, and a probe that silently
    // falls back to cwd is exactly how a "successful write" gets recorded in
    // the wrong place and reads as a pass.
    console.error(`snapshot: root ${name} is not absolute: ${dir}`);
    process.exit(2);
  }
  if (!fs.existsSync(dir)) {
    records.push({ root: name, rel: '__ABSENT__', type: 'meta' });
    continue;
  }
  walk(dir, name, records, budget);
}
fs.writeFileSync(outFile, records.map((r) => JSON.stringify(r)).join('\n') + '\n');
console.error(`snapshot: ${records.length} records -> ${outFile}`);

// TRUNCATION MUST BE FATAL, NOT A MARKER. `walk` stops at the entry budget and
// pushes `__TRUNCATED__`, but `delta.mjs` has no handling for it — so a truncated
// snapshot silently yields a delta computed over a PARTIAL tree, and every package
// whose files fell past the cut reports "no owned delta", which the verdict rule
// reads as NEVER-RAN-ITS-REAL-PATH. That is a false negative wearing the costume of
// a benign verdict, which is the exact failure mode this method exists to prevent.
// Harmless at 48 packages (well under the cap); reachable at 383 with native builds,
// where the store alone runs to hundreds of thousands of entries.
if (records.some((r) => r.rel === '__TRUNCATED__')) {
  console.error(
    `snapshot: FATAL — hit SNAP_MAX_ENTRIES (${budget.max}). The delta would be computed ` +
      `over a partial tree and would report NEVER-RAN for every package past the cut. ` +
      `Raise SNAP_MAX_ENTRIES and re-run; do not interpret this run.`
  );
  process.exit(3);
}

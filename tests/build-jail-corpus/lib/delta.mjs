#!/usr/bin/env node
// delta.mjs — diff two snapshots into the four change classes, per root.
//
// ACTED (the generic, class-independent "did it touch anything?" signal) is
// CREATED ∪ MODIFIED ∪ DELETED being non-empty. TOUCHED-ONLY is reported but
// deliberately excluded from ACTED: a metadata-only bump is produced by store
// materialisation and by any stat-updating read, so counting it would reinstate
// exactly the false positive the set-difference design exists to remove.

import fs from 'node:fs';
import { groupByOwner, cellsPresentIn } from './attribute.mjs';

// PM BOOKKEEPING — excluded from the delta, and counted separately so the
// exclusion is visible rather than silent. These are written by nub itself
// during `approve-builds`, not by the lifecycle script, and without the filter
// every package "acts" trivially: the store alone contributed 6,592 CREATED
// entries in the first real run, swamping a signal of order 1-20 files.
// Each entry was identified empirically from a measured run, not guessed.
const PM_NOISE = [
  // nub's OWN bookkeeping. Deliberately NOT a blanket store exclusion: the store
  // is where package build output actually lands (bcrypt's .node and every .o
  // were written under store/bcrypt@6.0.0-<hash>/), so excluding the store
  // wholesale would discard the very artifacts the study must detect. Only the
  // store's index/lock/tmp scaffolding is noise.
  /store\/(?:\.tmp|\.index|_locks|index-v\d)/,
  /^home:Library\/Caches\/nub\//,          // PM adaptive state
  /^home:[^:]*\/pm\/(?:meta|adaptive|index)/,
  /^store:/,                                 // the explicit NUB_CACHE_DIR root
  /^proj:package\.json$/,                    // the PM normalises the manifest it was handed
  /^proj:nub\.lock$/,
  /^proj:node_modules\/\.(modules|nub|package-lock)/,
  /\.aube-side-effects-cache/,               // aube's side-effect replay cache
  /^tmp:fslock\//,                           // PM file locks
];
const isNoise = (k) => PM_NOISE.some((re) => re.test(k));

const load = (f) =>
  fs
    .readFileSync(f, 'utf8')
    .split('\n')
    .filter(Boolean)
    .map((l) => JSON.parse(l));

// `root:rel` — the SAME form verdict.mjs matches its class predicates against.
// These were briefly different (this line held a stray NUL separator while the
// noise regexes and verdict.mjs both used a colon), so the PM-noise filter
// silently matched nothing and left 6,588 store entries in the delta.
const key = (r) => `${r.root}:${r.rel}`;

const [preF, postF, outF] = process.argv.slice(2);
if (!preF || !postF || !outF) {
  console.error('usage: delta.mjs <pre.ndjson> <post.ndjson> <out.json>');
  process.exit(2);
}

const preRecs = load(preF);
const postRecs = load(postF);
const preCells = cellsPresentIn(preRecs);
const pre = new Map(preRecs.map((r) => [key(r), r]));
const post = new Map(postRecs.map((r) => [key(r), r]));

const created = [], modified = [], deleted = [], touched = [];
const excluded = { created: 0, modified: 0, deleted: 0 };

for (const [k, b] of post) {
  if (b.rel.startsWith('__')) continue;
  const a = pre.get(k);
  if (isNoise(k)) { if (!a) excluded.created++; else if (a.digest !== b.digest) excluded.modified++; continue; }
  if (!a) {
    created.push({ root: b.root, rel: b.rel, type: b.type, size: b.size, digest: b.digest });
  } else if (a.digest !== b.digest || a.size !== b.size || a.type !== b.type) {
    modified.push({
      root: b.root, rel: b.rel, type: b.type,
      size_before: a.size, size_after: b.size,
      digest_before: a.digest, digest_after: b.digest,
    });
  } else if (a.mtime !== b.mtime || a.ctime !== b.ctime || a.ino !== b.ino) {
    touched.push({
      root: b.root, rel: b.rel,
      mtime_moved: a.mtime !== b.mtime,
      ctime_moved: a.ctime !== b.ctime,
      ino_changed: a.ino !== b.ino,
    });
  }
}
for (const [k, a] of pre) {
  if (a.rel.startsWith('__')) continue;
  if (isNoise(k)) { if (!post.has(k)) excluded.deleted++; continue; }
  if (!post.has(k)) deleted.push({ root: a.root, rel: a.rel, type: a.type });
}

const byRoot = {};
for (const [cls, arr] of [['created', created], ['modified', modified], ['deleted', deleted], ['touched', touched]]) {
  for (const r of arr) {
    byRoot[r.root] ??= { created: 0, modified: 0, deleted: 0, touched: 0 };
    byRoot[r.root][cls]++;
  }
}

// Attribution runs over the FULL arrays, BEFORE the display cap. It was briefly
// computed after capping at 400 entries, which silently hid every store-resident
// build artifact and made byOwner come back empty on a package that had plainly
// compiled.
const attrib = groupByOwner({ created, modified, deleted });
const owners = {};
for (const [o, v] of attrib.byOwner) owners[o] = { created: v.created.length, modified: v.modified.length, deleted: v.deleted.length, cell_preexisting: preCells.has(o), kind: preCells.has(o) ? 'script-output' : 'pm-materialisation' };
const unattributed_counts = Object.fromEntries(Object.entries(attrib.unattributed).map(([k, v]) => [k, v.length]));

const acted = created.length + modified.length + deleted.length > 0;
const cap = (a, n = 400) => a.slice(0, n);

fs.writeFileSync(
  outF,
  JSON.stringify(
    {
      acted,
      excluded_pm_noise: excluded,
      by_owner: owners,
      installed_cells: [...new Set([...preCells, ...cellsPresentIn(postRecs)])],
      unattributed_counts,
      unattributed_sample: Object.fromEntries(Object.entries(attrib.unattributed).map(([k, v]) => [k, v.slice(0, 40)])),
      owner_detail: Object.fromEntries([...attrib.byOwner].map(([o, v]) => [o, { created: v.created.slice(0, 200), modified: v.modified.slice(0, 100) }])),
      counts: { created: created.length, modified: modified.length, deleted: deleted.length, touched: touched.length },
      by_root: byRoot,
      created: cap(created), modified: cap(modified), deleted: cap(deleted), touched: cap(touched, 100),
    },
    null,
    2,
  ),
);
console.error(
  `delta: acted=${acted} created=${created.length} modified=${modified.length} deleted=${deleted.length} touched=${touched.length}`,
);

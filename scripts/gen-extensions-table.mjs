#!/usr/bin/env node
// Generate the blog's package-extensions table data from the published
// `@nubjs/extensions` package plus npm's weekly download counts.
//
// The version is PINNED, not `@latest`, because the blog prose quotes this
// dataset's own figures (791 packages, 142 from Yarn, 649 from the scan). The
// database rebuilds daily and publishes a patch whenever the rules move, so
// `@latest` would silently put a different row count under a paragraph that
// states 791. Bumping the pin is a reviewable edit that comes with re-reading
// the prose. Pass a spec to override:
//
//     node scripts/gen-extensions-table.mjs @nubjs/extensions@1.0.5
//
// Downloads are anchored to npm's own last-complete-week window rather than a
// date this script computes, so every row in one file covers one identical
// period and the caption can name it. A name npm has no counts for keeps a
// null and sorts last -- the row still ships, because the argument the table
// makes is about coverage, not about popularity.
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';

const OUT = new URL('../site/src/data/package-extensions.json', import.meta.url);
const API = 'https://api.npmjs.org/downloads/point';
const args = process.argv.slice(2);
// Reshaping a row costs ~365 registry requests otherwise. `--reuse-downloads`
// keeps the counts already in the output file, but only after checking they
// cover npm's CURRENT last-complete-week and every name in the dataset — so it
// is a cache hit or a full refetch, never a stale mixture.
const reuse = args.includes('--reuse-downloads');
const spec = args.find((arg) => !arg.startsWith('--')) ?? '@nubjs/extensions@1.0.4';

// Worst-first inside a row: the edge that can break an install is the one a
// reader is scanning for, so it leads and the type-only noise trails.
const CLASS_CODE = { runtime: 'r', adapter: 'a', guarded: 'g', types: 't' };
const CLASS_RANK = { r: 0, a: 1, g: 2, t: 3, '-': 4 };
const FIELD_RANK = { d: 0, q: 1, p: 2, o: 3 };

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Fetch and unpack the published tarball. `npm pack` resolves the spec. */
function fetchDataset(packageSpec) {
  const dir = mkdtempSync(join(tmpdir(), 'nub-extblog-'));
  execFileSync('npm', ['pack', packageSpec, '--silent'], {
    cwd: dir,
    stdio: ['ignore', 'ignore', 'inherit'],
  });
  const tgz = execFileSync('sh', ['-c', 'ls *.tgz'], { cwd: dir, encoding: 'utf8' }).trim();
  execFileSync('tar', ['xzf', tgz], { cwd: dir });
  return {
    pkg: JSON.parse(readFileSync(join(dir, 'package/package.json'), 'utf8')),
    data: JSON.parse(readFileSync(join(dir, 'package/package-extensions.json'), 'utf8')),
  };
}

/** `@scope/name@range` and `name@range` both split at the LAST `@`. */
const packageName = (selector) => selector.slice(0, selector.lastIndexOf('@'));

// Mirrors the dataset's own collector: three attempts with exponential backoff,
// a long floor on 429 so a rate-limit does not turn into a retry storm, and 404
// treated as a real "npm has no counts for this name" rather than an error.
async function json(url) {
  for (let attempt = 0; attempt < 3; attempt++) {
    let delay = 1000 * 2 ** attempt;
    try {
      const response = await fetch(url, { signal: AbortSignal.timeout(15_000) });
      if (response.ok) return await response.json();
      if (response.status === 404) return null;
      if (response.status === 429)
        delay = Math.max(
          30_000,
          Math.min(120_000, Number(response.headers?.get('retry-after')) * 1000 || 0),
        );
      if (attempt === 2) throw new Error(`HTTP ${response.status} from ${url}`);
    } catch (error) {
      if (attempt === 2) throw error;
    }
    await sleep(delay);
  }
  throw new Error('npm download API unavailable');
}

/** The counts already in the output file, if they cover this exact window. */
function cachedDownloads(period, names) {
  let previous;
  try {
    previous = JSON.parse(readFileSync(OUT, 'utf8'));
  } catch {
    return null;
  }
  if (previous.downloads?.start !== period.start || previous.downloads?.end !== period.end)
    return null;
  const downloads = Object.fromEntries(previous.rows.map((row) => [row.n, row.d]));
  if (names.some((name) => !(name in downloads))) return null;
  return downloads;
}

/** Weekly downloads for every name, keyed by name, `null` where npm has none. */
async function collectDownloads(names) {
  const period = await json(`${API}/last-week`);
  if (!/^\d{4}-\d{2}-\d{2}$/.test(period?.start) || !/^\d{4}-\d{2}-\d{2}$/.test(period?.end))
    throw new Error('npm returned an invalid weekly reporting period');
  const unique = [...new Set(names)].sort();
  if (reuse) {
    const cached = cachedDownloads(period, unique);
    if (cached) {
      console.log(`  reusing cached counts for ${period.start}..${period.end}`);
      return { start: period.start, end: period.end, downloads: cached };
    }
    console.log('  cached counts do not cover this week; refetching');
  }
  // The bulk endpoint takes up to 128 comma-joined names but rejects a scoped
  // name inside a batch, so those go one per request.
  const unscoped = unique.filter((name) => !name.startsWith('@'));
  const groups = [];
  for (let offset = 0; offset < unscoped.length; offset += 128)
    groups.push(unscoped.slice(offset, offset + 128));
  groups.push(...unique.filter((name) => name.startsWith('@')).map((name) => [name]));

  const downloads = Object.fromEntries(unique.map((name) => [name, null]));
  let cursor = 0;
  await Promise.all(
    Array.from({ length: Math.min(2, groups.length) }, async () => {
      while (cursor < groups.length) {
        const group = groups[cursor++];
        const data = await json(
          `${API}/${period.start}:${period.end}/${group.map(encodeURIComponent).join(',')}`,
        );
        for (const name of group) {
          const record = group.length === 1 ? data : data?.[name];
          // Only trust a record that names this package over this exact window:
          // npm answers an unknown name inside a batch with a null entry, and a
          // single-name request for an unknown name 404s.
          if (
            record?.package === name &&
            record.start === period.start &&
            record.end === period.end &&
            Number.isSafeInteger(record.downloads) &&
            record.downloads >= 0
          )
            downloads[name] = record.downloads;
        }
        if (cursor % 10 === 0) console.log(`  ${cursor}/${groups.length} download requests`);
        await sleep(500);
      }
    }),
  );
  return { start: period.start, end: period.end, downloads };
}

const { pkg, data } = fetchDataset(spec);

// Each row's edges come from the EXTENSION, not from `findings`: the extension
// is what actually installs, and it is the only source for the packages Yarn
// contributed that the scan never flagged. `findings` then annotates each edge
// with its class where it has one. Every finding target appears in the
// extension, so nothing is lost by walking the extension instead.
const edges = new Map();
for (const [selector, extension] of Object.entries(data.packageExtensions)) {
  const name = packageName(selector);
  if (!edges.has(name)) edges.set(name, new Map());
  const row = edges.get(name);
  for (const target of Object.keys(extension.dependencies ?? {})) row.set(target, 'd');
  for (const target of Object.keys(extension.peerDependencies ?? {}))
    if (!row.has(target))
      row.set(target, extension.peerDependenciesMeta?.[target]?.optional ? 'p' : 'q');
  // A `peerDependenciesMeta` key with no matching `peerDependencies` entry is a
  // different KIND of rule: the package already declares the peer and Yarn only
  // relaxes it to optional. Twelve of the carried rules are this shape, and
  // reading the two dependency fields alone leaves those rows with nothing to
  // show.
  for (const target of Object.keys(extension.peerDependenciesMeta ?? {}))
    if (!row.has(target)) row.set(target, 'o');
}

const classes = new Map();
for (const finding of data.findings)
  for (const target of finding.targets)
    classes.set(`${finding.package} ${target.target}`, CLASS_CODE[target.class] ?? '-');

const fromYarn = new Set(data.yarnKeys.map(packageName));

const names = [...edges.keys()];
const { start, end, downloads } = await collectDownloads(names);

const rows = names
  .map((name) => {
    const targets = [...edges.get(name)]
      .map(([target, field]) => [target, `${classes.get(`${name} ${target}`) ?? '-'}${field}`])
      .sort(
        (a, b) =>
          CLASS_RANK[a[1][0]] - CLASS_RANK[b[1][0]] ||
          FIELD_RANK[a[1][1]] - FIELD_RANK[b[1][1]] ||
          a[0].localeCompare(b[0]),
      );
    const row = { n: name, d: downloads[name], t: targets };
    if (fromYarn.has(name)) row.y = 1;
    return row;
  })
  // Downloads descending, unknown counts last, then alphabetical so the order is
  // stable across runs even where two packages tie.
  .sort((a, b) => (b.d ?? -1) - (a.d ?? -1) || a.n.localeCompare(b.n));

const header = {
  _source: '@nubjs/extensions',
  _refresh: `node scripts/gen-extensions-table.mjs ${spec}`,
  _legend: {
    n: 'package name',
    d: 'weekly npm downloads, null when npm returned no count',
    y: 'present when the rule was carried from @yarnpkg/extensions',
    t: '[target, "<class><field>"]; class r=runtime a=adapter g=guarded t=types -=not-scanned, field d=dependencies p=optional-peer q=required-peer o=existing-peer-relaxed-to-optional',
  },
  version: pkg.version,
  generated: data.generated,
  downloads: { start, end },
};

mkdirSync(dirname(OUT.pathname), { recursive: true });
// One row per line. 791 rows pretty-printed is an unreviewable 30k-line diff and
// one single line is an unreadable 70 KB; a row per line diffs as the rows that
// actually moved.
const body = rows.map((row) => `  ${JSON.stringify(row)}`).join(',\n');
writeFileSync(
  OUT,
  `${JSON.stringify(header, null, 1).slice(0, -2)},\n "rows": [\n${body}\n ]\n}\n`,
);

const missing = rows.filter((row) => row.d === null);
console.log(
  `@nubjs/extensions@${pkg.version} (${data.generated}) -> ${rows.length} packages, ` +
    `${rows.reduce((total, row) => total + row.t.length, 0)} edges, ` +
    `downloads ${start}..${end}`,
);
console.log(
  `  ${rows.filter((row) => row.y).length} carried from Yarn, ` +
    `${missing.length} without a download count` +
    (missing.length ? `: ${missing.map((row) => row.n).join(', ')}` : ''),
);

#!/usr/bin/env node
// Refresh the bundled packageExtensions database from the published
// `@nubjs/extensions` package.
//
// The database is a SNAPSHOT, deliberately. `nubjs/package-extensions` rebuilds
// daily and publishes a patch whenever the rules move, so tracking it live would
// make nub's resolution depend on a registry fetch and change what a given nub
// build installs from one day to the next. Vendoring it means a nub release
// pins one dataset, and refreshing it is a reviewable commit.
//
// Bundled extensions are the lowest-precedence layer and never feed the lockfile
// `packageExtensionsChecksum`, so a refresh does not drift anyone's lockfile —
// it only affects freshly-resolved packages.
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const ASSET = new URL('../crates/nub-cli/assets/bundled-package-extensions.json', import.meta.url);
const spec = process.argv[2] ?? '@nubjs/extensions@latest';

const dir = mkdtempSync(join(tmpdir(), 'nub-pkgext-'));
execFileSync('npm', ['pack', spec, '--silent'], { cwd: dir, stdio: ['ignore', 'ignore', 'inherit'] });
const tgz = execFileSync('sh', ['-c', 'ls *.tgz'], { cwd: dir, encoding: 'utf8' }).trim();
execFileSync('tar', ['xzf', tgz], { cwd: dir });

const pkg = JSON.parse(readFileSync(join(dir, 'package/package.json'), 'utf8'));
const data = JSON.parse(readFileSync(join(dir, 'package/package-extensions.json'), 'utf8'));

// Sorted, so a refresh diffs as the rules that actually changed. The harness
// emits insertion order, which reshuffles whenever the scan order does.
const rules = {};
for (const k of Object.keys(data.packageExtensions).sort()) rules[k] = data.packageExtensions[k];

const before = JSON.parse(readFileSync(ASSET, 'utf8')).packageExtensions;
writeFileSync(
  ASSET,
  JSON.stringify(
    {
      _source: '@nubjs/extensions',
      _version: pkg.version,
      _generated: data.generated,
      _refresh: 'node scripts/sync-package-extensions.mjs',
      packageExtensions: rules,
    },
    null,
    1,
  ) + '\n',
);

const added = Object.keys(rules).filter((k) => !(k in before));
const removed = Object.keys(before).filter((k) => !(k in rules));
const changed = Object.keys(rules).filter(
  (k) => k in before && JSON.stringify(before[k]) !== JSON.stringify(rules[k]),
);
console.log(`@nubjs/extensions@${pkg.version} (${data.generated}) -> ${Object.keys(rules).length} entries`);
console.log(`  ${added.length} added, ${removed.length} removed, ${changed.length} changed`);
for (const k of [...added.slice(0, 5), ...removed.slice(0, 5), ...changed.slice(0, 5)]) console.log(`    ${k}`);

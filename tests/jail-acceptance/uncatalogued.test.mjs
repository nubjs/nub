// The count this reports decides whether default-on ships, so its UNDER-count is the dangerous
// direction: a dep missed here is a package that looks measured and is not, and the per-install
// estimate comes out optimistic. Every case below is pinned in that polarity.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { installScriptDeps, specName, partition, loadCoverage } from './uncatalogued.mjs';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const tsv = (rows) => {
  const f = path.join(fs.mkdtempSync(path.join(os.tmpdir(), 'cov-')), 'coverage.tsv');
  fs.writeFileSync(f, '# name\tversion\tplatforms\treason\tmeasured-at\n' + rows.map((r) => r.join('\t')).join('\n') + '\n');
  return f;
};

const WARN = (specs) => `WARN ignored build scripts for ${specs.length} package(s): x. `
  + `Run \`nub approve-builds\`. code=WARN_NUB_IGNORED_BUILD_SCRIPTS count=${specs.length} `
  + `packages=${JSON.stringify(specs)}`;

test('the dep list comes from nub\'s own report', () => {
  assert.deepEqual(installScriptDeps(WARN(['esbuild@0.28.2', 'sharp@0.33.0'])),
    ['esbuild@0.28.2', 'sharp@0.33.0']);
});

test('⛔ a SCOPED name survives, and so does a version range containing a comma', () => {
  // The reason this is JSON.parse and not a comma split. A hand-rolled split halves the count on
  // exactly the packages most likely to be interesting, and an undercount reads as better coverage.
  const specs = ['@scope/pkg@1.0.0', 'weird@>=1.0.0,<2.0.0'];
  assert.deepEqual(installScriptDeps(WARN(specs)), specs);
});

test('⛔⛔ DEFAULT-TRUSTED packages count too, and reading only the ignored line reports ZERO', () => {
  // THE DEFECT THIS HARNESS SHIPPED WITH, caught on its first real run. nub does not merely ignore
  // build scripts pending approval — it also runs a DEFAULT-TRUST list without asking, announced on a
  // completely different line that carries no machine-readable `packages=` field (the log formatter
  // strips it deliberately). A project depending on esbuild and sharp therefore reported
  // `install-script deps 0` while both were being jailed. The line below is nub's REAL output, copied
  // from an actual install rather than composed from an assumption — which is what the first version of
  // this file got wrong.
  const real = '  linker  global-virtual-store\n\n'
    + 'WARN defaultTrust: running build scripts for esbuild@0.21.5\n'
    + 'dependencies:\n+ esbuild@0.21.5  latest 0.28.2\n';
  assert.deepEqual(installScriptDeps(real), ['esbuild@0.21.5']);
});

test('both populations are counted together, and de-duplicated', () => {
  const log = `${WARN(['sharp@0.33.0'])}\nWARN defaultTrust: running build scripts for esbuild@0.21.5, sharp@0.33.0`;
  assert.deepEqual(installScriptDeps(log).sort(), ['esbuild@0.21.5', 'sharp@0.33.0'],
    'a package named on both lines is one dependency, not two');
});

test('a log with no warning yields no deps — not an error', () => {
  // A project whose dependencies have no install scripts is the GOOD case, and reporting zero is the
  // correct answer for it. Throwing here would make a clean project look like a harness failure.
  assert.deepEqual(installScriptDeps('nub 0.7.1 · ✓ installed 42 packages in 900ms'), []);
});

test('two installs in one log union rather than overwrite', () => {
  // A run may install more than once (a workspace, a re-run). Taking only the last match would
  // undercount, and undercounting is the direction that flatters the result.
  const log = `${WARN(['a@1.0.0'])}\n…\n${WARN(['b@2.0.0'])}`;
  assert.deepEqual(installScriptDeps(log).sort(), ['a@1.0.0', 'b@2.0.0']);
});

test('a malformed packages= list is skipped rather than crashing the report', () => {
  const log = 'packages=[not json\n' + WARN(['ok@1.0.0']);
  assert.deepEqual(installScriptDeps(log), ['ok@1.0.0']);
});

test('a name is split from its version at the LAST @, so scopes survive', () => {
  assert.equal(specName('@scope/pkg@1.0.0'), '@scope/pkg');
  assert.equal(specName('esbuild@0.28.2'), 'esbuild');
  // A `file:` spec is what a local fixture produces; the name must still come out clean.
  assert.equal(specName('dep@file:./dep'), 'dep');
  assert.equal(specName('noversion'), 'noversion');
});

test('⛔ partition matches on NAME, because the catalog is keyed by name', () => {
  // A version-band entry lives INSIDE a package's entry, so a package present at any version is
  // catalogued for this count's purposes — the question here is "has anyone measured this package",
  // not "is this exact version measured". Matching on the full spec would report every package as
  // uncatalogued and make the number meaningless.
  const catalog = { packages: { esbuild: {}, '@scope/pkg': {} } };
  const r = partition(['esbuild@9.9.9', '@scope/pkg@1.0.0', 'unmeasured@1.0.0'], catalog);
  assert.deepEqual(r.catalogued.sort(), ['@scope/pkg@1.0.0', 'esbuild@9.9.9']);
  assert.deepEqual(r.uncatalogued, ['unmeasured@1.0.0']);
});

test('an empty or absent catalog makes everything uncatalogued, never everything catalogued', () => {
  // ⛔ THE FAIL-SAFE DIRECTION. A catalog that failed to load must not report a project as fully
  // covered — that is the false all-clear this whole number exists to prevent.
  assert.deepEqual(partition(['a@1.0.0'], {}).uncatalogued, ['a@1.0.0']);
  assert.deepEqual(partition(['a@1.0.0'], null).uncatalogued, ['a@1.0.0']);
});

test('⛔ a MEASURED package with no catalog entry is not risk — the whole point of the record', () => {
  // The inversion this file exists to encode. `node-pty` passes at the baseline on all three
  // platforms, so it wants no catalog entry; counting it as unmeasured made the only way to shrink
  // the number an under-grant. It is covered, and it is neither catalogued nor at risk.
  const cov = loadCoverage(tsv([['node-pty', '1.1.0', 'macos,linux,win', 'baseline-measured', 'abc1234']]));
  const r = partition(['node-pty@1.1.0', 'never-run@1.0.0'], { packages: {} }, cov);
  assert.deepEqual(r.measured, ['node-pty@1.1.0']);
  assert.deepEqual(r.uncatalogued, ['never-run@1.0.0'], 'a package in neither source is still risk');
  assert.deepEqual(r.catalogued, []);
});

test('a catalog entry outranks the coverage record, so the buckets never double-count', () => {
  const cov = loadCoverage(tsv([['sharp', '0.33.0', 'macos,linux,win', 'baseline-measured', 'abc1234']]));
  const r = partition(['sharp@0.33.0'], { packages: { sharp: {} } }, cov);
  assert.deepEqual(r.catalogued, ['sharp@0.33.0']);
  assert.deepEqual(r.measured, []);
});

test('⛔ a row without provenance is DROPPED — a bare name cannot launder itself into coverage', () => {
  // THE FAIL-SAFE THAT MATTERS. This record is the one place a package can be declared safe without
  // running anything, so a row that names no commit and no recognised reason must not count. If it
  // did, appending a line to a TSV would silently clear a package the sweep has never touched.
  const cov = loadCoverage(tsv([
    ['no-sha', '1.0.0', 'macos,linux,win', 'baseline-measured', ''],
    ['bad-reason', '1.0.0', 'macos,linux,win', 'looked-fine-to-me', 'abc1234'],
    ['good', '1.0.0', 'macos,linux,win', 'v1-curated', 'abc1234'],
  ]));
  assert.deepEqual([...cov], ['good']);
});

test('a missing coverage record reports everything as risk, never as covered', () => {
  // Same polarity as the absent-catalog case: an unreadable instrument must over-report risk.
  const cov = loadCoverage('/nonexistent/coverage.tsv');
  assert.equal(cov.size, 0);
  assert.deepEqual(partition(['a@1.0.0'], { packages: {} }, cov).uncatalogued, ['a@1.0.0']);
});

test('partition without a coverage argument behaves as it did before the record existed', () => {
  const r = partition(['a@1.0.0'], { packages: {} });
  assert.deepEqual(r.uncatalogued, ['a@1.0.0']);
  assert.deepEqual(r.measured, []);
});

test('⛔ KNOWN-ANSWER CONTROL: the SHIPPED record parses and carries both reasons', () => {
  // A parser change that silently rejected every row would pass every synthetic case above and
  // quietly restore the old behaviour, because an empty coverage set is indistinguishable from
  // "nothing measured yet". Run the real file through it.
  const cov = loadCoverage(path.join(HERE, 'results', 'baseline-coverage.tsv'));
  assert.ok(cov.size > 100, `shipped coverage record parsed to ${cov.size} rows — the parser is broken`);
  assert.ok(cov.has('node-pty'), 'node-pty is measured on all three platforms and must be covered');
  assert.ok(cov.has('@prisma/client'), '@prisma/client holds a v1 CuratedGrant and must be covered');
  assert.ok(cov.has('prisma'), 'prisma is measured on all three platforms since 852d073521');
  // ⛔ THE NEGATIVE WITNESS IS SYNTHETIC ON PURPOSE. It used to be `prisma`, chosen because nothing
  // had measured it — and then the sweep did, at 852d073521, which turned a control into a false
  // failure that nobody saw until the file was next run. A name no record can ever contain proves
  // the same thing (that `has` discriminates rather than answering true for everything) and cannot
  // be invalidated by measuring more packages, which is the whole point of the record.
  assert.ok(!cov.has('\u0000not-a-real-package'), 'an unmeasured package must not read as covered');
});

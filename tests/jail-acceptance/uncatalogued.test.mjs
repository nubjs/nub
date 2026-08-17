// The count this reports decides whether default-on ships, so its UNDER-count is the dangerous
// direction: a dep missed here is a package that looks measured and is not, and the per-install
// estimate comes out optimistic. Every case below is pinned in that polarity.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { installScriptDeps, specName, partition } from './uncatalogued.mjs';

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

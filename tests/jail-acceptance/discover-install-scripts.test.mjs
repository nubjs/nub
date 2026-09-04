// The population this builds is the denominator of every coverage claim the jail makes, so its
// UNDER-count is the dangerous direction: a candidate dropped here is a package the sweep never
// measures, reported as nothing rather than as a hole.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { highestStable } from './discover-install-scripts.mjs';

test('⛔ versions are ordered NUMERICALLY, so 10.0.0 beats 9.9.9', () => {
  // A lexical comparator answers `9.9.9` here and looks right on every single-digit fixture. It
  // would pick an older version whose install script may differ from the one users actually get.
  assert.equal(highestStable(['1.0.0', '10.0.0', '9.9.9']), '10.0.0');
  assert.equal(highestStable(['2.0.9', '2.1.0']), '2.1.0');
});

test('⛔⛔ a prerelease is never the answer — the defect this function exists to fix', () => {
  // Measured 2026-09-04: `prisma`'s `latest` dist-tag was `8.0.0-rc.12`, and that prerelease had
  // dropped the `preinstall` that `7.9.1` still carried. Judging by `latest` therefore removed a
  // package the jail confines for every real user, and three same-commit sweeps measured it nowhere.
  assert.equal(highestStable(['8.0.0-rc.12', '7.9.1', '7.10.0']), '7.10.0');
  assert.equal(highestStable(['2.1.0-rc.1', '2.1.0']), '2.1.0');
});

test('a package with no stable release yields null rather than a prerelease', () => {
  // Null is the honest answer: the caller then falls back to judging `latest`, which is the only
  // thing that exists. Returning the prerelease would hide that this package has no stable form.
  assert.equal(highestStable(['1.0.0-alpha.1', '1.0.0-alpha.2']), null);
  assert.equal(highestStable([]), null);
});

test('a malformed version key is skipped, not parsed as NaN and preferred', () => {
  assert.equal(highestStable(['not-a-version', '1.2.3']), '1.2.3');
  assert.equal(highestStable(['1.2', '1.2.3']), '1.2.3', 'a two-part key is not a semver version');
});

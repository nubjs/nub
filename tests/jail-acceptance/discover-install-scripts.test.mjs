// The population this builds is the denominator of every coverage claim the jail makes, so its
// UNDER-count is the dangerous direction: a candidate dropped here is a package the sweep never
// measures, reported as nothing rather than as a hole.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { highestStable, pickInstalledVersion } from './discover-install-scripts.mjs';

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

// Shorthand: a corgi packument's `versions` map, carrying only the field this rule reads.
const vs = (spec) => Object.fromEntries(Object.entries(spec).map(([v, has]) => [v, { hasInstallScript: has }]));

test('⛔⛔ the version people install wins, not `latest` — the defect this rule exists to fix', () => {
  // sharp, as measured 2026-09-05: no install script on its current release, one on 0.34.5, which
  // carries 30.7M weekly downloads. A latest-only read scored it a clean negative.
  const versions = vs({ '0.35.4': false, '0.34.5': true, '0.33.5': true });
  const downloads = { '0.35.4': 5_000_000, '0.34.5': 30_700_000, '0.33.5': 7_800_000 };
  assert.equal(pickInstalledVersion(versions, downloads), '0.34.5');
});

test('⛔⛔ absent download data never ranks — it would take the OLDEST versions and invent a carrier', () => {
  // With an empty map the comparator returns 0 for every pair, so the sort is a no-op and the top N
  // is packument order, i.e. the oldest releases. Measured on bare-fs: 1.5.3-1.5.5 all carry install
  // scripts while its real top three carry none. Returning null hands the caller the fallback path.
  const versions = vs({ '1.5.3': true, '1.5.4': true, '4.8.1': false, '4.7.1': false });
  assert.equal(pickInstalledVersion(versions, {}), null);
  assert.equal(pickInstalledVersion(versions, { '1.5.3': 0, '4.8.1': 0 }), null);
});

test('⛔ a version nobody installs cannot qualify a package, however it ranks', () => {
  // `environment` has few enough versions that top-3 reaches 0.0.1, which holds 14 weekly downloads
  // against 36M on its current release. Rank alone would score it a carrier.
  const versions = vs({ '1.1.0': false, '1.0.0': false, '0.0.1': true });
  assert.equal(pickInstalledVersion(versions, { '1.1.0': 36_000_000, '1.0.0': 1_000, '0.0.1': 14 }), null);
});

test('⛔⛔ RANK DOES NOT DECIDE INCLUSION — the download-share floor does, alone', () => {
  // The carrier sits at rank 6, holding 10% of the package's installs. A jail that breaks it breaks
  // a tenth of that package's users, so it belongs in the population however it ranks. Measured over
  // the band above the download gate, a top-3 window found 244 carriers, a top-5 window 362 and no
  // window 403 — a count that moves 48% between two defensible windows is a property of the window.
  const versions = vs({ a: false, b: false, c: false, d: false, e: false, f: true });
  const downloads = { a: 300, b: 200, c: 180, d: 120, e: 100, f: 100 };
  assert.equal(pickInstalledVersion(versions, downloads), 'f');
  assert.equal(pickInstalledVersion(versions, downloads, 5), null,
    'the window argument still works, because sensitivity sweeps depend on it');
});

test('the most-downloaded qualifying version wins, not the newest or the first found', () => {
  const versions = vs({ '3.0.0': true, '2.0.0': true, '1.0.0': true });
  assert.equal(pickInstalledVersion(versions, { '3.0.0': 100, '2.0.0': 800, '1.0.0': 100 }), '2.0.0');
});

test('⛔⛔ the share is SUMMED across carrying versions, not taken from the best one', () => {
  // @pulumi/azure-native, measured 2026-09-06: 719 of its 1,318 versions carry an install script, the
  // best single one holds 0.77% of downloads, and together they hold 1.82%. Judging by the best one
  // alone excluded a package where nearly one install in fifty runs a script.
  const spread = vs({ big: false, c1: true, c2: true, c3: true });
  assert.equal(pickInstalledVersion(spread, { big: 9880, c1: 60, c2: 40, c3: 20 }), 'c1',
    'best carrier holds 0.60%, under the floor; the three together hold 1.20%, over it');
  assert.equal(pickInstalledVersion(spread, { big: 9940, c1: 30, c2: 20, c3: 10 }), null,
    'and 0.60% summed is still under the floor');
});

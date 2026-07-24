import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { test } from 'node:test'
import { fileURLToPath } from 'node:url'

import { SOAK_DAYS, addDaysIso, todayIso } from './constants.mts'
import {
  checkCargoConfig,
  checkCatalogParity,
  checkExcludeAnnotations,
  checkNpmrc,
  checkRenovateConfig,
  checkTazeConfig,
  checkWorkspaceYaml,
  fixCargoConfig,
  fixNpmrc,
  fixRenovateConfig,
  fixWorkspaceYaml,
  main,
  parseExcludeEntries,
} from './soak.mts'

// A pin published yesterday is inside its window; one published long ago
// has expired. Built relative to today so the tests never go stale.
const FRESH_PUB = addDaysIso(todayIso(), -1)
const FRESH_REM = addDaysIso(FRESH_PUB, SOAK_DAYS)

const CLEAN_YAML = `catalog:
  taze: 19.14.1
minimumReleaseAge: 10080
minimumReleaseAgeExclude:
  # published: ${FRESH_PUB} | removable: ${FRESH_REM}
  - 'left-pad@1.3.0'
  - '@myorg/*'
  - react
`

test('cargo config: wrong window and missing unstable gate are findings', () => {
  const good = '[unstable]\nmin-publish-age = true\n\n[registry]\nglobal-min-publish-age = "7 days"\n'
  assert.equal(checkCargoConfig(good, 'c').length, 0)
  assert.equal(checkCargoConfig(good.replace('7 days', '3 days'), 'c').length, 1)
  assert.equal(checkCargoConfig('[registry]\nglobal-min-publish-age = "7 days"\n', 'c').length, 1)
})

test('npmrc: window must match SOAK_DAYS and fix writes it', () => {
  assert.equal(checkNpmrc('min-release-age=7\n', 'n').length, 0)
  assert.equal(checkNpmrc('min-release-age=3\n', 'n').length, 1)
  assert.equal(checkNpmrc('# nothing\n', 'n').length, 1)
  assert.match(fixNpmrc('# nothing\n'), /min-release-age=7/)
  assert.match(fixNpmrc('min-release-age=3\n'), /min-release-age=7/)
})

test('workspace yaml: clean fixture passes', () => {
  assert.deepEqual(checkWorkspaceYaml(CLEAN_YAML, 'y'), [])
})

test('workspace yaml: wrong minutes value is a finding', () => {
  const bad = CLEAN_YAML.replace('10080', '1440')
  assert.equal(checkWorkspaceYaml(bad, 'y').filter(f => f.what.includes('minimumReleaseAge')).length, 1)
})

test('excludes: flow-style list is rejected outright', () => {
  const flow = "minimumReleaseAge: 10080\nminimumReleaseAgeExclude: ['left-pad@1.3.0']\n"
  const findings = checkExcludeAnnotations(flow, 'y')
  assert.equal(findings.length, 1)
  assert.match(findings[0]!.what, /flow style/)
})

test('excludes: unannotated version pin is a finding, bare/glob are not', () => {
  const yaml = 'minimumReleaseAgeExclude:\n  - lodash@4.17.21\n  - react\n  - "@myorg/*"\n'
  const findings = checkExcludeAnnotations(yaml, 'y')
  assert.equal(findings.length, 1)
  assert.match(findings[0]!.what, /lodash@4\.17\.21/)
})

test('excludes: wrong removable date and expiry are findings', () => {
  const wrong = `minimumReleaseAgeExclude:\n  # published: ${FRESH_PUB} | removable: ${addDaysIso(FRESH_PUB, 3)}\n  - 'a@1.0.0'\n`
  assert.match(checkExcludeAnnotations(wrong, 'y')[0]!.what, /removable date/)
  const expired = `minimumReleaseAgeExclude:\n  # published: 2020-01-01 | removable: 2020-01-08\n  - 'b@1.0.0'\n`
  assert.match(checkExcludeAnnotations(expired, 'y')[0]!.what, /expired/)
})

test('excludes: impossible calendar dates are findings, not crashes', () => {
  const bad = `minimumReleaseAgeExclude:\n  # published: 2026-13-45 | removable: 2026-13-52\n  - 'c@1.0.0'\n`
  const findings = checkExcludeAnnotations(bad, 'y')
  assert.equal(findings.length, 1)
  assert.match(findings[0]!.what, /annotation dates/)
})

test('excludes: entries with trailing comments still parse', () => {
  const yaml = `minimumReleaseAgeExclude:\n  # published: ${FRESH_PUB} | removable: ${FRESH_REM}\n  - 'd@2.0.0'  # temp\n`
  assert.deepEqual(parseExcludeEntries(yaml).map(e => e.name), ['d@2.0.0'])
  assert.equal(checkExcludeAnnotations(yaml, 'y').length, 0)
})

test('fix prunes expired pins together with their annotations', () => {
  const yaml = `minimumReleaseAge: 10080\nminimumReleaseAgeExclude:\n  # published: 2020-01-01 | removable: 2020-01-08\n  - 'old@1.0.0'\n  # published: ${FRESH_PUB} | removable: ${FRESH_REM}\n  - 'fresh@1.0.0'\n`
  const fixed = fixWorkspaceYaml(yaml)
  assert.ok(!fixed.includes('old@1.0.0'))
  assert.ok(!fixed.includes('2020-01-01'))
  assert.ok(fixed.includes('fresh@1.0.0'))
})

test('catalog parity: exact pin must match, catalog: protocol no-ops', () => {
  const yaml = 'catalog:\n  taze: 19.14.1\n'
  const pin = (v: string) => JSON.stringify({ devDependencies: { taze: v } })
  assert.equal(checkCatalogParity(yaml, pin('19.14.1'), 'y').length, 0)
  assert.equal(checkCatalogParity(yaml, pin('19.14.2'), 'y').length, 1)
  assert.equal(checkCatalogParity(yaml, pin('catalog:'), 'y').length, 0)
})

test('catalog parity: entries after a blank line are still checked', () => {
  const yaml = 'catalog:\n  taze: 19.14.1\n\n  untracked: 1.6.4\n'
  const pkg = JSON.stringify({ devDependencies: { taze: '19.14.1', untracked: '1.0.0' } })
  assert.equal(checkCatalogParity(yaml, pkg, 'y').length, 1)
})

test('taze config: window must be imported, not hand-copied', () => {
  const good = "import { SOAK_DAYS } from './scripts/soak/constants.mts'\nexport default { maturityPeriod: SOAK_DAYS }\n"
  assert.equal(checkTazeConfig(good, 't').length, 0)
  assert.equal(checkTazeConfig('export default { maturityPeriod: 7 }\n', 't').length, 1)
  assert.equal(checkTazeConfig('export default {}\n', 't').length, 2)
})

test('parser: a column-0 line ends the exclude block', () => {
  const yaml = 'minimumReleaseAgeExclude:\n  - react\nonlyBuiltDependencies:\n  - esbuild\n'
  assert.deepEqual(parseExcludeEntries(yaml).map(e => e.name), ['react'])
})

test('parser: items at a different indent are not exclude entries', () => {
  const yaml = 'minimumReleaseAgeExclude:\n  - react\n    - not-an-entry\n  - vue\n'
  assert.deepEqual(parseExcludeEntries(yaml).map(e => e.name), ['react', 'vue'])
})

test('fix rewrites a drifted cargo window and leaves a clean one alone', () => {
  const fixed = fixCargoConfig('[registry]\nglobal-min-publish-age = "3 days"\n')
  assert.ok(fixed.includes(`"${SOAK_DAYS} days"`))
  assert.equal(fixCargoConfig(fixed), fixed)
})

test('renovate: window must be explicit in-repo; preset inheritance is drift', () => {
  const good = `{ "extends": ["some>preset"], "minimumReleaseAge": "${SOAK_DAYS} days" }`
  assert.equal(checkRenovateConfig(good, 'r').length, 0)
  // Missing key = inherited-at-best: the preset can change without a
  // commit here, so the gate demands the explicit value.
  assert.equal(checkRenovateConfig('{ "extends": ["some>preset"] }', 'r').length, 1)
  assert.equal(checkRenovateConfig('{ "minimumReleaseAge": "3 days" }', 'r').length, 1)
  assert.equal(checkRenovateConfig('{ "minimumReleaseAge": 7 }', 'r').length, 1)
  assert.equal(checkRenovateConfig('not json', 'r').length, 1)
})

test('renovate fix sets the window, preserves other keys, and is idempotent', () => {
  const fixed = fixRenovateConfig('{\n  "labels": ["dependencies"],\n  "minimumReleaseAge": "3 days"\n}\n')
  const parsed = JSON.parse(fixed)
  assert.equal(parsed.minimumReleaseAge, `${SOAK_DAYS} days`)
  assert.deepEqual(parsed.labels, ['dependencies'])
  assert.equal(fixRenovateConfig(fixed), fixed)
  assert.equal(JSON.parse(fixRenovateConfig('{}')).minimumReleaseAge, `${SOAK_DAYS} days`)
  // Unparseable input is left for a human — never rewritten blind.
  assert.equal(fixRenovateConfig('not json'), 'not json')
})

// Glue: the tracked surfaces of THIS repo must satisfy the gate — the same
// check CI runs, exercised in-process so main() itself stays covered.
test('main --check passes against the tracked repo surfaces', () => {
  assert.equal(main([]), 0)
  assert.equal(main(['--quiet']), 0)
})

// End to end through the entrypoint guard: the CLI must resolve as main
// (realpath + file URL) and exit 0 on a clean tree.
test('CLI: node soak.mts --check --quiet exits 0', () => {
  const script = fileURLToPath(new URL('./soak.mts', import.meta.url))
  const res = spawnSync(process.execPath, [script, '--check', '--quiet'], { encoding: 'utf8' })
  assert.equal(res.status, 0, res.stderr)
})

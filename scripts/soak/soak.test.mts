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
  checkNpmrcExcludes,
  checkRenovateConfig,
  checkTazeConfig,
  checkWorkspaceYaml,
  fixCargoConfig,
  fixNpmrc,
  fixRenovateConfig,
  fixWorkspaceYaml,
  main,
  parseExcludeEntries,
  staleExcludes,
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

test('npmrc excludes: version pins need dated annotations, globs do not', () => {
  // The shape a fleet repo actually uses: trusted scopes and bare names
  // are standing trust and need no annotation.
  const trusted = [
    'min-release-age=7',
    'min-release-age-exclude[]=@socketsecurity/*',
    'min-release-age-exclude[]=sfw',
  ].join('\n')
  assert.deepEqual(checkNpmrcExcludes(trusted, 'n'), [])

  // A VERSION-PINNED exclude is a dated bypass — unannotated is a finding.
  const unannotated = 'min-release-age-exclude[]=lodash@4.17.21\n'
  assert.match(checkNpmrcExcludes(unannotated, 'n')[0]!.what, /lodash@4\.17\.21/)

  // Correctly annotated passes; wrong arithmetic is a finding.
  const pub = addDaysIso(todayIso(), -1)
  const ok = `# published: ${pub} | removable: ${addDaysIso(pub, SOAK_DAYS)}\nmin-release-age-exclude[]=lodash@4.17.21\n`
  assert.deepEqual(checkNpmrcExcludes(ok, 'n'), [])
  const wrongMath = `# published: ${pub} | removable: ${addDaysIso(pub, 3)}\nmin-release-age-exclude[]=lodash@4.17.21\n`
  assert.match(checkNpmrcExcludes(wrongMath, 'n')[0]!.what, /removable date/)
  const badDates = `# published: 2026-13-45 | removable: 2026-13-52\nmin-release-age-exclude[]=lodash@4.17.21\n`
  assert.match(checkNpmrcExcludes(badDates, 'n')[0]!.what, /annotation dates/)
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

test('excludes: wrong removable date is a finding; expiry is a warning, not a finding', () => {
  const wrong = `minimumReleaseAgeExclude:\n  # published: ${FRESH_PUB} | removable: ${addDaysIso(FRESH_PUB, 3)}\n  - 'a@1.0.0'\n`
  assert.match(checkExcludeAnnotations(wrong, 'y')[0]!.what, /removable date/)
  // Expired-but-valid is STALE, not unsafe: check exits clean, the stale
  // list reports it, and --fix / the soak-autofix workflow prunes it.
  const expired = `minimumReleaseAgeExclude:\n  # published: 2020-01-01 | removable: 2020-01-08\n  - 'b@1.0.0'\n`
  assert.deepEqual(checkExcludeAnnotations(expired, 'y'), [])
  assert.deepEqual(staleExcludes(expired), ['b@1.0.0'])
  const malformed = `minimumReleaseAgeExclude:\n  # published: 2026-13-45 | removable: 2026-13-52\n  - 'c@1.0.0'\n`
  assert.deepEqual(staleExcludes(malformed), [])
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

test('fix and stale-list skip a wrong-arithmetic expired annotation', () => {
  // published + SOAK_DAYS != removable and removable is already past:
  // this must stay a check failure for a human, not silently prune —
  // the real window may still be open.
  const yaml = `minimumReleaseAge: 10080\nminimumReleaseAgeExclude:\n  # published: ${todayIso()} | removable: 2020-01-02\n  - 'wrongmath@1.0.0'\n`
  assert.deepEqual(staleExcludes(yaml), [])
  assert.ok(fixWorkspaceYaml(yaml).includes('wrongmath@1.0.0'))
  assert.ok(checkExcludeAnnotations(yaml, 'y').length >= 1)
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

test('parser: a trailing comment on the key line still opens the block', () => {
  // Without comment tolerance, every entry under a commented key line
  // silently escaped validation — a blind spot in the bypass gate.
  const yaml = 'minimumReleaseAgeExclude:  # temporary bypasses\n  - lodash@4.17.21\n'
  assert.deepEqual(parseExcludeEntries(yaml).map(e => e.name), ['lodash@4.17.21'])
  assert.equal(checkExcludeAnnotations(yaml, 'y').length, 1)
})

test('catalog parity: malformed package.json is a finding, not a crash', () => {
  const findings = checkCatalogParity('catalog:\n  taze: 19.14.1\n', 'not json', 'y')
  assert.equal(findings.length, 1)
  assert.match(findings[0]!.what, /parse/)
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
  const good = `{ "extends": ["some>preset"], "minimumReleaseAge": "${SOAK_DAYS} days", "internalChecksFilter": "strict" }`
  assert.equal(checkRenovateConfig(good, 'r').length, 0)
  // internalChecksFilter is load-bearing: renovate's default "flexible"
  // mode raises updates that have NOT cleared minimumReleaseAge.
  const noStrict = `{ "minimumReleaseAge": "${SOAK_DAYS} days" }`
  assert.match(checkRenovateConfig(noStrict, 'r')[0]!.what, /internalChecksFilter/)
  const flexible = `{ "minimumReleaseAge": "${SOAK_DAYS} days", "internalChecksFilter": "flexible" }`
  assert.match(checkRenovateConfig(flexible, 'r')[0]!.what, /internalChecksFilter/)
  // Missing key = inherited-at-best: the preset can change without a
  // commit here, so the gate demands the explicit value.
  // These fixtures each miss BOTH the window and the strict filter, so
  // both findings fire; assert on the window one specifically.
  for (const bad of [
    '{ "extends": ["some>preset"] }',
    '{ "minimumReleaseAge": "3 days" }',
    '{ "minimumReleaseAge": 7 }',
  ]) {
    const findings = checkRenovateConfig(bad, 'r')
    assert.ok(findings.some(f => /minimumReleaseAge window/.test(f.what)), bad)
  }
  // Unparseable input is a single parse finding, not a pile of key checks.
  assert.equal(checkRenovateConfig('not json', 'r').length, 1)
})

test('renovate fix touches ONLY the window line — no reformatting churn', () => {
  // A JSON round-trip would collapse these hand-written single-line
  // arrays and rewrite unrelated rules (e.g. the decmpfs musl hold).
  const original = [
    '{',
    '  "extends": ["local>preset"],',
    '  "packageRules": [',
    '    {',
    '      "matchPackageNames": ["decmpfs"],',
    '      "allowedVersions": "<=0.1.0"',
    '    }',
    '  ],',
    `  "minimumReleaseAge": "3 days"`,
    '}',
    '',
  ].join('\n')
  const fixed = fixRenovateConfig(original)
  assert.equal(JSON.parse(fixed).minimumReleaseAge, `${SOAK_DAYS} days`)
  // Every other line is byte-identical.
  const changed = original
    .split('\n')
    .map((line, i) => [line, fixed.split('\n')[i]])
    .filter(([a, b]) => a !== b)
  assert.equal(changed.length, 1)
  assert.match(changed[0]![1]!, /minimumReleaseAge/)
  // Rules survive verbatim, arrays stay inline.
  assert.ok(fixed.includes('"matchPackageNames": ["decmpfs"]'))
  assert.ok(fixed.includes('"allowedVersions": "<=0.1.0"'))
  assert.ok(fixed.includes('"extends": ["local>preset"]'))
})

test('renovate fix inserts the window into a minimal object without breaking JSON', () => {
  // Regression: the naive insert produced the invalid `{,\n ... }`.
  const fixed = fixRenovateConfig('{}')
  assert.equal(JSON.parse(fixed).minimumReleaseAge, `${SOAK_DAYS} days`)
  assert.equal(fixRenovateConfig(fixed), fixed)
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

// The vendored runtime dependency set is written out in four places, and nothing but this
// file makes them agree.
//
// `crates/nub-core/build.rs` REQUIRES a set under the `embed-runtime` feature; the release
// workflow, the Nix flake and the local npm build each STAGE one. A producer that stages too
// FEW is caught the moment it builds — that is what the gate in build.rs is for. The direction
// nothing catches is the gate falling BEHIND: add a sixth package to the release vendoring and
// forget build.rs, and every nub build still passes, because every nub build stages it. Only a
// third-party packager breaks, silently, at run time — which is the exact bug this gate was
// added to stop (a homebrew-core formula that built green and shipped a binary unable to
// resolve its transpile helpers).
//
// So the assertion that matters is `build.rs requires everything the producers stage`.
import { readFileSync } from 'node:fs'
import assert from 'node:assert/strict'
import test from 'node:test'

const read = (p) => readFileSync(new URL(`../${p}`, import.meta.url), 'utf8')

// The `[...]` array literal feeding the `missing` filter in build.rs's gate.
function gateRequires() {
  const src = read('crates/nub-core/build.rs')
  const from = src.indexOf('let missing: Vec<&str> = [')
  assert.ok(from > 0, 'could not find the vendored-package gate in crates/nub-core/build.rs')
  const body = src.slice(from, src.indexOf(']', from))
  return [...body.matchAll(/"([^"]+)"/g)].map((m) => m[1]).sort()
}

// Each producer, keyed by how it happens to spell the list.
const PRODUCERS = {
  '.github/workflows/release.yml': (s) => {
    const from = s.indexOf('EXPECTED_PACKAGES=(')
    assert.ok(from > 0, 'could not find EXPECTED_PACKAGES in release.yml')
    return [...s.slice(from, s.indexOf(')', from)).matchAll(/"([^"]+)"/g)].map((m) => m[1])
  },
  'flake.nix': (s) => {
    const from = s.indexOf('runtimeDeps = [')
    assert.ok(from > 0, 'could not find runtimeDeps in flake.nix')
    const body = s.slice(from, s.indexOf('\n          ];', from))
    return [...body.matchAll(/dest = "([^"]+)"/g)].map((m) => m[1])
  },
  'npm/build-local.sh': (s) =>
    [...s.matchAll(/node_modules\/((?:@[a-z0-9-]+\/)?[a-z0-9-]+)" "\$REPO_ROOT\/runtime/g)].map(
      (m) => m[1],
    ),
}

test('build.rs requires every package the release workflow vendors', () => {
  const required = gateRequires()
  assert.ok(required.length >= 5, `parsed only ${required.length} packages from build.rs`)
  const staged = PRODUCERS['.github/workflows/release.yml'](read('.github/workflows/release.yml')).sort()
  assert.deepEqual(
    required,
    staged,
    'the embed-runtime gate and release.yml disagree about the vendored runtime set. A package ' +
      'the release stages but the gate does not require is one a third-party packager can omit ' +
      'without the build failing — the silent-degradation bug the gate exists to prevent.',
  )
})

test('every producer stages at least what the gate requires', () => {
  const required = gateRequires()
  for (const [path, extract] of Object.entries(PRODUCERS)) {
    const staged = extract(read(path))
    assert.ok(staged.length > 0, `parsed no packages out of ${path} — the extractor broke`)
    const missing = required.filter((d) => !staged.includes(d))
    assert.deepEqual(missing, [], `${path} does not stage ${missing.join(', ')}, so its build will fail the gate`)
  }
})

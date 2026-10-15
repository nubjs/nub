import { describe, it, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { walkStorage } from './scan.mts'

describe('walkStorage', () => {
  let dir: string
  before(() => {
    dir = mkdtempSync(join(tmpdir(), 'scan-test-'))
    // A normal package with one version.
    mkdirSync(join(dir, 'is-positive'))
    writeFileSync(
      join(dir, 'is-positive', 'package.json'),
      JSON.stringify({ name: 'is-positive', versions: { '3.1.0': {} } }),
    )
    // A scoped package (Verdaccio stores it as @scope/pkg/).
    mkdirSync(join(dir, '@scope'), { recursive: true })
    mkdirSync(join(dir, '@scope', 'pkg'))
    writeFileSync(
      join(dir, '@scope', 'pkg', 'package.json'),
      JSON.stringify({ name: '@scope/pkg', versions: { '1.0.0': {}, '2.0.0': {} } }),
    )
    // A non-directory entry (a stray file) — skipped.
    writeFileSync(join(dir, 'stray.txt'), 'ignore me')
    // A directory with no package.json — skipped.
    mkdirSync(join(dir, 'empty'))
  })
  after(() => rmSync(dir, { recursive: true, force: true }))

  it('extracts name@version from each package packument', () => {
    const specs = walkStorage(dir).sort()
    assert.deepEqual(specs, [
      '@scope/pkg@1.0.0',
      '@scope/pkg@2.0.0',
      'is-positive@3.1.0',
    ])
  })
})

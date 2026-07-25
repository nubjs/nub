// Tests for the docker-context reduction: the `untracked`-managed
// .dockerignore section. Skips wholesale in a repo with no repo-context
// docker builds (DOCKERIGNORE is null there).
import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import path from 'node:path'
import { test } from 'node:test'

import { DOCKERIGNORE, REPO_ROOT } from './paths.mts'

const skip = DOCKERIGNORE ? false : 'repo has no repo-context docker builds'

const START = '### start auto generated using `untracked`'
const END = '### finished auto generated using `untracked`'

const body = DOCKERIGNORE ? readFileSync(path.join(REPO_ROOT, DOCKERIGNORE), 'utf8') : ''

test('managed .dockerignore carries the untracked markers with content between', { skip }, () => {
  const start = body.indexOf(START)
  const end = body.indexOf(END)
  assert.ok(start !== -1 && end !== -1, 'both markers present')
  assert.ok(end - start > 100, 'generated section is not empty')
})

test('hand-written custom rules survive above the managed section', { skip }, () => {
  const custom = body.slice(0, body.indexOf(START))
  assert.match(custom, /^target\//m, 'build-artifact rule preserved')
  assert.match(custom, /^node_modules\//m, 'node_modules rule preserved')
})

test('the package.json whitelist is honored as trailing negations', { skip }, () => {
  const pkg = JSON.parse(readFileSync(path.join(REPO_ROOT, 'package.json'), 'utf8'))
  const whitelist: string[] = pkg.untracked?.whitelist ?? []
  assert.ok(whitelist.length > 0, 'whitelist declares the load-bearing paths')
  for (const entry of whitelist) {
    // Docker applies patterns anchored to the context root, so only
    // root-level entries need an explicit negation to survive the `.*`
    // dotfile rule; nested ones are untouched by it but must exist.
    if (!entry.includes('/')) {
      assert.ok(body.includes(`!${entry}`), `negation for whitelisted '${entry}'`)
    }
    assert.ok(existsSync(path.join(REPO_ROOT, entry)), `whitelisted path '${entry}' exists`)
  }
})

test('the load-bearing nested .cargo config is not root-anchored away', { skip }, () => {
  // The in-container builders depend on crates/nub-native/.cargo/config.toml;
  // the generated `.*` rule is context-root-anchored so it must NOT appear in
  // a form that matches nested dotdirs.
  assert.ok(!body.includes('**/.*'), 'no recursive dotfile exclusion')
})

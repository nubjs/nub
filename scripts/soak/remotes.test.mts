/**
 * @file Unit tests for the remote-map law's pure core: URL identity
 *   normalization, `git remote -v` parsing, and drift classification —
 *   both arms of each. The git/process phases stay behind these functions
 *   so the suite runs offline on any clone.
 */

import assert from 'node:assert/strict'
import { test } from 'node:test'

import {
  EXPECTED_REMOTES,
  PR_HOME,
  PUSH_HOME,
  checkRemotes,
  normalizeGitHubRepo,
  parseRemotes,
} from './remotes.mts'

test('normalizeGitHubRepo accepts https, scp-ssh, ssh://, with/without .git', () => {
  assert.equal(normalizeGitHubRepo('https://github.com/nubjs/nub.git'), 'nubjs/nub')
  assert.equal(normalizeGitHubRepo('https://github.com/nubjs/nub'), 'nubjs/nub')
  assert.equal(normalizeGitHubRepo('git@github.com:nubjs/nub.git'), 'nubjs/nub')
  assert.equal(normalizeGitHubRepo('ssh://git@github.com/nubjs/nub'), 'nubjs/nub')
  assert.equal(normalizeGitHubRepo('https://github.com/nubjs/nub/'), 'nubjs/nub')
})

test('normalizeGitHubRepo rejects non-github and malformed URLs', () => {
  assert.equal(normalizeGitHubRepo('https://gitlab.com/nubjs/nub.git'), null)
  assert.equal(normalizeGitHubRepo('git@github.com:aube.git'), null)
  assert.equal(normalizeGitHubRepo('/local/path/aube'), null)
})

test('parseRemotes keeps one fetch URL per remote and ignores push lines', () => {
  const remotes = parseRemotes(
    [
      'origin\thttps://github.com/nubjs/nub.git (fetch)',
      'origin\thttps://github.com/nubjs/nub.git (push)',
      'fork\tgit@github.com:jdalton/nub.git (fetch)',
      'fork\tgit@github.com:jdalton/nub.git (push)',
      '',
    ].join('\n'),
  )
  assert.deepEqual(remotes, {
    origin: 'https://github.com/nubjs/nub.git',
    fork: 'git@github.com:jdalton/nub.git',
  })
})

test('checkRemotes passes when declared remotes match, any protocol or case', () => {
  assert.deepEqual(
    checkRemotes({
      origin: `https://github.com/${PR_HOME}.git`,
      fork: `git@github.com:${PUSH_HOME.toUpperCase()}.git`,
      extra: 'https://github.com/somebody/else.git',
    }),
    [],
  )
})

test('checkRemotes flags a missing fork and a mispointed origin', () => {
  const drift = checkRemotes({ origin: 'https://github.com/wrong/home.git' })
  assert.deepEqual(drift, [
    { name: 'origin', expected: PR_HOME, actual: 'wrong/home' },
    { name: 'fork', expected: PUSH_HOME, actual: null },
  ])
})

test('the declared map covers exactly the PR home and the push fork', () => {
  assert.deepEqual(EXPECTED_REMOTES, { origin: PR_HOME, fork: PUSH_HOME })
})

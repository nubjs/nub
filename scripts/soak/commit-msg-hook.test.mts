/**
 * @file Behavior tests for .githooks/commit-msg — the agent-co-author
 *   trailer strip + trailing-blank collapse. The hook ran half-broken for
 *   months because nothing exercised it: a C-style comment in an awk END
 *   block failed on BSD awk, the `&&` chain silently skipped the collapse
 *   step, and the error landed in ignored stderr while the visible half
 *   (trailer strip) kept working. Every test therefore asserts exit 0 AND
 *   empty stderr — a tool in the pipeline erroring is a failure even when
 *   the message still comes out right.
 */

import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { test } from 'node:test'
import { fileURLToPath } from 'node:url'

import { REPO_ROOT } from './paths.mts'

const HOOK = path.join(REPO_ROOT, '.githooks/commit-msg')
const TMP = mkdtempSync(path.join(os.tmpdir(), 'commit-msg-hook-'))
let seq = 0

function runHook(message: string): string {
  const msgFile = path.join(TMP, `msg-${(seq += 1)}`)
  writeFileSync(msgFile, message)
  const res = spawnSync('sh', [HOOK, msgFile], { encoding: 'utf8' })
  // The silent-failure arm: the pipeline must not error at all, not merely
  // still produce output (see the file header for the incident this pins).
  assert.equal(res.status, 0, res.stderr)
  assert.equal(res.stderr, '', 'hook wrote to stderr')
  return readFileSync(msgFile, 'utf8')
}

test('strips agent co-author trailers (Claude / anthropic.com / noreply)', () => {
  const out = runHook(
    'feat: subject\n\nCo-authored-by: Claude <noreply@anthropic.com>\n' +
      'Co-Authored-By: Claude Opus <bot@anthropic.com>\n',
  )
  assert.ok(!/co-authored-by/i.test(out), out)
  assert.ok(out.startsWith('feat: subject'))
})

test('preserves human co-authors unchanged', () => {
  const out = runHook(
    'feat: subject\n\nCo-authored-by: Jane Doe <jane@example.com>\n',
  )
  assert.match(out, /Co-authored-by: Jane Doe <jane@example\.com>/)
})

test('collapses runs of blank lines and drops trailing ones', () => {
  const out = runHook('feat: subject\n\n\n\nbody line\n\n\n')
  assert.equal(out, 'feat: subject\n\nbody line\n')
})

test('a message left with only its subject has no trailing blanks', () => {
  const out = runHook(
    'feat: subject\n\nCo-authored-by: Claude <noreply@anthropic.com>\n\n',
  )
  assert.equal(out, 'feat: subject\n')
})

test('hook file exists where core.hooksPath points', () => {
  // A moved/renamed hook silently deactivates (git ignores a missing
  // commit-msg); this pins the path the repo's .git/config relies on.
  assert.doesNotThrow(() => readFileSync(HOOK, 'utf8'))
  assert.equal(
    fileURLToPath(new URL(`file://${HOOK}`)).endsWith('.githooks/commit-msg'),
    true,
  )
})

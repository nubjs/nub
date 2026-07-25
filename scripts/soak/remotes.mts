#!/usr/bin/env node
/**
 * @file 1 remote map, 1 reference — where this repo's PRs live and where
 *   branches push, declared in code instead of rediscovered. The two homes
 *   differ here (PRs open on the upstream, branches push to a fork), and
 *   tooling that guesses wrong burns API round-trips proving 404s — worse
 *   when gh auth is unavailable and the only fallback is the anonymous API,
 *   which answers only on the PR's BASE repo (and needs the full 40-char
 *   SHA for check-runs).
 *
 *   Usage: node scripts/soak/remotes.mts [--check|--fix|--print]
 *   --check verifies the clone's git remotes match the declaration
 *   (skipped in CI — runner checkouts legitimately differ), --fix adds or
 *   repoints the `fork` remote (origin drift is report-only: repointing
 *   origin is a human decision), --print emits the map as JSON for
 *   tooling.
 */

import { spawnSync } from 'node:child_process'
import process from 'node:process'
import { realpathSync } from 'node:fs'
import { pathToFileURL } from 'node:url'

import { REPO_ROOT } from './paths.mts'

// PRs open on `origin` (the upstream); work branches push to `fork`.
// Protocol is irrelevant — only owner/repo is law — but --fix writes SSH
// because it keeps working when gh's stored https credentials get wiped.
export const PR_HOME = 'nubjs/nub'
export const PUSH_HOME = 'jdalton/nub'
export const EXPECTED_REMOTES: Record<string, string> = {
  origin: PR_HOME,
  fork: PUSH_HOME,
}

/**
 * Reduce a GitHub remote URL to its `owner/repo` identity: https, ssh
 * (scp-like and ssh://), with or without `.git`. Null for anything that is
 * not github.com — a non-GitHub URL on a declared remote is drift, not a
 * parse error.
 */
export function normalizeGitHubRepo(url: string): string | null {
  const m =
    /^(?:https:\/\/|ssh:\/\/git@|git@)github\.com[/:]([^/]+\/[^/]+?)(?:\.git)?\/?$/.exec(url.trim())
  return m ? m[1]! : null
}

/**
 * Parse `git remote -v` output into name -> fetch URL.
 */
export function parseRemotes(gitRemoteOutput: string): Record<string, string> {
  const remotes: Record<string, string> = {}
  for (const line of gitRemoteOutput.split('\n')) {
    const m = /^(\S+)\t(\S+)\s+\(fetch\)$/.exec(line)
    if (m) {
      remotes[m[1]!] = m[2]!
    }
  }
  return remotes
}

export interface RemoteDrift {
  name: string
  expected: string
  actual: string | null
}

/**
 * Every declared remote must exist and resolve to its declared owner/repo
 * (case-insensitively — GitHub is). Undeclared extra remotes are fine.
 */
export function checkRemotes(remotes: Record<string, string>): RemoteDrift[] {
  const drift: RemoteDrift[] = []
  for (const [name, expected] of Object.entries(EXPECTED_REMOTES)) {
    const url = remotes[name]
    const actual = url === undefined ? null : normalizeGitHubRepo(url)
    if (actual?.toLowerCase() !== expected.toLowerCase()) {
      drift.push({ name, expected, actual })
    }
  }
  return drift
}

function readClone(): Record<string, string> {
  const res = spawnSync('git', ['remote', '-v'], { cwd: REPO_ROOT, encoding: 'utf8' })
  if (res.status !== 0) {
    console.error(`[remotes] git remote -v failed: ${res.stderr?.trim() || res.error?.message}`)
    process.exit(1)
  }
  return parseRemotes(res.stdout)
}

function main(argv: string[] = process.argv.slice(2)): number {
  if (argv.includes('--print')) {
    console.log(JSON.stringify({ prHome: PR_HOME, pushHome: PUSH_HOME, pushRemote: 'fork' }))
    return 0
  }
  const fix = argv.includes('--fix')
  if (!fix && process.env.CI) {
    console.log('[remotes] CI checkout — remote-map law is a dev-machine guard, skipping')
    return 0
  }
  const drift = checkRemotes(readClone())
  if (!drift.length) {
    console.log(`[remotes] ok — PRs: ${PR_HOME} (origin), pushes: ${PUSH_HOME} (fork)`)
    return 0
  }
  let unfixed = 0
  for (const d of drift) {
    if (fix && d.name === 'fork') {
      const url = `git@github.com:${EXPECTED_REMOTES['fork']}.git`
      const args = d.actual === null && !readClone()['fork']
        ? ['remote', 'add', 'fork', url]
        : ['remote', 'set-url', 'fork', url]
      const res = spawnSync('git', args, { cwd: REPO_ROOT, stdio: 'inherit' })
      if (res.status === 0) {
        console.log(`[remotes] fixed: fork -> ${url}`)
        continue
      }
    }
    unfixed += 1
    console.error(
      `[remotes] drift: remote '${d.name}' should be github.com/${d.expected}, ` +
        `found ${d.actual ?? 'missing/non-github'}` +
        (d.name === 'fork' ? ' (run with --fix)' : ' (repoint manually — origin is never auto-rewritten)'),
    )
  }
  return unfixed ? 1 : 0
}

const isMain =
  process.argv[1] && pathToFileURL(realpathSync(process.argv[1])).href === import.meta.url
if (isMain) {
  process.exitCode = main()
}

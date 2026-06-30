#!/usr/bin/env node
// Classify a jest --json run of pnpm's suite-against-nub into:
//   PASS            — test passed
//   KNOWN-FAILING   — failed AND matches an allowlist entry (intended divergence
//                     or a tracked bug)
//   SURPRISE        — failed and matches NO allowlist entry (a real, unexpected
//                     divergence — fails the run)
//   STALE-ALLOW     — an allowlist entry that matched NO failure (the test now
//                     passes or was renamed — prune the entry)
//
// Exit 0 if there are zero SURPRISE failures. STALE-ALLOW entries are reported
// loudly but are NON-FATAL: a known failure that starts passing is an
// improvement, not a regression, so it must not turn the gate red (maintainer
// call 2026-06-30 — green on known reality, red ONLY on a genuine NEW failure).
import fs from 'node:fs'

const args = process.argv.slice(2)
const fullRun = args.includes('--full')
const [resultsPath, allowlistPath] = args.filter((a) => a !== '--full')
if (!resultsPath || !allowlistPath) {
  console.error('usage: classify.mjs [--full] <jest-results.json> <allowlist.txt>')
  console.error('  --full  also flag stale allowlist entries (only valid on a whole-suite run)')
  process.exit(2)
}

const results = JSON.parse(fs.readFileSync(resultsPath, 'utf8'))
const allow = fs
  .readFileSync(allowlistPath, 'utf8')
  .split('\n')
  .map((l) => l.trim())
  .filter((l) => l && !l.startsWith('#'))

const matchedAllow = new Set()

// An allowlist entry is EITHER a test-file path (matched as a substring of the
// failing suite's path — skips a wholly-intended file) OR an exact test fullName
// (matched by equality). Distinguishing them by shape is what stops a short name
// like "update" from substring-matching the path "pnpm/test/update.ts" and
// swallowing its siblings.
const isFilePath = (a) => /(^|\/)test\/.*\.(ts|tsx|js|mjs|cjs)$/.test(a)
const matches = (a, fullName, file) =>
  isFilePath(a) ? file.includes(a) : fullName === a

let passed = 0
const known = []
const surprises = []

for (const suite of results.testResults ?? []) {
  const file = suite.testFilePath?.replace(/.*\/pnpm\//, 'pnpm/') ?? suite.name ?? '?'
  // A suite that fails to even load/compile reports no assertionResults but a
  // failureMessage. Treat that as a surprise unless the file path is allowlisted.
  if ((suite.assertionResults ?? []).length === 0 && suite.status === 'failed') {
    const hit = allow.find((a) => isFilePath(a) && file.includes(a))
    if (hit) {
      matchedAllow.add(hit)
      known.push({ name: `${file} (suite load failure)`, hit })
    } else {
      surprises.push({ name: `${file} (suite load failure)`, msg: oneLine(suite.failureMessage) })
    }
    continue
  }
  for (const t of suite.assertionResults ?? []) {
    const fullName = t.fullName || `${(t.ancestorTitles || []).join(' > ')} > ${t.title}`
    if (t.status === 'passed') {
      passed++
    } else if (t.status === 'failed') {
      const hit = allow.find((a) => matches(a, fullName, file))
      if (hit) {
        matchedAllow.add(hit)
        known.push({ name: fullName, hit })
      } else {
        surprises.push({ name: fullName, msg: oneLine((t.failureMessages || []).join('\n')) })
      }
    }
    // 'pending'/'skipped'/'todo' ignored.
  }
}

// Stale-allowlist detection only makes sense on a WHOLE-suite run — a subset run
// (one test file, a -t filter) legitimately exercises none of most entries.
const staleAllow = fullRun ? allow.filter((a) => !matchedAllow.has(a)) : []

function oneLine(s) {
  if (!s) return ''
  return s.replace(/\s+/g, ' ').slice(0, 200)
}

// ── Report ───────────────────────────────────────────────────────────────────
console.log('')
console.log('================ pnpm-conformance: nub vs pnpm ================')
console.log(`  PASS:          ${passed}`)
console.log(`  KNOWN-FAILING: ${known.length}  (allowlisted divergences/bugs)`)
console.log(`  SURPRISE:      ${surprises.length}  (unexpected divergences)`)
console.log(`  STALE-ALLOW:   ${staleAllow.length}  (allowlist entries that matched nothing)`)
console.log('===============================================================')

if (known.length) {
  console.log('\n-- KNOWN-FAILING (expected) --')
  for (const k of known) console.log(`  [${k.hit}]  ${k.name}`)
}
if (surprises.length) {
  console.log('\n-- SURPRISE FAILURES (these fail the run) --')
  for (const s of surprises) {
    console.log(`  ✗ ${s.name}`)
    if (s.msg) console.log(`      ${s.msg}`)
  }
}
if (staleAllow.length) {
  console.log('\n-- STALE ALLOWLIST ENTRIES (non-fatal — matched no failure, prune them) --')
  for (const a of staleAllow) console.log(`  ? "${a}"`)
}

console.log('')
// Only a SURPRISE (a genuinely-new failure not on the allowlist) fails the run.
// Stale entries are surfaced above but never red — pruning them is housekeeping,
// not a regression gate.
if (surprises.length === 0) {
  const staleNote = staleAllow.length ? ` (${staleAllow.length} stale entr${staleAllow.length === 1 ? 'y' : 'ies'} to prune)` : ''
  console.log(`RESULT: green-or-known-failing ✓${staleNote}`)
  process.exit(0)
}
console.log(`RESULT: ${surprises.length} surprise failure(s) — a new regression. Investigate.`)
process.exit(1)

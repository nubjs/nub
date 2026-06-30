// Generate a pnpm-conformance allowlist from a jest results JSON.
//
// Each currently-failing test becomes an allowlist entry, bucketed by category
// (suite-file + error-signature heuristics) with header comments. The classifier
// matches each entry as a SUBSTRING against a failing test's fullName (or, for a
// suite-load failure, its file path). Two entry granularities:
//
//   - WHOLE-FILE INTENDED files (server, self-update, supportedArchitectures,
//     version-switching, global, patch, configurational-deps) are entirely
//     out-of-scope for nub, so they collapse to ONE version-stable file-path
//     entry — robust across pnpm versions (test names churn, paths don't).
//   - Every other failure is an EXACT-fullName entry — precise, so a genuinely
//     new regression in a mixed file still surfaces as a SURPRISE.
//
// Genuine nub<->pnpm divergences are grouped + thread-referenced, never buried.
//
//   node gen-allowlist.mjs <results.json> > allowlist.txt
import fs from 'node:fs'
const results = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'))

// Files that are wholly one intended-divergence concern — collapsed to a single
// file-path entry (skips the whole file, robust to per-test churn across pnpm
// versions). Only files where EVERY test is out-of-scope for nub belong here.
const WHOLE_FILE_INTENDED = new Set([
  'test/server.ts',
  'test/install/supportedArchitectures.ts',
  'test/install/selfUpdate.ts',
  'test/install/global.ts',
  'test/switchingVersions.test.ts',
  'test/packageManagerCheck.test.ts',
  'test/configurationalDependencies.test.ts',
  'test/patch/ignorePatchFailures.ts',
])

const CATS = [
  ['version-switching', 'INTENDED / out-of-scope — packageManager version-switching + self-update (nub does not manage pnpm versions). Threads: pnpm-conformance-99, pnpm-conformance-divergences (D8).'],
  ['node-provisioning', 'INTENDED / out-of-scope — per-package / --use-node-version Node provisioning (nub uses the host Node).'],
  ['server', 'INTENDED / out-of-scope — `pnpm server` store-server mode (unimplemented; tests also time out).'],
  ['supported-arch', 'INTENDED / out-of-scope — supportedArchitectures install matrix.'],
  ['global-layout', 'INTENDED / out-of-scope — global install/link/bin layout (nub uses its own global layout).'],
  ['patch-config-deps', 'INTENDED / out-of-scope — patch + configurational-dependencies hooks.'],
  ['infra-registry-404', 'HARNESS INFRA — registry-mock not honored on this flow -> real-registry 404. Harness bug, not a nub regression.'],
  ['infra-trim-crash', 'HARNESS INFRA — execPnpm.ts helper crashes on empty nub output (undefined.trim).'],
  ['infra-timeout', 'HARNESS INFRA — jest timeout (unimplemented feature hangs).'],
  ['genuine-divergence', 'GENUINE nub<->pnpm divergence worth fixing — NOT buried; tracked in pnpm-conformance-divergences (D1-D7) + pnpm-conformance-99. Remove the entry (or regenerate) when the divergence is fixed.'],
]

function catFor(file, name, msg) {
  const f = file.toLowerCase(), n = name.toLowerCase(), m = (msg || '').toLowerCase()
  if (m.includes('exceeded timeout')) return 'infra-timeout'
  if (m.includes('registry.npmjs.org') && m.includes('err_pnpm_fetch_404')) return 'infra-registry-404'
  if (m.includes("reading 'trim'") || m.includes('execpnpm.ts:11')) return 'infra-trim-crash'
  if (f.includes('supportedarchitectures')) return 'supported-arch'
  if (f.includes('server.ts')) return 'server'
  if (f.includes('selfupdate') || f.includes('switchingversions') || f.includes('packagemanagercheck') || n.includes('switch to') || n.includes('self-update') || n.includes('packagemanager')) return 'version-switching'
  if (f.includes('/patch/') || f.includes('configurationaldependencies') || n.includes('patch') || n.includes('configurational')) return 'patch-config-deps'
  if (f.includes('global') || n.includes('global') || n.includes('link ') || n.includes('-g')) return 'global-layout'
  if (n.includes('node version') || n.includes('node-version') || n.includes('executionenv') || n.includes('use-node-version') || n.includes('specified node')) return 'node-provisioning'
  return 'genuine-divergence'
}

const byCat = new Map(CATS.map(([k]) => [k, new Set()]))
let nFail = 0
for (const su of results.testResults ?? []) {
  const rel = (su.testFilePath ?? su.name ?? '?').replace(/.*\/pnpm\//, '').replace(/^test\//, 'test/')
  const file = rel.startsWith('test/') ? rel : `test/${rel.replace(/.*\/test\//, '')}`
  const ar = su.assertionResults ?? []
  if (ar.length === 0 && su.status === 'failed') {
    // A suite that fails to even load reports no assertions — allowlist it by its
    // file path (classifier matches suite-load failures with file.includes()).
    nFail++
    byCat.get(catFor(file, file, su.failureMessage)).add(file)
    continue
  }
  for (const t of ar) {
    if (t.status !== 'failed') continue
    nFail++
    const full = t.fullName || `${(t.ancestorTitles || []).join(' > ')} > ${t.title}`
    const cat = catFor(file, full, (t.failureMessages || []).join(' '))
    // Wholly-intended files collapse to one version-stable file-path entry.
    byCat.get(cat).add(WHOLE_FILE_INTENDED.has(file) ? file : full)
  }
}

const out = []
out.push("# Allowlist of KNOWN failures in pnpm's own black-box suite run against nub.")
out.push('#')
out.push('# GENERATED by gen-allowlist.mjs from a full-suite results JSON. Each non-')
out.push("# comment line is either a test-file path (e.g. test/server.ts — matches any")
out.push('# failure in that wholly-intended file) or an EXACT test fullName (matches that')
out.push('# one test). A failure matching ANY line is KNOWN (skipped); a failure matching')
out.push('# NO line is a SURPRISE that FAILS the run (a genuine new regression). A stale')
out.push('# entry (matches nothing) is reported but NON-FATAL — a known failure that')
out.push('# starts passing is an improvement, not a regression.')
out.push('#')
out.push('# Maintainer call (2026-06-30): skip all known/intentional divergences so the')
out.push('# nightly is green on known reality and red ONLY on a genuine NEW regression.')
out.push('# Genuine bugs are grouped + thread-referenced below, never silently buried —')
out.push('# fix the bug, then drop its line (or regenerate after the next run).')
out.push('#')
out.push('# Regenerate after a deliberate suite/pin change:')
out.push('#   node tests/pnpm-conformance/gen-allowlist.mjs <results.json> > tests/pnpm-conformance/allowlist.txt')
// A wholly-intended file's tests can span two categories (e.g. server.ts has
// both `server`- and timeout-flavored failures), so the same file-path entry
// would emit under each. Emit each path once, in its first category.
const emitted = new Set()
for (const [k, desc] of CATS) {
  const entries = [...byCat.get(k)].sort().filter((e) => !emitted.has(e))
  for (const e of entries) emitted.add(e)
  if (!entries.length) continue
  out.push('')
  out.push(`# ── ${k} (${entries.length}) ${'─'.repeat(Math.max(0, 58 - k.length))}`)
  out.push(`# ${desc}`)
  for (const e of entries) out.push(e)
}
console.error('total failures:', nFail)
for (const [k] of CATS) console.error(`  ${k}: ${byCat.get(k).size}`)
console.log(out.join('\n'))

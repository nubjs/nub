#!/usr/bin/env node
// check-safety — check packages against the Socket.dev malware API before
// seeding them into the nub test registry.
//
// Uses @socketsecurity/sdk's SocketSdk.checkMalware() to query the Socket
// malware detection API. If any package has malware alerts, exits non-zero
// with the alert details.
//
// Usage:
//   node tests/registry/check-safety.mts <pkg>@<version> [<pkg>@<version>...]
//   node tests/registry/check-safety.mts --from <file>   # newline-delimited list
//
// Requires SOCKET_API_KEY in the environment (or a configured SDK key).
// Runs under plain Node (type-stripping) — erasable TS only.

import { SocketSdk } from '@socketsecurity/sdk'
import { readFileSync, realpathSync } from 'node:fs'
import { argv, exit, stderr, env } from 'node:process'

interface PkgSpec {
  name: string
  version: string
}

function parseArgs(args: string[]): PkgSpec[] {
  const rest = args.filter(a => a !== '--json')
  if (rest[0] === '--from') {
    const names = readFileSync(rest[1]!, 'utf8')
      .split('\n')
      .map(s => s.trim())
      .filter(Boolean)
    return names.map(parseSpec)
  }
  return rest.map(parseSpec)
}

function parseSpec(spec: string): PkgSpec {
  // Handle @scope/name@version + bare name@version.
  const at = spec.startsWith('@')
    ? spec.indexOf('@', 1)
    : spec.indexOf('@')
  if (at === -1) {
    stderr.write(`check-safety: invalid spec "${spec}" — expected name@version\n`)
    exit(1)
  }
  return {
    name: spec.slice(0, at),
    version: spec.slice(at + 1),
  }
}

export type Spec = { name: string; version: string }
export type MalwarePkg = {
  namespace?: string | null
  name?: string | null
  version?: string | null
  alerts?: unknown[]
}
export type MatchVerdict = 'ok' | 'malware' | 'unverified'
export type MatchResult = { spec: Spec; verdict: MatchVerdict; alerts: any[] }

/**
 * Known CVE alerts in test-fixture packages that are acceptable to seed into
 * the OFFLINE test registry. These are published vulnerability disclosures —
 * Socket's four CVE alert types are `criticalCVE`, `cve` (high), `mediumCVE`,
 * `mildCVE` (see https://docs.socket.dev/docs/alert-types) — NOT malware
 * indicators. The packages are well-known libraries (lodash, etc.) with old
 * versions deliberately pinned by conformance fixtures to exercise
 * multi-version dedup. The registry serves only local integration tests; no
 * production code ever resolves from it.
 *
 * Each entry maps `name@version` → the alert *types* that are allowed. An
 * alert whose type is NOT in the allowlist still blocks seeding — this is a
 * narrow exception for documented CVEs, not a blanket CVE bypass.
 */
const KNOWN_CVE_ALLOWLIST: Record<string, Set<string>> = {
  // lodash@3.10.1 — prototype pollution CVE. Pinned by the workspace-dedup
  // fixture (lodash@^3.10.1 vs lodash@^4.17.0) to test two-major-version
  // dedup. Allowlisted across all four severities: a live checkMalware probe
  // returned no alerts for this version at all, but the fixture is meant to
  // seed regardless of which CVE severity Socket reports for it later.
  'lodash@3.10.1': new Set(['criticalCVE', 'cve', 'mediumCVE', 'mildCVE']),
}

/**
 * Partition a package's alerts into allowed (known CVE) and blocking. Returns
 * the blocking alerts — an empty array means the package is clear to seed.
 */
function blockingAlerts(spec: string, alerts: any[]): any[] {
  const allowed = KNOWN_CVE_ALLOWLIST[spec]
  if (!allowed) return alerts
  return alerts.filter((a) => !allowed.has(a.type))
}

/**
 * Match Socket malware results back to the input specs by identity
 * (name@version), not by index. The SDK's #checkMalwareFirewall compacts
 * results — it drops failed lookups rather than inserting placeholders — so
 * `data[i]` does not correspond to `specs[i]` after a drop. Keying on
 * (namespace, name, version) lands each result against the right spec, and a
 * miss is genuinely "not checked" instead of inheriting a neighbour's result.
 */
export function matchMalwareResults(specs: Spec[], data: MalwarePkg[]): MatchResult[] {
  const checked = new Map<string, MalwarePkg>()
  for (const pkg of data) {
    const name = pkg.namespace ? `${pkg.namespace}/${pkg.name ?? ''}` : (pkg.name ?? '')
    const version = pkg.version ?? ''
    if (name && version) checked.set(`${name}@${version}`, pkg)
  }
  return specs.map((spec) => {
    const pkg = checked.get(`${spec.name}@${spec.version}`)
    if (!pkg) return { spec, verdict: 'unverified' as const, alerts: [] }
    const alerts = pkg.alerts ?? []
    return {
      spec,
      verdict: alerts.length > 0 ? ('malware' as const) : ('ok' as const),
      alerts,
    }
  })
}

async function main(): Promise<void> {
  const apiKey = env['SOCKET_API_KEY']
  if (!apiKey) {
    stderr.write('check-safety: SOCKET_API_KEY not set — get one at https://socket.dev/settings/org-settings\n')
    exit(1)
  }

  const specs = parseArgs(argv.slice(2))
  if (specs.length === 0) {
    stderr.write('Usage: check-safety.mts <pkg>@<version>... | --from <file>\n')
    exit(1)
  }

  stderr.write(`check-safety: checking ${specs.length} package(s) against Socket malware API...\n`)

  const sdk = new SocketSdk(apiKey)
  // SocketSdk#checkMalware takes Array<{ purl: string }> — package URLs, not
  // { name, version, ecosystem }. Passing the wrong shape makes the SDK request
  // `/<encodeURIComponent(undefined)>` for every entry, drop them all as failed
  // lookups, and return data: [] — so the gate never fires. Build real purls.
  const components = specs.map(s => ({
    purl: `pkg:npm/${s.name}@${s.version}`,
  }))

  const result = await sdk.checkMalware(components)

  if (!result.success) {
    stderr.write(`check-safety: API call failed: ${JSON.stringify(result.error)}\n`)
    exit(1)
  }

  // The SDK silently DROPS any component whose lookup did not return an OK
  // response (malformed purl, unknown package, transient API error): it is
  // absent from `data`, not present with empty alerts. So "fewer results than
  // specs" is NOT "clean" — it is "not checked". Fail closed on any spec we
  // could not verify, otherwise a brand-new malicious version with no Socket
  // record sails through. Match by identity (name@version), not index — the
  // SDK compacts results (drops failed lookups), so data[i] != specs[i] after
  // a drop.
  const results = matchMalwareResults(specs, result.data)

  let hasAlerts = false
  for (const { spec, verdict, alerts } of results) {
    if (verdict === 'unverified') {
      hasAlerts = true
      stderr.write(
        `  UNVERIFIED: ${spec.name}@${spec.version} — no Socket result (malformed purl, unknown package, or API error); refusing to seed\n`,
      )
      continue
    }
    if (verdict === 'malware') {
      const specStr = `${spec.name}@${spec.version}`
      const blocking = blockingAlerts(specStr, alerts)
      if (blocking.length > 0) {
        hasAlerts = true
        stderr.write(`  MALWARE: ${specStr} has ${blocking.length} blocking alert(s):\n`)
        for (const alert of blocking) {
          const sev = alert.severity ? ` (${alert.severity})` : ''
          stderr.write(`    [${alert.type}] ${alert.key}${sev}\n`)
        }
      } else {
        // All alerts are known CVEs in the allowlist — report but don't block.
        const types = [...new Set(alerts.map((a: any) => a.type))].join(', ')
        stderr.write(`  OK (CVE allowlist): ${specStr} — ${alerts.length} known CVE alert(s) [${types}] allowed for offline test registry\n`)
      }
    } else {
      stderr.write(`  OK: ${spec.name}@${spec.version} — clean\n`)
    }
  }

  if (hasAlerts) {
    stderr.write('check-safety: one or more packages have malware alerts or could not be verified — refusing to seed\n')
    exit(1)
  }

  stderr.write('check-safety: all packages clean\n')
}

// Run only when executed directly, not when imported by the test. Both sides
// are realpath-resolved: `import.meta.filename` already is, and
// `process.argv[1]` is not (Node leaves it exactly as typed) — a symlinked
// checkout directory would otherwise make this compare unequal and main()
// would silently never run, exiting 0 having checked nothing.
if (process.argv[1] !== undefined && realpathSync(process.argv[1]) === import.meta.filename) {
  main().catch((err) => {
    stderr.write(`check-safety: error: ${err}\n`)
    exit(1)
  })
}

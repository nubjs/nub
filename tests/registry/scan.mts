#!/usr/bin/env node
// scan — re-scan all packages in the nub test registry's storage against
// the Socket.dev malware API. Run whenever tests/registry/storage/ changes
// (CI check or pre-commit hook) to catch a package that was safe at seed
// time but later flagged.
//
// Usage:
//   node tests/registry/scan.mts
//
// Requires SOCKET_API_KEY in the environment.

import { readdirSync, readFileSync, statSync, existsSync, realpathSync } from 'node:fs'
import { join } from 'node:path'
import { exit, stderr, env } from 'node:process'

/**
 * Walk a Verdaccio storage directory + extract `name@version` entries from
 * each package's packument. Pure-ish (reads the filesystem, no network) —
 * extracted so the storage-walk logic is testable against a fixture dir.
 */
export function walkStorage(storageDir: string): string[] {
  const specs: string[] = []
  const readPackument = (pkgDir: string, fallbackName: string) => {
    const packumentPath = join(pkgDir, 'package.json')
    if (!existsSync(packumentPath)) return
    const packument = JSON.parse(readFileSync(packumentPath, 'utf8'))
    const name = packument.name ?? fallbackName
    for (const version of Object.keys(packument.versions ?? {})) {
      specs.push(`${name}@${version}`)
    }
  }
  for (const entry of readdirSync(storageDir)) {
    const pkgDir = join(storageDir, entry)
    if (!statSync(pkgDir).isDirectory()) continue
    if (entry.startsWith('@')) {
      // Scoped packages are nested: storage/@scope/name/package.json.
      for (const sub of readdirSync(pkgDir)) {
        const subDir = join(pkgDir, sub)
        if (statSync(subDir).isDirectory()) readPackument(subDir, `${entry}/${sub}`)
      }
    } else {
      readPackument(pkgDir, entry)
    }
  }
  return specs
}

async function main(): Promise<void> {
  const apiKey = env['SOCKET_API_KEY']
  if (!apiKey) {
    stderr.write('scan: SOCKET_API_KEY not set\n')
    exit(1)
  }

  const storageDir = join(import.meta.dirname, 'storage')
  if (!existsSync(storageDir)) {
    stderr.write('scan: no storage directory — nothing to scan\n')
    return
  }

  const specs = walkStorage(storageDir)

  if (specs.length === 0) {
    stderr.write('scan: no packages found in storage\n')
    return
  }

  stderr.write(`scan: found ${specs.length} package(s) in storage — checking against Socket malware API...\n`)

  // Delegate to check-safety.mts with the found specs.
  const { spawnSync } = await import('node:child_process')
  const result = spawnSync('node', [
    join(import.meta.dirname, 'check-safety.mts'),
    ...specs,
  ], {
    stdio: 'inherit',
    env,
  })

  exit(result.status ?? 1)
}

// Run only when executed directly, not when imported by the test. Both sides
// are realpath-resolved: `import.meta.filename` already is, and
// `process.argv[1]` is not (Node leaves it exactly as typed) — a symlinked
// checkout directory would otherwise make this compare unequal and main()
// would silently never run, exiting 0 having checked nothing.
if (process.argv[1] !== undefined && realpathSync(process.argv[1]) === import.meta.filename) {
  main().catch((err) => {
    stderr.write(`scan: error: ${err}\n`)
    exit(1)
  })
}

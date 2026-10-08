#!/usr/bin/env node
// seed — download packages from npm, check them against the Socket malware
// API, and seed the safe ones into the nub test registry's Verdaccio storage.
//
// Usage:
//   node tests/registry/seed.mts <pkg>@<version> [<pkg>@<version>...]
//
// Requires SOCKET_API_KEY in the environment (for the safety check).
// Downloads from registry.npmjs.org (network needed for seeding only —
// the seeded storage is committed + used offline by tests).

import { spawnSync } from 'node:child_process'
import {
  mkdtempSync,
  writeFileSync,
  rmSync,
  mkdirSync,
  existsSync,
  readFileSync,
  realpathSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join, dirname } from 'node:path'
import { argv, exit, stderr, env } from 'node:process'

const STORAGE_DIR = join(import.meta.dirname, 'storage')
const REGISTRY = 'https://registry.npmjs.org'

function parseSpec(spec: string): { name: string; version: string } {
  const at = spec.startsWith('@') ? spec.indexOf('@', 1) : spec.indexOf('@')
  if (at === -1) {
    stderr.write(`seed: invalid spec "${spec}"\n`)
    exit(1)
  }
  return { name: spec.slice(0, at), version: spec.slice(at + 1) }
}

/**
 * Compare two semver versions (no prerelease support — the seeded packages are
 * all plain releases). Returns >0 if a > b, <0 if a < b, 0 if equal.
 */
function compareVersion(a: string, b: string): number {
  const pa = a.split('.').map(Number)
  const pb = b.split('.').map(Number)
  for (let i = 0; i < 3; i++) {
    const d = (pa[i] ?? 0) - (pb[i] ?? 0)
    if (d !== 0) return d
  }
  return 0
}

/**
 * Build the `dist-tags` for a seeded packument. Preserves the original npm
 * dist-tags but only keeps tags whose version is actually seeded. If the
 * `latest` tag is missing (the npm `latest` pointed to an unseeded version),
 * falls back to the highest seeded version — so `npm install <pkg>@latest`
 * resolves against the offline registry instead of 404-ing.
 */
function buildDistTags(
  seededVersions: string[],
  npmDistTags?: Record<string, string>,
): Record<string, string> {
  const seeded = new Set(seededVersions)
  const tags: Record<string, string> = {}
  if (npmDistTags) {
    for (const [tag, ver] of Object.entries(npmDistTags)) {
      if (seeded.has(ver)) tags[tag] = ver
    }
  }
  if (!tags.latest) {
    tags.latest = seededVersions.reduce((hi, v) =>
      compareVersion(v, hi) > 0 ? v : hi,
    )
  }
  return tags
}

/**
 * Merge a new version into an already-seeded Verdaccio packument. Accumulates
 * the version metadata + `time` entry + rewrites the tarball URL — the inverse
 * of [`buildMinimalPackument`] for the multi-version case (e.g. the tight set
 * installs both is-number@3.0.0 and is-number@6.0.0). Pure function — the I/O
 * (read, write) stays in `main()`.
 */
export function mergeVersionIntoPackument(opts: {
  packument: Record<string, unknown>
  name: string
  version: string
  versionMeta: Record<string, unknown>
  time?: Record<string, string | undefined>
  distTags?: Record<string, string>
  registryUrl: string
}): Record<string, unknown> {
  const { packument, name, version, versionMeta, time = {}, distTags, registryUrl } = opts
  const versions = (packument.versions ?? {}) as Record<string, Record<string, unknown>>
  const existingTime = (packument.time ?? {}) as Record<string, string | undefined>
  versions[version] = {
    ...versionMeta,
    dist: {
      ...(versionMeta.dist as Record<string, unknown> | undefined),
      tarball: `${registryUrl}${name}/-/${name}-${version}.tgz`,
    },
  }
  return {
    ...packument,
    name,
    versions,
    'dist-tags': buildDistTags(Object.keys(versions), distTags),
    time: {
      ...existingTime,
      created: existingTime.created ?? time.created,
      modified: existingTime.modified ?? time.modified,
      [version]: time[version],
    },
    _distfiles: {},
    _uplinks: {},
  }
}

/**
 * Build the minimal Verdaccio-format packument for a seeded package. Preserves
 * the npm `time` field (aube's minimumReleaseAge check reads it) + rewrites the
 * tarball URL to point at the local registry. Pure function — the I/O (fetch,
 * write) stays in `main()`.
 */
export function buildMinimalPackument(opts: {
  name: string
  version: string
  versionMeta: Record<string, unknown>
  time?: Record<string, string | undefined>
  distTags?: Record<string, string>
  registryUrl: string
}): Record<string, unknown> {
  const { name, version, versionMeta, time = {}, distTags, registryUrl } = opts
  return {
    name,
    versions: {
      [version]: {
        ...versionMeta,
        dist: {
          ...(versionMeta.dist as Record<string, unknown> | undefined),
          tarball: `${registryUrl}${name}/-/${name}-${version}.tgz`,
        },
      },
    },
    'dist-tags': buildDistTags([version], distTags),
    time: {
      created: time.created,
      modified: time.modified,
      [version]: time[version],
    },
    _distfiles: {},
    _uplinks: {},
  }
}

async function main(): Promise<void> {
  const specs = argv.slice(2).map(parseSpec)
  if (specs.length === 0) {
    stderr.write('Usage: seed.mts <pkg>@<version>...\n')
    exit(1)
  }

  if (!env['SOCKET_API_KEY']) {
    stderr.write('seed: SOCKET_API_KEY not set — required for the malware safety check\n')
    exit(1)
  }

  // Step 1: Check all packages against the Socket malware API.
  stderr.write('seed: step 1 — Socket malware safety check...\n')
  const checkResult = spawnSync('node', [
    join(import.meta.dirname, 'check-safety.mts'),
    ...specs.map(s => `${s.name}@${s.version}`),
  ], {
    stdio: 'inherit',
    env,
  })
  if (checkResult.status !== 0) {
    stderr.write('seed: safety check failed — refusing to seed\n')
    exit(1)
  }

  // Step 2: Download each package's tarball + packument from npm.
  stderr.write('seed: step 2 — downloading from npm...\n')
  mkdirSync(STORAGE_DIR, { recursive: true })

  for (const { name, version } of specs) {
    // Fetch the packument.
    const packumentUrl = name.startsWith('@')
      ? `${REGISTRY}/${name}`
      : `${REGISTRY}/${name}`
    const packumentResp = await fetch(packumentUrl)
    if (!packumentResp.ok) {
      stderr.write(`seed: failed to fetch packument for ${name}: ${packumentResp.status}\n`)
      exit(1)
    }
    const packument = await packumentResp.json()

    // Extract the version-specific packument + tarball URL.
    const versionMeta = packument.versions?.[version]
    if (!versionMeta) {
      stderr.write(`seed: version ${version} not found for ${name}\n`)
      exit(1)
    }
    const tarballUrl = versionMeta.dist?.tarball
    if (!tarballUrl) {
      stderr.write(`seed: no tarball URL for ${name}@${version}\n`)
      exit(1)
    }

    // Download the tarball.
    const tarballResp = await fetch(tarballUrl)
    if (!tarballResp.ok) {
      stderr.write(`seed: failed to download tarball for ${name}@${version}: ${tarballResp.status}\n`)
      exit(1)
    }
    const tarball = Buffer.from(await tarballResp.arrayBuffer())

    // Step 3: Place into Verdaccio storage.
    // Verdaccio stores: storage/<name>/package.json (packument) + storage/<name>/<version>.tgz (tarball)
    const pkgStorageDir = join(STORAGE_DIR, name)
    mkdirSync(pkgStorageDir, { recursive: true })

    // Write a minimal packument (Verdaccio format). Preserve the `time` field
    // from npm — aube's minimumReleaseAge check reads it to gate freshly
    // published packages, and errors with ERR_NUB_RELEASE_AGE_MISSING_TIME
    // when it's absent. MERGE into an existing packument when one is already
    // seeded (the tight set installs multiple versions of some packages —
    // e.g. is-number@3.0.0 + is-number@6.0.0 — so each version must accumulate
    // into the same packument rather than overwriting the prior one).
    const packumentPath = join(pkgStorageDir, 'package.json')
    const existingPackument = existsSync(packumentPath)
      ? (JSON.parse(readFileSync(packumentPath, 'utf8')) as Record<string, unknown>)
      : null
    const minimalPackument = existingPackument
      ? mergeVersionIntoPackument({
          packument: existingPackument,
          name,
          version,
          versionMeta,
          time: packument.time,
          distTags: packument['dist-tags'],
          registryUrl: 'http://localhost:4874/',
        })
      : buildMinimalPackument({
          name,
          version,
          versionMeta,
          time: packument.time,
          distTags: packument['dist-tags'],
          registryUrl: 'http://localhost:4874/',
        })
    writeFileSync(packumentPath, JSON.stringify(minimalPackument, null, 2) + '\n')

    // Write the tarball.
    const tarballName = tarballUrl.split('/').pop()!
    writeFileSync(join(pkgStorageDir, tarballName), tarball)

    stderr.write(`  seeded: ${name}@${version} (${tarball.length} bytes)\n`)
  }

  stderr.write(`seed: done — ${specs.length} package(s) seeded into ${STORAGE_DIR}\n`)
  stderr.write('seed: commit the storage with: git add tests/registry/storage/\n')
}

// Run only when executed directly, not when imported by the test. Both sides
// are realpath-resolved: `import.meta.filename` already is, and
// `process.argv[1]` is not (Node leaves it exactly as typed) — a symlinked
// checkout directory would otherwise make this compare unequal and main()
// would silently never run, exiting 0 having checked nothing.
if (process.argv[1] !== undefined && realpathSync(process.argv[1]) === import.meta.filename) {
  main().catch((err) => {
    stderr.write(`seed: error: ${err}\n`)
    exit(1)
  })
}

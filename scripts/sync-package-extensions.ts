#!/usr/bin/env node
// sync-package-extensions — regenerate the vendored unified package-extensions
// defaults under vendor/package-extensions/ from the live ecosystem sources.
//
// Runs under BOTH plain Node (type-stripping) and nub:
//   node scripts/sync-package-extensions.ts
//   nub  scripts/sync-package-extensions.ts
//
// Erasable TypeScript only (no enums/namespaces/parameter-properties) so plain
// modern `node` runs it with no build step — same constraint as the other scripts/*.ts.
//
// Sources:
//   - Yarn: `@yarnpkg/extensions` (npm-published, BSD-2-Clause). `npm pack`, then
//     read lib/index.js for the `packageExtensions` array.
//   - pnpm: not published standalone. Fetch pnpm/pnpm's
//     pnpm_compat_package_extensions.json (pnpm-specific entries not in Yarn yet).
//     compat_package_extensions.json is a stale copy of Yarn and is NOT merged.
//   - nub-phantom: vendor/package-extensions/nub-phantom-extensions.json, generated
//     separately by crates/nub-phantom-scan/src/bin/emit-extensions.rs. This script
//     preserves it as-is (does not regenerate the scan).
//
// The exported @yarnpkg/extensions list is an ARRAY of [selector, body] pairs and
// carries one duplicate selector (gatsby-core-utils@<2.14.0-next.1) with two
// different bodies. We deep-merge bodies on selector collision so the
// selector-keyed map representation is lossless (last-wins would drop a dep).
//
// Output is a selector -> body map (body fields: dependencies, optionalDependencies,
// peerDependencies, peerDependenciesMeta) — the same shape aube's
// `embedder_package_extensions` / `parse_package_extensions` consume.
//
// Idempotent: a second run with unchanged upstreams produces byte-identical output.

import { execSync } from 'node:child_process'
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, rmSync, existsSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { createRequire } from 'node:module'

const require = createRequire(import.meta.url)

const ROOT = resolve(import.meta.dirname ?? __dirname, '..')
const DIR = join(ROOT, 'vendor/package-extensions')
const PNPM_RAW = (sha: string, p: string) =>
  `https://raw.githubusercontent.com/pnpm/pnpm/${sha}/${p}`

type Body = Record<string, Record<string, unknown>>
type ExtMap = Record<string, Body>

// Deep-merge `from` into `into`: for each field, union the inner map's keys
// (first-write-wins per key, matching aube's `extend_missing` semantics).
function mergeBody(into: Body, from: Body): void {
  for (const field of Object.keys(from)) {
    into[field] = into[field] ?? {}
    for (const [k, v] of Object.entries(from[field])) {
      if (!(k in into[field])) into[field][k] = v
    }
  }
}

function sh(cmd: string): string {
  return execSync(cmd, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'] }).trim()
}

async function fetchText(url: string): Promise<string> {
  const res = await fetch(url)
  if (!res.ok) throw new Error(`GET ${url} -> ${res.status} ${res.statusText}`)
  return res.text()
}

function writeJson(file: string, obj: unknown): void {
  writeFileSync(file, JSON.stringify(obj, null, 2) + '\n')
}

// --- Yarn: npm pack @yarnpkg/extensions, require lib/index.js, deep-merge dup selectors ---
function fetchYarn(): { version: string; map: ExtMap } {
  const version = sh('npm view @yarnpkg/extensions version')
  const tmp = mkdtempSync(join(tmpdir(), 'yarnpkg-ext-'))
  try {
    sh(`npm pack @yarnpkg/extensions --pack-destination ${JSON.stringify(tmp)}`)
    const tarball = join(tmp, `yarnpkg-extensions-${version}.tgz`)
    sh(`tar -xzf ${JSON.stringify(tarball)} -C ${JSON.stringify(tmp)}`)
    // require() the CJS build to get the curated array verbatim.
    // eslint-disable-next-line @typescript-eslint/no-require-imports
    const mod = require(join(tmp, 'package/lib/index.js')) as { packageExtensions?: [string, Body][]; default?: { packageExtensions?: [string, Body][] } }
    const arr: [string, Body][] = mod.packageExtensions ?? mod.default?.packageExtensions
    const map: ExtMap = {}
    for (const [sel, body] of arr) {
      if (map[sel]) mergeBody(map[sel], body)
      else map[sel] = JSON.parse(JSON.stringify(body))
    }
    return { version, map }
  } finally {
    rmSync(tmp, { recursive: true, force: true })
  }
}

// --- pnpm: the pnpm-specific entries not in Yarn yet ---
async function fetchPnpm(): Promise<{ sha: string; map: ExtMap }> {
  const sha = sh('git ls-remote https://github.com/pnpm/pnpm HEAD').split(/\s+/)[0]
  const specUrl = PNPM_RAW(sha, 'pnpm/crates/package-manager/src/pnpm_compat_package_extensions.json')
  const spec: [string, Body][] = JSON.parse(await fetchText(specUrl))
  const map: ExtMap = {}
  for (const e of spec) {
    const sel = e[0]
    const body = e[1]
    map[sel] = JSON.parse(JSON.stringify(body))
  }
  return { sha, map }
}

async function main(): Promise<void> {
  mkdirSync(DIR, { recursive: true })

  const yarn = fetchYarn()
  writeJson(join(DIR, 'yarnpkg-extensions.json'), yarn.map)

  const pnpm = await fetchPnpm()
  writeJson(join(DIR, 'pnpm-extensions.json'), pnpm.map)

  // nub-phantom: preserve an existing generated file, else seed empty.
  const phantomPath = join(DIR, 'nub-phantom-extensions.json')
  const phantom: ExtMap = existsSync(phantomPath)
    ? JSON.parse(readFileSync(phantomPath, 'utf8'))
    : {}
  if (!existsSync(phantomPath)) writeJson(phantomPath, {})

  // unified: yarn ∪ pnpm-unique ∪ nub-phantom, deep-merging on selector collision.
  const unified: ExtMap = {}
  for (const [sel, body] of Object.entries(yarn.map)) unified[sel] = JSON.parse(JSON.stringify(body))
  for (const [sel, body] of Object.entries(pnpm.map)) {
    if (unified[sel]) mergeBody(unified[sel], body)
    else unified[sel] = JSON.parse(JSON.stringify(body))
  }
  for (const [sel, body] of Object.entries(phantom)) {
    if (unified[sel]) mergeBody(unified[sel], body)
    else unified[sel] = JSON.parse(JSON.stringify(body))
  }
  writeJson(join(DIR, 'unified.json'), unified)

  writeFileSync(
    join(DIR, 'UPSTREAM'),
    `# Which upstream revisions this vendored package-extensions data derives from.
#
# Regenerate with: nub scripts/sync-package-extensions.ts
# (or:               node scripts/sync-package-extensions.ts)
#
# The bundled defaults are binary-version ecosystem data, deliberately excluded
# from the lockfile packageExtensionsChecksum (see crates/nub-cli/src/pm_engine/mod.rs
# bundled_package_extensions_defaults). Bumping this data does NOT drift existing
# lockfiles.
#
# UPDATE THIS IN THE SAME COMMIT that changes the vendored data.

yarn_package = @yarnpkg/extensions
yarn_version = ${yarn.version}
yarn_source  = https://github.com/yarnpkg/berry  (packages/yarnpkg-extensions)
yarn_license = BSD-2-Clause  (Copyright (c) 2016-present, Yarn Contributors)

pnpm_repo    = pnpm/pnpm
pnpm_commit  = ${sha(pnpm.sha, 10)}
pnpm_paths   = pnpm/crates/package-manager/src/compat_package_extensions.json
               pnpm/crates/package-manager/src/pnpm_compat_package_extensions.json
pnpm_note    = compat_package_extensions.json is a copy of @yarnpkg/extensions (BSD-2-Clause);
               only pnpm_compat_package_extensions.json (pnpm-specific entries not in Yarn yet)
               is merged into unified.json.

nub_phantom  = generated by crates/nub-phantom-scan/src/bin/emit-extensions.rs
               from \`nub-phantom scan --top N --json\` output (npm-high-impact corpus).
`
  )

  const n = Object.keys(unified).length
  console.log(`synced package-extensions: yarn ${Object.keys(yarn.map).length} + pnpm ${Object.keys(pnpm.map).length} + nub-phantom ${Object.keys(phantom).length} -> unified ${n}`)
}

// `sha` helper: truncate a commit sha for the UPSTREAM marker.
function sha(s: string, len: number): string {
  return s.slice(0, len)
}

main().catch((err) => {
  console.error(err)
  process.exit(1)
})

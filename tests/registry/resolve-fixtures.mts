#!/usr/bin/env node
// resolve-fixtures — resolve the full transitive dependency graph of one or
// more conformance fixtures by running the real package managers against
// registry.npmjs.org (network needed) and extracting every name@version pair
// from the resulting lockfiles. The union across all PMs is printed as
// newline-delimited `name@version` specs on stdout — pipe into seed.mts:
//
//   node tests/registry/resolve-fixtures.mts simple scoped peers ... \
//     | node tests/registry/seed.mts --from /dev/stdin
//
// Or with no fixture args: resolves every tight-set fixture (the per-PR list
// from .github/workflows/lockfile-roundtrip.yml, minus git-dep which resolves
// from github.com, not the npm registry).
//
// Each fixture is resolved with every PM that can handle it: npm, pnpm, and
// bun. A PM that rejects the fixture (e.g. npm rejects the `workspace:`
// protocol, npm rejects `catalog:`) is silently skipped — the remaining PMs
// still produce the graph. The UNION across all succeeding PMs is emitted, so
// the seeded registry serves every version any PM in the tight set could ask
// for.
//
// Workspace-internal deps (`workspace:*`, `link:`) are excluded — they resolve
// on disk, not from the registry.

import { spawnSync } from 'node:child_process'
import { cpSync, mkdtempSync, rmSync, readFileSync, existsSync, realpathSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, dirname } from 'node:path'
import { argv, exit, stderr } from 'node:process'

const HERE = import.meta.dirname
const FIXTURES_DIR = join(HERE, '..', 'conformance', 'fixtures')

// The per-PR tight-set fixture list from the lockfile-roundtrip workflow,
// minus git-dep (resolves from github.com, not the npm registry — served by
// nightly, not the Verdaccio offline registry).
const TIGHT_SET = [
  'simple',
  'scoped',
  'peers',
  'overrides-ref',
  'overrides-nested',
  'patched-deps',
  'catalog',
  'workspace',
  'workspace-dedup',
  'empty-root-importer',
  'platform-optional',
  'dist-tag-spec',
  'range-forms',
  'alias-scoped',
  'has-install-script',
  'injected-deps',
]

type Spec = `${string}@${string}`

/**
 * Parse an npm package-lock.json (v3) `packages` object into name@version
 * specs. Keys are `node_modules/<name>` (possibly nested). Workspace links
 * (`link: true`, no `version`) are skipped — they resolve on disk.
 */
function parseNpmLock(lockPath: string): Set<Spec> {
  const specs = new Set<Spec>()
  if (!existsSync(lockPath)) return specs
  const lock = JSON.parse(readFileSync(lockPath, 'utf8'))
  const packages = lock.packages ?? {}
  for (const [key, meta] of Object.entries(packages)) {
    // The root package (key === '') is the fixture itself — skip.
    if (key === '') continue
    // Only real registry deps live under node_modules/. Workspace packages
    // have keys like `packages/pkg-a` (link to on-disk siblings) — skip them.
    if (!key.startsWith('node_modules/')) continue
    const m = meta as Record<string, unknown>
    // Workspace symlinks / links resolve on disk, not from the registry.
    if (m.link || m.extraneous) continue
    const version = m.version as string | undefined
    if (!version) continue
    // Extract the package name from the node_modules path. For nested paths
    // (node_modules/foo/node_modules/bar) the LAST segment is the real name.
    // Scoped names: node_modules/@scope/pkg.
    const parts = key.replace(/^node_modules\//, '').split(/\/node_modules\//)
    const name = parts[parts.length - 1]!
    // Alias guard: an npm: alias entry's `resolved` URL points at a DIFFERENT
    // package's tarball (e.g. `node_modules/semver-types` →
    // `.../@types/semver/-/@types/semver-7.5.0.tgz`). The real target is
    // already captured as its own node_modules entry, so skip the alias name
    // — Verdaccio has no packument for it.
    const resolved = m.resolved as string | undefined
    if (resolved) {
      // `.../@scope/pkg/-/pkg-1.0.0.tgz` keeps the whole registry path up to
      // `/-/`, so a bare `.split('/').pop()` returns only the final segment
      // (`pkg`) and drops the scope — every scoped entry would then compare
      // unequal to `name` and get skipped. Re-attach the scope when present.
      const pkgPath = resolved.split('/-/')[0]!
      const segs = pkgPath.split('/')
      const scope = segs[segs.length - 2]
      const actual = scope?.startsWith('@') ? `${scope}/${segs[segs.length - 1]}` : segs[segs.length - 1]!
      if (actual !== name) continue
    }
    specs.add(`${name}@${version}` as Spec)
  }
  // Also walk the legacy `dependencies` tree (v1/v2 lockfiles) if present.
  const walkDeps = (deps: Record<string, unknown> | undefined) => {
    if (!deps) return
    for (const [name, meta] of Object.entries(deps)) {
      const m = meta as Record<string, unknown>
      const version = m.version as string | undefined
      if (version && !version.startsWith('file:') && !version.startsWith('link:')) {
        specs.add(`${name}@${version}` as Spec)
      }
      walkDeps(m.dependencies as Record<string, unknown> | undefined)
    }
  }
  walkDeps(lock.dependencies)
  return specs
}

/**
 * Parse a pnpm-lock.yaml (v9) `packages:` section into name@version specs.
 * Entries look like `  ms@2.1.3:` or `  @babel/code-frame@7.26.2:` — possibly
 * with a `(peer@ver)` suffix that must be stripped. This is a lightweight
 * line-based scan (not a full YAML parse) — we only need the package keys.
 */
function parsePnpmLock(lockPath: string): Set<Spec> {
  const specs = new Set<Spec>()
  if (!existsSync(lockPath)) return specs
  const text = readFileSync(lockPath, 'utf8')
  // Find the top-level `packages:` section. It starts at column 0 and its
  // entries are indented by 2 spaces. The section ends at the next
  // column-0 key.
  const lines = text.split('\n')
  let inPackages = false
  for (const line of lines) {
    if (line.length === 0) continue
    if (line[0] !== ' ' && line[0] !== '\t') {
      inPackages = line.startsWith('packages:')
      continue
    }
    if (!inPackages) continue
    // Entry lines: exactly 2-space indent, then `name@version:` (or
    // `@scope/name@version:`), optionally followed by `(peerInfo)`. The \S
    // after the 2 spaces skips 4-space-indented fields (resolution, engines,
    // peerDependencies, …) which are not package entries.
    const m = line.match(/^  (\S[^:]*?):(?:\s|$)/)
    if (!m) continue
    let spec = m[1]!
    // Every real package entry is `name@version` — field names don't contain @.
    if (!spec.includes('@')) continue
    // Strip peer-resolution suffix: `ms@2.1.3(peer_react@18.3.1)` → `ms@2.1.3`
    const paren = spec.indexOf('(')
    if (paren !== -1) spec = spec.slice(0, paren)
    // Strip YAML quoting: `'@babel/code-frame@7.29.7'` → `@babel/code-frame@7.29.7`
    if (spec.startsWith("'") && spec.endsWith("'")) spec = spec.slice(1, -1)
    if (spec.startsWith('"') && spec.endsWith('"')) spec = spec.slice(1, -1)
    specs.add(spec as Spec)
  }
  return specs
}

/**
 * Parse a bun.lock (bun 1.3 JSON format) `packages` object into name@version
 * specs. Keys are `name@version`.
 */
function parseBunLock(lockPath: string): Set<Spec> {
  const specs = new Set<Spec>()
  if (!existsSync(lockPath)) return specs
  const lock = JSON.parse(readFileSync(lockPath, 'utf8'))
  const packages = lock.packages ?? {}
  for (const [key, meta] of Object.entries(packages)) {
    const m = meta as Record<string, unknown> | null
    // Only include real registry tarballs. Workspace packages have a symlink
    // resolution; alias names point at a different tarball and the real target
    // is captured under its own key.
    if (m && typeof m === 'object') {
      const resolution = m.resolution as Record<string, unknown> | undefined
      if (resolution) {
        const type = resolution.type as string | undefined
        // Skip symlink (workspace) and non-tarball resolutions.
        if (type && type !== 'tarball') continue
        // Alias guard: the tarball URL's package segment must match the key's
        // name, otherwise this is an alias entry whose real target is already
        // captured separately.
        const url = resolution.url as string | undefined
        if (url) {
          // Same scope-preserving extraction as parseNpmLock: a bare
          // `.split('/').pop()` on `.../@scope/pkg/-/pkg-1.0.0.tgz` drops the
          // scope and skips every scoped entry.
          const pkgPath = url.split('/-/')[0]!
          const segs = pkgPath.split('/')
          const scope = segs[segs.length - 2]
          const tarballName = scope?.startsWith('@') ? `${scope}/${segs[segs.length - 1]}` : segs[segs.length - 1]!
          // Extract the name portion from `name@version` (handle @scope).
          const at = key.startsWith('@') ? key.indexOf('@', 1) : key.indexOf('@')
          const name = at !== -1 ? key.slice(0, at) : key
          if (tarballName !== name) continue
        }
      }
    }
    // Exclude workspace-path keys like `packages/pkg-a@1.0.0` (contain a `/`
    // but are not scoped names starting with `@`).
    if (key.includes('/') && !key.startsWith('@')) continue
    specs.add(key as Spec)
  }
  return specs
}

/**
 * Resolve a single fixture with one PM. Returns the set of name@version specs
 * extracted from the produced lockfile, or null if the PM rejected the
 * fixture (non-zero exit, no lockfile).
 */
function resolveFixture(
  fixture: string,
  pm: 'npm' | 'pnpm' | 'bun',
  workDir: string,
): Set<Spec> | null {
  const proj = join(workDir, `${fixture}--${pm}`)
  rmSync(proj, { recursive: true, force: true })
  cpSync(join(FIXTURES_DIR, fixture), proj, { recursive: true })

  // Remove any pre-existing lockfiles so the PM writes fresh ones (fixtures
  // ship without lockfiles, but be defensive).
  for (const lf of ['package-lock.json', 'pnpm-lock.yaml', 'bun.lock', 'yarn.lock']) {
    rmSync(join(proj, lf), { force: true })
  }

  let result: ReturnType<typeof spawnSync>
  switch (pm) {
    case 'npm':
      // --package-lock-only: resolve + write the lockfile WITHOUT installing
      // node_modules. Fast + clean (no extraction step).
      result = spawnSync('npm', ['install', '--package-lock-only', '--no-audit', '--no-fund'], {
        cwd: proj,
        stdio: 'pipe',
        encoding: 'utf8',
      })
      if (result.status !== 0) return null
      return parseNpmLock(join(proj, 'package-lock.json'))
    case 'pnpm':
      // --lockfile-only: resolve + write pnpm-lock.yaml without installing.
      result = spawnSync('pnpm', ['install', '--lockfile-only', '--no-optional', '--config.confirmModulesPurge=false'], {
        cwd: proj,
        stdio: 'pipe',
        encoding: 'utf8',
        env: { ...process.env, CI: '1' },
      })
      if (result.status !== 0) {
        // Retry without --no-optional (the platform-optional fixture needs
        // optionals resolved into the lockfile even if not installed).
        rmSync(join(proj, 'pnpm-lock.yaml'), { force: true })
        result = spawnSync('pnpm', ['install', '--lockfile-only', '--config.confirmModulesPurge=false'], {
          cwd: proj,
          stdio: 'pipe',
          encoding: 'utf8',
          env: { ...process.env, CI: '1' },
        })
      }
      if (result.status !== 0) return null
      return parsePnpmLock(join(proj, 'pnpm-lock.yaml'))
    case 'bun':
      // bun has no --lockfile-only flag; `bun install --no-save` still writes
      // bun.lock in bun 1.3. Use --frozen-lockfile=false to be explicit.
      result = spawnSync('bun', ['install', '--no-save'], {
        cwd: proj,
        stdio: 'pipe',
        encoding: 'utf8',
        env: { ...process.env, BUN_INSTALL_CACHE_DIR: join(workDir, 'bun-cache') },
      })
      if (result.status !== 0) return null
      return parseBunLock(join(proj, 'bun.lock'))
  }
}

function main(): void {
  const fixtures = argv.slice(2).filter(a => !a.startsWith('-'))
  if (fixtures.length === 0) {
    fixtures.push(...TIGHT_SET)
  }

  // Validate fixture names.
  for (const f of fixtures) {
    if (!existsSync(join(FIXTURES_DIR, f))) {
      stderr.write(`resolve-fixtures: unknown fixture "${f}"\n`)
      exit(1)
    }
  }

  const workDir = mkdtempSync(join(tmpdir(), 'nub-resolve-'))
  const allSpecs = new Set<Spec>()
  let totalGraph = 0

  stderr.write(`resolve-fixtures: resolving ${fixtures.length} fixture(s) with npm + pnpm + bun...\n`)

  for (const fixture of fixtures) {
    let graphSize = 0
    const fixtureSpecs = new Set<Spec>()
    for (const pm of ['npm', 'pnpm', 'bun'] as const) {
      const specs = resolveFixture(fixture, pm, workDir)
      if (specs === null) {
        stderr.write(`  ${fixture} × ${pm}: skipped (PM rejected fixture)\n`)
        continue
      }
      for (const s of specs) fixtureSpecs.add(s)
      stderr.write(`  ${fixture} × ${pm}: ${specs.size} package(s)\n`)
    }
    for (const s of fixtureSpecs) allSpecs.add(s)
    graphSize = fixtureSpecs.size
    totalGraph += graphSize
    stderr.write(`  ${fixture}: ${graphSize} unique package(s) in transitive graph\n`)
  }

  // Print the union of all specs on stdout (newline-delimited).
  const sorted = [...allSpecs].sort()
  for (const spec of sorted) {
    process.stdout.write(spec + '\n')
  }

  stderr.write(`\nresolve-fixtures: ${allSpecs.size} unique package(s) across all fixtures\n`)
  rmSync(workDir, { recursive: true, force: true })
}

// Run only when executed directly, not when imported by the test. Both sides
// are realpath-resolved: `import.meta.filename` already is, and
// `process.argv[1]` is not (Node leaves it exactly as typed) — a symlinked
// checkout directory would otherwise make this compare unequal and main()
// would silently never run, exiting 0 having checked nothing.
if (process.argv[1] !== undefined && realpathSync(process.argv[1]) === import.meta.filename) {
  main()
}

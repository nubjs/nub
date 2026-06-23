#!/usr/bin/env node
import { spawnSync } from 'node:child_process'
import { mkdir, readFile } from 'node:fs/promises'
import { dirname, resolve } from 'node:path'

const args = new Map()
for (let i = 2; i < process.argv.length; i++) {
  const arg = process.argv[i]
  if (!arg.startsWith('--')) throw new Error(`unexpected argument: ${arg}`)
  const [key, inline] = arg.slice(2).split('=', 2)
  args.set(key, inline ?? process.argv[++i])
}

const top = Number(args.get('top') ?? 2000)
// Keep this default in sync with `DEFAULT_VERSION_CAP` in
// crates/aube-resolver/build.rs and `AUBE_PRIMER_VERSION_CAP` in
// .github/workflows/release.yml — the calibrated trade-off is
// documented in the build.rs comment.
const versionsArg = args.get('versions') ?? '100'
const versions = versionsArg === 'all' ? Infinity : Number(versionsArg)
const out = resolve(args.get('out') ?? `crates/aube-resolver/data/primer-top${top}.json`)
const namesFile = args.get('names')
const namesUrl = args.get('names-url') ?? 'https://raw.githubusercontent.com/jdx/aube-primer-packages/main/data/packages.json'

if (!Number.isInteger(top) || top < 1) throw new Error('--top must be a positive integer')
if (versions !== Infinity && (!Number.isInteger(versions) || versions < 1)) {
  throw new Error('--versions must be a positive integer or "all"')
}

const names = namesFile
  ? parseNames(await readFile(namesFile, 'utf8'), namesFile)
  : await fetchPopularNames(namesUrl)
if (!Array.isArray(names)) throw new Error('package-name source must be a JSON array')

const primer = {}
for (const [index, name] of names.slice(0, top).entries()) {
  console.error(`[${index + 1}/${top}] ${name} (${versions === Infinity ? 'all versions' : `latest ${versions}`})`)
  const seed = await packumentSeed(name, versions)
  if (seed) primer[name] = seed
}

await mkdir(dirname(out), { recursive: true })
const raw = Buffer.from(`${JSON.stringify(primer)}\n`)
if (out.endsWith('.json')) {
  await import('node:fs/promises').then(({ writeFile }) => writeFile(out, raw))
} else {
  const zstd = spawnSync('zstd', ['-q', '-19', '-f', '-o', out], { input: raw, stdio: ['pipe', 'inherit', 'inherit'] })
  if (zstd.status !== 0) throw new Error('zstd compression failed')
}
console.error(`wrote ${Object.keys(primer).length} packages to ${out}`)

async function packumentSeed(name, keepVersions) {
  const url = `https://registry.npmjs.org/${encodePackageName(name)}`
  // Request the *full* packument, NOT the abbreviated corgi
  // (`application/vnd.npm.install-v1+json`). Corgi omits the `time`
  // map, and the primer needs per-version publish times so the
  // resolver can honor `minimumReleaseAge` / `trustPolicy` straight
  // from the bundled seed instead of refetching the full packument
  // live for every primed package (a serialized refetch in the BFS
  // pick loop — the cold-install cliff this seed exists to avoid).
  // The accept header must list *only* `application/json`: npm's
  // content negotiation prefers corgi whenever it is named or a
  // `*/*` wildcard is present, regardless of the q-values, so adding
  // either silently drops us back to a time-less response.
  const { res, body: full } = await fetchBodyWithRetry(
    url,
    {
      headers: { accept: 'application/json' },
    },
    (res) => res.json(),
  )
  if (!res.ok) {
    console.error(`  skipped: HTTP ${res.status}`)
    return null
  }
  const selected = selectVersions(full, keepVersions)
  const packument = {
    n: full.name ?? name,
    m: full.modified,
    d: trimDistTags(full['dist-tags'], selected),
    v: selected.map((v) => ({
      v,
      t: full.time?.[v],
      m: trimVersion(full.versions[v]),
    })),
  }
  return {
    e: res.headers.get('etag'),
    lm: res.headers.get('last-modified'),
    p: packument,
  }
}

function selectVersions(packument, keepVersions) {
  const versions = Object.keys(packument.versions ?? {})
  const byTime = versions
    .filter((v) => packument.time?.[v])
    .sort((a, b) => packument.time[a].localeCompare(packument.time[b]))
  const ordered = byTime.length ? byTime : versions
  if (keepVersions === Infinity) return ordered
  return ordered.slice(-keepVersions)
}

function trimDistTags(tags = {}, selected) {
  const out = {}
  for (const [tag, version] of Object.entries(tags)) {
    if (selected.includes(version)) out[tag] = version
  }
  return out
}

function trimVersion(v = {}) {
  return {
    d: stringMap(v.dependencies),
    p: stringMap(v.peerDependencies),
    pm: peerDepMetaMap(v.peerDependenciesMeta),
    o: stringMap(v.optionalDependencies),
    b: bundledDependencies(v.bundledDependencies ?? v.bundleDependencies),
    dt:
      typeof v.dist?.tarball === 'string'
        ? {
            // Omit the tarball URL when it matches the deterministic
            // `{registry}/{name}/-/{unscoped}-{version}.tgz` pattern.
            // The runtime synthesizes that form from name+version, so
            // dropping it shaves ~20% of primer bytes. A handful of
            // legacy publishes (e.g. handlebars@1.0.2-beta -> `1.0.2beta`
            // in the basename) diverge from the pattern and still need
            // the field.
            t: deterministicTarball(v.name, v.version) === v.dist.tarball ? undefined : v.dist.tarball,
            i: typeof v.dist.integrity === 'string' ? v.dist.integrity : undefined,
            a: hasProvenance(v.dist.attestations),
          }
        : undefined,
    os: stringArray(v.os),
    cpu: stringArray(v.cpu),
    libc: stringArray(v.libc),
    e: stringMap(v.engines),
    l: typeof v.license === 'string' ? v.license : typeof v.license?.type === 'string' ? v.license.type : undefined,
    f: fundingUrl(v.funding),
    bin: binMap(v.name, v.bin),
    h: v.hasInstallScript,
    x: typeof v.deprecated === 'string' && v.deprecated ? v.deprecated : undefined,
    u: hasTrustedPublisher(v._npmUser),
  }
}

function stringArray(value) {
  if (typeof value === 'string') return [value]
  if (Array.isArray(value)) return value.filter((v) => typeof v === 'string')
  return undefined
}

function stringMap(value) {
  if (typeof value !== 'object' || !value || Array.isArray(value)) return undefined
  const out = Object.fromEntries(Object.entries(value).filter(([, v]) => typeof v === 'string'))
  return Object.keys(out).length ? out : undefined
}

function peerDepMetaMap(value) {
  if (typeof value !== 'object' || !value || Array.isArray(value)) return undefined
  const out = {}
  for (const [name, meta] of Object.entries(value)) {
    if (typeof meta === 'object' && meta && typeof meta.optional === 'boolean') {
      out[name] = { optional: meta.optional }
    }
  }
  return Object.keys(out).length ? out : undefined
}

function bundledDependencies(value) {
  if (value === true) return true
  if (Array.isArray(value)) return value.filter((v) => typeof v === 'string')
  return undefined
}

function binMap(name, bin) {
  if (typeof bin === 'string') return { [unscopedName(name)]: bin }
  if (typeof bin === 'object' && bin && !Array.isArray(bin)) return stringMap(bin)
  return undefined
}

function unscopedName(name = '') {
  return name.split('/').pop() || name
}

function deterministicTarball(name, version) {
  if (typeof name !== 'string' || typeof version !== 'string') return undefined
  return `https://registry.npmjs.org/${name}/-/${unscopedName(name)}-${version}.tgz`
}

function fundingUrl(funding) {
  if (typeof funding === 'string') return funding
  if (Array.isArray(funding)) return funding.map(fundingUrl).find(Boolean)
  if (typeof funding?.url === 'string') return funding.url
  return undefined
}

function hasTrustedPublisher(user) {
  return Boolean(
    user &&
      typeof user === 'object' &&
      user.trustedPublisher &&
      typeof user.trustedPublisher === 'object' &&
      typeof user.trustedPublisher.id === 'string' &&
      user.trustedPublisher.id,
  )
}

function hasProvenance(attestations) {
  const predicate = attestations?.provenance?.predicateType
  return typeof predicate === 'string' && /^https:\/\/slsa\.dev\/provenance\/v\d+$/.test(predicate)
}

async function fetchPopularNames(url) {
  const { res, body } = await fetchBodyWithRetry(url, undefined, (res) => res.text())
  if (!res.ok) throw new Error(`${url}: HTTP ${res.status}`)
  return parseNames(body, url)
}

// Wrap fetch and body reads to retry transient failures: socket resets /
// TLS hangups from npmjs.com can happen after headers arrive, while
// res.json() is still reading the body. Also retry 5xx and 429. 4xx
// other than 429 are permanent - propagate.
async function fetchBodyWithRetry(url, init, readBody, attempts = 5) {
  let delay = 1000
  for (let i = 1; i <= attempts; i++) {
    try {
      const res = await fetch(url, init)
      if (res.ok) return { res, body: await readBody(res) }
      if (res.status >= 400 && res.status < 500 && res.status !== 429) return { res }
      if (i === attempts) return { res }
      console.error(`  retry ${i}/${attempts - 1}: HTTP ${res.status}`)
    } catch (err) {
      if (i === attempts) throw err
      console.error(`  retry ${i}/${attempts - 1}: ${err.cause?.code ?? err.code ?? err.message}`)
    }
    await new Promise((r) => setTimeout(r, delay))
    delay *= 2
  }
  throw new Error('unreachable')
}

function parseNames(text, source) {
  const trimmed = text.trim()
  if (trimmed.startsWith('[')) return JSON.parse(trimmed)
  const lines = trimmed
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line && !line.startsWith('#'))
  if (lines.length && lines.every((line) => !/\s/.test(line))) return lines
  if (trimmed.includes('npmjs.com/package/')) return parseNpmRankHtml(trimmed)
  throw new Error(`could not parse package names from ${source}`)
}

function parseNpmRankHtml(html) {
  const names = []
  const seen = new Set()
  for (const match of html.matchAll(/https:\/\/www\.npmjs\.com\/package\/([^"'<>?#\s]+)/g)) {
    const name = decodeURIComponent(match[1])
    if (!seen.has(name)) {
      seen.add(name)
      names.push(name)
    }
  }
  if (!names.length) throw new Error('could not parse package names from npm-rank HTML')
  return names
}

function encodePackageName(name) {
  return name.startsWith('@') ? name.replace('/', '%2F') : encodeURIComponent(name)
}

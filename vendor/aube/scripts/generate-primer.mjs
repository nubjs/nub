#!/usr/bin/env node
import { spawnSync } from 'node:child_process'
import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { dirname, resolve } from 'node:path'

const args = new Map()
for (let i = 2; i < process.argv.length; i++) {
  const arg = process.argv[i]
  if (!arg.startsWith('--')) throw new Error(`unexpected argument: ${arg}`)
  const [key, inline] = arg.slice(2).split('=', 2)
  if (key === 'popular-names-only') {
    if (inline !== undefined) throw new Error('--popular-names-only does not take a value')
    args.set(key, true)
    continue
  }
  args.set(key, inline ?? process.argv[++i])
}

const top = Number(args.get('top') ?? 2000)
// Keep this default in sync with `DEFAULT_VERSION_CAP` in
// crates/aube-resolver/build.rs and `AUBE_PRIMER_VERSION_CAP` in
// .github/workflows/release.yml — the calibrated trade-off is
// documented in the build.rs comment.
const versionsArg = args.get('versions') ?? '100'
const versions = versionsArg === 'all' ? Infinity : Number(versionsArg)
// Age prune on top of the count cap — see `selectVersions`. `none` disables.
const pruneAgeArg = args.get('prune-age-days') ?? '1095'
const pruneAgeDays = pruneAgeArg === 'none' ? Infinity : Number(pruneAgeArg)
const out = resolve(args.get('out') ?? `crates/aube-resolver/data/primer-top${top}.json`)
const namesFile = args.get('names')
const namesUrl = args.get('names-url') ?? 'https://raw.githubusercontent.com/jdx/aube-primer-packages/main/data/packages.json'
const popularNamesOut = args.get('popular-names-out')
const popularNamesOnly = args.has('popular-names-only')
const popularNamesUrl =
  args.get('popular-names-url') ??
  'https://raw.githubusercontent.com/jdx/aube-primer-packages/main/data/popular.json'

if (!Number.isInteger(top) || top < 1) throw new Error('--top must be a positive integer')
if (versions !== Infinity && (!Number.isInteger(versions) || versions < 1)) {
  throw new Error('--versions must be a positive integer or "all"')
}
if (pruneAgeDays !== Infinity && (!Number.isInteger(pruneAgeDays) || pruneAgeDays < 1)) {
  throw new Error('--prune-age-days must be a positive integer or "none"')
}
if (popularNamesOnly && !popularNamesOut) {
  throw new Error('--popular-names-only requires --popular-names-out')
}

if (popularNamesOut) {
  const popularNames = validatePopularNames(await fetchPopularNames(popularNamesUrl), popularNamesUrl)
  await mkdir(dirname(resolve(popularNamesOut)), { recursive: true })
  await writeFile(resolve(popularNamesOut), `${JSON.stringify(popularNames)}\n`)
  console.error(`wrote ${popularNames.length} popular package names to ${resolve(popularNamesOut)}`)
}
if (popularNamesOnly) process.exit(0)

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
  await writeFile(out, raw)
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
  const { selected, sparse } = selectVersions(full, keepVersions, pruneAgeDays)
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
    ...(sparse ? { sp: true } : {}),
    p: packument,
  }
}

// The newest `keepVersions` by publish time, minus anything older than
// `pruneAgeDays` that is neither the highest of its `major.minor` line nor a
// dist-tag target. A fresh resolve takes the newest version a range admits, so
// an old version only wins when its whole line is old — and both `^x` and
// `~x.y` land on the highest of a line, which stays. Every other shape can
// land on a dropped version (`<4.17.21` against lodash: 4.17.20 is dropped,
// 4.16.6 stays), so a pruned seed is flagged `sparse` and the resolver
// refetches such picks (`semver_util::sparse_pick_needs_refetch`). That
// contract is what the highest-of-line rule exists for: the highest held
// version of a line must be the highest version of the line. The per-version
// SHA-512 is the primer's dominant byte cost and does not compress, so every
// pruned version is ~100 bytes off the shipped binary.
function selectVersions(packument, keepVersions, pruneAgeDays) {
  const time = packument.time ?? {}
  const versions = Object.keys(packument.versions ?? {})
  const byTime = versions.filter((v) => time[v]).sort((a, b) => time[a].localeCompare(time[b]))
  const ordered = byTime.length ? byTime : versions
  const kept = keepVersions === Infinity ? ordered : ordered.slice(-keepVersions)
  if (pruneAgeDays === Infinity || !byTime.length) return { selected: kept, sparse: false }
  const cutoff = Date.now() - pruneAgeDays * 86_400_000
  const highestOfLine = new Map()
  for (const v of versions) {
    const line = versionLine(v)
    const cur = highestOfLine.get(line)
    if (cur === undefined || compareVersions(v, cur) > 0) highestOfLine.set(line, v)
  }
  const pinned = new Set([...highestOfLine.values(), ...Object.values(packument['dist-tags'] ?? {})])
  const selected = kept.filter((v) => pinned.has(v) || Date.parse(time[v]) >= cutoff)
  return { selected, sparse: selected.length < kept.length }
}

// `major.minor`, with prereleases on their own line so a newer `-beta` never
// evicts the stable release a caret range actually resolves to.
function versionLine(version) {
  const m = /^(\d+)\.(\d+)\.\d+(-)?/.exec(version)
  return m ? `${m[1]}.${m[2]}${m[3] ? '-pre' : ''}` : version
}

// Semver precedence, enough to order versions within one line: numeric
// core, then prerelease identifiers (numeric before alphanumeric, a shorter
// prefix first). Only ever compares two versions of the same line.
function compareVersions(a, b) {
  const pa = parseVersion(a)
  const pb = parseVersion(b)
  if (pa === null || pb === null) return a.localeCompare(b)
  for (let i = 0; i < 3; i++) if (pa.core[i] !== pb.core[i]) return pa.core[i] - pb.core[i]
  if (!pa.pre.length || !pb.pre.length) return pb.pre.length - pa.pre.length
  const n = Math.max(pa.pre.length, pb.pre.length)
  for (let i = 0; i < n; i++) {
    const x = pa.pre[i]
    const y = pb.pre[i]
    if (x === undefined) return -1
    if (y === undefined) return 1
    const nx = /^\d+$/.test(x)
    const ny = /^\d+$/.test(y)
    if (nx && ny) {
      if (Number(x) !== Number(y)) return Number(x) - Number(y)
    } else if (nx !== ny) {
      return nx ? -1 : 1
    } else if (x !== y) {
      return x < y ? -1 : 1
    }
  }
  return 0
}

function parseVersion(version) {
  const m = /^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/.exec(version)
  if (!m) return null
  return { core: [Number(m[1]), Number(m[2]), Number(m[3])], pre: m[4] ? m[4].split('.') : [] }
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

function validatePopularNames(names, source) {
  if (!Array.isArray(names)) throw new Error(`popular-name source ${source} must be a JSON array`)
  if (names.length !== 100000) {
    throw new Error(`popular-name source must contain exactly 100000 names, found ${names.length}`)
  }
  for (const [index, name] of names.entries()) {
    if (typeof name !== 'string' || !name || /[\u0009-\u000d\u0020]/.test(name)) {
      throw new Error(`popular-name source contains an invalid name at index ${index}`)
    }
  }
  if (new Set(names).size !== names.length) {
    throw new Error('popular-name source must contain exactly 100000 unique names')
  }
  return names
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

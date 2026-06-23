#!/usr/bin/env node
// Fetch the download-weighting signals the primer's version selection needs.
//
// Two signals, two sources (see wiki/research/primer-historical-download-data.md
// for the full source survey):
//
//   (a) PACKAGE-level historical install frequency — which packages get
//       installed a lot, integrated over years. Source: the npm range API
//       `https://api.npmjs.org/downloads/range/{from}:{to}/{pkg}`, which serves
//       true per-day per-package counts back to 2015-01-10. Concurrency-friendly
//       and comma-bulkable (≤128 names/request), so this pass is fast. We sum a
//       trailing window (default 365 days) into one number per package and use
//       it to rank the NAME list (which packages earn a primer slot).
//
//   (b) VERSION-level install frequency — which specific versions of those
//       packages get installed, INCLUDING ancient high-download versions pinned
//       deep in dependency trees. Source: the per-version endpoint
//       `https://api.npmjs.org/versions/{pkg}/last-week`. This is the ONLY
//       per-version source that exists (no bulk dataset; see the research doc),
//       and it is `last-week`-only. That is sufficient for HISTORICAL depth
//       because the per-version download signal is TIME-INTEGRATED: a version
//       pinned in millions of trees keeps downloading for over a decade, so
//       this week's snapshot already contains the ancient survivors that real
//       lockfiles resolve to (empirically: lodash@3.10.1 — 10.9y old — still
//       1.88M dl/wk; express@4.17.1 — 7y — 1.96M dl/wk; react@17.0.2 — 5.2y —
//       3.6M dl/wk). The per-version endpoint is Cloudflare burst-limited, so
//       this pass is STRICTLY SEQUENTIAL at ~1.5s spacing with backoff.
//
// Output: a single JSON file
//   { generatedAt, window, packages: { "<name>": { downloads, versions: { "<ver>": <weekly> } } } }
// consumed by generate-primer.mjs to drive download-weighted version selection
// and name-list ranking. Run on a weekly cron OFF the release critical path;
// the release generator reads the committed snapshot.

import { spawnSync } from 'node:child_process'
import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { dirname, resolve } from 'node:path'

const args = new Map()
for (let i = 2; i < process.argv.length; i++) {
  const arg = process.argv[i]
  if (!arg.startsWith('--')) throw new Error(`unexpected argument: ${arg}`)
  const [key, inline] = arg.slice(2).split('=', 2)
  args.set(key, inline ?? process.argv[++i])
}

const top = Number(args.get('top') ?? 2000)
const windowDays = Number(args.get('window-days') ?? 365)
const spacingMs = Number(args.get('spacing-ms') ?? 1500)
const out = resolve(args.get('out') ?? 'crates/aube-resolver/data/download-weights.json')
const namesFile = args.get('names')
const namesUrl = args.get('names-url') ?? 'https://raw.githubusercontent.com/jdx/aube-primer-packages/main/data/packages.json'
const skipVersions = args.has('skip-versions') // package-signal-only (fast) mode
const limit = args.has('limit') ? Number(args.get('limit')) : Infinity // for smoke tests

if (!Number.isInteger(top) || top < 1) throw new Error('--top must be a positive integer')

const names = namesFile
  ? parseNames(await readFile(namesFile, 'utf8'))
  : await fetchNames(namesUrl)
const selected = names.slice(0, Math.min(top, limit === Infinity ? top : limit))

// ── Signal (a): package-level historical totals, bulk + concurrent ──────────
const { from, to } = trailingWindow(windowDays)
console.error(`signal (a): package totals over ${from}..${to} for ${selected.length} names (bulk, concurrent)`)
const packageTotals = await fetchPackageTotals(selected, from, to)

// ── Signal (b): per-version last-week, strictly sequential ──────────────────
const packages = {}
if (skipVersions) {
  for (const name of selected) packages[name] = { downloads: packageTotals[name] ?? 0, versions: {} }
} else {
  console.error(`signal (b): per-version last-week for ${selected.length} names (sequential, ${spacingMs}ms spacing)`)
  let i = 0
  for (const name of selected) {
    i++
    const versions = await fetchVersionDownloads(name)
    if (i % 50 === 0 || i === selected.length) console.error(`  [${i}/${selected.length}] ${name}`)
    packages[name] = { downloads: packageTotals[name] ?? 0, versions: versions ?? {} }
    if (i < selected.length) await sleep(spacingMs)
  }
}

const payload = {
  generatedAt: new Date().toISOString(),
  window: { packageTotalsDays: windowDays, perVersionPeriod: 'last-week', from, to },
  packages,
}

await mkdir(dirname(out), { recursive: true })
const raw = Buffer.from(`${JSON.stringify(payload)}\n`)
if (out.endsWith('.json')) {
  await writeFile(out, raw)
} else if (out.endsWith('.zst')) {
  const zstd = spawnSync('zstd', ['-q', '-19', '-f', '-o', out], { input: raw, stdio: ['pipe', 'inherit', 'inherit'] })
  if (zstd.status !== 0) throw new Error('zstd compression failed')
} else {
  await writeFile(out, raw)
}
const withVersions = Object.values(packages).filter((p) => Object.keys(p.versions).length).length
console.error(`wrote ${selected.length} packages (${withVersions} with per-version data) to ${out}`)

// ── helpers ─────────────────────────────────────────────────────────────────

function trailingWindow(days) {
  // The range API caps a single request at 365 days inclusive and has no data
  // for "today" yet, so end at yesterday and clamp the span to 364 days.
  const span = Math.min(days, 364)
  const end = new Date(Date.now() - 86_400_000) // yesterday
  const start = new Date(end.getTime() - span * 86_400_000)
  const fmt = (d) => d.toISOString().slice(0, 10)
  return { from: fmt(start), to: fmt(end) }
}

// Bulk per-package totals. The range API takes ≤128 comma-separated names and is
// concurrency-tolerant, so chunk into 128s and fan out. We sum the daily series
// into one number per package (the trailing-window install frequency).
async function fetchPackageTotals(allNames, from, to) {
  const totals = {}
  // Scoped names (`@scope/name`) are NOT supported in bulk range lookups — npm
  // rejects the WHOLE chunk with an error. Bulk the unscoped names (≤128/req),
  // and fetch each scoped name as its own single-name request.
  const unscoped = allNames.filter((n) => !n.startsWith('@'))
  const scoped = allNames.filter((n) => n.startsWith('@'))
  const chunks = []
  for (let i = 0; i < unscoped.length; i += 128) chunks.push(unscoped.slice(i, i + 128))
  for (const name of scoped) chunks.push([name])
  // Cap concurrency to be polite; the range endpoint tolerates this fine.
  const POOL = 6
  let cursor = 0
  async function worker() {
    while (cursor < chunks.length) {
      const chunk = chunks[cursor++]
      const url = `https://api.npmjs.org/downloads/range/${from}:${to}/${chunk.map(encodePackageName).join(',')}`
      const { res, body } = await fetchJsonWithRetry(url)
      if (!res?.ok || !body) continue
      // A single-name request returns {package, downloads:[...]}; a multi-name
      // request returns {"<name>": {downloads:[...]}, ...} (null for unknown).
      const entries = chunk.length === 1 ? { [chunk[0]]: body } : body
      for (const [name, rec] of Object.entries(entries)) {
        if (!rec || !Array.isArray(rec.downloads)) continue
        totals[name] = rec.downloads.reduce((s, d) => s + (d.downloads || 0), 0)
      }
    }
  }
  await Promise.all(Array.from({ length: POOL }, worker))
  return totals
}

// Per-version last-week. Strictly one-at-a-time (caller serializes + spaces).
// Returns {version: weeklyDownloads} or null on failure (caller tolerates).
async function fetchVersionDownloads(name) {
  const url = `https://api.npmjs.org/versions/${encodePackageName(name)}/last-week`
  const { res, body } = await fetchJsonWithRetry(url, { backoff1015: true })
  if (!res?.ok || !body || typeof body.downloads !== 'object') return null
  return body.downloads
}

// fetch + JSON with retry. Retries transient socket resets, 5xx, 429. On a
// Cloudflare 429/1015 (the per-version endpoint's burst limiter), backs off 30s.
async function fetchJsonWithRetry(url, { attempts = 5, backoff1015 = false } = {}) {
  let delay = 1000
  for (let i = 1; i <= attempts; i++) {
    try {
      const res = await fetch(url, { headers: { accept: 'application/json' } })
      if (res.ok) return { res, body: await res.json() }
      if (res.status === 429 && backoff1015) {
        if (i === attempts) return { res }
        console.error(`  rate limited (429) on ${url} — backing off 30s`)
        await sleep(30_000)
        continue
      }
      if (res.status >= 400 && res.status < 500 && res.status !== 429) return { res }
      if (i === attempts) return { res }
    } catch (err) {
      if (i === attempts) return { res: null }
      console.error(`  retry ${i}: ${err.cause?.code ?? err.code ?? err.message}`)
    }
    await sleep(delay)
    delay *= 2
  }
  return { res: null }
}

async function fetchNames(url) {
  const { res, body } = await fetchTextWithRetry(url)
  if (!res?.ok) throw new Error(`${url}: HTTP ${res?.status ?? 'fetch failed'}`)
  return parseNames(body)
}

async function fetchTextWithRetry(url, attempts = 5) {
  let delay = 1000
  for (let i = 1; i <= attempts; i++) {
    try {
      const res = await fetch(url)
      if (res.ok) return { res, body: await res.text() }
      if (i === attempts) return { res }
    } catch (err) {
      if (i === attempts) return { res: null }
    }
    await sleep(delay)
    delay *= 2
  }
  return { res: null }
}

function parseNames(text) {
  const trimmed = text.trim()
  if (trimmed.startsWith('[')) return JSON.parse(trimmed)
  return trimmed
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter((l) => l && !l.startsWith('#'))
}

function encodePackageName(name) {
  return name.startsWith('@') ? name.replace('/', '%2F') : encodeURIComponent(name)
}

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms))
}

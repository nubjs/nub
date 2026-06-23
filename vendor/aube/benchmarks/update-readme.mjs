#!/usr/bin/env node
// Regenerate the "Fast installs" ratio paragraph in README.md from
// benchmarks/results.json. Invoked at the tail of `mise run bench:bump`
// so bumping benchmark data keeps the landing-page ratios in sync.

import { readFileSync, writeFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const repo = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const results = JSON.parse(readFileSync(`${repo}/benchmarks/results.json`, 'utf8'))

const byKey = Object.fromEntries(results.rows.map((r) => [r.key, r.values]))

function row(key) {
  const v = byKey[key]
  if (!v) throw new Error(`results.json missing row with key='${key}'`)
  return v
}

function ratio(key, tool, { approximate = false } = {}) {
  const speedup = row(key)[tool] / row(key).aube
  const label = speedup < 2 ? `${speedup.toFixed(1)}x` : `${Math.round(speedup)}x`
  return approximate && speedup < 2 ? `~${label}` : label
}

function about(label) {
  return label.startsWith('~') ? label : `about ${label}`
}

const defaultPnpm = ratio('gvs-warm', 'pnpm', { approximate: true })
const defaultBun = ratio('gvs-warm', 'bun', { approximate: true })
const testPnpm = ratio('install-test', 'pnpm')
const testBun = ratio('install-test', 'bun')

const paragraph = `**[Fast installs](https://aube.jdx.dev/benchmarks).** Warm installs are ${about(defaultPnpm)} faster than pnpm and ${about(defaultBun)} faster than Bun in the current benchmarks. Repeat test commands run up to ${testPnpm} faster than pnpm and up to ${testBun} faster than Bun.`

const START = '<!-- BENCH_RATIOS:START -->'
const END = '<!-- BENCH_RATIOS:END -->'
const readmePath = `${repo}/README.md`
const readme = readFileSync(readmePath, 'utf8')

const startIdx = readme.indexOf(START)
const endIdx = readme.indexOf(END, startIdx)
if (startIdx === -1 || endIdx === -1) {
  throw new Error(`README.md is missing ${START} ... ${END} markers`)
}

writeFileSync(readmePath, readme.slice(0, startIdx) + `${START}\n${paragraph}\n${END}` + readme.slice(endIdx + END.length))
console.log(`bench ratios: gvs-warm pnpm=${defaultPnpm} bun=${defaultBun} / install-test pnpm=${testPnpm} bun=${testBun}`)

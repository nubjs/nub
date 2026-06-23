#!/usr/bin/env node
import { readFileSync, writeFileSync } from 'node:fs'
import { resolve } from 'node:path'

const input = process.argv[2]
const outputFile = process.argv[3]

if (!input || !outputFile) {
  console.error('usage: node benchmarks/generate-phase-results.mjs <jsonl> <markdown>')
  process.exit(1)
}

const labels = {
  'gvs-warm': 'Fresh install (warm cache)',
  'gvs-cold': 'Fresh install (cold cache)',
}

const phaseOrder = [
  'root_preinstall',
  'resolve',
  'fetch',
  'prewarm_gvs',
  'catchup_fetch',
  'link',
  'inject',
  'link_bins',
  'dep_lifecycle',
  'root_lifecycle',
  'state',
  'sweep',
]

function fmt(ms) {
  if (ms == null) return ''
  return `${ms}ms`
}

const rows = readFileSync(input, 'utf8')
  .split('\n')
  .filter(Boolean)
  .map((line) => JSON.parse(line))

const byScenario = new Map()
for (const row of rows) {
  if (byScenario.has(row.scenario)) {
    console.warn(`Warning: duplicate scenario '${row.scenario}' - keeping last entry`)
  }
  byScenario.set(row.scenario, row)
}

const usedPhases = phaseOrder.filter((phase) =>
  rows.some((row) => Object.hasOwn(row.phases_ms ?? {}, phase)),
)

const lines = [
  '# Aube Install Phase Timings',
  '',
  '| Scenario | Total | Packages | Cached | Fetched | ' + usedPhases.join(' | ') + ' |',
  '|---|---|---|---|---|' + usedPhases.map(() => '---').join('|') + '|',
]

for (const [key, label] of Object.entries(labels)) {
  const row = byScenario.get(key)
  if (!row) continue
  const cells = [
    label,
    fmt(row.total_ms),
    String(row.packages),
    String(row.cached),
    String(row.fetched),
    ...usedPhases.map((phase) => fmt(row.phases_ms?.[phase])),
  ]
  lines.push(`| ${cells.join(' | ')} |`)
}

lines.push('')

const output = lines.join('\n')
writeFileSync(outputFile, output)
writeFileSync(
  outputFile.replace(/\.md$/, '.json'),
  JSON.stringify({ updated: new Date().toISOString(), unit: 'ms', rows }, null, 2) + '\n',
)

console.log(output)
console.log(`Wrote structured phase results to ${resolve(outputFile.replace(/\.md$/, '.json'))}`)

#!/usr/bin/env node
import { readFileSync } from 'node:fs'

const [beforePath, afterPath] = process.argv.slice(2)
if (!beforePath || !afterPath) {
  console.error('usage: summarize-results-change.mjs <before-results.json> <after-results.json>')
  process.exit(1)
}

const before = JSON.parse(readFileSync(beforePath, 'utf8'))
const after = JSON.parse(readFileSync(afterPath, 'utf8'))

const beforeRows = new Map(before.rows.map((row) => [row.key, row]))
const afterRows = new Map(after.rows.map((row) => [row.key, row]))
const tools = (after.managers || ['aube', 'bun', 'pnpm'])
  .filter((tool) => before.managers?.includes(tool) !== false)

function ms(value) {
  if (value == null) return 'n/a'
  return `${value}ms`
}

function pct(beforeValue, afterValue) {
  if (beforeValue == null || afterValue == null) return 'n/a'
  const change = ((afterValue - beforeValue) / beforeValue) * 100
  const sign = change > 0 ? '+' : ''
  return `${sign}${Math.round(change)}%`
}

function ratio(row, tool) {
  const values = row.values
  if (values[tool] == null || values.aube == null) return null
  const speedup = values[tool] / values.aube
  return speedup < 2 ? `${speedup.toFixed(1)}x` : `${Math.round(speedup)}x`
}

function ratioChange(key, tool) {
  const beforeRow = beforeRows.get(key)
  const afterRow = afterRows.get(key)
  if (!beforeRow || !afterRow) return null
  const beforeRatio = ratio(beforeRow, tool)
  const afterRatio = ratio(afterRow, tool)
  if (!beforeRatio || !afterRatio) return null
  return `${beforeRatio} -> ${afterRatio}`
}

const versionChanges = Object.keys(after.versions)
  .filter((name) => before.versions[name] !== after.versions[name])
  .map((name) => `- ${name}: ${before.versions[name] ?? '<unset>'} -> ${after.versions[name]}`)

const table = []
for (const row of after.rows) {
  const oldRow = beforeRows.get(row.key)
  if (!oldRow) {
    table.push(`| ${row.label} | ${tools.map((tool) => `new ${ms(row.values[tool])}`).join(' | ')} |`)
    continue
  }

  table.push(
    `| ${row.label} | ${tools
      .map((tool) => `${ms(oldRow.values[tool])} -> ${ms(row.values[tool])} (${pct(oldRow.values[tool], row.values[tool])})`)
      .join(' | ')} |`,
  )
}

console.log('## Benchmark changes')
console.log()
if (versionChanges.length > 0) {
  console.log('Versions:')
  console.log(versionChanges.join('\n'))
  console.log()
}
const ratioChanges = [
  ['warm installs vs Bun', ratioChange('gvs-warm', 'bun')],
  ['warm installs vs pnpm', ratioChange('gvs-warm', 'pnpm')],
  ['repeat test vs Bun', ratioChange('install-test', 'bun')],
  ['repeat test vs pnpm', ratioChange('install-test', 'pnpm')],
].filter(([, value]) => value)
if (ratioChanges.length > 0) {
  console.log(`Public ratios: ${ratioChanges.map(([label, value]) => `${label} ${value}`).join('; ')}.`)
  console.log()
}
console.log(`| Benchmark | ${tools.join(' | ')} |`)
console.log(`| --- | ${tools.map(() => '---:').join(' | ')} |`)
console.log(table.join('\n'))

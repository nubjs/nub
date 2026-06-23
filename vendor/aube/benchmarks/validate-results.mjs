#!/usr/bin/env node
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

const resultsPath = resolve(process.argv[2] ?? 'benchmarks/results.json')
const results = JSON.parse(readFileSync(resultsPath, 'utf8'))
const rows = new Map(results.rows.map((row) => [row.key, row]))

const claims = [
  ['gvs-warm', 'bun', 'landing page hero and README warm installs'],
  ['gvs-warm', 'pnpm', 'landing page hero and README warm installs'],
  ['install-test', 'bun', 'README repeat test commands'],
  ['install-test', 'pnpm', 'README repeat test commands'],
]

let failed = false

for (const [key, tool, surface] of claims) {
  const values = rows.get(key)?.values
  const aube = values?.aube
  const other = values?.[tool]

  if (!Number.isFinite(aube) || !Number.isFinite(other)) {
    console.error(`${key}/${tool}: missing benchmark values for ${surface}`)
    failed = true
    continue
  }

  const multiple = other / aube
  const displayed = Number(multiple.toFixed(1))
  if (displayed <= 1) {
    console.error(
      `${key}/${tool}: ${surface} would claim ${displayed.toFixed(1)}x faster, ` +
        `but aube=${aube}ms and ${tool}=${other}ms`,
    )
    failed = true
  }
}

if (failed) {
  process.exit(1)
}

console.log('benchmark claims validated')

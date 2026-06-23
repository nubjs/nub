// Reads the per-scenario hyperfine JSON output from `bench.sh` and
// emits two artifacts:
//
//   1. A human-readable markdown summary at `outputFile` (same format
//      the console prints).
//   2. A structured JSON file at `<outputFile without extension>.json`
//      so downstream consumers — notably `docs/benchmarks.data.ts`,
//      the VitePress data loader behind the `<BenchChart>` on the
//      benchmarks page — can ingest the results without parsing
//      markdown.
//
// Usage:
//   node generate-results.js <benchDir> <outputMarkdown>
// Optional env:
//   BENCH_TOOLS=aube,bun,pnpm,npm,yarn,deno,vlt
//                                   comma-separated tool order
//                                   (defaults to aube + pnpm)
//   RESULTS_JSON=<path>             override the JSON output path

const fs = require('fs')
const path = require('path')

const benchDir = process.argv[2]
const outputFile = process.argv[3]

const benchmarks = [
  ['gvs-warm', 'Fresh install (warm cache)'],
  ['gvs-cold', 'Fresh install (cold cache)'],
  ['install-test', 'npm install && npm run test'],
]
const SELECTED_BENCHMARKS = new Set(
  (process.env.BENCH_SCENARIOS || benchmarks.map(([name]) => name).join(','))
    .split(',')
    .map((s) => s.trim())
    .filter(Boolean),
)

const TOOLS = (process.env.BENCH_TOOLS || 'aube,pnpm')
  .split(',')
  .map((s) => s.trim())
  .filter(Boolean)

function readResult (benchDir, name, tool) {
  try {
    const data = JSON.parse(fs.readFileSync(`${benchDir}/${name}-${tool}.json`, 'utf8'))
    const r = data.results[0]
    if (!r || !Number.isFinite(r.mean)) {
      throw new Error('missing benchmark mean')
    }
    const stddev = Number.isFinite(r.stddev) ? r.stddev : 0
    return {
      text: `${r.mean.toFixed(3)}s ± ${stddev.toFixed(3)}s`,
      mean: r.mean,
      stddev,
      min: r.min,
      max: r.max,
    }
  } catch (err) {
    if (err && err.code !== 'ENOENT') {
      console.error(`Warning: failed to read ${name}-${tool}: ${err.message}`)
    }
    return { text: 'n/a', mean: null, stddev: null, min: null, max: null }
  }
}

function fmtSpeedup (baseMean, aubeMean) {
  if (baseMean == null || aubeMean == null) return ''
  if (aubeMean < baseMean) {
    return ` (${(baseMean / aubeMean).toFixed(1)}x faster)`
  } else if (aubeMean > baseMean) {
    return ` (${(aubeMean / baseMean).toFixed(1)}x slower)`
  }
  return ''
}

// -- Markdown ---------------------------------------------------------------
// Emits one row per scenario with a column per tool plus trailing
// "vs pnpm" and "vs bun" speedup columns when those tools are present
// in the run. pnpm is aube's drop-in-replacement target; bun is the
// other "fast" package manager users compare against.
const headerCells = ['#', 'Scenario', ...TOOLS]
if (TOOLS.includes('pnpm') && TOOLS.includes('aube')) {
  headerCells.push('vs pnpm')
}
if (TOOLS.includes('bun') && TOOLS.includes('aube')) {
  headerCells.push('vs bun')
}

const lines = [
  '# Benchmark Results',
  '',
  `| ${headerCells.join(' | ')} |`,
  `|${headerCells.map(() => '---').join('|')}|`,
]

// -- Structured JSON --------------------------------------------------------
// bench.sh writes BENCH_VERSIONS_FILE as a "<tool>\t<semver>" TSV so
// the docs chart can render the actual version each manager was
// running rather than just the bare name.
const versions = {}
const versionsFile = process.env.BENCH_VERSIONS_FILE
if (versionsFile && fs.existsSync(versionsFile)) {
  for (const line of fs.readFileSync(versionsFile, 'utf8').split('\n')) {
    const [name, version] = line.split('\t')
    if (name && version) versions[name] = version.trim()
  }
}

const json = {
  updated: new Date().toISOString(),
  unit: 'ms',
  managers: TOOLS,
  versions,
  rows: [],
}

benchmarks.filter(([name]) => SELECTED_BENCHMARKS.has(name)).forEach(([name, label], i) => {
  const results = {}
  for (const tool of TOOLS) {
    results[tool] = readResult(benchDir, name, tool)
  }

  const cells = [String(i + 1), label]
  for (const tool of TOOLS) {
    cells.push(results[tool].text)
  }
  if (TOOLS.includes('pnpm') && TOOLS.includes('aube')) {
    cells.push(fmtSpeedup(results.pnpm.mean, results.aube.mean).trim())
  }
  if (TOOLS.includes('bun') && TOOLS.includes('aube')) {
    cells.push(fmtSpeedup(results.bun.mean, results.aube.mean).trim())
  }
  lines.push(`| ${cells.join(' | ')} |`)

  const values = {}
  const stats = {}
  for (const tool of TOOLS) {
    values[tool] = results[tool].mean == null ? null : Math.round(results[tool].mean * 1000)
    stats[tool] = results[tool].mean == null ? null : results[tool]
  }

  json.rows.push({ key: name, label, values, stats })
})

lines.push('')

const output = lines.join('\n')
fs.writeFileSync(outputFile, output)
console.log(output)

const jsonOut = process.env.RESULTS_JSON
  || `${outputFile.replace(/\.md$/, '')}.json`
fs.writeFileSync(jsonOut, JSON.stringify(json, null, 2) + '\n')
console.log(`Wrote structured results to ${path.resolve(jsonOut)}`)

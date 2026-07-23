#!/usr/bin/env node
// Measure aube's opt-in transparent FS-compression of native addons
// (`AUBE_COMPRESS_STORE`, added in #985): the on-disk footprint AND the
// addon load time, compressed vs not.
//
// Size: for each package the script performs three fully isolated installs
// (own HOME, XDG dirs, cache, and store) — compression off, the default
// `**/*.node` gate, and the whole-store `glob:**/*` gate — then reports the
// store's logical vs physical bytes. Logical bytes must MATCH across modes
// (the compression is transparent: every reader sees identical bytes);
// physical bytes show the savings.
//
// Load time: every compressed addon is `require()`d in a fresh Node process,
// compressed vs uncompressed, in two scenarios: FIRST load of a fresh file
// (what an install pays once — dominated by the OS validating a
// freshly-created binary, for both variants) and STEADY-STATE loads of the
// same inode (what every later process pays). Fresh files are minted with
// copy-on-write clones (`cp -c` on macOS, COPYFILE_FICLONE_FORCE reflinks on
// Linux): a clone preserves the filesystem-compression attribute where a
// plain copy would silently write the file back out uncompressed and
// invalidate the comparison.
//
// Prerequisites:
//   - aube built in release mode: cargo build --release
//   - a filesystem with transparent compression (APFS on macOS, btrfs on
//     Linux, NTFS on Windows). On anything else the feature fails soft to
//     plain writes and off/on report the same physical size.
//
// Usage:
//   node benchmarks/fs-compress-size.mts
//
// Environment variables:
//   PACKAGES — space-separated npm package names to measure
//              (default: "@rspack/core vite" — @rspack/core ships the ~40MB
//              @rspack/binding addon; vite 8.x ships the rolldown +
//              lightningcss native addons)
//   AUBE_BIN — aube binary to exercise (default: target/release/aube)
//   KEEP=1   — keep the scratch dir for inspection instead of deleting it

import { spawnSync } from 'node:child_process'
import {
  constants as fsConstants,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

interface FileSize {
  full: string
  rel: string
  logical: number
  physical: number
}

interface InstallResult {
  files: FileSize[]
  logicalBytes: number
  logicalKb: number
  physicalKb: number
}

interface VariantTiming {
  cold: number
  warm: number
  evicted: boolean
}

interface LoadResult {
  package: string
  addon: string
  onDiskKb: number
  logicalKb: number
  coldOnMs: number
  coldOffMs: number
  warmOnMs: number
  warmOffMs: number
}

const here = path.dirname(fileURLToPath(import.meta.url))
const packages = (process.env.PACKAGES ?? '@rspack/core vite')
  .split(/\s+/)
  .filter(Boolean)
const aubeBin = path.resolve(
  process.env.AUBE_BIN ?? path.join(here, '..', 'target', 'release', 'aube'),
)

if (!existsSync(aubeBin)) {
  console.error(
    `error: aube binary not found at ${aubeBin} — run 'cargo build --release' first`,
  )
  process.exit(1)
}

const scratch = mkdtempSync(path.join(tmpdir(), 'aube-fs-compress-size-'))
process.on('exit', () => {
  if (process.env.KEEP === '1') {
    console.log(`scratch kept at ${scratch}`)
  } else {
    rmSync(scratch, { recursive: true, force: true })
  }
})

const kb = (bytes: number): number => Math.floor(bytes / 1024)

// Walk a tree collecting every file's path plus logical (apparent) and
// physical (allocated) size. `blocks` is always 512-byte units in Node, so
// this is portable across APFS / btrfs / NTFS.
function walkSizes(dir: string, root: string = dir, files: FileSize[] = []): FileSize[] {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name)
    if (entry.isDirectory()) {
      walkSizes(full, root, files)
    } else if (entry.isFile()) {
      const st = statSync(full)
      files.push({
        full,
        rel: path.relative(root, full),
        logical: st.size,
        physical: st.blocks * 512,
      })
    }
  }
  return files
}

const median = (values: number[]): number => {
  const sorted = [...values].sort((a, b) => a - b)
  const mid = sorted.length >> 1
  // Even-length: average the two middle samples; odd-length: the middle one.
  return sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2
}

// Clone (copy-on-write) a file, preserving the filesystem-compression
// attribute — a plain copy would silently write the data back out
// uncompressed. On macOS Node's COPYFILE_FICLONE_FORCE can fail with ENOSYS
// for decmpfs-compressed sources, so shell out to `cp -c` (raw clonefile);
// on Linux FICLONE_FORCE is the btrfs reflink path.
function cloneFile(src: string, dest: string): void {
  if (process.platform === 'darwin') {
    const result = spawnSync('cp', ['-c', src, dest])
    if (result.status !== 0) {
      throw new Error(`clonefile failed: cp -c ${src} ${dest}`)
    }
    return
  }
  try {
    copyFileSync(src, dest, fsConstants.COPYFILE_FICLONE_FORCE)
  } catch (e) {
    // FICLONE_FORCE throws (ENOTSUP/EINVAL) on filesystems without reflink
    // support (ext4, most NFS, overlayfs). A plain copy would silently strip
    // the compression attribute and skew the timings, so fail loud with why.
    throw new Error(
      `clonefile (reflink) failed for ${src}: ${e.message}\n` +
        'This benchmark needs a reflink + transparent-compression filesystem ' +
        '(APFS on macOS, btrfs on Linux); ext4/NFS/overlayfs are not supported.',
    )
  }
}

// Time one `require()` of the addon in a fresh Node process. Returns
// milliseconds, or undefined for an addon that can't load standalone.
function loadMs(file: string): number | undefined {
  // Force the child to exit after timing: a native addon that keeps the event
  // loop alive (thread pools, timers — common in napi-rs addons like
  // @rspack/binding) would otherwise never exit and hang spawnSync forever.
  // writeSync(1, …) flushes synchronously before the exit, so the value can't
  // be lost the way an async stdout write + process.exit() can. The timeout is
  // a backstop for anything that still wedges.
  const probe =
    'const s = process.hrtime.bigint();' +
    `require(${JSON.stringify(file)});` +
    'require("node:fs").writeSync(1, String(Number(process.hrtime.bigint() - s) / 1e6));' +
    'process.exit(0)'
  const result = spawnSync(process.execPath, ['-e', probe], {
    encoding: 'utf8',
    timeout: 30_000,
  })
  return result.status === 0 ? Number(result.stdout) : undefined
}

// How many clean-room (cold) first-load samples to take per variant.
const COLD_SAMPLES = Math.max(1, Number(process.env.SAMPLES) || 5)

// Evict the OS page cache so the NEXT read is genuinely cold. A fresh clone
// alone is not enough — clonefile shares blocks with the source, which is
// already warm, so a "first load" would really time a warm cache. macOS: sync
// then `purge`. purge needs privileges; if it is denied the sample is merely
// warm (not wrong), and the caller reports that. Returns true only when a real
// eviction happened.
function dropCaches(): boolean {
  if (process.platform !== 'darwin') {
    return false
  }
  spawnSync('sync')
  return spawnSync('purge').status === 0
}

// Cold first-load vs warm second-load (steady-state) timings for one on-disk
// variant of an addon.
// Cold (clean room): a FRESH clone AND an evicted page cache before each of
// COLD_SAMPLES samples, so every sample is a real first touch, not a warm
// re-read of shared blocks; median of the samples. `evicted` is false if purge
// was denied (the samples were warm, not truly cold).
// Warm (second load): one clone loaded x7 in fresh processes with the cache
// warm, first (still-cold) sample dropped.
function timeVariant(src: string, dir: string, tag: string): VariantTiming | undefined {
  const cold: number[] = []
  let evicted = true
  for (let i = 0; i < COLD_SAMPLES; i += 1) {
    const clone = path.join(dir, `${tag}-cold-${i}.node`)
    cloneFile(src, clone)
    if (!dropCaches()) {
      evicted = false
    }
    const ms = loadMs(clone)
    rmSync(clone)
    if (ms === undefined) {
      return undefined
    }
    cold.push(ms)
  }
  const warmClone = path.join(dir, `${tag}-warm.node`)
  cloneFile(src, warmClone)
  const warm: number[] = []
  for (let i = 0; i < 7; i += 1) {
    const ms = loadMs(warmClone)
    if (ms === undefined) {
      rmSync(warmClone)
      return undefined
    }
    warm.push(ms)
  }
  rmSync(warmClone)
  return { cold: median(cold), warm: median(warm.slice(1)), evicted }
}

// One isolated install. `gate` is the AUBE_COMPRESS_STORE value ('' = off).
// Returns the store's summed sizes.
function runInstall(name: string, pkg: string, gate: string): InstallResult {
  const home = path.join(scratch, name)
  const project = path.join(home, 'project')
  mkdirSync(project, { recursive: true })
  writeFileSync(
    path.join(project, 'package.json'),
    JSON.stringify({
      name: 'probe',
      private: true,
      dependencies: { [pkg]: 'latest' },
    }),
  )
  // A minimal env so nothing leaks in from the host (a global aube config or
  // an inherited AUBE_COMPRESS_STORE would skew the comparison).
  // npm_config_minimum_release_age=0: measure the true latest publish.
  const result = spawnSync(aubeBin, ['install'], {
    cwd: project,
    env: {
      PATH: process.env.PATH,
      HOME: home,
      XDG_DATA_HOME: path.join(home, '.local', 'share'),
      XDG_CACHE_HOME: path.join(home, '.cache'),
      npm_config_minimum_release_age: '0',
      ...(gate ? { AUBE_COMPRESS_STORE: gate } : {}),
    },
    stdio: 'ignore',
  })
  if (result.status !== 0) {
    console.error(`error: '${aubeBin} install' failed for ${name}`)
    process.exit(1)
  }
  const store = path.join(home, '.local', 'share', 'aube', 'store')
  // Measure the CAS payload only (hash-addressed content, byte-stable across
  // runs) — the store also carries small index/metadata files whose bytes
  // legitimately vary run to run and would fail the transparency assert.
  const cas = path.join(store, 'v1', 'files')
  const files = walkSizes(existsSync(cas) ? cas : store)
  const logicalBytes = files.reduce((s: number, f) => s + f.logical, 0)
  return {
    files,
    logicalBytes,
    logicalKb: kb(logicalBytes),
    physicalKb: kb(files.reduce((s: number, f) => s + f.physical, 0)),
  }
}

console.log(
  'aube store FS-compression size comparison (AUBE_COMPRESS_STORE)\n' +
    `binary: ${aubeBin}\n`,
)

// The three modes measured per package: gate value, scratch key, label.
const MODES: Array<[string, string, string]> = [
  ['', 'off', 'compression off       '],
  ['1', 'node-gate', 'default gate (*.node) '],
  ['glob:**/*', 'all-gate', "gate 'glob:**/*'      "],
]

const loadDir = path.join(scratch, 'load')
mkdirSync(loadDir, { recursive: true })

// Per-addon load-timing rows, emitted as JSON at the end for the charts.
const loadResults: LoadResult[] = []
// Cleared if purge is ever denied, so the methodology note can say so.
let coldEvicted = true

for (const pkg of packages) {
  const safe = pkg.replace(/[^a-zA-Z0-9]/g, '-')
  const pad = (n: number): string => String(n).padStart(8)
  console.log(`── ${pkg} (latest) ──────────────────────────────────────────`)

  const runs: Record<string, InstallResult> = {}
  for (const [gate, key, label] of MODES) {
    const run = runInstall(`${safe}-${key}`, pkg, gate)
    runs[gate] = run
    const off = runs['']
    if (gate === '') {
      console.log(
        `  ${label}  ${pad(run.logicalKb)}KB logical  ${pad(run.physicalKb)}KB on disk`,
      )
      continue
    }
    if (run.logicalBytes !== off.logicalBytes) {
      console.error(
        '  ERROR: logical sizes differ across modes (raw bytes:\n' +
          `  ${run.logicalBytes} vs ${off.logicalBytes}) — compression must be\n` +
          '  byte-transparent; investigate before trusting these numbers.',
      )
      process.exit(1)
    }
    const savedPct = 100 - Math.floor((run.physicalKb * 100) / off.physicalKb)
    console.log(
      `  ${label}  ${pad(run.logicalKb)}KB logical  ${pad(run.physicalKb)}KB on disk  (−${savedPct}%)`,
    )
  }

  // Load-time comparison for every addon the default gate compressed. The
  // store is content-addressed, so the SAME rel path exists in the off store
  // with identical bytes — that copy is the uncompressed variant.
  console.log('  compressed addons — size and load time (compressed vs not):')
  const compressed = runs['1'].files.filter(
    f => f.logical > 1024 * 1024 && f.physical < f.logical * 0.95,
  )
  for (const f of compressed) {
    const plain = runs[''].files.find(p => p.rel === f.rel)
    const pct = 100 - Math.floor((f.physical * 100) / f.logical)
    const on = timeVariant(f.full, loadDir, 'on')
    const noff = plain && timeVariant(plain.full, loadDir, 'off')
    let timing = 'does not load standalone — timing skipped'
    if (on && noff) {
      if (!on.evicted || !noff.evicted) {
        coldEvicted = false
      }
      timing =
        `cold ${on.cold.toFixed(0)}ms vs ${noff.cold.toFixed(0)}ms, ` +
        `warm ${on.warm.toFixed(1)}ms vs ${noff.warm.toFixed(1)}ms`
      loadResults.push({
        package: pkg,
        addon: path.basename(f.rel),
        onDiskKb: kb(f.physical),
        logicalKb: kb(f.logical),
        coldOnMs: on.cold,
        coldOffMs: noff.cold,
        warmOnMs: on.warm,
        warmOffMs: noff.warm,
      })
    }
    console.log(
      `    ${pad(kb(f.logical))}KB → ${pad(kb(f.physical))}KB (−${pct}%)  ${timing}`,
    )
  }
  console.log('')
}

console.log(
  `cold = clean-room first load: a fresh clonefile + the page cache evicted\n` +
    `(purge) before each of ${COLD_SAMPLES} samples, so every sample is a real\n` +
    'first touch; median. warm = second load onward: same inode, fresh process\n' +
    'per load, median of 6 (cache warm).' +
    (coldEvicted
      ? ''
      : '\nWARNING: purge was denied, so the cold samples were NOT truly cold —\n' +
        'run with purge access (e.g. sudo) for real clean-room first-load numbers.'),
)

// Machine-readable rows for the load-perf charts (first-load + second-load).
console.log(`\nLOAD_RESULTS_JSON ${JSON.stringify(loadResults)}`)

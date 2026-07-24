#!/usr/bin/env node
/**
 * @file Soaked dependency updater — every ecosystem bumps through the same
 *   cooldown:
 *
 *   - npm: taze (maturityPeriod = SOAK_DAYS via the taze config next to the
 *     package.json) rewrites ranges, then the repo's own installer refreshes
 *     the lockfile.
 *   - cargo: `cargo +nightly update`, where `.cargo/config.toml`
 *     min-publish-age enforces the same window (too-new crate versions are
 *     skipped unless already locked). The nightly is requested per-invocation
 *     rather than pinned repo-wide — see .cargo/config.toml for why.
 *
 *   Usage: node scripts/soak/update-deps.mts [--npm|--cargo] [--dry-run]
 *   (no ecosystem flag = both)
 */

import { spawnSync } from 'node:child_process'
import { existsSync, realpathSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import { pathToFileURL } from 'node:url'

import { NPM_INSTALLERS, NPM_PKG_DIR, REPO_ROOT, RUSTUP_CARGO } from './paths.mts'

function run(cmd: string, args: string[], cwd: string): number {
  console.log(`[update-deps] ${cmd} ${args.join(' ')} (in ${path.relative(REPO_ROOT, cwd) || '.'})`)
  const res = spawnSync(cmd, args, { cwd, stdio: 'inherit' })
  if (res.error) {
    console.error(`[update-deps] ${cmd}: ${res.error.message}`)
  }
  return res.status ?? 1
}

function updateNpm(dryRun: boolean): number {
  const taze = path.join(NPM_PKG_DIR, 'node_modules/.bin/taze')
  if (!existsSync(taze)) {
    console.error(`[update-deps] taze not installed — run the installer in ${NPM_PKG_DIR} first`)
    return 1
  }
  // The taze config sets `write: true`; a dry run must override it
  // explicitly or "dry" would still rewrite package.json.
  const args = dryRun ? ['--no-write', '--no-install'] : ['--write']
  const status = run(taze, args, NPM_PKG_DIR)
  if (status !== 0 || dryRun) {
    return status
  }
  for (const [cmd, ...args] of NPM_INSTALLERS) {
    if (cmd!.includes('/') && !existsSync(cmd!)) {
      continue
    }
    return run(cmd!, args, NPM_PKG_DIR)
  }
  console.error('[update-deps] no installer found — refresh the lockfile manually')
  return 1
}

function updateCargo(dryRun: boolean): number {
  // The min-publish-age soak is an [unstable] cargo feature, honored only
  // under a nightly cargo. The repo pins no nightly toolchain (that would
  // outrank `rustup default` and silently redirect the version-pinned CI
  // jobs and the release build), so the nightly is requested explicitly
  // HERE — dependency resolution is the only step that picks versions, so
  // it is the only step that needs the soak. `+nightly` is rustup shim
  // syntax; a non-rustup cargo (e.g. Homebrew stable) would silently
  // update WITHOUT the soak, so refuse that rather than bypass it.
  if (!existsSync(RUSTUP_CARGO)) {
    console.error('[update-deps] rustup cargo shim not found — cargo update would bypass the min-publish-age soak')
    return 1
  }
  const args = dryRun ? ['+nightly', 'update', '--dry-run'] : ['+nightly', 'update']
  return run(RUSTUP_CARGO, args, REPO_ROOT)
}

// No flag = both; naming both explicitly also means both — a naive
// "flag present = only that one" reading once made `--npm --cargo` run
// NEITHER, so this rule lives in one exported, regression-tested place.
export function selectEcosystems(argv: string[]): { npm: boolean; cargo: boolean } {
  const npmFlag = argv.includes('--npm')
  const cargoFlag = argv.includes('--cargo')
  return { npm: npmFlag || !cargoFlag, cargo: cargoFlag || !npmFlag }
}

function main(argv: string[] = process.argv.slice(2)): number {
  const dryRun = argv.includes('--dry-run')
  const { npm, cargo } = selectEcosystems(argv)
  // Run every requested ecosystem even if an earlier one fails, then
  // aggregate, so one broken ecosystem can't hide the other's drift.
  const npmStatus = npm ? updateNpm(dryRun) : 0
  const cargoStatus = cargo ? updateCargo(dryRun) : 0
  return npmStatus || cargoStatus
}

// realpath + pathToFileURL so symlinked checkouts and paths needing URL
// encoding still register as the entrypoint (ESM realpaths import.meta.url).
const isMain =
  process.argv[1] && pathToFileURL(realpathSync(process.argv[1])).href === import.meta.url
if (isMain) {
  process.exitCode = main()
}

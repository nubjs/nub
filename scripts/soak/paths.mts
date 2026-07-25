/**
 * @file 1 path, 1 reference — every filesystem location the soak +
 *   external-tools scripts touch is declared here exactly once. Scripts
 *   import from this module instead of re-deriving paths, so a surface can
 *   move (or differ between repos carrying these scripts) with a one-line
 *   change.
 */

import os from 'node:os'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

export const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..')

// Soak surfaces (repo-relative). The repo ROOT is npm-only (.npmrc,
// package-lock.json — wpt-worker runs a root npm ci); the pnpm-side soak
// (workspace yaml with catalog + minimumReleaseAge, taze) is anchored in
// tools/ so the workspace yaml never marks the repo root as a workspace —
// nub would inherit the root engines and redirect the test matrix's
// older-Node legs (see .npmrc for the full story).
export const SURFACES = {
  cargoConfig: '.cargo/config.toml',
  npmrc: '.npmrc',
  workspaceYaml: 'tools/pnpm-workspace.yaml',
  tazeConfig: 'tools/taze.config.mts',
  toolchainToml: 'rust-toolchain.toml',
  renovateJson: '.github/renovate.json',
}

// The directory holding the npm package the soak governs (taze runs here,
// the repo's installer refreshes this package's lockfile).
export const NPM_PKG_DIR = path.join(REPO_ROOT, 'tools')

// Lockfile refreshers tried in order after taze rewrites package.json.
export const NPM_INSTALLERS: string[][] = [['pnpm', 'install']]

// rustup's cargo shim — the only cargo that understands `+nightly`, and so
// the only one whose `cargo update` can honor the [unstable]
// min-publish-age soak (see .cargo/config.toml).
export const RUSTUP_CARGO = path.join(os.homedir(), '.cargo/bin/cargo')

// Pinned external tool manifest + the local tool rack it installs into:
// exact versions under rack/<tool>/<version>/, flat PATH handles in bin/.
export const EXTERNAL_TOOLS_JSON = path.join(REPO_ROOT, 'external-tools.json')

// CI agent image that pre-bakes the pinned toolchain + sfw (null when the
// repo has no such image — nub's docker/ images are product artifacts that
// install released nub, not dev tooling).
export const DOCKER_PREBAKE: string | null = null

// .dockerignore managed by `untracked` (docker-smoke + the from-source
// image COPY the repo, so the managed exclusions shape their contexts).
export const DOCKERIGNORE: string | null = '.dockerignore'

const XDG_DATA_HOME = process.env.XDG_DATA_HOME || path.join(os.homedir(), '.local/share')
export const DEV_TOOLS_DIR = path.join(XDG_DATA_HOME, 'nub/dev-tools')
export const RACK_DIR = path.join(DEV_TOOLS_DIR, 'rack')
export const BIN_DIR = path.join(DEV_TOOLS_DIR, 'bin')


// Candidates (tried in order) for installing an extracted external tool's
// runtime deps — the repo's own package manager first, of course.
export const PM_DEP_INSTALLERS: string[][] = [
  [path.join(REPO_ROOT, 'target/debug/nub'), 'install', '--prod'],
  ['nub', 'install', '--prod'],
  ['pnpm', 'install', '--prod', '--ignore-scripts'],
  ['npm', 'install', '--omit=dev', '--ignore-scripts', '--no-audit', '--no-fund'],
]

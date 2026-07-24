#!/usr/bin/env node
/**
 * @file Soak manager — parity gate + fixer for the release-age cooldown.
 *   The window is ONE value (`SOAK_DAYS` in ./constants.mts); this script
 *   asserts every data surface matches it and that soak exclusions carry a
 *   valid, unexpired `# published: | removable:` annotation:
 *
 *   - `.cargo/config.toml`        `global-min-publish-age` + `[unstable] min-publish-age`
 *   - `tools/pnpm-workspace.yaml`  `minimumReleaseAge` (minutes) + annotated excludes
 *   - `.npmrc`               `min-release-age` (days)
 *   - `tools/taze.config.mts`      imports SOAK_DAYS (existence + import check)
 *   - `.github/renovate.json`     `minimumReleaseAge` ("N days", explicit — not preset-inherited)
 *
 *   `--check` (default) fails loud with What / Saw / Wanted / Fix on drift.
 *   `--fix` rewrites window values in place and prunes excludes whose
 *   `removable` date has passed (a cleared pin is dead weight — pruning it
 *   re-arms the soak for the next publish of that package).
 *
 *   Usage: node scripts/soak/soak.mts [--check|--fix] [--quiet]
 */

import { existsSync, readFileSync, realpathSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import { pathToFileURL } from 'node:url'

import {
  ANNOTATION_RE,
  SOAK_DAYS,
  SOAK_MINUTES,
  VERSION_PIN_RE,
  addDaysIso,
  isValidIsoDate,
  todayIso,
} from './constants.mts'
import { REPO_ROOT, SURFACES } from './paths.mts'

export interface Finding {
  file: string
  what: string
  saw: string
  wanted: string
  fix: string
}

export function checkCargoConfig(body: string, file: string): Finding[] {
  const out: Finding[] = []
  const age = /^global-min-publish-age\s*=\s*"([^"]*)"/m.exec(body)?.[1]
  const wanted = `${SOAK_DAYS} days`
  if (age !== wanted) {
    out.push({
      file,
      what: 'cargo min-publish-age window',
      saw: age ?? '(missing)',
      wanted,
      fix: `set [registry] global-min-publish-age = "${wanted}" (or run --fix)`,
    })
  }
  if (!/^\[unstable\][^[]*^min-publish-age\s*=\s*true/ms.test(body)) {
    out.push({
      file,
      what: 'cargo unstable feature gate',
      saw: '[unstable] min-publish-age missing or false',
      wanted: 'min-publish-age = true under [unstable]',
      fix: 'add `[unstable]\\nmin-publish-age = true` (nightly-only; the pinned toolchain provides it)',
    })
  }
  return out
}

export function checkNpmrc(body: string, file: string): Finding[] {
  const days = /^min-release-age=(\d+)\s*$/m.exec(body)?.[1]
  if (Number(days) === SOAK_DAYS) {
    return []
  }
  return [
    {
      file,
      what: 'npm min-release-age window',
      saw: days ?? '(missing)',
      wanted: String(SOAK_DAYS),
      fix: `set min-release-age=${SOAK_DAYS} (or run --fix)`,
    },
  ]
}

export function checkWorkspaceYaml(body: string, file: string): Finding[] {
  const out: Finding[] = []
  const minutes = /^minimumReleaseAge:\s*(\d+)\s*$/m.exec(body)?.[1]
  if (Number(minutes) !== SOAK_MINUTES) {
    out.push({
      file,
      what: 'minimumReleaseAge window',
      saw: minutes ?? '(missing)',
      wanted: `${SOAK_MINUTES} (SOAK_DAYS ${SOAK_DAYS} x 1440 minutes)`,
      fix: `set minimumReleaseAge: ${SOAK_MINUTES} (or run --fix)`,
    })
  }
  out.push(...checkExcludeAnnotations(body, file))
  return out
}

/**
 * Every version-pinned `minimumReleaseAgeExclude` entry must carry, on the
 * line directly above, `# published: YYYY-MM-DD | removable: YYYY-MM-DD`
 * with `removable = published + SOAK_DAYS`, and must be pruned once
 * `removable` is strictly in the past. Bare names and `@scope/*` globs are
 * standing trust, not dated bypasses — no annotation required.
 */
export function checkExcludeAnnotations(body: string, file: string): Finding[] {
  const out: Finding[] = []
  const today = todayIso()
  // Flow style would be invisible to the block parser below — an
  // unvalidated, never-expiring bypass. One canonical shape only.
  if (/^minimumReleaseAgeExclude:\s*\[/m.test(body)) {
    out.push({
      file,
      what: 'minimumReleaseAgeExclude flow style',
      saw: 'inline [...] list',
      wanted: 'a block list (one annotated `- entry` per line)',
      fix: 'rewrite as a block list so every pin can carry its annotation',
    })
    return out
  }
  for (const entry of parseExcludeEntries(body)) {
    if (!VERSION_PIN_RE.test(entry.name)) {
      continue
    }
    if (!entry.annotation) {
      out.push({
        file,
        what: `soak exclude '${entry.name}' annotation`,
        saw: '(no annotation on the line above)',
        wanted: `# published: YYYY-MM-DD | removable: <published + ${SOAK_DAYS}d>`,
        fix: `annotate the pin with its real registry publish date`,
      })
      continue
    }
    const { published, removable } = entry.annotation
    if (!isValidIsoDate(published) || !isValidIsoDate(removable)) {
      out.push({
        file,
        what: `soak exclude '${entry.name}' annotation dates`,
        saw: `${published} | ${removable}`,
        wanted: 'real YYYY-MM-DD calendar dates',
        fix: 'correct the annotation to the real registry publish date',
      })
      continue
    }
    const expected = addDaysIso(published, SOAK_DAYS)
    if (removable !== expected) {
      out.push({
        file,
        what: `soak exclude '${entry.name}' removable date`,
        saw: removable,
        wanted: `${expected} (published ${published} + ${SOAK_DAYS} days)`,
        fix: 'correct the removable date',
      })
    }
    if (removable < today) {
      out.push({
        file,
        what: `soak exclude '${entry.name}' expired`,
        saw: `removable ${removable} < today ${today}`,
        wanted: 'entry pruned once its window has passed',
        fix: 'delete the pin + its annotation (or run --fix)',
      })
    }
  }
  return out
}

interface ExcludeEntry {
  name: string
  line: number
  annotation?: { published: string; removable: string }
}

export function parseExcludeEntries(body: string): ExcludeEntry[] {
  const lines = body.split('\n')
  const out: ExcludeEntry[] = []
  let inBlock = false
  let blockIndent = 0
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]!
    if (/^minimumReleaseAgeExclude:\s*$/.test(line)) {
      inBlock = true
      blockIndent = -1
      continue
    }
    if (!inBlock) {
      continue
    }
    const item = /^(\s+)-\s*['"]?([^'"#\s]+)['"]?\s*(?:#.*)?$/.exec(line)
    if (!item) {
      // Comments stay inside the block; anything else at column 0 ends it.
      if (/^\S/.test(line)) {
        inBlock = false
      }
      continue
    }
    if (blockIndent === -1) {
      blockIndent = item[1]!.length
    }
    if (item[1]!.length !== blockIndent) {
      continue
    }
    const prev = lines[i - 1]?.trim() ?? ''
    const ann = ANNOTATION_RE.exec(prev)
    out.push({
      name: item[2]!,
      line: i + 1,
      ...(ann ? { annotation: { published: ann[1]!, removable: ann[2]! } } : {}),
    })
  }
  return out
}

/**
 * Catalog-shadowed pins stay in lockstep: when a package.json next to the
 * workspace yaml pins a cataloged package to an exact version instead of
 * `catalog:` (npm-cli compat — npm can't parse the protocol), the two
 * versions must match. `catalog:` references no-op here.
 */
export function checkCatalogParity(
  yamlBody: string,
  pkgJson: string,
  yamlFile: string,
): Finding[] {
  const out: Finding[] = []
  const catalog: Record<string, string> = {}
  const block = /^catalog:\s*\n((?:[ \t]+\S.*\n?|\s*\n)*)/m.exec(yamlBody)?.[1] ?? ''
  for (const m of block.matchAll(/^[ \t]+['"]?([^'":\s]+)['"]?:\s*['"]?([^'"\s]+)['"]?\s*$/gm)) {
    catalog[m[1]!] = m[2]!
  }
  const pkg = JSON.parse(pkgJson)
  const declared: Record<string, string> = {
    ...pkg.dependencies,
    ...pkg.devDependencies,
  }
  for (const [name, version] of Object.entries(catalog)) {
    const spec = declared[name]
    if (spec === undefined || spec === 'catalog:' || spec === version) {
      continue
    }
    out.push({
      file: yamlFile,
      what: `catalog-shadowed pin '${name}' out of lockstep`,
      saw: `catalog ${version} vs package.json ${spec}`,
      wanted: 'identical versions (or a catalog: reference)',
      fix: `bump both together — the catalog entry is the reference`,
    })
  }
  return out
}

export function checkTazeConfig(body: string, file: string): Finding[] {
  const out: Finding[] = []
  if (!body.includes('maturityPeriod')) {
    out.push({
      file,
      what: 'taze maturityPeriod',
      saw: '(not set)',
      wanted: 'maturityPeriod: SOAK_DAYS',
      fix: 'set maturityPeriod: SOAK_DAYS in the taze config',
    })
  }
  if (!body.includes('constants.mts')) {
    out.push({
      file,
      what: 'taze config soak import',
      saw: 'window not imported from scripts/soak/constants.mts',
      wanted: "import { SOAK_DAYS } from '<rel>/scripts/soak/constants.mts'",
      fix: 'import SOAK_DAYS instead of hand-copying the number',
    })
  }
  return out
}

/**
 * Renovate must carry the window EXPLICITLY in this repo's renovate.json.
 * Renovate bumps manifests + lockfiles server-side, and cargo's
 * min-publish-age skips already-locked versions — so a Renovate PR is the
 * one dependency path none of the local soak surfaces can stop. An
 * inherited preset value (extends:) doesn't count: presets change without
 * a commit here, which is exactly the silent drift this gate exists to
 * catch.
 */
export function checkRenovateConfig(body: string, file: string): Finding[] {
  let config: Record<string, unknown>
  try {
    config = JSON.parse(body)
  } catch {
    return [
      {
        file,
        what: 'renovate config parse',
        saw: '(invalid JSON)',
        wanted: 'parseable JSON carrying the soak window',
        fix: 'repair the JSON, then set minimumReleaseAge (or run --fix)',
      },
    ]
  }
  const wanted = `${SOAK_DAYS} days`
  const saw = config['minimumReleaseAge']
  if (SOAK_DAYS === 0 ? saw === undefined : saw === wanted) {
    return []
  }
  return [
    {
      file,
      what: 'renovate minimumReleaseAge window',
      saw: saw === undefined ? '(missing — an extends: preset does not count)' : String(saw),
      wanted: SOAK_DAYS === 0 ? '(absent — soak disabled)' : wanted,
      fix: `set "minimumReleaseAge": "${wanted}" at the top level (or run --fix)`,
    },
  ]
}

export function fixRenovateConfig(body: string): string {
  let config: Record<string, unknown>
  try {
    config = JSON.parse(body)
  } catch {
    return body
  }
  if (SOAK_DAYS === 0) {
    delete config['minimumReleaseAge']
  } else {
    config['minimumReleaseAge'] = `${SOAK_DAYS} days`
  }
  return `${JSON.stringify(config, null, 2)}\n`
}

export function fixCargoConfig(body: string): string {
  return body.replace(
    /^(global-min-publish-age\s*=\s*)"[^"]*"/m,
    `$1"${SOAK_DAYS} days"`,
  )
}

export function fixNpmrc(body: string): string {
  if (/^min-release-age=\d+\s*$/m.test(body)) {
    return body.replace(/^min-release-age=\d+\s*$/m, `min-release-age=${SOAK_DAYS}`)
  }
  return `${body.trimEnd()}\nmin-release-age=${SOAK_DAYS}\n`
}

export function fixWorkspaceYaml(body: string): string {
  let out = body.replace(
    /^(minimumReleaseAge:\s*)\d+\s*$/m,
    `$1${SOAK_MINUTES}`,
  )
  // Prune expired pins together with their annotation line.
  const today = todayIso()
  const lines = out.split('\n')
  const drop = new Set<number>()
  for (const entry of parseExcludeEntries(out)) {
    if (entry.annotation && entry.annotation.removable < today) {
      drop.add(entry.line - 1)
      if (ANNOTATION_RE.test(lines[entry.line - 2]?.trim() ?? '')) {
        drop.add(entry.line - 2)
      }
    }
  }
  if (drop.size > 0) {
    out = lines.filter((_, i) => !drop.has(i)).join('\n')
  }
  return out
}

function report(findings: Finding[], quiet: boolean): void {
  for (const f of findings) {
    console.error(`[soak] ${f.file}: ${f.what}`)
    console.error(`  saw:    ${f.saw}`)
    console.error(`  wanted: ${f.wanted}`)
    console.error(`  fix:    ${f.fix}`)
  }
  if (!quiet && findings.length === 0) {
    console.log(`[soak] all surfaces match SOAK_DAYS=${SOAK_DAYS} and no exclude has drifted`)
  }
}

export function main(argv: string[] = process.argv.slice(2)): number {
  const fix = argv.includes('--fix')
  const quiet = argv.includes('--quiet')
  const findings: Finding[] = []

  const surfaces: Array<{
    rel: string
    check: (body: string, file: string) => Finding[]
    fixer?: (body: string) => string
  }> = [
    { rel: SURFACES.cargoConfig, check: checkCargoConfig, fixer: fixCargoConfig },
    { rel: SURFACES.npmrc, check: checkNpmrc, fixer: fixNpmrc },
    { rel: SURFACES.workspaceYaml, check: checkWorkspaceYaml, fixer: fixWorkspaceYaml },
    { rel: SURFACES.tazeConfig, check: checkTazeConfig },
    { rel: SURFACES.renovateJson, check: checkRenovateConfig, fixer: fixRenovateConfig },
  ]

  for (const s of surfaces) {
    const abs = path.join(REPO_ROOT, s.rel)
    if (!existsSync(abs)) {
      findings.push({
        file: s.rel,
        what: 'soak surface missing',
        saw: '(file absent)',
        wanted: 'file present and carrying the soak window',
        fix: `create ${s.rel} — see scripts/soak/constants.mts header for the expected key`,
      })
      continue
    }
    let body = readFileSync(abs, 'utf8')
    if (fix && s.fixer) {
      const fixed = s.fixer(body)
      if (fixed !== body) {
        writeFileSync(abs, fixed)
        console.log(`[soak] fixed ${s.rel}`)
        body = fixed
      }
    }
    findings.push(...s.check(body, s.rel))
  }

  // Catalog <-> package.json lockstep for the package next to the yaml.
  const yamlAbs = path.join(REPO_ROOT, SURFACES.workspaceYaml)
  const pkgAbs = path.join(path.dirname(yamlAbs), 'package.json')
  if (existsSync(yamlAbs) && existsSync(pkgAbs)) {
    findings.push(
      ...checkCatalogParity(
        readFileSync(yamlAbs, 'utf8'),
        readFileSync(pkgAbs, 'utf8'),
        SURFACES.workspaceYaml,
      ),
    )
  }

  report(findings, quiet)
  return findings.length === 0 ? 0 : 1
}

// realpath + pathToFileURL so symlinked checkouts and paths needing URL
// encoding still register as the entrypoint (ESM realpaths import.meta.url).
const isMain =
  process.argv[1] && pathToFileURL(realpathSync(process.argv[1])).href === import.meta.url
if (isMain) {
  process.exitCode = main()
}

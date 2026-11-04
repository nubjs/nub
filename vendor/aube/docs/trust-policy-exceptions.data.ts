import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const TRUST_SOURCE_PATH = path.resolve(
  __dirname,
  '../crates/aube-resolver/src/trust.rs',
)

declare const data: string[]
export { data }

export function parseTrustPolicyExcludes(source: string): string[] {
  const block = source.match(
    /pub const DEFAULT_TRUST_POLICY_EXCLUDES:[\s\S]*?=\s*&\[(?<entries>[\s\S]*?)\];/,
  )?.groups?.entries

  if (!block) {
    throw new Error('could not find DEFAULT_TRUST_POLICY_EXCLUDES in trust.rs')
  }

  const uncommented = block
    .replace(/\/\*[\s\S]*?\*\//g, '')
    .replace(/\/\/.*$/gm, '')

  const entries = uncommented.split('\n').flatMap((line, index) => {
    const trimmed = line.trim()
    if (!trimmed) return []

    const match = trimmed.match(/^"([^"]+)"\s*,?$/)
    if (!match) {
      throw new Error(
        `unrecognized DEFAULT_TRUST_POLICY_EXCLUDES entry on line ${index + 1}: ${trimmed}`,
      )
    }
    return [match[1]]
  })

  return entries.sort((a, b) => a.localeCompare(b))
}

export default {
  watch: ['../crates/aube-resolver/src/trust.rs'],
  load(): string[] {
    const source = fs.readFileSync(TRUST_SOURCE_PATH, 'utf8')
    return parseTrustPolicyExcludes(source)
  },
}

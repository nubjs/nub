// Golden-reference suite for Nub's argv0=node flag-boundary parser.
//
// Under hijack-by-default, `nub` sits in front of every `node` invocation as
// argv0=node and MUST locate the node-flag/script boundary exactly as node
// does, then forward verbatim (wiki/research/node-flag-hijack-compat.md §6).
// Getting one flag's arity wrong silently runs the wrong file or aborts a
// program node would have run.
//
// Each case pins ONE boundary/identity contract, reproduced on real node. The
// `expect` assertions validate node itself (the contract is node's behavior).
// When a `nub` binary exists, every case ALSO runs through a real `node`-named
// shim → nub (mirroring the PATH hijack) and must match node byte-for-byte on
// exit code + stdout. That parity leg is the future-compat lock-in; until the
// binary lands it auto-skips, and the suite still guards node's contract.
//
// Run: node --test tests/flag-parsing/flag-parsing.test.mjs

import { test, before } from 'node:test'
import assert from 'node:assert/strict'
import { execFileSync, execFileSync as _e } from 'node:child_process'
import { mkdtempSync, symlinkSync, existsSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, dirname } from 'node:path'
import { fileURLToPath } from 'node:url'

const FIXTURES = join(dirname(fileURLToPath(import.meta.url)), 'fixtures')
const REPO = join(dirname(fileURLToPath(import.meta.url)), '..', '..')

// Locate a built nub binary; if absent, the parity leg is skipped (not failed).
const NUB = ['target/release/nub', 'target/debug/nub']
  .map((p) => join(REPO, p))
  .find((p) => existsSync(p))

let nodeShim // path to a directory whose `node` resolves to nub, set in before()

function run(bin, argv, stdin) {
  try {
    const stdout = execFileSync(bin, argv, {
      cwd: FIXTURES,
      input: stdin ?? '',
      encoding: 'utf8',
      stdio: ['pipe', 'pipe', 'pipe'],
    })
    return { code: 0, stdout, stderr: '' }
  } catch (e) {
    return { code: e.status ?? 1, stdout: e.stdout?.toString() ?? '', stderr: e.stderr?.toString() ?? '' }
  }
}

const node = (argv, stdin) => run(process.execPath, argv, stdin)
const nubAsNode = (argv, stdin) => run(join(nodeShim, 'node'), argv, stdin)

before(() => {
  if (!NUB) return
  nodeShim = mkdtempSync(join(tmpdir(), 'nub-flagtest-'))
  symlinkSync(NUB, join(nodeShim, 'node'))
})

// Each case: a name stating the contract, the argv, optional stdin, and an
// `expect(result)` asserting node's documented behavior.
const CASES = [
  {
    name: 'node-native value flag consumes its space-separated value (--require)',
    argv: ['--require', './pre.js', 'entry.js'],
    expect: (r) => { assert.equal(r.code, 0); assert.match(r.stdout, /RAN:pre/); assert.match(r.stdout, /RAN:entry/) },
  },
  {
    name: '--import likewise consumes its value; the trailing file is the script',
    argv: ['--import', './pre.js', 'entry.js'],
    expect: (r) => { assert.equal(r.code, 0); assert.match(r.stdout, /RAN:pre/); assert.match(r.stdout, /RAN:entry/) },
  },
  {
    name: 'V8 numeric option does NOT consume a space value; node aborts, script never runs',
    argv: ['--max-old-space-size', '100', 'entry.js'],
    expect: (r) => { assert.notEqual(r.code, 0); assert.doesNotMatch(r.stdout, /RAN:entry/) },
  },
  {
    name: 'same V8 option in =-form is accepted; the script runs',
    argv: ['--max-old-space-size=100', 'entry.js'],
    expect: (r) => { assert.equal(r.code, 0); assert.match(r.stdout, /RAN:entry/) },
  },
  {
    name: 'node-owned uint flag DOES consume its space value (--v8-pool-size), unlike --max-old-space-size',
    argv: ['--v8-pool-size', '4', 'entry.js'],
    expect: (r) => { assert.equal(r.code, 0); assert.match(r.stdout, /RAN:entry/) },
  },
  {
    name: 'valueless flag consumes nothing; the next token is the script (so a bad path fails, proving non-consumption)',
    argv: ['--jitless', 'does-not-exist.js'],
    expect: (r) => { assert.notEqual(r.code, 0); assert.match(r.stderr, /Cannot find module|ENOENT|does-not-exist/) },
  },
  {
    name: '-e consumes its code; a trailing file is argv, NOT executed',
    argv: ['-e', 'console.log("EVAL")', 'entry.js'],
    expect: (r) => { assert.equal(r.code, 0); assert.match(r.stdout, /EVAL/); assert.doesNotMatch(r.stdout, /RAN:entry/) },
  },
  {
    name: '-p evaluates-and-prints; trailing file is argv',
    argv: ['-p', '1+1', 'entry.js'],
    expect: (r) => { assert.equal(r.code, 0); assert.match(r.stdout, /2/); assert.doesNotMatch(r.stdout, /RAN:entry/) },
  },
  {
    name: 'early-exit flag fires AFTER -e (no script boundary): prints version, eval never runs',
    argv: ['-e', 'console.log("EVAL")', '--version'],
    expect: (r) => { assert.equal(r.code, 0); assert.match(r.stdout, /^v\d+\.\d+\.\d+/); assert.doesNotMatch(r.stdout, /EVAL/) },
  },
  {
    name: '-- is the hard boundary; the following token is the script even with a leading-dash sibling',
    argv: ['--jitless', '--', 'entry.js'],
    expect: (r) => { assert.equal(r.code, 0); assert.match(r.stdout, /RAN:entry/) },
  },
  {
    name: 'bare - reads the script from stdin',
    argv: ['-'],
    stdin: 'console.log("STDIN")\n',
    expect: (r) => { assert.equal(r.code, 0); assert.match(r.stdout, /STDIN/) },
  },
  {
    // Underscore normalization (_ -> -) applies to option NAME + arity, so
    // --env_file consumes its value exactly as --env-file: env.env is the
    // VALUE (not the script), and entry.js runs. (The env-file FEATURE itself
    // silently no-ops under the underscore spelling on node 26.2 — a node
    // quirk; the load-bearing part for the boundary parser is that the value
    // is consumed. If env.env were treated as the script it is not valid JS
    // and node would exit non-zero without running entry.js.)
    name: 'underscores normalize to dashes for arity: --env_file consumes its value, entry.js is the script',
    argv: ['--env_file', 'env.env', 'entry.js'],
    expect: (r) => { assert.equal(r.code, 0); assert.match(r.stdout, /RAN:entry/) },
  },
  {
    name: 'a value flag rejects a following token that starts with - (no silent consumption)',
    argv: ['--require', '-p', 'entry.js'],
    expect: (r) => { assert.notEqual(r.code, 0); assert.match(r.stderr, /requires an argument/) },
  },
  {
    name: '--stack-trace-limit space-form fails (dual-pushes bare flag to V8); script never runs',
    argv: ['--stack-trace-limit', '20', 'entry.js'],
    expect: (r) => { assert.notEqual(r.code, 0); assert.doesNotMatch(r.stdout, /RAN:entry/) },
  },
  {
    name: '--stack-trace-limit=20 (=-form) is accepted; script runs',
    argv: ['--stack-trace-limit=20', 'entry.js'],
    expect: (r) => { assert.equal(r.code, 0); assert.match(r.stdout, /RAN:entry/) },
  },
  {
    name: '-r alias inherits --require arity: consumes its value',
    argv: ['-r', './pre.js', 'entry.js'],
    expect: (r) => { assert.equal(r.code, 0); assert.match(r.stdout, /RAN:pre/); assert.match(r.stdout, /RAN:entry/) },
  },
  {
    name: '-e=x is rejected (=-split is --prefix only)',
    argv: ['-e=foo'],
    expect: (r) => { assert.notEqual(r.code, 0); assert.match(r.stderr, /bad option/) },
  },
  {
    name: 'an unknown --flag is FATAL on argv (bad option, exit 9), never warn-and-ignore',
    argv: ['--totally-bogus-flag', 'entry.js'],
    expect: (r) => { assert.notEqual(r.code, 0); assert.doesNotMatch(r.stdout, /RAN:entry/); assert.match(r.stderr, /bad option/) },
  },
  {
    name: '--version prints a real node version string (identity contract under hijack)',
    argv: ['--version'],
    expect: (r) => { assert.equal(r.code, 0); assert.match(r.stdout, /^v\d+\.\d+\.\d+/) },
  },
]

for (const c of CASES) {
  test(c.name, () => {
    const ref = node(c.argv, c.stdin)
    c.expect(ref)

    if (!NUB) return // parity leg skipped until a nub binary is built
    const got = nubAsNode(c.argv, c.stdin)
    // nub-as-node must reproduce node's boundary decision exactly: same exit
    // code and same stdout. (Injected execArgv flags must not leak into the
    // script's observable output.)
    assert.equal(got.code, ref.code, `exit code parity\n node: ${ref.code}\n  nub: ${got.code}\n stderr: ${got.stderr}`)
    assert.equal(got.stdout, ref.stdout, `stdout parity\n node: ${JSON.stringify(ref.stdout)}\n  nub: ${JSON.stringify(got.stdout)}`)
  })
}

if (!NUB) {
  test('nub-as-node parity leg', { skip: 'no nub binary at target/{release,debug}/nub — build it to enable the differential parity assertions' }, () => {})
}

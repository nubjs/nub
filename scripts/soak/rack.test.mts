// Filesystem-touching tests run against a throwaway rack under os.tmpdir():
// XDG_DATA_HOME is pointed at a fresh temp dir BEFORE paths.mts is imported
// (dynamic imports below), so nothing here can touch the real rack.
import assert from 'node:assert/strict'
import { mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { after, test } from 'node:test'

const SANDBOX = mkdtempSync(path.join(os.tmpdir(), 'soak-rack-test-'))
process.env.XDG_DATA_HOME = SANDBOX

const { linkHandle, rackRealFor, writeShims } = await import('./external-tools.mts')
const { BIN_DIR, RACK_DIR } = await import('./paths.mts')

after(() => {
  rmSync(SANDBOX, { recursive: true, force: true })
})

function plantRackBinary(cmd: string, version: string): string {
  const dir = path.join(RACK_DIR, cmd, version)
  mkdirSync(dir, { recursive: true })
  const bin = path.join(dir, cmd)
  writeFileSync(bin, '#!/usr/bin/env bash\necho real-binary\n')
  return bin
}

test('the sandbox rack lives under os.tmpdir()', () => {
  assert.ok(BIN_DIR.startsWith(os.tmpdir().replace(/\/$/, '')) || BIN_DIR.startsWith(SANDBOX))
})

test('linkHandle replaces existing and DANGLING handles without throwing', () => {
  const bin = plantRackBinary('fakea', '1.0.0')
  linkHandle(bin, 'fakea')
  // replace an existing handle
  linkHandle(bin, 'fakea')
  // dangling: remove the target, the handle now points nowhere
  rmSync(bin)
  const bin2 = plantRackBinary('fakea', '2.0.0')
  linkHandle(bin2, 'fakea')
  assert.equal(readFileSync(path.join(BIN_DIR, 'fakea'), 'utf8').includes('real-binary'), true)
})

test('rackRealFor resolves version binaries and wrappers, else empty', () => {
  const bin = plantRackBinary('fakeb', '3.1.4')
  assert.equal(rackRealFor('fakeb', { fakeb: { version: '3.1.4' } }), bin)
  const wrapper = path.join(RACK_DIR, 'fakec', 'fakec-wrapper')
  mkdirSync(path.dirname(wrapper), { recursive: true })
  writeFileSync(wrapper, '#!/usr/bin/env bash\n')
  assert.equal(rackRealFor('fakec', { fakec: { purl: 'pkg:npm/fakec@1.0.0' } }), wrapper)
  assert.equal(rackRealFor('missing', {}), '')
})

test('writeShims never clobbers a rack binary and embeds its path', t => {
  if (process.platform === 'win32') {
    t.skip('shims are POSIX-only')
    return
  }
  const tools = { pnpm: { version: '11.8.0' } }
  const bin = plantRackBinary('pnpm', '11.8.0')
  linkHandle(bin, 'pnpm')
  writeShims(tools)
  // regression (writing through the symlink): the rack binary must be intact
  assert.match(readFileSync(bin, 'utf8'), /real-binary/)
  const shim = readFileSync(path.join(BIN_DIR, 'pnpm'), 'utf8')
  assert.ok(shim.includes(`REAL='${bin}'`), 'pinned command embeds its rack path')
  // unpinned commands fall back to PATH-stripping resolution
  assert.match(readFileSync(path.join(BIN_DIR, 'cargo'), 'utf8'), /CLEAN_PATH/)
})

test('writeShims is idempotent: a re-run keeps the rack pin embedded', t => {
  if (process.platform === 'win32') {
    t.skip('shims are POSIX-only')
    return
  }
  const tools = { pnpm: { version: '11.8.0' } }
  writeShims(tools)
  writeShims(tools) // handle is now a regular shim file, not a symlink
  const shim = readFileSync(path.join(BIN_DIR, 'pnpm'), 'utf8')
  assert.ok(shim.includes("REAL='"), 'rack pin survives a second --shims run')
})

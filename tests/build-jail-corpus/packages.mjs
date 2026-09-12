import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { copyFileSync, mkdtempSync, mkdirSync, readFileSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

const binary = resolve(process.env.NUB_BIN);
// MSVC derives object-file names below the source tree and still rejects sufficiently
// deep paths. A caller can select a short, disposable root for native Windows probes;
// ordinary test runs retain the host temporary directory.
const fixtureBase = process.env.CORPUS_TEMP_ROOT || tmpdir();
mkdirSync(fixtureBase, { recursive: true });
const root = mkdtempSync(join(fixtureBase, 'nub-jail-packages-'));
const reportRoot = process.env.CORPUS_REPORT ? resolve(process.env.CORPUS_REPORT) : null;
if (reportRoot) mkdirSync(reportRoot, { recursive: true });
const report = (source, destination) => {
  if (!reportRoot) return;
  const target = join(reportRoot, destination);
  mkdirSync(dirname(target), { recursive: true });
  copyFileSync(source, target);
};
const gypRealpathTrace = String.raw`import ctypes
import json
import nt
import os
import sys

FILE_SHARE_READ = 0x00000001
FILE_SHARE_WRITE = 0x00000002
FILE_SHARE_DELETE = 0x00000004
OPEN_EXISTING = 3
FILE_FLAG_BACKUP_SEMANTICS = 0x02000000
VOLUME_NAME_DOS = 0
VOLUME_NAME_GUID = 1
VOLUME_NAME_NT = 2
FILE_NAME_OPENED = 8

kernel32 = ctypes.WinDLL('kernel32', use_last_error=True)
kernel32.CreateFileW.argtypes = [ctypes.c_wchar_p, ctypes.c_uint32, ctypes.c_uint32,
                                  ctypes.c_void_p, ctypes.c_uint32, ctypes.c_uint32,
                                  ctypes.c_void_p]
kernel32.CreateFileW.restype = ctypes.c_void_p
kernel32.GetFinalPathNameByHandleW.argtypes = [ctypes.c_void_p, ctypes.c_wchar_p,
                                                ctypes.c_uint32, ctypes.c_uint32]
kernel32.GetFinalPathNameByHandleW.restype = ctypes.c_uint32
kernel32.CloseHandle.argtypes = [ctypes.c_void_p]

def final_path(path, flags):
    handle = kernel32.CreateFileW(path, 0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                                  None, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, None)
    if handle == ctypes.c_void_p(-1).value:
        return {'error': ctypes.get_last_error()}
    try:
        buffer = ctypes.create_unicode_buffer(32768)
        length = kernel32.GetFinalPathNameByHandleW(handle, buffer, len(buffer), flags)
        if not length or length >= len(buffer):
            return {'error': ctypes.get_last_error(), 'length': length}
        return {'path': buffer.value}
    finally:
        kernel32.CloseHandle(handle)

def python_final(path):
    try:
        return {'path': nt._getfinalpathname(path)}
    except OSError as error:
        return {'error': error.winerror}

def inspect(path):
    return {
        'python': python_final(path),
        'realpath': os.path.realpath(path),
        'dos_normalized': final_path(path, VOLUME_NAME_DOS),
        'dos_opened': final_path(path, VOLUME_NAME_DOS | FILE_NAME_OPENED),
        'guid_normalized': final_path(path, VOLUME_NAME_GUID),
        'nt_normalized': final_path(path, VOLUME_NAME_NT),
        'nt_opened': final_path(path, VOLUME_NAME_NT | FILE_NAME_OPENED),
    }

if os.name == 'nt' and sys.argv and sys.argv[0].lower().endswith('gyp_main.py'):
    cwd = os.path.realpath('.')
    dependency = os.path.realpath(r'deps/sqlite3.gyp')
    print('NUB_GYP_REALPATH ' + json.dumps({
        'cwd': cwd,
        'binding': os.path.realpath('binding.gyp'),
        'dependency': dependency,
        'relative_dependency': os.path.relpath(dependency, cwd),
        'final_paths': {path: inspect(path) for path in ('.', r'.\\.', '..')},
    }, sort_keys=True), file=sys.stderr, flush=True)
`;
const isolatedEnvKeys = new Set([
  'npm_config_nodedir',
  'npm_config_python',
  'npm_config_build_from_source',
  'nub_cache_dir',
  'aube_cache_dir',
  'npm_config_cache',
  'npm_config_cache_dir',
  'npm_config_store_dir',
  'npm_config_virtual_store_dir',
  'npm_config_global_virtual_store_dir',
  'pnpm_config_cache_dir',
  'pnpm_config_store_dir',
  'pnpm_config_state_dir',
  'pnpm_config_config_dir',
  'pnpm_home',
  'npm_config_userconfig',
  'pnpm_config_userconfig',
  'npm_config_prefix',
  'prefix',
  'npm_config_globalconfig',
  'npm_config_builtin_config',
]);
const cases = [
  ['esbuild', '0.24.0', "assert.match(require('esbuild').transformSync('const x: number = 1', {loader:'ts'}).code, /const x = 1/)"] ,
  ['better-sqlite3', '11.8.1', "const db = require('better-sqlite3')(':memory:'); assert.equal(db.prepare('select 42 as n').get().n, 42); db.close()"],
  ['bcrypt', '5.1.1', "const b = require('bcrypt'); assert.ok(b.compareSync('fixture', b.hashSync('fixture', 4)))"],
  ['sharp', '0.33.5', "const s = require('sharp'); const b = await s({create:{width:2,height:3,channels:3,background:'red'}}).png().toBuffer(); assert.equal((await s(b).metadata()).height, 3)"],
  ['@swc/core', '1.15.46', "assert.match(require('@swc/core').transformSync('const x: number = 1', {jsc:{parser:{syntax:'typescript'}}}).code, /x = 1/)"],
  ['cpu-features', '0.0.10', "assert.equal(typeof require('cpu-features')().arch, 'string')", true],
  ['better-sqlite3', '11.8.1', "const db = require('better-sqlite3')(':memory:'); assert.equal(db.prepare('select 42 as n').get().n, 42); db.close()", true],
];
const selected = process.env.CORPUS_CASE ? cases.filter(([name]) => name === process.env.CORPUS_CASE) : cases;
assert.ok(selected.length, 'at least one package selected');
const results = [];
console.log(`Fixture root: ${root}`);
const provenance = join(root, 'provenance.json');
writeFileSync(provenance, JSON.stringify({
  binary, sha256: createHash('sha256').update(readFileSync(binary)).digest('hex'), fixtureBase,
  controlPython: process.env.CORPUS_CONTROL_PYTHON || null,
  node: process.execPath, version: process.version, platform: process.platform, arch: process.arch,
}, null, 2));
report(provenance, 'provenance.json');

for (const [name, version, probe, source = false] of selected) {
  for (const confined of [false, true]) {
    const label = `${name.replaceAll('/', '-')}-${source ? 'source' : 'default'}-${confined ? 'jailed' : 'control'}`;
    const base = join(root, label);
    const project = join(base, 'project');
    const backingProject = process.env.CORPUS_LINKED_PROJECT ? join(base, 'project-backing') : project;
    const home = join(base, 'home');
    const temp = join(home, 'tmp');
    mkdirSync(backingProject, { recursive: true });
    if (backingProject !== project) symlinkSync(backingProject, project, 'junction');
    mkdirSync(temp, { recursive: true });
    writeFileSync(join(project, 'package.json'), JSON.stringify({
      name: 'jail-corpus-consumer', private: true, dependencies: { [name]: version },
      allowScripts: { '*': true },
    }));
    writeFileSync(join(project, 'nub.jsonc'), JSON.stringify({ install: { buildJail: confined } }));
    if (source && name === 'better-sqlite3' && process.env.CORPUS_GYP_REALPATH_TRACE) {
      const traceDir = join(project, '.nub-gyp-realpath-trace');
      mkdirSync(traceDir, { recursive: true });
      writeFileSync(join(traceDir, 'sitecustomize.py'), gypRealpathTrace);
    }
    const env = { ...process.env, HOME: home, USERPROFILE: home,
      APPDATA: join(home, 'AppData', 'Roaming'), LOCALAPPDATA: join(home, 'AppData', 'Local'),
      XDG_CONFIG_HOME: join(home, 'config'), XDG_CACHE_HOME: join(home, 'cache'),
      XDG_DATA_HOME: join(home, 'data'), NODE_EXECUTABLE: process.execPath,
      TMPDIR: temp, TMP: temp, TEMP: temp,
      CI: '1', NO_COLOR: '1', NUB_JAIL_DUMP_POLICY: '1',
    };
    for (const key of Object.keys(env)) {
      if (isolatedEnvKeys.has(key.toLowerCase())) delete env[key];
    }
    if (source) env.npm_config_build_from_source = 'true';
    if (source && name === 'better-sqlite3' && process.env.CORPUS_GYP_REALPATH_TRACE && !confined) {
      env.PYTHONPATH = join(project, '.nub-gyp-realpath-trace');
    }
    // Keep the source-build control on the exact interpreter the jail selected.
    // A passed control therefore rules out interpreter-version drift rather than merely
    // showing that an unrelated host Python can build the addon.
    if (source && !confined && process.env.CORPUS_CONTROL_PYTHON) {
      env.npm_config_python = process.env.CORPUS_CONTROL_PYTHON;
    }
    const install = spawnSync(binary, ['install'], { cwd: project, env, encoding: 'utf8', timeout: 300_000 });
    const log = `${install.stdout}\n${install.stderr}`;
    const installLog = join(base, 'install.log');
    writeFileSync(installLog, log);
    report(installLog, join('cases', label, 'install.log'));
    try {
      assert.ifError(install.error);
      assert.equal(install.status, 0, 'install succeeded');
      if (confined) assert.ok(log.includes(`JAILDUMP pkg=Some("${name}")`), 'target lifecycle entered the jail');
      else assert.match(log, /running without the build sandbox/, 'control opt-out engaged');
      if (source) assert.match(log, /gyp info using node-gyp@/, 'native compilation actually ran');
      if (source && name === 'better-sqlite3' && process.env.CORPUS_GYP_REALPATH_TRACE) {
        assert.match(log, /NUB_GYP_REALPATH /, 'GYP realpath trace captured');
      }
      const check = spawnSync(process.execPath, ['-e', `const assert=require('node:assert/strict'); (async()=>{${probe}})().catch(e=>{console.error(e);process.exitCode=1})`],
        { cwd: project, env, encoding: 'utf8', timeout: 30_000 });
      const probeLog = join(base, 'probe.log');
      writeFileSync(probeLog, `${check.stdout}\n${check.stderr}`);
      report(probeLog, join('cases', label, 'probe.log'));
      assert.ifError(check.error);
      assert.equal(check.status, 0, 'installed artifact works');
      results.push({ label, pass: true });
      console.log(`PASS ${label}`);
    } catch (error) {
      results.push({ label, pass: false, error: error.message });
      console.error(`FAIL ${label}: ${error.message}; logs: ${base}`);
    }
    const resultFile = join(root, 'results.json');
    writeFileSync(resultFile, JSON.stringify(results, null, 2));
    report(resultFile, 'results.json');
  }
}
process.exitCode = results.every(result => result.pass) ? 0 : 1;

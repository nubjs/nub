import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { copyFileSync, existsSync, lstatSync, mkdtempSync, mkdirSync, readFileSync, realpathSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

const binary = resolve(process.env.NUB_BIN);
const requestedTempRoot = process.env.CORPUS_TEMP_ROOT ? resolve(process.env.CORPUS_TEMP_ROOT) : resolve(tmpdir());
mkdirSync(requestedTempRoot, { recursive: true });
const root = mkdtempSync(join(requestedTempRoot, 'nub-jail-packages-'));
assert.equal(resolve(dirname(root)), requestedTempRoot, 'fixture root uses CORPUS_TEMP_ROOT');
const linkedProject = process.env.CORPUS_LINKED_PROJECT === '1';
const reportRoot = process.env.CORPUS_REPORT ? resolve(process.env.CORPUS_REPORT) : null;
const screen = process.env.CORPUS_OSV_SCREEN ? resolve(process.env.CORPUS_OSV_SCREEN) : null;
const screenRoot = reportRoot ?? join(root, 'security');
if (reportRoot) mkdirSync(reportRoot, { recursive: true });
const report = (source, destination) => {
  if (!reportRoot) return;
  const target = join(reportRoot, destination);
  mkdirSync(dirname(target), { recursive: true });
  copyFileSync(source, target);
};
const isolatedEnvKeys = new Set([
  'npm_config_nodedir',
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
  binary, sha256: createHash('sha256').update(readFileSync(binary)).digest('hex'), requestedTempRoot, fixtureRoot: root,
  node: process.execPath, version: process.version, platform: process.platform, arch: process.arch,
}, null, 2));
report(provenance, 'provenance.json');

for (const [name, version, probe, source = false] of selected) {
  for (const confined of [false, true]) {
    const label = `${name.replaceAll('/', '-')}-${source ? 'source' : 'default'}${linkedProject ? '-junction' : ''}-${confined ? 'jailed' : 'control'}`;
    const base = join(root, label);
    const project = join(base, 'project');
    const projectTarget = join(base, 'project-target');
    const home = join(base, 'home');
    const temp = join(home, 'tmp');
    mkdirSync(projectTarget, { recursive: true });
    if (linkedProject) {
      symlinkSync(projectTarget, project, 'junction');
      assert.ok(lstatSync(project).isSymbolicLink(), 'project is a junction');
      assert.equal(realpathSync(project), realpathSync(projectTarget), 'junction resolves to project target');
    }
    else mkdirSync(project, { recursive: true });
    mkdirSync(temp, { recursive: true });
    writeFileSync(join(project, 'package.json'), JSON.stringify({
      name: 'jail-corpus-consumer', private: true, dependencies: { [name]: version },
      allowScripts: { '*': true },
    }));
    writeFileSync(join(project, 'nub.jsonc'), JSON.stringify({ install: { buildJail: confined } }));
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
    // Keep the source-build control on the exact interpreter the jail selected.
    // A passed control therefore rules out interpreter-version drift rather than merely
    // showing that an unrelated host Python can build the addon.
    if (source && !confined && process.env.CORPUS_CONTROL_PYTHON) {
      env.npm_config_python = process.env.CORPUS_CONTROL_PYTHON;
    }
    let preparedDigest = null;
    if (screen) {
      // Resolve this exact arm without scripts, screen its complete closure, then remove only the
      // materialized tree. The resolver's lockfile remains and binds the cold lifecycle install.
      const prepared = spawnSync(binary, ['install', '--ignore-scripts'], { cwd: project, env, encoding: 'utf8', timeout: 300_000 });
      writeFileSync(join(base, 'prepare.log'), `${prepared.stdout}\n${prepared.stderr}`);
      report(join(base, 'prepare.log'), join('cases', label, 'prepare.log'));
      assert.ifError(prepared.error);
      assert.equal(prepared.status, 0, 'script-free closure resolved');
      const cleared = spawnSync(process.execPath, [screen, '--tree', project, '--kind', `catalog-sanity-${label}-prepared`, '--cache-dir', join(screenRoot, 'clearances'), '--out', join(base, 'prepared-screen.json')], { env, encoding: 'utf8', timeout: 300_000 });
      writeFileSync(join(base, 'prepared-screen.log'), `${cleared.stdout}\n${cleared.stderr}`);
      report(join(base, 'prepared-screen.json'), join('cases', label, 'prepared-screen.json'));
      report(join(base, 'prepared-screen.log'), join('cases', label, 'prepared-screen.log'));
      assert.ifError(cleared.error);
      assert.equal(cleared.status, 0, 'resolved closure passed fail-closed screen');
      preparedDigest = JSON.parse(readFileSync(join(base, 'prepared-screen.json'), 'utf8')).digest;
      assert.ok(preparedDigest, 'prepared screen has an exact closure digest');
      rmSync(join(project, 'node_modules'), { recursive: true, force: true });
    }
    const install = spawnSync(binary, screen ? ['install', '--frozen-lockfile'] : ['install'], { cwd: project, env, encoding: 'utf8', timeout: 300_000 });
    const log = `${install.stdout}\n${install.stderr}`;
    const installLog = join(base, 'install.log');
    writeFileSync(installLog, log);
    report(installLog, join('cases', label, 'install.log'));
    // Native GYP inputs explain a failed source build without retaining a whole installed tree.
    // They are copied before the assertion so the failure arm has the same evidence as a pass.
    if (source) {
      for (const file of ['buildcheck.gypi', 'build/config.gypi', 'build/cpufeatures.vcxproj', 'build/cpu_features.vcxproj']) {
        const candidate = join(project, 'node_modules', name, file);
        if (existsSync(candidate)) report(candidate, join('cases', label, file));
      }
    }
    try {
      assert.ifError(install.error);
      assert.equal(install.status, 0, 'install succeeded');
      if (screen) {
        const bound = spawnSync(process.execPath, [screen, '--tree', project, '--kind', `catalog-sanity-${label}-bound`, '--cache-dir', join(screenRoot, 'clearances'), '--out', join(base, 'bound-screen.json')], { env, encoding: 'utf8', timeout: 300_000 });
        writeFileSync(join(base, 'bound-screen.log'), `${bound.stdout}\n${bound.stderr}`);
        report(join(base, 'bound-screen.json'), join('cases', label, 'bound-screen.json'));
        report(join(base, 'bound-screen.log'), join('cases', label, 'bound-screen.log'));
        assert.ifError(bound.error);
        assert.equal(bound.status, 0, 'installed closure remained screenable');
        assert.equal(JSON.parse(readFileSync(join(base, 'bound-screen.json'), 'utf8')).digest, preparedDigest, 'lifecycle used the screened frozen closure');
      }
      if (confined) assert.ok(log.includes(`JAILDUMP pkg=Some("${name}")`), 'target lifecycle entered the jail');
      else assert.match(log, /running without the build sandbox/, 'control opt-out engaged');
      if (source) assert.match(log, /gyp info using node-gyp@/, 'native compilation actually ran');
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

import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

const binary = resolve(process.env.NUB_BIN);
const root = mkdtempSync(join(tmpdir(), 'nub-jail-packages-'));
const cases = [
  ['esbuild', '0.24.0', "assert.match(require('esbuild').transformSync('const x: number = 1', {loader:'ts'}).code, /const x = 1/)"] ,
  ['better-sqlite3', '11.8.1', "const db = require('better-sqlite3')(':memory:'); assert.equal(db.prepare('select 42 as n').get().n, 42); db.close()"],
  ['bcrypt', '5.1.1', "const b = require('bcrypt'); assert.ok(b.compareSync('fixture', b.hashSync('fixture', 4)))"],
  ['sharp', '0.33.5', "const s = require('sharp'); const b = await s({create:{width:2,height:3,channels:3,background:'red'}}).png().toBuffer(); assert.equal((await s(b).metadata()).height, 3)"],
  ['@swc/core', '1.15.46', "assert.match(require('@swc/core').transformSync('const x: number = 1', {jsc:{parser:{syntax:'typescript'}}}).code, /x = 1/)"],
  ['cpu-features', '0.0.10', "assert.equal(typeof require('cpu-features')().arch, 'string')"],
  ['better-sqlite3', '11.8.1', "const db = require('better-sqlite3')(':memory:'); assert.equal(db.prepare('select 42 as n').get().n, 42); db.close()", true],
];
const selected = process.env.CORPUS_CASE ? cases.filter(([name]) => name === process.env.CORPUS_CASE) : cases;
assert.ok(selected.length, 'at least one package selected');
const results = [];
console.log(`Fixture root: ${root}`);
writeFileSync(join(root, 'provenance.json'), JSON.stringify({
  binary, sha256: createHash('sha256').update(readFileSync(binary)).digest('hex'),
  node: process.execPath, version: process.version, platform: process.platform, arch: process.arch,
}, null, 2));

for (const [name, version, probe, source = false] of selected) {
  for (const confined of [false, true]) {
    const label = `${name.replaceAll('/', '-')}-${source ? 'source' : 'default'}-${confined ? 'jailed' : 'control'}`;
    const base = join(root, label);
    const project = join(base, 'project');
    const home = join(base, 'home');
    mkdirSync(project, { recursive: true });
    mkdirSync(home, { recursive: true });
    writeFileSync(join(project, 'package.json'), JSON.stringify({
      name: 'jail-corpus-consumer', private: true, dependencies: { [name]: version },
      allowScripts: { '*': true },
    }));
    writeFileSync(join(project, 'nub.jsonc'), JSON.stringify({ install: { buildJail: confined } }));
    const env = { ...process.env, HOME: home, USERPROFILE: home,
      XDG_CONFIG_HOME: join(home, 'config'), XDG_CACHE_HOME: join(home, 'cache'),
      XDG_DATA_HOME: join(home, 'data'), NODE_EXECUTABLE: process.execPath,
      CI: '1', NO_COLOR: '1', NUB_JAIL_DUMP_POLICY: '1',
      ...(source ? { npm_config_build_from_source: 'true' } : {}),
    };
    const install = spawnSync(binary, ['install'], { cwd: project, env, encoding: 'utf8', timeout: 300_000 });
    const log = `${install.stdout}\n${install.stderr}`;
    writeFileSync(join(base, 'install.log'), log);
    try {
      assert.ifError(install.error);
      assert.equal(install.status, 0, 'install succeeded');
      if (confined) assert.ok(log.includes(`JAILDUMP pkg=Some("${name}")`), 'target lifecycle entered the jail');
      else assert.match(log, /running without the build sandbox/, 'control opt-out engaged');
      if (source) assert.match(log, /gyp info using node-gyp@/, 'native compilation actually ran');
      const check = spawnSync(process.execPath, ['-e', `const assert=require('node:assert/strict'); (async()=>{${probe}})().catch(e=>{console.error(e);process.exitCode=1})`],
        { cwd: project, env, encoding: 'utf8', timeout: 30_000 });
      writeFileSync(join(base, 'probe.log'), `${check.stdout}\n${check.stderr}`);
      assert.ifError(check.error);
      assert.equal(check.status, 0, 'installed artifact works');
      results.push({ label, pass: true });
      console.log(`PASS ${label}`);
    } catch (error) {
      results.push({ label, pass: false, error: error.message });
      console.error(`FAIL ${label}: ${error.message}; logs: ${base}`);
    }
    writeFileSync(join(root, 'results.json'), JSON.stringify(results, null, 2));
  }
}
process.exitCode = results.every(result => result.pass) ? 0 : 1;

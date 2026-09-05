import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { test } from 'node:test';

const binary = resolve(process.env.NUB_BIN);
const root = mkdtempSync(join(tmpdir(), 'nub-jail-contract-'));
console.log(`Fixture root: ${root}`);

test('user-authored root prepare remains unconfined', () => {
  const project = join(root, 'root-prepare');
  const home = join(root, 'root-home');
  mkdirSync(project);
  mkdirSync(home);
  const outside = join(root, 'root-output.json');
  writeFileSync(join(project, 'package.json'), JSON.stringify({ name: 'root-prepare', private: true,
    version: '1.0.0', scripts: { prepare: 'node prepare.cjs', prepack: 'node prepare.cjs' } }));
  writeFileSync(join(project, 'prepare.cjs'), `require('fs').writeFileSync(${JSON.stringify(outside)}, JSON.stringify({token:process.env.AWS_SECRET_ACCESS_KEY}));`);
  const options = { cwd: project, env: { ...process.env,
    HOME: home, USERPROFILE: home, XDG_CONFIG_HOME: join(home, 'config'),
    XDG_CACHE_HOME: join(home, 'cache'), XDG_DATA_HOME: join(home, 'data'),
    NODE_EXECUTABLE: process.execPath, AWS_SECRET_ACCESS_KEY: 'root-fixture-token', CI: '1', NO_COLOR: '1' } };
  const result = run(binary, ['install'], options);
  writeFileSync(join(project, 'install.log'), result.stdout + result.stderr);
  assert.deepEqual(JSON.parse(readFileSync(outside, 'utf8')), { token: 'root-fixture-token' });
  rmSync(outside);
  const packed = run(binary, ['pack'], options);
  writeFileSync(join(project, 'pack.log'), packed.stdout + packed.stderr);
  assert.deepEqual(JSON.parse(readFileSync(outside, 'utf8')), { token: 'root-fixture-token' });
});

function run(program, args, { expectedStatus = 0, ...options } = {}) {
  const result = spawnSync(program, args, { encoding: 'utf8', timeout: 120_000, ...options });
  assert.equal(result.error, undefined, `${program}: ${result.error}`);
  assert.equal(result.status, expectedStatus, `${program} ${args.join(' ')}\n${result.stdout}\n${result.stderr}`);
  return result;
}

for (const mode of ['confined', 'global-off', 'user-global-off', 'package-off', 'dependency-off', 'legacy-config-blocked']) {
  test(`dependency lifecycle ${mode}`, () => {
    const base = join(root, mode);
    const project = join(base, 'project');
    const pkg = join(base, 'package');
    const home = join(base, 'home');
    for (const dir of [project, pkg, home]) mkdirSync(dir, { recursive: true });
    const name = `jail-contract-${mode}-${Date.now()}`;
    const outside = join(base, 'outside-write');
    const secret = join(home, '.ssh', 'fixture-key');
    const toolConfig = join(home, 'cache', 'nub', 'pm', 'tools', 'node-gyp', 'v12', '.npmrc');
    if (mode === 'legacy-config-blocked') mkdirSync(toolConfig, { recursive: true });
    mkdirSync(join(home, '.ssh'));
    writeFileSync(secret, 'fixture-secret');
    writeFileSync(join(pkg, 'package.json'), JSON.stringify({
      name, version: '1.0.0', scripts: { postinstall: 'node probe.cjs' },
      ...(mode === 'dependency-off' ? { allowScripts: { [name]: 'no-jail' } } : {}),
    }));
    writeFileSync(join(pkg, 'probe.cjs'), `
      const fs = require('node:fs');
      const out = { ran: true, token: process.env.AWS_SECRET_ACCESS_KEY ?? null };
      try { fs.writeFileSync(${JSON.stringify(outside)}, 'outside'); out.write = true; }
      catch (e) { out.write = e.code; }
      try { out.read = fs.readFileSync(${JSON.stringify(secret)}, 'utf8'); }
      catch (e) { out.read = e.code; }
      try { out.toolConfig = fs.readFileSync(${JSON.stringify(toolConfig)}, 'utf8'); }
      catch (e) { out.toolConfig = e.code; }
      fs.writeFileSync('contract.json', JSON.stringify(out));
    `);
    const archive = join(base, 'dep.tgz');
    run('tar', ['-czf', archive, '-C', base, 'package']);
    writeFileSync(join(project, 'package.json'), JSON.stringify({
      name: `consumer-${mode}`, private: true,
      dependencies: { [name]: `file:${archive.replaceAll('\\', '/')}` },
      allowScripts: { [`${name}@file:${archive.replaceAll('\\', '/')}`]: mode === 'package-off' ? 'no-jail' : true },
    }));
    writeFileSync(join(project, '.npmrc'), '//unused.invalid/:_authToken=fixture-registry-secret\n');
    if (mode === 'global-off') {
      writeFileSync(join(project, 'nub.jsonc'), JSON.stringify({ install: { buildJail: false } }));
    }
    const env = { ...process.env, HOME: home, USERPROFILE: home,
      XDG_CONFIG_HOME: join(home, 'config'), XDG_CACHE_HOME: join(home, 'cache'),
      XDG_DATA_HOME: join(home, 'data'), AWS_SECRET_ACCESS_KEY: 'fixture-token',
      NODE_EXECUTABLE: process.execPath, CI: '1', NO_COLOR: '1' };
    if (mode === 'user-global-off') {
      const options = {cwd: project, env};
      run(binary, ['config', 'set', 'install.buildJail', 'false', '--global'], options);
      assert.equal(run(binary, ['config', 'get', 'install.buildJail', '--global'], options).stdout.trim(), 'false');
      run(binary, ['config', 'set', 'install.buildJail', 'true'], options);
      assert.equal(run(binary, ['config', 'get', 'install.buildJail'], options).stdout.trim(), 'true');
      run(binary, ['config', 'delete', 'install.buildJail'], options);
    }
    const result = run(binary, ['install'], { cwd: project, env,
      expectedStatus: mode === 'legacy-config-blocked' ? 1 : 0 });
    writeFileSync(join(base, 'install.log'), `${result.stdout}\n${result.stderr}`);
    const artifact = join(project, 'node_modules', name, 'contract.json');
    if (mode === 'legacy-config-blocked') {
      assert.ok(!existsSync(artifact), 'a cleanup failure must prevent the script from starting');
      assert.match(result.stdout + result.stderr, /removing cached node-gyp credentials/);
      return;
    }
    assert.ok(existsSync(artifact), `lifecycle did not produce ${artifact}\n${result.stdout}\n${result.stderr}`);
    const observed = JSON.parse(readFileSync(artifact, 'utf8'));
    assert.equal(observed.ran, true);
    if (mode === 'confined' || mode === 'dependency-off') {
      assert.notEqual(observed.write, true, JSON.stringify(observed));
      assert.notEqual(observed.read, 'fixture-secret', JSON.stringify(observed));
      assert.equal(observed.token, null, JSON.stringify(observed));
      assert.ok(!observed.toolConfig.includes('fixture-registry-secret'), JSON.stringify(observed));
      assert.equal(existsSync(outside), false);
    } else {
      assert.equal(observed.write, true, JSON.stringify(observed));
      assert.equal(observed.read, 'fixture-secret', JSON.stringify(observed));
      assert.equal(observed.token, 'fixture-token', JSON.stringify(observed));
      assert.match(result.stdout + result.stderr, /running without the build sandbox/);
    }
    if (mode === 'user-global-off') {
      run(binary, ['config', 'delete', 'install.buildJail', '--global'], {cwd: project, env});
      assert.equal(run(binary, ['config', 'get', 'install.buildJail', '--global'], {cwd: project, env}).stdout.trim(), 'undefined');
    }
  });
}

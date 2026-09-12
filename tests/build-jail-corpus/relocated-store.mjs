import assert from 'node:assert/strict';
import { execFile, execFileSync, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { basename, dirname, join, relative, resolve } from 'node:path';
import { test } from 'node:test';
import { promisify } from 'node:util';

const binary = resolve(process.env.NUB_BIN);
const execFileAsync = promisify(execFile);
const root = mkdtempSync(join(tmpdir(), 'nub-jail-relocated-store-'));
console.log(`Fixture root: ${root}`);

function contains(root, child) {
  const rel = relative(root, child);
  return rel === '' || (!rel.startsWith('..') && !rel.includes(`..${process.platform === 'win32' ? '\\' : '/'}`));
}

function gvsCell(path) {
  let current = path;
  while (basename(current) !== 'node_modules') {
    const parent = dirname(current);
    assert.notEqual(parent, current, `no node_modules ancestor for ${path}`);
    current = parent;
  }
  return dirname(current);
}

for (const source of ['NUB_CACHE_DIR', '.npmrc global-virtual-store-dir']) {
  test(`confined lifecycle reads the ${source}-relocated global store only`, async () => {
    const isEnv = source.startsWith('NUB');
    const base = join(root, isEnv ? 'env' : 'npmrc');
    const project = join(base, 'project');
    const targetDir = join(base, 'target');
    const dependencyDir = join(base, 'dependency');
    const home = join(base, 'home');
    const cacheDir = join(base, 'relocated-cache');
    const explicitStore = join(base, 'explicit-store');
    const canary = join(base, 'store-sibling-canary');
    for (const dir of [project, targetDir, dependencyDir, home]) mkdirSync(dir, { recursive: true });
    writeFileSync(canary, 'plain Node must read this; the jail must not');

    const target = `relocated-store-target-${isEnv ? 'env' : 'npmrc'}`;
    const dependency = `relocated-store-dependency-${isEnv ? 'env' : 'npmrc'}`;
    writeFileSync(join(dependencyDir, 'package.json'), JSON.stringify({ name: dependency, version: '1.0.0', main: 'index.cjs' }));
    writeFileSync(join(dependencyDir, 'index.cjs'), "module.exports = 'dependency-loaded';\n");
    const dependencyArchive = join(base, 'dependency.tgz');
    execFileSync('tar', ['-czf', dependencyArchive, '-C', base, 'dependency']);
    const dependencyBytes = readFileSync(dependencyArchive);

    writeFileSync(join(targetDir, 'package.json'), JSON.stringify({
      name: target, version: '1.0.0', dependencies: { [dependency]: '1.0.0' },
      scripts: { postinstall: 'node probe.cjs' },
    }));
    writeFileSync(join(targetDir, 'probe.cjs'), `
      const fs = require('node:fs');
      const dependency = require(${JSON.stringify(dependency)});
      const dependencyPath = fs.realpathSync(require.resolve(${JSON.stringify(dependency)}));
      let canary;
      try { fs.readFileSync(${JSON.stringify(canary)}, 'utf8'); canary = 'read'; }
      catch (error) { canary = error.code; }
      fs.writeFileSync('relocated-store-proof.json', JSON.stringify({ dependency, dependencyPath, canary }));
    `);
    writeFileSync(join(targetDir, 'plain-control.cjs'), `
      const fs = require('node:fs');
      if (require(${JSON.stringify(dependency)}) !== 'dependency-loaded') process.exit(2);
      process.exit(fs.readFileSync(${JSON.stringify(canary)}, 'utf8').length > 0 ? 0 : 3);
    `);
    const targetArchive = join(base, 'target.tgz');
    execFileSync('tar', ['-czf', targetArchive, '-C', base, 'target']);
    const targetBytes = readFileSync(targetArchive);
    const server = createServer((req, res) => {
      const tarball = (name, bytes) => ({
        name, version: '1.0.0',
        ...(name === target ? { dependencies: { [dependency]: '1.0.0' }, scripts: { postinstall: 'node probe.cjs' } } : {}),
        dist: {
          tarball: `http://127.0.0.1:${server.address().port}/${name}.tgz`,
          integrity: `sha512-${createHash('sha512').update(bytes).digest('base64')}`,
        },
      });
      if (req.url === `/${target}`) {
        res.setHeader('content-type', 'application/json');
        res.end(JSON.stringify({ name: target, 'dist-tags': { latest: '1.0.0' }, time: { '1.0.0': '2025-01-01T00:00:00.000Z' }, versions: { '1.0.0': tarball(target, targetBytes) } }));
      } else if (req.url === `/${dependency}`) {
        res.setHeader('content-type', 'application/json');
        res.end(JSON.stringify({ name: dependency, 'dist-tags': { latest: '1.0.0' }, time: { '1.0.0': '2025-01-01T00:00:00.000Z' }, versions: { '1.0.0': tarball(dependency, dependencyBytes) } }));
      } else if (req.url === `/${target}.tgz`) res.end(targetBytes);
      else if (req.url === `/${dependency}.tgz`) res.end(dependencyBytes);
      else res.writeHead(404).end();
    });
    await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
    writeFileSync(join(project, 'package.json'), JSON.stringify({
      name: `consumer-${target}`, private: true, dependencies: { [target]: '1.0.0' },
      allowScripts: { [`${target}@1.0.0`]: true },
    }));
    const npmrc = [`registry=http://127.0.0.1:${server.address().port}/`, 'enable-global-virtual-store=true', 'node-linker=isolated', 'minimum-release-age=0', 'trust-policy=off'];
    if (!isEnv) npmrc.push(`global-virtual-store-dir=${explicitStore.replaceAll('\\', '/')}`);
    writeFileSync(join(project, '.npmrc'), `${npmrc.join('\n')}\n`);

    const {
      NUB_CACHE_DIR: _inheritedCacheDir,
      npm_config_cache_dir: _inheritedNpmCacheDir,
      NPM_CONFIG_CACHE_DIR: _inheritedNpmCacheDirUpper,
      npm_config_global_virtual_store_dir: _inheritedGlobalStore,
      NPM_CONFIG_GLOBAL_VIRTUAL_STORE_DIR: _inheritedGlobalStoreUpper,
      ...withoutInheritedCacheDir
    } = process.env;
    const env = {
      ...withoutInheritedCacheDir,
      HOME: home, USERPROFILE: home, XDG_CONFIG_HOME: join(home, 'config'),
      XDG_CACHE_HOME: join(home, 'cache'), XDG_DATA_HOME: join(home, 'data'),
      NODE_EXECUTABLE: process.execPath, CI: '1', NO_COLOR: '1',
      ...(isEnv ? { NUB_CACHE_DIR: cacheDir } : {}),
    };
    let run;
    try {
      try {
        const output = await execFileAsync(binary, ['install'], { cwd: project, env, encoding: 'utf8', timeout: 120_000 });
        run = { ...output, status: 0 };
      } catch (error) {
        run = { stdout: error.stdout ?? '', stderr: error.stderr ?? '', status: error.code, error };
      }
      const log = `${run.stdout}\n${run.stderr}`;
      writeFileSync(join(base, 'install.log'), log);
      assert.ifError(run.error);
      assert.equal(run.status, 0, log);

      const proofPath = join(project, 'node_modules', target, 'relocated-store-proof.json');
      assert.ok(existsSync(proofPath), `lifecycle artifact missing\n${log}`);
      const proof = JSON.parse(readFileSync(proofPath, 'utf8'));
      assert.equal(proof.dependency, 'dependency-loaded', `cross-cell dependency was not loaded: ${JSON.stringify(proof)}`);
      assert.match(String(proof.canary), /^(EACCES|EPERM|ENOENT)$/, `the literal outside-store canary must be denied: ${JSON.stringify(proof)}`);
      const expectedStore = isEnv ? join(cacheDir, 'store', 'v1') : join(explicitStore, 'v1');
      const targetPath = realpathSync(join(project, 'node_modules', target));
      assert.ok(contains(expectedStore, targetPath), `target escaped relocated global store: ${targetPath} not in ${expectedStore}`);
      assert.ok(contains(expectedStore, proof.dependencyPath), `dependency escaped relocated global store: ${proof.dependencyPath} not in ${expectedStore}`);
      assert.notEqual(gvsCell(targetPath), gvsCell(proof.dependencyPath), `dependency stayed in target's own GVS cell: ${JSON.stringify(proof)}`);

      const control = spawnSync(process.execPath, [join(targetPath, 'plain-control.cjs')], { cwd: targetPath, encoding: 'utf8', timeout: 30_000 });
      assert.ifError(control.error);
      assert.equal(control.status, 0, `plain Node control must load the dependency and read the canary\n${control.stdout}\n${control.stderr}`);
    } finally {
      server.closeAllConnections();
      await new Promise(resolve => server.close(resolve));
    }
  });
}

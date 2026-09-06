import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFile, execFileSync } from 'node:child_process';
import { lstatSync, mkdtempSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { promisify } from 'node:util';
import { pathToFileURL } from 'node:url';
import { test } from 'node:test';

for (const sourceKind of ['tarball', 'git']) {
test(`${sourceKind} private registry bootstrap keeps credentials outside the jail grants`, async () => {
  const root = mkdtempSync(join(tmpdir(), 'nub-jail-registry-'));
  console.log(`Fixture root: ${root}`);
  const project = join(root, 'project');
  const home = join(root, 'home');
  const tool = join(root, 'tool', 'package');
  const dep = join(root, 'dependency', 'package');
  const outsideCredential = join(root, 'outside-checkout-credential');
  for (const dir of [project, home, tool, dep]) mkdirSync(dir, { recursive: true });
  writeFileSync(outsideCredential, 'outside-checkout-secret');
  writeFileSync(join(tool, 'package.json'), JSON.stringify({ name: 'node-gyp', version: '12.0.0', bin: { 'node-gyp': 'bin/node-gyp.js' } }));
  mkdirSync(join(tool, 'bin'));
  writeFileSync(join(tool, 'bin', 'node-gyp.js'), '#!/usr/bin/env node\nconsole.log("private-node-gyp-ran");\n', { mode: 0o755 });
  const toolArchive = join(root, 'tool.tgz');
  execFileSync('tar', ['-czf', '../tool.tgz', 'package'], { cwd: join(root, 'tool') });
  const bytes = readFileSync(toolArchive);
  const requests = [];
  const server = createServer((req, res) => {
    requests.push({ url: req.url, auth: req.headers.authorization });
    if (req.headers.authorization !== 'Bearer fixture-registry-secret') {
      res.writeHead(401).end('authentication required');
    } else if (req.url === '/node-gyp') {
      res.setHeader('content-type', 'application/json');
      res.end(JSON.stringify({
        name: 'node-gyp', 'dist-tags': { latest: '12.0.0' },
        time: { '12.0.0': '2025-01-01T00:00:00.000Z' },
        versions: { '12.0.0': { name: 'node-gyp', version: '12.0.0', bin: { 'node-gyp': 'bin/node-gyp.js' },
          dist: { tarball: `http://127.0.0.1:${server.address().port}/tool.tgz`,
            integrity: `sha512-${createHash('sha512').update(bytes).digest('base64')}` } } },
      }));
    } else if (req.url === '/tool.tgz') {
      res.end(bytes);
    } else {
      res.writeHead(404).end();
    }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  try {
    const port = server.address().port;
    writeFileSync(join(project, '.npmrc'), `registry=http://127.0.0.1:${port}/\n//127.0.0.1:${port}/:_authToken=fixture-registry-secret\nfetch-retries=0\nnode-linker=isolated\n`);
    writeFileSync(join(dep, 'package.json'), JSON.stringify({ name: 'private-bootstrap-probe', version: '1.0.0', scripts: { [sourceKind === 'git' ? 'prepare' : 'install']: 'node probe.cjs' } }));
    const cachedConfig = join(home, 'cache', 'nub', 'pm', 'tools', 'node-gyp', 'v12', '.npmrc');
    writeFileSync(join(dep, 'probe.cjs'), `
      const fs = require('node:fs');
      const output = require('node:child_process').execFileSync(process.execPath, [process.env.npm_config_node_gyp, '--version'], {encoding:'utf8'});
      let config;
      try { config = fs.readFileSync(${JSON.stringify(cachedConfig)}, 'utf8'); } catch (e) { config = e.code; }
      fs.writeFileSync('proof.json', JSON.stringify({output, config}));
    `);
    let source;
    let nestedApproval;
    if (sourceKind === 'git') {
      const nested = join(root, 'nested');
      mkdirSync(nested);
      writeFileSync(join(nested, 'package.json'), JSON.stringify({name:'nested-probe', version:'0.0.0', scripts:{install:'node probe.cjs'}}));
      writeFileSync(join(nested, 'probe.cjs'), `require('fs').writeFileSync('proof.json', JSON.stringify({token:process.env.AWS_SECRET_ACCESS_KEY ?? null}));`);
      const archive = join(root, 'nested.tgz');
      execFileSync('tar', ['-czf', 'nested.tgz', 'nested'], { cwd: root });
      const nestedSource = `file:${archive.replaceAll('\\', '/')}`;
      nestedApproval = `nested-probe@${nestedSource}`;
      const manifest = JSON.parse(readFileSync(join(dep, 'package.json'), 'utf8'));
      manifest.scripts['pnpm:devPreinstall'] = 'node root-hook.cjs pnpm:devPreinstall && node early.cjs';
      manifest.scripts.preinstall = 'node root-hook.cjs preinstall';
      manifest.scripts.install = 'node root-hook.cjs install';
      manifest.scripts.postinstall = 'node root-hook.cjs postinstall';
      manifest.scripts.prepare = 'node root-hook.cjs prepare && node probe.cjs';
      writeFileSync(join(dep, 'early.cjs'), `require('fs').writeFileSync('early-proof.json', JSON.stringify({ran:true, token:process.env.AWS_SECRET_ACCESS_KEY ?? null}));`);
      manifest.devDependencies = {'nested-probe':nestedSource};
      manifest.allowScripts = {'private-bootstrap-probe':'no-jail', [nestedApproval]:'no-jail'};
      writeFileSync(join(dep, 'package.json'), JSON.stringify(manifest));
      writeFileSync(join(dep, 'root-hook.cjs'), `
        const fs = require('node:fs');
        let outsideReadable = true;
        try { fs.readFileSync(${JSON.stringify(outsideCredential)}, 'utf8'); } catch { outsideReadable = false; }
        fs.mkdirSync('.root-hook-proofs', {recursive:true});
        fs.writeFileSync('.root-hook-proofs/' + process.argv[2].replaceAll(':', '-') + '.json', JSON.stringify({
          hook: process.argv[2],
          token: process.env.AWS_SECRET_ACCESS_KEY ?? null,
          outsideReadable,
        }));
      `);
      writeFileSync(join(dep, 'probe.cjs'), readFileSync(join(dep, 'probe.cjs'), 'utf8') + `\nconst nested = require('nested-probe/proof.json'); fs.writeFileSync('nested-proof.json', JSON.stringify(nested));`);
      writeFileSync(join(dep, '.npmrc'), readFileSync(join(project, '.npmrc')));
      writeFileSync(join(dep, 'nub.jsonc'), JSON.stringify({ install: { buildJail: false } }));
      execFileSync('git', ['init', '-q'], { cwd: dep });
      execFileSync('git', ['add', '.'], { cwd: dep });
      execFileSync('git', ['-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'fixture'], { cwd: dep });
      const commit = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: dep, encoding: 'utf8' }).trim();
      source = `git+${pathToFileURL(dep).href}#${commit}`;
    } else {
      const archive = join(root, 'dep.tgz');
      execFileSync('tar', ['-czf', '../dep.tgz', 'package'], { cwd: join(root, 'dependency') });
      source = `file:${archive.replaceAll('\\', '/')}`;
    }
    writeFileSync(join(project, 'package.json'), JSON.stringify({ name: 'registry-consumer', private: true,
      dependencies: { 'private-bootstrap-probe': source },
      allowScripts: { [`private-bootstrap-probe@${source}`]: true, ...(nestedApproval ? {[nestedApproval]:true} : {}) } }));
    const result = await promisify(execFile)(resolve(process.env.NUB_BIN), ['install'], {
      cwd: project, timeout: 60_000,
      env: { ...process.env, HOME: home, USERPROFILE: home, XDG_CONFIG_HOME: join(home, 'config'),
        XDG_CACHE_HOME: join(home, 'cache'), XDG_DATA_HOME: join(home, 'data'),
        NODE_EXECUTABLE: process.execPath, AWS_SECRET_ACCESS_KEY:'nested-fixture-token',
        CI: '1', NO_COLOR: '1' },
    });
    writeFileSync(join(root, 'install.log'), result.stdout + result.stderr);
    const proof = JSON.parse(readFileSync(join(project, 'node_modules', 'private-bootstrap-probe', 'proof.json'), 'utf8'));
    assert.match(proof.output, /private-node-gyp-ran/);
    if (sourceKind === 'git') {
      const early = JSON.parse(readFileSync(join(project, 'node_modules', 'private-bootstrap-probe', 'early-proof.json'), 'utf8'));
      assert.deepEqual(early, {ran:true, token:null}, 'fetched early hooks must run with confinement');
      for (const hook of ['pnpm:devPreinstall', 'preinstall', 'install', 'postinstall', 'prepare']) {
        const rootHook = JSON.parse(readFileSync(join(project, 'node_modules', 'private-bootstrap-probe', '.root-hook-proofs', `${hook.replaceAll(':', '-')}.json`), 'utf8'));
        assert.deepEqual(rootHook, {hook, token:null, outsideReadable:false},
          `fetched ${hook} must ignore its own opt-out and stay inside its checkout`);
      }
      const nested = JSON.parse(readFileSync(join(project, 'node_modules', 'private-bootstrap-probe', 'nested-proof.json'), 'utf8'));
      assert.equal(nested.token, null, 'fetched manifests cannot opt their dependencies out of confinement');
    }
    assert.equal(lstatSync(join(home, 'cache', 'nub', 'pm', 'tools', 'node-gyp', 'v12', 'node_modules', 'node-gyp')).isSymbolicLink(), false,
      'published tool must not retain a junction into private staging');
    assert.ok(!proof.config.includes('fixture-registry-secret'), JSON.stringify(proof));
    assert.ok(requests.some(req => req.url === '/node-gyp'), JSON.stringify(requests));
    assert.ok(requests.some(req => req.url === '/tool.tgz'), JSON.stringify(requests));
    assert.ok(requests.filter(req => req.url === '/node-gyp' || req.url === '/tool.tgz')
      .every(req => req.auth === 'Bearer fixture-registry-secret'), JSON.stringify(requests));
  } finally {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    writeFileSync(join(root, 'requests.json'), JSON.stringify(requests, null, 2));
  }
});
}

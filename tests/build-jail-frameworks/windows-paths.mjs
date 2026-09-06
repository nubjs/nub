import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { closeSync, mkdirSync, mkdtempSync, openSync, readFileSync, realpathSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { cases } from './cases.mjs';

assert.equal(process.platform, 'win32');
const report = resolve(process.env.FRAMEWORK_PATH_REPORT);
mkdirSync(report, { recursive: true });
const npm = join(dirname(process.execPath), 'node_modules/npm/bin/npm-cli.js');
const results = [];

for (const fixture of cases.filter(c => ['qwik', 'sveltekit', 'react-router'].includes(c.name))) {
  const temporary = mkdtempSync(join(tmpdir(), 'framework-path-control-'));
  const project = join(temporary, 'project');
  mkdirSync(project);
  const canonical = realpathSync.native(project);
  const manifest = { name: 'framework-path-control', private: true, type: 'module',
    dependencies: fixture.dependencies, scripts: { build: fixture.build, ...(fixture.prepare ? { prepare: fixture.prepare } : {}) } };
  writeFileSync(join(project, 'package.json'), JSON.stringify(manifest));
  for (const [path, content] of Object.entries(fixture.files)) {
    mkdirSync(dirname(join(project, path)), { recursive: true });
    writeFileSync(join(project, path), content);
  }
  const env = { ...process.env, CI: '1', NO_COLOR: '1', npm_config_cache: join(temporary, 'cache') };
  for (const key of ['NODE_OPTIONS', 'NODE_COMPAT', 'NUB_CACHE_DIR']) delete env[key];
  function command(args, cwd, label) {
    const fd = openSync(join(report, `${fixture.name}-${label}.log`), 'w');
    try {
      const result = spawnSync(process.execPath, [npm, ...args], { cwd, env, stdio: ['ignore', fd, fd], timeout: 600_000 });
      assert.equal(result.error, undefined, `${fixture.name} ${label}: ${result.error}`);
      return result.status;
    } finally { closeSync(fd); }
  }
  assert.equal(command(['install', '--no-audit', '--no-fund'], project, 'install'), 0);
  const shortStatus = command(['run', 'build'], project, 'short');
  const canonicalStatus = command(['run', 'build'], canonical, 'canonical');
  const lock = readFileSync(join(project, 'package-lock.json'));
  writeFileSync(join(report, `${fixture.name}-package-lock.json`), lock);
  results.push({ name: fixture.name, project, canonical, node: process.version, npm,
    shortStatus, canonicalStatus, lockfileSha256: createHash('sha256').update(lock).digest('hex') });
  writeFileSync(join(report, 'results.json'), JSON.stringify(results, null, 2));
  console.log(JSON.stringify(results.at(-1)));
}
assert.ok(results.every(row => row.canonicalStatus === 0), 'canonical-path npm controls must build');

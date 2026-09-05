import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { test } from 'node:test';

const root = mkdtempSync(join(tmpdir(), 'nub-jail-fetched-native-'));
console.log(`Fixture root: ${root}`);
for (const confined of [false, true]) {
  test(`fetched native dependency ${confined ? 'jailed' : 'control'}`, () => {
    const home = join(root, String(confined));
    const dep = join(home, 'dep');
    const project = join(home, 'p');
    mkdirSync(dep, { recursive: true });
    mkdirSync(project);
    writeFileSync(join(dep, 'package.json'), JSON.stringify({
      name: 'fetched-native-parent', version: '0.0.0',
      devDependencies: { 'cpu-features': '0.0.10' },
      allowScripts: { 'cpu-features': 'no-jail' },
      scripts: { prepare: 'node proof.cjs' },
    }));
    writeFileSync(join(dep, 'proof.cjs'), `require('fs').writeFileSync('proof.json', JSON.stringify({arch:require('cpu-features')().arch}));`);
    execFileSync('git', ['init', '-q'], { cwd: dep });
    execFileSync('git', ['add', '.'], { cwd: dep });
    execFileSync('git', ['-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'fixture'], { cwd: dep });
    const commit = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: dep, encoding: 'utf8' }).trim();
    const source = `git+${pathToFileURL(dep).href}#${commit}`;
    writeFileSync(join(project, 'package.json'), JSON.stringify({
      name: 'native-consumer', private: true,
      dependencies: { 'fetched-native-parent': source },
      allowScripts: { [`fetched-native-parent@${source}`]: true, 'cpu-features': true },
    }));
    writeFileSync(join(project, 'nub.jsonc'), JSON.stringify({ install: { buildJail: confined } }));
    const result = spawnSync(resolve(process.env.NUB_BIN), ['install'], {
      cwd: project, timeout: 300_000, encoding: 'utf8',
      env: { ...process.env, HOME: home, USERPROFILE: home,
        XDG_CONFIG_HOME: join(home, 'config'), XDG_CACHE_HOME: join(home, 'cache'),
        XDG_DATA_HOME: join(home, 'data'), NODE_EXECUTABLE: process.execPath,
        CI: '1', NO_COLOR: '1', NUB_JAIL_DUMP_POLICY: '1' },
    });
    const log = `${result.stdout}\n${result.stderr}`;
    writeFileSync(join(home, 'install.log'), log);
    assert.ifError(result.error);
    assert.equal(result.status, 0, log);
    if (confined) assert.match(log, /JAILDUMP pkg=Some\("cpu-features"\)/);
    else assert.match(log, /running without the build sandbox/);
    const proof = JSON.parse(readFileSync(join(project, 'node_modules', 'fetched-native-parent', 'proof.json')));
    assert.equal(typeof proof.arch, 'string');
  });
}

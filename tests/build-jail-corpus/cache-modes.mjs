import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { test } from 'node:test';

const binary = resolve(process.env.NUB_BIN);
const root = mkdtempSync(join(tmpdir(), 'nub-jail-cache-'));
console.log(`Fixture root: ${root}`);

for (const firstConfined of [true, false]) {
  test(`cached lifecycle switches from ${firstConfined ? 'confined' : 'unconfined'}`, () => {
    const base = join(root, String(firstConfined));
    const project = join(base, 'project');
    const home = join(base, 'home');
    const pkg = join(base, 'package');
    for (const dir of [project, home, pkg]) mkdirSync(dir, { recursive: true });
    const name = `jail-cache-${firstConfined}`;
    writeFileSync(join(pkg, 'package.json'), JSON.stringify({name, version: '0.0.0',
      scripts: {postinstall: 'node build.cjs'}}));
    writeFileSync(join(pkg, 'build.cjs'), `require('fs').writeFileSync('built.json', JSON.stringify({token:process.env.AWS_SECRET_ACCESS_KEY ?? null, home:process.env.HOME}));`);
    const archive = join(base, 'dep.tgz');
    const tar = spawnSync('tar', ['-czf', archive, '-C', base, 'package']);
    assert.equal(tar.status, 0);
    const spec = `file:${archive.replaceAll('\\', '/')}`;
    writeFileSync(join(project, 'package.json'), JSON.stringify({name: 'cache-consumer', private: true,
      dependencies: {[name]: spec}, allowScripts: {[`${name}@${spec}`]: true}}));
    writeFileSync(join(project, '.npmrc'), 'side-effects-cache=true\n');
    const env = {...process.env, HOME: home, USERPROFILE: home,
      XDG_CONFIG_HOME: join(home, 'config'), XDG_CACHE_HOME: join(home, 'cache'),
      XDG_DATA_HOME: join(home, 'data'), NODE_EXECUTABLE: process.execPath,
      AWS_SECRET_ACCESS_KEY: 'cache-fixture-token', CI: '1', NO_COLOR: '1', NUB_JAIL_DUMP_POLICY: '1'};
    for (const [index, confined] of [firstConfined, !firstConfined].entries()) {
      if (index) rmSync(join(project, 'node_modules'), {recursive: true, force: true});
      writeFileSync(join(project, 'nub.jsonc'), JSON.stringify({install: {buildJail: confined}}));
      const result = spawnSync(binary, ['install'], {cwd: project, env, encoding: 'utf8', timeout: 120000});
      writeFileSync(join(base, `install-${index}.log`), `${result.stdout}\n${result.stderr}`);
      assert.ifError(result.error);
      assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
      const built = JSON.parse(readFileSync(join(project, 'node_modules', name, 'built.json'), 'utf8'));
      assert.equal(built.token, confined ? null : 'cache-fixture-token', JSON.stringify(built));
    }
    const artifact = join(project, 'node_modules', name, 'built.json');
    rmSync(artifact);
    const rebuilt = spawnSync(binary, ['rebuild', name], {cwd: project, env, encoding: 'utf8', timeout: 120000});
    writeFileSync(join(base, 'rebuild.log'), `${rebuilt.stdout}\n${rebuilt.stderr}`);
    assert.ifError(rebuilt.error);
    assert.equal(rebuilt.status, 0, `${rebuilt.stdout}\n${rebuilt.stderr}`);
    const built = JSON.parse(readFileSync(artifact, 'utf8'));
    assert.equal(built.token, !firstConfined ? null : 'cache-fixture-token', JSON.stringify(built));
    writeFileSync(join(project, 'nub.jsonc'), JSON.stringify({install:{buildJail:true}}));
    writeFileSync(join(project, 'node_modules', name, 'build.cjs'), "throw new Error('intentional rebuild failure');\n");
    const failed = spawnSync(binary, ['rebuild', name], {cwd: project, env, encoding: 'utf8', timeout: 120000});
    writeFileSync(join(base, 'rebuild-failure.log'), `${failed.stdout}\n${failed.stderr}`);
    assert.ifError(failed.error);
    assert.notEqual(failed.status, 0);
    assert.match(failed.stdout + failed.stderr, /failed while jailed/);
    assert.match(failed.stdout + failed.stderr, /allowScripts/);
  });
}

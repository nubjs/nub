import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { test } from 'node:test';

const root = mkdtempSync(join(tmpdir(), 'nub-jail-layouts-'));
console.log(`Fixture root: ${root}`);
const linkers = process.env.CORPUS_LINKER ? [process.env.CORPUS_LINKER] : ['isolated', 'hoisted'];
for (const linker of linkers) {
  for (const confined of [false, true]) {
    test(`${linker} ${confined ? 'jailed' : 'control'} resolves the dependency's own version`, () => {
      const home = join(root, `${linker}-${confined}`);
      const project = join(home, 'p');
      mkdirSync(project, { recursive: true });
      function pack(label, manifest, files) {
        const dir = join(home, label);
        mkdirSync(dir);
        writeFileSync(join(dir, 'package.json'), JSON.stringify(manifest));
        for (const [name, content] of Object.entries(files)) writeFileSync(join(dir, name), content);
        const archive = join(home, `${label}.tgz`);
        execFileSync('tar', ['-czf', `${label}.tgz`, label], { cwd: home });
        return `file:${archive.replaceAll('\\', '/')}`;
      }
      const leaf = 'is-number';
      const parent = pack('parent', {
        name: 'jail-layout-parent', version: '0.0.0', dependencies: { [leaf]: '6.0.0' },
        scripts: { postinstall: 'node build.cjs' },
      }, { 'build.cjs': `
        const assert = require('node:assert/strict');
        (async () => {
          const cjs = require('${leaf}/package.json').version;
          const esm = (await import('${leaf}')).default === require('${leaf}');
          const child = require('node:child_process').execFileSync(process.execPath, ['-p', "require('${leaf}/package.json').version"], {encoding:'utf8'}).trim();
          assert.deepEqual([cjs, esm, child], ['6.0.0', true, '6.0.0']);
          require('node:fs').writeFileSync('proof.json', JSON.stringify({cjs, esm, child}));
        })().catch(error => { console.error(error); process.exitCode = 1; });
      ` });
      writeFileSync(join(project, 'package.json'), JSON.stringify({
        name: 'layout-consumer', private: true,
        dependencies: { 'jail-layout-parent': parent, [leaf]: '7.0.0' },
        allowScripts: { [`jail-layout-parent@${parent}`]: true },
      }));
      writeFileSync(join(project, 'nub.jsonc'), JSON.stringify({ install: { buildJail: confined } }));
      const env = { ...process.env, HOME: home, USERPROFILE: home,
        XDG_CONFIG_HOME: join(home, 'config'), XDG_CACHE_HOME: join(home, 'cache'),
        XDG_DATA_HOME: join(home, 'data'), NODE_EXECUTABLE: process.execPath,
        CI: '1', NO_COLOR: '1', NUB_JAIL_DUMP_POLICY: '1' };
      const result = spawnSync(resolve(process.env.NUB_BIN), ['install', '--node-linker', linker], {
        cwd: project, env, encoding: 'utf8', timeout: 120_000,
      });
      const log = `${result.stdout}\n${result.stderr}`;
      writeFileSync(join(home, 'install.log'), log);
      assert.ifError(result.error);
      assert.equal(result.status, 0, log);
      if (confined) assert.match(log, /JAILDUMP pkg=Some\("jail-layout-parent"\)/);
      else assert.match(log, /running without the build sandbox/);
      assert.deepEqual(JSON.parse(readFileSync(join(project, 'node_modules', 'jail-layout-parent', 'proof.json'))), {cjs:'6.0.0', esm:true, child:'6.0.0'});
      assert.equal(JSON.parse(readFileSync(join(project, 'node_modules', leaf, 'package.json'))).version, '7.0.0');
    });
  }
}

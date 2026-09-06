import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join, relative, resolve } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { test } from 'node:test';

const root = mkdtempSync(join(tmpdir(), 'nub-jail-descendants-'));
console.log(`Fixture root: ${root}`);

async function until(predicate, label, timeout = 30_000) {
  const deadline = Date.now() + timeout;
  while (!predicate()) {
    assert.ok(Date.now() < deadline, `timed out: ${label}`);
    await delay(50);
  }
}

for (const profile of process.platform === 'win32' ? ['baseline', 'catalog-full-disk'] : ['baseline']) {
for (const confined of [false, true]) {
  for (const mode of ['success', 'failure', 'cancel', 'sibling-failure']) {
    test(`${profile} ${confined ? 'jailed' : 'control'} descendants after ${mode}`, { timeout: 90_000 }, async () => {
      const base = join(root, `${profile}-${confined}-${mode}`);
      const project = join(base, 'project');
      const home = join(base, 'home');
      const pkg = join(base, 'package');
      for (const dir of [project, home, pkg]) mkdirSync(dir, { recursive: true });
      const dependencies = {};
      const allowScripts = {};
      function pack(name, dir, script) {
        writeFileSync(join(dir, 'package.json'), JSON.stringify({ name, version: '1.0.0', scripts: { install: 'node install.cjs' } }));
        writeFileSync(join(dir, 'install.cjs'), script);
        const archive = join(base, `${name}.tgz`);
        execFileSync('tar', ['-czf', relative(dirname(dir), archive), basename(dir)], { cwd: dirname(dir) });
        const source = `file:${archive.replaceAll('\\', '/')}`;
        dependencies[name] = source;
        allowScripts[`${name}@${source}`] = true;
      }
      writeFileSync(join(pkg, 'child.cjs'), `
        const fs = require('fs');
        let ticks = 0;
        fs.writeFileSync('child.pid', String(process.pid));
        setInterval(() => fs.writeFileSync('heartbeat', String(++ticks)), 50);
      `);
      // Pizzip has a baked Windows full-disk grant; no development override is needed.
      const name = profile === 'catalog-full-disk' ? 'pizzip' : 'descendant-probe';
      pack(name, pkg, `
        const fs = require('fs');
        fs.writeFileSync('parent.json', JSON.stringify({pid:process.pid, execPath:process.execPath}));
        require('child_process').spawn(process.execPath, ['child.cjs'], {stdio:'ignore'}).unref();
        const timer = setInterval(() => {
          if (!fs.existsSync('heartbeat') || Number(fs.readFileSync('heartbeat')) < 3) return;
          ${mode === 'success' || mode === 'failure' ? `clearInterval(timer); process.exit(${mode === 'success' ? 0 : 7});` : ''}
        }, 50);
      `);
      if (mode === 'sibling-failure') {
        const sibling = join(base, 'sibling');
        mkdirSync(sibling);
        pack('failing-sibling', sibling, `setInterval(() => {if(require('fs').existsSync('fail-now')) process.exit(9)}, 50);`);
      }
      writeFileSync(join(project, 'package.json'), JSON.stringify({ name: 'descendant-consumer', private: true, dependencies, allowScripts }));
      writeFileSync(join(project, 'nub.jsonc'), JSON.stringify({ install: { buildJail: confined } }));
      writeFileSync(join(project, '.npmrc'), 'child-concurrency=2\n');
      const env = { ...process.env, HOME: home, USERPROFILE: home,
        XDG_CONFIG_HOME: join(home, 'config'), XDG_CACHE_HOME: join(home, 'cache'), XDG_DATA_HOME: join(home, 'data'),
        NODE_EXECUTABLE: process.execPath, CI: '1', NO_COLOR: '1', NUB_JAIL_DUMP_POLICY:'1' };
      const child = spawn(resolve(process.env.NUB_BIN), ['install'], { cwd: project, env, stdio: ['ignore', 'pipe', 'pipe'] });
      let output = '';
      child.stdout.on('data', data => { output += data; });
      child.stderr.on('data', data => { output += data; });
      let exited = false;
      const completed = new Promise(resolve => child.once('exit', (code, signal) => { exited = true; resolve({ code, signal }); }));
      const installed = join(project, 'node_modules', name);
      const heartbeat = join(installed, 'heartbeat');
      let descendant;
      try {
        await until(() => {
          if (exited && !existsSync(heartbeat)) assert.fail(`installer exited before lifecycle progress: ${output}`);
          return existsSync(heartbeat) && Number(readFileSync(heartbeat)) >= 3;
        }, 'descendant made progress');
        descendant = Number(readFileSync(join(installed, 'child.pid')));
        if (mode === 'cancel') child.kill('SIGTERM');
        if (mode === 'sibling-failure') writeFileSync(join(project, 'node_modules', 'failing-sibling', 'fail-now'), 'fail');
        await until(() => exited, 'installer completed after lifecycle exit/cancellation', 8_000);
        const status = await completed;
        if (profile === 'catalog-full-disk' && confined) assert.match(output, /JAILDUMP fs default=Allow rules=0/);
        if (mode === 'success') assert.equal(status.code, 0, output);
        else assert.notEqual(status.code, 0, output);
        await delay(250);
        const finalHeartbeat = readFileSync(heartbeat, 'utf8');
        await delay(500);
        assert.equal(readFileSync(heartbeat, 'utf8'), finalHeartbeat, 'descendant kept running after install returned');
      } finally {
        if (!exited) child.kill('SIGTERM');
        if (descendant) { try { process.kill(descendant, 'SIGKILL'); } catch {} }
        const parentRecord = join(installed, 'parent.json');
        if (existsSync(parentRecord)) {
          const parent = JSON.parse(readFileSync(parentRecord, 'utf8'));
          try { process.kill(parent.pid, 'SIGKILL'); } catch {}
        }
        await completed;
        writeFileSync(join(base, 'install.log'), output);
      }
    });
  }
}
}

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { test } from 'node:test';

const root = mkdtempSync(join(tmpdir(), 'nub-jail-network-'));
console.log(`Fixture root: ${root}`);
// These unbanded catalog entries deny networking on their respective platforms.
const name = process.platform === 'win32' ? 'blake-hash' : 'pizzip';
for (const confined of [false, true]) {
  test(`descendant network ${confined ? 'jailed' : 'control'}`, () => {
    const home = join(root, String(confined));
    const project = join(home, 'p');
    const pkg = join(home, 'package');
    for (const path of [project, pkg]) mkdirSync(path, { recursive: true });
    writeFileSync(join(pkg, 'package.json'), JSON.stringify({ name, version: '1.0.0', scripts: { install: 'node install.cjs' } }));
    writeFileSync(join(pkg, 'install.cjs'), `
      const child = require('child_process').spawn(process.execPath, ['probe.cjs'], {env:{},stdio:'pipe'});
      child.stdout.resume(); child.stderr.resume();
      child.on('error', error => {throw error;});
      child.on('exit', code => {process.exitCode = code ?? 1;});
    `);
    writeFileSync(join(pkg, 'probe.cjs'), `
      const fs = require('fs');
      const socket = require('net').connect({host:'1.1.1.1',port:443});
      const finish = result => {fs.writeFileSync('network.json',JSON.stringify(result));socket.destroy();};
      socket.once('connect', () => finish({connected:true}));
      socket.once('error', error => finish({connected:false,code:error.code,reason:error.nubReason,message:error.message}));
      socket.setTimeout(10000, () => finish({timeout:true}));
    `);
    const archive = join(home, 'package.tgz');
    const pack = spawnSync('tar', ['-czf', archive, '-C', home, 'package']);
    assert.equal(pack.status, 0);
    const source = `file:${archive.replaceAll('\\', '/')}`;
    writeFileSync(join(project, 'package.json'), JSON.stringify({ name:'network-consumer',private:true,
      dependencies:{[name]:source},allowScripts:{[`${name}@${source}`]:true} }));
    writeFileSync(join(project, 'nub.jsonc'), JSON.stringify({install:{buildJail:confined}}));
    const run = spawnSync(resolve(process.env.NUB_BIN), ['install'], {
      cwd:project, env:{...process.env,HOME:home,USERPROFILE:home,XDG_CONFIG_HOME:join(home,'config'),
        XDG_CACHE_HOME:join(home,'cache'),XDG_DATA_HOME:join(home,'data'),NODE_EXECUTABLE:process.execPath,CI:'1',NO_COLOR:'1'},
      encoding:'utf8',timeout:120000,
    });
    writeFileSync(join(home,'install.log'),run.stdout+run.stderr);
    assert.equal(run.error,undefined);
    assert.equal(run.status,0,run.stdout+run.stderr);
    const result = JSON.parse(readFileSync(join(project,'node_modules',name,'network.json')));
    assert.equal(result.timeout,undefined,JSON.stringify(result));
    assert.equal(result.connected,!confined,JSON.stringify(result));
    if (confined && process.platform === 'win32') {
      assert.equal(result.reason,'ERR_NUB_JAIL_NET_DENIED');
      assert.match(result.message,/allowScripts/);
    }
  });
}

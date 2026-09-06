import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';

for (const mode of ['blocked', 'exotic', 'ordinary', 'missing-log', 'timeout', 'cleanup-failure']) {
  test(`reference control ${mode}`, () => {
    const root = mkdtempSync(join(tmpdir(), 'nub-reference-test-'));
    let report;
    const descendantPid = join(root, 'descendant.pid');
    try {
      mkdirSync(join(root, 'logs'));
      const marker = join(root, 'executed');
      const npm = join(root, 'npm.cjs');
      const ready = join(root, 'ready');
      writeFileSync(npm, mode === 'timeout'
        ? "const { spawn } = require('child_process'); const fs = require('fs'); const child = spawn(process.execPath, ['-e', 'setInterval(()=>{},1000)'], { stdio: 'ignore' }); fs.writeFileSync(process.env.REFERENCE_DESCENDANT_PID, String(child.pid)); fs.writeFileSync(process.env.REFERENCE_READY, 'ready'); setInterval(()=>{},1000);"
        : mode === 'cleanup-failure' ? "require('fs').writeFileSync(process.env.REFERENCE_READY, 'ready'); setInterval(()=>{},1000);"
        : `require('fs').writeFileSync(${JSON.stringify(marker)},'yes');process.exit(7);`);
      const timer = join(root, 'timer.cjs');
      writeFileSync(timer, "const fs=require('fs');const cp=require('child_process');const {syncBuiltinESMExports}=require('module');const originalSetTimeout=setTimeout;globalThis.setTimeout=(fn,ms,...args)=>{if(ms!==300_000)return originalSetTimeout(fn,ms,...args);const fire=()=>originalSetTimeout(fn,100,...args);const wait=()=>fs.existsSync(process.env.REFERENCE_READY)?fire():originalSetTimeout(wait,5);return wait();};if(process.env.REFERENCE_INJECT_KILL_FALSE){const spawnSync=cp.spawnSync;cp.spawnSync=function(command,...args){if(String(command).toLowerCase()==='taskkill')return {status:1,error:Object.assign(new Error('injected taskkill failure'),{code:'EACCES'})};return spawnSync.call(this,command,...args);};const kill=cp.ChildProcess.prototype.kill;cp.ChildProcess.prototype.kill=function(signal){kill.call(this,signal);return false;};syncBuiltinESMExports();}");
      writeFileSync(join(root, 'provenance.json'), JSON.stringify({ nodeVersion: process.version, platform: process.platform }));
      writeFileSync(join(root, 'results.json'), JSON.stringify([{ name: 'fixture', version: '1.0.0', verdict: 'CONTROL-FAILED' }]));
      if (mode !== 'missing-log') writeFileSync(join(root, 'logs', 'fixture-control.log'), mode === 'blocked' ? 'ERR_NUB_MALICIOUS_PACKAGE' : mode === 'exotic' ? 'blocked by blockExoticSubdeps' : 'ordinary failure');
      const injectedCleanupFailure = mode === 'cleanup-failure';
      const run = spawnSync(process.execPath, [...(mode === 'timeout' || injectedCleanupFailure ? ['--require', timer] : []), fileURLToPath(new URL('./reference-controls.mjs', import.meta.url))], {
        env: { ...process.env, CORPUS_RESULTS: join(root, 'results.json'), CORPUS_CASE: '', NPM_CLI: npm, REFERENCE_DESCENDANT_PID: descendantPid, REFERENCE_READY: ready, ...(injectedCleanupFailure ? { REFERENCE_INJECT_KILL_FALSE: '1', REFERENCE_CONTROLS_TEST_WINDOWS: '1' } : {}) },
        encoding: 'utf8', timeout: 30_000,
      });
      assert.equal(run.error, undefined);
      report = run.stdout.match(/Fixture root: (.+)/)?.[1].trim();
      assert.ok(report, run.stdout + run.stderr);
      assert.equal(existsSync(marker), mode === 'ordinary');
      if (mode !== 'missing-log') {
        const rows = JSON.parse(readFileSync(join(report, 'results.json'))).results;
        assert.equal(rows.length, 1);
        assert.equal(run.status === 0, !rows[0].cleanupError, run.stdout + run.stderr);
        if (mode === 'blocked' || mode === 'exotic') assert.equal(rows[0].verdict, 'POLICY-BLOCKED');
        else if (mode === 'timeout' || mode === 'cleanup-failure') {
          assert.equal(rows[0].timedOut, true);
          assert.match(run.stdout, /fixture@1\.0\.0: npm TIMEOUT(?: \(cleanup failed: .+\))?/);
          if (mode === 'timeout') {
            const pid = Number(readFileSync(descendantPid, 'utf8'));
            assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' }, 'timeout cleanup must not orphan the npm descendant');
          }
          if (injectedCleanupFailure) {
            assert.notEqual(run.status, 0, 'failed root fallback must end the runner visibly');
            assert.match(rows[0].cleanupError, /root fallback returned false/);
            assert.match(rows[0].error, /root fallback returned false/);
          }
          if (process.platform === 'win32' && rows[0].cleanupError) {
            assert.notEqual(run.status, 0, 'unverified Windows tree cleanup must fail closed');
            assert.match(rows[0].cleanupError, /taskkill failed/);
          }
        } else assert.equal(rows[0].status, 7);
      } else assert.notEqual(run.status, 0, run.stdout + run.stderr);
    } finally {
      if (existsSync(descendantPid)) {
        try { process.kill(Number(readFileSync(descendantPid, 'utf8')), 'SIGKILL'); } catch {}
      }
      rmSync(root, { recursive: true, force: true });
      if (report) rmSync(report, { recursive: true, force: true });
    }
  });
}

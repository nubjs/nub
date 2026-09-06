import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';

for (const mode of ['blocked', 'exotic', 'ordinary', 'missing-log']) {
  test(`reference control ${mode}`, () => {
    const root = mkdtempSync(join(tmpdir(), 'nub-reference-test-'));
    let report;
    try {
      mkdirSync(join(root, 'logs'));
      const marker = join(root, 'executed');
      const npm = join(root, 'npm.cjs');
      writeFileSync(npm, `require('fs').writeFileSync(${JSON.stringify(marker)},'yes');process.exit(7);`);
      writeFileSync(join(root, 'provenance.json'), JSON.stringify({ nodeVersion: process.version, platform: process.platform }));
      writeFileSync(join(root, 'results.json'), JSON.stringify([{ name: 'fixture', version: '1.0.0', verdict: 'CONTROL-FAILED' }]));
      if (mode !== 'missing-log') writeFileSync(join(root, 'logs', 'fixture-control.log'), mode === 'blocked' ? 'ERR_NUB_MALICIOUS_PACKAGE' : mode === 'exotic' ? 'blocked by blockExoticSubdeps' : 'ordinary failure');
      const run = spawnSync(process.execPath, [fileURLToPath(new URL('./reference-controls.mjs', import.meta.url))], {
        env: { ...process.env, CORPUS_RESULTS: join(root, 'results.json'), CORPUS_CASE: '', NPM_CLI: npm },
        encoding: 'utf8', timeout: 30_000,
      });
      assert.equal(run.error, undefined);
      report = run.stdout.match(/Fixture root: (.+)/)?.[1].trim();
      assert.equal(run.status === 0, mode !== 'missing-log', run.stdout + run.stderr);
      assert.equal(existsSync(marker), mode === 'ordinary');
      if (mode !== 'missing-log') {
        const rows = JSON.parse(readFileSync(join(report, 'results.json'))).results;
        assert.equal(rows.length, 1);
        if (mode === 'blocked' || mode === 'exotic') assert.equal(rows[0].verdict, 'POLICY-BLOCKED');
        else assert.equal(rows[0].status, 7);
      }
    } finally {
      rmSync(root, { recursive: true, force: true });
      if (report) rmSync(report, { recursive: true, force: true });
    }
  });
}

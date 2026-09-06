import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { installedArtifacts } from './artifact-inventory.mjs';

test('inventory preserves binaries while comparing generated signing directories', () => {
  const root = mkdtempSync(join(tmpdir(), 'nub-artifact-inventory-'));
  try {
    const arms = ['a', 'b'].map((arm, i) => {
      const dir = join(root, arm);
      const temp = `esy-npm-bigsur-workaround-${i ? 'abc123' : 'DEF456'}`;
      mkdirSync(join(dir, temp), { recursive: true });
      mkdirSync(join(dir, 'bin'));
      writeFileSync(join(dir, 'package.json'), JSON.stringify({ bin: { fixture: 'bin/command' } }));
      writeFileSync(join(dir, 'bin/command'), 'executable');
      writeFileSync(join(dir, temp, 'helper.exe'), 'binary');
      return dir;
    });
    assert.deepEqual(installedArtifacts(arms[0]), installedArtifacts(arms[1]));
    assert.ok(installedArtifacts(arms[0]).some(file => file.path === 'bin/command'));
    writeFileSync(join(arms[1], 'bin/command'), '');
    assert.notDeepEqual(installedArtifacts(arms[0]), installedArtifacts(arms[1]));
    rmSync(join(arms[1], 'bin/command'));
    assert.notDeepEqual(installedArtifacts(arms[0]), installedArtifacts(arms[1]));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

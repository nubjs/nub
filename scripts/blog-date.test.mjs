import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { test } from 'node:test';

const formatter = new URL('../site/src/lib/blog-date.ts', import.meta.url).href;

for (const timezone of ['UTC', 'America/Los_Angeles', 'Pacific/Kiritimati']) {
  test(`blog dates preserve the publication day in ${timezone}`, () => {
    const result = spawnSync(process.execPath, ['--input-type=module', '-e', `
      import { formatDate } from ${JSON.stringify(formatter)};
      console.log(JSON.stringify([
        formatDate('2026-09-19'),
        formatDate('2026-09-19T23:30:00Z'),
        formatDate(new Date('2026-09-19')),
        formatDate(undefined),
      ]));
    `], { encoding: 'utf8', env: { ...process.env, TZ: timezone } });
    assert.ifError(result.error);
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(JSON.parse(result.stdout), [
      'September 19, 2026', 'September 19, 2026', 'September 19, 2026', '',
    ]);
  });
}

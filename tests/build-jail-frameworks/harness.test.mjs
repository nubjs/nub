import assert from 'node:assert/strict';
import { test } from 'node:test';
import { cases } from './cases.mjs';
import { verdict } from './verdict.mjs';

test('framework controls cannot be hidden by a passing jailed arm', () => {
  assert.equal(verdict({}, null), 'PASS');
  assert.equal(verdict({}, {}), 'PASS');
  assert.equal(verdict({}, { error: 'broken control' }), 'CONTROL-FAILED');
  assert.equal(verdict({ error: 'broken build' }, { error: 'broken build' }), 'CONTROL-FAILED');
  assert.equal(verdict({ error: 'denied' }, {}), 'JAIL-FAILED');
  assert.equal(verdict({ error: 'unknown' }, null), 'UNRESOLVED');
});

test('each application has exact package versions and production output', () => {
  assert.equal(new Set(cases.map(c => c.name)).size, cases.length);
  for (const fixture of cases) {
    assert.ok(fixture.build && fixture.output && Object.keys(fixture.files).length, fixture.name);
    for (const [name, version] of Object.entries(fixture.dependencies)) {
      assert.match(version, /^\d+\.\d+\.\d+(?:-[\w.-]+)?$/, `${fixture.name}: ${name}`);
    }
  }
});

import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath, pathToFileURL } from 'node:url';
import test from 'node:test';

// Run the real fixture with only its external commands replaced. No packages are downloaded or
// executed: these tests check orchestration, while osv-screen.test.mjs checks the actual scanner.
const preload = `
import assert from 'node:assert/strict';
import cp from 'node:child_process';
import { syncBuiltinESMExports } from 'node:module';
import { appendFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
const mode = process.env.FIXTURE_MODE;
const record = (kind, cwd, args) => appendFileSync(process.env.FIXTURE_EVENTS, JSON.stringify({kind, cwd, args}) + '\\n');
const tree = (cwd) => {
  mkdirSync(join(cwd, 'node_modules', 'esbuild'), { recursive: true });
  writeFileSync(join(cwd, 'node_modules', 'esbuild', 'package.json'), JSON.stringify({name:'esbuild', version:'0.24.0'}));
};
const ok = (stdout = '') => ({status:0, stdout, stderr:''});
cp.spawnSync = (file, args, options) => {
  if (args[0] === 'install') {
    const cwd = options.cwd;
    assert.ok(cwd?.startsWith(process.env.CORPUS_TEMP_ROOT), 'install uses this arm, not harness cwd');
    const pkg = JSON.parse(readFileSync(join(cwd, 'package.json')));
    assert.equal(pkg.dependencies.esbuild, '0.24.0', 'actual selected fixture is resolved');
    if (args.includes('--ignore-scripts')) {
      record('prepare', cwd, args);
      if (mode === 'prepare-failure') return {status:1, stdout:'', stderr:'resolution failed'};
      writeFileSync(join(cwd, 'pnpm-lock.yaml'), 'frozen fixture lock');
      if (mode === 'empty-tree') mkdirSync(join(cwd, 'node_modules'));
      else if (mode !== 'unresolved-tree') tree(cwd);
      return ok();
    }
    record('lifecycle', cwd, args);
    assert.deepEqual(args, ['install', '--frozen-lockfile']);
    assert.equal(readFileSync(join(cwd, 'pnpm-lock.yaml'), 'utf8'), 'frozen fixture lock');
    assert.ok(!existsSync(join(cwd, 'node_modules')), 'lifecycle materialization starts cold');
    tree(cwd);
    return ok('JAILDUMP pkg=Some("esbuild") running without the build sandbox');
  }
  if (args[0] === process.env.CORPUS_OSV_SCREEN) {
    const value = (flag) => args[args.indexOf(flag) + 1];
    const cwd = value('--tree');
    const phase = value('--kind').endsWith('-prepared') ? 'prepared' : 'bound';
    record(phase, cwd, args);
    const complete = existsSync(join(cwd, 'node_modules', 'esbuild', 'package.json'));
    const denied = !complete || mode === 'screen-failure';
    writeFileSync(value('--out'), JSON.stringify({status:denied ? 'error' : 'clean', digest:mode === 'changed-tree' && phase === 'bound' ? 'changed' : 'same'}));
    return {status:denied ? 42 : 0, stdout:'', stderr:denied ? 'screen refused' : ''};
  }
  assert.equal(args[0], '-e', 'only the artifact probe remains');
  record('probe', options.cwd, args);
  return ok();
};
syncBuiltinESMExports();
`;

function run(t, mode, report = true) {
  const root = mkdtempSync(join(tmpdir(), 'corpus-screen-test-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const patch = join(root, 'commands # preload.mjs');
  const eventsFile = join(root, 'events.jsonl');
  writeFileSync(patch, preload);
  writeFileSync(eventsFile, '');
  const env = { ...process.env, NUB_BIN: process.execPath, CORPUS_CASE: 'esbuild',
    CORPUS_TEMP_ROOT: root, CORPUS_OSV_SCREEN: join(root, 'scanner.mjs'),
    FIXTURE_MODE: mode, FIXTURE_EVENTS: eventsFile };
  delete env.CORPUS_LINKED_PROJECT;
  delete env.CORPUS_REPORT;
  if (report) env.CORPUS_REPORT = join(root, 'report');
  const result = spawnSync(process.execPath, ['--import', pathToFileURL(patch).href, fileURLToPath(new URL('./packages.mjs', import.meta.url))],
    { env, cwd: tmpdir(), encoding: 'utf8', timeout: 30_000 });
  assert.ifError(result.error);
  const events = readFileSync(eventsFile, 'utf8').trim().split('\n').filter(Boolean).map(line => JSON.parse(line));
  return { ...result, events, report: env.CORPUS_REPORT };
}

for (const report of [true, false]) {
  test(`screens each actual arm before frozen cold install (report=${report})`, t => {
    const result = run(t, 'ok', report);
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(result.events.map(e => e.kind), ['prepare', 'prepared', 'lifecycle', 'bound', 'probe', 'prepare', 'prepared', 'lifecycle', 'bound', 'probe']);
    for (let i = 0; i < result.events.length; i += 5) {
      assert.equal(new Set(result.events.slice(i, i + 5).map(e => e.cwd)).size, 1);
    }
    assert.notEqual(result.events[0].cwd, result.events[5].cwd, 'control and jailed trees are independent');
  });
}

for (const mode of ['prepare-failure', 'screen-failure', 'empty-tree', 'unresolved-tree']) {
  test(`${mode} cannot reach lifecycle or artifact probe`, t => {
    const result = run(t, mode);
    assert.notEqual(result.status, 0);
    assert.ok(result.events.some(e => e.kind === 'prepare'));
    assert.ok(!result.events.some(e => ['lifecycle', 'probe'].includes(e.kind)));
  });
}

test('changed installed closure cannot be reported as a passing arm', t => {
  const result = run(t, 'changed-tree');
  assert.notEqual(result.status, 0);
  assert.equal(result.events.filter(e => e.kind === 'lifecycle').length, 2);
  assert.ok(!result.events.some(e => e.kind === 'probe'));
  const verdicts = JSON.parse(readFileSync(join(result.report, 'results.json')));
  assert.equal(verdicts.length, 2);
  assert.ok(verdicts.every(v => !v.pass && v.error.includes('screened frozen closure')));
});

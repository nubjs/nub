// Adjudicate failed control arms without confusing a PM failure with a jail failure.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { closeSync, mkdtempSync, mkdirSync, openSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';

const input = resolve(process.env.CORPUS_RESULTS);
const provenance = JSON.parse(readFileSync(join(dirname(input), 'provenance.json')));
assert.equal(process.version, provenance.nodeVersion, 'the reference must use the original Node version');
assert.equal(process.platform, provenance.platform, 'the reference must use the original OS');
assert.ok(process.env.NPM_CLI, 'NPM_CLI must name the reference npm CLI entry point');
const cases = JSON.parse(readFileSync(input)).filter(row => row.verdict === 'CONTROL-FAILED'
  && (!process.env.CORPUS_CASE || row.name === process.env.CORPUS_CASE));
assert.ok(cases.length, 'at least one failed control must be selected');
const root = mkdtempSync(join(tmpdir(), 'nub-jail-reference-'));
console.log(`Fixture root: ${root}`);
const results = [];
let stoppedForCleanupError = false;
const windowsCleanup = process.platform === 'win32' || process.env.REFERENCE_CONTROLS_TEST_WINDOWS === '1';
function record(row) {
  results.push(row);
  writeFileSync(join(root, 'results.json'), JSON.stringify({ input, provenance, npm: process.env.NPM_CLI, expected: cases.length, results }, null, 2));
  const verdict = row.verdict ?? (row.timedOut
    ? row.cleanupError ? `npm TIMEOUT (cleanup failed: ${row.cleanupError})` : 'npm TIMEOUT'
    : `npm ${row.status ?? row.error ?? row.signal}`);
  console.log(`${results.length}/${cases.length} ${row.name}@${row.version}: ${verdict}`);
}
for (const { name, version } of cases) {
  const controlLog = readFileSync(join(dirname(input), 'logs', `${name.replaceAll('/', '-')}-control.log`), 'utf8');
  if (/ERR_(?:NUB|AUBE)_MALICIOUS_PACKAGE|blockExoticSubdeps/.test(controlLog)) {
    record({ name, version, verdict: 'POLICY-BLOCKED', reason: 'package admission policy; no reference install attempted' });
    continue;
  }
  const home = join(root, name.replaceAll('/', '-'));
  const project = join(home, 'p');
  mkdirSync(project, { recursive: true });
  writeFileSync(join(project, 'package.json'), JSON.stringify({ name: 'reference-consumer', private: true, dependencies: { [name]: version } }));
  const fd = openSync(join(home, 'install.log'), 'w');
  const child = spawn(process.execPath, [resolve(process.env.NPM_CLI), 'install', '--foreground-scripts', '--no-audit', '--no-fund'], {
    cwd: project, stdio: ['ignore', fd, fd], detached: process.platform !== 'win32',
    env: { ...process.env, HOME: home, USERPROFILE: home,
      XDG_CONFIG_HOME: join(home, 'config'), XDG_CACHE_HOME: join(home, 'cache'),
      XDG_DATA_HOME: join(home, 'data'), npm_config_cache: join(home, 'npm-cache'),
      CI: '1', NO_COLOR: '1' },
  });
  let timedOut = false;
  let cleanupError;
  const killTree = () => {
    if (!child.pid) return;
    if (!windowsCleanup) {
      try { process.kill(-child.pid, 'SIGKILL'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
      return;
    }
    const tree = spawnSync('taskkill', ['/PID', String(child.pid), '/T', '/F'], { stdio: 'ignore', timeout: 10_000 });
    if (!tree.error && tree.status === 0) return;
    // A root kill unblocks this runner, but cannot establish that descendants died.
    // Keep that distinction in the result rather than hanging forever or reporting
    // a censored reference as a clean timeout.
    let rootKilled;
    try { rootKilled = child.kill('SIGKILL'); } catch (error) { cleanupError = `taskkill failed (${tree.error?.message ?? `exit ${tree.status}`}); root fallback failed (${error.message})`; return false; }
    if (!rootKilled) {
      cleanupError = `taskkill failed (${tree.error?.message ?? `exit ${tree.status}`}); root fallback returned false`;
      return false;
    }
    cleanupError = `taskkill failed (${tree.error?.message ?? `exit ${tree.status}`}); terminated only the root process`;
    return true;
  };
  let resolveResult;
  const completion = new Promise(resolve => {
    resolveResult = resolve;
    child.once('error', error => resolve({ status: null, error: error.message }));
    child.once('exit', (status, signal) => resolve({ status, signal }));
  });
  const timer = setTimeout(() => {
    timedOut = true;
    if (killTree() === false) {
      // No exit event is a safe prerequisite here: the fallback could not
      // terminate the root. Unref it, persist the censored result below, and
      // fail closed rather than pinning the reference runner forever.
      child.unref();
      resolveResult({ status: null, error: cleanupError });
    }
  }, 300_000);
  const result = await completion;
  clearTimeout(timer);
  if (!windowsCleanup) killTree();
  closeSync(fd);
  const row = { name, version, ...result, timedOut, ...(cleanupError ? { cleanupError } : {}) };
  record(row);
  // We cannot safely claim tree cleanup when the OS rejected taskkill. Preserve
  // the timeout result for the report, but make the reference run fail closed.
  if (cleanupError) {
    process.exitCode = 1;
    stoppedForCleanupError = true;
    break;
  }
}
assert.ok(stoppedForCleanupError || results.length === cases.length, 'every failed control gets a reference result');

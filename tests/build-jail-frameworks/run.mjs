import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import { closeSync, existsSync, mkdirSync, mkdtempSync, openSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { delimiter, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { cases, marker } from './cases.mjs';
import { verdict } from './verdict.mjs';
import { retainInputs } from './evidence.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const binary = resolve(process.env.NUB_BIN);
const selected = process.argv.slice(2);
const fixtures = selected.length ? cases.filter(c => selected.includes(c.name)) : cases;
assert.ok(fixtures.length && selected.every(name => cases.some(c => c.name === name)), 'unknown or empty framework selection');
const root = process.env.FRAMEWORK_REPORT ? resolve(process.env.FRAMEWORK_REPORT) : mkdtempSync(join(tmpdir(), 'nub-jail-frameworks-'));
mkdirSync(root, { recursive: true });
assert.ok(!existsSync(join(root, 'results.json')), 'use a fresh report directory');
const digest = path => createHash('sha256').update(readFileSync(path)).digest('hex');
const provenance = { binary, binarySha256: digest(binary), node: process.execPath, nodeVersion: process.version,
  platform: process.platform, arch: process.arch, sourceRevision: process.env.SOURCE_REVISION ?? null,
  harnessSha256: digest(join(here, 'run.mjs')), casesSha256: digest(join(here, 'cases.mjs')), scorerSha256: digest(join(here, 'verdict.mjs')),
  collectorSha256: digest(join(here, 'evidence.mjs')),
  linker: process.env.FRAMEWORK_LINKER ?? 'default',
  started: new Date().toISOString(), expected: fixtures.map(c => c.name) };
writeFileSync(join(root, 'provenance.json'), JSON.stringify(provenance, null, 2));
console.log(`Framework evidence: ${root}`);

function write(path, value) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, typeof value === 'string' ? value : JSON.stringify(value, null, 2));
}

function launch(program, args, cwd, env, log) {
  const fd = openSync(log, 'w');
  const child = spawn(program, args, { cwd, env, stdio: ['ignore', fd, fd], detached: process.platform !== 'win32' });
  const completion = new Promise(resolve => {
    child.once('error', error => resolve({ status: null, error: error.message }));
    child.once('exit', (status, signal) => resolve({ status, signal }));
  });
  let stopped = false;
  async function stop() {
    if (stopped) return;
    stopped = true;
    if (child.pid) {
      if (process.platform === 'win32') {
        if (child.exitCode === null && child.signalCode === null) {
          const result = spawnSync('taskkill', ['/PID', String(child.pid), '/T', '/F'], { timeout: 10_000, encoding: 'utf8' });
          if (result.error || result.status !== 0) {
            child.kill('SIGKILL');
            throw Object.assign(new Error(`process-tree cleanup failed: ${result.error?.message ?? result.stderr}`), { cleanupFailed: true });
          }
        }
      } else {
        try { process.kill(-child.pid, 'SIGKILL'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
      }
    }
    await completion;
    closeSync(fd);
  }
  return { child, completion, stop };
}

async function command(args, cwd, env, log, timeout = 600_000) {
  const process = launch(binary, args, cwd, env, log);
  let timer;
  try {
    const result = await Promise.race([process.completion, new Promise(resolve => { timer = setTimeout(() => resolve({ status: null, error: 'timeout' }), timeout); })]);
    assert.equal(result.error, undefined, `${args.join(' ')}: ${JSON.stringify(result)}; ${log}`);
    assert.equal(result.status, 0, `${args.join(' ')}: ${JSON.stringify(result)}; ${log}`);
    return readFileSync(log, 'utf8');
  } finally {
    clearTimeout(timer);
    await process.stop();
  }
}

async function freePort() {
  const server = createServer();
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  await new Promise(resolve => server.close(resolve));
  return port;
}

function outputFiles(path) {
  return readdirSync(path, { withFileTypes: true }).flatMap(entry => entry.isDirectory()
    ? outputFiles(join(path, entry.name)) : [join(path, entry.name)]);
}

async function arm(fixture, confined) {
  const label = `${fixture.name}-${confined ? 'jailed' : 'control'}`;
  const evidence = join(root, label);
  const base = mkdtempSync(join(tmpdir(), `nub-framework-${label}-`));
  const project = join(base, 'project');
  const home = join(base, 'home');
  mkdirSync(project, { recursive: true });
  mkdirSync(home);
  mkdirSync(evidence);
  const secret = join(home, '.ssh', 'fixture-key');
  const outside = join(base, 'dependency-write');
  const rootProof = join(base, 'root-prepare.json');
  write(secret, 'framework-fixture-secret');
  const name = `framework-sentinel-${randomUUID()}`;
  const pkg = join(base, 'package');
  write(join(pkg, 'package.json'), { name, version: '1.0.0', scripts: { postinstall: 'node probe.cjs' } });
  write(join(pkg, 'probe.cjs'), `const fs=require('node:fs'); const out={ran:true,token:process.env.AWS_SECRET_ACCESS_KEY??null}; try{out.read=fs.readFileSync(${JSON.stringify(secret)},'utf8')}catch(e){out.read=e.code} try{fs.writeFileSync(${JSON.stringify(outside)},'outside');out.write=true}catch(e){out.write=e.code} fs.writeFileSync('proof.json',JSON.stringify(out));`);
  const archive = join(base, 'sentinel.tgz');
  const packed = spawnSync('tar', ['-czf', 'sentinel.tgz', 'package'], { cwd: base, encoding: 'utf8' });
  assert.equal(packed.status, 0, packed.stderr);
  const specifier = `file:${archive.replaceAll('\\', '/')}`;
  const manifest = { name: `framework-${fixture.name}`, version: '1.0.0', private: true, type: 'module',
    dependencies: { ...fixture.dependencies, [name]: specifier }, allowScripts: { '*': true, [`${name}@${specifier}`]: true },
    scripts: { prepare: `${fixture.prepare ? `${fixture.prepare} && ` : ''}node root-prepare.cjs`, build: fixture.build, ...(fixture.start ? { start: fixture.start } : {}) } };
  write(join(project, 'package.json'), manifest);
  write(join(project, 'nub.jsonc'), { install: { buildJail: confined, ...(process.env.FRAMEWORK_LINKER ? { linker: process.env.FRAMEWORK_LINKER } : {}) } });
  write(join(project, '.npmrc'), 'strict-dep-builds=true\n');
  write(join(project, 'root-prepare.cjs'), `require('node:fs').writeFileSync(${JSON.stringify(rootProof)},JSON.stringify({token:process.env.AWS_SECRET_ACCESS_KEY}));`);
  for (const [path, content] of Object.entries(fixture.files)) write(join(project, path), content);
  const env = { ...process.env, HOME: home, USERPROFILE: home, XDG_CONFIG_HOME: join(home, 'config'),
    XDG_CACHE_HOME: join(home, 'cache'), XDG_DATA_HOME: join(home, 'data'),
    APPDATA: join(home, 'AppData', 'Roaming'), LOCALAPPDATA: join(home, 'AppData', 'Local'),
    NODE_EXECUTABLE: process.execPath, AWS_SECRET_ACCESS_KEY: 'framework-fixture-token',
    CI: '1', NO_COLOR: '1', NUB_JAIL_DUMP_POLICY: '1', NEXT_TELEMETRY_DISABLED: '1', NUXT_TELEMETRY_DISABLED: '1', ASTRO_TELEMETRY_DISABLED: '1', NG_CLI_ANALYTICS: 'false',
    npm_config_cache: join(home, 'npm-cache'), PATH: `${dirname(binary)}${delimiter}${process.env.PATH}` };
  for (const key of ['NUB_BUILD_JAIL_CATALOG', 'NODE_OPTIONS', 'NODE_COMPAT', 'npm_config_ignore_scripts', 'NUB_CACHE_DIR']) delete env[key];
  const result = { name: fixture.name, confined, project, evidence, stage: 'install', started: new Date().toISOString() };
  try {
    const log = await command(['install'], project, env, join(evidence, 'install.log'));
    result.launches = [...log.matchAll(/JAILDUMP pkg=Some\("([^"\n]+)"\)/g)].map(m => m[1]);
    result.realPackageLaunches = result.launches.filter(n => n !== name);
    result.optOut = /running without the build sandbox/.test(log);
    assert.doesNotMatch(log, /WARN_(?:NUB|AUBE)_IGNORED_BUILD_SCRIPTS/, 'every fixture script must be approved');
    result.proof = JSON.parse(readFileSync(join(project, 'node_modules', name, 'proof.json')));
    assert.equal(result.proof.ran, true);
    if (confined) {
      assert.ok(result.launches.includes(name), 'sentinel policy was compiled');
      assert.equal(result.optOut, false, 'no package opted out');
      assert.notEqual(result.proof.read, 'framework-fixture-secret');
      assert.notEqual(result.proof.write, true);
      assert.equal(result.proof.token, null);
      assert.equal(existsSync(outside), false);
    } else {
      assert.equal(result.proof.read, 'framework-fixture-secret');
      assert.equal(result.proof.write, true);
      assert.equal(result.proof.token, 'framework-fixture-token');
      assert.equal(result.optOut, true);
    }
    assert.deepEqual(JSON.parse(readFileSync(rootProof)), { token: 'framework-fixture-token' });
    result.versions = Object.fromEntries(Object.keys(fixture.dependencies).map(name => {
      const installed = JSON.parse(readFileSync(join(project, 'node_modules', name, 'package.json')));
      assert.equal(installed.version, fixture.dependencies[name], `${name}: installed version`);
      return [name, installed.version];
    }));
    if (existsSync(join(project, 'node_modules', '.modules.yaml'))) {
      result.layout = readFileSync(join(project, 'node_modules', '.modules.yaml'), 'utf8');
    }
    result.lockfileSha256 = digest(join(project, 'nub.lock'));
    result.stage = 'build';
    await command(['run', '--node', 'build'], project, env, join(evidence, 'build.log'));
    const emitted = outputFiles(join(project, fixture.output));
    assert.ok(emitted.length, 'production output exists');
    result.output = emitted.map(path => ({ path: path.slice(project.length + 1), sha256: digest(path) }));
    assert.ok(emitted.filter(path => /\.(?:html|[cm]?js)$/.test(path)).some(path => readFileSync(path, 'utf8').includes(marker)), 'production output contains application data');
    if (fixture.native) {
      write(join(project, 'native-proof.mjs'), `import assert from 'node:assert/strict'; import sharp from 'sharp'; const png=await sharp({create:{width:2,height:3,channels:3,background:'#123456'}}).png().toBuffer(); const meta=await sharp(png).metadata(); assert.equal(meta.width,2); assert.equal(meta.height,3); console.log(JSON.stringify({format:meta.format,width:meta.width,height:meta.height}));`);
      await command(['--node', 'native-proof.mjs'], project, env, join(evidence, 'native.log'));
    }
    if (fixture.start) {
      result.stage = 'serve';
      const port = await freePort();
      const server = launch(binary, ['run', '--node', 'start'], project, { ...env, PORT: String(port), HOST: '127.0.0.1', HOSTNAME: '127.0.0.1', ORIGIN: `http://127.0.0.1:${port}` }, join(evidence, 'server.log'));
      try {
        const deadline = Date.now() + 90_000;
        let response;
        while (Date.now() < deadline && server.child.exitCode === null) {
          try { response = await fetch(`http://127.0.0.1:${port}/`, { signal: AbortSignal.timeout(3000) }); break; } catch { await new Promise(resolve => setTimeout(resolve, 500)); }
        }
        assert.equal(response?.status, 200, `production server: ${join(evidence, 'server.log')}`);
        const body = await response.text();
        assert.ok(body.includes(marker), 'served response contains application data');
        write(join(evidence, 'response.txt'), body);
      } finally { await server.stop(); }
    }
    result.stage = 'frozen';
    await command(['install', '--frozen-lockfile'], project, env, join(evidence, 'frozen.log'));
    assert.equal(digest(join(project, 'nub.lock')), result.lockfileSha256, 'frozen install preserved the resolved graph');
    result.stage = 'passed';
  } catch (error) {
    result.error = error.stack;
    result.cleanupFailed = Boolean(error.cleanupFailed);
  } finally {
    result.retainedInputs = retainInputs(project, evidence);
    result.finished = new Date().toISOString();
    write(join(evidence, 'result.json'), result);
  }
  return result;
}

const results = [];
for (const fixture of fixtures) {
  console.log(`Starting ${fixture.name}`);
  const jailed = await arm(fixture, true);
  const control = !jailed.cleanupFailed && (jailed.error || process.env.FRAMEWORK_CONTROLS === 'all') ? await arm(fixture, false) : null;
  const outcome = verdict(jailed, control);
  results.push({ name: fixture.name, verdict: outcome, jailed, control });
  write(join(root, 'results.json'), results);
  console.log(`${results.length}/${fixtures.length} ${fixture.name}: ${outcome}${jailed.error ? ` (${jailed.stage})` : ''}`);
  if (jailed.cleanupFailed || control?.cleanupFailed) break;
}
assert.equal(digest(binary), provenance.binarySha256, 'binary changed during the sweep');
assert.equal(results.length, fixtures.length);
console.log(JSON.stringify(results.map(({ name, verdict }) => ({ name, verdict }))));
process.exitCode = results.every(row => row.verdict === 'PASS') ? 0 : 1;

// Broad lifecycle coverage complements packages.mjs's functional artifact checks.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { existsSync, lstatSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const binary = resolve(process.env.NUB_BIN);
const source = join(dirname(fileURLToPath(import.meta.url)), 'population.tsv');
const population = readFileSync(source, 'utf8').trim().split('\n')
  .filter(line => line && !line.startsWith('#')).map(line => line.split('\t'));
assert.equal(new Set(population.map(([name]) => name)).size, population.length);
const selected = process.env.CORPUS_CASE
  ? population.filter(([name]) => name === process.env.CORPUS_CASE) : population;
assert.ok(selected.length, 'the selected population is not empty');
const root = mkdtempSync(join(tmpdir(), 'nub-jail-population-'));
mkdirSync(join(root, 'logs'));
console.log(`Fixture root: ${root}`);
const digest = path => createHash('sha256').update(readFileSync(path)).digest('hex');
const provenance = {
  binary, binarySha256: digest(binary), populationSha256: digest(source),
  harnessSha256: digest(fileURLToPath(import.meta.url)),
  node: process.execPath, nodeVersion: process.version, platform: process.platform,
  arch: process.arch, expected: selected.length, started: new Date().toISOString(),
};
writeFileSync(join(root, 'provenance.json'), JSON.stringify(provenance, null, 2));
const results = [];

function nativeArtifacts(packageRoot) {
  const files = [];
  const seen = new Set();
  function walk(dir) {
    const real = realpathSync(dir);
    if (seen.has(real)) return;
    seen.add(real);
    for (const name of readdirSync(dir)) {
      if (name === 'node_modules' || name === '.git') continue;
      const path = join(dir, name);
      const stat = lstatSync(path);
      if (stat.isDirectory()) walk(path);
      else if (stat.isFile() && /\.(node|exe|dll|so|dylib)$/.test(name)) {
        files.push({ path: relative(packageRoot, path).replaceAll('\\', '/'), nonempty: stat.size > 0 });
      }
    }
  }
  walk(packageRoot);
  return files.sort((a, b) => a.path.localeCompare(b.path));
}

function arm(name, version, confined) {
  const label = `${name.replaceAll('/', '-')}-${confined ? 'jailed' : 'control'}`;
  const home = join(root, label);
  const project = join(home, 'p');
  mkdirSync(project, { recursive: true });
  writeFileSync(join(project, 'package.json'), JSON.stringify({
    name: 'population-consumer', private: true, dependencies: { [name]: version }, allowScripts: { '*': true },
  }));
  writeFileSync(join(project, 'nub.jsonc'), JSON.stringify({ install: { buildJail: confined } }));
  writeFileSync(join(project, '.npmrc'), 'side-effects-cache=false\nminimum-release-age=0\ntrust-policy=off\n');
  const env = { ...process.env, HOME: home, USERPROFILE: home,
    XDG_CONFIG_HOME: join(home, 'config'), XDG_CACHE_HOME: join(home, 'cache'),
    XDG_DATA_HOME: join(home, 'data'), NODE_EXECUTABLE: process.execPath,
    CI: '1', NO_COLOR: '1', NUB_JAIL_DUMP_POLICY: '1' };
  for (const key of ['NUB_BUILD_JAIL_CATALOG', 'ELECTRON_CACHE', 'ELECTRON_MIRROR', 'PLAYWRIGHT_BROWSERS_PATH', 'PUPPETEER_CACHE_DIR', 'npm_config_cache']) delete env[key];
  const start = Date.now();
  const install = spawnSync(binary, ['install'], {
    cwd: project, env, encoding: 'utf8', timeout: 300_000, maxBuffer: 32 * 1024 * 1024,
  });
  const log = `${install.stdout ?? ''}\n${install.stderr ?? ''}`;
  writeFileSync(join(root, 'logs', `${label}.log`), log);
  const packageRoot = join(project, 'node_modules', name);
  const manifestPath = join(packageRoot, 'package.json');
  const manifest = existsSync(manifestPath) ? JSON.parse(readFileSync(manifestPath)) : null;
  const launches = [...log.matchAll(/JAILDUMP pkg=Some\("([^"\n]+)"\)/g)].map(match => match[1]);
  return {
    status: install.status, error: install.error?.message, milliseconds: Date.now() - start,
    installedVersion: manifest?.version, launches,
    targetHasLifecycle: ['preinstall', 'install', 'postinstall'].some(key => manifest?.scripts?.[key]),
    artifacts: manifest ? nativeArtifacts(packageRoot) : [],
    optOut: /running without the build sandbox/.test(log),
  };
}

for (const [name, version] of selected) {
  const control = arm(name, version, false);
  const jailed = arm(name, version, true);
  let verdict;
  if (control.status !== 0 || control.error || control.installedVersion !== version) verdict = 'CONTROL-FAILED';
  else if (jailed.status !== 0 || jailed.error || jailed.installedVersion !== version) verdict = 'JAIL-FAILED';
  else if (!jailed.launches.length) verdict = 'NO-LIFECYCLE';
  else if (!control.optOut) verdict = 'INVALID-CONTROL';
  else if (jailed.targetHasLifecycle && !jailed.launches.includes(name)) verdict = 'TARGET-NOT-CONFINED';
  else if (JSON.stringify(control.artifacts) !== JSON.stringify(jailed.artifacts)) verdict = 'ARTIFACT-MISMATCH';
  else verdict = 'LIFECYCLE-PASS';
  results.push({ name, version, verdict, control, jailed });
  writeFileSync(join(root, 'results.json'), JSON.stringify(results, null, 2));
  console.log(`${results.length}/${selected.length} ${name}@${version}: ${verdict}`);
  // Successful trees can be large; retain their logs and artifact inventory, not their caches.
  if (verdict === 'LIFECYCLE-PASS' || verdict === 'NO-LIFECYCLE') {
    for (const mode of ['control', 'jailed']) rmSync(join(root, `${name.replaceAll('/', '-')}-${mode}`), { recursive: true, force: true });
  }
}
assert.equal(results.length, selected.length, 'every requested package has a record');
assert.equal(digest(binary), provenance.binarySha256, 'the binary did not change during the sweep');
const counts = results.reduce((out, { verdict }) => { out[verdict] = (out[verdict] ?? 0) + 1; return out; }, {});
console.log(JSON.stringify({ expected: selected.length, recorded: results.length, counts }));
process.exitCode = results.every(({ verdict }) => verdict === 'LIFECYCLE-PASS' || verdict === 'NO-LIFECYCLE') ? 0 : 1;

// Does a package write the SAME PATHS on two identical runs — and if not, is a single
// consistent grant still possible?
//
// `unrs-resolver@1.12.2` produced 40 distinct digests across 55 cells, with the control and
// state 53 — the SAME configuration — differing at an identical file count of 2730. A digest
// oracle can never score that package: every state fails, including the widest, and the run
// reports "no state passed" for a package that is fine.
//
// The tempting fix — score on the INTERSECTION of two control runs — is wrong, and worse than
// the disease. Intersecting compares on FEWER paths, so a cell that failed to write an unstable
// path still passes, and the search records too NARROW a minimum. That is the exact failure
// this whole effort exists to avoid: a grant that breaks the package in production. A third
// run can also write a path neither of the first two did, which no intersection covers.
//
// So the question is not "which paths are stable" but "do the varying paths share a SCOPE".
// A grant is a capability over a scope, not a set of names. If run A writes
// $store/foo@1/aaa and run B writes $store/foo@1/bbb, the names differ and `write.deps`
// covers both. Nondeterminism only defeats a grant if it crosses a scope BOUNDARY.
//
// Usage: node nondeterminism-probe.mjs <pkg@version> --nub <path> [--runs N]

import { execFileSync, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';

const OVERRIDE_ENV = 'NUB_BUILD_JAIL_CATALOG';

const argv = process.argv.slice(2);
const spec = argv.find((a) => !a.startsWith('--') && argv[argv.indexOf(a) - 1] !== '--nub' && argv[argv.indexOf(a) - 1] !== '--runs');
const nub = argv[argv.indexOf('--nub') + 1];
const RUNS = argv.includes('--runs') ? Number(argv[argv.indexOf('--runs') + 1]) : 3;
if (!spec || !nub) { console.error('usage: nondeterminism-probe.mjs <pkg@version> --nub <path> [--runs N]'); process.exit(2); }
const at = spec.lastIndexOf('@');
const pkg = spec.slice(0, at);
const version = spec.slice(at + 1);

function paths(root, prefix = '') {
  const out = [];
  let entries;
  try { entries = fs.readdirSync(root, { withFileTypes: true }); } catch { return out; }
  for (const e of entries) {
    const rel = prefix ? `${prefix}/${e.name}` : e.name;
    if (!prefix && e.name === '.git') { out.push(...paths(path.join(root, e.name, 'hooks'), '.git/hooks')); continue; }
    if (e.name === '__pycache__' || e.name.endsWith('.pyc')) continue;
    // NPM'S DEBUG LOG IS NOT A SIDE EFFECT — its FILENAME carries the wall-clock time
    // (`.npm/_logs/2026-08-01T16_39_35_911Z-debug-0.log`), so two identical runs never agree.
    // Measured on `unrs-resolver@1.12.2`: 3 runs at full grant produced 2731 identical paths
    // plus exactly one uniquely-named log each — and that alone mismatched all 55 cells and
    // reported "no state passed" for a package that installs fine. It also would have entered
    // the `writePaths` derivation as `.npm/_logs`, a directory to promote into the user's real
    // npm home for no reason. Costs nothing on the grant axis: `$home` is baseline-writable.
    if (rel.startsWith('.npm/_logs/') || /\/\.npm\/_logs\//.test(rel)) continue;
    if (e.isDirectory()) out.push(...paths(path.join(root, e.name), rel));
    else out.push(rel);
  }
  return out;
}

const tokenise = (p) => {
  if (p.startsWith('proj/')) return `$proj/${p.slice(5)}`;
  let m = p.match(/^home\/\.cache\/nub\/pm\/store\/(.+)$/);
  if (m) return `$store/${m[1].replace(/^([^/]+@[^/-]+(?:\.[^/-]+)*)-[0-9a-f]{8,}\//, '$1/')}`;
  m = p.match(/^home\/\.cache\/nub\/jail-home\/[^/]+\/(.+)$/);
  if (m) return `$home/${m[1]}`;
  return p;
};

/** Which capability scope a tokenised path falls in. This is the vocabulary a GRANT is
 *  written in, so it is the only classification that decides whether one grant covers a
 *  set of varying names. */
function scopeOf(p) {
  if (p.startsWith('$proj/')) return 'project';
  if (p.startsWith('$home/')) return 'userHome';
  if (p.startsWith('$store/')) {
    // The package's OWN entry is baseline (rw on self), not a grant. Everything else under
    // the store is another package's entry, which is what `deps` reaches.
    const entry = p.slice(7).split('/')[0];
    return entry.startsWith(`${pkg.replace('/', '+')}@`) ? 'self(baseline)' : 'deps';
  }
  return 'OUTSIDE-FIXTURE';
}

function runOnce(i) {
  const dir = path.join(os.homedir(), '.cache', 'nub', 'ndprobe', `${pkg.replace('/', '+')}-${i}`);
  fs.rmSync(dir, { recursive: true, force: true });
  const proj = path.join(dir, 'proj');
  const home = path.join(dir, 'home');
  fs.mkdirSync(proj, { recursive: true });
  fs.mkdirSync(home, { recursive: true });
  fs.writeFileSync(path.join(proj, 'package.json'),
    JSON.stringify({ name: 'ndprobe', version: '1.0.0', dependencies: { [pkg]: version } }, null, 2));
  fs.writeFileSync(path.join(proj, '.npmrc'), 'side-effects-cache=false\n');
  execFileSync('git', ['init', '-q', '.'], { cwd: proj });

  // WIDEST GRANT for everything. This probe is not searching; it is asking what the package
  // does when nothing constrains it, twice.
  const cat = path.join(dir, 'catalog.json');
  fs.writeFileSync(cat, JSON.stringify({
    packages: { '*': undefined },   // placeholder replaced below
  }));

  const env = { ...process.env, HOME: home };
  // Discover the tree first so every package can be named at full grant.
  let r = spawnSync(nub, ['install', '--ignore-scripts'], { cwd: proj, env, encoding: 'utf8' });
  const store = path.join(home, '.cache', 'nub', 'pm', 'store');
  let names = [];
  try {
    names = [...new Set(fs.readdirSync(store).map((e) => e.replace(/-[0-9a-f]{8,}$/, '').replace(/@[^@]*$/, '').replace('+', '/')))];
  } catch {}
  const packages = {};
  for (const n of names) packages[n] = [{ write: 'disk', network: true, notes: 'probe: full grant' }];
  fs.writeFileSync(cat, JSON.stringify({ packages }, null, 2));
  env[OVERRIDE_ENV] = cat;

  fs.rmSync(path.join(proj, 'node_modules'), { recursive: true, force: true });
  let rc = 0;
  let log = '';
  for (const args of [['install'], ['approve-builds', '--all']]) {
    r = spawnSync(nub, args, { cwd: proj, env, encoding: 'utf8' });
    log += (r.stdout || '') + (r.stderr || '');
    if (r.status !== 0) { rc = r.status ?? 1; break; }
  }
  const seen = paths(dir).map(tokenise);
  return { rc, seen: new Set(seen), count: seen.length,
           digest: createHash('sha256').update(seen.slice().sort().join('\n')).digest('hex').slice(0, 16),
           overrode: log.includes('build-jail catalog OVERRIDDEN from') && !log.includes('was REJECTED') };
}

const runs = [];
for (let i = 0; i < RUNS; i++) {
  process.stderr.write(`  run ${i + 1}/${RUNS}…\n`);
  runs.push(runOnce(i));
}

console.log(`\n${spec} — ${RUNS} identical runs at FULL grant\n`);
for (const [i, r] of runs.entries()) {
  console.log(`  run${i + 1}  rc=${r.rc}  files=${r.count}  digest=${r.digest}  override=${r.overrode}`);
}

const all = runs.map((r) => r.seen);
const union = new Set(all.flatMap((s) => [...s]));
const stable = [...union].filter((p) => all.every((s) => s.has(p)));
const varying = [...union].filter((p) => !all.every((s) => s.has(p)));

console.log(`\n  union=${union.size}  stable(all runs)=${stable.length}  VARYING=${varying.length}`);

if (!varying.length) {
  console.log('\n  DETERMINISTIC — identical path sets. The digest oracle is sound for this package.');
  process.exit(0);
}

// THE DECIDING QUESTION: do the varying paths introduce a scope the stable set lacks?
const stableScopes = new Set(stable.map(scopeOf));
const varyingScopes = new Set(varying.map(scopeOf));
const newScopes = [...varyingScopes].filter((s) => !stableScopes.has(s));

console.log(`\n  scopes in STABLE  : ${[...stableScopes].sort().join(', ') || '(none)'}`);
console.log(`  scopes in VARYING : ${[...varyingScopes].sort().join(', ')}`);
console.log(`  scopes ONLY in varying: ${newScopes.length ? newScopes.join(', ') : 'NONE'}`);

console.log(`\n  ${newScopes.length
  ? 'A SINGLE GRANT IS NOT DERIVABLE FROM STABLE PATHS ALONE — the varying set reaches a scope\n  the stable set never does, so a grant fitted to the stable paths would deny a real write.\n  The grant must be the UNION over runs, not the intersection.'
  : 'A SINGLE CONSISTENT GRANT COVERS ALL RUNS. The names vary; the scopes do not. Score this\n  package on the SCOPE SET rather than the path digest.'}`);

const byScope = {};
for (const p of varying) (byScope[scopeOf(p)] ??= []).push(p);
console.log('\n  varying paths by scope (up to 8 each):');
for (const [s, ps] of Object.entries(byScope).sort()) {
  console.log(`    ${s}  (${ps.length})`);
  for (const p of ps.slice(0, 8)) console.log(`      ${p}`);
}

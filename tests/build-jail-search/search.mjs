#!/usr/bin/env node
// Find the MINIMUM grant a package's lifecycle scripts need, by walking the grant
// state space in ascending cost order and stopping at the first pass.
//
// Usage:  node search.mjs <pkg>@<version> --nub <path> [--keep] [--selftest]
//
// The methodology and every per-cell requirement live in
// `.fray/build-jail-catalog-schema.md`. The short version:
//
//   1. CONTROL — run unjailed in a realistic fixture. Record exit code and the
//      digest of the sorted relative path list of everything created/modified,
//      across BOTH the project and the store (dep writes land in the store).
//   2. A cell PASSES iff it reproduces the control on exit code AND digest.
//      A package whose control set is EMPTY is a no-op: exit code alone decides
//      and it lands at state 0. That is the nag-message postinstall, handled
//      with no special case.
//   3. Walk STATES[0..N] in cost order, stop at the first pass. That state is the
//      minimum BY CONSTRUCTION — every cheaper one already failed.
//
// Why a path-list digest rather than a content hash: content hashing breaks on any
// package that writes a timestamp or an embedded absolute path, so two identical
// control runs disagree and every cell then reads as failed. Why a digest rather
// than a plain "did anything change" bit: partial success is the common shape here
// — prisma with its grant stripped still writes 5 client files and exits 0 while
// the 19.3 MB query engine is missing, and `ghooks` wrote 0 of 17 hooks.

import { execFileSync, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { STATES } from './states.mjs';

const OVERRIDE_ENV = 'NUB_BUILD_JAIL_CATALOG';
const BANNER_OK = 'build-jail catalog OVERRIDDEN from';
const BANNER_BAD = 'was REJECTED';

/** A state as a v2 catalog holding ONLY the package under investigation.
 *
 *  Every one of the 54 states is expressible — that is the point of v2 and the
 *  reason the earlier v1 translation was thrown away: it could express 14. */
function catalogFor(pkg, state, others = []) {
  // EVERY OTHER PACKAGE IN THE TREE IS HELD AT FULL GRANT, in every arm including the
  // control. Without this the control ran under nub's compiled-in catalog — where other
  // packages keep their grants — while each cell swapped in a catalog naming only the
  // package under test, silently stripping everyone else. Two arms then differed by more
  // than the one variable. `prisma@7.9.1` read as "no grant helps" purely because of it:
  // the missing artefact was a 24 MB engine downloaded by `@prisma/engines`, a DEPENDENCY,
  // and no grant on prisma can restore what another package was denied.
  const packages = {};
  for (const o of others) {
    if (o === pkg) continue;
    packages[o] = [{ write: 'disk', network: true, notes: 'held at full grant: not the variable' }];
  }

  // State 0 is the BASE PROFILE, and the catalog spells that as NO ENTRY for this package.
  // An entry with no capabilities is a different thing and the parser rejects it, correctly.
  if (state.atoms.size === 0) return { packages };

  const grant = { notes: `grant-search cell probing "${state.label}"` };

  if (state.atoms.has('write.disk')) {
    grant.write = 'disk';
  } else {
    const w = {};
    for (const s of ['deps', 'project', 'userHome']) if (state.atoms.has(`write.${s}`)) w[s] = true;
    if (Object.keys(w).length) grant.write = w;
  }

  // `read` carries only what `write` does not already imply at the same scope.
  if (grant.write !== 'disk') {
    if (state.atoms.has('read.disk')) {
      grant.read = 'disk';
    } else {
      const r = {};
      for (const s of ['project', 'userHome']) {
        if (state.atoms.has(`read.${s}`) && !(grant.write && grant.write[s])) r[s] = true;
      }
      if (Object.keys(r).length) grant.read = r;
    }
  }

  if (state.atoms.has('network')) grant.network = true;

  packages[pkg] = [grant];
  return { packages };
}

// ── fixture + measurement ─────────────────────────────────────────────────────

/** Sorted relative paths under `root`. The fixture's own `.git` is excluded at the
 *  top level only — git's index and logs churn on every run and are not a side
 *  effect of the script, but `.git/hooks` underneath it very much is. */
function paths(root, prefix = '') {
  const out = [];
  let entries;
  try { entries = fs.readdirSync(root, { withFileTypes: true }); } catch { return out; }
  for (const e of entries) {
    const rel = prefix ? `${prefix}/${e.name}` : e.name;
    if (!prefix && e.name === '.git') { out.push(...paths(path.join(root, e.name, 'hooks'), '.git/hooks')); continue; }
    // PYTHON BYTECODE IS NOT A SIDE EFFECT. node-gyp ships `gyp/pylib`, and CPython writes
    // `__pycache__/*.pyc` beside any module it imports. Unjailed it can; jailed it cannot —
    // and the build SUCCEEDS either way, because .pyc is a pure cache. Counting them made
    // nine native packages (keccak, lz4, ssh2, …) demand `write.userHome`, the second-widest
    // grant we have, when the only difference between arms was bytecode. Verified by diffing
    // the two path sets: __pycache__ entries were the ENTIRE delta.
    if (e.name === '__pycache__' || e.name.endsWith('.pyc')) continue;
    if (e.isDirectory()) out.push(...paths(path.join(root, e.name), rel));
    else out.push(rel);
  }
  return out;
}

const digestOf = (list) => createHash('sha256').update(list.slice().sort().join('\n')).digest('hex').slice(0, 16);

function makeFixture(dir, pkg, version, { jailOff }) {
  fs.rmSync(dir, { recursive: true, force: true });
  const proj = path.join(dir, 'proj');
  fs.mkdirSync(proj, { recursive: true });
  fs.mkdirSync(path.join(dir, 'home'), { recursive: true });
  const manifest = { name: 'searchfix', version: '1.0.0', dependencies: { [pkg]: version } };
  if (jailOff) manifest.dependenciesMeta = { [pkg]: { sandbox: false } };
  fs.writeFileSync(path.join(proj, 'package.json'), JSON.stringify(manifest, null, 2));
  // side-effects-cache=false is load-bearing: a warm cache replays a prior build
  // and the lifecycle script NEVER SPAWNS, which reads exactly like a jail denial.
  fs.writeFileSync(path.join(proj, '.npmrc'), 'side-effects-cache=false\n');
  execFileSync('git', ['init', '-q', '.'], { cwd: proj });   // hook installers no-op without one
  return { proj, home: path.join(dir, 'home') };
}

/** Was the package placed INSIDE the project, rather than symlinked into the global
 *  store? Materialization alone gates project access — measured, jail-irrelevant — so a
 *  verdict of "needed no project grant" means nothing unless this is true.
 *
 *  READ FROM THE LAYOUT, NOT THE LOG. An earlier version matched nub's
 *  `materialized <pkg> (build script reads the project)` line, which is not always emitted:
 *  `ghooks` is materialized (its `.store` entry is a real directory) and prints nothing, so
 *  all 12 packages of a batch reported `materialized: false` and I drew a conclusion from it.
 *  A symlinked entry points into `~/.cache/nub/pm/store`; a materialized one is a real dir. */
function isMaterialized(proj, pkgSpec) {
  const store = path.join(proj, 'node_modules', '.store');
  let entries;
  try { entries = fs.readdirSync(store); } catch { return false; }
  // `.store` names scoped packages `@scope+name@version`, so match on the leading name.
  const name = pkgSpec.replace(/@[^@/]*$/, '').replace('/', '+');
  const hit = entries.find((e) => e.startsWith(`${name}@`));
  if (!hit) return false;
  try { return !fs.lstatSync(path.join(store, hit)).isSymbolicLink(); } catch { return false; }
}

function runCell(nub, { proj, home }, { catalogFile, label, ignoreScripts, pkg }) {
  const env = { ...process.env, HOME: home };
  if (catalogFile) env[OVERRIDE_ENV] = catalogFile;
  let log = '';
  let rc = 0;
  const steps = ignoreScripts ? [['install', '--ignore-scripts']] : [['install'], ['approve-builds', '--all']];
  for (const args of steps) {
    // spawnSync, not execFileSync: the override banner is a `warning:` on STDERR, and
    // execFileSync returns stdout ONLY. Losing stderr on the success path made every cell
    // read as "the override did not engage".
    const r = spawnSync(nub, args, { cwd: proj, env, encoding: 'utf8' });
    log += (r.stdout || '') + (r.stderr || '');
    if (r.status !== 0) { rc = r.status ?? 1; break; }
  }
  // SCAN THE WHOLE FIXTURE ROOT, not a list of subpaths I expect to matter.
  //
  // The install runs with HOME pointed at this fixture, so everything it touches lands
  // under here — that is why a package caching to `~/.cache/<vendor>` is visible at all.
  // It is containment by construction, NOT disk-wide detection, and the boundary is
  // verified: after a day of runs the real `~/.cache/puppeteer`, `~/.cache/node-gyp` and
  // `~/.npm` had zero modifications.
  //
  // Enumerating three roots was UNSOUND once the control started running at `write: disk`.
  // A control that wrote somewhere unscanned, and a narrow cell that did not, would produce
  // matching digests — both missing it identically — and the search would record too NARROW
  // a minimum. Scanning the root removes that whole class.
  //
  // Still missed: a write outside the fixture entirely (`/usr/local`, the real home). Those
  // are denied for every cell below `write: disk`, proven with EPERM, but a `disk` cell is
  // not bounded by that. That residual is stated rather than papered over.
  const root = path.dirname(proj);
  const unhash = (p) => p
    .replace(/(store\/)([^/]+@[^/-]+(?:\.[^/-]+)*)-[0-9a-f]{8,}\//, '$1$2/')
    .replace(/(jail-home\/)([^/]+)-[0-9a-f]{8,}\//, '$1$2/');
  const seen = paths(root).map(unhash);
  return {
    label, rc, log, seen,
    digest: digestOf(seen),
    files: seen.length,
    overrideOk: !catalogFile || (log.includes(BANNER_OK) && !log.includes(BANNER_BAD)),
    materialized: isMaterialized(proj, pkg),
  };
}


// ── run records ───────────────────────────────────────────────────────────────

/** Where one package@version's measurement lives. One file per pair, so re-running a
 *  single package rewrites exactly its own record and a batch is resumable — the catalog
 *  is COLLATED from these later, never edited in place by a run. */
function runPath(dir, pkg, version) {
  return path.join(dir, `${pkg.replace('/', '+')}@${version}.json`);
}

/** What produced this record. Without it a results directory is a pile of numbers with no
 *  way to tell which binary or host they came from — and this effort has already been
 *  burned by results whose provenance had to be reconstructed after the fact. */
function provenance(nub) {
  let sha = null;
  try { sha = createHash('sha256').update(fs.readFileSync(nub)).digest('hex'); } catch {}
  let nubVersion = null;
  try { nubVersion = spawnSync(nub, ['--version'], { encoding: 'utf8' }).stdout?.trim() || null; } catch {}
  // THE HARNESS'S OWN HASH, because the harness decides the ANSWER as much as the binary
  // does. A filter change (Python bytecode stopped counting as a side effect) moved nine
  // packages from `write.userHome` to `(nothing)` without the binary changing at all, and a
  // long batch picked up the edit MID-RUN. Without this a results directory silently mixes
  // two methodologies and nothing in the file says so.
  let harnessSha = null;
  try {
    const here = new URL('.', import.meta.url).pathname;
    const h = createHash('sha256');
    for (const f of ['search.mjs', 'states.mjs']) h.update(fs.readFileSync(path.join(here, f)));
    harnessSha = h.digest('hex').slice(0, 16);
  } catch {}
  return {
    nubPath: nub, nubSha256: sha, nubVersion, harnessSha256: harnessSha,
    platform: `${process.platform}-${process.arch}`,
    node: process.version,
    at: new Date().toISOString(),
  };
}


/** Every package name in the installed tree, from the virtual store. Needed because the
 *  background grant must name each one — the catalog is keyed by name and has no wildcard,
 *  deliberately (a wildcard would be `disk` for everything, which is the shape this whole
 *  model exists to avoid). Learned by one cheap `--ignore-scripts` install. */
function discoverTree(nub, dir, pkg, version) {
  const fx = makeFixture(dir, pkg, version, { jailOff: true });
  runCell(nub, fx, { label: 'discover', ignoreScripts: true, pkg });
  const store = path.join(fx.proj, 'node_modules', '.store');
  let entries = [];
  try { entries = fs.readdirSync(store); } catch { return []; }
  // `.store` names entries `<name>@<version>`; scoped packages use `+` for the separator.
  return [...new Set(entries.map((e) => {
    const at = e.lastIndexOf('@');
    return (at > 0 ? e.slice(0, at) : e).replace('+', '/');
  }))];
}

// ── the walk ──────────────────────────────────────────────────────────────────

/** Paths the CONTROL produced that the no-grant floor did not — i.e. what a grant must
 *  restore. Store hashes are already normalised by `runCell`, so these compare directly. */
function controlOnly(control, floor) {
  const had = new Set(floor.seen);
  return control.seen.filter((p) => !had.has(p));
}

const brief = (r) => ({ rc: r.rc, files: r.files, materialized: r.materialized });

function search(nub, pkg, version, root, keep) {
  const cell = (name, opts) => {
    const dir = path.join(root, name);
    const fx = makeFixture(dir, pkg, version, { jailOff: !!opts.jailOff });
    const r = runCell(nub, fx, { ...opts, pkg });
    if (!keep) fs.rmSync(dir, { recursive: true, force: true });
    return r;
  };
  const write = (state) => {
    const f = path.join(root, `cat-${state.cost}-${state.label.replace(/[^a-z]+/gi, '')}.json`);
    fs.writeFileSync(f, JSON.stringify(catalogFor(pkg, state, others), null, 2));
    return f;
  };

  // DOES THIS PACKAGE RUN A SCRIPT AT ALL? Read from its manifest, not measured against an
  // `--ignore-scripts` arm. That arm was tried and is WRONG: `approve-builds` pulls in the
  // build toolchain (node-gyp, the tar family), so the scripts-on arm installs strictly more
  // packages and the diff reported ~845 phantom "side effects" for a script that wrote three
  // files. The two shapes are not comparable; a manifest field is a fact.
  const hasScript = (() => {
    try {
      const out = execFileSync('npm', ['view', `${pkg}@${version}`, 'scripts', '--json'], { encoding: 'utf8' });
      const s = JSON.parse(out || '{}') || {};
      return ['preinstall', 'install', 'postinstall'].some((k) => k in s);
    } catch {
      return true;   // unknown: assume it does, so the oracle stays strict
    }
  })();

  // CONTROL — scripts on, jail off. EVERY cell runs the identical two steps, so full path
  // sets are directly comparable and no baseline subtraction is needed.
  const others = discoverTree(nub, path.join(root, 'discover'), pkg, version);

  const cells = [];
  // THE CONTROL IS THE TOP CELL. With every other package pinned at full grant, "the tested
  // package gets everything" is the most permissive state that exists — so it doubles as the
  // control and the top-of-ladder check, and the two can no longer disagree about the regime
  // they run under. A failure here means the package is broken for reasons no grant fixes.
  const control = cell('control', { catalogFile: write(STATES[STATES.length - 1]), label: 'control (tested pkg: everything)' });
  cells.push({ index: null, state: 'CONTROL (tested pkg: everything; all others: everything)', cost: null, pass: control.rc === 0,
               rc: control.rc, digest: control.digest, files: control.files,
               overrideEngaged: null, materialized: control.materialized });
  // One check, because the control IS the most permissive state. Failing here means no
  // grant can help — the package is broken for reasons confinement does not cause.
  if (control.rc !== 0 || !control.overrideOk) {
    return { pkg, version, verdict: 'BROKEN-EVEN-WITH-EVERYTHING', cells, control: brief(control) };
  }
  const sideEffectful = hasScript;
  const matches = (r) => r.rc === control.rc && (!sideEffectful || r.digest === control.digest);

  // 3. Ascending walk. The first pass IS the minimum.
  //    WHAT THE GRANT BUYS is recorded, not just that it was needed. State 0 is the
  //    no-grant floor, so the paths present in the control and missing from it are exactly
  //    the side effects a grant has to restore. Persisting them turns "why does this package
  //    need userHome?" into a field lookup instead of a manual two-arm rebuild-and-diff — the
  //    detour that found the __pycache__ artefact, which the harness already had in memory
  //    and discarded.
  let floor = null;
  for (let i = 0; i < STATES.length; i++) {
    const r = cell(`s${i}`, { catalogFile: write(STATES[i]), label: STATES[i].label });
    if (i === 0) floor = r;
    cells.push({
      index: i, state: STATES[i].label, cost: STATES[i].cost,
      pass: matches(r), rc: r.rc, digest: r.digest, files: r.files,
      overrideEngaged: r.overrideOk, materialized: r.materialized,
    });
    if (!r.overrideOk) return { pkg, version, cells, verdict: 'HARNESS-ERROR', why: `override did not engage at state ${i}`, log: r.log.slice(-400) };
    if (matches(r)) {
      return {
        pkg, version,
        verdict: 'MINIMUM',
        cells,
        state: STATES[i].label,
        cost: STATES[i].cost,
        stateIndex: i,
        sideEffectful,
        materialized: r.materialized,
        // IS THIS VERDICT CONCLUSIVE ABOUT PROJECT ACCESS? Only if nub actually placed the
        // package inside the project. When it did not, the script could not reach the project
        // whatever it wanted, so "did not need project" and "never had the chance" are
        // byte-identical and this verdict says nothing about that axis. It remains conclusive
        // about deps / userHome / disk / network, which is why this is a scoped caveat rather
        // than a blanket "untrustworthy".
        //
        // An earlier form of this flag also required the package to have NO install script,
        // so it never fired for anything that runs one — i.e. never for a package the search
        // is about. Measured: 12 of 12 top-download packages came back `materialized=false`
        // and every one was reported trustworthy.
        projectAxisConclusive: r.materialized,
        // The paths the winning grant restores: present unjailed, absent at the no-grant
        // floor. Capped, with the true count kept, so one pathological package cannot make
        // the results file unreadable — a silent truncation would be worse than a number.
        boughtCount: floor ? controlOnly(control, floor).length : null,
        // ⚠ DID THE ARTEFACT LAND SOMEWHERE THAT SURVIVES? The jail redirects `$HOME` to a
        // throwaway per-package directory, so a package caching under `~/.cache/<vendor>`
        // writes into `jail-home/` — the install PASSES and the artefact is DISCARDED. At run
        // time `HOME` is the real home and the package finds nothing. MEASURED: puppeteer's
        // browser landed in `jail-home/puppeteer-<hash>/.cache/` with ZERO entries under the
        // real `~/.cache/puppeteer`. The oracle cannot catch this — the install reproduced the
        // control exactly — so it is flagged from WHERE the bought paths landed instead.
        ephemeralArtifacts: floor
          ? (() => {
              const b = controlOnly(control, floor).filter((p) => !p.startsWith('proj/node_modules/.nub'));
              return b.length > 0 && b.every((p) => p.startsWith('jailhome/'));
            })()
          : null,
        bought: floor ? controlOnly(control, floor).slice(0, 40) : null,
        control: brief(control),
      };
    }
  }
  return { pkg, version, verdict: 'NO-STATE-PASSED', cells, control: brief(control) };
}

// ── cli ───────────────────────────────────────────────────────────────────────

const argv = process.argv.slice(2);

if (argv.includes('--selftest')) {
  let bad = 0;
  for (const s of STATES) {
    const emitted = catalogFor('x', s).packages.x;
    if (!emitted) continue;   // state 0 is 'no entry', correctly
    const g = emitted[0];
    const has = (k) => {
      if (k === 'network') return !!g.network;
      if (k.startsWith('write.')) {
        return g.write === 'disk' ? true : !!g.write?.[k.slice(6)];
      }
      // a read atom is satisfied either by an explicit read or by the write that implies it
      const scope = k.slice(5);
      if (g.write === 'disk' || g.read === 'disk') return true;
      return !!g.read?.[scope] || !!g.write?.[scope];
    };
    // every cost-bearing atom must survive into the emitted grant
    for (const a of [...s.atoms].filter((a) => !(a.startsWith('read.') && s.atoms.has(a.replace('read.', 'write.'))))) {
      if (!has(a)) { console.log(`  LOST "${a}" from state "${s.label}" ->`, JSON.stringify(g)); bad++; }
    }
  }
  console.log(`states: ${STATES.length}, all representable in v2 (v1 could express 14)`);
  console.log(`atoms lost in emission: ${bad}`);
  console.log('digest order-independent:', digestOf(['b', 'a']) === digestOf(['a', 'b']) ? 'yes' : 'NO — BUG');
  console.log('\nsample emissions:');
  for (const i of [0, 1, 5, 13, 16, 51, 53]) {
    const e = catalogFor('p', STATES[i]).packages.p;
    console.log(' ', String(i).padStart(2), STATES[i].label, '->', e ? JSON.stringify(e[0]) : '(no entry — the base profile)');
  }
  process.exit(bad ? 1 : 0);
}

const spec = argv.find((a) => !a.startsWith('--'));
if (!spec) {
  console.error('usage: node search.mjs <pkg>@<version> --nub <path> [--keep] [--selftest]');
  process.exit(2);
}
const at = spec.lastIndexOf('@');
const pkg = at > 0 ? spec.slice(0, at) : spec;
const version = at > 0 ? spec.slice(at + 1) : 'latest';
const nub = argv.includes('--nub') ? argv[argv.indexOf('--nub') + 1] : 'nub';
const keep = argv.includes('--keep');

// Never /tmp: a /tmp/package.json on this box is found by walking up.
const root = fs.mkdtempSync(path.join(os.homedir(), '.cache', 'nub-search-'));
try {
  const runDir = argv.includes('--runs')
    ? argv[argv.indexOf('--runs') + 1]
    : path.join(path.dirname(new URL(import.meta.url).pathname), 'results', 'runs');
  const out = runPath(runDir, pkg, version);

  // RESUMABLE BY DEFAULT. A batch re-run should cost only what it has not already measured;
  // `--force` re-measures one package after a harness or jail change.
  if (!argv.includes('--force') && fs.existsSync(out)) {
    const prior = JSON.parse(fs.readFileSync(out, 'utf8'));
    console.log(JSON.stringify({ ...prior, skipped: 'already measured; pass --force to redo' }));
  } else {
    const record = search(nub, pkg, version, root, keep);
    record.provenance = provenance(nub);
    fs.mkdirSync(runDir, { recursive: true });
    fs.writeFileSync(out, JSON.stringify(record, null, 2));
    console.log(JSON.stringify(record));
  }
} finally {
  if (!keep) fs.rmSync(root, { recursive: true, force: true });
}

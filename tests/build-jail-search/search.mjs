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
function catalogFor(pkg, state) {
  // State 0 is the BASE PROFILE, and the catalog spells that as NO ENTRY. Emitting an
  // entry with no capabilities is a different thing — the parser rejects it, correctly,
  // because a grant that widens nothing is an authoring mistake everywhere except here.
  if (state.atoms.size === 0) return { packages: {} };

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

  return { packages: { [pkg]: [grant] } };
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

function runCell(nub, { proj, home }, { catalogFile, label, ignoreScripts }) {
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
  const store = path.join(home, '.cache', 'nub', 'pm', 'store');
  // Store entries are `<pkg>@<version>-<hash>`, and the hash moves between arms, so raw
  // store paths compare as ALL-NEW and swamp the real signal — 845 phantom "script effects"
  // for a package whose script wrote 3 files. Strip the hash so the two arms are comparable.
  const unhash = (p) => p.replace(/^([^/]+@[^/-]+(?:\.[^/-]+)*)-[0-9a-f]{8,}\//, '$1/');
  const seen = [
    ...paths(proj).map((p) => `proj/${p}`),
    ...paths(store).map((p) => `store/${unhash(p)}`),
  ];
  return {
    label, rc, log, seen,
    digest: digestOf(seen),
    files: seen.length,
    overrideOk: !catalogFile || (log.includes(BANNER_OK) && !log.includes(BANNER_BAD)),
    // nub prints this when it places a package inside the project rather than
    // symlinking it into the global store. Materialization ALONE gates project
    // access — measured, jail-irrelevant — so `needs nothing` on a NON-materialized
    // package is ambiguous and is flagged, not recorded.
    materialized: /materialized\s+\S+\s+\(build script reads the project\)/.test(log),
  };
}

// ── the walk ──────────────────────────────────────────────────────────────────

const brief = (r) => ({ rc: r.rc, files: r.files, materialized: r.materialized });

function search(nub, pkg, version, root, keep) {
  const cell = (name, opts) => {
    const dir = path.join(root, name);
    const fx = makeFixture(dir, pkg, version, { jailOff: !!opts.jailOff });
    const r = runCell(nub, fx, opts);
    if (!keep) fs.rmSync(dir, { recursive: true, force: true });
    return r;
  };
  const write = (state) => {
    const f = path.join(root, `cat-${state.cost}-${state.label.replace(/[^a-z]+/gi, '')}.json`);
    fs.writeFileSync(f, JSON.stringify(catalogFor(pkg, state), null, 2));
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
  const control = cell('control', { jailOff: true, label: 'control' });
  if (control.rc !== 0) return { pkg, version, verdict: 'CONTROL-FAILED', control: brief(control) };
  const sideEffectful = hasScript;
  const matches = (r) => r.rc === control.rc && (!sideEffectful || r.digest === control.digest);

  // 2. TOP first — if the widest grant cannot make it pass, no state can, so skip
  //    the other 52. The one step that assumes monotonicity; dropping it changes
  //    no answer, only the cost.
  const top = cell('top', { catalogFile: write(STATES[STATES.length - 1]), label: 'top' });
  if (!top.overrideOk) return { pkg, version, verdict: 'HARNESS-ERROR', why: 'override did not engage at top', log: top.log.slice(-400) };
  if (!matches(top)) return { pkg, version, verdict: 'FAILS-AT-TOP', control: brief(control), top: brief(top) };

  // 3. Ascending walk. The first pass IS the minimum.
  for (let i = 0; i < STATES.length; i++) {
    const r = cell(`s${i}`, { catalogFile: write(STATES[i]), label: STATES[i].label });
    if (!r.overrideOk) return { pkg, version, verdict: 'HARNESS-ERROR', why: `override did not engage at state ${i}`, log: r.log.slice(-400) };
    if (matches(r)) {
      return {
        pkg, version,
        verdict: 'MINIMUM',
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
        control: brief(control),
      };
    }
  }
  return { pkg, version, verdict: 'NO-STATE-PASSED', control: brief(control) };
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
  console.log(JSON.stringify(search(nub, pkg, version, root, keep)));
} finally {
  if (!keep) fs.rmSync(root, { recursive: true, force: true });
}

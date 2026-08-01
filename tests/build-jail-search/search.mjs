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
// THE GLOBAL FLOOR, loaded once. `baseline` and `env` are top-level in the catalog — not
// package-keyed — because they are the profile every jailed script starts from. Merging them
// into every cell means a package is measured against the SAME floor it will run under; a
// search that omitted them would attribute a baseline gap to the package.
const BASELINE = (() => {
  try { return JSON.parse(fs.readFileSync(new URL('./baseline.json', import.meta.url), 'utf8')); }
  catch { return { baseline: [], env: [] }; }
})();
const withFloor = (cat) => ({ ...cat, baseline: BASELINE.baseline ?? [], env: BASELINE.env ?? [] });

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
  if (state.atoms.size === 0) return withFloor({ packages });

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
  return withFloor({ packages });
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
  // The install runs with HOME pointed at this fixture, so everything it touches lands under
  // here — containment by construction, NOT disk-wide detection. Verified: after a day of runs
  // the real ~/.cache/puppeteer, ~/.cache/node-gyp and ~/.npm had zero modifications.
  //
  // Enumerating a few roots was UNSOUND once the control began running at write:disk — a
  // control that wrote somewhere unscanned and a narrow cell that did not would produce
  // MATCHING digests, both missing it identically, and the search would record too NARROW a
  // minimum. Still missed: writes outside the fixture entirely, which every cell below
  // write:disk is denied (proven with EPERM) but a disk cell is not.
  //
  // PATHS ARE TOKENISED into the catalog's own vocabulary, so a record reads in the same terms
  // an entry is written in and no one has to translate:
  //   $proj/…   the user's project
  //   $store/…  a package's store entry, content hash stripped so arms compare
  //   $home/…   the package's HOME — throwaway under the jail, the REAL home in production,
  //             which is exactly what a `writePaths` entry names
  const root = path.dirname(proj);
  const tokenise = (p) => {
    if (p.startsWith('proj/')) return `$proj/${p.slice(5)}`;
    let m = p.match(/^home\/\.cache\/nub\/pm\/store\/(.+)$/);
    if (m) return `$store/${m[1].replace(/^([^/]+@[^/-]+(?:\.[^/-]+)*)-[0-9a-f]{8,}\//, '$1/')}`;
    m = p.match(/^home\/\.cache\/nub\/jail-home\/[^/]+\/(.+)$/);
    if (m) return `$home/${m[1]}`;
    return p;
  };
  const seen = paths(root).map(tokenise);
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
  try { entries = fs.readdirSync(store); } catch { return { names: [], specs: [] }; }
  // `.store` names entries `<name>@<version>`; scoped packages use `+` for the separator.
  // Returns BOTH the bare names (the catalog is keyed by name) and name@version specs (the
  // malicious-package screen needs the exact version that is about to execute).
  const names = new Set();
  const specs = new Set();
  for (const e of entries) {
    const at = e.lastIndexOf('@');
    const name = (at > 0 ? e.slice(0, at) : e).replace('+', '/');
    names.add(name);
    // Strip the store's content hash: `pkg@1.2.3-93d5bfce` -> `1.2.3`.
    if (at > 0) specs.add(`${name}@${e.slice(at + 1).replace(/-[0-9a-f]{8,}$/, '')}`);
  }
  return { names: [...names], specs: [...specs] };
}


/** The exact v2 catalog fragment this state corresponds to — the thing a collator writes
 *  into the catalog. Without it a record carries only a HUMAN LABEL ("write.deps") and
 *  building a catalog means parsing prose back into structure. `null` for state 0, because
 *  the catalog spells "needs nothing" as NO ENTRY. */
function grantFor(state) {
  if (state.atoms.size === 0) return null;
  const g = catalogFor('X', state).packages.X;
  return g ? g[0] : null;
}

/** The MINIMAL set of `$home`-relative directories covering every path the script left in
 *  its home — the `writePaths` entry, derived rather than authored.
 *
 *  puppeteer writes 355 paths, all under `$home/.cache/puppeteer/chrome/mac_arm-…/…`; the
 *  answer is the single entry `.cache/puppeteer`. Grouping by longest common directory gets
 *  there with no judgement, and a package writing to two unrelated caches yields two entries
 *  rather than one bogus shared ancestor.
 *
 *  DEPTH IS CAPPED AT TWO SEGMENTS. `.cache/puppeteer` is the vendor directory; going deeper
 *  would pin a version (`chrome/mac_arm-151.0.7922.47`) that changes on the next release and
 *  turn a stable entry into a churning one. Going shallower would hand over all of `.cache`.
 */
function homeWritePaths(paths) {
  // PROMOTE EVERY DIRECTORY THE SCRIPT WROTE — do not try to be clever about which ones.
  //
  // The build jail is DEFENCE IN DEPTH, not a security boundary, and the failure mode it
  // exists to avoid is PACKAGES BREAKING. Lifecycle scripts run with COMPLETE access in the
  // status quo, so promoting anything the script already wrote is a REDUCTION from the
  // baseline, never an escalation — a package can write `~/.npm` today with nothing stopping
  // it.
  //
  // The stronger reason is fidelity: unjailed, `ip-num`'s `npx only-allow` warms
  // `~/.npm/_cacache`, so promoting it REPRODUCES unconfined behaviour and withholding it
  // DIVERGES from it. An earlier version here required the directory to be named after the
  // package, which dropped `.npm/_cacache` and `.npm/_npx` and discarded real artefacts to
  // buy nothing.
  const dirs = new Set();
  for (const p of paths) {
    if (!p.startsWith('$home/')) continue;
    const segs = p.slice('$home/'.length).split('/');
    // A DIRECTORY GRANT NEEDS A DIRECTORY. Two segments with nothing beneath them is a FILE,
    // and promoting it moves another tool's bookkeeping into the user's real home for nothing:
    // `unrs-resolver` derived `.npm/_update-notifier-last-checked`, npm's update-notifier
    // timestamp, which is not the package's artefact at all. Requiring something below the
    // prefix is a structural test, not a judgement about which files are worth keeping —
    // `.cache/puppeteer` has paths beneath it and survives. Audited over every record: one
    // junk entry, one legitimate entry, and this separates them.
    //
    // A bare file directly in $HOME (`segs.length < 2`) was already excluded, which is why
    // `.npmrc` — the config root whose capture the private home exists to prevent — can never
    // reach this list.
    if (segs.length < 2) continue;   // a bare file in $HOME is not a directory grant
    dirs.add(segs.slice(0, 2).join('/'));
  }
  // Drop any entry already covered by a shallower one, so the set is minimal by construction.
  const out = [...dirs].sort();
  return out.filter((d) => !out.some((o) => o !== d && d.startsWith(`${o}/`)));
}

/** The winning grant WITH its derived `writePaths`, for the verification cell.
 *
 *  `writePaths` is deliberately absent from every search cell: the move happens AFTER the
 *  scripts finish, so it cannot change what a script did in the run being scored, and leaving
 *  it out of the control too keeps the comparison symmetric. That is correct — but it means the
 *  derived value is an OUTPUT NOTHING EVER TESTS, which is the exact shape of the inert checks
 *  that have cost this harness repeatedly. So it gets one cell of its own at the end. */
function catalogWithWritePaths(pkg, state, others, writePaths) {
  const cat = catalogFor(pkg, state, others);
  const g = cat.packages[pkg]?.[0] ?? { notes: 'writePaths verification' };
  g.writePaths = writePaths;
  cat.packages[pkg] = [g];
  return cat;
}

/** Refuse to execute lifecycle scripts from a package OSV flags as MALICIOUS.
 *
 *  This harness runs arbitrary install scripts from the npm ecosystem, on a real machine, by
 *  design — which is precisely the delivery mechanism a Shai-Hulud-style worm uses. The jail is
 *  BEST EFFORT and explicitly not a boundary against a targeted attacker, so it must not be the
 *  only thing between a known-malicious package and this box.
 *
 *  Screens the WHOLE INSTALLED TREE, not just the package under test: the worm propagates
 *  through dependencies, and a clean target with a compromised transitive dep is the case that
 *  matters. The tree is enumerated by the `--ignore-scripts` discovery install, so nothing has
 *  executed at the point this runs.
 *
 *  OSV `MAL-*` ids come from the ossf/malicious-packages dataset. Verified able to fire:
 *  `@ctrl/tinycolor@4.1.2`, a real Shai-Hulud compromise, returns MAL-2025-47141 — an
 *  all-clear from an instrument never shown to alarm is worth nothing.
 *
 *  FAILS CLOSED on a network or API error. A screen that silently passes when it could not
 *  reach the data is the same inert check this harness has produced seven times.
 */
function screenForMalicious(specs) {
  const queries = specs.map((sp) => {
    const at = sp.lastIndexOf('@');
    const name = at > 0 ? sp.slice(0, at) : sp;
    const version = at > 0 ? sp.slice(at + 1) : undefined;
    return version ? { package: { name, ecosystem: 'npm' }, version } : { package: { name, ecosystem: 'npm' } };
  });
  const body = JSON.stringify({ queries });
  const r = spawnSync('curl', ['-sS', '--max-time', '60', '-X', 'POST',
    'https://api.osv.dev/v1/querybatch', '-H', 'Content-Type: application/json', '-d', body],
    { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });
  if (r.status !== 0) throw new Error(`OSV screen FAILED (curl ${r.status}): ${(r.stderr || '').slice(0, 200)}`);
  let parsed;
  try { parsed = JSON.parse(r.stdout); } catch (e) { throw new Error(`OSV screen FAILED to parse: ${e.message}`); }
  const results = parsed.results ?? [];
  if (results.length !== specs.length) throw new Error(`OSV screen returned ${results.length} results for ${specs.length} queries`);
  const flagged = [];
  results.forEach((res, i) => {
    for (const v of res.vulns ?? []) if ((v.id ?? '').startsWith('MAL-')) flagged.push({ spec: specs[i], id: v.id });
  });
  return flagged;
}

// ── the walk ──────────────────────────────────────────────────────────────────

/** Which capability scope a tokenised path falls in — the vocabulary a GRANT is written in.
 *  A grant is a capability over a scope, not a set of names, so this is what decides whether
 *  one grant covers a set of paths whose names vary between runs. */
function scopeOf(p, pkg) {
  if (p.startsWith('$proj/')) return 'project';
  if (p.startsWith('$home/')) return 'userHome';
  if (p.startsWith('$store/')) {
    const entry = p.slice('$store/'.length).split('/')[0];
    return entry.startsWith(`${pkg.replace('/', '+')}@`) ? null : 'deps';   // own entry is baseline
  }
  return 'disk';
}

/** Widen a grant to cover scopes the oracle could NOT reliably measure.
 *
 *  When two identical control runs disagree, the paths they disagree about cannot be required
 *  of a cell (the control itself does not always produce them) and cannot be ignored (a cell
 *  that skipped them may genuinely lack a capability). The only safe reading is that those
 *  scopes are UNMEASURED — so grant them. Escalating on uncertainty costs breadth; not
 *  escalating costs a broken package, and this project's stated failure mode is packages
 *  breaking. Only the scopes the varying paths actually touch are added, so a package with
 *  one unstable log does not get `disk`. */
function escalate(grant, scopes) {
  if (!scopes.length) return grant;
  const g = grant ? { ...grant } : {};
  if (scopes.includes('disk') || g.write === 'disk') { g.write = 'disk'; return g; }
  const w = typeof g.write === 'object' && g.write ? { ...g.write } : {};
  for (const s of scopes) w[s] = true;
  g.write = w;
  // `read` may now be dominated by the widened `write`; the parser rejects that, so drop it.
  if (g.read && typeof g.read === 'object') {
    const r = { ...g.read };
    for (const s of Object.keys(w)) delete r[s];
    if (Object.keys(r).length) g.read = r; else delete g.read;
  }
  return g;
}

/** Paths that exist after the UNJAILED run but not after the no-grant run — what the script
 *  produced that confinement prevented. Store and jail-home hashes are already normalised by
 *  `runCell`, so these compare directly. */
function controlOnly(control, floor) {
  const had = new Set(floor.seen);
  return control.seen.filter((p) => !had.has(p));
}

/** The unjailed run recorded as the BASE CASE every cell is compared against. Its own path
 *  list is kept, not just a count: a path-valued grant cannot be found by enumeration the way
 *  a capability can — the space of paths is open — so it has to be DERIVED from what the
 *  script actually wrote. That derivation needs the paths, and needs them from the run that
 *  was allowed to do everything. */
const baseCase = (r, root) => ({
  rc: r.rc,
  fileCount: r.files,
  digest: r.digest,
  materialized: r.materialized,
  pathsUnderThrowawayHome: r.seen.filter((p) => p.startsWith('$home/')).length,
});

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
  const tree = discoverTree(nub, path.join(root, 'discover'), pkg, version);
  const others = tree.names;

  // ⛔ SCREEN BEFORE ANY SCRIPT RUNS. Everything above this line used --ignore-scripts;
  // everything below EXECUTES third-party code on this machine.
  const flagged = screenForMalicious(tree.specs);
  if (flagged.length) {
    return {
      pkg, version, verdict: 'MALICIOUS-PACKAGE-DETECTED',
      flagged, provenance: provenance(nub),
    };
  }

  const cells = [];
  // THE CONTROL IS THE TOP CELL. With every other package pinned at full grant, "the tested
  // package gets everything" is the most permissive state that exists — so it doubles as the
  // control and the top-of-ladder check, and the two can no longer disagree about the regime
  // they run under. A failure here means the package is broken for reasons no grant fixes.
  const control = cell('control', { catalogFile: write(STATES[STATES.length - 1]), label: 'control (tested pkg: everything)' });
  // RUN THE CONTROL TWICE. A single control cannot tell "this cell lacks a capability" from
  // "this package does not write the same paths twice". `unrs-resolver@1.12.2` failed all 55
  // states on three consecutive runs for the second reason — one npm debug log whose FILENAME
  // carries a timestamp — and read as a package no grant could fix.
  //
  // The two runs are combined by UNION, never intersection. Intersecting compares on fewer
  // paths, so a cell that failed to write an unstable path still passes and the search records
  // too NARROW a minimum: the exact failure this effort exists to avoid. The union is what
  // `writePaths` is derived from, so a directory seen in only one run is still promoted.
  const controlB = cell('controlB', { catalogFile: write(STATES[STATES.length - 1]), label: 'control (repeat)' });
  const inB = new Set(controlB.seen);
  const stableSeen = control.seen.filter((x) => inB.has(x));
  const unionSeen = [...new Set([...control.seen, ...controlB.seen])];
  const stableSet = new Set(stableSeen);
  const varyingSeen = unionSeen.filter((x) => !stableSet.has(x));
  // ESCALATE ONLY FOR A SCOPE THE STABLE SET DOES NOT ALREADY COVER. A path whose only
  // variation is its NAME sits in a scope the stable paths already prove — a timestamped log
  // inside an otherwise-measured directory is the canonical case — so widening there buys
  // nothing and costs breadth on every package that writes one. Escalation is for a scope the
  // run genuinely could not measure.
  const stableScopes = new Set(stableSeen.map((x) => scopeOf(x, pkg)).filter(Boolean));
  const unmeasuredScopes = [...new Set(varyingSeen.map((x) => scopeOf(x, pkg)).filter(Boolean))]
    .filter((sc) => !stableScopes.has(sc));
  // Downstream derivations read the UNION, so nothing is lost by appearing in only one run.
  const controlU = { ...control, seen: unionSeen, files: unionSeen.length };
  cells.push({ index: null, state: 'CONTROL (tested pkg: everything; all others: everything)', cost: null, pass: control.rc === 0,
               rc: control.rc, digest: control.digest, files: control.files,
               overrideEngaged: null, materialized: control.materialized });
  // One check, because the control IS the most permissive state. Failing here means no
  // grant can help — the package is broken for reasons confinement does not cause.
  if (control.rc !== 0 || !control.overrideOk || controlB.rc !== 0 || !controlB.overrideOk) {
    return { pkg, version, verdict: 'BROKEN-EVEN-WITH-EVERYTHING', cells, control: baseCase(control) };
  }
  const sideEffectful = hasScript;
  // A cell passes iff it reproduces every path BOTH control runs produced. Paths only one
  // control produced are not required — the control itself is not reliable about them — and
  // the scopes they sit in are handled by escalating the recorded grant instead.
  const matches = (r) => {
    if (r.rc !== control.rc) return false;
    if (!sideEffectful) return true;
    const has = new Set(r.seen);
    for (const p of stableSet) if (!has.has(p)) return false;
    return true;
  };

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
      const wp = floor ? homeWritePaths(controlOnly(controlU, floor)) : null;
      const verifyWritePaths = () => {
        if (!wp || !wp.length) return null;
        const v = cell(`verify${i}`, {
          catalogFile: (() => {
            const f = path.join(root, `cat-verify-${i}.json`);
            fs.writeFileSync(f, JSON.stringify(catalogWithWritePaths(pkg, STATES[i], others, wp), null, 2));
            return f;
          })(),
          label: 'writePaths verification',
        });
        // A promoted path lands in the fixture's REAL home (`home/<entry>/...`), which
        // `tokenise` leaves alone; an unpromoted one stays in the throwaway and tokenises to
        // `$home/<entry>/...`. So the two are distinguishable in one scan.
        const real = v.seen.filter((q) => wp.some((e) => q.startsWith(`home/${e}/`) || q === `home/${e}`)).length;
        const kept = v.seen.filter((q) => wp.some((e) => q.startsWith(`$home/${e}/`) || q === `$home/${e}`)).length;
        return { rc: v.rc, promotedIntoRealHome: real, leftInThrowaway: kept, entries: wp };
      };
      return {
        pkg, version,
        verdict: 'MINIMUM',
        cells,
        // THE CATALOG FRAGMENT, machine-readable. `state` beside it is for humans.
        grant: (() => { let g = grantFor(STATES[i]); if (g) delete g.notes; return escalate(g, unmeasuredScopes); })(),
        state: STATES[i].label,
        cost: STATES[i].cost,
        declaresInstallScript: hasScript,
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
        pathsBlockedWithoutGrantCount: floor ? controlOnly(controlU, floor).length : null,
        // HOW MANY blocked paths land in the THROWAWAY home — a count, not a verdict.
        //
        // The jail redirects `$HOME` to a per-package directory that is discarded, so a
        // package caching under `~/.cache/<vendor>` writes there: the install passes,
        // reproduces the control exactly, and the artefact is thrown away. MEASURED —
        // puppeteer's browser is 359 paths under `jail-home/puppeteer/.cache/puppeteer/`
        // with ZERO under the real `~/.cache/puppeteer`.
        //
        // A BOOLEAN WAS WRONG TWICE: first it tested a `jailhome/` prefix the whole-root
        // scan stopped producing, then "every blocked path is ephemeral" read False because
        // a few bookkeeping paths sit elsewhere. A count cannot be quietly inert — 0 means
        // nothing landed there, and any other number is the promotion list's raw material.
        // The derived `writePaths` entry: the minimal directories that must survive.
        writePaths: wp,
        // DID THE PROMOTION ACTUALLY HAPPEN? Counts, not a boolean — a number cannot go
        // quietly inert, and this is the field that says whether the catalog's `writePaths`
        // is worth anything. `promotedIntoRealHome` 0 with a non-empty `writePaths` is a
        // defect in the MOVER, not in the package.
        writePathsVerified: verifyWritePaths(),
        pathsLandingInThrowawayHome: floor
          ? controlOnly(controlU, floor).filter((p) => p.startsWith('$home/')).length
          : null,
        pathsBlockedWithoutGrant: floor ? controlOnly(controlU, floor).slice(0, 40) : null,
        control: baseCase(controlU),
        // Two identical control runs, reconciled. `unstablePathCount` 0 means the package is
        // deterministic and the verdict rests on a clean comparison; non-zero means these
        // scopes were escalated into the grant because they could not be measured.
        unstablePathCount: varyingSeen.length,
        unmeasuredScopesGranted: unmeasuredScopes,
      };
    }
  }
  return { pkg, version, verdict: 'NO-STATE-PASSED', cells, control: baseCase(controlU), unstablePathCount: varyingSeen.length };
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
  // ESCALATION MUST BE ABLE TO FIRE. The recurring defect in this harness has not been a wrong
  // answer but a check that could never fire — a clamp that dropped every path, a flag whose
  // second condition was never true. Assert the widening directly rather than trusting that a
  // real package will exercise it.
  const esc = [
    ['no unmeasured scopes leaves the grant untouched', { write: { deps: true } }, [], { write: { deps: true } }],
    ['an unmeasured scope is added to write', { write: { deps: true } }, ['userHome'], { write: { deps: true, userHome: true } }],
    ['escalating from nothing produces a write', null, ['project'], { write: { project: true } }],
    ['disk subsumes every narrow scope', { write: { deps: true } }, ['disk'], { write: 'disk' }],
    // The parser REJECTS a read the write already covers, so widening must drop it or the
    // catalog it emits will not load — an escalation that produces an invalid grant is worse
    // than none, because it fails at the next cell instead of here.
    ['a read the widened write now covers is dropped', { read: { userHome: true }, write: { deps: true } }, ['userHome'], { write: { deps: true, userHome: true } }],
  ];
  for (const [name, grant, scopes, want] of esc) {
    const got = escalate(grant, scopes);
    if (JSON.stringify(got) !== JSON.stringify(want)) {
      console.log(`  ESCALATE "${name}": got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`); bad++;
    }
  }
  console.log(`escalate: ${esc.length} cases`);
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

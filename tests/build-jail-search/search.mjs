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
    // ENTRY SHAPE, not the retired `[grant]` array: the parser rejects an array outright, and a
    // rejected catalog makes EVERY cell a HARNESS-ERROR rather than a measurement.
    packages[o] = { default: { write: 'disk', network: true, notes: 'held at full grant: not the variable' } };
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

  // A cell probes ONE version, so the grant is the entry's `default` and there are no bands --
  // version banding is a COLLATION concern, decided across records, never inside a single cell.
  packages[pkg] = { default: grant };
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
    // GIT'S OWN BOOKKEEPING IS NOT A SIDE EFFECT, but `.git/hooks` underneath it very much is.
    //
    // This test was INERT until now: it read `!prefix`, i.e. only at the top level of the scan —
    // but the scan root is the FIXTURE root holding `proj/` and `home/`, so `.git` was always
    // reached with prefix `proj` and the whole tree was walked. It went unnoticed while the
    // repo was empty; giving the fixture a real initial commit made git objects appear, and a
    // commit object embeds a timestamp, so they differ per fixture and read as side effects.
    if (e.name === '.git' && e.isDirectory()) {
      out.push(...paths(path.join(root, e.name, 'hooks'), `${rel}/hooks`));
      continue;
    }
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
  // Config keys a real consumer would carry. Same reason as the fixture files below: a script
  // that bails for a missing key measures as "needs nothing", which is the verdict that ships a
  // broken grant. `simple-git-hooks` reads its own key and exits silently without one.
  const manifest = {
    name: 'searchfix',
    version: '1.0.0',
    private: true,
    type: 'module',
    main: 'src/index.ts',
    engines: { node: '>=18' },
    scripts: { build: 'echo build', test: 'echo test' },
    dependencies: { [pkg]: version },
    'simple-git-hooks': { 'pre-commit': 'echo nub-fixture' },
    husky: { hooks: { 'pre-commit': 'echo nub-fixture' } },
  };
  if (jailOff) manifest.dependenciesMeta = { [pkg]: { sandbox: false } };
  fs.writeFileSync(path.join(proj, 'package.json'), JSON.stringify(manifest, null, 2));
  // side-effects-cache=false is load-bearing: a warm cache replays a prior build
  // and the lifecycle script NEVER SPAWNS, which reads exactly like a jail denial.
  fs.writeFileSync(path.join(proj, '.npmrc'), 'side-effects-cache=false\n');
  // A REAL repository, not a bare `git init`. An empty repo has no HEAD, no identity and no
  // remote, and tools check all three: `git rev-parse HEAD` fails before the first commit, a
  // hook that commits fails without user.email, and anything deriving a repo name reads
  // `remote.origin.url`. Each of those is another silent bail.
  const git = (...a) => execFileSync('git', a, { cwd: proj, stdio: 'ignore' });
  git('init', '-q', '.');
  git('config', 'user.email', 'fixture@nub.invalid');
  git('config', 'user.name', 'nub fixture');
  git('config', 'commit.gpgsign', 'false');
  git('remote', 'add', 'origin', 'https://github.com/nub-fixture/fixture.git');
  fs.writeFileSync(path.join(proj, '.gitignore'), 'node_modules\n');
  fs.writeFileSync(path.join(proj, 'README.md'), '# fixture\n');
  git('add', '-A');
  // Pinned dates: a commit object embeds author/committer time, so an unpinned commit differs
  // in every fixture and injects varying paths into the oracle.
  execFileSync('git', ['commit', '-q', '-m', 'initial'], {
    cwd: proj,
    stdio: 'ignore',
    env: { ...process.env,
      GIT_AUTHOR_DATE: '2020-01-01T00:00:00Z', GIT_COMMITTER_DATE: '2020-01-01T00:00:00Z' },
  });

  // A REALISTIC PROJECT, not an empty directory.
  //
  // A script that BAILS for a missing precondition is UNTESTED, not passing — and it looks
  // exactly like a package that needs nothing, which is the verdict that ships a broken grant.
  // `simple-git-hooks` bails without its config key; hook installers bail without a `.git`.
  // Every precondition added here converts a silent no-op into a real measurement.
  //
  // Keep these GENERIC. This is scaffolding a real project would plausibly have, not a
  // per-package fixture — the moment it becomes "what does package X need", it is the curated
  // list this whole effort exists to delete.
  fs.mkdirSync(path.join(proj, 'prisma'), { recursive: true });
  fs.writeFileSync(path.join(proj, 'prisma', 'schema.prisma'),
    'generator client {\n  provider = "prisma-client-js"\n}\n\n' +
    'datasource db {\n  provider = "postgresql"\n  url = env("DATABASE_URL")\n}\n');
  fs.mkdirSync(path.join(proj, 'src'), { recursive: true });
  fs.writeFileSync(path.join(proj, 'src', 'index.ts'), 'export const x = 1;\n');
  fs.writeFileSync(path.join(proj, 'tsconfig.json'),
    JSON.stringify({ compilerOptions: { target: 'ES2022', module: 'ESNext', strict: true } }, null, 2));

  // ⛔ DO NOT PUT A package-lock.json IN THIS FIXTURE. It was tried and REVERTED the same day.
  //
  // The motivation was real: union@0.6.0's postinstall runs `npx npm-force-resolutions`, which
  // opens `./package-lock.json` and dies with ENOENT because nub writes nub.lock.
  //
  // But a package-lock.json makes nub detect npm as the INCUMBENT PACKAGE MANAGER
  // (`pm_engine/mod.rs`, "package-lock.json" -> "npm"), which changes project identity, config
  // scope and layout. MEASURED consequence: puppeteer's control went from 9,629 installed files
  // and materialized=True to 32 files and materialized=False, and ALL EIGHT packages in a
  // re-measure — including puppeteer and cypress, which cannot work without downloading a
  // binary — came back needing NOTHING. A fixture that silently stops the scripts from doing
  // anything makes every package look free.
  //
  // The union case does not need this: with the npm-oracle fixed to key on whether npm ALSO
  // failed, it classifies correctly as BROKEN-IN-ENVIRONMENT. A package that genuinely needs an
  // npm lockfile is a nub compat question about what file nub leaves in the project, not
  // something to paper over in the measuring fixture.

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

function runCell(nub, { proj, home }, { catalogFile, label, ignoreScripts, pkg, logDir, nodeBin }) {
  // Environment a real project would carry. `DATABASE_URL` is the prisma family's precondition —
  // without it `prisma generate` bails and measures as "needs nothing". Kept generic and
  // non-secret for the same reason as the fixture files above.
  // ⛔ SCRUB CI DETECTION. A large family of postinstalls — husky, cypress, puppeteer,
  // telemetry installers — deliberately SKIP their work when they believe they are on CI. The
  // harness spreads `process.env`, so a sweep run on a CI runner would measure those packages as
  // needing nothing and ship grants derived from scripts that never executed. The catalog is
  // meant to be regenerated on Linux and Windows machines, which is exactly where CI is set.
  //
  // Scrubbed rather than forced to a value: we want the DEVELOPER-MACHINE path, which is the one
  // that runs more code, and a wider measured grant is the safe direction.
  const env = { ...process.env, HOME: home, DATABASE_URL: 'postgresql://user:pass@localhost:5432/db' };
  // NO PYTHON PIN. A pin was added here on the theory that gyp rejects Python 3.14 — REFUTED:
  // node-gyp 12.4.0 declares `semverRange = '>=3.6.0'` in `lib/find-python.js`, which 3.14
  // satisfies, and `canvas@2.11.2` compiled cleanly under it once `pkg-config` was reachable.
  // The real failure was `pkg-config pixman-1` exiting 127 — a missing SYSTEM library, nothing
  // to do with Python. Left unpinned so the harness inherits whatever the host resolves, which
  // is what a user gets.

  for (const k of ['CI', 'CONTINUOUS_INTEGRATION', 'BUILD_NUMBER', 'RUN_ID', 'GITHUB_ACTIONS',
                   'GITLAB_CI', 'CIRCLECI', 'TRAVIS', 'JENKINS_URL', 'TEAMCITY_VERSION',
                   'BUILDKITE', 'DRONE', 'APPVEYOR', 'CODEBUILD_BUILD_ID', 'TF_BUILD']) {
    delete env[k];
  }
  if (catalogFile) env[OVERRIDE_ENV] = catalogFile;
  // PIN THE NODE. nub resolves its Node from PATH, so prefixing is the whole mechanism — and the
  // reference arms take the same prefix, or the oracle compares two RUNTIMES rather than two
  // package managers. Null when the chosen major is not installed, in which case the run stays on
  // the host's Node and provenance records what actually ran.
  if (nodeBin) env.PATH = `${nodeBin}:${env.PATH ?? ''}`;
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
  // ALWAYS PERSIST THE RAW LOG. Reconstructing a failure after the fact means re-running it,
  // and by then the fixture is gone and the binary may have moved. A log costs kilobytes and
  // removes an entire class of "I had to guess what went wrong".
  if (logDir) {
    try {
      fs.mkdirSync(logDir, { recursive: true });
      fs.writeFileSync(path.join(logDir, `${(label || 'cell').replace(/[^a-z0-9]+/gi, '-')}.log`), log);
    } catch {}
  }
  // ⛔ GROUND TRUTH FOR "DOES THIS RUN A SCRIPT" IS THE INSTALLED MANIFEST, NOT THE PACKUMENT.
  //
  // MEASURED on fsevents@2.3.3 (35.9M downloads/wk): the packument's version record carries
  // `"install": "node-gyp rebuild"`; the published TARBALL's package.json does NOT. The tarball is
  // what nub materializes and what the script runner reads, so the packument is simply wrong about
  // what will execute. Asking npm made this harness disagree with nub's own
  // `declares_install_script` (which reads the manifest out of the CAS), and the disagreement
  // surfaced as a spurious "declares a script but was not materialized" for a package that
  // declares none. Read it from the tree that was just installed and the two cannot drift.
  const declaresScript = (() => {
    for (const rel of [
      [proj, 'node_modules', ...(pkg ?? '').split('/'), 'package.json'],
      [proj, 'node_modules', '.store', `${(pkg ?? '').replace('/', '+')}@*`, 'node_modules',
        ...(pkg ?? '').split('/'), 'package.json'],
    ]) {
      try {
        const s = JSON.parse(fs.readFileSync(path.join(...rel), 'utf8')).scripts ?? {};
        return ['preinstall', 'install', 'postinstall'].some((k) => k in s);
      } catch {}
    }
    return null;   // could not read it — the CALLER decides, rather than a false reading as "no"
  })();

  return {
    label, rc, log, seen,
    declaresScript,
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
function runDirFor(dir, pkg, version) {
  // A VERSION IS A DIRECTORY, not a file. The record is one artefact of a run; the per-cell
  // logs are the rest, and they are what a later reader actually needs when a verdict is
  // surprising. Keeping them in one directory means nothing has to stay in sync with a
  // parallel tree, and `ls` on a version shows everything that run produced.
  //
  //   results/runs/<platform>/<package>/<version>/results.json
  //   results/runs/<platform>/<package>/<version>/control.log
  //   results/runs/<platform>/<package>/<version>/s13-write-project.log
  return path.join(dir, `${process.platform}-${process.arch}`, pkg.replace('/', '+'), version);
}

function runPath(dir, pkg, version) {
  return path.join(runDirFor(dir, pkg, version), 'results.json');
}

/** The BUILD TOOLCHAIN this run had available.
 *
 *  ⛔ THE SINGLE MOST EXPENSIVE OMISSION IN THIS EFFORT. One `canvas@2.11.2` failure was
 *  diagnosed FIVE different wrong ways — Node 26, then Python 3.14, then gyp incompatibility,
 *  then a missing prebuilt, then a missing toolchain — and the actual cause was `PATH`: one
 *  fixture reached `/opt/homebrew/bin` and found `pkg-config`, another did not and got exit
 *  127. Same machine, same package, opposite verdicts, and NOTHING in either record said so.
 *
 *  Every one of those five wrong answers dies instantly against a record that states
 *  `pkg-config: /opt/homebrew/bin/pkg-config` versus `pkg-config: null`. A native build is
 *  decided by ambient state, so the ambient state is part of the measurement. */
/** ⛔ THE MEASUREMENT NODE DECIDES THE ANSWER, SO IT MUST BE CHOSEN, NOT INHERITED.
 *
 *  MEASURED: `@puppeteer/browsers` extracts 2 of 17 zip entries on node 26.5.0 and 17 of 17 on
 *  node 22.7.0 — same host, same zip, and plain `unzip` handles it on either. Five packages in the
 *  random-100 sweep were recorded as BROKEN-EVEN-WITH-EVERYTHING (the verdict reserved for a nub
 *  defect) because of it. A Node the package was never built against manufactures failures that
 *  are indistinguishable from real grant gaps.
 *
 *  THE RULE: take the FLOOR the package declares in `engines.node`, then CLAMP it.
 *
 *  The clamp is not tidiness — `engines` floors run to `>=6` and `>=0.10`, and nub itself does not
 *  run below 18.19 (its compat tier starts there), so an unclamped floor would measure a runtime
 *  nub cannot support. The upper clamp keeps us off a Node newer than the ecosystem, which is the
 *  failure this exists to prevent.
 *
 *  MEASURED across 7 packages: every `engines.node` is a LOWER bound that Node 26 satisfies, and 2
 *  of 7 declare none at all. So `engines` alone can never pick a working version — it selects a
 *  floor, and the clamp and default do the rest. */
/** The package's own `engines.node`, read from the registry — metadata only, no tarball. Null when
 *  absent or unreachable, which the resolver turns into the default. */
function enginesFor(pkg, version) {
  try {
    const r = spawnSync('npm', ['view', `${pkg}@${version}`, 'engines.node'],
      { encoding: 'utf8', timeout: 30_000 });
    const out = (r.stdout || '').trim();
    return r.status === 0 && out ? out : null;
  } catch { return null; }
}

// ⛔ THE FLOOR IS WHATEVER IS ACTUALLY INSTALLED — not a hardcoded number.
//
// nub's 18.19 floor governs its RUNTIME augmentation and does not bind here, which three
// measurements establish:
//   - a lifecycle script receives the REAL node binary — `process.execPath` is `.../bin/node`,
//     `argv0` is `node` — so there is no shim and no nub-spawns-node double spawn;
//   - it runs with `NODE_COMPAT=1`, augmentation off by construction;
//   - `nub install` itself completes on node 4, 6, 8, 10, 14, 16 and 18 (all exit 0).
//
// A hardcoded floor was tried at 10 and was simply WRONG: v4, v6 and v8 were installed all along,
// so a package declaring `>=6` measured on 10 for no reason. Deriving the floor from the installed
// set is self-adjusting and cannot drift from reality the way a constant does.
const NODE_FLOOR = (() => {
  try {
    const root = path.join(os.homedir(), '.nvm', 'versions', 'node');
    const majors = fs.readdirSync(root)
      .map((n) => /^v(\d+)\./.exec(n)?.[1]).filter(Boolean).map(Number);
    return majors.length ? Math.min(...majors) : 10;
  } catch { return 10; }
})();
const NODE_DEFAULT = '22';      // when `engines.node` is absent or unparseable
const NODE_CEILING = 22;        // latest LTS — never measure on something newer than the ecosystem

function resolveNodeMajor(enginesNode) {
  if (!enginesNode || typeof enginesNode !== 'string') return NODE_DEFAULT;
  // Take the smallest major any comparator mentions: `>=14 <21` floors at 14, `^18.0.0` at 18.
  // Take the MAJOR of each version token, never every number that happens to precede a dot:
  // `^20.0.0` contains "0." twice, and matching those made its floor 0 instead of 20.
  const majors = [...enginesNode.matchAll(/(?:^|[\s|,>=<~^])v?(\d+)(?:\.\d+)*/g)]
    .map((m) => Number(m[1]))
    .filter((n) => Number.isFinite(n) && n >= 0);
  if (!majors.length) return NODE_DEFAULT;
  const floor = Math.min(...majors);
  return String(Math.min(Math.max(floor, NODE_FLOOR), NODE_CEILING));
}

/** The PATH prefix that pins a probe to `major`, or null when no such Node is installed. nub
 *  resolves its Node from PATH (verified), so this is all the pinning that is needed — and the
 *  REFERENCE ARMS must use the same one, or the oracle compares two runtimes rather than two
 *  package managers. */
function nodeBinFor(major) {
  const root = path.join(os.homedir(), '.nvm', 'versions', 'node');
  let best = null;
  try {
    for (const name of fs.readdirSync(root)) {
      const m = /^v(\d+)\.(\d+)\.(\d+)$/.exec(name);
      if (!m || m[1] !== String(major)) continue;
      const v = [Number(m[2]), Number(m[3])];
      if (!best || v[0] > best.v[0] || (v[0] === best.v[0] && v[1] > best.v[1])) {
        best = { dir: path.join(root, name, 'bin'), v, name };
      }
    }
  } catch {}
  return best;
}

function toolchain() {
  // `which`, not `command -v` through a shell: passing args with `shell: true` is deprecated
  // (DEP0190, args are concatenated rather than escaped) and would print a warning on every run.
  const which = (c) => {
    const r = spawnSync('which', [c], { encoding: 'utf8' });
    return r.status === 0 ? (r.stdout || '').trim() || null : null;
  };
  const ver = (c, a) => {
    const r = spawnSync(c, a, { encoding: 'utf8' });
    return r.status === 0 ? ((r.stdout || r.stderr || '').split('\n')[0] || '').trim() : null;
  };
  return {
    node: process.version,
    pkgConfig: which('pkg-config'),
    pkgConfigVersion: ver('pkg-config', ['--version']),
    python: which('python3'),
    pythonVersion: ver('python3', ['--version']),
    make: which('make'),
    cc: ver('cc', ['--version']),
    // The PREFIX matters, not the whole variable: whether Homebrew's bin is reachable is the
    // thing that decided the canvas verdict, and the full PATH is noise around that fact.
    pathPrefix: (process.env.PATH || '').split(':').slice(0, 4).join(':'),
  };
}

/** What produced this record. Without it a results directory is a pile of numbers with no
 *  way to tell which binary or host they came from — and this effort has already been
 *  burned by results whose provenance had to be reconstructed after the fact. */
function provenance(nub, nodePin, enginesNode, nodeMajor) {
  // THE MEASUREMENT NODE IS PART OF THE RESULT. A grant measured on a Node the package was never
  // built against is not the package's grant, so the record carries what was DECLARED, what was
  // CHOSEN, and what actually RAN — not just the last of the three.
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
    // THE MEASUREMENT NODE IS PART OF THE RESULT. A grant measured on a Node the package was never
    // built against is not that package's grant, so the record carries what was DECLARED, what the
    // rule CHOSE, what was actually PINNED, and what the host would have supplied unpinned.
    nodeSelection: {
      enginesNode: enginesNode ?? null,
      chosenMajor: nodeMajor ?? null,
      pinnedTo: nodePin?.name ?? null,
      hostNode: process.version,
    },
    node: process.version,
    toolchain: toolchain(),
    at: new Date().toISOString(),
  };
}


/** Every package name in the installed tree, from the virtual store. Needed because the
 *  background grant must name each one — the catalog is keyed by name and has no wildcard,
 *  deliberately (a wildcard would be `disk` for everything, which is the shape this whole
 *  model exists to avoid). Learned by one cheap `--ignore-scripts` install. */
function discoverTree(nub, dir, pkg, version, nodeBin) {
  const fx = makeFixture(dir, pkg, version, { jailOff: true });
  // The discovery arm installs with `--ignore-scripts`, so the tree is on disk and its manifests
  // are readable — which makes this the cheapest honest place to answer "does it declare a
  // script", without a second install and without asking the packument.
  const disc = runCell(nub, fx, { label: 'discover', ignoreScripts: true, pkg, nodeBin });
  const declaresScript = disc.declaresScript;
  const store = path.join(fx.proj, 'node_modules', '.store');
  let entries = [];
  try { entries = fs.readdirSync(store); } catch { return { names: [], specs: [], declaresScript }; }
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
  return { names: [...names], specs: [...specs], declaresScript };
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
  // COLLAPSE, BUT NEVER PAST A SHARED ROOT.
  //
  // Many observed paths must become one directory entry — cypress wrote 18,673 of them — so
  // some collapsing is required. But an unbounded collapse walks up to the shared ancestor,
  // and collapsing cypress by a fixed depth produced `Library/Caches`: the cache root of every
  // application on the machine, when the vendor directory was one segment further down. The
  // failure got WIDER the more a package wrote, which is exactly backwards.
  //
  // So the entry is the LONGEST shared root that prefixes the path, plus ONE segment — the
  // first directory the package itself owns. `.cache` + `puppeteer`, `Library/Caches` +
  // `Cypress`. Two vendors under one root therefore yield TWO entries and never their parent,
  // which is the case a shared-ancestor collapse gets most wrong.
  //
  // A path with nothing below the root is a FILE, not a directory grant, and is dropped.
  const roots = (BASELINE.sharedHomeRoots ?? []).map((r) => r.toLowerCase())
    .sort((a, b) => b.length - a.length);
  const dirs = new Set();
  for (const p of paths) {
    if (!p.startsWith('$home/')) continue;
    const rel = p.slice('$home/'.length);
    const low = rel.toLowerCase();
    const root = roots.find((r) => low === r || low.startsWith(`${r}/`));
    const depth = root ? root.split('/').length + 1 : 1;
    const segs = rel.split('/');
    if (segs.length <= depth) continue;
    dirs.add(segs.slice(0, depth).join('/'));
  }
  // A shallower entry still subsumes a deeper one when both survived (a package owning both
  // `x` and `x/y`); keep only the shallowest.
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

/** A comparable failure signature: the FIRST real error, not the last.
 *
 *  Logs end with a stack trace and a "failed to execute" summary; the cause sits far earlier.
 *  Reading the tail is how one canvas failure got diagnosed four different wrong ways in a row
 *  (Node 26, then Python 3.14, then gyp, then a missing prebuilt) when the log had said
 *  `node-pre-gyp ERR! install response status 404` and `pkg-config pixman-1 ... exit status 127`
 *  from the first run.
 *
 *  Normalised so two runs are comparable: absolute paths, versions and hashes differ between npm
 *  and nub for reasons that are not the failure. */
function failureSignature(log) {
  const line = (log || '').split('\n').find((l) => /\b(ERR!|error|fatal|failed|not ok)\b/i.test(l)
    && !/^\s*(npm )?(warn|notice)/i.test(l));
  if (!line) return null;
  return line
    .replace(/\/[^\s"']+/g, '<path>')
    .replace(/\bv?\d+\.\d+\.\d+\b/g, '<ver>')
    .replace(/\b[0-9a-f]{8,}\b/g, '<hash>')
    .trim()
    .slice(0, 160);
}

/** Does npm fail the same way? npm is the REFERENCE: nub's job is to match it.
 *
 *  A package that cannot install in this environment AT ALL — no prebuilt for this
 *  architecture, a missing system library, a toolchain nub does not control — is not a grant
 *  problem, and no entry in the catalog can fix it. MEASURED: `canvas@2.11.2` publishes 40
 *  release assets and ZERO for darwin-arm64, so on Apple Silicon it always compiles from source
 *  and always needs cairo/pkg-config. Recording that as "needs write.disk" would ship a wide
 *  grant for a package that never installs here regardless. */
/** Publish date and weekly downloads, for judging whether an npm failure is suspicious.
 *
 *  Best-effort and never fatal: no network, no metadata, no flag — the verdict still stands on
 *  the signature comparison. */
function packageStanding(pkg, version) {
  const j = (cmd, args) => {
    const r = spawnSync(cmd, args, { encoding: 'utf8', timeout: 30_000 });
    if (r.status !== 0) return null;
    try { return JSON.parse(r.stdout); } catch { return null; }
  };
  const time = j('npm', ['view', `${pkg}@${version}`, 'time', '--json']);
  const published = time && typeof time === 'object' ? time[version] ?? null : null;
  const dl = j('curl', ['-sS', '--max-time', '20',
    `https://api.npmjs.org/downloads/point/last-week/${encodeURIComponent(pkg)}`]);
  const weekly = dl && typeof dl.downloads === 'number' ? dl.downloads : null;
  const ageDays = published ? Math.floor((Date.now() - Date.parse(published)) / 86_400_000) : null;
  // THE DIST-TAG, because the collator generates the catalog's `default` grant from LATEST and
  // must not have to guess which measured version that is. Guessing highest-measured is right
  // only when latest was actually probed; when it was not, `default` comes from an older version
  // and every FUTURE release is under-granted — silently, in the one direction that breaks
  // packages. Recorded per run so a catalog collated months later still knows what `latest`
  // meant at measurement time rather than at collation time.
  const tags = j('npm', ['view', pkg, 'dist-tags', '--json']);
  const latestVersion = tags && typeof tags === 'object' ? tags.latest ?? null : null;
  return { published, ageDays, weeklyDownloads: weekly, latestVersion };
}

/** ⛔ A REFERENCE ARM THAT NEVER RAN THE SCRIPT MEASURES NOTHING.
 *
 *  BOTH mainstream PMs now refuse dependency install scripts by default, so the naive invocation
 *  silently answers a different question than the one asked:
 *
 *    npm 11.17+  skips anything not covered by `allowScripts`. From npm's own source,
 *                arborist/lib/arborist/rebuild.js: `if (key !== 'bin' && !scriptsAllowed) continue`
 *                — never queued, not warned-and-run. `--allow-scripts <list>` is REJECTED in a
 *                project-scoped install, so the boolean is the only flag form.
 *    pnpm 10+    skips anything absent from `pnpm.onlyBuiltDependencies` ("Ignored build scripts").
 *
 *  MEASURED consequence of missing this: the arm returned rc 0 for every package because no
 *  lifecycle script ever ran, and BROKEN-EVEN-WITH-EVERYTHING — the verdict that claims a nub
 *  defect — rested entirely on it.
 *
 *  Both also BUFFER script output. npm needs `--foreground-scripts` or the script's own stderr
 *  never appears, which is why signature comparison was matching nub's INNER error against npm's
 *  OUTER wrapper. pnpm's `--reporter=ndjson` emits structured per-lifecycle records instead of
 *  prose, so its outcome is read as data.
 *
 *  THE ARTIFACT IS RETURNED ALONGSIDE THE EXIT CODE, because rc is not sufficient. MEASURED on
 *  puppeteer@24.40.0: npm and pnpm both exit 0 while leaving the SAME broken extraction nub
 *  reports as a failure — a valid 96MB zip, 17 entries, 2 extracted, no executable. Judged by rc
 *  that reads as a nub defect; judged by artifact all three agree and it is environmental. */
/** Every npm era's way of announcing a lifecycle script. Kept beside the arm that uses it so a
 *  new npm output format is one entry, not a rediscovered defect. */
const NPM_RAN_SCRIPT = [
  /^\s*>\s+\S+@\S+\s+(pre|post)?install\b/m,      // npm 2..10
  /\binfo run\b[^\n]*\b(pre|post)?install\b/m,      // npm 11+
];

function referenceArm(tool, dir, pkg, version, nodeBin) {
  const d = path.join(dir, `${tool}ref`);
  const fx = makeFixture(d, pkg, version, { jailOff: true });
  if (tool === 'pnpm') {
    // pnpm's gate is a manifest field, so it must be written before the install runs.
    try {
      const mf = path.join(fx.proj, 'package.json');
      const m = JSON.parse(fs.readFileSync(mf, 'utf8'));
      m.pnpm = { ...(m.pnpm ?? {}), onlyBuiltDependencies: [pkg] };
      fs.writeFileSync(mf, `${JSON.stringify(m, null, 2)}\n`);
    } catch {}
  }
  const args = tool === 'npm'
    ? ['install', '--no-audit', '--no-fund', '--dangerously-allow-all-scripts', '--foreground-scripts']
    : ['install', '--no-frozen-lockfile', '--reporter=ndjson'];
  // SAME Node as the nub arm — otherwise this compares two runtimes, not two package managers.
  const refEnv = { ...process.env, HOME: fx.home };
  if (nodeBin) refEnv.PATH = `${nodeBin}:${refEnv.PATH ?? ''}`;
  const r = spawnSync(tool, args, {
    cwd: fx.proj, env: refEnv, encoding: 'utf8', timeout: 900_000,
  });
  const log = (r.stdout || '') + (r.stderr || '');

  const lifecycle = [];
  if (tool === 'pnpm') {
    for (const line of log.split('\n')) {
      if (!line.startsWith('{')) continue;
      try {
        const e = JSON.parse(line);
        if (e.name === 'pnpm:lifecycle') lifecycle.push({ stage: e.stage, exitCode: e.exitCode ?? null, line: e.line ?? null });
      } catch {}
    }
  }

  // The artifact, via the SAME walk every cell uses. Not a judgement — a path set and a digest.
  let files = null; let digest = null;
  try { const seen = paths(d, ''); files = seen.length; digest = digestOf(seen); } catch {}

  return {
    rc: r.status ?? 1,
    signature: failureSignature(log),
    log, files, digest,
    ...(tool === 'pnpm' ? { lifecycle } : {}),
    // DID A SCRIPT ACTUALLY RUN? A count, not a boolean guess — an arm that ran none is not
    // evidence of anything, and this is what makes that visible in the record.
    // ⛔ DID A SCRIPT ACTUALLY RUN? An arm that ran none is not evidence, and this is what makes
    // that visible. It must recognise EVERY npm era, because the Node pin selects the bundled npm
    // and that ranges from npm 2 to npm 11:
    //
    //   npm 2..10  "> pkg@1.0.0 postinstall /path/to/node_modules/pkg"
    //   npm 11+    "info run pkg@1.0.0 postinstall node_modules/pkg node install.mjs"
    //
    // MEASURED cost of knowing only the npm 11 form: on every old-Node pin `ranScripts` came back
    // falsely FALSE, the arm was dropped from the verdict vote, the vote came up empty, and the
    // result defaulted to BROKEN-IN-ENVIRONMENT. Four packages were written off on that basis
    // while their oracle had in fact run the script and succeeded.
    ranScripts: tool === 'pnpm'
      ? lifecycle.length > 0
      : NPM_RAN_SCRIPT.some((re) => re.test(log)),
  };
}

const npmReference = (dir, pkg, version, nodeBin) => referenceArm('npm', dir, pkg, version, nodeBin);
const pnpmReference = (dir, pkg, version, nodeBin) => referenceArm('pnpm', dir, pkg, version, nodeBin);

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

function search(nub, pkg, version, root, keep, runDir) {
  const logDir = runDirFor(runDir, pkg, version);
  const cell = (name, opts) => {
    const dir = path.join(root, name);
    const fx = makeFixture(dir, pkg, version, { jailOff: !!opts.jailOff });
    const r = runCell(nub, fx, { ...opts, pkg, logDir, nodeBin });
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
  // RESOLVE THE MEASUREMENT NODE BEFORE ANYTHING INSTALLS, from the package's own `engines`.
  // Discovery is pinned too, so the whole probe — discovery, every cell, and both reference arms —
  // runs on ONE runtime. Recorded in provenance so a catalog entry carries the Node it was
  // measured under rather than inheriting whatever the host happened to have.
  const enginesNode = enginesFor(pkg, version);
  const nodeMajor = resolveNodeMajor(enginesNode);
  const nodePin = nodeBinFor(nodeMajor);
  const nodeBin = nodePin?.dir ?? null;

  // CONTROL — scripts on, jail off. EVERY cell runs the identical two steps, so full path
  // sets are directly comparable and no baseline subtraction is needed.
  const tree = discoverTree(nub, path.join(root, 'discover'), pkg, version, nodeBin);
  const others = tree.names;

  // Read from the INSTALLED tree, never the packument, because the two disagree and only the
  // tarball's manifest describes what will run. Unreadable falls back to `true`, keeping the
  // oracle STRICT: assuming a script exists costs a stricter comparison, while assuming none
  // costs a package silently measured as needing nothing.
  const hasScript = tree.declaresScript ?? true;

  // ⛔ SCREEN BEFORE ANY SCRIPT RUNS. Everything above this line used --ignore-scripts;
  // everything below EXECUTES third-party code on this machine.
  const flagged = screenForMalicious(tree.specs);
  if (flagged.length) {
    return {
      pkg, version, verdict: 'MALICIOUS-PACKAGE-DETECTED',
      flagged, provenance: provenance(nub, null, null, null),
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
  // ⛔ AN OVERRIDE THAT DID NOT ENGAGE IS A HARNESS FAULT, NOT A BROKEN PACKAGE.
  //
  // These are opposite verdicts and they were being merged. `BROKEN-EVEN-WITH-EVERYTHING` says
  // "no grant can save this package" — a fact about the package, worth recording. A control
  // whose override never applied says "this run measured nothing", which must never enter the
  // catalog as a fact about anything.
  //
  // MEASURED: six packages (core-js, es5-ext, union, bufferutil@4.0.9, @parcel/watcher@2.6.0,
  // nx@18.3.5, @sentry/cli@2.21.2) were recorded broken-at-the-widest-grant while installing
  // FINE jailed, unjailed and under npm. A `cargo test --profile fast` running CONCURRENTLY with
  // the batch had rebuilt the binary without the `build-jail-catalog-override` feature, so every
  // package measured after that moment failed its control. The old code returned here before
  // ever consulting `overrideOk`, so a build accident was written down as a package property.
  if (!control.overrideOk || !controlB.overrideOk) {
    return {
      pkg, version, verdict: 'HARNESS-ERROR',
      why: 'the catalog override did not engage in the control — is the binary built with '
         + '`--features nub-cli/build-jail-catalog-override`, and did anything rebuild it mid-run?',
      cells, control: baseCase(control), provenance: provenance(nub, nodePin, enginesNode, nodeMajor),
    };
  }
  if (control.rc !== 0 || controlB.rc !== 0) {
    // ⛔ ASK NPM BEFORE BLAMING THE JAIL. npm is the REFERENCE — nub's job is to match it — so a
    // package failing IDENTICALLY under npm is broken in this ENVIRONMENT, not under-granted.
    // No grant can supply a missing system library or an architecture with no published binary,
    // and inventing one would ship a wide grant for a package that never installs here anyway.
    //
    // MEASURED, and it is why this check exists: `canvas@2.11.2` publishes 40 release assets —
    // 12 darwin-x64, 14 linux-x64, 14 win32-x64 — and ZERO for darwin-arm64 at ANY ABI. On Apple
    // Silicon it therefore always compiles from source and always needs cairo/pkg-config, which
    // npm needs too. It is also 3.1M weekly downloads, 39.8% of all canvas traffic, so this is
    // the COMMON case for that package rather than an obscure old pin.
    //
    // ⚠ THE VERDICT IS LOUD, NOT SILENT. It records both signatures and that they matched, so a
    // reader can see WHY it was dismissed and re-open it. A bin that hides its reasoning is how
    // six false failures survived earlier in this effort. During probing, look in this bin.
    //
    // npm SUCCEEDING where nub fails is the opposite finding — a nub defect, the most valuable
    // thing this harness can surface, and never a grant gap.
    const ref = npmReference(root, pkg, version, nodeBin);
    const ours = failureSignature(control.log);

    // ⛔ THE VERDICT TURNS ON WHETHER NPM SUCCEEDED, NOT ON WHETHER THE TWO MESSAGES MATCH.
    //
    // `BROKEN-EVEN-WITH-EVERYTHING` claims "npm succeeds where nub fails => a nub defect". It used
    // to be emitted whenever the two failure SIGNATURES differed — including when npm had failed
    // too. That manufactures false nub defects, the most expensive noise this harness can produce.
    //
    // MEASURED on union@0.6.0 (7.1M downloads/wk): nub's first error is the inner cause
    // ("ENOENT ... open './package-lock.json'"), npm's is its own wrapper ("npm error code 1"),
    // because npm buries the child's stderr and reports its own exit. Comparing those is comparing
    // two different LAYERS of one failure, so they will differ routinely even when the cause is
    // identical.
    //
    // So: a reference PM ALSO failing is decisive — the environment, not nub. The signature
    // comparison survives only as a QUALITY signal, feeding `needsInvestigation`.
    //
    // ⛔ TWO ORACLES, AND A NUB DEFECT REQUIRES *BOTH* TO DISAGREE WITH NUB.
    //
    // One oracle cannot distinguish "nub is wrong" from "that PM is unusual". MEASURED on
    // puppeteer@24.40.0: judged against npm alone it read as a nub defect; pnpm agreed with npm,
    // and the ARTIFACTS of all three were identical (a valid 96MB zip, 17 entries, 2 extracted,
    // no executable), so it is environmental. Two oracles is what turned a confident wrong answer
    // into a correct one.
    //
    // An arm that RAN NO SCRIPTS is not evidence either way and is excluded from the vote —
    // otherwise "the PM skipped everything" counts as "the PM succeeded", which is exactly the
    // defect that produced every false nub-defect verdict in the first sweep.
    const pnpmRef = pnpmReference(root, pkg, version, nodeBin);
    const arms = [
      { name: 'npm', ...ref },
      { name: 'pnpm', ...pnpmRef },
    ].filter((a) => a.ranScripts);

    const anyArmFailed = arms.some((a) => a.rc !== 0);
    // Every arm that ran scripts SUCCEEDED, and at least one did run => nub stands alone.
    const allArmsSucceeded = arms.length > 0 && arms.every((a) => a.rc === 0);
    const signaturesMatched = anyArmFailed && !!ours
      && arms.some((a) => a.rc !== 0 && !!a.signature && a.signature === ours);
    // BROKEN-IN-ENVIRONMENT unless every arm that actually ran scripts succeeded. When NO arm ran
    // scripts we learned nothing, so default to the environment rather than accusing nub.
    const matched = !allArmsSucceeded;

    // ⛔ A RECENT, POPULAR PACKAGE FAILING UNDER NPM INDICTS OUR ENVIRONMENT, NOT THE PACKAGE.
    //
    // "npm fails too" is a sound dismissal for something old and rarely installed. It is a RED
    // FLAG for something current with real traffic: thousands of people install that package
    // successfully every week, so if it fails here the missing piece is almost certainly a
    // toolchain WE have not provided, and dismissing it would silently drop a package the
    // catalog needs.
    //
    // Recorded as the raw numbers, not just a boolean — the thresholds are a starting point and
    // a reader needs the figures to re-judge them. A count cannot go quietly inert the way a
    // flag can.
    const standing = packageStanding(pkg, version);
    const recent = standing.ageDays !== null && standing.ageDays < 730;
    const popular = standing.weeklyDownloads !== null && standing.weeklyDownloads > 100_000;
    const needsInvestigation = matched && recent && popular;
    return {
      pkg, version,
      verdict: matched ? 'BROKEN-IN-ENVIRONMENT' : 'BROKEN-EVEN-WITH-EVERYTHING',
      grant: null,
      // Carried on the FAILURE path too, not just on MINIMUM. It was missing here, so the
      // watcher's placement warning — which keys on `declaresInstallScript && !projectAxis` —
      // was silently false on exactly the records most likely to need it.
      declaresInstallScript: hasScript,
      // BOTH arms recorded in full, including whether each actually RAN any script and what its
      // artifact looked like. `ranScripts: false` means that arm is not evidence — the record has
      // to say so, or a later reader repeats the mistake that produced every false nub defect.
      npmReference: {
        rc: ref.rc, signature: ref.signature, ranScripts: ref.ranScripts, files: ref.files, digest: ref.digest,
      },
      pnpmReference: {
        rc: pnpmRef.rc, signature: pnpmRef.signature, ranScripts: pnpmRef.ranScripts,
        files: pnpmRef.files, digest: pnpmRef.digest, lifecycle: pnpmRef.lifecycle ?? [],
      },
      // The artifact comparison, mechanical: nub's control against each arm that ran scripts.
      // Identical file counts across all three is the signature of an ENVIRONMENTAL failure that
      // only nub bothers to report.
      artifactComparison: {
        nubControlFiles: control.fileCount ?? null,
        npmFiles: ref.files, pnpmFiles: pnpmRef.files,
        armsThatRanScripts: arms.map((a) => a.name),
      },
      failureSignature: ours,
      signaturesMatched,
      standing,
      needsInvestigation,
      investigationReason: needsInvestigation
        ? `npm ALSO fails, but this is ${standing.ageDays}d old with ${standing.weeklyDownloads} `
          + 'weekly downloads — people install it successfully every week, so suspect a missing '
          + 'toolchain on the measuring host before accepting the dismissal'
        : null,
      cells,
      control: baseCase(control),
      controlLogTail: (control.log || '').split('\n').slice(-40).join('\n'),
      logDir: runDirFor(runDir, pkg, version),
      provenance: provenance(nub, nodePin, enginesNode, nodeMajor),
    };
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
        // THE MEASUREMENT NODE, on the success path too. This return had NO provenance at all, so
        // the caller's fallback filled it with nulls and every MINIMUM record claimed the Node pin
        // was off when it was on — a record that lies about its own measurement conditions.
        provenance: provenance(nub, nodePin, enginesNode, nodeMajor),
        // RECORDED ON THE SUCCESS PATH TOO, not just on failure: these are the records that
        // BECOME catalog entries, and the collator generates `default` from whichever measured
        // version is `latest`. Without the dist-tag here it falls back to highest-measured,
        // which is right only when latest happened to be probed.
        standing: packageStanding(pkg, version),
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
        // IS AN ENTRY VERSION-PINNED? An entry containing the measured version — `.cache/foo-1.2.3`
        // — names a directory that MOVES on the next release, so shipping it matcher-less would
        // silently stop matching. Walking UP is not the fix: the next level is a shared root, and
        // widening to that is the over-granting the collapse floor exists to prevent. So it is
        // recorded, and the collator pins the grant's `versions` matcher instead.
        //
        // Exact substring of the measured version, NOT a "looks like a version" regex — a regex
        // would fire on legitimate names and miss unusual schemes, and both failures are silent.
        writePathsVersionPinned: (wp ?? []).filter((e) => e.includes(version)),
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

// THE PRE-FLIGHT MUST VALIDATE THE ARTIFACT THE CELLS ACTUALLY USE. run-batch.sh used to probe
// the override with a hand-written `{"packages":{}}`, which parses under ANY packages shape --
// so when the catalog shape changed under it, the check passed and every cell in the batch then
// failed as HARNESS-ERROR. An empty map is adjacent to the question, not the question. This emits
// a catalog straight out of `catalogFor`, so the thing proven is the thing used.
if (argv.includes('--emit-sample-catalog')) {
  const widest = STATES[STATES.length - 1];
  process.stdout.write(JSON.stringify(catalogFor('ovcheck-probe', widest, ['other-pkg']), null, 2));
  process.exit(0);
}

if (argv.includes('--selftest')) {
  let bad = 0;
  for (const s of STATES) {
    const emitted = catalogFor('x', s).packages.x;
    if (!emitted) continue;   // state 0 is 'no entry', correctly
    const g = emitted.default;
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
    console.log(' ', String(i).padStart(2), STATES[i].label, '->', e ? JSON.stringify(e.default) : '(no entry — the base profile)');
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
/** Pin the binary into a CONTENT-ADDRESSED cache and measure against the copy.
 *
 *  ⛔ ANY cargo command on a profile rewrites that profile's binary with ITS OWN features. A
 *  probe takes minutes to hours, so a `cargo test --profile fast` started meanwhile silently
 *  swaps the binary mid-run. MEASURED: that happened during a batch and six packages recorded
 *  `BROKEN-EVEN-WITH-EVERYTHING` while installing fine jailed, unjailed AND under npm — a build
 *  accident written down as a fact about each package.
 *
 *  `run-batch.sh` already snapshots for batches, but every clobbering incident came from
 *  invoking THIS script directly, where `--nub` was used live. So the pin belongs here, at the
 *  bottom, where nothing can route around it.
 *
 *  Content-addressed rather than per-PID: the same binary reuses one copy across every run,
 *  and the cache key IS the `nubSha256` already recorded in provenance — so a record names the
 *  exact file it was measured against, and it is still on disk to re-check. Stale entries are
 *  just files under `~/.cache/nub/probe-bin/`; delete the directory to reclaim it. */
function pinBinary(p) {
  try {
    const bytes = fs.readFileSync(p);
    const sha = createHash('sha256').update(bytes).digest('hex').slice(0, 16);
    const dir = path.join(os.homedir(), '.cache', 'nub', 'probe-bin');
    fs.mkdirSync(dir, { recursive: true });
    const pinned = path.join(dir, sha);
    if (!fs.existsSync(pinned)) {
      // Write-then-rename so a concurrent probe never sees a half-copied binary.
      const tmp = `${pinned}.partial-${process.pid}`;
      fs.writeFileSync(tmp, bytes, { mode: 0o755 });
      fs.renameSync(tmp, pinned);
    }
    return pinned;
  } catch {
    return p;   // unreadable or not a path (bare `nub` on PATH) — measure against it directly
  }
}

const nubArg = argv.includes('--nub') ? argv[argv.indexOf('--nub') + 1] : 'nub';
const nub = pinBinary(nubArg);
const keep = argv.includes('--keep');

// Never /tmp: a /tmp/package.json on this box is found by walking up.
const root = fs.mkdtempSync(path.join(os.homedir(), '.cache', 'nub-search-'));
try {
  const runDir = argv.includes('--runs')
    ? argv[argv.indexOf('--runs') + 1]
    : path.join(path.dirname(new URL(import.meta.url).pathname), 'results', 'runs');
  const out = runPath(runDir, pkg, version);
  fs.mkdirSync(path.dirname(out), { recursive: true });

  // RESUMABLE BY DEFAULT. A batch re-run should cost only what it has not already measured;
  // `--force` re-measures one package after a harness or jail change.
  if (!argv.includes('--force') && fs.existsSync(out)) {
    const prior = JSON.parse(fs.readFileSync(out, 'utf8'));
    console.log(JSON.stringify({ ...prior, skipped: 'already measured; pass --force to redo' }));
  } else {
    const record = search(nub, pkg, version, root, keep, runDir);
    // ⛔ DO NOT CLOBBER. `search()` sets provenance with the resolved Node selection in scope; this
    // caller does not have those values, so an unconditional assignment silently replaced a
    // populated `nodeSelection` with nulls — the record then claimed the pin was off when it was
    // on. Fill in only for a path that returned without setting one.
    record.provenance ??= provenance(nub, null, null, null);
    fs.mkdirSync(runDir, { recursive: true });
    fs.writeFileSync(out, JSON.stringify(record, null, 2));
    console.log(JSON.stringify(record));
  }
} finally {
  if (!keep) fs.rmSync(root, { recursive: true, force: true });
}

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
// ⛔ `new URL(…, import.meta.url).pathname` IS NOT A PATH ON WINDOWS. It yields `/D:/a/nub/…` with a
// LEADING SLASH, so anything that resolves it against the drive produces `D:\D:\a\nub\…` — MEASURED
// on a windows-latest runner, where all 15 packages died at
// `ENOENT: mkdir 'D:\D:\a\nub\nub\tests\build-jail-search\results\runs\…'`. `fileURLToPath` is the
// only correct conversion and is a no-op difference on POSIX.
import { fileURLToPath } from 'node:url';
import { STATES } from './states.mjs';

/** The directory this script lives in, spelled natively on every platform. */
const HERE = path.dirname(fileURLToPath(import.meta.url));

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
/** ⛔ A `network: true` CAPABILITY IS NOT WHAT GRANTS EGRESS — `packageNetwork.full` IS.
 *
 *  The two are separate halves of the schema and only ONE is read by the thing that decides egress.
 *  `parse_package_network` (`crates/nub-sandbox/src/catalog.rs`) builds the allow table from
 *  `networkHosts[].fetchedBy` and `packageNetwork.full[]`, and from NOTHING else — a per-package
 *  `network: true` never reaches it. The shipped catalog agrees: 210 `packageNetwork.full` entries
 *  and ZERO packages carrying a network capability.
 *
 *  MEASURED consequence, and why it hid for hundreds of records: macOS and Linux deny egress
 *  IN-KERNEL (Seatbelt / Landlock+seccomp) straight off the per-package caps, so the capability
 *  alone works there. Windows has no per-process network filter, so it uses the userland JS net
 *  gate, which is driven by that table — the grant was invisible and every request denied. Windows
 *  packages needing network were accused at 67-87% against 0-5% for those that do not, and
 *  `@apollo/rover@0.29.1` at the WIDEST grant downloaded on macOS while Windows logged
 *  `blocked network access to rover.apollo.dev`.
 *
 *  Derived here rather than at each call site because every catalog this file emits flows through
 *  `withFloor`, so one derivation cannot drift from a second spelling of the same intent. */
const withFloor = (cat) => {
  const full = Object.entries(cat.packages ?? {})
    .filter(([, entry]) => entry?.default?.network === true)
    .map(([name]) => ({ package: name }))
    .sort((a, b) => (a.package < b.package ? -1 : a.package > b.package ? 1 : 0));
  return {
    ...cat,
    ...(full.length ? { packageNetwork: { full } } : {}),
    baseline: BASELINE.baseline ?? [],
    env: BASELINE.env ?? [],
  };
};

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
/** ⛔ `HOME` DOES NOT REDIRECT ANYTHING ON WINDOWS. Every cell depends on a throwaway home for
 *  isolation — that is what makes one package's measurement independent of the last. But nub
 *  resolves its cache through `dirs_next::home_dir()` (`crates/nub-core/src/node/discovery.rs`,
 *  `cache_dir`), and on Windows that reads **`USERPROFILE`**, not `HOME`. npm follows the same
 *  convention.
 *
 *  So on Windows the store landed in the RUNNER'S REAL PROFILE, outside the fixture: the canary
 *  counted 113 files against macOS's 6,222 with `materialized: true` — the tree was real, it was
 *  just somewhere the walk could not see. The quieter half is worse than the miscount: cells were
 *  sharing state, so nothing was isolated.
 *
 *  `LOCALAPPDATA`/`APPDATA` go too: `cache_dir` falls back to `%LOCALAPPDATA%` when the profile
 *  looks like a system directory, and npm keeps its cache under `%APPDATA%\npm-cache`. */
function homeEnv(home) {
  if (process.platform !== 'win32') return { HOME: home };
  return {
    HOME: home,
    USERPROFILE: home,
    LOCALAPPDATA: path.join(home, 'AppData', 'Local'),
    APPDATA: path.join(home, 'AppData', 'Roaming'),
  };
}

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

/** ⛔ REMOVING A node_modules TREE ON WINDOWS FAILS WITH EPERM, ROUTINELY. A lifecycle script's
 *  child can still hold a handle for a moment after exit, and an indexer or AV scanner opens files
 *  behind you — so the delete races them. `maxRetries`/`retryDelay` exist in Node for exactly this
 *  and are no-ops on POSIX.
 *
 *  MEASURED on nub-win: a run that MEASURED PUPPETEER SUCCESSFULLY still exited 1, because the final
 *  cleanup threw `EPERM, Permission denied: \\?\C:\Users\…\.cache\nub-search-msMlUt`. run-batch.sh
 *  reads a non-zero exit as "no measurement", so a completed record was thrown away by its own
 *  tidy-up.
 *
 *  `mustSucceed` splits the two cases, which need OPPOSITE handling:
 *   - the fixture PRE-clean must still throw. A stale tree left in place is the silent-void-every-
 *     measurement hazard: the install then looks like it needed nothing (#82).
 *   - post-measurement cleanup must NEVER lose a result. Warn, leak the directory, and carry on —
 *     run-batch.sh's startup sweep collects it later. */
function rmTree(p, { mustSucceed = false } = {}) {
  try {
    fs.rmSync(p, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  } catch (e) {
    if (mustSucceed) throw e;
    console.error(`warning: could not remove ${p} (${e.code}); leaving it for the startup sweep`);
  }
}

function makeFixture(dir, pkg, version, { jailOff }) {
  rmTree(dir, { mustSucceed: true });
  const proj = path.join(dir, 'proj');
  fs.mkdirSync(proj, { recursive: true });
  fs.mkdirSync(path.join(dir, 'home'), { recursive: true });
  // The Windows redirect (see homeEnv) points LOCALAPPDATA/APPDATA inside this home, so the
  // directories have to exist — a tool that finds its cache root missing may fall back to the real
  // profile rather than create it, which would silently restore the leak this exists to close.
  if (process.platform === 'win32') {
    fs.mkdirSync(path.join(dir, 'home', 'AppData', 'Local'), { recursive: true });
    fs.mkdirSync(path.join(dir, 'home', 'AppData', 'Roaming'), { recursive: true });
  }
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
  // ⛔ THE JAIL IS TURNED OFF BY `nub.jsonc`, NOT BY `dependenciesMeta`. This wrote
  // `dependenciesMeta.<pkg>.sandbox: false`, which was the per-package opt-out until c5651408f4
  // ("one global install.buildJail switch, no per-package opt-out") DELETED every path that read
  // it. `should_confine` now ignores both of its arguments and returns `build_jail_enabled()`,
  // which reads only `install.buildJail` — so the old key became a silently INERT no-op and every
  // "jail off" cell ran WITH THE JAIL ON.
  //
  // MEASURED on a Windows shard: `hugo-extended@0.101.0`'s jail-OFF log carries
  // `nub build sandbox: blocked network access to github.com`, raised by the JS net gate inside the
  // `jail-off-control` fixture. A cell that cannot differ from the control can only ever agree with
  // it, which is exactly the shape of a BROKEN-WITHOUT-JAIL-TOO verdict — so the classifier was
  // exonerating the jail using evidence produced with the jail enabled.
  //
  // Why this surfaced on Windows first, with macOS and Linux apparently fine: it is not platform
  // specific at all. c5651408f4 landed 2026-08-01, and every macOS/Linux jail-off record predates
  // it, back when the opt-out still worked. Windows was simply the first platform measured entirely
  // after the switch moved. Any record re-measured after that date is affected on every OS.
  //
  // Orthogonality, from that commit's own message: `approveBuilds`/`allowBuilds` decide WHETHER a
  // script runs; the jail decides whether a script that runs is CONFINED. This fixture needs both —
  // approval comes from the `approve-builds --all` step in `cell`, confinement from here.
  if (jailOff) {
    fs.writeFileSync(
      path.join(proj, 'nub.jsonc'),
      `${JSON.stringify({ install: { buildJail: false } }, null, 2)}\n`,
    );
  }
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
  const env = { ...process.env, ...homeEnv(home), DATABASE_URL: 'postgresql://user:pass@localhost:5432/db' };
  // PYTHON IS PINNED ONLY FOR OLD NODE — see gypPython(). The general no-pin rule still holds and
  // its reasoning is unchanged: node-gyp 12 declares `semverRange = '>=3.6.0'`, which 3.14
  // satisfies, and the failure once blamed on Python was really `pkg-config pixman-1` exiting 127,
  // a missing SYSTEM library. Unpinned is the DEVELOPER-MACHINE path, which runs more code and
  // yields the wider, safer grant.
  const py = gypPython(nodeBin);
  if (py) env.npm_config_python = py;

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
  if (nodeBin) {
    env.PATH = `${nodeBin}:${env.PATH ?? ''}`;
    // ⛔ PATH ALONE DOES NOT PIN THE JAILED SCRIPT. nub re-resolves its own interpreter inside the
    // jail, so a PATH prefix pins the harness's spawn and is then discarded for the confined child.
    //
    // MEASURED on Linux: a record asserting `pinnedTo: v14.21.3` while its build log said
    // `gyp ERR! node -v v22.15.0` and compiled against `.cache/node-gyp/22.15.0/include`. A 2020
    // addon against Node 22 headers dies on V8 API drift, no grant fixes a C++ error, and the walk
    // escalates until `write.disk + network` lets node-gyp fetch a PREBUILT and skip the compile —
    // so the recorded grant measured WHICH CODE PATH the package took, not what it needs.
    //
    // `NODE_EXECUTABLE` is the key nub's discovery honours BEFORE any pin file, which is exactly
    // what stops the in-jail re-resolution (build_jail.rs, `pinned_interpreter`). `npm_node_execpath`
    // is what npm/node-gyp follow and what the headers are derived from — a split between them is
    // its own silent wrong answer, so set both, as nub itself does.
    const nodeExe = path.join(nodeBin, process.platform === 'win32' ? 'node.exe' : 'node');
    if (fs.existsSync(nodeExe)) {
      env.NODE_EXECUTABLE = nodeExe;
      env.npm_node_execpath = nodeExe;
    }
  }
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
/** ⛔ `npm`, `npx` and `pnpm` ARE `.cmd` SHIMS ON WINDOWS, and `spawnSync` will not find them.
 *
 *  Node resolves a bare command name against PATHEXT only when it goes through a shell. Without one
 *  `spawnSync('npm', …)` returns **ENOENT** on win32 — MEASURED on nub-win: the same call is
 *  `status: null, error: ENOENT` bare and `status: 0, 54120 bytes` with `shell: true`.
 *
 *  This silently disabled THREE things on Windows at once, and none of them announced itself:
 *  `enginesAndDate` returned nulls, so every record recorded `publishedAt: null` / `pinnedTo: null`
 *  and the date-aware Node pin was inert; `packageStanding` lost its metadata; and BOTH oracle arms
 *  failed to launch, which removes the only check that distinguishes a nub defect from a package
 *  that is simply broken. The record still looked well-formed.
 *
 *  Spelling the `.cmd` explicitly is preferred over `shell: true`: passing args with a shell is
 *  DEP0190 (args are concatenated rather than escaped), which is exactly why `toolchain()` below
 *  already avoids it. A package spec like `@scope/pkg@1.0.0` must not be re-parsed by cmd.exe. */
/*  Spelling the `.cmd` is NOT enough, measured: `spawnSync('npm.cmd', …)` returns **EINVAL** on
 *  win32. Node has refused to CreateProcess a batch file directly since the CVE-2024-27980 fix, so
 *  the only ways through are a shell (DEP0190 — args are concatenated rather than escaped, which
 *  would let cmd.exe re-parse a spec like `@scope/pkg@1.0.0`) or bypassing the shim entirely.
 *
 *  We bypass it: npm and npx ship as ordinary JS beside the node binary, so running
 *  `node <npm-cli.js> …` needs no shell and keeps the args array intact. MEASURED on nub-win:
 *  `npm.cmd` EINVAL, `shell:true` works but warns, `node npm-cli.js view @babel/core@7.28.0 …`
 *  returns status 0 and 10,229 bytes with the scoped spec unmangled. */
const WIN_SHIMS = new Set(['npm', 'npx', 'pnpm', 'yarn', 'node-gyp']);
const npmJs = (tool) => {
  const cli = path.join(path.dirname(process.execPath), 'node_modules', tool,
    'bin', `${tool}-cli.js`);
  return fs.existsSync(cli) ? cli : null;
};
/** Resolve a tool to an argv PREFIX: `[command, ...leadingArgs]`. Non-Windows and anything already
 *  a real executable pass straight through, so every caller is `const [c, ...pre] = tool(name)`. */
const toolArgv = (name) => {
  if (process.platform !== 'win32') return [name];
  if (name === 'npm' || name === 'npx') {
    const cli = npmJs(name);
    if (cli) return [process.execPath, cli];
  }
  // Anything else that is a .cmd shim (pnpm, yarn) has no bundled-JS equivalent to reach for. Leave
  // the bare name: it fails loudly with EINVAL rather than silently mismeasuring, and the oracle
  // already excludes an arm that could not run.
  return [name];
};

/** `engines.node` AND the publish date in ONE registry round-trip — both feed the Node choice, and
 *  fetching them separately doubled the per-package latency for no reason. */
function enginesAndDate(pkg, version) {
  try {
    const [c, ...pre] = toolArgv('npm');
    const r = spawnSync(c, [...pre, 'view', `${pkg}@${version}`, 'engines.node', 'time', '--json'],
      { encoding: 'utf8', timeout: 30_000 });
    if (r.status !== 0) return { engines: null, published: null };
    const j = JSON.parse(r.stdout || '{}');
    // ⛔ `npm view` COLLAPSES its output when only ONE of the requested fields exists. Ask for two
    // and a normal package returns `{"engines.node": ..., "time": {...}}` — but a package with NO
    // `engines` returns the TIME MAP ITSELF at top level, with no `time` key to reach for.
    // Handling only the wrapped shape yielded published=null for exactly the packages that need
    // the date MOST (the ones with no engines to fall back on), so the date-aware pin was inert
    // precisely where it mattered and they dropped to the Node 22 default. MEASURED on
    // better-sqlite3@8.7.0 and @rspack/core@0.0.26. Detect both shapes via the version under test.
    const engines = typeof j['engines.node'] === 'string' ? j['engines.node'] : null;
    const time = j.time && typeof j.time === 'object' ? j.time
      : (typeof j[version] === 'string' ? j : null);
    return { engines, published: time?.[version] ?? null };
  } catch { return { engines: null, published: null }; }
}

// ⛔ THE FLOOR IS NUB'S OWN SUPPORTED FLOOR (18), NOT THE OLDEST NODE INSTALLED.
//
// This was briefly set to the oldest installed major (4), on the reasoning that `nub install`
// completes there — which it does. But install completing is not a BUILD completing, and that
// reasoning was wrong for the case the corpus is full of:
//
//   nub injects its own node-gyp at a HARDCODED version, independent of the ambient Node
//   (aube .../install/node_gyp_bootstrap.rs: `const SPEC: &str = "^12.0.0"`, zero references to
//   the node version). node-gyp 12 declares `engines.node ^20.17.0 || >=22.9.0` and genuinely
//   needs >= 15 to parse. So under nub, ANY native addon fails on Node < 15 regardless of the
//   package's own age — proven with a minimal 4-line addon, not just with fsevents:
//       node 14.18.3   npm BUILT   nub FAIL (`replaceAll is not a function`)
//       node 16.13.2   npm BUILT   nub BUILT
//   npm never hits this because its bundled node-gyp is contemporaneous with its Node.
//
// Measuring below that produces verdicts about a configuration nub does not support, and it did:
// four packages were recorded as BROKEN-EVEN-WITH-EVERYTHING — the verdict reserved for a nub
// defect — purely for being pinned to Node 4, 8 or 10.
//
// 10, NOT 18. Raising it to nub's own support floor was tried and reverted: it "breaks a lot more
// shit" by measuring old packages on a Node they never targeted, and the failures that motivated it
// were nub injecting a node-gyp too new for the ambient Node -- fixed at the source in
// node_gyp_bootstrap::bucket_for, so the floor no longer has to absorb it.
const NODE_FLOOR = 10;
const NODE_DEFAULT = '22';      // when `engines.node` is absent or unparseable
const NODE_CEILING = 22;        // latest LTS — never measure on something newer than the ecosystem

/** The Node major that was CURRENT when a version was published — i.e. the one its author actually
 *  built and tested against. Active-LTS boundaries, from nodejs/Release. */
const LTS_AT = [
  ['2011-01-01', 10], ['2018-10-30', 10], ['2019-10-22', 12], ['2020-10-27', 14],
  ['2021-10-26', 16], ['2022-10-25', 18], ['2023-10-24', 20], ['2024-10-29', 22],
];
function ltsAt(published) {
  const t = published ? Date.parse(published) : NaN;
  if (!Number.isFinite(t)) return null;
  let major = LTS_AT[0][1];
  for (const [when, m] of LTS_AT) if (t >= Date.parse(when)) major = m;
  return major;
}

/** Which Node to measure a package-version on.
 *
 *  ⛔ `engines` ALONE IS THE WRONG SIGNAL, and it fails in BOTH directions — MEASURED:
 *
 *    too OLD:  @tailwindcss/oxide@4.1.14, published 2025-10, declares `>= 10` -> pinned Node 10.
 *              A 2025 Rust-toolchain package cannot run there and nobody has ever tested it there;
 *              the floor is stale boilerplate. electron@31.7.7 (2025-01, `>= 12.20.55`) the same.
 *    too NEW:  better-sqlite3@8.7.0 (2023-09) and @rspack/core@0.0.26 (2023-03) declare NO engines,
 *              so they fell to the Node 22 default and their C++ met a V8 that had removed the API
 *              they call — `no matching member function for call to 'SetAccessor'`. uuid@0.0.2 is
 *              from 2011 and got Node 22.
 *
 *  So START from the Node that was CURRENT WHEN THE VERSION WAS PUBLISHED, then raise it to
 *  whatever `engines` genuinely requires. A publish date cannot be stale boilerplate the way a
 *  floor can, and it is present for every version. `engines` keeps exactly the authority it
 *  deserves — a LOWER bound — and loses the authority it does not, which is to nominate an
 *  ancient runtime for a modern release.
 *
 *  Both inputs may be missing: no date falls back to `engines`, and neither falls back to the
 *  default. Everything is clamped to [NODE_FLOOR, NODE_CEILING]. */
function resolveNodeMajor(enginesNode, published) {
  let floor = null;
  if (enginesNode && typeof enginesNode === 'string') {
    // Smallest major any comparator mentions: `>=14 <21` floors at 14, `^18.0.0` at 18. Take the
    // MAJOR of each version token, never every digit preceding a dot — `^20.0.0` contains "0."
    // twice, and matching those once made its floor 0 instead of 20.
    const majors = [...enginesNode.matchAll(/(?:^|[\s|,>=<~^])v?(\d+)(?:\.\d+)*/g)]
      .map((m) => Number(m[1]))
      .filter((n) => Number.isFinite(n) && n >= 0);
    if (majors.length) floor = Math.min(...majors);
  }
  const dated = ltsAt(published);
  const pick = dated === null ? (floor ?? Number(NODE_DEFAULT)) : Math.max(dated, floor ?? 0);
  return String(Math.min(Math.max(pick, NODE_FLOOR), NODE_CEILING));
}

/** A Python that the node-gyp THIS Node will get can actually run, or null to leave the host's
 *  own resolution alone.
 *
 *  nub selects node-gyp by ambient Node major (`node_gyp_bootstrap::bucket_for`), so an old Node
 *  gets an old node-gyp — and the gyp vendored in node-gyp <=9 opens with
 *  `from distutils.version import StrictVersion`, a module REMOVED in Python 3.12. On a host whose
 *  `python3` is newer (this one is 3.14.6 via pyenv) every native compile on Node <=13 therefore
 *  dies in `gyp configure`, which the harness scored as BROKEN-EVEN-WITH-EVERYTHING — the verdict
 *  reserved for a nub defect — for what is purely a host mismatch.
 *
 *  MEASURED, real `node-gyp rebuild` of a minimal addon:
 *
 *      node-gyp 5 / 8 / 9   ->  Python <=3.11 only
 *      node-gyp 10 / 12     ->  any Python >=3.6, 3.12+ included
 *
 *  So this pins ONLY for the old-node case and only when a suitable interpreter exists. Modern Node
 *  keeps the deliberate no-pin behaviour, which is the developer-machine path. Returning null when
 *  nothing suitable is installed is correct rather than fatal: the run then fails the same way it
 *  does today, and provenance records the Python that was actually used. */
function gypPython(nodeBin) {
  // ⛔ MATCH BOTH LAYOUTS. This was `/\/v(\d+)\./`, which only ever matches nvm
  // (`~/.nvm/versions/node/v14.18.3/bin`). nub's own provisioning has NO `v` prefix
  // (`<cache>/nub/node/14.21.3/bin`), and Windows separates with `\` — so once `nodeBinFor` began
  // PREFERRING nub's nodes, this returned null for every one of them and the Python pin went
  // silently inert. That pin is what took `fsevents@1.2.12` from BROKEN to MINIMUM, so losing it
  // costs real records and nothing reports it. Read the major from the version DIRECTORY instead
  // of the whole path, which is layout- and separator-independent.
  const seg = nodeBin ? nodeBin.split(/[\\/]/).filter(Boolean).slice(-2, -1)[0] : null;
  const m = seg && /^v?(\d+)\./.exec(seg);
  // <=15, NOT <=13. bucket_for maps Node 14..17 to node-gyp 10, and node-gyp 10 DOES handle
  // Python 3.12+ on Node 16+ — but MEASURED on Node 14 it does not: cbor-extract@0.1.0 died at
  // `gyp ERR! configure error` with `node@14.18.3 | darwin | x64` and Python 3.14.6, while the
  // same package on the same Node built fine under Python 3.9 and 3.11. Node 16 is the first
  // major where gyp 10 and a modern Python agree, so the pin has to cover 14 and 15 too.
  if (!m || Number(m[1]) > 15) return null;
  for (const name of ['python3.11', 'python3.10', 'python3.9']) {
    // `command -v` is a POSIX shell builtin that cmd.exe does not have, so the Windows arm needs
    // `where` on the versioned .exe name uv/python.org actually install.
    const r = process.platform === 'win32'
      ? spawnSync('where', [`${name}.exe`], { encoding: 'utf8' })
      : spawnSync('command', ['-v', name], { shell: true, encoding: 'utf8' });
    const p = (r.stdout ?? '').trim().split(/\r?\n/)[0]?.trim();
    if (r.status === 0 && p) return p;
  }
  return null;
}

/** The PATH prefix that pins a probe to `major`, or null when no such Node is installed. nub
 *  resolves its Node from PATH (verified), so this is all the pinning that is needed — and the
 *  REFERENCE ARMS must use the same one, or the oracle compares two runtimes rather than two
 *  package managers. */
function nodeBinFor(major) {
  // ⛔ PREFER NUB'S OWN PROVISIONED NODES over nvm. `nub node install <major>` is the mechanism a
  // real user gets, so measuring against it is dogfooding rather than measuring a parallel
  // installation nobody else has — and it removes nvm as a prerequisite for running the corpus at
  // all, which is what made the first Linux run silently record `pinnedTo: null` on every package
  // (chosenMajor was correct, so nothing looked wrong; a 2018 package was measured on Node 22).
  //
  // Layouts differ: nub is `<cache>/nub/node/22.23.1/bin`, nvm is `~/.nvm/versions/node/v22.23.1/bin`.
  // nvm stays as a fallback so an existing dev box keeps working.
  const xdgCache = process.env.XDG_CACHE_HOME || path.join(os.homedir(), '.cache');
  const roots = [
    { dir: path.join(xdgCache, 'nub', 'node'), re: /^(\d+)\.(\d+)\.(\d+)$/ },
    { dir: path.join(os.homedir(), '.nvm', 'versions', 'node'), re: /^v(\d+)\.(\d+)\.(\d+)$/ },
  ];
  let best = null;
  for (const { dir: root, re } of roots) {
  try {
    for (const name of fs.readdirSync(root)) {
      const m = re.exec(name);
      if (!m || m[1] !== String(major)) continue;
      const v = [Number(m[2]), Number(m[3])];
      if (!best || v[0] > best.v[0] || (v[0] === best.v[0] && v[1] > best.v[1])) {
        best = { dir: path.join(root, name, 'bin'), v, name };
      }
    }
  } catch {}
    if (best) break;          // nub's copy wins outright; do not mix roots for one major
  }
  return best;
}

/** The arch of the PINNED node, asked of that binary rather than assumed from the host. They differ
 *  under Rosetta, which is precisely the case that produced a false defect (`workerd` hunting
 *  `@cloudflare/workerd-darwin-64` from an "arm64" Node 16 that was really x64). */
function pinnedNodeArch(nodeBin) {
  if (!nodeBin) return null;
  const exe = path.join(nodeBin, process.platform === 'win32' ? 'node.exe' : 'node');
  if (!fs.existsSync(exe)) return null;
  const r = spawnSync(exe, ['-p', 'process.arch'], { encoding: 'utf8', timeout: 30_000 });
  return r.status === 0 ? (r.stdout || '').trim() || null : null;
}

/** glibc vs musl — a `-musl` package cannot load on a glibc host and vice versa, and neither is a
 *  nub defect. `ldd --version` names itself on both; absent (macOS/Windows) the question is moot. */
function detectLibc() {
  if (process.platform !== 'linux') return null;
  const r = spawnSync('ldd', ['--version'], { encoding: 'utf8', timeout: 10_000 });
  const out = `${r.stdout ?? ''}${r.stderr ?? ''}`;
  if (/musl/i.test(out)) return 'musl';
  if (/GNU libc|GLIBC/i.test(out)) return 'glibc';
  return null;
}

function toolchain() {
  // `which`, not `command -v` through a shell: passing args with `shell: true` is deprecated
  // (DEP0190, args are concatenated rather than escaped) and would print a warning on every run.
  // `where` is the Windows equivalent and there is no `which`; it prints one match PER LINE, so the
  // first line is the resolved one. Without this the whole toolchain block reads null on Windows —
  // and this block is the provenance a reader uses to tell which python/node a build actually used.
  const which = (c) => {
    const r = process.platform === 'win32'
      // `where` looks up the SHIM on disk, so it wants the .cmd spelling — unlike spawn, which
      // cannot execute one. The two Windows spellings are genuinely different questions.
      ? spawnSync('where', [WIN_SHIMS.has(c) ? `${c}.cmd` : c], { encoding: 'utf8' })
      : spawnSync('which', [c], { encoding: 'utf8' });
    const out = (r.stdout || '').trim();
    return r.status === 0 && out ? out.split(/\r?\n/)[0].trim() || null : null;
  };
  const ver = (c, a) => {
    const [vc, ...vpre] = toolArgv(c);
    const r = spawnSync(vc, [...vpre, ...a], { encoding: 'utf8' });
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
function provenance(nub, nodePin, enginesNode, nodeMajor, publishedAt = null) {
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
    const here = HERE;
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
      // The date the Node choice was derived FROM. Without it a reader cannot tell whether a
      // record was pinned by `engines` or by publish date, and those disagree constantly.
      publishedAt: publishedAt ?? null,
      chosenMajor: nodeMajor ?? null,
      pinnedTo: nodePin?.name ?? null,
      hostNode: process.version,
      // ⛔ ARCH AND LIBC ARE PART OF THE RESULT, and a mismatch is never a nub defect (#96).
      //
      // A package can be architecturally incapable of installing here. MEASURED:
      // `@napi-rs/simple-git-linux-x64-musl@0.1.8` cannot load on a glibc box, and `workerd` looked
      // for `@cloudflare/workerd-darwin-64` because the "arm64" Node 16 on that Mac was an x64
      // build under Rosetta. Node publishes NO darwin-arm64 below v16.0.0, so on Apple Silicon
      // every major <=15 is unavoidably mismatched.
      //
      // The PINNED Node's arch is what the build actually saw; `process.arch` is the harness's own
      // and can differ from it, which is exactly the Rosetta case above.
      hostArch: process.arch,
      pinnedArch: pinnedNodeArch(nodePin?.dir ?? null),
      libc: detectLibc(),
    },
    // THE PYTHON IS PART OF THE RESULT TOO, for the same reason. `toolchain.python` reports what
    // the HOST resolves; when this is non-null the build actually ran against something else, and
    // a record showing only the host value would misstate its own conditions.
    pythonPinned: gypPython(nodePin?.dir ?? null),
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
    // ⛔ A STORE ENTRY WITHOUT `@<version>` IS NOT A PACKAGE. `.store` also holds bookkeeping
    // directories — `.nub-state` and a nested `node_modules` — and the old `at > 0 ? … : e`
    // fallback turned each into a package NAME, which then entered the background grant set.
    // Inert while nothing validated that set; NOT inert now that the grant is emitted as
    // `packageNetwork.full`, where each becomes a catalog entry for a package that does not exist.
    // MEASURED in a real cell: `{"package":".nub-state"}` and `{"package":"node_modules"}` sat
    // alongside the 42 genuine names.
    //
    // `lastIndexOf` is what keeps a scope safe: `@apollo+rover@0.29.1` finds the VERSION `@`, and
    // `at > 0` rejects the scope's own leading one.
    if (at <= 0) continue;
    const name = e.slice(0, at).replace('+', '/');
    names.add(name);
    // Strip the store's content hash: `pkg@1.2.3-93d5bfce` -> `1.2.3`.
    specs.add(`${name}@${e.slice(at + 1).replace(/-[0-9a-f]{8,}$/, '')}`);
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
  // ⛔ `packages.X` IS AN OBJECT (`{ default: grant }`), NOT AN ARRAY. This read `g[0]` — left over
  // from the retired `[grant]` array shape — so it evaluated to `undefined` for EVERY non-empty
  // state and the field never reached the record.
  //
  // MEASURED on a real record (`@apollo/rover@0.29.1`): no `grant` key at all, only
  // `"state":"network","cost":3`. That is the failure this function's own doc warns about — a
  // record carrying only a HUMAN LABEL — and it reaches the deliverable: `collate.mjs` keys
  // `grantKey`/`capsKey` on `r.grant` and widens `unionGrant` off `a.write`/`a.read`/`a.network`,
  // so every package collapsed into one `null` band, the cross-platform union became a no-op, and
  // the generated catalog carried no capabilities at all.
  return g?.default ?? null;
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
  // ⛔ CHUNK THE BATCH. `querybatch` caps a request at ~1000 queries, so a single POST for a large
  // tree comes back short and the length check below (correctly) refuses — MEASURED on
  // `netlify-cli@3.9.0`: `OSV screen returned 0 results for 1284 queries`, which failed closed into
  // a HARNESS-CRASH, so the biggest dependency trees in the corpus were exactly the ones that could
  // never be screened OR measured. Failing closed was right; being unable to screen at all was not.
  //
  // The body also goes over STDIN rather than argv: a 1284-entry JSON body is hundreds of KB and
  // argv has its own ceiling, which would have been the next failure after chunking.
  const CHUNK = 500;
  const results = [];
  for (let i = 0; i < queries.length; i += CHUNK) {
    const slice = queries.slice(i, i + CHUNK);
    const r = spawnSync('curl', ['-sS', '--max-time', '60', '-X', 'POST',
      'https://api.osv.dev/v1/querybatch', '-H', 'Content-Type: application/json',
      '--data-binary', '@-'],
      { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024, input: JSON.stringify({ queries: slice }) });
    if (r.status !== 0) {
      throw new Error(`OSV screen FAILED (curl ${r.status}) on chunk ${i / CHUNK}: ${(r.stderr || '').slice(0, 200)}`);
    }
    let parsed;
    try { parsed = JSON.parse(r.stdout); } catch (e) { throw new Error(`OSV screen FAILED to parse: ${e.message}`); }
    const got = parsed.results ?? [];
    // Per-chunk length check, so a short chunk cannot be masked by a full one.
    if (got.length !== slice.length) {
      throw new Error(`OSV screen returned ${got.length} results for ${slice.length} queries `
        + `(chunk ${i / CHUNK} of ${Math.ceil(queries.length / CHUNK)})`);
    }
    results.push(...got);
  }
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
  const [npmC, ...npmPre] = toolArgv('npm');
  const time = j(npmC, [...npmPre, 'view', `${pkg}@${version}`, 'time', '--json']);
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
  const refEnv = { ...process.env, ...homeEnv(fx.home) };
  if (nodeBin) refEnv.PATH = `${nodeBin}:${refEnv.PATH ?? ''}`;
  // The SAME Python pin the nub arm gets — npm and pnpm bundle their own node-gyp, but on an old
  // Node theirs is old too, so they hit the identical distutils wall. Pinning one arm and not the
  // other would make the oracle a comparison of interpreters rather than of package managers.
  const refPy = gypPython(nodeBin);
  if (refPy) refEnv.npm_config_python = refPy;
  // Same interpreter pin the nub arm gets. npm and pnpm follow `npm_node_execpath` for node-gyp's
  // header derivation, so without it the oracle builds against a different Node than nub did and
  // the comparison is between two runtimes rather than two package managers.
  if (nodeBin) {
    const refExe = path.join(nodeBin, process.platform === 'win32' ? 'node.exe' : 'node');
    if (fs.existsSync(refExe)) refEnv.npm_node_execpath = refExe;
  }
  const [toolC, ...toolPre] = toolArgv(tool);
  const r = spawnSync(toolC, [...toolPre, ...args], {
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
    if (!keep) rmTree(dir);
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
  const { engines: enginesNode, published: publishedAt } = enginesAndDate(pkg, version);
  const nodeMajor = resolveNodeMajor(enginesNode, publishedAt);
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
  let control = cell('control', { catalogFile: write(STATES[STATES.length - 1]), label: 'control (tested pkg: everything)' });
  // RUN THE CONTROL TWICE. A single control cannot tell "this cell lacks a capability" from
  // "this package does not write the same paths twice". `unrs-resolver@1.12.2` failed all 55
  // states on three consecutive runs for the second reason — one npm debug log whose FILENAME
  // carries a timestamp — and read as a package no grant could fix.
  //
  // The two runs are combined by UNION, never intersection. Intersecting compares on fewer
  // paths, so a cell that failed to write an unstable path still passes and the search records
  // too NARROW a minimum: the exact failure this effort exists to avoid. The union is what
  // `writePaths` is derived from, so a directory seen in only one run is still promoted.
  let controlB = cell('controlB', { catalogFile: write(STATES[STATES.length - 1]), label: 'control (repeat)' });
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
      cells, control: baseCase(control), provenance: provenance(nub, nodePin, enginesNode, nodeMajor, publishedAt),
    };
  }
  // ⛔ A MALICIOUS-PACKAGE REFUSAL IS THE SCREEN WORKING, NOT A DEFECT AND NOT A GRANT GAP.
  //
  // The OSV screen refuses to install anything carrying a `MAL-*` advisory, whole-tree and
  // fail-closed. That refusal fails the control, and the control-failure path below would then
  // hand it to the oracle and score it BROKEN-EVEN-WITH-EVERYTHING — the verdict reserved for a
  // nub defect. MEASURED on `fsevents@1.2.4`, which is refused and was recorded as a defect.
  //
  // It is its own outcome: no grant is applicable (the package never installs by design), it must
  // never enter the catalog, and it must never be counted as a failure of the jail.
  const refusal = /ERR_NUB_MALICIOUS_PACKAGE|refusing to install malicious package/;
  if (refusal.test(control.log ?? '') || refusal.test(controlB.log ?? '')) {
    return {
      pkg, version, verdict: 'REFUSED-MALICIOUS',
      // ⛔ SAY *WHAT* WAS FLAGGED. The screen is WHOLE-TREE, so the advisory usually names a
      // TRANSITIVE DEPENDENCY, not the package under test — this text used to read "refused this
      // package", which is wrong in the common case and unauditable in every case.
      //
      // MEASURED: `@azure-devops/mcp@0.1.0` was refused, and querying OSV for that exact
      // package@version returns NO vulns at all. The advisory (`MAL-2025-32741`) is on something in
      // its tree. A security verdict that cannot name what it caught cannot be acted on, verified,
      // or re-checked when the advisory changes.
      why: 'the OSV screen refused this INSTALL because a MAL-* advisory covers something in its '
         + 'dependency tree — often NOT the package under test. Working as designed: no grant '
         + 'applies, and it must not be recorded as a nub defect or enter the catalog. See '
         + '`maliciousAdvisories` for what was actually flagged.',
      // Scraped from the cell log because that is where nub reports it; the harness's own screen
      // runs earlier and its `flagged` list is not in scope here. Recording BOTH the advisory ids
      // and any named spec keeps the record self-contained.
      maliciousAdvisories: (() => {
        const text = `${control.log ?? ''}\n${controlB.log ?? ''}`;
        const ids = [...new Set(text.match(/MAL-\d{4}-\d+/g) ?? [])];
        const specs = [...new Set(text.match(/malicious package\s+(\S+)/g) ?? [])]
          .map((s) => s.replace(/^malicious package\s+/, ''));
        return { ids, specs, note: ids.length ? 'ids scraped from the cell log' : 'NONE FOUND — the refusal fired but named no advisory, which is itself worth investigating' };
      })(),
      cells, control: baseCase(control),
      provenance: provenance(nub, nodePin, enginesNode, nodeMajor, publishedAt),
    };
  }
  // ⛔ CONTROL-ONLY MODE — what the FIXTURE CANARY actually needs, and only that.
  //
  // The canary asks one question: does the fixture still install a REAL TREE? That is answered
  // entirely by the control's `fileCount` and `materialized`. It was answering it by running the
  // FULL 54-state walk, which is ~50x the work.
  //
  // MEASURED on a windows-latest runner: the canary ran 21:06:58 -> 21:21:59 and died on its own
  // 900s cap, having proved nothing — and because `timeout` killed it, the stderr it would have
  // written was EMPTY, so the refusal had no reason attached either. Every Windows shard was
  // blocked behind a check that could not finish.
  //
  // Returns the same record shape so the canary's `control.fileCount` / `control.materialized`
  // reads are unchanged, and the verdict names itself so this can never be mistaken for a
  // measurement or land in the catalog.
  if (argv.includes('--control-only')) {
    return {
      pkg, version,
      verdict: 'CONTROL-ONLY',
      why: 'ran the widest-grant control and stopped — fixture health check, not a measurement.',
      cells, control: baseCase(control),
      provenance: provenance(nub, nodePin, enginesNode, nodeMajor, publishedAt),
    };
  }

  // ⛔ CONFIRM A CONTROL FAILURE BEFORE IT CAN BECOME AN ACCUSATION.
  //
  // A failing control is the gateway to BROKEN-EVEN-WITH-EVERYTHING, the verdict that says "nub has
  // a defect" — the most consequential thing this harness emits. It must not rest on one
  // observation, because a lifecycle script that runs its OWN installer competes with every other
  // package being measured concurrently, and loses.
  //
  // MEASURED on this corpus, running 3 shards: six packages were recorded
  // BROKEN-EVEN-WITH-EVERYTHING and ALL SIX passed when re-run alone —
  // `classify-broken.sh` gave `off=0 on=0 npm=0` for docxtemplater@0.3.0, docxtemplater@0.5.2 and
  // react-native-purchases@0.5.1. Their logs show `npm ERR! Callback called more than once` and
  // `npm ERR! rimraf: missing path`, i.e. an old npm losing a race, not nub failing. At 1.6% of
  // records that would have been ~36 false accusations across the full 2,250.
  //
  // So: re-run BOTH controls once. If the retry succeeds, the first result was contention and the
  // retry is authoritative. This is NOT the banned retry-wrapper — that ban is about masking flakes
  // in OUR tests. Here we measure third-party installs that genuinely fail transiently on the
  // network, and the point is to avoid writing a transient down as a permanent fact about nub.
  if (control.rc !== 0 || controlB.rc !== 0) {
    const retryA = cell('control-retry', {
      catalogFile: write(STATES[STATES.length - 1]), label: 'control (confirm)',
    });
    const retryB = cell('controlB-retry', {
      catalogFile: write(STATES[STATES.length - 1]), label: 'control (confirm repeat)',
    });
    if (retryA.rc === 0 && retryB.rc === 0) {
      control = retryA;
      controlB = retryB;
      cells.push(
        { index: null, state: 'CONTROL (confirm — first attempt failed under load)', cost: null,
          pass: true, rc: retryA.rc, digest: retryA.digest, files: retryA.files,
          overrideEngaged: null, materialized: retryA.materialized },
      );
    }
  }
  // ⛔ npm 6 RACES ON ITS OWN _cacache, AND THE JAIL GETS THE BLAME. An old package pins an old
  // Node, an old Node bundles npm 6, and npm 6 corrupts its own cache under concurrency — surfacing
  // as `rimraf: missing path` and `Callback called more than once`. It has nothing to do with
  // confinement, but it lands on the CONTROL, and a failing control is what every defect verdict is
  // built on.
  //
  // The double control and the confirmation retry CANNOT catch it: both attempts run inside the same
  // busy window, so they agree with each other and are both wrong. That is what makes this worth a
  // signature check rather than more retries.
  //
  // MEASURED: re-measuring 19 macOS records produced 7 `BROKEN-EVEN-WITH-EVERYTHING` verdicts — the
  // verdict that ACCUSES THE JAIL — and 7 of 7 carried this signature, every one an ancient version
  // (`victory-bar@0.1.0`, `docxtemplater@0.3.0`, `console-stream@0.1.0`). I read the first two as
  // real jail defects before reading the per-cell log. They were not.
  //
  // Recorded as HARNESS-FLAKE rather than retried in-loop: the cure is a SERIAL re-run on a quiet
  // box, which `run-batch.sh` already does once the batch drains. Retrying here would spend two more
  // installs inside the same contended window that caused it.
  const NPM6_RACE = /rimraf: missing path|Callback called more than once/;
  if ((control.rc !== 0 || controlB.rc !== 0)
      && (NPM6_RACE.test(control.log ?? '') || NPM6_RACE.test(controlB.log ?? ''))) {
    return {
      pkg, version,
      verdict: 'HARNESS-FLAKE',
      why: "the control failed with npm 6's _cacache race (`rimraf: missing path` / `Callback "
         + 'called more than once`) — contention on this host, not a statement about the jail. '
         + 'Re-run SERIALLY on a quiet box for a real measurement.',
      declaresInstallScript: hasScript,
      cells, control: baseCase(control),
      provenance: provenance(nub, nodePin, enginesNode, nodeMajor, publishedAt),
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
    // ⛔ ASK NUB-WITHOUT-THE-JAIL **FIRST**, AND SHORT-CIRCUIT. If the package fails identically
    // with confinement off, the verdict is BROKEN-WITHOUT-JAIL-TOO no matter what npm and pnpm say
    // — the oracles compare nub against other PMs, which cannot separate "nub's PM is wrong" from
    // "nub's jail is wrong", and this cell can. Running them anyway costs two full installs to
    // learn nothing that changes the answer.
    //
    // MEASURED: this is not a rare path. 35 of 35 re-measured defect verdicts across Linux and
    // macOS came back BROKEN-WITHOUT-JAIL-TOO, and on Windows `pre-push@0.1.4` +
    // `spawn-sync@1.0.15` fail with 2 lifecycle failures BOTH jailed and unjailed. Old packages
    // that simply cannot run on the measuring host are the DOMINANT failure mode, and each one was
    // paying for two oracle installs before reaching a verdict that was already determined.
    //
    // Ordering note: this sits AFTER the confirmation retry deliberately, so a control that only
    // failed under load is re-tried before we conclude anything at all.
    const jailOffFirst = cell('jail-off-control', {
      catalogFile: write(STATES[STATES.length - 1]), label: 'control (jail OFF)', jailOff: true,
    });

    // ⛔ MAKE THE CELL PROVE IT RAN UNJAILED. Do not take the flag's word for it.
    //
    // This verdict's whole meaning is "the jail is not implicated", and it is inferred from a cell
    // AGREEING with the control. An off-switch that silently stops working therefore does not
    // produce an error — it produces unanimous agreement, which reads as a confident exoneration.
    // That is exactly what happened: the fixture set `dependenciesMeta.<pkg>.sandbox: false` for
    // months after c5651408f4 deleted every path that read it, so every "jail off" cell ran jailed
    // and every failing package was filed as NOT-THE-JAIL. Re-measuring the affected macOS records
    // with a working switch flipped 2 of the first 5 to BROKEN-EVEN-WITH-EVERYTHING — real jail
    // defects the broken control had buried.
    //
    // nub announces an unconfined dependency script once per package, so the announcement is
    // POSITIVE EVIDENCE the jail was actually off. Its absence means the switch did not engage, and
    // the honest answer is then HARNESS-ERROR — an instrument failure, never a measurement, and
    // never a verdict about the jail.
    //
    // Deliberately NOT a warning: a warning here would scroll past in a 2,250-package sweep, which
    // is precisely how the original went unnoticed. It must stop the record from being written.
    if (!/without the build sandbox/.test(jailOffFirst.log ?? '')) {
      return {
        pkg, version,
        verdict: 'HARNESS-ERROR',
        why: 'the jail-off control could not be shown to have run UNJAILED: nub never announced '
           + '"build scripts are running without the build sandbox". The off-switch did not engage, '
           + 'so this cell is a duplicate of the control and cannot exonerate (or implicate) the '
           + 'jail. Check that the fixture writes nub.jsonc {"install":{"buildJail":false}} and that '
           + 'the binary honours it.',
        jailOffControl: { rc: jailOffFirst.rc, digest: jailOffFirst.digest, files: jailOffFirst.files },
        cells, control: baseCase(control),
        provenance: provenance(nub, nodePin, enginesNode, nodeMajor, publishedAt),
      };
    }

    if (jailOffFirst.rc !== 0) {
      return {
        pkg, version,
        verdict: 'BROKEN-WITHOUT-JAIL-TOO',
        why: 'fails IDENTICALLY with the jail off, so confinement is not implicated. A nub PM/linker '
           + 'or packaging problem, or a package that cannot run on this host at all. Oracles were '
           + 'SKIPPED: they could not change this verdict.',
        declaresInstallScript: hasScript,
        jailOffControl: {
          rc: jailOffFirst.rc, digest: jailOffFirst.digest, files: jailOffFirst.files,
          note: 'ran BEFORE the oracles and short-circuited them',
        },
        cells, control: baseCase(control),
        provenance: provenance(nub, nodePin, enginesNode, nodeMajor, publishedAt),
      };
    }

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

    // ⛔ ASK NUB-WITHOUT-THE-JAIL BEFORE BLAMING THE JAIL. `BROKEN-EVEN-WITH-EVERYTHING` reads as
    // "the jail broke this", and it is the verdict that decides whether the build jail is
    // releasable — so it must not absorb defects that have nothing to do with confinement.
    //
    // MEASURED on `@pulumi/kubernetes@0.21.1`: fails `sh: 1: node-pre-gyp: not found` in the
    // transitive `grpc@1.21.1`, IDENTICALLY with the jail off (rc=1 both ways), while plain npm
    // succeeds rc=0. nub never creates grpc's `.bin/node-pre-gyp` shim — a PM/linker defect that
    // no grant can fix and that the jail is innocent of. Three `@pulumi/*` records carried this
    // same signature and all read as jail failures.
    //
    // The jail-off control already ran ABOVE and PASSED — anything reaching here installs fine
    // unjailed, so the jail IS implicated and the oracles decide between "nub alone is wrong"
    // (a defect) and "everyone fails here" (the environment). Re-running the cell would be a
    // second identical install for an answer already in hand.
    return {
      pkg, version,
      verdict: matched ? 'BROKEN-IN-ENVIRONMENT' : 'BROKEN-EVEN-WITH-EVERYTHING',
      jailOffControl: {
        rc: jailOffFirst.rc, digest: jailOffFirst.digest, files: jailOffFirst.files,
        note: 'succeeds with the jail off, so the jail IS implicated (checked before the oracles)',
      },
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
      provenance: provenance(nub, nodePin, enginesNode, nodeMajor, publishedAt),
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
        provenance: provenance(nub, nodePin, enginesNode, nodeMajor, publishedAt),
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
        // ⛔ A TRUNCATED EVIDENCE ARRAY IS WORSE THAN NONE. This was `.slice(0, 40)` beside a
        // count that read 529 — so every prefix conclusion was drawn from a 7.5% sample, and one
        // was drawn WRONG: all 40 visible paths were `$home/.cache/node-gyp`, which made a
        // baseline write entry look obvious. Adding it changed the grant NOT AT ALL.
        //
        // The question a reader actually asks is "which SCOPE is missing", and that is answered by
        // a histogram over the FULL set, not by the first N paths. Keep a bigger raw sample too,
        // but the histogram is the thing to reason from.
        pathsBlockedWithoutGrant: floor ? controlOnly(controlU, floor).slice(0, 200) : null,
        pathsBlockedByPrefix: floor ? (() => {
          const all = controlOnly(controlU, floor);
          const h = {};
          for (const raw of all) {
            // Bucket to the first three segments: enough to name the scope
            // (`$home/.cache/node-gyp`, `$proj/node_modules/.store`) without exploding per-file.
            h[raw.split('/').slice(0, 3).join('/')] = (h[raw.split('/').slice(0, 3).join('/')] ?? 0) + 1;
          }
          return Object.fromEntries(Object.entries(h).sort((a, b) => b[1] - a[1]));
        })() : null,
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
    // ⛔ KEEP THE EXTENSION. Windows decides executability from the SUFFIX, so a content-addressed
    // copy named by bare hash cannot be spawned at all — `ENOENT` even though the file is there,
    // 1.17 GB and mode 0755. MEASURED on nub-win: the pinned copy at
    // `…\probe-bin\4888d4dfd3e599ae` was ENOENT while the same bytes at `…\debug\nub.exe` ran.
    // Everything downstream (the sha cache key, provenance) is unchanged — only the name gains
    // `.exe`, and on POSIX `path.extname` is empty so the name is byte-identical to before.
    const pinned = path.join(dir, sha + path.extname(p));
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

/** ⛔ A GIT-BASH PATH IS NOT SPAWNABLE BY WINDOWS NODE. `run-batch.sh` resolves the binary with
 *  `cd $(dirname) && pwd`, which under Git Bash yields `/c/Users/…` — and `spawnSync` on win32
 *  hands that straight to CreateProcess, which fails **ENOENT**. MEASURED, varying only the path
 *  spelling: `/c/Users/nub.NUB-WIN/…/nub.exe` → ENOENT, `C:\Users\nub.NUB-WIN\…\nub.exe` →
 *  status 0, `v0.6.0`. */
const fromGitBash = (p) => {
  const m = /^\/([A-Za-z])\/(.*)$/.exec(p ?? '');
  return m ? `${m[1].toUpperCase()}:\\${m[2].split('/').join('\\')}` : p;
};

const nubArg = argv.includes('--nub') ? argv[argv.indexOf('--nub') + 1] : 'nub';
const nub = pinBinary(process.platform === 'win32' ? fromGitBash(nubArg) : nubArg);

// ⛔ PROVE THE BINARY LAUNCHES BEFORE MEASURING ANYTHING WITH IT.
//
// Without this the failure surfaced as `HARNESS-ERROR: the catalog override did not engage — is the
// binary built with --features nub-cli/build-jail-catalog-override?`. That message sent me to
// inspect cargo features on a binary whose features were CORRECT; the real fault was that nub never
// ran at all, and every per-cell log was 0 bytes. A diagnostic that names the wrong subsystem is
// worse than none, because it is followed.
{
  const probe = spawnSync(nub, ['--version'], { encoding: 'utf8', timeout: 60_000 });
  if (probe.error || probe.status !== 0) {
    console.error(`REFUSING TO RUN: cannot execute the nub binary at ${nub}`);
    console.error(`  ${probe.error ? `${probe.error.code}: ${probe.error.message}` : `exit ${probe.status}`}`);
    if (process.platform === 'win32' && /^\/[A-Za-z]\//.test(nubArg)) {
      console.error('  It looks like a Git Bash path. Windows Node spawns native paths only —');
      console.error(`  pass ${fromGitBash(nubArg)} instead.`);
    }
    process.exit(2);
  }
}

// ⛔ THE FIXTURE NEEDS `git`, AND NOTHING SAYS SO UNTIL IT CRASHES. `makeFixture` builds a REAL
// repository (see the comment there — husky and simple-git-hooks check HEAD, identity and
// remote.origin.url, so a bare `git init` is not enough). That makes git a hard dependency of every
// single measurement, declared nowhere.
//
// MEASURED on a fresh Windows box: all 12 packages of a shard "completed" in 1-5 seconds each with
// no verdict, because every one died at `spawnSync git ENOENT` inside makeFixture. A CI runner ships
// git so this never fires there, which is exactly why it stayed hidden — the failure only appears on
// the clean machines that are hardest to debug on, and it presents as twelve mysterious fast
// no-verdicts rather than as one missing tool.
{
  const probe = spawnSync('git', ['--version'], { encoding: 'utf8', timeout: 30_000 });
  if (probe.error || probe.status !== 0) {
    console.error('REFUSING TO RUN: `git` is not on PATH, and every fixture needs it.');
    console.error('  The fixture is a real repository — lifecycle scripts from husky and');
    console.error('  simple-git-hooks bail silently without HEAD, an identity, and a remote.');
    console.error(process.platform === 'win32'
      ? '  On Windows, MinGit is a plain zip and needs no installer or elevation:\n'
        + '  https://github.com/git-for-windows/git/releases (MinGit-*-64-bit.zip)'
      : '  Install git (e.g. `apt-get install -y git`) and re-run.');
    process.exit(2);
  }
}
const keep = argv.includes('--keep');

// Never /tmp: a /tmp/package.json on this box is found by walking up.
const root = fs.mkdtempSync(path.join(os.homedir(), '.cache', 'nub-search-'));
try {
  const runDir = argv.includes('--runs')
    ? argv[argv.indexOf('--runs') + 1]
    : path.join(HERE, 'results', 'runs');
  const out = runPath(runDir, pkg, version);
  fs.mkdirSync(path.dirname(out), { recursive: true });

  // RESUMABLE BY DEFAULT. A batch re-run should cost only what it has not already measured;
  // `--force` re-measures one package after a harness or jail change.
  //
  // ⛔ A `HARNESS-*` RECORD IS NOT A MEASUREMENT, so it must not satisfy the resume check. This was
  // `fs.existsSync(out)` alone, which counted a crash or a timeout as "done" — so the packages that
  // BROKE the harness were exactly the ones a later fix could never reach, and a resumed sweep
  // skipped straight past them looking complete. MEASURED: `netlify-cli@3.9.0` recorded
  // HARNESS-CRASH from the unchunked OSV screen; with the screen fixed, a plain resume would still
  // never re-run it.
  //
  // A `BROKEN-*` verdict is the OPPOSITE case and DOES count: "this package genuinely fails here" is
  // real data about the package, not an instrument failure.
  let priorRecord = null;
  if (fs.existsSync(out)) {
    try { priorRecord = JSON.parse(fs.readFileSync(out, 'utf8')); } catch { priorRecord = null; }
  }
  const priorIsMeasurement = priorRecord && !String(priorRecord.verdict ?? '').startsWith('HARNESS-');
  if (!argv.includes('--force') && priorIsMeasurement) {
    console.log(JSON.stringify({ ...priorRecord, skipped: 'already measured; pass --force to redo' }));
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
  if (!keep) rmTree(root);
}

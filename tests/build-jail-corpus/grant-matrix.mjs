#!/usr/bin/env node
// grant-matrix.mjs — determine EMPIRICALLY what each package's install script needs,
// by running it under a ladder of catalog configurations and reading which cells pass.
//
// WHY THIS EXISTS. Every grant in the build-jail catalog was inferred from a single
// failure line and confirmed only in aggregate, by the corpus break count falling. That
// shows a grant is SUFFICIENT. It never showed one was NECESSARY, and it never showed it
// was the SMALLEST that works — so a package can hold a capability it does not need,
// which is the direction that quietly widens the jail. Running the same script under a
// ladder and reading the pass/fail PATTERN answers both at once:
//
//   c1  jail off          the control. Fails => inadmissible; the row tells us nothing.
//   c2  jail on, ungranted    passes => needs NOTHING. Strip whatever it holds.
//   c3  jail on, + project    first pass here => needs project read/write/cwd
//   c4  jail on, + network    first pass here => needs egress
//   c5  jail on, + both       only pass here => needs both
//   c6  jail on, + full disk  only pass here => needs the whole filesystem. Grant it.
//       fails at c6           => not a filesystem need at all. RECORD AND MOVE ON.
//
// c6 IS WHY THIS LADDER TERMINATES. Before it, a package failing at c5 was an open
// question — some capability the catalog had no vocabulary for — and every such row was a
// candidate investigation. `fullDisk` is the catalog's terminal tier, so the row becomes a
// one-line entry instead, and coverage stops forking into root-cause work. What still
// reaches the bottom is a genuinely different animal: a failure no filesystem grant can
// fix (a COM server, a host daemon, an ABI mismatch), which is worth writing down.
//
// THROUGHPUT IS THE POINT, NOT FORENSICS. ~246 of the most popular install-scripted
// packages have never run under the jail at all; that gap, not the handful of understood
// breaks, is what stops the jail shipping on by default. So the primary signal is the
// lifecycle script's EXIT CODE, which costs nothing, and a package that fails every cell
// is written down rather than investigated. A wrong verdict on one package is
// recoverable; not covering two hundred is not.
//
// ORDER IS AN OPTIMISATION, NOT A SEMANTIC. Cells run c1, c2, c5, c4, c3 with an early
// exit: c5 failing settles the row in four runs instead of five, and egress is by far the
// commonest need. The requirement is named from the resulting pattern, which does not
// depend on the order the cells were taken in.
//
// ── the three checks that are kept, because each has silently invalidated a whole run ──
//
// 1. THE `catalog OVERRIDDEN` BANNER. The override is forwarded through `env -i`; an
//    earlier harness dropped it and every arm silently scored the COMPILED catalog, so
//    "the grant changed nothing" was an artefact. The banner is the only proof the binary
//    read the file on disk, and this script also compares the counts in it against counts
//    it derives itself — two counts of one set, from two programs.
// 2. THE WINDOW ACTUALLY RAN. `approve-builds` WRITES the approval into package.json, so
//    a fixture reused from a previous cell has the script already approved and runs it
//    during `install` instead — leaving an empty window that exits 0 and reads as a pass.
//    (Hit while building this script.) Every cell gets a freshly generated fixture, and a
//    window that reports nothing to approve is a harness failure, never a verdict.
// 3. A NONCE, AND AN ARTIFACT DELTA. The nonce is the fixture path component, so every
//    absolute path the script prints carries it and captured output cannot be a stale
//    log. The artifact delta is the fallback signal for the one case an exit code cannot
//    decide: a script whose inputs are missing bails early and exits 0. `tree-sitter-cli`
//    once wrote a 0-byte file at exactly the right path, so a presence check passed and
//    only the SIZE falsified it — hence bytes, and hence the ratio against the unconfined
//    arm rather than an absolute floor.
//
// USAGE
//   node grant-matrix.mjs --list packages.tsv [--out DIR] [--limit N] [--jobs N]
//   node grant-matrix.mjs --minimality [--out DIR] [--jobs N]
//
//   --list        TSV: name <TAB> version  (blank version = resolve `latest`)
//   --minimality  instead of the ladder, run cell c2 ONLY for every package the catalog
//                 already grants. A pass there means the shipped grant is redundant.
//   --out         run root (default ~/.cache/nub/grant-matrix). NEVER /tmp: macOS clears
//                 it on reboot, which once destroyed a multi-hour run's inputs mid-resume.
//
// Env: NUB_BIN (required), STUDY_PATH, BASE_CATALOG.
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import crypto from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const WINDOWS = process.platform === 'win32';

// ── args ──────────────────────────────────────────────────────────────────────
const argv = process.argv.slice(2);
const arg = (n, d) => { const i = argv.indexOf(n); return i >= 0 ? argv[i + 1] : d; };
const has = (n) => argv.includes(n);

const NUB = process.env.NUB_BIN;
if (!NUB || !fs.existsSync(NUB)) { console.error('set NUB_BIN to a nub built with --features build-jail-catalog-override'); process.exit(2); }
// `new URL(...).pathname` yields `/C:/…` on Windows, which every `path.join` below then
// resolves to a directory that does not exist — so `BASE_CATALOG` silently pointed at
// nothing and the run died on the first cell.
const HARNESS = path.dirname(fileURLToPath(import.meta.url));
const BASE_CATALOG = process.env.BASE_CATALOG || path.join(HARNESS, '../../crates/nub-sandbox/data/build-jail-catalog.json');
// The default is the system tool floor a lifecycle script may reach, plus the node that
// runs it — appended below. Windows has no `/usr/bin`, and a PATH of POSIX directories
// there resolves nothing at all.
const STUDY_PATH_DEFAULT = WINDOWS
  ? [
      `${process.env.SystemRoot || 'C:\\Windows'}\\system32`,
      process.env.SystemRoot || 'C:\\Windows',
      `${process.env.SystemRoot || 'C:\\Windows'}\\System32\\Wbem`,
    ].join(path.delimiter)
  : '/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin';
// A lifecycle script's `node` must be reachable or every cell fails for a reason that has
// nothing to do with the jail. `run-shard.sh` prepends the caller's node dir for exactly
// this. WINDOWS ONLY, deliberately: the POSIX floor already contains a `node` and 643
// packages have been measured against it across Linux and macOS, so prepending there would
// change which interpreter those runs used and break comparability with them for no gain.
const STUDY_PATH = (() => {
  const base = process.env.STUDY_PATH || STUDY_PATH_DEFAULT;
  if (!WINDOWS) return base;
  const nodeDir = path.dirname(process.execPath);
  const present = base
    .split(path.delimiter)
    .some((d) => d && path.resolve(d).toLowerCase() === nodeDir.toLowerCase());
  return present ? base : [nodeDir, base].join(path.delimiter);
})();
const ROOT = arg('--out', path.join(os.homedir(), '.cache/nub/grant-matrix'));
const JOBS = Number(arg('--jobs', '3'));
const LIMIT = Number(arg('--limit', '0'));
const MINIMALITY = has('--minimality');

fs.mkdirSync(path.join(ROOT, 'logs'), { recursive: true });
fs.mkdirSync(path.join(ROOT, 'catalogs'), { recursive: true });
fs.mkdirSync(path.join(ROOT, 'fx'), { recursive: true });
// ONE store shared by every cell and every package. The store is a CAS and this is what
// production does, so sharing is faithful as well as an order-of-magnitude saving on
// fetch. Replay of a previous cell's RESULT is prevented by `side-effects-cache=false`
// in each fixture, which is a stronger guarantee than a private store would be.
const STORE = path.join(ROOT, 'store');
fs.mkdirSync(STORE, { recursive: true });

const RESULTS = path.join(ROOT, MINIMALITY ? 'minimality.ndjson' : 'matrix.ndjson');

// ── the catalog cells ─────────────────────────────────────────────────────────
// Egress is granted from TWO places — `packageNetwork.full` and any
// `networkHosts[].fetchedBy` naming the package; catalog.rs unions them. A cell that
// strips only the first leaves the package granted and reports "passes without its
// grant", i.e. a redundant grant that is not redundant. This inverts the minimality
// answer, so both are cleared and the result is asserted below.
const baseCatalog = JSON.parse(fs.readFileSync(BASE_CATALOG, 'utf8'));

function cellCatalog(pkg, cell, version) {
  const c = JSON.parse(JSON.stringify(baseCatalog));
  c.packageNetwork.full = (c.packageNetwork.full || []).filter((e) => e.package !== pkg);
  for (const h of c.networkHosts || []) if (Array.isArray(h.fetchedBy)) h.fetchedBy = h.fetchedBy.filter((n) => n !== pkg);
  c.packageGrants = (c.packageGrants || []).filter((e) => e.package !== pkg);

  const observed = `Grant-configuration matrix probe cell "${cell}" for ${pkg}; a scripted ladder arm generated by grant-matrix.mjs, never a shipped entry.`;
  // The full-disk cell carries egress too, deliberately: it is only ever reached after
  // `both` has already failed, so it has to be a strict SUPERSET of that cell or a pass
  // would not be attributable to the filesystem breadth it adds.
  if (cell === 'net' || cell === 'both' || cell === 'fulldisk') {
    c.packageNetwork.full.push({ package: pkg, evidence: 'measured', observed, platform: 'macos-arm64' });
  }
  if (cell === 'fulldisk') {
    c.packageGrants.push({
      package: pkg, fullDisk: true,
      mechanism: 'Matrix probe arm: the whole filesystem, to determine whether this install script fails for a filesystem reason at all.',
      evidence: 'measured', observed, platform: 'macos-arm64',
    });
  }
  if (cell === 'project' || cell === 'both') {
    // The BROADEST project shape the schema can express. Deliberately unscoped: the
    // cell's question is "does this script need the project at all", and a scoped probe
    // that missed the real path would answer no for the wrong reason. Scoping is a
    // follow-up decision, made once a package is known to need project access.
    //
    // `**`, NOT `.` — and this cell spelled it `.` until 2026-07-31, which made it INERT.
    // `contained()` (compiler/curated.rs) returns None when the joined path EQUALS the
    // project root: `(joined != root && joined.starts_with(&root))`. The catalog VALIDATOR
    // accepts `.`, so nothing errored; the compiler just dropped it, leaving `projectCwd`
    // (by its own comment "the NODE alone") as the whole grant. Measured by intervention,
    // one variable, four arms on nx@15.9.7 with banners engaged in all of them: no grant
    // rc=1 EPERM on `<proj>/nx.json`; `.` rc=1, the SAME EPERM; `nx.json` rc=0; `*` rc=0;
    // `**` rc=0. So every `project`/`both` cell before this measured cwd and nothing else,
    // and the corrupted verdict is FAILS-AT-BOTH specifically — a package needing project
    // access failed the `both` cell and was recorded as "something the catalog cannot
    // express" when the catalog expresses it fine. NEEDS-NOTHING and NEEDS-EGRESS rows are
    // unaffected, since neither depends on this grant compiling.
    c.packageGrants.push({
      package: pkg, projectReads: ['**'], projectWrites: { literal: ['**'] }, projectCwd: true,
      mechanism: 'Matrix probe arm: unscoped project read/write/cwd, to determine empirically whether this install script needs project access at all.',
      evidence: 'measured', observed, platform: 'macos-arm64',
    });
  }

  // Re-derive the effective sets exactly as parse_package_network / parse_grants do, and
  // refuse to run a cell that does not express what it claims.
  const refused = new Set((c.notGranted?.packages || []).map((e) => e.package));
  const egress = new Set();
  for (const h of c.networkHosts || []) for (const n of h.fetchedBy || []) egress.add(n);
  for (const e of c.packageNetwork.full) egress.add(e.package);
  for (const n of refused) egress.delete(n);
  const wantNet = cell === 'net' || cell === 'both' || cell === 'fulldisk';
  const wantGrant = cell === 'project' || cell === 'both' || cell === 'fulldisk';
  if (egress.has(pkg) !== wantNet) throw new Error(`cell ${cell}/${pkg}: egress=${egress.has(pkg)} want ${wantNet}`);
  if (c.packageGrants.some((e) => e.package === pkg) !== wantGrant) throw new Error(`cell ${cell}/${pkg}: grant mismatch`);
  if (c.packageGrants.some((e) => e.package === pkg && e.fullDisk === true) !== (cell === 'fulldisk')) {
    throw new Error(`cell ${cell}/${pkg}: fullDisk mismatch`);
  }

  // The VERSION is in the filename only to keep two slots running the same package at
  // different versions off one path: `writeFileSync` truncates, so a concurrent read of a
  // half-written catalog would trip the banner check and waste an otherwise good row.
  const file = path.join(ROOT, 'catalogs', `${slug(pkg)}-${slug(version)}-${cell}.json`);
  fs.writeFileSync(file, JSON.stringify(c, null, 2));
  // The exact substring the binary's banner must contain. Deriving it here and matching
  // it there is what makes "the edit took effect" a measurement rather than an assumption.
  return { file, banner: `(${(c.networkHosts || []).length} hosts, ${c.packageGrants.length} package grants, ${egress.size} egress entries)` };
}

const slug = (s) => s.replace(/[^A-Za-z0-9]+/g, '-');

// ── one cell ──────────────────────────────────────────────────────────────────
// Paths that change on EVERY run whether or not the lifecycle script did anything: npm's
// log directory, and the manifest/lockfile that `approve-builds` itself rewrites to record
// the approval. Left in, they keep the measured delta permanently non-zero, which disables
// the no-op detector — a pure nag script then looks like it produced output, is admitted as
// measurable, and trivially "passes" every cell. Excluding them is what lets CONTROL-NOOP
// mean "the script genuinely did nothing".
const NOISE = /(\/_logs\/|\/\.npm\/|\/package\.json$|\/nub\.lock$|\/package-lock\.json$|\/\.modules\.yaml$)/;

function walk(dir, acc) {
  let ents;
  try { ents = fs.readdirSync(dir, { withFileTypes: true }); } catch { return acc; }
  for (const e of ents) {
    const p = path.join(dir, e.name);
    if (e.isSymbolicLink()) continue;
    if (e.isDirectory()) walk(p, acc);
    else if (e.isFile() && !NOISE.test(p)) { try { acc.bytes += fs.statSync(p).size; acc.files++; } catch { /* raced */ } }
  }
  return acc;
}
const measure = (...roots) => roots.reduce((a, r) => walk(r, a), { files: 0, bytes: 0 });

// ── fixture provisioning — the difference between a measurement and a false pass ──
//
// A package whose INPUT is absent exits 0 having done nothing, in EVERY cell, and the
// ladder then reports NEEDS-NOTHING for a package that in fact needs a project write.
// Run against the shipped catalog it is worse: the grant reads as REDUNDANT, and acting
// on that would strip a grant a package genuinely needs — the one direction this whole
// effort is trying not to move in. (Caught here after an unprovisioned minimality pass
// called 8 project-shaped grants redundant, every one of them a script that no-opped in
// both arms for want of a `.git` or a schema.)
//
// Capabilities are named per package in the worklist's third column, matching the corpus
// manifests' NEED column so the two harnesses agree on what a fixture owes a package.
function provision(proj, need, nonce) {
  for (const cap of (need || '').split(',').map((s) => s.trim()).filter(Boolean)) {
    const pj = path.join(proj, 'package.json');
    switch (cap) {
      case 'git-repo': {
        // A real repository, not just the directory: some installers check validity.
        const git = (...a) => spawnSync('git', ['-C', proj, ...a], { encoding: 'utf8' });
        git('init', '-q');
        git('config', 'user.email', 'probe@example.com');
        git('config', 'user.name', 'probe');
        git('commit', '-qm', 'probe', '--allow-empty');
        break;
      }
      case 'prisma-schema': {
        fs.mkdirSync(path.join(proj, 'prisma'), { recursive: true });
        // The nonce rides IN the generator input, so generated output carrying it cannot
        // have come from a cache — no cache could supply a name invented this run.
        fs.writeFileSync(path.join(proj, 'prisma/schema.prisma'),
          `generator client {\n  provider = "prisma-client-js"\n}\ndatasource db {\n  provider = "sqlite"\n  url      = "file:./dev.db"\n}\nmodel ${nonce} {\n  id Int @id @default(autoincrement())\n}\n`);
        break;
      }
      case 'lefthook-config':
        fs.writeFileSync(path.join(proj, 'lefthook.yml'), `pre-commit:\n  commands:\n    probe:\n      run: echo ${nonce}\n`);
        break;
      case 'msw-manifest': {
        fs.mkdirSync(path.join(proj, 'public'), { recursive: true });
        const j = JSON.parse(fs.readFileSync(pj, 'utf8'));
        j.msw = { workerDirectory: ['public'] };
        fs.writeFileSync(pj, JSON.stringify(j, null, 2));
        break;
      }
      case 'nx-json':
        fs.writeFileSync(path.join(proj, 'nx.json'), '{ "affected": { "defaultBase": "main" } }\n');
        break;
      case 'vue-dep': {
        const j = JSON.parse(fs.readFileSync(pj, 'utf8'));
        j.dependencies.vue = '3.5.13';
        fs.writeFileSync(pj, JSON.stringify(j, null, 2));
        break;
      }
      default:
        throw new Error(`unknown fixture capability '${cap}'`);
    }
  }
}

// WINDOWS ENV FLOOR — transplanted verbatim from `run-shard.sh`'s, deliberately, so the
// matrix and the corpus measure the SAME environment and their Windows numbers stay
// comparable. A near-empty env is the whole point of this harness on POSIX, but on Windows
// it is not a clean room, it is a broken one: a native process resolves system DLLs and
// the winsock provider catalogue relative to `%SystemRoot%`, so a child without it fails
// to START. Every cell would then score `INSTALL-FAILED` and every package would read as
// fails-at-every-cell — a whole sweep of false findings that look exactly like the
// interesting category.
//
// Every path-shaped member points into the cell's own private tree, so isolation survives.
// `LOCALAPPDATA` is the exception and is NOT like the others: `CreateAppContainerProfile`
// takes a name and no path, so Windows creates the profile under the CALLING user's real
// `%LOCALAPPDATA%\Packages` while the confined child resolves its redirected temp from
// whatever it was handed — point that at a synthetic tree and the two compose different
// paths. `WIN_HOST_LOCALAPPDATA=1` hands the child the host's value instead; off by
// default so this matches the corpus rather than silently diverging from it.
function winEnvFloor(home, tmp) {
  if (!WINDOWS) return {};
  const sysRoot = process.env.SystemRoot || process.env.SYSTEMROOT || 'C:\\Windows';
  fs.mkdirSync(path.join(home, 'AppData', 'Roaming'), { recursive: true });
  fs.mkdirSync(path.join(home, 'AppData', 'Local'), { recursive: true });
  const localAppData = process.env.WIN_HOST_LOCALAPPDATA
    ? process.env.LOCALAPPDATA || process.env.LocalAppData || path.join(home, 'AppData', 'Local')
    : path.join(home, 'AppData', 'Local');
  return {
    SystemRoot: sysRoot,
    windir: process.env.windir || process.env.WINDIR || sysRoot,
    COMSPEC: process.env.COMSPEC || process.env.ComSpec || `${sysRoot}\\system32\\cmd.exe`,
    PATHEXT: process.env.PATHEXT || '.COM;.EXE;.BAT;.CMD',
    OS: process.env.OS || 'Windows_NT',
    NUMBER_OF_PROCESSORS: process.env.NUMBER_OF_PROCESSORS || '2',
    PROCESSOR_ARCHITECTURE: process.env.PROCESSOR_ARCHITECTURE || 'AMD64',
    SystemDrive: process.env.SystemDrive || process.env.SYSTEMDRIVE || 'C:',
    ProgramData: process.env.ProgramData || process.env.PROGRAMDATA || 'C:\\ProgramData',
    ProgramFiles: process.env.ProgramFiles || process.env.PROGRAMFILES || 'C:\\Program Files',
    USERPROFILE: home,
    APPDATA: path.join(home, 'AppData', 'Roaming'),
    LOCALAPPDATA: localAppData,
    TEMP: tmp,
    TMP: tmp,
  };
}

function runCell(pkg, version, cell, nonce, need) {
  const jailOff = cell === 'off';
  const dir = path.join(ROOT, 'fx', `${slug(pkg)}-${cell}-${nonce}`);
  const proj = path.join(dir, 'proj'), home = path.join(dir, 'home'), tmp = path.join(dir, 'tmp');
  fs.rmSync(dir, { recursive: true, force: true });
  for (const d of [proj, home, tmp]) fs.mkdirSync(d, { recursive: true });

  // FRESHLY GENERATED, never copied: `approve-builds` writes the approval INTO
  // package.json, so a reused fixture runs the script during `install` and leaves an
  // empty window that exits 0 and reads as a pass.
  const manifest = { name: `mx-${nonce}`, version: '1.0.0', private: true, dependencies: { [pkg]: version } };
  if (jailOff) manifest.dependenciesMeta = { [pkg]: { sandbox: false } };
  fs.writeFileSync(path.join(proj, 'package.json'), JSON.stringify(manifest, null, 2));
  provision(proj, need, nonce);
  // minimumReleaseAge blocks ~70 packages at RESOLUTION. side-effects-cache=false is what
  // makes a grant iteration mean anything: the cache otherwise replays the PREVIOUS
  // cell's postinstall result when only the catalog changed.
  fs.writeFileSync(path.join(proj, '.npmrc'), 'minimumReleaseAge=0\nminimumReleaseAgeStrict=false\nside-effects-cache=false\n');

  // The catalog is forwarded even into the jail-off control, so a binary silently lacking
  // the override feature fails on the FIRST cell rather than part-way through a sweep.
  const { file: catalog, banner } = cellCatalog(pkg, jailOff ? 'none' : cell, version);
  const env = {
    ...winEnvFloor(home, tmp),
    PATH: STUDY_PATH, HOME: home, TMPDIR: tmp,
    NUB_CACHE_DIR: STORE, NUB_BUILD_JAIL_CATALOG: catalog,
  };
  const opts = { cwd: proj, env, encoding: 'utf8', timeout: 900_000, maxBuffer: 64 << 20 };

  const inst = spawnSync(NUB, ['install'], opts);
  const instOut = (inst.stdout || '') + (inst.stderr || '');
  if (inst.status !== 0) return { cell, outcome: 'INSTALL-FAILED', rc: inst.status, log: instOut.slice(-4000), dir };

  const pre = measure(proj, home);
  const win = spawnSync(NUB, ['approve-builds', pkg], opts);
  const winOut = (win.stdout || '') + (win.stderr || '');
  const post = measure(proj, home);
  const delta = { files: post.files - pre.files, bytes: post.bytes - pre.bytes };

  // ── the invalidating checks, before any verdict ──
  if (!winOut.includes('build-jail catalog OVERRIDDEN')) return { cell, outcome: 'NO-BANNER', rc: win.status, log: winOut.slice(-4000), dir };
  if (!winOut.includes(banner)) return { cell, outcome: 'BANNER-MISMATCH', want: banner, rc: win.status, log: winOut.slice(-4000), dir };
  if (/No ignored builds to approve/.test(winOut)) return { cell, outcome: 'EMPTY-WINDOW', rc: win.status, log: winOut.slice(-4000), dir };

  return { cell, outcome: 'RAN', rc: win.status, delta, nonce_seen: winOut.includes(nonce), log: winOut.slice(-4000), dir };
}

// Bounded by disk, not by politeness: the host runs at 96% and a single cell's tree can
// be hundreds of MB. Fixtures are dropped as soon as their verdict is recorded; only
// logs survive, which is all a re-read needs.
const prune = (r) => { if (r?.dir) fs.rmSync(r.dir, { recursive: true, force: true }); };

// ── the ladder ────────────────────────────────────────────────────────────────
// Exit code first. The artifact delta decides ONLY the two cases an exit code cannot:
// a control that exits 0 having produced nothing (the early-bail no-op, which makes the
// row unmeasurable), and a cell that exits 0 with an artifact a fraction of the
// control's (the refused fetch that something else satisfied). An order of magnitude is
// the threshold because the measured false passes are three to nine orders down.
const UNDERSIZED = 0.1;

function verdictFor(ctrl, r) {
  if (r.outcome !== 'RAN') return { pass: false, why: r.outcome };
  if (r.rc !== 0) return { pass: false, why: `rc=${r.rc}` };
  if (ctrl.delta.bytes > 0 && r.delta.bytes / ctrl.delta.bytes < UNDERSIZED) {
    return { pass: false, why: `undersized ${r.delta.bytes}B vs control ${ctrl.delta.bytes}B` };
  }
  return { pass: true, why: 'rc=0' };
}

function ladder(pkg, version, need) {
  const nonce = `N${Date.now().toString(36)}${Math.floor(Math.random() * 1e6).toString(36)}`;
  const cells = {};
  const ctrl = runCell(pkg, version, 'off', nonce, need);
  cells.off = { outcome: ctrl.outcome, rc: ctrl.rc, delta: ctrl.delta };
  if (ctrl.outcome !== 'RAN' || ctrl.rc !== 0) { prune(ctrl); return { pkg, version, requirement: 'CONTROL-FAILED', detail: `${ctrl.outcome} rc=${ctrl.rc}`, cells, log: ctrl.log }; }
  if (ctrl.delta.files === 0) { prune(ctrl); return { pkg, version, requirement: 'CONTROL-NOOP', detail: 'script produced nothing even unconfined — nothing to measure', cells }; }
  prune(ctrl);

  const run = (cell) => { const r = runCell(pkg, version, cell, nonce, need); const v = verdictFor(ctrl, r); cells[cell] = { outcome: r.outcome, rc: r.rc, delta: r.delta, pass: v.pass, why: v.why }; const log = r.log; prune(r); return { v, log }; };

  const none = run('none');
  if (none.v.pass) return { pkg, version, requirement: 'NEEDS-NOTHING', detail: 'passes ungranted', cells };
  if (['NO-BANNER', 'BANNER-MISMATCH', 'EMPTY-WINDOW', 'INSTALL-FAILED'].includes(cells.none.outcome)) {
    return { pkg, version, requirement: 'UNMEASURABLE', detail: cells.none.outcome, cells, log: none.log };
  }

  const both = run('both');
  if (!both.v.pass) {
    // THE TERMINAL RUNG. Everything the catalog's narrow vocabulary can express has now
    // failed, so the only remaining filesystem question is whether the need is a file at
    // all — and that is one cell, not an investigation.
    const full = run('fulldisk');
    if (full.v.pass) return { pkg, version, requirement: 'NEEDS-FULL-DISK', detail: `fails at both (${both.v.why}), passes with the whole filesystem`, cells, log: both.log };
    return { pkg, version, requirement: 'FAILS-AT-FULL-DISK', detail: `${both.v.why}; still fails with the whole filesystem (${full.v.why}) — not a filesystem need`, cells, log: full.log };
  }

  const net = run('net');
  if (net.v.pass) return { pkg, version, requirement: 'NEEDS-EGRESS', detail: 'passes with egress alone', cells };

  const proj = run('project');
  if (proj.v.pass) return { pkg, version, requirement: 'NEEDS-PROJECT', detail: 'passes with project access alone', cells };
  return { pkg, version, requirement: 'NEEDS-BOTH', detail: 'neither grant alone suffices', cells };
}

// ── the minimality check ──────────────────────────────────────────────────────
// Same shape, one cell: run a package the catalog ALREADY grants with its grant removed.
// A pass means the shipped grant is not doing anything and should be dropped. The
// control still runs, because a package that no-ops unconfined would "pass" ungranted
// for a reason that has nothing to do with the grant.
function minimality(pkg, version, need) {
  const nonce = `N${Date.now().toString(36)}${Math.floor(Math.random() * 1e6).toString(36)}`;
  const cells = {};
  const ctrl = runCell(pkg, version, 'off', nonce, need);
  cells.off = { outcome: ctrl.outcome, rc: ctrl.rc, delta: ctrl.delta };
  if (ctrl.outcome !== 'RAN' || ctrl.rc !== 0) { prune(ctrl); return { pkg, version, requirement: 'CONTROL-FAILED', detail: `${ctrl.outcome} rc=${ctrl.rc}`, cells }; }
  if (ctrl.delta.files === 0) { prune(ctrl); return { pkg, version, requirement: 'CONTROL-NOOP', detail: 'no-op unconfined', cells }; }
  prune(ctrl);
  const r = runCell(pkg, version, 'none', nonce, need);
  const v = verdictFor(ctrl, r);
  cells.none = { outcome: r.outcome, rc: r.rc, delta: r.delta, pass: v.pass, why: v.why };
  const log = r.log; prune(r);
  return v.pass
    ? { pkg, version, requirement: 'GRANT-REDUNDANT', detail: `passes with its grant removed (${v.why})`, cells }
    : { pkg, version, requirement: 'GRANT-NEEDED', detail: v.why, cells, log };
}

// ── the worklist ──────────────────────────────────────────────────────────────
function worklist() {
  const list = arg('--list');
  if (list) {
    return fs.readFileSync(list, 'utf8').split('\n').filter((l) => l.trim() && !l.startsWith('#'))
      .map((l) => { const [name, version, need] = l.split('\t'); return { name: name.trim(), version: (version || 'latest').trim(), need: (need || '').split('#')[0].trim() }; });
  }
  if (MINIMALITY) {
    // Everything the catalog grants, egress and project alike.
    //
    // PIN THE VERSION IF YOU CAN — pass `--list` with the versions the grant was
    // measured against. Falling back to `latest` asks a DIFFERENT question: a package
    // whose newer release dropped its install script (as `@prisma/client` 7.0.0 did)
    // no-ops unconfined and the row reports CONTROL-NOOP, which says nothing about
    // whether the shipped grant is redundant for the version it was written for.
    const names = new Set();
    for (const e of baseCatalog.packageNetwork.full || []) names.add(e.package);
    for (const h of baseCatalog.networkHosts || []) for (const n of h.fetchedBy || []) names.add(n);
    for (const e of baseCatalog.packageGrants || []) names.add(e.package);
    return [...names].sort().map((name) => ({ name, version: 'latest', need: '' }));
  }
  console.error('--list <file.tsv> required (name<TAB>version per line)');
  process.exit(2);
}

// ── the emitted record ────────────────────────────────────────────────────────
//
// THE RECORD IS THE DELIVERABLE, not the table. A verdict transcribed by hand into the
// catalog loses its evidence and does not scale past a few dozen packages, so every run
// emits something `apply-matrix.mjs` can ingest without a human in the middle. Three
// things make a record ingestible rather than merely informative:
//
//   the CELLS that produced the verdict — so the ingester can re-derive the verdict and
//     refuse a record whose cells contradict it, which is what stops a bad run poisoning
//     the catalog;
//   the exact GRANT implied, in the catalog's own vocabulary, so applying it is a merge
//     and not an interpretation;
//   PROVENANCE — binary, platform, host, time — because a grant measured on one platform
//     against one binary is not a fact about every platform forever, and a later reader
//     needs to be able to tell whether it still holds.
const PROVENANCE = {
  // Hashed in-process rather than by shelling out: `shasum` is not on a Windows PATH, and
  // the provenance silently degrading to `null` is exactly the kind of quiet gap that makes
  // a later reader unable to tell which binary produced a grant.
  binary_sha256: (() => { try { return crypto.createHash('sha256').update(fs.readFileSync(NUB)).digest('hex'); } catch { return null; } })(),
  nub_version: (() => { const r = spawnSync(NUB, ['--version'], { encoding: 'utf8' }); return (r.stdout || '').trim().split('\n')[0] || null; })(),
  platform: `${process.platform}-${process.arch}`,
  node: process.version,
  host: os.hostname(),
  harness: 'grant-matrix.mjs',
};

// Machine-specific prefixes stripped from a line before it can reach the catalog. The
// catalog is a TRACKED, world-readable file, and a filesystem-shaped failure — which is
// exactly what the full-disk rung surfaces — otherwise carries the operator's home
// directory and this run's output root into it verbatim. Neither is evidence. The SHAPE of
// the denied path is the whole finding and it survives the substitution unchanged.
const RUN_PATHS = [[ROOT, '<run>'], [os.homedir(), '~']].sort((a, b) => b[0].length - a[0].length);
const scrubPaths = (s) => RUN_PATHS.reduce((acc, [from, to]) => acc.split(from).join(to), s);

// The first line that looks like the reason, for a human triaging the table and for the
// `observed` string the ingester writes into the catalog. Best-effort by design: a
// missing line weakens a record's readability, never its verdict, which rests on cells.
function failingLine(log) {
  if (!log) return null;
  for (const l of log.split('\n')) {
    if (/EPERM|EACCES|ENOTFOUND|ECONNREFUSED|EAI_AGAIN|operation not permitted|Permission denied|not a git repository|getwd:|uv_cwd|Error:|error:|npm error/.test(l)) {
      const t = scrubPaths(l.trim());
      if (t && t.length < 400) return t;
    }
  }
  return null;
}

// Cell results as a flat pass/fail map: 0 = passed, 1 = failed, null = not run because
// the ladder exited early. The names are the ladder's, not the internal keys, so a
// reader of the JSON does not need this file open beside it.
const CELL_NAMES = { off: 'jail_off', none: 'no_grants', project: 'project_write', net: 'network', both: 'both', fulldisk: 'full_disk' };

function shapeRecord(raw, meta) {
  const cells = { jail_off: null, no_grants: null, project_write: null, network: null, both: null, full_disk: null };
  for (const [k, v] of Object.entries(raw.cells || {})) {
    const name = CELL_NAMES[k];
    if (!name) continue;
    cells[name] = k === 'off' ? (v.outcome === 'RAN' && v.rc === 0 ? 0 : 1) : (v.pass ? 0 : 1);
  }
  // The grant the verdict implies, in the catalog's vocabulary. `null` for every verdict
  // that implies no catalog change — including the failures, which are a worklist for a
  // human rather than something to apply.
  const grant =
    raw.requirement === 'NEEDS-EGRESS' ? { packageNetwork: 'full' }
      // `**` for the same reason the probe cell uses it: a `.` here would have had
      // `apply-matrix.mjs` write a grant into the shipped catalog that the compiler
      // silently drops — a catalog entry that looks like a capability and is not. No
      // record ever reached this branch (with `.` inert, the ladder produced FAILS-AT-BOTH
      // instead of NEEDS-PROJECT), so nothing shipped; the bug was latent, not live.
      : raw.requirement === 'NEEDS-PROJECT' ? { packageGrant: { projectReads: ['**'], projectWrites: { literal: ['**'] }, projectCwd: true } }
        : raw.requirement === 'NEEDS-BOTH' ? { packageNetwork: 'full', packageGrant: { projectReads: ['**'], projectWrites: { literal: ['**'] }, projectCwd: true } }
          // The full-disk cell carries egress, so the grant it implies does too. Recording
          // only the filesystem half would apply a configuration that was never measured.
          : raw.requirement === 'NEEDS-FULL-DISK' ? { packageNetwork: 'full', packageGrant: { fullDisk: true } }
            : raw.requirement === 'GRANT-REDUNDANT' ? { remove: true }
              : null;
  return {
    package: raw.pkg,
    version: raw.version,
    weekly_downloads: meta.weekly ?? null,
    fixture_capabilities: meta.need || null,
    verdict: raw.requirement,
    mode: MINIMALITY ? 'minimality' : 'ladder',
    cells,
    cell_detail: raw.cells || {},
    grant,
    evidence: {
      detail: raw.detail || null,
      failing_line: failingLine(raw.log),
      control_artifact: raw.cells?.off?.delta ?? null,
      ungranted_artifact: raw.cells?.none?.delta ?? null,
    },
    ...PROVENANCE,
    measured_at: new Date().toISOString(),
    secs: meta.secs,
  };
}

// ── driver ────────────────────────────────────────────────────────────────────
// Packages are independent, so several run at once; the cells WITHIN a package stay
// sequential because they share a fixture root and the ladder's early exit depends on
// the previous cell's answer.
const CENSUS = arg('--census');
const weeklyOf = (() => {
  if (!CENSUS || !fs.existsSync(CENSUS)) return () => null;
  const m = new Map();
  for (const l of fs.readFileSync(CENSUS, 'utf8').split('\n')) {
    if (!l.trim()) continue;
    try { const o = JSON.parse(l); m.set(o.name, o.package_weekly_downloads ?? null); } catch { /* skip */ }
  }
  return (n) => m.get(n) ?? null;
})();

// Resume and log keys are name@VERSION, not name. A worklist may legitimately carry the
// same package at several versions — that is the whole shape of the old-pinned-version
// study, where the comparison IS latest-vs-older of one package. Keyed on the name alone,
// a resume would silently drop every band but the first, and the second band's log would
// overwrite the first's; both failures are invisible in the output.
const key = (name, version) => `${name}@${version}`;
const work = worklist().slice(0, LIMIT || undefined);
const done = new Set();
if (fs.existsSync(RESULTS)) {
  for (const l of fs.readFileSync(RESULTS, 'utf8').split('\n')) { if (!l.trim()) continue; try { const o = JSON.parse(l); done.add(key(o.package, o.version)); } catch { /* partial line */ } }
}
const pending = work.filter((w) => !done.has(key(w.name, w.version)));
console.log(`${MINIMALITY ? 'minimality' : 'ladder'}: ${pending.length} to run (${done.size} already recorded) -> ${RESULTS}`);

// A worker per job slot, each pulling the next name. Written as processes rather than
// promises because spawnSync is what keeps a cell's steps ordered and legible.
const slot = Number(arg('--slot', '-1'));
if (slot >= 0) {
  for (let i = slot; i < pending.length; i += JOBS) {
    const { name, version, need } = pending[i];
    const t = Date.now();
    let raw;
    try { raw = MINIMALITY ? minimality(name, version, need) : ladder(name, version, need); }
    catch (e) { raw = { pkg: name, version, requirement: 'ERROR', detail: String(e && e.message), cells: {} }; }
    const secs = Math.round((Date.now() - t) / 1000);
    const rec = shapeRecord(raw, { weekly: weeklyOf(name), need, secs });
    fs.appendFileSync(RESULTS, JSON.stringify(rec) + '\n');
    if (raw.log) fs.writeFileSync(path.join(ROOT, 'logs', `${slug(name)}-${slug(version)}.log`), raw.log);
    console.log(`[${slot}] ${name}@${version}\t${rec.verdict}\t${secs}s\t${rec.evidence.detail || ''}`);
  }
  process.exit(0);
}
console.log(`run ${JOBS} slots:  for i in $(seq 0 ${JOBS - 1}); do node grant-matrix.mjs ${argv.join(' ')} --slot $i & done; wait`);

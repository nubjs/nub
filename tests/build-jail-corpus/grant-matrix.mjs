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
//       fails at c5           => something the catalog cannot express. RECORD AND MOVE ON.
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
import { spawnSync } from 'node:child_process';

// ── args ──────────────────────────────────────────────────────────────────────
const argv = process.argv.slice(2);
const arg = (n, d) => { const i = argv.indexOf(n); return i >= 0 ? argv[i + 1] : d; };
const has = (n) => argv.includes(n);

const NUB = process.env.NUB_BIN;
if (!NUB || !fs.existsSync(NUB)) { console.error('set NUB_BIN to a nub built with --features build-jail-catalog-override'); process.exit(2); }
const HARNESS = path.dirname(new URL(import.meta.url).pathname);
const BASE_CATALOG = process.env.BASE_CATALOG || path.join(HARNESS, '../../crates/nub-sandbox/data/build-jail-catalog.json');
const STUDY_PATH = process.env.STUDY_PATH || '/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin';
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

function cellCatalog(pkg, cell) {
  const c = JSON.parse(JSON.stringify(baseCatalog));
  c.packageNetwork.full = (c.packageNetwork.full || []).filter((e) => e.package !== pkg);
  for (const h of c.networkHosts || []) if (Array.isArray(h.fetchedBy)) h.fetchedBy = h.fetchedBy.filter((n) => n !== pkg);
  c.packageGrants = (c.packageGrants || []).filter((e) => e.package !== pkg);

  const observed = `Grant-configuration matrix probe cell "${cell}" for ${pkg}; a scripted ladder arm generated by grant-matrix.mjs, never a shipped entry.`;
  if (cell === 'net' || cell === 'both') {
    c.packageNetwork.full.push({ package: pkg, evidence: 'measured', observed, platform: 'macos-arm64' });
  }
  if (cell === 'project' || cell === 'both') {
    // The BROADEST project shape the schema can express. Deliberately unscoped: the
    // cell's question is "does this script need the project at all", and a scoped probe
    // that missed the real path would answer no for the wrong reason. Scoping is a
    // follow-up decision, made once a package is known to need project access.
    c.packageGrants.push({
      package: pkg, projectReads: ['.'], projectWrites: { literal: ['.'] }, projectCwd: true,
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
  const wantNet = cell === 'net' || cell === 'both';
  const wantProject = cell === 'project' || cell === 'both';
  if (egress.has(pkg) !== wantNet) throw new Error(`cell ${cell}/${pkg}: egress=${egress.has(pkg)} want ${wantNet}`);
  if (c.packageGrants.some((e) => e.package === pkg) !== wantProject) throw new Error(`cell ${cell}/${pkg}: grant mismatch`);

  const file = path.join(ROOT, 'catalogs', `${slug(pkg)}-${cell}.json`);
  fs.writeFileSync(file, JSON.stringify(c, null, 2));
  // The exact substring the binary's banner must contain. Deriving it here and matching
  // it there is what makes "the edit took effect" a measurement rather than an assumption.
  return { file, banner: `(${(c.networkHosts || []).length} hosts, ${c.packageGrants.length} package grants, ${egress.size} egress entries)` };
}

const slug = (s) => s.replace(/[^A-Za-z0-9]+/g, '-');

// ── one cell ──────────────────────────────────────────────────────────────────
function walk(dir, acc) {
  let ents;
  try { ents = fs.readdirSync(dir, { withFileTypes: true }); } catch { return acc; }
  for (const e of ents) {
    const p = path.join(dir, e.name);
    if (e.isSymbolicLink()) continue;
    if (e.isDirectory()) walk(p, acc);
    else if (e.isFile()) { try { acc.bytes += fs.statSync(p).size; acc.files++; } catch { /* raced */ } }
  }
  return acc;
}
const measure = (...roots) => roots.reduce((a, r) => walk(r, a), { files: 0, bytes: 0 });

function runCell(pkg, version, cell, nonce) {
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
  // minimumReleaseAge blocks ~70 packages at RESOLUTION. side-effects-cache=false is what
  // makes a grant iteration mean anything: the cache otherwise replays the PREVIOUS
  // cell's postinstall result when only the catalog changed.
  fs.writeFileSync(path.join(proj, '.npmrc'), 'minimumReleaseAge=0\nminimumReleaseAgeStrict=false\nside-effects-cache=false\n');

  // The catalog is forwarded even into the jail-off control, so a binary silently lacking
  // the override feature fails on the FIRST cell rather than part-way through a sweep.
  const { file: catalog, banner } = cellCatalog(pkg, jailOff ? 'none' : cell);
  const env = {
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

function ladder(pkg, version) {
  const nonce = `N${Date.now().toString(36)}${Math.floor(Math.random() * 1e6).toString(36)}`;
  const cells = {};
  const ctrl = runCell(pkg, version, 'off', nonce);
  cells.off = { outcome: ctrl.outcome, rc: ctrl.rc, delta: ctrl.delta };
  if (ctrl.outcome !== 'RAN' || ctrl.rc !== 0) { prune(ctrl); return { pkg, version, requirement: 'CONTROL-FAILED', detail: `${ctrl.outcome} rc=${ctrl.rc}`, cells, log: ctrl.log }; }
  if (ctrl.delta.files === 0) { prune(ctrl); return { pkg, version, requirement: 'CONTROL-NOOP', detail: 'script produced nothing even unconfined — nothing to measure', cells }; }
  prune(ctrl);

  const run = (cell) => { const r = runCell(pkg, version, cell, nonce); const v = verdictFor(ctrl, r); cells[cell] = { outcome: r.outcome, rc: r.rc, delta: r.delta, pass: v.pass, why: v.why }; const log = r.log; prune(r); return { v, log }; };

  const none = run('none');
  if (none.v.pass) return { pkg, version, requirement: 'NEEDS-NOTHING', detail: 'passes ungranted', cells };
  if (['NO-BANNER', 'BANNER-MISMATCH', 'EMPTY-WINDOW', 'INSTALL-FAILED'].includes(cells.none.outcome)) {
    return { pkg, version, requirement: 'UNMEASURABLE', detail: cells.none.outcome, cells, log: none.log };
  }

  const both = run('both');
  if (!both.v.pass) return { pkg, version, requirement: 'FAILS-AT-BOTH', detail: both.v.why, cells, log: both.log };

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
function minimality(pkg, version) {
  const nonce = `N${Date.now().toString(36)}${Math.floor(Math.random() * 1e6).toString(36)}`;
  const cells = {};
  const ctrl = runCell(pkg, version, 'off', nonce);
  cells.off = { outcome: ctrl.outcome, rc: ctrl.rc, delta: ctrl.delta };
  if (ctrl.outcome !== 'RAN' || ctrl.rc !== 0) { prune(ctrl); return { pkg, version, requirement: 'CONTROL-FAILED', detail: `${ctrl.outcome} rc=${ctrl.rc}`, cells }; }
  if (ctrl.delta.files === 0) { prune(ctrl); return { pkg, version, requirement: 'CONTROL-NOOP', detail: 'no-op unconfined', cells }; }
  prune(ctrl);
  const r = runCell(pkg, version, 'none', nonce);
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
      .map((l) => { const [name, version] = l.split('\t'); return { name: name.trim(), version: (version || 'latest').trim() }; });
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
    return [...names].sort().map((name) => ({ name, version: 'latest' }));
  }
  console.error('--list <file.tsv> required (name<TAB>version per line)');
  process.exit(2);
}

// ── driver ────────────────────────────────────────────────────────────────────
// Packages are independent, so several run at once; the cells WITHIN a package stay
// sequential because they share a fixture root and the ladder's early exit depends on
// the previous cell's answer.
const work = worklist().slice(0, LIMIT || undefined);
const done = new Set();
if (fs.existsSync(RESULTS)) {
  for (const l of fs.readFileSync(RESULTS, 'utf8').split('\n')) { if (!l.trim()) continue; try { done.add(JSON.parse(l).pkg); } catch { /* partial line */ } }
}
const pending = work.filter((w) => !done.has(w.name));
console.log(`${MINIMALITY ? 'minimality' : 'ladder'}: ${pending.length} to run (${done.size} already recorded) -> ${RESULTS}`);

// A worker per job slot, each pulling the next name. Written as processes rather than
// promises because spawnSync is what keeps a cell's steps ordered and legible.
const slot = Number(arg('--slot', '-1'));
if (slot >= 0) {
  for (let i = slot; i < pending.length; i += JOBS) {
    const { name, version } = pending[i];
    const t = Date.now();
    let out;
    try { out = MINIMALITY ? minimality(name, version) : ladder(name, version); }
    catch (e) { out = { pkg: name, version, requirement: 'ERROR', detail: String(e && e.message) }; }
    out.secs = Math.round((Date.now() - t) / 1000);
    fs.appendFileSync(RESULTS, JSON.stringify(out) + '\n');
    if (out.log) { fs.writeFileSync(path.join(ROOT, 'logs', `${slug(name)}.log`), out.log); delete out.log; }
    console.log(`[${slot}] ${name}@${version}\t${out.requirement}\t${out.secs}s\t${out.detail || ''}`);
  }
  process.exit(0);
}
console.log(`run ${JOBS} slots:  for i in $(seq 0 ${JOBS - 1}); do node grant-matrix.mjs ${argv.join(' ')} --slot $i & done; wait`);

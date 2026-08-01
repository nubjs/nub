#!/usr/bin/env node
// store-isolation-probe.mjs — does a package that writes into a SIBLING package's CAS
// store entry still fail when it is the ONLY package in a brand-new store?
//
// WHY THIS EXISTS. Six packages reach the catalog's terminal (whole-disk) rung by
// touching another package's store entry, and every one of those siblings is an ordinary
// `dependencies` entry of the failing package. That invites a tempting reading: the CAS
// DEDUPLICATES, so the sibling entry is shared machinery the jail is right to protect,
// and installing the package alone would hand it a private copy that needs no grant at
// all. If that held, six whole-disk grants would be an artifact of install SHAPE rather
// than a capability any package genuinely needs.
//
// It does not hold, and this probe is the one-variable demonstration. `grant-matrix.mjs`
// already installs exactly one package per fixture, and — though its own comment claims a
// shared store — `NUB_CACHE_DIR` redirects only the primer cache, so the CAS lands under
// the per-cell `HOME` and every cell has always run against an empty store. This script
// makes that explicit instead of incidental: it asserts the store does not exist before
// the install, enumerates the closure that install materialises, and then checks the
// ARTIFACT the postinstall is supposed to produce rather than trusting an exit code.
//
// THE ARTIFACT CHECK IS NOT OPTIONAL HERE. `wordpos`'s postinstall builds index files into
// `wordnet-db/dict/`; a run that is denied those writes can still exit 0, so rc alone
// cannot tell a working install from a silently crippled one. The probe records the index
// files' presence and size in both arms and compares them.
//
// USAGE
//   NUB_BIN=/path/to/nub node store-isolation-probe.mjs \
//     --package wordpos --version 2.1.0 \
//     --artifact 'wordnet-db/dict/fast-index.adv.json' [--linker isolated|hoisted] [--out DIR]
//
// `--artifact` is a path RELATIVE to a store entry's `node_modules/`, since the write under
// test is by definition into a package other than the one being installed.
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import crypto from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const argv = process.argv.slice(2);
const arg = (n, d) => { const i = argv.indexOf(n); return i >= 0 ? argv[i + 1] : d; };

const NUB = process.env.NUB_BIN;
if (!NUB || !fs.existsSync(NUB)) { console.error('set NUB_BIN to a nub built with --features build-jail-catalog-override'); process.exit(2); }

const HARNESS = path.dirname(fileURLToPath(import.meta.url));
const BASE_CATALOG = process.env.BASE_CATALOG || path.join(HARNESS, '../../crates/nub-sandbox/data/build-jail-catalog.json');
const PKG = arg('--package');
const VERSION = arg('--version', 'latest');
const ARTIFACT = arg('--artifact');
const LINKER = arg('--linker', '');
const ROOT = arg('--out', path.join(os.homedir(), '.cache/nub/store-isolation-probe'));
if (!PKG) { console.error('--package required'); process.exit(2); }

const RM = { recursive: true, force: true, maxRetries: 20, retryDelay: 200 };
const STUDY_PATH = process.env.STUDY_PATH || '/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin';
const slug = (s) => String(s).replace(/[^A-Za-z0-9]+/g, '-');
const base = JSON.parse(fs.readFileSync(BASE_CATALOG, 'utf8'));

// The package's own entries are stripped from every arm, so the shipped catalog cannot
// supply the capability under test and a pass is attributable to the arm alone. Egress
// comes from two places and catalog.rs unions them, so both are cleared.
function cellCatalog(cell, { project = false, egress: wantEgress = false } = {}) {
  const c = JSON.parse(JSON.stringify(base));
  c.packageNetwork.full = (c.packageNetwork.full || []).filter((e) => e.package !== PKG);
  for (const h of c.networkHosts || []) if (Array.isArray(h.fetchedBy)) h.fetchedBy = h.fetchedBy.filter((n) => n !== PKG);
  c.packageGrants = (c.packageGrants || []).filter((e) => e.package !== PKG);

  // The broadest project shape the schema can express — the same one `grant-matrix.mjs`'s
  // `project` cell writes, `**` and not `.`, since `contained()` drops a rule that resolves
  // to the project root itself and the cell would then be silently inert.
  if (project) {
    c.packageGrants.push({
      package: PKG, projectReads: ['**'], projectWrites: { literal: ['**'] }, projectCwd: true,
      mechanism: 'Store-isolation probe arm: unscoped project read/write/cwd, to determine whether a hoisted layout moves this package\'s write into a path the catalog can already express.',
      evidence: 'measured', observed: `store-isolation-probe arm "${cell}" for ${PKG}; never a shipped entry.`, platform: 'macos-arm64',
    });
  }

  if (wantEgress) c.packageNetwork.full.push({ package: PKG, evidence: 'measured', observed: `store-isolation-probe arm "${cell}" for ${PKG}; never a shipped entry.`, platform: 'macos-arm64' });

  const refused = new Set((c.notGranted?.packages || []).map((e) => e.package));
  const egress = new Set();
  for (const h of c.networkHosts || []) for (const n of h.fetchedBy || []) egress.add(n);
  for (const e of c.packageNetwork.full) egress.add(e.package);
  for (const n of refused) egress.delete(n);
  // Re-derived exactly as `parse_package_network` / `parse_grants` do, so an arm that does
  // not express what it claims refuses to run rather than producing a verdict nobody can
  // attribute. Egress is never granted by any arm here: the writes under test are local.
  if (egress.has(PKG) !== wantEgress) throw new Error(`arm ${cell}: egress=${egress.has(PKG)} want ${wantEgress}`);
  if (c.packageGrants.some((e) => e.package === PKG) !== project) throw new Error(`arm ${cell}: grant mismatch (want project=${project})`);

  const file = path.join(ROOT, `catalog-${cell}.json`);
  fs.writeFileSync(file, JSON.stringify(c, null, 2));
  // Derived here and matched against the binary's banner there: that pairing is what makes
  // "the override took effect" a measurement rather than an assumption.
  return { file, banner: `(${(c.networkHosts || []).length} hosts, ${c.packageGrants.length} package grants, ${egress.size} egress entries)` };
}

// Enumerate the store's top level. This is the isolation claim itself: if the closure
// listed here is only the package under test plus its own dependencies, then the sibling
// entry it fails to write is one this very install created, not shared machinery.
function storeEntries(store) {
  try { return fs.readdirSync(store).filter((n) => !n.startsWith('.')).sort(); } catch { return null; }
}

// The artifact is looked for in BOTH layouts, because the layout is exactly what the
// `--linker` arm changes: under the default isolated linker a dependency's files live in
// its own store entry, and under `hoisted` they live in the project's `node_modules`.
// Checking only the store would report "no artifact" for a perfectly successful hoisted
// control — a false negative that reads identically to a denied write.
function artifactState(store, proj) {
  if (!ARTIFACT) return null;
  const out = [];
  for (const e of storeEntries(store) || []) {
    const p = path.join(store, e, 'node_modules', ARTIFACT);
    try { const st = fs.statSync(p); out.push({ where: `store/${e}`, bytes: st.size }); } catch { /* absent */ }
  }
  const hoisted = path.join(proj, 'node_modules', ARTIFACT);
  try { const st = fs.statSync(hoisted); out.push({ where: 'project/node_modules', bytes: st.size }); } catch { /* absent */ }
  return out;
}

function arm(name, { jail, project = false, egress = false }) {
  const dir = path.join(ROOT, `arm-${name}`);
  const proj = path.join(dir, 'proj'), home = path.join(dir, 'home'), tmp = path.join(dir, 'tmp');
  fs.rmSync(dir, RM);
  for (const d of [proj, home, tmp]) fs.mkdirSync(d, { recursive: true });

  const manifest = { name: `iso-${slug(PKG)}`, version: '1.0.0', private: true, dependencies: { [PKG]: VERSION } };
  if (!jail) manifest.dependenciesMeta = { [PKG]: { sandbox: false } };
  fs.writeFileSync(path.join(proj, 'package.json'), JSON.stringify(manifest, null, 2));
  let npmrc = 'minimumReleaseAge=0\nminimumReleaseAgeStrict=false\nside-effects-cache=false\n';
  if (LINKER) npmrc += `node-linker=${LINKER}\n`;
  fs.writeFileSync(path.join(proj, '.npmrc'), npmrc);

  // FRESHNESS, ASSERTED RATHER THAN ASSUMED. A warm store defeats the whole experiment: a
  // sibling entry materialised by an earlier run would reproduce the failure for exactly
  // the reason under test, and the probe would confirm itself.
  const store = path.join(home, '.cache/nub/pm/store');
  const preexisting = fs.existsSync(store);

  const { file: catalog, banner } = cellCatalog(name, { project, egress });
  const env = { PATH: STUDY_PATH, HOME: home, TMPDIR: tmp, NUB_BUILD_JAIL_CATALOG: catalog };
  const opts = { cwd: proj, env, encoding: 'utf8', timeout: 900_000, maxBuffer: 64 << 20 };

  const inst = spawnSync(NUB, ['install'], opts);
  const instOut = (inst.stdout || '') + (inst.stderr || '');
  if (inst.status !== 0) return { arm: name, preexisting_store: preexisting, outcome: 'INSTALL-FAILED', rc: inst.status, log: instOut.slice(-3000) };

  const closure = storeEntries(store);
  const before = artifactState(store, proj);
  const win = spawnSync(NUB, ['approve-builds', PKG], opts);
  const winOut = (win.stdout || '') + (win.stderr || '');
  const after = artifactState(store, proj);

  const banner_seen = winOut.includes('build-jail catalog OVERRIDDEN');
  const banner_match = winOut.includes(banner);
  const empty_window = /No ignored builds to approve/.test(winOut);
  const denial = (winOut.split('\n').find((l) => /EPERM|EACCES|operation not permitted|Permission denied/.test(l)) || '').trim();

  return {
    arm: name, jail, project, egress, preexisting_store: preexisting, store,
    closure, closure_size: closure ? closure.length : null,
    outcome: empty_window ? 'EMPTY-WINDOW' : 'RAN', rc: win.status,
    banner_seen, banner_match, banner_want: banner,
    artifact_before: before, artifact_after: after,
    denial: denial || null, log: winOut.slice(-3000),
  };
}

fs.rmSync(ROOT, RM);
fs.mkdirSync(ROOT, { recursive: true });
const provenance = {
  package: PKG, version: VERSION, artifact: ARTIFACT || null, linker: LINKER || 'default(isolated)',
  binary_sha256: crypto.createHash('sha256').update(fs.readFileSync(NUB)).digest('hex'),
  nub_version: (spawnSync(NUB, ['--version'], { encoding: 'utf8' }).stdout || '').trim().split('\n')[0] || null,
  platform: `${process.platform}-${process.arch}`, host: os.hostname(), measured_at: new Date().toISOString(),
};

// The `project_write` arm is the one that turns this from a refutation into a decision:
// under the default isolated layout the denied path is a SIBLING STORE ENTRY, which no
// catalog vocabulary reaches, so the ladder walks to whole-disk. Under `hoisted` the same
// write lands in the project's own `node_modules` — and whether the existing project grant
// covers that is a question only a run can answer, not a reading of the path.
const results = [arm('jail_off', { jail: false }), arm('no_grants', { jail: true })];
if (argv.includes('--project-arm')) results.push(arm('project_write', { jail: true, project: true }));
// Only reached deliberately: a residual failure under the project arm that is a DNS error
// rather than a denial says the filesystem question is already settled and the remainder is
// the ordinary egress tier — but saying so from the error string is an inference, and this
// arm is what turns it into a measurement.
if (argv.includes('--both-arm')) results.push(arm('project_and_egress', { jail: true, project: true, egress: true }));
const out = { ...provenance, arms: results };
fs.writeFileSync(path.join(ROOT, 'result.json'), JSON.stringify(out, null, 2));

console.log(`${PKG}@${VERSION}  linker=${provenance.linker}  binary=${provenance.binary_sha256.slice(0, 12)}`);
for (const r of results) {
  console.log(`  ${r.arm.padEnd(10)} rc=${r.rc} outcome=${r.outcome} store_preexisted=${r.preexisting_store} closure=${r.closure_size} banner=${r.banner_seen}/${r.banner_match}`);
  if (r.closure) console.log(`    closure: ${r.closure.join(' ')}`);
  console.log(`    artifact before: ${JSON.stringify(r.artifact_before)}`);
  console.log(`    artifact after : ${JSON.stringify(r.artifact_after)}`);
  if (r.denial) console.log(`    denial: ${r.denial.slice(0, 200)}`);
}
console.log(`-> ${path.join(ROOT, 'result.json')}`);

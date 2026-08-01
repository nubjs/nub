#!/usr/bin/env node
// apply-matrix.mjs — ingest grant-matrix.mjs records into the build-jail catalog.
//
// WHY THIS IS A SCRIPT AND NOT A PROCEDURE. Until now a measurement reached the catalog
// by a human reading a report and typing an entry. That loses findings, cannot be
// re-run, and does not scale past a few dozen packages — and the matrix exists precisely
// to make thousands of packages cheap. This closes the loop: measure -> JSON -> catalog.
//
// THE FOUR PROPERTIES THAT MAKE AUTOMATED CATALOG WRITES SAFE. Each guards a way an
// automated writer could do damage that a human writing one entry would not:
//
// 1. IDEMPOTENT. Re-running changes nothing. The catalog is codegen'd into the binary, so
//    a writer that churns entries produces a rebuild and a diff on every invocation and
//    nobody can tell a real change from noise.
// 2. NEVER OVERWRITES HUMAN-AUTHORED REASONING. An entry whose `observed` was not written
//    by this script is left exactly as it is, and a `notGranted` refusal is never
//    contradicted. Those refusals carry recorded argument — `github.com` and
//    `registry.npmjs.org` are refused because each hosts an authenticated WRITE route on
//    the same hostname, so "the package needs to reach it" is not a reason to admit it.
//    A machine that could overrule that would undo the most considered part of the file.
// 3. RE-DERIVES THE VERDICT FROM THE CELLS AND REFUSES A RECORD THAT CONTRADICTS ITSELF.
//    This is the guard that stops one bad run poisoning the catalog. A record is a claim
//    plus its evidence; if the evidence does not entail the claim, the record is rejected
//    rather than trusted, because the failure mode of an automated writer is applying a
//    hundred wrong entries before anyone notices.
// 4. PRINTS WHAT IT CHANGED, so the result is reviewable as a diff rather than as a
//    before/after file nobody reads.
//
// 5. A FULL-DISK GRANT NEEDS CORROBORATION FROM OUTSIDE ITS OWN RUN. See the section
//    below; this is the only guard that exists because the terminal rung was added.
//
// REMOVALS ARE PROPOSED, NEVER APPLIED. A minimality record saying a shipped grant is
// redundant is the one direction where being wrong BREAKS a package, which is the failure
// mode the jail cares most about. So `--apply` adds; removals print as a proposal and
// need `--remove-redundant` said out loud on top.
//
// SUPERSEDING A FULL-DISK GRANT IS ITS OWN FLAG, AND THE SCRIPT COULD NOT DO IT AT ALL.
// A re-measurement that resolves a `fullDisk` package to something NARROWER used to leave
// the whole-disk grant standing: a NEEDS-EGRESS record carries no `packageGrant`, so the
// existing entry was never looked at and the package ended up holding the whole disk AND a
// new egress entry. A NEEDS-PROJECT record was worse — it reached the entry through
// `Object.assign`, which merges, so `fullDisk: true` survived alongside the narrow keys and
// the run reported `refreshed` as though it had narrowed something. Both are silent wrong
// answers in the widening direction, which is the one direction this file exists to police.
// `--supersede-fulldisk` withdraws a MACHINE-AUTHORED whole-disk grant when a real
// measurement of the same package lands on any other verdict. It is off by default because
// withdrawing a grant is the direction that breaks packages.
//
//   node apply-matrix.mjs <records.ndjson> [more.ndjson ...]        # dry run, the default
//   node apply-matrix.mjs <records.ndjson> --apply
//   node apply-matrix.mjs <records.ndjson> --apply --remove-redundant
//   node apply-matrix.mjs <records.ndjson> --apply --supersede-fulldisk
//   node apply-matrix.mjs ... --catalog <path>
import fs from 'node:fs';
import path from 'node:path';

const argv = process.argv.slice(2);
const arg = (n, d) => { const i = argv.indexOf(n); return i >= 0 ? argv[i + 1] : d; };
const APPLY = argv.includes('--apply');
const REMOVE = argv.includes('--remove-redundant');
const SUPERSEDE = argv.includes('--supersede-fulldisk');
const HARNESS = path.dirname(new URL(import.meta.url).pathname);
const CATALOG = arg('--catalog', path.join(HARNESS, '../../crates/nub-sandbox/data/build-jail-catalog.json'));
const files = argv.filter((a) => !a.startsWith('--') && a !== CATALOG && /\.(ndjson|json)$/.test(a));
if (!files.length) { console.error('usage: apply-matrix.mjs <records.ndjson>... [--apply] [--remove-redundant] [--catalog PATH]'); process.exit(2); }

// The marker that tells a later run — and a later reader — that this entry was written
// by the pipeline and may be updated by it. Absent it, the entry is human-authored and
// off limits. It rides inside `observed`, which the catalog validator already requires,
// so no schema change is needed to carry it.
const MARK = '[grant-matrix]';

// ── the verdict, re-derived ───────────────────────────────────────────────────
// Deliberately a SECOND implementation of the ladder's conclusion rather than a shared
// helper: its job is to disagree with the recorded verdict when the cells do not support
// it, and a shared helper could not. 0 = pass, 1 = fail, null = not run.
function derive(c) {
  if (c.jail_off === null) return null;
  if (c.jail_off === 1) return 'CONTROL-FAILED';
  if (c.no_grants === 0) return 'NEEDS-NOTHING';
  if (c.no_grants === null) return null;
  // The terminal rung. A record written before the full-disk cell existed carries no
  // `full_disk` key at all and still derives FAILS-AT-BOTH, so archived runs stay
  // ingestible; a record that HAS the key must be judged by it.
  if (c.both === 1) {
    if (!('full_disk' in c)) return 'FAILS-AT-BOTH';
    if (c.full_disk === 0) return 'NEEDS-FULL-DISK';
    if (c.full_disk === 1) return 'FAILS-AT-FULL-DISK';
    return null;
  }
  if (c.both === null) return null;
  if (c.network === 0) return 'NEEDS-EGRESS';
  if (c.project_write === 0) return 'NEEDS-PROJECT';
  if (c.network === 1 && c.project_write === 1) return 'NEEDS-BOTH';
  return null;
}

const records = [];
for (const f of files) {
  for (const l of fs.readFileSync(f, 'utf8').split('\n')) {
    if (!l.trim()) continue;
    try { records.push(JSON.parse(l)); } catch { console.error(`skipped unparseable line in ${f}`); }
  }
}

// ── GUARD 5: a full-disk grant needs corroboration from outside its own run ────
//
// ADDING THE TERMINAL RUNG CHANGED WHAT A UNIFORM ENVIRONMENTAL DENIAL COSTS. Before it,
// a package that failed every cell landed as FAILS-AT-BOTH: recorded, applied to nothing,
// moved past. Now the same shape runs one cell further and lands NEEDS-FULL-DISK, which
// WRITES THE MOST PERMISSIVE GRANT THE CATALOG HAS. So a misconfigured host no longer
// produces a harmless row — it produces a whole-disk grant, and the ladder cannot tell the
// difference, because a host-level denial fails every cell exactly as a real need does.
//
// MEASURED, not hypothetical (linux-x64, 2026-08-01). A sweep host had node installed at
// `/opt/node` with `/usr/local/bin/node` a symlink; the jail grants the toolchain at its
// real path, so every script shelling out to bare `npm` was denied. `eth-gas-reporter` and
// `exiftool-vendored` walked the whole ladder and scored NEEDS-FULL-DISK. Both need only
// egress — which is what they score on macOS, and what they score on the same host once
// node is installed at the canonical prefix. Two whole-disk grants would have shipped.
//
// THE TELL GENERALISES AND IS MECHANICAL: `sh: 1: npm: Permission denied` appears in ZERO
// committed records across every platform. A failure signature never seen in the corpus,
// appearing on a new host, is the host. So a full-disk record must clear one of two bars:
//
//   (a) its failing signature is already KNOWN to the committed corpus from a different
//       PLATFORM — a whole different OS hit this too, so it is not environmental; or
//   (b) the same package is CROSS-PLATFORM CORROBORATED — a committed record from another
//       platform also failed the `both` cell. Both survivors have this: `wordpos` and
//       `unrs-resolver`, each a sibling-store write seen on two platforms.
//
// (b) is separate from (a) because the errno spelling is per-OS: Linux reports the
// `unrs-resolver` sibling-store write as `EACCES ... mkdir`, macOS the same write as
// `EPERM ... mkdir`, so signature matching alone would reject a genuinely corroborated row.
//
// ★ (a) SAYS PLATFORM, AND IT SAID `host` UNTIL 2026-08-01. THAT WAS THE HOLE.
// `host` is the machine name, and every CI runner gets a fresh one — so two `windows-latest`
// jobs read as two independent hosts while being the SAME IMAGE with the same tool layout
// and the same tools missing or present. A correlated environment could therefore corroborate
// itself, which is precisely the case the guard exists to exclude, and it is how four
// `Could not find any Python installation to use` rows cleared the bar on the strength of a
// same-image sibling. Measured consequence: under the tightened rule those four go back to
// UNCORROBORATED, joining the seven that never cleared it. That is the correct outcome, and
// they are NOT grandfathered.
//
// A genuinely different same-platform host — a developer's Windows box against a runner —
// SHOULD be able to corroborate, and cannot today, because no record carries a description
// of its toolchain layout to tell the two cases apart. `grant-matrix.mjs --toolchain-proof`
// now produces exactly that description; wiring its fingerprint into the record is what
// would let (a) relax from "a different platform" to "a demonstrably different environment".
// Until a record carries one, platform is the only honest proxy.
//
// A record clearing neither is reported as NEEDS-FULL-DISK:UNCORROBORATED and NOT written.
// It is not silently dropped either — the whole point is that a human looks at it, because
// the honest reading is "we cannot yet tell a real need from this host", not "no".
const RESULTS_DIR = path.join(HARNESS, 'results');
const inputPaths = new Set(files.map((f) => path.resolve(f)));

// Everything already committed, minus whatever is being ingested right now — a run must
// not be allowed to corroborate itself, which is exactly what a contaminated sweep would do.
const corpus = [];
try {
  for (const f of fs.readdirSync(RESULTS_DIR)) {
    if (!f.endsWith('.ndjson')) continue;
    const p = path.join(RESULTS_DIR, f);
    if (inputPaths.has(path.resolve(p))) continue;
    for (const l of fs.readFileSync(p, 'utf8').split('\n')) {
      if (!l.trim()) continue;
      try { corpus.push(JSON.parse(l)); } catch { /* partial line */ }
    }
  }
} catch { /* no results dir — every full-disk row is then uncorroborated, which is correct */ }

// Run-specific detail is noise: fixture paths, nonces, store hashes and byte counts differ
// on every cell of every run. What survives is the error class and the shape of the
// message, which is the part that is a property of the package rather than of the machine.
function signature(line) {
  if (!line) return null;
  return String(line)
    .replace(/'[^']*'/g, "'P'").replace(/"[^"]*"/g, '"P"')
    .replace(/[A-Za-z]:\\[^\s'"]+/g, 'P')
    .replace(/\/[^\s'"]+/g, 'P')
    .replace(/\b[0-9a-f]{8,}\b/gi, 'H')
    .replace(/\b\d+\b/g, 'N')
    .replace(/\s+/g, ' ').trim().toLowerCase() || null;
}

const knownSignatures = new Map(); // signature -> Set of PLATFORMS that produced it
const failedBothElsewhere = new Map(); // package -> Set of platforms where `both` failed
for (const r of corpus) {
  const s = signature(r.evidence?.failing_line);
  if (s) {
    if (!knownSignatures.has(s)) knownSignatures.set(s, new Set());
    knownSignatures.get(s).add(r.platform);
  }
  if (r.cells?.both === 1) {
    if (!failedBothElsewhere.has(r.package)) failedBothElsewhere.set(r.package, new Set());
    failedBothElsewhere.get(r.package).add(r.platform);
  }
}

// Returns null when the record may be written, or the reason it may not.
function fullDiskObjection(r) {
  const platforms = failedBothElsewhere.get(r.package);
  if (platforms && [...platforms].some((p) => p !== r.platform)) return null; // (b)
  const s = signature(r.evidence?.failing_line);
  const seenOn = s ? knownSignatures.get(s) : null;
  if (seenOn && [...seenOn].some((p) => p !== r.platform)) return null;       // (a)
  return s
    ? `failure signature "${s}" has been seen on no platform but ${r.platform}, and no other platform failed the both cell for this package`
    : `no failing line recorded, and no other platform failed the both cell for this package`;
}

const catalog = JSON.parse(fs.readFileSync(CATALOG, 'utf8'));
const refusedPkgs = new Set((catalog.notGranted?.packages || []).map((e) => e.package));
const granted = {
  net: new Set([
    ...(catalog.packageNetwork.full || []).map((e) => e.package),
    ...(catalog.networkHosts || []).flatMap((h) => h.fetchedBy || []),
  ]),
  project: new Set((catalog.packageGrants || []).map((e) => e.package)),
};

// A guard that silently stops guarding is worse than no guard, and this one is a single
// `if` in the middle of a long loop — exactly the shape a later refactor drops. The two
// cases are the two real ones, kept as a regression check rather than as evidence: the
// evidence is the measured run in the comment above.
if (argv.includes('--selftest')) {
  const base = { package: 'p', version: '1', verdict: 'NEEDS-FULL-DISK', platform: 'linux-x64', host: 'H' };
  const cases = [
    ['a host-local denial is refused', { ...base, package: 'eth-gas-reporter', evidence: { failing_line: 'sh: 1: npm: Permission denied' } }, false],
    ['a cross-platform failure is admitted', { ...base, package: 'unrs-resolver', evidence: { failing_line: 'Error: EACCES: permission denied, mkdir \'/x\'' } }, true],
    ['no failing line is refused', { ...base, package: 'never-measured-anywhere', evidence: {} }, false],
    // The hole rule (a) carried until 2026-08-01: this signature exists in the committed
    // corpus on win32-x64 ONLY, produced by several runners with different `host` names.
    // Under the old spelling a different name was enough and the row was admitted.
    ['a same-platform sibling does not corroborate',
      { ...base, package: 'registry-js', platform: 'win32-x64', host: 'runnervm-new', evidence: { failing_line: 'gyp ERR! stack Error: Could not find any Python installation to use' } }, false],
  ];
  let bad = 0;
  for (const [name, rec, wantAdmit] of cases) {
    const admitted = fullDiskObjection(rec) === null;
    if (admitted !== wantAdmit) { console.error(`FAIL ${name}: admitted=${admitted}, want ${wantAdmit}`); bad++; }
    else console.log(`ok  ${name}`);
  }

  // The merge-vs-replace bug had no test and was invisible in the report, which called the
  // widening a refresh. Pin the property directly: a narrow entry replacing a whole-disk one
  // must not carry `fullDisk` forward.
  const wide = { package: 'q', fullDisk: true, evidence: 'measured', observed: `${MARK} old` };
  const narrow = { package: 'q', projectCwd: true, evidence: 'measured', observed: `${MARK} new` };
  const merged = Object.assign({ ...wide }, narrow);
  if (merged.fullDisk !== true) { console.error('FAIL merge-vs-replace: the fixture no longer reproduces the merge'); bad++; }
  else if (narrow.fullDisk !== undefined) { console.error('FAIL merge-vs-replace: replacement still carries fullDisk'); bad++; }
  else console.log('ok  a narrow entry replacing a whole-disk one drops fullDisk');

  process.exit(bad ? 1 : 0);
}

// A REAL MEASUREMENT, as opposed to a row that says the run could not measure anything.
// `CONTROL-FAILED` means the package failed UNJAILED, `UNMEASURABLE`/`ERROR` that the
// harness gave up — none of them is evidence about what the package needs, so none may
// withdraw a grant. Getting this set wrong would strip grants on a bad run, which is the
// exact damage `--supersede-fulldisk` is otherwise designed to avoid.
const MEASURED = new Set(['NEEDS-NOTHING', 'NEEDS-EGRESS', 'NEEDS-PROJECT', 'NEEDS-BOTH', 'FAILS-AT-BOTH', 'FAILS-AT-FULL-DISK']);

const added = [], skipped = [], rejected = [], proposedRemovals = [], uncorroborated = [], superseded = [];

for (const r of records) {
  const where = `${r.package}@${r.version}`;
  // GUARD 3 — the record must entail its own verdict.
  if (r.mode === 'ladder') {
    const d = derive(r.cells || {});
    if (d !== r.verdict) {
      // A verdict the cells cannot reach is either a harness bug or a corrupted record.
      // Both are reasons to stop, not to guess.
      if (!['CONTROL-NOOP', 'UNMEASURABLE', 'ERROR'].includes(r.verdict)) {
        rejected.push(`${where}: verdict ${r.verdict} but cells derive ${d ?? 'nothing'} (${JSON.stringify(r.cells)})`);
      }
      continue;
    }
  }

  // WITHDRAW A SUPERSEDED WHOLE-DISK GRANT. Placed after the entailment guard so only a
  // record whose cells support its verdict can withdraw anything, and before the
  // `no catalog entry` skip below, because the commonest narrowing — NEEDS-NOTHING —
  // carries no grant at all and would otherwise never reach the existing entry.
  // Detected unconditionally and WITHDRAWN only under the flag, so a dry run names every
  // grant the measurement supersedes without touching the file.
  if (MEASURED.has(r.verdict)) {
    const i = (catalog.packageGrants || []).findIndex((e) => e.package === r.package && e.fullDisk === true);
    if (i >= 0) {
      if (!String(catalog.packageGrants[i].observed || '').includes(MARK)) {
        skipped.push(`${where}: holds a human-authored fullDisk grant, left alone`);
      } else {
        superseded.push(`- fullDisk ${r.package} — re-measured as ${r.verdict} on ${r.platform} (${r.host})`);
        if (SUPERSEDE) { catalog.packageGrants.splice(i, 1); granted.project.delete(r.package); }
      }
    }
  }

  if (r.verdict === 'GRANT-REDUNDANT') {
    if (!granted.net.has(r.package) && !granted.project.has(r.package)) { skipped.push(`${where}: already ungranted`); continue; }
    proposedRemovals.push(r);
    continue;
  }
  if (!r.grant || r.grant.remove) { skipped.push(`${where}: ${r.verdict} implies no catalog entry`); continue; }

  // GUARD 2a — a deliberate refusal outranks a measurement. The package was refused with
  // recorded reasoning; that a script needs the capability is what the refusal already
  // assumed, not new information.
  if (refusedPkgs.has(r.package)) { rejected.push(`${where}: listed in notGranted.packages — a refusal is not overridable by measurement`); continue; }

  // GUARD 5 — the terminal rung is the one verdict a broken host can manufacture.
  if (r.verdict === 'NEEDS-FULL-DISK') {
    const objection = fullDiskObjection(r);
    if (objection) { uncorroborated.push(`${where}: ${objection}`); continue; }
  }

  const observed = `${MARK} ${r.verdict} on ${r.platform}, ${String(r.measured_at).slice(0, 10)}. `
    + `Cells jail_off=${r.cells.jail_off} no_grants=${r.cells.no_grants} network=${r.cells.network} project_write=${r.cells.project_write} both=${r.cells.both}`
    + ('full_disk' in r.cells ? ` full_disk=${r.cells.full_disk}` : '') + ` (0=pass). `
    + (r.evidence?.failing_line ? `Ungranted failure: ${r.evidence.failing_line}. ` : '')
    + (r.evidence?.control_artifact ? `Unconfined artifact ${r.evidence.control_artifact.bytes} B / ${r.evidence.control_artifact.files} files. ` : '')
    + `nub ${r.nub_version} ${String(r.binary_sha256).slice(0, 12)}.`;

  if (r.grant.packageNetwork === 'full') {
    const existing = (catalog.packageNetwork.full || []).find((e) => e.package === r.package);
    if (existing) {
      // GUARD 1 + 2b — idempotence, and hands off anything a human wrote.
      if (!String(existing.observed || '').includes(MARK)) { skipped.push(`${where}: packageNetwork entry is human-authored, left alone`); }
      else if (existing.observed === observed) { skipped.push(`${where}: packageNetwork already current`); }
      else { existing.observed = observed; existing.platform = r.platform; added.push(`~ packageNetwork.full ${r.package} (refreshed)`); }
    } else if (granted.net.has(r.package)) {
      skipped.push(`${where}: already reaches the network via a networkHosts.fetchedBy entry`);
    } else {
      catalog.packageNetwork.full.push({ package: r.package, evidence: 'measured', observed, platform: r.platform });
      added.push(`+ packageNetwork.full ${r.package}`);
    }
  }

  if (r.grant.packageGrant) {
    const existing = (catalog.packageGrants || []).find((e) => e.package === r.package);
    const entry = {
      package: r.package,
      // `versionsObserved`, never `versions`: the latter is an ENFORCED semver range, and
      // the matrix measures one version rather than a boundary — writing it there would
      // pin every machine-generated grant to the single version it happened to run on.
      ...(r.version && r.version !== 'latest' ? { versionsObserved: r.version } : {}),
      ...r.grant.packageGrant,
      // The mechanism sentence has to name the rung the grant came from, because that is
      // the only thing a later reader can use to decide whether it is worth narrowing.
      // A full-disk row in particular must say WHY the narrower grants were insufficient
      // — its whole justification is that they were measured and failed.
      mechanism: r.verdict === 'NEEDS-FULL-DISK'
        ? 'Measured by the grant-configuration matrix as the TERMINAL rung: the install script fails with the jail on and no grants, still fails with unscoped project read/write/cwd AND egress together, and succeeds only with the whole filesystem. Every narrower grant the catalog can express was therefore run against it and was insufficient. This is a candidate for narrowing once the specific path it needs is known — it is the widest tier, taken because a package with no working grant would otherwise be an open investigation.'
        : 'Measured by the grant-configuration matrix: the install script fails with the jail on and no grants, and succeeds with unscoped project read/write/cwd. The grant is UNSCOPED because the matrix measures whether project access is needed, not which paths — scoping it is a follow-up that needs the specific paths observed.',
      evidence: 'measured',
      observed,
      platform: r.platform,
    };
    if (existing) {
      if (!String(existing.observed || '').includes(MARK)) { skipped.push(`${where}: packageGrant is human-authored, left alone`); }
      else if (JSON.stringify(existing) === JSON.stringify(entry)) { skipped.push(`${where}: packageGrant already current`); }
      else {
        // REPLACED WHOLESALE, NOT MERGED. `Object.assign` kept every capability key the new
        // entry does not mention, so refreshing a `fullDisk` package with a narrow verdict
        // left the whole disk in place and still printed `refreshed` — a widening the report
        // called a narrowing. An entry must describe ONE measurement, so a key the current
        // measurement does not imply has no business surviving it.
        catalog.packageGrants[catalog.packageGrants.indexOf(existing)] = entry;
        added.push(`~ packageGrants ${r.package} (replaced)`);
      }
    } else {
      catalog.packageGrants.push(entry);
      added.push(`+ packageGrants ${r.package}`);
    }
  }
}

// ── report ────────────────────────────────────────────────────────────────────
const say = (title, xs) => { if (xs.length) { console.log(`\n${title} (${xs.length})`); for (const x of xs) console.log(`  ${x}`); } };
say('CHANGES', added);
say(`SUPERSEDED FULL-DISK GRANTS — ${SUPERSEDE ? 'withdrawn' : 'NOT withdrawn; pass --supersede-fulldisk'}`, superseded);
say('REJECTED — not applied', rejected);
say('NEEDS-FULL-DISK:UNCORROBORATED — not applied, needs a human look', uncorroborated);
say(`PROPOSED REMOVALS — ${REMOVE ? 'applying' : 'NOT applied; pass --remove-redundant'}`, proposedRemovals.map((r) => `- ${r.package}@${r.version}: passes ungranted (${r.evidence?.detail || ''})`));
if (process.env.VERBOSE) say('skipped', skipped);
else console.log(`\nskipped: ${skipped.length} (VERBOSE=1 to list)`);

if (REMOVE) {
  for (const r of proposedRemovals) {
    catalog.packageNetwork.full = (catalog.packageNetwork.full || []).filter((e) => e.package !== r.package);
    for (const h of catalog.networkHosts || []) if (Array.isArray(h.fetchedBy)) h.fetchedBy = h.fetchedBy.filter((n) => n !== r.package);
    catalog.packageGrants = (catalog.packageGrants || []).filter((e) => e.package !== r.package);
    added.push(`- removed all grants for ${r.package}`);
  }
}

// A withdrawal counts as a change on its own: the commonest narrowing is NEEDS-NOTHING,
// which adds nothing at all, so keying the write on `added` alone would splice the grant
// out of the in-memory catalog and then decline to save it.
if (SUPERSEDE) added.push(...superseded);
const changed = added.length > 0;
if (!APPLY) { console.log(`\nDRY RUN — ${changed ? `${added.length} change(s) would be written` : 'nothing to change'}. Pass --apply to write ${CATALOG}`); process.exit(0); }
if (!changed) { console.log('\nnothing to write — catalog already current (idempotent)'); process.exit(0); }

// Sorted so the file's order is a function of its contents rather than of the order runs
// happened to finish in; otherwise every ingest produces a diff that is mostly movement.
catalog.packageNetwork.full.sort((a, b) => a.package.localeCompare(b.package));
catalog.packageGrants.sort((a, b) => a.package.localeCompare(b.package));
fs.writeFileSync(CATALOG, JSON.stringify(catalog, null, 2) + '\n');
console.log(`\nwrote ${CATALOG} — ${added.length} change(s). Rebuild to compile them in; \`cargo build\` re-runs the catalog validator.`);

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
// REMOVALS ARE PROPOSED, NEVER APPLIED. A minimality record saying a shipped grant is
// redundant is the one direction where being wrong BREAKS a package, which is the failure
// mode the jail cares most about. So `--apply` adds; removals print as a proposal and
// need `--remove-redundant` said out loud on top.
//
//   node apply-matrix.mjs <records.ndjson> [more.ndjson ...]        # dry run, the default
//   node apply-matrix.mjs <records.ndjson> --apply
//   node apply-matrix.mjs <records.ndjson> --apply --remove-redundant
//   node apply-matrix.mjs ... --catalog <path>
import fs from 'node:fs';
import path from 'node:path';

const argv = process.argv.slice(2);
const arg = (n, d) => { const i = argv.indexOf(n); return i >= 0 ? argv[i + 1] : d; };
const APPLY = argv.includes('--apply');
const REMOVE = argv.includes('--remove-redundant');
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
  if (c.both === 1) return 'FAILS-AT-BOTH';
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

const catalog = JSON.parse(fs.readFileSync(CATALOG, 'utf8'));
const refusedPkgs = new Set((catalog.notGranted?.packages || []).map((e) => e.package));
const granted = {
  net: new Set([
    ...(catalog.packageNetwork.full || []).map((e) => e.package),
    ...(catalog.networkHosts || []).flatMap((h) => h.fetchedBy || []),
  ]),
  project: new Set((catalog.packageGrants || []).map((e) => e.package)),
};

const added = [], skipped = [], rejected = [], proposedRemovals = [];

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

  const observed = `${MARK} ${r.verdict} on ${r.platform}, ${String(r.measured_at).slice(0, 10)}. `
    + `Cells jail_off=${r.cells.jail_off} no_grants=${r.cells.no_grants} network=${r.cells.network} project_write=${r.cells.project_write} both=${r.cells.both} (0=pass). `
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
      mechanism: 'Measured by the grant-configuration matrix: the install script fails with the jail on and no grants, and succeeds with unscoped project read/write/cwd. The grant is UNSCOPED because the matrix measures whether project access is needed, not which paths — scoping it is a follow-up that needs the specific paths observed.',
      evidence: 'measured',
      observed,
      platform: r.platform,
    };
    if (existing) {
      if (!String(existing.observed || '').includes(MARK)) { skipped.push(`${where}: packageGrant is human-authored, left alone`); }
      else if (JSON.stringify(existing) === JSON.stringify(entry)) { skipped.push(`${where}: packageGrant already current`); }
      else { Object.assign(existing, entry); added.push(`~ packageGrants ${r.package} (refreshed)`); }
    } else {
      catalog.packageGrants.push(entry);
      added.push(`+ packageGrants ${r.package}`);
    }
  }
}

// ── report ────────────────────────────────────────────────────────────────────
const say = (title, xs) => { if (xs.length) { console.log(`\n${title} (${xs.length})`); for (const x of xs) console.log(`  ${x}`); } };
say('CHANGES', added);
say('REJECTED — not applied', rejected);
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

const changed = added.length > 0;
if (!APPLY) { console.log(`\nDRY RUN — ${changed ? `${added.length} change(s) would be written` : 'nothing to change'}. Pass --apply to write ${CATALOG}`); process.exit(0); }
if (!changed) { console.log('\nnothing to write — catalog already current (idempotent)'); process.exit(0); }

// Sorted so the file's order is a function of its contents rather than of the order runs
// happened to finish in; otherwise every ingest produces a diff that is mostly movement.
catalog.packageNetwork.full.sort((a, b) => a.package.localeCompare(b.package));
catalog.packageGrants.sort((a, b) => a.package.localeCompare(b.package));
fs.writeFileSync(CATALOG, JSON.stringify(catalog, null, 2) + '\n');
console.log(`\nwrote ${CATALOG} — ${added.length} change(s). Rebuild to compile them in; \`cargo build\` re-runs the catalog validator.`);

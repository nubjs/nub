// How many of a project's install-script dependencies has nobody measured?
//
// ⛔⛔ THIS IS THE NUMBER THAT DECIDES WHETHER DEFAULT-ON IS READY, and it is not the catalog's size.
// The bar is per-INSTALL success, and an install succeeds only if EVERY one of its install-script
// dependencies works under the grant it gets. An uncatalogued package gets the baseline, which is
// sufficient for 96.4% of measured packages — so the per-install rate is roughly that rate raised to
// the number of UNMEASURED install-script deps. At 96.4%, ten of them compounds to ~69%, which is
// nowhere near the 99.9% bar. Driving THIS count toward zero is what meets the bar; widening the
// baseline is not, because each widening is a capability handed to every unmeasured package.
//
// ⛔⛔ UNMEASURED, NOT UNCATALOGUED — AND THE DIFFERENCE IS WHY THIS COUNT CAN REACH ZERO AT ALL.
// This file used to count "absent from the catalog" and call that the risk. It is not: a package
// whose scripts pass at the DEFAULT grant wants no entry, because an empty cell is strictly TIGHTER
// than no entry, so writing one would confine it further than it is today (the invariant is pinned
// by `no_cell_denies_everything_unless_its_package_runs_no_lifecycle_hook`). Under the old reading
// the count could only be driven down by UNDER-GRANTING — measured once as 67 packages' worth. So
// the measurement lives in `results/baseline-coverage.tsv` instead, and a dep is at risk only when
// it is in neither that record nor the catalog. Same for the ~20 packages holding a v1
// `CuratedGrant`, which a v2-keyed check cannot see and would otherwise report as unmeasured.
//
// ⛔ IT READS NUB'S OWN REPORT RATHER THAN RE-DETECTING SCRIPTS. nub already prints
// `WARN_NUB_IGNORED_BUILD_SCRIPTS … packages=[…]` naming exactly the dependencies whose lifecycle
// scripts it declined to run pending approval. That list IS the population under test. Walking
// node_modules for `scripts.install`/`postinstall` myself would be a second implementation of "which
// packages have build scripts", and the two would disagree — on optional deps that were not installed,
// on the platform-specific packages npm's os/cpu filters skipped, and on nub's own approval rules. When
// they disagree the local copy is the one that is wrong, and nothing would say so.
//
//   usage: node uncatalogued.mjs --install-log <path> --catalog <path> [--json]
//   exit 0 always: this REPORTS a number, and the caller decides what it means.
import fs from 'node:fs';
import path from 'node:path';

/** The prefix of the line nub prints when it RUNS a default-trusted package's scripts. */
const TRUSTED_PREFIX = 'defaultTrust: running build scripts for ';

/** Package specs whose lifecycle scripts nub either ran or declined to run, from one install's output.
 *
 * ⛔⛔ TWO LINES, NOT ONE, AND READING ONLY THE FIRST REPORTS ZERO FOR A PROJECT FULL OF THEM. nub does
 * not merely ignore build scripts pending approval; it also has a DEFAULT-TRUST list it runs without
 * asking, and those packages are announced on a completely different line. Measured on the first real
 * run of this harness: a project depending on esbuild and sharp reported `install-script deps 0`, because
 * both are default-trusted and neither appears in the ignored-scripts warning. Both populations are
 * jailed, so both belong in this count.
 *
 * ⛔ AND THE TRUSTED LINE CARRIES NO MACHINE-READABLE LIST. `pm_engine::log::format_line` deliberately
 * strips `code`, `count` and `packages` from that disclosure and rewrites the message, so the specs exist
 * only as prose inside it. That is why this parses the sentence rather than a JSON field — verified
 * against real `nub install` output, not against an assumption about it.
 */
export function installScriptDeps(log) {
  // The line is `… packages=["a@1.0.0","@scope/b@2.0.0"]`. Parsed as JSON rather than split on commas,
  // because a scoped name contains no comma but a version RANGE can, and a hand-rolled split would
  // quietly halve the count on exactly the packages most likely to be interesting.
  //
  // ⛔ THE CHARACTER CLASS EXCLUDES A NEWLINE, AND THAT IS NOT TIDINESS. With `[^\]]*` an unterminated
  // `packages=[` — a truncated line, an interleaved write from a parallel install — consumes forward
  // across the newline and swallows the NEXT valid list into one malformed match, which is then
  // skipped. The result is an undercount, silently, and undercounting is the direction that flatters
  // the coverage number. The list is always on one line, so refusing to cross one costs nothing.
  const out = new Set();
  for (const m of log.matchAll(/packages=(\[[^\]\n]*\])/g)) {
    let specs;
    try { specs = JSON.parse(m[1]); } catch { continue; }
    for (const s of specs) if (typeof s === 'string') out.add(s);
  }
  // The default-trusted packages, whose scripts RAN. Comma-separated in prose, and safe to split that
  // way here because the specs on this line are RESOLVED exact versions — a comma can appear in a
  // version RANGE but never in a resolved one.
  for (const line of log.split('\n')) {
    const at = line.indexOf(TRUSTED_PREFIX);
    if (at < 0) continue;
    for (const spec of line.slice(at + TRUSTED_PREFIX.length).split(',')) {
      const trimmed = spec.trim();
      if (trimmed) out.add(trimmed);
    }
  }
  return [...out];
}

/** `name@version` → the name, handling scopes. `@scope/n@1.0.0` → `@scope/n`. */
export function specName(spec) {
  const at = spec.lastIndexOf('@');
  return at > 0 ? spec.slice(0, at) : spec;
}

/** Rows of the coverage record, keyed by package name. Absent or unreadable file -> empty set. */
export function loadCoverage(path) {
  const names = new Set();
  let text;
  try {
    text = fs.readFileSync(path, 'utf8');
  } catch {
    return names; // A missing record means "nothing is known measured", which OVER-reports risk.
  }
  for (const line of text.split('\n')) {
    if (!line || line.startsWith('#')) continue;
    const [name, , , reason, sha] = line.split('\t');
    // ⛔ A ROW WITHOUT PROVENANCE IS NOT EVIDENCE. Every row must name a recognised reason and the
    // commit it was measured at, or it is dropped -- otherwise a hand-added name silently converts
    // an unmeasured package into a measured one, which is exactly the false all-clear this count
    // exists to prevent.
    if (!name || !sha?.trim()) continue;
    if (reason !== 'baseline-measured' && reason !== 'v1-curated') continue;
    names.add(name);
  }
  return names;
}

/**
 * Split the deps three ways against the catalog and the coverage record.
 *
 * ⛔ ABSENCE FROM THE CATALOG IS NOT A GAP. A package whose lifecycle scripts pass at the DEFAULT
 * grant wants no catalog entry -- an empty cell is STRICTLY TIGHTER than no entry (absence takes
 * `baseline_caps()`, an empty cell takes nothing), so writing one would CONFINE it further than it
 * is today. That is why the risk count cannot be driven to zero by adding entries, and why the
 * measurement lives in a separate record instead.
 *
 * `catalogued` — an entry exists, so a grant was measured and recorded.
 * `measured`   — no entry, and none is wanted: recorded as passing at the baseline, or covered by a
 *                v1 `CuratedGrant` that this v2-keyed check cannot see.
 * `uncatalogued` — genuinely nobody has run it. THIS is the number that gates default-on.
 */
export function partition(specs, catalog, coverage = new Set()) {
  const known = new Set(Object.keys(catalog?.packages ?? {}));
  const catalogued = [];
  const measured = [];
  const uncatalogued = [];
  for (const spec of specs) {
    const name = specName(spec);
    if (known.has(name)) catalogued.push(spec);
    else if (coverage.has(name)) measured.push(spec);
    else uncatalogued.push(spec);
  }
  return { catalogued, measured, uncatalogued };
}

if (process.argv[1] && import.meta.url === (await import('node:url')).pathToFileURL(process.argv[1]).href) {
  const argv = process.argv.slice(2);
  const arg = (k) => (argv.includes(k) ? argv[argv.indexOf(k) + 1] : undefined);
  const [logPath, catalogPath] = [arg('--install-log'), arg('--catalog')];
  if (!logPath || !catalogPath) {
    console.error('usage: uncatalogued.mjs --install-log <path> --catalog <path> [--coverage <tsv>] [--json]');
    process.exit(2);
  }
  const here = path.dirname((await import('node:url')).fileURLToPath(import.meta.url));
  const coveragePath = arg('--coverage') ?? path.join(here, 'results', 'baseline-coverage.tsv');
  const log = fs.readFileSync(logPath, 'utf8');
  const catalog = JSON.parse(fs.readFileSync(catalogPath, 'utf8'));
  const coverage = loadCoverage(coveragePath);
  const specs = installScriptDeps(log);
  const { catalogued, measured, uncatalogued } = partition(specs, catalog, coverage);
  if (argv.includes('--json')) {
    console.log(JSON.stringify({ total: specs.length, coverageRows: coverage.size, catalogued, measured, uncatalogued }, null, 2));
  } else {
    // ⛔ THE FIRST LINE IS PARSED BY `sweep.sh`, which greps it for `UNCATALOGUED <n>` and reads only
    // ONE line. Keep that token on this line and keep the line singular.
    const src = coverage.size ? `measured ${measured.length}` : 'measured 0 (NO COVERAGE RECORD)';
    console.log(`install-script deps ${specs.length}   catalogued ${catalogued.length}   ${src}   UNCATALOGUED ${uncatalogued.length}`);
    for (const u of uncatalogued) console.log(`  UNCATALOGUED ${u}`);
  }
}

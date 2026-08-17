// How many of a project's install-script dependencies has nobody measured?
//
// ⛔⛔ THIS IS THE NUMBER THAT DECIDES WHETHER DEFAULT-ON IS READY, and it is not the catalog's size.
// The bar is per-INSTALL success, and an install succeeds only if EVERY one of its install-script
// dependencies works under the grant it gets. An uncatalogued package gets the baseline, which is
// sufficient for 96.4% of measured packages — so the per-install rate is roughly that rate raised to
// the number of uncatalogued install-script deps. At 96.4%, ten of them compounds to ~69%, which is
// nowhere near the 99.9% bar. Driving THIS count toward zero is what meets the bar; widening the
// baseline is not, because each widening is a capability handed to every unmeasured package.
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

/** Split the deps into catalogued and not, against a real catalog document. */
export function partition(specs, catalog) {
  const known = new Set(Object.keys(catalog?.packages ?? {}));
  const catalogued = [];
  const uncatalogued = [];
  for (const spec of specs) (known.has(specName(spec)) ? catalogued : uncatalogued).push(spec);
  return { catalogued, uncatalogued };
}

if (process.argv[1] && import.meta.url === (await import('node:url')).pathToFileURL(process.argv[1]).href) {
  const argv = process.argv.slice(2);
  const arg = (k) => (argv.includes(k) ? argv[argv.indexOf(k) + 1] : undefined);
  const [logPath, catalogPath] = [arg('--install-log'), arg('--catalog')];
  if (!logPath || !catalogPath) {
    console.error('usage: uncatalogued.mjs --install-log <path> --catalog <path> [--json]');
    process.exit(2);
  }
  const log = fs.readFileSync(logPath, 'utf8');
  const catalog = JSON.parse(fs.readFileSync(catalogPath, 'utf8'));
  const specs = installScriptDeps(log);
  const { catalogued, uncatalogued } = partition(specs, catalog);
  if (argv.includes('--json')) {
    console.log(JSON.stringify({ total: specs.length, catalogued, uncatalogued }, null, 2));
  } else {
    console.log(`install-script deps ${specs.length}   catalogued ${catalogued.length}   UNCATALOGUED ${uncatalogued.length}`);
    for (const u of uncatalogued) console.log(`  UNCATALOGUED ${u}`);
  }
}

#!/usr/bin/env node
// ONE FILE THAT TELLS YOU WHAT HAPPENED to a package-version probe: the verdict, what nub did,
// what each reference PM did, the first real error from every arm, and ABSOLUTE PATHS to every log
// on disk so the next question is one `Read` away.
//
// WHY THIS EXISTS. Diagnosing a single package meant: open results.json, guess which of five
// `*.log` files mattered, grep it for an error, remember that a node stack trace ends with the
// version banner so the tail is useless, then separately discover that npm buffers script output
// so its log says nothing, then hunt npm's own debug log under a fixture HOME that no longer
// exists. Every one of those steps was rediscovered from scratch each time, and each was a place
// to reach a confident wrong answer. This collapses them into a file you read top to bottom.
//
//   node digest.mjs <pkg>@<version> [--runs <dir>]   -> writes digest.md beside results.json
//   node digest.mjs --all [--runs <dir>]             -> every record with a non-MINIMUM verdict
import fs from 'node:fs';
import path from 'node:path';

const argv = process.argv.slice(2);
const runs = argv.includes('--runs')
  ? argv[argv.indexOf('--runs') + 1]
  : path.join(import.meta.dirname, 'results', 'runs');

/** The FIRST real error line, never the tail: a node stack trace ends with the version banner and
 *  an install log ends with a summary, so tailing reports the least informative line in the file.
 *  This is the single habit that cost five successive wrong diagnoses of one package. */
const firstError = (text, max = 4) => {
  const out = [];
  for (const line of (text || '').split('\n')) {
    if (out.length >= max) break;
    if (!/\b(error|ERR!|fatal|failed|denied|EACCES|EPERM|ENOENT|not ok)\b/i.test(line)) continue;
    if (/^\s*(npm )?(warn|notice|info)\b/i.test(line)) continue;
    if (/^\s*$/.test(line)) continue;
    out.push(line.trim().slice(0, 220));
  }
  return out;
};

const read = (p) => { try { return fs.readFileSync(p, 'utf8'); } catch { return ''; } };
const size = (p) => { try { return fs.statSync(p).size; } catch { return 0; } };

function digestFor(dir) {
  const rec = JSON.parse(read(path.join(dir, 'results.json')) || '{}');
  if (!rec.pkg) return null;
  const L = [];
  const abs = (f) => path.resolve(dir, f);

  L.push(`# ${rec.pkg}@${rec.version} — ${rec.verdict}`);
  L.push('');
  L.push(`- record: \`${abs('results.json')}\``);
  if (rec.provenance) {
    const p = rec.provenance;
    L.push(`- measured: ${p.at ?? '?'} on ${p.platform ?? '?'}, node ${p.node ?? '?'}`);
    L.push(`- harness \`${(p.harnessSha256 ?? '?').slice(0, 8)}\`, nub \`${(p.nubSha256 ?? '?').slice(0, 8)}\` (${p.nubVersion ?? '?'})`);
  }
  L.push('');

  // THE VERDICT, and what it is allowed to mean. Each of these has been misread at least once.
  L.push('## Verdict');
  L.push('');
  const meaning = {
    MINIMUM: 'measured — `state` is the minimal grant that reproduces the control',
    'BROKEN-IN-ENVIRONMENT': 'a reference PM fails too — grant nothing, and suspect this host',
    'BROKEN-EVEN-WITH-EVERYTHING': 'a reference PM SUCCEEDS where nub fails — a nub defect, never a grant gap',
    'HARNESS-ERROR': 'the catalog override did not engage — wrong binary, or a catalog shape the parser rejects',
    'HARNESS-CRASH': 'the probe itself died — read harness-stderr.log; never a package fact',
    'HARNESS-TIMEOUT': 'the probe exceeded its cap; never a package fact',
    'NO-STATE-PASSED': 'no cell reproduced the control — SUSPECT THE ORACLE before the package',
  }[rec.verdict] ?? '';
  L.push(`**${rec.verdict}** — ${meaning}`);
  if (rec.state) L.push(`\n- grant: \`${rec.state}\` (cost ${rec.cost}), ${(rec.cells ?? []).length} cells`);
  if (rec.writePaths?.length) L.push(`- writePaths: ${rec.writePaths.map((w) => `\`${w}\``).join(', ')}`);
  if (rec.why) L.push(`- why: ${rec.why}`);
  L.push('');

  // THE CONTROL'S SHAPE. A fixture that stopped installing makes every package look free, and the
  // only tell is here — not in the verdict, which stays MINIMUM.
  const c = rec.control ?? {};
  L.push('## Did the fixture actually install?');
  L.push('');
  L.push(`- control: rc ${c.rc}, **${c.fileCount ?? '?'} files**, materialized \`${c.materialized}\``);
  L.push(`- declares an install script: \`${rec.declaresInstallScript}\` (read from the TARBALL manifest, not the packument)`);
  if (rec.projectAxisConclusive === false && rec.declaresInstallScript) {
    L.push('- ⚠ declares a script but was NOT materialized — this verdict says nothing about the project axis');
  }
  if ((c.fileCount ?? 0) < 100) L.push('- ⛔ SUSPICIOUSLY FEW FILES — suspect the fixture, not the package');
  L.push('');

  // EVERY ARM'S FIRST ERROR, side by side. The comparison the verdict is built on.
  L.push('## First error, per arm');
  L.push('');
  const logs = fs.existsSync(dir) ? fs.readdirSync(dir).filter((f) => f.endsWith('.log')) : [];
  const arms = [];
  for (const f of logs.sort()) {
    const errs = firstError(read(path.join(dir, f)));
    arms.push({ arm: f.replace(/\.log$/, ''), file: abs(f), bytes: size(path.join(dir, f)), errs });
  }
  if (!arms.length) L.push('_no logs on disk — the probe died before any cell ran_');
  for (const a of arms) {
    L.push(`### \`${a.arm}\`  (${a.bytes} bytes)`);
    L.push(`\`${a.file}\``);
    if (a.errs.length) { L.push('```'); a.errs.forEach((e) => L.push(e)); L.push('```'); }
    else L.push('_no error lines_');
    L.push('');
  }

  // THE ORACLES. What each reference PM did, and the caveats that make a naive reading wrong.
  L.push('## Reference PMs');
  L.push('');
  if (rec.npmReference) {
    L.push(`- **npm**: rc \`${rec.npmReference.rc}\`, signature \`${rec.npmReference.signature ?? 'none'}\``);
    L.push('  - ⚠ npm 11.17+ SKIPS a dependency\'s install script unless covered by `allowScripts`');
    L.push('    (`arborist/lib/arborist/rebuild.js`), so rc 0 may mean NO SCRIPT RAN.');
    L.push('  - ⚠ npm BUFFERS script output unless `--foreground-scripts`, so its log shows npm\'s');
    L.push('    wrapper error rather than the script\'s own. Its full debug log is `$HOME/.npm/_logs/*-debug-0.log`.');
  }
  if (rec.pnpmReference) {
    L.push(`- **pnpm**: rc \`${rec.pnpmReference.rc}\`, signature \`${rec.pnpmReference.signature ?? 'none'}\``);
    if (rec.pnpmReference.lifecycle?.length) {
      L.push('  - lifecycle events:');
      for (const e of rec.pnpmReference.lifecycle.slice(0, 6)) {
        L.push(`    - \`${e.stage}\` exit \`${e.exitCode ?? '—'}\`${e.line ? `: ${String(e.line).slice(0, 120)}` : ''}`);
      }
    }
    L.push('  - ⚠ pnpm gates scripts behind `onlyBuiltDependencies`; `--reporter=ndjson` gives per-event records.');
  }
  if (rec.signaturesMatched !== undefined) L.push(`- signatures matched: \`${rec.signaturesMatched}\``);
  if (rec.standing) {
    const s = rec.standing;
    L.push(`- standing: published ${s.published ?? '?'}, ${s.ageDays ?? '?'}d old, ${s.weeklyDownloads ?? '?'}/wk, latest \`${s.latestVersion ?? '?'}\``);
  }
  if (rec.needsInvestigation) L.push(`- ⛔ needsInvestigation: ${rec.investigationReason ?? 'recent and popular but failing here — suspect OUR environment'}`);
  L.push('');

  // THE WALK. Where the search spent its cells, so a wide grant can be checked at a glance.
  if ((rec.cells ?? []).length) {
    L.push('## The walk');
    L.push('');
    L.push('| # | state | cost | pass | rc | files |');
    L.push('|---|---|---|---|---|---|');
    for (const cell of rec.cells) {
      L.push(`| ${cell.index ?? 'ctl'} | \`${cell.state}\` | ${cell.cost ?? '—'} | ${cell.pass ? '✓' : '✗'} | ${cell.rc} | ${cell.files} |`);
    }
    L.push('');
  }

  if (rec.pathsBlockedWithoutGrant?.length) {
    L.push('## Paths denied at the base profile');
    L.push('');
    L.push(`${rec.pathsBlockedWithoutGrantCount ?? rec.pathsBlockedWithoutGrant.length} total; first 10:`);
    L.push('```');
    rec.pathsBlockedWithoutGrant.slice(0, 10).forEach((p) => L.push(p));
    L.push('```');
    L.push('');
  }

  return L.join('\n');
}

const targets = [];
if (argv[0] && !argv[0].startsWith('--')) {
  const [pkg, version] = [argv[0].slice(0, argv[0].lastIndexOf('@')), argv[0].slice(argv[0].lastIndexOf('@') + 1)];
  const walk = (d) => {
    for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      const p = path.join(d, e.name);
      if (e.isDirectory()) walk(p);
      else if (e.name === 'results.json') {
        try {
          const r = JSON.parse(fs.readFileSync(p, 'utf8'));
          if (r.pkg === pkg && String(r.version) === version) targets.push(d);
        } catch {}
      }
    }
  };
  try { walk(runs); } catch {}
} else {
  const walk = (d) => {
    for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      const p = path.join(d, e.name);
      if (e.isDirectory()) walk(p);
      else if (e.name === 'results.json') {
        try {
          const r = JSON.parse(fs.readFileSync(p, 'utf8'));
          if (!argv.includes('--all') || r.verdict !== 'MINIMUM') targets.push(d);
        } catch {}
      }
    }
  };
  try { walk(runs); } catch {}
}

if (!targets.length) { console.error('no matching records under', runs); process.exit(1); }
for (const d of targets) {
  const md = digestFor(d);
  if (!md) continue;
  const out = path.join(d, 'digest.md');
  fs.writeFileSync(out, `${md}\n`);
  console.log(out);
}

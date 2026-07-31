#!/usr/bin/env node
// errsig.mjs — separate FS cost from NETWORK cost, per failure, from the log.
//
// WHY THIS EXISTS. The clean way to split the two is an arm that confines the
// filesystem while leaving the network open (the matrix's A1). That arm is NOT
// EXPRESSIBLE on this binary: the build-jail net axis is a compile-time constant
// (`build_jail_net()` returns `["$downloads"]`, or `false` on Windows) with no
// config, env, or flag route, so producing it needs a patched build — and this
// lane is read-only on nub's source. The honest substitute is to classify each
// observed failure by its ERRNO SHAPE, which distinguishes the two mechanisms
// directly, and to corroborate it against the platform pair (Linux denies the
// socket outright; macOS admits four curated hosts through the proxy).
//
// The classifier is deliberately CONSERVATIVE: anything it cannot place lands in
// `unclassified` and is reported as such. A miscategorised denial would silently
// move cost between the two answers this study exists to produce.

import fs from 'node:fs';
import { parseWindows, ownerAt } from './windows.mjs';

const [logF, manifestF, verdictsF, outF] = process.argv.slice(2);
const log = fs.readFileSync(logF, 'utf8');
const lines = log.split('\n');
const verdicts = JSON.parse(fs.readFileSync(verdictsF, 'utf8'));
const pkgs = fs
  .readFileSync(manifestF, 'utf8')
  .split('\n')
  .filter((l) => l.trim() && !l.startsWith('#'))
  .map((l) => l.split('\t')[0]);

// Ordered most-specific first; the first match wins, so a line naming both a
// socket and a path is scored as network (the socket is the proximate refusal).
const SIGS = [
  // ── network ────────────────────────────────────────────────────────────────
  // Linux: seccomp refuses socket(2) itself, so the failure surfaces before any
  // address is resolved. Node renders it as an EPERM/EACCES on connect/lookup.
  ['net', /\b(getaddrinfo|connect|socket|request to)\b[^\n]*\b(EPERM|EACCES|EAI_AGAIN|ENOTFOUND|ECONNREFUSED|ENETUNREACH)\b/i],
  ['net', /EPROTO|ERR_SOCKET|ERR_NETWORK|ECONNRESET[^\n]*(fetch|download|https?:)/i],
  // macOS: egress is pinned to the proxy port, so a non-allowlisted host is
  // refused BY THE PROXY and shows up as a tunnel/CONNECT failure, not an errno.
  ['net', /tunneling socket could not be established|proxy|CONNECT [^\s]+ (403|407|502)|407 Proxy/i],
  ['net', /\b(ETIMEDOUT|ESOCKETTIMEDOUT)\b/],
  ['net', /(failed to (download|fetch)|download failed|unable to (download|fetch|get))/i],
  // ── filesystem ─────────────────────────────────────────────────────────────
  // Landlock renders a denial as EACCES on a VISIBLE path; Seatbelt as EPERM.
  // EROFS is the bubblewrap shape and should not appear on the build-jail path.
  ['fs', /\b(EACCES|EPERM|EROFS)\b[^\n]*(open|mkdir|write|unlink|rename|scandir|chmod|access|stat|copyfile|symlink|\/)/i],
  ['fs', /(permission denied|Operation not permitted|read-only file system)/i],
  // The Landlock errno-shape compat cost: an optional file that bubblewrap makes
  // ENOENT is EACCES here, and the ecosystem idiom only tolerates ENOENT.
  ['fs', /\bENOENT\b[^\n]*(\.npmrc|\.config|no such file)/i],
  // ── neither ────────────────────────────────────────────────────────────────
  ['build', /gyp ERR!|node-gyp|make: \*\*\*|clang: error|fatal error:|C\+\+ compiler/i],
  ['engine', /Unsupported engine|EBADENGINE|requires Node|Unsupported platform/i],
];

function classify(line) {
  for (const [kind, re] of SIGS) if (re.test(line)) return kind;
  return null;
}

// ATTRIBUTION IS THE ENCLOSING WINDOW FIRST, because that is a boundary the
// runner PARSED rather than one this file infers. The path-shaped rules below
// were the only mechanism for a long time and they systematically lose the
// commonest lifecycle failure there is: a bare message with no path in it.
// `getaddrinfo ENOTFOUND github.com` names no package, so it fell through the
// cell match, through the bare-name match, through a six-line lookback, and into
// a `(shard-level)` bucket that joins to no row — measured on `default3-PROD`,
// 61 of 291 classified lines, leaving 7 of that shard's 10 break rows with no
// signature at all while a hand triage using these same markers attributed 67 of
// 67. The window is authoritative when it exists; the path rules stay as the
// fallback for a `--all` run, which emits no markers, and for install-phase
// lines that sit outside every window.
const segs = parseWindows(lines);

function attributeLine(line) {
  const cell = line.match(/store\/(.+?)@[^/@]+?(?:_[^/]*_)?-[0-9a-f]{8,}\//);
  if (cell) return cell[1].replace(/\+/g, '/');
  for (const p of pkgs) {
    if (line.includes(`/${p}/`) || line.includes(`node_modules/${p}`)) return p;
  }
  return null;
}

const hits = [];
for (let i = 0; i < lines.length; i++) {
  const kind = classify(lines[i]);
  if (!kind) continue;
  hits.push({
    kind,
    pkg:
      ownerAt(segs, i)
      ?? attributeLine(lines[i])
      ?? attributeLine(lines.slice(Math.max(0, i - 6), i).join('\n')),
    line: lines[i].trim().slice(0, 400),
  });
}

// A BREAK WITH NO SIGNATURE IS A BREAK NOBODY CAN TRIAGE, and the classifier
// above is deliberately narrow — it recognises errno shapes, not prose. Ten of
// the 67 breaks on the macOS sweep failed in words it does not match at all
// (`could not be installed: fetch failed`, `Could not connect to CDN`, `getwd:
// invalid argument`, `not a git repository`), so they carried nothing at all
// into triage.
//
// The fix is NOT to widen SIGS. Guessing whether `fetch failed` is a network
// denial or an application bug is exactly the miscategorisation this file's
// header refuses to make, and it would move cost between the two answers the
// study exists to produce. Instead the window's own most problem-shaped line is
// carried under an explicit `unclassified` kind: enough to triage by hand,
// labelled so it can never be counted as fs or net.
//
// Gated on the window having actually gone wrong, so a healthy package that
// merely prints the word "error" in its banner picks up nothing.
const PROBLEM = /\b(error|fail(ed|ure)?|cannot|could not|unable|denied|not permitted|fatal|invalid|refused|timed out)\b/i;
const verdictByPkg = new Map((verdicts.results || []).map((r) => [r.pkg, r.verdict]));
const classified = new Set(hits.filter((h) => h.pkg).map((h) => h.pkg));
for (const s of segs) {
  if (classified.has(s.pkg)) continue;
  if (s.rc === 0 && verdictByPkg.get(s.pkg) === 'DID-WORK-AND-SUCCEEDED') continue;
  const line = lines.slice(s.from, s.to).find((l) => PROBLEM.test(l) && !/^\s*(Approved \d+|warning: )/.test(l));
  if (!line) continue;
  hits.push({ kind: 'unclassified', pkg: s.pkg, line: line.trim().slice(0, 400) });
}

const byPkg = {};
for (const h of hits) {
  const k = h.pkg || '(shard-level)';
  byPkg[k] ??= { fs: 0, net: 0, build: 0, engine: 0, unclassified: 0, samples: [] };
  byPkg[k][h.kind]++;
  if (byPkg[k].samples.length < 4) byPkg[k].samples.push(`[${h.kind}] ${h.line}`);
}

const totals = hits.reduce((a, h) => ((a[h.kind] = (a[h.kind] || 0) + 1), a), {});
const out = {
  shard: verdicts.shard,
  arm: verdicts.arm,
  platform: process.env.PLATFORM,
  lever: process.env.LEVER,
  nonce: process.env.NONCE,
  // The gate that makes every verdict in this file admissible. `FAILED-*` means the
  // arm did not take effect and the run measured something other than what it says.
  arm_effect: process.env.ARM_EFFECT,
  optout_warnings: Number(process.env.WARN_COUNT || 0),
  // A bare-PATH `node-gyp` on this host resolves a stale global 3.8.0, so which one
  // actually ran is recorded rather than assumed.
  node_gyp_identity: process.env.GYP_ID || 'none-observed',
  rc_install: verdicts.rc_install,
  rc_script: verdicts.rc_script,
  // Scheduling facts travel with the report, because the aggregate — not the
  // runner — is where a break count is finally asserted, and a break asserted
  // over a truncated window is a measurement of which sibling failed first.
  scheduling_truncated: verdicts.scheduling_truncated ?? null,
  // Which mechanism produced that flag — so a reader can tell "no sibling failed
  // first" from "no sibling COULD have", which is a much stronger statement.
  per_package_windows: verdicts.per_package_windows ?? null,
  approved_jobs: verdicts.approved_jobs ?? null,
  failed_packages: verdicts.failed_packages ?? [],
  isolated: verdicts.isolated ?? false,
  manifest_rows: verdicts.manifest_rows ?? null,
  verdict_summary: verdicts.summary,
  failure_signature_totals: totals,
  // The residue, reported rather than buried: how much of the classified log
  // this run could NOT attach to a package. It is the direct measure of whether
  // window attribution is working, and it was ~21% before it existed.
  windows_parsed: segs.length,
  unattributed_failure_lines: hits.filter((h) => !h.pkg).length,
  failure_signatures_by_package: byPkg,
  verdicts: verdicts.results,
  unattributed: verdicts.unattributed,
  interaction_candidates: verdicts.interaction_candidates,
};
fs.writeFileSync(outF, JSON.stringify(out, null, 2));
console.log(
  JSON.stringify({
    shard: out.shard, arm: out.arm, platform: out.platform, lever: out.lever,
    arm_effect: out.arm_effect, node_gyp: out.node_gyp_identity,
    rc_install: out.rc_install, rc_script: out.rc_script,
    verdicts: out.verdict_summary, failures: totals,
  })
);

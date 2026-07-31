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

// Attribution: prefer an exact store-cell key (`name@version-hash`), which is a
// parse rather than an inference; fall back to a bare package name appearing as a
// path segment. A line naming no package is kept as `shard-level` — those are the
// ones a per-package rc would have resolved and this harness cannot.
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
    pkg: attributeLine(lines[i]) ?? attributeLine(lines.slice(Math.max(0, i - 6), i).join('\n')),
    line: lines[i].trim().slice(0, 400),
  });
}

const byPkg = {};
for (const h of hits) {
  const k = h.pkg || '(shard-level)';
  byPkg[k] ??= { fs: 0, net: 0, build: 0, engine: 0, samples: [] };
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

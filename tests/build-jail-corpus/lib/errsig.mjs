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

// ── WHAT THE PACKAGE ACTUALLY NEEDED — the study's real deliverable ───────────
// The survival percentage is not the point. The point is which network hosts and
// which filesystem paths must be carved out for the ecosystem to work under the
// jail. That list was not obtainable from this file, for two reasons now fixed: the
// per-package record kept only FOUR sample lines, so any package denied more than
// four things reported an arbitrary subset; and hosts and paths were never extracted
// at all, leaving a reader to eyeball prose out of the truncated sample.
//
// Both are extracted as deduplicated, uncapped SETS. A host or path that cannot be
// attributed still lands in the shard-level union below, so the carve-out list never
// shrinks because a line failed to name its owner.
const HOST_PATTERNS = [
  /\b(?:getaddrinfo|connect)\s+(?:EAI_AGAIN|ENOTFOUND|EPERM|EACCES|ECONNREFUSED|ENETUNREACH|ETIMEDOUT)\s+([A-Za-z0-9._-]+\.[A-Za-z]{2,})/g,
  /\brequest to https?:\/\/([A-Za-z0-9._-]+\.[A-Za-z]{2,})/g,
  /\bCONNECT\s+([A-Za-z0-9._-]+\.[A-Za-z]{2,})(?::\d+)?\b/g,
  /\b(?:GET|POST|HEAD)\s+https?:\/\/([A-Za-z0-9._-]+\.[A-Za-z]{2,})/g,
  /https?:\/\/([A-Za-z0-9._-]+\.[A-Za-z]{2,})[^\s]*[^\n]*?(?:failed|denied|403|407|502)/gi,
  /(?:fetching|downloading|download failed)[^\n]*?https?:\/\/([A-Za-z0-9._-]+\.[A-Za-z]{2,})/gi,
];
// Landlock renders a denial as EACCES on a visible path, Seatbelt as EPERM. Node's
// message shape puts the path in single quotes; the quoted-generic form is the
// fallback for tools that print their own wording.
const PATH_PATTERNS = [
  /\b(?:EACCES|EPERM|EROFS)[^\n]*?,\s*\w+\s+'([^']+)'/g,
  /\b(?:permission denied|Operation not permitted)[^\n]*?['"]([^'"]+)['"]/gi,
];

function extract(line, patterns) {
  const out = [];
  for (const re of patterns) {
    re.lastIndex = 0;
    let m;
    while ((m = re.exec(line)) !== null) if (m[1]) out.push(m[1]);
  }
  return out;
}

const hits = [];
// PROXIMITY ATTRIBUTION, kept strictly separate from the exact path parse. A denial
// line often names no package at all — "Error fetching release: getaddrinfo EAI_AGAIN
// workers.cloudflare.com" is the whole line — which on the first full run pushed most
// net denials to `(shard-level)`, i.e. exactly the information the carve-out list
// needs, unattributed. The cursor remembers the last package a store path named, and
// its guess is LABELLED so it can never be mistaken for the exact parse. Treat a
// `proximity` attribution as a lead: the single-package re-run of the break set is
// what settles ownership.
let cursor = null;
let cursorAge = Infinity;
for (let i = 0; i < lines.length; i++) {
  const named = attributeLine(lines[i]);
  if (named) { cursor = named; cursorAge = 0; } else cursorAge++;
  const kind = classify(lines[i]);
  if (!kind) continue;
  const exact = named ?? attributeLine(lines.slice(Math.max(0, i - 6), i).join('\n'));
  hits.push({
    kind,
    pkg: exact ?? (cursorAge <= 40 ? cursor : null),
    attribution: exact ? 'path' : cursorAge <= 40 ? 'proximity' : 'none',
    hosts: kind === 'net' ? extract(lines[i], HOST_PATTERNS) : [],
    paths: kind === 'fs' ? extract(lines[i], PATH_PATTERNS) : [],
    line: lines[i].trim().slice(0, 400),
  });
}

const byPkg = {};
for (const h of hits) {
  const k = h.pkg || '(shard-level)';
  byPkg[k] ??= { fs: 0, net: 0, build: 0, engine: 0, denied_hosts: [], denied_paths: [], attribution: {}, samples: [] };
  byPkg[k][h.kind]++;
  byPkg[k].attribution[h.attribution] = (byPkg[k].attribution[h.attribution] || 0) + 1;
  for (const x of h.hosts) if (!byPkg[k].denied_hosts.includes(x)) byPkg[k].denied_hosts.push(x);
  for (const x of h.paths) if (!byPkg[k].denied_paths.includes(x)) byPkg[k].denied_paths.push(x);
  // 12, not 4 — four was below the number of distinct denials a single package
  // routinely produces, so the record was a truncation rather than a summary.
  if (byPkg[k].samples.length < 12) byPkg[k].samples.push(`[${h.kind}/${h.attribution}] ${h.line}`);
}

// The union, independent of attribution: the list that answers "what would have to be
// carved out". It must not shrink when a line fails to name an owner.
const allHosts = [...new Set(hits.flatMap((h) => h.hosts))].sort();
const allPaths = [...new Set(hits.flatMap((h) => h.paths))].sort();

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
  // Whether the lifecycle-script window actually CLOSED before the post-snapshot.
  // `TIMEOUT` means work was still in flight when the delta was taken, so the run's
  // verdicts are over a partially-written tree and must not be folded into a number.
  // post_approve_log_lines quantifies the leak: 6 is the harness's own trailing
  // output, anything above it is script output that arrived after the command returned.
  settle_state: process.env.SETTLE_STATE || 'not-measured',
  settle_waited_s: Number(process.env.SETTLE_WAITED || 0),
  post_approve_log_lines: Number(process.env.POST_APPROVE_LINES || 0),
  rc_install: verdicts.rc_install,
  rc_script: verdicts.rc_script,
  verdict_summary: verdicts.summary,
  predicate_spec: verdicts.predicate_spec,
  failure_signature_totals: totals,
  failure_signatures_by_package: byPkg,
  // THE CARVE-OUT DELIVERABLE. Attribution-independent, so it is complete even where
  // ownership is not resolved.
  denied_hosts_all: allHosts,
  denied_paths_all: allPaths,
  binary_writes_outside_cells: verdicts.binary_writes_outside_cells,
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

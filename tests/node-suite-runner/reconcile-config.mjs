// Reconcile tests/node-compat-config.jsonc against a Node 26 corpus run.
//
// Design rules (deliberate, and the reason this is not a blind regeneration):
//   1. EXISTING entries are preserved verbatim. Their `reason` strings are
//      hand-written curation; a generated run must not clobber them.
//   2. A test is only ADDED if upstream Node 26 itself passes it here. A test
//      that fails on real Node is an environment artifact of this harness (no
//      built checkout, sandbox, missing fixture), not a nub compat signal --
//      adding it as `ignore` would inflate the file with noise.
//   3. Disagreements with existing entries are REPORTED, never silently
//      flipped: an active entry that now fails is a regression a human should
//      look at, and an ignored entry that now passes is an un-ignore candidate.

import fs from "node:fs";

const [, , CONFIG, NODE26_JSON, NUB_JSON, OUT_CONFIG, OUT_REPORT] = process.argv;
if (!OUT_REPORT) {
  console.error("usage: update-config.mjs <config.jsonc> <node26.json> <nub.json> <out.jsonc> <report.md>");
  process.exit(2);
}

function stripJsonc(s) {
  let o = "", i = 0, q = false, e = false;
  while (i < s.length) {
    const c = s[i];
    if (q) { o += c; if (e) e = false; else if (c === "\\") e = true; else if (c === '"') q = false; i++; continue; }
    if (c === '"') { q = true; o += c; i++; continue; }
    if (c === "/" && s[i + 1] === "/") { while (i < s.length && s[i] !== "\n") i++; continue; }
    if (c === "/" && s[i + 1] === "*") { i += 2; while (i < s.length && !(s[i] === "*" && s[i + 1] === "/")) i++; i += 2; continue; }
    o += c; i++;
  }
  return o.replace(/,(\s*[}\]])/g, "$1");
}

const srcText = fs.readFileSync(CONFIG, "utf8");
const existing = JSON.parse(stripJsonc(srcText));
const header = srcText.slice(0, srcText.indexOf("{"));

const loadRun = (p) => {
  const j = JSON.parse(fs.readFileSync(p, "utf8"));
  const m = new Map();
  for (const r of j.results) m.set(r.name, r);
  return m;
};
const node26 = loadRun(NODE26_JSON);
const nub = loadRun(NUB_JSON);

// results are keyed bare for parallel/sequential; the config keys them dir/name
const CANON = new Map();
for (const name of node26.keys()) CANON.set(name, name);
const configKeyFor = (runName, dir) => (runName.includes("/") ? runName : `${dir}/${runName}`);

// ---- classify a nub failure into the file's existing reason vocabulary ------
// A reason is committed to a tracked file, so it must carry no machine-specific
// path, no control character, and no "//" (which the shell runner's JSONC strip
// would treat as a comment mid-string).
function sanitize(t) {
  return t
    .replace(/[\u0000-\u001f\u007f]/g, " ")       // control chars break JSON.parse
    .replace(/\b[a-z]+:\/\/\S*/gi, "<url>")        // file:///... , http://...
    .replace(/(^|\s)\/\S+/g, "$1<path>")           // absolute paths
    .replace(/\/\//g, "/")                         // any surviving //
    .replace(/\(node:\d+\)/g, "(node)")            // pid churns the file on every run
    .replace(/\b\d{4,}\b/g, "N")                   // other pids / ports / byte counts
    .replace(/\s+/g, " ")
    .trim();
}
function reasonFor(r) {
  const e = sanitize(r.stderr || "");
  if (r.status === "timeout") return "timeout: exceeded the harness timeout under nub";
  if (/ExperimentalWarning/.test(e)) return "feature-enabled: nub enables a flag whose ExperimentalWarning the test asserts on";
  if (/localStorage|sessionStorage|webstorage/i.test(e)) return "webstorage: nub enables --experimental-webstorage";
  if (/The "message" argument must be one of type string or function/.test(e))
    return "source-maps: nub enables --enable-source-maps, which breaks assert's generated-message path";
  if (/Cannot find module|ERR_MODULE_NOT_FOUND|ERR_UNKNOWN_BUILTIN_MODULE/.test(e))
    return `divergence: module resolution -- ${e.slice(0, 100)}`;
  const firstErr = (e.match(/([A-Za-z]*Error[^.]{0,120})/) || [, ""])[1];
  return `divergence: ${sanitize(firstErr || e).slice(0, 110) || "non-zero exit under nub"}`;
}

// ---- build ------------------------------------------------------------------
const added = [], skippedUpstreamFail = [], regressions = [], unignoreCandidates = [], notRun = [];
const out = { ...existing };

// The control run enumerated the whole corpus, so a config entry it never saw
// names a test upstream has deleted. Those are dropped: run-node-compat.sh
// reports them as "SKIP (not found)" forever otherwise.
const corpus = new Set();
for (const [runName, r] of node26) corpus.add(runName.includes("/") ? runName : `${r.dir}/${runName}`);
const stale = Object.keys(existing).filter((k) => !corpus.has(k));
for (const k of stale) delete out[k];

for (const [runName, n26] of node26) {
  const dir = n26.dir;
  const key = configKeyFor(runName, dir);
  const nr = nub.get(runName);

  if (key in existing) {
    if (!nr) continue;
    const wasIgnored = !!existing[key].ignore;
    if (!wasIgnored && nr.status !== "pass" && n26.status === "pass") {
      regressions.push({ key, status: nr.status, err: sanitize((nr.stderr || "").split("\n")[0]).slice(0, 160) });
    } else if (wasIgnored && nr.status === "pass" && n26.status === "pass") {
      unignoreCandidates.push({ key, reason: existing[key].reason || "" });
    }
    continue;
  }

  if (n26.status !== "pass") { skippedUpstreamFail.push({ key, status: n26.status }); continue; }
  if (!nr) { notRun.push(key); continue; }

  out[key] = nr.status === "pass" ? {} : { ignore: true, reason: reasonFor(nr) };
  added.push({ key, verdict: nr.status === "pass" ? "active" : "ignore" });
}

// ---- emit config (sorted, matching the file's existing shape) ---------------
const keys = Object.keys(out).sort();
const body = keys.map((k) => {
  const v = out[k];
  const val = v.ignore
    ? `{ "ignore": true, "reason": ${JSON.stringify(v.reason)} }`
    : Object.keys(v).length ? JSON.stringify(v) : "{}";
  return `  ${JSON.stringify(k)}: ${val}`;
}).join(",\n");
fs.writeFileSync(OUT_CONFIG, `${header}{\n${body}\n}\n`);

// ---- report -----------------------------------------------------------------
const addedActive = added.filter((a) => a.verdict === "active").length;
const addedIgnore = added.filter((a) => a.verdict === "ignore").length;
const lines = [];
const corpusLabel = process.env.NODE_SUITE_VERSION || "Node";
lines.push(`# ${corpusLabel} suite reconciliation\n`);
lines.push(`| | count |`, `|---|---:|`);
lines.push(`| entries before | ${Object.keys(existing).length} |`);
lines.push(`| removed (deleted upstream) | ${stale.length} |`);
lines.push(`| entries after | ${keys.length} |`);
lines.push(`| added (active, nub passes) | ${addedActive} |`);
lines.push(`| added (ignore, nub diverges) | ${addedIgnore} |`);
lines.push(`| skipped (upstream Node 26 fails here) | ${skippedUpstreamFail.length} |`);
lines.push(`| existing active now failing (review) | ${regressions.length} |`);
lines.push(`| existing ignored now passing (un-ignore candidates) | ${unignoreCandidates.length} |`);
if (notRun.length) lines.push(`| not run under nub | ${notRun.length} |`);

if (stale.length) {
  lines.push(`\n## Removed: no longer present upstream\n`);
  for (const k of stale) lines.push(`- \`${k}\``);
}

const group = (arr, f) => arr.reduce((a, x) => ((a[f(x)] = (a[f(x)] || 0) + 1), a), {});
lines.push(`\n## Added-as-ignore, by reason category\n`);
const cats = group(added.filter(a => a.verdict === "ignore").map(a => ({ k: out[a.key].reason.split(":")[0] })), (x) => x.k);
for (const [c, n] of Object.entries(cats).sort((a, b) => b[1] - a[1])) lines.push(`- ${c}: ${n}`);

if (regressions.length) {
  lines.push(`\n## Existing active entries that now FAIL under nub\n`);
  lines.push("These were curated as passing. Each is a regression or an environment change; not auto-flipped.\n");
  for (const r of regressions.slice(0, 80)) lines.push(`- \`${r.key}\` (${r.status}) — ${r.err}`);
  if (regressions.length > 80) lines.push(`- …and ${regressions.length - 80} more`);
}
if (unignoreCandidates.length) {
  lines.push(`\n## Existing ignored entries that now PASS under nub\n`);
  for (const r of unignoreCandidates.slice(0, 60)) lines.push(`- \`${r.key}\` — was: ${r.reason}`);
  if (unignoreCandidates.length > 60) lines.push(`- …and ${unignoreCandidates.length - 60} more`);
}
lines.push(`\n## Skipped: upstream ${corpusLabel} does not pass these under this harness\n`);
lines.push(`Not added, because a test real Node fails here measures the environment, not nub.\n`);
for (const s of skippedUpstreamFail.slice(0, 60)) lines.push(`- \`${s.key}\` (${s.status})`);
if (skippedUpstreamFail.length > 60) lines.push(`- …and ${skippedUpstreamFail.length - 60} more`);

fs.writeFileSync(OUT_REPORT, lines.join("\n") + "\n");
console.log(lines.slice(0, 14).join("\n"));
console.log(`\nwrote ${OUT_CONFIG} and ${OUT_REPORT}`);

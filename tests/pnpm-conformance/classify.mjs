#!/usr/bin/env node
// Classify a nextest JUnit report against the allowlist.
//
// Usage: node classify.mjs [--full] <junit.xml> <allowlist.txt>
//
// An allowlist line is `<test name>  # <category>: <reason>`, where the name is
// nextest's `<module>::<test>` as it appears in the report. Matching is exact.
//
//   SURPRISE      a failing test with no entry. Fatal: a new divergence.
//   STALE-ALLOW   an entry whose test passed (only with --full, a whole-suite
//                 run). Reported, not fatal: an improvement is not a regression.
//   KNOWN         a failing test with an entry.
import { readFileSync } from "node:fs";
import { parseJunit, readAllowlist } from "./junit.mjs";

const args = process.argv.slice(2);
const full = args[0] === "--full";
if (full) args.shift();
const [junitPath, allowPath] = args;
if (!junitPath || !allowPath) {
  console.error("usage: classify.mjs [--full] <junit.xml> <allowlist.txt>");
  process.exit(2);
}

const cases = parseJunit(readFileSync(junitPath, "utf8"));
const allow = readAllowlist(readFileSync(allowPath, "utf8"));
const failing = cases.filter((c) => c.failed);
const byName = new Map(cases.map((c) => [c.name, c]));

const surprises = failing.filter((c) => !allow.has(c.name));
const known = failing.length - surprises.length;
// A test that no longer exists is stale too: renamed or deleted upstream.
const stale = full ? [...allow.keys()].filter((n) => !byName.get(n)?.failed) : [];

// nextest leaves skipped tests out of the report; its own summary counts them.
console.log(`tests: ${cases.length}  passed: ${cases.length - failing.length}  failed: ${failing.length}`);
console.log(`KNOWN: ${known}  SURPRISE: ${surprises.length}  STALE-ALLOW: ${full ? stale.length : "n/a (partial run)"}`);

const counts = new Map();
for (const c of failing) {
  const category = allow.get(c.name)?.category;
  if (category) counts.set(category, (counts.get(category) ?? 0) + 1);
}
for (const [category, n] of [...counts].sort((a, b) => b[1] - a[1])) {
  console.log(`  ${String(n).padStart(4)}  ${category}`);
}

for (const c of surprises) {
  console.log(`\nSURPRISE ${c.name}\n${c.message.split("\n").slice(0, 25).join("\n")}`);
}
for (const name of stale) {
  console.log(`STALE-ALLOW ${name}${byName.has(name) ? " (now passes)" : " (no such test)"}`);
}
process.exit(surprises.length > 0 ? 1 : 0);

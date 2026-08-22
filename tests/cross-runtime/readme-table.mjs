#!/usr/bin/env node
// The README's results table, derived from results.json — never retyped.
//
//   node tests/cross-runtime/readme-table.mjs          # print the rows
//   node tests/cross-runtime/readme-table.mjs --check  # exit 1 if README.md drifted
//   node tests/cross-runtime/readme-table.mjs --write  # rewrite the rows in README.md
//
// The rows live between the `<!-- results-table -->` markers in README.md.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const results = JSON.parse(fs.readFileSync(path.join(HERE, "results.json"), "utf8"));
const README = path.join(HERE, "README.md");

const LENSES = ["denoExclusions", "bunUniverse", "fullCorpus", "fullCorpusNoEngine", "bunUniverseNoEngine", "engineSpecificOnly"];
const COLUMNS = ["nub", "deno", "bun", "node25"];
const n = (x) => x.toLocaleString("en-US");

function rows() {
  return LENSES.map((lens) => {
    const s = results.scores[lens];
    const by = Object.fromEntries(s.runtimes.map((r) => [r.runtime, r]));
    const cells = COLUMNS.map((rt) => {
      const r = by[rt];
      const pct = `${r.pct.toFixed(2)}%`;
      return `${rt === "nub" ? `**${pct}**` : pct} (${r.rawPct.toFixed(2)})`;
    });
    return `| \`${lens}\` | ${n(s.files)} / ${n(s.nodePass)} | ${cells.join(" | ")} |`;
  }).join("\n");
}

const START = "<!-- results-table -->", END = "<!-- /results-table -->";
const readme = fs.readFileSync(README, "utf8");
const a = readme.indexOf(START), b = readme.indexOf(END);
if (a === -1 || b === -1) { console.error(`README.md lacks the ${START} … ${END} markers`); process.exit(2); }
const current = readme.slice(a + START.length, b).trim();
const expected = rows();

if (process.argv.includes("--check")) {
  if (current === expected) { console.log("README results table matches results.json"); process.exit(0); }
  console.error("README results table drifted from results.json — run with --write:\n" + expected);
  process.exit(1);
}
if (process.argv.includes("--write")) {
  fs.writeFileSync(README, readme.slice(0, a + START.length) + "\n" + expected + "\n" + readme.slice(b));
  console.log("README results table rewritten");
} else {
  console.log(expected);
}

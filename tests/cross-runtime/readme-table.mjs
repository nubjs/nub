#!/usr/bin/env node
// The README's results table, derived from results.json — never retyped.
//
//   node tests/cross-runtime/readme-table.mjs          # print the rows
//   node tests/cross-runtime/readme-table.mjs --check  # exit 1 if README.md drifted
//   node tests/cross-runtime/readme-table.mjs --write  # rewrite the rows in README.md
//
// The table — header with the measured runtime versions, then one row per
// lens — lives between the `<!-- results-table -->` markers in README.md.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
// Exit codes: 0 match, 1 drift, 2 could not check (missing/unreadable input or
// markers) — the pre-push hook blocks only on 1.
let results;
try {
  results = JSON.parse(fs.readFileSync(path.join(HERE, "results.json"), "utf8"));
} catch (e) {
  console.error(`cannot read results.json: ${e.message}`);
  process.exit(2);
}
const README = path.join(HERE, "README.md");

const LENSES = ["denoExclusions", "bunUniverse", "fullCorpus", "fullCorpusNoEngine", "bunUniverseNoEngine", "engineSpecificOnly"];
const COLUMNS = ["nub", "deno", "bun", "node25"];
const n = (x) => x.toLocaleString("en-US");

// "deno 2.9.5 (stable, release, …)\nv8 …" → "2.9.5"; "v25.9.0" → "25.9.0"; "1.4.0" → "1.4.0".
function version(rt) {
  const raw = (results.meta.binaries[rt]?.version || "?").split("\n")[0];
  const m = /(\d+\.\d+\.\d+)/.exec(raw);
  return m ? m[1] : raw;
}

function rows() {
  const header = `| Lens | files / node passes | nub | deno ${version("deno")} | bun ${version("bun")} | node ${version("node25")} |
|------|---------------------|-----|------------|-----------|-------------|`;
  return header + "\n" + LENSES.map((lens) => {
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

// The "Runtime versions we measured" table, from the same metadata.
function versionsTable() {
  const b = results.meta.binaries;
  const v = (rt) => (b[rt]?.version || "?").split("\n")[0];
  return [
    "| Runtime | Version |",
    "|---------|---------|",
    `| node    | ${v("node")} |`,
    `| nub     | ${v("nub")}, augmented default mode, on Node ${b.nub?.node || "?"} |`,
    `| bun     | ${version("bun")} |`,
    `| deno    | ${version("deno")} |`,
    `| node25  | ${v("node25")} — the latest Node 25, run on the Node 26 corpus to size version skew |`,
  ].join("\n");
}

const BLOCKS = [
  { name: "results table", start: "<!-- results-table -->", end: "<!-- /results-table -->", render: rows },
  { name: "versions table", start: "<!-- versions-table -->", end: "<!-- /versions-table -->", render: versionsTable },
];

let readme = fs.readFileSync(README, "utf8");
let drifted = false;
for (const blk of BLOCKS) {
  const a = readme.indexOf(blk.start), b = readme.indexOf(blk.end);
  if (a === -1 || b === -1) { console.error(`README.md lacks the ${blk.start} … ${blk.end} markers`); process.exit(2); }
  const current = readme.slice(a + blk.start.length, b).trim();
  const expected = blk.render();
  if (process.argv.includes("--check")) {
    if (current !== expected) { drifted = true; console.error(`README ${blk.name} drifted from results.json — run with --write:\n` + expected); }
  } else if (process.argv.includes("--write")) {
    readme = readme.slice(0, a + blk.start.length) + "\n" + expected + "\n" + readme.slice(b);
  } else {
    console.log(expected + "\n");
  }
}
// The published figures are hand-copied into the homepage COMPAT array and the
// blog's compatibility sentence, and nothing else rechecks them — both drifted
// silently when a re-measurement moved bun's score. --check compares each
// surface's one-decimal denoExclusions rates against results.json; a missing
// file exits 2 (uncheckable), a mismatch exits 1 like table drift.
function checkHandCopies() {
  const s = results.scores.denoExclusions;
  const by = Object.fromEntries(s.runtimes.map((r) => [r.runtime, r]));
  const rate = (rt) => (by[rt].pass / s.nodePass * 100).toFixed(1);
  const surfaces = [
    {
      file: path.join(HERE, "../../site/src/app/(home)/page.tsx"),
      wants: [
        { re: /name: 'Nub', rate: ([\d.]+), tests: '([\d,]+) \/ ([\d,]+)'/, rt: "nub" },
        { re: /name: 'Deno [\d.]+', rate: ([\d.]+), tests: '([\d,]+) \/ ([\d,]+)'/, rt: "deno" },
        { re: /name: 'Bun [\d.]+', rate: ([\d.]+), tests: '([\d,]+) \/ ([\d,]+)'/, rt: "bun" },
      ],
    },
    {
      file: path.join(HERE, "../../site/content/blog/introducing-nub.mdx"),
      wants: [
        { re: /clears ([\d.]+)% of what real Node passes/, rt: "nub" },
        { re: /([\d.]+)% for Deno/, rt: "deno" },
        { re: /([\d.]+)% for Bun/, rt: "bun" },
      ],
    },
  ];
  let bad = false;
  for (const { file, wants } of surfaces) {
    let src;
    try { src = fs.readFileSync(file, "utf8"); } catch (e) { console.error(`cannot read ${file}: ${e.message}`); process.exit(2); }
    for (const { re, rt } of wants) {
      const m = re.exec(src);
      if (!m) { console.error(`${path.basename(file)}: pattern for ${rt} not found (${re})`); bad = true; continue; }
      if (m[1] !== rate(rt)) { console.error(`${path.basename(file)}: ${rt} says ${m[1]}, results.json says ${rate(rt)}`); bad = true; }
      if (m[2] && (m[2] !== n(by[rt].pass) || m[3] !== n(s.nodePass))) {
        console.error(`${path.basename(file)}: ${rt} tests say ${m[2]} / ${m[3]}, results.json says ${n(by[rt].pass)} / ${n(s.nodePass)}`);
        bad = true;
      }
    }
  }
  return bad;
}

if (process.argv.includes("--check")) {
  if (checkHandCopies()) drifted = true;
  if (drifted) process.exit(1);
  console.log("README tables and site figures match results.json");
} else if (process.argv.includes("--write")) {
  fs.writeFileSync(README, readme);
  console.log("README tables rewritten");
}

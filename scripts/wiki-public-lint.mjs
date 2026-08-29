#!/usr/bin/env node
// Lints docs under wiki/ for material that must not ship in the public graph:
// pointers into gitignored directories, ship-scope vocabulary, private
// decision attributions, and process narration. It matches the mechanical
// tells only — whether a doc's shape is publishable is a judgment the author
// makes before this runs. Wired into .githooks/pre-commit and pre-push next to
// `lat check`; `node scripts/wiki-public-lint.mjs [file…]` runs it by hand.
import { readFileSync } from "node:fs";
import { execSync } from "node:child_process";

const EXEMPT = new Set(["wiki/agents.md", "wiki/lat.md"]);

const RULES = [
  [/(?<![\w/:.-])(?:\.repos|\.frizz|\.fray|epics)\//, "path into a gitignored directory"],
  [/(?<![\w/:.-])internal\/(?:research|commands|runtime|proposals)\b|(?<![\w/:.-])internal\/[\w-]+\.md\b/, "path into the private corpus"],
  [/AGENTS\.local\.md|CLAUDE\.local\.md/, "reference to the local-only orientation file"],
  [/internal research corpus|internal planning (?:link|doc)/i, "provenance line naming the private corpus"],
  [/maintainer.s call|rejected by the maintainer|DECIDED \(maintainer\)|needs sign-off/i, "private decision attribution"],
  [/\bv0\.x\b|\bv1\.x\b|\bpost-v0\b|\bnot in v0\b|\bfor v0\b|\bin v0\b|\bv0 (?:cut|scope|set)\b|\bPhase[- ]?[12]\b|already planned/i, "ship-scope vocabulary"],
  [/\bv0\.1 (?:set|scope|ship|cut|default|marketing|target|inclusion|feature|pitch)/i, "ship-scope vocabulary"],
  [/\bsub-?agents?\b|the agent.s machine|via WebFetch|\bthis session\b|\bresearch session\b|\bProng [A-D]\b|\(workflow:/i, "internal process vocabulary"],
  [/Nub stance suggestion/i, "undecided stance block"],
];

const argv = process.argv.slice(2);
const files = argv.length
  ? argv
  : execSync("git ls-files wiki", { encoding: "utf8" }).split("\n").filter((f) => f.endsWith(".md"));

let hits = 0;
for (const file of files) {
  if (EXEMPT.has(file)) continue;
  let text;
  try { text = readFileSync(file, "utf8"); } catch { continue; }
  const lines = text.split("\n");
  for (let i = 0; i < lines.length; i++) {
    for (const [re, why] of RULES) {
      const m = re.exec(lines[i]);
      if (!m) continue;
      hits++;
      const at = Math.max(0, m.index - 40);
      console.log(`${file}:${i + 1}: ${why}: …${lines[i].slice(at, m.index + m[0].length + 40)}…`);
    }
  }
}
if (hits) {
  console.error(`\nwiki-public-lint: ${hits} hit${hits === 1 ? "" : "s"}. wiki/ is public; move the material to internal/ or reword it.`);
  process.exit(1);
}

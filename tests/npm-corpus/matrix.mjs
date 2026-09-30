// Emits corpus.tsv as a GitHub Actions matrix, `{ include: [{repo, commit, node}, ...] }`.
// Any arguments are `owner/repo` names to keep (comma- or space-separated); none keeps all.
//
//   node tests/npm-corpus/matrix.mjs
//   node tests/npm-corpus/matrix.mjs mochajs/mocha,avajs/ava
// Every project is crossed with the install modes run.sh knows (MODES, default
// "install,npm-ci"), so `{repo, commit, node, mode}`.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const keep = new Set(process.argv.slice(2).flatMap((a) => a.split(/[,\s]+/)).filter(Boolean));
const modes = (process.env.MODES || "install,npm-ci").split(/[,\s]+/).filter(Boolean);
for (const mode of modes) {
  if (mode !== "install" && mode !== "npm-ci") throw new Error(`MODES: unknown mode ${JSON.stringify(mode)}`);
}
const include = readFileSync(join(here, "corpus.tsv"), "utf8")
  .split("\n")
  .filter((line) => line.trim() && !line.startsWith("#"))
  .map((line) => {
    const [repo, commit, node] = line.split("\t");
    if (!/^[\w.-]+\/[\w.-]+$/.test(repo) || !/^[0-9a-f]{40}$/.test(commit) || !/^\d+(\.\d+)*$/.test(node)) {
      throw new Error(`corpus.tsv: malformed line: ${JSON.stringify(line)}`);
    }
    return { repo, commit, node };
  })
  .filter((entry) => keep.size === 0 || keep.has(entry.repo));
if (keep.size > 0 && include.length !== keep.size) {
  const known = new Set(include.map((e) => e.repo));
  throw new Error(`not in corpus.tsv: ${[...keep].filter((r) => !known.has(r)).join(", ")}`);
}
const crossed = include.flatMap((entry) => modes.map((mode) => ({ ...entry, mode })));
process.stdout.write(`${JSON.stringify({ include: crossed })}\n`);

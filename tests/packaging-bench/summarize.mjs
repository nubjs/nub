// Reduce one hyperfine export to a table of MINIMUMS, with the baseline spread
// stated first. Minimums, not means: a shared runner's contention only ever adds
// time, so the minimum is the number that survives it. The spread between the
// duplicate baselines is the error bar — a gap smaller than it is not a result.
import { readFileSync } from "node:fs";

const results = JSON.parse(readFileSync(process.argv[2], "utf8")).results;
const min = (r) => Math.min(...r.times) * 1000;

const baselines = results.filter((r) => r.command.startsWith("baseline-"));
const bmins = baselines.map(min);
const spread = Math.max(...bmins) - Math.min(...bmins);
const floor = Math.min(...bmins);

console.log(`BASELINE SPREAD: ${spread.toFixed(2)} ms   (${bmins.length} interleaved duplicates: ${bmins.map((b) => b.toFixed(2)).join(", ")})`);
console.log(`Any gap below ${spread.toFixed(2)} ms is inside the noise and must not be read as a result.\n`);
// The ratio column is the one that survives a change of runner. Absolute
// milliseconds move by ~1.6x between two ubuntu-latest machines running this
// same harness; every row moves together, so x-node stays put.
console.log(`  ${"row".padEnd(26)}${"min ms".padStart(9)}${"vs node".padStart(10)}${"x node".padStart(9)}   ${"mean ms".padStart(8)}`);
for (const r of results) {
  const label = r.command;
  console.log(
    `  ${label.padEnd(26)}${min(r).toFixed(2).padStart(9)}${("+" + (min(r) - floor).toFixed(2)).padStart(10)}${(min(r) / floor).toFixed(3).padStart(9)}   ${(r.mean * 1000).toFixed(2).padStart(8)}`,
  );
}

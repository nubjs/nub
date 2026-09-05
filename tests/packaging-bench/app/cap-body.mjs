// Shared body for the capability-matched rows, so the polyfilled and the
// nub-supplied artifact run identical application work and differ ONLY by where
// the globals come from.
import { Command } from "commander";
import chalk from "chalk";
import { parse } from "yaml";

const CONFIG = `
name: demo
items:
  - id: 1
    label: alpha
  - id: 2
    label: beta
  - id: 3
    label: gamma
`;

// The globals a nub artifact supplies on a Node that lacks them, and that a SEA
// author has to ship for themselves. Printed rather than asserted, so a mismatch
// names the global instead of just failing.
const PROBES = [
  "Temporal",
  "URLPattern",
  "Float16Array",
  "reportError",
  "Worker",
];

export function digest() {
  const out = PROBES.map((n) => `${n}=${typeof globalThis[n]}`);
  out.push(`navigator.locks=${typeof globalThis.navigator?.locks}`);
  console.log(out.join(" "));
}

export function run() {
  const program = new Command();
  program
    .name("cap")
    .version("1.0.0")
    .argument("[filter]", "substring filter", "")
    .action((filter) => {
      const cfg = parse(CONFIG);
      for (const row of cfg.items.filter((i) => i.label.includes(filter))) {
        console.log(`${chalk.cyan(String(row.id))}  ${row.label}`);
      }
    });
  program.parse([], { from: "user" });
}

export function main() {
  if (process.env.CAP_DIGEST) digest();
  else run();
}

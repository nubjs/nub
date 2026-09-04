#!/usr/bin/env node
// A representative small CLI: argument parsing, colour, and a config parser —
// three dependencies with real module graphs, so the measurement covers module
// load and compile rather than only a runtime's fixed startup cost.
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

const program = new Command();
program
  .name("demo")
  .description("packaging-bench fixture CLI")
  .version("1.0.0")
  .option("-q, --quiet", "suppress colour")
  .argument("[filter]", "substring filter", "")
  .action((filter, opts) => {
    const cfg = parse(CONFIG);
    const paint = opts.quiet ? (s) => s : chalk.cyan;
    for (const row of cfg.items.filter((i) => i.label.includes(filter))) {
      console.log(`${paint(String(row.id))}  ${row.label}`);
    }
  });

// Parse a FIXED argv rather than process.argv: every packager presents a
// different argv[0]/argv[1] shape, and the benchmark must run identical work
// under all of them.
program.parse([], { from: "user" });

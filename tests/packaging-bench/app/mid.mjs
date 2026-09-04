#!/usr/bin/env node
// The middle fixture: one real dependency instead of three. It exists to locate
// the crossover between a compiled artifact's roughly FIXED overhead and the
// per-module resolution cost a globally installed CLI pays, which scales. Same
// construction as cli.mjs — a real published package, not generated modules.
import { Command } from "commander";

const program = new Command();
program
  .name("mid")
  .description("packaging-bench fixture: argument parsing only")
  .version("1.0.0")
  .argument("[filter]", "substring filter", "")
  .action((filter) => {
    for (const label of ["alpha", "beta", "gamma"]) {
      if (label.includes(filter)) console.log(label);
    }
  });

// Fixed argv, for the same reason cli.mjs uses one: every packager presents a
// different argv shape, and the benchmark must run identical work under all.
program.parse([], { from: "user" });

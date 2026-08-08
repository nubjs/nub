#!/usr/bin/env node
// Typecheck-fixture runner for @nubjs/types.
//
// Runs `tsc --noEmit` on each fixture under both sides of the package's
// `typesVersions` boundary and asserts the expected pass/fail outcome. This catches
// broken declarations, lost wildcards, accidental module conversion, and collisions
// with current or future standard-library members. Each fixture exercises the REAL
// package via `file:..`, not a copied declaration.
//
// Fixtures:
//   positive        — every @nubjs/types surface resolves (lib es2024, no dom) → PASS
//   future-stdlib   — proposal members are declared a second time, as a future lib → PASS
//   stepaside-dom   — consumer also has Worker via lib.dom → no TS2403, coexists → PASS
//   stepaside-stub  — a separate DOM-shaped lib declares global Worker → step aside → PASS
//   negative-export — common.d.ts + `export {}` breaks wildcards/globals → FAIL
//
// Usage: node run.mjs   (run from npm/nub-types/test, after `npm install`)

import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const currentTsc = join(here, "node_modules", "typescript", "lib", "tsc.js");
const modernTsc = join(here, "node_modules", "typescript-6-0", "lib", "tsc.js");
const legacyTsc = join(here, "node_modules", "typescript-5-9", "lib", "tsc.js");
const commonDts = join(here, "node_modules", "@nubjs", "types", "common.d.ts");

if (!existsSync(currentTsc) || !existsSync(modernTsc) || !existsSync(legacyTsc)) {
  console.error("TypeScript fixture compilers are missing — install npm/nub-types/test dependencies first.");
  process.exit(1);
}

// Generate the negative control from the CURRENT shared global script. Appending
// `export {}` makes its wildcards and bare globals module-local.
const negDts = join(here, "fixtures", "negative-export", "nub-env-as-module.d.ts");
writeFileSync(
  negDts,
  `${readFileSync(commonDts, "utf8")}\ndeclare namespace Temporal { interface Instant {} }\nexport {};\n`,
);

/** @type {{name: string, dir: string, expect: "pass" | "fail"}[]} */
const fixtures = [
  { name: "positive", dir: "positive", expect: "pass" },
  { name: "future-stdlib", dir: "future-stdlib", expect: "pass" },
  { name: "stepaside-dom", dir: "stepaside-dom", expect: "pass" },
  { name: "stepaside-stub", dir: "stepaside-stub", expect: "pass" },
  { name: "negative-export", dir: "negative-export", expect: "fail" },
];

const compilers = [
  { name: "TypeScript 7.0", command: process.execPath, args: [currentTsc] },
  { name: "TypeScript 6.0", command: process.execPath, args: [modernTsc] },
  { name: "TypeScript 5.9", command: process.execPath, args: [legacyTsc] },
];

/** Run one compiler on a fixture's tsconfig; return { ok, output }. */
function runTsc(compiler, dir) {
  const project = join(here, "fixtures", dir);
  try {
    const output = execFileSync(
      compiler.command,
      [...compiler.args, "--noEmit", "-p", join(project, "tsconfig.json")],
      {
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
      },
    );
    return { ok: true, output };
  } catch (err) {
    return { ok: false, output: `${err.stdout ?? ""}${err.stderr ?? ""}` };
  }
}

let failed = 0;
for (const compiler of compilers) {
  for (const { name, dir, expect } of fixtures) {
    const { ok, output } = runTsc(compiler, dir);
    const got = ok ? "pass" : "fail";
    if (got === expect) {
      console.log(`✓ ${compiler.name} / ${name}: tsc ${got} (expected ${expect})`);
    } else {
      failed++;
      console.error(`✗ ${compiler.name} / ${name}: tsc ${got}, expected ${expect}`);
      if (output.trim()) console.error(output.trim().split("\n").map((l) => `    ${l}`).join("\n"));
    }
  }
}

if (failed > 0) {
  console.error(`\n${failed} fixture(s) failed.`);
  process.exit(1);
}
console.log(`\nAll ${fixtures.length} fixtures behaved as expected under all three compilers.`);

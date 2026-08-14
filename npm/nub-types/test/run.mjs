#!/usr/bin/env node
// Typecheck-fixture runner for @nubjs/types.
//
// Runs `tsc --noEmit` on each fixture under four compilers spanning both sides of the
// package's `typesVersions` boundary, and asserts the expected pass/fail outcome. This
// catches broken declarations, lost wildcards, accidental module conversion, collisions
// with current or future standard-library members, and — from the oldest leg — a
// `reference lib` naming a library that TypeScript did not have yet. Each fixture
// exercises the REAL package via `file:..`, not a copied declaration.
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
// The OLDEST compiler `typesVersions` routes to ts5.9/index.d.ts. It is here because
// the 5.9 leg cannot see a whole class of defect: a `reference lib` naming a library
// that 5.9 has but an earlier TypeScript does not is a TS2726 that fails the
// consumer's entire build, and `<=5.9` routes those consumers to the same file.
// Caught exactly that with lib.esnext.error, which first exists in 5.9.
const floorTsc = join(here, "node_modules", "typescript-5-8", "lib", "tsc.js");
const commonDts = join(here, "node_modules", "@nubjs", "types", "common.d.ts");

if (!existsSync(currentTsc) || !existsSync(modernTsc) || !existsSync(legacyTsc) || !existsSync(floorTsc)) {
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

/** @type {{name: string, dir: string, expect: "pass" | "fail", dom?: boolean}[]} */
const fixtures = [
  { name: "positive", dir: "positive", expect: "pass" },
  { name: "future-stdlib", dir: "future-stdlib", expect: "pass" },
  { name: "stepaside-dom", dir: "stepaside-dom", expect: "pass", dom: true },
  { name: "stepaside-stub", dir: "stepaside-stub", expect: "pass", dom: true },
  { name: "negative-export", dir: "negative-export", expect: "fail" },
];

// `dom: false` marks a compiler whose OWN lib.dom is incompatible with the pinned
// @types/node, independently of anything @nubjs/types declares — on 5.8 the two
// disagree about TextDecoder (TS2430, raised inside lib.dom.d.ts). The two
// step-aside fixtures are the only ones that pull lib.dom, so they are skipped
// there and the skip is printed rather than silently dropped. Every other fixture,
// including the one that would catch a missing-lib TS2726, still runs.
const compilers = [
  { name: "TypeScript 7.0", command: process.execPath, args: [currentTsc] },
  { name: "TypeScript 6.0", command: process.execPath, args: [modernTsc] },
  { name: "TypeScript 5.9", command: process.execPath, args: [legacyTsc] },
  { name: "TypeScript 5.8", command: process.execPath, args: [floorTsc], dom: false },
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
let skipped = 0;
for (const compiler of compilers) {
  for (const { name, dir, expect, dom } of fixtures) {
    if (dom && compiler.dom === false) {
      skipped++;
      console.log(`– ${compiler.name} / ${name}: skipped (its lib.dom conflicts with the pinned @types/node)`);
      continue;
    }
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
const ran = compilers.length * fixtures.length - skipped;
console.log(
  `\nAll ${ran} fixture runs behaved as expected across ${compilers.length} compilers` +
    (skipped > 0 ? ` (${skipped} skipped, listed above).` : "."),
);

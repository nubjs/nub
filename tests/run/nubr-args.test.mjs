// `nubr <script> -- <args>` argument fidelity, run against the shell this
// platform actually uses. The escape itself has unit coverage in
// scripts/nubr-escape.test.mjs, which is where the algorithm is pinned to npm's;
// what a unit test cannot answer is whether the escaped string then SURVIVES the
// shell. On Windows that is cmd.exe's own second parse of the caret pass, and no
// macOS or Linux run reaches it — so this file is the only place that code
// executes at all, which is why it carries a three-OS CI leg of its own.
//
// It needs no install and no native addon: with the platform package absent the
// preload warns on stderr and passes source through unchanged, and these
// fixtures are plain CommonJS. That keeps the leg to a checkout plus Node.
import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const nubr = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..", "runtime", "nubr.mjs");

const fixture = mkdtempSync(join(tmpdir(), "nubr-args-"));
writeFileSync(
  join(fixture, "package.json"),
  `${JSON.stringify(
    {
      name: "nubr-args-fixture",
      private: true,
      version: "1.0.0",
      scripts: { args: "node argv-echo.cjs" },
    },
    null,
    2,
  )}\n`,
);
writeFileSync(join(fixture, "argv-echo.cjs"), "console.log(JSON.stringify(process.argv.slice(2)));\n");

function forwarded(args) {
  const out = execFileSync(process.execPath, [nubr, "args", "--", ...args], {
    cwd: fixture,
    encoding: "utf8",
  });
  return JSON.parse(out.trim().split(/\r?\n/).filter(Boolean).at(-1));
}

// One case per class of thing a shell would otherwise act on. `%VAR%` is
// deliberately absent: cmd.exe expands it before the caret pass can matter, and
// npm does not defend against that either — asserting it would pin a divergence
// from npm rather than a contract.
const CASES = [
  ["a b", "x;y", "$HOME"],
  ['he said "hi"', "a\\b", "c&d"],
  ["(e)", "f|g", "100%"],
  ["", "after-empty"],
];

for (const args of CASES) {
  test(`forwards ${JSON.stringify(args)} to the script as literals`, () => {
    assert.deepEqual(forwarded(args), args);
  });
}

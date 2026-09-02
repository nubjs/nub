// What `nubr <name>` does with a package script: which target the name resolves
// to, what the script receives as arguments, and what it receives in its
// environment. All three are npm-compatibility contracts, and each had a real
// defect that only a run could find.
//
// The argument cases carry the extra weight, because they run against the shell
// this platform actually uses. The escape algorithm is pinned to npm's by unit
// test in scripts/nubr-escape.test.mjs; what a unit test cannot answer is
// whether the escaped string then SURVIVES the shell. On Windows that is
// cmd.exe's own second parse of the caret pass, and no macOS or Linux run
// reaches it — so this file is the only place that code executes at all, which
// is why it carries a three-OS CI leg of its own.
//
// It needs no install and no native addon: with the platform package absent the
// preload warns on stderr and passes source through unchanged, and these
// fixtures are plain CommonJS. That keeps the leg to a checkout plus Node.
import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const nubr = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..", "runtime", "nubr.mjs");

const fixture = mkdtempSync(join(tmpdir(), "nubr-script-"));
writeFileSync(
  join(fixture, "package.json"),
  `${JSON.stringify(
    {
      name: "nubr-script-fixture",
      private: true,
      version: "1.0.0",
      config: { port: 8080 },
      scripts: {
        args: "node argv-echo.cjs",
        env: "node env-echo.cjs",
        // Shadowed by a directory of the same name, created below.
        build: "node -e \"console.log('script')\"",
      },
    },
    null,
    2,
  )}\n`,
);
writeFileSync(join(fixture, "argv-echo.cjs"), "console.log(JSON.stringify(process.argv.slice(2)));\n");
writeFileSync(
  join(fixture, "env-echo.cjs"),
  `console.log(JSON.stringify({
  name: process.env.npm_package_name,
  version: process.env.npm_package_version,
  json: process.env.npm_package_json ? "set" : "unset",
  port: process.env.npm_package_config_port,
  event: process.env.npm_lifecycle_event,
}));\n`,
);
mkdirSync(join(fixture, "build"), { recursive: true });

// stdout only, last non-empty line: the preload writes an addon warning to
// stderr whenever no platform package is installed, which is the normal state
// here.
function run(...argv) {
  const out = execFileSync(process.execPath, [nubr, ...argv], { cwd: fixture, encoding: "utf8" });
  return out.trim().split(/\r?\n/).filter(Boolean).at(-1);
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
    assert.deepEqual(JSON.parse(run("args", "--", ...args)), args);
  });
}

test("a script wins over a directory of the same name", () => {
  // Keying the file check on mere existence ran the `build` DIRECTORY and died
  // in Node's resolver with ERR_UNSUPPORTED_DIR_IMPORT, while npm ran the
  // script. `build`, `dist`, `test` and `docs` are ordinary directory names and
  // ordinary script names at once, so this is the common case, not a corner.
  assert.equal(run("build"), "script");
});

test("a script receives the manifest-derived npm environment", () => {
  assert.deepEqual(JSON.parse(run("env")), {
    name: "nubr-script-fixture",
    version: "1.0.0",
    json: "set",
    port: "8080",
    event: "env",
  });
});

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
import { existsSync, mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
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
        // Shadowed by an installed bin of the same name, created below.
        shadowed: "node -e \"console.log('script')\"",
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

// A stand-in for an installed dependency's bin, written in the one form the
// default shell here can execute: the `.cmd` under cmd.exe, the extensionless
// script elsewhere. npm writes both on Windows and neither shell can run the
// other's, which is what `whichshim` below exists to pin.
const binDir = join(fixture, "node_modules", ".bin");
mkdirSync(binDir, { recursive: true });
function installBin(name, body) {
  if (process.platform === "win32") {
    writeFileSync(join(binDir, `${name}.cmd`), `@echo off\r\nnode -e "${body}" %*\r\n`);
  } else {
    writeFileSync(join(binDir, name), `#!/usr/bin/env node\n${body}\n`, { mode: 0o755 });
  }
}
installBin("faketool", "console.log(JSON.stringify(process.argv.slice(2)))");
installBin("shadowed", "console.log('bin')");
// `test` is a shell builtin on POSIX and a real package-bin name in the wild.
installBin("test", "console.log('the installed bin')");

// Both Windows shims for one bin, saying which one ran. npm writes all three
// forms unconditionally — an extensionless `#!/bin/sh` script, a `.cmd` and a
// `.ps1` — and only the shell decides which is runnable, so a fixture that
// carries just one cannot tell a correct choice from a lucky one.
if (process.platform === "win32") {
  writeFileSync(join(binDir, "whichshim.cmd"), "@echo off\r\necho cmd shim\r\n");
  writeFileSync(join(binDir, "whichshim"), "#!/bin/sh\necho 'sh shim'\n");
}

// Git Bash, not the System32 `bash.exe` that launches WSL — that one would
// resolve the fixture path inside a different filesystem namespace.
function findPosixShellOnWindows() {
  const git = join(process.env.ProgramFiles ?? "C:\\Program Files", "Git", "bin", "bash.exe");
  if (existsSync(git)) return git;
  try {
    const found = execFileSync("where.exe", ["bash"], { encoding: "utf8" })
      .trim()
      .split(/\r?\n/)
      .find((p) => p && !/\\System32\\/i.test(p));
    return found ?? null;
  } catch {
    return null;
  }
}

// stdout only, last non-empty line: the preload writes an addon warning to
// stderr whenever no platform package is installed, which is the normal state
// here.
function runWith(env, ...argv) {
  const out = execFileSync(process.execPath, [nubr, ...argv], {
    cwd: fixture,
    encoding: "utf8",
    env: { ...process.env, ...env },
  });
  return out.trim().split(/\r?\n/).filter(Boolean).at(-1);
}

function run(...argv) {
  return runWith({}, ...argv);
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

test("an installed bin runs, and forwarded arguments reach it as literals", () => {
  // The ad-hoc bin run is the thing a standalone install cannot otherwise do
  // without editing package.json first. It executes through the shell with
  // node_modules/.bin on PATH, so this also covers the Windows shim lookup.
  assert.deepEqual(JSON.parse(run("faketool", "--", "a b", "x;y")), ["a b", "x;y"]);
});

test("a bin whose name is a shell builtin still runs the bin", () => {
  // Passing the bare name to the shell ran `sh`'s `test` builtin instead: no
  // output, exit 1, and nothing to tell the user their bin never ran. The fix
  // is to hand the shell the resolved path, so its own lookup never applies.
  assert.equal(run("test"), "the installed bin");
});

test(
  "a Windows ComSpec that is not cmd runs the shim that shell can execute",
  { skip: process.platform === "win32" ? false : "Windows only" },
  () => {
    const bash = findPosixShellOnWindows();
    // A precondition that quietly fails to hold turns this into a test that
    // cannot fail. Every GitHub Windows runner ships Git Bash, so a miss there
    // is a defect in this harness rather than a reason to skip.
    if (!bash) {
      assert.ok(!process.env.CI, "no POSIX shell found on a CI runner: the differential never ran");
      return;
    }
    // Both shims exist; only the ComSpec differs. Picking the extension from
    // process.platform selected the batch file for BOTH runs, and a POSIX-like
    // shell cannot execute one — so every ordinary dependency bin failed.
    assert.equal(runWith({ ComSpec: bash }, "whichshim"), "sh shim");
    assert.equal(runWith({ ComSpec: "cmd.exe" }, "whichshim"), "cmd shim");
  },
);

test("a script wins over an installed bin of the same name", () => {
  // npm's precedence: a script usually wraps the bin it is named after, so
  // resolving to the bin would silently skip whatever the script adds.
  assert.equal(run("shadowed"), "script");
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

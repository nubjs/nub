// The JS escape used by `nubr` when it splices forwarded arguments onto a
// package script, checked against the same expectations the Rust port asserts in
// crates/nub-core/src/workspace/shell_escape.rs. Both are ports of npm's
// `@npmcli/promise-spawn` (`lib/escape.js`), so these cases are ultimately
// npm's own behavior — if one port drifts, that is the bug, and this catches it.
//
// The cmd.exe cases matter most here: they are the half a macOS or Linux run
// cannot exercise end to end, so a unit check is the only local coverage.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  shEscape,
  binExts,
  cmdEscape,
  bodyTargetsBatchFile,
  commandLine,
  isCmdShell,
  isPowerShell,
  spliceArgs,
} from "../runtime/nubr-escape.mjs";

test("sh leaves an argument with no shell-special character alone", () => {
  for (const s of ["build", "--watch", "src/index.ts", "key=value", "a-b_c.d"]) {
    assert.equal(shEscape(s), s);
  }
});

test("sh quotes metacharacters so the shell cannot act on them", () => {
  assert.equal(shEscape("hello world"), "'hello world'");
  assert.equal(shEscape("a*b"), "'a*b'");
  assert.equal(shEscape("$HOME"), "'$HOME'");
  assert.equal(shEscape("x;y"), "'x;y'");
  assert.equal(shEscape(""), "''");
});

test("sh renders an embedded single quote as close-escape-reopen", () => {
  assert.equal(shEscape("it's"), `'it'\\''s'`);
  // A leading quote triggers npm's leading-''-pair cleanup.
  assert.equal(shEscape("'foo"), `\\''foo'`);
});

test("cmd quotes whitespace and carets what cmd.exe would reinterpret", () => {
  assert.equal(cmdEscape("build", false), "build");
  assert.equal(cmdEscape("src\\index.ts", false), "src\\index.ts");
  assert.equal(cmdEscape("hello world", false), '^"hello^ world^"');
  // A bare metacharacter with no whitespace is caret-escaped, not quoted.
  assert.equal(cmdEscape("a&b", false), "a^&b");
  assert.equal(cmdEscape("", false), '""');
});

test("cmd repeats the caret pass for a batch target that reparses", () => {
  assert.equal(cmdEscape("a&b", true), "a^^^&b");
});

test("a batch target is detected from the body's first token", () => {
  assert.equal(bodyTargetsBatchFile("foo.cmd x"), true);
  assert.equal(bodyTargetsBatchFile("foo.BAT"), true);
  // Documented limitation, shared with the Rust port: PATHEXT is not resolved,
  // so an `eslint` that resolves to eslint.cmd is single-escaped.
  assert.equal(bodyTargetsBatchFile("eslint ."), false);
});

test("only a real cmd shell selects cmd escaping", () => {
  for (const s of ["cmd", "cmd.exe", "CMD.EXE", "C:\\Windows\\System32\\cmd.exe", "\\cmd"]) {
    assert.equal(isCmdShell(s), true, s);
  }
  // The boundary matters: `mycmd` is a different program.
  for (const s of ["bash", "/bin/sh", "mycmd", "C:\\tools\\bash.exe"]) {
    assert.equal(isCmdShell(s), false, s);
  }
});

test("the bin shim is chosen by the shell that will run it, not by the platform", () => {
  // npm writes an extensionless `#!/bin/sh` shim AND a `.cmd` for every bin, and
  // each shell can run only one of them. The case no host here can reach end to
  // end is a Windows box whose ComSpec is bash: Node hands the command to that
  // shell with `-c`, so the batch file is unrunnable and the shell shim is the
  // only usable entry.
  assert.deepEqual(binExts("cmd.exe"), [".cmd", ".exe", ".bat"]);
  assert.deepEqual(binExts("C:\\Program Files\\Git\\bin\\bash.exe"), ["", ".exe"]);
  assert.deepEqual(binExts("/bin/sh"), ["", ".exe"]);
  // `.ps1` is never a candidate: no shell in either list can execute one.
  for (const shell of ["cmd.exe", "/bin/sh"]) {
    assert.equal(binExts(shell).includes(".ps1"), false, shell);
  }
});

test("PowerShell offers no shim at all, rather than one it cannot invoke", () => {
  // The escaping here is a pinned port of npm's, and npm has exactly two
  // branches — cmd and POSIX sh — so a command line for PowerShell would arrive
  // POSIX-quoted, where a quoted command path is a string expression and not an
  // invocation. Offering the `.ps1` under that quoting would resolve and THEN
  // fail; an empty list keeps the miss clean, and nubr explains it on stderr.
  const shells = [
    "powershell.exe",
    "pwsh",
    "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
    "/usr/bin/pwsh",
  ];
  for (const shell of shells) {
    assert.equal(isPowerShell(shell), true, shell);
    assert.deepEqual(binExts(shell), [], shell);
  }
  // A name that merely contains it is a different program.
  for (const shell of ["cmd.exe", "/bin/sh", "bash", "mypwsh", "powershellx"]) {
    assert.equal(isPowerShell(shell), false, shell);
  }
});

test("splicing returns the body untouched when there are no arguments", () => {
  assert.equal(spliceArgs("jest --ci", [], "/bin/sh"), "jest --ci");
});

test("a resolved .cmd target gets the second caret pass a script body cannot", () => {
  // The whole point of resolving the bin before building the command line: npm
  // double-escapes when the target re-parses, and a bare `vitest` in a script
  // body cannot be known to be `vitest.cmd`. `a&b` single-escaped is `a^&b`.
  const shim = "C:\\proj\\node_modules\\.bin\\vitest.cmd";
  assert.match(commandLine(shim, ["a&b"], "cmd.exe"), /a\^\^\^&b$/);
  // A non-batch target on the same shell stays single-escaped.
  assert.match(commandLine("C:\\proj\\node_modules\\.bin\\tool.exe", ["a&b"], "cmd.exe"), /a\^&b$/);
});

test("a resolved command path is escaped, so a space in it cannot split", () => {
  assert.equal(
    commandLine("/My Project/node_modules/.bin/tool", ["--flag"], "/bin/sh"),
    "'/My Project/node_modules/.bin/tool' --flag",
  );
});

test("splicing escapes for the shell it is given, not for the platform", () => {
  // The bug this pins: a Windows host whose ComSpec is bash gets `-c` from
  // Node, so caret escaping would arrive mangled.
  assert.equal(spliceArgs("jest", ["-u", "two words"], "/bin/sh"), "jest -u 'two words'");
  assert.equal(spliceArgs("jest", ["a b"], "cmd.exe"), `jest ^"a^ b^"`);
  assert.equal(spliceArgs("jest", ["a b"], "C:\\Program Files\\bash.exe"), "jest 'a b'");
});

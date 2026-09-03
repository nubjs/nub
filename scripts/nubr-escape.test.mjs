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
  cmdEscape,
  bodyTargetsBatchFile,
  commandLine,
  isCmdShell,
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

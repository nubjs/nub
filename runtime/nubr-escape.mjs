// A forwarded argument reaches the script through a shell command line, so it
// has to survive that shell as ONE literal token. A raw join loses argv
// boundaries and lets the shell run its own substitutions: `nubr s -- "a b"
// 'x;y' '$HOME'` reached the script as three mangled words, ran `y`, and
// expanded $HOME, where npm delivers the three literals untouched.
//
// Ported from npm's `@npmcli/promise-spawn` (`lib/escape.js`), the same source
// `crates/nub-core/src/workspace/shell_escape.rs` was ported from for `nub run`.
// Keep the two in step: divergence from npm IS the bug, so do not "improve"
// either one. A cmd.exe argument needs far more than quote-doubling — `%`, `!`,
// `^`, `&`, parens and redirection all survive quotes and need caret escaping.
//
// Everything here hangs off ONE question — which shell will run the command —
// so `effectiveShell`, `isCmdShell` and `binExts` live here too. Answering it
// from `process.platform` instead has produced two separate defects.

// POSIX `sh -c`. An argument with no shell-special character passes through
// untouched, so plain words and `--flags` read exactly as the user typed them.
export function shEscape(input) {
  if (input === "") return "''";
  if (!/[\t\n\r "#$&'()*;<>?\\`|~]/.test(input)) return input;
  return `'${input.replace(/'/g, `'\\''`)}'`
    .replace(/^(?:'')+(?!$)/, "")
    .replace(/\\'''/g, `\\'`);
}

// `cmd.exe`. First the MS command-line backslash/quote rules, then a caret pass
// over the metacharacters cmd re-interprets. `doubleEscape` repeats the caret
// pass, which npm does when the target is a `.cmd`/`.bat` that re-parses once
// more.
export function cmdEscape(input, doubleEscape) {
  if (input === "") return '""';
  let result = input;
  if (/[ \t\n\v"]/.test(input)) {
    result = '"';
    let i = 0;
    for (;;) {
      let slashes = 0;
      while (i < input.length && input[i] === "\\") {
        i += 1;
        slashes += 1;
      }
      if (i === input.length) {
        result += "\\".repeat(slashes * 2);
        break;
      }
      if (input[i] === '"') result += `${"\\".repeat(slashes * 2 + 1)}"`;
      else result += "\\".repeat(slashes) + input[i];
      i += 1;
    }
    result += '"';
  }
  const caret = (s) => s.replace(/[ !%^&()<>|"]/g, "^$&");
  result = caret(result);
  return doubleEscape ? caret(result) : result;
}

// Does this path name a batch file? cmd.exe re-parses a `.cmd`/`.bat` command
// line a second time, which is what the second caret pass defends against.
export function isBatchFile(p) {
  const lower = String(p).toLowerCase();
  return lower.endsWith(".cmd") || lower.endsWith(".bat");
}

// Best-effort: does the script body invoke a batch file? LIMITATION, shared with
// the Rust port — npm resolves the token through PATH/PATHEXT, so a body like
// `eslint .` whose `eslint` resolves to `eslint.cmd` is treated as non-batch and
// single-escaped here. `commandLine` below does not inherit the limitation,
// because there the target is already resolved.
export function bodyTargetsBatchFile(body) {
  return isBatchFile(body.trim().split(/\s+/)[0] ?? "");
}

// The shell Node will actually use for `spawn(..., { shell })`: ComSpec on
// Windows, `/bin/sh` elsewhere. Escaping has to be chosen from THIS, not from
// the platform — a Windows box whose ComSpec is bash gets `-c` from Node
// (child_process.js checks the same cmd regex below), and caret escaping would
// arrive mangled.
export function effectiveShell() {
  if (process.platform === "win32") return process.env.ComSpec || "cmd.exe";
  return "/bin/sh";
}

// npm's `/(?:^|\\)cmd(?:\.exe)?$/i`, matching Node's own shell test. The
// boundary matters: `mycmd` is not cmd.
export function isCmdShell(shell) {
  const lower = String(shell).toLowerCase();
  const stem = lower.endsWith(".exe") ? lower.slice(0, -4) : lower;
  return stem === "cmd" || stem.endsWith("\\cmd");
}

// Which `node_modules/.bin` shims the effective shell can actually execute, in
// preference order. It belongs beside `isCmdShell` because it is the same fact:
// npm writes THREE files for every bin — an extensionless `#!/bin/sh` script (one
// that handles CYGWIN/MINGW/MSYS/WSL2 explicitly, so it is meant to run on
// Windows too), a `.cmd`, and a `.ps1` — and which of them is runnable is decided
// by the shell, never by the platform. cmd.exe cannot run the shell shim; a
// POSIX-like shell cannot run the batch file. Keying this on `process.platform`
// picked the `.cmd` on a Windows box whose ComSpec is bash, where Node hands the
// command to that shell with `-c`.
//
// `.ps1` is deliberately absent from both lists: npm always writes a `.cmd` next
// to it, cmd.exe cannot execute a PowerShell script, and a POSIX shell cannot
// either — so listing it could only ever select a file the shell then fails to
// run. `.exe` is in both because a real executable needs no shim and no shell
// disagrees about it.
export function binExts(shell) {
  return isCmdShell(shell) ? [".cmd", ".exe", ".bat"] : ["", ".exe"];
}

export function spliceArgs(body, args, shell = effectiveShell()) {
  if (args.length === 0) return body;
  const useCmd = isCmdShell(shell);
  const doubleEscape = useCmd && bodyTargetsBatchFile(body);
  const escaped = args.map((a) =>
    useCmd ? cmdEscape(String(a), doubleEscape) : shEscape(String(a)),
  );
  return `${body} ${escaped.join(" ")}`;
}

// A command line for an ALREADY-RESOLVED executable, as opposed to a script body
// the author wrote. Two things follow from knowing the real target, and both are
// bugs when a bare name is passed to the shell instead:
//
//   - The NAME is not the program. `sh -c "test …"` runs the shell's `test`
//     builtin, not `node_modules/.bin/test`, and reports exit 1 with no output —
//     a silent wrong answer. Escaping the absolute path removes the shell's
//     lookup from the picture entirely (and survives spaces in the path).
//   - The batch test runs against the PATH, so a Windows `.cmd` shim gets the
//     second caret pass it needs. Deriving it from the body could not: the first
//     whitespace token of `C:\My Project\...\vitest.cmd` is `C:\My`.
export function commandLine(commandPath, args, shell = effectiveShell()) {
  const useCmd = isCmdShell(shell);
  const doubleEscape = useCmd && isBatchFile(commandPath);
  const esc = (s, dbl) => (useCmd ? cmdEscape(String(s), dbl) : shEscape(String(s)));
  return [esc(commandPath, false), ...args.map((a) => esc(a, doubleEscape))].join(" ");
}

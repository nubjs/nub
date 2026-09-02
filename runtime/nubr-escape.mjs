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

// Best-effort: does the script body invoke a batch file? LIMITATION, shared with
// the Rust port — npm resolves the token through PATH/PATHEXT, so a body like
// `eslint .` whose `eslint` resolves to `eslint.cmd` is treated as non-batch and
// single-escaped here.
export function bodyTargetsBatchFile(body) {
  const first = body.trim().split(/\s+/)[0]?.toLowerCase() ?? "";
  return first.endsWith(".cmd") || first.endsWith(".bat");
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

export function spliceArgs(body, args, shell = effectiveShell()) {
  if (args.length === 0) return body;
  const useCmd = isCmdShell(shell);
  const doubleEscape = useCmd && bodyTargetsBatchFile(body);
  const escaped = args.map((a) =>
    useCmd ? cmdEscape(String(a), doubleEscape) : shEscape(String(a)),
  );
  return `${body} ${escaped.join(" ")}`;
}

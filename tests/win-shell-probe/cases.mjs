// The probe's case matrix.
//
// `spec` is the COMPAT BAR each case sits at, and it is the whole point of the
// tagging: npm runs `package.json` script bodies with `sh -c`, and on
// Debian/Ubuntu `/bin/sh` is dash — so the bar a candidate shell must clear is
// POSIX sh, NOT bash. A candidate that covers every POSIX row is sufficient;
// bash rows are a bonus.
//
//   POSIX — mandated by POSIX.1-2017 "Shell Command Language". A `sh` that
//           fails one of these is not a POSIX shell.
//   bash  — bash (or ksh) extension. Nice to have; npm never guaranteed it.
//   npm   — not a language feature: `node_modules/.bin` shim resolution, the
//           reason a cross-platform shell is wanted on Windows at all.
//   win   — Windows-path handling; no spec, but a hard practical requirement.
//
// `guard: true` marks a case with a KNOWN hang mode in at least one candidate
// (`deno_task_shell` deadlocks on `2>&1 |` and waits forever on `&`). Those run
// under the driver's per-case timeout with file-backed stdio so a wedged
// grandchild cannot wedge the driver.

export const CASES = [
  // ── HEADLINE: node_modules/.bin shim resolution ──────────────────────────
  // `mytool` has the full npm triplet (.cmd + extensionless sh + .ps1);
  // `onlycmd` has ONLY the .cmd (requires PATHEXT-style extension probing);
  // `onlysh` has ONLY the extensionless sh script (requires shebang handling).
  { id: "shim_both", group: "shim", spec: "npm", body: "mytool --flag value" },
  { id: "shim_only_cmd", group: "shim", spec: "npm", body: "onlycmd --flag value" },
  { id: "shim_only_sh", group: "shim", spec: "npm", body: "onlysh --flag value" },
  { id: "shim_explicit_ext", group: "shim", spec: "npm", body: "mytool.cmd --flag value" },
  { id: "shim_chained", group: "shim", spec: "npm", body: "mytool a && mytool b" },
  { id: "shim_command_v", group: "shim", spec: "POSIX", body: "command -v mytool" },
  { id: "shim_quoted_arg", group: "shim", spec: "npm", body: 'mytool --msg "two words"' },
  // busybox prefers its own built-in applets over PATH. `which` is a real npm
  // bin AND a busybox applet, so this row detects silent hijack of a `.bin` tool.
  { id: "shim_shadowed_applet", group: "shim", spec: "npm", body: "which --flag value" },

  // ── basic external command + quoting ─────────────────────────────────────
  { id: "node_basic", group: "basic", spec: "POSIX", body: 'node -e "console.log(1+1)"' },
  { id: "exit_code_3", group: "basic", spec: "POSIX", body: 'node -e "process.exit(3)"', expectExit: 3 },
  { id: "and_or", group: "basic", spec: "POSIX", body: "false || echo fallback" },
  { id: "semicolon_list", group: "basic", spec: "POSIX", body: "echo one; echo two" },
  { id: "pipeline", group: "basic", spec: "POSIX", body: "echo piped | cat" },
  { id: "subshell", group: "basic", spec: "POSIX", body: "(echo in-subshell)" },
  { id: "bang_negate", group: "basic", spec: "POSIX", body: "! false && echo negated" },

  // ── POSIX-isms real npm scripts use ──────────────────────────────────────
  { id: "rm_rf_mkdir", group: "posix", spec: "POSIX", body: "rm -rf dist && mkdir dist && echo made" },
  { id: "cp_cat", group: "posix", spec: "POSIX", body: "cp a.txt b.txt && cat b.txt" },
  { id: "mv", group: "posix", spec: "POSIX", body: "cp a.txt c.txt && mv c.txt d.txt && cat d.txt" },
  {
    id: "inline_env_prefix",
    group: "posix",
    spec: "POSIX",
    body: 'NODE_ENV=production node -e "console.log(process.env.NODE_ENV)"',
  },
  { id: "redirect_then_cat", group: "posix", spec: "POSIX", body: "echo hi > out.txt && cat out.txt" },
  { id: "append_redirect", group: "posix", spec: "POSIX", body: "echo a > ap.txt && echo b >> ap.txt && cat ap.txt" },
  { id: "dev_null", group: "posix", spec: "POSIX", body: "echo noisy > /dev/null && echo quiet" },
  // busybox-w32's README warns its device emulations "can't be used as
  // arguments to other programs" — only as redirection targets. This row is the
  // difference, and `--log=/dev/null`-shaped args are common in npm scripts.
  {
    id: "dev_null_as_arg",
    group: "posix",
    spec: "POSIX",
    body: `node -e "const fs=require('fs');try{fs.appendFileSync(process.argv[1],'x');console.log('OPENED-OK')}catch(e){console.log('OPEN-FAIL '+e.code)}" /dev/null`,
  },
  { id: "export_var", group: "posix", spec: "POSIX", body: 'export FOO=bar && node -e "console.log(process.env.FOO)"' },
  { id: "glob", group: "posix", spec: "POSIX", body: "echo *.txt" },
  { id: "test_builtin", group: "posix", spec: "POSIX", body: "test -f a.txt && echo file-exists" },
  { id: "bracket_test", group: "posix", spec: "POSIX", body: "[ -f a.txt ] && echo bracket-ok" },
  { id: "exit_status_var", group: "posix", spec: "POSIX", body: "false; echo status=$?" },
  { id: "set_e", group: "posix", spec: "POSIX", body: "set -e; false; echo NOT-REACHED", expectExit: 1 },

  // ── parameter expansion (POSIX §2.6.2) ───────────────────────────────────
  // deno_task_shell supports NONE of the braced forms and fails them SILENTLY.
  { id: "expand_bare", group: "expand", spec: "POSIX", body: "V=abc; echo bare=$V" },
  { id: "expand_braced", group: "expand", spec: "POSIX", body: "V=abc; echo braced=${V}" },
  { id: "expand_env_bare", group: "expand", spec: "POSIX", body: "echo user=$USERNAME" },
  { id: "expand_env_braced", group: "expand", spec: "POSIX", body: "echo user=${USERNAME}" },
  { id: "expand_default", group: "expand", spec: "POSIX", body: "echo mem=${NODE_MEM:-4096}" },
  { id: "expand_alt", group: "expand", spec: "POSIX", body: "V=x; echo alt=${V:+set}" },
  { id: "expand_prefix_strip", group: "expand", spec: "POSIX", body: "V=abcdef; echo strip=${V#abc}" },
  { id: "expand_suffix_strip", group: "expand", spec: "POSIX", body: "V=abcdef; echo strip=${V%def}" },
  { id: "expand_length", group: "expand", spec: "POSIX", body: "V=abcdef; echo len=${#V}" },
  { id: "cmd_subst", group: "expand", spec: "POSIX", body: 'echo sub=$(node -e "console.log(42)")' },
  { id: "cmd_subst_backtick", group: "expand", spec: "POSIX", body: "echo sub=`echo bt`" },
  { id: "arith_expansion", group: "expand", spec: "POSIX", body: "echo port=$((3000+1))" },
  { id: "dollar_zero", group: "expand", spec: "POSIX", body: "echo zero=[$0]" },
  { id: "tilde", group: "expand", spec: "POSIX", body: "echo ~" },

  // ── redirection (POSIX §2.7) — MULTIPLE redirects on one command ─────────
  {
    id: "multi_redirect_2gt1",
    group: "redir",
    spec: "POSIX",
    body: 'node -e "console.error(\'E\');console.log(\'O\')" > log.txt 2>&1 && cat log.txt',
  },
  {
    id: "multi_redirect_split",
    group: "redir",
    spec: "POSIX",
    body: 'node -e "console.error(\'E\');console.log(\'O\')" > o.txt 2> e.txt && cat o.txt && cat e.txt',
  },
  { id: "heredoc", group: "redir", spec: "POSIX", body: "cat <<EOF\nheredoc-line\nEOF\n" },
  { id: "stdin_redirect", group: "redir", spec: "POSIX", body: "cat < a.txt" },

  // ── control flow (POSIX §2.9.4 compound commands) ────────────────────────
  { id: "for_loop", group: "flow", spec: "POSIX", body: "for i in a b c; do echo item=$i; done" },
  { id: "while_loop", group: "flow", spec: "POSIX", body: "i=0; while [ $i -lt 2 ]; do echo w=$i; i=$((i+1)); done" },
  { id: "if_then", group: "flow", spec: "POSIX", body: "if true; then echo yes; else echo no; fi" },
  { id: "case_stmt", group: "flow", spec: "POSIX", body: "case abc in a*) echo matched;; *) echo nope;; esac" },
  { id: "function_def", group: "flow", spec: "POSIX", body: "f() { echo fn-ok; }; f" },
  { id: "dot_source", group: "flow", spec: "POSIX", body: ". ./dot.sh" },
  { id: "trap_exit", group: "flow", spec: "POSIX", body: "trap 'echo TRAPPED' EXIT; echo body" },
  // `done` / `then` / `in` used as ORDINARY ARGUMENTS — deno_task_shell 0.33.0
  // rejects these as reserved words. Ubiquitous in real script bodies.
  { id: "reserved_word_arg", group: "flow", spec: "POSIX", body: "mkdir -p dist2 && echo done" },

  // ── quoting — the exact apostrophe round-trip nub's own escaper emits ────
  { id: "apostrophe_roundtrip", group: "quote", spec: "POSIX", body: String.raw`echo greet 'it'\''s'` },
  { id: "dquote_in_squote", group: "quote", spec: "POSIX", body: `echo 'a "b" c'` },
  { id: "backslash_escape", group: "quote", spec: "POSIX", body: String.raw`echo a\ b` },

  // ── bash extensions (BONUS — not required by the npm compat bar) ─────────
  { id: "double_bracket", group: "bashext", spec: "bash", body: "[[ -f a.txt ]] && echo dbracket-ok" },
  { id: "brace_expansion", group: "bashext", spec: "bash", body: "echo {a,b,c}" },
  { id: "here_string", group: "bashext", spec: "bash", body: "cat <<< here-string" },
  { id: "upper_expansion", group: "bashext", spec: "bash", body: "V=abc; echo ${V^^}" },
  { id: "local_in_fn", group: "bashext", spec: "bash", body: "f() { local x=1; echo local=$x; }; f" },

  // ── Windows path handling (no spec; hard practical requirement) ──────────
  // MSYS2 (Git Bash) rewrites POSIX-looking arguments into Windows paths when
  // handing them to a non-MSYS program. Quantify the tax both directions.
  // `printargv.js` (not `node -e`) so a `--out=…` value reaches process.argv
  // instead of being eaten by node's own option parser.
  { id: "path_echo_posix", group: "path", spec: "win", body: "echo /c/Users" },
  { id: "path_arg_posix", group: "path", spec: "win", body: "node printargv.js /c/Users" },
  { id: "path_flag_posix", group: "path", spec: "win", body: "node printargv.js --out=/c/tmp/x" },
  { id: "path_slash_flag", group: "path", spec: "win", body: "node printargv.js /Q /nologo" },
  { id: "path_arg_win_raw", group: "path", spec: "win", body: String.raw`node printargv.js C:\Users\runneradmin` },
  {
    id: "path_arg_win_quoted",
    group: "path",
    spec: "win",
    body: String.raw`node printargv.js "C:\Users\runneradmin"`,
  },
  { id: "path_flag_win", group: "path", spec: "win", body: String.raw`node printargv.js --out=C:\path\to\thing` },
  { id: "path_prefix_unix", group: "path", spec: "win", body: "node printargv.js --prefix=/usr/local" },
  { id: "path_docker_volume", group: "path", spec: "win", body: "node printargv.js -v /c/Users/x:/app" },
  { id: "path_pwd", group: "path", spec: "POSIX", body: "pwd" },
  { id: "path_cd_persist", group: "path", spec: "POSIX", body: "mkdir -p sub && cd sub && pwd" },

  // ── the known hang modes (timeout-guarded) ───────────────────────────────
  {
    id: "pipe_stderr_merge",
    group: "hang",
    spec: "POSIX",
    guard: true,
    body: 'node -e "console.error(\'E\');console.log(\'O\')" 2>&1 | cat',
  },
  {
    id: "background_amp",
    group: "hang",
    spec: "POSIX",
    guard: true,
    body: 'node -e "setInterval(()=>{},1000)" & echo started',
  },
  { id: "background_bounded", group: "hang", spec: "POSIX", guard: true, body: "sleep 2 & echo bg-started" },

  // ── fork cost ────────────────────────────────────────────────────────────
  // 200 command substitutions = 200 subshells. Read the RESULT line's elapsed
  // field: this is the load-bearing number for whether an MSYS/Cygwin fork
  // emulation (Git Bash) is materially slower than a native-Win32 shell
  // (busybox-w32, brush) at the thing script bodies do constantly.
  {
    id: "fork_cost_200_subshells",
    group: "perf",
    spec: "POSIX",
    guard: true,
    body: "i=0; while [ $i -lt 200 ]; do x=$(true); i=$((i+1)); done; echo forks-done",
  },
];

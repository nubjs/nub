// Branch-scoped Windows probe for nub's bundled busybox-w32 script shell.
//
// Unlike tests/win-shell-probe/ (which drove candidate SHELLS standalone to pick
// one), this drives the INTEGRATED `nub run` end-to-end: it asserts that a real
// built nub, with busybox.exe staged next to nub.exe exactly as the win32 package
// lays it out, resolves and runs POSIX script bodies on Windows — the sidecar
// resolution (resolve_bundled_busybox), the /tmp + TMPDIR glue, .bin/*.cmd shim
// resolution, and the `--script-shell` override all through the actual nub CLI.
//
// Usage: node run-probe.mjs <path-to-nub.exe> <work-dir> [git-bash.exe]
// Exit 0 iff every case passes; prints one greppable RESULT line per case.

import { spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync, rmSync, chmodSync, copyFileSync, existsSync } from "node:fs";
import { join, resolve, dirname } from "node:path";

const nubBin = resolve(process.argv[2] ?? "");
const workRoot = resolve(process.argv[3] ?? "work");
const gitBash = process.argv[4] ?? "C:\\Program Files\\Git\\bin\\bash.exe";
if (!process.argv[2]) {
  process.stderr.write("usage: run-probe.mjs <nub.exe> <work-dir> [git-bash.exe]\n");
  process.exit(2);
}
// The caller stages busybox.exe beside nub.exe (the production sidecar layout). The
// relocated-layout case below sources its copy from there, so a missing one is a
// harness setup error, not a nub defect — say so rather than throwing mid-run or
// reporting a wall of failures that look like the binary's fault.
const sidecarSrc = join(dirname(nubBin), "busybox.exe");
if (!existsSync(sidecarSrc)) {
  process.stderr.write(`setup error: no busybox.exe beside ${nubBin} — stage it first\n`);
  process.exit(2);
}

// ── fixture: package.json scripts + a real npm-style node_modules/.bin ────────
const fixture = join(workRoot, "fixture");
rmSync(fixture, { recursive: true, force: true });
mkdirSync(fixture, { recursive: true });

const bin = join(fixture, "node_modules", ".bin");
mkdirSync(bin, { recursive: true });
for (const tool of ["mytool", "onlycmd"]) {
  const pkgBin = join(fixture, "node_modules", tool, "bin");
  mkdirSync(pkgBin, { recursive: true });
  writeFileSync(
    join(pkgBin, `${tool}.js`),
    `console.log("TOOL_OK name=${tool} argv=" + JSON.stringify(process.argv.slice(2)));\n`,
  );
}

// npm 10's generated shim bodies — the real triplet, so shim resolution is tested
// against what `npm install` actually leaves behind (not a simplification).
const cmdShim = (tool) => `@ECHO off\r
GOTO start\r
:find_dp0\r
SET dp0=%~dp0\r
EXIT /b\r
:start\r
SETLOCAL\r
CALL :find_dp0\r
IF EXIST "%dp0%\\node.exe" (SET "_prog=%dp0%\\node.exe") ELSE (SET "_prog=node" & SET PATHEXT=%PATHEXT:;.JS;=;%)\r
endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & "%_prog%"  "%dp0%\\..\\${tool}\\bin\\${tool}.js" %*\r
`;
const shShim = (tool) => `#!/bin/sh
basedir=$(dirname "$(echo "$0" | sed -e 's,\\\\,/,g')")
if [ -x "$basedir/node" ]; then exec "$basedir/node"  "$basedir/../${tool}/bin/${tool}.js" "$@"
else exec node  "$basedir/../${tool}/bin/${tool}.js" "$@"; fi
`;
function shim(path, body, exec) {
  writeFileSync(path, body);
  if (exec) try { chmodSync(path, 0o755); } catch { /* no exec bit on this fs */ }
}
// mytool: the full triplet (.cmd + extensionless sh). onlycmd: .cmd ONLY, so
// resolving it requires PATHEXT-style extension probing.
shim(join(bin, "mytool.cmd"), cmdShim("mytool"));
shim(join(bin, "mytool"), shShim("mytool"), true);
shim(join(bin, "onlycmd.cmd"), cmdShim("onlycmd"));

writeFileSync(
  join(fixture, "package.json"),
  JSON.stringify(
    {
      name: "busybox-run-probe-fixture",
      version: "0.0.0",
      scripts: {
        shim: "mytool --flag value",
        shimcmd: "onlycmd --flag value",
        rmmkdir: "rm -rf dist && mkdir dist && echo made",
        envprefix: 'NODE_ENV=production node -e "console.log(process.env.NODE_ENV)"',
        braced: "echo mem=${NODE_MEM:-4096}",
        cmdsubst: 'echo sub=$(node -e "console.log(42)")',
        forloop: "for i in a b c; do echo item=$i; done",
        // busybox-w32 historically did its OWN argv globbing; confirm the shell
        // (not the applet) expands an unquoted pattern to the matching paths. The
        // pattern is its own word — a `GLOB:g/*.txt` (prefix fused to the pattern)
        // matches no file and POSIX leaves it literal on EVERY shell, so keep the
        // marker and the glob separate words.
        glob: "mkdir -p g && echo 1 > g/a.txt && echo 2 > g/b.txt && echo GLOB: g/*.txt",
        // The load-bearing augmentation property, probed not inferred: a `node`
        // spawned INSIDE a busybox script must inherit nub's NODE_OPTIONS preload
        // (busybox passes the inherited env to children), so it transpiles a
        // TS-only annotation. A bare env_clear (the closed deno_task_shell path)
        // would strip the preload and this would throw on `: number`.
        transpile: "node probe.ts",
        tmpredir: "echo hi > /tmp/nub_probe_out.txt && cat /tmp/nub_probe_out.txt",
        // $BASH_VERSION is set only by real bash — empty under busybox sh. Run once
        // on the default shell (must be `none` ⇒ busybox) and once via
        // `--script-shell <bash>` (must be a version ⇒ the override reached bash).
        bashver: "echo bashver=${BASH_VERSION:-none}",
      },
    },
    null,
    2,
  ),
);

// TS-only syntax: transpiles under nub's augmentation, throws under plain node —
// so the `transpile` case proves the preload reached the busybox-spawned child.
writeFileSync(join(fixture, "probe.ts"), 'const n: number = 7;\nconsole.log("TS_OK=" + n);\n');

// ── run each case through the integrated `nub run` ───────────────────────────
function run(exe, extraArgs, script) {
  const r = spawnSync(exe, ["run", ...extraArgs, script], {
    cwd: fixture,
    encoding: "utf8",
    timeout: 60000,
  });
  return { out: (r.stdout ?? "") + (r.stderr ?? ""), code: r.status, flag: r.error?.code };
}

const cases = [
  { id: "shim_both", args: [], script: "shim", ok: (o) => o.includes("TOOL_OK name=mytool") },
  { id: "shim_only_cmd", args: [], script: "shimcmd", ok: (o) => o.includes("TOOL_OK name=onlycmd") },
  { id: "posix_rm_mkdir", args: [], script: "rmmkdir", ok: (o) => o.includes("made") },
  { id: "posix_env_prefix", args: [], script: "envprefix", ok: (o) => /(^|\W)production(\W|$)/.test(o) },
  { id: "posix_braced_default", args: [], script: "braced", ok: (o) => o.includes("mem=4096") },
  { id: "posix_cmd_subst", args: [], script: "cmdsubst", ok: (o) => o.includes("sub=42") },
  {
    id: "posix_for_loop",
    args: [],
    script: "forloop",
    ok: (o) => o.includes("item=a") && o.includes("item=b") && o.includes("item=c"),
  },
  {
    id: "posix_glob",
    args: [],
    script: "glob",
    // Expanded stdout carries `g/a.txt g/b.txt`; the `$ …` preamble only ever
    // shows the unexpanded `g/*.txt`, so matching both paths proves expansion.
    ok: (o) => o.includes("g/a.txt") && o.includes("g/b.txt"),
  },
  // Augmentation reaches a node child spawned by a busybox script (NODE_OPTIONS
  // preload inherited → TS annotation transpiled, not a SyntaxError).
  { id: "augmentation_transpiles_ts", args: [], script: "transpile", ok: (o) => o.includes("TS_OK=7") },
  { id: "posix_tmp_redirect", args: [], script: "tmpredir", ok: (o) => o.includes("hi") },
  // The default shell must be busybox, NOT a system bash: $BASH_VERSION is unset.
  { id: "default_shell_is_busybox", args: [], script: "bashver", ok: (o) => o.includes("bashver=none") },
  // `--script-shell` must still reach a real native shell (Git Bash), which DOES
  // set $BASH_VERSION — proving the override bypasses busybox.
  {
    id: "script_shell_override_reaches_bash",
    args: ["--script-shell", gitBash],
    script: "bashver",
    ok: (o) => /bashver=\d/.test(o) && !o.includes("bashver=none"),
  },
];

let failures = 0;
let total = 0;

function runMatrix(exe, layout) {
  process.stdout.write(`\n--- layout=${layout} exe=${exe}\n`);
  for (const c of cases) {
    const { out, code, flag } = run(exe, c.args, c.script);
    const pass = flag === undefined && c.ok(out);
    total++;
    if (!pass) failures++;
    const first = out.split(/\r?\n/).find((l) => l.trim()) ?? "";
    process.stdout.write(
      `RESULT|${layout}|${c.id}|exit=${code}|${flag ? `flag=${flag}|` : ""}${pass ? "PASS" : "FAIL"}|${JSON.stringify(first.slice(0, 160))}\n`,
    );
    if (!pass) process.stdout.write(`  FULL|${c.id}|${JSON.stringify(out.slice(0, 800))}\n`);
  }
}

function check(id, cond, detail) {
  total++;
  if (!cond) failures++;
  process.stdout.write(`RESULT|${id}|${cond ? "PASS" : "FAIL"}|${JSON.stringify(String(detail).slice(0, 300))}\n`);
}

// (1) SIDECAR layout — busybox.exe beside nub.exe, how the win32 package, the release
// .zip behind install.ps1, and `nub upgrade`'s ~/.nub/bin all lay it out.
runMatrix(nubBin, "sidecar");

// (2) RELOCATED layout — the #687 shape. The npm launcher hardlinks nub.exe into npm's
// global bin dir for the PATHEXT fast path, which has no sibling busybox; the shell is
// carried into a nub-owned `nub-sh/` subdir instead (launch.js healWindowsBinDir, and
// cli.rs resolve_bundled_busybox's second candidate). Reproduce that exactly: the exe
// alone in a fresh dir, the shell ONLY under nub-sh/.
//
// The busybox source is the sidecar beside the real binary — the layout the caller
// staged for (1) — so this needs no second copy of the vendored blob.
const relocated = join(workRoot, "relocated");
rmSync(relocated, { recursive: true, force: true });
mkdirSync(join(relocated, "nub-sh"), { recursive: true });
const relocatedExe = join(relocated, "nub.exe");
copyFileSync(nubBin, relocatedExe);
copyFileSync(sidecarSrc, join(relocated, "nub-sh", "busybox.exe"));
check(
  "relocated_layout_has_no_sibling_busybox",
  !existsSync(join(relocated, "busybox.exe")),
  "the relocated dir must NOT have a sibling busybox, or this proves nothing",
);
runMatrix(relocatedExe, "relocated");

// (3) NEGATIVE CONTROL. Without it (1) and (2) are unfalsifiable: if the binary ever
// found a busybox some third way (one on PATH, a stale absolute default), every case
// above would pass while the resolution under test did nothing. A nub.exe with NEITHER
// candidate present must fail, and must fail with the specific shell-resolution error
// rather than some unrelated non-zero exit.
const bare = join(workRoot, "bare");
rmSync(bare, { recursive: true, force: true });
mkdirSync(bare, { recursive: true });
const bareExe = join(bare, "nub.exe");
copyFileSync(nubBin, bareExe);
const neg = run(bareExe, [], "braced");
check(
  "no_busybox_anywhere_fails_cleanly",
  neg.code !== 0 && /bundled POSIX shell/.test(neg.out),
  `exit=${neg.code} out=${neg.out.slice(0, 200)}`,
);

process.stdout.write(`\nSUMMARY|${total - failures}/${total} passed\n`);
process.exit(failures === 0 ? 0 : 1);

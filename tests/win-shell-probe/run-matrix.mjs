// Runs the case matrix (cases.mjs) against ONE candidate shell and prints a
// greppable result line per case, so the findings survive in the CI run log
// without needing the artifact.
//
// Invocation mirrors how nub actually spawns a script shell today
// (crates/nub-cli/src/cli.rs ~3819-3890):
//   POSIX shell  →  <shell> -c <body>          (normal argv escaping)
//   cmd.exe      →  cmd /d /s /c <body>        (windowsVerbatimArguments — the
//                                               body reaches cmd exactly as
//                                               written, npm's behavior)
//
// Robustness: every child's stdio is redirected to FILES, never pipes. Two of
// the cases are known to wedge at least one candidate (`2>&1 |` deadlock,
// background `&` wait), and a wedged GRANDCHILD holding a pipe write-end would
// otherwise hang the driver itself past the per-case timeout. Files make the
// timeout authoritative. Results are written with fs.writeSync so the log is
// complete up to the point of any hang.

import { spawnSync } from "node:child_process";
import { openSync, closeSync, readFileSync, mkdirSync, copyFileSync, writeSync, rmSync } from "node:fs";
import { join, resolve } from "node:path";
import { CASES } from "./cases.mjs";

const argv = process.argv.slice(2);
function opt(name, dflt) {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 ? argv[i + 1] : dflt;
}

const shellId = opt("shell");
const shellBin = opt("bin");
const fixture = resolve(opt("fixture", "fixture"));
const workRoot = resolve(opt("work", join(fixture, "..", "work")));
const timeoutMs = Number(opt("timeout", "12000"));
const timingIters = Number(opt("timing-iters", "20"));
// Argv inserted BEFORE `-c`. busybox is a multi-call binary: the applet name is
// mandatory (`busybox64.exe ash -c <body>`), and `-X` is its opt-out for the
// backslash→forward-slash argument rewriting.
const argvPrefix = (opt("argv-prefix", "") || "").split(",").filter(Boolean);

if (!shellId || !shellBin) {
  process.stderr.write("usage: run-matrix.mjs --shell <id> --bin <path> [--fixture <dir>]\n");
  process.exit(2);
}

const out = (s) => writeSync(1, s);

function esc(buf) {
  let s = buf.toString("utf8");
  if (s.length > 600) s = `${s.slice(0, 600)}…<truncated>`;
  return JSON.stringify(s);
}

function buildSpawn(body) {
  if (shellId === "cmd") {
    // npm / nub's Windows default: the body is handed to cmd VERBATIM.
    return { args: ["/d", "/s", "/c", body], verbatim: true };
  }
  return { args: [...argvPrefix, "-c", body], verbatim: false };
}

// PATH: the fixture's node_modules/.bin FIRST, exactly as `nub run` / `npm run`
// assemble it. This is what makes the shim cases meaningful.
const binDir = join(fixture, "node_modules", ".bin");
const childPath = `${binDir}${process.platform === "win32" ? ";" : ":"}${process.env.PATH ?? ""}`;
// Strip every case-variant of PATH before re-adding exactly one. Windows env
// blocks are case-insensitive but a JS object is not, so a naive
// `{...process.env, PATH: x}` would hand the child BOTH `Path` and `PATH` and
// let libuv pick — the exact nondeterminism this probe must not introduce.
const childEnv = {};
for (const [k, v] of Object.entries(process.env)) {
  if (k.toLowerCase() !== "path") childEnv[k] = v;
}
childEnv.PATH = childPath;

// ── header ───────────────────────────────────────────────────────────────────
out(`\n##### SHELL ${shellId} @ ${shellBin}\n`);
{
  const v = spawnSync(shellBin, shellId === "cmd" ? ["/d", "/s", "/c", "ver"] : [...argvPrefix, "--version"], {
    encoding: "utf8",
    windowsVerbatimArguments: shellId === "cmd",
  });
  out(`VERSION|${shellId}|${JSON.stringify(((v.stdout || "") + (v.stderr || "")).trim().split("\n")[0] || "?")}\n`);
}

// ── the matrix ───────────────────────────────────────────────────────────────
const rows = [];
for (const c of CASES) {
  const cwd = join(workRoot, shellId, c.id);
  rmSync(cwd, { recursive: true, force: true });
  mkdirSync(cwd, { recursive: true });
  for (const f of ["a.txt", "dot.sh", "printargv.js"]) copyFileSync(join(fixture, f), join(cwd, f));

  const outPath = join(cwd, "__stdout");
  const errPath = join(cwd, "__stderr");
  const ofd = openSync(outPath, "w");
  const efd = openSync(errPath, "w");
  const { args, verbatim } = buildSpawn(c.body);

  const t0 = Date.now();
  const r = spawnSync(shellBin, args, {
    cwd,
    stdio: ["ignore", ofd, efd],
    env: childEnv,
    timeout: timeoutMs,
    killSignal: "SIGKILL",
    windowsVerbatimArguments: verbatim,
  });
  const elapsed = Date.now() - t0;
  closeSync(ofd);
  closeSync(efd);

  const so = readFileSync(outPath);
  const se = readFileSync(errPath);
  let flag = "ok";
  if (r.error) flag = r.error.code === "ETIMEDOUT" ? "TIMEOUT" : `SPAWN_ERR:${r.error.code}`;
  else if (r.signal) flag = `SIGNAL:${r.signal}`;
  const code = r.status === null || r.status === undefined ? "null" : String(r.status);

  out(`BODY|${shellId}|${c.id}|${c.spec}|${JSON.stringify(c.body)}\n`);
  out(`RESULT|${shellId}|${c.id}|${c.group}|${c.spec}|${code}|${flag}|${elapsed}ms|${esc(so)}|${esc(se)}\n`);
  rows.push({
    id: c.id,
    group: c.group,
    spec: c.spec,
    expectExit: c.expectExit ?? 0,
    code,
    flag,
    elapsed,
    so: so.toString(),
    se: se.toString(),
  });
}

// ── startup overhead ─────────────────────────────────────────────────────────
{
  const trivial = shellId === "cmd" ? ["/d", "/s", "/c", "exit /b 0"] : [...argvPrefix, "-c", "exit 0"];
  // one warm-up spawn so the first-run page-in cost is not counted
  spawnSync(shellBin, trivial, { stdio: "ignore", windowsVerbatimArguments: shellId === "cmd" });
  const t0 = Date.now();
  for (let i = 0; i < timingIters; i++) {
    spawnSync(shellBin, trivial, { stdio: "ignore", windowsVerbatimArguments: shellId === "cmd" });
  }
  const total = Date.now() - t0;
  out(`TIMING|${shellId}|${timingIters}|${total}ms total|${(total / timingIters).toFixed(1)}ms mean\n`);
}

// ── human-scannable summary ──────────────────────────────────────────────────
const firstLine = (s) => (s.split(/\r?\n/).find((l) => l.trim().length) ?? "").slice(0, 72);
out(`\n##### SUMMARY ${shellId}\n`);
out(`SUM|${"case".padEnd(24)}|${"spec".padEnd(5)}|exit|flag    |first stdout line\n`);
for (const r of rows) {
  out(
    `SUM|${r.id.padEnd(24)}|${r.spec.padEnd(5)}|${r.code.padStart(4)}|${r.flag.padEnd(8)}|${firstLine(r.so) || `(stderr) ${firstLine(r.se)}`}\n`,
  );
}
const posix = rows.filter((r) => r.spec === "POSIX");
const npmRows = rows.filter((r) => r.spec === "npm");
// A coarse screen only — a zero exit does NOT mean the case behaved correctly
// (deno_task_shell's whole `${…}` class fails SILENTLY with exit 0 and a
// literal passthrough). Read the per-case stdout for the real verdict.
const bad = (r) => r.flag !== "ok" || r.code !== String(r.expectExit);
out(
  `TALLY|${shellId}|posix_nonzero_or_flagged=${posix.filter(bad).length}/${posix.length}|npm_shim_nonzero_or_flagged=${npmRows.filter(bad).length}/${npmRows.length}\n`,
);

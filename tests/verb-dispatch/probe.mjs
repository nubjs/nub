// Verb-dispatch probe: prove `nub` and `nubx` both dispatch correctly when the
// platform package ships ONE binary.
//
// The platform packages used to ship two byte-identical copies of the 45 MB binary,
// purely so the Rust CLI could read its verb off argv[0]'s basename. That doubled every
// package past cnpm/npmmirror's 80 MiB sync cap. The verb now travels in `__NUB_ARGV0`
// (npm/nub/bin/launch.js sets it on both dispatch paths; Argv0::capture_argv0_override
// reads it and erases it).
//
// Runs on Linux and Windows because the two exercise DIFFERENT paths and only one of
// them is covered elsewhere:
//   - POSIX: first call goes through Node, which then heals the on-PATH entry into an
//     sh trampoline; every later call is sh -> exec, and the trampoline is the only
//     thing carrying the verb. tests/launcher/ covers this with a fake native.
//   - Windows: the heal is a no-op, so EVERY call goes through the Node launcher and
//     the verb rides on `opts.env`. Nothing covered this before, because before this
//     change Windows got a correctly-named `nubx.exe` and needed no verb plumbing.
//
// Usage: node tests/verb-dispatch/probe.mjs <path-to-built-nub-binary>
//
// Reproduce the CI condition locally with `CI=true node tests/verb-dispatch/probe.mjs …`.
// nub gates registry fetches on CI, so a bare local run does NOT exercise the same paths —
// a probe bug here passed on macOS and failed on the runner for exactly that reason.

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const nativeSrc = path.resolve(process.argv[2] ?? "");
const isWin = process.platform === "win32";
const exe = isWin ? ".exe" : "";

let pass = 0;
const failures = [];
const ok = (m) => { console.log(`  ok: ${m}`); pass++; };
const no = (m) => { console.log(`  FAIL: ${m}`); failures.push(m); };

if (!fs.existsSync(nativeSrc)) {
  console.error(`probe: no binary at ${nativeSrc}`);
  process.exit(2);
}

// ── fixture: a real node_modules tree with a ONE-BINARY platform package ──────────
const root = fs.mkdtempSync(path.join(os.tmpdir(), "nub-verb-"));
const launcher = path.join(root, "node_modules", "@nubjs", "nub");
const hostPkg = path.join(root, "node_modules", "@nubjs", "nub-host");
fs.mkdirSync(path.join(launcher, "bin"), { recursive: true });
fs.mkdirSync(path.join(hostPkg, "bin"), { recursive: true });

for (const f of ["nub", "nubx", "launch.js"]) {
  const dest = path.join(launcher, "bin", f);
  fs.copyFileSync(path.join(repo, "npm", "nub", "bin", f), dest);
  // npm sets 0o755 on every `bin`-field entry at install time regardless of the mode in
  // the tarball, so a fixture built by raw copy has to do the same or it is not modelling
  // a real install. (Worth knowing: `bin/nubx` is committed 0o644 while `bin/nub` is
  // 0o755, so without this the nubx stub is simply not executable and spawn returns
  // empty output with no error — which is exactly how this bit the probe first time.)
  if (!isWin) fs.chmodSync(dest, 0o755);
}
// Map every platform to the fake host package so resolveBinary finds our binary.
fs.writeFileSync(
  path.join(launcher, "platform.js"),
  'module.exports={platformPackage(){return{key:"host",pkg:"@nubjs/nub-host"};}};\n',
);
fs.writeFileSync(
  path.join(launcher, "package.json"),
  JSON.stringify({ name: "@nubjs/nub", version: "9.9.9", optionalDependencies: { "@nubjs/nub-host": "9.9.9" } }),
);
fs.writeFileSync(
  path.join(hostPkg, "package.json"),
  JSON.stringify({ name: "@nubjs/nub-host", version: "9.9.9", files: ["bin"] }),
);

// THE change under test: one binary, named `nub`, for both verbs.
fs.copyFileSync(nativeSrc, path.join(hostPkg, "bin", `nub${exe}`));
if (!isWin) fs.chmodSync(path.join(hostPkg, "bin", "nub"), 0o755);

const shipped = fs.readdirSync(path.join(hostPkg, "bin"));
shipped.length === 1 && shipped[0] === `nub${exe}`
  ? ok(`platform package ships exactly one binary (${shipped[0]})`)
  : no(`platform package should ship only nub${exe}, has: ${shipped.join(", ")}`);

// ── helpers ───────────────────────────────────────────────────────────────────────
const NUBX_MARK = "Run a tool from";       // nubx --help
const NUB_MARK = "all-in-one";             // nub --help

// Merge both streams and fold the exit status into the label. A silent failure here
// (non-executable stub, missing binary) otherwise reads as an empty string, which says
// nothing about why — the first run of this probe reported `??()` for a plain EACCES.
function verbOf(res) {
  const out = `${res.stdout ?? ""}${res.stderr ?? ""}`;
  if (out.includes(NUBX_MARK)) return "nubx";
  if (out.includes(NUB_MARK)) return "nub";
  const why = res.error ? res.error.message : `exit=${res.status}`;
  return `??(${why}: ${out.replace(/\s+/g, " ").trim().slice(0, 80) || "<no output>"})`;
}

const runNode = (stub, env = {}) =>
  spawnSync(process.execPath, [path.join(launcher, "bin", stub), "--help"], {
    encoding: "utf8", env: { ...process.env, ...env },
  });

// ── 1. the Node launcher path — Windows takes this on EVERY call ─────────────────
console.log(`== node launcher path (${process.platform}) ==`);
for (const [stub, want] of [["nub", "nub"], ["nubx", "nubx"]]) {
  const got = verbOf(runNode(stub));
  got === want ? ok(`node bin/${stub} -> ${want} mode`) : no(`node bin/${stub} -> ${got}, want ${want}`);
}

// ── 2. the healed fast path — POSIX only (heal is a no-op on Windows) ────────────
// Regression guard: deleting the duplicate WITHOUT teaching the trampoline the verb
// leaves call 1 correct (the node path falls back to bin/nub and sets argv0) and call 2
// silently running `nub`. Both calls are asserted for exactly that reason.
if (!isWin) {
  console.log("== healed fast path (posix) ==");
  const binDir = path.join(root, "bin");
  fs.mkdirSync(binDir, { recursive: true });
  for (const v of ["nub", "nubx"]) fs.symlinkSync(path.join(launcher, "bin", v), path.join(binDir, v));
  const env = { ...process.env, PATH: `${binDir}${path.delimiter}${process.env.PATH}` };
  const call = (v) => spawnSync(path.join(binDir, v), ["--help"], { encoding: "utf8", env });

  for (const v of ["nub", "nubx"]) {
    const first = verbOf(call(v));
    first === v ? ok(`${v} call 1 -> ${v} mode`) : no(`${v} call 1 -> ${first}`);

    const healed = fs.readFileSync(path.join(binDir, v), "utf8");
    healed.startsWith("#!/bin/sh")
      ? ok(`${v} entry healed to sh trampoline`)
      : no(`${v} entry not healed: ${healed.slice(0, 40)}`);
    // nub needs no assignment (it is the default); nubx must carry one.
    if (v === "nubx") {
      healed.includes("__NUB_ARGV0='nubx'")
        ? ok("nubx trampoline carries __NUB_ARGV0")
        : no(`nubx trampoline missing __NUB_ARGV0: ${healed.split("\n")[1]?.slice(0, 90)}`);
    }

    const second = verbOf(call(v));
    second === v
      ? ok(`${v} call 2 (through the trampoline) -> ${v} mode`)
      : no(`${v} call 2 -> ${second}, want ${v} — the trampoline lost the verb`);
  }
}

// ── 3. the variable must not survive into a child ────────────────────────────────
// nub spawns Node, and a script under it may re-enter nub. An inherited __NUB_ARGV0
// would silently put that grandchild in exec mode.
console.log("== env hygiene ==");
const script = path.join(root, "probe-env.js");
fs.writeFileSync(script, 'console.log("CHILD:" + String(process.env.__NUB_ARGV0));\n');
// A bare temp dir has no dependencies, so `nub <script>` warns and can exit non-zero
// (it does on Windows; on macOS it only warns). We are asserting what the CHILD SAW,
// not nub's exit status — so use spawnSync, which does not throw, and read the output
// either way. An execFileSync here failed the Windows leg on the warning alone.
// Deliberately `nub`, not `nubx`. The property under test is that the variable is
// REMOVED from the environment at startup, which capture_argv0_override does for any
// non-empty value — so the value is irrelevant to the assertion. Setting `nubx` here
// instead puts the PARENT in exec mode, which treats the script path as a tool to fetch
// and refuses under CI ("refusing to download … in CI"), failing the Windows leg for a
// reason that has nothing to do with env hygiene. It passed on macOS only because CI
// was unset there.
const res = spawnSync(path.join(hostPkg, "bin", `nub${exe}`), [script], {
  encoding: "utf8", env: { ...process.env, __NUB_ARGV0: "nub" },
});
const seen = `${res.stdout ?? ""}${res.stderr ?? ""}`;
if (seen.includes("CHILD:undefined")) ok("__NUB_ARGV0 erased before any child sees it");
else if (seen.includes("CHILD:nubx")) no("__NUB_ARGV0 LEAKED to child — erasure is not working");
else no(`env-hygiene child never reported (exit=${res.status}): ${seen.replace(/\s+/g, " ").trim().slice(0, 160)}`);

fs.rmSync(root, { recursive: true, force: true });
console.log(`\nRESULT: ${pass} ok, ${failures.length} failed`);
process.exit(failures.length === 0 ? 0 : 1);

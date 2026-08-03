// Probe for the lazy node-gyp shim on a real OS. Usage:
//
//   node tests/node-gyp-shim/run.mjs <path-to-nub>
//
// The shims live in `<cache>/nub/pm/tools/node-gyp/lazy-bin/` and only bootstrap
// the real node-gyp when something executes them. That path is exercised by
// nothing else in CI on Windows: `native-deps.yml` is the only workflow that
// touches node-gyp and it is ubuntu-only, so the `.cmd` shim ships unverified.
//
// Scenarios climb from "does the binary spawn" upward, cheapest first, so a
// failure names the smallest broken layer instead of an end-to-end symptom.
// Scenario 3 is a POSITIVE CONTROL for the bucket path: it must CREATE a bucket
// before scenario 4's "no bucket" assertion means anything. Asserting absence
// against a path nothing ever writes passes trivially and proves nothing.
import { spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, existsSync, readdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, delimiter } from "node:path";

const NUB = process.argv[2];
if (!NUB) {
  console.error("usage: run.mjs <path-to-nub>");
  process.exit(2);
}
const isWin = process.platform === "win32";
const UNREACHABLE = "registry=http://127.0.0.1:1/\nfetch-retries=0\n";

let failures = 0;
const results = [];

function check(name, ok, detail) {
  results.push({ name, ok, detail });
  if (!ok) failures++;
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${ok ? "" : `\n      ${detail}`}`);
}

// PATH with every directory that provides a node-gyp removed, so the only way a
// script can resolve one is through nub's shim. Mirrors the scrub in aube's
// node_gyp_bootstrap.bats setup().
function pathWithoutNodeGyp() {
  const names = isWin ? ["node-gyp.cmd", "node-gyp.exe", "node-gyp"] : ["node-gyp"];
  return (process.env.PATH || "")
    .split(delimiter)
    .filter((d) => d && !names.some((n) => existsSync(join(d, n))))
    .join(delimiter);
}
const CLEAN_PATH = pathWithoutNodeGyp();

function sandbox(tag) {
  const root = mkdtempSync(join(tmpdir(), `nubgyp-${tag}-`));
  for (const d of ["cache", "data", "home", "proj"]) mkdirSync(join(root, d));
  return root;
}

// Isolated roots. XDG_CACHE_HOME is honored ahead of %LOCALAPPDATA% on Windows
// (aube-store/src/dirs.rs), so one variable isolates the tool dir everywhere.
function env(root, extra = {}) {
  const e = {
    ...process.env,
    PATH: CLEAN_PATH,
    Path: CLEAN_PATH, // Windows env is case-insensitive but Node's object is not
    XDG_CACHE_HOME: join(root, "cache"),
    XDG_DATA_HOME: join(root, "data"),
    HOME: join(root, "home"),
    ...extra,
  };
  for (const [k, v] of Object.entries(extra)) if (v === undefined) delete e[k];
  return e;
}

function toolDir(root) {
  return join(root, "cache", "nub", "pm", "tools", "node-gyp");
}
// Any bootstrapped tree, whatever the pinned bucket is named. `lazy-bin` holds
// only the shims and is free to write, so it never counts as a bootstrap.
function buckets(root) {
  const d = toolDir(root);
  return existsSync(d) ? readdirSync(d).filter((n) => n !== "lazy-bin") : [];
}

function nub(root, args, extra) {
  return spawnSync(NUB, args, {
    cwd: join(root, "proj"),
    env: env(root, extra),
    encoding: "utf8",
    timeout: 300_000,
  });
}

// A dependency whose build script is `node marker.cjs` or `node-gyp --version`.
// The marker is a FILE, never `node -e "..."`: inline quoting is re-parsed by
// cmd.exe on Windows and by sh elsewhere, which is a portability trap unrelated
// to anything under test.
function fixture(root, { script, deps = 1 }) {
  const proj = join(root, "proj");
  const dependencies = {};
  const allowBuilds = {};
  for (let i = 1; i <= deps; i++) {
    const name = `d${i}`;
    mkdirSync(join(proj, name), { recursive: true });
    writeFileSync(join(proj, name, "marker.cjs"), `require("fs").writeFileSync("built-ok","ok")\n`);
    writeFileSync(
      join(proj, name, "package.json"),
      JSON.stringify({ name, version: "1.0.0", scripts: { postinstall: script } }),
    );
    dependencies[name] = `file:./${name}`;
    allowBuilds[`${name}@file:./${name}`] = true;
  }
  writeFileSync(
    join(proj, "package.json"),
    JSON.stringify({ name: "probe", version: "1.0.0", private: true, dependencies, allowBuilds }),
  );
  return proj;
}

// Run a lazy shim directly. Node cannot spawn a .cmd since CVE-2024-27980
// (bare name => ENOENT, .cmd spelling => EINVAL), so Windows goes through
// cmd.exe /c. The timeout is load-bearing: the shim is itself the `node-gyp`
// on PATH, so a bad fallback would re-exec itself forever, and a hang must
// register as a failure rather than stall the probe.
function runShim(root, args, extra) {
  const dir = join(toolDir(root), "lazy-bin");
  const [cmd, argv] = isWin
    ? ["cmd.exe", ["/c", join(dir, "node-gyp.cmd"), ...args]]
    : [join(dir, "node-gyp"), args];
  return spawnSync(cmd, argv, {
    cwd: join(root, "proj"),
    env: env(root, extra),
    encoding: "utf8",
    timeout: 60_000,
  });
}

console.log(`node-gyp shim probe — ${process.platform} ${process.arch}, node ${process.version}`);
console.log(`nub: ${NUB}`);
console.log(`node-gyp on PATH after scrub: ${spawnSync(isWin ? "where" : "which", ["node-gyp"], { env: { ...process.env, PATH: CLEAN_PATH }, encoding: "utf8" }).status === 0 ? "YES (probe is INVALID)" : "no"}\n`);

// 1 — the binary exists, is executable, spawns.
{
  const r = spawnSync(NUB, ["--version"], { encoding: "utf8", timeout: 60_000 });
  check("1 nub --version spawns", r.status === 0, `status=${r.status} err=${r.error?.message} ${r.stderr}`);
}

// 2 — fixture, store and linker work at all on this OS.
{
  const root = sandbox("plain");
  const proj = join(root, "proj");
  mkdirSync(join(proj, "d1"), { recursive: true });
  writeFileSync(join(proj, "d1", "package.json"), JSON.stringify({ name: "d1", version: "1.0.0" }));
  writeFileSync(
    join(proj, "package.json"),
    JSON.stringify({ name: "probe", version: "1.0.0", private: true, dependencies: { d1: "file:./d1" } }),
  );
  writeFileSync(join(proj, ".npmrc"), UNREACHABLE);
  const r = nub(root, ["install"]);
  check("2 install with no build scripts", r.status === 0, `status=${r.status} ${r.stdout}${r.stderr}`);
}

// 3 — POSITIVE CONTROL. An approved build that really calls node-gyp, with none
// on PATH, must resolve through the shim, bootstrap, and leave a bucket behind.
// Establishes that the bucket path scenario 4 asserts against is the real one.
const warm = sandbox("shim");
{
  fixture(warm, { script: "node-gyp --version" });
  const r = nub(warm, ["install"]);
  const got = buckets(warm);
  const printed = /v?\d+\.\d+\.\d+/.test(r.stdout + r.stderr);
  check(
    "3 approved build resolves node-gyp through the shim (POSITIVE CONTROL)",
    r.status === 0 && got.length > 0 && printed,
    `status=${r.status} buckets=[${got}] printedVersion=${printed}\n${r.stdout}${r.stderr}`,
  );
}

// 4 — THE REGRESSION. An approved build that never calls node-gyp must install
// with no registry reachable, and must not drag in the bootstrap. Meaningful
// only because scenario 3 proved a bucket does appear at this path when earned.
{
  const root = sandbox("unrelated");
  fixture(root, { script: "node marker.cjs" });
  writeFileSync(join(root, "proj", ".npmrc"), UNREACHABLE);
  const started = Date.now();
  const r = nub(root, ["install"]);
  const secs = ((Date.now() - started) / 1000).toFixed(1);
  const got = buckets(root);
  const ran = existsSync(join(root, "proj", "node_modules", "d1", "built-ok"));
  check(
    "4 non-gyp build installs offline and bootstraps nothing (THE REGRESSION)",
    r.status === 0 && got.length === 0 && ran,
    `status=${r.status} secs=${secs} buckets=[${got}] buildRan=${ran}\n${r.stdout}${r.stderr}`,
  );
}

// 5 — the hunk itself: AUBE_NODE_GYP_PROJECT_DIR unset must fall back to the
// shim's own cwd. Before the fix the sh shim aborted under `set -u` with
// "unbound variable", and the cmd shim expanded %VAR% literally and handed the
// bootstrap that text as a path.
{
  const r = runShim(warm, ["--version"], { AUBE_NODE_GYP_EXE: NUB, AUBE_NODE_GYP_PROJECT_DIR: undefined });
  const printed = /v?\d+\.\d+\.\d+/.test(r.stdout + r.stderr);
  check(
    "5 shim falls back to cwd when AUBE_NODE_GYP_PROJECT_DIR is unset",
    r.status === 0 && printed,
    `status=${r.status} signal=${r.signal} err=${r.error?.message}\n${r.stdout}${r.stderr}`,
  );
}

// 6 — a failing node-gyp must still surface as a non-zero exit. `setlocal` in
// the cmd shim and `exec` in the sh shim both have to preserve the child status,
// or a broken native build would read as a successful install.
//
// `configure` against a dir with no binding.gyp is the failing command: an
// unrecognized SUBCOMMAND is not, because node-gyp prints its usage and exits
// 0, which silently made this assertion vacuous on the first run.
//
// "Exited non-zero" alone is also not enough. While the Windows `.cmd` shim
// could not parse its own capture this passed for the wrong reason — the shim
// broke before node-gyp ever ran. The failure has to be gyp's, so require gyp's
// own output and reject the two shim-level breakages by name.
{
  const r = runShim(warm, ["configure"], { AUBE_NODE_GYP_EXE: NUB });
  const out = `${r.stdout}${r.stderr}`;
  const shimBroke = /syntax is incorrect|unbound variable/i.test(out);
  const gypSpoke = /gyp/i.test(out);
  check(
    "6 shim propagates a non-zero node-gyp exit",
    typeof r.status === "number" && r.status !== 0 && !shimBroke && gypSpoke,
    `status=${r.status} shimBroke=${shimBroke} gypSpoke=${gypSpoke}\n${out}`,
  );
}

// 7 — with no AUBE_NODE_GYP_EXE the shim must fail fast. The shim IS the
// `node-gyp` on PATH, so a fallback that re-resolves the bare name would
// re-exec itself forever; the timeout is what catches that.
{
  const r = runShim(warm, ["--version"], { AUBE_NODE_GYP_EXE: undefined, AUBE_NODE_GYP_PROJECT_DIR: undefined });
  const terminated = r.signal !== "SIGTERM" && !/timed out/i.test(r.error?.message || "");
  check(
    "7 shim fails fast without AUBE_NODE_GYP_EXE (no infinite re-exec)",
    terminated && typeof r.status === "number" && r.status !== 0,
    `status=${r.status} signal=${r.signal} err=${r.error?.message}\n${r.stdout}${r.stderr}`,
  );
}

// 8 — concurrent builds against a cold bucket. Every job races on the same
// bootstrap; `ensure_cached` serializes them on the tool dir's project lock.
// Worth running on Windows specifically, where the atomic-rename the shim
// writers use can fail against a handle another process still holds.
{
  const root = sandbox("race");
  fixture(root, { script: "node-gyp --version", deps: 6 });
  const r = nub(root, ["install"]);
  const got = buckets(root);
  check(
    "8 six concurrent builds share one cold bootstrap",
    r.status === 0 && got.length === 1,
    `status=${r.status} buckets=[${got}]\n${r.stdout}${r.stderr}`,
  );
}

// 9 — a real native addon compiled through the shim. The scenarios above prove
// node-gyp is REACHABLE; only an actual compile proves the thing it exists for
// works, and on Windows that is the MSVC path a `--version` call never touches.
{
  const root = sandbox("native");
  const proj = join(root, "proj");
  const dep = join(proj, "nativedep");
  mkdirSync(dep, { recursive: true });
  // No install script at all, so aube's default_install_script fallback has to
  // synthesize `node-gyp rebuild` off the binding.gyp — the implicit path most
  // native packages actually take.
  writeFileSync(join(dep, "package.json"), JSON.stringify({ name: "nativedep", version: "1.0.0" }));
  writeFileSync(join(dep, "binding.gyp"), JSON.stringify({ targets: [{ target_name: "hello", sources: ["hello.cc"] }] }));
  writeFileSync(
    join(dep, "hello.cc"),
    "#include <node_api.h>\nnapi_value Init(napi_env env, napi_value exports) { return exports; }\nNAPI_MODULE(NODE_GYP_MODULE_NAME, Init)\n",
  );
  writeFileSync(
    join(proj, "package.json"),
    JSON.stringify({
      name: "probe",
      version: "1.0.0",
      private: true,
      dependencies: { nativedep: "file:./nativedep" },
      allowBuilds: { "nativedep@file:./nativedep": true },
    }),
  );
  const r = nub(root, ["install"]);
  const built = existsSync(join(proj, "node_modules", "nativedep", "build", "Release", "hello.node"));
  check(
    "9 real binding.gyp addon compiles through the shim",
    r.status === 0 && built,
    `status=${r.status} builtArtifact=${built}\n${r.stdout}${r.stderr}`,
  );
}

// 10 — a node-gyp already on PATH wins and nothing is bootstrapped. On Windows
// this is also the only coverage of the three-name lookup in
// `node_gyp_bin_exists` (node-gyp.cmd / node-gyp.exe / bare), which decides
// whether the user's copy is seen at all.
{
  const root = sandbox("onpath");
  const fake = join(root, "fakebin");
  mkdirSync(fake);
  const stamp = join(root, "fake-ran");
  if (isWin) {
    // cmd resolves .cmd via PATHEXT; the bare name is what node_gyp_bin_exists
    // may match first, so provide both and have either record the hit.
    writeFileSync(join(fake, "node-gyp.cmd"), `@echo off\r\necho fake-node-gyp>"${stamp}"\r\necho v99.0.0\r\n`);
    writeFileSync(join(fake, "node-gyp"), "");
  } else {
    writeFileSync(join(fake, "node-gyp"), `#!/usr/bin/env sh\necho fake-node-gyp > "${stamp}"\necho v99.0.0\n`, { mode: 0o755 });
  }
  fixture(root, { script: "node-gyp --version" });
  const withFake = `${fake}${delimiter}${CLEAN_PATH}`;
  const r = spawnSync(NUB, ["install"], {
    cwd: join(root, "proj"),
    env: { ...env(root), PATH: withFake, Path: withFake },
    encoding: "utf8",
    timeout: 300_000,
  });
  const got = buckets(root);
  check(
    "10 a node-gyp already on PATH wins, nothing is bootstrapped",
    r.status === 0 && got.length === 0 && existsSync(stamp),
    `status=${r.status} buckets=[${got}] fakeRan=${existsSync(stamp)}\n${r.stdout}${r.stderr}`,
  );
}

// 11 — the `npm_config_node_gyp` channel, which is separate from PATH: npm and
// pnpm always set it and tools run `node $npm_config_node_gyp`. That resolves
// the `.js` shim, not the `sh`/`cmd` one, so it is its own path — and on
// Windows it re-spawns through `shell: true`, which PATH never exercises.
{
  const root = sandbox("cfgvar");
  const proj = fixture(root, { script: "node usegyp.cjs" });
  writeFileSync(
    join(proj, "d1", "usegyp.cjs"),
    [
      'const { spawnSync } = require("child_process");',
      "const shim = process.env.npm_config_node_gyp;",
      'if (!shim) { console.error("npm_config_node_gyp unset"); process.exit(3); }',
      'const r = spawnSync(process.execPath, [shim, "--version"], { stdio: "inherit" });',
      "process.exit(r.status === null ? 1 : r.status);",
    ].join("\n"),
  );
  const r = nub(root, ["install"]);
  check(
    "11 npm_config_node_gyp resolves the .js shim and runs",
    r.status === 0,
    `status=${r.status}\n${r.stdout}${r.stderr}`,
  );
}

// 12 — jailBuilds=true, the case the lazy shim cannot serve by itself. The jail
// clears the environment and substitutes a temporary HOME, so a shim re-entry
// resolves the tool dir under THAT home, finds nothing, and cannot refetch
// because the jail denies network. Warming the real cache first does not help.
// So a jailed job has to be handed an already-resolved node-gyp.
{
  const root = sandbox("jail");
  const proj = fixture(root, { script: "node usegyp.cjs" });
  writeFileSync(
    join(proj, "d1", "usegyp.cjs"),
    [
      // HOME is the positive control: the jail substitutes a temp one, so this
      // reports whether the script really ran jailed rather than assuming it.
      'console.log("HOME_IS=" + process.env.HOME);',
      'const r = require("child_process").spawnSync("node-gyp", ["--version"], { encoding: "utf8", shell: process.platform === "win32" });',
      'console.log("GYP_STATUS=" + r.status);',
      "process.exit(r.status === 0 ? 0 : 1);",
    ].join("\n"),
  );
  writeFileSync(join(proj, ".npmrc"), "jail-builds=true\n");
  const r = nub(root, ["install"]);
  const out = r.stdout + r.stderr;
  const jailed = /HOME_IS=.*nub-jail/.test(out);
  check(
    "12 jailed build resolves node-gyp with a cold cache",
    r.status === 0 && /GYP_STATUS=0/.test(out),
    `status=${r.status} jailEngaged=${jailed}\n${out}`,
  );
  console.log(`      (jail engaged: ${jailed ? "yes" : "no — platform may not jail, scenario still valid"})`);
}

// 13 — jailBuilds=true where nothing wants node-gyp and no registry is
// reachable. Preparing node-gyp for the jail is best-effort: failing to do it
// must warn, not sink an install that never needed it.
{
  const root = sandbox("jailoffline");
  const proj = fixture(root, { script: "node marker.cjs" });
  writeFileSync(join(proj, ".npmrc"), `jail-builds=true\n${UNREACHABLE}`);
  const r = nub(root, ["install"]);
  const out = r.stdout + r.stderr;
  check(
    "13 jailed build that never needs node-gyp installs offline",
    r.status === 0,
    `status=${r.status}\n${out}`,
  );
  // Brand-agnostic: nub's presenter rewrites WARN_AUBE_* to WARN_NUB_* on the
  // way out, so matching the raw aube spelling silently finds nothing.
  console.log(`      (best-effort warning emitted: ${/WARN_(AUBE|NUB)_NODE_GYP_BOOTSTRAP_FAILED/.test(out) ? "yes" : "no"})`);
}

// Scenario 4 asserts a bucket is ABSENT, which is free if nothing on this
// platform could produce one. Windows passed it while the shim was unable to
// bootstrap at all. Say so rather than letting the PASS stand unqualified.
if (!results.find((r) => r.name.startsWith("3"))?.ok) {
  console.log(
    "\n!! scenario 3 (positive control) FAILED — nothing here can bootstrap, so\n" +
      "   scenario 4's 'no bucket' PASS is vacuous and proves nothing.",
  );
}

console.log(`\n${results.filter((r) => r.ok).length}/${results.length} passed`);
process.exit(failures === 0 ? 0 : 1);

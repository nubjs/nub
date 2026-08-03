// Windows end-to-end + A/B timing for the one-binary change.
//
// The dispatch probe (probe.mjs) hand-assembles a node_modules tree and calls
// `node bin/nubx` directly. That leaves two things unproven on Windows, which is where
// this harness comes in:
//
//   1. A REAL `npm install -g`. Production Windows is
//      cmd.exe -> nub.cmd -> node bin/nub -> spawn nub.exe, and probe.mjs only covers
//      the last two hops. npm's own .cmd/.ps1 shim generation and PATH resolution were
//      never exercised.
//   2. SPEED. The change adds an env-var assignment and, for nubx, spawns bin/nub.exe
//      rather than bin/nubx.exe. Both should be free. "Should" is not a measurement.
//
// The A/B holds the BINARY FIXED and swaps only launch.js, so the only variable is the
// launcher change. Comparing two different builds would confound it with codegen noise.
//
// Usage: node tests/verb-dispatch/win-e2e.mjs <release-nub.exe> <launcher-dir> <old-launch.js>

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const [nativeSrc, launcherSrc, oldLaunchJs] = process.argv.slice(2).map((p) => path.resolve(p));
const isWin = process.platform === "win32";
const exe = isWin ? ".exe" : "";
const N = Number(process.env.VERB_TIMING_N || 40);
const WARMUP = 5;

let pass = 0;
const failures = [];
const ok = (m) => { console.log(`  ok: ${m}`); pass++; };
const no = (m) => { console.log(`  FAIL: ${m}`); failures.push(m); };

// ── a REAL global install ─────────────────────────────────────────────────────────
// Build the two packages on disk and `npm install -g` the root, letting npm resolve the
// platform package as a real optionalDependency and generate its own shims.
const root = fs.mkdtempSync(path.join(os.tmpdir(), "nub-wine2e-"));
const prefix = path.join(root, "prefix");
const platDir = path.join(root, "platform");
const rootDir = path.join(root, "rootpkg");
fs.mkdirSync(path.join(platDir, "bin"), { recursive: true });
fs.mkdirSync(prefix, { recursive: true });
fs.cpSync(launcherSrc, rootDir, { recursive: true });

// ONE binary. The platform package declares no `bin` field — it is a carrier, exactly
// like the published @nubjs/nub-<platform> packages.
fs.copyFileSync(nativeSrc, path.join(platDir, "bin", `nub${exe}`));
const platName = `@nubjs/nub-e2e-${process.platform}-${process.arch}`;
fs.writeFileSync(path.join(platDir, "package.json"), JSON.stringify({
  name: platName, version: "9.9.9", files: ["bin"],
}));
// Point the launcher's platform resolver at our fake platform package.
fs.writeFileSync(path.join(rootDir, "platform.js"),
  `module.exports={platformPackage(){return{key:"e2e",pkg:${JSON.stringify(platName)}};}};\n`);
const rootPkg = JSON.parse(fs.readFileSync(path.join(rootDir, "package.json"), "utf8"));
rootPkg.version = "9.9.9";
// No optionalDependencies. A `file:` optional dep is NOT installed into a global prefix,
// and leaving it declared makes npm's failed attempt shadow the manual placement below —
// resolution then fails even though the package is on disk. (Measured: identical trees
// resolve or don't purely on whether this field is present.) The package is placed by
// hand a few lines down, in the layout npm's os/cpu hoisting produces for real.
delete rootPkg.optionalDependencies;
delete rootPkg.scripts; // postinstall is not what we are testing; the launcher must stand alone
fs.writeFileSync(path.join(rootDir, "package.json"), JSON.stringify(rootPkg));

// spawnSync cannot execute npm.cmd on Windows (ENOENT bare / EINVAL on .cmd since
// CVE-2024-27980). Run npm's bundled CLI through node instead.
const npmCli = path.join(path.dirname(process.execPath), "node_modules", "npm", "bin", "npm-cli.js");
const npmCliPath = fs.existsSync(npmCli)
  ? npmCli
  : path.join(path.dirname(process.execPath), "..", "lib", "node_modules", "npm", "bin", "npm-cli.js");

// PACK FIRST, then install the tarball. `npm install -g <dir>` SYMLINKS the directory
// (measured: `nub -> ../../../../rootpkg`), so the launcher's realpath becomes the source
// tree and module resolution walks up from THERE — it never sees the platform package in
// the global node_modules, and every dispatch fails with "package is not installed".
// A tarball is copied, which is what a registry install does, and packing also honours
// the `files` field so we ship what a publish would.
const pack = spawnSync(process.execPath, [npmCliPath, "pack", rootDir, "--pack-destination", root], {
  encoding: "utf8",
});
const tgz = (pack.stdout ?? "").trim().split(/\r?\n/).filter((l) => l.endsWith(".tgz")).pop();
if (pack.status !== 0 || !tgz) {
  console.log(pack.stdout, pack.stderr);
  no(`npm pack failed (exit ${pack.status})`);
}
const install = spawnSync(process.execPath, [
  npmCliPath, "install", "-g", path.join(root, tgz ?? ""), "--prefix", prefix, "--no-audit", "--no-fund",
], { encoding: "utf8" });
if (install.status !== 0) {
  console.log(install.stdout, install.stderr);
  no(`npm install -g failed (exit ${install.status})`);
} else {
  ok("npm install -g (from a packed tarball) completed");
}

// Place the platform package as a SIBLING in the global node_modules — the layout a real
// `npm i -g @nubjs/nub` produces, since npm hoists the os/cpu-selected optionalDependency
// there. npm will not resolve a `file:` optional dep into a global install, and faking the
// registry's os/cpu filtering is not what this harness is for: the chain under test is
// generated shim -> node bin/nub -> spawn nub.exe, not npm's platform SELECTION.
const globalNm = isWin
  ? path.join(prefix, "node_modules")
  : path.join(prefix, "lib", "node_modules");
fs.cpSync(platDir, path.join(globalNm, ...platName.split("/")), { recursive: true });
fs.existsSync(path.join(globalNm, ...platName.split("/"), "bin", `nub${exe}`))
  ? ok("platform package placed in the global node_modules (one binary)")
  : no("could not place the platform package");

const binHome = isWin ? prefix : path.join(prefix, "bin");
const listing = fs.existsSync(binHome) ? fs.readdirSync(binHome) : [];
console.log(`  (generated shims: ${listing.filter((f) => /^nubx?(\.|$)/.test(f)).join(", ") || "none"})`);

// ── dispatch through npm's OWN shims, on PATH ─────────────────────────────────────
const NUBX_MARK = "Run a tool from";
const NUB_MARK = "all-in-one";
const env = { ...process.env, PATH: `${binHome}${path.delimiter}${process.env.PATH}` };
// `shell: true` is how a human's cmd.exe resolves `nub` -> nub.cmd; direct spawn of a
// .cmd is refused by Node. This is the real production hop we could not reach before.
const viaShim = (verb) => {
  const r = spawnSync(`${verb} --help`, { shell: true, encoding: "utf8", env, cwd: root });
  return `${r.stdout ?? ""}${r.stderr ?? ""}`;
};
for (const [verb, mark, label] of [["nub", NUB_MARK, "nub"], ["nubx", NUBX_MARK, "nubx"]]) {
  const out = viaShim(verb);
  if (out.includes(mark)) { ok(`${verb} via npm's generated shim on PATH -> ${label} mode`); continue; }
  no(`${verb} via shim -> wrong: ${out.replace(/\s+/g, " ").trim().slice(0, 120)}`);
  // Report the tree rather than leaving the reader to guess which layer broke — this
  // harness has three candidate failure points (shim generation, platform placement,
  // module resolution) and the error text alone does not distinguish them.
  const installedRoot = path.join(globalNm, "@nubjs", "nub");
  console.log(`     globalNm      : ${globalNm} (exists=${fs.existsSync(globalNm)})`);
  console.log(`     @nubjs/*      : ${fs.existsSync(path.join(globalNm, "@nubjs")) ? fs.readdirSync(path.join(globalNm, "@nubjs")).join(", ") : "MISSING"}`);
  console.log(`     platform.js   : ${fs.existsSync(path.join(installedRoot, "platform.js")) ? fs.readFileSync(path.join(installedRoot, "platform.js"), "utf8").trim().slice(0, 90) : "MISSING"}`);
  try {
    const { createRequire } = await import("node:module");
    const req = createRequire(path.join(installedRoot, "bin", "launch.js"));
    console.log(`     resolve()     : ${req.resolve(`${platName}/bin/nub${exe}`)}`);
  } catch (e) { console.log(`     resolve()     : FAILED ${e.code}`); }
}

// ── A/B timing: same binary, only launch.js differs ───────────────────────────────
const launchJs = path.join(binHome, "node_modules", "@nubjs", "nub", "bin", "launch.js");
const launchJsAlt = fs.existsSync(launchJs)
  ? launchJs
  : path.join(prefix, "lib", "node_modules", "@nubjs", "nub", "bin", "launch.js");
const installedLaunch = fs.existsSync(launchJs) ? launchJs : launchJsAlt;

function median(xs) { const s = [...xs].sort((a, b) => a - b); return s[Math.floor(s.length / 2)]; }
function iqr(xs) { const s = [...xs].sort((a, b) => a - b); return s[Math.floor(s.length * 0.75)] - s[Math.floor(s.length * 0.25)]; }
function timeIt(verb) {
  const samples = [];
  for (let i = 0; i < N + WARMUP; i++) {
    const t = process.hrtime.bigint();
    spawnSync(`${verb} --version`, { shell: true, encoding: "utf8", env, cwd: root });
    const ms = Number(process.hrtime.bigint() - t) / 1e6;
    if (i >= WARMUP) samples.push(ms);
  }
  return { med: median(samples), iqr: iqr(samples) };
}

console.log("== A/B timing (same binary, launch.js is the only variable) ==");
if (!fs.existsSync(installedLaunch)) {
  no(`cannot A/B: no installed launch.js under ${prefix}`);
} else {
  const newJs = fs.readFileSync(installedLaunch, "utf8");
  const results = {};
  for (const [arm, body] of [["new", newJs], ["old", fs.readFileSync(oldLaunchJs, "utf8")]]) {
    fs.writeFileSync(installedLaunch, body);
    for (const verb of ["nub", "nubx"]) results[`${arm}/${verb}`] = timeIt(verb);
  }
  fs.writeFileSync(installedLaunch, newJs); // leave the tree in the state under test
  for (const k of Object.keys(results)) {
    console.log(`  ${k.padEnd(10)} median ${results[k].med.toFixed(1)} ms (IQR ${results[k].iqr.toFixed(1)})`);
  }
  // The old launcher cannot dispatch nubx off a one-binary package, so old/nubx is a
  // WRONG-ANSWER timing, not a comparable arm — nub is the honest comparison.
  const dNub = results["new/nub"].med - results["old/nub"].med;
  console.log(`  delta (nub, new - old): ${dNub >= 0 ? "+" : ""}${dNub.toFixed(1)} ms`);
  const budget = Math.max(5, results["old/nub"].med * 0.10);
  Math.abs(dNub) <= budget
    ? ok(`launcher change costs nothing measurable on nub (|${dNub.toFixed(1)}| <= ${budget.toFixed(1)} ms)`)
    : no(`launcher change moved nub by ${dNub.toFixed(1)} ms (budget ${budget.toFixed(1)})`);
}

try { fs.rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 }); } catch {}
console.log(`\nRESULT: ${pass} ok, ${failures.length} failed`);
process.exit(failures.length === 0 ? 0 : 1);

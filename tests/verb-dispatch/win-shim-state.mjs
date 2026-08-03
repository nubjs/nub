// Windows shim-state probe: is DELETING npm's .ps1 + extensionless shims worth more than
// REWRITING them to point straight at the binary — and is the difference ONE-TIME or PER-CALL?
//
// It is per-call. In the rewrite state those files still EXIST, and each shell resolves them
// ahead of the .exe (measured PATHEXT matrix: cmd -> EXE, but ps51/pwsh -> PS1 and bash -> SH),
// so every invocation pays an interpreter hop forever. Deleting them lets PATHEXT find nub.exe
// directly with no hop. This harness measures that gap first-hand rather than inheriting it.
//
// Each shell is driven EXPLICITLY, because the whole point is that they disagree about which
// shim wins. `shell: true` on Windows means cmd.exe only and would hide the effect entirely.
//
// Usage: node tests/verb-dispatch/win-shim-state.mjs <nub.exe>

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const nativeSrc = path.resolve(process.argv[2] ?? "");
const isWin = process.platform === "win32";

// WINDOWS ONLY, and not out of laziness: the question does not exist elsewhere. On POSIX npm
// creates a single SYMLINK, not the .cmd/.ps1/extensionless triple, so there is no shim to
// delete-or-rewrite and no shell disagreement about which one wins — and the POSIX heal already
// reaches ~4 ms. Running this there produces a state that cannot work (a rewritten shim pointing
// at a `nub.exe` that does not exist) and reads as a false BROKEN.
if (!isWin) {
  console.log("win-shim-state: Windows-only (POSIX gets a symlink, not a shim triple) — skipping.");
  process.exit(0);
}
const exe = isWin ? ".exe" : "";
const N = Number(process.env.VERB_TIMING_N || 40);
const WARMUP = 5;
if (!fs.existsSync(nativeSrc)) { console.error(`no binary at ${nativeSrc}`); process.exit(2); }

const npmCli = (() => {
  const a = path.join(path.dirname(process.execPath), "node_modules", "npm", "bin", "npm-cli.js");
  return fs.existsSync(a) ? a
    : path.join(path.dirname(process.execPath), "..", "lib", "node_modules", "npm", "bin", "npm-cli.js");
})();

const root = fs.mkdtempSync(path.join(os.tmpdir(), "nub-shimstate-"));
const median = (xs) => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)];
const iqr = (xs) => { const s = [...xs].sort((a, b) => a - b);
  return s[Math.floor(s.length * 0.75)] - s[Math.floor(s.length * 0.25)]; };

// A global install whose bin is a #!/usr/bin/env node launcher — TODAY's shape, so npm
// generates the node-hopping shim triple we are trying to get out of.
const pkg = path.join(root, "pkg");
const prefix = path.join(root, "prefix");
fs.mkdirSync(path.join(pkg, "bin"), { recursive: true });
fs.mkdirSync(prefix, { recursive: true });
fs.copyFileSync(nativeSrc, path.join(pkg, "bin", `native${exe}`));
fs.writeFileSync(path.join(pkg, "bin", "nub"),
  '#!/usr/bin/env node\n' +
  'const {spawnSync}=require("child_process");const path=require("path");\n' +
  `const r=spawnSync(path.join(__dirname,"native${exe}"),process.argv.slice(2),{stdio:"inherit"});\n` +
  'process.exit(r.status==null?1:r.status);\n');
fs.writeFileSync(path.join(pkg, "package.json"), JSON.stringify({
  name: "nub-shimstate", version: "9.9.9", bin: { nub: "bin/nub" },
}));

// Pack then install: `npm install -g <dir>` symlinks, which is not a registry install.
const pack = spawnSync(process.execPath, [npmCli, "pack", pkg, "--pack-destination", root], { encoding: "utf8" });
const tgz = (pack.stdout ?? "").trim().split(/\r?\n/).filter((l) => l.endsWith(".tgz")).pop();
const inst = spawnSync(process.execPath, [
  npmCli, "install", "-g", path.join(root, tgz ?? ""), "--prefix", prefix, "--no-audit", "--no-fund",
], { encoding: "utf8" });
if (inst.status !== 0) { console.error("install failed:", inst.stderr); process.exit(1); }

// The bin dir and the global node_modules are NOT the same parent on both platforms:
// Windows puts shims at <prefix>\ and packages at <prefix>\node_modules, POSIX puts them at
// <prefix>/bin and <prefix>/lib/node_modules.
const binHome = isWin ? prefix : path.join(prefix, "bin");
const globalNm = isWin ? path.join(prefix, "node_modules") : path.join(prefix, "lib", "node_modules");
const installedNative = path.join(globalNm, "nub-shimstate", "bin", `native${exe}`);
console.log(`shims as generated: ${fs.readdirSync(binHome).filter((f) => /^nub(\.|$)/.test(f)).join(", ")}`);

// Snapshot the generated shims so each state can be rebuilt from the same starting point.
const shimNames = fs.readdirSync(binHome).filter((f) => /^nub(\.|$)/.test(f));
const pristine = Object.fromEntries(shimNames.map((f) => [f, fs.readFileSync(path.join(binHome, f))]));

function restore() {
  for (const f of fs.readdirSync(binHome).filter((f) => /^nub(\.|$)/.test(f))) {
    try { fs.rmSync(path.join(binHome, f), { force: true, maxRetries: 5, retryDelay: 100 }); } catch {}
  }
  for (const [f, buf] of Object.entries(pristine)) fs.writeFileSync(path.join(binHome, f), buf);
}

function addExe() {
  const dest = path.join(binHome, `nub${exe}`);
  try { fs.rmSync(dest, { force: true }); } catch {}
  try { fs.linkSync(installedNative, dest); } catch { fs.copyFileSync(installedNative, dest); }
}

// Rewrite the two non-.cmd shims to invoke the .exe directly — no node, but the FILES REMAIN,
// so each shell still resolves them ahead of nub.exe and pays its interpreter.
function rewriteShims() {
  const ps1 = path.join(binHome, "nub.ps1");
  if (fs.existsSync(ps1)) fs.writeFileSync(ps1, '& "$PSScriptRoot\\nub.exe" $args\nexit $LASTEXITCODE\n');
  const sh = path.join(binHome, "nub");
  if (fs.existsSync(sh)) fs.writeFileSync(sh, '#!/bin/sh\nexec "$(dirname "$0")/nub.exe" "$@"\n');
}
function deleteShims() {
  for (const f of ["nub.ps1", "nub"]) {
    try { fs.rmSync(path.join(binHome, f), { force: true, maxRetries: 5, retryDelay: 100 }); } catch {}
  }
}

// Each shell explicitly. Git Bash is the one that punishes an extra hop hardest on Windows.
const gitBash = ["C:\\Program Files\\Git\\bin\\bash.exe", "C:\\Program Files\\Git\\usr\\bin\\bash.exe"]
  .find((p) => fs.existsSync(p));
const shells = isWin
  ? [
      ["cmd.exe", (c) => ["cmd.exe", ["/c", c]]],
      ["pwsh", (c) => ["pwsh", ["-NoProfile", "-Command", c]]],
      ...(gitBash ? [["git-bash", (c) => [gitBash, ["-c", c]]]] : []),
    ]
  : [["sh", (c) => ["/bin/sh", ["-c", c]]]];

const env = { ...process.env, PATH: `${binHome}${path.delimiter}${process.env.PATH}` };
function timeIn(mk) {
  const [bin, args] = mk("nub --version");
  const samples = [];
  let ok = false;
  for (let i = 0; i < N + WARMUP; i++) {
    const t = process.hrtime.bigint();
    const r = spawnSync(bin, args, { encoding: "utf8", env });
    const ms = Number(process.hrtime.bigint() - t) / 1e6;
    if (i === 0) ok = /\d+\.\d+\.\d+/.test(`${r.stdout ?? ""}${r.stderr ?? ""}`);
    if (i >= WARMUP) samples.push(ms);
  }
  return { med: median(samples), iqr: iqr(samples), ok };
}

const states = [
  ["shipped (npm's shims, node hop)", () => { restore(); }],
  ["+exe, shims REWRITTEN direct", () => { restore(); addExe(); rewriteShims(); }],
  ["+exe, shims DELETED", () => { restore(); addExe(); deleteShims(); }],
];

const table = {};
for (const [label, setup] of states) {
  setup();
  table[label] = {};
  for (const [name, mk] of shells) table[label][name] = timeIn(mk);
}

console.log(`\n== shim-state x shell (${process.platform}, N=${N}) — median ms (IQR), works? ==`);
const shellNames = shells.map(([n]) => n);
console.log(`  ${"state".padEnd(34)}${shellNames.map((n) => n.padStart(18)).join("")}`);
for (const [label, row] of Object.entries(table)) {
  console.log(`  ${label.padEnd(34)}${shellNames.map((n) =>
    `${row[n].med.toFixed(1)} (${row[n].iqr.toFixed(1)})${row[n].ok ? "" : " BROKEN"}`.padStart(18)).join("")}`);
}
console.log(`\n  PER-CALL cost of REWRITE vs DELETE (positive = rewrite is slower, every call):`);
for (const n of shellNames) {
  const d = table["+exe, shims REWRITTEN direct"][n].med - table["+exe, shims DELETED"][n].med;
  console.log(`    ${n.padEnd(12)} ${d >= 0 ? "+" : ""}${d.toFixed(1)} ms`);
}
try { fs.rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 }); } catch {}

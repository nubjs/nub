// Windows launcher-latency variants — measure the CEILING of each mechanism before
// engineering any of them.
//
// Today a Windows `nub` call costs ~94 ms. Measured breakdown from run 30680335776:
//   cmd /c rem              10.22 ms   shell floor
//   native no-op exe         6.11 ms   process-creation floor
//   nub.exe --version        19.19 ms  our binary, direct, no shim
//   node -e ""               57.98 ms  the Node hop
//
// So ~58 of the ~94 ms is a Node boot we pay because our `bin` target starts with
// `#!/usr/bin/env node`. npm's cmd-shim branches on exactly that
// (cmd-shim/lib/index.js:42 "check if the bin is a #! of some sort. If not ... just
// call it directly") and, for a SHEBANG-LESS target, emits a direct invocation in ALL
// THREE shims — .cmd, .ps1 and the extensionless sh — with no `node` and nothing to
// delete. This harness measures what that is actually worth.
//
// Variant B deliberately ships the REAL binary as the bin target. That is not
// shippable (a cross-platform package cannot carry a 45 MB Windows exe) — it is here to
// measure the mechanism's CEILING. If B lands near 29 ms the approach is worth building
// a tiny native launcher for; if it lands near 90 ms the idea is dead and we stop.
//
// Usage: node tests/verb-dispatch/win-latency-variants.mjs <nub.exe>

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const nativeSrc = path.resolve(process.argv[2] ?? "");
const isWin = process.platform === "win32";
const exe = isWin ? ".exe" : "";
const N = Number(process.env.VERB_TIMING_N || 40);
const WARMUP = 5;

if (!fs.existsSync(nativeSrc)) { console.error(`no binary at ${nativeSrc}`); process.exit(2); }

const npmCli = (() => {
  const a = path.join(path.dirname(process.execPath), "node_modules", "npm", "bin", "npm-cli.js");
  return fs.existsSync(a) ? a
    : path.join(path.dirname(process.execPath), "..", "lib", "node_modules", "npm", "bin", "npm-cli.js");
})();

const root = fs.mkdtempSync(path.join(os.tmpdir(), "nub-winvar-"));
const median = (xs) => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)];
const iqr = (xs) => { const s = [...xs].sort((a, b) => a - b);
  return s[Math.floor(s.length * 0.75)] - s[Math.floor(s.length * 0.25)]; };

// Build one global install whose root package's `bin` points at `binRel`, then time
// `nub --version` through whatever npm generated, resolved off PATH.
function variant(label, { binRel, seedBin, extraBinDirFile }) {
  const dir = path.join(root, label);
  const pkg = path.join(dir, "pkg");
  const prefix = path.join(dir, "prefix");
  fs.mkdirSync(path.join(pkg, "bin"), { recursive: true });
  fs.mkdirSync(prefix, { recursive: true });
  seedBin(path.join(pkg, "bin"));
  fs.writeFileSync(path.join(pkg, "package.json"), JSON.stringify({
    name: "nub-winvar", version: "9.9.9", bin: { nub: binRel },
  }));

  // Pack then install: `npm install -g <dir>` SYMLINKS, which changes realpath-relative
  // resolution and is not what a registry install does.
  const pack = spawnSync(process.execPath, [npmCli, "pack", pkg, "--pack-destination", dir], { encoding: "utf8" });
  const tgz = (pack.stdout ?? "").trim().split(/\r?\n/).filter((l) => l.endsWith(".tgz")).pop();
  const inst = spawnSync(process.execPath, [
    npmCli, "install", "-g", path.join(dir, tgz ?? ""), "--prefix", prefix, "--no-audit", "--no-fund",
  ], { encoding: "utf8" });
  if (inst.status !== 0) return { label, error: `install failed: ${(inst.stderr ?? "").slice(0, 160)}` };

  const binHome = isWin ? prefix : path.join(prefix, "bin");
  if (extraBinDirFile) extraBinDirFile(binHome);
  const shims = fs.existsSync(binHome) ? fs.readdirSync(binHome).filter((f) => /^nub(\.|$)/.test(f)) : [];

  const env = { ...process.env, PATH: `${binHome}${path.delimiter}${process.env.PATH}` };
  const samples = [];
  let out = "";
  for (let i = 0; i < N + WARMUP; i++) {
    const t = process.hrtime.bigint();
    const r = spawnSync("nub --version", { shell: true, encoding: "utf8", env, cwd: dir });
    const ms = Number(process.hrtime.bigint() - t) / 1e6;
    if (i === 0) out = `${r.stdout ?? ""}${r.stderr ?? ""}`;
    if (i >= WARMUP) samples.push(ms);
  }
  return { label, med: median(samples), iqr: iqr(samples), shims, works: /\d+\.\d+\.\d+/.test(out), out };
}

const rows = [];

// A — TODAY. bin target is a #!/usr/bin/env node script that spawns the native binary.
rows.push(variant("A-node-shebang", {
  binRel: "bin/nub",
  seedBin(binDir) {
    fs.copyFileSync(nativeSrc, path.join(binDir, `native${exe}`));
    fs.writeFileSync(path.join(binDir, "nub"),
      '#!/usr/bin/env node\n' +
      'const {spawnSync}=require("child_process");const path=require("path");\n' +
      `const r=spawnSync(path.join(__dirname,"native${exe}"),process.argv.slice(2),{stdio:"inherit"});\n` +
      'process.exit(r.status==null?1:r.status);\n');
  },
}));

// B — SHEBANG-LESS bin target. The real binary stands in for a tiny native launcher, to
// measure the ceiling of npm's direct-shim path.
rows.push(variant("B-shebangless-bin", {
  binRel: `bin/nub${exe}`,
  seedBin(binDir) { fs.copyFileSync(nativeSrc, path.join(binDir, `nub${exe}`)); },
}));

// C — B plus a real nub.exe IN the bin dir, so PATHEXT resolves .exe ahead of .cmd and
// even the one cmd.exe hop disappears.
rows.push(variant("C-exe-on-PATH", {
  binRel: `bin/nub${exe}`,
  seedBin(binDir) { fs.copyFileSync(nativeSrc, path.join(binDir, `nub${exe}`)); },
  extraBinDirFile(binHome) {
    if (!isWin) return;
    try { fs.linkSync(path.join(binHome, "node_modules", "nub-winvar", "bin", "nub.exe"), path.join(binHome, "nub.exe")); }
    catch { try { fs.copyFileSync(nativeSrc, path.join(binHome, "nub.exe")); } catch {} }
  },
}));

// D — the floor: the binary by absolute path, no shim, no shell resolution.
{
  const direct = path.join(root, `direct${exe}`);
  fs.copyFileSync(nativeSrc, direct);
  if (!isWin) fs.chmodSync(direct, 0o755);
  const samples = [];
  for (let i = 0; i < N + WARMUP; i++) {
    const t = process.hrtime.bigint();
    spawnSync(direct, ["--version"], { encoding: "utf8" });
    const ms = Number(process.hrtime.bigint() - t) / 1e6;
    if (i >= WARMUP) samples.push(ms);
  }
  rows.push({ label: "D-direct-exec", med: median(samples), iqr: iqr(samples), shims: ["(none)"], works: true });
}

console.log(`\n== Windows launcher-latency variants (${process.platform}, N=${N}) ==`);
for (const r of rows) {
  if (r.error) { console.log(`  ${r.label.padEnd(20)} ERROR ${r.error}`); continue; }
  console.log(`  ${r.label.padEnd(20)} ${r.med.toFixed(1).padStart(6)} ms (IQR ${r.iqr.toFixed(1)})  works=${r.works}  shims: ${r.shims.join(", ")}`);
}
const a = rows.find((r) => r.label.startsWith("A"))?.med;
for (const r of rows.slice(1)) {
  if (r.med && a) console.log(`  ${r.label} vs today: ${(a / r.med).toFixed(1)}x faster (-${(a - r.med).toFixed(1)} ms)`);
}
try { fs.rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 }); } catch {}

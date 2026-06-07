"use strict";
// Tests for the POSIX runtime self-heal (bin/launch.js -> shims.js::installShims).
//
// The contract under test (see shims.js / bin/launch.js for the full rationale):
//   - When the JS launcher runs at all on POSIX, the install-time symlink is absent
//     (a package manager skipped postinstall — pnpm v10+/bun block lifecycle scripts).
//     The launcher must atomically replace BOTH bin/nub and bin/nubx with symlinks to
//     the platform binary, so every FUTURE invocation execs the native binary directly.
//   - The heal is best-effort: if it cannot create the symlinks (read-only bin dir,
//     EXDEV, …) it must leave the JS launcher in place and the command must STILL run.
//   - It must be race-safe: N concurrent launchers must converge on a valid symlink
//     with no missing-file window and no corruption.
//
// FIXTURE NOTE — the platform binary is synthetic on purpose. The committed
// @nubjs/nub-darwin-arm64/bin/nub on this host is a 4-byte text placeholder ("stub"),
// not a real Rust binary; spawnSync rejects it with ENOEXEC, which would make
// launch.js exit 1 for reasons unrelated to the heal. The heal logic is entirely
// agnostic to the binary's CONTENTS — it only symlinks to its PATH — so each fixture
// writes a tiny real shebang executable as the platform binary. That keeps the
// load-bearing assertions (symlink side-effects) faithful to production while letting
// the "it ran / exit 0" assertions be meaningful. The real runtime/ tree is copied
// verbatim from the platform package so the layout is otherwise byte-identical.

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";
import fs from "node:fs";

const execFileP = promisify(execFile);
const require = createRequire(import.meta.url);

// Resolve paths relative to THIS test file so the suite is checkout-portable:
//   npm/nub/test/heal.test.mjs  ->  NUB_SRC = npm/nub,  REPO = npm
const NUB_SRC = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const REPO = path.resolve(NUB_SRC, "..");

// Pick the platform package matching THIS host using the package's OWN selection
// logic (the same platform.js that postinstall/launch use), so the suite runs on any
// host whose platform package is present and skips cleanly where it isn't built.
const { pkg: PLATFORM_PKG } = require(path.join(NUB_SRC, "platform.js")).platformPackage();
const PLATFORM_SRC = PLATFORM_PKG ? path.join(REPO, PLATFORM_PKG.split("/")[1]) : null;

// Skip on Windows (POSIX-only heal) or where this host's platform package isn't built
// (no binary/runtime/ to build a faithful fixture from). Defined before the hooks/tests
// below because the root `before` hook references it eagerly.
const SKIP =
  process.platform === "win32" ? "POSIX-only heal"
  : !PLATFORM_SRC ? "no platform package for this host"
  : !fs.existsSync(path.join(PLATFORM_SRC, "runtime")) ? `platform package ${PLATFORM_PKG} not built (no runtime/)`
  : false;
const posixOnly = { skip: SKIP };

const FAKE_VERSION = "nub 0.0.17-fake-test-binary";

// Track every tmp dir so we can clean up even if a test throws mid-way.
const tmpDirs = [];

// Build a fresh fake HOISTED install:
//   <tmp>/node_modules/@nubjs/nub/{bin/{nub,nubx,launch.js}, platform.js, postinstall.js, shims.js, package.json}
//   <tmp>/node_modules/@nubjs/nub-darwin-arm64/{bin/nub, runtime/...}
// require.resolve("@nubjs/nub-darwin-arm64/bin/nub") from inside the nub package then
// walks the node_modules chain to the sibling platform package — exactly like a real
// hoisted install. The platform binary is a working shebang executable (see header).
function makeLayout() {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "nub-heal-"));
  tmpDirs.push(tmp);

  const scope = path.join(tmp, "node_modules", "@nubjs");
  const nubPkg = path.join(scope, "nub");
  const platPkg = path.join(scope, PLATFORM_PKG.split("/")[1]);

  // Copy the nub package (the files npm would ship — mirror package.json "files").
  fs.mkdirSync(path.join(nubPkg, "bin"), { recursive: true });
  for (const f of ["nub", "nubx", "launch.js"]) {
    fs.copyFileSync(path.join(NUB_SRC, "bin", f), path.join(nubPkg, "bin", f));
  }
  for (const f of ["platform.js", "postinstall.js", "shims.js", "package.json"]) {
    fs.copyFileSync(path.join(NUB_SRC, f), path.join(nubPkg, f));
  }
  // bin/nub + bin/nubx must be executable JS launchers (as npm would mark them).
  fs.chmodSync(path.join(nubPkg, "bin", "nub"), 0o755);
  fs.chmodSync(path.join(nubPkg, "bin", "nubx"), 0o755);

  // Copy the platform package's real runtime/ tree verbatim, then write a WORKING
  // binary in place of the committed 4-byte stub (see fixture note in the header).
  fs.mkdirSync(path.join(platPkg, "bin"), { recursive: true });
  fs.cpSync(path.join(PLATFORM_SRC, "runtime"), path.join(platPkg, "runtime"), { recursive: true });
  const binDst = path.join(platPkg, "bin", "nub");
  fs.writeFileSync(binDst, `#!/bin/sh\necho "${FAKE_VERSION}"\nexit 0\n`);
  fs.chmodSync(binDst, 0o755);

  return {
    tmp,
    nubBin: path.join(nubPkg, "bin", "nub"),
    nubxBin: path.join(nubPkg, "bin", "nubx"),
    binDir: path.join(nubPkg, "bin"),
    platBin: binDst,
  };
}

// Path equality after canonicalizing both sides — guards against /var vs /private/var
// (macOS tmpdir is a symlink) and any intermediate symlinks.
function sameRealpath(a, b) {
  return fs.realpathSync(a) === fs.realpathSync(b);
}

function isLauncher(p) {
  // The JS launcher is a regular file whose first bytes are the node shebang.
  if (fs.lstatSync(p).isSymbolicLink()) return false;
  return fs.readFileSync(p, "utf8").startsWith("#!/usr/bin/env node");
}

before(() => {
  if (SKIP) return; // POSIX-only, and needs the host's platform package built (see SKIP).
  assert.ok(fs.existsSync(path.join(NUB_SRC, "shims.js")), "shims.js must exist in nub pkg");
});

after(() => {
  for (const d of tmpDirs) {
    try { fs.rmSync(d, { recursive: true, force: true }); } catch {}
  }
});

// ---------------------------------------------------------------------------
// Scenario A — blocked-postinstall heal. The launcher runs (postinstall was
// skipped), so it must heal BOTH names to symlinks pointing at the platform binary.
// ---------------------------------------------------------------------------
test("A: launcher run heals both bin/nub and bin/nubx to platform-binary symlinks", posixOnly, async (t) => {
  const L = makeLayout();

  // Precondition: both are the copied JS launchers, not symlinks.
  assert.ok(isLauncher(L.nubBin), "precondition: bin/nub is the JS launcher");
  assert.ok(isLauncher(L.nubxBin), "precondition: bin/nubx is the JS launcher");

  const { stdout, stderr } = await execFileP("node", [L.nubBin, "--version"]);

  // It ran: clean exit (execFileP rejects on nonzero) and the binary's output reached us.
  assert.match(stdout, /fake-test-binary/, `unexpected stdout: ${JSON.stringify(stdout)} stderr: ${JSON.stringify(stderr)}`);

  // Load-bearing: BOTH names are now symlinks resolving to the platform binary.
  assert.ok(fs.lstatSync(L.nubBin).isSymbolicLink(), "bin/nub must be a symlink after heal");
  assert.ok(fs.lstatSync(L.nubxBin).isSymbolicLink(), "bin/nubx must be a symlink after heal");
  assert.ok(sameRealpath(L.nubBin, L.platBin), "bin/nub symlink must resolve to the platform binary");
  assert.ok(sameRealpath(L.nubxBin, L.platBin), "bin/nubx symlink must resolve to the platform binary");
});

// ---------------------------------------------------------------------------
// Scenario B — already-healed direct exec. After the heal, executing the symlink
// DIRECTLY (no node bootstrap) must run the native binary and remain a symlink.
// ---------------------------------------------------------------------------
test("B: post-heal direct exec of bin/nub runs the binary and stays a symlink", posixOnly, async (t) => {
  const L = makeLayout();

  // Heal first (Scenario A's effect).
  await execFileP("node", [L.nubBin, "--version"]);
  assert.ok(fs.lstatSync(L.nubBin).isSymbolicLink(), "precondition: bin/nub healed to a symlink");

  // Now exec the symlink directly — this is the fast path real users hit post-heal.
  const { stdout } = await execFileP(L.nubBin, ["--version"]);
  assert.match(stdout, /fake-test-binary/, "direct exec of the healed symlink must run the binary");

  // The direct exec must not have disturbed the symlink.
  assert.ok(fs.lstatSync(L.nubBin).isSymbolicLink(), "bin/nub must still be a symlink after direct exec");
  assert.ok(sameRealpath(L.nubBin, L.platBin), "bin/nub must still resolve to the platform binary");
});

// ---------------------------------------------------------------------------
// Scenario C — read-only bin dir → silent fallback. The heal cannot create the
// symlinks (renameSync into a 0o555 dir fails EACCES/EROFS). It must swallow that,
// the command must STILL run via spawn, and bin/nub must remain the JS launcher.
// ---------------------------------------------------------------------------
test("C: read-only bin dir makes heal silently fall back, command still runs", posixOnly, async (t) => {
  const L = makeLayout();

  fs.chmodSync(L.binDir, 0o555);
  t.after(() => { try { fs.chmodSync(L.binDir, 0o755); } catch {} }); // restore so cleanup can rm

  const { stdout, stderr } = await execFileP("node", [L.nubBin, "--version"]);

  // Command still works despite the failed heal.
  assert.match(stdout, /fake-test-binary/, `command must still run; stderr: ${JSON.stringify(stderr)}`);
  // No brand-leak / crash noise: launch.js only logs on a real spawn error, which
  // didn't happen here. (We don't assert empty stderr strictly — node may warn — but
  // it must not contain our launch-failure marker.)
  assert.doesNotMatch(stderr, /failed to launch/, "heal failure must not surface as a launch error");

  // Heal silently failed: bin/nub is STILL the JS launcher, not a symlink.
  assert.ok(!fs.lstatSync(L.nubBin).isSymbolicLink(), "bin/nub must NOT become a symlink when heal can't write");
  assert.ok(isLauncher(L.nubBin), "bin/nub must remain the JS launcher after a failed heal");
});

// ---------------------------------------------------------------------------
// Scenario D — concurrency. N launchers race to heal the same files, and a poller
// stats bin/nub throughout. The atomic symlink-to-temp + renameSync replace must
// guarantee: every worker succeeds, bin/nub NEVER disappears mid-race (renameSync
// clobbers in place — an unlink+symlink would open an ENOENT window a concurrent
// shell PATH lookup could hit), the final state is ONE valid symlink to the platform
// binary, and no per-pid temp files leak.
//
// Each worker is a SEPARATE process running the real shims.js::installShims — exactly
// what bin/launch.js does under its `process.platform !== "win32"` guard, with the
// pre-spawn ordering preserved (heal runs before the binary spawn). Distinct pids are
// load-bearing: installShims keys its temp file on process.pid, so only separate
// processes genuinely exercise the cross-process temp-uniqueness + rename race. (We
// can't reuse `node bin/nub` here: once one worker wins and swaps bin/nub to a symlink
// pointing at the binary, a *subsequent* `node bin/nub` would ask Node to load the
// native binary as a JS module and crash — but that's a path production never takes,
// since post-heal bin/nub is exec'd directly by the shell via PATH, never `node`-run.)
test("D: concurrent heals atomically converge — bin/nub never vanishes, no corruption", posixOnly, async (t) => {
  const L = makeLayout();
  const N = 8;

  const shimsPath = path.join(path.dirname(L.binDir), "shims.js"); // <nubPkg>/shims.js (binDir is <nubPkg>/bin)
  const worker = (i) =>
    execFileP("node", [
      "-e",
      // Mirror launch.js: require the real shims and call installShims(binSrc, binDir).
      `const {installShims}=require(${JSON.stringify(shimsPath)});` +
        `process.exitCode = installShims(${JSON.stringify(L.platBin)}, ${JSON.stringify(L.binDir)}) ? 0 : 3;`,
    ]);

  // Poll bin/nub for an ENOENT window for the duration of the race.
  let polling = true;
  let vanished = 0;
  const poller = (async () => {
    while (polling) {
      try { fs.lstatSync(L.nubBin); } catch (e) { if (e.code === "ENOENT") vanished++; }
      await new Promise((r) => setImmediate(r));
    }
  })();

  const runs = await Promise.allSettled(Array.from({ length: N }, (_, i) => worker(i)));
  polling = false;
  await poller;

  for (let i = 0; i < runs.length; i++) {
    const r = runs[i];
    assert.equal(r.status, "fulfilled", `worker ${i} heal must succeed (exit 0); reason: ${r.status === "rejected" ? r.reason : ""}`);
  }

  // The atomic replace must never have left bin/nub absent.
  assert.equal(vanished, 0, "bin/nub must never be missing during the concurrent race (atomic rename)");

  // Final state: exactly one valid symlink per name, resolving to the platform binary.
  assert.ok(fs.lstatSync(L.nubBin).isSymbolicLink(), "bin/nub must be a symlink after the race");
  assert.ok(sameRealpath(L.nubBin, L.platBin), "bin/nub must resolve to the platform binary after the race");
  assert.ok(fs.lstatSync(L.nubxBin).isSymbolicLink(), "bin/nubx must be a symlink after the race");
  assert.ok(sameRealpath(L.nubxBin, L.platBin), "bin/nubx must resolve to the platform binary after the race");

  // No leftover .nub.<pid>.tmp / .nubx.<pid>.tmp files (each heal cleans up its temp).
  const leftovers = fs.readdirSync(L.binDir).filter((f) => /^\.nubx?\.\d+\.tmp$/.test(f));
  assert.deepEqual(leftovers, [], `no temp files must remain; found: ${leftovers.join(", ")}`);
});

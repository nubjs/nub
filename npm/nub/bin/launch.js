"use strict";
// Shared launcher used by bin/nub and bin/nubx.
//
// bin/nub and bin/nubx ship as committed `#!/usr/bin/env node` shims because the
// cross-platform @nubjs/nub package cannot ship a native binary (it doesn't know
// the target platform at publish time). On Windows that is the whole story: npm's
// generated nub.cmd / nubx.cmd invoke `node bin/nub`, which spawns the platform
// .exe (no shebang/symlink fast path on Windows).
//
// On its FIRST POSIX invocation this launcher SELF-HEALS: it rewrites the on-PATH
// entry that dispatched it — the package manager's bin shim (pnpm cmd-shim) or
// symlink (npm/bun) — into a MINIMAL `#!/bin/sh` trampoline that exec's the native
// binary directly. Every later call then resolves PATH -> that tiny sh trampoline
// -> native, skipping Node entirely (~native cold-start; the sh hop is ~1-2ms on
// Linux dash/busybox, ~4ms on macOS bash — vs ~50ms for this Node launcher).
//
// CRITICAL: the heal target is a minimal sh SCRIPT, never a native binary. A
// script->binary swap has an irreducible TOCTOU race (the kernel reads `#!/bin/sh`
// then `/bin/sh` reopens the path and finds a Mach-O -> "cannot execute binary
// file"; ~24% under a concurrent burst). A script->script swap (both `#!/bin/sh`,
// always valid scripts) is race-free by construction — a concurrent `/bin/sh`
// reopening the entry mid-swap always reads a valid trampoline (measured 0/600 vs
// 146/600). So the heal needs no lock: it is best-effort, atomic (write temp +
// rename), verify-before-clobber, and a no-op on Windows.
//
// The native binary selects its verb from argv[0]'s basename (nub vs nubx); the
// healed trampoline exec's bin/<verb> in the platform package (which ships both
// names), so no argv0 override is needed past the heal.
const { spawnSync } = require("child_process");
const fs = require("fs");
const path = require("path");
const { platformPackage } = require("../platform.js");

function resolveBinary(verb) {
  const { key, pkg } = platformPackage();
  if (!pkg) {
    console.error(`@nubjs/nub: no prebuilt binary for ${key}`);
    process.exit(1);
  }
  const ext = process.platform === "win32" ? ".exe" : "";
  try {
    return require.resolve(`${pkg}/bin/${verb}${ext}`);
  } catch {
    // bin/nubx may be absent on an older platform package; fall back to bin/nub.
    try {
      return require.resolve(`${pkg}/bin/nub${ext}`);
    } catch {
      console.error(`@nubjs/nub: the ${pkg} package is not installed. Try: npm rebuild @nubjs/nub`);
      process.exit(1);
    }
  }
}

// POSIX single-quote a string for safe embedding in the sh trampoline.
function shq(s) { return `'${String(s).replace(/'/g, "'\\''")}'`; }

// Verify a PATH entry demonstrably resolves to OUR launcher before replacing it —
// never clobber an unrelated `nub` (there is an unrelated nub@1.0.0 on npm). For a
// symlink, realpath(entry) must equal our launcher's realpath. For a pnpm cmd-shim
// (a regular #!/bin/sh file), every quoted path it references is $basedir-resolved
// and realpath'd; one must equal our launcher. Comparing realpaths (not substrings)
// matches pnpm's fresh AND regenerated shim forms and rejects comment-only mentions.
function leadsToUs(entry, st, ourReal) {
  try {
    if (st.isSymbolicLink()) {
      try { return fs.realpathSync(entry) === ourReal; } catch { return false; }
    }
    if (st.isFile()) {
      const body = fs.readFileSync(entry, "utf8");
      const basedir = path.dirname(entry);
      const quoted = body.match(/"([^"]+)"/g) || [];
      for (const q of quoted) {
        let p = q.slice(1, -1).replace(/\$\{?basedir\}?/g, basedir);
        if (!p.includes("/")) continue;
        if (!path.isAbsolute(p)) p = path.join(basedir, p);
        try { if (fs.realpathSync(p) === ourReal) return true; } catch {}
      }
    }
  } catch {}
  return false;
}

// Best-effort, never throws. Rewrite the on-PATH `<verb>` entry that dispatched us
// into a minimal sh trampoline -> the native binary. POSIX only.
function healPathEntry(verb, nativePath) {
  if (process.platform === "win32") return;
  try {
    const ourBin = path.join(__dirname, verb); // .../@nubjs/nub/bin/<verb>
    let ourReal; try { ourReal = fs.realpathSync(ourBin); } catch { ourReal = ourBin; }
    let nativeReal; try { nativeReal = fs.realpathSync(nativePath); } catch { nativeReal = nativePath; }
    const content = `#!/bin/sh\nexec ${shq(nativeReal)} "$@"\n`;

    for (const dir of (process.env.PATH || "").split(path.delimiter)) {
      if (!dir) continue;
      const entry = path.join(dir, verb);
      let st;
      try { st = fs.lstatSync(entry); } catch { continue; }
      if (!leadsToUs(entry, st, ourReal)) continue;
      // Atomic replace: write a unique temp in the SAME dir, then rename over the
      // entry (rename is atomic on POSIX; script->script means no exec-format race).
      const tmp = path.join(dir, `.${verb}.nub.${process.pid}.${Date.now()}.tmp`);
      try {
        fs.writeFileSync(tmp, content, { mode: 0o755 });
        fs.chmodSync(tmp, 0o755);
        fs.renameSync(tmp, entry);
      } catch {
        try { fs.unlinkSync(tmp); } catch {}
      }
      break; // the first matching PATH entry is the one that dispatched us
    }
  } catch {}
}

// argv0Name: the verb this stub represents ("nubx" for bin/nubx; undefined => nub).
module.exports = function launch(argv0Name) {
  const verb = argv0Name || "nub";
  const binPath = resolveBinary(verb);
  // Self-heal the PATH entry on first POSIX call so later calls skip Node entirely.
  healPathEntry(verb, binPath);
  // This call still runs through Node; spawn the native binary. argv0 basename of
  // binPath is the verb (bin/nub or bin/nubx), so the Rust CLI dispatches correctly
  // without an argv0 override.
  const opts = { stdio: "inherit", windowsHide: true };
  if (argv0Name) opts.argv0 = argv0Name; // belt-and-suspenders for the bin/nub fallback path
  const result = spawnSync(binPath, process.argv.slice(2), opts);
  if (result.error) {
    console.error(`@nubjs/nub: failed to launch ${binPath}: ${result.error.message}`);
    process.exit(1);
  }
  process.exit(result.status == null ? 1 : result.status);
};

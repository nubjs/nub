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
// symlink (npm/bun/yarn) — into a tiny `#!/bin/sh` sh/node POLYGLOT trampoline that
// exec's the native binary. Every later call then resolves PATH -> that trampoline
// -> native, skipping Node entirely (~native cold-start; the sh hop is ~1-2ms on
// Linux dash/busybox, ~4ms on macOS bash — vs ~50ms for this Node launcher).
//
// CRITICAL: the heal target is a SCRIPT, never a native binary (a script->binary swap
// has an irreducible exec-format TOCTOU race), AND it is an sh/node polyglot so the
// swap is safe under concurrency on every PM. Two race classes are closed: (1) pnpm's
// entry is an sh cmd-shim, so sh->sh is race-free by construction; (2) npm/bun/yarn's
// entry is a symlink to this #!node launcher, so a concurrent Node that already passed
// the shebang and re-reads the swapped file would parse sh-as-JS and die — UNLESS the
// new file is also valid JS, which the polyglot is (it runs a JS fallback that spawns
// native). Measured: pure-sh heal ~6%/200 concurrent first-call failures on npm/bun;
// polyglot 0/600. So the heal needs no lock: it is best-effort, atomic (write temp +
// rename), verify-before-clobber, and a no-op on Windows.
//
// The native binary selects its verb from argv[0]'s basename (nub vs nubx); the
// healed trampoline exec's bin/<verb> in the platform package (which ships both
// names), so no argv0 override is needed past the heal.
const { spawn } = require("child_process");
const os = require("os");
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
    // The healed entry is an sh/node POLYGLOT. A fresh exec reads `#!/bin/sh` and sh
    // runs line 2 (`exec native`, the fast path). But a concurrent Node that already
    // spawned through the pre-heal node shim — passing the `#!node` shebang BEFORE the
    // heal renamed this file in — then re-opens this path as its "script" and reads
    // line 2 as a `":"` string statement + a `//` comment (no-op), running line 3's JS
    // fallback (spawn native) instead of choking on sh-as-JS. So the heal is race-free
    // on symlink-to-node-shim PMs (npm/bun/yarn) too, the guarantee pnpm gets for free.
    // Measured: pure-sh heal ~6%/200 concurrent first-call failures; polyglot 0/600.
    const content =
      `#!/bin/sh\n` +
      `":" //# nub launcher; exec ${shq(nativeReal)} "$@"\n` +
      `var r=require("child_process").spawnSync(${JSON.stringify(nativeReal)},process.argv.slice(2),{stdio:"inherit"});process.exit(r.status==null?1:r.status)\n`;

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
  // Ensure the platform binary is executable. npm strips the +x bit on install from
  // files that aren't `bin`-field entries, and the platform package declares no bin
  // field — so the staged-executable binary lands 0o644 and both spawnSync (below)
  // and the healed sh trampoline's `exec` would EACCES / "Permission denied". The old
  // postinstall did this chmod; doing it in the launcher covers EVERY package manager
  // (npm strips, others may not) and runs before the heal writes a trampoline that
  // exec's this binary. Best-effort; harmless if already executable.
  try { fs.chmodSync(binPath, 0o755); } catch {}
  // Self-heal the PATH entry on first POSIX call so later calls skip Node entirely.
  healPathEntry(verb, binPath);
  // This call still runs through Node; spawn the native binary. argv0 basename of
  // binPath is the verb (bin/nub or bin/nubx), so the Rust CLI dispatches correctly
  // without an argv0 override. We use async `spawn` (not `spawnSync`) ONLY so this
  // Node launcher can forward terminating signals to the native child: `spawnSync`
  // blocks the event loop, so a SIGTERM (docker stop on a `nub run` entrypoint whose
  // first-ever call hasn't been healed to the sh trampoline yet) would terminate
  // this launcher and orphan the workload. With async spawn we relay SIGTERM/INT/HUP
  // to the child — which then relays to its own subtree — and mirror its exit
  // status. (Subsequent calls skip Node entirely via the healed trampoline's `exec`,
  // where signals reach the binary directly.)
  const opts = { stdio: "inherit", windowsHide: true };
  if (argv0Name) opts.argv0 = argv0Name; // belt-and-suspenders for the bin/nub fallback path
  const child = spawn(binPath, process.argv.slice(2), opts);
  let forwarding = true;
  const forward = (sig) => {
    if (forwarding && child.pid) {
      try { process.kill(child.pid, sig); } catch {}
    }
  };
  for (const sig of ["SIGTERM", "SIGINT", "SIGHUP"]) process.on(sig, () => forward(sig));
  child.on("error", (err) => {
    console.error(`@nubjs/nub: failed to launch ${binPath}: ${err.message}`);
    process.exit(1);
  });
  child.on("exit", (code, signal) => {
    forwarding = false;
    if (signal) {
      // Mirror death-by-signal as 128+signo, matching the native binary and a shell.
      const signo = (os.constants && os.constants.signals && os.constants.signals[signal]) || 0;
      process.exit(signo ? 128 + signo : 1);
    }
    process.exit(code == null ? 1 : code);
  });
};

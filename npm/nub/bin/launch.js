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
// VERB SELECTION. The platform package ships ONE binary, `bin/nub` — shipping a
// byte-identical second copy under the name `nubx` doubled every platform package
// (77-99 MiB) and put them over cnpm/npmmirror's 80 MiB sync cap, breaking installs
// behind that mirror. So the verb travels in `__NUB_ARGV0`, which BOTH dispatch paths
// set: the healed sh trampoline as a prefix assignment, and the Node spawn below via
// `opts.env`. The Rust side reads it, then erases it so it survives exactly one
// process (Argv0::capture_argv0_override in crates/nub-cli/src/cli.rs).
//
// argv[0]'s basename remains the FALLBACK, and still carries every direct invocation:
// the curl installer's `~/.nub/bin/nubx` symlink, `nub pm shim` hardlinks, `nubx-dev`.
// It cannot serve the healed path because POSIX `sh` has no portable argv[0] override
// (`exec -a` is a bash/zsh-ism; dash — `/bin/sh` on Debian and Ubuntu — rejects it).
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
    // Expected for `nubx` on any current platform package — only `bin/nub` ships now,
    // and the verb rides in `__NUB_ARGV0` (see the header). The `<verb>` probe above
    // is kept so a NEWER launcher still works against an OLDER platform package that
    // does carry `bin/nubx`: a PM can pin the two to mismatched versions, and picking
    // the verb-named file there costs nothing and stays correct.
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

// True iff `p` is executable by THIS process (the +x bits that matter to us).
function isExecutable(p) {
  try { fs.accessSync(p, fs.constants.X_OK); return true; } catch { return false; }
}

// Make `binPath` runnable by us, returning a path that IS executable — `binPath`
// itself when we can fix it in place, otherwise a user-owned executable copy.
//
// npm strips +x on extract from any file that isn't a `bin`-field entry, and the
// platform packages declare no `bin` field, so the native binary lands 0o644. The
// install-time `postinstall.js` chmods it back — but when npm SKIPS lifecycle scripts
// (npm v12's default, or `ignore-scripts=true`) that never runs, leaving a 0o644
// binary at runtime. The launcher's in-place chmod recovers the common case (we own
// the file). It CANNOT recover the canonical container case: `root` does `npm i -g`,
// the image drops to a non-root `USER`, and that user's first `nub` can neither run
// the 0o644 binary nor chmod a file it doesn't own — `spawn` dies EACCES (verified:
// scripts-off install + non-root run + root-owned 0o644 binary). For THAT case we
// copy the binary into a user-owned cache dir at 0o755 and run the copy. This is the
// scripts-off complement to postinstall's install-time chmod: self-heal that holds
// even when we're not the binary's owner. Best-effort; returns the original path if
// every recovery fails (the caller surfaces the spawn error).
function ensureExecutable(binPath, verb) {
  if (process.platform === "win32") return binPath; // .exe needs no +x bit
  if (isExecutable(binPath)) return binPath;
  // Common case: we own the file (e.g. `sudo`-free user-prefix install) — chmod in place.
  try { fs.chmodSync(binPath, 0o755); } catch {}
  if (isExecutable(binPath)) return binPath;
  // Non-owner / read-only store: stage a user-owned executable copy in the cache.
  // Key the copy on the source path + size + mtime so a binary upgrade re-stages and
  // we never exec stale bytes; an existing fresh+executable copy is reused as-is.
  try {
    const st = fs.statSync(binPath);
    const base = process.env.XDG_CACHE_HOME ||
      (os.homedir() && os.homedir() !== "/" ? path.join(os.homedir(), ".cache") : null);
    if (!base) return binPath;
    // The copy's FILENAME must stay the bare verb (`nub`/`nubx`): the native binary
    // selects its verb from argv[0]'s basename, and the healed sh trampoline `exec`s
    // this path with no argv0 override — a tagged filename like `nubx-<tag>` would make
    // `nubx` misclassify as plain `nub`. So the staleness tag goes in the DIRECTORY,
    // and the executable keeps its real name: `<cache>/nub/bin/<tag>/<verb>`.
    const tag = `${st.size}-${Math.trunc(st.mtimeMs)}`;
    const dir = path.join(base, "nub", "bin", tag);
    fs.mkdirSync(dir, { recursive: true });
    const dest = path.join(dir, verb);
    if (isExecutable(dest)) {
      const dst = fs.statSync(dest);
      if (dst.size === st.size) return dest; // already staged, current, runnable
    }
    // Atomic stage: copy to a unique temp in the same dir, chmod, rename into place.
    const tmp = path.join(dir, `.${verb}.${process.pid}.${Date.now()}.tmp`);
    fs.copyFileSync(binPath, tmp);
    fs.chmodSync(tmp, 0o755);
    fs.renameSync(tmp, dest);
    if (isExecutable(dest)) return dest;
  } catch {}
  return binPath; // give up; caller's spawn surfaces the real error
}

// Verify a PATH entry demonstrably resolves to OUR launcher before replacing it —
// never clobber an unrelated `nub` (there is an unrelated nub@1.0.0 on npm). For a
// symlink, realpath(entry) must equal our launcher's realpath. For a pnpm cmd-shim
// (a regular #!/bin/sh file) the target is recovered two ways — pnpm >=11's own
// `# cmd-shim-target=` declaration, then every quoted path — each $basedir-resolved
// and realpath'd against our launcher. Comparing realpaths rather than substrings is
// what matches pnpm's fresh AND regenerated shim forms without matching a file that
// merely NAMES us; the declaration is additionally trusted only in a file that execs,
// so a non-dispatching file that mentions our path does not qualify either.
//
// The quote scan MUST tolerate empty pairs (`[^"]*`, not `[^"]+`). pnpm 11's cmd-shim
// opens with `exe=""` / `msys=""`; a `+` class cannot match `""`, so those two lines
// consumed one quote each and re-paired every subsequent quote off-by-one — the real
// target token was never produced and the heal silently never fired under pnpm 11
// (pnpm 10, whose template has no empty assignment, was unaffected). That is a SILENT
// perf regression, not a crash: every call kept paying the ~50ms node hop forever.
function leadsToUs(entry, st, ourReal) {
  try {
    if (st.isSymbolicLink()) {
      try { return fs.realpathSync(entry) === ourReal; } catch { return false; }
    }
    if (st.isFile()) {
      // A PATH `nub` that is a regular file is usually a PM shim — but it can also be a
      // REAL 45 MB nub binary (curl-install at ~/.nub/bin alongside an npm install). Reading
      // that as utf8 and regexing it costs ~1.1s: measured 379ms to read, 41,781 quoted
      // matches, 17,355 realpath syscalls, vs 0ms on an actual shim. Under a PM whose heal
      // never lands, that is paid on EVERY call. Every shim shape we handle is ~0.5-2 KB and
      // starts with `#!`. The size cap rejects the binary off the `lstat` healPathEntry
      // already took, for zero extra syscalls; only a file that passes the cap is opened at
      // all. Cap is deliberately far above any real shim rather than tight, and matches
      // aube's own MAX_BIN_SHIM_BYTES (aube-linker/src/sys.rs) for the same reason.
      if (st.size > 64 * 1024) return false;
      const body = fs.readFileSync(entry, "utf8");
      if (!body.startsWith("#!")) return false;
      const basedir = path.dirname(entry);
      // pnpm >=11 declares its own target in a `# cmd-shim-target=` trailer. Prefer it: the
      // shim naming what it dispatches to cannot drift with template churn the way
      // quote-scraping does. Two guards on trusting it, both because healPathEntry's rename
      // is unrecoverable — require an `exec`, so a file that merely MENTIONS our path is a
      // mention and not a target; and `[ \t]*` rather than `\s*`, which spans newlines and
      // would pair a bare `#` line with a following `cmd-shim-target=` line. Resolve against
      // the SHIM's dir as pnpm's own reader does (`path.resolve(path.dirname(shShim),
      // target)`, engine/pm/commands/src/self-updater/selfUpdate.ts): every pnpm writer path
      // emits an absolute value, so that is defensive, but bare `realpathSync` would resolve
      // a relative one against CWD — and the quoted branch below already resolves relatives
      // against basedir, so the two must agree.
      const declared = /\bexec\b/.test(body)
        ? body.match(/^#[ \t]*cmd-shim-target=(.+)$/m)
        : null;
      if (declared) {
        const target = path.resolve(basedir, declared[1].trim());
        try { if (fs.realpathSync(target) === ourReal) return true; } catch {}
      }
      const quoted = body.match(/"([^"]*)"/g) || [];
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

// Does the npm-generated `<verb>.cmd` in `dir` demonstrably dispatch to OUR launcher?
//
// The Windows analogue of leadsToUs, and needed for the same reason: healWindowsBinDir
// drops a `nub.exe` into a directory on the user's PATH, and there is a real unrelated
// `nub@1.0.0` on npm. Matching on the NAME alone would shadow someone else's tool with
// our binary — worse than the POSIX case, because PATHEXT makes our `.exe` win over
// their `.cmd` silently.
//
// npm's batch shim references its target as `"%dp0%\..\<pkg>\bin\nub"`, so the scan is
// the same shape as the sh one: pull quoted tokens, expand the basedir variable, realpath,
// compare. `%dp0%` already ends in a separator (`%~dp0` expands with a trailing slash),
// hence the `\\?` in the pattern.
function cmdShimLeadsToUs(dir, verb, ourReal) {
  try {
    const body = fs.readFileSync(path.join(dir, `${verb}.cmd`), "utf8");
    for (const q of body.match(/"([^"]*)"/g) || []) {
      let p = q.slice(1, -1).replace(/%dp0%\\?/gi, `${dir}${path.sep}`);
      if (!p.includes(path.sep) && !p.includes("/")) continue;
      if (!path.isAbsolute(p)) p = path.resolve(dir, p);
      try { if (fs.realpathSync(p) === ourReal) return true; } catch {}
    }
  } catch {}
  return false;
}

// WINDOWS: put a real `<verb>.exe` next to npm's shims and CHANGE NOTHING ELSE.
//
// The heal below is POSIX-only because there is no shebang or symlink fast path on
// Windows — every call goes cmd.exe -> nub.cmd -> node -> spawn nub.exe, and the node
// boot is ~58 ms of it. A hardlinked `nub.exe` in the same directory is resolved AHEAD
// of `nub.cmd` by PATHEXT, so cmd.exe reaches the binary directly. Measured on
// windows-latest, N=40: 95.6 -> 35.8 ms.
//
// DELIBERATELY ADD-ONLY. npm's `.ps1` and extensionless shims are left exactly as
// generated: we are not the first package to start editing files npm owns (checked —
// esbuild, bun and @pnpm/exe all modify only files inside their OWN package and never
// touch the global bin dir). The cost is that PowerShell and every sh-family shell keep
// preferring those shims and see no improvement — including nub's OWN Windows script
// shell, the bundled busybox (cli.rs `resolve_bundled_busybox`), measured 170.3 -> 169.0
// ms, i.e. nothing. `nub run` therefore does not benefit. That trade was made explicitly.
//
// TWO RESIDUES THIS SHAPE OWNS, both from the `.exe` being a file npm does not track:
//
//   UNINSTALL. `npm uninstall -g @nubjs/nub` removes only the shims npm generated;
//   cmd-shim never created `<verb>.exe` and npm has run no uninstall lifecycle script
//   since v7, so there is no hook to clean it up. The file STAYS ON PATH and keeps
//   answering `nub` from cmd.exe after the user believes nub is gone — and on the
//   hardlink path the surviving link also keeps the binary's bytes on disk. This is a
//   real user-visible residue, not merely wasted space; do not describe it as "npm's
//   uninstall is unaffected".
//
//   UPGRADE. Once the `.exe` wins PATHEXT, cmd.exe never dispatches through npm's `.cmd`
//   again, so THIS FUNCTION NEVER RUNS AGAIN for the users it serves and its currency
//   check below cannot fire for them. `postinstall.js` (dropStaleWindowsExe) removes the
//   file on every install so the next call re-heals against the new binary — but that
//   only runs when lifecycle scripts do, so an `--ignore-scripts` upgrade still leaves
//   cmd.exe executing the previous version silently.
//
// Best-effort and silent, like every other heal step: any failure leaves a working
// (slower) install rather than a broken one.
function healWindowsBinDir(verb, nativePath) {
  if (process.platform !== "win32") return;
  try {
    const ourBin = path.join(__dirname, verb);
    let ourReal; try { ourReal = fs.realpathSync(ourBin); } catch { ourReal = ourBin; }
    let nativeReal; try { nativeReal = fs.realpathSync(nativePath); } catch { nativeReal = nativePath; }
    let src; try { src = fs.statSync(nativeReal); } catch { return; }

    for (const dir of (process.env.PATH || "").split(path.delimiter)) {
      if (!dir) continue;
      if (!cmdShimLeadsToUs(dir, verb, ourReal)) continue;
      const dest = path.join(dir, `${verb}.exe`);
      // Idempotent, and correct across an upgrade: `npm i -g` extracts a NEW binary at a
      // new inode, so an existing .exe from a previous version is stale and must be
      // re-linked. Comparing ino+dev is exact for a hardlink; the size fallback covers
      // the copy path, where ino necessarily differs.
      try {
        const cur = fs.statSync(dest);
        if ((cur.ino && cur.ino === src.ino && cur.dev === src.dev) || cur.size === src.size) return;
        fs.rmSync(dest, { force: true });
      } catch {}
      try {
        fs.linkSync(nativeReal, dest);
      } catch {
        // EXDEV (prefix on a different volume from the store) or a filesystem without
        // hardlinks: fall back to a copy. Costs the binary's size on disk once, which is
        // why it is the fallback and not the default.
        try { fs.copyFileSync(nativeReal, dest); } catch {}
      }
      break; // the first PATH entry that dispatches to us is the one that matters
    }
  } catch {}
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
    //
    // Both branches carry the verb in `__NUB_ARGV0`. The platform package ships ONE
    // binary, so `nativeReal` is `bin/nub` for BOTH verbs and its basename can no
    // longer distinguish them — and POSIX `sh` cannot set argv[0] portably (`exec -a`
    // is a bash/zsh-ism; dash, i.e. `/bin/sh` on Debian and Ubuntu, rejects it). The
    // Rust side reads this var, then erases it so it survives exactly one process
    // (Argv0::capture_argv0_override). Without it the healed `nubx` entry silently
    // runs `nub` — exit 0, wrong command, which is why this is not optional.
    const envAssign = `__NUB_ARGV0=${shq(verb)} `;
    const content =
      `#!/bin/sh\n` +
      `":" //# nub launcher; ${envAssign}exec ${shq(nativeReal)} "$@"\n` +
      `var r=require("child_process").spawnSync(${JSON.stringify(nativeReal)},process.argv.slice(2),{stdio:"inherit",env:Object.assign({},process.env,{__NUB_ARGV0:${JSON.stringify(verb)}})});process.exit(r.status==null?1:r.status)\n`;

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
  const resolved = resolveBinary(verb);
  // Ensure the platform binary is executable. npm strips the +x bit on install from
  // files that aren't `bin`-field entries, and the platform package declares no bin
  // field — so the binary lands 0o644 and both spawn (below) and the healed sh
  // trampoline's `exec` would EACCES / "Permission denied". `postinstall.js` chmods it
  // at INSTALL time, but that's skipped under npm v12's scripts-off default (or
  // `ignore-scripts=true`); ensureExecutable is the runtime net for that. It chmods in
  // place when we own the file, and falls back to a user-owned executable COPY when we
  // don't (the root-installs-then-drops-to-non-root container case the launcher's bare
  // chmod cannot reach). Returns a path that IS executable; the heal + spawn both use
  // it so the fast-path trampoline targets the runnable file too.
  const binPath = ensureExecutable(resolved, verb);
  // Self-heal the PATH entry on first POSIX call so later calls skip Node entirely.
  healPathEntry(verb, binPath);
  // The Windows counterpart. Separate function rather than a branch inside healPathEntry
  // because the two do genuinely different things: POSIX REWRITES the entry that
  // dispatched us, Windows only ADDS a sibling `.exe` and leaves npm's shims untouched.
  healWindowsBinDir(verb, binPath);
  // This call still runs through Node; spawn the native binary. The platform package
  // ships ONE binary, so binPath's basename is `nub` for both verbs and cannot carry
  // the mode — `__NUB_ARGV0` does, below. We set `argv0` too, but the env var is the
  // load-bearing one: Node documents argv0 as affecting only the process TITLE on
  // Windows, and Windows is precisely where this path runs on EVERY call (the heal is
  // POSIX-only). The env var makes the two platforms agree instead of resting on
  // per-OS argv0 semantics. We use async `spawn` (not `spawnSync`) ONLY so this
  // Node launcher can forward terminating signals to the native child: `spawnSync`
  // blocks the event loop, so a SIGTERM (docker stop on a `nub run` entrypoint whose
  // first-ever call hasn't been healed to the sh trampoline yet) would terminate
  // this launcher and orphan the workload. With async spawn we relay SIGTERM/INT/HUP
  // to the child — which then relays to its own subtree — and mirror its exit
  // status. (Subsequent calls skip Node entirely via the healed trampoline's `exec`,
  // where signals reach the binary directly.)
  const opts = { stdio: "inherit", windowsHide: true };
  if (argv0Name) {
    opts.argv0 = argv0Name;
    opts.env = Object.assign({}, process.env, { __NUB_ARGV0: argv0Name });
  }
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

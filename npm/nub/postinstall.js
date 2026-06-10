"use strict";
// Install-time fixups, run as the INSTALLER by npm's postinstall hook. Two
// jobs, both best-effort and silent on any failure:
//
//   1. chmod +x the platform binary (chmodExecutable)
//   2. refresh existing ~/.nub/shims hardlinks (refreshShims)

const fs = require("fs");
const os = require("os");
const path = require("path");

// "@nubjs/nub-<platform>" or undefined (unsupported platform, or platform.js
// absent — shouldn't happen; the launcher handles both at runtime).
function platformPkg() {
  try {
    return require("./platform.js").platformPackage().pkg;
  } catch {
    return undefined;
  }
}

// Set the execute bit on the platform binary at INSTALL time.
//
// npm normalizes file modes on extract: a file referenced by a package's `bin`
// field lands 0o755, everything else 0o644. The platform packages
// (`@nubjs/nub-<platform>`) deliberately declare NO `bin` field — they're carriers
// selected by npm's os/cpu filters — so their `bin/nub` / `bin/nubx` extract 0o644
// (no +x). Something must add it back.
//
// `bin/launch.js` also chmods at runtime, but that runs as the END user and chmod
// only succeeds for the file's OWNER. The canonical container/CI pattern installs
// as root (`RUN npm i -g`) then drops to a non-root user (`USER app`); that user's
// first `nub` can't chmod a root-owned 0o644 binary and dies EACCES. THIS script
// runs as the INSTALLER (root, during the image build), so it sets the bit for
// everyone before any privilege drop. The launcher chmod stays as the fallback for
// PMs that skip postinstall — it just can't cover the non-owner case, which is why
// this exists. (Same shape as esbuild / @swc / @biomejs install-time chmod.)
//
// Best-effort and silent: a missing/already-executable binary, an unsupported
// platform, or a PM that ran us in a sandbox are all non-fatal — the launcher's
// runtime chmod is the second line of defense.
function chmodExecutable(pkg) {
  const ext = process.platform === "win32" ? ".exe" : "";
  for (const verb of ["nub", "nubx"]) {
    let binPath;
    try {
      binPath = require.resolve(`${pkg}/bin/${verb}${ext}`);
    } catch {
      continue; // an older platform package may not ship bin/nubx.
    }
    try {
      // Preserve read/write bits, add execute for user/group/other (umask-free —
      // an install-time binary should be runnable by whoever the image runs as).
      const mode = fs.statSync(binPath).mode;
      fs.chmodSync(binPath, mode | 0o111);
    } catch {
      // Not the owner, read-only store, etc. — the launcher chmod is the fallback.
    }
  }
}

// Re-link existing PM shims to the freshly-installed binary.
//
// `nub pm shim` populates ~/.nub/shims with HARDLINKS to the nub binary
// (crates/nub-core/src/pm/shim.rs; spec: wiki/research/package-manager-shims.md).
// An `npm i -g @nubjs/nub` upgrade extracts a NEW binary — new inode — so the
// shims keep executing the OLD bytes until re-linked. This is the installer-side
// re-link: if (and only if) a shims dir already exists, point every entry we own
// back at the fresh binary. It never CREATES the dir or missing entries — shims
// are `nub pm shim`'s explicit opt-in; this only refreshes one. (Under
// `sudo npm i -g`, os.homedir() is root's HOME, which has no shims dir — the
// correct do-nothing path.) Same names, same remove-then-link with hardlink →
// cross-device copy fallback, same dev+ino currency check as the Rust installer.
//
// Best-effort like everything here: any per-entry failure skips that entry — a
// stale shim is degraded, not broken (the shim-dir `nub` entry re-execs the real
// nub found past the shim dir, and `nub pm shim` re-links by hand).
const SHIM_NAMES = ["npm", "npx", "pnpm", "pnpx", "yarn", "yarnpkg", "nub"];

function refreshShims(pkg) {
  const ext = process.platform === "win32" ? ".exe" : "";
  let binPath, binStat;
  try {
    binPath = require.resolve(`${pkg}/bin/nub${ext}`);
    binStat = fs.statSync(binPath);
  } catch {
    return;
  }

  const shimDir = path.join(os.homedir(), ".nub", "shims");
  let entries;
  try {
    entries = fs.readdirSync(shimDir); // ENOENT/ENOTDIR = no opt-in, do nothing
  } catch {
    return;
  }

  // Writers on the shim dir serialize on ~/.nub/shims.lock (the Rust ShimLock —
  // see shim.rs). Take it O_EXCL; steal a stale one (>30s old = the holder died
  // mid-operation); a FRESH lock means a live `nub pm shim`/`unshim` is rewriting
  // the dir right now — skip the refresh rather than interleave or block the
  // install. Lock failures for any other reason (read-only ~/.nub) proceed
  // unlocked, matching the Rust side's best-effort posture.
  const lockPath = shimDir + ".lock";
  let locked = false;
  try {
    fs.writeFileSync(lockPath, "", { flag: "wx" });
    locked = true;
  } catch (e) {
    if (e && e.code === "EEXIST") {
      let stale = true; // unreadable mtime counts as stale, like the Rust impl
      try {
        stale = Date.now() - fs.statSync(lockPath).mtimeMs > 30_000;
      } catch {}
      if (!stale) return;
      try {
        fs.unlinkSync(lockPath);
        fs.writeFileSync(lockPath, "", { flag: "wx" });
        locked = true;
      } catch {
        return;
      }
    }
  }

  let refreshed = 0;
  try {
    for (const name of SHIM_NAMES) {
      const file = name + ext; // Windows shims are <name>.exe
      if (!entries.includes(file)) continue; // refresh-only: never create
      const target = path.join(shimDir, file);
      try {
        let st;
        try {
          st = fs.statSync(target);
        } catch {} // broken entry — fall through and re-link it
        if (st && st.dev === binStat.dev && st.ino === binStat.ino) {
          continue; // already a hardlink of the fresh bytes (re-run, npm rebuild)
        }
        // Remove-then-link, same documented non-atomic window as the Rust
        // installer (a concurrent exec of this exact name can hit ENOENT for
        // microseconds; the lock above serializes writers, not readers).
        fs.unlinkSync(target);
        try {
          fs.linkSync(binPath, target); // zero disk; +x travels with the inode
        } catch {
          fs.copyFileSync(binPath, target); // shim dir on another filesystem
          fs.chmodSync(target, 0o755); // a copy doesn't inherit the inode's +x
        }
        refreshed++;
      } catch {
        // Skip this entry silently — degraded, not broken (see above).
      }
    }
  } finally {
    if (locked) {
      try {
        fs.unlinkSync(lockPath);
      } catch {}
    }
  }

  if (refreshed > 0) {
    console.log(`refreshed ${refreshed} nub shim${refreshed === 1 ? "" : "s"} in ~/.nub/shims`);
  }
}

const pkg = platformPkg();
if (pkg) {
  chmodExecutable(pkg);
  refreshShims(pkg); // after chmod, so the linked inode already carries +x
}

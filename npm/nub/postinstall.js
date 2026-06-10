"use strict";
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

const fs = require("fs");
const path = require("path");

function chmodExecutable() {
  let platformPackage;
  try {
    ({ platformPackage } = require("./platform.js"));
  } catch {
    return; // platform.js absent (shouldn't happen) — let the launcher handle it.
  }

  const { pkg } = platformPackage();
  if (!pkg) return; // unsupported platform — nothing to chmod.

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

chmodExecutable();

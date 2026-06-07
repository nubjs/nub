"use strict";
// Shared POSIX symlink installer, used by both postinstall.js (install-time fast
// path) and bin/launch.js (runtime self-heal when a package manager skipped the
// postinstall — e.g. pnpm v10+ / bun block dependency lifecycle scripts by default).
//
// SYMLINK ONLY — never copy. The native binary finds its sibling runtime/ directory
// (runtime/preload.mjs) by canonicalizing its own exe path and walking up. runtime/
// ships ONLY inside the platform package (@nubjs/nub-<key>/runtime), NOT in
// @nubjs/nub. A symlink at bin/nub resolves to the platform binary's real location,
// where runtime/ is a sibling — preload resolution works. A COPY placed at
// @nubjs/nub/bin/nub has NO sibling runtime/ — preload resolution BREAKS. So if a
// symlink cannot be created (read-only FS, EXDEV, no symlink support), we do NOT
// fall back to copying; we leave the existing JS launcher in place. The launcher
// spawns the platform binary directly from its real location, which resolves
// runtime/ correctly — the no-heal fallback is correct, just slower.
const { mkdirSync, unlinkSync, symlinkSync, renameSync, chmodSync } = require("fs");
const { join } = require("path");

// installShims(binSrc, binDir): atomically replace binDir/nub and binDir/nubx with
// symlinks to the native binary at binSrc. Pure POSIX symlink logic — the CALLER is
// responsible for the process.platform !== "win32" guard (Windows has no symlink
// fast path, symlinks need privilege, and you cannot replace a running .exe).
//
// NEVER throws. Returns true iff BOTH shims were installed, else false.
function installShims(binSrc, binDir) {
  // mkdir is best-effort: binDir normally already exists (it's our own bin/), but a
  // missing parent must not make the whole heal throw.
  try { mkdirSync(binDir, { recursive: true }); } catch {}
  // Best-effort: ensure the native binary is executable. Failure here (e.g. we don't
  // own it) must not abort symlink creation.
  try { chmodSync(binSrc, 0o755); } catch {}

  // Always heal BOTH names: a single `nub` run also fixes `nubx`, since the Rust CLI
  // picks its verb from argv[0]'s basename and both names point at the same binary.
  let installed = 0;
  for (const name of ["nub", "nubx"]) {
    const dest = join(binDir, name);
    // Unique temp keyed on pid so concurrent heals (nub a & nub b, CI parallelism)
    // never collide on the same temp path.
    const tmp = join(binDir, `.${name}.${process.pid}.tmp`);
    try {
      // ATOMIC REPLACE: symlink to a unique temp, then renameSync(tmp, dest).
      // renameSync is atomic on POSIX and clobbers an existing dest in place — there
      // is never a window where bin/nub does not exist (which a plain
      // unlink+symlink would open, breaking a concurrent shell PATH lookup).
      try { unlinkSync(tmp); } catch {}
      symlinkSync(binSrc, tmp);
      renameSync(tmp, dest);
      installed++;
    } catch {
      // Symlink/rename failed (EROFS, EXDEV, EACCES, no symlink support, …). Clean up
      // the temp and move on — NO copy fallback (see the runtime/-sibling rationale
      // above). The existing JS launcher stays in place as the slow-but-correct path.
      try { unlinkSync(tmp); } catch {}
    }
  }
  return installed === 2;
}

module.exports = { installShims };

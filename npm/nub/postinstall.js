const { platform } = process;
const { mkdirSync, unlinkSync, symlinkSync, copyFileSync, chmodSync } = require("fs");
const { join } = require("path");
const { platformPackage } = require("./platform.js");

const { key, pkg } = platformPackage();

if (!pkg) {
  console.error(`@nubjs/nub: no prebuilt binary for ${key}`);
  process.exit(0);
}

// Windows: there is no symlink fast path. npm's generated nub.cmd / nubx.cmd invoke
// the JS launchers (bin/nub, bin/nubx), which resolve and spawn the platform .exe at
// runtime. Nothing to do at install time — leave the launchers in place.
if (platform === "win32") {
  process.exit(0);
}

let binSrc;
try {
  binSrc = require.resolve(`${pkg}/bin/nub`);
} catch {
  // optionalDependency not installed (wrong platform) — leave the JS launchers as a
  // fallback; they print an actionable error if invoked.
  process.exit(0);
}

// POSIX fast path: replace each JS launcher with a direct symlink to the platform
// binary, so `nub` and `nubx` exec the native Rust binary with no Node bootstrap.
// Both names point at the SAME binary; the Rust CLI selects its verb from argv[0]'s
// basename (nub vs nubx — see crates/nub-cli/src/cli.rs Argv0::detect), and
// process.execPath still resolves to the platform binary so the sibling runtime/
// directory is found by walking up.
const binDir = join(__dirname, "bin");
mkdirSync(binDir, { recursive: true });
for (const name of ["nub", "nubx"]) {
  const dest = join(binDir, name);
  try { unlinkSync(dest); } catch {}
  try {
    symlinkSync(binSrc, dest);
    chmodSync(dest, 0o755);
  } catch {
    // Fallback: copy if symlink fails (e.g. cross-device). Slower path resolution
    // but correct — argv[0] still resolves to a binary named nub/nubx.
    try {
      copyFileSync(binSrc, dest);
      chmodSync(dest, 0o755);
    } catch (err) {
      console.error(`@nubjs/nub: failed to install ${name} binary: ${err.message}`);
      process.exit(0);
    }
  }
}
chmodSync(binSrc, 0o755);

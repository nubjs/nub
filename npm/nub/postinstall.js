const { platform } = process;
const { join } = require("path");
const { platformPackage } = require("./platform.js");
const { installShims } = require("./shims.js");

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

// POSIX fast path: atomically replace each JS launcher with a direct SYMLINK to the
// platform binary, so `nub` and `nubx` exec the native Rust binary with no Node
// bootstrap. Both names point at the SAME binary; the Rust CLI selects its verb from
// argv[0]'s basename (nub vs nubx — see crates/nub-cli/src/cli.rs Argv0::detect), and
// the symlink resolves to the platform binary's real location so the sibling runtime/
// directory is found by walking up.
//
// SYMLINK ONLY — no copy fallback. runtime/ ships only inside the platform package
// (next to its binary), so a copy at bin/nub would have no sibling runtime/ and break
// preload resolution. If the symlink can't be made (read-only FS, EXDEV, …),
// installShims leaves the JS launcher in place — correct, just slower. (See shims.js
// for the full rationale; the same logic self-heals at runtime in bin/launch.js when
// a package manager skips this postinstall.)
const binDir = join(__dirname, "bin");
installShims(binSrc, binDir);

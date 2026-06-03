"use strict";
// Shared launcher used by bin/nub and bin/nubx.
//
// On POSIX, postinstall.js replaces bin/nub and bin/nubx with direct symlinks to
// the platform binary, so this module never runs on the hot path — `nub`/`nubx`
// exec the native Rust binary directly (no Node bootstrap, preserving cold-start).
// It DOES run on Windows (npm's generated nub.cmd / nubx.cmd invoke `node bin/nub`
// — there is no symlink fast path there) and as a fallback on any platform where
// postinstall could not create the symlink.
//
// The Rust CLI selects its verb from argv[0]'s basename (nub vs nubx vs node — see
// crates/nub-cli/src/cli.rs Argv0::detect). spawnSync's `argv0` option sets that
// basename for the child without changing process.execPath, so the binary still
// resolves its sibling runtime/ directory by walking up from its real location.
const { spawnSync } = require("child_process");
const { platformPackage } = require("../platform.js");

function resolveBinary() {
  const { key, pkg } = platformPackage();
  if (!pkg) {
    console.error(`@nubjs/nub: no prebuilt binary for ${key}`);
    process.exit(1);
  }
  try {
    return require.resolve(`${pkg}/bin/nub${process.platform === "win32" ? ".exe" : ""}`);
  } catch {
    console.error(`@nubjs/nub: the ${pkg} package is not installed. Try: npm rebuild @nubjs/nub`);
    process.exit(1);
  }
}

// argv0Name: the basename the child should see as argv[0] ("nub" or "nubx"). When
// omitted the child sees the binary path, whose basename is "nub" — the default verb.
module.exports = function launch(argv0Name) {
  const binPath = resolveBinary();
  const opts = { stdio: "inherit", windowsHide: true };
  if (argv0Name) opts.argv0 = argv0Name;
  const result = spawnSync(binPath, process.argv.slice(2), opts);
  if (result.error) {
    console.error(`@nubjs/nub: failed to launch ${binPath}: ${result.error.message}`);
    process.exit(1);
  }
  process.exit(result.status == null ? 1 : result.status);
};

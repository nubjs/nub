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
const { installShims } = require("../shims.js");

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

  // Self-heal (POSIX only). If we're running at all on POSIX, the install-time
  // symlink is absent — i.e. the package manager skipped this package's postinstall
  // (pnpm v10+ and bun block dependency lifecycle scripts by default). Atomically
  // replace bin/nub and bin/nubx with symlinks to the native binary so EVERY FUTURE
  // invocation execs the Rust CLI directly and skips this ~35ms Node bootstrap. This
  // first call still pays it; later calls don't.
  //
  // Best-effort and silent: installShims never throws and the heal is wrapped here
  // too, so a failure never delays or breaks the user's actual command — we fall
  // straight through to the spawn below. We heal BOTH names so a single `nub` run
  // also fixes `nubx`. Overwriting our own bin/nub + launch.js mid-execution is safe
  // on POSIX: Node already read+compiled both into memory before launch() runs, and
  // renameSync just swaps the directory entry (the in-use inode lives on); the binary
  // we spawn (binPath, in the platform package) is a different file the heal never
  // touches. Windows is excluded: no symlink fast path, symlinks need privilege, and
  // a running .exe cannot be replaced.
  if (process.platform !== "win32") {
    try { installShims(binPath, __dirname); } catch {}
  }

  const opts = { stdio: "inherit", windowsHide: true };
  if (argv0Name) opts.argv0 = argv0Name;
  const result = spawnSync(binPath, process.argv.slice(2), opts);
  if (result.error) {
    console.error(`@nubjs/nub: failed to launch ${binPath}: ${result.error.message}`);
    process.exit(1);
  }
  process.exit(result.status == null ? 1 : result.status);
};

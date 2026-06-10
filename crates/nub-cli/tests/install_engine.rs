//! `nub install` / `nub ci` through the embedded aube engine, end-to-end
//! through the binary: real fixtures, real node_modules, real lockfiles.
//! The layout policy and the yarn write gate live in
//! `crates/nub-cli/src/pm_engine.rs`.
//!
//! The two installing tests are `#[ignore]` (network) following the
//! provisioning-test convention — run them via
//! `cargo test -p nub-cli --test install_engine -- --ignored`. They also
//! self-skip when the npm registry is unreachable so an offline `--ignored`
//! sweep doesn't report false failures.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

/// A unique temp project dir under the system temp root (never under $HOME,
/// so manifest/lockfile walk-ups can't escape into stray ancestors).
fn pm_tmpdir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-install-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Spawn `nub <args>` in `dir` with the aube store/cache isolated to fresh
/// temp roots (XDG_DATA_HOME carries the CAS store, XDG_CACHE_HOME the
/// packument cache) so tests never warm-hit the dev box's real store.
fn run_install(dir: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env("XDG_DATA_HOME", pm_tmpdir("xdg-data"))
        .env("XDG_CACHE_HOME", pm_tmpdir("xdg-cache"))
        .output()
        .expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

/// Offline guard for the `#[ignore]` network tests: true when the registry
/// answers a TCP connect within 3s.
fn registry_reachable() -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    "registry.npmjs.org:443"
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .is_some_and(|addr| {
            TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(3)).is_ok()
        })
}

/// Fresh project (no lockfile): the engine resolves, links the isolated
/// (pnpm-style) layout, and writes aube's own lockfile.
#[test]
#[ignore = "network: resolves + fetches is-positive@3.1.0 from the npm registry"]
fn install_fresh_project_links_isolated_and_writes_a_lockfile() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("fresh");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"fresh","version":"1.0.0","dependencies":{"is-positive":"3.1.0"}}"#,
    )
    .unwrap();

    let (stdout, stderr, code) = run_install(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    // Isolated layout: the top-level entry is a symlink into the virtual store.
    let dep = dir.join("node_modules/is-positive");
    assert!(
        dep.join("package.json").is_file(),
        "is-positive must be installed: stderr: {stderr}"
    );
    assert!(
        dep.symlink_metadata().unwrap().file_type().is_symlink(),
        "no-lockfile projects default to the isolated layout (symlink into .aube)"
    );

    // TODO(aube-integration/defaultLockfileFormat): CURRENT behavior — a
    // fresh project gets aube's native lockfile. Once the fork's
    // `defaultLockfileFormat` toggle is wired through nub, flip this
    // assertion to whatever format the toggle selects for fresh projects.
    assert!(
        dir.join("aube-lock.yaml").is_file(),
        "fresh-project install writes aube-lock.yaml (pinned-engine behavior)"
    );
}

/// A project with a (frozen-satisfiable) package-lock.json: the layout policy
/// defaults to the hoisted (npm-style) layout, and the lockfile format is
/// preserved — no aube-lock.yaml appears next to package-lock.json.
#[test]
#[ignore = "network: fetches is-positive@3.1.0 (resolution comes from the lockfile)"]
fn install_with_package_lock_hoists_and_preserves_the_npm_lockfile() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let dir = pm_tmpdir("npmlock");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"npmlock","version":"1.0.0","dependencies":{"is-positive":"3.1.0"}}"#,
    )
    .unwrap();
    // In-sync npm v3 lockfile for is-positive@3.1.0 (integrity is the
    // published registry value — stable forever for a published version).
    let package_lock = r#"{
  "name": "npmlock",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "npmlock",
      "version": "1.0.0",
      "dependencies": { "is-positive": "3.1.0" }
    },
    "node_modules/is-positive": {
      "version": "3.1.0",
      "resolved": "https://registry.npmjs.org/is-positive/-/is-positive-3.1.0.tgz",
      "integrity": "sha512-8ND1j3y9/HP94TOvGzr69/FgbkX2ruOldhLEsTWwcJVfo4oRjwemJmJxt7RJkKYH8tz7vYBP9JcKQY8CLuJ90Q==",
      "engines": { "node": ">=0.10.0" }
    }
  }
}
"#;
    std::fs::write(dir.join("package-lock.json"), package_lock).unwrap();

    let (stdout, stderr, code) = run_install(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let dep = dir.join("node_modules/is-positive");
    assert!(
        dep.join("package.json").is_file(),
        "is-positive must be installed: stderr: {stderr}"
    );
    assert!(
        !dep.symlink_metadata().unwrap().file_type().is_symlink(),
        "package-lock projects default to the hoisted layout (real dir, not a symlink)"
    );
    assert!(
        dir.join("package-lock.json").is_file(),
        "the npm lockfile must be preserved"
    );
    assert!(
        !dir.join("aube-lock.yaml").exists(),
        "no aube-lock.yaml may appear next to package-lock.json"
    );
}

/// The yarn write gate, both trigger paths — no network either way:
/// a drifted yarn.lock is refused at pre-flight (before any resolution), and
/// `--no-frozen-lockfile` (an explicit "rewrite the lockfile" request) is
/// refused upfront. yarn.lock must be byte-identical afterwards.
#[test]
fn install_refuses_to_mutate_a_drifted_yarn_lock() {
    let dir = pm_tmpdir("yarngate");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"yarngate","version":"1.0.0","dependencies":{"is-positive":"3.1.0"}}"#,
    )
    .unwrap();
    // Valid yarn-classic lockfile that does NOT satisfy the manifest
    // (only left-pad) — installing would require a re-resolve + rewrite.
    let yarn_lock = "# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.\n\
                     # yarn lockfile v1\n\n\n\
                     left-pad@^1.3.0:\n\
                     \x20\x20version \"1.3.0\"\n\
                     \x20\x20resolved \"https://registry.yarnpkg.com/left-pad/-/left-pad-1.3.0.tgz#5b8a3a7765dfe001261dde915589e782f8c94d1e\"\n\
                     \x20\x20integrity sha512-XI5MPzVNApjAyhQzphX8BkmKsKUxD4LdyK24iZeQGinBN9yTQT3bFlCBy/aVx2HrNcqQGsdot8ghrjyrvMCoEA==\n";
    std::fs::write(dir.join("yarn.lock"), yarn_lock).unwrap();

    // Drifted lockfile → the gate, with the drift reason and the remedy.
    let (_, stderr, code) = run_install(&dir, &["install"]);
    assert_ne!(code, 0, "a drifted yarn.lock must be refused: {stderr}");
    assert!(
        stderr.contains("refusing to modify yarn.lock") && stderr.contains("yarn install"),
        "the gate must name the refusal and the yarn remedy: {stderr}"
    );
    assert!(
        !dir.join("node_modules/is-positive").exists(),
        "nothing may be installed past the gate"
    );

    // Explicit rewrite request → refused upfront, same gate.
    let (_, stderr2, code2) = run_install(&dir, &["install", "--no-frozen-lockfile"]);
    assert_ne!(code2, 0, "--no-frozen-lockfile must be refused: {stderr2}");
    assert!(
        stderr2.contains("refusing to modify yarn.lock"),
        "the explicit-rewrite path must hit the same gate: {stderr2}"
    );

    assert_eq!(
        std::fs::read_to_string(dir.join("yarn.lock")).unwrap(),
        yarn_lock,
        "yarn.lock must be byte-identical after refused installs"
    );
    assert!(
        !dir.join("aube-lock.yaml").exists(),
        "the gate must not leave an aube-lock.yaml behind"
    );
}

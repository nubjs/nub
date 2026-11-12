//! The nub self-shim, behaviorally, through the binary: a workspace-root
//! `packageManager: "nub@X.Y.Z"` pinning a DIFFERENT nub provisions that nub from
//! the release channel (redirected to a `file://` fixture via the internal
//! `NUB_RELEASE_BASE_URL` seam) and execs it. Unix-only: the fixture's `bin/nub`
//! is a shell script, and the delegate handoff is a POSIX `exec`; the Windows
//! spawn path is exercised by the windows-latest leg separately.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

/// The release-artifact target for this host — the subset of `platform_target()`
/// the Unix `Test` legs run on. `None` on a platform with no mapping (the test
/// then skips, mirroring the shim's own graceful platform fallback).
fn host_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        _ => None,
    }
}

fn scratch(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-selfshim-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Stage a fake nub release at `<releases>/v<ver>/nub-<target>.tar.gz` (+ `.sha256`)
/// whose `bin/nub` prints a marker naming the verb and the re-entry guard, and
/// reports `v<ver>` to `--version` (so provision-then-verify passes).
fn stage_fake_release(releases: &Path, target: &str, ver: &str) {
    let stage = releases.join(format!(".stage-{ver}"));
    std::fs::create_dir_all(stage.join("bin")).unwrap();
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"v{ver}\"; exit 0; fi\n\
         echo \"FAKE-NUB verb=$1 guard=${{__NUB_SELF_DISPATCHED:-UNSET}}\"\nexit 0\n"
    );
    let bin = stage.join("bin").join("nub");
    std::fs::write(&bin, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let vdir = releases.join(format!("v{ver}"));
    std::fs::create_dir_all(&vdir).unwrap();
    let tarball = format!("nub-{target}.tar.gz");
    assert!(
        Command::new("tar")
            .arg("-czf")
            .arg(vdir.join(&tarball))
            .arg("-C")
            .arg(&stage)
            .arg("bin")
            .status()
            .unwrap()
            .success(),
        "tar the fake release"
    );
    let sum = Command::new("shasum")
        .args(["-a", "256"])
        .arg(&tarball)
        .current_dir(&vdir)
        .output()
        .unwrap();
    assert!(sum.status.success(), "shasum the fake release");
    std::fs::write(vdir.join(format!("{tarball}.sha256")), &sum.stdout).unwrap();
}

/// Run `nub <args>` in `project`, self-shim ENABLED, provisioning redirected to
/// the `file://` release fixture and the store isolated to a fresh cache root.
fn run(project: &Path, releases: &Path, cache: &Path, args: &[&str]) -> String {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(project)
        .env(
            "NUB_RELEASE_BASE_URL",
            format!("file://{}", releases.display()),
        )
        .env("XDG_CACHE_HOME", cache)
        .env("XDG_DATA_HOME", cache.join("data"))
        // A dead-port registry so an in-process (non-delegated) install can't
        // reach the network — any accidental non-delegation fails loud, not slow.
        .env("XDG_CONFIG_HOME", cache.join("config"))
        .output()
        .expect("spawn nub");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn write_manifest(project: &Path, pin: &str) {
    std::fs::write(
        project.join("package.json"),
        format!(r#"{{"name":"app","version":"1.0.0","packageManager":"{pin}"}}"#),
    )
    .unwrap();
    std::fs::write(project.join(".npmrc"), "registry=http://127.0.0.1:1/\n").unwrap();
}

#[test]
fn differing_exact_pin_provisions_verifies_and_delegates() {
    let Some(target) = host_target() else {
        return; // no release mapping for this host — the shim falls through
    };
    let root = scratch("delegate");
    let releases = root.join("releases");
    std::fs::create_dir_all(&releases).unwrap();
    stage_fake_release(&releases, target, "99.99.99");
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    write_manifest(&project, "nub@99.99.99");
    let cache = root.join("cache");

    let out = run(&project, &releases, &cache, &["install"]);
    assert!(
        out.contains("provisioning pinned nub@99.99.99"),
        "provisions the pinned nub: {out}"
    );
    assert!(
        out.contains("FAKE-NUB verb=install"),
        "execs the delegated binary: {out}"
    );
    assert!(
        out.contains("guard=99.99.99"),
        "the re-entry guard is set on the child: {out}"
    );

    // Second run is a verified store hit — no re-provision.
    let out2 = run(&project, &releases, &cache, &["install"]);
    assert!(
        out2.contains("FAKE-NUB verb=install"),
        "still delegates: {out2}"
    );
    assert!(
        !out2.contains("provisioning pinned nub@99.99.99"),
        "cache hit does not re-provision: {out2}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn matching_pin_does_not_delegate() {
    let Some(target) = host_target() else {
        return;
    };
    let root = scratch("match");
    let releases = root.join("releases");
    std::fs::create_dir_all(&releases).unwrap();
    // Stage a release AT the running version — if the shim wrongly treated a
    // matching pin as a mismatch it would provision + exec this and print FAKE-NUB.
    let running = env!("CARGO_PKG_VERSION");
    stage_fake_release(&releases, target, running);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    write_manifest(&project, &format!("nub@{running}"));
    let cache = root.join("cache");

    let out = run(&project, &releases, &cache, &["install"]);
    assert!(
        !out.contains("FAKE-NUB") && !out.contains("provisioning pinned nub"),
        "a matching pin runs in-process, never delegates: {out}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn non_exact_pin_notices_and_runs_in_process() {
    let root = scratch("nonexact");
    let releases = root.join("releases");
    std::fs::create_dir_all(&releases).unwrap();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    write_manifest(&project, "nub@0.2");
    let cache = root.join("cache");

    let out = run(&project, &releases, &cache, &["install"]);
    assert!(
        out.contains("isn't an exact version"),
        "a range pin prints the not-exact notice: {out}"
    );
    assert!(
        !out.contains("FAKE-NUB"),
        "a range pin never delegates: {out}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

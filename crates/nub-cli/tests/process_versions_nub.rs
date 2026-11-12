//! `process.versions.nub` — the self-identification marker.
//!
//! Augmented `nub` publishes `process.versions.nub` = the running binary's
//! version, following the universal `process.versions.<runtime>` convention
//! (cf. `.bun`, `.electron`) so tooling can detect "running under nub". The
//! value is sourced from the binary itself (`env!("CARGO_PKG_VERSION")`, carried
//! to the preload via the internal `__NUB_VERSION` env var), so this test asserts
//! it against the same constant the binary was built with.
//!
//! The marker is present ONLY when augmentation is active (the preload runs).
//! Under `--node` / `NODE_COMPAT` no preload runs, so it is correctly absent —
//! the plain-Node fingerprint. The mechanism is tier-independent (both preload
//! entries call the same installer), so this host-node test covers whichever
//! tier the runner's Node falls on.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

fn fixture() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    Path::new(&manifest).join("../../tests/fixtures/process-versions/versions.js")
}

/// Run the fixture under `nub [extra_args] [env]` and parse its JSON.
fn run(extra_args: &[&str], env: &[(&str, &str)]) -> serde_json::Value {
    let f = fixture();
    let mut cmd = Command::new(nub_binary());
    cmd.args(extra_args)
        .arg(&f)
        .current_dir(f.parent().unwrap());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("failed to spawn nub");
    assert!(
        output.status.success(),
        "nub exited {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim())
        .expect("fixture must emit valid JSON")
}

/// Augmented run: the marker equals the binary's version and mirrors the shape
/// of Node's native `process.versions` entries (enumerable, configurable,
/// non-writable).
#[test]
fn augmented_publishes_binary_version() {
    let v = run(&[], &[]);
    assert_eq!(
        v["nub"].as_str(),
        Some(env!("CARGO_PKG_VERSION")),
        "process.versions.nub must equal the running binary's version"
    );

    let desc = &v["desc"];
    assert_eq!(
        desc["writable"], false,
        "match native entries: non-writable"
    );
    assert_eq!(desc["enumerable"], true, "match native entries: enumerable");
    assert_eq!(
        desc["configurable"], true,
        "match native entries: configurable"
    );
}

/// `--node` disables augmentation (no preload), so the marker is absent — the
/// plain-Node fingerprint a tool checking `process.versions.nub` should see.
#[test]
fn node_compat_flag_omits_marker() {
    let v = run(&["--node"], &[]);
    assert!(
        v["nub"].is_null(),
        "--node must not publish process.versions.nub, got {:?}",
        v["nub"]
    );
}

/// `NODE_COMPAT=1` is the persistent, tree-wide augmentation opt-out; same
/// contract as `--node` — the marker is absent.
#[test]
fn node_compat_env_omits_marker() {
    let v = run(&[], &[("NODE_COMPAT", "1")]);
    assert!(
        v["nub"].is_null(),
        "NODE_COMPAT=1 must not publish process.versions.nub, got {:?}",
        v["nub"]
    );
}

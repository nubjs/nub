//! Process-identity fidelity: `nub` must spawn node with argv0 set to `"node"`
//! so the spawned process reports `process.title` and `process.argv0` as
//! `"node"`, matching plain `node` invoked by PATH name.  `process.execPath`
//! must remain the absolute binary path — it is derived from the resolved
//! executable, not from argv0, so the two invariants are independent.
//!
//! Applies to both the augmented path and `--node` compat mode.
//!
//! Platform note — the `"node"` invariant is **Unix-only**. The spawn-side fix
//! is `CommandExt::arg0("node")`, which exists only on Unix; Windows has no
//! argv0 channel (a process receives a single command-line string whose first
//! token, by universal launcher convention, is the executable path). Worse,
//! `process.title` on Windows is NOT argv0-derived at all: libuv's
//! `uv_get_process_title` reads `GetModuleFileNameW(NULL)` — the OS image path
//! — so it is always the absolute `node.exe` path regardless of how the child
//! was launched, and `process.argv0` is the verbatim command-line token[0]
//! (also the full path). Critically, **plain Windows `node` reports the exact
//! same full path** for both fields, so nub does not diverge from Node there —
//! there is nothing to "fix" and nothing the spawner can do to force `"node"`.
//! Hence on Windows we assert the platform-true contract (the binary path
//! passes through, matching plain Node) rather than the Unix `"node"` value.
//! The cross-platform invariant — `process.execPath` is the absolute binary
//! path, never `"node"` — is asserted everywhere.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/ or release/
    path.push("nub");
    path
}

fn fixtures_dir() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    Path::new(&manifest).join("../../tests/fixtures")
}

/// Run `nub [extra_args] <fixture>` and parse the JSON the fixture emits.
fn run_identity(extra_args: &[&str]) -> serde_json::Value {
    let fixture = fixtures_dir().join("process-identity/identity.js");
    let output = Command::new(nub_binary())
        .args(extra_args)
        .arg(&fixture)
        .current_dir(fixture.parent().unwrap())
        .output()
        .expect("failed to spawn nub");
    assert!(
        output.status.success(),
        "nub exited {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).expect("fixture must emit valid JSON")
}

/// Assert the process-identity invariants on the fixture's JSON.
///
/// `mode` labels the spawn path (augmented vs `--node`) so a CI failure names
/// which one diverged. `execPath` is checked on every platform; `title`/`argv0`
/// are the Unix-only `"node"` invariant (see the module docs for why Windows
/// cannot and need not honor it).
fn assert_identity(v: &serde_json::Value, mode: &str) {
    // Cross-platform: execPath is the absolute binary path, never "node".
    // argv0 must not have leaked into it.
    assert!(
        v["execPathIsAbsolute"].as_bool().unwrap(),
        "{mode}: process.execPath must remain an absolute path (must not become \"node\")"
    );

    let title = v["title"].as_str().unwrap();
    let argv0 = v["argv0"].as_str().unwrap();

    if cfg!(unix) {
        // The arg0("node") fix is in effect: both fields read back as "node",
        // matching plain `node` invoked by PATH name.
        assert_eq!(
            title, "node",
            "{mode}: process.title must be \"node\" on Unix (was the full binary path before the fix)"
        );
        assert_eq!(
            argv0, "node",
            "{mode}: process.argv0 must be \"node\" on Unix"
        );
    } else {
        // Windows has no argv0 channel and `process.title` is GetModuleFileNameW,
        // so both fields are the absolute node.exe path — identical to plain
        // Windows `node`. We assert that platform-true contract (the path passes
        // through unmangled) rather than the impossible "node".
        assert!(
            std::path::Path::new(title).is_absolute() && title.to_lowercase().ends_with("node.exe"),
            "{mode}: on Windows process.title is the OS image path (GetModuleFileNameW); \
             expected an absolute path ending in node.exe, got {title:?}"
        );
        assert!(
            std::path::Path::new(argv0).is_absolute() && argv0.to_lowercase().ends_with("node.exe"),
            "{mode}: on Windows process.argv0 is command-line token[0] (the full node.exe path); \
             expected an absolute path ending in node.exe, got {argv0:?}"
        );
    }
}

/// Augmented mode: on Unix `process.title`/`process.argv0` are `"node"`; on
/// Windows they are the absolute `node.exe` path (matching plain Node).
/// `process.execPath` is an absolute path (not `"node"`) everywhere.
#[test]
fn augmented_spawn_reports_node_as_title_and_argv0() {
    let v = run_identity(&[]);
    assert_identity(&v, "augmented");
}

/// `--node` compat mode mirrors the augmented path: the Unix fix applies there
/// too, and Windows behaves identically to plain Node.
// @lat: [[compat-mode-tests#Compat mode#Compat mode reports node as the process title and argv0]]
#[test]
fn node_compat_mode_reports_node_as_title_and_argv0() {
    let v = run_identity(&["--node"]);
    assert_identity(&v, "--node");
}

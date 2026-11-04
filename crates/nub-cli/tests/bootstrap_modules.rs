//! Bootstrap-module budget for an augmented startup.
//!
//! Every internal module the preload pulls in is compiled on EVERY augmented
//! process, so the preload's whole design is to defer them: the ESM side-effect
//! polyfills load lazily, `navigator` is version-gated rather than read, and the
//! `MessageEvent.ports` freeze is skipped where Node's own undici-backed global
//! already provides it. That last gate is invisible in ordinary behavior tests —
//! it regressed once and cost ~112 extra modules per startup with no functional
//! symptom — so it needs a test that measures the module set directly.
//!
//! `nub --node` runs the same fixture with the preload off, which makes the
//! baseline a differential against this exact Node rather than a pinned number.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Realizing any of Node >= 22.3's undici-backed lazy globals loads
/// `internal/deps/undici/undici`, which drags `http` in behind it. Their presence
/// in an augmented bootstrap means the preload touched one — and both are
/// tier-independent (neither is part of nub's own preload machinery on either
/// tier). `worker_threads` is deliberately NOT a marker: `module.register` on the
/// compat tier loads it legitimately (the async loader runs on a worker thread),
/// so it appears there independent of any undici regression and would false-positive.
const UNDICI_TREE_MARKERS: [&str; 2] = [
    "NativeModule http",
    "NativeModule internal/deps/undici/undici",
];

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/ or release/
    path.push("nub");
    path
}

fn fixture() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    Path::new(&manifest).join("../../tests/fixtures/bootstrap-modules/modules.js")
}

fn run(extra_args: &[&str]) -> serde_json::Value {
    let f = fixture();
    let mut cmd = Command::new(nub_binary());
    cmd.args(extra_args)
        .arg(&f)
        .current_dir(f.parent().unwrap());
    let output = cmd.output().expect("failed to spawn nub");
    assert!(
        output.status.success(),
        "nub {:?} exited {:?}\nstderr: {}",
        extra_args,
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim())
        .expect("fixture must emit valid JSON")
}

fn modules(v: &serde_json::Value) -> Vec<String> {
    v["modules"]
        .as_array()
        .expect("modules array")
        .iter()
        .map(|m| m.as_str().unwrap().to_string())
        .collect()
}

/// Tier-independent: no preload path may realize an undici-backed lazy global.
/// Below Node 22.3 the markers are absent for a different reason (`MessageEvent`
/// comes from `internal/worker/io` there), which is equally correct — the
/// assertion is that a plain script never pays for the HTTP/streams tree.
#[test]
fn augmented_startup_leaves_the_undici_module_tree_unloaded() {
    let augmented = run(&[]);
    assert!(
        augmented["augmented"].is_string(),
        "fixture did not run augmented (process.versions.nub absent); the rest of this test is vacuous"
    );
    let loaded = modules(&augmented);
    for marker in UNDICI_TREE_MARKERS {
        assert!(
            !loaded.iter().any(|m| m == marker),
            "augmented startup loaded `{marker}` — a preload step realized an \
             undici-backed lazy global. Bootstrap set was {} modules: {loaded:?}",
            loaded.len()
        );
    }
}

/// The augmented bootstrap set must stay within a few modules of plain Node's on
/// the fast tier. The budget is deliberately loose — it is a cliff detector for a
/// whole internal tree being pulled in, not a golden count that churns with every
/// Node release. The compat tier is excluded: its `--import` preload eagerly loads
/// the worker/locks ESM polyfills by design, so it has a legitimately larger set.
#[test]
fn fast_tier_augmentation_stays_within_its_bootstrap_budget() {
    const BUDGET: i64 = 25;

    let plain = run(&["--node"]);
    let node = plain["node"].as_str().unwrap();
    let (major, minor) = {
        let mut parts = node.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
        (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
    };
    if major < 22 || (major == 22 && minor < 15) {
        eprintln!("skipped: Node {node} is on the compat tier");
        return;
    }

    let augmented = run(&[]);
    let (plain_count, augmented_count) = (
        plain["count"].as_i64().unwrap(),
        augmented["count"].as_i64().unwrap(),
    );
    let extra = augmented_count - plain_count;
    assert!(
        extra <= BUDGET,
        "augmentation added {extra} bootstrap modules on Node {node} (plain {plain_count} -> \
         augmented {augmented_count}), over the {BUDGET} budget. Something in the preload is \
         realizing a lazy global or eagerly loading an ESM polyfill."
    );
}

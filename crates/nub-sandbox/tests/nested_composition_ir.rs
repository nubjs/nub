//! Hermetic IR-level proof of the nested-composition ceiling accumulation (epic D7).
//!
//! The end-to-end kernel behavior is proven on a set-up Ubuntu host by
//! `linux_nested_composition.rs`; THIS test proves the compiler half with no kernel:
//! feeding each level's constructed env back in as the next level's ambient snapshot
//! (exactly what a nested `apply_with_runtime` sees when it compiles from its current
//! view) yields a monotonically TIGHTENING policy — a withheld value never refills,
//! and a value dropped at level 2 stays dropped. It runs on every OS because
//! `compile` is the engine-pure half.

use nub_sandbox::policy::Effect;
use nub_sandbox::{CompileCtx, Homes, SandboxPolicy, compile};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The per-level surface the composition harness builds: read `/`, a writable declared
/// root, the built-in `.env` mask, a monotone env ceiling (level 1 keeps INHERITED +
/// TIGHTEN_ME; level 2+ keep only INHERITED), and a network ceiling that denies at
/// level 1 and requests FULL below it.
fn surface(level: u32, allowed: &str) -> Value {
    let mut fs_map = Map::new();
    fs_map.insert("/".into(), json!("r"));
    fs_map.insert(allowed.into(), json!("rw"));
    let env = if level == 1 {
        json!(["INHERITED", "TIGHTEN_ME"])
    } else {
        json!(["INHERITED"])
    };
    let net = if level == 1 {
        json!(false)
    } else {
        json!(true)
    };
    json!({ "fs": Value::Object(fs_map), "env": env, "net": net })
}

fn compile_level(level: u32, ambient: BTreeMap<String, String>) -> SandboxPolicy {
    let proj = PathBuf::from("/var/tmp/nub-nest-ir/proj");
    let allowed = proj.join("allowed");
    let homes = Homes {
        home: PathBuf::from("/var/tmp/nub-nest-ir/home"),
        tmp: PathBuf::from("/tmp"),
        cache: PathBuf::from("/var/tmp/nub-nest-ir/cache"),
        project: proj.clone(),
    };
    let ctx = CompileCtx::new(homes, proj, true, ambient);
    compile(&surface(level, &allowed.to_string_lossy()), &ctx).expect("level policy compiles")
}

#[test]
fn env_ceiling_accumulates_monotonically_across_three_levels() {
    // Level 1's ambient is the driver's chosen snapshot: a scrubbed value to propagate,
    // a host-only secret to withhold, and a value that survives only this level.
    let driver = BTreeMap::from([
        ("INHERITED".to_string(), "scrubbed-parent-value".to_string()),
        ("HOST_SECRET".to_string(), "must-not-cross".to_string()),
        ("TIGHTEN_ME".to_string(), "present-at-level-1".to_string()),
    ]);

    let l1 = compile_level(1, driver);
    assert_eq!(
        l1.env.constructed.get("INHERITED").map(String::as_str),
        Some("scrubbed-parent-value"),
        "level 1 must keep the scrubbed value"
    );
    assert!(
        !l1.env.constructed.contains_key("HOST_SECRET"),
        "the host-only secret must never cross level 1"
    );
    assert!(
        l1.env.constructed.contains_key("TIGHTEN_ME"),
        "TIGHTEN_ME is present at level 1"
    );

    // Level 2 compiles from level 1's constructed env — the view a nested launch sees.
    let l2 = compile_level(2, l1.env.constructed.clone());
    assert_eq!(
        l2.env.constructed.get("INHERITED").map(String::as_str),
        Some("scrubbed-parent-value"),
        "the scrubbed value propagates to level 2"
    );
    assert!(
        !l2.env.constructed.contains_key("TIGHTEN_ME"),
        "level 2 drops TIGHTEN_ME — a monotone tighten"
    );
    assert!(
        !l2.env.constructed.contains_key("HOST_SECRET"),
        "the host-only secret cannot reappear at level 2"
    );

    // Level 3 compiles from level 2's constructed env: nothing refills.
    let l3 = compile_level(3, l2.env.constructed.clone());
    assert_eq!(
        l3.env.constructed.get("INHERITED").map(String::as_str),
        Some("scrubbed-parent-value"),
        "the scrubbed value survives to level 3"
    );
    assert!(
        !l3.env.constructed.contains_key("TIGHTEN_ME"),
        "a value dropped at level 2 stays dropped at level 3 (no widening)"
    );
    assert!(
        l3.env.enforce && l1.env.enforce && l2.env.enforce,
        "every level enforces its constructed env"
    );
}

#[test]
fn network_ceiling_denies_at_level_one_and_inner_levels_request_full() {
    let l1 = compile_level(1, BTreeMap::new());
    assert!(
        l1.net.enforce && l1.net.default_effect == Effect::Deny,
        "level 1 enforces a deny-all network ceiling"
    );
    // Level 2+ do NOT enforce (they REQUEST full) — the inherited kernel ceiling from
    // level 1 (empty netns + socket-family seccomp) is what still denies them; that is
    // the invariant `linux_nested_composition.rs` proves on a real kernel.
    let l2 = compile_level(2, BTreeMap::new());
    let l3 = compile_level(3, BTreeMap::new());
    assert!(
        !l2.net.enforce && !l3.net.enforce,
        "inner levels request full networking; the outer ceiling must still deny them"
    );
}

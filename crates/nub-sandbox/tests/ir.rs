//! IR serde round-trip + the conformance harness (compiler/matcher verdicts vs a
//! fixture manifest) + the apply() env-scrub skeleton.

mod common;

use nub_sandbox::compiler::compile;
use nub_sandbox::conformance::{Fixture, run_fixture};
use nub_sandbox::policy::SandboxPolicy;
use nub_sandbox::{CommandSpec, apply};
use serde_json::json;

#[test]
fn policy_round_trips_through_serde() {
    let ctx = common::ctx(true, &[("PORT", "3000"), ("NODE_ENV", "prod")]);
    let policy = compile(
        &json!({
            "fs": ["...", "!~/.ssh", "./data"],
            "net": ["*.sentry.io", "10.0.0.0/8"],
            "env": { "PORT": "port", "NODE_ENV": true }
        }),
        &ctx,
    )
    .unwrap();

    let text = serde_json::to_string(&policy).unwrap();
    let back: SandboxPolicy = serde_json::from_str(&text).unwrap();
    assert_eq!(
        policy, back,
        "IR must round-trip byte-for-byte through serde"
    );
}

#[test]
fn conformance_fixture_passes_when_verdicts_match() {
    let ctx = common::ctx(true, &[("KEEP", "1"), ("DROP_TOKEN", "s")]);
    let proj = common::homes().project;
    let fixture: Fixture = serde_json::from_value(json!({
        "name": "basic",
        "sandbox": {
            "fs": ["...", "./build"],
            "net": ["github.com"],
            "env": ["KEEP", "!*_TOKEN"]
        },
        "fs": [
            { "path": format!("{}/build/out", proj.display()), "read": true, "write": true },
            { "path": format!("{}/.env", proj.display()), "read": false, "write": false }
        ],
        "net": [
            { "host": "github.com", "admit": true },
            { "host": "evil.com", "admit": false }
        ],
        "env": [
            { "key": "KEEP", "present": true, "value": "1" },
            { "key": "DROP_TOKEN", "present": false }
        ]
    }))
    .unwrap();

    let mismatches = run_fixture(&fixture, &ctx);
    assert!(
        mismatches.is_empty(),
        "fixture should pass, got: {mismatches:?}"
    );
}

#[test]
fn conformance_reports_a_mismatch() {
    let ctx = common::ctx(true, &[]);
    let fixture: Fixture = serde_json::from_value(json!({
        "name": "wrong-expectation",
        "sandbox": { "net": false },   // deny all egress
        "net": [ { "host": "github.com", "admit": true } ] // wrong: it's denied
    }))
    .unwrap();
    let mismatches = run_fixture(&fixture, &ctx);
    assert_eq!(mismatches.len(), 1);
    assert_eq!(mismatches[0].axis, "net");
}

#[test]
fn apply_scrubs_env_when_enforced() {
    let ctx = common::ctx(true, &[("KEEP", "1"), ("SECRET_TOKEN", "s")]);
    let policy = compile(&json!({ "env": ["KEEP"] }), &ctx).unwrap();
    let prepared = apply(&policy, CommandSpec::new("true")).unwrap();
    // The env axis is enforced by construction; fs/net report as not-yet-enforced.
    let envs: std::collections::BTreeMap<_, _> = prepared
        .command
        .get_envs()
        .filter_map(|(k, v)| {
            v.map(|v| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
        })
        .collect();
    assert_eq!(envs.get("KEEP").map(String::as_str), Some("1"));
    assert!(
        !envs.contains_key("SECRET_TOKEN"),
        "secret withheld from child env"
    );
}

#[test]
fn apply_degradation_reflects_backend_capability() {
    let ctx = common::ctx(true, &[]);
    let cwd = std::env::current_dir().unwrap();
    let policy = compile(
        &json!({ "fs": [cwd.to_string_lossy().to_string()], "net": false }),
        &ctx,
    )
    .unwrap();
    let prepared = match apply(
        &policy,
        CommandSpec::new("true").cwd(&cwd).deny_search_root(&cwd),
    ) {
        Ok(prepared) => prepared,
        #[cfg(target_os = "linux")]
        Err(degradation) => {
            assert!(
                degradation.lost.contains(&"fs".to_string()),
                "missing Bubblewrap must fail the fs axis explicitly"
            );
            return;
        }
        #[cfg(not(target_os = "linux"))]
        Err(degradation) => panic!("backend setup failed unexpectedly: {degradation:?}"),
    };
    let d = &prepared.degradation;
    // macOS (Seatbelt) and Linux (Bubblewrap+seccomp) have real backends: fs +
    // deny-all net are genuinely enforced, so nothing is degraded. Other OSes still
    // run the env-scrub skeleton, which honestly reports fs + net as not-enforced.
    #[cfg(target_os = "macos")]
    assert!(d.is_full(), "macOS enforces fs + deny-all net");
    #[cfg(target_os = "linux")]
    assert!(
        d.is_full(),
        "Linux enforces fs + deny-all net with Bubblewrap"
    );
    // Windows (AppContainer) enforces the literal read-confine grant (`./x`) + coarse
    // deny-all net fully — BUT the default `.env*` READ-deny is now injected on every
    // read-granting policy, and a deny landing INSIDE a granted read subtree can't be
    // carved under the AppContainer allowlist model (an inheritable read-allow defeats
    // it — the AAP-class trap). So the backend honestly reports `fs-read-deny` (the
    // `.env*`-inside-grant residual), never silently leaving it unenforced. Net stays
    // fully enforced. (A future DACL inheritance-break mechanism would carve it and
    // remove this degradation.)
    #[cfg(target_os = "windows")]
    {
        assert!(
            d.lost.contains(&"fs-read-deny".to_string()),
            "Windows honestly reports the un-carvable `.env*`-inside-grant deny"
        );
        assert!(
            !d.lost.iter().any(|l| l.starts_with("net")),
            "coarse deny-all net is still fully enforced"
        );
    }
    // Any OS with no wired backend still runs the env-scrub skeleton, which honestly
    // reports fs + net as not-enforced.
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        assert!(!d.is_full(), "skeleton does not enforce fs/net");
        assert!(d.lost.contains(&"fs".to_string()));
        assert!(d.lost.contains(&"net".to_string()));
    }
}

//! IR serde round-trip + the conformance harness (compiler/matcher verdicts vs a
//! fixture manifest) + the apply() env-scrub skeleton.

mod common;

use nub_sandbox::CommandSpec;
#[cfg(not(target_os = "linux"))]
use nub_sandbox::apply;
use nub_sandbox::compiler::compile;
use nub_sandbox::conformance::{Fixture, run_fixture};
use nub_sandbox::policy::SandboxPolicy;
use serde_json::json;

#[cfg(target_os = "linux")]
const TRUE_PROGRAM: &str = "/usr/bin/true";
#[cfg(not(target_os = "linux"))]
const TRUE_PROGRAM: &str = "true";

#[cfg(target_os = "linux")]
fn apply(
    policy: &SandboxPolicy,
    spec: CommandSpec,
) -> Result<nub_sandbox::Prepared, nub_sandbox::Degradation> {
    static RUNTIME: std::sync::OnceLock<nub_sandbox::RuntimeCapability> =
        std::sync::OnceLock::new();
    let runtime = RUNTIME.get_or_init(|| {
        nub_sandbox::RuntimeCapability::from_verified_executable(env!(
            "CARGO_BIN_EXE_nub-sandbox-monitor-harness"
        ))
        .expect("verify the purpose-built sandbox monitor harness")
    });
    nub_sandbox::apply_with_runtime(policy, spec, runtime)
}

#[test]
fn policy_round_trips_through_serde() {
    let ctx = common::ctx(true, &[("PORT", "3000"), ("NODE_ENV", "prod")]);
    let policy = compile(
        &json!({
            "fs": ["...", "!~/.ssh", "./data"],
            "net": ["*.sentry.io", "10.0.0.0/8"],
            "proxy": "auto",
            "vars": { "PORT": "port", "NODE_ENV": true }
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
fn broker_ir_round_trips_without_serializing_the_parent_secret() {
    let ctx = common::ctx(true, &[("STRIPE_TOKEN", "real-parent-secret")]);
    let policy = compile(
        &json!({
            "net": { "api.stripe.com": { "env": ["STRIPE_TOKEN"] } },
            "vars": true
        }),
        &ctx,
    )
    .unwrap();
    let text = serde_json::to_string(&policy).unwrap();
    assert!(!text.contains("real-parent-secret"));
    let back: SandboxPolicy = serde_json::from_str(&text).unwrap();
    assert_eq!(policy, back);
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
            "proxy": "auto",
            "vars": ["KEEP", "!*_TOKEN"]
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
    let policy = compile(&json!({ "vars": ["KEEP"] }), &ctx).unwrap();
    let _prepared = apply(&policy, CommandSpec::new(TRUE_PROGRAM)).unwrap();
    assert_eq!(
        policy.env.constructed.get("KEEP").map(String::as_str),
        Some("1")
    );
    assert!(
        !policy.env.constructed.contains_key("SECRET_TOKEN"),
        "secret withheld from child env"
    );
}

#[cfg(unix)]
#[test]
fn apply_replaces_a_brokered_parent_value_with_a_fresh_marker() {
    let home = std::env::var("HOME").expect("test parent has HOME");
    let ambient = [("HOME", home.as_str())];
    let ctx = common::ctx(true, &ambient);
    let policy = compile(
        &json!({
            "fs": true,
            "net": { "api.example.com": { "env": ["HOME"] } },
            "vars": true
        }),
        &ctx,
    )
    .unwrap();
    assert!(!policy.env.constructed.contains_key("HOME"));

    let run = || {
        let output = apply(&policy, CommandSpec::new("/usr/bin/env"))
            .unwrap()
            .output()
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(!stdout.contains(&format!("HOME={home}")));
        stdout
            .lines()
            .find_map(|line| line.strip_prefix("HOME="))
            .expect("broker marker is present in the child env")
            .to_string()
    };
    let first = run();
    let second = run();
    assert!(first.starts_with("nub-credential-v1-"));
    assert_ne!(first, second, "each apply must mint a fresh marker");
}

#[test]
fn apply_fails_closed_when_a_brokered_parent_value_is_missing() {
    const MISSING: &str = "NUB_TEST_DEFINITELY_MISSING_BROKER_CREDENTIAL_7B1D";
    assert!(std::env::var_os(MISSING).is_none());
    let ctx = common::ctx(true, &[]);
    let policy = compile(
        &json!({
            "fs": true,
            "net": { "api.example.com": { "env": [MISSING] } },
            "vars": true
        }),
        &ctx,
    )
    .unwrap();
    let degradation = match apply(&policy, CommandSpec::new(TRUE_PROGRAM)) {
        Ok(_) => panic!("missing broker credential unexpectedly produced a launchable child"),
        Err(degradation) => degradation,
    };
    assert_eq!(degradation.lost, vec!["credential-broker"]);
    assert!(
        degradation
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains(MISSING) && !reason.contains("secret"))
    );
}

#[cfg(unix)]
#[test]
fn apply_uses_the_compile_time_environment_snapshot() {
    const KEY: &str = "NUB_SANDBOX_COMPILE_SNAPSHOT_TEST";
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var(KEY, value) },
                None => unsafe { std::env::remove_var(KEY) },
            }
        }
    }

    let restore = Restore(std::env::var_os(KEY));
    let ctx = common::ctx(true, &[(KEY, "compiled")]);
    let policy = compile(&json!(false), &ctx).unwrap();
    unsafe { std::env::set_var(KEY, "mutated-after-compile") };

    let output = apply(&policy, CommandSpec::new("/usr/bin/env"))
        .unwrap()
        .output()
        .unwrap();
    drop(restore);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.lines().any(|line| line == format!("{KEY}=compiled")));
    assert!(!stdout.contains("mutated-after-compile"));
}

#[test]
fn apply_rejects_an_unresolved_direct_ir_environment() {
    let result = apply(&SandboxPolicy::default(), CommandSpec::new("/usr/bin/true"));
    let degradation = match result {
        Ok(_) => panic!("unresolved direct IR unexpectedly reached a backend"),
        Err(degradation) => degradation,
    };
    assert_eq!(degradation.lost, vec!["env-unresolved"]);
    assert!(
        degradation
            .reason
            .as_deref()
            .is_some_and(|reason| { reason.contains("no resolved target environment") })
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
        CommandSpec::new(TRUE_PROGRAM)
            .cwd(&cwd)
            .deny_search_root(&cwd),
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
        #[cfg(target_os = "windows")]
        Err(degradation) => {
            assert!(
                degradation.lost.contains(&"fs-root".to_string())
                    || degradation.lost.contains(&"fs-read-deny".to_string()),
                "Windows must fail either the clean-root precondition or the nested read deny"
            );
            return;
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
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
    // Any OS with no wired backend still runs the env-scrub skeleton, which honestly
    // reports fs + net as not-enforced.
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        assert!(!d.is_full(), "skeleton does not enforce fs/net");
        assert!(d.lost.contains(&"fs".to_string()));
        assert!(d.lost.contains(&"net".to_string()));
    }
}

//! IR serde round-trip + the conformance harness (compiler/matcher verdicts vs a
//! fixture manifest) + the apply() env-scrub skeleton.

mod common;

use nub_sandbox::CommandSpec;
#[cfg(not(target_os = "linux"))]
use nub_sandbox::apply;
use nub_sandbox::compiler::CompileError;
use nub_sandbox::compiler::compile;
use nub_sandbox::conformance::{Fixture, run_fixture};
use nub_sandbox::policy::{FsOrigin, SandboxPolicy};
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

/// Returns true when the caller must skip the `apply()` leg of a test.
///
/// Linux `apply()` launches through a real Bubblewrap, which needs a working
/// unprivileged user namespace. The hosted `ubuntu-latest` image ships no `bwrap`
/// AND sets `apparmor_restrict_unprivileged_userns=1`, so no amount of staging the
/// packaged fallback makes enforcement reachable there — the primitive is simply
/// absent. Skip rather than fail, and let a prepared conformance runner set
/// `NUB_SANDBOX_REQUIRE_BWRAP` to turn the same call into a hard admission, so an
/// unconfigured host can never report a hollow green. Same gate as
/// `linux_enforcement.rs`; every assertion that does NOT need a launched child stays
/// outside it.
#[cfg(target_os = "linux")]
fn skip_without_bwrap() -> bool {
    nub_sandbox::host_probe::skip_without_bwrap("ir (apply() leg)")
}

#[cfg(not(target_os = "linux"))]
fn skip_without_bwrap() -> bool {
    false
}

#[test]
fn policy_round_trips_through_serde() {
    let ctx = common::ctx(true, &[("PORT", "3000"), ("NODE_ENV", "prod")]);
    let policy = compile(
        &json!({
            // `$tooldirs` expands to speculative rules, so this one surface covers both
            // `FsOrigin` arms: the authored globs must keep serializing without an
            // `origin` key (older fixtures round-trip unchanged), while a set member must
            // carry its origin across the wire — the mount plan reads it to decide whether
            // an absent bind source is an authoring mistake or just a missing toolchain.
            "fs": ["**", "!~/.ssh", "./data", "$tooldirs"],
            "net": ["*.sentry.io", "10.0.0.0/8"],
            "vars": { "PORT": "port", "NODE_ENV": true }
        }),
        &ctx,
    )
    .unwrap();
    assert!(
        policy
            .fs
            .rules
            .entries
            .iter()
            .any(|rule| rule.origin == FsOrigin::Speculative),
        "the $tooldirs expansion must mark its rules as speculative"
    );

    let text = serde_json::to_string(&policy).unwrap();
    let back: SandboxPolicy = serde_json::from_str(&text).unwrap();
    assert_eq!(
        policy, back,
        "IR must round-trip byte-for-byte through serde"
    );
    assert_eq!(
        text.matches("\"origin\"").count(),
        policy
            .fs
            .rules
            .entries
            .iter()
            .filter(|rule| rule.origin == FsOrigin::Speculative)
            .count(),
        "only a speculative rule may put an origin key on the wire"
    );
}

#[test]
fn broker_ir_round_trips_without_serializing_the_parent_secret() {
    let ctx = common::ctx(true, &[("STRIPE_TOKEN", "real-parent-secret")]);
    let policy = compile(
        &json!({
            "net": ["api.stripe.com"],
            "secrets": { "STRIPE_TOKEN": { "brokerTo": ["api.stripe.com"] } },
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
            "fs": ["**", "./build"],
            "net": ["github.com"],
            "vars": ["KEEP", "!*_TOKEN"]
        },
        "fs": [
            { "path": format!("{}/build/out", proj.display()), "read": true, "write": true },
            { "path": format!("{}/.env", proj.display()), "read": false, "write": false },
            // The secret-file floor denies a project-local `.npmrc` (hardcoded registry
            // token) at the root AND nested, even inside the `**` read grant.
            { "path": format!("{}/.npmrc", proj.display()), "read": false, "write": false },
            { "path": format!("{}/packages/api/.npmrc", proj.display()), "read": false, "write": false }
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
    // `fs: true` keeps the ENV axis the only thing under test. Under the default
    // deny-all fs the Linux backend refuses the launch because the inherited working
    // directory is outside the resulting filesystem view, so apply() would fail for a
    // reason this test is not about — and never reach env construction at all.
    let policy = compile(&json!({ "fs": true, "vars": ["KEEP"] }), &ctx).unwrap();
    assert_eq!(
        policy.env.constructed.get("KEEP").map(String::as_str),
        Some("1")
    );
    assert!(
        !policy.env.constructed.contains_key("SECRET_TOKEN"),
        "secret withheld from child env"
    );
    if skip_without_bwrap() {
        return;
    }
    apply(&policy, CommandSpec::new(TRUE_PROGRAM))
        .expect("an enforced env policy must still prepare a launchable command");
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
            "net": ["api.example.com"],
            "secrets": { "HOME": { "brokerTo": ["api.example.com"] } },
            "vars": true
        }),
        &ctx,
    )
    .unwrap();
    assert!(!policy.env.constructed.contains_key("HOME"));
    // Everything below reads the LAUNCHED child's environment, so unlike the scrub test
    // above there is no assertion left to make without a working backend.
    if skip_without_bwrap() {
        return;
    }

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

/// Brokering is only usable if the child can VERIFY the leaves the proxy mints, so the
/// trust bundle reaching it is part of the contract — and the bundle is delivered as
/// bytes copied out of an anonymous descriptor, where an emitted-the-right-option
/// assertion proves nothing about what the child can read. Assert the certificate.
#[cfg(target_os = "linux")]
#[test]
fn a_brokering_child_can_read_the_minted_trust_bundle() {
    let home = std::env::var("HOME").expect("test parent has HOME");
    let ambient = [("HOME", home.as_str())];
    let ctx = common::ctx(true, &ambient);
    let policy = compile(
        &json!({
            "fs": true,
            "net": ["api.example.com"],
            "secrets": { "HOME": { "brokerTo": ["api.example.com"] } },
            "vars": true
        }),
        &ctx,
    )
    .unwrap();
    if skip_without_bwrap() {
        return;
    }
    let output = apply(
        &policy,
        // Echoing the path first is what keeps this from passing on a HOST trust store:
        // the bundle is only proof of delivery if it was read from the sandbox-private
        // destination, which no path outside the sandbox resolves to.
        CommandSpec::new("/bin/sh")
            .args(["-c", "echo \"at=$SSL_CERT_FILE\"; cat \"$SSL_CERT_FILE\""]),
    )
    .unwrap()
    .output()
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success()
            && stdout.contains("at=/dev/.nub-sandbox/support/")
            && stdout.contains("-----BEGIN CERTIFICATE-----"),
        // A whole trust bundle is hundreds of KB of PEM; the `at=` line and the first
        // certificate header are the entire diagnostic.
        "the child must read a certificate from the sandbox-private bundle: \
         status={:?} stdout={:?} stderr={}",
        output.status,
        &stdout[..stdout.len().min(300)],
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn brokered_secret_missing_from_the_environment_fails_at_compile() {
    // A brokered secret is a REQUIRED env entry (a broker must hold a real value to
    // swap), so a value absent from the compile-time ambient fails CLOSED at compile
    // — earlier than the old net-axis form, which deferred the check to apply. (The
    // apply-time fail-closed path still exists in the backend/proxy and is covered by
    // the mitm `from_policy` unit tests.)
    const MISSING: &str = "NUB_TEST_DEFINITELY_MISSING_BROKER_CREDENTIAL_7B1D";
    assert!(std::env::var_os(MISSING).is_none());
    let ctx = common::ctx(true, &[]);
    let err = compile(
        &json!({
            "fs": true,
            "net": ["api.example.com"],
            "secrets": { MISSING: { "brokerTo": ["api.example.com"] } },
            "vars": true
        }),
        &ctx,
    )
    .unwrap_err();
    assert!(
        matches!(err, CompileError::MissingRequired { ref key } if key == MISSING),
        "a brokered secret absent from the environment must fail at compile: {err:?}"
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
    // Windows reaches here only on the success path, and every assertion below is cfg'd to
    // another target — the binding is genuinely unused there, not accidentally dropped.
    #[cfg_attr(target_os = "windows", allow(unused_variables))]
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

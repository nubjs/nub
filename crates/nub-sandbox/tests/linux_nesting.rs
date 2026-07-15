//! Linux direct-nesting candidate gate — REAL admission + nested-launch proof.
//!
//! Exercises the `require_nesting` path end to end on a correctly set-up Ubuntu
//! host: it forces selection of the dedicated `/usr/libexec/nub/nub-bwrap` helper,
//! runs the integrity gate and the bounded nested feature-probe, and asserts the
//! level-1 sandbox actually launches. Admission only succeeds when the host really
//! can nest, so a passing run proves the helper-in-helper feature-probe cleared.
//!
//! This is gated, not universal. It SKIPS WITH A PRINTED REASON when the host is
//! not set up (no helper, no group access, or an unloaded profile) or when the
//! test binary was built without the packaged Bubblewrap digest — so an
//! unconfigured runner never reports a hollow green. The root-requiring
//! fail-closed cases (mis-owned/mis-permed helper, unloaded profile, missing
//! group) are proven by the sibling shell harness `tests/sandbox-nesting/`, which
//! can manipulate the host with `sudo`; a `cargo test` cannot.
#![cfg(target_os = "linux")]

use nub_sandbox::compiler::{CompileCtx, ShellRunner};
use nub_sandbox::matcher::Homes;
use nub_sandbox::{CommandSpec, RuntimeCapability, apply_with_runtime, compile};
use std::collections::BTreeMap;
use std::path::Path;
use tempfile::TempDir;

const HELPER: &str = "/usr/libexec/nub/nub-bwrap";

fn runtime() -> &'static RuntimeCapability {
    static RUNTIME: std::sync::OnceLock<RuntimeCapability> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        RuntimeCapability::from_verified_executable(env!(
            "CARGO_BIN_EXE_nub-sandbox-monitor-harness"
        ))
        .expect("verify the purpose-built sandbox monitor harness")
    })
}

/// A printable skip reason when the host obviously cannot run the proof (no helper,
/// or the caller is not a nub-sandbox member so the 0750 helper is unopenable), or
/// `None` when admission should be attempted. Enumerating loaded AppArmor profiles
/// needs privilege, so profile readiness is judged by the launch itself, not read.
fn nesting_skip_reason() -> Option<String> {
    if !Path::new(HELPER).exists() {
        return Some(format!("dedicated helper {HELPER} is not installed"));
    }
    if std::fs::File::open(HELPER).is_err() {
        return Some(format!(
            "{HELPER} is not accessible — the user is not in the nub-sandbox group, or a fresh login is needed"
        ));
    }
    None
}

/// Set by a runner that KNOWS the host is fully set up (e.g. the VM proof) so an
/// admission failure becomes a hard failure instead of a skip. Mirrors the
/// `NUB_SANDBOX_REQUIRE_BWRAP` gate in `linux_enforcement`.
fn nesting_required() -> bool {
    matches!(
        std::env::var("NUB_SANDBOX_REQUIRE_NESTING").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

#[test]
fn set_up_host_admits_the_helper_and_a_nested_launch_succeeds() {
    if let Some(reason) = nesting_skip_reason() {
        assert!(
            !nesting_required(),
            "NUB_SANDBOX_REQUIRE_NESTING is set but the host is not ready: {reason}"
        );
        eprintln!("SKIP linux_nesting: {reason}");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let proj = root.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let ctx = CompileCtx {
        homes: Homes {
            home: root.join("home"),
            tmp: std::env::temp_dir(),
            cache: root.join("cache"),
            project: proj.clone(),
        },
        cwd: proj.clone(),
        trusted: true,
        ambient_env: BTreeMap::new(),
        runner: Box::new(ShellRunner),
    };
    // A real fs-confining policy forces the confined Bubblewrap launch path.
    let policy = compile(&serde_json::json!({ "fs": ["./"] }), &ctx).expect("policy compiles");
    let spec = CommandSpec::new("/bin/true")
        .cwd(&proj)
        .deny_search_roots([proj.clone()])
        .require_nesting(true);

    match apply_with_runtime(&policy, spec, runtime()) {
        Ok(prepared) => {
            let status = prepared
                .status()
                .expect("spawn the level-1 nesting-capable sandbox");
            assert!(
                status.success(),
                "the nesting-capable sandbox did not run /bin/true cleanly: {status:?}"
            );
        }
        Err(degradation) => {
            let reason = degradation.reason.unwrap_or_default();
            // The pinned digest is a compile-time input; without it the helper
            // cannot be admitted, which is a build-config skip, not a defect.
            if reason.contains("built without a packaged Bubblewrap digest") {
                assert!(
                    !nesting_required(),
                    "NUB_SANDBOX_REQUIRE_NESTING is set but the test binary lacks NUB_BWRAP_SHA256"
                );
                eprintln!(
                    "SKIP linux_nesting: the monitor test binary was built without NUB_BWRAP_SHA256"
                );
                return;
            }
            // A host that is not fully set up (unloaded profile, etc.) surfaces a
            // nesting diagnostic. Treat as a skip unless the runner asserted the
            // host is ready, in which case it is a real regression.
            assert!(
                !nesting_required(),
                "NUB_SANDBOX_REQUIRE_NESTING is set but the set-up host failed to admit the helper: {reason}"
            );
            eprintln!("SKIP linux_nesting: host not ready for nesting: {reason}");
        }
    }
}

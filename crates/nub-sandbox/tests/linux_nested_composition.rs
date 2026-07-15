//! Linux direct-nesting COMPOSITION proof (epic D7/D8) — end to end.
//!
//! Runs the purpose-built `nub-sandbox-nesting-harness` `drive` verb, which launches
//! THREE real nested sandboxes through the production `apply_with_runtime` path (two
//! concurrent level-2 siblings each creating a level-3 child) and self-asserts every
//! composition invariant: inherited-then-narrowed masks/write-ceilings, env non-refill,
//! the network + seccomp ceiling holding against an inner FULL request, distinct PID
//! namespaces, the raw exit-143-vs-SIGTERM distinction across a nesting boundary, and
//! zero survivors after a deep cancellation.
//!
//! Like `linux_nesting.rs`, this is GATED, not universal. It SKIPS WITH A PRINTED
//! REASON when the host is not set up for nesting (no dedicated helper, no group
//! access) or when the harness was built without the packaged Bubblewrap digest, so
//! an unconfigured runner never reports a hollow green. Set `NUB_SANDBOX_REQUIRE_NESTING`
//! on a host you KNOW is fully set up (e.g. the VM proof) to turn a skip into a hard
//! failure. The host-manipulating fail-closed matrix (mis-owned helper, unloaded
//! profile) is proven by the sibling `tests/sandbox-nesting/` shell harness.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;

const HELPER: &str = "/usr/libexec/nub/nub-bwrap";

/// A printable skip reason when the host obviously cannot run the proof, or `None`
/// when the composition drive should be attempted. Mirrors `linux_nesting.rs`.
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

fn nesting_required() -> bool {
    matches!(
        std::env::var("NUB_SANDBOX_REQUIRE_NESTING").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

#[test]
fn three_level_two_sibling_composition_holds_every_ceiling() {
    if let Some(reason) = nesting_skip_reason() {
        assert!(
            !nesting_required(),
            "NUB_SANDBOX_REQUIRE_NESTING is set but the host is not ready: {reason}"
        );
        eprintln!("SKIP linux_nested_composition: {reason}");
        return;
    }

    let output = Command::new(env!("CARGO_BIN_EXE_nub-sandbox-nesting-harness"))
        .arg("drive")
        .output()
        .expect("run the nesting-composition harness");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Without the compile-time packaged digest the helper cannot be admitted; that is a
    // build-config skip, not a defect. The harness surfaces it as a fail-closed reason.
    if !output.status.success() && stderr.contains("built without a packaged Bubblewrap digest") {
        assert!(
            !nesting_required(),
            "NUB_SANDBOX_REQUIRE_NESTING is set but the harness lacks NUB_BWRAP_SHA256"
        );
        eprintln!("SKIP linux_nested_composition: the harness was built without NUB_BWRAP_SHA256");
        return;
    }

    assert!(
        output.status.success() && stdout.trim().ends_with("verified-nested-composition"),
        "nested composition drive failed\nstatus: {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status
    );
}

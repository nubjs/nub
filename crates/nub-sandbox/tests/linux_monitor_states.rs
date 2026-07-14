//! Real stock-Bubblewrap exercise for retained-monitor states 1-5.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;

#[test]
fn retained_monitor_bootstrap_stop_gate_and_exec_transition_are_real() {
    let Some(bwrap) = usable_bwrap() else {
        return;
    };
    let output = Command::new(env!("CARGO_BIN_EXE_nub-sandbox-monitor-harness"))
        .arg("exercise-states-1-5")
        .arg(bwrap)
        .output()
        .expect("run the purpose-built retained-monitor harness");
    assert!(
        output.status.success(),
        "states 1-5 harness failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "verified-monitor-states-1-5"
    );
}

#[test]
fn retained_monitor_runtime_signal_wait_and_cleanup_are_real() {
    let Some(bwrap) = usable_bwrap() else {
        return;
    };
    let output = Command::new(env!("CARGO_BIN_EXE_nub-sandbox-monitor-harness"))
        .arg("exercise-state-6")
        .arg(bwrap)
        .output()
        .expect("run the state-6 retained-monitor harness");
    assert!(
        output.status.success(),
        "state-6 harness failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "verified-monitor-state-6"
    );
}

#[test]
fn retained_monitor_session_completion_is_real() {
    let Some(bwrap) = usable_bwrap() else {
        return;
    };
    let output = Command::new(env!("CARGO_BIN_EXE_nub-sandbox-monitor-harness"))
        .arg("exercise-state-7")
        .arg(bwrap)
        .output()
        .expect("run the state-7 retained-monitor harness");
    assert!(
        output.status.success(),
        "state-7 harness failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "verified-monitor-state-7"
    );
}

#[test]
fn retained_monitor_parent_session_boundary_is_real() {
    let Some(bwrap) = usable_bwrap() else {
        return;
    };
    let output = Command::new(env!("CARGO_BIN_EXE_nub-sandbox-monitor-harness"))
        .arg("exercise-state-8")
        .arg(bwrap)
        .output()
        .expect("run the state-8 retained-monitor harness");
    assert!(
        output.status.success(),
        "state-8 harness failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "verified-monitor-state-8"
    );
}

#[test]
fn nested_worker_reentry_is_exact_and_compiles_only_inside_the_retained_view() {
    let Some(_bwrap) = usable_bwrap() else {
        return;
    };
    for mode in ["full", "restricted"] {
        let output = Command::new(env!("CARGO_BIN_EXE_nub-sandbox-monitor-harness"))
            .arg(format!("exercise-nested-worker-{mode}"))
            .output()
            .expect("run the retained-view nested worker harness");
        assert!(
            output.status.success(),
            "nested worker {mode} harness failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            format!("verified-nested-worker-{mode}")
        );
    }
}

fn usable_bwrap() -> Option<&'static str> {
    let bwrap = ["/usr/bin/bwrap", "/bin/bwrap"]
        .into_iter()
        .find(|candidate| Path::new(candidate).is_file());
    let Some(bwrap) = bwrap else {
        require_or_skip("stock Bubblewrap is unavailable");
        return None;
    };
    let available = Command::new(bwrap)
        .args([
            "--die-with-parent",
            "--unshare-user",
            "--ro-bind",
            "/",
            "/",
            "--",
            "/bin/true",
        ])
        .status()
        .is_ok_and(|status| status.success());
    if !available {
        require_or_skip("stock Bubblewrap cannot create the required user namespace");
        return None;
    }
    Some(bwrap)
}

fn require_or_skip(reason: &str) {
    let required = matches!(
        std::env::var("NUB_SANDBOX_REQUIRE_BWRAP").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    );
    assert!(!required, "NUB_SANDBOX_REQUIRE_BWRAP set but {reason}");
}

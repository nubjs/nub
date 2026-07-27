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

/// The harness takes the Bubblewrap to exercise as an argv, so this gate needs the
/// resolved path rather than a yes/no. It comes from the engine's own candidate
/// resolution — on an AppArmor-restricted host the dedicated helper is the only one
/// that works, and a probe that knew only the stock pair skipped the whole file there.
fn usable_bwrap() -> Option<&'static Path> {
    nub_sandbox::host_probe::usable_bwrap("linux_monitor_states")
}

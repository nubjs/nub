//! Orphan-regression for #463: SIGKILL on the Nub leader must not orphan the
//! `nub run` workload. The signal FORWARDER cannot cover SIGKILL (never
//! delivered to userspace), so this exercises the Linux kernel-side backstop —
//! `PR_SET_PDEATHSIG(SIGTERM)` armed in the script child's `pre_exec`
//! (`group_on_spawn`). Linux-only by nature; macOS has no pdeathsig.

#![cfg(target_os = "linux")]

use std::path::PathBuf;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

/// `nub run serve` (a plain `sleep`, which `sh -c` execs so the sleeper IS the
/// pdeathsig'd child) → SIGKILL the nub leader → the sleeper must die within a
/// grace window instead of running on orphaned.
#[test]
fn sigkill_on_nub_leader_terminates_the_script_child() {
    let dir = std::env::temp_dir().join(format!("nub-pdeathsig-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        // `$$` is the sh pid; `exec sleep` replaces sh in-place, so child.pid
        // names the surviving sleeper the pdeathsig is armed on.
        r#"{"name":"f","version":"1.0.0","scripts":{"serve":"echo $$ > child.pid && exec sleep 30"}}"#,
    )
    .unwrap();

    let mut nub = std::process::Command::new(nub_binary())
        .args(["run", "serve"])
        .current_dir(&dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn nub run");

    // Wait for the script to write its pid (bounded).
    let pid_file = dir.join("child.pid");
    let mut child_pid: Option<i32> = None;
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(&pid_file)
            && let Ok(pid) = s.trim().parse::<i32>()
        {
            child_pid = Some(pid);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let child_pid = child_pid.expect("script child never wrote its pid");
    assert!(
        unsafe { libc::kill(child_pid, 0) } == 0,
        "sleeper must be alive before the kill"
    );

    // The unforwardable path: SIGKILL the leader directly.
    unsafe { libc::kill(nub.id() as i32, libc::SIGKILL) };
    let _ = nub.wait();

    // The kernel delivers the pdeathsig on leader death; give it a grace window.
    let mut died = false;
    for _ in 0..50 {
        if unsafe { libc::kill(child_pid, 0) } != 0 {
            died = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if !died {
        // Never leak a 30s sleeper into the test environment on failure.
        unsafe { libc::kill(child_pid, libc::SIGKILL) };
    }
    assert!(
        died,
        "script child (pid {child_pid}) survived SIGKILL of the nub leader — pdeathsig backstop missing"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

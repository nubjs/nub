//! Orphan-regression for #480: SIGKILL of the nub process GROUP must not
//! orphan the workload on macOS. Linux covers leader death kernel-side with
//! `PR_SET_PDEATHSIG` (see `pdeathsig.rs`); macOS has no equivalent, so nub
//! plants a watcher process inside the workload's own process group
//! (`spawn_group_reaper`) that tears the group down on pipe EOF. These tests
//! exercise the three halves of that contract: the kill path, the no-leak
//! normal path, and the disarm (don't kill deliberate survivors) path.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

fn write_fixture(dir: &Path, script: &str) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        format!(r#"{{"name":"f","version":"1.0.0","scripts":{{"serve":"{script}"}}}}"#),
    )
    .unwrap();
}

/// Spawn `nub run serve` in the fixture as its OWN process-group leader (the
/// supervised topology from #480) and return the handle.
fn spawn_nub_run(dir: &Path) -> std::process::Child {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(nub_binary());
    cmd.args(["run", "serve"])
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: setpgid(0, 0) between fork and exec is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    cmd.spawn().expect("spawn nub run")
}

/// Poll `path` for a pid written by the script (bounded).
fn read_pid(path: &Path) -> i32 {
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(path)
            && let Ok(pid) = s.trim().parse::<i32>()
        {
            return pid;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("script never wrote its pid to {}", path.display());
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// The #480 repro: SIGKILL the whole nub GROUP (which no forwarder or
/// pdeathsig can see on macOS) → the watcher must tear the workload down
/// within a grace window instead of leaving it running under PID 1.
#[test]
fn sigkill_on_nub_group_terminates_the_script_child() {
    let dir = std::env::temp_dir().join(format!("nub-pdw-kill-{}", std::process::id()));
    // `$$` is the sh pid; `exec sleep` replaces sh in-place, so child.pid names
    // the surviving sleeper (the workload group's leader).
    write_fixture(&dir, "echo $$ > child.pid && exec sleep 30");
    let mut nub = spawn_nub_run(&dir);
    let child_pid = read_pid(&dir.join("child.pid"));
    assert!(alive(child_pid), "sleeper must be alive before the kill");

    // SIGKILL the entire nub process group — the unforwardable supervisor path.
    unsafe { libc::kill(-(nub.id() as i32), libc::SIGKILL) };
    let _ = nub.wait();

    let mut died = false;
    for _ in 0..50 {
        if !alive(child_pid) {
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
        "script child (pid {child_pid}) survived SIGKILL of the nub group — \
         group-reaper backstop missing"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Normal completion must not leak a watcher: the guard disarms and reaps it
/// before nub exits. The private environment selects the watcher, leaving only
/// its ordinary `<child-pid> <fd>` payload visible in `ps`.
#[test]
fn normal_exit_leaves_no_watcher_process() {
    let dir = std::env::temp_dir().join(format!("nub-pdw-clean-{}", std::process::id()));
    write_fixture(&dir, "echo $$ > child.pid");
    let mut nub = spawn_nub_run(&dir);
    let status = nub.wait().expect("wait nub");
    assert!(status.success(), "nub run must succeed");

    let child_pid = read_pid(&dir.join("child.pid"));
    // The child PID is unique to this fixture, so the private mode's visible
    // `<child-pid> <fd>` payload identifies only this watcher's command line.
    let ps = std::process::Command::new("ps")
        .args(["-ax", "-o", "command"])
        .output()
        .expect("ps");
    let needle = format!(" {child_pid} 3");
    assert!(
        !String::from_utf8_lossy(&ps.stdout).contains(&needle),
        "a watcher for exited child {child_pid} is still running"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The watcher is re-invoked through `current_exe()`, which carries whatever
/// NAME nub is running under — `node` for anything spawned through nub's own
/// PATH shim, not `nub`. Its private environment mode keeps the PID/read-fd
/// payload ordinary application arguments; the legacy hidden-token protocol
/// remains accepted for old callers. Both forms must be honored under EVERY
/// argv0 identity nub answers to. If the dispatcher coupled either form to
/// `nub` argv0, a shim-named re-invocation would run a watcher payload as a
/// workload and recursively spawn more watchers (regression from #504).
#[test]
fn pdeath_watch_protocols_are_honored_under_every_argv0() {
    // Alongside the binary, so the aliases can be HARDLINKS — same-filesystem,
    // no 78MB copy per name, and the same shape as nub's real PATH shims.
    let nub = nub_binary();
    let dir = nub
        .parent()
        .unwrap()
        .join(format!("nub-pdw-argv0-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // `getpgrp() + 1` can never equal the child's own group (children inherit
    // ours), so the watcher deterministically takes its membership self-check
    // exit. That keeps the assertion on "the protocol was RECOGNIZED" alone —
    // no pipe timing, and it never reaches the group kill.
    let foreign_pgid = (unsafe { libc::getpgrp() } + 1).to_string();
    for name in ["nub", "node", "nubx", "npm"] {
        let aliased = dir.join(name);
        std::fs::hard_link(&nub, &aliased).unwrap();
        for (protocol, env_mode, args) in [
            (
                "private environment mode",
                Some("pdeath-watch"),
                vec![foreign_pgid.as_str(), "3"],
            ),
            (
                "legacy hidden token",
                None,
                vec!["__pdeath-watch", foreign_pgid.as_str(), "3"],
            ),
        ] {
            let mut cmd = std::process::Command::new(&aliased);
            cmd.args(args);
            if let Some(env_mode) = env_mode {
                cmd.env("__NUB_COMPILED_LAUNCHER_MODE", env_mode);
            }
            let out = cmd.output().expect("spawn aliased nub");
            assert_eq!(
                out.status.code(),
                Some(0),
                "nub invoked as `{name}` did not honor {protocol} (stderr: {})",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                out.stdout.is_empty() && out.stderr.is_empty(),
                "nub invoked as `{name}` ran {protocol} as a workload instead of \
                 the watcher loop (stdout: {}, stderr: {})",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The disarm half of the contract: a background process the script
/// DELIBERATELY leaves behind survives nub's normal exit, exactly as it would
/// under plain sh/node — the watcher must stand down, not sweep the group.
#[test]
fn deliberate_background_survivor_outlives_normal_exit() {
    let dir = std::env::temp_dir().join(format!("nub-pdw-bg-{}", std::process::id()));
    // The backgrounded sleeper stays in the script's process group; sh writes
    // its pid and exits, then nub exits normally.
    write_fixture(&dir, "sleep 30 & echo $! > survivor.pid");
    let mut nub = spawn_nub_run(&dir);
    let status = nub.wait().expect("wait nub");
    assert!(status.success(), "nub run must succeed");

    let survivor = read_pid(&dir.join("survivor.pid"));
    // Give a mis-armed watcher's SIGTERM time to land before asserting.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let survived = alive(survivor);
    unsafe { libc::kill(survivor, libc::SIGKILL) }; // always reap the sleeper
    assert!(
        survived,
        "deliberate background survivor (pid {survivor}) was killed on nub's \
         normal exit — the watcher failed to disarm"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spawn `nub watch <file>` as its own process-group leader, mirroring
/// [`spawn_nub_run`]'s topology.
fn spawn_nub_watch(dir: &Path, file: &str) -> std::process::Child {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(nub_binary());
    cmd.args(["watch", file])
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: setpgid(0, 0) between fork and exec is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    cmd.spawn().expect("spawn nub watch")
}

/// The watch-path twin of [`sigkill_on_nub_group_terminates_the_script_child`],
/// and a regression for the leak it did not cover.
///
/// `run_watch` was the one long-lived spawn in nub that called `Command::status()`
/// directly, so its `node --watch` supervisor was never placed in nub's process
/// group and never covered by the #480 reaper. That matters more here than
/// anywhere else: `node --watch` is designed never to exit, so a leader death left
/// it — and the grandchild it supervises — running under PID 1 forever, each
/// holding an fsevents watch on a source tree. They accumulated monotonically;
/// 99 such orphans were found on one dev host, the oldest 17 hours old.
#[test]
fn sigkill_on_nub_group_terminates_the_watched_node() {
    let dir = std::env::temp_dir().join(format!("nub-pdw-watch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // The watched script records its own pid, then idles. `node --watch` keeps
    // supervising it after it exits, which is precisely the process that leaked.
    std::fs::write(
        dir.join("w.js"),
        "require('fs').writeFileSync('child.pid', String(process.pid));\nsetTimeout(() => {}, 30000);\n",
    )
    .unwrap();

    let mut nub = spawn_nub_watch(&dir, "w.js");
    let child_pid = read_pid(&dir.join("child.pid"));
    assert!(
        alive(child_pid),
        "watched child must be alive before the kill"
    );

    // SIGKILL the nub LEADER ONLY — deliberately not the group.
    //
    // This distinction is the whole test. Killing the GROUP proves nothing here: on
    // the unfixed code the watched node INHERITS nub's process group, so a group
    // signal reaches it directly and the test passes without any reaper at all
    // (verified — that version passed against unfixed code). Once fixed, node is its
    // own group leader via `group_on_spawn`, so a group signal cannot reach it and
    // only the reaper can.
    //
    // Killing the leader alone is also the shape of the real leak: an agent session
    // or test harness dies, nub goes with it, and `node --watch` — which never exits
    // on its own — reparents to launchd and runs forever.
    unsafe { libc::kill(nub.id() as i32, libc::SIGKILL) };
    let _ = nub.wait();

    let mut died = false;
    for _ in 0..80 {
        if !alive(child_pid) {
            died = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if !died {
        // Never leak a watcher into the test environment on failure.
        unsafe { libc::kill(child_pid, libc::SIGKILL) };
    }
    assert!(
        died,
        "watched node (pid {child_pid}) survived SIGKILL of the nub LEADER — \
         run_watch is bypassing the group reaper again, so a dying session leaks a \
         watcher that never exits on its own"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

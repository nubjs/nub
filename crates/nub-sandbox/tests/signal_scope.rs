//! Signals are scoped to the confined process tree, and this is the case that proves it.
//!
//! There is no PID namespace — isolating processes is outside what this mechanism claims (A2.3) —
//! so before signal brokering a confined command could `kill` anything sharing its uid: the user's
//! editor, a sibling agent, or the nub process that launched it. Nothing else covered it. The
//! seccomp ceiling governs sockets and the keyring, and Landlock is a filesystem LSM that never
//! sees a signal.
//!
//! What makes the scope decidable is the process-group freeze: `linux_lifetime::program(true)`
//! answers `setsid` and `setpgid` with EPERM, and a seccomp filter is inherited across fork and
//! exec, so every descendant is placed in the guardian's private group at launch and can never
//! leave. The tree IS that group.
//!
//! ⚠️ THE BROADCAST CASE DELIBERATELY USES SIGNAL 0. `kill(-1, SIGKILL)` from an unconfined child
//! would take down every process the uid owns, which is the test runner and whatever else is on
//! the box — so the one case whose whole point is that enforcement might be absent must not carry
//! a lethal signal. Signal 0 runs the same permission path and delivers nothing.
#![cfg(target_os = "linux")]

#[path = "common/tool_output.rs"]
mod tool_output;

use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

const VICTIM: &str = "NUB_SIGNAL_SCOPE_VICTIM";

/// `errno` after a failed `kill`, or `None` when it succeeded.
fn kill_errno(pid: i32, sig: i32) -> Option<i32> {
    if unsafe { libc::kill(pid, sig) } == 0 {
        return None;
    }
    Some(std::io::Error::last_os_error().raw_os_error().unwrap())
}

fn tgkill_errno(tgid: i32, tid: i32, sig: i32) -> Option<i32> {
    if unsafe { libc::syscall(libc::SYS_tgkill, tgid, tid, sig) } == 0 {
        return None;
    }
    Some(std::io::Error::last_os_error().raw_os_error().unwrap())
}

#[test]
fn signal_scope_child() {
    let Ok(victim) = std::env::var(VICTIM) else {
        return;
    };
    let victim: i32 = victim.parse().expect("victim pid");

    // A descendant of our own, forked rather than exec'd so the case needs no grant for an
    // interpreter. It inherits the process group and the filter, which is exactly the thing under
    // test. `_exit` because a forked copy of a test binary must not run the harness's teardown.
    let mine = unsafe { libc::fork() };
    if mine == 0 {
        unsafe {
            libc::pause();
            libc::_exit(0)
        };
    }
    assert!(mine > 0, "fork a descendant to signal");

    // POSITIVE CONTROLS. Without these the refusals below would pass just as well if the broker
    // refused every signal outright, which is the failure mode that makes such a test worthless.
    assert_eq!(kill_errno(mine, 0), None, "a descendant must be signalable");
    assert_eq!(
        kill_errno(0, 0),
        None,
        "the command's own group must be signalable"
    );
    assert_eq!(
        kill_errno(unsafe { libc::getpid() }, 0),
        None,
        "a command must be able to signal itself",
    );

    // THE REFUSALS.
    assert_eq!(
        kill_errno(victim, libc::SIGKILL),
        Some(libc::EPERM),
        "a process outside the tree must not be signalable",
    );
    assert_eq!(
        tgkill_errno(victim, victim, libc::SIGKILL),
        Some(libc::EPERM),
        "tgkill must be scoped too — one spelling guarded is not the syscall guarded",
    );
    assert_eq!(
        kill_errno(-1, 0),
        Some(libc::EPERM),
        "the broadcast form must be refused; nothing narrows it to the tree",
    );
    assert_eq!(
        kill_errno(-victim, 0),
        Some(libc::EPERM),
        "another process group must not be signalable either",
    );

    // A REAL signal to a real descendant still lands — scoping is not a blanket refusal.
    assert_eq!(
        kill_errno(mine, libc::SIGKILL),
        None,
        "an in-tree kill must work"
    );
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(mine, &mut status, 0) },
        mine,
        "the descendant must actually have died",
    );
}

#[test]
fn a_confined_command_cannot_signal_outside_its_own_process_tree() {
    let root = tempfile::tempdir().expect("fixture root");
    let project = root.path().join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("index.js"), "ordinary-source").unwrap();

    // The out-of-tree process is a plain `sleep` this test owns, so the case is self-contained.
    let mut victim = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("a sibling process to aim at");
    let victim_pid = victim.id();

    let mut policy = compile(
        &json!({"fs": {(project.to_string_lossy()): "rw"}, "net": false}),
        &ctx(root.path()),
    )
    .expect("a project grant compiles");
    policy
        .env
        .constructed
        .insert(VICTIM.into(), victim_pid.to_string());

    let sandbox = Sandbox::new(&policy).expect("the supervised backend acquires");
    let prepared = sandbox
        .prepare(
            CommandSpec::new(std::env::current_exe().unwrap())
                .args(["--exact", "signal_scope_child", "--nocapture"])
                .cwd(&project)
                .redact_stdout(true)
                .redact_stderr(true),
        )
        .expect("the child prepares");
    assert!(prepared.degradation.is_full(), "{:?}", prepared.degradation);
    let output = tool_output::output(prepared);

    // THE DECISIVE ASSERTION, and it is behavioural rather than an errno: the process the child
    // aimed SIGKILL at is still there. An errno can be produced by a dozen accidents; a live
    // victim can only mean the signal never reached it.
    //
    // It is asserted BEFORE the child's exit status deliberately. A falsification run trips both,
    // and whichever fires first is what the reader sees — "the out-of-tree process was killed" is
    // the finding, where a bare non-zero child exit is only its symptom. A victim that survived
    // leaves the status assertion below to report the child's own stderr, which is the useful
    // diagnostic for every other way this test can fail.
    let alive = victim.try_wait().expect("check the victim").is_none();
    let _ = victim.kill();
    let _ = victim.wait();
    assert!(
        alive,
        "the out-of-tree process was killed by the confined command"
    );
    assert!(
        output.status.success(),
        "signal scoping failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn ctx(root: &Path) -> CompileCtx {
    let project = root.join("project");
    CompileCtx::new(
        Homes {
            home: root.join("home"),
            cache: root.join("cache"),
            tmp: root.join("tmp"),
            project: project.clone(),
        },
        project,
        ScopeCapabilities::approved(),
        BTreeMap::new(),
    )
}

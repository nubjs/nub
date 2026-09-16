//! Launching this sandbox inside another one fails CLOSED, and says which limit it hit.
//!
//! THE KERNEL ALLOWS ONE NOTIFYING FILTER PER TASK. A second `seccomp(…NEW_LISTENER)` returns
//! `EBUSY`, so a confined command cannot start inside a process that already installed one — an
//! agent platform's own sandbox, most realistically. Measured in `sandbox-netprobes/nest.c`; this
//! is the port of that finding into something that stays true.
//!
//! WHAT IS ACTUALLY AT RISK HERE IS THE DIAGNOSTIC, NOT THE SAFETY. The failing path was always
//! fail-closed: the forked child `_exit`s before `execve`, so nothing runs. But it communicates
//! only through an exit code — it is post-fork and async-signal-safe, so it cannot format a
//! message — and until the parent learned to reap it and translate, the caller saw
//! `failed to fill whole buffer`, which names none of the twelve ways that run can end.
//!
//! The filter installed below traps a syscall NOBODY CALLS, so it changes nothing about how this
//! process runs. Its only job is to occupy the one notifying-filter slot. Seccomp filters are
//! inherited across fork, so the sandbox's own child inherits it and its `install_notifier` is
//! the call that gets `EBUSY`.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::path::Path;

use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
use serde_json::json;

const CASE: &str = "NUB_NESTED_SANDBOX_CHILD";

/// Occupy this task's single notifying-filter slot. Returns the listener fd, which is
/// deliberately leaked: closing it would free the slot and undo the whole point.
fn install_outer_notifier() -> i32 {
    // `vhangup` is the target precisely because nothing in this process will ever call it, so the
    // filter is inert as policy and meaningful only as an occupant of the slot.
    const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    let target = libc::SYS_vhangup as u32;
    let prog: [libc::sock_filter; 4] = [
        // A = seccomp_data.nr, which sits at offset 0.
        libc::sock_filter {
            code: 0x20,
            jt: 0,
            jf: 0,
            k: 0,
        },
        libc::sock_filter {
            code: 0x15,
            jt: 0,
            jf: 1,
            k: target,
        },
        libc::sock_filter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_USER_NOTIF,
        },
        libc::sock_filter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ALLOW,
        },
    ];
    let fprog = libc::sock_fprog {
        len: prog.len() as u16,
        filter: prog.as_ptr() as *mut libc::sock_filter,
    };
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
        0,
        "NO_NEW_PRIVS is what lets an unprivileged task install a filter at all",
    );
    let fd = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            1,      // SECCOMP_SET_MODE_FILTER
            1 << 3, // SECCOMP_FILTER_FLAG_NEW_LISTENER
            &fprog as *const libc::sock_fprog,
        )
    };
    assert!(
        fd >= 0,
        "the OUTER filter must install, or this test proves nothing: {}",
        std::io::Error::last_os_error(),
    );
    fd as i32
}

/// The half that runs with a notifying filter already installed on itself.
#[test]
fn nested_sandbox_child() {
    if std::env::var(CASE).is_err() {
        return;
    }
    let root = tempfile::tempdir().expect("fixture root");
    let project = root.path().join("project");
    std::fs::create_dir_all(&project).unwrap();

    // POSITIVE CONTROL, and it has to come FIRST: without a filter installed, this exact launch
    // must SUCCEED. Otherwise the refusal below could be any ordinary launch failure — a missing
    // binary, an unusable fixture — and the test would report the nesting limit for something
    // that has nothing to do with it.
    let policy = compile(
        &json!({"fs": {(project.to_string_lossy()): "rw"}, "net": false}),
        &ctx(root.path()),
    )
    .expect("the policy compiles");
    let before = Sandbox::new(&policy)
        .expect("the backend acquires")
        .prepare(CommandSpec::new(Path::new("/bin/true")).cwd(&project))
        .expect("prepares")
        .status()
        .expect("an UNNESTED launch must succeed, or the refusal below means nothing");
    assert!(before.success(), "control launch failed: {before:?}");

    let _outer = install_outer_notifier();

    // Same policy, same command, one thing changed.
    let err = Sandbox::new(&policy)
        .expect("acquisition does not need the listener")
        .prepare(CommandSpec::new(Path::new("/bin/true")).cwd(&project))
        .expect("preparation does not need it either")
        .status()
        .expect_err("a launch nested inside another notifying filter must FAIL, never run plain");

    let text = err.to_string();
    assert!(
        text.contains("another sandbox is already active"),
        "the failure must name the limit it hit, not report a short read; got: {text}",
    );
}

/// Drives the child in its own process, because installing a seccomp filter is IRREVERSIBLE and
/// inherited — doing it in the test runner would poison every test sharing that process.
#[test]
fn a_sandbox_nested_in_another_notifier_refuses_and_says_why() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "nested_sandbox_child", "--nocapture"])
        .env(CASE, "1")
        .output()
        .expect("the child test runs");
    assert!(
        out.status.success(),
        "nested-launch refusal failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
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

//! `/proc` is deny-by-default, and this is the case that proves it rather than assuming it.
//!
//! There is no whole-`/proc` grant anywhere: Landlock grants exactly `PROC_READ_PATHS`, a bounded
//! list of GLOBAL files, and per-process files come only from the opt-in `fs.self_proc` grammar.
//! `/proc/<other-pid>/environ` is therefore denied by NOT BEING GRANTED — which is Landlock's
//! only way to deny anything.
//!
//! It is worth a real child rather than an assertion about the constant, because the kernel's own
//! default does NOT close this: Yama `ptrace_scope=1` (the Ubuntu default) restricts ptrace
//! ATTACH, not `PTRACE_MODE_READ_FSCREDS`, so an ordinary same-uid process CAN read an unrelated
//! process's environment — measured. Landlock is the only thing standing in the way, and a grant
//! that quietly widened to `/proc` would reopen it with every test still green.
#![cfg(target_os = "linux")]

#[path = "common/tool_output.rs"]
mod tool_output;

use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

const TARGET: &str = "NUB_PROC_ISOLATION_TARGET";
const PROJECT: &str = "NUB_PROC_ISOLATION_PROJECT";

#[test]
fn proc_isolation_child() {
    let Ok(target) = std::env::var(TARGET) else {
        return;
    };
    let project = std::env::var(PROJECT).expect("project root");

    // POSITIVE CONTROL, and it is what makes the refusal below mean something: procfs itself is
    // reachable, and so is the granted tree. A child that simply could not read anything would
    // pass the refusal for the wrong reason.
    assert!(
        fs::read_to_string("/proc/cpuinfo").is_ok(),
        "the global procfs read closure must stay reachable",
    );
    assert_eq!(
        fs::read_to_string(Path::new(&project).join("index.js")).expect("granted tree"),
        "ordinary-source",
    );

    let environ = format!("/proc/{target}/environ");
    let read = fs::read(&environ);
    assert!(
        read.is_err(),
        "{environ} was readable — a confined command reached an unrelated process's environment",
    );
    let cmdline = format!("/proc/{target}/cmdline");
    assert!(
        fs::read(&cmdline).is_err(),
        "{cmdline} was readable — per-process procfs is opt-in through fs.self_proc only",
    );
}

/// The unrelated process is a plain `sleep` this test owns, so the case is self-contained and
/// does not depend on anything else running on the host.
#[test]
fn a_confined_command_cannot_read_an_unrelated_processes_environment() {
    let root = tempfile::tempdir().expect("fixture root");
    let project = root.path().join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("index.js"), "ordinary-source").unwrap();

    let mut target = std::process::Command::new("sleep")
        .arg("30")
        .env("NUB_PROC_ISOLATION_SECRET", "must-not-be-readable")
        .spawn()
        .expect("a sibling process to read from");
    let target_pid = target.id();

    let mut policy = compile(
        &json!({"fs": {(project.to_string_lossy()): "rw"}, "net": false}),
        &ctx(root.path()),
    )
    .expect("a project grant compiles");
    policy
        .env
        .constructed
        .insert(TARGET.into(), target_pid.to_string());
    policy
        .env
        .constructed
        .insert(PROJECT.into(), project.to_string_lossy().into_owned());

    let sandbox = Sandbox::new(&policy).expect("the supervised backend acquires");
    let prepared = sandbox
        .prepare(
            CommandSpec::new(std::env::current_exe().unwrap())
                .args(["--exact", "proc_isolation_child", "--nocapture"])
                .cwd(&project)
                .redact_stdout(true)
                .redact_stderr(true),
        )
        .expect("the child prepares");
    assert!(prepared.degradation.is_full(), "{:?}", prepared.degradation);
    let output = tool_output::output(prepared);
    let _ = target.kill();
    let _ = target.wait();
    assert!(
        output.status.success(),
        "proc isolation failed:\nstdout:\n{}\nstderr:\n{}",
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

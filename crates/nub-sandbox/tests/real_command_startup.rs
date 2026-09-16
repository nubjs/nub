//! A real interpreter starts, runs and exits under the shipped policy — with the secret floor in
//! place, so the read broker is armed and EVERY `openat` the process makes is a supervisor round
//! trip.
//!
//! This is the compatibility claim the whole read-deny design rests on. `node` opens hundreds of
//! files before it runs a line of script: its own binary, the dynamic loader, several shared
//! objects, ICU data, `/proc` knobs. If brokering reads broke any of that, deny-inside-allow
//! would be unshippable no matter how correct it is — and nothing else in the suite would say so,
//! because every other confined child is a Rust test binary that opens almost nothing.
#![cfg(target_os = "linux")]

#[path = "common/tool_output.rs"]
mod tool_output;

use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The absolute path of a program on `PATH`, resolved OUTSIDE the sandbox so the test does not
/// also depend on the policy granting a `PATH` lookup.
fn locate(program: &str) -> PathBuf {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {program}"))
        .output()
        .expect("resolving a program on PATH");
    assert!(
        out.status.success(),
        "`{program}` must be on PATH — CI runners and the builder image both ship it, so an \
         absence here is an environment fault, not a reason to skip the case",
    );
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
}

#[test]
fn node_starts_and_runs_a_script_with_every_read_brokered() {
    let node = locate("node");
    let root = tempfile::tempdir().expect("fixture root");
    let project = root.path().join("project");
    std::fs::create_dir_all(&project).unwrap();

    let policy = compile(
        &json!({
            "fs": {(project.to_string_lossy()): "rw", "$tmp": "rw"},
            "net": false,
        }),
        &ctx(root.path()),
    )
    .expect("the fixture policy compiles");
    // The precondition this whole file exists to exercise: a deny in the ruleset is what arms
    // the read broker, and the secret floor is what puts one in every read-granting policy.
    assert!(
        policy
            .fs
            .rules
            .entries
            .iter()
            .any(|rule| rule.effect == nub_sandbox::policy::Effect::Deny),
        "without a deny the read broker never arms and this test measures nothing",
    );

    let sandbox = Sandbox::new(&policy).expect("the supervised backend acquires");
    let prepared = sandbox
        .prepare(
            // `redact_*` is what asks for piped streams, which is how the shared harness
            // collects output at all — without them `take_stdout()` returns None.
            CommandSpec::new(&node)
                .args(["-e", "process.stdout.write('node-ok')"])
                .cwd(&project)
                .redact_stdout(true)
                .redact_stderr(true),
        )
        .expect("the child prepares");
    assert!(prepared.degradation.is_full(), "{:?}", prepared.degradation);
    let output = tool_output::output(prepared);
    assert!(
        output.status.success(),
        "node failed to start under a read-brokered policy:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "node-ok",
        "node ran but produced the wrong output",
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

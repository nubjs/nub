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

/// What read-brokering COSTS per open, measured where it is actually paid.
///
/// Ignored by default because it is a measurement, not a contract: it has no failing condition
/// beyond the run itself, and its number is only meaningful against the same run with
/// `NUB_SANDBOX_READ_BROKER=off`. Drive both arms on one host, back to back:
///
/// ```text
/// cargo test -p nub-sandbox --test real_command_startup -- --ignored --nocapture
/// NUB_SANDBOX_READ_BROKER=off cargo test … -- --ignored --nocapture
/// ```
///
/// A plain `node -e` start is NOT the workload to read this off — it makes about 18 `openat`
/// calls, so the round trips disappear into process startup. This one opens the same granted
/// file `NUB_OPEN_LOOPS` times (default 20000), which is what a real dependency graph does.
#[test]
#[ignore = "measurement, not a contract: meaningful only as an A/B against NUB_SANDBOX_READ_BROKER=off"]
fn node_open_throughput_under_the_broker() {
    let node = locate("node");
    let loops: u32 = std::env::var("NUB_OPEN_LOOPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);
    let root = tempfile::tempdir().expect("fixture root");
    let project = root.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("index.js"), "ordinary-source").unwrap();

    let policy = compile(
        &json!({
            "fs": {(project.to_string_lossy()): "rw", "$tmp": "rw"},
            "net": false,
        }),
        &ctx(root.path()),
    )
    .expect("the fixture policy compiles");
    let sandbox = Sandbox::new(&policy).expect("the supervised backend acquires");
    let script = format!(
        "const fs=require('fs');for(let i=0;i<{loops};i++)fs.readFileSync('index.js');         process.stdout.write('done')"
    );
    let prepared = sandbox
        .prepare(
            CommandSpec::new(&node)
                .args(["-e", &script])
                .cwd(&project)
                .redact_stdout(true)
                .redact_stderr(true),
        )
        .expect("the child prepares");
    let started = std::time::Instant::now();
    let output = tool_output::output(prepared);
    let elapsed = started.elapsed();
    assert!(
        output.status.success(),
        "throughput child failed:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    let broker = if std::env::var("NUB_SANDBOX_READ_BROKER").as_deref() == Ok("off") {
        "off"
    } else {
        "on"
    };
    println!(
        "OPEN_THROUGHPUT broker={broker} loops={loops} elapsed_ms={} per_open_us={:.2}",
        elapsed.as_millis(),
        elapsed.as_secs_f64() * 1e6 / f64::from(loops),
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

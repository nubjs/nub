//! Windows: a lifecycle script's outcome is the LAST member of its job to exit.
//!
//! WHAT THIS PINS, and why nothing else does. `drain_job_and_status` exists because the
//! direct child exiting is not the script finishing: a shell that hands off to a trailing
//! `node-gyp rebuild` can exit 0 while the real work is still running, and without the
//! drain that build is killed mid-flight and reported successful. So the drain consults
//! what the shell handed off to — but ONLY when the shell itself reported success
//! (`if code == 0 { handed_off } else { code }`), because a shell that reports its own
//! failure is authoritative.
//!
//! THE ORDERING IS THE WHOLE RULE, AND IT WAS BEING GUESSED. The drain polls every 50ms
//! while a process tree tears down in far less, so several members land in ONE poll window
//! and were then "ordered" by the job's pid-list order — an arbitrary tiebreak the rule
//! read as fact. Measured cost: `puppeteer@25.8.0` produced OPPOSITE verdicts on two runs
//! whose postinstall completed identically, and `nx`, `@mui/x-telemetry` and `lefthook`
//! were all reported failed at 128 while their scripts exited 0, because a git helper they
//! deliberately tolerate (each catches the failure) happened to be observed last. Ordering
//! now comes from `GetProcessTimes`'s `ExitTime` at 100ns resolution instead.
//!
//! WHY BOTH DIRECTIONS ARE HERE. Fixing the false FAILURE is only half of it — the drain's
//! reason for existing is catching a false SUCCESS, and a change that quietly stopped
//! catching it would look like a clean win on every package that had been over-reported.
//! So the load-bearing case is `a_trailing_failure_still_decides_the_outcome`: the direct
//! child exits 0 and a handed-off process exits NON-ZERO after it. If that ever goes green
//! by reporting 0, the drain has stopped doing its job.

#![cfg(windows)]

use nub_sandbox::{CommandSpec, apply};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn homes(home: &Path, project: &Path) -> nub_sandbox::Homes {
    nub_sandbox::Homes {
        home: home.to_path_buf(),
        tmp: std::env::temp_dir(),
        cache: home.join("AppData").join("Local"),
        project: project.to_path_buf(),
    }
}

/// Absolute path to `node.exe`, or `None` when the runner has no Node on PATH (the test
/// then skips rather than failing for an unrelated reason).
fn node_exe() -> Option<PathBuf> {
    let out = std::process::Command::new("where")
        .arg("node")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let first = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();
    let p = PathBuf::from(first);
    p.is_file().then_some(p)
}

/// A script that exits 0 immediately after handing off to a child exiting `code` later.
///
/// `stdio: "inherit"`, never `"ignore"`: a LowBox token is refused `\Device\Null`, so
/// `"ignore"` fails EPERM inside `uv_spawn` and the case would measure that instead of
/// the ordering.
fn handoff(code: i32) -> String {
    format!(
        "const cp=require('child_process');\
         cp.spawn(process.execPath,['-e','setTimeout(()=>process.exit({code}),1500)'],\
         {{detached:true,stdio:'inherit'}}).unref();\
         process.exit(0);"
    )
}

/// Run `script` under the REAL build jail and return the status nub reports for it.
fn jailed_status(node: &Path, script: &str) -> i32 {
    let root = tempfile::tempdir().expect("tempdir");
    let project = root.path().join("project");
    let package_dir = project.join("node_modules").join("drainpkg");
    std::fs::create_dir_all(&package_dir).unwrap();

    // The REAL ambient environment and the REAL home, as the other Windows backend tests
    // do. A hand-rolled two-entry env fails the launch outright with ERROR_ENVVAR_NOT_FOUND
    // (203) before any child exists, which reads as a drain failure and is not one.
    let ambient: BTreeMap<String, String> = std::env::vars().collect();
    let home = std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root.path().to_path_buf());

    let policy = nub_sandbox::compile_build_jail(
        homes(&home, &project),
        &package_dir,
        Some("drainpkg"),
        Some("1.0.0"),
        vec![node.to_path_buf()],
        Vec::new(),
        ambient,
    )
    .expect("compile build-jail");

    let spec = CommandSpec::new(node.as_os_str())
        .args(["-e", script])
        .cwd(&package_dir);
    apply(&policy, spec)
        .expect("apply")
        .status()
        .expect("spawn")
        .code()
        .unwrap_or(-1)
}

#[test]
fn a_trailing_failure_still_decides_the_outcome() {
    let Some(node) = node_exe() else { return };
    // THE CASE THE DRAIN EXISTS FOR. The shell says success and the work it handed off
    // fails afterwards, so the handed-off status must win. A 0 here means the drain has
    // stopped catching false successes — exactly what a too-eager ordering fix would do.
    assert_eq!(
        jailed_status(&node, &handoff(7)),
        7,
        "a handed-off process that fails LAST must decide the outcome"
    );
}

#[test]
fn a_trailing_success_leaves_the_outcome_alone() {
    let Some(node) = node_exe() else { return };
    // The positive control that makes the assertion above meaningful: same shape, same
    // timing, only the handed-off exit code differs.
    assert_eq!(
        jailed_status(&node, &handoff(0)),
        0,
        "a handed-off process that succeeds must not fabricate a failure"
    );
}

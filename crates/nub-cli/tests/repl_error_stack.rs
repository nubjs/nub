//! The nub REPL keeps the stack trace of a failed `require()`.
//!
//! `node:repl` truncates an uncaught error's trace at the LAST null-named frame —
//! normally its own `REPL1:1` eval frame, which is exactly the boundary between user
//! code and REPL machinery. nub's CJS augmentation adds frames to every resolve
//! (a `registerHooks` resolve hook plus the `Module._resolveFilename` wrapper), and
//! with `Error.stackTraceLimit` at its default 10 those extra frames pushed the REPL's
//! own frame off the end of the captured array. The only null-named frame left was the
//! Node internal nub delegates to through `.call()`, so the REPL cut the ENTIRE trace
//! and `require("./missing")` printed as a bare `[Error: Cannot find module …]`.
//!
//! The frame text is deliberately not pinned: line numbers inside `node:internal/…`
//! move between Node versions. What is asserted is the shape node produces — a trace
//! with at least one `at ` frame — with plain node run on the same input as the
//! positive control, so the test cannot pass by both sides printing nothing.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/ or release/
    path.push("nub");
    path
}

/// An empty directory, so `require("./bar")` is guaranteed to miss.
fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nub-repl-stack-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Drive `<program> -i` with a failing `require`, returning stdout+stderr combined
/// (the REPL writes the error to stdout; nub's provisioning banner goes to stderr).
fn repl_require_missing(program: &Path, cwd: &Path) -> String {
    let mut child = Command::new(program)
        .arg("-i")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn {}: {e}", program.display()));
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(b"require(\"./bar\")\n.exit\n")
        .expect("write REPL input");
    let out = child.wait_with_output().expect("wait for REPL");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn frame_count(output: &str) -> usize {
    output
        .lines()
        .filter(|l| l.trim_start().starts_with("at "))
        .count()
}

#[test]
fn repl_prints_frames_for_a_failed_require() {
    let cwd = scratch_dir();

    let node = repl_require_missing(Path::new("node"), &cwd);
    assert!(
        node.contains("Cannot find module"),
        "control: plain node must report the missing module.\n--- node output ---\n{node}"
    );
    assert!(
        frame_count(&node) > 0,
        "control: plain node must print stack frames, otherwise this test proves nothing.\n\
         --- node output ---\n{node}"
    );

    let nub = repl_require_missing(&nub_binary(), &cwd);
    assert!(
        nub.contains("Cannot find module"),
        "nub must report the missing module.\n--- nub output ---\n{nub}"
    );
    assert!(
        frame_count(&nub) > 0,
        "nub dropped every stack frame; node printed {}.\n--- nub output ---\n{nub}",
        frame_count(&node)
    );
}

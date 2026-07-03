//! The `--posix-shell` run flag (renamed from `--shell-emulator`) and the hard
//! error the removed spelling produces. Two contracts:
//!
//!   - `--posix-shell` routes the script body through a POSIX `sh`, so an inline
//!     env-assignment (`FOO=… cmd`) that Windows' `cmd` default can't parse runs.
//!     On Unix `sh` is always present, so this exercises flag acceptance + the
//!     found-`sh` path; the Windows Git-for-Windows detection rides the
//!     `tests/windows` harness (Docker on the dev box is Linux-only).
//!   - the removed `--shell-emulator` spelling is NOT a silent alias — the name
//!     was a misnomer (nub's flag forces a system `sh`; it is not pnpm's built-in
//!     JavaScript shell), so it hard-errors with guidance toward `--posix-shell`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

fn tmp_pkg(script: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-posix-shell-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        format!(r#"{{"name":"fix","scripts":{{"greet":"{script}"}}}}"#),
    )
    .unwrap();
    dir
}

fn run_nub(dir: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

#[cfg(unix)]
#[test]
fn posix_shell_runs_inline_env_assignment_script() {
    // `FOO=hello printenv FOO` is a POSIX-ism cmd.exe rejects; through a found
    // `sh` it prints `hello`. On Unix the flag is a no-op over the default, so
    // this pins that `--posix-shell` is accepted and routes through `sh`.
    let dir = tmp_pkg("FOO=hello printenv FOO");
    let (stdout, stderr, code) = run_nub(&dir, &["run", "--posix-shell", "greet"]);
    assert_eq!(code, 0, "exit {code}; stderr: {stderr}");
    assert!(
        stdout.contains("hello"),
        "expected script output `hello`, got stdout: {stdout:?}"
    );
}

#[test]
fn shell_emulator_old_spelling_hard_errors_with_guidance() {
    // The removed spelling must guide toward the new one, not silently alias:
    // the mechanisms differ (system `sh` vs. pnpm's JS shell), so aliasing would
    // carry the false-friend semantics forward.
    let dir = tmp_pkg("echo hi");
    let (_stdout, stderr, code) = run_nub(&dir, &["run", "--shell-emulator", "greet"]);
    assert_ne!(code, 0, "old spelling must fail, got exit 0");
    assert!(
        stderr.contains("--shell-emulator was renamed to --posix-shell"),
        "expected rename guidance, got stderr: {stderr:?}"
    );
}

//! `nub run` / `nub exec` behaviors driven through the binary against throwaway
//! fixtures: a workspace bin that is not a Node script runs in each member's
//! directory, a concurrent multi-script run reports a script that could not
//! start, and the global `--cwd` takes its `=` spelling before the verb.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

fn tmp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-run-exec-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn run_nub(dir: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env("XDG_DATA_HOME", tmp_dir("xdg-data"))
        .env("XDG_CACHE_HOME", tmp_dir("xdg-cache"))
        .env_remove("NODE_COMPAT")
        .env_remove("npm_lifecycle_event")
        // The status-line assertions match the uncolored prefix.
        .env_remove("FORCE_COLOR")
        .output()
        .expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

/// The `.bin` entries Nub's own installs write are `#!/bin/sh` shims, so they take
/// the non-Node launch path. Under `exec -r` that path must start the bin in the
/// member's directory, as the Node path and pnpm both do — not in the directory
/// the command was typed in.
#[cfg(unix)]
#[test]
fn exec_recursive_shell_bin_runs_in_each_members_directory() {
    use std::os::unix::fs::PermissionsExt;
    let root = tmp_dir("exec-sh-cwd");
    write(
        &root.join("package.json"),
        r#"{"name":"root","private":true,"workspaces":["packages/*"]}"#,
    );
    for member in ["a", "b"] {
        write(
            &root.join("packages").join(member).join("package.json"),
            &format!(r#"{{"name":"@org/{member}"}}"#),
        );
    }
    // Hoisted to the root `.bin`, so both members resolve the same file.
    let bin = root.join("node_modules/.bin/where");
    write(&bin, "#!/bin/sh\necho \"cwd:$(pwd -P)\"\n");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let (stdout, stderr, code) = run_nub(&root, &["exec", "-r", "where"]);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(code, 0, "stderr: {stderr}\nstdout: {stdout}");
    for member in ["a", "b"] {
        let suffix = format!("/packages/{member}");
        assert!(
            stdout.lines().any(|line| line.ends_with(&suffix)),
            "member {member}'s bin must run in {suffix}: {stdout}"
        );
    }
}

/// A `/regexp/` selector matching several scripts runs them concurrently. A script
/// whose shell cannot even be started must say so, as the sequential path and the
/// workspace path do, rather than exit 1 with no message.
#[test]
fn concurrent_regex_run_reports_a_script_that_failed_to_start() {
    let root = tmp_dir("regex-spawn-error");
    write(
        &root.join("package.json"),
        r#"{"name":"rx","scripts":{"a1":"echo one","a2":"echo two"}}"#,
    );
    let missing_shell = root.join("no-such-shell");
    let missing_shell = missing_shell.to_str().unwrap();

    let (stdout, stderr, code) = run_nub(
        &root,
        &[
            "run",
            "--workspace-concurrency",
            "2",
            "--script-shell",
            missing_shell,
            "/^a/",
        ],
    );
    let _ = std::fs::remove_dir_all(&root);
    assert_ne!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    for script in ["a1", "a2"] {
        assert!(
            stderr.contains(&format!("{script}: error: ")),
            "the failure to start {script} must be reported: {stderr}"
        );
    }
}

/// `--cwd` is a long flag like any other, so `--cwd=<dir>` is the same request as
/// `--cwd <dir>`, before the verb as well as after it — not a token handed to
/// Node as an unknown option.
#[test]
fn cwd_equals_spelling_before_the_verb_changes_directory() {
    let root = tmp_dir("cwd-equals");
    write(
        &root.join("app/package.json"),
        r#"{"name":"app","scripts":{"hello":"echo hello-from-app"}}"#,
    );

    for args in [
        &["--cwd=app", "run", "hello"][..],
        &["--cwd", "app", "run", "hello"][..],
    ] {
        let (stdout, stderr, code) = run_nub(&root, args);
        assert_eq!(code, 0, "{args:?}\nstdout: {stdout}\nstderr: {stderr}");
        assert!(
            stdout.contains("hello-from-app"),
            "{args:?} must run the script in app/: {stdout}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

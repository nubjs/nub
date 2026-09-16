//! `nub sandbox <command>` — the confinement frontend's contract.
//!
//! The case that matters most here is the NEGATIVE one, and it is not about error text: an
//! invocation that cannot be confined must not run the command ANYWAY. A frontend that printed a
//! warning and executed would hand the user the confinement they asked for in name only, and the
//! failure would be silent — the command succeeds, so nothing looks wrong. Both refusal tests
//! therefore assert on a MARKER FILE the command would have created, not only on the exit code:
//! an exit code can be non-zero for unrelated reasons, where a missing marker proves the program
//! never ran.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/ or fast/
    path.push("nub");
    path
}

fn fixture(case: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nub-sandbox-cmd-{}-{}-{case}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A shell script that creates `marker` when it runs. Its EXISTENCE is the evidence.
fn marker_command(dir: &Path) -> (PathBuf, PathBuf) {
    let script = dir.join("touch-marker.sh");
    let marker = dir.join("marker");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho ran > '{}'\n", marker.display()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, marker)
}

#[test]
fn a_command_with_no_policy_is_refused_and_never_runs() {
    let dir = fixture("nopolicy");
    let (script, marker) = marker_command(&dir);

    let out = Command::new(nub_binary())
        .args(["sandbox", script.to_str().unwrap()])
        .current_dir(&dir)
        .output()
        .expect("nub runs");

    assert!(!out.status.success(), "a policy-less run must not succeed");
    assert!(
        !marker.exists(),
        "the command RAN despite having no policy — an unconfined run is the one outcome this \
         surface must never produce",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--policy"),
        "the refusal must name the flag that fixes it; got:\n{stderr}",
    );
}

#[test]
fn help_is_available_and_states_the_platform_limit() {
    let out = Command::new(nub_binary())
        .args(["sandbox", "--help"])
        .output()
        .expect("nub runs");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--policy"), "{stdout}");
    assert!(
        stdout.contains("Linux"),
        "help must say where the sandbox enforces; got:\n{stdout}",
    );
}

/// Off Linux the engine enforces nothing, so the frontend must refuse — and, again, not run.
///
/// This is the whole point of keeping the subcommand VISIBLE on macOS and Windows: the user gets a
/// sentence naming the platform instead of clap's "unrecognized subcommand".
#[cfg(not(target_os = "linux"))]
#[test]
fn an_unsupported_host_refuses_by_name_and_never_runs_the_command() {
    let dir = fixture("unsupported");
    let (script, marker) = marker_command(&dir);
    let policy = dir.join("policy.json");
    std::fs::write(&policy, r#"{"fs": {".": "rw"}, "net": false}"#).unwrap();

    let out = Command::new(nub_binary())
        .args([
            "sandbox",
            "--policy",
            policy.to_str().unwrap(),
            script.to_str().unwrap(),
        ])
        .current_dir(&dir)
        .output()
        .expect("nub runs");

    assert!(!out.status.success());
    assert!(
        !marker.exists(),
        "the command ran on a host that cannot confine it",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Linux"),
        "the refusal must name the platform that works; got:\n{stderr}",
    );
}

/// On Linux the command actually runs, and a flag AFTER it belongs to it rather than to nub.
#[cfg(target_os = "linux")]
#[test]
fn a_confined_command_runs_and_owns_the_flags_that_follow_it() {
    let dir = fixture("confined");
    let script = dir.join("argv.sh");
    std::fs::write(&script, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let policy = dir.join("policy.json");
    std::fs::write(
        &policy,
        format!(r#"{{"fs": {{"{}": "rw"}}, "net": false}}"#, dir.display()),
    )
    .unwrap();

    let out = Command::new(nub_binary())
        .args([
            "sandbox",
            "--policy",
            policy.to_str().unwrap(),
            script.to_str().unwrap(),
            "--help",
        ])
        .current_dir(&dir)
        .output()
        .expect("nub runs");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a confined command must run:\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
    // `--help` reached the SCRIPT. Had the frontend claimed it, nub's own help would have printed
    // and the script would never have run.
    assert_eq!(stdout.trim(), "--help", "stderr:\n{stderr}");
}

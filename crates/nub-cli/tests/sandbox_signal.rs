#![cfg(target_os = "linux")]

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn nub_bin() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    path.pop();
    path.push("nub");
    path
}

fn spawn_fixture(
    make_script: impl FnOnce(&Path) -> (String, PathBuf),
) -> (TempDir, PathBuf, Child) {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("package.json"), "{}").unwrap();
    let (script, ready) = make_script(&project);
    let policy = temp.path().join("policy.json");
    std::fs::write(&policy, r#"{"fs":["...","./"],"net":true,"vars":true}"#).unwrap();
    let child = Command::new(nub_bin())
        .args(["run", "--sandbox"])
        .arg(policy)
        .arg("--")
        .arg("/bin/sh")
        .args(["-c", &script])
        .current_dir(&project)
        .env("PATH", "/usr/bin:/bin")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(
            Instant::now() < deadline,
            "sandbox target did not become ready"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    (temp, project, child)
}

fn terminate(child: &mut Child) -> ExitStatus {
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    child.wait().unwrap()
}

#[test]
fn term_reaches_the_sandboxed_descendant_group() {
    let (_fixture, project, mut child) = spawn_fixture(|project| {
        let ready = project.join("ready");
        let handled = project.join("handled");
        let script = format!(
            "trap 'printf handled > {} ; exit 42' TERM; sleep 30 & printf ready > {}; wait",
            handled.display(),
            ready.display()
        );
        (script, ready)
    });
    let status = terminate(&mut child);
    assert_eq!(status.code(), Some(42));
    assert!(
        project.join("handled").exists(),
        "the target shell handled the forwarded TERM"
    );
}

#[test]
fn default_term_exit_maps_to_143() {
    let (_fixture, _project, mut child) = spawn_fixture(|project| {
        let ready = project.join("ready");
        let script = format!("printf ready > {}; exec sleep 30", ready.display());
        (script, ready)
    });
    let status = terminate(&mut child);
    assert_eq!(status.code(), Some(143), "raw status: {status:?}");
    assert_eq!(status.signal(), None, "Nub maps the child signal to a code");
}

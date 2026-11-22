//! `nubr --snapshot-blob <blob>` starts Node directly instead of going through
//! the JavaScript runner, but it must keep the runner's loader preload. A blob
//! with no deserialize main function runs a script piped on stdin against the
//! restored heap, and that script is ordinary user code: it gets TypeScript the
//! same as any other `nubr` run. The import below uses an `enum`, which no
//! Node's own type stripping accepts, so it only loads with the loader armed.

#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn nubr(dir: &Path) -> PathBuf {
    let link = dir.join("nubr");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_nub"), &link).unwrap();
    link
}

fn run(cmd: &mut Command, stdin: &str) -> Output {
    let mut child = cmd
        .env_remove("NODE_COMPAT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "exited {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

#[test]
fn stdin_script_under_a_snapshot_without_main_keeps_the_loader() {
    let dir = tempfile::tempdir().unwrap();
    let nubr = nubr(dir.path());

    // A blob only loads on the Node that built it, so build it with the Node
    // `nubr` resolves here.
    let node = run(
        Command::new(&nubr)
            .args(["-p", "process.execPath"])
            .current_dir(dir.path()),
        "",
    );
    let node = String::from_utf8(node.stdout).unwrap().trim().to_owned();
    std::fs::write(
        dir.path().join("entry.js"),
        "globalThis.fromSnapshot = 42;\n",
    )
    .unwrap();
    run(
        Command::new(&node)
            .args([
                "--snapshot-blob",
                "app.blob",
                "--build-snapshot",
                "entry.js",
            ])
            .current_dir(dir.path()),
        "",
    );

    std::fs::write(dir.path().join("mod.ts"), "export enum E { A = 5 }\n").unwrap();
    let script = "const { pathToFileURL } = require('node:url');\n\
                  import(pathToFileURL(process.cwd() + '/mod.ts').href)\n\
                  .then((m) => console.log(globalThis.fromSnapshot, m.E.A));\n";
    let out = run(
        Command::new(&nubr)
            .args(["--snapshot-blob", "app.blob"])
            .current_dir(dir.path()),
        script,
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "42 5");
}

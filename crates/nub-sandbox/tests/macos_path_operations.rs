#![cfg(target_os = "macos")]

//! Live Seatbelt probes for path operations that do not reduce to ordinary I/O.
//!
//! The policy below contains only positive grants.  It distinguishes the pathname
//! checked by each operation from an already-open object: all mutations are made by
//! the confined child through a fresh name lookup, while the host changes the
//! replaceable symlink only after `prepare` has materialized the profile.

#[path = "common/tool_output.rs"]
mod tool_output;

use nub_sandbox::policy::Effect;
use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

const CASE: &str = "NUB_MACOS_PATH_OPERATIONS_CASE";
const RENAME_SOURCE: &str = "NUB_MACOS_PATH_OPERATIONS_RENAME_SOURCE";
const RENAME_ALLOWED: &str = "NUB_MACOS_PATH_OPERATIONS_RENAME_ALLOWED";
const RENAME_DENIED: &str = "NUB_MACOS_PATH_OPERATIONS_RENAME_DENIED";
const HARD_SOURCE: &str = "NUB_MACOS_PATH_OPERATIONS_HARD_SOURCE";
const HARD_ALLOWED: &str = "NUB_MACOS_PATH_OPERATIONS_HARD_ALLOWED";
const HARD_DENIED: &str = "NUB_MACOS_PATH_OPERATIONS_HARD_DENIED";
const SYMLINK: &str = "NUB_MACOS_PATH_OPERATIONS_SYMLINK";
const SYMLINK_INITIAL: &str = "NUB_MACOS_PATH_OPERATIONS_SYMLINK_INITIAL";
const SYMLINK_REPLACEMENT: &str = "NUB_MACOS_PATH_OPERATIONS_SYMLINK_REPLACEMENT";
const EXEC_ALLOWED: &str = "NUB_MACOS_PATH_OPERATIONS_EXEC_ALLOWED";
const EXEC_DENIED: &str = "NUB_MACOS_PATH_OPERATIONS_EXEC_DENIED";
const NATIVE_EXEC_ALLOWED: &str = "NUB_MACOS_PATH_OPERATIONS_NATIVE_EXEC_ALLOWED";
const NATIVE_EXEC_DENIED: &str = "NUB_MACOS_PATH_OPERATIONS_NATIVE_EXEC_DENIED";

#[test]
fn macos_path_operations_child() {
    if std::env::var_os(CASE).is_none() {
        return;
    }
    let path = |name| PathBuf::from(std::env::var_os(name).expect("path-operation canary"));

    let rename_source = path(RENAME_SOURCE);
    let rename_allowed = path(RENAME_ALLOWED);
    let rename_denied = path(RENAME_DENIED);
    fs::rename(&rename_source, &rename_allowed).expect("rename within positive rw grant");
    assert!(
        fs::rename(&rename_allowed, &rename_denied).is_err(),
        "rename into an ungranted future destination succeeded"
    );
    assert!(
        rename_allowed.exists(),
        "failed cross-grant rename removed source name"
    );
    assert!(
        !rename_denied.exists(),
        "failed cross-grant rename created ungranted destination"
    );

    let hard_source = path(HARD_SOURCE);
    let hard_allowed = path(HARD_ALLOWED);
    let hard_denied = path(HARD_DENIED);
    fs::hard_link(&hard_source, &hard_allowed).expect("hard link within positive rw grant");
    assert_eq!(fs::read(&hard_allowed).unwrap(), b"hard-link-source");
    assert!(
        fs::hard_link(&hard_source, &hard_denied).is_err(),
        "hard link into an ungranted future destination succeeded"
    );
    assert!(
        !hard_denied.exists(),
        "failed hard-link operation created ungranted destination"
    );

    let symlink = path(SYMLINK);
    let initial = path(SYMLINK_INITIAL);
    let replacement = path(SYMLINK_REPLACEMENT);
    assert_eq!(fs::read(&initial).unwrap(), b"initial-granted-target");
    assert!(
        fs::read(&symlink).is_err(),
        "post-prepare symlink replacement exposed an ungranted target"
    );
    assert_eq!(
        fs::read(&replacement).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );

    let allowed = Command::new(path(EXEC_ALLOWED))
        .status()
        .expect("allowed future executable starts");
    assert!(
        allowed.success(),
        "allowed future executable failed: {allowed}"
    );
    // A script can reach its already-granted `/bin/sh` interpreter before the interpreter's
    // open of this ungranted pathname fails.  Treat either that non-zero shell status or a direct
    // `execve` refusal as the denied-execution outcome; requiring `Command` itself to return an
    // error would incorrectly model only native-binary execution.
    match Command::new(path(EXEC_DENIED)).status() {
        Ok(status) => assert!(
            !status.success(),
            "ungranted future executable completed successfully"
        ),
        Err(_) => {}
    }

    // Unlike the scripts above, these are copied Mach-O executables: a success proves direct
    // executable mapping/loading, not just that a permitted interpreter could read a script.
    let native_allowed = Command::new(path(NATIVE_EXEC_ALLOWED))
        .arg("native-allowed")
        .output()
        .expect("allowed future Mach-O executable starts");
    assert!(
        native_allowed.status.success(),
        "allowed future Mach-O executable failed: {}",
        native_allowed.status
    );
    assert_eq!(native_allowed.stdout, b"native-allowed\n");
    assert!(
        fs::read(path(NATIVE_EXEC_DENIED)).is_err(),
        "ungranted future Mach-O executable was readable before direct execution"
    );
    match Command::new(path(NATIVE_EXEC_DENIED))
        .arg("native-denied")
        .output()
    {
        Ok(output) => assert!(
            !output.status.success(),
            "ungranted future Mach-O executable completed successfully: {}",
            String::from_utf8_lossy(&output.stdout)
        ),
        Err(_) => {}
    }
}

#[test]
fn positive_grants_cover_path_operations_and_future_execs() {
    let fixture = tempfile::Builder::new()
        .prefix("nub-macos-path-operations-")
        .tempdir_in(std::env::var_os("HOME").expect("HOME"))
        .expect("fixture");
    let root = fixture.path();
    let allowed = root.join("allowed");
    let denied = root.join("denied");
    let initial = root.join("initial-target");
    let replacement = root.join("replacement-target");
    let symlink_path = root.join("replaceable-link");
    let allowed_exec = root.join("exec-allowed-future");
    let denied_exec = root.join("exec-denied-future");
    let allowed_native = root.join("native-allowed-future");
    let denied_native = root.join("native-denied-future");
    fs::create_dir_all(&allowed).unwrap();
    fs::create_dir_all(&denied).unwrap();
    fs::write(allowed.join("rename-source"), b"rename-source").unwrap();
    fs::write(allowed.join("hard-source"), b"hard-link-source").unwrap();
    fs::write(&initial, b"initial-granted-target").unwrap();
    fs::write(&replacement, b"replacement-ungranted-target").unwrap();
    symlink(&initial, &symlink_path).unwrap();

    let ambient = BTreeMap::from([
        (CASE.into(), "path-operations".into()),
        (
            RENAME_SOURCE.into(),
            allowed.join("rename-source").to_string_lossy().into_owned(),
        ),
        (
            RENAME_ALLOWED.into(),
            allowed
                .join("rename-allowed")
                .to_string_lossy()
                .into_owned(),
        ),
        (
            RENAME_DENIED.into(),
            denied.join("rename-denied").to_string_lossy().into_owned(),
        ),
        (
            HARD_SOURCE.into(),
            allowed.join("hard-source").to_string_lossy().into_owned(),
        ),
        (
            HARD_ALLOWED.into(),
            allowed.join("hard-allowed").to_string_lossy().into_owned(),
        ),
        (
            HARD_DENIED.into(),
            denied.join("hard-denied").to_string_lossy().into_owned(),
        ),
        (SYMLINK.into(), symlink_path.to_string_lossy().into_owned()),
        (
            SYMLINK_INITIAL.into(),
            initial.to_string_lossy().into_owned(),
        ),
        (
            SYMLINK_REPLACEMENT.into(),
            replacement.to_string_lossy().into_owned(),
        ),
        (
            EXEC_ALLOWED.into(),
            allowed_exec.to_string_lossy().into_owned(),
        ),
        (
            EXEC_DENIED.into(),
            denied_exec.to_string_lossy().into_owned(),
        ),
        (
            NATIVE_EXEC_ALLOWED.into(),
            allowed_native.to_string_lossy().into_owned(),
        ),
        (
            NATIVE_EXEC_DENIED.into(),
            denied_native.to_string_lossy().into_owned(),
        ),
    ]);
    let ctx = CompileCtx::new(
        Homes {
            home: root.join("home"),
            cache: root.join("cache"),
            tmp: root.join("tmp"),
            project: root.to_path_buf(),
        },
        root.to_path_buf(),
        ScopeCapabilities::approved(),
        ambient,
    );
    // No root-level grant and no deny entry: all denied cases fall through the positive-only
    // default.  `replaceable-link` is canonicalized while it still names `initial-target`.
    let policy = compile(
        &json!({
            "fs": {
                (allowed.to_string_lossy()): "rw",
                (initial.to_string_lossy()): "r",
                (symlink_path.to_string_lossy()): "r",
                (allowed_exec.to_string_lossy()): "r",
                (allowed_native.to_string_lossy()): "r",
            },
            "net": false,
            "vars": {
                (CASE): true,
                (RENAME_SOURCE): true,
                (RENAME_ALLOWED): true,
                (RENAME_DENIED): true,
                (HARD_SOURCE): true,
                (HARD_ALLOWED): true,
                (HARD_DENIED): true,
                (SYMLINK): true,
                (SYMLINK_INITIAL): true,
                (SYMLINK_REPLACEMENT): true,
                (EXEC_ALLOWED): true,
                (EXEC_DENIED): true,
                (NATIVE_EXEC_ALLOWED): true,
                (NATIVE_EXEC_DENIED): true,
            },
        }),
        &ctx,
    )
    .expect("positive-only policy compiles");
    assert_eq!(policy.fs.rules.default_effect, Effect::Deny);
    assert!(
        policy
            .fs
            .rules
            .entries
            .iter()
            .all(|rule| rule.effect == Effect::Allow),
        "operation probe must not rely on generic deny grammar"
    );
    assert!(
        policy
            .fs
            .rules
            .entries
            .iter()
            .all(|rule| !matches!(rule.matcher.as_str(), "/" | "/**")),
        "operation probe must not rely on a whole-filesystem grant"
    );
    let sandbox = Sandbox::new(&policy).expect("Seatbelt sandbox");
    let prepared = sandbox
        .prepare(
            CommandSpec::new(std::env::current_exe().unwrap())
                .args(["--exact", "macos_path_operations_child", "--nocapture"])
                .cwd(Path::new("/"))
                .redact_stdout(true)
                .redact_stderr(true)
                .audit_label("macos-path-operations"),
        )
        .expect("command prepares");
    assert!(
        prepared.degradation.is_full(),
        "macOS enforcement degraded: {:?}",
        prepared.degradation
    );

    // These files do not exist when compilation or profile preparation occurs.  The host
    // controls establish that both scripts work before the child attempts them.
    write_executable(&allowed_exec);
    write_executable(&denied_exec);
    fs::copy("/bin/echo", &allowed_native).expect("copy native allowed control");
    fs::copy("/bin/echo", &denied_native).expect("copy native denied control");
    for path in [&allowed_native, &denied_native] {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
        // A raw copy of Apple's platform-signed `/bin/echo` is killed by AMFI before it can
        // print even outside the sandbox.  Re-sign the staged byte-identical program ad hoc so
        // the unconfined controls prove a directly executable Mach-O before Seatbelt runs it.
        let signed = Command::new("/usr/bin/codesign")
            .args(["-f", "-s", "-"])
            .arg(path)
            .status()
            .expect("codesign staged native control");
        assert!(
            signed.success(),
            "codesign staged native control failed: {signed}"
        );
    }
    assert!(Command::new(&allowed_exec).status().unwrap().success());
    assert!(Command::new(&denied_exec).status().unwrap().success());
    assert_eq!(
        Command::new(&allowed_native)
            .arg("native-unconfined")
            .output()
            .unwrap()
            .stdout,
        b"native-unconfined\n"
    );
    assert_eq!(
        Command::new(&denied_native)
            .arg("native-unconfined")
            .output()
            .unwrap()
            .stdout,
        b"native-unconfined\n"
    );
    fs::remove_file(&symlink_path).unwrap();
    symlink(&replacement, &symlink_path).unwrap();

    let output = tool_output::output(prepared);
    assert!(
        output.status.success(),
        "path-operation probe failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_executable(path: &Path) {
    fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).unwrap();
}

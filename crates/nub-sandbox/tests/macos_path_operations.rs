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
use std::process::{Command, Output};

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
const NATIVE_LEAF: &str = "NUB_MACOS_PATH_OPERATIONS_NATIVE_LEAF";

#[test]
fn macos_path_operations_child() {
    if std::env::var_os(CASE).is_none() {
        return;
    }
    if let Some(label) = std::env::var_os(NATIVE_LEAF) {
        println!("native-leaf:{}", label.to_string_lossy());
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

    // Unlike the scripts above, these are copies of this already-built Rust Mach-O test binary.
    // Its one leaf re-entry proves direct executable mapping/loading without relocating an Apple
    // platform binary, whose unconfined AMFI behavior differs across runner versions.
    let native_allowed = native_leaf(&path(NATIVE_EXEC_ALLOWED), "native-allowed")
        .expect("allowed future Mach-O executable starts");
    assert_native_leaf_success(
        "allowed future Mach-O executable",
        &native_allowed,
        "native-allowed",
    );
    assert!(
        fs::read(path(NATIVE_EXEC_DENIED)).is_err(),
        "ungranted future Mach-O executable was readable before direct execution"
    );
    match native_leaf(&path(NATIVE_EXEC_DENIED), "native-denied") {
        Ok(output) => assert!(
            !output.status.success(),
            "ungranted future Mach-O executable completed successfully:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
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
    // controls establish that both scripts and native Mach-O leaf copies work before the child
    // attempts them.
    write_executable(&allowed_exec);
    write_executable(&denied_exec);
    let test_exe = std::env::current_exe().expect("test executable");
    fs::copy(&test_exe, &allowed_native).expect("copy native allowed control");
    fs::copy(&test_exe, &denied_native).expect("copy native denied control");
    for path in [&allowed_native, &denied_native] {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }
    assert!(Command::new(&allowed_exec).status().unwrap().success());
    assert!(Command::new(&denied_exec).status().unwrap().success());
    for path in [&allowed_native, &denied_native] {
        let output = native_leaf(path, "native-unconfined").expect("unconfined native leaf starts");
        assert_native_leaf_success("unconfined native leaf", &output, "native-unconfined");
    }
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

fn native_leaf(path: &Path, label: &str) -> std::io::Result<Output> {
    Command::new(path)
        .args(["--exact", "macos_path_operations_child", "--nocapture"])
        .env(CASE, "path-operations")
        .env(NATIVE_LEAF, label)
        .output()
}

fn assert_native_leaf_success(context: &str, output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{context} failed ({})\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&format!("native-leaf:{label}")),
        "{context} did not emit its leaf marker\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{context} wrote to stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

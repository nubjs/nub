//! Transitional rename of nub's own lockfile: `lock.yaml` → `nub.lock`.
//! A virgin install writes the new name; an unmigrated `lock.yaml` is renamed
//! byte-identically on the next mutating op (and a redundant one is removed);
//! workspace members migrate too. All rows run OFFLINE with empty-dependency
//! manifests, pointing the registry at a dead port so any accidental network
//! fails loudly.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

fn project(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-lockfile-rename-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".npmrc"), "registry=http://127.0.0.1:1/\n").unwrap();
    for (name, body) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }
    dir
}

fn run(dir: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("XDG_CACHE_HOME", dir.join("xdg-cache"))
        .output()
        .expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

const EMPTY_LOCK: &str = "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n";
const EMPTY_PKG: &str = r#"{"name":"app","version":"1.0.0"}"#;

/// A truly-fresh project (no lockfile, no PM signal) writes `nub.lock`,
/// not the legacy `lock.yaml`.
#[test]
fn virgin_install_writes_package_lock() {
    let dir = project("virgin", &[("package.json", EMPTY_PKG)]);
    let (stdout, stderr, code) = run(&dir, &["install", "--offline"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        dir.join("nub.lock").is_file(),
        "virgin install must write nub.lock: {stderr}"
    );
    assert!(
        !dir.join("lock.yaml").exists(),
        "the legacy name must never be written fresh"
    );
}

/// An existing `lock.yaml` is migrated to `nub.lock` byte-identically on
/// the next install, and the stale file is gone.
#[test]
fn existing_lock_yaml_is_migrated_byte_identically() {
    let dir = project(
        "migrate",
        &[("package.json", EMPTY_PKG), ("lock.yaml", EMPTY_LOCK)],
    );
    let (stdout, stderr, code) = run(&dir, &["install", "--offline"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        !dir.join("lock.yaml").exists(),
        "the legacy lock.yaml must be removed after migration"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("nub.lock")).unwrap(),
        EMPTY_LOCK,
        "the migration is a byte-identical rename"
    );
}

/// When BOTH names exist, the redundant `lock.yaml` is removed and
/// `nub.lock` is kept untouched (no double-lockfile left behind).
#[test]
fn both_present_keeps_package_lock_and_removes_legacy() {
    let kept = "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n# kept\n";
    let dir = project(
        "both",
        &[
            ("package.json", EMPTY_PKG),
            ("nub.lock", kept),
            ("lock.yaml", EMPTY_LOCK),
        ],
    );
    let (stdout, stderr, code) = run(&dir, &["install", "--offline"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        !dir.join("lock.yaml").exists(),
        "the redundant legacy lock.yaml must be removed"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("nub.lock")).unwrap(),
        kept,
        "the existing nub.lock must be kept verbatim, not overwritten by the legacy file"
    );
}

/// A read-only op (here, the same install path is mutating, so use a workspace
/// member) — a `lock.yaml` in a workspace member dir migrates too.
#[test]
fn workspace_member_lock_yaml_migrates() {
    let dir = project(
        "ws",
        &[
            (
                "package.json",
                r#"{"name":"root","version":"1.0.0","private":true,"workspaces":["packages/*"]}"#,
            ),
            // Root carries nub's lockfile so the project resolves as nub identity.
            ("nub.lock", EMPTY_LOCK),
            (
                "packages/a/package.json",
                r#"{"name":"a","version":"1.0.0"}"#,
            ),
            ("packages/a/lock.yaml", EMPTY_LOCK),
        ],
    );
    let (stdout, stderr, code) = run(&dir, &["install", "--offline"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        !dir.join("packages/a/lock.yaml").exists(),
        "a workspace member's legacy lock.yaml must migrate too: {stderr}"
    );
    assert!(
        dir.join("packages/a/nub.lock").is_file(),
        "the member's lockfile must land at nub.lock"
    );
}

/// `nub ci` is a frozen, ephemeral install: it installs fine from an existing
/// `lock.yaml` (read-both) but must NEVER migrate or otherwise mutate a
/// checked-in file — no rename, no `nub.lock` written, `lock.yaml` left
/// byte-for-byte untouched.
#[test]
fn ci_never_migrates_or_mutates_the_lockfile() {
    let dir = project(
        "ci",
        &[("package.json", EMPTY_PKG), ("lock.yaml", EMPTY_LOCK)],
    );
    let (stdout, stderr, code) = run(&dir, &["ci"]);
    assert_eq!(
        code, 0,
        "ci must install from lock.yaml: {stdout}\n{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("lock.yaml")).unwrap(),
        EMPTY_LOCK,
        "ci must leave lock.yaml byte-for-byte untouched"
    );
    assert!(
        !dir.join("nub.lock").exists(),
        "ci must not rename or write nub.lock — a frozen install never mutates checked-in files"
    );
}

/// Read-both: a read-only op resolves a project still carrying the legacy
/// `lock.yaml` without migrating it (read-only must not rewrite the tree).
#[test]
fn read_only_op_resolves_legacy_lock_yaml_without_migrating() {
    let dir = project(
        "readonly",
        &[
            (
                "package.json",
                r#"{"name":"app","version":"1.0.0","dependencies":{}}"#,
            ),
            ("lock.yaml", EMPTY_LOCK),
        ],
    );
    // `why` is read-only: it must find the lockfile (read-both) but leave the
    // legacy name in place.
    let (stdout, stderr, code) = run(&dir, &["why", "anything"]);
    // `why` for an absent dep exits non-zero or zero depending on output, but
    // it must NOT error with "no lockfile" and must NOT migrate the file.
    assert!(
        !stderr.contains("no lockfile") && !stderr.to_lowercase().contains("err_nub_no_lockfile"),
        "read-both must let a read-only op find the legacy lock.yaml: {stderr}\n{stdout} (code {code})"
    );
    assert!(
        dir.join("lock.yaml").is_file(),
        "a read-only op must not migrate the legacy lockfile"
    );
}

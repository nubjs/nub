//! Transitional rename of nub's own lockfile: `lock.yaml` → `nub.lock`.
//!
//! The migration rides a REAL lockfile write — nub writes the lockfile only
//! when the resolved graph actually changes (no-churn), and that write lands
//! under the new name (`nub.lock`) with the pre-rename `lock.yaml` removed, so
//! the rename shows up in the same diff as the dep change. A no-op op writes
//! nothing and leaves the legacy file exactly as-is; a frozen/`ci` op never
//! writes, so it never migrates. All rows run OFFLINE, pointing the registry at
//! a dead port so any accidental network fails loudly.

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
    run_env(dir, args, &[])
}

/// Like [`run`] but with extra env vars. The `CI` var is stripped from the
/// inherited environment unless a caller overrides it: aube defaults a flagless
/// install to FROZEN when `CI` is set (pnpm parity), and a frozen op never
/// writes the lockfile, so a real-change row that relied on the flagless
/// writable default would flip to frozen (no write, no migration) when the
/// suite runs on a CI runner. Rows exercising the frozen-read-only path opt
/// back in explicitly (`CI=1`) or pass `--frozen-lockfile`.
fn run_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (String, String, i32) {
    let mut cmd = Command::new(nub_binary());
    cmd.args(args)
        .current_dir(dir)
        .env_remove("CI")
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("XDG_CACHE_HOME", dir.join("xdg-cache"));
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

const EMPTY_LOCK: &str = "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n";
const EMPTY_PKG: &str = r#"{"name":"app","version":"1.0.0"}"#;

/// A manifest with one `file:` local dependency, absent from `EMPTY_LOCK`. An
/// install must re-resolve to record it — a genuine, network-free graph change
/// that exercises the migrating write.
const PKG_WITH_LOCAL_DEP: &str =
    r#"{"name":"app","version":"1.0.0","dependencies":{"localdep":"file:./localdep"}}"#;

/// A truly-fresh project (no lockfile, no PM signal) writes `nub.lock`,
/// not the legacy `lock.yaml`.
#[test]
fn virgin_install_writes_nub_lock() {
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

/// CASE 1 (the load-bearing one): a NO-OP install on a project still carrying
/// `lock.yaml` writes nothing — no re-resolve, no rewrite. The legacy file is
/// left byte-for-byte and no `nub.lock` appears. Migration must not be a
/// proactive rename; it rides a real change only.
#[test]
fn no_op_install_leaves_legacy_untouched() {
    let dir = project(
        "noop",
        &[("package.json", EMPTY_PKG), ("lock.yaml", EMPTY_LOCK)],
    );
    let (stdout, stderr, code) = run(&dir, &["install", "--offline"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        std::fs::read_to_string(dir.join("lock.yaml")).unwrap(),
        EMPTY_LOCK,
        "a no-op install must leave lock.yaml byte-for-byte untouched"
    );
    assert!(
        !dir.join("nub.lock").exists(),
        "a no-op install must not create nub.lock — no write means no migration"
    );
}

/// CASE 2: a REAL lockfile change (here a `file:` dep the lockfile lacks) on a
/// project carrying `lock.yaml` writes the new graph to `nub.lock` and removes
/// the legacy file — the rename rides the change. `--no-frozen-lockfile` forces
/// the writable path deterministically (independent of a CI runner's frozen
/// default).
#[test]
fn real_change_migrates_to_nub_lock() {
    let dir = project(
        "realchange",
        &[
            ("package.json", PKG_WITH_LOCAL_DEP),
            (
                "localdep/package.json",
                r#"{"name":"localdep","version":"1.0.0"}"#,
            ),
            ("lock.yaml", EMPTY_LOCK),
        ],
    );
    let (stdout, stderr, code) = run(&dir, &["install", "--no-frozen-lockfile", "--offline"]);
    assert_eq!(
        code, 0,
        "real-change install must succeed: {stdout}\n{stderr}"
    );
    assert!(
        !dir.join("lock.yaml").exists(),
        "the legacy lock.yaml must be removed once the migrating write lands: {stderr}"
    );
    let nub_lock =
        std::fs::read_to_string(dir.join("nub.lock")).expect("a real change must write nub.lock");
    assert!(
        nub_lock.contains("localdep"),
        "the migrated nub.lock must reflect the resolved change:\n{nub_lock}"
    );
}

/// `nub ci` is a frozen, ephemeral install: it installs fine from an existing
/// `lock.yaml` (read-both) but never writes the lockfile, so it never migrates
/// — no rename, no `nub.lock`, `lock.yaml` left byte-for-byte untouched.
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
        "ci must not rename or write nub.lock — a frozen install never writes the lockfile"
    );
}

/// `nub install --frozen-lockfile` is read-only over the lockfile, so it
/// installs from the legacy `lock.yaml` (read-both) but must not migrate it —
/// no rename, no `nub.lock`. The rename is a write; a frozen install makes none.
#[test]
fn frozen_flag_install_does_not_migrate() {
    let dir = project(
        "frozen-flag",
        &[("package.json", EMPTY_PKG), ("lock.yaml", EMPTY_LOCK)],
    );
    let (stdout, stderr, code) = run(&dir, &["install", "--frozen-lockfile", "--offline"]);
    assert_eq!(
        code, 0,
        "frozen install must succeed from lock.yaml: {stdout}\n{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("lock.yaml")).unwrap(),
        EMPTY_LOCK,
        "a frozen install must leave lock.yaml byte-for-byte untouched"
    );
    assert!(
        !dir.join("nub.lock").exists(),
        "a frozen install must not rename or write nub.lock"
    );
}

/// A flagless `nub install` under `CI=1` defaults to FROZEN (pnpm parity), so
/// it too must not migrate — the rename would surprise a CI run with an
/// unexpected git diff. Migration is a developer-machine (writable) event.
#[test]
fn ci_env_default_frozen_does_not_migrate() {
    let dir = project(
        "ci-env",
        &[("package.json", EMPTY_PKG), ("lock.yaml", EMPTY_LOCK)],
    );
    let (stdout, stderr, code) = run_env(&dir, &["install", "--offline"], &[("CI", "1")]);
    assert_eq!(
        code, 0,
        "CI-frozen install must succeed from lock.yaml: {stdout}\n{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("lock.yaml")).unwrap(),
        EMPTY_LOCK,
        "a CI-default frozen install must leave lock.yaml untouched"
    );
    assert!(
        !dir.join("nub.lock").exists(),
        "a CI-default frozen install must not rename or write nub.lock"
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

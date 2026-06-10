//! The two-mode model, behaviorally, through the binary: `pm use nub`'s
//! offline migration invariants, the role-first lifecycle UA, and the
//! nub-identity config gating (stray-yaml warning). All rows run OFFLINE —
//! `pm use nub` never touches a registry by design, the install rows use
//! empty-dependency manifests, and every project points its registry at a
//! dead port so accidental network fails loudly. The online halves (real
//! pnpm judging the reversed state) live in tests/aube-conformance (the
//! `nub` format leg) and tests/brand-sweep.

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
        "nub-two-mode-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".npmrc"), "registry=http://127.0.0.1:1/\n").unwrap();
    for (name, body) in files {
        std::fs::write(dir.join(name), body).unwrap();
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

/// `pm use nub` on a single-package pnpm project carrying a catalog: the
/// whole switch is offline, the yaml dies, the catalog lands as a
/// packages-less `workspaces` object (the Bun shape), settings land in
/// `.npmrc`, and the lockfile is renamed byte-identically. Rerunning is a
/// no-op (idempotence is the contract).
#[test]
fn use_nub_migrates_a_single_package_catalog_project_offline_and_idempotently() {
    let dir = project(
        "use-nub",
        &[
            (
                "package.json",
                r#"{"name":"app","version":"1.0.0","packageManager":"pnpm@10.0.0"}"#,
            ),
            ("pnpm-lock.yaml", EMPTY_LOCK),
            (
                "pnpm-workspace.yaml",
                "catalog:\n  left-pad: 1.3.0\nminimumReleaseAge: 1440\nproduction: true\n",
            ),
        ],
    );
    let (stdout, stderr, code) = run(&dir, &["pm", "use", "nub"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    // Zero pnpm-named files; lock.yaml carries the exact prior bytes.
    assert!(!dir.join("pnpm-workspace.yaml").exists(), "{stdout}");
    assert!(!dir.join("pnpm-lock.yaml").exists(), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(dir.join("lock.yaml")).unwrap(),
        EMPTY_LOCK,
        "the rename must be byte-identical"
    );

    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    assert_eq!(
        manifest["packageManager"],
        format!("nub@{}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(manifest["devEngines"]["packageManager"]["name"], "nub");
    assert_eq!(manifest["devEngines"]["packageManager"]["onFail"], "warn");
    assert_eq!(
        manifest["workspaces"]["catalog"]["left-pad"], "1.3.0",
        "single-package catalogs land as a packages-less workspaces object"
    );
    assert!(
        manifest["workspaces"].get("packages").is_none(),
        "no packages key must be invented for a single-package repo"
    );

    let npmrc = std::fs::read_to_string(dir.join(".npmrc")).unwrap();
    assert!(
        npmrc.contains("minimum-release-age=1440"),
        "settings must land in .npmrc: {npmrc}"
    );
    assert!(
        stdout.contains("production"),
        "the warn tail must name the transient key loudly: {stdout}"
    );
    assert!(
        stdout.contains("corepack") && stdout.contains("nub pm use pnpm"),
        "the summary must carry the teammates consequences block: {stdout}"
    );

    // Idempotent rerun: same identity, lockfile kept, nothing new to migrate.
    let (stdout2, stderr2, code2) = run(&dir, &["pm", "use", "nub"]);
    assert_eq!(code2, 0, "stdout: {stdout2}\nstderr: {stderr2}");
    assert!(
        stdout2.contains("lock.yaml: kept"),
        "rerun must keep, not re-convert: {stdout2}"
    );
}

/// Injected-deps state refuses the whole switch before anything is written —
/// the engine has no injected-deps implementation, and `use` never silently
/// changes install semantics.
#[test]
fn use_nub_refuses_injected_deps_with_nothing_written() {
    let dir = project(
        "injected",
        &[
            (
                "package.json",
                r#"{"name":"app","version":"1.0.0","packageManager":"pnpm@10.0.0","dependenciesMeta":{"sibling":{"injected":true}}}"#,
            ),
            ("pnpm-lock.yaml", EMPTY_LOCK),
            ("pnpm-workspace.yaml", "packages:\n  - \"packages/*\"\n"),
        ],
    );
    let (stdout, stderr, code) = run(&dir, &["pm", "use", "nub"]);
    assert_ne!(code, 0, "injected deps must refuse: {stdout}");
    assert!(
        stderr.contains("injected") && stderr.contains("package.json"),
        "the refusal must name the state and where it lives: {stderr}"
    );
    assert!(
        dir.join("pnpm-workspace.yaml").exists()
            && dir.join("pnpm-lock.yaml").exists()
            && !dir.join("lock.yaml").exists(),
        "a refusal must leave the project byte-untouched"
    );
    let manifest = std::fs::read_to_string(dir.join("package.json")).unwrap();
    assert!(
        manifest.contains("pnpm@10.0.0"),
        "the declaration must be untouched after the refusal"
    );
}

/// Under nub identity a stray pnpm-workspace.yaml is ignore-with-warning:
/// exactly one warning naming it unread plus the remedies, and the install
/// proceeds against lock.yaml.
#[test]
fn stray_workspace_yaml_under_nub_identity_warns_once_and_install_proceeds() {
    let dir = project(
        "stray-yaml",
        &[
            (
                "package.json",
                r#"{"name":"app","version":"1.0.0","packageManager":"nub@0.0.1"}"#,
            ),
            ("lock.yaml", EMPTY_LOCK),
            ("pnpm-workspace.yaml", "nodeLinker: hoisted\n"),
        ],
    );
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        stderr
            .matches("pnpm-workspace.yaml is not read under nub identity")
            .count(),
        1,
        "exactly one warning: {stderr}"
    );
    assert!(
        stderr.contains("nub pm use pnpm") && stderr.contains("nub pm use nub"),
        "the warning must carry both remedies: {stderr}"
    );
    assert!(
        dir.join("lock.yaml").is_file() && !dir.join("pnpm-lock.yaml").exists(),
        "lock.yaml stays the lockfile"
    );
}

/// The role-first lifecycle UA, observed by a real root postinstall: a
/// pnpm-declared project is served pnpm-first at the PINNED version with the
/// nub token second; a nub-identity project is nub-first in the runner
/// dialect. (The fresh + engine-parity cases live in tests/brand-sweep and
/// the pm_engine unit tests.)
#[test]
fn lifecycle_ua_is_pnpm_first_in_compat_and_nub_first_under_nub_identity() {
    let postinstall = r#""scripts":{"postinstall":"node -e \"require('fs').writeFileSync('ua.txt', process.env.npm_config_user_agent||'')\""}"#;

    let dir = project(
        "ua-compat",
        &[(
            "package.json",
            &format!(
                r#"{{"name":"app","version":"1.0.0","packageManager":"pnpm@9.9.9",{postinstall}}}"#
            ),
        )],
    );
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let ua = std::fs::read_to_string(dir.join("ua.txt")).expect("postinstall must run");
    assert!(
        ua.starts_with(&format!(
            "pnpm/9.9.9 nub/{} node/v",
            env!("CARGO_PKG_VERSION")
        )),
        "compat UA must be pnpm-first at the pinned version, nub second: {ua}"
    );

    let dir = project(
        "ua-nub",
        &[
            (
                "package.json",
                &format!(
                    r#"{{"name":"app","version":"1.0.0","packageManager":"nub@0.0.1",{postinstall}}}"#
                ),
            ),
            ("lock.yaml", EMPTY_LOCK),
        ],
    );
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let ua = std::fs::read_to_string(dir.join("ua.txt")).expect("postinstall must run");
    assert!(
        ua.starts_with(&format!("nub/{} npm/? node/v", env!("CARGO_PKG_VERSION"))),
        "nub-identity UA must be nub-first in the runner dialect: {ua}"
    );
}

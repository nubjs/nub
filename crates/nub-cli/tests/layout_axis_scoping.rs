//! Where the `node_modules` layout comes from follows the project's identity.
//! A pnpm project takes it from pnpm's own configuration, read the way pnpm 12
//! reads it, and `nub config` answers from the same files. A nub project takes
//! it from `nub.jsonc`, `.npmrc`, or the command line.
//!
//! Yarn PnP is deliberately not retested here — refusing to build a tree Nub
//! cannot produce is not the same as honoring a layout preference, and
//! `abort_eagerly.rs` covers that the refusal still fires.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
        "nub-layout-axis-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (name, body) in files {
        std::fs::write(dir.join(name), body).unwrap();
    }
    dir
}

/// Run Nub against a scratch HOME/XDG so the developer's own global config
/// cannot supply or mask any setting.
fn run(dir: &Path, args: &[&str]) -> Output {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("NUB_SELF_SHIM", "0")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", dir.join("xdg-config"))
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("XDG_CACHE_HOME", dir.join("xdg-cache"))
        .output()
        .expect("failed to spawn nub")
}

fn config_get(dir: &Path, key: &str) -> String {
    let out = run(dir, &["config", "get", key]);
    assert!(
        out.status.success(),
        "config get {key} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Install in `dir` and return everything it printed, failing if it failed.
fn install(dir: &Path) -> String {
    let out = run(dir, &["install", "--offline"]);
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "install failed:\n{report}");
    report
}

/// The install report's first row, which a nub install always prints.
fn has_layout_header(report: &str) -> bool {
    report
        .lines()
        .any(|line| line.trim_start().starts_with("linker "))
}

const PNPM_LOCK: &str = "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n";

/// pnpm 12 reads `nodeLinker` from `pnpm-workspace.yaml` and not from a
/// project `.npmrc`, and prints no layout header of nub's.
#[test]
fn a_pnpm_project_takes_its_layout_from_pnpm_workspace_yaml() {
    let dir = project(
        "pnpm",
        &[
            ("package.json", r#"{"name":"app","version":"1.0.0"}"#),
            ("pnpm-lock.yaml", PNPM_LOCK),
            (
                "pnpm-workspace.yaml",
                "nodeLinker: hoisted\nautoInstallPeers: false\n",
            ),
        ],
    );
    assert_eq!(config_get(&dir, "nodeLinker"), "hoisted");
    assert_eq!(config_get(&dir, "autoInstallPeers"), "false");
    let report = install(&dir);
    assert!(
        !has_layout_header(&report),
        "pnpm prints no layout header:\n{report}"
    );

    let npmrc = project(
        "pnpm-npmrc",
        &[
            ("package.json", r#"{"name":"app","version":"1.0.0"}"#),
            ("pnpm-lock.yaml", PNPM_LOCK),
            (".npmrc", "node-linker=hoisted\n"),
        ],
    );
    assert_eq!(config_get(&npmrc, "nodeLinker"), "undefined");
}

/// A nub project takes its layout from its own sources, and its install
/// reports the layout it resolved.
#[test]
fn a_nub_project_takes_its_layout_from_npmrc() {
    let dir = project(
        "nub",
        &[(
            "package.json",
            r#"{"name":"app","version":"1.0.0","packageManager":"nub@0.1.0"}"#,
        )],
    );
    assert_eq!(config_get(&dir, "nodeLinker"), "isolated");
    std::fs::write(dir.join(".npmrc"), "nodeLinker=hoisted\n").unwrap();
    assert_eq!(config_get(&dir, "nodeLinker"), "hoisted");
    let report = install(&dir);
    assert!(
        has_layout_header(&report),
        "a nub install reports its layout:\n{report}"
    );
}

/// A yarn declaration makes a nub project, which reads no yarn configuration:
/// `nub pm migrate` converts a yarn lockfile once, and after that the project
/// is nub's. Its layout key is no more special than the rest of the file.
#[test]
fn a_yarnrc_supplies_no_config_and_no_layout() {
    let files = [
        (
            "package.json",
            r#"{"name":"app","version":"1.0.0","packageManager":"yarn@4.6.0"}"#,
        ),
        ("yarn.lock", "__metadata:\n  version: 8\n"),
        (
            ".yarnrc.yml",
            "npmRegistryServer: \"https://registry.yarn.example\"\nnodeLinker: node-modules\n",
        ),
    ];

    let dir = project("yarn", &files);
    assert_ne!(
        config_get(&dir, "registry"),
        "https://registry.yarn.example/",
        "yarn config must not reach nub's own view"
    );
    assert_eq!(
        config_get(&dir, "nodeLinker"),
        "isolated",
        "the file's nodeLinker must not displace nub's default linker"
    );

    // The neutral file still decides layout, which is the point of the axis:
    // dropping the branded reader must not drop the unbranded one.
    let neutral = project("yarn-npmrc", &files);
    std::fs::write(neutral.join(".npmrc"), "nodeLinker=hoisted\n").unwrap();
    assert_eq!(config_get(&neutral, "nodeLinker"), "hoisted");
}

/// `nub ci` builds the tree a deploy copies into an image, where the shared
/// store does not exist, so it keeps the store inside the project the way CI
/// does. A plain install in the same project keeps sharing it.
#[test]
fn nub_ci_keeps_the_virtual_store_inside_the_project() {
    let dir = project(
        "ci",
        &[(
            "package.json",
            r#"{"name":"app","version":"1.0.0","packageManager":"nub@0.1.0"}"#,
        )],
    );
    let virtual_store_dir = || {
        let state = std::fs::read_to_string(dir.join("node_modules/.modules.yaml")).unwrap();
        let state: serde_json::Value = serde_json::from_str(&state).unwrap();
        state["virtualStoreDir"].as_str().unwrap().to_string()
    };

    install(&dir);
    let shared = virtual_store_dir();
    assert!(
        shared.ends_with("links"),
        "a plain install shares the store: {shared}"
    );

    let out = run(&dir, &["ci"]);
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "ci failed:\n{report}");
    assert_eq!(virtual_store_dir(), ".store", "{report}");
    assert!(
        report.contains("isolated (global virtual store auto-disabled in CI)"),
        "the header names the store the install built:\n{report}"
    );
}

//! Nub chooses the `node_modules` layout from `nub.jsonc`, `.npmrc`, or the
//! command line under every incumbent. Branded manager files remain usable for
//! resolution settings but not layout.
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

const PNPM_LOCK: &str = "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n";

#[test]
fn a_pnpm_workspace_yaml_supplies_resolution_config_but_not_layout() {
    let files = [
        (
            "package.json",
            r#"{"name":"app","version":"1.0.0","packageManager":"pnpm@10.4.1"}"#,
        ),
        ("pnpm-lock.yaml", PNPM_LOCK),
        (
            "pnpm-workspace.yaml",
            "nodeLinker: hoisted\nshamefullyHoist: true\nautoInstallPeers: false\n",
        ),
    ];

    let dir = project("pnpm", &files);
    assert_eq!(
        config_get(&dir, "autoInstallPeers"),
        "false",
        "resolution config from pnpm-workspace.yaml must still be mirrored"
    );
    assert_eq!(
        config_get(&dir, "nodeLinker"),
        "isolated",
        "the file's layout key must not displace nub's default linker"
    );
    assert_eq!(
        config_get(&dir, "shamefullyHoist"),
        "undefined",
        "nor any other layout key in it"
    );

    let neutral = project("pnpm-npmrc", &files);
    std::fs::write(
        neutral.join(".npmrc"),
        "nodeLinker=hoisted\nshamefully-hoist=true\n",
    )
    .unwrap();
    assert_eq!(config_get(&neutral, "nodeLinker"), "hoisted");
    assert_eq!(config_get(&neutral, "shamefullyHoist"), "true");
}

/// Pnpm 11 takes resolution settings from workspace YAML. Nub keeps layout in
/// `nub.jsonc`, `.npmrc`, or the command line under every pnpm major.
#[test]
fn a_pnpm_11_project_takes_resolution_from_workspace_yaml_but_never_layout() {
    let files = [
        (
            "package.json",
            r#"{"name":"app","version":"1.0.0","packageManager":"pnpm@11.3.0"}"#,
        ),
        ("pnpm-lock.yaml", PNPM_LOCK),
        (
            "pnpm-workspace.yaml",
            "nodeLinker: hoisted\nshamefullyHoist: true\nautoInstallPeers: false\n",
        ),
    ];

    let dir = project("pnpm11", &files);
    assert_eq!(
        config_get(&dir, "autoInstallPeers"),
        "false",
        "resolution config from pnpm-workspace.yaml must still be mirrored"
    );
    assert_eq!(
        config_get(&dir, "nodeLinker"),
        "isolated",
        "the file's layout key must not displace Nub's default linker"
    );
    assert_eq!(
        config_get(&dir, "shamefullyHoist"),
        "undefined",
        "nor any other layout key in it"
    );

    // The paired half: refusing the YAML as a layout source is what keeps
    // `.npmrc` layout keys readable under pnpm 11's otherwise auth-only allowlist.
    let neutral = project("pnpm11-npmrc", &files);
    std::fs::write(
        neutral.join(".npmrc"),
        "nodeLinker=hoisted\nshamefully-hoist=true\n",
    )
    .unwrap();
    assert_eq!(config_get(&neutral, "nodeLinker"), "hoisted");
    assert_eq!(config_get(&neutral, "shamefullyHoist"), "true");
}

#[test]
fn a_pnpm_11_global_config_reports_ignored_layout_but_keeps_resolution() {
    let files = [
        (
            "package.json",
            r#"{"name":"app","version":"1.0.0","packageManager":"pnpm@11.3.0"}"#,
        ),
        ("pnpm-lock.yaml", PNPM_LOCK),
    ];
    let dir = project("pnpm11-global", &files);
    let global = dir.join("xdg-config/pnpm");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::write(
        global.join("config.yaml"),
        "nodeLinker: hoisted\npublicHoistPattern:\n  - '*'\nautoInstallPeers: false\n",
    )
    .unwrap();

    assert_eq!(config_get(&dir, "nodeLinker"), "isolated");
    assert_eq!(config_get(&dir, "publicHoistPattern"), "undefined");
    assert_eq!(
        config_get(&dir, "autoInstallPeers"),
        "false",
        "non-layout settings in pnpm's global config must remain readable"
    );

    let out = run(&dir, &["install", "--offline", "--ignore-scripts"]);
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "install failed:\n{report}");
    assert!(
        report.contains(
            "configurable via nub.jsonc install.linker, .npmrc node-linker, or --node-linker"
        ),
        "the install report must disclose the ignored global layout setting:\n{report}"
    );
    assert!(
        report.contains("auto-install-peers=false (pnpm global config.yaml)"),
        "the report must still attribute the readable global resolution setting:\n{report}"
    );
}

#[test]
fn a_yarnrc_supplies_registry_config_but_not_layout() {
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
    assert_eq!(
        config_get(&dir, "registry"),
        "https://registry.yarn.example/",
        "registry config from .yarnrc.yml must still be mirrored"
    );
    assert_eq!(
        config_get(&dir, "nodeLinker"),
        "isolated",
        "the file's nodeLinker must not displace nub's default linker"
    );

    let neutral = project("yarn-npmrc", &files);
    std::fs::write(neutral.join(".npmrc"), "nodeLinker=hoisted\n").unwrap();
    assert_eq!(config_get(&neutral, "nodeLinker"), "hoisted");
}

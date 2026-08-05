//! Layout config follows the incumbent package manager and its major version.
//! Pnpm 10 and below use `.npmrc`; pnpm 11 and later use
//! `pnpm-workspace.yaml`. Yarn and Bun layout keys remain unsupported, with the
//! neutral `.npmrc` spelling available instead.
//!
//! Yarn PnP is deliberately not retested here — refusing to build a tree nub
//! cannot produce is not the same as honoring a layout preference, and
//! `abort_eagerly.rs` covers that the refusal still fires.

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

/// `nub config get <key>`, run against a scratch HOME/XDG so the developer's
/// own global config cannot supply or mask any of these settings.
fn config_get(dir: &Path, key: &str) -> String {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let out = Command::new(nub_binary())
        .args(["config", "get", key])
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("NUB_SELF_SHIM", "0")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", dir.join("xdg-config"))
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("XDG_CACHE_HOME", dir.join("xdg-cache"))
        .output()
        .expect("failed to spawn nub");
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

/// Pnpm 11 reads behavior and layout settings from workspace YAML rather than
/// `.npmrc`; Nub mirrors the same source.
#[test]
fn a_pnpm_11_project_reads_layout_from_workspace_yaml() {
    let files = [
        (
            "package.json",
            r#"{"name":"app","version":"1.0.0","packageManager":"pnpm@11.3.0"}"#,
        ),
        ("pnpm-lock.yaml", PNPM_LOCK),
        ("pnpm-workspace.yaml", "nodeLinker: hoisted\n"),
        (".npmrc", "nodeLinker=isolated\nauto-install-peers=false\n"),
    ];

    let dir = project("pnpm11", &files);
    assert_eq!(config_get(&dir, "nodeLinker"), "hoisted");
    assert_eq!(
        config_get(&dir, "autoInstallPeers"),
        "undefined",
        "resolution config in .npmrc still follows pnpm 11, which ignores it there"
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

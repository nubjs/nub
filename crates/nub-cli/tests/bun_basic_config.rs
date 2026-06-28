use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

fn temp_project(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-bun-config-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (name, body) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }
    dir
}

fn run_with_xdg_config(dir: &Path, xdg_config: &Path, args: &[&str]) -> (String, String, i32) {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", path)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", xdg_config)
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

fn config_get(dir: &Path, xdg_config: &Path, key: &str) -> String {
    let (stdout, stderr, code) = run_with_xdg_config(dir, xdg_config, &["config", "get", key]);
    assert_eq!(code, 0, "key={key}\nstdout: {stdout}\nstderr: {stderr}");
    stdout.trim().to_string()
}

#[test]
fn bun_incumbent_reads_project_and_global_bunfig_install_subset() {
    let dir = temp_project(
        "bun",
        &[
            (
                "package.json",
                r#"{"name":"app","version":"1.0.0","packageManager":"bun@1.2.0"}"#,
            ),
            (
                "bunfig.toml",
                r#"
                [install]
                registry = { url = "https://project.registry.example/", token = "project-token" }

                [install.scopes]
                "@acme" = "https://scope.registry.example/"
                "#,
            ),
        ],
    );
    let xdg_config = dir.join("xdg-config");
    std::fs::create_dir_all(&xdg_config).unwrap();
    std::fs::write(
        xdg_config.join(".bunfig.toml"),
        r#"
        [install]
        registry = "https://global.registry.example/"
        linker = "hoisted"
        "#,
    )
    .unwrap();

    assert_eq!(
        config_get(&dir, &xdg_config, "registry"),
        "https://project.registry.example/"
    );
    assert_eq!(
        config_get(&dir, &xdg_config, "@acme:registry"),
        "https://scope.registry.example/"
    );
    // The global bunfig's `linker` IS read for a bun incumbent. `hoisted` is the
    // NON-default value (nub's own default is now isolated+GVS), so resolving to
    // `hoisted` proves the bunfig was read and overrode the embedder default —
    // an `isolated` bunfig would be indistinguishable from that default.
    assert_eq!(config_get(&dir, &xdg_config, "nodeLinker"), "hoisted");
}

#[test]
fn bun_incumbent_maps_url_less_scope_auth_to_default_registry() {
    let dir = temp_project(
        "bun-url-less-scope",
        &[
            (
                "package.json",
                r#"{"name":"app","version":"1.0.0","packageManager":"bun@1.2.0"}"#,
            ),
            (
                "bunfig.toml",
                r#"
                [install]
                registry = { url = "https://default.registry.example/", token = "default-token" }

                [install.scopes]
                "@acme" = { token = "scope-token" }
                "#,
            ),
        ],
    );
    let xdg_config = dir.join("xdg-config");
    std::fs::create_dir_all(&xdg_config).unwrap();

    assert_eq!(
        config_get(&dir, &xdg_config, "@acme:registry"),
        "https://default.registry.example/"
    );
}

#[test]
fn nub_identity_does_not_read_project_or_global_bunfig() {
    let dir = temp_project(
        "nub",
        &[
            (
                "package.json",
                r#"{"name":"app","version":"1.0.0","packageManager":"nub@0.0.1"}"#,
            ),
            (
                "lock.yaml",
                "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n",
            ),
            (
                "bunfig.toml",
                r#"
                [install]
                registry = "https://must-not-read.project.example/"
                linker = "hoisted"
                "#,
            ),
        ],
    );
    let xdg_config = dir.join("xdg-config");
    std::fs::create_dir_all(&xdg_config).unwrap();
    std::fs::write(
        xdg_config.join(".bunfig.toml"),
        r#"
        [install]
        registry = "https://must-not-read.global.example/"
        "#,
    )
    .unwrap();

    assert_eq!(
        config_get(&dir, &xdg_config, "registry"),
        "https://registry.npmjs.org/"
    );
    // The bunfig's `linker = "hoisted"` must NOT be read under nub identity:
    // nodeLinker resolves to nub's own default (isolated, with hoist=false for
    // GVS), not the bunfig's `hoisted`. Asserting the NON-bunfig value is what
    // proves the bunfig was ignored — the project still gets nub's isolated+GVS
    // default like any other non-injected project.
    assert_eq!(config_get(&dir, &xdg_config, "nodeLinker"), "isolated");
}

#[test]
fn non_bun_incumbents_do_not_read_project_or_global_bunfig() {
    let cases = [
        (
            "npm",
            r#"{"name":"app","version":"1.0.0","packageManager":"npm@10.0.0"}"#,
        ),
        (
            "yarn",
            r#"{"name":"app","version":"1.0.0","packageManager":"yarn@4.0.0"}"#,
        ),
        (
            "pnpm",
            r#"{"name":"app","version":"1.0.0","packageManager":"pnpm@10.0.0"}"#,
        ),
        ("fresh", r#"{"name":"app","version":"1.0.0"}"#),
    ];

    for (name, package_json) in cases {
        let dir = temp_project(
            name,
            &[
                ("package.json", package_json),
                (
                    "bunfig.toml",
                    r#"
                    [install]
                    registry = "https://must-not-read.project.example/"
                    linker = "isolated"

                    [install.scopes]
                    "@acme" = "https://must-not-read.scope.example/"
                    "#,
                ),
            ],
        );
        let xdg_config = dir.join("xdg-config");
        std::fs::create_dir_all(&xdg_config).unwrap();
        std::fs::write(
            xdg_config.join(".bunfig.toml"),
            r#"
            [install]
            registry = "https://must-not-read.global.example/"
            linker = "hoisted"
            "#,
        )
        .unwrap();

        assert_eq!(
            config_get(&dir, &xdg_config, "registry"),
            "https://registry.npmjs.org/",
            "case={name}"
        );
        assert_eq!(
            config_get(&dir, &xdg_config, "@acme:registry"),
            "undefined",
            "case={name}"
        );
    }
}

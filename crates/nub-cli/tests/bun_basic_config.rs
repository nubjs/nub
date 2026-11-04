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
        // The fixture pins a differing `nub@<v>` to exercise nub identity, not the
        // self-shim — opt out so a PM verb doesn't try to provision that nub.
        // (env_clear above drops the ambient value, so set it explicitly.)
        .env("NUB_SELF_SHIM", "0")
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
        minimumReleaseAge = 3600
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
    // The global bunfig IS read for a bun incumbent — `minimumReleaseAge` is
    // resolution config, and 3600 bunfig seconds arriving as 60 engine minutes
    // could come from nowhere else.
    assert_eq!(config_get(&dir, &xdg_config, "minimumReleaseAge"), "60");
    // Its unsupported `linker` is not mapped, so the setting stays at Nub's
    // default instead of the bunfig's `hoisted`.
    assert_eq!(config_get(&dir, &xdg_config, "nodeLinker"), "isolated");
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
                "nub.lock",
                "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n",
            ),
            (
                "bunfig.toml",
                r#"
                [install]
                registry = "https://must-not-read.project.example/"
                minimumReleaseAge = 3600
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
    // Resolution config in the bunfig is ignored too, not just the registry:
    // under a bun incumbent 3600 bunfig seconds arrive as 60 engine minutes, so
    // anything else proves the file went unread. (Asserting the exact value
    // would pin nub's own default, which is not what this test is about.)
    assert_ne!(
        config_get(&dir, &xdg_config, "minimumReleaseAge"),
        "60",
        "the bunfig's minimumReleaseAge must not be read under nub identity"
    );
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

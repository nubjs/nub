use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

fn temp_project(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-yarn-classic-config-{tag}-{}-{}",
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

fn run_config_get(dir: &Path, key: &str) -> (String, String, i32) {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let out = Command::new(nub_binary())
        .args(["config", "get", key])
        .current_dir(dir)
        .env_clear()
        .env("PATH", path)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", dir.join("xdg-config"))
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

fn config_get(dir: &Path, key: &str) -> String {
    let (stdout, stderr, code) = run_config_get(dir, key);
    assert_eq!(code, 0, "key={key}\nstdout: {stdout}\nstderr: {stderr}");
    stdout.trim().to_string()
}

const CLASSIC_YARNRC: &str = r#"registry "https://classic.registry.example"
"@acme:registry" "https://scope.registry.example"
"//scope.registry.example/:_authToken" "scope-token"
"#;

#[test]
fn yarn_incumbent_reads_classic_yarnrc_registry_and_scope() {
    let dir = temp_project(
        "yarn1",
        &[
            (
                "package.json",
                r#"{"name":"app","version":"1.0.0","packageManager":"yarn@1.22.22"}"#,
            ),
            ("yarn.lock", "# yarn lockfile v1\n"),
            (".yarnrc", CLASSIC_YARNRC),
        ],
    );

    assert_eq!(
        config_get(&dir, "registry"),
        "https://classic.registry.example/"
    );
    assert_eq!(
        config_get(&dir, "@acme:registry"),
        "https://scope.registry.example/"
    );
}

#[test]
fn yarn_berry_incumbent_ignores_stray_classic_yarnrc() {
    // A Berry project (a `.yarnrc.yml` setting registry A) carrying a stray
    // legacy `.yarnrc` (setting registry B). Yarn Berry abandoned `.yarnrc`, so
    // B must NOT be read: the resolved registry is A, and the scope key the
    // `.yarnrc` declares is absent.
    let dir = temp_project(
        "berry-stray",
        &[
            (
                "package.json",
                r#"{"name":"app","version":"1.0.0","packageManager":"yarn@4.2.2"}"#,
            ),
            (
                ".yarnrc.yml",
                "npmRegistryServer: https://berry.registry.example\n",
            ),
            (".yarnrc", CLASSIC_YARNRC),
        ],
    );

    assert_eq!(
        config_get(&dir, "registry"),
        "https://berry.registry.example/"
    );
    // The classic `.yarnrc`'s scope registry is not read under Berry.
    assert_eq!(config_get(&dir, "@acme:registry"), "undefined");
}

#[test]
fn non_yarn_incumbents_do_not_read_classic_yarnrc() {
    let cases = [
        (
            "npm",
            r#"{"name":"app","version":"1.0.0","packageManager":"npm@10.0.0"}"#,
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
            &[("package.json", package_json), (".yarnrc", CLASSIC_YARNRC)],
        );

        assert_eq!(
            config_get(&dir, "registry"),
            "https://registry.npmjs.org/",
            "case={name}"
        );
        assert_eq!(
            config_get(&dir, "@acme:registry"),
            "undefined",
            "case={name}"
        );
    }
}

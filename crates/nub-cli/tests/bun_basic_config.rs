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
        // One fixture pins a differing `nub@<v>` to exercise nub identity, not
        // the self-shim — opt out so a PM verb doesn't try to provision that
        // nub. (env_clear above drops the ambient value, so set it explicitly.)
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

/// Every shape a bunfig can state a setting in: the registry as a table and as
/// a bare string, a scope with a URL and a url-less one carrying only a token,
/// plus the two non-registry settings that used to be mapped.
const PROJECT_BUNFIG: &str = r#"
[install]
registry = { url = "https://project.registry.example/", token = "project-token" }
linker = "hoisted"
minimumReleaseAge = 3600

[install.scopes]
"@acme" = "https://scope.registry.example/"
"@urlless" = { token = "urlless-token" }
"#;

const GLOBAL_BUNFIG: &str = r#"
[install]
registry = "https://global.registry.example/"
linker = "hoisted"
minimumReleaseAge = 3600
"#;

/// A `bunfig.toml` supplies nub with nothing, under every incumbent — bun's own
/// included, and whether the file is the project's or the global one.
///
/// Two of these cases used to assert the opposite: a bun incumbent mapped the
/// bunfig's registry, its scopes (including a url-less scope, which fell back
/// to the default registry) and `minimumReleaseAge` into nub's own view. Nub no
/// longer reads bun configuration for any setting — `nub pm migrate` converts a
/// bun lockfile once and the project is nub's afterwards — so bun incumbency no
/// longer differs from any other, and the cases collapse into one sweep.
///
/// The two claims that never depended on reading the file for config are kept
/// and are why this is not simply a deletion: the bunfig's `linker` must not
/// direct layout, and its `minimumReleaseAge` must not move the release-age
/// floor. Both are asserted as inequalities rather than against nub's own
/// defaults, which differ per incumbent and are not what this test is about.
#[test]
fn no_incumbent_reads_a_project_or_global_bunfig() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        (
            "bun",
            &[
                (
                    "package.json",
                    r#"{"name":"app","version":"1.0.0","packageManager":"bun@1.2.0"}"#,
                ),
                ("bunfig.toml", PROJECT_BUNFIG),
            ],
        ),
        (
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
                ("bunfig.toml", PROJECT_BUNFIG),
            ],
        ),
        (
            "npm",
            &[
                (
                    "package.json",
                    r#"{"name":"app","version":"1.0.0","packageManager":"npm@10.0.0"}"#,
                ),
                ("bunfig.toml", PROJECT_BUNFIG),
            ],
        ),
        (
            "yarn",
            &[
                (
                    "package.json",
                    r#"{"name":"app","version":"1.0.0","packageManager":"yarn@4.0.0"}"#,
                ),
                ("bunfig.toml", PROJECT_BUNFIG),
            ],
        ),
        (
            "pnpm",
            &[
                (
                    "package.json",
                    r#"{"name":"app","version":"1.0.0","packageManager":"pnpm@10.0.0"}"#,
                ),
                ("bunfig.toml", PROJECT_BUNFIG),
            ],
        ),
        (
            "fresh",
            &[
                ("package.json", r#"{"name":"app","version":"1.0.0"}"#),
                ("bunfig.toml", PROJECT_BUNFIG),
            ],
        ),
    ];

    for (name, files) in cases {
        let dir = temp_project(name, files);
        let xdg_config = dir.join("xdg-config");
        std::fs::create_dir_all(&xdg_config).unwrap();
        std::fs::write(xdg_config.join(".bunfig.toml"), GLOBAL_BUNFIG).unwrap();

        assert_eq!(
            config_get(&dir, &xdg_config, "registry"),
            "https://registry.npmjs.org/",
            "case={name}: neither bunfig may supply the registry"
        );
        assert_eq!(
            config_get(&dir, &xdg_config, "@acme:registry"),
            "undefined",
            "case={name}: a bunfig scope must not supply a scope registry"
        );
        assert_eq!(
            config_get(&dir, &xdg_config, "@urlless:registry"),
            "undefined",
            "case={name}: a url-less bunfig scope must not map to any registry"
        );
        // 3600 bunfig SECONDS arriving as 60 engine MINUTES could come from
        // nowhere else, so that one value is the discriminator for the file
        // having been read at all.
        assert_ne!(
            config_get(&dir, &xdg_config, "minimumReleaseAge"),
            "60",
            "case={name}: a bunfig must not move the release-age floor"
        );
        assert_ne!(
            config_get(&dir, &xdg_config, "nodeLinker"),
            "hoisted",
            "case={name}: a bunfig's linker must not direct the node_modules layout"
        );
    }
}

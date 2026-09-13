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

/// A classic `.yarnrc` supplies nub with nothing, under every incumbent —
/// yarn's own included.
///
/// This file used to assert the opposite for a yarn-classic project, and to
/// draw a Berry-versus-classic distinction on top of it: Berry abandoned
/// `.yarnrc`, so a stray one had to go unread while `.yarnrc.yml` was honored.
/// Neither half survives. Nub no longer reads yarn configuration for any
/// setting — `nub pm migrate` converts a `yarn.lock` once and the project is
/// nub's afterwards — so the two majors no longer differ in what they
/// contribute, which is why the cases collapse into one sweep. The Berry file's
/// own retirement is covered by
/// `layout_axis_scoping::a_yarnrc_supplies_no_config_and_no_layout`.
///
/// The stray-`.yarnrc` fixture is kept because it is the one that could still
/// regress quietly: a resurrected reader would answer from a file no yarn
/// version has read since v1.
#[test]
fn no_incumbent_reads_a_classic_yarnrc() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        (
            "yarn-classic",
            &[
                (
                    "package.json",
                    r#"{"name":"app","version":"1.0.0","packageManager":"yarn@1.22.22"}"#,
                ),
                ("yarn.lock", "# yarn lockfile v1\n"),
                (".yarnrc", CLASSIC_YARNRC),
            ],
        ),
        (
            "yarn-berry-with-stray",
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
        ),
        (
            "npm",
            &[
                (
                    "package.json",
                    r#"{"name":"app","version":"1.0.0","packageManager":"npm@10.0.0"}"#,
                ),
                (".yarnrc", CLASSIC_YARNRC),
            ],
        ),
        (
            "pnpm",
            &[
                (
                    "package.json",
                    r#"{"name":"app","version":"1.0.0","packageManager":"pnpm@10.0.0"}"#,
                ),
                (".yarnrc", CLASSIC_YARNRC),
            ],
        ),
        (
            "fresh",
            &[
                ("package.json", r#"{"name":"app","version":"1.0.0"}"#),
                (".yarnrc", CLASSIC_YARNRC),
            ],
        ),
    ];

    for (name, files) in cases {
        let dir = temp_project(name, files);

        assert_eq!(
            config_get(&dir, "registry"),
            "https://registry.npmjs.org/",
            "case={name}: a yarn file must not supply the registry"
        );
        assert_eq!(
            config_get(&dir, "@acme:registry"),
            "undefined",
            "case={name}: a yarn file must not supply a scope registry"
        );
    }
}

//! Environment-sourced PM settings, behaviorally, through the binary (#654).
//!
//! Two properties that drifted apart once and had nothing pinning them:
//!
//! 1. **The engine acts on an env-set `cacheDir`.** A hand-written
//!    `.npmrc`-only presence gate in front of the settings accessor made every
//!    env spelling inert; it shipped in 0.6.0 and was removed incidentally by
//!    an engine sync, so nothing here would have caught either the break or
//!    the fix. The host-branded `NUB_CACHE_DIR` was separately inert until the
//!    same issue.
//! 2. **`config get` reports what the engine will act on.** The command read
//!    files only, so it denied values the install was already using — the
//!    failure the reporter could not diagnose from the output.
//!
//! All rows run OFFLINE. `cache list` walks `<resolved cacheDir>/packuments-v1`
//! and prints what it finds, so seeding one file there and asking which
//! directory the engine looked in needs no registry — and gives every row a
//! negative control, since an unset override finds nothing.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

/// Env vars that would silently decide these rows if the developer running
/// the suite happens to export one. Scrubbed from every child. The two
/// `userconfig` spellings matter as much as the rest: they relocate the user
/// `.npmrc` even once `HOME` is pinned, which would put the host's config back
/// in front of the fixture's.
const SCRUBBED: &[&str] = &[
    "NUB_CACHE_DIR",
    "AUBE_CACHE_DIR",
    "npm_config_cache_dir",
    "NPM_CONFIG_CACHE_DIR",
    "pnpm_config_cache_dir",
    "PNPM_CONFIG_CACHE_DIR",
    "npm_config_node_linker",
    "NPM_CONFIG_NODE_LINKER",
    "npm_config_http_proxy",
    "NPM_CONFIG_HTTP_PROXY",
    "npm_config_userconfig",
    "NPM_CONFIG_USERCONFIG",
    "http_proxy",
    "HTTP_PROXY",
    "PROXY",
    "proxy",
    "CI",
];

/// A project dir, an empty home, and an override cache dir seeded with one
/// packument file. The dead-port registry makes any accidental network use
/// fail loudly.
struct Fixture {
    project: PathBuf,
    home: PathBuf,
    override_dir: PathBuf,
}

fn fixture(tag: &str) -> Fixture {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "nub-pm-env-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::write(project.join(".npmrc"), "registry=http://127.0.0.1:1/\n").unwrap();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    Fixture {
        project,
        home,
        override_dir: seeded_cache(&root, "override"),
    }
}

/// A cache directory holding one packument named after its own leaf, so a
/// `cache list` naming it identifies WHICH directory the engine resolved
/// rather than merely that it resolved one.
fn seeded_cache(root: &Path, leaf: &str) -> PathBuf {
    let dir = root.join(leaf);
    std::fs::create_dir_all(dir.join("packuments-v1")).unwrap();
    std::fs::write(dir.join("packuments-v1").join(format!("{leaf}.json")), "{}").unwrap();
    dir
}

/// `.npmrc` takes forward slashes on every platform; a Windows path written
/// raw would land backslashes in an ini value.
fn npmrc_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// Every config root is pinned to the fixture, not just the cache and data
/// dirs. The user `.npmrc` is resolved from `HOME` (`USERPROFILE` on Windows),
/// so leaving those on the host puts the developer's own `~/.npmrc` in the
/// chain — and a `cache-dir` line there falsifies three of the controls below,
/// which would go red on that one machine and nowhere else.
fn run(fx: &Fixture, env: &[(&str, &str)], args: &[&str]) -> (String, i32) {
    let mut cmd = Command::new(nub_binary());
    cmd.args(args)
        .current_dir(&fx.project)
        .env("NUB_SELF_SHIM", "0")
        .env("HOME", &fx.home)
        .env("USERPROFILE", &fx.home)
        .env("XDG_CONFIG_HOME", fx.home.join("xdg-config"))
        .env("XDG_DATA_HOME", fx.project.join("xdg-data"))
        .env("XDG_CACHE_HOME", fx.project.join("xdg-cache"));
    for key in SCRUBBED {
        cmd.env_remove(key);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(
        out.status.code().unwrap_or(-1),
        0,
        "`nub {}` failed\nstdout: {stdout}\nstderr: {stderr}",
        args.join(" ")
    );
    (stdout, out.status.code().unwrap_or(-1))
}

/// Which seeded cache directory the engine resolved, as named by `cache list`.
fn cached_names(fx: &Fixture, env: &[(&str, &str)]) -> Vec<String> {
    let (stdout, _) = run(fx, env, &["cache", "list"]);
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Every declared spelling of the `cacheDir` override relocates the cache the
/// engine actually reads — the neutral npm forms and the host-branded
/// `NUB_CACHE_DIR`, which reaches the cache through the embedder's
/// `config_env` knob rather than the settings table.
#[test]
fn cache_dir_env_moves_the_pm_cache() {
    let fx = fixture("cache-dir-env");
    let dir = fx.override_dir.display().to_string();

    assert_eq!(
        cached_names(&fx, &[]),
        Vec::<String>::new(),
        "control: with no override set, the default cache is empty — without \
         this every row below would pass on a cache that never moved"
    );

    for key in [
        "npm_config_cache_dir",
        "NPM_CONFIG_CACHE_DIR",
        "NUB_CACHE_DIR",
    ] {
        assert_eq!(
            cached_names(&fx, &[(key, dir.as_str())]),
            vec!["override".to_string()],
            "{key} did not move the cache the engine reads"
        );
    }

    assert_eq!(
        cached_names(&fx, &[("AUBE_CACHE_DIR", dir.as_str())]),
        Vec::<String>::new(),
        "the engine's own brand must stay unreadable under nub"
    );
}

/// Env outranks `.npmrc`, in both directions. The `.npmrc`-only row is the
/// control: without it, a platform that mangled the path out of the ini file
/// would make the env row pass for the wrong reason.
#[test]
fn cache_dir_env_outranks_npmrc() {
    let fx = fixture("cache-dir-precedence");
    let from_npmrc = seeded_cache(fx.project.parent().unwrap(), "npmrc-dir");
    std::fs::write(
        fx.project.join(".npmrc"),
        format!(
            "registry=http://127.0.0.1:1/\ncache-dir={}\n",
            npmrc_path(&from_npmrc)
        ),
    )
    .unwrap();

    assert_eq!(
        cached_names(&fx, &[]),
        vec!["npmrc-dir".to_string()],
        "control: `.npmrc cache-dir` alone must move the cache"
    );
    assert_eq!(
        cached_names(
            &fx,
            &[(
                "npm_config_cache_dir",
                fx.override_dir.display().to_string().as_str()
            )]
        ),
        vec!["override".to_string()],
        "env must outrank `.npmrc` — the inverted-precedence failure in #654"
    );
}

/// Both env spellings set at once. This is where the precedence rule is
/// encoded twice — the early return in `resolved_cache_dir` and the push order
/// of the config env tier — so the two have to name the same directory. If
/// they drift, `config get` reports a cache the install is not using, which is
/// the failure the whole change exists to remove.
#[test]
fn both_cache_dir_spellings_agree_across_surfaces() {
    let fx = fixture("cache-dir-both");
    let branded = seeded_cache(fx.project.parent().unwrap(), "branded");
    let branded_path = branded.display().to_string();
    let neutral_path = fx.override_dir.display().to_string();
    let env: &[(&str, &str)] = &[
        ("NUB_CACHE_DIR", branded_path.as_str()),
        ("npm_config_cache_dir", neutral_path.as_str()),
    ];

    assert_eq!(
        cached_names(&fx, env),
        vec!["branded".to_string()],
        "the host's own knob must outrank the neutral form, as the branded \
         alias already does inside the settings chain"
    );
    let (reported, _) = run(&fx, env, &["config", "get", "cache-dir"]);
    assert_eq!(
        reported.trim(),
        branded_path,
        "config get named a different directory than the engine resolved"
    );
}

/// `config get` and `config list` report the env tier, so the command agrees
/// with the install about the effective value. Two deliberate exclusions ride
/// along: a scope selector asks about a file, and an ambient variable is not
/// configuration.
#[test]
fn config_reports_env_sourced_values() {
    let fx = fixture("config-env-tier");
    let dir = fx.override_dir.display().to_string();
    let env: &[(&str, &str)] = &[
        ("npm_config_cache_dir", dir.as_str()),
        ("npm_config_node_linker", "hoisted"),
    ];

    let (unset, _) = run(&fx, &[], &["config", "get", "cache-dir"]);
    assert_eq!(
        unset.trim(),
        "undefined",
        "control: unset must read as undefined, or the rows below prove nothing"
    );

    let (value, _) = run(&fx, env, &["config", "get", "cache-dir"]);
    assert_eq!(value.trim(), dir, "config get denied an env-set cacheDir");
    let (linker, _) = run(&fx, env, &["config", "get", "node-linker"]);
    assert_eq!(
        linker.trim(),
        "hoisted",
        "config get returned the embedder default over an env-set nodeLinker"
    );

    let (listed, _) = run(&fx, env, &["config", "list"]);
    assert!(
        listed
            .lines()
            .any(|line| line == format!("cache-dir={dir}")),
        "config list omitted the env-set cacheDir:\n{listed}"
    );

    for scope in ["--local", "--global"] {
        let (scoped, _) = run(&fx, env, &["config", "get", "cache-dir", scope]);
        assert_eq!(
            scoped.trim(),
            "undefined",
            "{scope} asks about a file and must not answer from the environment"
        );
    }

    let (ambient, _) = run(&fx, &[("CI", "true")], &["config", "list"]);
    assert!(
        !ambient.lines().any(|line| line.starts_with("ci=")),
        "an ambient variable became a config row:\n{ambient}"
    );
}

/// A setting can be fed by an ambient variable OR a config-carrying one —
/// `proxy` declares `HTTP_PROXY`, `http_proxy` AND `npm_config_http_proxy`.
/// Only the config-carrying spelling is configuration, and BOTH ambient
/// spellings have to be excluded: a deny-list naming just the uppercase forms
/// hides one and reports the other, which is what an allow-list avoids.
#[test]
fn only_config_carrying_env_vars_are_configuration() {
    let fx = fixture("config-env-ambient");

    for spelling in ["HTTP_PROXY", "http_proxy"] {
        let (listed, _) = run(
            &fx,
            &[(spelling, "http://ambient.example")],
            &["config", "list"],
        );
        assert!(
            !listed.lines().any(|line| line.starts_with("http-proxy=")),
            "{spelling} is ambient environment and must not read as configuration:\n{listed}"
        );
    }

    let (listed, _) = run(
        &fx,
        &[("npm_config_http_proxy", "http://config.example")],
        &["config", "list"],
    );
    assert!(
        listed
            .lines()
            .any(|line| line == "http-proxy=http://config.example"),
        "control: the config-carrying spelling of the same setting must be \
         reported, or the exclusions above prove nothing:\n{listed}"
    );
}

//! What `nub config list --all` renders in the DEFAULT column, through the
//! real binary.
//!
//! That column is the shared engine table's `default` field — a build-time
//! constant. A directory default written as a literal is therefore baked with
//! the ENGINE's namespace and printed verbatim by whichever host embeds the
//! engine, so nub advertised its cache default as `$XDG_CACHE_HOME/aube`, a
//! directory nub never uses. The defaults now carry namespace tokens that
//! resolve against the active embedder at display time.
//!
//! Offline: `config list` reads config files and the static table, nothing else.
//!
//! These rows deliberately do NOT reuse `pm_publish_store_config`'s harness,
//! which asserts brand-cleanliness over ALL output on every spawn. The `--all`
//! listing cannot satisfy that yet for a reason unrelated to the default
//! column: `aubeNoAutoInstall` is an engine-branded setting NAME that nub
//! genuinely reads. Scoping to the rows under test keeps this file honest
//! about what it proves.

use std::path::PathBuf;
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

/// `config list --all` in a throwaway project with every config root pinned to
/// the fixture, so no host `.npmrc` can supply a value where a default is
/// expected.
fn list_all(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "nub-cfg-defaults-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    let home = root.join("home");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();

    let mut cmd = Command::new(nub_binary());
    cmd.args(["config", "list", "--all"])
        .current_dir(&project)
        .env("NUB_SELF_SHIM", "0")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"));
    for key in [
        "npm_config_cache_dir",
        "NPM_CONFIG_CACHE_DIR",
        "NUB_CACHE_DIR",
    ] {
        cmd.env_remove(key);
    }
    let out = cmd.output().expect("failed to spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(
        out.status.code().unwrap_or(-1),
        0,
        "`nub config list --all` failed\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

fn row<'a>(listing: &'a str, key: &str) -> &'a str {
    listing
        .lines()
        .find(|line| line.starts_with(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no `{key}` row in `config list --all`:\n{listing}"))
}

/// The cache default names nub's own namespace. This is the row that carried
/// the engine's name, so it is also the control: the assertion below fails on
/// the literal that shipped before the tokens existed.
#[test]
fn the_cache_default_names_nubs_namespace() {
    let listing = list_all("cache");
    let cache = row(&listing, "cache-dir");
    assert!(
        cache.contains("nub/pm"),
        "the cache default must name nub's cache namespace: {cache}"
    );
    assert!(
        !cache.to_lowercase().contains("aube"),
        "the cache default still names the engine: {cache}"
    );
}

/// No KNOWN token survives into the listing — the substitution ran for every
/// row, not just the one asserted above.
///
/// This deliberately proves less than it might: a MISSPELLED token is invisible
/// here, because `render_namespaces` leaves it untouched and the result looks
/// like ordinary prose. That case is caught at its source by
/// `aube_settings::meta`'s audit of the raw defaults against the substitution
/// table, which is where a typo can actually be told apart from text.
#[test]
fn no_known_token_survives_into_the_listing() {
    let listing = list_all("tokens");
    let leaked: Vec<&str> = listing
        .lines()
        .filter(|line| line.contains("{cache_namespace}") || line.contains("{data_namespace}"))
        .collect();
    assert!(
        leaked.is_empty(),
        "unsubstituted namespace tokens in the listing: {leaked:?}"
    );
}

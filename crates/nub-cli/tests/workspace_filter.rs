//! Workspace commands scoped by `-r` / `--filter`, against pnpm's behavior:
//! which projects a recursive `remove` edits, and when a filtered `install`
//! is already up to date. Offline: the only dependencies are local
//! directories.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

fn tmpdir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-wsfilter-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A workspace at `<tmp>/proj`: a root and the given members under
/// `packages/`, each a `(name, dependencies)` pair, plus a local package
/// `dep` at the root for `file:` specs.
fn workspace(
    tag: &str,
    root_deps: serde_json::Value,
    members: &[(&str, serde_json::Value)],
) -> PathBuf {
    let dir = tmpdir(tag).join("proj");
    let write = |path: PathBuf, value: serde_json::Value| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    };
    write(
        dir.join("package.json"),
        serde_json::json!({ "name": "root", "private": true, "dependencies": root_deps }),
    );
    for (name, deps) in members {
        write(
            dir.join("packages").join(name).join("package.json"),
            serde_json::json!({ "name": name, "version": "1.0.0", "dependencies": deps }),
        );
    }
    write(
        dir.join("dep/package.json"),
        serde_json::json!({ "name": "dep", "version": "1.0.0" }),
    );
    std::fs::write(
        dir.join("pnpm-workspace.yaml"),
        "packages:\n  - packages/*\n",
    )
    .unwrap();
    dir
}

/// Run `nub <args>` in `dir`; panics on failure, returns stdout + stderr.
fn nub(dir: &Path, args: &[&str]) -> String {
    let home = dir.parent().unwrap();
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env_remove("NODE_COMPAT")
        .env_remove("CI")
        .output()
        .expect("failed to spawn nub");
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "nub {args:?} failed:\n{output}");
    output
}

fn declares_dep(manifest: &Path) -> bool {
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(manifest).unwrap()).unwrap();
    manifest["dependencies"].get("dep").is_some()
}

#[test]
fn recursive_remove_edits_the_workspace_root_and_a_filtered_one_does_not() {
    let dir = workspace(
        "remove",
        serde_json::json!({ "dep": "file:./dep" }),
        &[
            ("a", serde_json::json!({ "dep": "file:../../dep" })),
            ("b", serde_json::json!({ "dep": "file:../../dep" })),
        ],
    );
    nub(&dir, &["install"]);
    nub(&dir, &["remove", "--filter", "a", "dep"]);
    assert!(
        declares_dep(&dir.join("package.json"))
            && declares_dep(&dir.join("packages/b/package.json")),
        "a filter naming one member leaves the root and the others alone"
    );
    assert!(!declares_dep(&dir.join("packages/a/package.json")));
    nub(&dir, &["remove", "-r", "--filter", "!root", "dep"]);
    assert!(
        declares_dep(&dir.join("package.json")),
        "an exclusion naming the root keeps it out of a recursive remove"
    );
    assert!(!declares_dep(&dir.join("packages/b/package.json")));
    std::fs::write(
        dir.join("packages/b/package.json"),
        r#"{ "name": "b", "version": "1.0.0", "dependencies": { "dep": "file:../../dep" } }"#,
    )
    .unwrap();
    nub(&dir, &["remove", "-r", "dep"]);
    assert!(
        !declares_dep(&dir.join("package.json")),
        "`remove -r` takes the dependency out of the root's manifest too"
    );
    assert!(!declares_dep(&dir.join("packages/b/package.json")));
}

#[test]
fn a_repeated_filtered_install_is_up_to_date_and_an_unfiltered_one_after_it_is_not() {
    let dir = workspace(
        "filtered",
        serde_json::json!({}),
        &[
            ("a", serde_json::json!({ "dep": "file:../../dep" })),
            ("b", serde_json::json!({ "dep": "file:../../dep" })),
        ],
    );
    // The root's postinstall marks every install that did any work; the
    // warm path, which finds the tree up to date, runs no hooks.
    let manifest = dir.join("package.json");
    let mut root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
    root["scripts"] = serde_json::json!({
        "postinstall": "node -e \"require('fs').appendFileSync('../installs.log', 'x')\""
    });
    std::fs::write(&manifest, root.to_string()).unwrap();
    let installs =
        || std::fs::read_to_string(dir.join("../installs.log")).map_or(0, |log| log.len());

    nub(&dir, &["install", "--filter", "b"]);
    assert_eq!(installs(), 1);
    assert!(
        !dir.join("packages/a/node_modules/dep").exists(),
        "the filtered install leaves `a` unlinked"
    );
    nub(&dir, &["install", "--filter", "b"]);
    assert_eq!(installs(), 1, "the same filter again has nothing to do");
    nub(&dir, &["install"]);
    assert_eq!(installs(), 2, "an unfiltered install is not up to date");
    assert!(
        dir.join("packages/a/node_modules/dep").exists(),
        "it links what the filtered one skipped"
    );
    nub(&dir, &["install", "--filter", "a"]);
    assert_eq!(
        installs(),
        2,
        "a filter inside a full install has nothing to do"
    );
}

#[test]
fn a_filtered_install_after_a_root_only_install_links_the_member() {
    // pnpm reads the setting from either file.
    let forms: [(&str, &str); 2] = [
        (".npmrc", "recursive-install=false\n"),
        (
            "pnpm-workspace.yaml",
            "packages:\n  - packages/*\nrecursiveInstall: false\n",
        ),
    ];
    for (file, setting) in forms {
        let dir = workspace(
            "nonrecursive",
            serde_json::json!({ "dep": "file:./dep" }),
            &[("a", serde_json::json!({ "dep": "file:../../dep" }))],
        );
        std::fs::write(dir.join(file), setting).unwrap();
        nub(&dir, &["install"]);
        assert!(
            !dir.join("packages/a/node_modules/dep").exists(),
            "`recursiveInstall: false` in {file} links only the root"
        );
        nub(&dir, &["install"]);
        nub(&dir, &["install", "--filter", "a"]);
        assert!(
            dir.join("packages/a/node_modules/dep").exists(),
            "a root-only install does not make a filtered one up to date ({file})"
        );
    }
}

#[test]
fn a_state_without_a_coverage_record_counts_as_covering_the_root_only() {
    let dir = workspace(
        "legacy-state",
        serde_json::json!({ "dep": "file:./dep" }),
        &[("a", serde_json::json!({ "dep": "file:../../dep" }))],
    );
    std::fs::write(dir.join(".npmrc"), "recursive-install=false\n").unwrap();
    let manifest = dir.join("package.json");
    let mut root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
    root["scripts"] = serde_json::json!({
        "postinstall": "node -e \"require('fs').appendFileSync('../installs.log', 'x')\""
    });
    std::fs::write(&manifest, root.to_string()).unwrap();
    let installs =
        || std::fs::read_to_string(dir.join("../installs.log")).map_or(0, |log| log.len());

    nub(&dir, &["install"]);
    // What an install from before the coverage record leaves behind.
    let state_dir = dir.join("node_modules/.store/.nub-state");
    for file in ["state.json", "fresh.json"] {
        let path = state_dir.join(file);
        let mut state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let fields = state.as_object_mut().unwrap();
        assert!(
            fields.remove("workspace_selection_recorded").is_some(),
            "{file} records its coverage"
        );
        fields.remove("workspace_selection");
        std::fs::write(&path, state.to_string()).unwrap();
    }
    nub(&dir, &["install"]);
    assert_eq!(installs(), 1, "an unfiltered install is still up to date");
    nub(&dir, &["install", "--filter", "a"]);
    assert!(
        dir.join("packages/a/node_modules/dep").exists(),
        "a filtered install does not trust the unrecorded coverage"
    );
}

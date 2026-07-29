//! `--preserve-symlinks` is INERT in a hoisted layout — the same fixture resolves identically
//! with and without it.
//!
//! WHY THIS MATTERS. `--preserve-symlinks` (with `--preserve-symlinks-main`) is the only lever
//! that stops Node calling `realpathSync` at all, and under the Windows build jail that is the
//! one operation the OS refuses: `realpathSync` opens each ancestor directory as a TARGET and a
//! LowBox token has no access to `C:\` or `C:\Users`. Ordinary file I/O on granted leaves works
//! unprivileged, so the flag is the difference between a jail that runs lifecycle scripts and one
//! that cannot start them.
//!
//! It was disqualified because under nub's DEFAULT `NodeLinker::Isolated` layout it silently binds
//! the WRONG version — a package whose private dependency is `bar@2.0.0` gets the unrelated
//! top-level `bar@1.0.0`, exit 0, no error. That is measured next door in
//! `preserve_symlinks_isolated_layout`, and it is still true.
//!
//! The reasoning this test checks is that the hazard is a property of the LAYOUT rather than of
//! the flag: `--preserve-symlinks` makes a dependency resolve under its LINK path, so it can only
//! change an answer where there is a link to preserve. `NodeLinker::Hoisted` writes real package
//! directories (reflink/hardlink/copy — `aube-linker/src/hoisted.rs`) and nests a conflicting
//! version under the parent that requires it, so the module graph contains no symlinks for the
//! flag to act on and Node's ordinary parent-directory walk finds the nested copy either way.
//!
//! Reasoning is not evidence, which is why this is a differential on the SAME fixture rather than
//! an argument: the assertion is that both arms return the nested version, and specifically that
//! the flagged arm does not return the top-level one. A test that only asserted "the flagged arm
//! returned something" would pass on exactly the bug it exists to exclude.
//!
//! The fixture also ASSERTS ITS OWN PREMISE — that the tree it built contains no symlinks. A
//! hoisted fixture that accidentally contained one would make the flag inert-looking for the
//! wrong reason, and the whole conclusion rests on that being true.
//!
//! SCOPE, stated because the inertness is not unconditional: a hoisted install still symlinks
//! `link:`/`file:` dependencies and workspace members (`materialize_hoisted_node`), so the flag is
//! inert for the registry dependencies whose lifecycle scripts the build jail confines, NOT for a
//! workspace member's own scripts. Those are not routed through the jail today.

use std::path::Path;
use std::process::Command;

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// The npm-classic shape: the majority version at the top, the conflicting one NESTED under the
/// package that needs it. Every entry is a real directory — that is the whole point.
fn build_hoisted_fixture(root: &Path) {
    let nm = root.join("node_modules");

    // Top level: the version an unrelated consumer sees.
    write(
        &nm.join("bar").join("index.js"),
        "module.exports={v:'1.0.0'};\n",
    );
    write(
        &nm.join("bar").join("package.json"),
        "{\"name\":\"bar\",\"version\":\"1.0.0\"}\n",
    );

    // `foo` and, nested beneath it, the version `foo` actually requires.
    let foo_pkg = nm.join("foo");
    write(
        &foo_pkg.join("index.js"),
        "module.exports={barVersion:require('bar').v};\n",
    );
    write(
        &foo_pkg.join("package.json"),
        "{\"name\":\"foo\",\"version\":\"1.0.0\",\"main\":\"index.js\"}\n",
    );
    write(
        &foo_pkg.join("node_modules").join("bar").join("index.js"),
        "module.exports={v:'2.0.0'};\n",
    );
    write(
        &foo_pkg
            .join("node_modules")
            .join("bar")
            .join("package.json"),
        "{\"name\":\"bar\",\"version\":\"2.0.0\"}\n",
    );

    write(
        &root.join("script.js"),
        "try{console.log('bar@'+require('foo').barVersion);}\
         catch(e){console.log('threw:'+e.code);}\n",
    );
}

/// Every path under `dir`, and whether any of them is a symlink. The fixture's premise.
fn contains_symlink(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if meta.file_type().is_symlink() {
            return true;
        }
        if meta.is_dir() && contains_symlink(&entry.path()) {
            return true;
        }
    }
    false
}

fn resolve_with(root: &Path, node_options: Option<&str>) -> String {
    let mut cmd = Command::new("node");
    cmd.arg("script.js").current_dir(root);
    match node_options {
        Some(v) => cmd.env("NODE_OPTIONS", v),
        // Cleared, not left ambient: an inherited NODE_OPTIONS would make the control arm
        // measure whatever the developer's shell happens to carry.
        None => cmd.env_remove("NODE_OPTIONS"),
    };
    let out = cmd.output().expect("run node");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn preserve_symlinks_is_inert_in_a_hoisted_layout() {
    if Command::new("node").arg("-v").output().is_err() {
        eprintln!("no node on PATH — skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    build_hoisted_fixture(root);

    assert!(
        !contains_symlink(&root.join("node_modules")),
        "the fixture's premise: a hoisted tree has no symlinks for the flag to act on. \
         One here would make the flag look inert for the wrong reason."
    );

    let without = resolve_with(root, None);
    let with = resolve_with(root, Some("--preserve-symlinks-main --preserve-symlinks"));

    assert_eq!(
        without, "bar@2.0.0",
        "control: `foo` must reach its own NESTED dependency (got {without:?})"
    );
    assert_eq!(
        with, "bar@2.0.0",
        "the claim under test: with no symlink to preserve, the flag cannot move the answer. \
         `bar@1.0.0` here would mean the hoisted layout has the same silent wrong-version hazard \
         as the isolated one and the Windows realpath repair is disqualified again (got {with:?})"
    );
    assert_eq!(
        with, without,
        "inert means IDENTICAL, not merely non-empty — an arm that returned something plausible \
         but different is the bug this test exists to exclude"
    );
}

/// A package that requires the TOP-LEVEL copy must still get that one. The sibling assertion:
/// showing the nested version resolves proves the flag does not break nesting, and this proves it
/// does not over-reach in the other direction either.
#[test]
fn a_hoisted_top_level_dependency_still_resolves_under_the_flag() {
    if Command::new("node").arg("-v").output().is_err() {
        eprintln!("no node on PATH — skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    build_hoisted_fixture(root);
    write(
        &root.join("script.js"),
        "try{console.log('bar@'+require('bar').v);}catch(e){console.log('threw:'+e.code);}\n",
    );

    let without = resolve_with(root, None);
    let with = resolve_with(root, Some("--preserve-symlinks-main --preserve-symlinks"));
    assert_eq!(without, "bar@1.0.0", "control (got {without:?})");
    assert_eq!(with, "bar@1.0.0", "got {with:?}");
}

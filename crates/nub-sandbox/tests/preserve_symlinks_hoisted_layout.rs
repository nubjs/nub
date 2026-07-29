//! `--preserve-symlinks` is inert in a HOISTED layout and disqualifying in an ISOLATED one — the
//! hazard belongs to the LAYOUT. Recorded because the fact is durable; the flag is not usable.
//!
//! READ THIS FIRST: THE FLAG IS OFF THE TABLE. This test was written to support using
//! `--preserve-symlinks` under a hoisted layout to stop Node calling `realpathSync`, which is the
//! one operation the Windows build jail's AppContainer cannot perform. That argument is REFUTED
//! and the conclusion it was written for is withdrawn:
//!
//!  - A hoisted install still symlinks WORKSPACE MEMBERS and `link:`/`file:` dependencies
//!    (`aube-linker/src/hoisted.rs`, `materialize_hoisted_node`). In any monorepo there are
//!    therefore symlinks for the flag to act on, so "semantically inert under hoisted" is false in
//!    general — it is true only of the registry-dependency subtree this fixture models. The scope
//!    paragraph below said as much while the conclusion ignored it.
//!  - Independently: a process-wide flag that changes module resolution for every module, to route
//!    around one failing syscall, is the wrong shape of fix. It is not additive.
//!
//! WHAT THE MEASUREMENT IS STILL GOOD FOR, and why it is kept rather than deleted: it pins WHERE
//! the wrong-version hazard comes from. `preserve_symlinks_isolated_layout` next door shows the
//! flag silently binding `bar@1.0.0` where `bar@2.0.0` was required; this shows the identical
//! fixture resolving correctly once the links are gone. Together they say the hazard is a property
//! of the layout and not of the flag — which is the fact a future reader needs, and which neither
//! test alone establishes. If anyone revisits this, the blocker to clear is workspace symlinks,
//! not the resolution semantics.
//!
//! The fixture ASSERTS ITS OWN PREMISE — that the tree it built contains no symlinks — because a
//! stray one would make the flag look inert for the wrong reason.

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

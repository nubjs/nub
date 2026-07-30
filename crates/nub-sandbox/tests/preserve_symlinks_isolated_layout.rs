//! `--preserve-symlinks` silently resolves the WRONG package under nub's default layout.
//!
//! This is the measurement that disqualified the Windows build jail's realpath repair. Under
//! the AppContainer, Node's `realpathSync` dies on `EPERM: lstat 'C:\'` for every absolute
//! `require()`, and `--preserve-symlinks` is the only lever that stops Node calling realpath at
//! all. It cannot be used, and the reason is not a Windows fact — it is a resolution fact, so
//! it is measured here on every platform rather than behind a Windows-only probe.
//!
//! nub's DEFAULT node-linker is `Isolated` (`aube-linker/src/lib.rs`), which materialises each
//! package in its own store cell under `node_modules/.aube/<dep_path>/node_modules/<name>` and
//! wires dependencies as symlinks. With `--preserve-symlinks`, a dependency resolves under its
//! LINK path, so the parent-directory walk from `node_modules/<pkg>` never enters the store
//! cell holding that package's private dependencies and reaches the project's top-level
//! `node_modules` instead — where an unrelated version of the same name may sit.
//!
//! The failure is SILENT, which is what makes it disqualifying rather than merely awkward: a
//! lifecycle script builds against the wrong dependency version and exits 0. A jail in which
//! scripts fail loudly is strictly better than one in which they succeed wrongly.
//!
//! (Written as a differential — the same fixture resolved both ways — because "resolves
//! bar@1.0.0" is only meaningful next to "and resolves bar@2.0.0 without the flag".)

use std::path::Path;
use std::process::Command;

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

#[cfg(unix)]
fn link(original: &str, link: &Path) {
    std::os::unix::fs::symlink(original, link).unwrap();
}

#[cfg(windows)]
fn link(original: &str, link: &Path) {
    // Directory symlinks need Developer Mode or elevation on Windows; a junction does not, but
    // it needs an absolute target. Resolve against the link's own parent so the fixture keeps
    // the same shape either way.
    let target = link.parent().unwrap().join(original);
    if std::os::windows::fs::symlink_dir(original, link).is_err() {
        let status = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(&target)
            .status()
            .expect("run mklink");
        assert!(status.success(), "could not link {}", link.display());
    }
}

/// `foo`'s private dependency is `bar@2.0.0`, a sibling inside its own store cell. An
/// unrelated `bar@1.0.0` sits at the project's top level, which is where a link-path walk
/// lands.
fn build_isolated_fixture(root: &Path) {
    let nm = root.join("node_modules");
    let store = nm.join(".aube");
    for (dep, version) in [("bar@2.0.0", "2.0.0"), ("bar@1.0.0", "1.0.0")] {
        let cell = store.join(dep).join("node_modules").join("bar");
        write(
            &cell.join("index.js"),
            &format!("module.exports={{v:'{version}'}};\n"),
        );
        write(
            &cell.join("package.json"),
            &format!("{{\"name\":\"bar\",\"version\":\"{version}\"}}\n"),
        );
    }
    let foo_dir = store.join("foo@1.0.0").join("node_modules").join("foo");
    write(
        &foo_dir.join("index.js"),
        "module.exports={barVersion:require('bar').v};\n",
    );
    write(
        &foo_dir.join("package.json"),
        "{\"name\":\"foo\",\"version\":\"1.0.0\",\"main\":\"index.js\"}\n",
    );

    link(
        "../../bar@2.0.0/node_modules/bar",
        &store.join("foo@1.0.0").join("node_modules").join("bar"),
    );
    link(".aube/foo@1.0.0/node_modules/foo", &nm.join("foo"));
    link(".aube/bar@1.0.0/node_modules/bar", &nm.join("bar"));

    write(
        &root.join("script.js"),
        "try{console.log('bar@'+require('foo').barVersion);}\
         catch(e){console.log('threw:'+e.code);}\n",
    );
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
fn preserve_symlinks_silently_resolves_the_wrong_version_in_an_isolated_layout() {
    if Command::new("node").arg("-v").output().is_err() {
        eprintln!("no node on PATH — skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    build_isolated_fixture(root);

    let without = resolve_with(root, None);
    let with = resolve_with(root, Some("--preserve-symlinks-main --preserve-symlinks"));

    assert_eq!(
        without, "bar@2.0.0",
        "control: without the flag a package must reach its OWN private dependency, \
         not the unrelated top-level copy (got {without:?})"
    );
    assert_eq!(
        with, "bar@1.0.0",
        "the disqualifying behaviour: with --preserve-symlinks the walk leaves the store cell \
         and silently binds the top-level version instead (got {with:?}). If this ever stops \
         reproducing, the Windows build jail's realpath repair becomes available again — see \
         nub_sandbox::windows_realpath_node_options."
    );
    assert!(
        !with.starts_with("threw:"),
        "the failure must be SILENT to be worth this test; a throw would be recoverable"
    );
}

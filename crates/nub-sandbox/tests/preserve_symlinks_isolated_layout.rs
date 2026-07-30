//! Which of the two preserve-symlinks flags carries the silent wrong-version hazard, and when.
//!
//! This file exists because the Windows build jail's realpath repair turns on one question: an
//! AppContainer cannot perform `realpathSync` (it opens every ancestor as a TARGET, and
//! bypass-traverse exempts intermediate components only), and the only lever that stops Node
//! calling realpath is a preserve-symlinks flag. There are two of them, they are NOT
//! interchangeable, and "the flags are disqualified" was too coarse a statement to act on.
//!
//! The attribution, measured below on nub's default `Isolated` layout
//! (`aube-linker/src/lib.rs`, which materialises each package in its own store cell under
//! `node_modules/.aube/<dep_path>/node_modules/<name>` and wires dependencies as symlinks):
//!
//! | flags | entry = a real file | entry reached THROUGH a link |
//! | --- | --- | --- |
//! | none (control) | `bar@2.0.0` correct | `bar@2.0.0` correct |
//! | `--preserve-symlinks-main` | `bar@2.0.0` correct | **`bar@1.0.0` WRONG** |
//! | `--preserve-symlinks` | **`bar@1.0.0` WRONG** | `bar@1.0.0` WRONG |
//!
//! So the hazard is not a property of one flag. It is a property of whether a SYMLINK sits in
//! the path the resolver walks, and each flag puts one there by a different route:
//! `--preserve-symlinks` leaves every dependency on its link path, while
//! `--preserve-symlinks-main` leaves the ENTRY POINT on its link path — and the entry point's
//! resolved path is the root of the `node_modules` walk, so it moves dependency resolution
//! transitively. Either way the walk from `node_modules/<pkg>` skips the store cell holding that
//! package's private dependencies and lands on the project's top-level `node_modules`, where an
//! unrelated version of the same name may sit.
//!
//! The failure is SILENT, which is what makes it disqualifying rather than awkward: a lifecycle
//! script builds against the wrong dependency version and exits 0. A jail in which scripts fail
//! loudly is strictly better than one in which they succeed wrongly.
//!
//! WHAT THIS MEANS FOR THE JAIL, so nobody reads the table as better news than it is: main-only
//! is clean on the axis measured here, and under nub it never meets the link-path shape either —
//! `materialized_pkg_dir` (`install/bin_linking.rs`) hands a dep's lifecycle script a store-cell
//! REAL path for its `current_dir`, and gives the Windows `.cmd` bin shims real-path targets too,
//! so no lifecycle entry point arrives through a symlink. It is nevertheless NOT usable, for a
//! reason this file cannot see: `realpath_unavailable_resolution` next door measures main-only
//! leaving every `require()` dead, because Node realpaths every non-main resolution unless
//! `--preserve-symlinks` is set. Read the two files together before concluding anything about
//! the repair.
//!
//! Measured on every platform rather than behind a Windows-only probe: the resolution semantics
//! are Node's, not Windows's, and the fixture needs no jail.

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

/// `foo`'s private dependency is `bar@2.0.0`, a sibling inside its own store cell. An unrelated
/// `bar@1.0.0` sits at the project's top level, which is where a link-path walk lands. Two entry
/// points are written so one fixture answers both questions: `script.js` at the project root (a
/// real file) and `build.js` INSIDE `foo`'s cell (reachable through `node_modules/foo`, a link).
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
    // The shape a dep's own lifecycle script has: a file inside the store cell, requiring the
    // cell's private `bar`.
    write(
        &foo_cell.join("build.js"),
        "try{console.log('bar@'+require('bar').v);}\
         catch(e){console.log('threw:'+e.code);}\n",
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

/// Run `entry` from `root` and return its one line of stdout. `None` CLEARS `NODE_OPTIONS`
/// rather than leaving it ambient: an inherited value would make the control arm measure
/// whatever the developer's shell happens to carry.
fn resolve_with(root: &Path, entry: &str, node_options: Option<&str>) -> String {
    let mut cmd = Command::new("node");
    cmd.arg(entry).current_dir(root);
    match node_options {
        Some(v) => cmd.env("NODE_OPTIONS", v),
        None => cmd.env_remove("NODE_OPTIONS"),
    };
    let out = cmd.output().expect("run node");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn have_node() -> bool {
    Command::new("node").arg("-v").output().is_ok()
}

/// Four arms, one fixture, entry point a REAL file. The two single-flag arms are what make this
/// an attribution rather than a restatement: without them, "the flags bind the wrong version" is
/// a fact about the pair that says nothing about either one — and the whole shipping question is
/// whether the main flag can be used without the other.
#[test]
fn the_wrong_version_hazard_follows_the_non_main_flag_when_the_entry_is_real() {
    if !have_node() {
        eprintln!("no node on PATH — skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    build_isolated_fixture(root);

    let control = resolve_with(root, "script.js", None);
    let main_only = resolve_with(root, "script.js", Some("--preserve-symlinks-main"));
    let non_main = resolve_with(root, "script.js", Some("--preserve-symlinks"));
    let both = resolve_with(
        root,
        "script.js",
        Some("--preserve-symlinks-main --preserve-symlinks"),
    );

    assert_eq!(
        control, "bar@2.0.0",
        "control: with no flag a package must reach its OWN private dependency, not the \
         unrelated top-level copy (got {control:?})"
    );
    assert_eq!(
        main_only, "bar@2.0.0",
        "--preserve-symlinks-main governs the ENTRY POINT only, and this entry is a real file, \
         so it must not move dependency resolution at all (got {main_only:?})"
    );
    assert_eq!(
        non_main, "bar@1.0.0",
        "the hazard, attributed: --preserve-symlinks ALONE leaves each dependency on its link \
         path, so the walk leaves the store cell and silently binds the top-level version \
         (got {non_main:?})"
    );
    assert_eq!(
        both, "bar@1.0.0",
        "the pair behaves exactly as the non-main flag alone does — the hazard is that flag's, \
         not an interaction between the two (got {both:?})"
    );
    assert!(
        !non_main.starts_with("threw:"),
        "the failure must be SILENT to be worth this test; a throw would be recoverable \
         (got {non_main:?})"
    );
}

/// The other route to the same wrong answer, and the reason main-only cannot be called safe
/// without naming its precondition: when the entry point is reached THROUGH a link, the main flag
/// alone binds the wrong version, because the entry's resolved path is where the `node_modules`
/// walk starts.
///
/// nub does not produce this shape — `materialized_pkg_dir` gives a dep's lifecycle script a
/// store-cell real path for `current_dir` and gives the `.cmd` shims real-path targets. That is a
/// property of the CALLER though, not of the flag, so the boundary of the claim is measured here
/// rather than left implicit for the next reader to rediscover.
#[test]
fn preserve_symlinks_main_binds_the_wrong_version_when_the_entry_is_reached_through_a_link() {
    if !have_node() {
        eprintln!("no node on PATH — skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    build_isolated_fixture(root);

    // `node_modules/foo` is a link into the store cell; the file itself lives in the cell.
    let entry = Path::new("node_modules")
        .join("foo")
        .join("build.js")
        .to_string_lossy()
        .into_owned();

    let control = resolve_with(root, &entry, None);
    let main_only = resolve_with(root, &entry, Some("--preserve-symlinks-main"));

    assert_eq!(
        control, "bar@2.0.0",
        "control: Node realpaths the main path, so the walk starts inside the store cell and \
         finds the cell's private dependency (got {control:?})"
    );
    assert_eq!(
        main_only, "bar@1.0.0",
        "main-only is NOT unconditionally safe: with the realpath skipped the entry keeps its \
         LINK path, so the walk starts at node_modules/foo instead of the store cell and the \
         top-level bar@1.0.0 answers silently (got {main_only:?}). If this stops reproducing, \
         re-derive this file's module-doc table before relying on it."
    );

    // THE THIRD ARM, and the reason this file is the right home for it: the repair that ships
    // instead of the flag is measured on the SAME fixture, one line below the hazard it must not
    // reintroduce. `realpath_shim_node_options` tolerates a refused `lstat` only for a strict
    // ancestor of a granted root and still reads every symlink at or below one, so the store-cell
    // link resolves and the private dependency wins. If this ever answers `bar@1.0.0` the repair
    // has become the flag and must not ship — the full differential, including the simulated
    // jail that makes the tolerance actually fire, is `realpath_shim_semantics`.
    let repaired = resolve_with_shim(root);
    assert_eq!(
        repaired, "bar@2.0.0",
        "the shipped realpath repair must resolve like the control, not like the flag \
         (got {repaired:?})"
    );
}

/// The same fixture under the shipping realpath repair. The force seam activates the shim off
/// Windows; the root is passed as a jail anchor because that is what scopes its tolerance rule.
fn resolve_with_shim(root: &Path) -> String {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let out = Command::new("node")
        .arg("script.js")
        .current_dir(root)
        .env(
            "NODE_OPTIONS",
            nub_sandbox::realpath_shim_node_options(std::slice::from_ref(&canonical)),
        )
        .env("__NUB_JAIL_REALPATH_SHIM_FORCE", &canonical)
        .output()
        .expect("run node");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

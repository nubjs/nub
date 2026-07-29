//! A failing dependency build fails the install; it does not tear its SIBLINGS
//! down mid-write.
//!
//! The parallel lifecycle pass used to return from its `join_next` loop on the
//! first error, dropping the `JoinSet` and aborting every outstanding task. That
//! made a batch install's outcome depend on which UNRELATED package happened to
//! fail first: measured on a 50-package corpus shard, 25 of 27 packages that
//! produced no build artifact in-batch produced one when run as the batch's only
//! package. It also bought nothing — the abort SIGKILLs each sibling's shell and
//! nothing else, so its `node-gyp`/`make`/`cc` reparent to init and keep writing.
//!
//! WHY `sandbox: false`. This has to run on the UNCONFINED lifecycle path. When
//! nub's build jail confines the spawn, aube hands it to the embedder hook via
//! `spawn_blocking`, which is not cancellable — the script runs to completion
//! whatever the `JoinSet` does, so a confined fixture passes vacuously and
//! proves nothing about the abort. `dependenciesMeta.<pkg>.sandbox = false` is
//! the per-package opt-out that reproduces the corpus study's jail-off arm.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// The one package dir the store materialized for `name`.
fn store_package_dir(root: &Path, name: &str) -> Option<PathBuf> {
    root.join("node_modules/.store")
        .read_dir()
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path().join("node_modules").join(name))
        .find(|p| p.exists())
}

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // <profile>/
    path.push("nub");
    path
}

/// Store and cache are per-fixture, never the dev box's real ones. Load-bearing
/// beyond ordinary hygiene: a warm side-effects cache would RESTORE a previously
/// built package dir — marker included — without the script ever running, which
/// would make the assertion below pass while proving nothing.
fn run(dir: &Path, args: &[&str]) -> i32 {
    Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("XDG_CACHE_HOME", dir.join("xdg-cache"))
        .output()
        .expect("failed to spawn nub")
        .status
        .code()
        .unwrap_or(-1)
}

#[test]
fn a_failing_dep_build_does_not_abort_its_siblings() {
    let dir = std::env::temp_dir().join(format!("nub-drain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let deps = dir.join("deps");
    std::fs::create_dir_all(deps.join("dep-fails")).unwrap();
    std::fs::create_dir_all(deps.join("dep-slow")).unwrap();

    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"drain-root","version":"1.0.0",
            "dependencies":{"dep-fails":"file:./deps/dep-fails","dep-slow":"file:./deps/dep-slow"},
            "dependenciesMeta":{"dep-fails":{"sandbox":false},"dep-slow":{"sandbox":false}}}"#,
    )
    .unwrap();
    // Pinned so both jobs are in flight together on any core count. At
    // concurrency 1 they serialize and the fixture would test nothing.
    std::fs::write(dir.join(".npmrc"), "child-concurrency=8\n").unwrap();
    std::fs::write(
        deps.join("dep-fails/package.json"),
        r#"{"name":"dep-fails","version":"1.0.0","scripts":{"postinstall":"sleep 0.4; exit 1"}}"#,
    )
    .unwrap();
    // The marker is written by the CHILD, LAST, after a sleep that outlasts the
    // sibling's failure — so its presence can only mean this script reached its
    // own end, and never that the runner reported something about it. Written
    // into the package's own cwd because the fixture must keep working if the
    // jail is ever armed for it.
    std::fs::write(
        deps.join("dep-slow/package.json"),
        r#"{"name":"dep-slow","version":"1.0.0","scripts":{"postinstall":"sleep 2.5; printf ran > ./RAN-MARKER"}}"#,
    )
    .unwrap();

    assert_eq!(run(&dir, &["install"]), 0, "install should link both deps");
    let approve = run(&dir, &["approve-builds", "--all"]);

    let found = store_package_dir(&dir, "dep-slow").is_some_and(|d| d.join("RAN-MARKER").exists());
    let _ = std::fs::remove_dir_all(&dir);

    // Parity half: the install still FAILS. Draining is not tolerating.
    assert_eq!(
        approve, 1,
        "a failing dependency build must still fail the command (npm/pnpm parity)"
    );
    assert!(
        found,
        "dep-slow never reached the end of its postinstall — a sibling package's \
         unrelated failure aborted it mid-build, leaving a half-written package dir"
    );
}

/// Defect A at the install level: once the command has returned, nothing a
/// lifecycle script spawned may still be writing into the package directory.
///
/// The script SUCCEEDS here — this is not the failure path. It backgrounds a
/// writer the way `node-gyp` backgrounds `make`, and exits. Before the runner
/// reaped process groups, that writer reparented to init and kept appending to a
/// package dir the installer had already reported on; the corpus study saw up to
/// 11,407 lines arrive after the command returned.
#[test]
fn nothing_keeps_writing_after_the_install_command_returns() {
    let dir = std::env::temp_dir().join(format!("nub-orphan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let dep = dir.join("deps/dep-orphan");
    std::fs::create_dir_all(&dep).unwrap();

    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"orphan-root","version":"1.0.0",
            "dependencies":{"dep-orphan":"file:./deps/dep-orphan"},
            "dependenciesMeta":{"dep-orphan":{"sandbox":false}}}"#,
    )
    .unwrap();
    // `&` detaches the writer from the script's shell exactly as node-gyp
    // detaches make: killing the shell alone leaves it running. It appends at
    // ~20 Hz so any survival shows up as file growth.
    std::fs::write(
        dep.join("package.json"),
        r#"{"name":"dep-orphan","version":"1.0.0","scripts":{"postinstall":"sh -c 'while :; do echo x >> ./ORPHAN-LOG; sleep 0.05; done' & sleep 0.6"}}"#,
    )
    .unwrap();

    assert_eq!(run(&dir, &["install"]), 0, "install should link the dep");
    assert_eq!(
        run(&dir, &["approve-builds", "--all"]),
        0,
        "the fixture script exits 0; this test is about the success path"
    );

    let log = store_package_dir(&dir, "dep-orphan")
        .expect("store materialized the package")
        .join("ORPHAN-LOG");
    let at_return = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    std::thread::sleep(std::time::Duration::from_millis(700));
    let after = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        at_return > 0,
        "the fixture never wrote anything — the test would pass vacuously"
    );
    assert_eq!(
        after, at_return,
        "a process the postinstall spawned was still writing into the package \
         dir after the command returned ({at_return} -> {after} bytes)"
    );
}

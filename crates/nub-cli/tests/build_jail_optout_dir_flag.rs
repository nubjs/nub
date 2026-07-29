//! The per-package build-jail opt-out survives `-C/--dir` — an install driven from
//! OUTSIDE the project honours `dependenciesMeta.<name>.sandbox: false` exactly as one
//! run from inside it does.
//!
//! WHY THIS NEEDS PINNING. The opt-out's provenance gate asks whether `project_root`
//! contains nub's process CWD (`pm_engine::build_jail::should_confine_from`) — the check
//! that keeps a fetched git dependency from unjailing the packages its nested install
//! builds, where aube's root IS the attacker's checkout. Nothing in that gate mentions
//! `-C`. It works because `-C` is implemented as a real `chdir`
//! (`pm_engine::apply_dir`), applied by every install-family verb before anything reads
//! the project — so the property holds INCIDENTALLY, through a module that has no idea
//! the jail depends on it. Re-implementing `-C` as an `opts.project_dir` hand-off
//! instead (the shape `run_git_dep_prepare` already uses) would silently confine every
//! opted-out package on the `-C` route. This file is what makes that loud.
//!
//! The adversarial half is NOT here: `build_jail::tests::
//! a_fetched_checkout_cannot_unjail_the_packages_its_nested_install_builds` pins that a
//! checkout whose root does NOT contain the cwd stays confined however inviting its
//! `dependenciesMeta` looks. That test and this one are the two directions of one
//! predicate and must both stay green.
//!
//! Fully offline: the dependency is a local tarball, so no registry and no release-age
//! gate is involved.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The exact string the unconfined decision announces. Pinned here because it is what a
/// corpus run asserts to prove its jail-off arm took effect.
const OPT_OUT_NOTICE: &str = "build scripts are running without the build sandbox";

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // <profile>/
    path.push("nub");
    path
}

/// A one-file npm tarball whose `postinstall` is a trivial, always-available command.
/// The script's OUTPUT is irrelevant — what is under test is which side of the opt-out
/// gate the spawn lands on, which nub announces before it runs anything.
fn write_dep_tarball(at: &Path) {
    let stage = at.parent().unwrap().join("pkgstage");
    std::fs::create_dir_all(stage.join("package")).unwrap();
    std::fs::write(
        stage.join("package/package.json"),
        r#"{"name":"jaildep","version":"1.0.0","scripts":{"postinstall":"node --version"}}"#,
    )
    .unwrap();
    let status = Command::new("tar")
        .args(["czf", at.to_str().unwrap(), "package"])
        .current_dir(&stage)
        .status()
        .expect("tar the fixture dependency");
    assert!(status.success(), "fixture tarball");
}

fn write_project(dir: &Path, tarball: &Path, opt_out: bool) {
    std::fs::create_dir_all(dir).unwrap();
    let meta = if opt_out {
        r#","dependenciesMeta":{"jaildep":{"sandbox":false}}"#
    } else {
        ""
    };
    std::fs::write(
        dir.join("package.json"),
        format!(
            r#"{{"name":"jailproj","version":"1.0.0","dependencies":{{"jaildep":"file:{}"}}{meta}}}"#,
            tarball.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
}

/// `install` then `approve-builds --all`, both carrying the same `dir_flag` argv, and
/// both spawned from `cwd`. Returns the merged output of the approval run — the phase
/// that actually reaches the jail gate.
fn install_and_approve(cwd: &Path, project: &Path, dir_flag: &[&str], scratch: &Path) -> String {
    let run = |args: Vec<&str>| -> String {
        let out = Command::new(nub_binary())
            .args(args)
            .current_dir(cwd)
            .env("XDG_DATA_HOME", scratch.join("data"))
            .env("XDG_CACHE_HOME", scratch.join("cache"))
            .output()
            .expect("failed to spawn nub");
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    };
    let mut install = vec!["install"];
    install.extend_from_slice(dir_flag);
    let installed = run(install);
    assert!(
        project.join("node_modules/jaildep").exists(),
        "the fixture must actually install before the approval phase means anything: {installed}"
    );
    let mut approve = vec!["approve-builds", "--all"];
    approve.extend_from_slice(dir_flag);
    let approved = run(approve);
    // The shared waypoint both arms must reach. Without it a "no notice" assertion
    // passes vacuously on a run that fell over before the gate was ever consulted.
    assert!(
        approved.contains("Approved 1 package"),
        "the approval must reach the build gate: {approved}"
    );
    approved
}

#[test]
fn the_per_package_opt_out_survives_the_dir_flag() {
    let temp = tempfile::tempdir().unwrap();
    let tarball = temp.path().join("jaildep-1.0.0.tgz");
    write_dep_tarball(&tarball);
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();

    // THE CASE UNDER TEST: cwd outside the project, `-C` pointing into it.
    let via_flag = temp.path().join("via-flag");
    write_project(&via_flag, &tarball, true);
    let flagged = install_and_approve(
        &outside,
        &via_flag,
        &["-C", via_flag.to_str().unwrap()],
        &temp.path().join("s1"),
    );
    assert!(
        flagged.contains(OPT_OUT_NOTICE),
        "`-C` must honour the project's own opt-out: {flagged}"
    );

    // CONTROL A — the same fixture minus the opt-out, same argv, same cwd. This is what
    // makes the assertion above about `dependenciesMeta` rather than about the notice
    // being unconditional.
    let no_opt_out = temp.path().join("no-opt-out");
    write_project(&no_opt_out, &tarball, false);
    let confined = install_and_approve(
        &outside,
        &no_opt_out,
        &["-C", no_opt_out.to_str().unwrap()],
        &temp.path().join("s2"),
    );
    assert!(
        !confined.contains(OPT_OUT_NOTICE),
        "a project that did not opt out must stay confined under `-C`: {confined}"
    );

    // CONTROL B — the cwd-inside route, which must keep working. If a future change made
    // the opt-out depend on `-C` being present, this arm is what catches it.
    let from_inside = temp.path().join("from-inside");
    write_project(&from_inside, &tarball, true);
    let inside = install_and_approve(&from_inside, &from_inside, &[], &temp.path().join("s3"));
    assert!(
        inside.contains(OPT_OUT_NOTICE),
        "the cwd-inside route must keep honouring the opt-out: {inside}"
    );
}

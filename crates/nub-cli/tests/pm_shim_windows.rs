//! Windows PM-shim integration tests — the cfg(windows) half of `pm_shim.rs`
//! (which is `#![cfg(unix)]`): the `.exe`-named shim entries `install_shims_into`
//! produces, argv0 dispatch through a `pnpm.exe`/`npm.exe` link, the
//! `.exe`/`.cmd`/`.bat` PATH probing of the fall-through scan, and the
//! spawn+wait exec replacement (no Unix `exec` on Windows) — asserting exit-code
//! fidelity through it. These run on the windows-latest CI leg; they are the
//! only place this cfg-gated code executes at all.
//!
//! Hermetic like the Unix suite, with one Windows twist: `dirs_next::home_dir()`
//! reads the Known Folder API, NOT an env var, so a child's HOME/USERPROFILE
//! override cannot redirect `~/.nub/shims` or shell profiles. Nothing here may
//! therefore invoke `nub pm shim`/`unshim` end-to-end (they would write the
//! runner's real profile) — the shim DIR machinery is exercised against an
//! explicit temp dir via `install_shims_into`, and the dispatch tests only
//! touch `shim_dir()` as the fall-through's skip dir (read-only, absent on a
//! fresh runner). The cache IS redirectable: `cache_dir()` honors
//! `XDG_CACHE_HOME` on every platform.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub.exe");
    path
}

/// Unique temp dir under the system temp root (never under the user profile —
/// the manifest walk-up must not escape into a stray ancestor package.json).
fn tmp(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-pmshim-win-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Link the real nub binary under a PM name — what `nub pm shim` produces.
/// Hardlink like the real shim; copy fallback when temp sits on a different
/// volume than the build dir (the GitHub runner splits C:/D:), exactly the
/// fallback `install_shims_into` itself takes.
fn shim_link(dir: &Path, name: &str) -> PathBuf {
    let link = dir.join(format!("{name}.exe"));
    if std::fs::hard_link(nub_binary(), &link).is_err() {
        std::fs::copy(nub_binary(), &link).unwrap();
    }
    link
}

/// A fake system PM as the `.cmd` launcher real npm-on-Windows installs ship:
/// prints `FAKE-<NAME> ARGS:<args>` and exits `code`, so a test asserts WHICH
/// program ran, that argv passed verbatim, and that the exit code survives the
/// spawn+wait exec replacement. (Full-path echo equality is the Unix suite's
/// job — 8.3 short names in %TEMP% make path equality brittle here.)
fn fake_pm_cmd(dir: &Path, name: &str, code: i32) -> PathBuf {
    let path = dir.join(format!("{name}.cmd"));
    std::fs::write(
        &path,
        format!(
            "@echo off\r\necho FAKE-{} ARGS:%*\r\nexit /b {code}\r\n",
            name.to_uppercase()
        ),
    )
    .unwrap();
    path
}

/// Spawn `program args…` from `cwd` with `env` applied on top of the inherited
/// environment (ambient npm_config_* stripped so a dev-box registry override or
/// a PM-launched test runner can't skew nesting/registry assertions). Returns
/// (stdout, stderr, code).
fn run(program: &Path, args: &[&str], cwd: &Path, env: &[(&str, &str)]) -> (String, String, i32) {
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(cwd);
    cmd.env_remove("npm_config_registry");
    cmd.env_remove("npm_config_user_agent");
    cmd.env_remove("npm_execpath");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("failed to spawn");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn install_shims_into_names_entries_exe_and_the_link_dispatches_by_argv0() {
    use nub_core::pm::shim::{SHIM_NAMES, ShimAction, install_shims_into};

    let work = tmp("install");
    let shims = work.join("shims");

    // The real install body, against a temp dir: every entry lands as
    // `<name>.exe` (shim_file_name's Windows spelling), freshly created.
    // Hardlink-vs-copy is volume-dependent on the runner (C: temp vs D:
    // checkout), so `copied` is deliberately not asserted.
    let report = install_shims_into(&shims, &nub_binary()).unwrap();
    assert_eq!(report.len(), SHIM_NAMES.len());
    for shim in &report {
        assert_eq!(
            shim.action,
            ShimAction::Created,
            "{} must be fresh in an empty dir",
            shim.name
        );
        assert_eq!(
            shim.path,
            shims.join(format!("{}.exe", shim.name)),
            "Windows entries carry the .exe suffix"
        );
        assert!(shim.path.is_file(), "{} must exist on disk", shim.name);
    }

    // Dispatch through the produced entry: argv0 `pnpm.exe` → file_stem strips
    // `.exe` → the PM-shim path. Unpinned project → fall-through; the PATH scan
    // probes `pnpm.exe` (miss) then `pnpm.cmd` (hit — the launcher real npm
    // installs ship), and the spawn+wait exec forwards the child's exit code.
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("package.json"), r#"{"name":"app"}"#).unwrap();
    let sys = work.join("sys");
    std::fs::create_dir_all(&sys).unwrap();
    fake_pm_cmd(&sys, "pnpm", 5);
    let cache = work.join("cache");

    let (stdout, stderr, code) = run(
        &shims.join("pnpm.exe"),
        &["install", "--frozen-lockfile"],
        &proj,
        &[
            ("PATH", sys.to_str().unwrap()),
            ("XDG_CACHE_HOME", cache.to_str().unwrap()),
        ],
    );
    assert!(
        stdout.contains("FAKE-PNPM ARGS:install --frozen-lockfile"),
        "the fall-through must run the system pnpm.cmd with argv verbatim, got stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        code, 5,
        "spawn+wait must forward the .cmd's exit code; stderr:\n{stderr}"
    );
}

#[test]
fn pinned_pnpm_exe_runs_the_cached_pm_under_node_and_forwards_its_exit_code() {
    let work = tmp("pinned");
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join("package.json"),
        r#"{"packageManager":"pnpm@9.0.0"}"#,
    )
    .unwrap();
    // Tripwire: registry config reads the PROJECT .npmrc, so an accidental
    // cache miss fails fast against a dead registry instead of touching the
    // network (the exact-pin cache hit below must never get that far).
    std::fs::write(proj.join(".npmrc"), "registry=http://127.0.0.1:1/\r\n").unwrap();

    // Seed the store with a fake cached pnpm@9.0.0 — the exact-pin zero-network
    // hit. The bin prints its argv and exits 7, so one invocation asserts both
    // the node-run dispatch and exit-code fidelity through spawn+wait.
    let cache = work.join("cache");
    let pkg = cache.join("nub/pm/pnpm/9.0.0/package");
    std::fs::create_dir_all(pkg.join("bin")).unwrap();
    std::fs::write(
        pkg.join("package.json"),
        r#"{"name":"pnpm","bin":{"pnpm":"bin/pnpm.cjs","pnpx":"bin/pnpx.cjs"}}"#,
    )
    .unwrap();
    std::fs::write(
        pkg.join("bin/pnpm.cjs"),
        "console.log('PINNED-PNPM ' + process.argv.slice(2).join(' '));\nprocess.exit(7);\n",
    )
    .unwrap();
    let link = shim_link(&work, "pnpm");

    // PATH is inherited: the project Node resolves from it (no Node pin here);
    // RunPinned never PATH-scans for pnpm, so an ambient real pnpm is inert.
    let (stdout, stderr, code) = run(
        &link,
        &["--version"],
        &proj,
        &[("XDG_CACHE_HOME", cache.to_str().unwrap())],
    );
    assert_eq!(
        stdout, "PINNED-PNPM --version\n",
        "the pinned PM's bin must run under node with argv verbatim; stderr:\n{stderr}"
    );
    assert_eq!(
        code, 7,
        "the cached PM's exit code must survive spawn+wait; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("Installing"),
        "an exact cached pin must be a silent zero-network hit, got stderr:\n{stderr}"
    );
}

#[test]
fn mismatched_npm_exe_in_a_pinned_project_refuses_before_the_system_npm() {
    let work = tmp("refuse");
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join("package.json"),
        r#"{"packageManager":"pnpm@9.0.0"}"#,
    )
    .unwrap();
    // A system npm.cmd exists on PATH — the strict check must refuse BEFORE it.
    let sys = work.join("sys");
    std::fs::create_dir_all(&sys).unwrap();
    fake_pm_cmd(&sys, "npm", 0);
    let link = shim_link(&work, "npm");
    let cache = work.join("cache");

    let (stdout, stderr, code) = run(
        &link,
        &["install", "react"],
        &proj,
        &[
            ("PATH", sys.to_str().unwrap()),
            ("XDG_CACHE_HOME", cache.to_str().unwrap()),
        ],
    );
    assert_eq!(code, 1, "the strict refusal exits 1; stderr:\n{stderr}");
    assert!(
        !stdout.contains("FAKE-NPM"),
        "the system npm must NOT run on a refusal, got stdout:\n{stdout}"
    );
    for needle in [
        "pnpm",
        "package.json#packageManager",
        "pnpm install react",
        "nub pm unshim",
    ] {
        assert!(
            stderr.contains(needle),
            "the refusal must contain {needle:?}, got:\n{stderr}"
        );
    }
}

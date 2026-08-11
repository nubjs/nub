//! Pre-run dependency-freshness gate (issue #252), end-to-end through the
//! binary. The gate warns (or, per policy, aborts) before `nub run`/file/bin
//! execution when `node_modules` looks stale, so a missing dep surfaces as a
//! clear nub message instead of a raw `foo: command not found`.
//!
//! These fixtures are node-free: scripts are bare `echo`s and the installed
//! trees are hand-built (a `node_modules/<dep>/package.json`), so the run path
//! never needs a real Node or a network install — the gate reads the manifest
//! and the tree directly.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

fn tmp(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-vdbr-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// Hand-place an installed package: `node_modules/<name>/package.json` at
/// `version`. Mirrors what any PM's flat/symlinked layout exposes for a read.
fn install_pkg(root: &Path, name: &str, version: &str) {
    write(
        &root.join("node_modules").join(name).join("package.json"),
        &format!(r#"{{"name":"{name}","version":"{version}"}}"#),
    );
}

struct Output {
    stdout: String,
    stderr: String,
    code: i32,
}

fn run(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(nub_binary());
    let home = tmp("home");
    cmd.args(args)
        .current_dir(dir)
        .env("XDG_DATA_HOME", tmp("xdg-data"))
        .env("XDG_CACHE_HOME", tmp("xdg-cache"))
        // Both of these decide the gate BEFORE the fixture gets a say, so a
        // suite that inherits them asserts the dev box rather than the tree
        // under test. `verifyDeps` in the user's global `nub.jsonc` outranks
        // every project source in `resolve_policy`, and the marker skips the
        // check outright — which any suite launched from inside a nub-run
        // script inherits, silently turning `an_inherited_checked_marker_…`
        // into a test that cannot fail.
        //
        // `resolve_policy`'s last rung, the user `~/.npmrc`, stays ambient on
        // Windows and only there: it resolves through `dirs_next::home_dir()`,
        // which is `SHGetKnownFolderPath` on that platform and reads no
        // environment variable at all. The `XDG_CONFIG_HOME` redirect below is
        // cross-platform — `config_dir()` checks it ahead of every platform
        // branch.
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env_remove(CHECKED_MARKER);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("failed to spawn nub");
    Output {
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        code: out.status.code().unwrap_or(-1),
    }
}

const STALE: &str = "out of date";

/// The internal re-entry sentinel (`verify_deps::CHECKED_MARKER`), which a
/// binary-level test can't import.
const CHECKED_MARKER: &str = "__NUB_DEPS_CHECKED";

#[test]
fn fresh_clone_warns_but_still_runs_the_script() {
    // The reported bug: a fresh checkout with no `node_modules` and a devDep
    // (husky/tsc) — warn, and still run (the warning is non-fatal by default).
    let d = tmp("fresh");
    write(
        &d.join("package.json"),
        r#"{"name":"f","version":"1.0.0","scripts":{"build":"echo BUILD_RAN"},"devDependencies":{"typescript":"^5.0.0"}}"#,
    );
    let out = run(&d, &["run", "build"], &[]);
    assert!(
        out.stderr.contains(STALE),
        "expected a warning: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("BUILD_RAN"),
        "script must still run: {}",
        out.stdout
    );
    assert_eq!(out.code, 0);
}

#[test]
fn warm_tree_is_silent() {
    let d = tmp("warm");
    write(
        &d.join("package.json"),
        r#"{"name":"w","version":"1.0.0","scripts":{"build":"echo OK"},"dependencies":{"lodash":"^4.0.0"}}"#,
    );
    install_pkg(&d, "lodash", "4.17.21"); // satisfies ^4.0.0
    let out = run(&d, &["run", "build"], &[]);
    assert!(
        !out.stderr.contains(STALE),
        "warm tree must not warn: {}",
        out.stderr
    );
    assert!(out.stdout.contains("OK"));
}

#[test]
fn version_drift_warns_and_names_the_dependency() {
    // Installed version no longer satisfies a bumped manifest range.
    let d = tmp("drift");
    write(
        &d.join("package.json"),
        r#"{"name":"d","version":"1.0.0","scripts":{"build":"echo OK"},"dependencies":{"lodash":"^5.0.0"}}"#,
    );
    install_pkg(&d, "lodash", "4.17.21"); // does NOT satisfy ^5.0.0
    let out = run(&d, &["run", "build"], &[]);
    assert!(out.stderr.contains(STALE), "{}", out.stderr);
    assert!(
        out.stderr.contains("lodash"),
        "reason should name the dep: {}",
        out.stderr
    );
}

#[test]
fn a_prod_install_with_devdeps_absent_is_not_flagged() {
    // Production deps present, devDependencies absent — the shape a
    // `--prod`/`--omit=dev` install leaves. Warning here would be a false
    // positive, so an absent devDep is tolerated once anything is installed.
    let d = tmp("prod");
    write(
        &d.join("package.json"),
        r#"{"name":"p","version":"1.0.0","scripts":{"build":"echo OK"},"dependencies":{"lodash":"^4.0.0"},"devDependencies":{"typescript":"^5.0.0"}}"#,
    );
    install_pkg(&d, "lodash", "4.17.21"); // prod present; typescript (dev) absent
    let out = run(&d, &["run", "build"], &[]);
    assert!(
        !out.stderr.contains(STALE),
        "prod install must not warn: {}",
        out.stderr
    );
}

#[test]
fn no_check_flag_opts_out() {
    let d = tmp("nocheck");
    write(
        &d.join("package.json"),
        r#"{"name":"n","version":"1.0.0","scripts":{"build":"echo OK"},"devDependencies":{"typescript":"^5.0.0"}}"#,
    );
    let out = run(&d, &["run", "--no-check", "build"], &[]);
    assert!(
        !out.stderr.contains(STALE),
        "--no-check must silence: {}",
        out.stderr
    );
    assert!(out.stdout.contains("OK"));
}

#[test]
fn node_compat_env_skips_the_check() {
    // `NODE_COMPAT=1` is the tree-wide zero-augmentation opt-out; the freshness
    // check is augmentation, so it's skipped.
    let d = tmp("compat");
    write(
        &d.join("package.json"),
        r#"{"name":"c","version":"1.0.0","scripts":{"build":"echo OK"},"devDependencies":{"typescript":"^5.0.0"}}"#,
    );
    let out = run(&d, &["run", "build"], &[("NODE_COMPAT", "1")]);
    assert!(
        !out.stderr.contains(STALE),
        "compat mode must skip: {}",
        out.stderr
    );
}

#[test]
fn npmrc_off_disables_the_check() {
    let d = tmp("npmrcoff");
    write(
        &d.join("package.json"),
        r#"{"name":"o","version":"1.0.0","scripts":{"build":"echo OK"},"devDependencies":{"typescript":"^5.0.0"}}"#,
    );
    write(&d.join(".npmrc"), "verify-deps-before-run=off\n");
    let out = run(&d, &["run", "build"], &[]);
    assert!(
        !out.stderr.contains(STALE),
        "npmrc off must silence: {}",
        out.stderr
    );
}

#[test]
fn npmrc_error_aborts_before_running() {
    let d = tmp("npmrcerr");
    write(
        &d.join("package.json"),
        r#"{"name":"e","version":"1.0.0","scripts":{"build":"echo BUILD_RAN"},"devDependencies":{"typescript":"^5.0.0"}}"#,
    );
    write(&d.join(".npmrc"), "verify-deps-before-run=error\n");
    let out = run(&d, &["run", "build"], &[]);
    assert_eq!(
        out.code, 1,
        "error policy must abort non-zero: {:?}",
        out.code
    );
    assert!(out.stderr.contains(STALE), "{}", out.stderr);
    assert!(
        !out.stdout.contains("BUILD_RAN"),
        "script must NOT run: {}",
        out.stdout
    );
}

#[test]
fn env_override_beats_npmrc() {
    // `NUB_VERIFY_DEPS` wins over the `.npmrc` key.
    let d = tmp("envwin");
    write(
        &d.join("package.json"),
        r#"{"name":"ev","version":"1.0.0","scripts":{"build":"echo OK"},"devDependencies":{"typescript":"^5.0.0"}}"#,
    );
    write(&d.join(".npmrc"), "verify-deps-before-run=error\n");
    let out = run(&d, &["run", "build"], &[("NUB_VERIFY_DEPS", "off")]);
    assert_eq!(
        out.code, 0,
        "env `off` must override npmrc `error`: {}",
        out.stderr
    );
    assert!(!out.stderr.contains(STALE));
}

#[test]
fn yarn_pnp_tree_is_silently_skipped() {
    // A PnP project has no `node_modules`; the walk would read "nothing
    // installed", so PnP degrades to a silent skip rather than false-warning.
    let d = tmp("pnp");
    write(
        &d.join("package.json"),
        r#"{"name":"pnp","version":"1.0.0","scripts":{"build":"echo OK"},"dependencies":{"lodash":"^4.0.0"}}"#,
    );
    write(&d.join(".pnp.cjs"), "// pnp\n");
    let out = run(&d, &["run", "build"], &[]);
    assert!(
        !out.stderr.contains(STALE),
        "PnP must be silent: {}",
        out.stderr
    );
    assert!(out.stdout.contains("OK"));
}

#[test]
fn inside_a_running_script_the_gate_does_not_re_fire() {
    // A script that itself invokes `nub`/`node` sets `npm_lifecycle_event`; the
    // re-entry guard skips the check there so the run warns at most once.
    let d = tmp("reentry");
    write(
        &d.join("package.json"),
        r#"{"name":"r","version":"1.0.0","scripts":{"build":"echo OK"},"devDependencies":{"typescript":"^5.0.0"}}"#,
    );
    let out = run(&d, &["run", "build"], &[("npm_lifecycle_event", "outer")]);
    assert!(
        !out.stderr.contains(STALE),
        "re-entry must skip the check: {}",
        out.stderr
    );
}

#[test]
fn pnpm_11_incumbent_reads_the_policy_from_workspace_yaml() {
    // pnpm 11 dropped `.npmrc` support for `verifyDepsBeforeRun` entirely — the
    // key lives SOLELY in `pnpm-workspace.yaml` (issue #286-adjacent bug: nub
    // never looked there at all).
    let d = tmp("pnpm11yaml");
    write(
        &d.join("package.json"),
        r#"{"name":"y","version":"1.0.0","packageManager":"pnpm@11.0.0","scripts":{"build":"echo OK"},"devDependencies":{"typescript":"^5.0.0"}}"#,
    );
    write(
        &d.join("pnpm-workspace.yaml"),
        "verifyDepsBeforeRun: false\n",
    );
    let out = run(&d, &["run", "build"], &[]);
    assert!(
        !out.stderr.contains(STALE),
        "pnpm-workspace.yaml `verifyDepsBeforeRun: false` must silence the check under a pnpm-11 incumbent: {}",
        out.stderr
    );
    assert!(out.stdout.contains("OK"));
}

#[test]
fn pnpm_11_incumbent_workspace_yaml_wins_over_a_stale_npmrc() {
    // Real pnpm 11 does not read `.npmrc` for this key at all, so a leftover
    // `.npmrc` setting (e.g. from a pre-v11 migration) must never shadow the
    // yaml value — the precedence risk this fix must get right.
    let d = tmp("pnpm11precedence");
    write(
        &d.join("package.json"),
        r#"{"name":"y2","version":"1.0.0","packageManager":"pnpm@11.2.0","scripts":{"build":"echo OK"},"devDependencies":{"typescript":"^5.0.0"}}"#,
    );
    write(
        &d.join("pnpm-workspace.yaml"),
        "verifyDepsBeforeRun: false\n",
    );
    write(&d.join(".npmrc"), "verify-deps-before-run=error\n");
    let out = run(&d, &["run", "build"], &[]);
    assert!(
        !out.stderr.contains(STALE),
        "the workspace-yaml value must win over a stale .npmrc under pnpm 11: {}",
        out.stderr
    );
    assert_eq!(out.code, 0);
}

#[test]
fn pnpm_10_incumbent_still_reads_the_policy_from_npmrc() {
    // Regression guard: pnpm ≤10 (and the pnpm-unknown default) keep reading
    // `.npmrc` — only a confirmed pnpm-11+ incumbent switches homes.
    let d = tmp("pnpm10npmrc");
    write(
        &d.join("package.json"),
        r#"{"name":"n10","version":"1.0.0","packageManager":"pnpm@10.5.0","scripts":{"build":"echo OK"},"devDependencies":{"typescript":"^5.0.0"}}"#,
    );
    write(&d.join(".npmrc"), "verify-deps-before-run=off\n");
    let out = run(&d, &["run", "build"], &[]);
    assert!(
        !out.stderr.contains(STALE),
        "a pnpm-10 incumbent's .npmrc setting must still be honored: {}",
        out.stderr
    );
    assert!(out.stdout.contains("OK"));
}

#[test]
fn an_inherited_checked_marker_skips_the_gate() {
    // A `nub <file>`/`nub exec` target that spawns `node` re-enters nub through
    // the PATH shim; nub sets the internal `__NUB_DEPS_CHECKED` marker on that
    // child's env so it doesn't repeat the warning. This asserts the guard side:
    // an inherited marker suppresses the check (the propagation side — that nub
    // sets it — is covered by the ad-hoc node-spawning e2e).
    let d = tmp("marker");
    write(
        &d.join("package.json"),
        r#"{"name":"m","version":"1.0.0","scripts":{"build":"echo OK"},"devDependencies":{"typescript":"^5.0.0"}}"#,
    );
    let out = run(&d, &["run", "build"], &[(CHECKED_MARKER, "1")]);
    assert!(
        !out.stderr.contains(STALE),
        "an inherited __NUB_DEPS_CHECKED must skip the check: {}",
        out.stderr
    );
}

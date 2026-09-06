//! Cross-platform happy-path smoke tests for the `aube` binary.
//!
//! Deliberately small: a handful of hermetic checks that exercise the
//! CLI entry point, a no-op install, and the lifecycle script runner
//! without touching the network or the user's real store. The heavier
//! coverage lives in the BATS suite under `test/`, which only runs on
//! Unix. These tests fill the gap for the Windows CI job.

use assert_cmd::Command;
use predicates::boolean::PredicateBooleanExt;
use std::fs;
use std::sync::{Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;

/// Build an isolated project root plus private `HOME` / aube store /
/// cache so the test can't see or mutate the developer's real state.
struct Sandbox {
    _root: TempDir,
    project: std::path::PathBuf,
    home: std::path::PathBuf,
    store: std::path::PathBuf,
    cache: std::path::PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("aube-e2e-")
            .tempdir()
            .unwrap();
        let project = root.path().join("project");
        let home = root.path().join("home");
        let store = root.path().join("store");
        let cache = root.path().join("cache");
        for dir in [&project, &home, &store, &cache] {
            fs::create_dir_all(dir).unwrap();
        }
        Self {
            _root: root,
            project,
            home,
            store,
            cache,
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("aube").unwrap();
        cmd.current_dir(&self.project)
            .env_remove("AUBE_CONFIG")
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("AUBE_STORE_DIR", &self.store)
            .env("AUBE_CACHE_DIR", &self.cache)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("NO_COLOR", "1");
        cmd
    }

    fn write_manifest(&self, contents: &str) {
        fs::write(self.project.join("package.json"), contents).unwrap();
    }

    fn write_file(&self, rel: &str, contents: &str) {
        let path = self.project.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
}

/// Whether any file named `marker` exists anywhere under `dir`. Used to
/// find a lifecycle script's output without hard-coding the hashed
/// virtual-store path it lands in.
fn marker_exists_under(dir: &std::path::Path, marker: &str) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            if marker_exists_under(&path, marker) {
                return true;
            }
        } else if entry.file_name() == marker {
            return true;
        }
    }
    false
}

fn e2e_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    match LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[test]
fn version_flag_reports_binary_version() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn help_flag_lists_install_command() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains("install"));
}

#[cfg(unix)]
#[test]
fn aubr_replaces_itself_with_the_final_script_shell() {
    use std::os::unix::fs::symlink;
    use std::process::Stdio;

    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(r#"{"name":"exec-handoff","private":true,"scripts":{"pid":"echo $$"}}"#);
    let aubr = sbx._root.path().join("aubr");
    symlink(assert_cmd::cargo::cargo_bin!("aube"), &aubr).unwrap();

    let child = std::process::Command::new(aubr)
        .args(["--no-install", "pid"])
        .current_dir(&sbx.project)
        .env_remove("AUBE_CONFIG")
        .env("HOME", &sbx.home)
        .env("AUBE_STORE_DIR", &sbx.store)
        .env("AUBE_CACHE_DIR", &sbx.cache)
        .env("XDG_CACHE_HOME", &sbx.cache)
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let child_pid = child.id();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        child_pid.to_string()
    );
}

#[cfg(unix)]
#[test]
fn aubr_keeps_control_when_a_post_script_must_run() {
    use assert_cmd::assert::OutputAssertExt;
    use std::os::unix::fs::symlink;

    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(
        r#"{"name":"exec-handoff","private":true,"scripts":{"build":"echo build","postbuild":"echo postbuild"}}"#,
    );
    let aubr = sbx._root.path().join("aubr");
    symlink(assert_cmd::cargo::cargo_bin!("aube"), &aubr).unwrap();

    std::process::Command::new(aubr)
        .args(["--no-install", "build"])
        .current_dir(&sbx.project)
        .env_remove("AUBE_CONFIG")
        .env("HOME", &sbx.home)
        .env("AUBE_STORE_DIR", &sbx.store)
        .env("AUBE_CACHE_DIR", &sbx.cache)
        .env("XDG_CACHE_HOME", &sbx.cache)
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .stdout(predicates::str::contains("build\npostbuild\n"));
}

#[test]
fn dynamic_completion_keeps_stdout_when_use_stderr_is_configured() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    fs::write(sbx.project.join(".npmrc"), "use-stderr=true\n").unwrap();

    sbx.cmd()
        .args([
            "__complete_word__",
            "--shell",
            "bash",
            "--line",
            "aube config get ",
            "--cursor",
            "16",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("auto-install-peers"));
}

#[test]
fn dynamic_completion_rejects_an_invalid_dir() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(r#"{"dependencies":{"react":"^19"}}"#);

    sbx.cmd()
        .args([
            "__complete_word__",
            "--shell",
            "bash",
            "--line",
            "aube -C missing add ",
            "--cursor",
            "20",
            "--candidates",
            "package",
        ])
        .assert()
        .success()
        .stdout("");
}

#[test]
fn install_on_manifest_without_deps_creates_state_file() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(r#"{"name":"e2e-empty","version":"0.0.0"}"#);

    sbx.cmd().arg("install").assert().success();

    assert!(
        sbx.project.join("node_modules/.aube-state").exists(),
        "expected aube to drop a state file after install"
    );
}

#[test]
fn run_executes_a_simple_script() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(
        r#"{
            "name": "e2e-run",
            "version": "0.0.0",
            "scripts": { "greet": "echo aube-e2e-ok" }
        }"#,
    );

    sbx.cmd()
        .arg("run")
        .arg("greet")
        .assert()
        .success()
        .stdout(predicates::str::contains("aube-e2e-ok"));
}

// --- standalone exit-code contract ---------------------------------------
//
// The command layer (`aube::commands::*::run`) no longer calls
// `std::process::exit` directly: a failing command returns a propagatable
// exit code that the binary's single `std::process::exit` (in `main.rs`)
// applies. That keeps the layer embed-safe — a host driving aube as a
// library is handed the code instead of being hard-killed. These tests pin
// the *standalone* contract so the indirection stays byte-for-byte: the code
// the user sees on the command line must be unchanged by the refactor.

#[test]
fn run_propagates_a_failing_scripts_exact_exit_code() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(
        r#"{
            "name": "e2e-exit",
            "version": "0.0.0",
            "scripts": { "boom": "exit 7" }
        }"#,
    );

    // The child's exit code (7) must surface as aube's own exit code,
    // not be flattened to a generic 1 — this is the path that previously
    // called `std::process::exit(exit_code_from_status(status))` in place.
    sbx.cmd()
        .args(["run", "--no-install", "boom"])
        .assert()
        .code(7);
}

#[test]
fn run_if_present_on_missing_script_exits_zero() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(r#"{"name":"e2e-ifpresent","version":"0.0.0"}"#);

    // The `--if-present` no-op path returns `Ok(None)` (success) rather
    // than a non-zero code; pin it so the success side of the contract
    // can't silently start exiting non-zero.
    sbx.cmd()
        .args(["run", "--no-install", "--if-present", "nope"])
        .assert()
        .success();
}

#[test]
fn run_failing_pre_script_short_circuits_with_its_code() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(
        r#"{
            "name": "e2e-prescript",
            "version": "0.0.0",
            "scripts": { "prebuild": "exit 5", "build": "echo MAIN_RAN" }
        }"#,
    );

    // A failing pre-script propagates its exact code (5) AND stops the
    // chain before the main script runs — the previous in-place exit gave
    // the same ordering, so the short-circuit must be preserved.
    sbx.cmd()
        .args(["run", "--no-install", "build"])
        .assert()
        .code(5)
        .stdout(predicates::str::contains("MAIN_RAN").not());
}

#[test]
fn approve_builds_surfaces_and_runs_a_local_source_dep() {
    // Regression for the `file:`-dep approve-builds dead-end: install
    // warns about a local-source dependency's build script, but
    // `ignored-builds` / `approve-builds` then reported nothing to
    // approve — the warned dep was keyed only in the install-recorded
    // set, never in the store the enumeration read. This pins the whole
    // contract: the dep is listed, approving it writes the *source* key
    // (a bare name never authorizes a source-backed build), and the
    // build runs during that same `approve-builds` call, then survives
    // the next install while the warning clears.
    let _guard = e2e_lock();
    let sbx = Sandbox::new();

    // A directory dependency is fully offline — no registry, no network.
    // Its postinstall drops a marker so we can prove the script ran.
    sbx.write_file(
        "dep/package.json",
        r#"{
            "name": "dep",
            "version": "1.0.0",
            "scripts": { "postinstall": "node -e \"require('fs').writeFileSync('BUILT_527_MARKER','ok')\"" }
        }"#,
    );
    sbx.write_manifest(
        r#"{
            "name": "e2e-approve-local",
            "version": "0.0.0",
            "dependencies": { "dep": "file:./dep" }
        }"#,
    );

    // Install gates the build: the marker must not exist yet, and the
    // ignored-build warning must fire naming the source key.
    sbx.cmd()
        .arg("install")
        .assert()
        .success()
        .stderr(predicates::str::contains("dep@file:./dep"));
    let node_modules = sbx.project.join("node_modules");
    assert!(
        !marker_exists_under(&node_modules, "BUILT_527_MARKER"),
        "postinstall must not run before approval"
    );

    // The dead-end: `ignored-builds` must now list the local dep by its
    // source key (it previously printed "No ignored builds").
    sbx.cmd()
        .arg("ignored-builds")
        .assert()
        .success()
        .stdout(predicates::str::contains("dep@file:./dep"));

    // Approving records the source key AND runs the build in the same
    // invocation, exactly like a registry dep. The scoped rebuild reaches
    // the tree because approval cannot move a local dep's cell: the
    // build-state-sensitive graph hash is applied only by
    // `virtual_store_subdir` (the shared global store, which `file:` deps
    // never enter), never by the `aube_dir_entry_name` that
    // `materialized_pkg_dir` reconstructs.
    sbx.cmd()
        .args(["approve-builds", "--all"])
        .assert()
        .success();
    assert!(
        marker_exists_under(&node_modules, "BUILT_527_MARKER"),
        "approve-builds must run a local-source dep's build in the same invocation"
    );

    // Reinstalling keeps the build and clears the warning.
    sbx.cmd()
        .arg("install")
        .assert()
        .success()
        .stderr(predicates::str::contains("dep@file:./dep").not());
    assert!(
        marker_exists_under(&node_modules, "BUILT_527_MARKER"),
        "the approved build must survive a reinstall"
    );
    sbx.cmd()
        .arg("ignored-builds")
        .assert()
        .success()
        .stdout(predicates::str::contains("No ignored builds"));
}

#[test]
fn dependency_build_finishes_before_its_consumers_build_starts() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();

    sbx.write_file(
        "child/package.json",
        r#"{
            "name": "build-child",
            "version": "1.0.0",
            "scripts": {
                "postinstall": "node -e \"setTimeout(() => require('fs').writeFileSync('READY', 'ok'), 300)\""
            }
        }"#,
    );
    sbx.write_file(
        "parent/package.json",
        r#"{
            "name": "build-parent",
            "version": "1.0.0",
            "dependencies": { "build-child": "file:../child" },
            "scripts": {
                "postinstall": "node -e \"const fs=require('fs'), p=require('path').join(require('path').dirname(require.resolve('build-child/package.json')), 'READY'); if (!fs.existsSync(p)) process.exit(17); fs.writeFileSync('PARENT_READY', 'ok')\""
            }
        }"#,
    );
    sbx.write_manifest(
        r#"{
            "name": "dependency-build-order-e2e",
            "version": "1.0.0",
            "dependencies": { "build-parent": "file:./parent" }
        }"#,
    );

    sbx.cmd()
        .args(["install", "--dangerously-allow-all-builds"])
        .assert()
        .success();
    let node_modules = sbx.project.join("node_modules");
    assert!(
        marker_exists_under(&node_modules, "READY"),
        "build-child's postinstall never wrote READY under {}",
        node_modules.display()
    );
    // The parent's postinstall exits 17 when READY is missing, so reaching
    // PARENT_READY is the ordering assertion: without phases the parent starts
    // while the child is still inside its 300ms timer and the install fails.
    assert!(
        marker_exists_under(&node_modules, "PARENT_READY"),
        "build-parent's postinstall never wrote PARENT_READY under {}",
        node_modules.display()
    );
}

// --- optional-dependency build failures are non-fatal ---------------------
//
// A package reachable only through `optionalDependencies` is one the project
// declared it can live without, so npm (`_handleOptionalFailure`) and pnpm
// (`buildDependency`'s catch) both let its build fail without failing the
// install. Both tests use `file:` directory deps, so they are fully offline.

#[test]
fn a_failing_optional_dependency_build_does_not_fail_the_install() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_file(
        "required-dep/package.json",
        r#"{
            "name": "required-dep",
            "version": "1.0.0",
            "scripts": { "postinstall": "node -e \"require('fs').writeFileSync('REQUIRED_BUILT_MARKER','ok')\"" }
        }"#,
    );
    sbx.write_file(
        "optional-dep/package.json",
        r#"{
            "name": "optional-dep",
            "version": "1.0.0",
            "scripts": { "postinstall": "exit 1" }
        }"#,
    );
    sbx.write_manifest(
        r#"{
            "name": "e2e-optional-build",
            "version": "0.0.0",
            "dependencies": { "required-dep": "file:./required-dep" },
            "optionalDependencies": { "optional-dep": "file:./optional-dep" }
        }"#,
    );

    // The skip must be recorded, not hidden — a native addon that silently
    // never built is miserable to trace back from the runtime import error.
    // Pinned on the stable warning CODE, not the spec string: the underlying
    // build error names the package too, so a spec-only assertion would pass
    // even if the skip stopped being warned about at all. Referenced through
    // the constant rather than a literal — `aube-codes` is a normal dependency
    // of this package, so an integration test can name it, and no self-test
    // ties a constant's value to its identifier.
    sbx.cmd()
        .args(["install", "--dangerously-allow-all-builds"])
        .assert()
        .success()
        .stderr(predicates::str::contains(
            aube_codes::warnings::WARN_AUBE_OPTIONAL_BUILD_FAILED,
        ))
        .stderr(predicates::str::contains("optional-dep@1.0.0"));

    // Non-fatal must also mean non-blocking: the failure raises no
    // short-circuit, so a sibling's build still runs to completion.
    assert!(
        marker_exists_under(&sbx.project.join("node_modules"), "REQUIRED_BUILT_MARKER"),
        "a sibling package's build must still run after an optional build fails"
    );
}

#[test]
fn a_failing_required_dependency_build_still_fails_the_install() {
    // The control for the test above: if the optional carve-out ever widened to
    // swallow every build failure, this is what would stop passing.
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_file(
        "required-dep/package.json",
        r#"{
            "name": "required-dep",
            "version": "1.0.0",
            "scripts": { "postinstall": "exit 1" }
        }"#,
    );
    sbx.write_manifest(
        r#"{
            "name": "e2e-required-build",
            "version": "0.0.0",
            "dependencies": { "required-dep": "file:./required-dep" }
        }"#,
    );

    sbx.cmd()
        .args(["install", "--dangerously-allow-all-builds"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("required-dep"));
}

#[test]
fn an_optional_dependency_that_is_also_required_still_fails_the_install() {
    // `optional-dep` is declared BOTH optional (by the root) and as a plain
    // dependency of `required-dep`, so one fully-required path reaches it and
    // npm/pnpm both treat it as required. This is the case where a
    // reachability bug would silently swallow a real failure.
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_file(
        "required-dep/package.json",
        r#"{
            "name": "required-dep",
            "version": "1.0.0",
            "dependencies": { "optional-dep": "file:../optional-dep" }
        }"#,
    );
    sbx.write_file(
        "optional-dep/package.json",
        r#"{
            "name": "optional-dep",
            "version": "1.0.0",
            "scripts": { "postinstall": "exit 1" }
        }"#,
    );
    sbx.write_manifest(
        r#"{
            "name": "e2e-dual-path-build",
            "version": "0.0.0",
            "dependencies": { "required-dep": "file:./required-dep" },
            "optionalDependencies": { "optional-dep": "file:./optional-dep" }
        }"#,
    );

    // Pinned on the lifecycle error, not a bare `.failure()`. `required-dep`
    // reaches `optional-dep` through `file:../optional-dep`, which points
    // outside its own directory — so if that ever stopped resolving, a bare
    // exit-code assertion would keep passing while testing nothing, and the
    // reachability regression this guards would go unnoticed.
    sbx.cmd()
        .args(["install", "--dangerously-allow-all-builds"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "lifecycle script postinstall failed",
        ))
        .stderr(predicates::str::contains("optional-dep@1.0.0"));
}

#[test]
fn run_forwards_extra_args_verbatim() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    // A quote-free body takes the direct-exec path on Unix and the shell
    // on Windows, so this pins the same argv contract on both: forwarded
    // args reach the script as written, with nothing expanded or split.
    sbx.write_manifest(
        r#"{
            "name": "e2e-run-args",
            "version": "0.0.0",
            "scripts": { "probe": "node probe.js" }
        }"#,
    );
    fs::write(
        sbx.project.join("probe.js"),
        "console.log(JSON.stringify(process.argv.slice(2)))",
    )
    .unwrap();

    sbx.cmd()
        .args(["run", "--no-install", "probe", "--", "a b", "$HOME", "*"])
        .assert()
        .success()
        .stdout(predicates::str::contains(r#"["a b","$HOME","*"]"#));
}

#[test]
fn run_reports_a_missing_command_like_a_shell() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(
        r#"{
            "name": "e2e-run-missing",
            "version": "0.0.0",
            "scripts": { "nope": "aube-definitely-not-a-real-binary" }
        }"#,
    );

    // The body is shaped for direct exec, but the program does not
    // resolve, so it must fall back to the shell rather than inventing an
    // error — which is what keeps 127 (and the shell's own stderr) intact.
    let assert = sbx.cmd().args(["run", "--no-install", "nope"]).assert();
    if cfg!(unix) {
        assert.code(127);
    } else {
        assert.failure();
    }
}

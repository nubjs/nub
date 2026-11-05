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
    // reports a local-source dependency's build script, but
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

    // Install gates the build and, under the default `strictDepBuilds`,
    // fails naming the source key — after linking, so the dep is in place
    // and the next step is approving it. A repeat install from the
    // up-to-date state fails the same way.
    for _ in 0..2 {
        sbx.cmd()
            .arg("install")
            .assert()
            .code(1)
            .stderr(predicates::str::contains("ERR_AUBE_IGNORED_BUILDS"))
            .stderr(predicates::str::contains("dep@file:./dep"));
    }
    let node_modules = sbx.project.join("node_modules");
    assert!(
        node_modules.join("dep/package.json").exists(),
        "the failing install must still link the dependency"
    );
    // Naming the package to `rebuild` does not approve it either.
    sbx.cmd()
        .args(["rebuild", "dep"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("ERR_AUBE_IGNORED_BUILDS"));
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

    // Reinstalling keeps the build and clears the failure.
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

/// A `file:` dependency whose postinstall writes `marker` into its own dir.
fn write_marker_dep(sbx: &Sandbox, name: &str, marker: &str) {
    sbx.write_file(
        &format!("{name}/package.json"),
        &format!(
            r#"{{
                "name": "{name}",
                "version": "1.0.0",
                "scripts": {{ "postinstall": "node -e \"require('fs').writeFileSync('{marker}','ok')\"" }}
            }}"#
        ),
    );
}

#[test]
fn allow_build_records_denials_and_never_overturns_one() {
    // pnpm 12's `--allow-build` grammar: `!pkg` records a denial, and an
    // approval of a package the project denies is refused rather than
    // silently flipping the entry.
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    write_marker_dep(&sbx, "dep", "DENIED_DEP_BUILT");
    sbx.write_manifest(r#"{"name":"e2e-allow-build-denial","version":"0.0.0"}"#);

    sbx.cmd()
        .args(["add", "file:./dep", "--allow-build=!dep"])
        .assert()
        .success();
    let manifest = fs::read_to_string(sbx.project.join("package.json")).unwrap();
    assert!(
        manifest.contains(r#""dep": false"#),
        "`--allow-build=!dep` must record a denial:\n{manifest}"
    );

    sbx.cmd()
        .args(["add", "file:./dep", "--allow-build=dep"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains(
            "ERR_AUBE_OVERRIDING_IGNORED_BUILT_DEPENDENCIES",
        ));
    assert_eq!(
        fs::read_to_string(sbx.project.join("package.json")).unwrap(),
        manifest,
        "a refused approval must leave the denial in place"
    );
    assert!(!marker_exists_under(
        &sbx.project.join("node_modules"),
        "DENIED_DEP_BUILT"
    ));
}

#[test]
fn rebuild_and_ignored_builds_honor_a_workspace_members_approval() {
    // Install builds what a member's manifest approves. `rebuild` and
    // `ignored-builds` must read the same approvals: naming a package to
    // `rebuild` does not bypass the policy, so a root-only read would report
    // an approved package as unapproved.
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    write_marker_dep(&sbx, "dep", "MEMBER_BUILT");
    sbx.write_manifest(
        r#"{"name":"e2e-member-approval","version":"0.0.0","private":true,"workspaces":["packages/*"]}"#,
    );
    sbx.write_file(
        "packages/a/package.json",
        r#"{
            "name": "a",
            "version": "1.0.0",
            "dependencies": { "dep": "file:../../dep" },
            "aube": { "allowBuilds": { "dep@file:dep": true } }
        }"#,
    );

    sbx.cmd().arg("install").assert().success();
    assert!(marker_exists_under(
        &sbx.project.join("node_modules"),
        "MEMBER_BUILT"
    ));
    sbx.cmd().args(["rebuild", "dep"]).assert().success();
    sbx.cmd()
        .arg("ignored-builds")
        .assert()
        .success()
        .stdout(predicates::str::contains("No ignored builds"));
}

#[test]
fn remove_runs_postuninstall_before_reporting_an_unapproved_build() {
    // pnpm 12 runs `postuninstall` inside a remove's install and raises
    // `ERR_PNPM_IGNORED_BUILDS` only after it.
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    write_marker_dep(&sbx, "dep", "DEP_BUILT");
    sbx.write_file(
        "other/package.json",
        r#"{"name":"other","version":"1.0.0"}"#,
    );
    sbx.write_manifest(
        r#"{
            "name": "e2e-remove-hooks",
            "version": "0.0.0",
            "dependencies": { "dep": "file:./dep", "other": "file:./other" },
            "scripts": { "postuninstall": "node -e \"require('fs').writeFileSync('POSTUNINSTALL_RAN','ok')\"" }
        }"#,
    );

    sbx.cmd().arg("install").assert().code(1);
    sbx.cmd()
        .args(["remove", "other"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("ERR_AUBE_IGNORED_BUILDS"));
    assert!(
        sbx.project.join("POSTUNINSTALL_RAN").exists(),
        "postuninstall must run before the unapproved build fails the remove"
    );
    let manifest = fs::read_to_string(sbx.project.join("package.json")).unwrap();
    assert!(!manifest.contains(r#""other""#), "{manifest}");
}

#[test]
fn filtered_rebuild_builds_only_the_selected_projects_dependencies() {
    // Members of a shared-lockfile workspace have no lockfile of their own,
    // so a filtered rebuild works from the root's, over the selected
    // projects' dependencies only.
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    let log = sbx.project.join("builds.log");
    let log_js = log.display().to_string().replace('\\', "/");
    for name in ["x", "y"] {
        sbx.write_file(
            &format!("{name}/package.json"),
            &format!(
                r#"{{
                    "name": "{name}",
                    "version": "1.0.0",
                    "scripts": {{ "postinstall": "node -e \"require('fs').appendFileSync('{log_js}','{name};')\"" }}
                }}"#
            ),
        );
    }
    sbx.write_manifest(
        r#"{
            "name": "e2e-filtered-rebuild",
            "version": "0.0.0",
            "private": true,
            "workspaces": ["packages/*"],
            "aube": { "allowBuilds": { "x@file:x": true, "y@file:y": true } }
        }"#,
    );
    sbx.write_file(
        "packages/a/package.json",
        r#"{"name":"a","version":"1.0.0","dependencies":{"x":"file:../../x"}}"#,
    );
    sbx.write_file(
        "packages/b/package.json",
        r#"{"name":"b","version":"1.0.0","dependencies":{"y":"file:../../y"}}"#,
    );

    sbx.cmd().arg("install").assert().success();
    assert_eq!(fs::read_to_string(&log).unwrap().len(), "x;y;".len());
    fs::write(&log, "").unwrap();
    sbx.cmd()
        .args(["--filter", "a", "rebuild"])
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&log).unwrap(), "x;");
}

#[test]
fn filtered_rebuild_keeps_the_dependency_groups_the_install_linked() {
    // After `install --prod`, a package one member uses only as a
    // devDependency is installed solely for the member that needs it in
    // production. pnpm rebuilds it only when that member is selected.
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    let log = sbx.project.join("builds.log");
    let log_js = log.display().to_string().replace('\\', "/");
    sbx.write_file(
        "x/package.json",
        &format!(
            r#"{{
                "name": "x",
                "version": "1.0.0",
                "scripts": {{ "postinstall": "node -e \"require('fs').appendFileSync('{log_js}','x;')\"" }}
            }}"#
        ),
    );
    sbx.write_manifest(
        r#"{
            "name": "e2e-filtered-rebuild-prod",
            "version": "0.0.0",
            "private": true,
            "workspaces": ["packages/*"],
            "aube": { "allowBuilds": { "x@file:x": true } }
        }"#,
    );
    sbx.write_file(
        "packages/a/package.json",
        r#"{"name":"a","version":"1.0.0","devDependencies":{"x":"file:../../x"}}"#,
    );
    sbx.write_file(
        "packages/b/package.json",
        r#"{"name":"b","version":"1.0.0","dependencies":{"x":"file:../../x"}}"#,
    );

    sbx.cmd().args(["install", "--prod"]).assert().success();
    fs::write(&log, "").unwrap();
    sbx.cmd()
        .args(["--filter", "a", "rebuild"])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "",
        "a's devDependency was not installed"
    );
    sbx.cmd()
        .args(["--filter", "b", "rebuild"])
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&log).unwrap(), "x;");
}

#[test]
fn dangerously_allow_all_builds_from_npmrc_runs_builds_but_keeps_denials() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    write_marker_dep(&sbx, "open", "OPEN_BUILT");
    write_marker_dep(&sbx, "denied", "DENIED_BUILT");
    sbx.write_manifest(
        r#"{
            "name": "e2e-allow-all-npmrc",
            "version": "0.0.0",
            "dependencies": { "open": "file:./open", "denied": "file:./denied" },
            "aube": { "allowBuilds": { "denied": false } }
        }"#,
    );
    sbx.write_file(".npmrc", "dangerouslyAllowAllBuilds=true\n");

    sbx.cmd().arg("install").assert().success();
    let node_modules = sbx.project.join("node_modules");
    assert!(marker_exists_under(&node_modules, "OPEN_BUILT"));
    assert!(
        !marker_exists_under(&node_modules, "DENIED_BUILT"),
        "an explicit `false` must outrank dangerouslyAllowAllBuilds"
    );
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
            "scripts": { "postinstall": "exit 3" }
        }"#,
    );
    sbx.write_manifest(
        r#"{
            "name": "e2e-required-build",
            "version": "0.0.0",
            "dependencies": { "required-dep": "file:./required-dep" }
        }"#,
    );

    // `.code(3)`, not `.failure()`: the script's own exit code is the
    // install's, as under npm, pnpm and `run` — neither the generic 1 nor
    // the exit table's 50 (nubjs/nub#670).
    sbx.cmd()
        .args(["install", "--dangerously-allow-all-builds"])
        .assert()
        .code(3)
        .stderr(predicates::str::contains("required-dep"));
}

#[test]
fn every_failed_required_build_is_reported_and_the_first_sets_the_exit_code() {
    // Three required builds fail in the same phase with distinct codes. All
    // three must be named — not whichever the scheduler drained first — and
    // the exit code is the FIRST failure's in build order, so the same
    // fixture answers the same way every run (nubjs/nub#670).
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    for (name, code) in [("failing-a", 3), ("failing-b", 4), ("failing-c", 7)] {
        sbx.write_file(
            &format!("{name}/package.json"),
            &format!(
                r#"{{
                    "name": "{name}",
                    "version": "1.0.0",
                    "scripts": {{ "postinstall": "exit {code}" }}
                }}"#
            ),
        );
    }
    sbx.write_manifest(
        r#"{
            "name": "e2e-many-failed-builds",
            "version": "0.0.0",
            "dependencies": {
                "failing-a": "file:./failing-a",
                "failing-b": "file:./failing-b",
                "failing-c": "file:./failing-c"
            }
        }"#,
    );

    sbx.cmd()
        .args(["install", "--dangerously-allow-all-builds"])
        .assert()
        .code(3)
        .stderr(predicates::str::contains("3 dependency builds failed"))
        .stderr(predicates::str::contains("failing-a@1.0.0"))
        .stderr(predicates::str::contains("exited with code 4"))
        .stderr(predicates::str::contains("failing-b@1.0.0"))
        .stderr(predicates::str::contains("exited with code 7"))
        .stderr(predicates::str::contains("failing-c@1.0.0"));
}

#[test]
fn a_failing_root_postinstall_exits_with_the_scripts_code() {
    // The root hook takes a different wrapper from a dependency build and
    // used to flatten the script error to a string the same way.
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_manifest(
        r#"{
            "name": "e2e-root-postinstall",
            "version": "0.0.0",
            "scripts": { "postinstall": "exit 3" }
        }"#,
    );

    sbx.cmd()
        .args(["install"])
        .assert()
        .code(3)
        .stderr(predicates::str::contains("root postinstall script failed"));
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

/// What a `.bin/<name>` shim resolves to: the link target when it is a
/// symlink, else the wrapper's text (which names its target).
fn bin_shim_target(shim: &std::path::Path) -> String {
    match fs::read_link(shim) {
        Ok(target) => target.to_string_lossy().into_owned(),
        Err(_) => fs::read_to_string(shim).unwrap(),
    }
}

/// An importer's own `bin` never takes a name a dependency claimed. A
/// package that builds itself with its own released version (rollup's
/// `prepare` runs the devDependency `rollup`) was handed its own unbuilt
/// `dist/bin` instead, and the build died on the missing file. The
/// dependency's shim wins in both layouts; an own bin under a free name is
/// still linked.
#[test]
fn an_importers_own_bin_never_shadows_a_dependencys_bin() {
    let _guard = e2e_lock();
    for linker_args in [&[][..], &["--node-linker=hoisted"][..]] {
        let sbx = Sandbox::new();
        sbx.write_file(
            "vendor/tool/package.json",
            r#"{ "name": "tool", "version": "1.0.0", "bin": { "tool": "cli.js" } }"#,
        );
        sbx.write_file(
            "vendor/tool/cli.js",
            "#!/usr/bin/env node\nconsole.log('dep')\n",
        );
        sbx.write_manifest(
            r#"{
                "name": "tool",
                "version": "0.0.0",
                "bin": { "tool": "dist/tool.js", "tool-dev": "dev.js" },
                "devDependencies": { "tool": "file:./vendor/tool" }
            }"#,
        );
        sbx.cmd()
            .arg("install")
            .args(linker_args)
            .assert()
            .success();

        let bin_dir = sbx.project.join("node_modules/.bin");
        let shims: Vec<_> = ["tool", "tool.cmd"]
            .iter()
            .map(|name| bin_dir.join(name))
            .filter(|path| path.symlink_metadata().is_ok())
            .collect();
        assert!(
            !shims.is_empty(),
            "the dependency's bin is linked ({linker_args:?})"
        );
        for shim in shims {
            let target = bin_shim_target(&shim);
            assert!(
                target.contains("cli.js") && !target.contains("dist/tool.js"),
                "{} runs the dependency, not the importer's own unbuilt bin ({linker_args:?}):\n{target}",
                shim.display()
            );
        }
        assert!(
            bin_dir.join("tool-dev").symlink_metadata().is_ok()
                || bin_dir.join("tool-dev.cmd").symlink_metadata().is_ok(),
            "an own bin under a free name is still linked ({linker_args:?})"
        );
    }
}

/// The contents of the first file named `name` under `dir`, if any.
fn read_file_under(dir: &std::path::Path, name: &str) -> Option<String> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let file_type = entry.file_type().ok()?;
        if file_type.is_dir() {
            if let Some(found) = read_file_under(&path, name) {
                return Some(found);
            }
        } else if entry.file_name() == name {
            return fs::read_to_string(path).ok();
        }
    }
    None
}

/// Under `sharedWorkspaceLockfile: false` a member's lockfile is its own, but
/// install links its packages into the workspace root's virtual store and
/// takes approvals from the root. A member-scoped rebuild has to look there:
/// resolved against the member it found nothing and exited 0. The root may
/// also be config-only, a `pnpm-workspace.yaml` with no `package.json`.
#[test]
fn member_rebuild_with_per_project_lockfiles_runs_root_approved_builds() {
    let _guard = e2e_lock();
    for root_manifest in [true, false] {
        let sbx = Sandbox::new();
        sbx.write_file(
            "dep/package.json",
            r#"{
                "name": "rebuild-dep",
                "version": "1.0.0",
                "scripts": { "postinstall": "node -e \"require('fs').appendFileSync('RUNS', 'x')\"" }
            }"#,
        );
        sbx.write_file(
            "packages/a/package.json",
            r#"{ "name": "a", "version": "1.0.0", "dependencies": { "rebuild-dep": "file:../../dep" } }"#,
        );
        sbx.write_file(
            "pnpm-workspace.yaml",
            "packages:\n  - packages/*\nsharedWorkspaceLockfile: false\nallowBuilds:\n  \"rebuild-dep@file:dep\": true\n",
        );
        if root_manifest {
            sbx.write_manifest(r#"{ "name": "root", "version": "1.0.0", "private": true }"#);
        }

        sbx.cmd().arg("install").assert().success();
        let node_modules = sbx.project.join("node_modules");
        assert_eq!(
            read_file_under(&node_modules, "RUNS").as_deref(),
            Some("x"),
            "install must build rebuild-dep once in the root virtual store \
             (root package.json: {root_manifest})"
        );

        sbx.cmd()
            .args(["--filter", "a", "rebuild"])
            .assert()
            .success();
        assert_eq!(
            read_file_under(&node_modules, "RUNS").as_deref(),
            Some("xx"),
            "`--filter a rebuild` must re-run rebuild-dep's postinstall where install put it \
             (root package.json: {root_manifest})"
        );
    }
}

/// Find `program` on the test's own `PATH`.
fn find_on_path(program: &str) -> std::path::PathBuf {
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{program}.exe"), format!("{program}.cmd")]
    } else {
        vec![program.to_string()]
    };
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{program} must be on PATH for this test"))
}

/// The Node binary itself. `node` on the host's PATH may be a version
/// manager's shim, Nub's included, which looks `node` up on PATH again; linked
/// into a private bin dir it finds its own link and recurses.
fn real_node() -> std::path::PathBuf {
    let out = std::process::Command::new(find_on_path("node"))
        .args(["-p", "process.execPath"])
        .output()
        .expect("running node");
    assert!(
        out.status.success(),
        "node -p process.execPath failed: {out:?}"
    );
    std::path::PathBuf::from(String::from_utf8(out.stdout).unwrap().trim())
}

/// A `PATH` holding `node`, `git` and a shell, and no `cp`. On Unix the
/// tools are symlinked into a private dir, because the dir `git` lives in
/// (`/usr/bin`) carries `cp` too. On Windows each tool's own dir is used
/// as is, which is what a stock PowerShell session sees.
fn path_without_cp(sbx: &Sandbox) -> std::ffi::OsString {
    let dirs: Vec<std::path::PathBuf> = if cfg!(windows) {
        let system32 = std::path::PathBuf::from(
            std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()),
        )
        .join("System32");
        vec![
            real_node().parent().unwrap().to_path_buf(),
            find_on_path("git").parent().unwrap().to_path_buf(),
            system32,
        ]
    } else {
        let bin = sbx.home.join("bin-without-cp");
        fs::create_dir_all(&bin).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(real_node(), bin.join("node")).unwrap();
            for tool in ["git", "sh"] {
                std::os::unix::fs::symlink(find_on_path(tool), bin.join(tool)).unwrap();
            }
        }
        vec![bin]
    };
    for dir in &dirs {
        assert!(
            !dir.join("cp").exists() && !dir.join("cp.exe").exists(),
            "{} must not provide cp, or the test proves nothing",
            dir.display()
        );
    }
    std::env::join_paths(dirs).unwrap()
}

/// A local git repo holding `files`, committed once, as a `git+file:` URL.
fn git_repo(sbx: &Sandbox, files: &[(&str, &str)]) -> String {
    let repo = sbx.home.join("prep-repo");
    fs::create_dir_all(&repo).unwrap();
    for (path, contents) in files {
        fs::write(repo.join(path), contents).unwrap();
    }
    for args in [
        &["init", "-q"][..],
        &["add", "-A"][..],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "init",
        ][..],
    ] {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
    format!(
        "git+file:///{}",
        repo.to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches('/')
    )
}

/// A local git repo whose `prepare` builds `dist/index.js`.
fn git_repo_with_prepare(sbx: &Sandbox) -> String {
    git_repo(
        sbx,
        &[
            (
                "package.json",
                r#"{
                    "name": "prep-demo",
                    "version": "1.0.0",
                    "main": "dist/index.js",
                    "files": ["dist"],
                    "scripts": { "prepare": "node build.js" }
                }"#,
            ),
            (
                "build.js",
                "require('fs').mkdirSync('dist', { recursive: true });\n\
                 require('fs').writeFileSync('dist/index.js', 'module.exports = 42;\\n');\n",
            ),
        ],
    )
}

/// Preparing a git dependency copies its checkout to a scratch dir before
/// the build runs. That copy used to spawn `cp`, which stock Windows has
/// no copy of, so any git dep with a `prepare` failed to install there.
#[test]
fn git_dep_prepare_runs_with_no_cp_on_path() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    let url = git_repo_with_prepare(&sbx);
    // The commit-free repository key approves every commit of the repo.
    sbx.write_manifest(&format!(
        r#"{{ "name": "consumer", "version": "0.0.0", "dependencies": {{ "prep-demo": "{url}" }},
             "pnpm": {{ "allowBuilds": {{ "prep-demo@{url}": true }} }} }}"#
    ));

    sbx.cmd()
        .arg("install")
        .env("PATH", path_without_cp(&sbx))
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(sbx.project.join("node_modules/prep-demo/dist/index.js"))
            .unwrap_or_default(),
        "module.exports = 42;\n",
        "the dependency's prepare script built dist/ before it was linked"
    );
}

/// A git dependency that has to be built runs its build only when the
/// allowlist names its source or repository, as pnpm's
/// `ERR_PNPM_GIT_DEP_PREPARE_NOT_ALLOWED` gate does. A bare package name
/// cannot approve it; a denial installs the checkout unbuilt.
#[test]
fn git_dep_prepare_is_refused_unless_the_allowlist_names_its_source() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    let url = git_repo_with_prepare(&sbx);
    let install = |allow: &str| {
        let _ = fs::remove_dir_all(sbx.project.join("node_modules"));
        sbx.write_manifest(&format!(
            r#"{{ "name": "consumer", "version": "0.0.0", "dependencies": {{ "prep-demo": "{url}" }},
                 "pnpm": {{ "allowBuilds": {{ {allow} }} }} }}"#
        ));
        sbx.cmd().arg("install").assert()
    };

    for allow in ["", r#""prep-demo": true"#] {
        let out = install(allow).code(1);
        let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
        assert!(
            stderr.contains("ERR_AUBE_GIT_DEP_PREPARE_NOT_ALLOWED"),
            "allowBuilds {{{allow}}} refuses the build:\n{stderr}"
        );
        assert!(!sbx.project.join("node_modules/prep-demo").exists());
    }

    install(r#""prep-demo": false"#).success();
    assert!(
        sbx.project.join("node_modules/prep-demo/build.js").exists()
            && !sbx.project.join("node_modules/prep-demo/dist").exists(),
        "a denied build installs the checkout as it is"
    );
}

/// A git dependency with no `prepare` but a `prepack` and its `main`
/// missing is built too, and its publishing scripts run with their
/// `pre`/`post` hooks, as `npm run prepack` runs them for pnpm: here
/// `postprepack` writes the entry point.
#[test]
fn git_dep_prepack_runs_with_its_hooks_when_main_is_missing() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    let url = git_repo(
        &sbx,
        &[
            (
                "package.json",
                r#"{
                    "name": "pack-demo",
                    "version": "1.0.0",
                    "main": "dist/index.js",
                    "files": ["dist"],
                    "scripts": { "prepack": "node -e 1", "postprepack": "node post.js" }
                }"#,
            ),
            (
                "post.js",
                "require('fs').mkdirSync('dist', { recursive: true });\n\
                 require('fs').writeFileSync('dist/index.js', 'module.exports = 9;\\n');\n",
            ),
        ],
    );
    sbx.write_manifest(&format!(
        r#"{{ "name": "consumer", "version": "0.0.0", "dependencies": {{ "pack-demo": "{url}" }},
             "pnpm": {{ "allowBuilds": {{ "pack-demo@{url}": true }} }} }}"#
    ));

    sbx.cmd().arg("install").assert().success();
    assert_eq!(
        fs::read_to_string(sbx.project.join("node_modules/pack-demo/dist/index.js"))
            .unwrap_or_default(),
        "module.exports = 9;\n",
        "postprepack built the entry point before the package was packed"
    );
}

/// A git dependency's build is decided under pnpm's exact key,
/// `<name>@git+<repository>#<commit>`, though the lockfile spells its
/// source without `git+`.
#[test]
fn git_dep_build_is_decided_under_pnpms_exact_key() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    let url = git_repo(
        &sbx,
        &[(
            "package.json",
            r#"{ "name": "post-demo", "version": "1.0.0",
                 "scripts": { "postinstall": "node -e \"require('fs').writeFileSync('BUILT', '')\"" } }"#,
        )],
    );
    let head = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(sbx.home.join("prep-repo"))
        .output()
        .unwrap();
    let key = format!(
        "post-demo@{url}#{}",
        String::from_utf8_lossy(&head.stdout).trim()
    );
    for allowed in [true, false] {
        let _ = fs::remove_dir_all(sbx.project.join("node_modules"));
        sbx.write_manifest(&format!(
            r#"{{ "name": "consumer", "version": "0.0.0", "dependencies": {{ "post-demo": "{url}" }},
                 "pnpm": {{ "allowBuilds": {{ "{key}": {allowed} }} }} }}"#
        ));
        sbx.cmd().arg("install").assert().success();
        assert_eq!(
            sbx.project.join("node_modules/post-demo/BUILT").exists(),
            allowed,
            "allowBuilds {{{key}: {allowed}}}"
        );
    }
}

/// A gzipped tarball shaped like a codeload archive: every entry under one
/// `<owner>-<repo>-<sha>/` wrapper dir.
fn codeload_tarball(wrapper: &str, files: &[(&str, &str)]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(gz);
    for (path, contents) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                format!("{wrapper}/{path}"),
                contents.as_bytes(),
            )
            .unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

/// Serve `body` to every request through a plain-HTTP forward proxy on
/// loopback, recording each request line. Lets a test reach a URL on a
/// real host name (the codeload host is what marks a tarball as a git
/// checkout) without the network.
fn serve_through_proxy(
    body: Vec<u8>,
) -> (std::net::SocketAddr, std::sync::Arc<Mutex<Vec<String>>>) {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = std::sync::Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            seen.lock().unwrap().push(line.trim_end().to_string());
            loop {
                let mut header = String::new();
                match reader.read_line(&mut header) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if header == "\r\n" || header == "\n" => break,
                    Ok(_) => {}
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    (addr, requests)
}

/// A hosted git dependency pinned to a commit is fetched and recorded as
/// its codeload archive rather than as a git clone, and that route used
/// to skip the `prepare` build the clone route runs: the install exited 0
/// with no `dist/`. `rimraf@github:isaacs/rimraf#v6.1.3` was the reported
/// case.
#[test]
fn hosted_git_tarball_dep_runs_its_prepare_script() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let tarball = codeload_tarball(
        &format!("acme-demo-{sha}"),
        &[
            (
                "package.json",
                r#"{
                    "name": "demo",
                    "version": "1.0.0",
                    "main": "dist/index.js",
                    "files": ["dist"],
                    "scripts": { "prepare": "node build.js" }
                }"#,
            ),
            (
                "build.js",
                "require('fs').mkdirSync('dist', { recursive: true });\n\
                 require('fs').writeFileSync('dist/index.js', 'module.exports = 7;\\n');\n",
            ),
        ],
    );
    let (proxy, requests) = serve_through_proxy(tarball);
    let url = format!("http://codeload.github.com/acme/demo/tar.gz/{sha}");
    let manifest = |allow: &str| {
        sbx.write_manifest(&format!(
            r#"{{ "name": "consumer", "version": "0.0.0", "dependencies": {{ "demo": "{url}" }},
                 "pnpm": {{ "allowBuilds": {{ {allow} }} }} }}"#
        ))
    };
    let install = |args: &[&str]| {
        let _ = fs::remove_dir_all(sbx.project.join("node_modules"));
        sbx.cmd()
            .arg("install")
            .args(args)
            .env_remove("https_proxy")
            .env_remove("HTTPS_PROXY")
            .env_remove("no_proxy")
            .env_remove("NO_PROXY")
            .env_remove("http_proxy")
            // Last: Windows env names are case-insensitive, so removing
            // `http_proxy` after this would remove it too.
            .env("HTTP_PROXY", format!("http://{proxy}/"))
            .assert()
    };

    manifest("");
    let out = install(&[]).code(1);
    assert!(
        String::from_utf8_lossy(&out.get_output().stderr)
            .contains("ERR_AUBE_GIT_DEP_PREPARE_NOT_ALLOWED"),
        "an archive nobody approved is not built"
    );

    // The archive is approved under the repository it was cut from.
    manifest(r#""demo@git+https://github.com/acme/demo.git": true"#);
    install(&[]).success();
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains(&url)),
        "the archive was fetched through the proxy: {:?}",
        requests.lock().unwrap()
    );
    assert_eq!(
        fs::read_to_string(sbx.project.join("node_modules/demo/dist/index.js")).unwrap_or_default(),
        "module.exports = 7;\n",
        "the archive's prepare script built dist/ before it was linked"
    );
    assert!(
        !sbx.project.join("node_modules/demo/build.js").exists(),
        "the prepared package is what `files` publishes, not the raw archive"
    );

    // Scripts disabled: the archive is linked as fetched, unbuilt.
    install(&["--ignore-scripts"]).success();
    assert!(sbx.project.join("node_modules/demo/build.js").exists());
    assert!(!sbx.project.join("node_modules/demo/dist").exists());
}

/// A member's lockfile records a `file:` dep relative to the member, which
/// hashes to a different package key than the root-relative path install
/// materialized it under. A rebuild from the member has to read the lockfile
/// back in the workspace frame, or it looks up a store entry that does not
/// exist and skips the build. The virtual store is pinned to one absolute
/// directory so the member and the root resolve the same store.
#[test]
fn member_rebuild_finds_a_local_dep_installed_under_per_project_lockfiles() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    let store = sbx.cache.join("vstore");
    let npmrc = format!("virtual-store-dir={}\n", store.display());
    sbx.write_file(".npmrc", &npmrc);
    sbx.write_file("packages/app/.npmrc", &npmrc);
    sbx.write_file(
        "pnpm-workspace.yaml",
        "packages:\n  - packages/*\nsharedWorkspaceLockfile: false\nallowBuilds:\n  \"ldir@file:vendor/ldir\": true\n",
    );
    sbx.write_manifest(r#"{ "name": "root", "version": "1.0.0", "private": true }"#);
    sbx.write_file(
        "vendor/ldir/package.json",
        r#"{
            "name": "ldir",
            "version": "1.0.0",
            "scripts": { "postinstall": "node -e \"require('fs').appendFileSync('RUNS', 'x')\"" }
        }"#,
    );
    sbx.write_file(
        "packages/app/package.json",
        r#"{ "name": "app", "version": "1.0.0", "dependencies": { "ldir": "file:../../vendor/ldir" } }"#,
    );

    sbx.cmd()
        .args(["install", "--ignore-scripts"])
        .assert()
        .success();
    let lock = fs::read_to_string(sbx.project.join("packages/app/aube-lock.yaml")).unwrap();
    assert!(
        lock.contains("file:../../vendor/ldir"),
        "the member lockfile must record the dep from the member:\n{lock}"
    );

    sbx.cmd()
        .current_dir(sbx.project.join("packages/app"))
        .args(["rebuild", "ldir"])
        .assert()
        .success();
    assert_eq!(
        read_file_under(&store, "RUNS").as_deref(),
        Some("x"),
        "`rebuild ldir` from the member must run the build of the installed copy"
    );
}

/// A per-project-lockfile member's graph holds its local paths relative to the
/// workspace root, so `licenses` from the member has to resolve them there. A
/// linked sibling's license is read live, so changing it after install shows
/// which directory answered.
#[test]
fn member_licenses_read_a_linked_sibling_under_per_project_lockfiles() {
    let _guard = e2e_lock();
    let sbx = Sandbox::new();
    sbx.write_file(
        "pnpm-workspace.yaml",
        "packages:\n  - packages/*\nsharedWorkspaceLockfile: false\n",
    );
    sbx.write_manifest(r#"{ "name": "root", "version": "1.0.0", "private": true }"#);
    sbx.write_file(
        "packages/a/package.json",
        r#"{ "name": "a", "version": "1.0.0", "license": "MIT" }"#,
    );
    sbx.write_file(
        "packages/b/package.json",
        r#"{ "name": "b", "version": "1.0.0", "dependencies": { "a": "workspace:*" } }"#,
    );
    sbx.cmd().arg("install").assert().success();
    sbx.write_file(
        "packages/a/package.json",
        r#"{ "name": "a", "version": "1.0.0", "license": "Apache-2.0" }"#,
    );

    for args in [&["licenses"][..], &["licenses", "--long"][..]] {
        let output = sbx
            .cmd()
            .current_dir(sbx.project.join("packages/b"))
            .args(args)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("Apache-2.0"),
            "{args:?} must read the sibling's current license:\n{stdout}"
        );
        assert!(
            !stdout.contains("packages/b/packages/a"),
            "{args:?} resolved the sibling against the member:\n{stdout}"
        );
    }
}

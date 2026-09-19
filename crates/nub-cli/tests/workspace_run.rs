//! `nub -r run <script>` and `nub remove --filter` workspace behaviors,
//! end-to-end through the binary against real fixture monorepos. These pin the
//! pnpm-parity contracts a workspaces differential found nub diverging on:
//!
//!   - a recursive run SKIPS packages that lack the script (exit 0), and fails
//!     only when *no* selected package has it, unless `--if-present` (as pnpm 12
//!     does);
//!   - a genuinely failing script still propagates non-zero;
//!   - a filter that matches nothing is an exit-0 no-op, not an error;
//!   - `remove --filter` on a package with a surviving `workspace:*` dep
//!     resolves that dep locally instead of hitting the registry (the
//!     critical crash: `ERR_NUB_NO_MATCHING_VERSION` for `workspace:*`).
//!
//! The script-runner tests need no install (the scripts are bare `echo`s), so
//! they run offline. The remove-seeding test does a real install and is
//! `#[ignore]`d (network) per the install-engine convention.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

fn tmp_workspace(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-ws-{tag}-{}-{}",
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

fn run_nub(dir: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env("XDG_DATA_HOME", tmp_workspace("xdg-data"))
        .env("XDG_CACHE_HOME", tmp_workspace("xdg-cache"))
        .output()
        .expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

/// A three-package monorepo: `utils` (leaf, has `build` only), `api` and `web`
/// (both have `build` + `dev`). `web` additionally declares `workspace:*` on
/// `utils` so the remove-seeding test has a local dep to resolve. No external
/// deps, so the script-runner tests need no install.
fn script_workspace(tag: &str) -> PathBuf {
    let root = tmp_workspace(tag);
    write(
        &root.join("package.json"),
        r#"{"name":"e2e-root","version":"1.0.0","private":true,"workspaces":["packages/*"]}"#,
    );
    write(
        &root.join("packages/utils/package.json"),
        r#"{"name":"utils","version":"1.0.0","scripts":{"build":"echo BUILD:utils"}}"#,
    );
    write(
        &root.join("packages/api/package.json"),
        r#"{"name":"api","version":"1.0.0","scripts":{"build":"echo BUILD:api","dev":"echo DEV:api"}}"#,
    );
    write(
        &root.join("packages/web/package.json"),
        r#"{"name":"web","version":"1.0.0","dependencies":{"utils":"workspace:*"},"scripts":{"build":"echo BUILD:web","dev":"echo DEV:web"}}"#,
    );
    root
}

#[test]
fn recursive_run_skips_packages_without_the_script_and_exits_zero() {
    let root = script_workspace("skip-missing");
    // `dev` exists in api + web but not utils. pnpm runs the two that have it
    // and exits 0; nub used to error on the missing one.
    let (stdout, stderr, code) = run_nub(&root, &["run", "-r", "dev"]);
    let combined = format!("{stdout}{stderr}");
    assert_eq!(
        code, 0,
        "missing script in one package must not fail the run\n{combined}"
    );
    assert!(
        combined.contains("DEV:api"),
        "api's dev must run\n{combined}"
    );
    assert!(
        combined.contains("DEV:web"),
        "web's dev must run\n{combined}"
    );
    assert!(
        !combined.contains("utils") || !combined.contains("missing"),
        "utils must be skipped silently, not reported as a missing-script failure\n{combined}"
    );
}

#[test]
fn recursive_run_discovers_object_form_workspace_packages() {
    let root = tmp_workspace("object-form");
    write(
        &root.join("package.json"),
        r#"{"name":"object-root","version":"1.0.0","private":true,"workspaces":{"packages":["packages/*"]}}"#,
    );
    write(
        &root.join("packages/api/package.json"),
        r#"{"name":"api","version":"1.0.0","scripts":{"who":"echo OBJECT:api"}}"#,
    );
    write(
        &root.join("packages/web/package.json"),
        r#"{"name":"web","version":"1.0.0","scripts":{"who":"echo OBJECT:web"}}"#,
    );

    let (stdout, stderr, code) = run_nub(&root, &["run", "-r", "who"]);
    let combined = format!("{stdout}{stderr}");
    assert_eq!(
        code, 0,
        "object-form workspaces must discover members for recursive run\n{combined}"
    );
    assert!(combined.contains("OBJECT:api"), "api must run\n{combined}");
    assert!(combined.contains("OBJECT:web"), "web must run\n{combined}");
}

/// A pnpm project runs the projects pnpm 12 runs. The nearest
/// `pnpm-workspace.yaml` names them — the root alone when `packages` is absent,
/// and none at all for `packages: []` — and `package.json` `workspaces` is not
/// read. With no workspace file a recursive run walks every package below the
/// project, the root included and `node_modules` excluded.
#[test]
fn a_pnpm_project_runs_the_projects_pnpm_runs() {
    const TAGS: [&str; 5] = ["root", "a", "b", "z", "hidden"];
    let package = |dir: &Path, tag: &str| {
        write(
            &dir.join("package.json"),
            &format!(
                r#"{{"name":"probe-{tag}","version":"1.0.0","scripts":{{"x":"echo RAN_{tag}"}}}}"#
            ),
        );
    };
    let ran = |tag: &str, files: &[(&str, &str)], members: &[(&str, &str)]| {
        let root = tmp_workspace(tag);
        package(&root, "root");
        for (path, contents) in files {
            write(&root.join(path), contents);
        }
        for (dir, member) in members {
            package(&root.join(dir), member);
        }
        let (stdout, stderr, code) = run_nub(&root, &["run", "-r", "x"]);
        assert_eq!(code, 0, "{tag}:\n{stdout}\n{stderr}");
        let ran: Vec<&str> = TAGS
            .into_iter()
            .filter(|tag| stdout.contains(&format!("RAN_{tag}")))
            .collect();
        (ran, stderr)
    };

    let (both, _) = ran(
        "pnpm-both",
        &[
            (
                "package.json",
                r#"{"name":"probe-root","version":"1.0.0","workspaces":["json/*"],"scripts":{"x":"echo RAN_root"}}"#,
            ),
            (
                "pnpm-workspace.yaml",
                "packages:\n  - yaml/*\nverifyDepsBeforeRun: false\n",
            ),
        ],
        &[("yaml/a", "a"), ("json/b", "b")],
    );
    assert_eq!(both, ["a"], "the workspace file names the members");

    let (walked, stderr) = ran(
        "pnpm-lockfile-only",
        &[("pnpm-lock.yaml", "lockfileVersion: '9.0'\n")],
        &[
            ("a", "a"),
            ("x/y/z", "z"),
            ("node_modules/hidden", "hidden"),
        ],
    );
    assert_eq!(
        walked,
        ["root", "a", "z"],
        "no workspace file walks the tree"
    );
    assert!(stderr.contains("Scope: all 3 projects"), "{stderr}");

    // pnpm reads `package.yaml` as a manifest, so a member written that way is
    // one of the projects it walks. It used to be dropped from the run with no
    // error, because the member list was rebuilt by re-reading `package.json`.
    let (yaml_member, _) = ran(
        "pnpm-package-yaml-member",
        &[
            (
                "pnpm-workspace.yaml",
                "packages:\n  - packages/*\nverifyDepsBeforeRun: false\n",
            ),
            (
                "packages/b/package.yaml",
                "name: probe-b\nversion: 1.0.0\nscripts:\n  x: echo RAN_b\n",
            ),
        ],
        &[("packages/a", "a")],
    );
    assert_eq!(
        yaml_member,
        ["a", "b"],
        "a `package.yaml` member is one of the projects pnpm walks"
    );

    let (root_only, _) = ran(
        "pnpm-no-packages-key",
        &[("pnpm-workspace.yaml", "verifyDepsBeforeRun: false\n")],
        &[("a", "a")],
    );
    assert_eq!(
        root_only,
        ["root"],
        "an absent `packages` is the root alone"
    );

    let (none, stderr) = ran(
        "pnpm-empty-packages",
        &[(
            "pnpm-workspace.yaml",
            "packages: []\nverifyDepsBeforeRun: false\n",
        )],
        &[("a", "a")],
    );
    assert!(none.is_empty(), "`packages: []` runs nothing: {none:?}");
    assert!(
        stderr.contains("Scope: 0 of 1 workspace projects"),
        "{stderr}"
    );
}

#[test]
fn recursive_run_with_no_matching_script_anywhere_is_an_error() {
    let root = script_workspace("none-have-it");
    // pnpm 12 fails a recursive run that found nothing to run, unless
    // `--if-present` waives it.
    let (_stdout, stderr, code) = run_nub(&root, &["run", "-r", "absent-everywhere"]);
    assert_eq!(
        code, 1,
        "no selected package has the script\nstderr: {stderr}"
    );
    assert!(
        stderr.contains(
            "RECURSIVE_RUN_NO_SCRIPT: None of the selected packages has a \"absent-everywhere\" script"
        ),
        "the error must name the script, got stderr: {stderr}"
    );
    let (_stdout, stderr, code) =
        run_nub(&root, &["run", "-r", "--if-present", "absent-everywhere"]);
    assert_eq!(code, 0, "--if-present waives the error\nstderr: {stderr}");
}

#[test]
fn recursive_run_propagates_a_real_script_failure() {
    let root = script_workspace("real-failure");
    // Give every package a `boom` that exits non-zero so the run *ran* the
    // script and it failed — distinct from a missing-script skip.
    for pkg in ["utils", "api", "web"] {
        let manifest = root.join(format!("packages/{pkg}/package.json"));
        let raw = std::fs::read_to_string(&manifest).unwrap();
        let mut json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        json["scripts"]["boom"] = serde_json::Value::String("exit 3".into());
        std::fs::write(&manifest, serde_json::to_string(&json).unwrap()).unwrap();
    }
    let (_stdout, stderr, code) = run_nub(&root, &["run", "-r", "boom"]);
    assert_ne!(
        code, 0,
        "a failing script must propagate a non-zero exit\n{stderr}"
    );
}

#[test]
fn filter_matching_no_package_is_a_clean_no_op() {
    let root = script_workspace("no-match-filter");
    let (_stdout, stderr, code) = run_nub(&root, &["run", "-F", "does-not-exist", "build"]);
    assert_eq!(
        code, 0,
        "a filter that matches nothing exits 0 (pnpm parity)\n{stderr}"
    );
    assert!(
        stderr.contains("Scope: 0 of 4 workspace projects"),
        "pnpm 12 announces the empty scope, got: {stderr}"
    );
}

#[test]
fn fail_if_no_match_turns_an_empty_filter_into_an_error() {
    let root = script_workspace("fail-if-no-match");
    let (stdout, stderr, code) = run_nub(
        &root,
        &["run", "-F", "does-not-exist", "--fail-if-no-match", "build"],
    );
    assert_eq!(
        code, 1,
        "--fail-if-no-match restores the hard error\n{stderr}"
    );
    assert!(
        stdout.contains("No projects matched the filters in"),
        "pnpm's notice goes to stdout, got: {stdout}"
    );
}

/// A directory selector resolves from the project the command runs in, as in
/// pnpm: `.` is that project and `../web` its sibling.
#[test]
fn a_directory_selector_resolves_from_the_project_it_runs_in() {
    let root = script_workspace("dir-selector");
    let api = root.join("packages/api");
    for (filter, ran, skipped) in [
        (".", "BUILD:api", "BUILD:web"),
        ("../web", "BUILD:web", "BUILD:api"),
    ] {
        let (stdout, stderr, code) = run_nub(&api, &["run", "--filter", filter, "build"]);
        let combined = format!("{stdout}{stderr}");
        assert_eq!(code, 0, "--filter {filter}\n{combined}");
        assert!(
            combined.contains(ran) && !combined.contains(skipped),
            "--filter {filter} from packages/api runs only that project\n{combined}"
        );
    }
}

/// Offline guard for the network-backed remove test.
fn registry_reachable() -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    "registry.npmjs.org:443"
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .is_some_and(|addr| {
            TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(3)).is_ok()
        })
}

/// The critical bug: `remove --filter web` re-resolves through the install
/// pipeline, which seeds the resolver with the local workspace packages, so
/// `web`'s surviving `workspace:*` dep on `utils` resolves locally instead of
/// failing against the registry with `ERR_NUB_NO_MATCHING_VERSION`. We add then
/// remove `is-positive` (a tiny real package) so the remove path runs with a
/// `workspace:*` dep still present in the manifest.
#[test]
#[ignore = "network: installs is-positive + resolves the workspace graph"]
fn filtered_remove_keeps_a_workspace_dep_resolvable() {
    if !registry_reachable() {
        eprintln!("skipping: registry.npmjs.org unreachable");
        return;
    }
    let root = script_workspace("remove-seeding");

    let (o1, e1, c1) = run_nub(&root, &["install"]);
    assert_eq!(
        c1, 0,
        "initial install must succeed\nstdout: {o1}\nstderr: {e1}"
    );

    let (o2, e2, c2) = run_nub(&root, &["add", "is-positive", "--filter", "web"]);
    assert_eq!(
        c2, 0,
        "add into web must succeed\nstdout: {o2}\nstderr: {e2}"
    );

    let (o3, e3, c3) = run_nub(&root, &["remove", "is-positive", "--filter", "web"]);
    assert_eq!(
        c3, 0,
        "remove must not fail re-resolving web's workspace:* dep on utils\nstdout: {o3}\nstderr: {e3}"
    );
    assert!(
        !format!("{o3}{e3}").contains("NO_MATCHING_VERSION"),
        "no registry-resolution failure for the workspace:* dep\nstdout: {o3}\nstderr: {e3}"
    );

    // Manifest + lockfile must both reflect the removal (atomic update): the
    // dep is gone from web's package.json and the lockfile carries no
    // is-positive entry, while the workspace:* dep survives.
    let web = std::fs::read_to_string(root.join("packages/web/package.json")).unwrap();
    assert!(
        !web.contains("is-positive"),
        "is-positive must be gone from web's manifest: {web}"
    );
    assert!(
        web.contains("workspace:*"),
        "the workspace:* dep on utils must survive: {web}"
    );
    let lock = std::fs::read_to_string(root.join("pnpm-lock.yaml")).unwrap();
    assert!(
        !lock.contains("is-positive"),
        "the lockfile must be updated in lockstep, not left stale"
    );
}

/// Regression for #281: a script that creates `node_modules/.bin/<tool>` and
/// then invokes it by bare name in the same run must succeed — the install-
/// then-invoke pattern. nub used to drop `node_modules/.bin` from the child's
/// PATH when the directory did not exist at spawn time; npm/pnpm prepend it
/// unconditionally. Unix-only: the fixture writes a `#!/bin/sh` shim, which is
/// runnable on Unix but not the `.cmd`-shim shape Windows resolves.
#[cfg(unix)]
#[test]
fn run_script_finds_bin_created_mid_run() {
    let root = tmp_workspace("bin-created-mid-run");
    write(
        &root.join("package.json"),
        r#"{"name":"mid-run","version":"1.0.0","scripts":{"go":"mkdir -p node_modules/.bin && printf '#!/bin/sh\nprintf IT-WORKS\n' > node_modules/.bin/mytool && chmod +x node_modules/.bin/mytool && mytool"}}"#,
    );
    // No node_modules/.bin exists at spawn — the script creates it, then calls
    // `mytool` by bare name, which resolves only if `.bin` is on PATH.
    let (stdout, stderr, code) = run_nub(&root, &["run", "go"]);
    let combined = format!("{stdout}{stderr}");
    assert_eq!(
        code, 0,
        "bare-name invocation of a mid-run-created .bin tool must succeed\n{combined}"
    );
    assert!(
        stdout.contains("IT-WORKS"),
        "the mid-run-created tool must run and print its output\n{combined}"
    );
}

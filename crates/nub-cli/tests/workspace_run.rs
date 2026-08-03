//! `nub -r run <script>` and `nub remove --filter` workspace behaviors,
//! end-to-end through the binary against real fixture monorepos. These pin the
//! pnpm-parity contracts a workspaces differential found nub diverging on:
//!
//!   - a recursive run SKIPS packages that lack the script (exit 0), and only
//!     prints an informational notice — never fails — when *no* selected
//!     package has it (matching pnpm 10.x, which exits 0 there);
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

/// A recursive workspace whose later member carries a malformed Yarn route.
/// The root's hard freshness policy makes this fixture distinguish ordering:
/// malformed member config must win before verify-deps can abort, and before
/// either the sequential branch or a parallel worker launches a script.
fn malformed_later_member_config_workspace(tag: &str) -> PathBuf {
    let root = tmp_workspace(tag);
    write(
        &root.join("package.json"),
        r#"{"name":"registry-preflight-root","version":"1.0.0","private":true,"packageManager":"yarn@4.2.2","workspaces":["packages/*"],"dependencies":{"missing":"1.0.0"}}"#,
    );
    write(&root.join(".npmrc"), "verify-deps-before-run=error\n");
    for member in ["first", "later"] {
        write(
            &root.join(format!("packages/{member}/package.json")),
            &format!(
                r#"{{"name":"{member}","version":"1.0.0","scripts":{{"gated":"echo started > ../../script-started"}}}}"#
            ),
        );
    }
    write(
        &root.join("packages/later/.yarnrc.yml"),
        "npmRegistryServer: ftp://registry.invalid/\n",
    );
    root
}

/// A workspace whose members deliberately select different configuration
/// adapters. The root pnpm route is a sentinel: a worker that reconstructs its
/// engine context from the process cwd will export it for every member instead
/// of the Yarn/Bun routes captured during member preflight.
fn mixed_pm_registry_workspace(tag: &str, include_invalid_member: bool) -> PathBuf {
    let root = tmp_workspace(tag);
    write(
        &root.join("package.json"),
        r#"{"name":"mixed-registry-root","version":"1.0.0","private":true,"packageManager":"pnpm@10.0.0","workspaces":["packages/*"]}"#,
    );
    write(
        &root.join(".npmrc"),
        "registry=https://root.registry.example/\n",
    );
    write(
        &root.join("packages/yarn/package.json"),
        r#"{"name":"yarn-member","version":"1.0.0","packageManager":"yarn@4.2.2","scripts":{"route":"printf '%s' \"$npm_config_registry\" > ../../yarn-registry; printf '%s' \"$npm_config_user_agent\" > ../../yarn-ua"}}"#,
    );
    write(
        &root.join("packages/yarn/.yarnrc.yml"),
        "npmRegistryServer: https://yarn.member.example/\n",
    );
    write(
        &root.join("packages/bun/package.json"),
        r#"{"name":"bun-member","version":"1.0.0","packageManager":"bun@1.1.0","scripts":{"route":"printf '%s' \"$npm_config_registry\" > ../../bun-registry; printf '%s' \"$npm_config_user_agent\" > ../../bun-ua"}}"#,
    );
    write(
        &root.join("packages/bun/bunfig.toml"),
        "[install]\nregistry = \"https://bun-user:bun-secret@bun.member.example/\"\n",
    );
    if include_invalid_member {
        write(
            &root.join("packages/invalid/package.json"),
            r#"{"name":"invalid-member","version":"1.0.0","packageManager":"yarn@4.2.2","scripts":{"route":"echo should-not-run > ../../invalid-ran"}}"#,
        );
        write(
            &root.join("packages/invalid/.yarnrc.yml"),
            "npmRegistryServer: ftp://registry.invalid/\n",
        );
    }
    root
}

#[test]
fn recursive_run_preflights_later_member_config_before_dependencies_or_work() {
    for (tag, args) in [
        ("config-preflight-sequential", &["run", "-r", "gated"][..]),
        (
            "config-preflight-parallel",
            &["run", "-r", "--parallel", "gated"][..],
        ),
    ] {
        let root = malformed_later_member_config_workspace(tag);
        let marker = root.join("script-started");
        let (stdout, stderr, code) = run_nub(&root, args);
        let combined = format!("{stdout}{stderr}");

        assert_ne!(code, 0, "{tag}: the malformed member must fail\n{combined}");
        assert!(
            combined.contains("invalid configured registry URL"),
            "{tag}: member config must fail before execution\n{combined}"
        );
        assert!(
            !combined.contains("dependencies are out of date"),
            "{tag}: verify-deps must not run before all member config is valid\n{combined}"
        );
        assert!(
            !marker.exists(),
            "{tag}: no sequential or parallel script may launch before config preflight\n{combined}"
        );
    }
}

#[test]
fn recursive_parallel_run_uses_each_members_pm_registry_snapshot() {
    let root = mixed_pm_registry_workspace("member-registry-snapshots", false);
    let (stdout, stderr, code) = run_nub(&root, &["run", "-r", "--parallel", "route"]);
    let combined = format!("{stdout}{stderr}");

    assert_eq!(
        code, 0,
        "member routes must launch successfully\n{combined}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("yarn-registry")).unwrap(),
        "https://yarn.member.example/",
        "the Yarn member command must receive its preflighted route"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("bun-registry")).unwrap(),
        "https://bun.member.example/",
        "the Bun member command must receive its preflighted route without URL userinfo"
    );
}

#[test]
fn recursive_mixed_pm_run_uses_each_members_lifecycle_user_agent() {
    let root = mixed_pm_registry_workspace("member-lifecycle-ua", false);
    let (stdout, stderr, code) = run_nub(&root, &["run", "-r", "--parallel", "route"]);
    let combined = format!("{stdout}{stderr}");

    assert_eq!(code, 0, "member scripts must run\n{combined}");
    let node_version = Command::new("node")
        .arg("--version")
        .output()
        .expect("resolve test Node version");
    assert!(node_version.status.success(), "Node must be executable");
    let node_version = String::from_utf8(node_version.stdout)
        .expect("Node version is UTF-8")
        .trim()
        .trim_start_matches('v')
        .to_owned();
    for (member, expected_pm) in [("yarn", "yarn/4.2.2"), ("bun", "bun/1.1.0")] {
        let ua = std::fs::read_to_string(root.join(format!("{member}-ua"))).unwrap();
        assert!(
            ua.starts_with(expected_pm),
            "{member} member must retain its PM identity: {ua}"
        );
        assert!(
            ua.contains(&format!("node/v{node_version}")),
            "{member} member must retain the resolved Node version: {ua}"
        );
    }
}

#[test]
fn direct_run_uses_declared_lifecycle_user_agent() {
    let root = tmp_workspace("direct-lifecycle-ua");
    write(
        &root.join("package.json"),
        r#"{"name":"direct-ua","version":"1.0.0","packageManager":"yarn@4.2.2","scripts":{"route":"printf '%s' \"$npm_config_user_agent\" > user-agent"}}"#,
    );
    let (stdout, stderr, code) = run_nub(&root, &["run", "route"]);
    assert_eq!(code, 0, "direct script must run\n{stdout}{stderr}");
    let ua = std::fs::read_to_string(root.join("user-agent")).unwrap();
    assert!(
        ua.starts_with("yarn/4.2.2 nub/"),
        "direct script must advertise declared PM: {ua}"
    );
    assert!(ua.contains(" node/v"), "direct UA must include Node: {ua}");
}

#[test]
fn recursive_mixed_pm_preflight_rejects_any_invalid_member_before_commands() {
    let root = mixed_pm_registry_workspace("member-registry-invalid", true);
    let (stdout, stderr, code) = run_nub(&root, &["run", "-r", "--parallel", "route"]);
    let combined = format!("{stdout}{stderr}");

    assert_ne!(code, 0, "invalid member route must fail\n{combined}");
    assert!(
        combined.contains("invalid configured registry URL"),
        "the invalid member route must be reported before execution\n{combined}"
    );
    for marker in ["yarn-registry", "bun-registry", "invalid-ran"] {
        assert!(
            !root.join(marker).exists(),
            "no member command may run when any selected route is invalid\n{combined}"
        );
    }
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

#[test]
fn recursive_run_with_no_matching_script_anywhere_notifies_and_exits_zero() {
    let root = script_workspace("none-have-it");
    let (stdout, stderr, code) = run_nub(&root, &["run", "-r", "absent-everywhere"]);
    // pnpm 10.x prints "None of the selected packages has a ..." on stdout and
    // exits 0 — it's informational, not a failure.
    assert_eq!(
        code, 0,
        "all-missing recursive run matches pnpm's exit 0\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("None of the selected packages has a \"absent-everywhere\" script"),
        "the pnpm-style notice must print on stdout, got stdout: {stdout}"
    );
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
        stderr.contains("No projects matched the filters"),
        "the pnpm-style no-match message must surface, got: {stderr}"
    );
}

#[test]
fn fail_if_no_match_turns_an_empty_filter_into_an_error() {
    let root = script_workspace("fail-if-no-match");
    let (_stdout, stderr, code) = run_nub(
        &root,
        &["run", "-F", "does-not-exist", "--fail-if-no-match", "build"],
    );
    assert_ne!(
        code, 0,
        "--fail-if-no-match restores the hard error\n{stderr}"
    );
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

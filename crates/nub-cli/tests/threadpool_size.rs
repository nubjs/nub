//! libuv threadpool sizing — `UV_THREADPOOL_SIZE` set to `max(4, cores)` on an
//! augmented run.
//!
//! Node reads the variable once at startup, so the spawn is the only place it
//! can be set; nub sets it when the user has not, and leaves a user value alone.
//! Under `--node` / `NODE_COMPAT` the variable is absent, the plain-Node
//! fingerprint (libuv's own default of 4). A value from an env file is the
//! user's too, on every launch path, while a shell value still beats the file.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

fn fixture() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    Path::new(&manifest).join("../../tests/fixtures/threadpool/size.js")
}

/// Run the fixture under `nub [extra_args] [env]` and parse its JSON.
fn run(extra_args: &[&str], env: &[(&str, &str)]) -> serde_json::Value {
    let f = fixture();
    let mut cmd = Command::new(nub_binary());
    cmd.args(extra_args)
        .arg(&f)
        .current_dir(f.parent().unwrap())
        .env_remove("UV_THREADPOOL_SIZE");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("failed to spawn nub");
    assert!(
        output.status.success(),
        "nub exited {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim())
        .expect("fixture must emit valid JSON")
}

/// Augmented run: the pool is at least libuv's default of 4 and never exceeds the
/// parallelism Node itself reports (the two runtimes read a cgroup quota
/// differently, so the exact value is not pinned).
#[test]
fn augmented_sizes_pool_to_cores() {
    let v = run(&[], &[]);
    let size: usize = v["size"]
        .as_str()
        .expect("UV_THREADPOOL_SIZE must be set on an augmented run")
        .parse()
        .expect("UV_THREADPOOL_SIZE must be an integer");
    let cores = v["cores"].as_u64().unwrap() as usize;
    assert!(
        size >= 4,
        "pool must be at least libuv's default, got {size}"
    );
    assert!(
        size <= cores.max(4),
        "pool must not exceed max(4, cores={cores}), got {size}"
    );
    // The pool libuv really built, not the value nub installed: libuv reads the
    // variable at first use, so a preload that hid it too early would leave four.
    if cfg!(target_os = "linux") {
        assert_eq!(
            v["demoted"].as_u64(),
            Some(size.saturating_sub(4) as u64),
            "every worker beyond four runs at nice 10: {v}"
        );
        if names_workers(&v) {
            assert_eq!(
                v["workers"].as_u64(),
                Some(size as u64),
                "libuv must build the installed pool: {v}"
            );
        }
    }
}

/// libuv names its workers from 1.50 (Node 22.22+, 24+); before that the fixture
/// can only count the demoted threads.
fn names_workers(v: &serde_json::Value) -> bool {
    let mut parts = v["uv"].as_str().unwrap_or("0.0").split('.');
    let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (major, minor) >= (1, 50)
}

/// `nub run` goes through the shared script-runner environment rather than the
/// direct spawn, so it is covered on its own: the script's `node` child carries
/// the same value a direct run gets.
#[test]
fn run_script_children_get_the_same_pool() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(fixture(), dir.path().join("size.js")).unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{ "name": "tp", "private": true, "scripts": { "probe": "node size.js" } }"#,
    )
    .unwrap();
    let mut cmd = Command::new(nub_binary());
    cmd.args(["run", "probe"])
        .current_dir(dir.path())
        .env_remove("UV_THREADPOOL_SIZE");
    let output = cmd.output().expect("failed to spawn nub");
    assert!(
        output.status.success(),
        "nub run exited {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .expect("fixture JSON line in `nub run` output");
    let v: serde_json::Value = serde_json::from_str(json.trim()).unwrap();
    let direct = run(&[], &[]);
    assert_eq!(
        v["size"], direct["size"],
        "`nub run` must size the pool exactly as a direct run does"
    );
}

/// A value the user set is theirs, whatever the core count.
#[test]
fn user_value_is_never_overwritten() {
    let v = run(&[], &[("UV_THREADPOOL_SIZE", "3")]);
    assert_eq!(v["size"].as_str(), Some("3"));
}

/// A project whose `.env` sets the pool, with both fixtures and a `probe` script.
fn project_with_env_file() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for name in ["size.js", "watch-size.js"] {
        std::fs::copy(fixture().with_file_name(name), dir.path().join(name)).unwrap();
    }
    std::fs::write(
        dir.path().join("package.json"),
        r#"{ "name": "tp", "private": true, "scripts": { "probe": "node size.js" } }"#,
    )
    .unwrap();
    std::fs::write(dir.path().join(".env"), "UV_THREADPOOL_SIZE=3\n").unwrap();
    dir
}

/// The first JSON line `nub <args>` prints from `dir`, killed if it outlives
/// `limit` (a watch supervisor never exits on its own).
fn first_json_line(dir: &Path, args: &[&str], env: &[(&str, &str)], limit: Duration) -> String {
    let mut cmd = Command::new(nub_binary());
    cmd.args(args)
        .current_dir(dir)
        .env_remove("UV_THREADPOOL_SIZE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("failed to spawn nub");
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.trim_start().starts_with('{') {
                let _ = tx.send(line);
                break;
            }
        }
    });
    let line = rx.recv_timeout(limit);
    let _ = child.kill();
    let _ = child.wait();
    line.unwrap_or_else(|_| {
        panic!(
            "no JSON line from `nub {}` within {limit:?}",
            args.join(" ")
        )
    })
}

/// `.env` is the user's value on a direct run, and the shell still beats it.
#[test]
fn env_file_value_wins_on_a_direct_run_but_not_over_the_shell() {
    let dir = project_with_env_file();
    let from_file = first_json_line(dir.path(), &["size.js"], &[], Duration::from_secs(60));
    let v: serde_json::Value = serde_json::from_str(&from_file).unwrap();
    assert_eq!(
        v["size"].as_str(),
        Some("3"),
        "the .env value must reach the script"
    );
    let from_shell = first_json_line(
        dir.path(),
        &["size.js"],
        &[("UV_THREADPOOL_SIZE", "7")],
        Duration::from_secs(60),
    );
    let v: serde_json::Value = serde_json::from_str(&from_shell).unwrap();
    assert_eq!(
        v["size"].as_str(),
        Some("7"),
        "a shell value must beat the .env value"
    );
}

/// `nub run` installs nub's default, then the script's `node` re-enters nub
/// through the shim, where `.env` is loaded: the installed default must read as
/// nub's, not as a shell value the file may not touch.
#[test]
fn env_file_value_beats_the_installed_default_under_run() {
    let dir = project_with_env_file();
    let line = first_json_line(dir.path(), &["run", "probe"], &[], Duration::from_secs(60));
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["size"].as_str(), Some("3"));
}

/// A non-Node local bin under `nub exec` has its `--env-file` values staged
/// before augmentation runs; the pool default must not land on top of them.
#[cfg(unix)]
#[test]
fn env_file_value_survives_exec_of_a_non_node_bin() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{ "name": "tp", "private": true }"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("custom.env"), "UV_THREADPOOL_SIZE=5\n").unwrap();
    let bin_dir = dir.path().join("node_modules").join(".bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let bin = bin_dir.join("pool-echo");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
echo "{\"size\":\"$UV_THREADPOOL_SIZE\"}"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let from_file = first_json_line(
        dir.path(),
        &["--env-file=custom.env", "exec", "pool-echo"],
        &[],
        Duration::from_secs(60),
    );
    let v: serde_json::Value = serde_json::from_str(&from_file).unwrap();
    assert_eq!(
        v["size"].as_str(),
        Some("5"),
        "the --env-file value must reach the bin"
    );
    let plain = first_json_line(
        dir.path(),
        &["exec", "pool-echo"],
        &[],
        Duration::from_secs(60),
    );
    let v: serde_json::Value = serde_json::from_str(&plain).unwrap();
    let size: usize = v["size"]
        .as_str()
        .unwrap()
        .parse()
        .expect("nub's default must be a number");
    assert!(
        size >= 4,
        "without a file the bin gets nub's default, got {size}"
    );
}

/// Windows environment keys are case-insensitive, so a differently cased key in
/// `.env` is the same user value, at the nested `nub run` boundary as well.
#[cfg(windows)]
#[test]
fn env_file_key_case_is_folded_on_windows() {
    let dir = project_with_env_file();
    std::fs::write(dir.path().join(".env"), "uv_threadpool_size=3\n").unwrap();
    for args in [&["size.js"][..], &["run", "probe"][..]] {
        let line = first_json_line(dir.path(), args, &[], Duration::from_secs(60));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            v["size"].as_str(),
            Some("3"),
            "under `nub {}`",
            args.join(" ")
        );
    }
}

/// Watch forwards `.env` to Node's own `--env-file`, which never overrides a
/// value already in the command environment, so nub must not pre-install one.
#[test]
fn env_file_value_wins_in_watch_mode() {
    let dir = project_with_env_file();
    let line = first_json_line(
        dir.path(),
        &["watch", "watch-size.js"],
        &[],
        Duration::from_secs(90),
    );
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["size"].as_str(), Some("3"));
}

/// `--node` is plain Node: the variable is absent.
#[test]
fn node_compat_flag_leaves_pool_alone() {
    let v = run(&["--node"], &[]);
    assert!(
        v["size"].is_null(),
        "--node must not set UV_THREADPOOL_SIZE, got {:?}",
        v["size"]
    );
}

/// `NODE_COMPAT=1` is the tree-wide opt-out; same contract as `--node`.
#[test]
fn node_compat_env_leaves_pool_alone() {
    let v = run(&[], &[("NODE_COMPAT", "1")]);
    assert!(
        v["size"].is_null(),
        "NODE_COMPAT=1 must not set UV_THREADPOOL_SIZE, got {:?}",
        v["size"]
    );
}

/// Run a named fixture from the threadpool fixture dir under `nub [extra_args] [env]`.
fn run_named(name: &str, extra_args: &[&str], env: &[(&str, &str)]) -> serde_json::Value {
    let f = fixture().with_file_name(name);
    let mut cmd = Command::new(nub_binary());
    cmd.args(extra_args)
        .arg(&f)
        .current_dir(f.parent().unwrap())
        .env_remove("UV_THREADPOOL_SIZE")
        .env("NUB_BIN", nub_binary());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("failed to spawn nub");
    assert!(
        output.status.success(),
        "nub exited {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim())
        .expect("fixture must emit valid JSON")
}

/// Nub's own value is for this process only: the preload deletes it from
/// `process.env`, so a plain `node` child and a cluster worker start with Node's
/// default, while a child launched through nub is sized again.
#[test]
fn children_keep_nodes_default_and_a_nub_child_is_sized_again() {
    let v = run_named("children.js", &[], &[]);
    assert!(
        v["parent"].is_null(),
        "the preload must strip nub's value from process.env, got {:?}",
        v["parent"]
    );
    assert_eq!(
        v["node"].as_str(),
        Some("null"),
        "a node child must not inherit nub's pool size"
    );
    assert!(
        v["cluster"].is_null(),
        "a cluster worker must not inherit nub's pool size, got {:?}",
        v["cluster"]
    );
    let nub: usize = v["nub"]
        .as_str()
        .expect("a nub child must be sized again")
        .parse()
        .unwrap();
    assert!(
        nub >= 4,
        "a nub child must be sized to at least 4, got {nub}"
    );
}

/// A value the user set inherits exactly as it does under plain Node.
#[test]
fn user_value_still_inherits() {
    let v = run_named("children.js", &[], &[("UV_THREADPOOL_SIZE", "3")]);
    assert_eq!(v["parent"].as_str(), Some("3"));
    assert_eq!(v["node"].as_str(), Some("3"));
    assert_eq!(v["cluster"].as_str(), Some("3"));
    assert_eq!(v["nub"].as_str(), Some("3"));
}

/// The workers beyond Node's four run at nice 10 on Linux, so on a busy box they
/// only take idle cycles. CI's runners have 4 cores, where nub sizes nothing to
/// demote, so this drives the preload under plain `node` with the environment a
/// launcher hands it: the value, its augmented marker, and a compat capture saying
/// the variable was absent before nub.
#[cfg(target_os = "linux")]
fn demotion_under_plain_node(entry: &[&std::ffi::OsStr]) -> serde_json::Value {
    let f = fixture();
    let output = Command::new("node")
        .args(entry)
        .arg(&f)
        .current_dir(f.parent().unwrap())
        .env("UV_THREADPOOL_SIZE", "8")
        .env("__NUB_AUGMENTED_UV_THREADPOOL_SIZE", "8")
        .env("__NUB_AUGMENTED_UV_THREADPOOL_SIZE_PRESENT", "1")
        .env("__NUB_COMPAT_UV_THREADPOOL_SIZE", "")
        .env("__NUB_COMPAT_PRESENT", "0")
        .output()
        .expect("failed to spawn node");
    assert!(
        output.status.success(),
        "node exited {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).expect("json");
    assert_eq!(v["size"].as_str(), Some("8"));
    assert!(
        v["env"].is_null(),
        "nub's value must be stripped from process.env: {v}"
    );
    assert_eq!(
        v["demoted"].as_u64(),
        Some(4),
        "workers 5..8 must run at nice 10: {v}"
    );
    if names_workers(&v) {
        assert_eq!(
            v["workers"].as_u64(),
            Some(8),
            "libuv must build the sized pool: {v}"
        );
        let nices: Vec<i64> = v["nices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_i64().unwrap())
            .collect();
        assert_eq!(nices, vec![0, 0, 0, 0, 10, 10, 10, 10], "{v}");
    }
    v
}

#[cfg(target_os = "linux")]
fn runtime_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime")
        .join(name)
        .canonicalize()
        .unwrap()
}

/// The fast tier: the `--require` preload runs before any pool use, so its own
/// thread-id snapshot precedes the pool.
#[cfg(target_os = "linux")]
#[test]
fn extra_workers_run_at_low_priority() {
    let preload = runtime_file("preload.cjs");
    demotion_under_plain_node(&["--require".as_ref(), preload.as_os_str()]);
}

/// The compat tier: Node's ESM loader reads the `--import` preload through the
/// pool, so the pool exists before that preload runs and, before libuv 1.50, its
/// workers carry no name. The launcher's `--require` sidecar takes the snapshot
/// first; without it nothing is demoted on the Node versions CI runs.
#[cfg(target_os = "linux")]
#[test]
fn extra_workers_run_at_low_priority_on_the_compat_tier() {
    let sidecar = runtime_file("threadpool-snapshot.cjs");
    let preload = format!("file://{}", runtime_file("preload.mjs").display());
    demotion_under_plain_node(&[
        "--require".as_ref(),
        sidecar.as_os_str(),
        "--import".as_ref(),
        preload.as_ref(),
    ]);
}

/// A user's value is never demoted: nub did not size that pool, so it does not
/// touch its threads.
#[cfg(target_os = "linux")]
#[test]
fn a_user_sized_pool_is_not_demoted() {
    let v = run(&[], &[("UV_THREADPOOL_SIZE", "6")]);
    assert_eq!(v["env"].as_str(), Some("6"));
    assert_eq!(v["demoted"].as_u64(), Some(0), "{v}");
    if names_workers(&v) {
        assert_eq!(v["workers"].as_u64(), Some(6), "{v}");
    }
}

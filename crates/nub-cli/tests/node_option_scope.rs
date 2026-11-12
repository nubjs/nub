//! How `nub <file>` reads the Node command line it is handed: an argument after
//! the entry belongs to the program, even when it is spelled like one of Node's
//! flags, and Node's permission model can start with Nub's runtime readable.

use std::path::PathBuf;
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

fn tmp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-node-option-scope-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_nub(dir: &std::path::Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env_remove("NODE_COMPAT")
        .env_remove("NODE_OPTIONS")
        .output()
        .expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn a_permission_flag_after_the_entry_is_the_programs_argument() {
    let dir = probe_dir("permission");
    std::fs::write(
        dir.join("app.js"),
        "console.log(JSON.stringify(process.argv.slice(2)));\n",
    )
    .unwrap();

    let (stdout, stderr, code) = run_nub(&dir, &["app.js", "--permission"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(stdout.trim(), r#"["--permission"]"#, "stderr: {stderr}");

    // The same flag ahead of the entry is Node's, and Nub refuses it for want of
    // `--allow-addons`. Node 20.0-20.11 and 21.0-21.5 have the permission model
    // without that flag, so the refusal there names no flag to add.
    let (stdout, stderr, code) = run_nub(&dir, &["--permission", "app.js"]);
    assert_ne!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let version = node_version(&probe(&dir, &["probe.js"]));
    let refusal = if ((20, 0)..(20, 12)).contains(&version) || ((21, 0)..(21, 6)).contains(&version)
    {
        "--permission blocks Nub's native addon"
    } else {
        "--permission requires --allow-addons"
    };
    assert!(stderr.contains(refusal), "stderr: {stderr}");
}

/// Under the permission model the program may read only what the user granted
/// plus what Nub grants for its own runtime. The runtime is not under the install
/// directory (a shipped build extracts it to the cache, a dev build reads the
/// in-repo `runtime/`), so a grant of the install directory alone denied the
/// preload and the run died with `ERR_ACCESS_DENIED`.
#[test]
fn the_permission_model_starts_with_nubs_runtime_readable() {
    let dir = probe_dir("permission-runtime");
    // `--permission` is the stable spelling from 22.13 and 23.5; 23.0-23.4 and
    // older lines know only `--experimental-permission`.
    let version = node_version(&probe(&dir, &["probe.js"]));
    if !(version >= (23, 5) || ((22, 13)..(23, 0)).contains(&version)) {
        eprintln!("skipping: Node {version:?} has no --permission");
        return;
    }
    std::fs::write(
        dir.join("app.js"),
        "console.log(JSON.stringify(process.argv.slice(2)));\n",
    )
    .unwrap();
    let grant = format!("--allow-fs-read={}", dir.display());
    let mut args = vec!["--permission", "--allow-addons", grant.as_str()];
    // Before 22.15 Nub's hooks run on a loader worker thread, which the
    // permission model gates on its own flag. That grant is the user's to make.
    if version < (22, 15) {
        args.push("--allow-worker");
    }
    args.extend(["app.js", "x"]);

    let (stdout, stderr, code) = run_nub(&dir, &args);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(stdout.trim(), r#"["x"]"#, "stderr: {stderr}");
}

const PROBE: &str = r#"console.log(JSON.stringify({
  argv: process.argv.slice(2),
  node: process.versions.node,
  vm: typeof require("vm").SourceTextModule,
  exclude: process.execArgv.some((a) => a.startsWith("--test-coverage-exclude=")),
  storage: typeof globalThis.sessionStorage,
  runtimeV8: process.env.__NUB_RUNTIME_V8_FLAGS ?? null,
}));
"#;

/// Run the probe under `nub <args>` and parse what it printed.
fn probe(dir: &std::path::Path, args: &[&str]) -> serde_json::Value {
    let (stdout, stderr, code) = run_nub(dir, args);
    assert_eq!(code, 0, "{args:?}\nstdout: {stdout}\nstderr: {stderr}");
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("{args:?}: {e}\nstdout: {stdout}\nstderr: {stderr}"))
}

fn probe_dir(tag: &str) -> PathBuf {
    let dir = tmp_dir(tag);
    std::fs::write(dir.join("probe.js"), PROBE).unwrap();
    dir
}

fn node_version(report: &serde_json::Value) -> (u32, u32) {
    let mut parts = report["node"]
        .as_str()
        .expect("the probe reports its Node version")
        .split('.')
        .map(|part| part.parse::<u32>().unwrap());
    (parts.next().unwrap(), parts.next().unwrap())
}

#[test]
fn a_negation_after_the_entry_keeps_nubs_injected_flag() {
    let dir = probe_dir("negation");
    let after = probe(&dir, &["probe.js", "--no-experimental-vm-modules"]);
    assert_eq!(after["vm"], "function", "{after}");
    assert_eq!(after["argv"][0], "--no-experimental-vm-modules", "{after}");

    let before = probe(&dir, &["--no-experimental-vm-modules", "probe.js"]);
    assert_eq!(before["vm"], "undefined", "{before}");
}

#[test]
fn a_coverage_flag_after_the_entry_is_not_a_coverage_run() {
    let dir = probe_dir("coverage");
    let after = probe(&dir, &["probe.js", "--experimental-test-coverage"]);
    assert_eq!(after["exclude"], false, "{after}");
    assert_eq!(after["argv"][0], "--experimental-test-coverage", "{after}");

    // Nub adds its runtime exclude on argv only where Node has the flag (22.5+).
    if node_version(&after) >= (22, 5) {
        let before = probe(&dir, &["--experimental-test-coverage", "probe.js"]);
        assert_eq!(before["exclude"], true, "{before}");
    }
}

#[test]
fn a_webstorage_negation_after_the_entry_keeps_session_storage() {
    let dir = probe_dir("webstorage");
    let after = probe(&dir, &["probe.js", "--no-experimental-webstorage"]);
    // Nub injects `--experimental-webstorage` only on 22.4 through 24.
    let version = node_version(&after);
    if !((22, 4)..(25, 0)).contains(&version) {
        eprintln!("skipping: Node {version:?} is outside the injected band");
        return;
    }
    assert_eq!(after["storage"], "object", "{after}");
    assert_eq!(after["argv"][0], "--no-experimental-webstorage", "{after}");

    let before = probe(&dir, &["--no-experimental-webstorage", "probe.js"]);
    assert_eq!(before["storage"], "undefined", "{before}");
}

#[test]
fn a_runtime_v8_negation_after_the_entry_keeps_the_preload_armed() {
    let dir = probe_dir("runtime-v8");
    let after = probe(&dir, &["probe.js", "--no-js-defer-import-eval"]);
    // The `import defer` flip is armed from Node 26.4.
    if node_version(&after) < (26, 4) {
        eprintln!("skipping: Node {} has no runtime V8 flag", after["node"]);
        return;
    }
    let armed = after["runtimeV8"].as_str().unwrap_or_default();
    assert!(armed.contains("--js-defer-import-eval"), "{after}");
    assert_eq!(after["argv"][0], "--no-js-defer-import-eval", "{after}");

    let before = probe(&dir, &["--no-js-defer-import-eval", "probe.js"]);
    assert_eq!(before["runtimeV8"], serde_json::Value::Null, "{before}");
}

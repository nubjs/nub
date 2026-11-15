//! The preload's hooks keep working after user code has altered the intrinsics
//! they might lean on.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_file(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
        .canonicalize()
        .unwrap()
}

fn node_version() -> String {
    let out = Command::new("node")
        .args(["-p", "process.versions.node"])
        .output()
        .expect("failed to spawn node");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A script that deletes `Array.prototype[Symbol.iterator]` and then imports still
/// gets its module. The load hook scans every result for a pending runtime V8 flag
/// while any is armed, and a scan that spread its list threw on the deleted
/// iterator and failed the import. The launcher arms the only such flag from Node
/// 26.4, so this drives the preload under plain `node` and arms it by hand, stamped
/// with this Node's version as the launcher would; the fixture uses no `import
/// defer`, so the flag is scanned for and never turned on. The compat tier runs its
/// hooks in a loader worker with intrinsics of its own, so only the fast tier can
/// reach the failure.
#[test]
fn load_hook_survives_a_deleted_array_iterator() {
    let f = repo_file("tests/fixtures/runtime-startup/delete-array-iterator.js");
    let output = Command::new("node")
        .arg("--require")
        .arg(repo_file("runtime/preload.cjs"))
        .arg(&f)
        .current_dir(f.parent().unwrap())
        .env(
            "__NUB_RUNTIME_V8_FLAGS",
            format!("{} --js-defer-import-eval", node_version()),
        )
        .output()
        .expect("failed to spawn node");
    assert!(
        output.status.success(),
        "node exited {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        r#"{"ok":true}"#
    );
}

/// With require(esm) off and no `module.register()`, both `--require` entries start
/// the program unhooked instead of dying in the preload. That is Node 28 under
/// `--no-experimental-require-module`: the flag leaves the preload no synchronous way
/// to load its transform core, and the loader worker it fell back to is gone. A shim
/// required first deletes `register` so any Node in the matrix can stand in for 28.
#[test]
fn preloads_start_unhooked_without_require_esm_or_module_register() {
    let major: u32 = node_version().split('.').next().unwrap().parse().unwrap();
    if major < 22 {
        eprintln!("skipping: Node {major} predates --no-experimental-require-module");
        return;
    }
    let entry = repo_file("tests/fixtures/runtime-startup/entry.js");
    for preload in ["runtime/preload.cjs", "runtime/loader-register.cjs"] {
        let output = Command::new("node")
            .arg("--no-experimental-require-module")
            .arg("--require")
            .arg(repo_file(
                "tests/fixtures/runtime-startup/no-module-register.cjs",
            ))
            .arg("--require")
            .arg(repo_file(preload))
            .arg(&entry)
            .current_dir(entry.parent().unwrap())
            .env_remove("NODE_OPTIONS")
            .output()
            .expect("failed to spawn node");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{preload}: node exited {:?}\nstderr: {stderr}",
            output.status
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            r#"{"ok":true}"#,
            "{preload}: stderr: {stderr}"
        );
        assert!(
            stderr.contains("no module.register() to fall back to"),
            "{preload}: the unhooked start must say why; stderr: {stderr}"
        );
    }
}

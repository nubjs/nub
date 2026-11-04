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

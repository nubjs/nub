//! The preload's hooks keep working after user code has altered the intrinsics
//! they might lean on.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

fn fixture(name: &str) -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    Path::new(&manifest)
        .join("../../tests/fixtures/runtime-startup")
        .join(name)
}

/// A script that deletes `Array.prototype[Symbol.iterator]` and then imports still
/// gets its module: the load hook's runtime-flag scan walks its list by index.
#[test]
fn load_hook_survives_a_deleted_array_iterator() {
    let f = fixture("delete-array-iterator.js");
    let output = Command::new(nub_binary())
        .arg(&f)
        .current_dir(f.parent().unwrap())
        .output()
        .expect("failed to spawn nub");
    assert!(
        output.status.success(),
        "nub exited {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        r#"{"ok":true}"#
    );
}

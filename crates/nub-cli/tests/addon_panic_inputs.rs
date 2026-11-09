//! The N-API addon runs inside the user's Node process, so any input that reaches
//! a panic in it takes that process down with an abort instead of a catchable
//! error. These drive such inputs through the binary and assert the import fails,
//! or succeeds, the way plain Node's resolution would.

use std::path::PathBuf;
use std::process::Command;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

fn host_node_usable() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

/// `extname` reads past a `?query`, so for `./b.js?😀` it reports `.js` while the
/// path does not end in it. A `.js`→`.ts` swap that slices `.js` off the END of
/// such a path cuts the four-byte emoji in half, which is a panic. The specifier
/// names no file, so Node's own not-found is the answer.
#[test]
fn a_query_after_a_js_extension_falls_through_to_node_instead_of_aborting() {
    if !host_node_usable() {
        eprintln!("skipping: no usable `node` on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("b.ts"), "export default \"b\";\n").unwrap();
    std::fs::write(
        dir.path().join("main.ts"),
        "import x from \"./b.js?\u{1F600}\";\nconsole.log(x);\n",
    )
    .unwrap();
    let cache = tempfile::tempdir().unwrap();
    let out = Command::new(nub_binary())
        .arg("main.ts")
        .current_dir(dir.path())
        .env("XDG_CACHE_HOME", cache.path())
        .env_remove("NODE_COMPAT")
        .output()
        .expect("failed to spawn nub");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("panicked"),
        "the addon panicked resolving the import:\n{stderr}"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected Node's module-not-found exit, got {:?}:\n{stderr}",
        out.status
    );
    assert!(
        stderr.contains("ERR_MODULE_NOT_FOUND"),
        "expected Node's own not-found error:\n{stderr}"
    );
}

/// A tsconfig `paths` pattern whose prefix and suffix overlap inside the
/// specifier (`ab*b` against `ab`) is no match in TypeScript, and taking the
/// text between them would slice backwards, which is a panic. The `a*` alias
/// resolves the import.
#[test]
fn an_overlapping_paths_pattern_is_skipped_and_the_next_alias_resolves() {
    if !host_node_usable() {
        eprintln!("skipping: no usable `node` on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("tsconfig.json"),
        r#"{ "compilerOptions": { "paths": { "ab*b": ["./nope/*"], "a*": ["./lib/*"] } } }"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("lib")).unwrap();
    std::fs::write(dir.path().join("lib/b.ts"), "export default \"lib-b\";\n").unwrap();
    std::fs::write(
        dir.path().join("main.ts"),
        "import x from \"ab\";\nconsole.log(x);\n",
    )
    .unwrap();
    let cache = tempfile::tempdir().unwrap();
    let out = Command::new(nub_binary())
        .arg("main.ts")
        .current_dir(dir.path())
        .env("XDG_CACHE_HOME", cache.path())
        .env_remove("NODE_COMPAT")
        .output()
        .expect("failed to spawn nub");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{:?}:\n{stderr}", out.status);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "lib-b");
}

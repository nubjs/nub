//! A hard link (or copy) of nub named `node`, first on PATH. No path test sees
//! it as nub, so this shape used to recurse without bound on Windows. The version
//! probe must reach nub, be refused, and send discovery on down PATH.
//!
//! Unlike `launch_loop.rs` there is no shell wrapper to cap the chain, so each
//! run starts nub's recursion count four short of its bound: a regression ends
//! within a few levels with an error, instead of filling the runner.

use std::path::{Path, PathBuf};
use std::process::Command;

const EXE: &str = if cfg!(windows) { ".exe" } else { "" };

/// `nub_core::node::nesting::MAX_DEPTH`; the CLI's integration tests do not link
/// nub-core.
const MAX_DEPTH: u32 = 64;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/ (or fast/)
    path.push(format!("nub{EXE}"));
    path
}

fn real_node() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|dir| !dir.to_string_lossy().contains("node-shim"))
        .map(|dir| dir.join(format!("node{EXE}")))
        .find(|node| node.is_file())
        .and_then(|node| node.canonicalize().ok())
}

#[test]
fn a_hard_link_of_nub_named_node_is_skipped_for_the_next_node_on_path() {
    let Some(node) = real_node() else {
        eprintln!("skipping: no Node on PATH");
        return;
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!(
        "nub-node-link-loop-{}-{nanos:x}",
        std::process::id()
    ));
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let link = bin.join(format!("node{EXE}"));
    // A hard link cannot cross volumes (a CI checkout and its temp dir can sit on
    // different ones); a copy is the same shape to every check nub makes.
    if std::fs::hard_link(nub_binary(), &link).is_err() {
        std::fs::copy(nub_binary(), &link).unwrap();
    }
    std::fs::write(root.join("package.json"), r#"{"name":"p","private":true}"#).unwrap();

    let mut path = vec![bin, node.parent().unwrap().to_path_buf()];
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        path.push(Path::new(&system_root).join("System32"));
    }
    let out = Command::new(nub_binary())
        .args(["-p", "process.execPath"])
        .current_dir(&root)
        .env("PATH", std::env::join_paths(path).unwrap())
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("__NUB_NESTING", (MAX_DEPTH - 4).to_string())
        .env_remove("NODE_COMPAT")
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_EXECUTABLE")
        .output()
        .expect("spawn nub");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    let ran_on = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    assert_eq!(
        ran_on.canonicalize().unwrap_or(ran_on),
        node,
        "the run lands on the Node after the link; stderr:\n{stderr}"
    );
}

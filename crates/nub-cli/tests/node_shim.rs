//! `nub node shim` / `nub node unshim` integration tests: install the persistent
//! global `node` shim through the real binary and assert the end-to-end contract
//! — the shim file lands under `~/.nub/node-shim`, the PATH block is written and
//! stripped, and the installed shim runs the resolved Node VANILLA (version
//! management only; augmentation stays on `nub`). Spec: internal/commands/node-versions.md.
//!
//! Hermetic: every child gets an explicit HOME + SHELL, so the install writes
//! into a throwaway tree, never the developer's real `~/.nub` / profiles.

#![cfg(unix)] // hardlink + shell-script probes; Windows PATH editing is out of scope for v0.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/ (or fast/)
    path.push("nub");
    path
}

fn tmp(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-nodeshim-{tag}-{}-{nanos:x}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run the real nub binary with an explicit HOME (+ SHELL for the profile block).
fn nub(args: &[&str], home: &Path) -> (String, String, i32) {
    let mut cmd = Command::new(nub_binary());
    cmd.args(args)
        .current_dir(home)
        .env("HOME", home)
        // bash so the PATH block lands in a real profile (`sh` is an unknown
        // shell → Manual). bash with no `.bashrc`/`.bash_profile` creates
        // `~/.profile`, which the install test asserts on.
        .env("SHELL", "/bin/bash");
    let out = cmd.output().expect("spawn nub failed");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn shim_installs_the_node_hardlink_and_path_block_then_unshim_reverses_it() {
    let home = tmp("install");
    let (stdout, stderr, code) = nub(&["node", "shim"], &home);
    assert_eq!(code, 0, "nub node shim must succeed; stderr:\n{stderr}");

    let dir = home.join(".nub/node-shim");
    let node = dir.join("node");
    assert!(node.is_file(), "the shim dir must carry the `node` entry");
    // The entry is the SAME file as nub (hardlink) — argv0 dispatch re-enters nub.
    assert!(
        same_file(&node, &nub_binary()),
        "the `node` shim must be a hardlink/copy of the nub binary"
    );
    assert!(
        stdout.contains("version management only"),
        "the install must state the shim is vanilla (no augmentation):\n{stdout}"
    );

    // The PATH block landed in a `/bin/sh` (bash-family) login profile with the
    // node-shim marker — distinct from the PM shims' `# nub shims` block.
    let profile = home.join(".profile");
    let contents = std::fs::read_to_string(&profile).unwrap_or_default();
    assert!(
        contents.contains("# nub node shim") && contents.contains(".nub/node-shim"),
        "the node-shim PATH block must be written:\n{contents}"
    );

    // unshim: the dir is gone and the block is stripped, leaving the profile clean.
    let (_out, err, code) = nub(&["node", "unshim"], &home);
    assert_eq!(code, 0, "nub node unshim must succeed; stderr:\n{err}");
    assert!(!dir.exists(), "unshim removes ~/.nub/node-shim");
    let after = std::fs::read_to_string(&profile).unwrap_or_default();
    assert!(
        !after.contains("# nub node shim"),
        "unshim strips the node-shim block:\n{after}"
    );
}

#[test]
fn installed_shim_delegates_to_real_node_and_runs_it_vanilla() {
    // The headline default: a bare `node` through the persistent shim respects
    // node semantics — it delegates to the real Node (version resolution is the
    // shim's job) and runs it VANILLA (no augmentation). Two proofs, both
    // addon-free (the augment side is the transpiler suite's job, not here):
    //   (a) `node --version` prints the real Node's version — the shim resolved
    //       and delegated, skipping its own dir (the recursion guard);
    //   (b) a non-erasable TS `enum` is REJECTED by Node's strip-only mode — the
    //       shim did NOT engage nub's transpiler, i.e. it ran vanilla.
    let real_node = match system_node() {
        Some(n) => n,
        None => {
            eprintln!("skipping: no system node on PATH");
            return;
        }
    };

    let home = tmp("vanilla");
    let (_o, err, code) = nub(&["node", "shim"], &home);
    assert_eq!(code, 0, "install failed:\n{err}");
    let shim_node = home.join(".nub/node-shim/node");

    // PATH = [shim dir, real node's dir]: the shim's own which_node skips its dir
    // (recursion guard) and resolves the real node behind it.
    let path = std::env::join_paths([
        home.join(".nub/node-shim"),
        real_node.parent().unwrap().to_path_buf(),
    ])
    .unwrap();
    let run = |args: &[&std::ffi::OsStr], cwd: &Path| {
        Command::new(&shim_node)
            .args(args)
            .current_dir(cwd)
            .env("HOME", &home)
            .env("PATH", &path)
            .output()
            .expect("spawn shim node failed")
    };

    // (a) delegates to the real Node.
    let ver = run(&["--version".as_ref()], &home);
    let real_ver = Command::new(&real_node).arg("--version").output().unwrap();
    assert_eq!(ver.status.code(), Some(0), "`node --version` via the shim");
    assert_eq!(
        String::from_utf8_lossy(&ver.stdout).trim(),
        String::from_utf8_lossy(&real_ver.stdout).trim(),
        "the shim reports the REAL node's version — it delegated"
    );

    // (b) runs vanilla: a non-erasable enum is NOT transpiled. If nub augmented,
    // it would transpile the enum and print "0" (exit 0); vanilla node instead
    // rejects the file. The rejection wording is Node-version-dependent — a
    // strip-only `TypeScript enum is not supported` on Node ≥23.6, an
    // `ERR_UNKNOWN_FILE_EXTENSION` on the 22.x floor (which won't run `.ts` at
    // all) — so the version-independent proof is "failed AND printed no `0`".
    let proj = tmp("vanilla-proj");
    let app = proj.join("app.ts");
    std::fs::write(&app, "enum E { A }\nconsole.log(E.A);\n").unwrap();
    let out = run(&[app.as_os_str()], &proj);
    assert_ne!(
        out.status.code(),
        Some(0),
        "the shim runs node VANILLA — a non-erasable enum is not transpiled"
    );
    assert_ne!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "0",
        "vanilla node never produced the enum's transpiled output — no augmentation ran"
    );
}

/// The first real `node` on the inherited PATH (skipping any nub shim dir), used
/// as the resolvable Node behind the persistent shim in the behavioral test.
fn system_node() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir
            .file_name()
            .is_some_and(|n| n.to_string_lossy().contains("node-shim"))
        {
            continue;
        }
        let cand = dir.join("node");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Same underlying file (device + inode) — the hardlink identity check.
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (a.metadata(), b.metadata()) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev() && ma.ino() == mb.ino(),
        _ => false,
    }
}

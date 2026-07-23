//! Nub re-invokes ITSELF for a few internal handoffs — the engine's lazy
//! node-gyp shims, the macOS parent-death watcher — always via
//! `current_exe()`. That path carries whatever NAME nub is running under, and
//! nub answers to four argv0 identities (`nub`, `node`, `nubx`, a PM shim), so
//! a hidden re-entry verb dispatched off the `nub` arm alone silently
//! misroutes under the other three.
//!
//! That is not hypothetical: it broke both verbs. The watcher's verb ran as a
//! SCRIPT under the `node` shim, spawning a workload plus another watcher per
//! level until the process table was exhausted (regression from #504, fixed in
//! #517 — `pdeath_watch.rs` covers that one, macOS-gated). This file covers the
//! general invariant on every platform: a hidden re-entry verb resolves to its
//! handler under EVERY argv0 identity.

use std::path::PathBuf;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // <profile>/
    path.push(format!("nub{}", std::env::consts::EXE_SUFFIX));
    path
}

/// Every name nub dispatches on in `Argv0::detect`.
const ARGV0_IDENTITIES: &[&str] = &["nub", "node", "nubx", "npm", "pnpm", "yarn"];

/// The node-gyp bootstrap's own arity error is the cheapest proof the verb
/// REACHED its handler: no project dir means it bails before any bootstrap, so
/// the check needs no network, no cache, and no fixture. Every misroute
/// produces visibly different output — the PM engine's
/// `ERR_PNPM_*`, a Node `cjs/loader` throw, or nubx's
/// "refusing to download" registry fetch.
#[test]
fn node_gyp_bootstrap_verb_is_honored_under_every_argv0() {
    let nub = nub_binary();
    let dir = nub
        .parent()
        .unwrap()
        .join(format!("nub-reentry-argv0-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    for name in ARGV0_IDENTITIES {
        let aliased = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        // Hardlink, not copy: same filesystem, no multi-MB duplication per
        // name, and the same shape as nub's real PATH shims.
        std::fs::hard_link(&nub, &aliased).unwrap();
        let out = std::process::Command::new(&aliased)
            .arg("__node-gyp-bootstrap")
            .current_dir(&dir)
            .output()
            .expect("spawn aliased nub");
        let merged = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            merged.contains("usage: nub __node-gyp-bootstrap"),
            "nub invoked as `{name}` did not route __node-gyp-bootstrap to its \
             handler — got: {merged}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

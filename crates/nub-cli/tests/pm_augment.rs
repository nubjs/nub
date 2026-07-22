//! The lifecycle-augmentation seam, end-to-end through the real binary.
//!
//! `nub install` must run a project's lifecycle scripts under nub's runtime
//! augmentation — nub's preload in `NODE_OPTIONS` and the node-shim dir leading
//! `PATH`, so a build script's `node`/`$NODE child.js` re-enters nub augmented and
//! node-gyp compiles against the provisioned Node. Both halves of that seam
//! (`augmentation_to_lifecycle_overlay` in pm_engine, aube's env-overlay
//! application in aube-scripts) are unit-tested in isolation over hand-built
//! structs; nothing joined `compute_augmentation_env` → the overlay → a real
//! spawn. That uncovered join is what let a lifecycle hang survive 2,672 aube +
//! 443 nub tests during the v1.32 sync (#528). This test closes it by observing a
//! real root `postinstall`'s environment.
//!
//! It runs OFFLINE — a nub-identity project with an empty lock, no dependencies,
//! and its registry pointed at a dead port so any accidental network fails loudly.
//!
//! The harness fails LOUDLY, never vacuously: if this build cannot even LOCATE its
//! preload (`find_public_preload` → `None`), the augmentation seam is inexercisable
//! and the test reports exactly that rather than skipping or passing empty. That
//! is the #528 failure mode: a binary built into a target dir with no `runtime/`
//! ancestor (the shared cross-worktree dir) used to find no preload and augment
//! nothing, so a lifecycle test that merely didn't assert on augmentation passed
//! green while running un-augmented.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/ (or fast/)
    path.push("nub");
    path
}

/// A root `postinstall` that records the two augmentation signals a lifecycle
/// script actually sees — `NODE_OPTIONS` (carries nub's preload injection) and
/// the FIRST `PATH` entry (must be nub's node shim) — to `aug.json`. Only single
/// quotes inside the JS so the `node -e "…"` wrapper needs no further escaping.
const POSTINSTALL_PROBE: &str = "node -e \"const fs=require('fs'),sep=require('path').delimiter;fs.writeFileSync('aug.json',JSON.stringify({no:process.env.NODE_OPTIONS||'',p0:(process.env.PATH||'').split(sep)[0]||''}))\"";

const EMPTY_LOCK: &str = "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n";

#[test]
fn install_runs_lifecycle_scripts_under_runtime_augmentation() {
    let nub = nub_binary();

    // Precondition — the seam must be exercisable, or this is a harness fault, not
    // a nub regression. `find_public_preload` here uses the exact same resolution
    // the spawned nub binary uses (both compiled from this source), so a `None`
    // means no build layout on this machine can augment: report it as such and fail
    // hard instead of running an install that would silently prove nothing (#528).
    let preload = nub_core::node::spawn::find_public_preload(&nub).unwrap_or_else(|| {
        panic!(
            "harness cannot exercise augmentation: find_public_preload returned None for {} — \
             no runtime/preload.mjs is reachable from the nub binary nor from the compile-time \
             source root, so this build applies NO lifecycle augmentation and the assertions \
             below would pass only vacuously (#528). Build via `cargo build`/`scripts/rust-build.sh` \
             from a checkout whose runtime/ is intact.",
            nub.display()
        )
    });

    // The concrete tokens nub injects for OUR preload — fast tier `--require=<cjs>`
    // (raw path) or compat tier `--import=file://<mjs>` (slash-form URL). Matching
    // one of these (not a bare "preload." substring) ties the assertion to nub's
    // own runtime file, not a coincidental user preload.
    let mjs = preload.clone();
    let cjs = preload
        .strip_suffix(".mjs")
        .map(|stem| format!("{stem}.cjs"))
        .unwrap_or_default();

    let dir = fixture();
    let (stdout, stderr, code) = run(&nub, &dir, &["install"]);
    assert_eq!(
        code, 0,
        "install failed\nstdout: {stdout}\nstderr: {stderr}"
    );

    let recorded = std::fs::read_to_string(dir.join("aug.json")).unwrap_or_else(|_| {
        panic!(
            "the root postinstall did not run — aug.json was never written; lifecycle scripts \
             were not executed.\nstdout: {stdout}\nstderr: {stderr}"
        )
    });
    let aug: serde_json::Value = serde_json::from_str(&recorded).unwrap();
    let node_options = aug["no"].as_str().unwrap_or_default();
    let first_path = aug["p0"].as_str().unwrap_or_default();

    // Slash-normalize so the compat-tier file:// URL (forward slashes) matches the
    // filesystem path on Windows too.
    let norm = |s: &str| s.replace('\\', "/");
    // `mjs` is always the full `preload.mjs` path (never empty); `cjs` is only
    // matched when non-empty so a degenerate `contains("")` can't pass vacuously.
    let carries_preload = (!cjs.is_empty() && node_options.contains(&cjs))
        || norm(node_options).contains(&norm(&mjs));
    assert!(
        carries_preload,
        "the postinstall's NODE_OPTIONS must carry nub's preload injection \
         (`--require={cjs}` on the fast tier, or `--import=file://{mjs}` on the compat tier) — \
         augmentation did not reach the lifecycle script.\nNODE_OPTIONS = {node_options:?}"
    );

    assert!(
        first_path.contains("nub-node-shim-"),
        "the FIRST PATH entry in the lifecycle script must be nub's node-shim dir so a bare \
         `node` in a build script re-enters nub augmented; got {first_path:?}"
    );
}

/// A nub-identity project with a root postinstall probe, an empty lock, no
/// dependencies, and a dead-port registry (offline).
fn fixture() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-augment-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".npmrc"), "registry=http://127.0.0.1:1/\n").unwrap();
    std::fs::write(dir.join("nub.lock"), EMPTY_LOCK).unwrap();
    let pkg = format!(
        r#"{{"name":"app","version":"1.0.0","packageManager":"nub@0.0.1","scripts":{{"postinstall":{}}}}}"#,
        serde_json::to_string(POSTINSTALL_PROBE).unwrap()
    );
    std::fs::write(dir.join("package.json"), pkg).unwrap();
    dir
}

fn run(nub: &Path, dir: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(nub)
        .args(args)
        .current_dir(dir)
        // The fixture pins `nub@0.0.1` to exercise nub identity, not the self-shim —
        // opt out so a PM verb doesn't try to provision that nub.
        .env("NUB_SELF_SHIM", "0")
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("XDG_CACHE_HOME", dir.join("xdg-cache"))
        .output()
        .expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

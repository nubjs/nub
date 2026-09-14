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
//!
//! The second test covers the other thing nub has to hand a lifecycle script for
//! a native addon to build: a runnable node-gyp, through both channels npm and
//! pnpm supply it on.

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
/// the whole `PATH` (nub's node shim must lead the inherited entries) — to
/// `aug.json`. Only single quotes inside the JS so the `node -e "…"` wrapper
/// needs no further escaping.
const POSTINSTALL_PROBE: &str = "node -e \"const fs=require('fs'),sep=require('path').delimiter;fs.writeFileSync('aug.json',JSON.stringify({no:process.env.NODE_OPTIONS||'',p:(process.env.PATH||'').split(sep)}))\"";

/// A root `postinstall` that records both channels a lifecycle script can reach
/// node-gyp through: `npm_config_node_gyp`, and every executable `node-gyp` its
/// `PATH` resolves. It RESOLVES rather than runs them — invoking nub's shim
/// would trigger the real bootstrap install, so a probe that executed it would
/// need the network to answer a question about the environment.
const NODE_GYP_PROBE: &str = "node -e \"const fs=require('fs'),p=require('path');const found=(process.env.PATH||'').split(p.delimiter).map(d=>p.join(d,'node-gyp')).filter(f=>{try{fs.accessSync(f,fs.constants.X_OK);return true}catch(e){return false}});fs.writeFileSync('gyp.json',JSON.stringify({gyp:process.env.npm_config_node_gyp||'',onPath:found}))\"";

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

    let dir = fixture(POSTINSTALL_PROBE);
    let (stdout, stderr, code) = run(&nub, &dir, &["install"], None);
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
    let path: Vec<&str> = aug["p"].as_array().map_or_else(Vec::new, |entries| {
        entries
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect()
    });

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

    // Ahead of the inherited PATH, not first outright. The package manager
    // puts a project's own `node_modules/.bin` ahead of anything the host
    // can reach — npm and pnpm both do, and overriding that would change
    // which binary a build script's bare `node` means, which augmentation
    // must never do. What has to hold is that nub's shim beats the SYSTEM
    // node, so a build script re-enters nub augmented.
    let shim = path
        .iter()
        .position(|entry| entry.contains("nub-node-shim-"));
    let system = path
        .iter()
        .position(|entry| matches!(*entry, "/usr/bin" | "/usr/local/bin" | "/bin"));
    assert!(
        shim.is_some_and(|shim| system.is_none_or(|system| shim < system)),
        "nub's node-shim dir must lead the inherited PATH so a bare `node` in a build script \
         re-enters nub augmented rather than reaching the system one; shim at {shim:?}, system \
         at {system:?} in {path:?}"
    );
}

/// npm and pnpm both bundle node-gyp with themselves, so a dependency with a
/// native addon builds on a machine that has none installed. nub has no bundled
/// JavaScript and supplies lazy shims instead — one for `npm_config_node_gyp`,
/// one on `PATH` for the far commoner `"install": "node-gyp rebuild"`. The
/// engine supplies neither: `node_gyp_path` is `None` at all of its call sites,
/// and its `PATH` channel looks for a `dist/node-gyp-bin` beside the running
/// executable, a layout pnpm's npm package has and nub's binary does not.
///
/// The `nub run` path stamps the variable at its own site and is covered by
/// `pm_identity`'s brand test; this is the install path, which is where the two
/// silently diverged.
#[test]
fn install_hands_lifecycle_scripts_a_runnable_node_gyp() {
    let nub = nub_binary();
    let dir = fixture(NODE_GYP_PROBE);
    let cache_root = dir.join("xdg-cache");
    let scrubbed = scrubbed_path(&dir);

    let (stdout, stderr, code) = run(&nub, &dir, &["install"], scrubbed.as_deref());
    assert_eq!(
        code, 0,
        "install failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    let recorded = std::fs::read_to_string(dir.join("gyp.json")).unwrap_or_else(|_| {
        panic!(
            "the root postinstall did not run — gyp.json was never written.\n\
             stdout: {stdout}\nstderr: {stderr}"
        )
    });
    let probe: serde_json::Value = serde_json::from_str(&recorded).unwrap();

    // Asserted against THIS test's cache root, not merely "non-empty": the
    // harness inherits an npm lifecycle environment that can carry
    // `npm_config_node_gyp` already, and nub deliberately leaves an ambient
    // value alone, so a laxer assertion would read its own environment back.
    let gyp = probe["gyp"].as_str().unwrap_or_default();
    assert!(
        !gyp.is_empty() && Path::new(gyp).starts_with(&cache_root),
        "the postinstall's npm_config_node_gyp must name a node-gyp nub supplied under {} — \
         a script's `node $npm_config_node_gyp …` has nothing to run otherwise.\n\
         npm_config_node_gyp = {gyp:?}",
        cache_root.display()
    );

    // Only meaningful under the scrub: with an ambient node-gyp reachable, nub
    // stands down by design and finding one would prove nothing.
    let Some(_) = &scrubbed else {
        return;
    };
    let on_path: Vec<&str> = probe["onPath"].as_array().map_or_else(Vec::new, |found| {
        found.iter().filter_map(serde_json::Value::as_str).collect()
    });
    assert!(
        on_path
            .iter()
            .any(|found| Path::new(found).starts_with(&cache_root)),
        "a bare `node-gyp` — what almost every native addon's install script runs — must \
         resolve on the lifecycle script's PATH to one nub supplied under {}; the PATH held \
         only node and the system dirs, so nothing else could have put one there.\n\
         resolved: {on_path:?}",
        cache_root.display()
    );
}

/// A `PATH` carrying only `node` and the system directories, so "node-gyp
/// resolves" can only be true because nub put one there. Without this the
/// assertion is free on any developer machine: npm installs its own `node-gyp`
/// beside `node`.
///
/// `None` when the link cannot be made — Windows grants symlink creation only
/// under Developer Mode, and a hard link fails across volumes. The caller drops
/// the PATH half of its assertion rather than letting an ambient node-gyp
/// satisfy it.
fn scrubbed_path(dir: &Path) -> Option<std::ffi::OsString> {
    let exe = if cfg!(windows) { "node.exe" } else { "node" };
    let node = std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|entry| entry.join(exe))
        .find(|candidate| candidate.is_file())
        .expect("no `node` on PATH — this suite cannot run a lifecycle script without one");

    let bin = dir.join("scrubbed-bin");
    std::fs::create_dir_all(&bin).ok()?;
    let linked = bin.join(exe);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&node, &linked).ok()?;
    #[cfg(windows)]
    std::fs::hard_link(&node, &linked).ok()?;

    let system: Vec<PathBuf> = if cfg!(windows) {
        std::env::var_os("SystemRoot")
            .map(|root| {
                let root = PathBuf::from(root);
                vec![root.join("system32"), root]
            })
            .unwrap_or_default()
    } else {
        ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
            .iter()
            .map(PathBuf::from)
            .collect()
    };
    let path = std::env::join_paths(std::iter::once(bin).chain(system)).ok()?;
    // The scrub is an instrument, so it is checked against the thing it claims:
    // a `node-gyp` still reachable means the assertion below would pass for free.
    assert!(
        !std::env::split_paths(&path).any(|entry| entry.join("node-gyp").is_file()),
        "the scrubbed PATH still resolves a node-gyp: {path:?}"
    );
    Some(path)
}

/// A nub-identity project with a root postinstall probe, an empty lock, no
/// dependencies, and a dead-port registry (offline).
fn fixture(postinstall: &str) -> PathBuf {
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
        serde_json::to_string(postinstall).unwrap()
    );
    std::fs::write(dir.join("package.json"), pkg).unwrap();
    dir
}

fn run(
    nub: &Path,
    dir: &Path,
    args: &[&str],
    path: Option<&std::ffi::OsStr>,
) -> (String, String, i32) {
    let mut command = Command::new(nub);
    command
        .args(args)
        .current_dir(dir)
        // The fixture pins `nub@0.0.1` to exercise nub identity, not the self-shim —
        // opt out so a PM verb doesn't try to provision that nub.
        .env("NUB_SELF_SHIM", "0")
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("XDG_CACHE_HOME", dir.join("xdg-cache"))
        // `cargo test` is routinely run from an npm lifecycle environment, which
        // carries a `node-gyp` of its own; nub honours an ambient value, so the
        // probe would read the harness's environment back.
        .env_remove("npm_config_node_gyp")
        .env_remove("NPM_CONFIG_NODE_GYP");
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let out = command.output().expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

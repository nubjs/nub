//! Why the compiled-artifact cache is sometimes refused on a Windows CI runner.
//!
//! `nub` declines a runtime-cache root whose path fails
//! [`windows_security::directory_is_stable`], printing "runtime cache path has an
//! unsafe owner or DACL" and relocating. On `nubjs/nub#830` that happened on 2 of 8
//! CI runs while never happening on main — and the only difference in the binary
//! under test is that the PR links the MSVC CRT statically. This decides between
//! the two live explanations without a `nub` build:
//!
//! - the CRT linkage changes the verdict (the probe is built BOTH ways),
//! - a runner-created ancestor's ACL varies between VMs (the matrix samples several,
//!   and every component's verdict is printed rather than just the final answer).
//!
//! `src/windows_security.rs` is a VERBATIM copy of the shipped module, so the
//! verdict here is the product's, not a paraphrase of it.

mod windows_security;

use std::path::{Path, PathBuf};

/// The traversal from `runtime_cache::walk_windows_base`, reproduced so each
/// component's verdict can be printed. The security decision itself is the real
/// one; only the loop around it lives here.
fn walk(path: &Path, create_missing: bool, label: &str) -> bool {
    let mut chain: Vec<PathBuf> = path.ancestors().map(Path::to_path_buf).collect();
    chain.reverse();
    let mut all_ok = true;
    for (index, component) in chain.iter().enumerate() {
        let leaf = index + 1 == chain.len();
        let volume_root = index == 0;
        if std::fs::symlink_metadata(component).is_err() && create_missing {
            match windows_security::create_private_directory(component) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    println!("{label} CREATE-FAIL {} {e}", component.display());
                    return false;
                }
            }
        }
        match windows_security::directory_is_stable(component, leaf, volume_root) {
            Ok(true) => {}
            Ok(false) => {
                println!(
                    "{label} UNSTABLE leaf={leaf} volume_root={volume_root} {}",
                    component.display()
                );
                all_ok = false;
            }
            Err(e) => {
                println!("{label} ERROR {} {e}", component.display());
                all_ok = false;
            }
        }
    }
    all_ok
}

fn main() {
    // Recorded rather than assumed: this is the variable under test, so the log has
    // to say which binary produced each row.
    let linkage = if cfg!(target_feature = "crt-static") {
        "static"
    } else {
        "dynamic"
    };
    let iterations: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(40);
    let base = std::env::var("RUNNER_TEMP").unwrap_or_else(|_| std::env::temp_dir().display().to_string());

    println!("linkage={linkage} base={base} iterations={iterations}");

    // ARM 3: which base is usable on EVERY runner image?
    //
    // `RUNNER_TEMP` is the control and must fail on a bad image — if it passes there,
    // the reproduction is broken and no other row here means anything. The rest are
    // candidates for where the CI job should put its cold cache instead. A candidate
    // is only useful if it is stable on every shard, so the interesting output is the
    // shard that fails the control.
    for (name, var) in [
        ("RUNNER_TEMP(control)", "RUNNER_TEMP"),
        ("USERPROFILE", "USERPROFILE"),
        ("LOCALAPPDATA", "LOCALAPPDATA"),
        ("TEMP", "TEMP"),
    ] {
        let Ok(root) = std::env::var(var) else {
            println!("BASE {name} linkage={linkage} unset");
            continue;
        };
        let chain = PathBuf::from(&root).join("nub-base-probe").join("nub");
        let _ = std::fs::remove_dir_all(PathBuf::from(&root).join("nub-base-probe"));
        let ok = walk(&chain, true, &format!("[base {name}]"));
        println!("BASE {name} linkage={linkage} root={root} usable={ok}");
        let _ = std::fs::remove_dir_all(PathBuf::from(&root).join("nub-base-probe"));
    }

    // ARM 2: the chain created the way `spawn::default_compile_cache_dir` creates
    // it — a bare `create_dir_all`, which takes the parent's INHERITED ACEs instead
    // of the private DACL `create_private_directory` installs. That call site takes
    // `<XDG_CACHE_HOME>/nub` and appends `v8-compile-cache`, so it materialises the
    // cache root AND its parent before the validating walk ever sees them. If the
    // walk then refuses either, this is the CI failure, and arm 1 missed it only
    // because it created everything the safe way.
    {
        let poisoned = PathBuf::from(&base).join("nub-poisoned-probe").join("nub");
        let _ = std::fs::remove_dir_all(PathBuf::from(&base).join("nub-poisoned-probe"));
        match std::fs::create_dir_all(poisoned.join("v8-compile-cache")) {
            Ok(()) => {
                // create_missing=false: the question is whether the walk ACCEPTS what
                // the bare call left behind, not whether it can repair it.
                let ok = walk(&poisoned, false, &format!("[{linkage} poisoned]"));
                println!("POISONED linkage={linkage} accepted={ok}");
            }
            Err(e) => println!("POISONED linkage={linkage} setup-failed {e}"),
        }
        let _ = std::fs::remove_dir_all(PathBuf::from(&base).join("nub-poisoned-probe"));
    }

    let mut failures = 0usize;
    for i in 0..iterations {
        // A fresh leaf each time, under the same runner-created ancestors nub uses,
        // so a per-iteration failure is the cache root's own creation and not a
        // leftover from the previous round.
        let root = PathBuf::from(&base)
            .join(format!("nub-dacl-probe-{i}"))
            .join("nub");
        if !walk(&root, true, &format!("[{linkage} iter={i}]")) {
            failures += 1;
        }
        let _ = std::fs::remove_dir_all(PathBuf::from(&base).join(format!("nub-dacl-probe-{i}")));
    }
    println!("RESULT linkage={linkage} failures={failures}/{iterations}");
}

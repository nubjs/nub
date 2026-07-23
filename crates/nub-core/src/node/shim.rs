//! The persistent global `node`→nub shim: `nub node shim` / `nub node unshim`.
//!
//! A fresh machine with no Node can use nub as its default node runner. This
//! module installs a `node` (Unix) / `node.exe` (Windows) hardlink to nub in a
//! DEDICATED dir (`~/.nub/node-shim`) and wires that dir onto PATH — so a
//! bare-shell `node foo.js` resolves through nub (version resolution + the
//! no-pin/no-node auto-provision of #294), NOT to a missing binary.
//!
//! **Vanilla by default (maintainer, 2026-07-03).** Unlike the per-invocation
//! hijack (`node::spawn::setup_path_shim`, a temp dir scoped to a `nub …`
//! subtree the user opted into, which runs AUGMENTED), the persistent global
//! shim runs the resolved Node VANILLA — version management is its job;
//! augmentation belongs to `nub`/`nubx`. This respects the node-hijack contract
//! (a bare-shell `node` must behave like node) and bounds the blast radius: a
//! global augmenting `node` would auto-load `.env` and inject globals for EVERY
//! node process on the machine. The augment opt-in is one keystroke away —
//! `nub foo.ts`. `run_as_node` reads [`invoked_as_persistent_node_shim`] to pick
//! the compat default; `discovery::which_node` skips [`node_shim_dir`] as the
//! recursion guard (nub-as-node must not re-resolve its own shim).
//!
//! The heavy lifting — the shim dir hardlink/copy install, the all-shells PATH
//! block, the noexec probe, the reachability check — is the SAME engine the PM
//! shims use, reused from [`crate::pm::shim`] parameterized on a distinct
//! [`ShimBlock`] (so `nub node unshim` strips exactly its own block and never
//! the PM shims' or install.sh's).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::pm::shim::{
    self, InstalledShim, ProfileOutcome, ShimBlock, ShimReachability, add_path_block_for,
    remove_path_block_from_profiles,
};

/// The persistent node shim's PATH block — a DEDICATED dir + marker, distinct
/// from the PM shims' (`~/.nub/shims`, `# nub shims`) so the two install and
/// uninstall independently.
const NODE_SHIM_BLOCK: ShimBlock = ShimBlock {
    marker: "# nub node shim",
    posix_line: r#"export PATH="$HOME/.nub/node-shim:$PATH""#,
    fish_line: "set -gx PATH $HOME/.nub/node-shim $PATH",
    dir_marker: ".nub/node-shim",
};

/// The name the shim intercepts.
const NODE: &str = "node";

/// `~/.nub/node-shim` — sibling of the PM shims' `~/.nub/shims` and install.sh's
/// `~/.nub/bin`, under the `~/.nub` install surface (NOT the wipeable cache): a
/// shim the user opted into must not vanish when a cache is cleared.
pub fn node_shim_dir() -> Result<PathBuf> {
    dirs_next::home_dir()
        .map(|h| h.join(".nub").join("node-shim"))
        .context("cannot locate the home directory for ~/.nub/node-shim")
}

/// Whether THIS nub process was invoked as the persistent `node` shim — i.e.
/// `current_exe`'s dir canonicalizes to [`node_shim_dir`]. `run_as_node` reads
/// this to default to VANILLA (compat) mode: only the persistent global shim
/// runs vanilla; the per-invocation hijack (temp dir) and a direct `nub` still
/// augment. Canonical-path comparison so a symlinked `~/.nub` still matches.
pub fn invoked_as_persistent_node_shim() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Some(parent) = exe.parent() else {
        return false;
    };
    match (
        parent.canonicalize(),
        node_shim_dir().and_then(|d| d.canonicalize().map_err(Into::into)),
    ) {
        (Ok(p), Ok(d)) => p == d,
        _ => false,
    }
}

/// `nub node shim`: hardlink the running nub as `node` in [`node_shim_dir`],
/// write the marked PATH block into the shell profiles, and return the entry +
/// profile outcome for the CLI to report. Idempotent — re-running re-links,
/// which is how the shim is refreshed after `nub upgrade` (same story as the PM
/// shims). PATH wiring is skipped on Windows (profile editing isn't automated
/// there yet — the CLI prints the dir to add).
pub fn install_node_shim(nub_binary: &Path) -> Result<InstalledShim> {
    let dir = node_shim_dir()?;
    let mut report = shim::install_named_shims(&dir, &[NODE], nub_binary)?;
    Ok(report
        .pop()
        .expect("install_named_shims returns one entry per requested name"))
}

/// Add the node-shim PATH block to the current shell's profiles.
pub fn add_node_path_block() -> Result<ProfileOutcome> {
    let home = dirs_next::home_dir().context("cannot locate the home directory")?;
    let shell = std::env::var("SHELL").unwrap_or_default();
    let shell = Path::new(&shell)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "bash".to_string());
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    add_path_block_for(&shell, &home, xdg.as_deref(), &NODE_SHIM_BLOCK)
}

/// `nub node unshim`: delete [`node_shim_dir`] and strip the node-shim PATH
/// block from every profile. Touches only the dedicated dir + profile files, so
/// it keeps working from any nub still on PATH. Returns `(dir_existed, changed
/// profile files)`. Idempotent.
pub fn remove_node_shim() -> Result<(bool, Vec<PathBuf>)> {
    let dir = node_shim_dir()?;
    // The dir is dedicated to the single `node` entry, so removing it wholesale
    // is correct (unlike a shared dir, which would need per-entry removal).
    let existed = shim::remove_shims_from(&dir)?;
    let home = dirs_next::home_dir().context("cannot locate the home directory")?;
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let changed = remove_path_block_from_profiles(&home, xdg.as_deref(), &NODE_SHIM_BLOCK)?;
    Ok((existed, changed))
}

/// The post-install reachability check for the `node` name (parity with the PM
/// shim's sweep): does the first `node` on PATH resolve to the shim?
pub fn check_node_shim_reachable(dir: &Path) -> ShimReachability {
    shim::check_shim_reachable(dir, NODE)
}

/// Whether the shim dir is on a `noexec` mount — where the shim installs but
/// every invocation dies with "Permission denied". Thin pass-through to the PM
/// shim's probe so the CLI reads one cohesive `node::shim` surface.
pub fn check_node_shim_noexec(dir: &Path) -> bool {
    shim::dir_is_noexec(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_shim_dir_is_under_the_install_surface_not_the_cache() {
        let dir = node_shim_dir().unwrap();
        assert!(
            dir.ends_with(".nub/node-shim"),
            "the shim lives under ~/.nub (opt-in install), not the wipeable cache: {}",
            dir.display()
        );
    }

    #[test]
    fn node_block_is_distinct_from_the_pm_block() {
        // Independent install/uninstall lifecycles hinge on distinct markers +
        // dirs — sharing either would let one unshim strip the other's block.
        assert_ne!(NODE_SHIM_BLOCK.marker, shim::PM_SHIM_BLOCK.marker);
        assert_ne!(NODE_SHIM_BLOCK.dir_marker, shim::PM_SHIM_BLOCK.dir_marker);
    }

    #[test]
    fn install_and_remove_roundtrip_via_testable_seams() {
        // `install_node_shim`/`remove_node_shim` key on $HOME; exercise the same
        // single-`node`-entry install + wholesale-dir removal through the
        // dir-explicit seams so the test stays hermetic.
        let tmp = unique_tmp();
        let dir = tmp.join("node-shim");
        let fake_nub = tmp.join("nub");
        std::fs::write(&fake_nub, b"#!/bin/sh\n").unwrap();

        let report = shim::install_named_shims(&dir, &[NODE], &fake_nub).unwrap();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].name, NODE);
        assert!(
            dir.join(if cfg!(windows) { "node.exe" } else { "node" })
                .exists()
        );

        assert!(shim::remove_shims_from(&dir).unwrap(), "the dir existed");
        assert!(!dir.exists(), "unshim removes the dedicated dir wholesale");
        assert!(
            !shim::remove_shims_from(&dir).unwrap(),
            "removing again is a no-op"
        );
    }

    /// A unique throwaway dir (nub-core has no `tempfile` dev-dep).
    fn unique_tmp() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "nub-nodeshim-ut-{}-{nanos:x}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

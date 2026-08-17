//! The persistent global `node`→nub shim: `nub node shim` / `nub node unshim`.
//!
//! A fresh machine with no Node can use nub as its default node runner. This
//! module installs a `node` (Unix) / `node.exe` (Windows) hardlink to nub in a
//! DEDICATED dir (`~/.nub/node-shim`, or `$XDG_DATA_HOME/nub/node-shim` on a
//! fresh unix install — see `pm::shim::resolve_shim_dir`) and wires it onto PATH — so a
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
    marker: NODE_SHIM_MARKER,
    posix_line: r#"export PATH="$HOME/.nub/node-shim:$PATH""#,
    fish_line: "set -gx PATH $HOME/.nub/node-shim $PATH",
    dir_marker: NODE_SHIM_DIR_MARKER,
};

/// The node shim's block for an XDG install — the `node-shim` twin of
/// [`nub_core::pm::shim::PM_SHIM_BLOCK_XDG`], sharing this family's marker so an
/// unshim strips whichever line is present.
const NODE_SHIM_BLOCK_XDG: ShimBlock = ShimBlock {
    marker: NODE_SHIM_MARKER,
    posix_line: r#"export PATH="${XDG_DATA_HOME:-$HOME/.local/share}/nub/node-shim:$PATH""#,
    // `test -n` rather than fish's `set -q` — see SHIMS_FISH_PATH_LINE_XDG for why
    // a defined-but-empty variable would otherwise put `/nub/node-shim` on PATH.
    fish_line: concat!(
        "set -gx PATH (test -n \"$XDG_DATA_HOME\"; and echo $XDG_DATA_HOME; ",
        "or echo $HOME/.local/share)/nub/node-shim $PATH"
    ),
    dir_marker: NODE_SHIM_DIR_MARKER,
};

const NODE_SHIM_MARKER: &str = "# nub node shim";

/// Matches both `$HOME/.nub/node-shim` and the XDG `…/nub/node-shim` — see the
/// `dir_marker` field docs for why it deliberately drops the leading dot.
const NODE_SHIM_DIR_MARKER: &str = "nub/node-shim";

/// The name the shim intercepts.
const NODE: &str = "node";

/// The `~/.nub/<leaf>` basename for this shim family.
const NODE_SHIM_LEAF: &str = "node-shim";

/// `~/.nub/node-shim` — sibling of the PM shims' `~/.nub/shims` and install.sh's
/// `~/.nub/bin`, under the `~/.nub` install surface (NOT the wipeable cache): a
/// shim the user opted into must not vanish when a cache is cleared. On a FRESH
/// install with `XDG_DATA_HOME` set it is `$XDG_DATA_HOME/nub/node-shim` instead
/// (`nub_core::pm::shim::resolve_shim_dir` carries the legacy-wins rule).
pub fn node_shim_dir() -> Result<PathBuf> {
    let home =
        dirs_next::home_dir().context("cannot locate the home directory for the node shim")?;
    Ok(crate::pm::shim::resolve_shim_dir(
        &home,
        crate::pm::shim::xdg_data_home().as_deref(),
        NODE_SHIM_LEAF,
    ))
}

/// The node block whose PATH line names the directory [`node_shim_dir`] resolved.
fn node_shim_block(home: &Path) -> &'static ShimBlock {
    let resolved = crate::pm::shim::resolve_shim_dir(
        home,
        crate::pm::shim::xdg_data_home().as_deref(),
        NODE_SHIM_LEAF,
    );
    if resolved == home.join(".nub").join(NODE_SHIM_LEAF) {
        &NODE_SHIM_BLOCK
    } else {
        &NODE_SHIM_BLOCK_XDG
    }
}

/// Whether THIS nub process was invoked as the persistent `node` shim.
/// `run_as_node` reads this to default to VANILLA (compat) mode: only the
/// persistent global shim runs vanilla; the per-invocation hijack (temp dir) and
/// a direct `nub` still augment.
///
/// Matched TWO ways, and both are load-bearing. `current_exe`'s directory is
/// canonicalized, compared against [`node_shim_dir`], and — failing that —
/// tested for the `<nub|.nub>/node-shim` SHAPE.
///
/// The exact comparison alone was correct while the dir was always
/// `~/.nub/node-shim`, but it now depends on `XDG_DATA_HOME`, which need not be
/// set in the shell that ends up RUNNING the shim; an XDG install entered from a
/// plain shell would fail it. The shape alone fails the opposite case, a `~/.nub`
/// symlinked to a differently-named target (`~/.nub -> ~/dotfiles/nub-config`),
/// where the canonical parent is `nub-config`.
///
/// Either gap has the same consequence: the global `node` silently starts
/// AUGMENTING — auto-loading `.env` and injecting globals into every node process
/// on the machine — which is exactly what the node-hijack contract forbids.
// @lat: [[research/node-impersonation#Research: node-executable impersonation (the  PATH shim)#Implications for Nub]]
pub fn invoked_as_persistent_node_shim() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Some(parent) = exe.parent() else {
        return false;
    };
    let Ok(dir) = parent.canonicalize() else {
        return false;
    };
    // BOTH checks, not either. Exact-path equality is what handles a `~/.nub`
    // symlinked to a differently-named target (`~/.nub -> ~/dotfiles/nub-config`):
    // the canonical parent is then `nub-config`, which the shape rejects. The
    // shape is what handles a dir this process cannot name, because XDG_DATA_HOME
    // is unset in the shell running the shim. Dropping either one regresses a
    // real case into the global `node` silently AUGMENTING.
    if node_shim_dir()
        .ok()
        .and_then(|d| d.canonicalize().ok())
        .is_some_and(|d| d == dir)
    {
        return true;
    }
    is_node_shim_dir_shape(&dir)
}

/// The `<nub|.nub>/node-shim` shape [`invoked_as_persistent_node_shim`] matches.
/// Also the recursion guard in `discovery::which_node_in`, which must skip this
/// dir under EITHER root or nub-as-node resolves its own shim and recurses.
pub(crate) fn is_node_shim_dir_shape(dir: &Path) -> bool {
    dir.file_name().is_some_and(|n| n == NODE_SHIM_LEAF)
        && dir
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|n| n == "nub" || n == ".nub")
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
    add_path_block_for(&shell, &home, xdg.as_deref(), node_shim_block(&home))
}

/// `nub node unshim`: delete the node shim dir and strip the node-shim PATH
/// block from every profile. Touches only the dedicated dir + profile files, so
/// it keeps working from any nub still on PATH. Returns `(dirs actually removed,
/// changed profile files)`. Idempotent.
///
/// The removed dirs are returned rather than a bool so the CLI can name what it
/// touched: the sweep may clear a dir that is not the one resolving now, and
/// reporting [`node_shim_dir`] instead would print a path nothing happened to.
pub fn remove_node_shim() -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let home = dirs_next::home_dir().context("cannot locate the home directory")?;
    // Sweep every candidate root, not just the one resolving now: a shim
    // installed under XDG_DATA_HOME and unshimmed from a shell without that
    // variable would otherwise be left on disk with its PATH line stripped.
    // The dir is dedicated to the single `node` entry, so removing it wholesale
    // is correct (unlike a shared dir, which would need per-entry removal).
    let mut removed = Vec::new();
    for dir in shim::shim_dirs_for_removal(&home, shim::xdg_data_home().as_deref(), NODE_SHIM_LEAF)
    {
        // An absent candidate creates nothing: `remove_shims_from` returns before
        // `ShimLock::acquire`, whose `create_dir_all(parent)` would otherwise make
        // this sweep materialize a root that was never used.
        if shim::remove_shims_from(&dir)? {
            removed.push(dir);
        }
    }
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let changed = remove_path_block_from_profiles(&home, xdg.as_deref(), &NODE_SHIM_BLOCK)?;
    Ok((removed, changed))
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
        // The invariant is "an opted-into shim survives a cache wipe", so it has to
        // be asserted against the REAL `node_shim_dir()` and the REAL cache root —
        // asserting through `resolve_shim_dir` with invented arguments would let a
        // change that repointed `node_shim_dir` at the cache sail through.
        //
        // Comparing the two real functions keeps this independent of WHERE either
        // root points — with one exception worth naming rather than asserting
        // through: an environment that collapses the two roots together
        // (`XDG_DATA_HOME` = `XDG_CACHE_HOME` = `/x`, a shape sandboxed CI images
        // use, or a data home nested under the cache home) genuinely does put the
        // shim inside the cache root. That is the user's layout, not nub's choice,
        // and nub cannot resolve its way out of it — so the containment check is
        // skipped there instead of failing for something this test does not govern.
        let dir = node_shim_dir().expect("a home dir resolves in the test environment");
        if let Some(cache) = crate::node::discovery::cache_dir() {
            // Skip ONLY the layout that genuinely puts the shim inside the cache:
            // the dir came from the data root AND that lands it under the cache
            // root. Comparing the two ROOTS instead is too broad in both
            // directions — `cache.starts_with(&data)` fires for a data root that is
            // merely an ancestor of the cache (`XDG_DATA_HOME=$HOME` with the
            // default `~/.cache/nub`, where the shim is `$HOME/nub/node-shim` and
            // not under the cache at all), and without the data-root test a LEGACY
            // `~/.nub/node-shim` would skip an assertion that would have passed.
            let user_collapsed_the_roots = crate::pm::shim::xdg_data_home()
                .is_some_and(|data| dir.starts_with(&data) && dir.starts_with(&cache));
            if !user_collapsed_the_roots {
                assert!(
                    !dir.starts_with(&cache),
                    "a shim the user opted into must not live in the wipeable cache: \
                     {} is under {}",
                    dir.display(),
                    cache.display()
                );
            }
        }
        // And it is always the dedicated leaf, under whichever root resolved.
        assert!(
            dir.ends_with(NODE_SHIM_LEAF),
            "the node shim lives in its own dedicated dir: {}",
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

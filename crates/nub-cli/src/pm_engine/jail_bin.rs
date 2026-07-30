//! The nub-owned interpreter copy a Windows build jail runs its lifecycle scripts on.
//!
//! WHY A COPY EXISTS AT ALL. The build jail must hold at ZERO privilege — there is no setup
//! command and never will be — and on Windows a leaf read grant is an ACE, which needs
//! `WRITE_DAC` on the target. The stock Node MSI installs to `%ProgramFiles%\nodejs`, where a
//! standard user holds read but not `WRITE_DAC`; `C:\hostedtoolcache` (what `actions/setup-node`
//! unzips into) is the same. Measured de-elevated on a restricted token, granting the interpreter
//! there fails `Access is denied. (os error 5)` and the jail cannot start a single lifecycle
//! script. `%ProgramFiles%\nodejs` is the outlier that makes this nub's problem rather than
//! Windows': 43 of the 44 `C:\Program Files` children publish read to AppContainers already, so
//! an all-users Python or Visual Studio install needs no grant — the Node installer is the one
//! that ships without it.
//!
//! AND A SECOND, INDEPENDENT REASON, which is why widening the DACL would not have been the fix
//! even where nub can: `CreateProcessW` opens the image in the CALLER's context, and inside the
//! jail the caller is itself in the AppContainer. Naming the ambient `%ProgramFiles%\nodejs\
//! node.exe` by ABSOLUTE path from a confined `cmd.exe` gets `Access is denied.`, while the
//! identical command line unconfined succeeds — one variable, confinement (run 30517334191, both
//! Windows images). Everything a lifecycle script spawns is such a nested spawn. So the
//! interpreter has to be a file the jail can read, not merely one nub can point at.
//!
//! THE VERSION IS NOT NEGOTIABLE — a copy, never an upgrade. `prebuild-install` and
//! `node-gyp-build` both default the ABI they fetch for to `process.versions.modules` of the
//! RUNNING Node (`prebuild-install/util.js`, `node-gyp-build/node-gyp-build.js`), so staging a
//! newer Node than the project's would make that whole family download a prebuild that then fails
//! to load. The copy is byte-identical to the project's own distribution and keyed by the version
//! it reported, which makes the constraint structural rather than a check.
//!
//! WHAT LIVES WHERE. The publishing MECHANISM — grant the empty directory, copy into it, rename
//! into place — is the engine's
//! (`nub_sandbox::backend::windows_jail_bin::stage_appcontainer_readable_copy`), because it is
//! Windows confinement plumbing with no PM knowledge in it, and because that is what lets the
//! branch-scoped Windows probe drive the same code this does rather than a re-implementation of
//! it. What stays HERE is every PM decision: which interpreter, what the cache key is, when a
//! previous stage counts as complete, and how the child's env is rewritten.
//!
//! WHAT THIS DOES NOT DO. It does not copy Python or the MSVC toolchain, and must not: those are
//! the user's own, versions matter, and they are granted read where they already live. Nor is the
//! child's `PATH` replaced wholesale — only the source distribution is dropped from it. Dropping
//! the user's `git`/`python`/system entries would trade a jail that cannot start for packages that
//! cannot find their tools, and the ambient install needs no removal to be beaten anyway: an
//! un-ACE'd directory on `PATH` is SILENTLY skipped by cmd's probe, measured with the MSI dir
//! listed first and the child's own `execPath` still the nub-owned copy. It is dropped because a
//! shim that re-searches `PATH` (stock `npm.cmd`: `IF NOT EXIST "%~dp0\node.exe" SET
//! "NODE_EXE=node"`) is the class of defect that has defeated a caller-side allowlist before, and
//! removing the one entry closes it without costing the rest.

// Windows-only in production, but COMPILED EVERYWHERE on purpose: the path rewrite and the
// distribution-shape checks are ordinary logic, and compiling them on the dev host is what lets
// their tests run there instead of only on a Windows runner.
#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The staged distribution: what the child runs, and where it came from.
pub(super) struct JailBin {
    /// The staged interpreter — `<dir>/node.exe`.
    exe: PathBuf,
    /// The staged distribution root, which is also the interpreter's bin dir (the Windows
    /// layout is FLAT: `node.exe` with `node_modules` beside it, no `bin/`).
    dir: PathBuf,
    /// The distribution the copy was taken from. Removed from the child's `PATH`.
    source_root: PathBuf,
}

/// Stage the ambient interpreter into a nub-owned, AppContainer-readable directory, reusing a
/// previous stage when one is already published.
///
/// `None` — any reason at all — leaves the caller's env untouched, i.e. the behavior before this
/// existed. Every failure here is a jail that grants the ambient path instead, which is a worse
/// outcome than staging but a better one than refusing to install.
pub(super) fn stage(
    ambient: &BTreeMap<String, String>,
    probe: &super::build_jail::ProbeScope,
) -> Option<JailBin> {
    let exec = PathBuf::from(ambient.get("npm_node_execpath")?);
    // THE COPY SOURCE IS EXECUTED (to read its version) and then becomes the interpreter every
    // lifecycle script on the machine runs, so it passes the same scope the Python probe does: a
    // dependency that lands `node.exe` on the lifecycle `PATH` must not be able to nominate
    // itself as the thing nub stages and blesses.
    if !probe.allows(&exec) {
        return None;
    }
    // The FLAT Windows layout is the only one this handles, and the file name is the
    // discriminator (same rule as `build_jail::node_layout`). A POSIX `bin/node` reaching here
    // would stage the bin dir and miss `lib/`, so it declines instead.
    if !exec
        .file_name()
        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("node.exe"))
    {
        return None;
    }
    let source_root = exec.parent()?.to_path_buf();
    let dest = super::build_prefetch::cache_root()?
        .join("jail-bin")
        .join(super::build_prefetch::node_dist_key(ambient, probe)?);

    let bin = JailBin {
        exe: dest.join("node.exe"),
        dir: dest,
        source_root,
    };
    // `node.exe` IS the completeness test, and the engine's publish-by-rename is what makes it
    // sound: the destination is never observable half-populated, so its presence means the whole
    // distribution is there. Which file that is, is PM knowledge, so the engine asks the caller.
    if bin.exe.is_file() {
        return Some(bin);
    }
    // A leftover directory without `node.exe` cannot be a concurrent publish (those are atomic) —
    // it is broken state in a directory nub owns and keys by version, so it is replaced.
    if bin.dir.exists() {
        std::fs::remove_dir_all(&bin.dir).ok();
    }
    if publish(&bin.source_root, &bin.dir) {
        return Some(bin);
    }
    // A concurrent install may have published first, in which case its tree is a copy of the same
    // distribution and is adopted rather than refused.
    bin.exe.is_file().then_some(bin)
}

#[cfg(windows)]
fn publish(source: &Path, dest: &Path) -> bool {
    nub_sandbox::backend::windows_jail_bin::stage_appcontainer_readable_copy(source, dest).is_ok()
}

/// Staging exists for a Windows ACE constraint, so off Windows there is nothing to publish and
/// [`stage`] always declines. Present as a stub rather than `cfg`-ing the module out, so the env
/// rewrite and its tests compile — and run — on the dev host.
#[cfg(not(windows))]
fn publish(_source: &Path, _dest: &Path) -> bool {
    false
}

impl JailBin {
    /// Point the child at the staged copy: both interpreter spellings, and `PATH`.
    ///
    /// `NODE` is redirected as well as `npm_node_execpath`, which changes what it means inside the
    /// jail. Outside it, `NODE` is nub's own PATH shim so a build script's `$NODE child.js`
    /// re-enters nub augmented — but a jailed script runs on vanilla Node by contract
    /// (`NODE_COMPAT=1` is set unconditionally for exactly that reason), so the shim's only
    /// remaining effect would be a re-exec hop through a binary the jail does not grant. On
    /// Windows the shim is also a HARDLINK to `nub.exe`, and a hardlink shares one MFT record and
    /// therefore one security descriptor — so granting it would write nub's own binary's DACL
    /// (measured: an ace written on a link read back on the original). Naming the staged copy
    /// directly removes the hop and the leak together.
    pub(super) fn redirect_env(&self, ambient: &mut BTreeMap<String, String>) {
        let exe = self.exe.to_string_lossy().into_owned();
        ambient.insert("npm_node_execpath".to_string(), exe.clone());
        ambient.insert("NODE".to_string(), exe);
        if let Some(path) = ambient.get("PATH") {
            ambient.insert("PATH".to_string(), self.rewrite_path(path));
        }
    }

    /// `<staged dir>` in front, and every entry inside the source distribution dropped. Split out
    /// from the env write so the ordering and the removal are testable without a Node on disk.
    fn rewrite_path(&self, path: &str) -> String {
        let kept = std::iter::once(self.dir.clone())
            .chain(std::env::split_paths(path).filter(|dir| !dir.starts_with(&self.source_root)));
        std::env::join_paths(kept)
            .map(|joined| joined.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bin() -> JailBin {
        JailBin {
            exe: PathBuf::from("/cache/jail-bin/24.18.1-x64/node.exe"),
            dir: PathBuf::from("/cache/jail-bin/24.18.1-x64"),
            source_root: PathBuf::from("/progfiles/nodejs"),
        }
    }

    /// The staged dir leads, and the source distribution is gone — including the `npm/bin`
    /// subdirectory some installers add separately, which is why the filter is prefix-based
    /// rather than an equality test on the root.
    #[test]
    fn the_staged_dir_leads_and_the_source_distribution_is_dropped() {
        let sep = if cfg!(windows) { ';' } else { ':' };
        let original =
            ["/progfiles/nodejs", "/usr/bin", "/progfiles/nodejs/npm/bin"].join(&sep.to_string());
        let rewritten = bin().rewrite_path(&original);
        let entries: Vec<PathBuf> = std::env::split_paths(&rewritten).collect();
        assert_eq!(
            entries,
            vec![
                PathBuf::from("/cache/jail-bin/24.18.1-x64"),
                PathBuf::from("/usr/bin"),
            ]
        );
    }

    /// Both interpreter spellings move, because a lifecycle script reaches the interpreter
    /// through either — and `NODE` left pointing at nub's shim would send the child through a
    /// binary the jail does not grant.
    #[test]
    fn both_interpreter_spellings_name_the_staged_copy() {
        let mut env = BTreeMap::from([
            (
                "npm_node_execpath".to_string(),
                "/progfiles/nodejs/node.exe".to_string(),
            ),
            (
                "NODE".to_string(),
                "/tmp/nub-node-shim-1-abc/node.exe".to_string(),
            ),
        ]);
        bin().redirect_env(&mut env);
        let staged = "/cache/jail-bin/24.18.1-x64/node.exe";
        assert_eq!(
            env.get("npm_node_execpath").map(String::as_str),
            Some(staged)
        );
        assert_eq!(env.get("NODE").map(String::as_str), Some(staged));
        // No PATH in, no PATH out: the jail's env is an allowlist and inventing a key it did not
        // carry would put the staged dir on a child that had no PATH at all.
        assert!(!env.contains_key("PATH"));
    }
}

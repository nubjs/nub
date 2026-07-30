//! Publishing a nub-owned, AppContainer-readable COPY of a tool tree the jail must run.
//!
//! WHY THE ENGINE OWNS THIS. A Windows leaf read grant is an ACE, and writing one needs
//! `WRITE_DAC` on the target — which a standard user does not hold on an all-users install
//! (`%ProgramFiles%\nodejs`, `C:\hostedtoolcache`). The build jail must hold at ZERO privilege, so
//! where the ACE cannot be written the tree has to be somewhere nub owns. And a DACL widening
//! would not have sufficed even where nub can write one: `CreateProcessW` opens the image in the
//! CALLER's context, so once the caller is itself inside the AppContainer, opening an un-ACE'd
//! image is a confined open and is REFUSED — measured against the identical command line
//! unconfined (run 30517334191, both Windows images). A copy is the only thing that answers both.
//!
//! GRANT ON THE EMPTY DIRECTORY, THEN POPULATE — the whole reason this is a function rather than
//! two calls at a call site. The ace is inheritable, so every entry picks it up AT CREATION and
//! there is no propagation pass: 24 ms writing it empty against 426 ms re-granting an
//! already-populated 2,435-entry Node distribution (measured, run 30517506683). The same number
//! is also the per-launch saving, because an inheritable `ALL APPLICATION PACKAGES` ace is exactly
//! what [`super::windows_leaf_grant_redundant`] reports on — so the backend's own leaf grant on a
//! published tree SKIPS, and a per-run package sid, which would have to be written every single
//! spawn, is never needed.
//!
//! COPY, NEVER HARD LINK. An NTFS hard link is a second directory entry on the SAME MFT record,
//! and the security descriptor lives on the record — so an ace written "on the link" is an ace on
//! the original path. Measured in both directions, including onto a protected
//! `%ProgramFiles%` file. Linking would be a grant LEAK dressed as a saving.
//!
//! WHAT IT IS NOT. It is not a way to reach a toolchain the user already has: Python and MSVC are
//! granted read where they live, which works because 43 of the 44 `C:\Program Files` children
//! already publish read to AppContainers. The Node installer is the outlier this exists for.

use std::io;
use std::path::Path;

/// Bound the copy. A real Node distribution is 2,435 entries / ~101 MiB (measured on both Windows
/// images), so these never bind on one — they are what stops a caller handing over a mis-resolved
/// path and turning an install into an unbounded copy.
const MAX_ENTRIES: usize = 20_000;
const MAX_BYTES: u64 = 512 * 1024 * 1024;

/// Publish a copy of `source`'s tree at `dest`, readable and executable by every AppContainer.
///
/// `Ok(())` means `dest` now holds a complete copy. The caller decides what "complete" means for
/// its own tree and MUST test that before calling (to reuse a previous publish) and again after an
/// `Err` (to adopt a concurrent one) — the engine has no idea what file makes a Node distribution
/// usable, and inventing a sentinel here would be the wrong crate guessing.
///
/// ATOMIC BY RENAME. The copy lands in a staging sibling and is renamed into place, so `dest` is
/// never observable half-populated and two concurrent publishers cannot see each other's partial
/// tree. The ace is written while the staging directory is still EMPTY and travels with it: an
/// EXPLICIT ace is not recomputed by a same-volume move, and the entries' inherited copies are
/// real ace entries rather than a computation redone at the destination.
///
/// The trustee is the STABLE `ALL APPLICATION PACKAGES`, not a per-run profile sid, which is sound
/// because a zero-capability LowBox token reads through it (it is why System32 is readable at
/// all). The cost is that `dest` becomes readable to every AppContainer on the machine, so ONLY
/// PUBLIC BYTES MAY BE PUBLISHED HERE — the intended tree is a copy of a Node distribution, which
/// is public bytes from nodejs.org. Do not hand this a tree carrying user data.
///
/// Symlinks and reparse points in `source` are SKIPPED, not followed: the Windows Node archive has
/// none, and following one would either copy an unbounded foreign tree in or leave the jail
/// reading through a link whose target was never granted.
pub fn stage_appcontainer_readable_copy(source: &Path, dest: &Path) -> io::Result<()> {
    let parent = dest.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("staging destination has no parent: {}", dest.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let staging = tempfile::TempDir::new_in(parent)?;
    publish_read(staging.path())?;
    copy_tree(source, staging.path(), &mut Budget::default())?;
    let staged = staging.keep();
    std::fs::rename(&staged, dest).inspect_err(|_| {
        std::fs::remove_dir_all(&staged).ok();
    })
}

#[cfg(target_os = "windows")]
fn publish_read(dir: &Path) -> io::Result<()> {
    super::windows::windows_publish_appcontainer_read(dir)
}

/// The copy half is ordinary fs work, so it compiles and is tested on the dev host; only the ace
/// needs Windows. A non-Windows build therefore stages an UNPUBLISHED copy, which is why nothing
/// but the Windows backend calls this.
#[cfg(not(target_os = "windows"))]
fn publish_read(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[derive(Default)]
struct Budget {
    entries: usize,
    bytes: u64,
}

impl Budget {
    fn exceeded(&self, what: &str) -> io::Error {
        io::Error::other(format!(
            "staging source exceeds the {what} bound ({} entries, {} bytes)",
            self.entries, self.bytes
        ))
    }
}

fn copy_tree(source: &Path, dest: &Path, budget: &mut Budget) -> io::Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_type()?.is_symlink() {
            continue;
        }
        budget.entries += 1;
        if budget.entries > MAX_ENTRIES {
            return Err(budget.exceeded("entry-count"));
        }
        let target = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir(&target)?;
            copy_tree(&entry.path(), &target, budget)?;
            continue;
        }
        budget.bytes += entry.metadata()?.len();
        if budget.bytes > MAX_BYTES {
            return Err(budget.exceeded("total-bytes"));
        }
        std::fs::copy(entry.path(), &target)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tree arrives whole and nested, and `dest` is created by the rename rather than
    /// pre-existing — the property the atomicity rests on.
    #[test]
    fn the_published_copy_is_complete_and_nested() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src");
        std::fs::create_dir_all(source.join("node_modules/npm/bin")).unwrap();
        std::fs::write(source.join("node.exe"), b"MZ").unwrap();
        std::fs::write(source.join("node_modules/npm/bin/npm-cli.js"), b"//").unwrap();

        let dest = tmp.path().join("published/24.18.1-x64");
        assert!(!dest.exists());
        stage_appcontainer_readable_copy(&source, &dest).expect("publishes");
        assert_eq!(std::fs::read(dest.join("node.exe")).unwrap(), b"MZ");
        assert!(dest.join("node_modules/npm/bin/npm-cli.js").is_file());
    }

    /// A source over the bound is refused rather than copied, and — because the staging dir is
    /// dropped on the error path — leaves no partial tree at `dest` for a completeness test to
    /// mistake for a publish.
    #[test]
    fn an_oversized_source_is_refused_and_publishes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("huge.bin"), vec![0u8; 8]).unwrap();

        let mut budget = Budget {
            entries: 0,
            bytes: MAX_BYTES,
        };
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        assert!(copy_tree(&source, &dest, &mut budget).is_err());
        assert!(!dest.join("huge.bin").exists());
    }
}

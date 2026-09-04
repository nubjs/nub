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
    publish_by_rename(&staged, dest).inspect_err(|_| {
        std::fs::remove_dir_all(&staged).ok();
    })
}

/// How long to keep retrying a rename another process is transiently blocking.
///
/// A real-user scanner cleared at ~3.9s in measurement, so the window is several times that: the cost
/// of waiting is a slow install, and the cost of giving up is a BROKEN one (see [`publish_by_rename`]).
#[cfg(windows)]
const PUBLISH_RETRY_WINDOW: std::time::Duration = std::time::Duration::from_secs(20);

/// Rename the staged tree into place, retrying while another process is transiently holding it.
///
/// ⛔⛔ WHY A RETRY IS THE FIX AND NOT A GUARDRAIL. `copy_tree` has just written executables, and an
/// antivirus scanner opens exactly those to scan them — Defender is the ordinary case, not an exotic
/// one. Windows refuses a DIRECTORY rename while any handle is open on a file inside it, so the
/// publish fails for a reason that has nothing to do with nub and everything to do with timing.
///
/// What made it worth fixing rather than tolerating is what the failure costs. This function's caller
/// treats a failed publish as "decline", and declining leaves the confined child pointed at the
/// AMBIENT interpreter — an image the AppContainer token cannot open at all (see
/// `nub-cli/src/pm_engine/jail_bin.rs`'s module doc). So a transient scanner collision did not degrade
/// the jail, it broke the install.
///
/// Two bounds keep this from turning a fast failure into a slow one:
/// - only a transient BLOCK is retried, never any other error, so a genuine permission or layout
///   problem still fails immediately;
/// - if `dest` has appeared meanwhile, a concurrent publisher won the race and its tree is a copy of
///   the same distribution, so this returns at once and lets the caller adopt it rather than waiting
///   out a window that can never clear.
#[cfg(windows)]
fn publish_by_rename(staged: &Path, dest: &Path) -> io::Result<()> {
    let deadline = std::time::Instant::now() + PUBLISH_RETRY_WINDOW;
    let mut backoff = std::time::Duration::from_millis(20);
    loop {
        match std::fs::rename(staged, dest) {
            Ok(()) => return Ok(()),
            Err(err) => {
                // A concurrent publish is not something to wait out.
                if dest.is_dir() {
                    return Err(err);
                }
                if !is_transient_publish_block(err.raw_os_error())
                    || std::time::Instant::now() >= deadline
                {
                    return Err(err);
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(std::time::Duration::from_millis(400));
            }
        }
    }
}

#[cfg(not(windows))]
fn publish_by_rename(staged: &Path, dest: &Path) -> io::Result<()> {
    std::fs::rename(staged, dest)
}

/// Is this raw OS error a transient hold on the staged tree, rather than a real refusal?
///
/// Takes the raw code rather than the `io::Error` so the numbers can be asserted directly off Windows;
/// they are Windows codes and mean nothing on another platform. BOTH are needed and only one is
/// obvious: the intuitive `ERROR_SHARING_VIOLATION` (32) is NOT what a scanner collision produces here
/// — a measured reproduction reported `ERROR_ACCESS_DENIED` (5), and every share mode was refused
/// including full `READ|WRITE|DELETE`. A predicate keyed on 32 alone would never have fired.
fn is_transient_publish_block(raw: Option<i32>) -> bool {
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    matches!(
        raw,
        Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
    )
}

#[cfg(test)]
mod publish_retry_tests {
    use super::is_transient_publish_block;

    /// The retry has to ride out a handle held INSIDE the staged tree, which is the real-world shape:
    /// a scanner opens a freshly written executable and Windows then refuses the directory rename.
    /// Deterministic rather than waiting on Defender — this test holds the handle itself and drops it
    /// on a timer, so the assertion is that publishing SUCCEEDS after the hold clears, and that it
    /// actually waited rather than winning immediately.
    #[cfg(windows)]
    #[test]
    fn a_handle_held_inside_the_staged_tree_is_waited_out_rather_than_failing_the_publish() {
        use std::io::Write as _;
        let root = tempfile::tempdir().expect("tempdir");
        let staged = root.path().join("staged");
        std::fs::create_dir_all(&staged).expect("staged dir");
        let held = staged.join("node.exe");
        // `MZ` because that is what a scanner reacts to, and what the original reproduction used.
        let mut file = std::fs::File::create(&held).expect("create");
        file.write_all(b"MZ\0\0").expect("write");

        let dest = root.path().join("published");
        let start = std::time::Instant::now();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(750));
            drop(file); // release the hold; the retry should then succeed
        });

        let result = super::publish_by_rename(&staged, &dest);
        releaser.join().expect("releaser");

        assert!(
            result.is_ok(),
            "the publish must survive a transient hold: {result:?}"
        );
        assert!(
            dest.join("node.exe").is_file(),
            "the published tree must be complete"
        );
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(700),
            "it should have waited for the hold to clear, not won instantly — \
             if this fires, an open handle no longer blocks the rename and the test proves nothing"
        );
    }

    /// The code that actually fires is the counter-intuitive one. A measured reproduction of a scanner
    /// holding a handle inside the staged tree reported `ERROR_ACCESS_DENIED` (5), not
    /// `ERROR_SHARING_VIOLATION` (32) — so a predicate written from intuition would never have fired,
    /// and the install would still break. Asserted on raw codes so this runs on the dev host too.
    #[test]
    fn access_denied_counts_as_transient_and_an_unrelated_error_does_not() {
        assert!(
            is_transient_publish_block(Some(5)),
            "ERROR_ACCESS_DENIED is what a scanner collision reports"
        );
        assert!(
            is_transient_publish_block(Some(32)),
            "ERROR_SHARING_VIOLATION is the documented sibling"
        );
        assert!(
            is_transient_publish_block(Some(33)),
            "ERROR_LOCK_VIOLATION is the same class"
        );
        // Anything else must fail fast: waiting out a window that cannot clear turns a quick decline
        // into a 20-second one.
        assert!(
            !is_transient_publish_block(Some(2)),
            "FILE_NOT_FOUND is not transient"
        );
        assert!(
            !is_transient_publish_block(Some(183)),
            "ALREADY_EXISTS is a concurrent publish, not a hold"
        );
        assert!(
            !is_transient_publish_block(Some(87)),
            "INVALID_PARAMETER is a real defect"
        );
        assert!(
            !is_transient_publish_block(None),
            "an error with no OS code cannot be classified"
        );
    }
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

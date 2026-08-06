//! Re-anchor pinned runtime objects at paths Bubblewrap can still resolve.
//!
//! `--ro-bind-fd FD DEST` is not a descriptor bind. Bubblewrap rewrites it to an ordinary
//! bind whose source is the literal string `/proc/self/fd/FD`, then `realpath()`s that
//! string from `resolve_symlinks_in_ops()` — which runs AFTER it has entered the new user
//! namespace and written a map covering exactly one uid and one gid. Resolving the source
//! by name that late is the root of all three problems this module exists for — two that
//! abort a launch which is otherwise correct:
//!
//! * **euid 0 with nub under a 0750 `$HOME`** — Ubuntu's `HOME_MODE` default since 21.04,
//!   so the curl installer's `~/.nub/bin/nub` and every dev build. A capability held in a
//!   non-initial user namespace does not override DAC for an inode whose uid is not mapped
//!   into that namespace, so root-inside-the-namespace loses the traversal it had outside
//!   and `realpath()` returns EACCES. This is what breaks `sudo nub install`.
//! * **the pinned file unlinked while it is still running** — an in-place upgrade. The
//!   descriptor stays valid, but `/proc/self/fd/FD` reads back as `<path> (deleted)` and
//!   `realpath()` returns ENOENT.
//!
//! and one that does something worse than abort it:
//!
//! * **a closure object the invoking user can write**, which under `sudo` is a privilege
//!   escalation: overwriting the pinned inode in place leaves the name, the dev/ino and the
//!   resolution untouched while the bytes become the attacker's, and the monitor runs them
//!   as root. See requirement 2 on [`bwrap_resolves`] for the measured primitive and why
//!   ETXTBSY does not cover it.
//!
//! The repair for both is to copy the object out of the descriptor that was ALREADY pinned
//! and verified, into a private directory every component of which resolves under those
//! reduced credentials, and bind the copy. Copying from the descriptor rather than
//! re-opening the path is what preserves the anti-TOCTOU guarantee: the staged bytes are
//! the bytes that were verified, even when the original path has since come to name a
//! different file.
//!
//! Staging is a REPAIR, not a precondition. When the object already resolves, nothing is
//! copied; when no usable staging directory exists, or the copy fails, the original
//! descriptor is left in place and Bubblewrap remains the oracle. A host that launches
//! today therefore cannot be broken by the attempt.

use super::linux_monitor::{FileIdentity, PinnedObject, duplicate_above_stdio};
use std::collections::HashMap;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Bases tried in order. `TMPDIR` is deliberately NOT honoured: under `sudo` it still
/// carries the invoking user's value, and staging the monitor executable somewhere that
/// user controls would hand them the privileged binary the sandbox is about to exec.
const STAGE_BASES: &[&str] = &["/tmp", "/var/tmp", "/run"];

const STAGE_PREFIX: &str = "nub-sandbox-runtime.";

/// Held with `LOCK_EX` for the owning process's whole life, so a later run can tell a live
/// staging directory from an abandoned one. nub-cli deliberately leaks its
/// `RuntimeCapability`, so [`StagedRuntime`]'s destructor never runs in production and
/// reclaiming abandoned directories is the only cleanup there is.
///
/// A lock rather than the pid in the directory name: a pid is meaningful only in the
/// namespace that minted it, so a nub in a container sharing the host `/tmp` would read
/// another namespace's pid as dead and delete a LIVE image out from under it. `flock` is
/// namespace-independent and the kernel releases it exactly when the holder dies.
const STAGE_LOCK: &str = ".lock";

const COPY_CHUNK: usize = 1 << 20;

/// Owns the staging directory for as long as the runtime image that binds out of it.
pub(super) struct StagedRuntime {
    dir: PathBuf,
    _lock: File,
}

impl std::fmt::Debug for StagedRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedRuntime")
            .field("dir", &self.dir)
            .finish()
    }
}

impl Drop for StagedRuntime {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Accumulates the re-anchoring decisions for one runtime image.
pub(super) struct RuntimeStaging {
    dir: Option<(PathBuf, File)>,
    /// Set once no staging directory could be established, so the remaining objects skip
    /// the (filesystem-touching) search instead of repeating it.
    unavailable: bool,
    /// One copy per SOURCE inode. A library reached under several `DT_SONAME` aliases is
    /// several `PinnedObject`s over one file; re-anchoring must not fan that into several
    /// distinct staged inodes, because the manifest the monitor verifies against records
    /// one identity per alias and today they legitimately coincide.
    staged: HashMap<(u64, u64), StagedObject>,
}

struct StagedObject {
    file: File,
    path: PathBuf,
    identity: FileIdentity,
}

impl RuntimeStaging {
    pub(super) fn new() -> Self {
        Self {
            dir: None,
            unavailable: false,
            staged: HashMap::new(),
        }
    }

    /// Point `object` at a copy Bubblewrap can resolve, if it needs one and one can be
    /// made. Never fails the caller: an unrepairable object keeps its original descriptor.
    pub(super) fn reanchor(&mut self, object: &mut PinnedObject) {
        if bwrap_resolves(object) {
            return;
        }
        // Read before any mutation: the map is keyed by the ORIGINAL inode, which is what
        // makes two aliases over one file collapse onto one copy.
        let key = (object.identity.dev, object.identity.ino);
        if let Some(existing) = self.staged.get(&key) {
            let replacement = existing
                .file
                .try_clone()
                .map(|file| (file, existing.path.clone(), existing.identity));
            if let Ok((file, path, identity)) = replacement {
                self.adopt(object, file, path, identity);
            }
            return;
        }
        let Some(dir) = self.directory() else {
            return;
        };
        let staged = match stage_object(&dir, object) {
            Ok(staged) => staged,
            Err(error) => {
                tracing::debug!(
                    object = %object.source_path.display(),
                    %error,
                    "staging a sandbox runtime object failed; binding the original path"
                );
                return;
            }
        };
        match duplicate_above_stdio(&staged.file) {
            Ok(file) => {
                if self.adopt(object, file, staged.path.clone(), staged.identity) {
                    self.staged.insert(key, staged);
                }
            }
            Err(error) => tracing::debug!(%error, "duplicating a staged runtime descriptor"),
        }
    }

    /// Swap the copy in, then hold it to the same test the original failed. Proving the
    /// replacement rather than assuming it is what makes "a host that launches today cannot
    /// be broken by the attempt" an enforced property: a staging directory that is itself
    /// unresolvable (a symlinked base whose real ancestors are not searchable) rolls back
    /// to the original descriptor instead of substituting something no better.
    fn adopt(
        &self,
        object: &mut PinnedObject,
        file: File,
        path: PathBuf,
        identity: FileIdentity,
    ) -> bool {
        let original = std::mem::replace(
            object,
            PinnedObject {
                file,
                source_path: path,
                identity,
                private_name: object.private_name.clone(),
            },
        );
        if bwrap_resolves(object) {
            return true;
        }
        tracing::debug!(
            staged = %object.source_path.display(),
            "the staged runtime object is no more reachable than the original; rolling back"
        );
        *object = original;
        false
    }

    /// The guard the runtime image must hold, or `None` when nothing was staged.
    pub(super) fn finish(self) -> Option<StagedRuntime> {
        self.dir
            .map(|(dir, lock)| StagedRuntime { dir, _lock: lock })
    }

    fn directory(&mut self) -> Option<PathBuf> {
        if let Some((dir, _)) = &self.dir {
            return Some(dir.clone());
        }
        if self.unavailable {
            return None;
        }
        match create_stage_dir() {
            Some((dir, lock)) => {
                self.dir = Some((dir.clone(), lock));
                Some(dir)
            }
            None => {
                self.unavailable = true;
                None
            }
        }
    }
}

/// Whether the NAME is a faithful stand-in for the descriptor at the moment Bubblewrap
/// re-resolves it — which is what decides whether the copy is optional or mandatory.
///
/// Two independent requirements, and only the first is about reachability:
///
/// 1. `realpath()` must succeed under the sandbox user namespace's credentials, where a
///    capability overrides DAC only for an inode BOTH of whose owners are mapped and
///    everything else falls back to plain DAC under this process's own uid/gid with
///    supplementary groups dropped. Wrong in the safe direction only: an ACL or LSM that
///    grants access this cannot see costs a needless copy, never a missed repair.
///
/// 2. No other uid may be able to change what the name refers to. THIS ONE IS A PRIVILEGE
///    BOUNDARY, not robustness. Bubblewrap binds the path it re-resolves, not the
///    descriptor nub pinned and verified, and the window between the two is wide.
///
///    The primitive is an IN-PLACE overwrite, and only that — all three were measured.
///    Renaming a replacement over the path leaves `/proc/self/fd/N` reading back as
///    `<path> (deleted)`, so Bubblewrap fails ENOENT. Renaming the original aside and
///    planting a replacement makes the link follow the ORIGINAL's dentry to its new name,
///    so Bubblewrap binds the original. Both are denial of service. Writing THROUGH the
///    existing inode is the one that works: the link, the dev/ino and the resolution all
///    stay intact while the bytes become the attacker's, and the monitor runs them AS ROOT.
///
///    ETXTBSY covers less of this than it looks. The running executable cannot be written,
///    but a mapped SHARED LIBRARY can (measured — the kernel takes a write-deny for
///    `execve`, not for `mmap`), and the loader and every `DT_NEEDED` library are bound
///    alongside the executable. So the exposure is a closure object sitting where the
///    invoking user can write while nub runs under `sudo`. Nothing downstream catches it:
///    the in-sandbox identity check and the readiness marker are both attestations BY the
///    substituted code. Copying the bytes out of the descriptor takes the name out of the
///    trust path, so a writable chain is staged even though it resolves perfectly well.
///    Ubuntu's 0750 home is the SAFE shape here precisely because it forces staging.
fn bwrap_resolves(object: &PinnedObject) -> bool {
    let link = PathBuf::from(format!("/proc/self/fd/{}", object.file.as_raw_fd()));
    let Ok(path) = fs::canonicalize(&link) else {
        return false;
    };
    let uid = unsafe { libc::getuid() };
    if !fs::metadata(&path).is_ok_and(|meta| {
        meta.dev() == object.identity.dev
            && meta.ino() == object.identity.ino
            && meta.len() == object.identity.size
            && tamper_proof(&meta, uid)
    }) {
        return false;
    }
    resolvable_directory_chain(&path)
}

fn resolvable_directory_chain(path: &Path) -> bool {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    path.ancestors().skip(1).all(|dir| {
        fs::metadata(dir).is_ok_and(|meta| {
            searchable(meta.uid(), meta.gid(), meta.mode(), uid, gid) && tamper_proof(&meta, uid)
        })
    })
}

/// Whether this component denies every uid but ours the ability to replace what it holds.
/// See requirement 2 on [`bwrap_resolves`] for why a "no" is a privilege boundary.
fn tamper_proof(meta: &Metadata, uid: u32) -> bool {
    tamper_proof_parts(meta.uid(), meta.mode(), meta.is_dir(), uid)
}

fn tamper_proof_parts(owner: u32, mode: u32, is_dir: bool, uid: u32) -> bool {
    // A root-owned component is not an escalation vector: root already outranks whoever is
    // launching. Any OTHER foreign owner can rewrite or rename the entry at will.
    if owner != uid && owner != 0 {
        return false;
    }
    // Sticky withholds rename and unlink of entries you do not own, which is exactly the
    // substitution at issue — it is what keeps a 1777 `/tmp` usable as a staging base.
    if is_dir && mode & 0o1000 != 0 {
        return true;
    }
    mode & 0o022 == 0
}

fn searchable(owner: u32, group: u32, mode: u32, uid: u32, gid: u32) -> bool {
    if owner == uid && group == gid {
        return true;
    }
    if owner == uid {
        return mode & 0o100 != 0;
    }
    if group == gid {
        return mode & 0o010 != 0;
    }
    mode & 0o001 != 0
}

fn create_stage_dir() -> Option<(PathBuf, File)> {
    let uid = unsafe { libc::getuid() };
    let pid = std::process::id();
    let mut suffix = [0u8; 8];
    getrandom::getrandom(&mut suffix).ok()?;
    let suffix = u64::from_le_bytes(suffix);
    for base in STAGE_BASES {
        // Canonicalized because `Path::ancestors` is purely lexical: on a host where a base
        // is a symlink, the searchability of the link's REAL ancestors is what Bubblewrap
        // will meet, and the lexical chain would never look at them.
        let Ok(base) = fs::canonicalize(base) else {
            continue;
        };
        if !base.is_dir() {
            continue;
        }
        sweep_abandoned(&base, uid);
        let dir = base.join(format!("{STAGE_PREFIX}{pid}.{suffix:016x}"));
        // 0700 at CREATION, not afterwards: `mkdir` applies the umask, so a
        // create-then-chmod would leave the directory group/other-readable for a window in
        // a world-writable base. `mkdir` also never follows a final symlink, so a
        // pre-created name there is an EEXIST rather than a redirect.
        if fs::DirBuilder::new().mode(0o700).create(&dir).is_err() {
            continue;
        }
        if let Some(lock) = claim(&dir)
            && resolvable_directory_chain(&dir.join("x"))
            && exec_permitted(&dir)
        {
            return Some((dir, lock));
        }
        let _ = fs::remove_dir_all(&dir);
    }
    None
}

fn claim(dir: &Path) -> Option<File> {
    let lock = File::create(dir.join(STAGE_LOCK)).ok()?;
    (unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0).then_some(lock)
}

/// A `noexec` staging filesystem would produce a monitor that binds and then cannot exec,
/// so the base is rejected before a multi-megabyte copy lands on it. `faccessat(X_OK)`
/// reports the mount flag, not just the mode bits — including for root.
fn exec_permitted(dir: &Path) -> bool {
    let probe = dir.join("exec-probe");
    let permitted = create_owner_executable(&probe).is_ok() && access_x_ok(&probe);
    let _ = fs::remove_file(&probe);
    permitted
}

/// `OpenOptions::mode` is `open(2)`'s mode argument and the umask masks it, so a umask
/// carrying the owner-execute bit would silently produce a non-executable file — and then
/// `exec_permitted` would reject every base. `chmod` is not masked, so it is what decides.
fn create_owner_executable(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o500)
        .open(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o500))?;
    Ok(file)
}

fn access_x_ok(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

/// One object's copy. The destination is named after `private_name`, which is unique across
/// an image by construction (the fixed `nub-monitor` and `ld.so`, then one entry per
/// resolved `DT_SONAME`); were that ever to stop holding, `create_new` turns the collision
/// into an error and the object keeps its original descriptor rather than binding another
/// object's bytes.
fn stage_object(dir: &Path, object: &PinnedObject) -> io::Result<StagedObject> {
    let path = dir.join(&object.private_name);
    let mut destination = create_owner_executable(&path)?;
    // `pread` throughout: the source descriptor is a `dup` of a caller-owned file and
    // shares its offset, so a read that advanced it would corrupt an unrelated reader.
    let copied = copy_from_offset_zero(&object.file, &mut destination, object.identity.size);
    if let Err(error) = copied {
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    // The writable descriptor must be gone before the monitor is exec'd from this file:
    // Linux refuses `execve` with ETXTBSY while any process holds the image open for
    // writing, so keeping one read-write handle instead of reopening read-only would break
    // every launch.
    drop(destination);
    let file = duplicate_above_stdio(&File::open(&path)?)?;
    let identity = FileIdentity::from_file(&file)?;
    if identity.size != object.identity.size {
        let _ = fs::remove_file(&path);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "staged sandbox runtime object changed size while being written",
        ));
    }
    Ok(StagedObject {
        file,
        path,
        identity,
    })
}

fn copy_from_offset_zero(source: &File, destination: &mut File, size: u64) -> io::Result<()> {
    let mut buffer = vec![0u8; COPY_CHUNK];
    let mut offset = 0u64;
    while offset < size {
        let want = COPY_CHUNK.min((size - offset) as usize);
        let read = source.read_at(&mut buffer[..want], offset)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "sandbox runtime object was truncated while being staged",
            ));
        }
        destination.write_all_at(&buffer[..read], offset)?;
        offset += read as u64;
    }
    Ok(())
}

/// Reclaim staging directories whose owner is gone, identified by a lock nobody holds.
/// Only a real directory this uid owns is a candidate, so a name planted by another user in
/// a world-writable base is never followed or removed; the bases are sticky or root-owned,
/// so nobody else can swap a candidate between the check and the removal.
fn sweep_abandoned(base: &Path, uid: u32) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(STAGE_PREFIX)
        {
            continue;
        }
        let path = entry.path();
        if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir() && meta.uid() == uid) {
            continue;
        }
        // The lock is dropped here either way: taking it proves the owner is gone, and
        // holding it no longer matters once the directory is being removed.
        if claim(&path).is_some() {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A fixture root that satisfies [`bwrap_resolves`] by construction, for the tests that
    /// must watch an object stop resolving and therefore have to see it resolve FIRST.
    ///
    /// `tempfile::tempdir()` cannot carry that precondition: it honours `TMPDIR`, and a
    /// runner whose temp root is foreign-owned or group-writable fails the very first
    /// assertion, so the property under test never runs and the failure says nothing about
    /// the code. Reusing production's own base search instead makes the fixture valid
    /// exactly where staging itself is — the same reason bubblewrap's suite stages under a
    /// hardcoded `/var/tmp` rather than the environment's.
    ///
    /// `None` is the one honest skip, and it announces itself on the REAL stderr because
    /// libtest swallows the `eprintln!` family on a run that passes.
    fn resolvable_root(test: &str) -> Option<StagedRuntime> {
        match create_stage_dir() {
            Some((dir, lock)) => Some(StagedRuntime { dir, _lock: lock }),
            None => {
                let _ = std::io::stderr().write_all(
                    format!(
                        "SKIP {test}: no base in {STAGE_BASES:?} yields a directory whose chain \
                         resolves, so this test's precondition cannot hold on this host.\n"
                    )
                    .as_bytes(),
                );
                None
            }
        }
    }

    /// `fs::write` creates 0666 masked by the umask, so a 002 umask — Debian's
    /// `USERGROUPS_ENAB` default — leaves the fixture group-writable, which `tamper_proof`
    /// then rejects for exactly the right reason. Pin the mode instead of inheriting it.
    fn write_fixture(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn a_0750_home_owned_by_a_foreign_uid_is_unsearchable_from_the_namespace() {
        // The `sudo nub install` shape: uid 0, gid 0 mapped; `/home/<user>` is 1001:1001
        // 0750, so the capability does not apply and the "other" bits decide. This is the
        // single fact the whole re-anchoring decision turns on.
        assert!(!searchable(1001, 1001, 0o750, 0, 0));
        // Its own owner still traverses it, capability or not.
        assert!(searchable(1001, 1001, 0o750, 1001, 1001));
        // One search bit for the world is enough, whoever owns it.
        assert!(searchable(1001, 1001, 0o751, 0, 0));
    }

    #[test]
    fn the_owner_class_decides_even_when_the_group_would_be_more_generous() {
        // Plain DAC: the owner class wins outright for a matching uid, so a 0-owner-x
        // directory is unsearchable by its owner however open the group bits are.
        assert!(!searchable(1001, 50, 0o070, 1001, 1001));
        assert!(searchable(1001, 50, 0o070, 4242, 50));
    }

    #[test]
    fn a_component_a_foreign_uid_can_rewrite_is_not_tamper_proof() {
        // The escalation shape: `sudo nub install` with nub at `~/.nub/bin/nub` on a 0755
        // home. Every component is world-searchable, so reachability says "fine" — but
        // `~/.nub/bin` is owned by uid 1000, who can rename an ELF over the pinned binary
        // between nub's check and Bubblewrap's re-resolve, and the monitor would exec it as
        // root. Staging is mandatory here, and this is the predicate that says so.
        assert!(
            searchable(1000, 1000, 0o755, 0, 0),
            "the fixture must be reachable"
        );
        assert!(!tamper_proof_parts(1000, 0o755, false, 0));
        // Root-owned is not an escalation vector: root already outranks the launcher.
        assert!(tamper_proof_parts(0, 0o755, false, 0));
        assert!(tamper_proof_parts(0, 0o755, false, 1000));
        // Group- or world-writable is, whoever owns it.
        assert!(!tamper_proof_parts(0, 0o775, false, 0));
        assert!(!tamper_proof_parts(0, 0o777, false, 0));
        // ...unless sticky withholds rename/unlink of entries you do not own, which is what
        // keeps a 1777 `/tmp` usable as a staging base.
        assert!(tamper_proof_parts(0, 0o1777, true, 0));
        // Sticky is a directory property; it exculpates nothing on the bound file itself.
        assert!(!tamper_proof_parts(0, 0o1777, false, 0));
    }

    #[test]
    fn an_unlinked_descriptor_does_not_resolve() {
        let Some(root) = resolvable_root("an_unlinked_descriptor_does_not_resolve") else {
            return;
        };
        let path = root.dir.join("victim");
        write_fixture(&path, b"payload");
        let file = File::open(&path).unwrap();
        let identity = FileIdentity::from_file(&file).unwrap();
        let object = PinnedObject {
            file,
            source_path: path.clone(),
            identity,
            private_name: "victim".into(),
        };
        assert!(
            bwrap_resolves(&object),
            "the fixture must resolve before it is broken"
        );
        fs::remove_file(&path).unwrap();
        assert!(!bwrap_resolves(&object));
    }

    #[test]
    fn a_path_that_now_names_a_different_inode_does_not_resolve() {
        let Some(root) =
            resolvable_root("a_path_that_now_names_a_different_inode_does_not_resolve")
        else {
            return;
        };
        let path = root.dir.join("upgraded");
        write_fixture(&path, b"old");
        let file = File::open(&path).unwrap();
        let identity = FileIdentity::from_file(&file).unwrap();
        let object = PinnedObject {
            file,
            source_path: path.clone(),
            identity,
            private_name: "upgraded".into(),
        };
        assert!(
            bwrap_resolves(&object),
            "the fixture must resolve before it is replaced"
        );
        let replacement = root.dir.join("replacement");
        write_fixture(&replacement, b"new");
        fs::rename(&replacement, &path).unwrap();
        assert!(!bwrap_resolves(&object));
    }

    #[test]
    fn staging_copies_the_descriptor_bytes_and_yields_a_resolvable_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("original");
        fs::write(&path, b"runtime-image-bytes").unwrap();
        let file = File::open(&path).unwrap();
        let identity = FileIdentity::from_file(&file).unwrap();
        let mut object = PinnedObject {
            file,
            source_path: path.clone(),
            identity,
            private_name: "nub-monitor".into(),
        };
        // Unlink first, so the only way to produce a resolvable object is through the
        // descriptor — the B3 shape, and the proof that no path re-open is involved.
        fs::remove_file(&path).unwrap();
        assert!(!bwrap_resolves(&object));

        let mut staging = RuntimeStaging::new();
        staging.reanchor(&mut object);
        assert!(
            bwrap_resolves(&object),
            "re-anchored object still does not resolve"
        );
        assert_ne!(object.identity.ino, identity.ino);
        assert_eq!(object.identity.size, identity.size);
        assert_eq!(
            fs::read(&object.source_path).unwrap(),
            b"runtime-image-bytes"
        );

        let staged_dir = object.source_path.parent().unwrap().to_path_buf();
        drop(staging.finish());
        assert!(
            !staged_dir.exists(),
            "the staging directory outlived its guard"
        );
    }

    #[test]
    fn an_unlocked_staging_directory_is_reclaimed_and_a_locked_one_survives() {
        use std::os::unix::ffi::OsStringExt;

        // flock is namespace-independent and per open-file-description, so a child holding
        // the lock contends exactly as an abandoned run's process would.
        //
        // The holder is a CHILD, and this process never opens the lock at all. That is
        // hermeticity, not ceremony: `fork` in ANY concurrent test thread duplicates every
        // open file description this process holds, and an flock outlives `drop` for as
        // long as that copy does. Holding the lock here instead made the release half fail
        // ~1 run in 50 of the full suite and never once in isolation — measured, with the
        // stale holder visible in `/proc/locks` under this very pid after the drop.
        // Nothing can duplicate a descriptor this process never had, and `waitpid`
        // returning proves the child's copy is gone rather than merely dropped.
        let base = tempfile::tempdir().unwrap();
        let uid = unsafe { libc::getuid() };
        let live = base
            .path()
            .join(format!("{STAGE_PREFIX}1.0000000000000001"));
        let abandoned = base
            .path()
            .join(format!("{STAGE_PREFIX}2.0000000000000002"));
        fs::create_dir(&live).unwrap();
        fs::create_dir(&abandoned).unwrap();

        // Built BEFORE the fork: a child forked from a multi-threaded parent may only run
        // async-signal-safe code, which rules out the allocation `Path::join` would make.
        let lock_path =
            std::ffi::CString::new(live.join(STAGE_LOCK).into_os_string().into_vec()).unwrap();
        let mut ready = [-1; 2];
        assert_eq!(unsafe { libc::pipe(ready.as_mut_ptr()) }, 0);
        let [ready_read, ready_write] = ready;

        let holder = unsafe { libc::fork() };
        assert!(holder >= 0, "{}", io::Error::last_os_error());
        if holder == 0 {
            let fd = unsafe {
                libc::open(
                    lock_path.as_ptr(),
                    libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
                    0o600,
                )
            };
            let locked = fd >= 0 && unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0;
            let byte = u8::from(locked);
            let _ = unsafe { libc::write(ready_write, (&raw const byte).cast(), 1) };
            // The parent's SIGKILL is what releases the lock. The alarm is a backstop so a
            // parent that panics before it gets there orphans nothing.
            unsafe { libc::alarm(60) };
            loop {
                unsafe { libc::pause() };
            }
        }
        assert_eq!(unsafe { libc::close(ready_write) }, 0);
        let mut byte = 0u8;
        assert_eq!(
            unsafe { libc::read(ready_read, (&raw mut byte).cast(), 1) },
            1,
            "the lock holder never reported in"
        );
        assert_eq!(byte, 1, "the child could not lock the live directory");

        sweep_abandoned(base.path(), uid);
        assert!(live.exists(), "a directory whose lock is held must survive");
        assert!(
            !abandoned.exists(),
            "an unlocked directory must be reclaimed"
        );

        assert_eq!(unsafe { libc::kill(holder, libc::SIGKILL) }, 0);
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(holder, &mut status, 0) }, holder);
        assert_eq!(unsafe { libc::close(ready_read) }, 0);

        sweep_abandoned(base.path(), uid);
        assert!(
            !live.exists(),
            "once the lock is released it must be reclaimed too"
        );
    }

    #[test]
    fn aliases_over_one_inode_stage_exactly_one_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("libshared.so");
        fs::write(&path, b"shared").unwrap();
        let file = File::open(&path).unwrap();
        let identity = FileIdentity::from_file(&file).unwrap();
        let mut first = PinnedObject {
            file: duplicate_above_stdio(&File::open(&path).unwrap()).unwrap(),
            source_path: path.clone(),
            identity,
            private_name: "libshared.so.1".into(),
        };
        let mut second = PinnedObject {
            file,
            source_path: path.clone(),
            identity,
            private_name: "libshared.so".into(),
        };
        fs::remove_file(&path).unwrap();

        let mut staging = RuntimeStaging::new();
        staging.reanchor(&mut first);
        staging.reanchor(&mut second);
        assert_eq!(first.identity, second.identity);
        assert_eq!(first.source_path, second.source_path);
        drop(staging.finish());
    }
}

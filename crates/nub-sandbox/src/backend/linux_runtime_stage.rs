//! Re-anchor pinned runtime objects at paths Bubblewrap can still resolve.
//!
//! `--ro-bind-fd FD DEST` is not a descriptor bind. Bubblewrap rewrites it to an ordinary
//! bind whose source is the literal string `/proc/self/fd/FD`, then `realpath()`s that
//! string from `resolve_symlinks_in_ops()` — which runs AFTER it has entered the new user
//! namespace and written a map covering exactly one uid and one gid. Two ordinary host
//! shapes therefore abort a launch that is otherwise correct:
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
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Bases tried in order. `TMPDIR` is deliberately NOT honoured: under `sudo` it still
/// carries the invoking user's value, and staging the monitor executable somewhere that
/// user controls would hand them the privileged binary the sandbox is about to exec.
const STAGE_BASES: &[&str] = &["/tmp", "/var/tmp", "/run"];

/// The pid embedded after this prefix is what lets a later run reclaim a directory whose
/// owner died without unwinding — nub-cli deliberately leaks its `RuntimeCapability`, so
/// [`StagedRuntime`]'s destructor never runs in production.
const STAGE_PREFIX: &str = "nub-sandbox-runtime.";

const COPY_CHUNK: usize = 1 << 20;

/// Owns the staging directory for as long as the runtime image that binds out of it.
pub(super) struct StagedRuntime {
    dir: PathBuf,
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
    dir: Option<PathBuf>,
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
        let key = (object.identity.dev, object.identity.ino);
        if let Some(existing) = self.staged.get(&key) {
            if let Ok(file) = duplicate_above_stdio(&existing.file) {
                object.file = file;
                object.source_path = existing.path.clone();
                object.identity = existing.identity;
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
                object.file = file;
                object.source_path = staged.path.clone();
                object.identity = staged.identity;
                self.staged.insert(key, staged);
            }
            Err(error) => tracing::debug!(%error, "duplicating a staged runtime descriptor"),
        }
    }

    /// The guard the runtime image must hold, or `None` when nothing was staged.
    pub(super) fn finish(self) -> Option<StagedRuntime> {
        self.dir.map(|dir| StagedRuntime { dir })
    }

    fn directory(&mut self) -> Option<PathBuf> {
        if let Some(dir) = &self.dir {
            return Some(dir.clone());
        }
        if self.unavailable {
            return None;
        }
        match create_stage_dir() {
            Some(dir) => {
                self.dir = Some(dir.clone());
                Some(dir)
            }
            None => {
                self.unavailable = true;
                None
            }
        }
    }
}

/// Whether Bubblewrap's `realpath("/proc/self/fd/N")` still succeeds once it has entered
/// the sandbox user namespace, where a capability overrides DAC only for an inode BOTH of
/// whose owners are mapped and everything else falls back to plain DAC under this
/// process's own uid/gid with supplementary groups dropped.
///
/// Wrong in the safe direction only: an ACL or LSM that grants access this cannot see
/// costs a needless copy, never a missed repair.
fn bwrap_resolves(object: &PinnedObject) -> bool {
    let link = PathBuf::from(format!("/proc/self/fd/{}", object.file.as_raw_fd()));
    let Ok(path) = fs::canonicalize(&link) else {
        return false;
    };
    // Bubblewrap binds whatever that path names NOW, not the descriptor. A path that has
    // come to name a different inode is therefore no longer a usable stand-in for the
    // pinned one, even though it resolves.
    if !fs::metadata(&path).is_ok_and(|meta| {
        meta.dev() == object.identity.dev
            && meta.ino() == object.identity.ino
            && meta.len() == object.identity.size
    }) {
        return false;
    }
    resolvable_directory_chain(&path)
}

fn resolvable_directory_chain(path: &Path) -> bool {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    path.ancestors().skip(1).all(|dir| {
        fs::metadata(dir)
            .is_ok_and(|meta| searchable(meta.uid(), meta.gid(), meta.mode(), uid, gid))
    })
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

fn create_stage_dir() -> Option<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let pid = std::process::id();
    let mut suffix = [0u8; 8];
    getrandom::getrandom(&mut suffix).ok()?;
    let suffix = u64::from_le_bytes(suffix);
    for base in STAGE_BASES {
        let base = Path::new(base);
        if !base.is_dir() {
            continue;
        }
        sweep_abandoned(base, uid);
        let dir = base.join(format!("{STAGE_PREFIX}{pid}.{suffix:016x}"));
        // 0700 at CREATION, not afterwards: `mkdir` applies the umask, so a
        // create-then-chmod would leave the directory group/other-readable for a window in
        // a world-writable base. `mkdir` also never follows a final symlink, so a
        // pre-created name there is an EEXIST rather than a redirect.
        if fs::DirBuilder::new().mode(0o700).create(&dir).is_err() {
            continue;
        }
        if resolvable_directory_chain(&dir.join("x")) && exec_permitted(&dir) {
            return Some(dir);
        }
        let _ = fs::remove_dir_all(&dir);
    }
    None
}

/// A `noexec` staging filesystem would produce a monitor that binds and then cannot exec,
/// so the base is rejected before a multi-megabyte copy lands on it. `faccessat(X_OK)`
/// reports the mount flag, not just the mode bits — including for root.
fn exec_permitted(dir: &Path) -> bool {
    let probe = dir.join("exec-probe");
    let created = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o500)
        .open(&probe);
    if created.is_err() {
        return false;
    }
    drop(created);
    let permitted = fs::metadata(&probe).is_ok_and(|meta| meta.is_file()) && access_x_ok(&probe);
    let _ = fs::remove_file(&probe);
    permitted
}

fn access_x_ok(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

fn stage_object(dir: &Path, object: &PinnedObject) -> io::Result<StagedObject> {
    let path = dir.join(&object.private_name);
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o500)
        .open(&path)?;
    // `pread` throughout: the source descriptor is a `dup` of a caller-owned file and
    // shares its offset, so a read that advanced it would corrupt an unrelated reader.
    let copied = copy_from_offset_zero(&object.file, &mut destination, object.identity.size);
    if let Err(error) = copied {
        let _ = fs::remove_file(&path);
        return Err(error);
    }
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

/// Reclaim staging directories left by processes that are gone. Only a real directory this
/// uid owns is a candidate, so a name planted by another user in a world-writable base is
/// never followed or removed.
fn sweep_abandoned(base: &Path, uid: u32) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(STAGE_PREFIX)) else {
            continue;
        };
        let Some(pid) = rest.split('.').next().and_then(|p| p.parse::<i32>().ok()) else {
            continue;
        };
        if pid <= 0 || process_alive(pid) {
            continue;
        }
        let path = entry.path();
        if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir() && meta.uid() == uid) {
            continue;
        }
        let _ = fs::remove_dir_all(&path);
    }
}

fn process_alive(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn an_unlinked_descriptor_does_not_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("victim");
        fs::write(&path, b"payload").unwrap();
        let file = File::open(&path).unwrap();
        let identity = FileIdentity::from_file(&file).unwrap();
        let object = PinnedObject {
            file,
            source_path: path.clone(),
            identity,
            private_name: "victim".into(),
        };
        assert!(bwrap_resolves(&object));
        fs::remove_file(&path).unwrap();
        assert!(!bwrap_resolves(&object));
    }

    #[test]
    fn a_path_that_now_names_a_different_inode_does_not_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upgraded");
        fs::write(&path, b"old").unwrap();
        let file = File::open(&path).unwrap();
        let identity = FileIdentity::from_file(&file).unwrap();
        let object = PinnedObject {
            file,
            source_path: path.clone(),
            identity,
            private_name: "upgraded".into(),
        };
        assert!(bwrap_resolves(&object));
        let replacement = dir.path().join("replacement");
        fs::write(&replacement, b"new").unwrap();
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

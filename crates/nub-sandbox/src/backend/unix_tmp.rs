//! Private temporary storage with crash-recoverable, per-user ownership.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const PREFIX: &str = "session-";
const MAX_FAILED_ENTRIES: usize = 64;

pub(crate) struct PrivateTemp {
    root: PathBuf,
    entry: PathBuf,
    data: PathBuf,
    _lease: File,
    closed: bool,
}

impl PrivateTemp {
    pub(super) fn new() -> io::Result<Self> {
        Self::create_in(&root())
    }

    fn create_in(root: &Path) -> io::Result<Self> {
        let _operation = operation(root)?;
        let failures = collect(root);
        if failures.len() >= MAX_FAILED_ENTRIES {
            return Err(io::Error::other(
                "private sandbox temp recovery capacity exhausted",
            ));
        }
        for error in failures {
            tracing::warn!(%error, "private sandbox temp requires cleanup");
        }
        let directory = tempfile::Builder::new()
            .prefix(PREFIX)
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(root)?;
        let entry = directory.path();
        let mut lease = open_owned(&entry.join("lease"), true)?;
        lock(&lease, false)?;
        let metadata = std::fs::symlink_metadata(entry)?;
        serde_json::to_writer(&mut lease, &(metadata.dev(), metadata.ino()))?;
        lease.write_all(b"\n")?;
        lease.sync_all()?;
        let data = entry.join("data");
        std::fs::DirBuilder::new().mode(0o700).create(&data)?;
        // Keep the journal and payload together; Drop verifies their identity before
        // removing anything, unlike TempDir's unconditional path-based destructor.
        let entry = directory.keep();
        Ok(Self {
            root: root.into(),
            entry,
            data,
            _lease: lease,
            closed: false,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.data
    }

    pub(super) fn close(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        let _operation = operation(&self.root)?;
        remove_owned(&self.entry, &self._lease)?;
        self.closed = true;
        Ok(())
    }
}

impl Drop for PrivateTemp {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            tracing::warn!(%error, "private sandbox temp close requires cleanup");
        }
    }
}

pub(super) fn cleanup() -> io::Result<()> {
    let root = root();
    let _operation = operation(&root)?;
    match collect(&root).into_iter().next() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn root() -> PathBuf {
    // A private namespace beneath the OS temp root, not a scan of arbitrary temp
    // files or PID-named directories. Existing legacy nub-tmp-* paths are unowned.
    std::env::temp_dir().join(format!("nub-sandbox-tmp-{}", unsafe { libc::geteuid() }))
}

fn operation(root: &Path) -> io::Result<File> {
    match std::fs::DirBuilder::new().mode(0o700).create(root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(root)?;
    if !metadata.is_dir() || !private_owner(&metadata) {
        return Err(io::Error::other(
            "sandbox temp registry is not a private owned directory",
        ));
    }
    let file = open_owned(&root.join("lock"), true)?;
    lock(&file, false)?;
    Ok(file)
}

fn private_owner(metadata: &std::fs::Metadata) -> bool {
    metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0
}

fn open_owned(path: &Path, create: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || !private_owner(&metadata) || metadata.nlink() != 1 {
        return Err(io::Error::other(
            "sandbox temp lease is not a private owned file",
        ));
    }
    Ok(file)
}

fn lock(file: &File, nonblocking: bool) -> io::Result<()> {
    let flags = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), flags) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn collect(root: &Path) -> Vec<io::Error> {
    let mut failures = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => return vec![error],
    };
    for entry in entries {
        let result = (|| {
            let entry = entry?;
            if !entry.file_name().to_string_lossy().starts_with(PREFIX) {
                return Ok(());
            }
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if !metadata.is_dir() || !private_owner(&metadata) {
                return Err(io::Error::other("sandbox temp entry was replaced"));
            }
            let lease = open_owned(&entry.path().join("lease"), false)?;
            match lock(&lease, true) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
                Err(error) => Err(error),
                Ok(()) => remove_owned(&entry.path(), &lease),
            }
        })();
        if let Err(error) = result {
            failures.push(error);
        }
    }
    failures
}

fn remove_owned(entry: &Path, lease: &File) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    let mut record = [0; 128];
    let count = lease.read_at(&mut record, 0)?;
    let expected: (u64, u64) = serde_json::from_slice(&record[..count])?;
    let metadata = std::fs::symlink_metadata(entry)?;
    if !metadata.is_dir()
        || !private_owner(&metadata)
        || (metadata.dev(), metadata.ino()) != expected
    {
        return Err(io::Error::other("sandbox temp ownership identity changed"));
    }
    std::fs::remove_dir_all(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn registry() -> tempfile::TempDir {
        tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap()
    }

    #[test]
    fn live_lease_survives_cleanup_and_close_removes_it() {
        let root = registry();
        let session = PrivateTemp::create_in(root.path()).unwrap();
        std::fs::write(session.path().join("output"), b"private").unwrap();
        assert!(collect(root.path()).is_empty());
        assert!(session.path().join("output").exists());
        let entry = session.entry.clone();
        drop(session);
        assert!(!entry.exists());
    }

    #[test]
    fn abandoned_owner_is_recovered() {
        if let Some(root) = std::env::var_os("__NUB_TMP_CRASH_ROOT") {
            let session = PrivateTemp::create_in(Path::new(&root)).unwrap();
            std::fs::write(session.path().join("output"), b"private").unwrap();
            std::process::exit(91);
        }
        let root = registry();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "backend::unix_tmp::tests::abandoned_owner_is_recovered",
            ])
            .env("__NUB_TMP_CRASH_ROOT", root.path())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(91));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
        assert!(collect(root.path()).is_empty());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn explicit_close_is_idempotent_and_drop_does_not_touch_a_replacement() {
        let root = registry();
        let mut session = PrivateTemp::create_in(root.path()).unwrap();
        let entry = session.entry.clone();
        session.close().unwrap();
        assert!(!entry.exists());
        std::fs::create_dir(&entry).unwrap();
        std::fs::write(entry.join("keep"), b"replacement").unwrap();
        session.close().unwrap();
        drop(session);
        assert_eq!(std::fs::read(entry.join("keep")).unwrap(), b"replacement");
    }

    #[test]
    fn explicit_close_retains_identity_and_lease_until_retry_succeeds() {
        let root = registry();
        let mut session = PrivateTemp::create_in(root.path()).unwrap();
        let entry = session.entry.clone();
        let moved = root.path().join("moved");
        let foreign = root.path().join("foreign");
        std::fs::write(session.path().join("output"), b"owned").unwrap();
        std::fs::rename(&entry, &moved).unwrap();
        std::fs::create_dir(&entry).unwrap();
        std::fs::write(entry.join("keep"), b"replacement").unwrap();

        assert!(session.close().is_err());
        assert!(!session.closed);
        let other = open_owned(&moved.join("lease"), false).unwrap();
        assert_eq!(
            lock(&other, true).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(std::fs::read(moved.join("data/output")).unwrap(), b"owned");

        std::fs::rename(&entry, &foreign).unwrap();
        std::fs::rename(&moved, &entry).unwrap();
        session.close().unwrap();
        assert!(!entry.exists());
        assert_eq!(std::fs::read(foreign.join("keep")).unwrap(), b"replacement");
    }

    #[test]
    fn incomplete_ownership_records_are_retained_and_bound_new_admission() {
        let root = registry();
        for index in 0..MAX_FAILED_ENTRIES {
            let entry = root.path().join(format!("{PREFIX}{index}"));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&entry)
                .unwrap();
            std::fs::write(entry.join("keep"), b"uncertain ownership").unwrap();
        }
        assert!(PrivateTemp::create_in(root.path()).is_err());
        assert_eq!(collect(root.path()).len(), MAX_FAILED_ENTRIES);
        for index in 0..MAX_FAILED_ENTRIES {
            assert!(root.path().join(format!("{PREFIX}{index}/keep")).exists());
        }
    }

    #[test]
    fn cleanup_does_not_follow_payload_symlinks_or_replace_foreign_entries() {
        let root = registry();
        let foreign = tempfile::tempdir().unwrap();
        std::fs::write(foreign.path().join("keep"), b"caller").unwrap();
        let session = PrivateTemp::create_in(root.path()).unwrap();
        symlink(foreign.path(), session.path().join("link")).unwrap();
        drop(session);
        assert_eq!(
            std::fs::read(foreign.path().join("keep")).unwrap(),
            b"caller"
        );
        let session = PrivateTemp::create_in(root.path()).unwrap();
        let original = root.path().join("moved");
        std::fs::rename(&session.entry, &original).unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&session.entry)
            .unwrap();
        std::fs::copy(original.join("lease"), session.entry.join("lease")).unwrap();
        std::fs::write(session.entry.join("keep"), b"replacement").unwrap();
        let entry = session.entry.clone();
        drop(session);
        assert_eq!(std::fs::read(entry.join("keep")).unwrap(), b"replacement");
        assert!(!collect(root.path()).is_empty());
    }
}

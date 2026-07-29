use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

pub fn sibling_tempdir(final_path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut name: OsString = final_path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("aube-tmp"));
    name.push(format!(".tmp.{pid}.{nanos}.{n}"));
    match final_path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

pub fn rename_with_retry(src: &Path, dst: &Path) -> io::Result<()> {
    const MAX_ATTEMPTS: u32 = 5;
    let mut backoff_ms = 20u64;
    for attempt in 0..MAX_ATTEMPTS {
        match std::fs::rename(src, dst) {
            Ok(()) => return Ok(()),
            Err(err) => {
                if !is_transient(&err) || attempt == MAX_ATTEMPTS - 1 {
                    if dst.exists() {
                        let _ = std::fs::remove_dir_all(src).or_else(|_| std::fs::remove_file(src));
                        return Ok(());
                    }
                    return Err(err);
                }
                thread::sleep(Duration::from_millis(backoff_ms));
                backoff_ms = backoff_ms.saturating_mul(2);
            }
        }
    }
    Err(io::Error::other("rename_with_retry exhausted attempts"))
}

fn is_transient(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::AlreadyExists
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
    )
}

pub fn atomic_write(final_path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = sibling_tempdir(final_path);
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        // No fsync: every aube caller writes content that is either
        // regenerable from network/manifest (packument cache, install
        // state, version-check cache) or paired with explicit user
        // intent that can be retried (patches, `aube config set`).
        // The atomic rename below still gives us "all or nothing"
        // visibility — without fsync we just trade a narrow crash-
        // window where the renamed file can read back as zero bytes
        // for a measurable cold-install win (~1.6% of CPU was in
        // sync_all on the packument cache writers).
    }
    match rename_with_retry(&tmp, final_path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Outcome of a `write_excl` attempt. `Created` means our bytes
/// committed at the final path. `AlreadyExists` means another writer
/// (or a prior process) committed first; for content-addressed
/// stores this is a success path because the existing bytes are
/// bit-identical by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Created,
    AlreadyExists,
}

/// Direct `O_CREAT|O_EXCL|O_WRONLY` write to a final path. Skips the
/// tempfile + rename dance for content-addressed paths where racing
/// writers produce bit-identical content.
///
/// Creates parent directories on demand. On `EEXIST`, returns
/// `AlreadyExists` rather than erroring — the caller decides whether
/// that's a success (CAS) or a real error (other layouts).
///
/// Sets POSIX mode via `set_permissions` while the file is still
/// open. Windows inherits ACLs; the `mode` argument is ignored
/// there.
pub fn write_excl(path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<WriteOutcome> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    let mut file = match opts.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Ok(WriteOutcome::AlreadyExists);
        }
        Err(e) => return Err(e),
    };
    // On any failure after the file exists, best-effort unlink so we
    // don't leave a partial / mode-incorrect entry at the final
    // path. The previous `tempfile + persist_noclobber` shape got
    // this for free (drop unlinks the temp); the direct-write shape
    // has to do it explicitly. Unlink errors are ignored — the
    // primary error wins.
    let cleanup_on_err = |path: &Path, e: io::Error| -> io::Error {
        let _ = std::fs::remove_file(path);
        e
    };
    {
        use std::io::Write as _;
        if let Err(e) = file.write_all(bytes) {
            return Err(cleanup_on_err(path, e));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Some(m) = mode
            && let Err(e) = file.set_permissions(std::fs::Permissions::from_mode(m))
        {
            return Err(cleanup_on_err(path, e));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
    }
    Ok(WriteOutcome::Created)
}

pub mod sentinel {
    use super::*;

    pub fn mark(path: &Path, tag: &str) -> io::Result<bool> {
        match std::fs::read(path) {
            Ok(existing) if existing.as_slice() == tag.as_bytes() => Ok(false),
            Ok(_) => {
                write_sentinel(path, tag)?;
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                write_sentinel(path, tag)?;
                Ok(true)
            }
            Err(e) => Err(e),
        }
    }

    pub fn read_tag(path: &Path) -> Option<Vec<u8>> {
        std::fs::read(path).ok()
    }

    pub fn present(path: &Path) -> bool {
        std::fs::metadata(path).is_ok()
    }

    fn write_sentinel(path: &Path, tag: &str) -> io::Result<()> {
        super::atomic_write(path, tag.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_tempdir_differs_per_call() {
        let base = Path::new("/tmp/aube-fake-final");
        let a = sibling_tempdir(base);
        let b = sibling_tempdir(base);
        assert_ne!(a, b);
        assert_eq!(a.parent(), b.parent());
    }

    #[test]
    fn atomic_write_replaces_existing() -> io::Result<()> {
        let dir = tempdir()?;
        let path = dir.join("target.bin");
        atomic_write(&path, b"first")?;
        assert_eq!(std::fs::read(&path)?, b"first");
        atomic_write(&path, b"second")?;
        assert_eq!(std::fs::read(&path)?, b"second");
        Ok(())
    }

    #[test]
    fn atomic_write_creates_parent() -> io::Result<()> {
        let dir = tempdir()?;
        let path = dir.join("nested/deep/file.txt");
        atomic_write(&path, b"ok")?;
        assert_eq!(std::fs::read(&path)?, b"ok");
        Ok(())
    }

    #[test]
    fn sentinel_roundtrip() -> io::Result<()> {
        let dir = tempdir()?;
        let path = dir.join(".aube-initialized");
        assert!(!sentinel::present(&path));
        assert!(sentinel::mark(&path, "v1")?);
        assert!(sentinel::present(&path));
        assert!(!sentinel::mark(&path, "v1")?);
        assert_eq!(sentinel::read_tag(&path).as_deref(), Some(b"v1".as_ref()));
        assert!(sentinel::mark(&path, "v2")?);
        assert_eq!(sentinel::read_tag(&path).as_deref(), Some(b"v2".as_ref()));
        Ok(())
    }

    fn tempdir() -> io::Result<PathBuf> {
        let base = std::env::temp_dir().join(format!(
            "aube-util-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&base)?;
        Ok(base)
    }
}

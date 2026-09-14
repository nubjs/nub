//! Whole-file replacement for the small files nub writes itself: a sibling temp
//! file renamed over the destination, so a reader sees the old bytes or the new
//! ones and never a partial write.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Replace `path` with `bytes`, creating its directory.
///
/// `permissions` go on the temp file before the rename, so the replacement never
/// appears with a wider mode than the file it replaced, even for the moment
/// between the two; `None` keeps the platform default for a new file. A rename
/// that fails is an error, including when something already occupies the
/// destination. There is no fsync: every caller writes content the user can
/// write again.
pub(crate) fn write(
    path: &Path,
    bytes: &[u8],
    permissions: Option<std::fs::Permissions>,
) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let temp = sibling_temp(path);
    let written = std::fs::File::create(&temp).and_then(|mut file| {
        file.write_all(bytes)?;
        if let Some(permissions) = permissions {
            // Best effort: a filesystem that will not carry the mode still gets
            // the content, which is what the caller asked for first.
            let _ = file.set_permissions(permissions);
        }
        Ok(())
    });
    let renamed = written.and_then(|()| rename(&temp, path));
    if renamed.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    renamed
}

fn sibling_temp(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".nub-write.{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    path.with_file_name(name)
}

/// A rename that retries briefly on the errors Windows reports while another
/// process (an editor, an indexer, antivirus) holds the destination open.
fn rename(from: &Path, to: &Path) -> io::Result<()> {
    let mut attempt = 0;
    loop {
        match std::fs::rename(from, to) {
            Err(error)
                if attempt < 4
                    && matches!(
                        error.kind(),
                        io::ErrorKind::PermissionDenied | io::ErrorKind::Interrupted
                    ) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(20 << attempt));
                attempt += 1;
            }
            result => return result,
        }
    }
}

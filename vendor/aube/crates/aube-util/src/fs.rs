//! Cross-platform filesystem helpers shared by store, linker, install.

use std::path::Path;

/// True when `a` and `b` live on different volumes (Windows) or
/// mounts (Linux/Mac). Used by the linker probe and warning paths to
/// detect when hardlink/reflink can't cross the device boundary so
/// installs fall back to per-file copy. Best-effort: errors and
/// platforms without device-id semantics return false (no warning,
/// no false-positive on platforms where the check isn't meaningful).
pub fn cross_volume(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        let drive = |p: &Path| -> Option<char> {
            let s = p.to_string_lossy();
            let bytes = s.as_bytes();
            if bytes.len() >= 2 && bytes[1] == b':' {
                Some(bytes[0].to_ascii_uppercase() as char)
            } else {
                None
            }
        };
        match (drive(a), drive(b)) {
            (Some(da), Some(db)) => da != db,
            _ => false,
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = |p: &Path| std::fs::metadata(p).ok().map(|m| m.dev());
        match (dev(a), dev(b)) {
            (Some(da), Some(db)) => da != db,
            _ => false,
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (a, b);
        false
    }
}

/// Hint Windows Search and Defender to skip indexing/scanning this
/// directory. No-op on non-Windows. Best-effort: failure (non-NTFS,
/// permission denied) is harmless and silent.
pub fn set_not_content_indexed(path: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_NOT_CONTENT_INDEXED: u32 = 0x2000;
        let Ok(meta) = std::fs::metadata(path) else {
            return;
        };
        let current = meta.file_attributes();
        if current & FILE_ATTRIBUTE_NOT_CONTENT_INDEXED != 0 {
            return;
        }
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        // SAFETY: wide is null-terminated UTF-16. SetFileAttributesW reads sync.
        let _ = unsafe {
            set_file_attributes_w(wide.as_ptr(), current | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED)
        };
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
}

#[cfg(windows)]
unsafe extern "system" {
    #[link_name = "SetFileAttributesW"]
    fn set_file_attributes_w(lpFileName: *const u16, dwFileAttributes: u32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn cross_volume_same_path_is_false() {
        let p = PathBuf::from(".");
        assert!(!cross_volume(&p, &p));
    }

    #[cfg(windows)]
    #[test]
    fn cross_volume_different_drives_is_true() {
        let a = PathBuf::from("C:/foo");
        let b = PathBuf::from("D:/bar");
        assert!(cross_volume(&a, &b));
    }

    #[cfg(windows)]
    #[test]
    fn cross_volume_same_drive_is_false() {
        let a = PathBuf::from("C:/foo");
        let b = PathBuf::from("C:/bar");
        assert!(!cross_volume(&a, &b));
    }
}

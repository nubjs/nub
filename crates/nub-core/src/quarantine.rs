//! Clear `com.apple.quarantine` from artifacts nub itself writes and then
//! loads or executes (macOS only; a no-op stub elsewhere).
//!
//! macOS stamps the attribute on every inode created by a process that
//! inherited quarantine flags — a terminal embedded in a Gatekeeper-enabled
//! app, such as a GUI Git client with `LSFileQuarantineEnabled`. The flags
//! are inherited across `fork`/`exec`, so a `curl` or `tar` nub spawns
//! stamps everything it writes too. Gatekeeper then refuses the result, and
//! every native artifact nub places is ad-hoc signed (`codesign` reports
//! `adhoc, linker-signed`, no Team ID), which it kills outright rather than
//! merely warning about.
//!
//! Clearing it is sound because each caller has already verified the bytes:
//! the self-upgrade checks the archive's SHA-256 against the published
//! checksum, and the embedded runtime checks each entrypoint against a
//! BLAKE3 digest baked in at build time. The attribute is also not a
//! judgement about the artifact — it lands only when this process
//! inherited the flags, so an upgrade run from a GUI Git client's
//! terminal produces a binary that cannot start while the identical
//! upgrade from Terminal.app produces one that can.
//!
//! (Homebrew is often cited as precedent for stripping after
//! verification. It is not: brew *adds* quarantine to casks deliberately
//! and warns that `--no-quarantine` reduces system security. Its
//! formula/bottle path — the comparable case, a verified artifact the
//! package manager fetched itself — never involves the attribute at all.)
//!
//! Note that content verification cannot substitute for this: an xattr does
//! not change a file's bytes, so a digest check passes on a quarantined file
//! and any self-heal keyed on content never fires.

/// Best-effort clear of `com.apple.quarantine` on one path.
///
/// Returns whether the attribute was actually removed, which only the
/// callers that want to report a failure need; the common outcome is
/// `false` because nothing was there. Never fails: the worst case is an
/// artifact Gatekeeper may refuse, which is the status quo without this.
#[cfg(target_os = "macos")]
pub fn clear(path: &std::path::Path) -> Result<bool, std::io::Error> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return Ok(false);
    };
    // SAFETY: both pointers are NUL-terminated C strings that outlive the
    // call. `XATTR_NOFOLLOW` keeps a symlinked path from redirecting the
    // removal onto whatever it points at.
    let rc = unsafe {
        libc::removexattr(
            c_path.as_ptr(),
            c"com.apple.quarantine".as_ptr(),
            libc::XATTR_NOFOLLOW,
        )
    };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        // ENOATTR — nothing to remove — is the overwhelmingly common case,
        // and ENOENT means the caller raced a path away. Neither is a
        // failure worth surfacing.
        Some(libc::ENOATTR | libc::ENOENT) => Ok(false),
        _ => Err(err),
    }
}

/// Quarantine is a macOS concept; every other platform loads without it.
#[cfg(not(target_os = "macos"))]
pub fn clear(_path: &std::path::Path) -> Result<bool, std::io::Error> {
    Ok(false)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    fn seed(path: &Path) {
        let c = CString::new(path.as_os_str().as_bytes()).unwrap();
        let v = b"0083;68000000;TestApp;";
        // SAFETY: NUL-terminated path/name outliving the call.
        let rc = unsafe {
            libc::setxattr(
                c.as_ptr(),
                c"com.apple.quarantine".as_ptr(),
                v.as_ptr().cast(),
                v.len(),
                0,
                libc::XATTR_NOFOLLOW,
            )
        };
        assert_eq!(rc, 0, "seeding com.apple.quarantine failed");
    }

    fn quarantined(path: &Path) -> bool {
        let c = CString::new(path.as_os_str().as_bytes()).unwrap();
        let mut buf = [0u8; 512];
        // SAFETY: `buf` is written up to the length passed; a negative
        // return means nothing was written.
        let n = unsafe {
            libc::getxattr(
                c.as_ptr(),
                c"com.apple.quarantine".as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                0,
                libc::XATTR_NOFOLLOW,
            )
        };
        n >= 0
    }

    /// Matches the crate's existing temp-dir convention (`runtime_cache`
    /// does the same) rather than pulling in a dev-dependency.
    fn scratch(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let suffix = nanos
            ^ COUNTER
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_mul(0x9E37_79B9);
        let dir = std::env::temp_dir().join(format!("nub-qtn-{tag}-{suffix}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn clears_the_attribute_and_reports_whether_there_was_one() {
        let dir = scratch("clear");
        let f = dir.join("nub-native.node");
        std::fs::write(&f, b"x").unwrap();
        seed(&f);
        assert!(quarantined(&f));

        assert!(super::clear(&f).unwrap(), "should report a removal");
        assert!(!quarantined(&f));
        // Idempotent: a second pass finds nothing and is not an error.
        assert!(!super::clear(&f).unwrap(), "second pass removes nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing path is a race, not a failure — callers run this on
    /// best-effort paths and must not see an error.
    #[test]
    fn absent_path_is_not_an_error() {
        let dir = scratch("absent");
        assert!(!super::clear(&dir.join("gone")).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

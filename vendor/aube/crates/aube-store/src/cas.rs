use crate::{Error, Store, StoredFile};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

thread_local! {
    static B3_HASHER: RefCell<blake3::Hasher> = RefCell::new(blake3::Hasher::new());
}

/// Per-shard mutex array used by the macOS CAS fast path to serialize
/// concurrent writers within a single process. Indexed by the first
/// byte of the file's BLAKE3 hash (matching the on-disk 2-char shard
/// layout), so two threads writing the same hash always collide; threads
/// writing different hashes typically don't. The array is process-global
/// rather than per-`Store` because there is at most one active store
/// per install, and a static avoids carrying 256 mutexes in every cheap
/// `Store::clone()` along the fetch pipeline.
///
/// macOS-gated rather than `not(linux)` because the fast-path block
/// itself uses `OpenOptionsExt::mode`, which only exists on Unix —
/// Windows would fail to compile under `not(linux)`. Linux already has
/// `O_TMPFILE + linkat` (atomic-by-construction, faster than either
/// alternative); Windows keeps the tempfile + persist_noclobber path.
#[cfg(target_os = "macos")]
static FAST_PATH_SHARD_LOCKS: [std::sync::Mutex<()>; 256] =
    [const { std::sync::Mutex::new(()) }; 256];

/// Recursively copy `src` into `dst`. Used only by the one-shot
/// legacy-index migration fallback when `rename` fails (typically
/// cross-filesystem). Not a hot path; correctness > speed.
pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to)?;
        }
        // Symlinks and other types are skipped — the index cache only
        // ever contains regular JSON files (optionally under a single
        // level of integrity-shard subdirs).
    }
    Ok(())
}

pub(crate) fn blake3_hex(content: &[u8]) -> String {
    B3_HASHER.with(|cell| {
        let mut h = cell.borrow_mut();
        h.reset();
        h.update(content);
        h.finalize().to_hex().to_string()
    })
}

pub(crate) fn cas_file_matches_len(path: &Path, expected_len: u64) -> bool {
    path.metadata()
        .map(|metadata| metadata.len() == expected_len)
        .unwrap_or(false)
}

/// Outcome of `create_cas_file`. `Created` means we wrote the bytes
/// at the final path; `AlreadyExisted` means another writer (or a
/// previous import) had already committed bit-identical content. The
/// distinction lets `import_bytes` skip the post-write length check
/// on the freshly-created path — the file IS exactly the bytes we
/// just wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CasWriteOutcome {
    Created,
    AlreadyExisted,
}

impl Store {
    /// Ensure every two-char shard directory under the CAS root exists.
    /// CAS files live under `<root>/<ab>/<cdef...>` for 256 possible
    /// prefixes. Running this once before a batch of `import_bytes`
    /// calls lets the per-file hot path skip the `mkdirp(parent)` stat
    /// entirely (the parent is guaranteed to exist). On APFS that
    /// removes ~7.5k redundant `stat` syscalls per cold install — the
    /// `mkdirp` inside `xx::file::write` was the #1 stat hotspot in a
    /// dtrace profile.
    ///
    /// Cheap to call repeatedly: each `create_dir_all` is a no-op when
    /// the directory already exists, but callers should still hoist the
    /// call out of tight loops.
    pub fn ensure_shards_exist(&self) -> Result<(), Error> {
        std::fs::create_dir_all(&self.root).map_err(|e| Error::Io(self.root.clone(), e))?;
        // Windows Defender and Search both touch every file in the
        // store on default installs. Setting this attribute makes
        // them skip. Non-NTFS volumes ignore it harmlessly.
        aube_util::fs::set_not_content_indexed(&self.root);
        let mut buf = [0u8; 2];
        for hi in 0u8..16 {
            for lo in 0u8..16 {
                buf[0] = hex_digit(hi);
                buf[1] = hex_digit(lo);
                // SAFETY: every byte in `buf` comes from `hex_digit`,
                // which only emits `0-9` / `a-f` — always valid UTF-8.
                let shard = std::str::from_utf8(&buf).unwrap();
                let path = self.root.join(shard);
                std::fs::create_dir_all(&path).map_err(|e| Error::Io(path, e))?;
            }
        }
        Ok(())
    }

    /// Atomically create `path` without overwriting an existing CAS entry.
    /// `AlreadyExists` is a no-op here; callers that know the expected content
    /// length must verify it before trusting a reused path. Non-empty files are
    /// written through a sibling temp file and persisted with no-clobber
    /// semantics so an interrupted import cannot leave a torn file at the
    /// content-addressed path. We intentionally do not fsync every CAS file:
    /// cold installs import tens of thousands of files, and package-index
    /// loading rejects missing/truncated entries so they can be fetched again.
    /// `NotFound` means a concurrent prune or a missed `ensure_shards_exist`
    /// removed the parent shard; recreate it and retry exactly once before
    /// surfacing.
    fn create_cas_file(
        &self,
        path: &Path,
        content: Option<&[u8]>,
    ) -> Result<CasWriteOutcome, Error> {
        fn do_create_and_write(
            this: &Store,
            path: &Path,
            content: Option<&[u8]>,
        ) -> Result<CasWriteOutcome, Error> {
            if let Some(bytes) = content {
                // O_TMPFILE creates anon file in parent, linkat
                // publishes atomically. Skips mkstemp uniqueness probe
                // and post-write fchmod. Docker overlayfs hits the
                // EOPNOTSUPP fallback. AUBE_DISABLE_O_TMPFILE for
                // regression cover.
                #[cfg(target_os = "linux")]
                {
                    static O_TMPFILE_DISABLED: std::sync::OnceLock<bool> =
                        std::sync::OnceLock::new();
                    let disabled = *O_TMPFILE_DISABLED.get_or_init(|| {
                        aube_util::env::embedder_env("DISABLE_O_TMPFILE").is_some()
                    });
                    if !disabled {
                        match try_o_tmpfile_publish(path, bytes) {
                            Ok(outcome) => return Ok(outcome),
                            Err(OTmpfileFallback::Unsupported) => {}
                            Err(OTmpfileFallback::Hard(e)) => return Err(e),
                        }
                    }
                }

                // macOS fast path: direct O_CREAT|O_EXCL at the final
                // content-addressed path, no tempfile dance. Caller (the
                // install command) flips `fast_path` on only after
                // acquiring an exclusive store-level lock against other
                // aube processes. We additionally serialize writers
                // *within* this process per shard: two threads importing
                // the same hash (a CAS-dedupe across packages, 35% of
                // files on dep-heavy graphs like MUI/CodeMirror) would
                // otherwise both attempt create_new — the loser sees an
                // EEXIST against the winner's still-empty fd and the
                // caller's size-mismatch recovery in `import_bytes` would
                // unlink the file out from under the still-writing
                // winner. The shard mutex sequences the open+write so the
                // loser only observes the file at its final size.
                //
                // Crashed-predecessor recovery (the unlink+rewrite path
                // that the slow path defers to `import_bytes`) runs here
                // while the mutex is still held, so the caller's recovery
                // can safely no-op for fast-path writes.
                //
                // On APFS the fast path is ~2.25x faster than
                // tempfile+chmod+persist (~64µs/file vs ~145µs/file in
                // isolation). macOS-gated rather than `not(linux)`
                // because `OpenOptionsExt::mode` is unix-only — Windows
                // keeps the tempfile path.
                #[cfg(target_os = "macos")]
                if this.fast_path.load(Ordering::Acquire) {
                    use std::io::Write;
                    use std::os::unix::fs::OpenOptionsExt;

                    let shard_idx = path
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|s| s.to_str())
                        .and_then(|s| u8::from_str_radix(s, 16).ok())
                        .map(|b| b as usize);
                    // Every path produced by `file_path_from_hex` lives
                    // under a 2-char hex shard, so this is the contract
                    // every fast-path caller satisfies today. The assert
                    // pins the invariant; if a future caller hands in a
                    // non-CAS path, release builds skip the fast path
                    // (falling through to the safe tempfile branch)
                    // rather than do an unsynchronized write that could
                    // race with another thread on the same hash.
                    debug_assert!(
                        shard_idx.is_some(),
                        "fast-path CAS write to path without a valid hex shard parent: {}",
                        path.display()
                    );
                    if let Some(i) = shard_idx {
                        // Mutex poisoning is impossible here — the guard
                        // is dropped at end of scope without us panicking
                        // inside, so we either return cleanly or propagate
                        // an `Err` while still releasing the lock. If a
                        // future caller panics inside, `unwrap_or_else`
                        // recovers the guard anyway.
                        let _shard_guard = FAST_PATH_SHARD_LOCKS[i]
                            .lock()
                            .unwrap_or_else(|p| p.into_inner());

                        // `OpenOptionsExt::mode(0o644)` is masked by the
                        // process umask, so a non-default umask (e.g.
                        // 0o077) would give CAS files 0o600. The
                        // tempfile path uses `fchmod`, which ignores
                        // umask. Match it with an explicit
                        // `set_permissions` so the same store can't end
                        // up with mixed-mode files depending on which
                        // path wrote each entry.
                        use std::os::unix::fs::PermissionsExt;
                        let force_mode = std::fs::Permissions::from_mode(0o644);
                        let open_result = std::fs::OpenOptions::new()
                            .mode(0o644)
                            .create_new(true)
                            .write(true)
                            .open(path);
                        match open_result {
                            Ok(mut f) => {
                                f.set_permissions(force_mode.clone())
                                    .map_err(|e| Error::Io(path.to_path_buf(), e))?;
                                f.write_all(bytes)
                                    .map_err(|e| Error::Io(path.to_path_buf(), e))?;
                                return Ok(CasWriteOutcome::Created);
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                                // Holding the shard lock, so any in-process
                                // writer for this hash already finished. If
                                // the file size matches, it's a genuine
                                // dedupe. If not, it's a crashed-predecessor
                                // remnant — unlink and rewrite inline.
                                if cas_file_matches_len(path, bytes.len() as u64) {
                                    return Ok(CasWriteOutcome::AlreadyExisted);
                                }
                                let _ = xx::file::remove_file(path);
                                match std::fs::OpenOptions::new()
                                    .mode(0o644)
                                    .create_new(true)
                                    .write(true)
                                    .open(path)
                                {
                                    Ok(mut f) => {
                                        f.set_permissions(force_mode)
                                            .map_err(|e| Error::Io(path.to_path_buf(), e))?;
                                        f.write_all(bytes)
                                            .map_err(|e| Error::Io(path.to_path_buf(), e))?;
                                        return Ok(CasWriteOutcome::Created);
                                    }
                                    Err(e) => {
                                        return Err(Error::Io(path.to_path_buf(), e));
                                    }
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                // Shard dir missing — fall through to the slow
                                // path; the outer wrapper will create_dir_all
                                // and retry once.
                            }
                            Err(e) => return Err(Error::Io(path.to_path_buf(), e)),
                        }
                    }
                }

                // Tempfile + persist_noclobber gives atomic crash
                // semantics: a partial write on `tmp` is dropped by
                // tempfile's Drop impl, so the final path either
                // contains the complete bytes or doesn't exist. A
                // direct O_CREAT|O_EXCL write to the final path was
                // tried (faster path, ~3 syscalls per file) but
                // raced with concurrent installs in CI where two
                // processes saw the same partial file in different
                // orders and clobbered each other's recovery. The
                // fast-path branch above re-enables it under an
                // exclusive store lock.
                let _ = this; // suppress unused warning on Linux
                let parent = path.parent().ok_or_else(|| {
                    Error::Io(path.to_path_buf(), std::io::ErrorKind::NotFound.into())
                })?;
                let mut tmp = tempfile::Builder::new()
                    .prefix(".aube-cas-")
                    .tempfile_in(parent)
                    .map_err(|e| Error::Io(path.to_path_buf(), e))?;
                use std::io::Write;
                tmp.write_all(bytes)
                    .map_err(|e| Error::Io(path.to_path_buf(), e))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    tmp.as_file()
                        .set_permissions(std::fs::Permissions::from_mode(0o644))
                        .map_err(|e| Error::Io(path.to_path_buf(), e))?;
                }
                return match tmp.persist_noclobber(path) {
                    Ok(_) => Ok(CasWriteOutcome::Created),
                    Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                        Ok(CasWriteOutcome::AlreadyExisted)
                    }
                    Err(e) => Err(Error::Io(path.to_path_buf(), e.error)),
                };
            }

            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(_) => Ok(CasWriteOutcome::Created),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    Ok(CasWriteOutcome::AlreadyExisted)
                }
                Err(e) => Err(Error::Io(path.to_path_buf(), e)),
            }
        }

        match do_create_and_write(self, path, content) {
            Ok(outcome) => Ok(outcome),
            Err(Error::Io(_, ref ioe)) if ioe.kind() == std::io::ErrorKind::NotFound => {
                // Shard dir missing. `ensure_shards_exist` normally
                // pre-creates all 256 shards; this only fires when the
                // caller didn't call it or a concurrent prune wiped
                // the tree mid-install.
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| Error::Io(parent.to_path_buf(), e))?;
                }
                do_create_and_write(self, path, content)
            }
            Err(e) => Err(e),
        }
    }

    /// Import a single file's content into the store. Returns the stored file info.
    ///
    /// Hot path on cold installs: callers should invoke
    /// [`Store::ensure_shards_exist`] once before a batch of imports so
    /// this function can skip the per-file `mkdirp`. When shards don't
    /// exist yet, the `create_new` open will fail with `NotFound`; we
    /// fall back to the slow path for correctness.
    pub fn import_bytes(&self, content: &[u8], executable: bool) -> Result<StoredFile, Error> {
        let hash_t0 = std::time::Instant::now();
        let hex_hash = blake3_hex(content);
        if aube_util::diag::enabled() {
            aube_util::diag::event_lazy(
                aube_util::diag::Category::Store,
                "blake3_hash",
                hash_t0.elapsed(),
                || format!(r#"{{"size":{}}}"#, content.len()),
            );
        }

        let store_path = self.file_path_from_hex(&hex_hash);
        let _diag_write =
            aube_util::diag::Span::new(aube_util::diag::Category::Store, "import_bytes_write")
                .with_meta_fn(|| format!(r#"{{"size":{}}}"#, content.len()));

        // Fast path: open-with-create-new combines the existence check
        // and the open into a single syscall. On a cold CAS this does
        // one open(O_CREAT|O_EXCL|O_WRONLY) per file and replaces the
        // previous stat+create pair (~15k redundant stats per cold
        // install). On a warm CAS, concurrent writers are safe: EEXIST
        // means another writer already materialized this content (same
        // hash = same bytes), so we skip and share the entry.
        //
        // `Created` means we just wrote the bytes — they are exactly
        // `content.len()` by construction, no need to re-stat. Only
        // the `AlreadyExisted` branch can produce a torn file (from a
        // crashed predecessor) so the length check runs there only.
        let outcome = self.create_cas_file(&store_path, Some(content))?;
        // Surface CAS dedup hit/miss to diag so cold vs warm vs partial
        // installs can be classified post-hoc. `cas_hit` fires when an
        // identical-content file already lived in the store; `cas_miss`
        // fires when we just wrote new bytes.
        if aube_util::diag::enabled() {
            let name = match outcome {
                CasWriteOutcome::Created => "cas_miss",
                CasWriteOutcome::AlreadyExisted => "cas_hit",
            };
            aube_util::diag::instant_lazy(aube_util::diag::Category::Store, name, || {
                format!(r#"{{"size":{}}}"#, content.len())
            });
        }
        // The macOS fast path verifies the file size inline under its
        // shard mutex before returning `AlreadyExisted`, so this
        // recovery only needs to run when we took the tempfile path.
        // Skipping it there also prevents a race where the recovery
        // unlinks a file that another in-process thread is concurrently
        // re-creating after observing the same crashed-predecessor.
        //
        // `cfg!(target_os = "macos")` matches the cfg gate on the only
        // code path that flips `fast_path` to true (and on the inline
        // recovery inside `create_cas_file`). Without the cfg!, a future
        // caller setting the flag on Linux would silently disable this
        // recovery — the Linux O_TMPFILE branch has no inline
        // length-check substitute, so torn CAS files would be accepted.
        let fast_path_handled_recovery =
            cfg!(target_os = "macos") && self.fast_path.load(Ordering::Acquire);
        if outcome == CasWriteOutcome::AlreadyExisted && !fast_path_handled_recovery {
            // A length mismatch from this branch can mean either
            //   (a) a crashed predecessor left a torn file (the recovery
            //       case this code was originally written for), or
            //   (b) on macOS, another *process* is currently writing to
            //       the same path via the fast path (no atomic publish
            //       at the final path, so its in-progress fd is visible
            //       by name to other writers).
            // Burning the file in case (b) would unlink the active
            // writer's inode and trigger a cascading recovery race. Wait
            // briefly (50ms is dozens of typical small-file writes) for
            // the partial file to settle. If it stays mismatched past
            // the deadline, treat it as (a) and recover.
            if !cas_file_matches_len(&store_path, content.len() as u64) {
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
                while !cas_file_matches_len(&store_path, content.len() as u64)
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(std::time::Duration::from_micros(250));
                }
            }
            if !cas_file_matches_len(&store_path, content.len() as u64) {
                let _ = xx::file::remove_file(&store_path);
                self.create_cas_file(&store_path, Some(content))?;
                if !cas_file_matches_len(&store_path, content.len() as u64) {
                    let actual_len = store_path.metadata().map(|metadata| metadata.len()).ok();
                    return Err(Error::Io(
                        store_path.clone(),
                        std::io::Error::other(format!(
                            "CAS entry has wrong size after import: expected {} bytes, got {}",
                            content.len(),
                            actual_len
                                .map(|len| format!("{len} bytes"))
                                .unwrap_or_else(|| "missing file".to_owned())
                        )),
                    ));
                }
            }
        }

        if executable {
            // Behavior note: this branch now runs unconditionally when
            // `executable=true`, including when the content file
            // already existed (`AlreadyExists` above). Previously the
            // marker was only written in the fresh-content branch.
            // The new shape is strictly more correct — if the same
            // bytes are imported twice, once with `executable=false`
            // and once with `true`, the marker should exist after the
            // second call. Auditing the callers of the `-exec` marker:
            //   - `aube-store::import_bytes` (this function, the only
            //     writer).
            //   - `aube-store` tests (assert the marker exists after
            //     an `executable=true` import).
            //   - `aube::commands::store` (`aube store prune`)
            //     uses the marker to skip bumping the "freed bytes"
            //     counter when unlinking exec-marker sidecars.
            // No code path reads the marker to decide executability —
            // that's carried in `StoredFile.executable`, threaded
            // through the `PackageIndex` and the linker. So flipping
            // a marker-absent-to-present for a shared hash is safe.
            let exec_marker = PathBuf::from(format!("{}-exec", store_path.display()));
            self.create_cas_file(&exec_marker, None)?;
        }

        Ok(StoredFile {
            hex_hash,
            store_path,
            executable,
            size: Some(content.len() as u64),
        })
    }
}

// Thin wrapper over posix_fallocate(3) which returns the error code
// directly (does not set errno). Caller decides how to handle the
// error. Existing call site uses `let _ = ...` to ignore EOPNOTSUPP /
// ENOSYS / EINVAL on filesystems where pre-allocation is a no-op.
//
// `len` is `libc::off_t` because that's the type the underlying glibc
// signature uses, and it varies per target: `i64` on 64-bit Linux and
// on 32-bit Linux when the libc bindings opt into _FILE_OFFSET_BITS=64,
// `i32` on 32-bit Linux otherwise (e.g. Debian/Ubuntu's armhf packaging
// build env). Taking it directly keeps the call-site cast in one place.
#[cfg(target_os = "linux")]
fn posix_fallocate(file: &std::fs::File, len: libc::off_t) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    if len <= 0 {
        return Ok(());
    }
    // SAFETY: fd is owned by `file` for the duration of the call.
    let r = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, len) };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(r))
    }
}

// Unsupported means kernel/fs lacks O_TMPFILE, caller falls back.
// Hard is a real I/O error that bubbles up.
#[cfg(target_os = "linux")]
enum OTmpfileFallback {
    Unsupported,
    Hard(Error),
}

// Size threshold below which we skip both `posix_fallocate` and
// `posix_fadvise(DONTNEED)` on the CAS write path. Both are
// fixed-cost-per-call best-effort advisory syscalls whose benefits
// (avoid ext4 fragmentation, evict pages) don't apply to small
// writes — the kernel won't fragment a single-block write, and tiny
// pages don't meaningfully pressure the cache. samply on a cold
// 1230-pkg install pinned the two at ~4.4% + ~4.8% of self time
// before this gate; gating to ≥64KB skips them for >95% of npm
// tarball entries while preserving the original behavior on the
// large files (typescript.js, monaco-editor, etc.) where it pays.
//
// Overridable via `AUBE_CAS_SMALL_FILE_THRESHOLD` (bytes). Set to 0
// to restore the always-on behavior; set to a very large number to
// effectively disable both syscalls.
#[cfg(target_os = "linux")]
const CAS_SMALL_FILE_THRESHOLD_DEFAULT: usize = 64 * 1024;

#[cfg(target_os = "linux")]
fn cas_small_file_threshold() -> usize {
    static THRESHOLD: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *THRESHOLD.get_or_init(|| {
        match aube_util::env::embedder_env("CAS_SMALL_FILE_THRESHOLD")
            .as_deref()
            .map(|s| s.to_string_lossy().into_owned())
        {
            None => CAS_SMALL_FILE_THRESHOLD_DEFAULT,
            Some(raw) => raw.parse::<usize>().unwrap_or_else(|_| {
                warn!(
                    "CAS_SMALL_FILE_THRESHOLD={raw:?} is not a non-negative integer; \
                     falling back to default {CAS_SMALL_FILE_THRESHOLD_DEFAULT}"
                );
                CAS_SMALL_FILE_THRESHOLD_DEFAULT
            }),
        }
    })
}

// Open anonymous file in parent dir, write, linkat via /proc/self/fd.
// Skips the tempfile unique-name probe and explicit fchmod. Falls
// back via Unsupported on EOPNOTSUPP, ENOENT (no /proc), or EXDEV.
// AUBE_DISABLE_O_TMPFILE forces the legacy path.
#[cfg(target_os = "linux")]
fn try_o_tmpfile_publish(path: &Path, bytes: &[u8]) -> Result<CasWriteOutcome, OTmpfileFallback> {
    use std::ffi::CString;
    use std::io::Write;
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    let parent = path.parent().ok_or(OTmpfileFallback::Hard(Error::Io(
        path.to_path_buf(),
        std::io::ErrorKind::NotFound.into(),
    )))?;
    let parent_c = CString::new(parent.as_os_str().as_bytes()).map_err(|_| {
        OTmpfileFallback::Hard(Error::Io(
            path.to_path_buf(),
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "parent path has nul"),
        ))
    })?;
    // SAFETY: `parent_c` is valid for the duration of the call.
    let raw_fd = unsafe {
        libc::open(
            parent_c.as_ptr(),
            libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
            0o644 as libc::c_uint,
        )
    };
    if raw_fd < 0 {
        let err = std::io::Error::last_os_error();
        return match err.raw_os_error() {
            // Old kernels lack O_TMPFILE. Overlayfs/tmpfs return
            // EOPNOTSUPP, EISDIR, or EINVAL on some kernels.
            // ENOTSUP is the same value as EOPNOTSUPP on Linux.
            Some(libc::EOPNOTSUPP) | Some(libc::EISDIR) | Some(libc::EINVAL) => {
                Err(OTmpfileFallback::Unsupported)
            }
            _ => Err(OTmpfileFallback::Hard(Error::Io(path.to_path_buf(), err))),
        };
    }
    // SAFETY: raw_fd is owned, OwnedFd closes on drop.
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_fd) };
    let mut file = std::fs::File::from(owned);
    let small_threshold = cas_small_file_threshold();
    let is_large = bytes.len() >= small_threshold;
    // Best-effort fallocate so the kernel allocates contiguous extents
    // up front. Skips ext4 fragmentation churn on the next write.
    // EOPNOTSUPP and ENOSYS are fine, regular write_all handles them.
    // Skipped below `small_threshold`: fragmentation only matters for
    // multi-block writes, and most npm tarball entries are well under
    // that. See `cas_small_file_threshold` for rationale.
    if is_large {
        let _ = posix_fallocate(&file, bytes.len() as libc::off_t);
    }
    file.write_all(bytes)
        .map_err(|e| OTmpfileFallback::Hard(Error::Io(path.to_path_buf(), e)))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o644))
        .map_err(|e| OTmpfileFallback::Hard(Error::Io(path.to_path_buf(), e)))?;
    // No sync_data: contradicts the no-fsync CAS policy. Crash window
    // between write and linkat is acceptable, lockfile + state hash
    // recovers the missing entry on next install.

    let proc_link = format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&file));
    let proc_c = CString::new(proc_link.as_bytes()).map_err(|_| {
        OTmpfileFallback::Hard(Error::Io(
            path.to_path_buf(),
            std::io::Error::other("fd path has nul"),
        ))
    })?;
    let final_c = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        OTmpfileFallback::Hard(Error::Io(
            path.to_path_buf(),
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has nul"),
        ))
    })?;
    // SAFETY: both CStrings live through the call. AT_SYMLINK_FOLLOW
    // resolves the /proc/self/fd magic-link to the anon inode.
    let r = unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            proc_c.as_ptr(),
            libc::AT_FDCWD,
            final_c.as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    };
    if r == 0 {
        // CAS bytes are read-once into reflinks/hardlinks. Drop them
        // from the page cache so the parallel linker pass over many
        // packages doesn't push the working set out. Per-file cost is
        // roughly fixed regardless of size, so small files paid a
        // disproportionate share — gate on `small_threshold` to match
        // the fallocate gate above.
        if is_large {
            use std::os::fd::AsRawFd;
            let fd = file.as_raw_fd();
            // SAFETY: fd is still owned by `file` here. POSIX_FADV_DONTNEED
            // is advisory, return value is ignored.
            unsafe {
                libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED);
            }
        }
        return Ok(CasWriteOutcome::Created);
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EEXIST) => Ok(CasWriteOutcome::AlreadyExisted),
        // No /proc in this sandbox.
        Some(libc::ENOENT) => Err(OTmpfileFallback::Unsupported),
        // Kernel opens O_TMPFILE but rejects linkat from /proc/self/fd.
        // ENOTSUP is same value as EOPNOTSUPP on Linux.
        Some(libc::EOPNOTSUPP) | Some(libc::EXDEV) => Err(OTmpfileFallback::Unsupported),
        // Seccomp-filtered containers (gVisor, strict k8s pod-security
        // profiles) block linkat and return EPERM/EACCES. Fall through
        // to the tempfile path instead of aborting the install.
        Some(libc::EPERM) | Some(libc::EACCES) => Err(OTmpfileFallback::Unsupported),
        _ => Err(OTmpfileFallback::Hard(Error::Io(path.to_path_buf(), err))),
    }
}

/// Map a nibble (0–15) to its lowercase hex ASCII byte. Used by
/// `ensure_shards_exist` to build the 256 two-character shard names
/// without pulling in `format!`/`hex` per call.
fn hex_digit(n: u8) -> u8 {
    match n {
        0..=9 => b'0' + n,
        10..=15 => b'a' + n - 10,
        _ => unreachable!(),
    }
}

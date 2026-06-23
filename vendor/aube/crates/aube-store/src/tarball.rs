use crate::{Error, PackageIndex, Store, StoredFile};
use std::path::Path;

impl Store {
    /// Import every file under a directory into the store, producing a
    /// `PackageIndex` keyed by paths relative to `dir`. Used by `file:`
    /// deps pointing at an on-disk package directory. Common noise
    /// (`.git`, `node_modules`) is skipped so local packages don't drag
    /// the target's own installed deps into the virtual store.
    pub fn import_directory(&self, dir: &Path) -> Result<PackageIndex, Error> {
        let mut index = PackageIndex::default();
        self.import_directory_recursive(dir, dir, &mut index)?;
        Ok(index)
    }

    fn import_directory_recursive(
        &self,
        base: &Path,
        current: &Path,
        index: &mut PackageIndex,
    ) -> Result<(), Error> {
        let entries = std::fs::read_dir(current)
            .map_err(|e| Error::Tar(format!("read_dir {}: {e}", current.display())))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| Error::Tar(format!("read_dir {}: {e}", current.display())))?;
            let file_type = entry
                .file_type()
                .map_err(|e| Error::Tar(format!("file_type: {e}")))?;
            let name_os = entry.file_name();
            let name_str = name_os.to_string_lossy();
            if matches!(name_str.as_ref(), ".git" | "node_modules") {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                self.import_directory_recursive(base, &path, index)?;
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let content = std::fs::read(&path)
                .map_err(|e| Error::Tar(format!("read {}: {e}", path.display())))?;
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                let meta = entry
                    .metadata()
                    .map_err(|e| Error::Tar(format!("metadata: {e}")))?;
                meta.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            let stored = self.import_bytes(&content, executable)?;
            let rel = path
                .strip_prefix(base)
                .map_err(|e| Error::Tar(format!("strip_prefix: {e}")))?
                .to_string_lossy()
                .replace('\\', "/");
            index.insert(rel, stored);
        }
        Ok(())
    }

    /// Import a tarball (.tgz) into the store.
    /// Returns a PackageIndex mapping relative paths to stored files.
    ///
    /// Two-phase: serial tar walk that stages
    /// `(rel_path, content, executable)` triples (the tar reader is
    /// inherently sequential), then a CAS-write batch. When the
    /// staged batch crosses [`PARALLEL_IMPORT_THRESHOLD`] entries,
    /// the writes fan out via `rayon::par_iter` — the per-file CAS
    /// path is `O_CREAT|O_EXCL` and uses a shared `&Store`, so
    /// parallel writers are race-safe by construction (`EEXIST` on
    /// content collision is a success path because BLAKE3 paths are
    /// content-addressed).
    ///
    /// `AUBE_DISABLE_PARALLEL_IMPORT=1` forces the serial path. Use
    /// it as a regression killswitch if a future rayon scope inversion
    /// (linker symlink pass running concurrently) shows contention.
    /// Below the threshold the small-tarball overhead of rayon
    /// dispatch outweighs the win, so the cutover is conditional.
    pub fn import_tarball(&self, tarball_bytes: &[u8]) -> Result<PackageIndex, Error> {
        // &[u8] impls std::io::Read by advancing the slice.
        self.import_tarball_reader(tarball_bytes)
    }

    /// Streaming variant. Accepts any compressed-tarball Read source so
    /// callers can pipe HTTP body chunks straight through without
    /// buffering the whole archive into memory first. Caps and CAS
    /// publish semantics match `import_tarball` exactly.
    pub fn import_tarball_reader<R: std::io::Read>(
        &self,
        compressed_reader: R,
    ) -> Result<PackageIndex, Error> {
        use std::io::Read;

        let _diag =
            aube_util::diag::Span::new(aube_util::diag::Category::Store, "import_tarball_reader");
        let _diag_decode = aube_util::diag::inflight(aube_util::diag::Slot::Decode);
        let extract_t0 = std::time::Instant::now();

        // Caps defend against gzip bombs and lying tar headers. The
        // values sit well above any real npm package (largest top
        // 1000 are in the tens of MiB) but low enough to prevent a
        // malicious registry or mirror from OOMing the installer
        // with a small high-compression-ratio payload.
        //
        // CappedReader instead of Read::take for the archive-level
        // cap so exhaustion surfaces as an Err. A clean EOF landing on
        // a tar block boundary would let a crafted archive silently
        // truncate into a partial index.
        let gz = flate2::read::GzDecoder::new(compressed_reader);
        let capped = CappedReader::new(gz, MAX_TARBALL_DECOMPRESSED_BYTES);
        let buffered = std::io::BufReader::with_capacity(256 * 1024, capped);
        let mut archive = tar::Archive::new(buffered);
        /*
         * Chunked staged pipeline. Read N entries, flush them to CAS
         * via rayon parallel writes, repeat. Keeps the existing
         * rayon global pool warm across chunks and partially
         * overlaps tar parsing with file writes within a single
         * tarball. No new threads spawned (per-call thread::scope
         * was tried and live locked at 80 s, see git history if
         * curious). Chunk size of 64 is roughly the median npm
         * package's file count, so most tarballs flush at most
         * once or twice; fat native bindings (next, sharp, swc)
         * with 1k+ files chunk through 16+ flushes. The legacy
         * "stage everything then flush" path remains under
         * `AUBE_DISABLE_PIPELINED_IMPORT=1` for byte-identity
         * regression debugging.
         */
        const PIPELINE_CHUNK_SIZE: usize = 64;
        let pipelined_disabled = aube_util::env::embedder_env("DISABLE_PIPELINED_IMPORT").is_some();
        let parallel_disabled = aube_util::env::embedder_env("DISABLE_PARALLEL_IMPORT").is_some();
        let mut staged: Vec<(String, Vec<u8>, bool)> = Vec::new();
        let mut entries_seen: usize = 0;
        let mut total_uncompressed: u64 = 0;
        let mut decode_ns: u128 = 0;
        let mut cas_ns: u128 = 0;
        let mut index = PackageIndex::default();
        let mut staged_count: usize = 0;

        let flush_chunk = |chunk: Vec<(String, Vec<u8>, bool)>,
                           index: &mut PackageIndex,
                           cas_ns: &mut u128|
         -> Result<(), Error> {
            if chunk.is_empty() {
                return Ok(());
            }
            let chunk_t0 = std::time::Instant::now();
            if parallel_disabled || chunk.len() < PARALLEL_IMPORT_THRESHOLD {
                for (rel_path, content, executable) in chunk {
                    let stored = self.import_bytes(&content, executable)?;
                    index.insert(rel_path, stored);
                }
            } else {
                use rayon::iter::{
                    IndexedParallelIterator, IntoParallelIterator, ParallelIterator,
                };
                // `with_min_len` raises the minimum work unit per
                // rayon task. samply on a 1230-pkg cold install
                // pinned `crossbeam_deque::Stealer::steal` at 4.1%
                // self time; each per-file task is ~50µs of useful
                // work, below rayon's amortization threshold for
                // its work-stealing overhead. Grouping 8 files per
                // task amortizes the dispatch/steal cost without
                // losing meaningful parallelism — 8 × 50µs = 400µs,
                // well under a typical OS scheduling slice.
                const RAYON_TASK_MIN_LEN: usize = 8;
                let results: Vec<Result<(String, StoredFile), Error>> = chunk
                    .into_par_iter()
                    .with_min_len(RAYON_TASK_MIN_LEN)
                    .map(|(rel_path, content, executable)| {
                        self.import_bytes(&content, executable)
                            .map(|stored| (rel_path, stored))
                    })
                    .collect();
                for r in results {
                    let (rel_path, stored) = r?;
                    index.insert(rel_path, stored);
                }
            }
            *cas_ns += chunk_t0.elapsed().as_nanos();
            Ok(())
        };

        for entry in archive.entries().map_err(|e| Error::Tar(e.to_string()))? {
            entries_seen += 1;
            if entries_seen > MAX_TARBALL_ENTRIES {
                return Err(Error::Tar(format!(
                    "tarball exceeds entry cap of {MAX_TARBALL_ENTRIES}"
                )));
            }

            let mut entry = entry.map_err(|e| Error::Tar(e.to_string()))?;

            // Directories don't carry content, skip them. PAX global
            // and extension headers (type `g` / `x`) carry metadata
            // only — GitHub-generated tarballs (e.g. `imap@0.8.19`)
            // start with one that embeds the source git blob SHA.
            // npm/pnpm/bun tolerate these; we do too. Every other
            // non-regular entry type (symlink, hardlink, character
            // device, block device, fifo) is rejected. Real npm
            // packages ship files and directories only. Symlink and
            // hardlink entries are the load-bearing primitive of the
            // node-tar CVE-2021-37701 class and have no legitimate
            // use here.
            let entry_type = entry.header().entry_type();
            // GNU LongName/LongLink and PAX X-headers carry metadata
            // for the next real entry. The tar crate folds the long
            // name into Entry::path() automatically. Just skip the
            // metadata records themselves.
            if entry_type.is_dir()
                || matches!(
                    entry_type,
                    tar::EntryType::XGlobalHeader
                        | tar::EntryType::XHeader
                        | tar::EntryType::GNULongName
                        | tar::EntryType::GNULongLink
                )
            {
                continue;
            }
            if !matches!(
                entry_type,
                tar::EntryType::Regular | tar::EntryType::Continuous
            ) {
                return Err(Error::Tar(format!(
                    "tarball entry type {entry_type:?} is not allowed"
                )));
            }

            // Reject oversized entries up front on the declared size
            // so we never allocate a huge `Vec` just to error after.
            // `.take()` below is the belt-and-suspenders guard for
            // the case where the header lies about the stream length.
            let declared = entry
                .header()
                .size()
                .map_err(|e| Error::Tar(e.to_string()))?;
            if declared > MAX_TARBALL_ENTRY_BYTES {
                return Err(Error::Tar(format!(
                    "tarball entry exceeds per-entry cap: {declared} bytes > {MAX_TARBALL_ENTRY_BYTES}"
                )));
            }

            let raw_path = entry
                .path()
                .map_err(|e| Error::Tar(e.to_string()))?
                .to_path_buf();
            let Some(rel_path) = normalize_tar_entry_path(&raw_path)? else {
                // Entry was the wrapper directory itself with no
                // interior path after stripping. Nothing to store.
                continue;
            };

            // Clamp upfront alloc so a lying header can't force a 512
            // MiB reservation before any byte has been read. read_to_end
            // grows the Vec for the rare entry that really is huge.
            let mut content = Vec::with_capacity((declared as usize).min(VEC_PREALLOC_CEILING));
            let read_t0 = std::time::Instant::now();
            (&mut entry)
                .take(MAX_TARBALL_ENTRY_BYTES)
                .read_to_end(&mut content)
                .map_err(|e| Error::Tar(e.to_string()))?;
            decode_ns += read_t0.elapsed().as_nanos();

            // Reject header that declared 0 bytes but produced a
            // non-empty stream. Synthetic-entry injection: header
            // claims empty file, real bytes go to disk.
            if declared == 0 && !content.is_empty() {
                return Err(Error::Tar(format!(
                    "tarball entry declared 0 bytes but yielded {} bytes",
                    content.len()
                )));
            }

            let mode = entry.header().mode().unwrap_or(0o644);
            let executable = mode & 0o111 != 0;
            total_uncompressed = total_uncompressed.saturating_add(content.len() as u64);
            staged.push((rel_path, content, executable));
            staged_count += 1;

            if !pipelined_disabled && staged.len() >= PIPELINE_CHUNK_SIZE {
                let chunk = std::mem::take(&mut staged);
                flush_chunk(chunk, &mut index, &mut cas_ns)?;
            }
        }

        aube_util::diag::event_lazy(
            aube_util::diag::Category::Store,
            "tar_extract_complete",
            extract_t0.elapsed(),
            || format!(r#"{{"entries":{staged_count},"bytes_uncompressed":{total_uncompressed}}}"#),
        );
        if aube_util::diag::enabled() {
            aube_util::diag::event_lazy(
                aube_util::diag::Category::Store,
                "gzip_decompress",
                std::time::Duration::from_nanos(decode_ns as u64),
                || format!(r#"{{"bytes_uncompressed":{total_uncompressed}}}"#),
            );
        }

        if !staged.is_empty() {
            let chunk = std::mem::take(&mut staged);
            flush_chunk(chunk, &mut index, &mut cas_ns)?;
        }
        aube_util::diag::event_lazy(
            aube_util::diag::Category::Store,
            "cas_import_complete",
            // Saturating cast: u128 cas_ns won't realistically
            // exceed u64::MAX (~584 years in nanoseconds), but a
            // bug or runaway accumulator should clamp to the diag
            // ceiling rather than silently truncate the high bits
            // and emit a misleadingly small duration.
            std::time::Duration::from_nanos(u64::try_from(cas_ns).unwrap_or(u64::MAX)),
            || {
                let pipelined = !pipelined_disabled;
                let parallel = !parallel_disabled && staged_count >= PARALLEL_IMPORT_THRESHOLD;
                format!(
                    r#"{{"files":{staged_count},"parallel":{parallel},"pipelined":{pipelined}}}"#
                )
            },
        );
        Ok(index)
    }
}

// Median npm tarball has 7 files. Old 256 threshold almost never
// tripped. Rayon dispatch is cheap on tiny batches.
// AUBE_DISABLE_PARALLEL_IMPORT kills the parallel path entirely.
const PARALLEL_IMPORT_THRESHOLD: usize = 16;

/// Strip the wrapper directory from `raw` and return a safe POSIX-style
/// index key, or refuse the entry outright.
///
/// Rejects every shape that would let a crafted tarball place the
/// eventual `pkg_dir.join(key)` file outside the package root:
/// `..` anywhere in the remaining path, absolute paths, Windows
/// drive prefixes, backslash separators smuggled inside a single
/// component, and NUL bytes. Non-UTF-8 paths are also rejected because
/// the stored index is a JSON map keyed by string.
///
/// `Ok(None)` means the entry stripped down to the wrapper itself
/// and should be skipped by the caller.
///
/// Matches the class of defences node-tar added for CVE-2021-32804,
/// CVE-2021-37713, and what pnpm added for CVE-2024-27298.
pub(crate) fn normalize_tar_entry_path(raw: &Path) -> Result<Option<String>, Error> {
    use std::path::Component;

    // Peek past any leading `.` segments (`./package/foo.js` appears
    // in some tar implementations' wrapper representations) so the
    // first-component reject and the wrapper-strip both work off the
    // same "first real" position. Running the reject before the
    // stripping loop would otherwise let `./../file` silently
    // consume the `..` as the wrapper.
    let mut components = raw.components().peekable();
    while matches!(components.peek(), Some(Component::CurDir)) {
        components.next();
    }

    // Reject absolute, drive-prefixed, or `..`-rooted paths before
    // wrapper-strip runs. A naive "skip the first component" would
    // otherwise strip the `RootDir` marker and accept `/etc` as
    // `etc`, or consume a leading `..` as the wrapper.
    match components.peek() {
        Some(Component::RootDir) => {
            return Err(Error::Tar(format!(
                "tarball entry path is absolute: {raw:?}"
            )));
        }
        Some(Component::Prefix(_)) => {
            return Err(Error::Tar(format!(
                "tarball entry path has a Windows drive prefix: {raw:?}"
            )));
        }
        Some(Component::ParentDir) => {
            return Err(Error::Tar(format!(
                "tarball entry path escapes package root via `..`: {raw:?}"
            )));
        }
        _ => {}
    }

    // Drop the first real component as the wrapper directory. npm
    // convention is `package/`, but some packages ship the package
    // name or another identifier. Whatever it is, drop it.
    components.next();

    // Pre-size to the raw path length so growing the output never
    // reallocates: the normalized form drops the wrapper segment and
    // converts `\` to `/` but never grows beyond the input size.
    let mut out = String::with_capacity(raw.as_os_str().len());
    for comp in components {
        match comp {
            Component::Normal(os) => {
                let s = os.to_str().ok_or_else(|| {
                    Error::Tar(format!(
                        "tarball entry path contains non-UTF-8 bytes: {raw:?}"
                    ))
                })?;
                if s.is_empty() || s.contains('\0') || s.contains('\\') || s.contains('/') {
                    return Err(Error::Tar(format!(
                        "tarball entry path contains a malformed component: {raw:?}"
                    )));
                }
                // Windows-only filename restrictions. Gated to
                // cfg(windows) so Unix hosts keep tarballs with
                // valid-on-Linux names like `CON.js` or `foo.`.
                // Rejecting those cross-platform would regress real
                // Linux installs for a hazard that only hits
                // Windows users. Windows users get the checks they
                // need, portability of a package to Windows is the
                // publisher's problem to validate.
                #[cfg(windows)]
                {
                    // `:` is an alternate data stream separator on
                    // NTFS and is rejected by Windows path creation.
                    if s.contains(':') {
                        return Err(Error::Tar(format!(
                            "tarball entry path contains a malformed component: {raw:?}"
                        )));
                    }
                    // NTFS reserved device names. `CON`, `con.txt`,
                    // `CON.tar.gz` all resolve to the console
                    // device. Writing one either fails with
                    // ERROR_INVALID_NAME or gets silently consumed
                    // by the device driver and hangs the writer.
                    if is_windows_reserved_name(s) {
                        return Err(Error::Tar(format!(
                            "tarball entry path contains a Windows reserved device name: {raw:?}"
                        )));
                    }
                    // NTFS strips trailing `.` and trailing space
                    // on create so `foo` and `foo.` alias. Reject
                    // both rather than sort out aliasing at
                    // materialize time.
                    if s.ends_with('.') || s.ends_with(' ') {
                        return Err(Error::Tar(format!(
                            "tarball entry path has a trailing dot or space which Windows strips: {raw:?}"
                        )));
                    }
                    // Control chars 0x01..0x1F invalid on NTFS.
                    // Reject the whole tarball instead of hitting
                    // per-file create errors mid-extract.
                    if s.bytes().any(|b| b < 0x20) {
                        return Err(Error::Tar(format!(
                            "tarball entry path contains control characters: {raw:?}"
                        )));
                    }
                }
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(s);
            }
            Component::ParentDir => {
                return Err(Error::Tar(format!(
                    "tarball entry path escapes package root via `..`: {raw:?}"
                )));
            }
            Component::RootDir => {
                return Err(Error::Tar(format!(
                    "tarball entry path is absolute: {raw:?}"
                )));
            }
            Component::Prefix(_) => {
                return Err(Error::Tar(format!(
                    "tarball entry path has a Windows drive prefix: {raw:?}"
                )));
            }
            // `.` components are harmless and appear in some tar
            // implementations' wrapper representations.
            Component::CurDir => {}
        }
    }

    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

/// Check if a tarball path component matches a Windows reserved
/// device name. Compare case-insensitively on the stem only.
/// `CON`, `Con`, `con`, `con.txt`, `CON.tar.gz` all resolve to the
/// same DOS device on NTFS. Only base name matters, the extension
/// is irrelevant to the device lookup.
#[cfg(windows)]
fn is_windows_reserved_name(name: &str) -> bool {
    let stem = name.split_once('.').map(|(a, _)| a).unwrap_or(name);
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// Hard ceiling on the per-entry `Vec::with_capacity` hint. 64 KiB
/// covers the bulk of real npm package files (JS / JSON / TS, all
/// typically under a few KiB each) without trusting the declared
/// header size, which an attacker controls. `read_to_end` grows
/// past this ceiling when a legitimate larger file warrants it.
const VEC_PREALLOC_CEILING: usize = 64 * 1024;

/// A `Read` wrapper that refuses to deliver more than `remaining`
/// bytes. Unlike `std::io::Read::take`, exhaustion produces an
/// explicit `io::Error` rather than a clean EOF. When the wrapped
/// reader is a gzip decoder feeding a tar archive, a clean EOF at
/// a block boundary would let a crafted archive silently truncate
/// into a partial index. Surfacing an error keeps the archive
/// iterator from accepting a half-read stream as complete.
pub(crate) struct CappedReader<R: std::io::Read> {
    inner: R,
    remaining: u64,
}

impl<R: std::io::Read> CappedReader<R> {
    pub(crate) fn new(inner: R, cap: u64) -> Self {
        Self {
            inner,
            remaining: cap,
        }
    }
}

impl<R: std::io::Read> std::io::Read for CappedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // A zero-length read is a no-op by the `Read` contract and
        // must not error even if the cap is already exhausted.
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "tarball decompression exceeds archive cap of {MAX_TARBALL_DECOMPRESSED_BYTES} bytes"
                ),
            ));
        }
        let want = buf.len().min(self.remaining as usize);
        let n = self.inner.read(&mut buf[..want])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// Maximum total decompressed bytes accepted from a single tarball.
/// 1 GiB. Reality check against the npm registry on 2026-04-19.
/// Biggest tarball in the top 1000 by download count is `next` at
/// 154 MiB unpacked. Second is `@tensorflow/tfjs` at 147 MiB. The
/// cap sits ~6x above both, leaves room for future growth, and
/// stays well below the process RSS a gzip bomb would otherwise
/// force the installer to allocate.
#[cfg(not(test))]
pub(crate) const MAX_TARBALL_DECOMPRESSED_BYTES: u64 = 1 << 30;
#[cfg(test)]
pub(crate) const MAX_TARBALL_DECOMPRESSED_BYTES: u64 = 1 << 20;

/// Maximum bytes for a single tar entry. 512 MiB. Reality check: the
/// largest legitimate single file shipped by a top-1000 npm package
/// sits in the tens of MiB range (bundled WASM blobs in `@swc/wasm`,
/// `@babel/standalone`, `monaco-editor`). 512 MiB leaves a full
/// order of magnitude of headroom.
#[cfg(not(test))]
pub(crate) const MAX_TARBALL_ENTRY_BYTES: u64 = 512 << 20;
#[cfg(test)]
pub(crate) const MAX_TARBALL_ENTRY_BYTES: u64 = 1 << 20;

/// Maximum number of tar entries in a single archive. 200_000.
/// Reality check: `next` ships 8_065 files and `@fluentui/react`
/// ships 7_448, the largest counts in the top 1000. 200_000 is
/// ~25x above that and stops a crafted archive from pinning the
/// CPU on iteration alone.
#[cfg(not(test))]
pub(crate) const MAX_TARBALL_ENTRIES: usize = 200_000;
#[cfg(test)]
pub(crate) const MAX_TARBALL_ENTRIES: usize = 64;

//! PM tarball extraction. npm publishes `.tgz` (gzip + tar) with everything under
//! a single `package/` dir — the same single-top-dir shape as a Node dist archive,
//! so this reuses the shared [`single_top_dir`] guard.
//!
//! This extractor handles ONLY package-manager tarballs (pnpm / npm / yarn), whose
//! genuine publishes ship regular files and directories — never symlinks or
//! hardlinks. So, unlike the Node-dist extractor (which must allow Node's
//! intra-tree symlinks), this path REJECTS symlink/hardlink entries outright
//! (F0b, CVE-2021-37701 class): the extracted tree is executed as the PM, and a
//! symlink whose target escapes the package dir would otherwise become an
//! exec-able file pointing anywhere on disk. It also caps the decompressed stream
//! against a gzip bomb (N2) — mirroring the engine's own tarball importer.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;

use crate::version_management::extract::{
    CappedReader, MAX_ARCHIVE_DECOMPRESSED_BYTES, MAX_ARCHIVE_ENTRIES, single_top_dir,
};

/// Decode a `.tgz` (gzip + tar) and unpack it under `dest_parent`, returning the
/// single top-level directory it created (`package/` for an npm tarball).
///
/// A manual entry walk (rather than `Archive::unpack`) so each entry's TYPE is
/// vetted: symlink and hardlink entries are rejected (F0b — a PM tarball never
/// ships them, and one would let extraction plant an exec-able link escaping the
/// package dir). The gzip stream is wrapped in a [`CappedReader`] so a
/// decompression bomb errors instead of exhausting disk (N2). `Entry::unpack_in`
/// keeps the `tar` crate's `..`/absolute path-traversal guard (an escaping entry
/// is skipped, not written) and preserves the bin's executable mode;
/// `single_top_dir` enforces the one-dir invariant. The entry COUNT is bounded
/// too (N2 — the `tar` crate has no count guard), mirroring aube-store's caps.
pub(crate) fn extract_tgz(archive: &Path, dest_parent: &Path) -> Result<PathBuf> {
    extract_tgz_capped(
        archive,
        dest_parent,
        MAX_ARCHIVE_DECOMPRESSED_BYTES,
        MAX_ARCHIVE_ENTRIES,
    )
}

/// [`extract_tgz`] with the decompression + entry-count caps as explicit
/// parameters — the seam a bomb test uses to inject tiny caps (the public entry
/// uses the prod constants, which the real-tarball provisioning e2e relies on).
fn extract_tgz_capped(
    archive: &Path,
    dest_parent: &Path,
    decompressed_cap: u64,
    max_entries: usize,
) -> Result<PathBuf> {
    let file =
        std::fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let capped = CappedReader::new(GzDecoder::new(file), decompressed_cap);
    let mut tar = tar::Archive::new(capped);
    std::fs::create_dir_all(dest_parent)
        .with_context(|| format!("create {}", dest_parent.display()))?;

    let mut count = 0usize;
    for entry in tar
        .entries()
        .with_context(|| format!("reading {}", archive.display()))?
    {
        count += 1;
        if count > max_entries {
            bail!(
                "{} exceeds the {max_entries}-entry archive cap",
                archive.display()
            );
        }
        let mut entry = entry.with_context(|| format!("reading entry in {}", archive.display()))?;
        let entry_type = entry.header().entry_type();
        if matches!(entry_type, tar::EntryType::Symlink | tar::EntryType::Link) {
            let name = entry
                .path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "<unreadable>".to_string());
            bail!(
                "refusing {entry_type:?} entry {name:?} in package-manager tarball {} \
                 (symlink/hardlink entries are not allowed — CVE-2021-37701 class)",
                archive.display()
            );
        }
        // `unpack_in` applies the path-traversal guard (an entry that resolves
        // outside `dest_parent` returns Ok(false) and is skipped, never written).
        entry
            .unpack_in(dest_parent)
            .with_context(|| format!("extracting {}", archive.display()))?;
    }
    single_top_dir(dest_parent, archive)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique temp dir per test (`std::process::id()` alone collides across the
    /// two tests in this module under the parallel harness).
    fn tmpdir() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nub-tgz-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Author a `.tgz` from `(path, contents)` entries, as npm publishes (gzip +
    /// tar). A path is written verbatim into the header's name field, bypassing
    /// `Builder::append_data`'s authoring-time `..` rejection — so a test can plant
    /// a hostile `../escape` entry the way a malicious registry tarball would, to
    /// exercise the *extraction*-time traversal guard (the one that actually
    /// matters).
    fn write_tgz(archive: &Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(archive).unwrap();
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(gz);
        for (path, contents) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(contents.len() as u64);
            h.set_mode(0o755);
            // Write the name bytes directly (the GNU `name` field is 100 bytes) so
            // an unsafe path lands in the archive verbatim; `set_cksum` last.
            let name = path.as_bytes();
            let gnu = h.as_gnu_mut().expect("new_gnu header is GNU");
            gnu.name[..name.len()].copy_from_slice(name);
            h.set_cksum();
            builder.append(&h, *contents).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }

    /// Extract a `package/`-rooted `.tgz` (as npm publishes) and confirm the
    /// returned top dir is `package` and a nested bin file survives — the real
    /// gzip+tar decode path, no network.
    #[test]
    fn extract_tgz_returns_the_package_dir_with_nested_bin_intact() {
        let dir = tmpdir();
        let archive = dir.join("sample.tgz");
        write_tgz(
            &archive,
            &[
                ("package/bin/x.cjs", b"#!/usr/bin/env node\n"),
                ("package/package.json", br#"{"name":"x"}"#),
            ],
        );

        let out = dir.join("extracted");
        let top = extract_tgz(&archive, &out).unwrap();
        assert_eq!(top.file_name().unwrap(), "package");
        assert!(
            top.join("bin").join("x.cjs").is_file(),
            "nested bin survives extraction"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hostile tarball entry that `../`-escapes the extraction dir must NOT land
    /// outside it. This is the load-bearing guard: extraction writes executables to
    /// disk, so a compromised/malicious registry tarball must not be able to plant
    /// a file in a sibling of the store (or anywhere above `dest_parent`). The
    /// `tar` crate skips escaping entries during `unpack`; this pins that contract
    /// so a future refactor (e.g. a hand-rolled unpack loop) can't silently drop it.
    #[test]
    fn extract_tgz_contains_a_path_traversal_entry() {
        let dir = tmpdir();
        let archive = dir.join("evil.tgz");
        // One legit `package/` entry (so extraction has its single top dir) plus a
        // sibling-escaping entry. `dest_parent` is `dir/extracted`, so `../escaped`
        // would land at `dir/escaped` if the guard failed.
        write_tgz(
            &archive,
            &[
                ("package/package.json", br#"{"name":"x"}"#),
                ("../escaped.txt", b"pwned"),
            ],
        );

        let out = dir.join("extracted");
        let top = extract_tgz(&archive, &out).unwrap();
        assert_eq!(top.file_name().unwrap(), "package");
        assert!(
            !dir.join("escaped.txt").exists(),
            "a `../`-escaping entry must not be written outside the extraction dir"
        );
        assert!(
            !out.join("escaped.txt").exists(),
            "the escaping entry must not appear inside the extraction dir either"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F0b: a PM tarball ships regular files only; a symlink entry (the
    /// CVE-2021-37701 primitive) whose target escapes the package dir would land
    /// an exec-able link pointing anywhere on disk, then be run as the PM bin.
    /// Extraction must REFUSE the archive, not silently materialize the link.
    #[test]
    fn extract_tgz_rejects_a_symlink_entry() {
        let dir = tmpdir();
        let archive = dir.join("symlink.tgz");
        {
            let file = std::fs::File::create(&archive).unwrap();
            let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut builder = tar::Builder::new(gz);
            // A legit package.json gives the archive its single top dir...
            let manifest = br#"{"name":"x"}"#;
            let mut h = tar::Header::new_gnu();
            h.set_size(manifest.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            builder
                .append_data(&mut h, "package/package.json", &manifest[..])
                .unwrap();
            // ...then a hostile symlink pointing outside the package dir.
            let mut link = tar::Header::new_gnu();
            link.set_entry_type(tar::EntryType::Symlink);
            link.set_size(0);
            link.set_mode(0o777);
            link.set_cksum();
            builder
                .append_link(&mut link, "package/evil", "/etc/passwd")
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        let out = dir.join("extracted");
        let err = extract_tgz(&archive, &out).unwrap_err().to_string();
        assert!(
            err.contains("Symlink") && err.contains("CVE-2021-37701"),
            "a symlink entry must be refused with a clear reason: {err}"
        );
        assert!(
            !out.join("evil").exists(),
            "the symlink must not be materialized"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// N2: a small-compressed / huge-decompressed `.tgz` whose sha512 (computed
    /// over the COMPRESSED bytes) passes integrity, then extracts unbounded. The
    /// `CappedReader` must error before the whole payload lands on disk. Drives
    /// the `_capped` seam with a 1 MiB cap (the public entry uses the 1 GiB prod
    /// cap, which the real-tarball provisioning e2e relies on).
    #[test]
    fn extract_tgz_rejects_a_decompression_bomb() {
        let dir = tmpdir();
        let archive = dir.join("bomb.tgz");
        {
            let file = std::fs::File::create(&archive).unwrap();
            let gz = flate2::write::GzEncoder::new(file, flate2::Compression::best());
            let mut builder = tar::Builder::new(gz);
            let big = vec![0u8; 4 * 1024 * 1024]; // 4 MiB of zeros → compresses tiny
            let mut h = tar::Header::new_gnu();
            h.set_size(big.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            builder
                .append_data(&mut h, "package/big.bin", &big[..])
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        assert!(
            std::fs::metadata(&archive).unwrap().len() < 64 * 1024,
            "the compressed bomb must be tiny for this to be a real amplification"
        );
        let out = dir.join("extracted");
        let err = format!(
            "{:#}",
            extract_tgz_capped(&archive, &out, 1 << 20, MAX_ARCHIVE_ENTRIES).unwrap_err()
        );
        assert!(
            err.to_lowercase().contains("cap") || err.to_lowercase().contains("decompress"),
            "a decompression bomb must be refused by the cap: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// N2 (entry-count): an archive of many tiny entries stays under the byte cap
    /// but drives a `File::create` per entry. The entry-count cap must refuse it.
    /// Drives the `_capped` seam with a 2-entry cap.
    #[test]
    fn extract_tgz_rejects_too_many_entries() {
        let dir = tmpdir();
        let archive = dir.join("manyentries.tgz");
        write_tgz(
            &archive,
            &[
                ("package/a", b"x"),
                ("package/b", b"y"),
                ("package/c", b"z"),
            ],
        );
        let out = dir.join("extracted");
        let err = format!(
            "{:#}",
            extract_tgz_capped(&archive, &out, 1 << 20, 2).unwrap_err()
        );
        assert!(
            err.contains("entry archive cap"),
            "an over-count archive must be refused: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

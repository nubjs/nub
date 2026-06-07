//! PM tarball extraction. npm publishes `.tgz` (gzip + tar) with everything under
//! a single `package/` dir — the same single-top-dir shape as a Node dist archive,
//! so this reuses the shared [`single_top_dir`] guard (which also keeps the `tar`
//! crate's path-traversal protection in one place).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use flate2::read::GzDecoder;

use crate::version_management::extract::single_top_dir;

/// Decode a `.tgz` (gzip + tar) and unpack it under `dest_parent`, returning the
/// single top-level directory it created (`package/` for an npm tarball). The
/// `tar` crate guards against path-traversal (`..` / absolute entries) during
/// `unpack`, and `single_top_dir` enforces the one-dir invariant.
pub fn extract_tgz(archive: &Path, dest_parent: &Path) -> Result<PathBuf> {
    let file =
        std::fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let mut tar = tar::Archive::new(GzDecoder::new(file));
    std::fs::create_dir_all(dest_parent)
        .with_context(|| format!("create {}", dest_parent.display()))?;
    tar.unpack(dest_parent)
        .with_context(|| format!("extracting {}", archive.display()))?;
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
}

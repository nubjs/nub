//! Extract a verified Node dist archive into nub's store.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The single top-level directory `dest_parent` now holds after an archive was
/// unpacked into it — the `node-v<ver>-<plat>` dir for Node tarballs/zips, the
/// `package` dir for an npm `.tgz`. Errors if zero or more than one dir is present
/// (a stock archive always has exactly one). Shared by every extractor so the
/// "one top dir, or it's malformed" rule lives in one place; `archive` is only
/// used to name the file in error messages.
pub(crate) fn single_top_dir(dest_parent: &Path, archive: &Path) -> Result<PathBuf> {
    let mut top: Option<PathBuf> = None;
    for entry in std::fs::read_dir(dest_parent)? {
        let path = entry?.path();
        if path.is_dir() && top.replace(path).is_some() {
            bail!(
                "expected a single top-level directory in {}",
                archive.display()
            );
        }
    }
    top.with_context(|| format!("no directory extracted from {}", archive.display()))
}

/// Decode a `.tar.xz` and unpack it under `dest_parent`, returning the single
/// top-level directory it created (the `node-v<ver>-<plat>` dir). The `tar` crate
/// guards against path-traversal (`..` / absolute entries) during `unpack`.
pub fn extract_tar_xz(archive: &Path, dest_parent: &Path) -> Result<PathBuf> {
    let file =
        std::fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let decoder = liblzma::read::XzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    std::fs::create_dir_all(dest_parent)
        .with_context(|| format!("create {}", dest_parent.display()))?;
    tar.unpack(dest_parent)
        .with_context(|| format!("extracting {}", archive.display()))?;
    single_top_dir(dest_parent, archive)
}

/// Unpack a Node Windows dist `.zip` under `dest_parent`, returning the single
/// top-level directory it created (the `node-v<ver>-win-<arch>` dir holding
/// `node.exe`). Pure-Rust via the `zip` crate — no shell-out to PowerShell's
/// `Expand-Archive` or `tar.exe`, so extraction is identical across Windows
/// versions and the checksum-verify-then-extract flow stays in one process.
/// `ZipArchive::extract` guards against path-traversal (`..` / absolute entries).
///
/// Mode handling: `extract` applies stored unix mode bits on unix targets and is
/// a no-op for them on Windows (where executability is by extension, so
/// `node.exe` is runnable automatically). Stock Node `.zip`s carry POSIX modes
/// in their extra fields, so a unix build extracting one keeps `node.exe`
/// readable; the normal Windows-only path doesn't depend on it.
pub fn extract_zip(archive: &Path, dest_parent: &Path) -> Result<PathBuf> {
    let file =
        std::fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let mut zip =
        zip::ZipArchive::new(file).with_context(|| format!("reading zip {}", archive.display()))?;
    std::fs::create_dir_all(dest_parent)
        .with_context(|| format!("create {}", dest_parent.display()))?;
    zip.extract(dest_parent)
        .with_context(|| format!("extracting {}", archive.display()))?;
    single_top_dir(dest_parent, archive)
}

/// Extract `archive` by type: `.tar.xz` (macOS/Linux) or `.zip` (Windows). Both
/// paths verify the archive against its published SHA-256 before this call (see
/// `provision_node`) and unpack in-process — no `tar`/`xz`/`Expand-Archive`
/// shell-out — so the same verify-then-extract guarantee holds on every host.
pub fn extract_archive(archive: &Path, dest_parent: &Path) -> Result<PathBuf> {
    let name = archive
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if name.ends_with(".tar.xz") {
        extract_tar_xz(archive, dest_parent)
    } else if name.ends_with(".zip") {
        extract_zip(archive, dest_parent)
    } else {
        bail!("unrecognized Node archive format: {name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tiny `.tar.xz` with a single top dir, extract it, and confirm the
    /// returned top dir + a nested file survive. Exercises the real liblzma + tar
    /// decode path without the network.
    #[test]
    fn extract_tar_xz_returns_the_single_top_dir() {
        let dir = std::env::temp_dir().join(format!("nub-xz-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("sample.tar.xz");

        // Author the archive: top/bin/node + top/README.
        {
            let file = std::fs::File::create(&archive).unwrap();
            let enc = liblzma::write::XzEncoder::new(file, 6);
            let mut builder = tar::Builder::new(enc);
            let mut header = |path: &str, contents: &[u8]| {
                let mut h = tar::Header::new_gnu();
                h.set_size(contents.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                builder.append_data(&mut h, path, contents).unwrap();
            };
            header("top/bin/node", b"#!/bin/sh\n");
            header("top/README", b"hi\n");
            builder.into_inner().unwrap().finish().unwrap();
        }

        let out = dir.join("extracted");
        let top = extract_tar_xz(&archive, &out).unwrap();
        assert_eq!(top.file_name().unwrap(), "top");
        assert!(top.join("bin").join("node").is_file());
        assert!(top.join("README").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Build a tiny `.zip` with a single top dir + a nested file, extract it, and
    /// confirm the returned top dir + nested file survive. Mirrors the tar test.
    /// The zip format is identical cross-platform, so this proves the extraction
    /// logic on the dev box (macOS) even though the real Windows provisioning e2e
    /// can only run on the windows-latest CI leg.
    #[test]
    fn extract_zip_returns_the_single_top_dir() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("nub-zip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("sample.zip");

        // Author the archive: top/node.exe + top/README — one top-level dir, like
        // a stock node-v<ver>-win-<arch>.zip.
        {
            let file = std::fs::File::create(&archive).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            writer.start_file("top/node.exe", opts).unwrap();
            writer.write_all(b"MZ\x90\x00").unwrap();
            writer.start_file("top/README", opts).unwrap();
            writer.write_all(b"hi\n").unwrap();
            writer.finish().unwrap();
        }

        let out = dir.join("extracted");
        let top = extract_zip(&archive, &out).unwrap();
        assert_eq!(top.file_name().unwrap(), "top");
        assert!(top.join("node.exe").is_file());
        assert!(top.join("README").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

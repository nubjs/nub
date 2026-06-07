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

    /// Author a tiny `package/`-rooted `.tgz` (as npm publishes), extract it, and
    /// confirm the returned top dir is `package` and a nested bin file survives —
    /// the real gzip+tar decode path, no network.
    #[test]
    fn extract_tgz_returns_the_package_dir_with_nested_bin_intact() {
        let dir = std::env::temp_dir().join(format!("nub-tgz-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("sample.tgz");

        {
            let file = std::fs::File::create(&archive).unwrap();
            let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut builder = tar::Builder::new(gz);
            let mut header = |path: &str, contents: &[u8]| {
                let mut h = tar::Header::new_gnu();
                h.set_size(contents.len() as u64);
                h.set_mode(0o755);
                h.set_cksum();
                builder.append_data(&mut h, path, contents).unwrap();
            };
            header("package/bin/x.cjs", b"#!/usr/bin/env node\n");
            header("package/package.json", br#"{"name":"x"}"#);
            builder.into_inner().unwrap().finish().unwrap();
        }

        let out = dir.join("extracted");
        let top = extract_tgz(&archive, &out).unwrap();
        assert_eq!(top.file_name().unwrap(), "package");
        assert!(
            top.join("bin").join("x.cjs").is_file(),
            "nested bin survives extraction"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

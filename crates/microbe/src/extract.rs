//! Integrity verification and tarball extraction.

use crate::error::Error;
use crate::registry::Dist;
use base64::Engine;
use sha1::Sha1;
use sha2::{Digest, Sha512};
use std::path::{Component, Path, PathBuf};

/// `integrity` is an SRI string (`sha512-<base64>`); `shasum` is the pre-SRI hex sha1. A
/// tarball with neither is accepted: the registry contract predates both for old packages.
pub fn verify(bytes: &[u8], dist: &Dist, name: &str, version: &str) -> Result<(), Error> {
    let mismatch = || Error::Integrity {
        name: name.to_string(),
        version: version.to_string(),
    };
    if let Some(sri) = &dist.integrity {
        // SRI may list several algorithms space-separated; sha512 is what the registry emits.
        for entry in sri.split_whitespace() {
            if let Some(b64) = entry.strip_prefix("sha512-") {
                let want = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|_| mismatch())?;
                return if want[..] == Sha512::digest(bytes)[..] {
                    Ok(())
                } else {
                    Err(mismatch())
                };
            }
        }
    }
    if let Some(hex) = &dist.shasum {
        let got = Sha1::digest(bytes);
        let got_hex: String = got.iter().map(|b| format!("{b:02x}")).collect();
        return if got_hex.eq_ignore_ascii_case(hex) {
            Ok(())
        } else {
            Err(mismatch())
        };
    }
    Ok(())
}

/// Unpack a registry tarball into `dest`, dropping the leading directory (`package/` by
/// convention, but not always — a few publishers ship a different top-level name, so the
/// first component is stripped whatever it is). Only regular files and directories are
/// written; the registry rejects symlinks and hardlinks at publish time, and a path with a
/// `..` or root component is refused rather than normalised.
pub fn extract(tgz: &[u8], dest: &Path) -> Result<(), Error> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tgz));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw = entry.path()?.into_owned();
        let mut rel = PathBuf::new();
        for (i, c) in raw.components().enumerate() {
            match c {
                Component::Normal(_) if i == 0 => {}
                Component::Normal(part) => rel.push(part),
                Component::CurDir => {}
                _ => return Err(Error::UnsafePath(raw.display().to_string())),
            }
        }
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&rel);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            std::fs::create_dir_all(&out)?;
        } else if kind.is_file() {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            entry.unpack(&out)?;
        }
    }
    Ok(())
}

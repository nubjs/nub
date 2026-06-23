//! Plain extract-to-directory for Node release archives. Not the CAS
//! import path — runtime installs keep Node's native layout so
//! aube-managed and mise-managed installs look identical to
//! discovery.

use crate::error::Error;
use std::path::{Component, Path, PathBuf};

/// Extract `archive_path` into `dest`. `strip_first` drops the
/// top-level directory (`node-v{V}-{slug}/`; aube release archives
/// have no top dir and pass `false`). `zip` selects the Windows zip
/// format; everything else is gzipped tar.
///
/// Runs blocking I/O — call inside `spawn_blocking`.
pub(crate) fn extract_archive(
    archive_path: &Path,
    dest: &Path,
    zip: bool,
    strip_first: bool,
) -> Result<(), Error> {
    if zip {
        extract_zip(archive_path, dest, strip_first)
    } else {
        extract_tar_gz(archive_path, dest, strip_first)
    }
}

fn extract_tar_gz(archive_path: &Path, dest: &Path, strip_first: bool) -> Result<(), Error> {
    let file = std::fs::File::open(archive_path)
        .map_err(|e| Error::io(format!("open {}", archive_path.display()), e))?;
    let decoder = flate2::read::GzDecoder::new(std::io::BufReader::new(file));
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().map_err(|e| Error::ExtractFailed {
        reason: e.to_string(),
    })? {
        let mut entry = entry.map_err(|e| Error::ExtractFailed {
            reason: e.to_string(),
        })?;
        let path = entry.path().map_err(|e| Error::ExtractFailed {
            reason: e.to_string(),
        })?;
        // `entry_dest_path` doubles as the path-escape guard: it
        // returns None for the top-level dir entry (when stripping)
        // and for any path containing `..` / absolute components.
        let Some(stripped) = entry_dest_path(&path, strip_first) else {
            continue;
        };
        // Node tarballs legitimately contain intra-tree symlinks
        // (`bin/npm → ../lib/node_modules/npm/bin/npm-cli.js`), so
        // symlinks are allowed — but only when the resolved target
        // stays inside `dest`. The checksum gate upstream already
        // authenticates the archive; this is defense in depth.
        if matches!(
            entry.header().entry_type(),
            tar::EntryType::Symlink | tar::EntryType::Link
        ) {
            let target = entry
                .link_name()
                .ok()
                .flatten()
                .ok_or_else(|| Error::ExtractFailed {
                    reason: format!("link entry {} has no target", stripped.display()),
                })?;
            if !link_target_stays_inside(dest, &stripped, &target) {
                return Err(Error::ExtractFailed {
                    reason: format!(
                        "link {} escapes the install dir (target {})",
                        stripped.display(),
                        target.display()
                    ),
                });
            }
        }
        let dest_path = dest.join(&stripped);
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io(format!("create {}", parent.display()), e))?;
        }
        entry.unpack(&dest_path).map_err(|e| Error::ExtractFailed {
            reason: format!("{}: {e}", stripped.display()),
        })?;
    }
    Ok(())
}

/// Lexically resolve a link target relative to its entry location and
/// check the result stays under `dest`. Absolute targets are rejected
/// outright.
fn link_target_stays_inside(dest: &Path, entry_rel: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let from_dir = match entry_rel.parent() {
        Some(p) => dest.join(p),
        None => dest.to_path_buf(),
    };
    let resolved = aube_util::path::normalize_lexical(&from_dir.join(target));
    resolved.starts_with(dest)
}

fn extract_zip(archive_path: &Path, dest: &Path, strip_first: bool) -> Result<(), Error> {
    let file = std::fs::File::open(archive_path)
        .map_err(|e| Error::io(format!("open {}", archive_path.display()), e))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| Error::ExtractFailed {
        reason: e.to_string(),
    })?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| Error::ExtractFailed {
            reason: e.to_string(),
        })?;
        let Some(raw_path) = entry.enclosed_name() else {
            return Err(Error::ExtractFailed {
                reason: format!("unsafe entry path {:?}", entry.name()),
            });
        };
        let Some(stripped) = entry_dest_path(&raw_path, strip_first) else {
            continue;
        };
        let dest_path = dest.join(&stripped);
        if entry.is_dir() {
            std::fs::create_dir_all(&dest_path)
                .map_err(|e| Error::io(format!("create {}", dest_path.display()), e))?;
            continue;
        }
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io(format!("create {}", parent.display()), e))?;
        }
        let mut out = std::fs::File::create(&dest_path)
            .map_err(|e| Error::io(format!("create {}", dest_path.display()), e))?;
        std::io::copy(&mut entry, &mut out).map_err(|e| Error::ExtractFailed {
            reason: format!("{}: {e}", stripped.display()),
        })?;
    }
    Ok(())
}

/// Compute an entry's destination-relative path, optionally dropping
/// the leading component, and validate it stays relative (no `..`, no
/// absolute components). Returns `None` for the bare top-level dir
/// entry when stripping, and for any unsafe path.
fn entry_dest_path(path: &Path, strip_first: bool) -> Option<PathBuf> {
    let rest: PathBuf = if strip_first {
        let mut components = path.components();
        components.next()?;
        components.as_path().to_path_buf()
    } else {
        path.to_path_buf()
    };
    if rest.as_os_str().is_empty() {
        return None;
    }
    for c in rest.components() {
        match c {
            Component::Normal(_) => {}
            _ => return None,
        }
    }
    Some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_tar_gz(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (path, content) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, path, content.as_bytes())
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn tar_strips_top_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let bytes = make_tar_gz(&[
            ("node-v22.1.0-linux-x64/bin/node", "fake-binary"),
            ("node-v22.1.0-linux-x64/LICENSE", "mit"),
        ]);
        let archive = tmp.path().join("a.tar.gz");
        std::fs::write(&archive, bytes).unwrap();
        let dest = tmp.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        extract_archive(&archive, &dest, false, true).unwrap();
        assert!(dest.join("bin/node").is_file());
        assert!(dest.join("LICENSE").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn tar_intra_tree_symlink_is_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(4);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "top/lib/real.js", "hi()".as_bytes())
            .unwrap();
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_cksum();
        builder
            .append_link(&mut link, "top/bin/npm", "../lib/real.js")
            .unwrap();
        let bytes = builder.into_inner().unwrap().finish().unwrap();
        let archive = tmp.path().join("a.tar.gz");
        std::fs::write(&archive, bytes).unwrap();
        let dest = tmp.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        extract_archive(&archive, &dest, false, true).unwrap();
        assert!(dest.join("bin/npm").exists());
        assert_eq!(
            std::fs::read_to_string(dest.join("bin/npm")).unwrap(),
            "hi()"
        );
    }

    #[test]
    fn escape_paths_are_rejected_by_strip_guard() {
        // The tar crate refuses to even *author* `..` entries, so the
        // guard is exercised directly: any remainder containing `..`,
        // `.` or absolute components must be dropped.
        assert_eq!(entry_dest_path(Path::new("top/../escape.txt"), true), None);
        assert_eq!(entry_dest_path(Path::new("top/a/../../b"), true), None);
        assert_eq!(
            entry_dest_path(Path::new("top//etc/passwd"), true),
            Some(PathBuf::from("etc/passwd"))
        );
        assert_eq!(entry_dest_path(Path::new("top"), true), None);
        assert_eq!(
            entry_dest_path(Path::new("top/bin/node"), true),
            Some(PathBuf::from("bin/node"))
        );
        // No-strip mode: paths kept verbatim, same escape rules.
        assert_eq!(
            entry_dest_path(Path::new("aube"), false),
            Some(PathBuf::from("aube"))
        );
        assert_eq!(entry_dest_path(Path::new("../aube"), false), None);
    }

    #[test]
    fn symlink_escape_guard() {
        let dest = Path::new("/x/dest");
        assert!(link_target_stays_inside(
            dest,
            Path::new("bin/npm"),
            Path::new("../lib/npm-cli.js")
        ));
        assert!(!link_target_stays_inside(
            dest,
            Path::new("bin/npm"),
            Path::new("../../../etc/passwd")
        ));
        assert!(!link_target_stays_inside(
            dest,
            Path::new("bin/npm"),
            Path::new("/etc/passwd")
        ));
    }

    #[test]
    fn zip_strips_top_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("a.zip");
        let file = std::fs::File::create(&archive).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let opts: zip::write::SimpleFileOptions = Default::default();
        writer
            .start_file("node-v22.1.0-win-x64/node.exe", opts)
            .unwrap();
        writer.write_all(b"fake-exe").unwrap();
        writer
            .start_file("node-v22.1.0-win-x64/npm.cmd", opts)
            .unwrap();
        writer.write_all(b"@echo off").unwrap();
        writer.finish().unwrap();

        let dest = tmp.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        extract_archive(&archive, &dest, true, true).unwrap();
        assert!(dest.join("node.exe").is_file());
        assert!(dest.join("npm.cmd").is_file());
    }
}

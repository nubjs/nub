//! Deterministic representation of the staged/embedded runtime tree.

use std::path::Path;

fn walk<F>(root: &Path, dir: &Path, visit: &mut F) -> std::io::Result<()>
where
    F: FnMut(&Path, &Path, bool) -> std::io::Result<()>,
{
    let metadata = std::fs::symlink_metadata(dir)?;
    if unsupported_entry(&metadata) || !metadata.is_dir() {
        return Err(std::io::Error::other(format!(
            "unsupported runtime directory {}",
            dir.display()
        )));
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|_| std::io::Error::other("runtime entry escaped its root"))?;
        entries.push((encoded_relative_path(rel)?, entry));
    }
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (_, entry) in entries {
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if unsupported_entry(&meta) {
            return Err(std::io::Error::other(format!(
                "unsupported runtime entry {}",
                path.display()
            )));
        }
        if meta.is_dir() {
            visit(root, &path, true)?;
            walk(root, &path, visit)?;
        } else if meta.is_file() {
            visit(root, &path, false)?;
        } else {
            return Err(std::io::Error::other(format!(
                "unsupported runtime entry {}",
                path.display()
            )));
        }
    }
    Ok(())
}

/// `symlink_metadata` keeps this check at the entry itself rather than following
/// redirects. Windows junctions are directories with `FILE_ATTRIBUTE_REPARSE_POINT`,
/// so `FileType::is_symlink` alone would recurse through them.
fn unsupported_entry(meta: &std::fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x0400 != 0 {
            return true;
        }
    }
    !meta.is_dir() && !meta.is_file()
}

/// Encode a relative runtime path as UTF-8 components, not an OS display string.
/// The component count and byte lengths make `a/b` distinct from any filename that
/// merely contains a separator on another platform.
fn encoded_relative_path(rel: &Path) -> std::io::Result<Vec<u8>> {
    use std::path::Component;

    let components: Vec<_> = rel.components().collect();
    let mut encoded = Vec::new();
    encoded.extend_from_slice(&(components.len() as u64).to_le_bytes());
    for component in components {
        let Component::Normal(component) = component else {
            return Err(std::io::Error::other(
                "runtime entry has a non-normal path component",
            ));
        };
        let component = component
            .to_str()
            .ok_or_else(|| std::io::Error::other("runtime entry path is not UTF-8"))?
            .as_bytes();
        encoded.extend_from_slice(&(component.len() as u64).to_le_bytes());
        encoded.extend_from_slice(component);
    }
    Ok(encoded)
}

/// BLAKE3 over sorted relative path components and bytes, rejecting symlinks and
/// special files. The path encoding is platform-neutral so a build host and target
/// that stage the same tree produce the same digest.
pub(crate) fn tree_blake3(root: &Path) -> std::io::Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    walk(root, root, &mut |root, path, is_dir| {
        let rel = path
            .strip_prefix(root)
            .expect("runtime entry stays below root");
        let rel = encoded_relative_path(rel)?;
        if is_dir {
            hasher.update(b"D");
            hasher.update(&rel);
        } else {
            let bytes = std::fs::read(path)?;
            hasher.update(b"F");
            hasher.update(&rel);
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
        }
        Ok(())
    })?;
    Ok(hasher.finalize())
}

/// Tar a runtime tree with normalized metadata and lexical entry order.
// This shared source is included by both build.rs and the embed-runtime library.
// Only the build-script copy creates the archive; the library copy verifies it.
#[allow(dead_code)]
pub(crate) fn deterministic_tar(root: &Path) -> std::io::Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    builder.mode(tar::HeaderMode::Deterministic);
    builder.follow_symlinks(false);
    walk(root, root, &mut |root, path, is_dir| {
        let rel = path
            .strip_prefix(root)
            .expect("runtime entry stays below root");
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        if is_dir {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            header.set_cksum();
            builder.append_data(&mut header, rel, std::io::empty())?;
        } else {
            let bytes = std::fs::read(path)?;
            header.set_mode(0o644);
            header.set_size(bytes.len() as u64);
            header.set_cksum();
            builder.append_data(&mut header, rel, std::io::Cursor::new(bytes))?;
        }
        Ok(())
    })?;
    builder.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn fixture(root: &Path, reverse: bool, body: &[u8]) {
        let dirs = if reverse { ["z", "a"] } else { ["a", "z"] };
        for dir in dirs {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let files = if reverse {
            [
                ("z/second.cjs", b"second".as_slice()),
                ("a/first.mjs", body),
            ]
        } else {
            [
                ("a/first.mjs", body),
                ("z/second.cjs", b"second".as_slice()),
            ]
        };
        for (name, bytes) in files {
            let path = root.join(name);
            std::fs::write(&path, bytes).unwrap();
            // Windows `SetFileTime` needs FILE_WRITE_ATTRIBUTES, which the
            // GENERIC_READ handle from `File::open` does not carry; Unix
            // `futimens` accepts a read-only fd, which is why this only ever
            // failed on Windows.
            let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.set_times(std::fs::FileTimes::new().set_modified(
                SystemTime::UNIX_EPOCH + Duration::from_secs(if reverse { 123 } else { 456 }),
            ))
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if reverse { 0o700 } else { 0o600 };
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
            }
        }
    }

    #[test]
    fn runtime_tree_path_encoding_is_component_delimited_and_separator_neutral() {
        let rel = Path::new("addons").join("native").join("nub-native.node");
        let got = encoded_relative_path(&rel).unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(&3_u64.to_le_bytes());
        for component in ["addons", "native", "nub-native.node"] {
            expected.extend_from_slice(&(component.len() as u64).to_le_bytes());
            expected.extend_from_slice(component.as_bytes());
        }
        assert_eq!(got, expected);
        assert!(!got.contains(&b'/'));
        assert!(!got.contains(&b'\\'));
    }

    #[test]
    fn runtime_tree_traversal_orders_non_ascii_names_by_portable_encoding() {
        let base =
            std::env::temp_dir().join(format!("nub-runtime-tree-unicode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // These equal-length names sort differently as UTF-8 bytes and UTF-16 code
        // units: the explicit encoded-relative-path key pins UTF-8 order everywhere.
        let first = "\u{e000}x.mjs";
        let second = "\u{10000}.mjs";
        std::fs::write(base.join(second), b"second").unwrap();
        std::fs::write(base.join(first), b"first").unwrap();
        let mut seen = Vec::new();
        walk(&base, &base, &mut |root, path, _| {
            seen.push(
                path.strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, [first, second]);
        let digest = tree_blake3(&base).unwrap();
        let expected = blake3::hash(
            &[
                b"F".as_slice(),
                encoded_relative_path(Path::new(first)).unwrap().as_slice(),
                &(5_u64).to_le_bytes(),
                b"first",
                b"F",
                encoded_relative_path(Path::new(second)).unwrap().as_slice(),
                &(6_u64).to_le_bytes(),
                b"second",
            ]
            .concat(),
        );
        assert_eq!(
            digest, expected,
            "portable ordering must drive the tree digest"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn deterministic_runtime_tar_normalizes_tree_metadata_and_order() {
        let base = std::env::temp_dir().join(format!("nub-runtime-tar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let first = base.join("first");
        let second = base.join("second");
        fixture(&first, false, b"same");
        fixture(&second, true, b"same");
        assert_eq!(
            deterministic_tar(&first).unwrap(),
            deterministic_tar(&second).unwrap()
        );
        assert_eq!(tree_blake3(&first).unwrap(), tree_blake3(&second).unwrap());
        std::fs::write(second.join("a/first.mjs"), b"changed").unwrap();
        assert_ne!(
            deterministic_tar(&first).unwrap(),
            deterministic_tar(&second).unwrap()
        );
        assert_ne!(tree_blake3(&first).unwrap(), tree_blake3(&second).unwrap());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn tree_hash_rejects_symlink_and_special_entries() {
        let base =
            std::env::temp_dir().join(format!("nub-runtime-tree-special-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("real.mjs"), b"trusted").unwrap();

        std::os::unix::fs::symlink(base.join("real.mjs"), base.join("linked.mjs")).unwrap();
        assert!(
            tree_blake3(&base).is_err(),
            "a symlink must not be hashed as runtime content"
        );
        assert!(
            deterministic_tar(&base).is_err(),
            "a symlink must not enter the runtime archive"
        );
        std::fs::remove_file(base.join("linked.mjs")).unwrap();

        let root_link = base.with_extension("link");
        std::os::unix::fs::symlink(&base, &root_link).unwrap();
        assert!(
            tree_blake3(&root_link).is_err(),
            "a symlinked runtime root must be rejected"
        );
        assert!(
            deterministic_tar(&root_link).is_err(),
            "a symlinked runtime root must not be archived"
        );
        std::fs::remove_file(&root_link).unwrap();

        let fifo = base.join("runtime.fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(
            tree_blake3(&base).is_err(),
            "a Unix special file must be rejected"
        );
        assert!(
            deterministic_tar(&base).is_err(),
            "a Unix special file must not enter the runtime archive"
        );
        std::fs::remove_file(&fifo).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(windows)]
    #[test]
    fn tree_hash_rejects_directory_reparse_points_before_recursing() {
        use std::os::windows::fs::symlink_dir;
        use std::process::Command;

        let base =
            std::env::temp_dir().join(format!("nub-runtime-tree-reparse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let target = base.join("target");
        let reparse = base.join("linked-dir");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("nested.mjs"), b"trusted").unwrap();
        let junction = Command::new("cmd")
            .arg("/C")
            .arg(format!(
                "mklink /J \"{}\" \"{}\"",
                reparse.display(),
                target.display()
            ))
            .output()
            .unwrap();
        if !junction.status.success() {
            if let Err(error) = symlink_dir(&target, &reparse) {
                if error.raw_os_error() == Some(1314) {
                    eprintln!(
                        "skipping Windows runtime-tree reparse test: junction and symlink creation unavailable"
                    );
                    let _ = std::fs::remove_dir_all(&base);
                    return;
                }
                panic!("creating Windows runtime-tree reparse point: {error}");
            }
        }
        assert!(
            tree_blake3(&base).is_err(),
            "a directory reparse point must be rejected before recursion"
        );
        assert!(
            deterministic_tar(&base).is_err(),
            "a directory reparse point must not enter the runtime archive"
        );
        std::fs::remove_dir(&reparse).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }
}

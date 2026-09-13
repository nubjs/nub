use super::*;
use crate::policy::{CanonGlob, Effect, FsOrigin, FsRule, FsRuleSet};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};

fn rules(grants: &[(&str, FsAccess)]) -> FsRuleSet {
    FsRuleSet {
        entries: grants
            .iter()
            .map(|(path, access)| FsRule {
                matcher: CanonGlob((*path).into()),
                effect: Effect::Allow,
                access: *access,
                origin: FsOrigin::Authored,
            })
            .collect(),
        default_effect: Effect::Deny,
    }
}

fn projection(root: &Path, grants: &[(&str, FsAccess)]) -> Projection {
    projection_with_identity(root, grants, ProjectionIdentity::InNamespace)
}

fn projection_with_identity(
    root: &Path,
    grants: &[(&str, FsAccess)],
    identity: ProjectionIdentity,
) -> Projection {
    let root = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
        .open(root)
        .unwrap();
    Projection::new_with_identity(
        Rules::compile(&rules(grants)).unwrap(),
        Backing::new(root).unwrap(),
        identity,
    )
    .unwrap()
}

fn parent_identity() -> ProjectionIdentity {
    ProjectionIdentity::Parent {
        host_uid: unsafe { libc::getuid() },
        host_gid: unsafe { libc::getgid() },
    }
}

#[test]
fn native_export_requires_a_readonly_backing_view() {
    let root = tempfile::tempdir().unwrap();
    let fs = projection(root.path(), &[("/**", FsAccess::ReadWrite)]);
    assert_errno(fs.register_export(1), libc::EACCES);
    let open = || {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
            .open(root.path())
            .unwrap()
    };
    assert_errno(
        Projection::acquire_native(&rules(&[("/**", FsAccess::Read)]), open(), open()),
        libc::EROFS,
    );
}

#[test]
fn native_export_uses_the_exact_retained_open_handle() {
    use std::io::Read;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file"), b"original").unwrap();
    let fs = projection(root.path(), &[("/**", FsAccess::ReadWrite)]);
    let mut state = fs.state().unwrap();
    let ino = lookup(&mut state, ROOT, "file");
    let handle = state.open(ino, libc::O_RDONLY).unwrap();
    assert_eq!(
        state.handle(ino, handle).unwrap().authority,
        FsAccess::ReadWrite
    );
    let HandleKind::File { writable, .. } = &state.handle(ino, handle).unwrap().kind else {
        panic!("file")
    };
    assert!(!*writable);
    // Direct unit control of the slot state machine, not mounted enforcement.
    // Real registration requires the separate read-only view checked above.
    state.export = Some(ExportSlot {
        tid: 42,
        armed: true,
        file: None,
    });
    assert_errno(state.export_handle(41, ino, handle), libc::EACCES);
    assert_errno(state.export_handle(42, ROOT, handle), libc::EBADF);
    std::fs::rename(root.path().join("file"), root.path().join("moved")).unwrap();
    std::fs::write(root.path().join("file"), b"replacement").unwrap();
    state.export_handle(42, ino, handle).unwrap();
    assert_errno(state.export_handle(42, ino, handle), libc::EACCES);
    state.handles.remove(&handle);
    drop(state);
    let mut file = fs.take_export().unwrap();
    let mut bytes = String::new();
    file.read_to_string(&mut bytes).unwrap();
    assert_eq!(bytes, "original");
    assert_errno(fs.take_export(), libc::EIO);
    fs.unregister_export();
    assert!(fs.state().unwrap().export.is_none());
}

fn lookup(state: &mut State, parent: u64, name: &str) -> u64 {
    state.lookup(parent, OsStr::new(name)).unwrap().ino.0
}

fn assert_errno<T>(result: io::Result<T>, expected: i32) {
    match result {
        Err(err) => assert_eq!(err.raw_os_error(), Some(expected)),
        Ok(_) => panic!("expected errno {expected}"),
    }
}

#[test]
fn rename_rebase_keeps_a_leaf_at_its_exact_destination_spelling() {
    assert_eq!(
        rebase_path(
            Path::new("/source.json"),
            Path::new("/source.json"),
            Path::new("/destination.json"),
        ),
        Some(PathBuf::from("/destination.json"))
    );
    assert_eq!(
        rebase_path(
            Path::new("/source.dir/child.json"),
            Path::new("/source.dir"),
            Path::new("/destination.dir"),
        ),
        Some(PathBuf::from("/destination.dir/child.json"))
    );
}

#[test]
fn kernel_open_flags_preserve_read_and_write_authority() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("read-only"), b"original").unwrap();
    std::fs::create_dir(root.path().join("generated")).unwrap();
    let fs = projection(
        root.path(),
        &[
            ("/read-only", FsAccess::Read),
            ("/generated/*.json", FsAccess::ReadWrite),
        ],
    );
    let mut state = fs.state().unwrap();
    let ino = lookup(&mut state, ROOT, "read-only");
    let kernel_flags = KERNEL_O_LARGEFILE | KERNEL_FMODE_EXEC;
    assert_eq!(
        normalize_open_flags(kernel_flags).unwrap(),
        libc::O_LARGEFILE
    );
    let handle = state.open(ino, kernel_flags | libc::O_RDONLY).unwrap();
    assert_eq!(state.read(ino, handle, 0, 100).unwrap(), b"original");
    assert_errno(state.write(ino, handle, 0, b"no"), libc::EBADF);
    assert_errno(
        state.open(ino, KERNEL_O_LARGEFILE | libc::O_WRONLY),
        libc::EACCES,
    );
    for flags in [libc::O_WRONLY, libc::O_TRUNC, libc::O_CREAT, libc::O_PATH] {
        assert_errno(state.open(ino, kernel_flags | flags), libc::EOPNOTSUPP);
    }
    let dir = lookup(&mut state, ROOT, "generated");
    let (attr, created) = state
        .create(
            dir,
            OsStr::new("new.json"),
            0o600,
            0,
            KERNEL_O_LARGEFILE | libc::O_RDWR,
        )
        .unwrap();
    state.write(attr.ino.0, created, 0, b"created").unwrap();
    assert_eq!(
        std::fs::read(root.path().join("generated/new.json")).unwrap(),
        b"created"
    );
    assert_errno(
        state.create(dir, OsStr::new("exec.json"), 0o600, 0, kernel_flags),
        libc::EOPNOTSUPP,
    );
    assert!(!root.path().join("generated/exec.json").exists());
    assert_eq!(
        std::fs::read(root.path().join("read-only")).unwrap(),
        b"original"
    );
}

#[test]
fn future_create_reopen_truncate_and_host_writes_follow_fixed_pattern() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("generated")).unwrap();
    let fs = projection(root.path(), &[("/generated/*.json", FsAccess::ReadWrite)]);
    std::fs::write(root.path().join("generated/near.txt"), b"untouched").unwrap();
    let mut state = fs.state().unwrap();
    let dir = lookup(&mut state, ROOT, "generated");
    assert_errno(state.lookup(dir, OsStr::new("near.txt")), libc::ENOENT);
    assert_errno(
        state.create(dir, OsStr::new("near.txt"), 0o600, 0, libc::O_RDWR),
        libc::EACCES,
    );
    assert_errno(state.opendir(dir), libc::EACCES);
    let (attr, handle) = state
        .create(dir, OsStr::new("new.json"), 0o666, 0o027, libc::O_RDWR)
        .unwrap();
    assert_eq!(attr.perm & 0o027, 0);
    state.write(attr.ino.0, handle, 0, b"host-visible").unwrap();
    assert_eq!(
        state.read(attr.ino.0, handle, 0, 99).unwrap(),
        b"host-visible"
    );
    assert_eq!(
        std::fs::read(root.path().join("generated/new.json")).unwrap(),
        b"host-visible"
    );
    state.handles.remove(&handle);
    let reopened = state
        .open(attr.ino.0, libc::O_RDWR | libc::O_TRUNC)
        .unwrap();
    state.write(attr.ino.0, reopened, 3, b"!").unwrap();
    assert_eq!(
        std::fs::read(root.path().join("generated/new.json")).unwrap(),
        b"\0\0\0!"
    );
    state.truncate(attr.ino.0, Some(reopened), 2).unwrap();
    assert_eq!(
        std::fs::read(root.path().join("generated/new.json")).unwrap(),
        b"\0\0"
    );
    assert_eq!(
        std::fs::read(root.path().join("generated/near.txt")).unwrap(),
        b"untouched"
    );
}

#[test]
fn matching_hardlinks_share_content_without_admitting_an_omitted_name() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("allowed"), b"before").unwrap();
    std::fs::hard_link(root.path().join("allowed"), root.path().join("omitted")).unwrap();
    std::fs::hard_link(
        root.path().join("allowed"),
        root.path().join("also-allowed"),
    )
    .unwrap();
    let fs = projection(
        root.path(),
        &[
            ("/allowed", FsAccess::ReadWrite),
            ("/also-allowed", FsAccess::Read),
        ],
    );
    let mut state = fs.state().unwrap();
    let first = lookup(&mut state, ROOT, "allowed");
    let second = lookup(&mut state, ROOT, "also-allowed");
    assert_ne!(first, second);
    assert_errno(state.lookup(ROOT, OsStr::new("omitted")), libc::ENOENT);
    let handle = state.open(first, libc::O_RDWR).unwrap();
    state.write(first, handle, 0, b"after!").unwrap();
    assert_eq!(
        std::fs::read(root.path().join("omitted")).unwrap(),
        b"after!"
    );
    let other = state.open(second, libc::O_RDONLY).unwrap();
    assert_eq!(state.read(second, other, 0, 100).unwrap(), b"after!");
    assert_errno(state.open(second, libc::O_WRONLY), libc::EACCES);
    assert_errno(state.write(second, other, 0, b"no"), libc::EBADF);
    assert_errno(state.read(second, handle, 0, 1), libc::EBADF);
}

#[test]
fn retained_handle_survives_rename_but_fresh_open_does_not_follow_identity() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("allowed"), b"original").unwrap();
    let fs = projection(root.path(), &[("/allowed", FsAccess::ReadWrite)]);
    let mut state = fs.state().unwrap();
    let ino = lookup(&mut state, ROOT, "allowed");
    let old = state.open(ino, libc::O_RDWR).unwrap();
    std::fs::rename(root.path().join("allowed"), root.path().join("omitted")).unwrap();
    assert_errno(state.open(ino, libc::O_RDONLY), libc::ENOENT);
    assert_errno(state.lookup(ROOT, OsStr::new("omitted")), libc::ENOENT);
    std::fs::write(root.path().join("allowed"), b"replacement").unwrap();
    assert_errno(
        state.open(ino, libc::O_WRONLY | libc::O_TRUNC),
        libc::ESTALE,
    );
    assert_eq!(
        std::fs::read(root.path().join("allowed")).unwrap(),
        b"replacement"
    );
    let replacement = lookup(&mut state, ROOT, "allowed");
    assert_ne!(ino, replacement);
    state.forget(ino, 1);
    state.write(ino, old, 0, b"retained").unwrap();
    assert_eq!(state.read(ino, old, 0, 100).unwrap(), b"retained");
    assert_eq!(
        std::fs::read(root.path().join("omitted")).unwrap(),
        b"retained"
    );
    assert_eq!(state.attr(ino, Some(old)).unwrap().size, 8);
    assert_eq!(state.paths.get(Path::new("/allowed")), Some(&replacement));
}

#[test]
fn backing_symlinks_are_pinned_not_followed_and_special_files_are_not_opened() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("real")).unwrap();
    std::fs::write(root.path().join("real/value"), b"data").unwrap();
    symlink("real", root.path().join("redirect")).unwrap();
    symlink("/etc/passwd", root.path().join("absolute")).unwrap();
    let fs = projection(root.path(), &[("/**", FsAccess::ReadWrite)]);
    let mut state = fs.state().unwrap();
    let link = lookup(&mut state, ROOT, "absolute");
    let node = state.node(link).unwrap();
    assert_eq!(read_link(&node.pin).unwrap(), b"/etc/passwd");
    assert_errno(state.open(link, libc::O_RDONLY), libc::EACCES);
    assert_errno(state.backing.pin(Path::new("/redirect/value")), libc::ELOOP);
    assert_errno(state.backing.pin(Path::new("/../outside")), libc::EINVAL);
    assert_errno(
        child_path(Path::new("/"), OsStr::new("../outside")),
        libc::EINVAL,
    );
    assert_errno(child_path(Path::new("/"), OsStr::new("..")), libc::EINVAL);
    // Lookup only pins a FIFO. It never performs a potentially blocking/activating
    // data open, even with a broad authored grant.
    let fifo =
        std::ffi::CString::new(root.path().join("fifo").as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert_errno(state.lookup(ROOT, OsStr::new("fifo")), libc::EACCES);
}

#[test]
fn directory_stream_is_filtered_stable_and_byte_preserving() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("data")).unwrap();
    let unusual = OsString::from_vec(b"raw-\xff.json".to_vec());
    for name in [OsStr::new("yes.json"), OsStr::new("no.txt"), &unusual] {
        std::fs::write(root.path().join("data").join(name), b"x").unwrap();
    }
    let fs = projection(
        root.path(),
        &[("/data", FsAccess::Read), ("/data/*.json", FsAccess::Read)],
    );
    let mut state = fs.state().unwrap();
    let dir = lookup(&mut state, ROOT, "data");
    let stream = state.opendir(dir).unwrap();
    std::fs::write(root.path().join("data/later.json"), b"future").unwrap();
    let HandleKind::Directory(names) = &state.handle(dir, stream).unwrap().kind else {
        panic!("directory handle")
    };
    let names: Vec<_> = names.iter().map(|(name, _)| name.clone()).collect();
    assert!(names.contains(&unusual));
    assert!(names.contains(&OsString::from("yes.json")));
    assert!(!names.contains(&OsString::from("no.txt")));
    assert!(!names.contains(&OsString::from("later.json")));
    lookup(&mut state, dir, "later.json");
}

#[test]
fn descriptors_are_cloexec_and_forget_does_not_leak_inodes() {
    let _serve: fn(Projection, std::os::fd::OwnedFd) -> io::Result<()> = Projection::serve;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file"), b"x").unwrap();
    let fs = projection(root.path(), &[("/**", FsAccess::ReadWrite)]);
    let mut state = fs.state().unwrap();
    for _ in 0..100 {
        let ino = lookup(&mut state, ROOT, "file");
        let node = state.node(ino).unwrap();
        assert_ne!(
            unsafe { libc::fcntl(node.pin.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let handle = state.open(ino, libc::O_RDONLY).unwrap();
        let HandleKind::File { file, .. } = &state.handle(ino, handle).unwrap().kind else {
            panic!("file handle")
        };
        assert_ne!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        state.forget(ino, 1);
        assert_eq!(state.read(ino, handle, 0, 1).unwrap(), b"x");
        state.handles.remove(&handle);
    }
    assert_eq!(state.nodes.len(), 1);
    assert_eq!(state.paths.len(), 1);
    assert!(state.handles.is_empty());
}

#[test]
fn append_preserves_descriptor_access_mode() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file"), b"start").unwrap();
    let fs = projection(root.path(), &[("/file", FsAccess::ReadWrite)]);
    let mut state = fs.state().unwrap();
    let ino = lookup(&mut state, ROOT, "file");
    assert_errno(
        state.open(ino, libc::O_TMPFILE | libc::O_RDWR),
        libc::EOPNOTSUPP,
    );
    let handle = state.open(ino, libc::O_WRONLY | libc::O_APPEND).unwrap();
    state.write(ino, handle, 0, b"+append").unwrap();
    assert_eq!(
        std::fs::read(root.path().join("file")).unwrap(),
        b"start+append"
    );
    assert_errno(state.read(ino, handle, 0, 1), libc::EBADF);
    assert_errno(
        state.write(ino, handle, i64::MAX as u64, b"x"),
        libc::EINVAL,
    );
}

#[test]
fn readonly_truncate_requires_write_authority_but_returns_a_readonly_handle() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("file");
    std::fs::write(&file, b"canary").unwrap();
    let read = projection(root.path(), &[("/file", FsAccess::Read)]);
    let mut state = read.state().unwrap();
    let ino = lookup(&mut state, ROOT, "file");
    assert_errno(
        state.open(ino, libc::O_RDONLY | libc::O_TRUNC),
        libc::EACCES,
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"canary");
    drop(state);

    let write = projection(root.path(), &[("/file", FsAccess::ReadWrite)]);
    let mut state = write.state().unwrap();
    let ino = lookup(&mut state, ROOT, "file");
    let handle = state.open(ino, libc::O_RDONLY | libc::O_TRUNC).unwrap();
    assert!(std::fs::read(&file).unwrap().is_empty());
    assert!(state.read(ino, handle, 0, 1).unwrap().is_empty());
    assert_errno(state.write(ino, handle, 0, b"x"), libc::EBADF);
    let HandleKind::File { file, writable } = &state.handle(ino, handle).unwrap().kind else {
        panic!("regular file expected")
    };
    assert!(!writable);
    assert_eq!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) } & libc::O_ACCMODE,
        libc::O_RDONLY
    );
}

#[test]
fn setattr_preserves_rw_handle_identity_and_refuses_read_policy() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("rw"), b"rw").unwrap();
    std::fs::write(root.path().join("locked"), b"locked").unwrap();
    std::fs::create_dir(root.path().join("dir")).unwrap();
    let fs = projection(
        root.path(),
        &[
            ("/rw", FsAccess::ReadWrite),
            ("/dir", FsAccess::ReadWrite),
            ("/locked", FsAccess::Read),
        ],
    );
    let mut state = fs.state().unwrap();
    let rw = lookup(&mut state, ROOT, "rw");
    let dir = lookup(&mut state, ROOT, "dir");
    let locked = lookup(&mut state, ROOT, "locked");
    let atime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_001);

    // Fresh operations use the current authorized spelling and preserve the
    // actor's normal ownership checks instead of accepting an O_PATH fd.
    state
        .setattr(
            rw,
            Some(0o600),
            Some(unsafe { libc::getuid() }),
            Some(unsafe { libc::getgid() }),
            None,
            Some(TimeOrNow::SpecificTime(atime)),
            Some(TimeOrNow::SpecificTime(mtime)),
            None,
        )
        .unwrap();
    let rw_meta = std::fs::metadata(root.path().join("rw")).unwrap();
    assert_eq!(rw_meta.mode() & 0o7777, 0o600);
    assert_eq!(rw_meta.atime(), 1_700_000_000);
    assert_eq!(rw_meta.mtime(), 1_700_000_001);

    state
        .setattr(
            dir,
            Some(0o700),
            None,
            None,
            None,
            None,
            Some(TimeOrNow::SpecificTime(mtime)),
            None,
        )
        .unwrap();
    let dir_meta = std::fs::metadata(root.path().join("dir")).unwrap();
    assert_eq!(dir_meta.mode() & 0o7777, 0o700);
    assert_eq!(dir_meta.mtime(), 1_700_000_001);

    // An RW-authorized O_RDONLY file handle still names the original object.
    let file_handle = state.open(rw, libc::O_RDONLY).unwrap();
    std::fs::rename(root.path().join("rw"), root.path().join("moved-rw")).unwrap();
    std::fs::write(root.path().join("rw"), b"replacement").unwrap();
    let replacement_mode = std::fs::metadata(root.path().join("rw")).unwrap().mode() & 0o7777;
    assert_errno(
        state.setattr(rw, Some(0o640), None, None, None, None, None, None),
        libc::ESTALE,
    );
    state
        .setattr(
            rw,
            Some(0o640),
            None,
            None,
            None,
            None,
            None,
            Some(file_handle),
        )
        .unwrap();
    assert_eq!(
        std::fs::metadata(root.path().join("moved-rw"))
            .unwrap()
            .mode()
            & 0o7777,
        0o640
    );
    assert_eq!(
        std::fs::metadata(root.path().join("rw")).unwrap().mode() & 0o7777,
        replacement_mode
    );

    // Directory handles retain only an O_PATH pin, so this exercises the
    // trusted proc-fd metadata reopen after the original spelling is gone.
    let directory_handle = state.opendir(dir).unwrap();
    std::fs::rename(root.path().join("dir"), root.path().join("moved-dir")).unwrap();
    std::fs::create_dir(root.path().join("dir")).unwrap();
    let replacement_dir_mode = std::fs::metadata(root.path().join("dir")).unwrap().mode() & 0o7777;
    state
        .setattr(
            dir,
            Some(0o750),
            None,
            None,
            None,
            None,
            Some(TimeOrNow::SpecificTime(atime)),
            Some(directory_handle),
        )
        .unwrap();
    let moved_dir = std::fs::metadata(root.path().join("moved-dir")).unwrap();
    assert_eq!(moved_dir.mode() & 0o7777, 0o750);
    assert_eq!(moved_dir.mtime(), 1_700_000_000);
    assert_eq!(
        std::fs::metadata(root.path().join("dir")).unwrap().mode() & 0o7777,
        replacement_dir_mode
    );

    assert_errno(
        state.setattr(locked, Some(0o600), None, None, None, None, None, None),
        libc::EACCES,
    );
    let locked_handle = state.open(locked, libc::O_RDONLY).unwrap();
    assert_errno(
        state.setattr(
            locked,
            None,
            None,
            None,
            None,
            Some(TimeOrNow::Now),
            None,
            Some(locked_handle),
        ),
        libc::EACCES,
    );
}

#[test]
fn setattr_mode_zero_uses_empty_path_metadata_operations() {
    let root = tempfile::tempdir().unwrap();
    let regular = root.path().join("mode-zero");
    let directory = root.path().join("mode-zero-dir");
    let locked = root.path().join("mode-zero-locked");
    std::fs::write(&regular, b"regular").unwrap();
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(&locked, b"locked").unwrap();
    let fs = projection(
        root.path(),
        &[
            ("/mode-zero", FsAccess::ReadWrite),
            ("/mode-zero-dir", FsAccess::ReadWrite),
            ("/mode-zero-locked", FsAccess::Read),
        ],
    );
    let mut state = fs.state().unwrap();
    let regular_ino = lookup(&mut state, ROOT, "mode-zero");
    let directory_ino = lookup(&mut state, ROOT, "mode-zero-dir");
    let locked_ino = lookup(&mut state, ROOT, "mode-zero-locked");
    let timestamp = UNIX_EPOCH + Duration::from_secs(1_700_000_003);
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    for path in [&regular, &directory, &locked] {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0);
        std::fs::set_permissions(path, permissions).unwrap();
    }
    // Exercise timestamps while read permission is still absent, independently
    // of the combined setattr below which restores a readable mode first.
    for ino in [regular_ino, directory_ino] {
        state
            .setattr(
                ino,
                None,
                None,
                None,
                None,
                None,
                Some(TimeOrNow::SpecificTime(timestamp)),
                None,
            )
            .unwrap();
    }

    state
        .setattr(
            regular_ino,
            Some(0o600),
            Some(uid),
            Some(gid),
            None,
            None,
            Some(TimeOrNow::SpecificTime(timestamp)),
            None,
        )
        .unwrap();
    let regular_meta = std::fs::metadata(&regular).unwrap();
    assert_eq!(regular_meta.mode() & 0o7777, 0o600);
    assert_eq!((regular_meta.uid(), regular_meta.gid()), (uid, gid));
    assert_eq!(regular_meta.mtime(), 1_700_000_003);

    state
        .setattr(
            directory_ino,
            Some(0o700),
            Some(uid),
            Some(gid),
            None,
            None,
            Some(TimeOrNow::SpecificTime(timestamp)),
            None,
        )
        .unwrap();
    let directory_meta = std::fs::metadata(&directory).unwrap();
    assert_eq!(directory_meta.mode() & 0o7777, 0o700);
    assert_eq!((directory_meta.uid(), directory_meta.gid()), (uid, gid));
    assert_eq!(directory_meta.mtime(), 1_700_000_003);

    let locked_before = std::fs::metadata(&locked).unwrap();
    assert_errno(
        state.setattr(
            locked_ino,
            Some(0o600),
            Some(uid),
            Some(gid),
            None,
            None,
            Some(TimeOrNow::SpecificTime(timestamp)),
            None,
        ),
        libc::EACCES,
    );
    let locked_after = std::fs::metadata(&locked).unwrap();
    assert_eq!(locked_after.mode(), locked_before.mode());
    assert_eq!(locked_after.uid(), locked_before.uid());
    assert_eq!(locked_after.gid(), locked_before.gid());
    assert_eq!(locked_after.mtime(), locked_before.mtime());
}

#[test]
fn setattr_timestamp_conversion_preserves_pre_epoch_values() {
    let [atime, mtime] = requested_times(
        Some(TimeOrNow::SpecificTime(
            UNIX_EPOCH - Duration::from_nanos(500_000_000),
        )),
        Some(TimeOrNow::Now),
    )
    .unwrap();
    assert_eq!((atime.tv_sec, atime.tv_nsec), (-1, 500_000_000));
    assert_eq!(mtime.tv_nsec, libc::UTIME_NOW);
}

#[test]
fn parent_identity_maps_every_attribute_producer_to_protocol_zero() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("existing"), b"existing").unwrap();
    let fs = projection_with_identity(
        root.path(),
        &[("/**", FsAccess::ReadWrite)],
        parent_identity(),
    );
    let mut state = fs.state().unwrap();

    let existing = state.lookup(ROOT, OsStr::new("existing")).unwrap();
    assert_eq!((existing.uid, existing.gid), (0, 0));
    let existing_ino = existing.ino.0;
    assert_eq!(state.attr(existing_ino, None).unwrap().uid, 0);
    let existing_handle = state.open(existing_ino, libc::O_RDONLY).unwrap();
    assert_eq!(
        state.attr(existing_ino, Some(existing_handle)).unwrap().gid,
        0
    );

    let (created, _) = state
        .create(ROOT, OsStr::new("created"), 0o600, 0, libc::O_RDWR)
        .unwrap();
    assert_eq!((created.uid, created.gid), (0, 0));
    let directory = state
        .mkdir(ROOT, OsStr::new("directory"), 0o700, 0)
        .unwrap();
    assert_eq!((directory.uid, directory.gid), (0, 0));
    let link = state
        .symlink(ROOT, OsStr::new("link"), Path::new("existing"))
        .unwrap();
    assert_eq!((link.uid, link.gid), (0, 0));
    let hardlink = state
        .link(existing_ino, ROOT, OsStr::new("hardlink"))
        .unwrap();
    assert_eq!((hardlink.uid, hardlink.gid), (0, 0));
}

#[test]
fn parent_identity_returns_an_unmapped_protocol_identity_for_system_files() {
    // This deliberately uses an identity different from the process so a
    // normal host-owned system file exercises the one-ID-map overflow path.
    // FUSE accepts the reply; Linux's `make_kuid` makes it invalid in the
    // direct map and stat-like callers subsequently receive their configured
    // overflow identity rather than an EOVERFLOW response.
    let fs = projection_with_identity(
        Path::new("/"),
        &[("/usr/bin/env", FsAccess::Read)],
        ProjectionIdentity::Parent {
            host_uid: u32::MAX - 1,
            host_gid: u32::MAX - 1,
        },
    );
    let mut state = fs.state().unwrap();
    let usr = lookup(&mut state, ROOT, "usr");
    let bin = lookup(&mut state, usr, "bin");
    let env = state.lookup(bin, OsStr::new("env")).unwrap();
    assert_eq!(
        (env.uid, env.gid),
        (
            ProjectionIdentity::UNMAPPED_ID,
            ProjectionIdentity::UNMAPPED_ID
        )
    );
    let handle = state.open(env.ino.0, libc::O_RDONLY).unwrap();
    assert!(!state.read(env.ino.0, handle, 0, 1).unwrap().is_empty());
}

#[test]
fn parent_identity_translates_only_zero_owner_requests_before_mutation() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("file");
    std::fs::write(&file, b"data").unwrap();
    let fs = projection_with_identity(
        root.path(),
        &[("/file", FsAccess::ReadWrite)],
        parent_identity(),
    );
    let mut state = fs.state().unwrap();
    let ino = lookup(&mut state, ROOT, "file");
    let host_uid = unsafe { libc::getuid() };
    let host_gid = unsafe { libc::getgid() };

    state
        .setattr(ino, None, Some(0), None, None, None, None, None)
        .unwrap();
    assert_eq!(std::fs::metadata(&file).unwrap().uid(), host_uid);
    state
        .setattr(ino, None, None, Some(0), None, None, None, None)
        .unwrap();
    assert_eq!(std::fs::metadata(&file).unwrap().gid(), host_gid);

    let before = std::fs::metadata(&file).unwrap();
    assert_errno(
        state.setattr(
            ino,
            Some(0o600),
            Some(1),
            Some(0),
            None,
            None,
            None,
            None,
        ),
        libc::EINVAL,
    );
    let after_uid = std::fs::metadata(&file).unwrap();
    assert_eq!(after_uid.mode(), before.mode());
    assert_eq!(
        (after_uid.uid(), after_uid.gid()),
        (before.uid(), before.gid())
    );

    assert_errno(
        state.setattr(
            ino,
            Some(0o640),
            Some(0),
            Some(1),
            None,
            None,
            None,
            None,
        ),
        libc::EINVAL,
    );
    let after_gid = std::fs::metadata(&file).unwrap();
    assert_eq!(after_gid.mode(), before.mode());
    assert_eq!(
        (after_gid.uid(), after_gid.gid()),
        (before.uid(), before.gid())
    );
}

#[test]
fn in_namespace_identity_preserves_legacy_raw_owner_behavior() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("file");
    std::fs::write(&file, b"data").unwrap();
    let fs = projection(root.path(), &[("/file", FsAccess::ReadWrite)]);
    let mut state = fs.state().unwrap();
    let ino = state.lookup(ROOT, OsStr::new("file")).unwrap().ino.0;
    let metadata = std::fs::metadata(&file).unwrap();
    let attr = state.attr(ino, None).unwrap();
    assert_eq!((attr.uid, attr.gid), (metadata.uid(), metadata.gid()));

    state
        .setattr(
            ino,
            None,
            Some(metadata.uid()),
            Some(metadata.gid()),
            None,
            None,
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        ProjectionIdentity::InNamespace
            .incoming_owner(Some(42), Some(43))
            .unwrap(),
        (Some(42), Some(43))
    );
}

#[test]
fn setattr_updates_symlink_owner_and_times_without_following_target() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("target"), b"target").unwrap();
    symlink("target", root.path().join("link")).unwrap();
    let fs = projection(root.path(), &[("/link", FsAccess::ReadWrite)]);
    let mut state = fs.state().unwrap();
    let link = lookup(&mut state, ROOT, "link");
    let time = UNIX_EPOCH + Duration::from_secs(1_700_000_002);

    state
        .setattr(
            link,
            None,
            Some(unsafe { libc::getuid() }),
            Some(unsafe { libc::getgid() }),
            None,
            None,
            Some(TimeOrNow::SpecificTime(time)),
            None,
        )
        .unwrap();
    assert_eq!(
        std::fs::symlink_metadata(root.path().join("link"))
            .unwrap()
            .mtime(),
        1_700_000_002
    );
    assert_ne!(
        std::fs::metadata(root.path().join("target"))
            .unwrap()
            .mtime(),
        1_700_000_002
    );
    assert_errno(
        state.setattr(link, Some(0o600), None, None, None, None, None, None),
        libc::EOPNOTSUPP,
    );
}

#[test]
fn namespace_mutations_follow_matching_names_and_kernel_results() {
    let root = tempfile::tempdir().unwrap();
    let generated = root.path().join("generated");
    std::fs::create_dir(&generated).unwrap();
    std::fs::write(generated.join("source.json"), b"source").unwrap();
    std::fs::write(generated.join("near.txt"), b"near").unwrap();
    let fs = projection(
        root.path(),
        &[
            ("/generated/*.json", FsAccess::ReadWrite),
            ("/generated/*.dir", FsAccess::ReadWrite),
        ],
    );
    let mut state = fs.state().unwrap();
    let parent = lookup(&mut state, ROOT, "generated");

    let directory = state
        .mkdir(parent, OsStr::new("made.dir"), 0o777, 0o027)
        .unwrap();
    assert_eq!(directory.kind, FileType::Directory);
    assert_eq!(directory.perm & 0o027, 0);
    assert_errno(
        state.mkdir(parent, OsStr::new("near.txt"), 0o700, 0),
        libc::EACCES,
    );

    let link = state
        .symlink(parent, OsStr::new("link.json"), Path::new("../outside"))
        .unwrap();
    assert_eq!(link.kind, FileType::Symlink);
    let link_ino = lookup(&mut state, parent, "link.json");
    let link_node = state.node(link_ino).unwrap();
    assert_eq!(read_link(&link_node.pin).unwrap(), b"../outside");
    let source = lookup(&mut state, parent, "source.json");
    let hardlink = state
        .link(source, parent, OsStr::new("linked.json"))
        .unwrap();
    assert_eq!(hardlink.kind, FileType::RegularFile);
    assert_eq!(
        std::fs::read(generated.join("linked.json")).unwrap(),
        b"source"
    );
    assert_errno(
        state.link(source, parent, OsStr::new("linked.txt")),
        libc::EACCES,
    );
    assert_errno(
        state.unlink(parent, OsStr::new("made.dir"), false),
        libc::EISDIR,
    );
    assert_errno(
        state.unlink(parent, OsStr::new("linked.json"), true),
        libc::ENOTDIR,
    );

    state
        .rename(
            parent,
            OsStr::new("source.json"),
            parent,
            OsStr::new("moved.json"),
            0,
        )
        .unwrap();
    assert!(!generated.join("source.json").exists());
    assert_eq!(
        std::fs::read(generated.join("moved.json")).unwrap(),
        b"source"
    );
    assert_errno(
        state.rename(
            parent,
            OsStr::new("moved.json"),
            parent,
            OsStr::new("linked.json"),
            libc::RENAME_NOREPLACE,
        ),
        libc::EEXIST,
    );
    state
        .rename(
            parent,
            OsStr::new("moved.json"),
            parent,
            OsStr::new("linked.json"),
            libc::RENAME_EXCHANGE,
        )
        .unwrap();
    assert_errno(
        state.rename(
            parent,
            OsStr::new("moved.json"),
            parent,
            OsStr::new("linked.json"),
            libc::RENAME_NOREPLACE | libc::RENAME_EXCHANGE,
        ),
        libc::EINVAL,
    );
    assert_errno(
        state.rename(
            parent,
            OsStr::new("moved.json"),
            parent,
            OsStr::new("linked.json"),
            libc::RENAME_WHITEOUT,
        ),
        libc::EINVAL,
    );
    assert_errno(
        state.rename(
            parent,
            OsStr::new("linked.json"),
            parent,
            OsStr::new("near.txt"),
            0,
        ),
        libc::EACCES,
    );

    state
        .unlink(parent, OsStr::new("link.json"), false)
        .unwrap();
    state.unlink(parent, OsStr::new("made.dir"), true).unwrap();
    assert!(!generated.join("link.json").exists());
    assert!(!generated.join("made.dir").exists());
    assert_eq!(std::fs::read(generated.join("near.txt")).unwrap(), b"near");
}

#[test]
fn namespace_mutations_keep_handles_but_fresh_names_are_rechecked() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("allowed.json"), b"original").unwrap();
    let fs = projection(root.path(), &[("/*.json", FsAccess::ReadWrite)]);
    let mut state = fs.state().unwrap();
    let ino = lookup(&mut state, ROOT, "allowed.json");
    let handle = state.open(ino, libc::O_RDWR).unwrap();

    state
        .rename(
            ROOT,
            OsStr::new("allowed.json"),
            ROOT,
            OsStr::new("moved.json"),
            0,
        )
        .unwrap();
    // A FUSE rename changes the name bound to this cached inode. Fresh opens
    // re-authorize the destination spelling while the original handle keeps
    // its already-acquired rights.
    let reopened = state.open(ino, libc::O_RDONLY).unwrap();
    let moved = lookup(&mut state, ROOT, "moved.json");
    assert_eq!(ino, moved);
    assert_eq!(state.read(ino, reopened, 0, 100).unwrap(), b"original");
    state.write(ino, handle, 0, b"retained").unwrap();
    assert_eq!(
        std::fs::read(root.path().join("moved.json")).unwrap(),
        b"retained"
    );

    state.unlink(ROOT, OsStr::new("moved.json"), false).unwrap();
    assert_errno(state.lookup(ROOT, OsStr::new("moved.json")), libc::ENOENT);
    assert_eq!(state.read(ino, handle, 0, 100).unwrap(), b"retained");
    state.write(ino, handle, 0, b"unlinked").unwrap();
    assert_eq!(state.read(ino, handle, 0, 100).unwrap(), b"unlinked");
}

#[test]
fn directory_rename_rebases_cached_descendants_without_reusing_replaced_inodes() {
    let root = tempfile::tempdir().unwrap();
    let source_path = root.path().join("directory-source.dir");
    let destination_path = root.path().join("directory-renamed.dir");
    std::fs::create_dir(&source_path).unwrap();
    std::fs::write(source_path.join("fresh.json"), b"fresh").unwrap();
    std::fs::write(source_path.join("retained.json"), b"retained").unwrap();
    let fs = projection(
        root.path(),
        &[
            ("/directory-source.dir", FsAccess::ReadWrite),
            ("/directory-source.dir/*.json", FsAccess::ReadWrite),
            ("/directory-renamed.dir", FsAccess::ReadWrite),
            ("/directory-renamed.dir/*.json", FsAccess::Read),
        ],
    );
    let mut state = fs.state().unwrap();
    let source = lookup(&mut state, ROOT, "directory-source.dir");
    let fresh = lookup(&mut state, source, "fresh.json");
    let retained = lookup(&mut state, source, "retained.json");
    let handle = state.open(retained, libc::O_RDWR).unwrap();

    state
        .rename(
            ROOT,
            OsStr::new("directory-source.dir"),
            ROOT,
            OsStr::new("directory-renamed.dir"),
            0,
        )
        .unwrap();
    assert_errno(
        state.lookup(ROOT, OsStr::new("directory-source.dir")),
        libc::ENOENT,
    );
    assert_eq!(lookup(&mut state, ROOT, "directory-renamed.dir"), source);
    let fresh_handle = state.open(fresh, libc::O_RDONLY).unwrap();
    assert_eq!(state.read(fresh, fresh_handle, 0, 100).unwrap(), b"fresh");
    assert_errno(state.open(fresh, libc::O_WRONLY), libc::EACCES);
    state.write(retained, handle, 0, b"retained-held").unwrap();
    assert_eq!(
        std::fs::read(destination_path.join("retained.json")).unwrap(),
        b"retained-held"
    );

    std::fs::rename(&destination_path, root.path().join("moved-aside")).unwrap();
    std::fs::create_dir(&destination_path).unwrap();
    std::fs::write(destination_path.join("fresh.json"), b"replacement").unwrap();
    assert_errno(state.open(fresh, libc::O_RDONLY), libc::ESTALE);
}

#[test]
fn rename_overwrite_keeps_the_displaced_inode_stale() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("source.json"), b"source").unwrap();
    std::fs::write(root.path().join("destination.json"), b"destination").unwrap();
    let fs = projection(
        root.path(),
        &[
            ("/source.json", FsAccess::ReadWrite),
            ("/destination.json", FsAccess::ReadWrite),
        ],
    );
    let mut state = fs.state().unwrap();
    let source = lookup(&mut state, ROOT, "source.json");
    let destination = lookup(&mut state, ROOT, "destination.json");

    state
        .rename(
            ROOT,
            OsStr::new("source.json"),
            ROOT,
            OsStr::new("destination.json"),
            0,
        )
        .unwrap();
    assert_errno(state.open(destination, libc::O_RDONLY), libc::ESTALE);
    assert_eq!(lookup(&mut state, ROOT, "destination.json"), source);
    let source_handle = state.open(source, libc::O_RDONLY).unwrap();
    assert_eq!(
        state.read(source, source_handle, 0, 100).unwrap(),
        b"source"
    );
}

#[test]
fn hardlink_rename_and_exchange_keep_separate_path_identities() {
    let root = tempfile::tempdir().unwrap();
    let left_path = root.path().join("left.json");
    let right_path = root.path().join("right.json");
    std::fs::write(&left_path, b"shared").unwrap();
    std::fs::hard_link(&left_path, &right_path).unwrap();
    let fs = projection(
        root.path(),
        &[
            ("/left.json", FsAccess::ReadWrite),
            ("/right.json", FsAccess::ReadWrite),
        ],
    );
    let mut state = fs.state().unwrap();
    let left = lookup(&mut state, ROOT, "left.json");
    let right = lookup(&mut state, ROOT, "right.json");
    assert_ne!(left, right);

    for flags in [0, libc::RENAME_EXCHANGE] {
        state
            .rename(
                ROOT,
                OsStr::new("left.json"),
                ROOT,
                OsStr::new("right.json"),
                flags,
            )
            .unwrap();
        assert_eq!(lookup(&mut state, ROOT, "left.json"), left);
        assert_eq!(lookup(&mut state, ROOT, "right.json"), right);
    }
}

#[test]
fn host_replacement_before_rename_cannot_be_rebound_to_the_cached_source_inode() {
    let root = tempfile::tempdir().unwrap();
    let source_path = root.path().join("source.json");
    std::fs::write(&source_path, b"original").unwrap();
    let fs = projection(
        root.path(),
        &[
            ("/source.json", FsAccess::ReadWrite),
            ("/destination.json", FsAccess::ReadWrite),
        ],
    );
    let mut state = fs.state().unwrap();
    let source = lookup(&mut state, ROOT, "source.json");
    std::fs::rename(&source_path, root.path().join("original-aside")).unwrap();
    std::fs::write(&source_path, b"replacement").unwrap();

    state
        .rename(
            ROOT,
            OsStr::new("source.json"),
            ROOT,
            OsStr::new("destination.json"),
            0,
        )
        .unwrap();
    assert_errno(state.open(source, libc::O_RDONLY), libc::ESTALE);
    let destination = lookup(&mut state, ROOT, "destination.json");
    assert_ne!(destination, source);
    let handle = state.open(destination, libc::O_RDONLY).unwrap();
    assert_eq!(
        state.read(destination, handle, 0, 100).unwrap(),
        b"replacement"
    );
}

#[test]
fn rename_exchange_rebinds_each_cached_path_and_preserves_handles() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("left.json"), b"left").unwrap();
    std::fs::write(root.path().join("right.json"), b"right").unwrap();
    let fs = projection(
        root.path(),
        &[
            ("/left.json", FsAccess::ReadWrite),
            ("/right.json", FsAccess::ReadWrite),
        ],
    );
    let mut state = fs.state().unwrap();
    let left = lookup(&mut state, ROOT, "left.json");
    let right = lookup(&mut state, ROOT, "right.json");
    let held_left = state.open(left, libc::O_RDONLY).unwrap();

    state
        .rename(
            ROOT,
            OsStr::new("left.json"),
            ROOT,
            OsStr::new("right.json"),
            libc::RENAME_EXCHANGE,
        )
        .unwrap();
    assert_eq!(lookup(&mut state, ROOT, "left.json"), right);
    assert_eq!(lookup(&mut state, ROOT, "right.json"), left);
    let reopened_left = state.open(right, libc::O_RDONLY).unwrap();
    assert_eq!(state.read(right, reopened_left, 0, 100).unwrap(), b"right");
    assert_eq!(state.read(left, held_left, 0, 100).unwrap(), b"left");
}

#[test]
fn namespace_mutations_require_writable_source_and_destination_names() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("source.json"), b"source").unwrap();
    let fs = projection(
        root.path(),
        &[
            ("/source.json", FsAccess::Read),
            ("/destination.json", FsAccess::ReadWrite),
        ],
    );
    let mut state = fs.state().unwrap();
    let source = lookup(&mut state, ROOT, "source.json");
    assert_errno(
        state.link(source, ROOT, OsStr::new("destination.json")),
        libc::EACCES,
    );
    assert_errno(
        state.unlink(ROOT, OsStr::new("source.json"), false),
        libc::EACCES,
    );
    assert_errno(
        state.rename(
            ROOT,
            OsStr::new("source.json"),
            ROOT,
            OsStr::new("destination.json"),
            0,
        ),
        libc::EACCES,
    );
    assert!(root.path().join("source.json").exists());
    assert!(!root.path().join("destination.json").exists());
}

#[test]
fn stale_parent_is_rejected_while_an_already_held_parent_remains_distinct() {
    let root = tempfile::tempdir().unwrap();
    let parent_path = root.path().join("parent");
    std::fs::create_dir(&parent_path).unwrap();
    let fs = projection(root.path(), &[("/parent/*.json", FsAccess::ReadWrite)]);
    let mut state = fs.state().unwrap();
    let parent = lookup(&mut state, ROOT, "parent");
    let held = state.current(&state.node(parent).unwrap()).unwrap();

    std::fs::rename(&parent_path, root.path().join("moved")).unwrap();
    std::fs::create_dir(&parent_path).unwrap();
    assert_errno(
        state.lookup(parent, OsStr::new("before.json")),
        libc::ESTALE,
    );
    assert_errno(
        state.mkdir(parent, OsStr::new("before.json"), 0o700, 0),
        libc::ESTALE,
    );
    assert!(!parent_path.join("before.json").exists());
    assert!(!root.path().join("moved/before.json").exists());

    // A provider-held parent is a capability for that exact directory after it
    // was acquired. This does not claim that a final leaf cannot be renamed by
    // another host actor before a later namespace syscall.
    state
        .backing
        .mkdir_at(&held, OsStr::new("held.json"), 0o700)
        .unwrap();
    let held_child = state
        .backing
        .pin_child(&held, OsStr::new("held.json"))
        .unwrap();
    assert!(held_child.metadata().unwrap().is_dir());
    assert!(root.path().join("moved/held.json").is_dir());
    assert!(!parent_path.join("held.json").exists());
}

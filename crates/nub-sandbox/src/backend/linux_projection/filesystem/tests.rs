use super::*;
use crate::policy::{CanonGlob, Effect, FsOrigin, FsRule, FsRuleSet};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{OpenOptionsExt, symlink};

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
    let root = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
        .open(root)
        .unwrap();
    Projection::acquire(&rules(grants), root).unwrap()
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
fn append_and_readonly_truncate_do_not_change_authority() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file"), b"start").unwrap();
    let fs = projection(root.path(), &[("/file", FsAccess::ReadWrite)]);
    let mut state = fs.state().unwrap();
    let ino = lookup(&mut state, ROOT, "file");
    assert_errno(
        state.open(ino, libc::O_RDONLY | libc::O_TRUNC),
        libc::EACCES,
    );
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

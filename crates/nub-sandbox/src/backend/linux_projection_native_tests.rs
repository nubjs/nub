//! Client-side acceptance for native regular descriptors delivered through a
//! mounted projection. Mount and supervisor ownership remain in the parent
//! helper; this module only supplies the fixture and the command-side contract.

use super::super::linux_projection::{NativeOpenClient, NativeOpenRequest, NativeOpenService};
use super::super::linux_supervisor::{
    EgressPolicy, ProjectedLaunch, SupervisedChild, SupervisedLaunch, SupervisedStdio,
    spawn_supervised_projected,
};
use super::*;
use crate::policy::{CanonGlob, Effect, FsAccess, FsOrigin, FsRule};
use std::ffi::CStr;
use std::io::{BufRead, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

const PAGE: usize = 4096;
const FORGED_EXPORT_IOCTL: libc::c_ulong = 0x4e80;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    mode: u32,
    uid: u32,
    gid: u32,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    bytes: Vec<u8>,
    xattr: Option<Vec<u8>>,
}

fn native_rule(path: &str, access: FsAccess) -> FsRule {
    FsRule {
        matcher: CanonGlob(path.into()),
        access,
        effect: Effect::Allow,
        origin: FsOrigin::Authored,
    }
}

pub(super) fn native_fixture(root: &Path, exe: &Path) -> FsRuleSet {
    let mut rules = fixture(root, exe);
    rules.entries.extend([
        native_rule("/app/native-read", FsAccess::Read),
        native_rule("/app/native-mapped-r", FsAccess::Read),
        native_rule("/app/native-mapped-rw", FsAccess::ReadWrite),
        native_rule("/app/native-rw", FsAccess::ReadWrite),
        native_rule("/app/native-rw-link", FsAccess::Read),
        native_rule("/app/native-rw-absolute", FsAccess::Read),
        native_rule("/app/native-retained", FsAccess::ReadWrite),
        native_rule("/app/native-concurrent", FsAccess::Read),
        native_rule("/app/native-queue-hold", FsAccess::Read),
        native_rule("/app/native-queue-cancelled-create", FsAccess::ReadWrite),
    ]);
    let app = root.join("raw/app");
    fs::write(app.join("native-read"), b"read-canary").unwrap();
    let mut rw = vec![0; PAGE * 2];
    rw[..9].copy_from_slice(b"rw-canary");
    fs::write(app.join("native-rw"), rw).unwrap();
    fs::write(app.join("native-retained"), b"retained-old").unwrap();
    fs::write(app.join("native-concurrent"), b"concurrent-canary").unwrap();
    fs::write(app.join("native-queue-hold"), b"queue-hold-canary").unwrap();
    let mut mapped = vec![0; PAGE * 2];
    mapped[..8].copy_from_slice(b"initial!");
    fs::write(app.join("native-mapped-rw"), &mapped).unwrap();
    fs::hard_link(app.join("native-mapped-rw"), app.join("native-mapped-r")).unwrap();
    fs::hard_link(app.join("native-retained"), app.join("native-denied-alias")).unwrap();
    std::os::unix::fs::symlink("native-rw", app.join("native-rw-link")).unwrap();
    std::os::unix::fs::symlink("/app/native-rw", app.join("native-rw-absolute")).unwrap();
    rules
}

fn snapshot(path: &Path) -> Snapshot {
    use std::os::unix::fs::MetadataExt;
    let file = File::open(path).unwrap();
    let metadata = file.metadata().unwrap();
    let mut xattr = vec![0; 256];
    let length = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            c"user.nub_native_projection".as_ptr(),
            xattr.as_mut_ptr().cast(),
            xattr.len(),
        )
    };
    let xattr = if length < 0 {
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ENODATA)
        );
        None
    } else {
        xattr.truncate(length as usize);
        Some(xattr)
    };
    Snapshot {
        mode: metadata.mode(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        size: metadata.size(),
        mtime: (metadata.mtime(), metadata.mtime_nsec()),
        ctime: (metadata.ctime(), metadata.ctime_nsec()),
        bytes: fs::read(path).unwrap(),
        xattr,
    }
}

fn assert_unchanged(path: &Path, before: &Snapshot, label: &str) {
    assert_eq!(
        &snapshot(path),
        before,
        "{label} changed the backing canary"
    );
}

fn fd_error(result: libc::c_int, label: &str) {
    assert_eq!(result, -1, "{label} unexpectedly succeeded");
}

fn ssize_error(result: isize, label: &str) {
    assert_eq!(result, -1, "{label} unexpectedly succeeded");
}

fn native_event(reader: &mut impl BufRead, prefix: &str) -> io::Result<String> {
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("missing {prefix}"),
            ));
        }
        print!("{line}");
        if let Some(value) = line.trim().strip_prefix(prefix) {
            return Ok(value.to_owned());
        }
    }
}

fn open_read(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .unwrap_or_else(|error| panic!("open read {}: {error}", path.display()))
}

fn open_rw(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .unwrap_or_else(|error| panic!("open rw {}: {error}", path.display()))
}

fn mapping(file: &File, protection: i32, flags: i32) -> *mut u8 {
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            PAGE * 2,
            protection,
            flags,
            file.as_raw_fd(),
            0,
        )
    };
    assert_ne!(
        map,
        libc::MAP_FAILED,
        "mmap: {}",
        io::Error::last_os_error()
    );
    map.cast()
}

fn prefix(map: *const u8, offset: usize) -> [u8; 8] {
    std::array::from_fn(|index| unsafe { std::ptr::read_volatile(map.add(offset + index)) })
}

fn native_mapping_contract(app: &Path, mediated: bool) {
    let read = open_read(&app.join("native-mapped-r"));
    let write = open_rw(&app.join("native-mapped-rw"));
    let rmap = mapping(&read, libc::PROT_READ, libc::MAP_SHARED);
    let wmap = mapping(&write, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED);
    assert_eq!(prefix(rmap, 0), *b"initial!", "prefaulted R map");
    assert_eq!(
        unsafe { libc::pwrite(write.as_raw_fd(), b"changed!".as_ptr().cast(), 8, 0) },
        8
    );
    assert_eq!(
        prefix(rmap, 0),
        *b"changed!",
        "write must reach prefaulted R map"
    );

    let mut pipe = [-1; 2];
    assert_eq!(
        unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        unsafe {
            libc::close(pipe[0]);
            std::ptr::copy_nonoverlapping(b"mapped!!".as_ptr(), wmap.add(16), 8);
            libc::write(pipe[1], b"M".as_ptr().cast(), 1);
            libc::_exit(0);
        }
    }
    unsafe { libc::close(pipe[1]) };
    let mut token = 0;
    assert_eq!(
        unsafe { libc::read(pipe[0], &mut token as *mut _ as *mut _, 1) },
        1
    );
    unsafe { libc::close(pipe[0]) };
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    assert_eq!(
        prefix(rmap, 16),
        *b"mapped!!",
        "fork-dirty shared map visible in R map"
    );
    let mut bytes = [0; 8];
    assert_eq!(
        unsafe { libc::pread(read.as_raw_fd(), bytes.as_mut_ptr().cast(), 8, 16) },
        8
    );
    assert_eq!(bytes, *b"mapped!!", "fork-dirty shared map visible in R fd");

    if mediated {
        println!("NATIVE_HOST_WRITE_READY");
        io::stdout().flush().unwrap();
        let mut line = String::new();
        io::stdin().read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "host-written");
    } else {
        assert_eq!(
            unsafe { libc::pwrite(write.as_raw_fd(), b"host!!!!".as_ptr().cast(), 8, 32) },
            8
        );
    }
    assert_eq!(
        prefix(rmap, 32),
        *b"host!!!!",
        "host write visible in prefaulted R map"
    );
    let cow = mapping(&read, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE);
    unsafe { std::ptr::copy_nonoverlapping(b"private!".as_ptr(), cow, 8) };
    assert_eq!(
        unsafe { libc::pwrite(write.as_raw_fd(), b"aftercow".as_ptr().cast(), 8, 0) },
        8
    );
    assert_eq!(
        prefix(cow, 0),
        *b"private!",
        "private COW must remain private"
    );
    assert_eq!(
        prefix(rmap, 0),
        *b"aftercow",
        "shared R map follows authorized write"
    );
    assert_eq!(
        unsafe { libc::ftruncate(write.as_raw_fd(), PAGE as i64) },
        0
    );
    assert_eq!(
        unsafe { libc::ftruncate(write.as_raw_fd(), (PAGE * 2) as i64) },
        0
    );
    assert_eq!(prefix(rmap, PAGE), [0; 8], "regrown page refaults as zero");
    assert_eq!(
        unsafe {
            libc::pwrite(
                write.as_raw_fd(),
                b"regrown!".as_ptr().cast(),
                8,
                PAGE as i64,
            )
        },
        8
    );
    assert_eq!(
        prefix(rmap, PAGE),
        *b"regrown!",
        "regrown page remains coherent"
    );
    for map in [cow.cast(), rmap.cast(), wmap.cast()] {
        assert_eq!(unsafe { libc::munmap(map, PAGE * 2) }, 0);
    }
    println!("NATIVE_MAPPING_COHERENCE_OK mediated={mediated}");
}

fn native_policy_contract(app: &Path, mediated: bool) {
    let read = open_read(&app.join("native-read"));
    let write = open_rw(&app.join("native-rw"));
    let mut times = [
        libc::timespec {
            tv_sec: 1_700_000_000,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: 1_700_000_001,
            tv_nsec: 0,
        },
    ];
    assert_eq!(unsafe { libc::fchmod(write.as_raw_fd(), 0o600) }, 0);
    assert_eq!(
        unsafe { libc::futimens(write.as_raw_fd(), times.as_ptr()) },
        0
    );
    assert_eq!(
        unsafe {
            libc::fsetxattr(
                write.as_raw_fd(),
                c"user.nub_native_projection".as_ptr(),
                b"positive".as_ptr().cast(),
                8,
                0,
            )
        },
        0
    );
    if mediated {
        for (label, result) in [
            ("R-fchmod", unsafe { libc::fchmod(read.as_raw_fd(), 0o644) }),
            ("R-fchown", unsafe {
                libc::fchown(read.as_raw_fd(), libc::getuid(), libc::getgid())
            }),
            ("R-futimens", unsafe {
                libc::futimens(read.as_raw_fd(), times.as_mut_ptr())
            }),
            ("R-fsetxattr", unsafe {
                libc::fsetxattr(
                    read.as_raw_fd(),
                    c"user.nub_native_projection".as_ptr(),
                    b"denied".as_ptr().cast(),
                    6,
                    0,
                )
            }),
        ] {
            fd_error(result, label);
        }
    } else {
        // Raw O_RDONLY is intentionally not equivalent to a descriptor from
        // the R view: raw descriptors retain mount-independent metadata
        // authority. These positives are the control for mediated denials.
        assert_eq!(unsafe { libc::fchmod(read.as_raw_fd(), 0o640) }, 0);
        assert_eq!(
            unsafe { libc::fchown(read.as_raw_fd(), libc::getuid(), libc::getgid()) },
            0
        );
        assert_eq!(
            unsafe { libc::futimens(read.as_raw_fd(), times.as_ptr()) },
            0
        );
        assert_eq!(
            unsafe {
                libc::fsetxattr(
                    read.as_raw_fd(),
                    c"user.nub_native_projection".as_ptr(),
                    b"raw".as_ptr().cast(),
                    3,
                    0,
                )
            },
            0
        );
        println!("RAW_O_RDONLY_METADATA_POSITIVE");
    }
    fd_error(
        unsafe { libc::ftruncate(read.as_raw_fd(), 0) },
        "R-ftruncate",
    );
    ssize_error(
        unsafe { libc::pwrite(read.as_raw_fd(), b"bad".as_ptr().cast(), 3, 0) },
        "R-write",
    );
    let writable = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            read.as_raw_fd(),
            0,
        )
    };
    assert_eq!(
        writable,
        libc::MAP_FAILED,
        "R shared writable mmap must fail"
    );
    let read_map = mapping(&read, libc::PROT_READ, libc::MAP_SHARED);
    fd_error(
        unsafe { libc::mprotect(read_map.cast(), PAGE, libc::PROT_READ | libc::PROT_WRITE) },
        "R mprotect write",
    );
    assert_eq!(unsafe { libc::munmap(read_map.cast(), PAGE * 2) }, 0);
    assert_eq!(
        unsafe { libc::fcntl(read.as_raw_fd(), libc::F_SETFL, libc::O_RDWR) },
        0
    );
    assert_eq!(
        unsafe { libc::fcntl(read.as_raw_fd(), libc::F_GETFL) } & libc::O_ACCMODE,
        libc::O_RDONLY,
        "F_SETFL cannot upgrade an R descriptor"
    );
    ssize_error(
        unsafe { libc::pwrite(read.as_raw_fd(), b"bad".as_ptr().cast(), 3, 0) },
        "R write after F_SETFL",
    );
    let duplicated = unsafe { libc::fcntl(read.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    assert!(duplicated >= 0);
    if mediated {
        fd_error(
            unsafe { libc::fchmod(duplicated, 0o644) },
            "R dup metadata mutation",
        );
    } else {
        assert_eq!(unsafe { libc::fchmod(duplicated, 0o644) }, 0);
    }
    unsafe { libc::close(duplicated) };
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        let metadata = unsafe { libc::fchmod(read.as_raw_fd(), 0o644) };
        let write = unsafe { libc::pwrite(read.as_raw_fd(), b"bad".as_ptr().cast(), 3, 0) };
        let expected = if mediated {
            metadata == -1 && write == -1
        } else {
            metadata == 0 && write == -1
        };
        unsafe { libc::_exit(if expected { 0 } else { 1 }) };
    }
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    for fd in [read.as_raw_fd(), write.as_raw_fd()] {
        let path = CString::new("../host-canary").unwrap();
        assert_eq!(
            unsafe { libc::openat(fd, path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ENOTDIR)
        );
    }
    let read_on_rw = open_read(&app.join("native-rw"));
    assert_eq!(
        unsafe { libc::fchmod(read_on_rw.as_raw_fd(), 0o640) },
        0,
        "O_RDONLY descriptor on RW mount retains metadata authority"
    );
    assert_eq!(
        unsafe { libc::fchown(read_on_rw.as_raw_fd(), libc::getuid(), libc::getgid()) },
        0
    );
    assert_eq!(
        unsafe { libc::futimens(read_on_rw.as_raw_fd(), times.as_ptr()) },
        0
    );
    let rw_map = mapping(&write, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED);
    unsafe { std::ptr::copy_nonoverlapping(b"rw-map!!".as_ptr(), rw_map, 8) };
    assert_eq!(unsafe { libc::munmap(rw_map.cast(), PAGE * 2) }, 0);
    assert_eq!(
        unsafe { libc::pwrite(write.as_raw_fd(), b"rw-write".as_ptr().cast(), 8, 0) },
        8
    );
    let duplicate_rw = unsafe { libc::fcntl(write.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    assert!(duplicate_rw >= 0);
    assert_eq!(unsafe { libc::fchmod(duplicate_rw, 0o600) }, 0);
    unsafe { libc::close(duplicate_rw) };
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        let written =
            unsafe { libc::pwrite(write.as_raw_fd(), b"fork-rw!".as_ptr().cast(), 8, 128) };
        unsafe { libc::_exit(if written == 8 { 0 } else { 1 }) };
    }
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    let mut fork_bytes = [0; 8];
    assert_eq!(
        unsafe { libc::pread(write.as_raw_fd(), fork_bytes.as_mut_ptr().cast(), 8, 128) },
        8
    );
    assert_eq!(fork_bytes, *b"fork-rw!", "RW descriptor survives fork");
    println!("NATIVE_POLICY_FD_CONTRACT_OK mediated={mediated}");
}

fn openat2(dirfd: i32, name: &CStr, flags: i32, resolve: u64) -> io::Result<File> {
    let how = OpenHow {
        flags: flags as u64,
        mode: 0,
        resolve,
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dirfd,
            name.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        ) as i32
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn ordinary_open_contract(root: &Path, mediated: bool) {
    let app = root.join("app");
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(&app)
        .unwrap();
    assert_ne!(
        unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_GETFL) } & libc::O_PATH,
        0,
        "directory must retain O_PATH semantics"
    );
    let name = CString::new("native-rw").unwrap();
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    assert!(
        fd >= 0,
        "openat regular path: {}",
        io::Error::last_os_error()
    );
    let fd = unsafe { File::from_raw_fd(fd) };
    assert!(fd.metadata().unwrap().is_file());
    assert_ne!(
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0,
        "delivered regular descriptor must honour O_CLOEXEC"
    );
    let duplicate = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    assert!(duplicate >= 0);
    assert_ne!(
        unsafe { libc::fcntl(duplicate, libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    unsafe { libc::close(duplicate) };
    let normal = openat2(
        directory.as_raw_fd(),
        c"native-rw",
        libc::O_RDONLY | libc::O_CLOEXEC,
        0,
    )
    .expect("openat2 regular path");
    assert!(normal.metadata().unwrap().is_file());
    assert_eq!(
        openat2(
            directory.as_raw_fd(),
            c"native-rw-link",
            libc::O_RDONLY | libc::O_CLOEXEC,
            0x04, // RESOLVE_NO_SYMLINKS
        )
        .unwrap_err()
        .raw_os_error(),
        Some(libc::ELOOP)
    );
    let nofollow = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c"native-rw-link".as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    assert_eq!(nofollow, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ELOOP));
    let directory_on_file = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c"native-rw".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    assert_eq!(directory_on_file, -1);
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ENOTDIR)
    );
    assert!(
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
            .open(app.join("native-rw"))
            .is_ok()
    );
    assert!(
        open_read(&app.join("native-rw-link"))
            .metadata()
            .unwrap()
            .is_file()
    );
    if mediated {
        assert!(
            open_read(&app.join("native-rw-absolute"))
                .metadata()
                .unwrap()
                .is_file()
        );
    } else {
        // A raw absolute symlink resolves from the host root, which is not a
        // stable fixture namespace. The mediated positive above specifically
        // proves projected-root resolution; the relative link is the shared
        // raw/FUSE symlink control.
        println!("RAW_ABSOLUTE_SYMLINK_HOST_ROOT_CONTROL");
    }
    let original_rw = fs::read(app.join("native-rw")).unwrap();
    let trunc = unsafe {
        libc::open(
            CString::new(app.join("native-rw").as_os_str().as_bytes())
                .unwrap()
                .as_ptr(),
            libc::O_RDONLY | libc::O_TRUNC | libc::O_CLOEXEC,
        )
    };
    assert!(
        trunc >= 0,
        "O_RDONLY|O_TRUNC native-rw: {}",
        io::Error::last_os_error()
    );
    let trunc = unsafe { File::from_raw_fd(trunc) };
    assert_eq!(
        unsafe { libc::fcntl(trunc.as_raw_fd(), libc::F_GETFL) } & libc::O_ACCMODE,
        libc::O_RDONLY
    );
    assert_eq!(
        trunc.metadata().unwrap().len(),
        0,
        "O_RDONLY|O_TRUNC must truncate through write authority"
    );
    assert_eq!(
        unsafe { libc::write(trunc.as_raw_fd(), b"x".as_ptr().cast(), 1) },
        -1
    );
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
    drop(trunc);
    fs::write(app.join("native-rw"), original_rw).unwrap();
    if mediated {
        let before = fs::read(app.join("native-read")).unwrap();
        let read = CString::new(app.join("native-read").as_os_str().as_bytes()).unwrap();
        assert_eq!(
            unsafe {
                libc::open(
                    read.as_ptr(),
                    libc::O_RDONLY | libc::O_TRUNC | libc::O_CLOEXEC,
                )
            },
            -1
        );
        assert!(matches!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::EACCES | libc::ENOENT)
        ));
        assert_eq!(
            fs::read(app.join("native-read")).unwrap(),
            before,
            "R O_TRUNC denial must not mutate canary"
        );
    }
    println!("NATIVE_O_RDONLY_TRUNCATE_OK mediated={mediated}");
    let forbidden = OpenOptions::new()
        .read(true)
        .open(app.join("native-denied-alias"));
    if mediated {
        denied(forbidden, "fresh denied hardlink alias");
    } else {
        assert!(
            forbidden.is_ok(),
            "raw arm must expose denied-alias control"
        );
    }
    println!("NATIVE_ORDINARY_OPENAT_OPENAT2_OK mediated={mediated}");
}

fn retained_handle_contract(root: &Path, mediated: bool) {
    let app = root.join("app");
    let mut retained = open_rw(&app.join("native-retained"));
    if mediated {
        println!("NATIVE_RETAINED_HANDLE_READY");
        io::stdout().flush().unwrap();
        let mut line = String::new();
        io::stdin().read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "replaced");
    } else {
        fs::rename(
            app.join("native-retained"),
            app.join("native-retained-hidden"),
        )
        .unwrap();
        fs::write(app.join("native-retained"), b"retained-replacement").unwrap();
    }
    retained.write_all(b"retained-write").unwrap();
    retained.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    retained.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"retained-write");
    assert_eq!(
        fs::read(app.join("native-retained")).unwrap(),
        b"retained-replacement"
    );
    let fresh_alias = OpenOptions::new()
        .read(true)
        .open(app.join("native-denied-alias"));
    if mediated {
        denied(fresh_alias, "fresh denied alias after retained replacement");
    } else {
        assert!(fresh_alias.is_ok(), "raw alias control after replacement");
    }
    let forged = unsafe { libc::ioctl(retained.as_raw_fd(), FORGED_EXPORT_IOCTL) };
    // The confined command reaches the shared seccomp ioctl ceiling before a
    // provider callback; the provider's unfiltered non-resolver-thread unit
    // control owns callback-authentication proof. This is still the client
    // contract: an arbitrary command ioctl cannot create an export slot.
    fd_error(forged, "command forged native export ioctl");
    println!("NATIVE_RETAINED_FORGED_IOCTL_CEILING_OK mediated={mediated}");
}

fn native_client(root: &Path, mediated: bool) {
    if mediated {
        audit_command();
    } else {
        // The raw arm intentionally runs outside the chroot/supervisor. Its
        // NNP/capability state is therefore a control, not a projection claim.
        println!("NATIVE_RAW_CONTROL_NO_SUPERVISOR");
    }
    ordinary_open_contract(root, mediated);
    native_mapping_contract(&root.join("app"), mediated);
    native_policy_contract(&root.join("app"), mediated);
    retained_handle_contract(root, mediated);
    println!("NATIVE_CLIENT_ACCEPTANCE_OK mediated={mediated}");
}

fn native_concurrent_client(root: &Path) {
    audit_command();
    println!("NATIVE_CONCURRENT_READY");
    io::stdout().flush().unwrap();
    let mut line = String::new();
    io::stdin().read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "go");
    let mut file = open_read(&root.join("app/native-concurrent"));
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"concurrent-canary");
    println!("NATIVE_CONCURRENT_OK");
}

fn native_cancelled_create_client(root: &Path) {
    audit_command();
    println!("NATIVE_CANCELLED_CREATE_READY");
    io::stdout().flush().unwrap();
    let mut line = String::new();
    io::stdin().read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "go");
    let result = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join("app/native-queue-cancelled-create"));
    assert!(
        result.is_err(),
        "cancelled create unexpectedly reached the projected filesystem"
    );
}

fn wait_for_unreaped_exit(child: &SupervisedChild) -> io::Result<()> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        if unsafe {
            libc::waitid(
                libc::P_PID,
                child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        } == 0
        {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn native_request(path: &CStr, flags: i32, mode: u64) -> NativeOpenRequest {
    NativeOpenRequest {
        path: path.to_owned(),
        directory: None,
        flags: flags as u64,
        mode,
        resolve: None,
        umask: 0,
    }
}

fn assert_native_identity(file: &File, backing: &Path) {
    let delivered = file.metadata().unwrap();
    let backing = fs::metadata(backing).unwrap();
    assert_eq!(
        (delivered.dev(), delivered.ino()),
        (backing.dev(), backing.ino()),
        "native result must be the backing descriptor rather than the FUSE path"
    );
}

fn native_queue_contract(service: &NativeOpenService, raw: &Path) {
    let client = service.client();
    let hold_path = c"/app/native-queue-hold";
    let before = client.stats();
    let gate = service.block_next_path(hold_path).unwrap();
    let active = client
        .submit(native_request(hold_path, libc::O_RDONLY, 0))
        .unwrap();
    gate.wait_until_active().unwrap();
    let queued = client
        .submit(native_request(c"/app/native-read", libc::O_RDONLY, 0))
        .unwrap();
    let overflow = client.submit(native_request(c"/app/native-concurrent", libc::O_RDONLY, 0));
    match overflow {
        Err(error) => assert_eq!(
            error.raw_os_error(),
            Some(libc::EAGAIN),
            "one active request plus one queued request must saturate the native queue"
        ),
        Ok(_) => {
            panic!("one active request plus one queued request must saturate the native queue")
        }
    }
    gate.release().unwrap();
    let active = active.finish_blocking().unwrap();
    let queued = queued.finish_blocking().unwrap();
    assert_native_identity(&active, &raw.join("app/native-queue-hold"));
    assert_native_identity(&queued, &raw.join("app/native-read"));
    assert_eq!(
        client.stats(),
        (before.0 + 2, before.1 + 2),
        "each completed regular native open must be exported exactly once"
    );
    println!("NATIVE_QUEUE_SATURATION_OK");

    let cancelled = raw.join("app/native-queue-cancelled-create");
    assert!(!cancelled.exists(), "cancelled create starts absent");
    let before = client.stats();
    let gate = service.block_next_path(hold_path).unwrap();
    let active = client
        .submit(native_request(hold_path, libc::O_RDONLY, 0))
        .unwrap();
    gate.wait_until_active().unwrap();
    let queued = client
        .submit(native_request(
            c"/app/native-queue-cancelled-create",
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o600,
        ))
        .unwrap();
    drop(queued);
    let cancelled_gate = service
        .block_next_path(c"/app/native-queue-cancelled-create")
        .unwrap();
    gate.release().unwrap();
    let active = active.finish_blocking().unwrap();
    assert_native_identity(&active, &raw.join("app/native-queue-hold"));
    cancelled_gate.wait_until_active().unwrap();
    let sentinel = client
        .submit(native_request(c"/app/native-read", libc::O_RDONLY, 0))
        .unwrap();
    cancelled_gate.release().unwrap();
    let sentinel = sentinel.finish_blocking().unwrap();
    assert_native_identity(&sentinel, &raw.join("app/native-read"));
    assert!(
        !cancelled.exists(),
        "dropping a queued native create must prevent its side effect"
    );
    assert_eq!(
        client.stats(),
        (before.0 + 2, before.1 + 2),
        "cancelled queued create must never reach projected open/export before the sentinel"
    );
    println!("NATIVE_QUEUED_CANCEL_OK");
}

fn spawn_native_command(role: &str, view: &CString, opener: NativeOpenClient) -> SupervisedChild {
    let exe = [
        CString::new("/app/run").unwrap(),
        CString::new("--exact").unwrap(),
        CString::new(HELPER).unwrap(),
        CString::new("--nocapture").unwrap(),
        CString::new("--test-threads=1").unwrap(),
    ];
    let env = [
        CString::new("PATH=/usr/bin:/bin").unwrap(),
        CString::new(format!("{ROLE}={role}")).unwrap(),
        CString::new("NUB_PROJECTION_TEST_ROOT=/").unwrap(),
    ];
    let launch = SupervisedLaunch {
        argv: &exe,
        envp: &env,
        cwd: Some(c"/"),
        ruleset_fd: -1,
        seccomp_ceiling: None,
        stdin: SupervisedStdio::Piped,
        stdout: SupervisedStdio::Piped,
        stderr: SupervisedStdio::Piped,
        inherited_fds: &[],
    };
    let policy = EgressPolicy {
        self_proc: BTreeSet::new(),
        allow_all: false,
        allow: vec![],
        write_policy: None,
        proxy_port: None,
        proxy_token: None,
    };
    spawn_supervised_projected(
        policy,
        launch,
        ProjectedLaunch::at_path(view, opener).unwrap(),
    )
    .unwrap()
}

fn native_simultaneous_command_contract(service: &NativeOpenService, view: &CString) {
    let mut first = spawn_native_command("native-concurrent", view, service.client());
    let mut second = spawn_native_command("native-concurrent", view, service.client());
    let mut first_output = BufReader::new(first.take_stdout().unwrap());
    let mut second_output = BufReader::new(second.take_stdout().unwrap());
    let first_stderr = first.take_stderr().unwrap();
    let second_stderr = second.take_stderr().unwrap();
    let first_stderr_drain = std::thread::spawn(move || -> io::Result<String> {
        let mut output = String::new();
        BufReader::new(first_stderr).read_to_string(&mut output)?;
        Ok(output)
    });
    let second_stderr_drain = std::thread::spawn(move || -> io::Result<String> {
        let mut output = String::new();
        BufReader::new(second_stderr).read_to_string(&mut output)?;
        Ok(output)
    });
    let mut first_input = first.take_stdin().unwrap();
    let mut second_input = second.take_stdin().unwrap();
    let mut gate = None;
    let protocol = (|| -> io::Result<(u64, u64)> {
        native_event(&mut first_output, "NATIVE_CONCURRENT_READY")?;
        native_event(&mut second_output, "NATIVE_CONCURRENT_READY")?;
        let before = service.client().stats();
        gate = Some(service.block_next_path(c"/app/native-concurrent")?);
        first_input.write_all(b"go\n")?;
        gate.as_ref()
            .ok_or_else(|| io::Error::other("first command gate was not armed"))?
            .wait_until_active()?;
        second_input.write_all(b"go\n")?;
        gate.take()
            .ok_or_else(|| io::Error::other("first command gate was released twice"))?
            .release()?;
        native_event(&mut first_output, "NATIVE_CONCURRENT_OK")?;
        native_event(&mut second_output, "NATIVE_CONCURRENT_OK")?;
        Ok(before)
    })();
    drop(gate.take());
    let killed = if protocol.is_err() {
        Some((first.kill(), second.kill()))
    } else {
        None
    };
    drop(first_input);
    drop(second_input);
    let outcome = (
        protocol,
        first.wait(),
        second.wait(),
        first_stderr_drain.join(),
        second_stderr_drain.join(),
    );
    match outcome {
        (
            Ok(before),
            Ok(first_status),
            Ok(second_status),
            Ok(Ok(first_stderr)),
            Ok(Ok(second_stderr)),
        ) => {
            assert!(
                first_status.success(),
                "first native command {first_status:?}\n{first_stderr}"
            );
            assert!(
                second_status.success(),
                "second native command {second_status:?}\n{second_stderr}"
            );
            assert_eq!(
                service.client().stats(),
                (before.0 + 2, before.1 + 2),
                "two supervised commands sharing one native service must each receive one exported file"
            );
            println!("NATIVE_SHARED_SERVICE_COMMANDS_OK");
        }
        outcome => panic!(
            "native shared-service protocol failed after reaping children; killed={killed:?}; outcome={outcome:?}"
        ),
    }
}

fn native_shared_service_cancellation_contract(service: &NativeOpenService, view: &CString) {
    let mut victim = spawn_native_command("native-concurrent", view, service.client());
    let mut survivor = spawn_native_command("native-concurrent", view, service.client());
    let mut victim_output = BufReader::new(victim.take_stdout().unwrap());
    let mut survivor_output = BufReader::new(survivor.take_stdout().unwrap());
    let victim_stderr = victim.take_stderr().unwrap();
    let survivor_stderr = survivor.take_stderr().unwrap();
    let victim_stderr_drain = std::thread::spawn(move || -> io::Result<String> {
        let mut output = String::new();
        BufReader::new(victim_stderr).read_to_string(&mut output)?;
        Ok(output)
    });
    let survivor_stderr_drain = std::thread::spawn(move || -> io::Result<String> {
        let mut output = String::new();
        BufReader::new(survivor_stderr).read_to_string(&mut output)?;
        Ok(output)
    });
    let victim_input = victim.take_stdin().unwrap();
    let mut survivor_input = survivor.take_stdin().unwrap();
    let protocol = (|| -> io::Result<(u64, u64)> {
        native_event(&mut victim_output, "NATIVE_CONCURRENT_READY")?;
        native_event(&mut survivor_output, "NATIVE_CONCURRENT_READY")?;
        let before = service.client().stats();
        victim.kill()?;
        let victim_status = victim.wait()?;
        if victim_status.success() {
            return Err(io::Error::other(
                "cancelled native command unexpectedly succeeded",
            ));
        }
        survivor_input.write_all(b"go\n")?;
        native_event(&mut survivor_output, "NATIVE_CONCURRENT_OK")?;
        Ok(before)
    })();
    let killed = if protocol.is_err() {
        Some((victim.kill(), survivor.kill()))
    } else {
        None
    };
    drop(victim_input);
    drop(survivor_input);
    let outcome = (
        protocol,
        victim.wait(),
        survivor.wait(),
        victim_stderr_drain.join(),
        survivor_stderr_drain.join(),
    );
    match outcome {
        (
            Ok(before),
            Ok(victim_status),
            Ok(survivor_status),
            Ok(Ok(victim_stderr)),
            Ok(Ok(survivor_stderr)),
        ) => {
            assert!(
                !victim_status.success(),
                "cancelled native command unexpectedly succeeded: {victim_status:?}\n{victim_stderr}"
            );
            assert!(
                survivor_status.success(),
                "surviving native command {survivor_status:?}\n{survivor_stderr}"
            );
            assert_eq!(
                service.client().stats(),
                (before.0 + 1, before.1 + 1),
                "only the surviving command may acquire a native file after its peer is cancelled"
            );
            println!("NATIVE_SHARED_SERVICE_CANCELLATION_OK");
        }
        outcome => panic!(
            "native shared-service cancellation protocol failed after reaping children; killed={killed:?}; outcome={outcome:?}"
        ),
    }
}

fn native_admission_cancellation_contract(service: &NativeOpenService, view: &CString, raw: &Path) {
    let mut first = spawn_native_command("native-concurrent", view, service.client());
    let mut second = spawn_native_command("native-concurrent", view, service.client());
    let mut cancelled = spawn_native_command("native-cancelled-create", view, service.client());
    let mut first_output = BufReader::new(first.take_stdout().unwrap());
    let mut second_output = BufReader::new(second.take_stdout().unwrap());
    let mut cancelled_output = BufReader::new(cancelled.take_stdout().unwrap());
    let first_stderr = first.take_stderr().unwrap();
    let second_stderr = second.take_stderr().unwrap();
    let cancelled_stderr = cancelled.take_stderr().unwrap();
    let first_stderr_drain = std::thread::spawn(move || -> io::Result<String> {
        let mut output = String::new();
        BufReader::new(first_stderr).read_to_string(&mut output)?;
        Ok(output)
    });
    let second_stderr_drain = std::thread::spawn(move || -> io::Result<String> {
        let mut output = String::new();
        BufReader::new(second_stderr).read_to_string(&mut output)?;
        Ok(output)
    });
    let cancelled_stderr_drain = std::thread::spawn(move || -> io::Result<String> {
        let mut output = String::new();
        BufReader::new(cancelled_stderr).read_to_string(&mut output)?;
        Ok(output)
    });
    let mut first_input = first.take_stdin().unwrap();
    let mut second_input = second.take_stdin().unwrap();
    let mut cancelled_input = cancelled.take_stdin().unwrap();
    let cancelled_path = raw.join("app/native-queue-cancelled-create");
    assert!(
        !cancelled_path.exists(),
        "cancelled admission create starts absent"
    );
    let client = service.client();
    let mut gate = None;
    let protocol = (|| -> io::Result<(u64, u64)> {
        native_event(&mut first_output, "NATIVE_CONCURRENT_READY")?;
        native_event(&mut second_output, "NATIVE_CONCURRENT_READY")?;
        native_event(&mut cancelled_output, "NATIVE_CANCELLED_CREATE_READY")?;
        let before = client.stats();
        gate = Some(service.block_next_path(c"/app/native-concurrent")?);
        first_input.write_all(b"go\n")?;
        gate.as_ref()
            .ok_or_else(|| io::Error::other("admission gate was not armed"))?
            .wait_until_active()?;
        second_input.write_all(b"go\n")?;
        client.wait_for_admission(1, 0)?;
        cancelled_input.write_all(b"go\n")?;
        client.wait_for_admission(1, 1)?;
        cancelled.kill()?;
        let cancelled_status = cancelled.wait()?;
        if cancelled_status.success() {
            return Err(io::Error::other(
                "capacity-cancelled command unexpectedly succeeded",
            ));
        }
        gate.take()
            .ok_or_else(|| io::Error::other("admission gate was released twice"))?
            .release()?;
        native_event(&mut first_output, "NATIVE_CONCURRENT_OK")?;
        native_event(&mut second_output, "NATIVE_CONCURRENT_OK")?;
        Ok(before)
    })();
    drop(gate.take());
    let killed = if protocol.is_err() {
        Some((first.kill(), second.kill(), cancelled.kill()))
    } else {
        None
    };
    drop(first_input);
    drop(second_input);
    drop(cancelled_input);
    let outcome = (
        protocol,
        first.wait(),
        second.wait(),
        cancelled.wait(),
        first_stderr_drain.join(),
        second_stderr_drain.join(),
        cancelled_stderr_drain.join(),
    );
    match outcome {
        (
            Ok(before),
            Ok(first_status),
            Ok(second_status),
            Ok(cancelled_status),
            Ok(Ok(first_stderr)),
            Ok(Ok(second_stderr)),
            Ok(Ok(cancelled_stderr)),
        ) => {
            assert!(
                first_status.success(),
                "first admission command {first_status:?}\n{first_stderr}"
            );
            assert!(
                second_status.success(),
                "second admission command {second_status:?}\n{second_stderr}"
            );
            assert!(
                !cancelled_status.success(),
                "capacity-cancelled command unexpectedly succeeded: {cancelled_status:?}\n{cancelled_stderr}"
            );
            assert!(
                !cancelled_path.exists(),
                "capacity-cancelled create reached the projected filesystem"
            );
            assert_eq!(
                client.stats(),
                (before.0 + 2, before.1 + 2),
                "only the two admitted commands may acquire native files"
            );
            println!("NATIVE_ADMISSION_CANCELLATION_OK");
        }
        outcome => panic!(
            "native admission cancellation failed after reaping children; killed={killed:?}; outcome={outcome:?}"
        ),
    }
}

fn native_stale_notification_contract(service: &NativeOpenService, view: &CString, raw: &Path) {
    let mut first = spawn_native_command("native-concurrent", view, service.client());
    let mut second = spawn_native_command("native-concurrent", view, service.client());
    let mut stale = spawn_native_command("native-cancelled-create", view, service.client());
    let mut first_output = BufReader::new(first.take_stdout().unwrap());
    let mut second_output = BufReader::new(second.take_stdout().unwrap());
    let mut stale_output = BufReader::new(stale.take_stdout().unwrap());
    let first_stderr = first.take_stderr().unwrap();
    let second_stderr = second.take_stderr().unwrap();
    let stale_stderr = stale.take_stderr().unwrap();
    let first_stderr_drain = std::thread::spawn(move || -> io::Result<String> {
        let mut output = String::new();
        BufReader::new(first_stderr).read_to_string(&mut output)?;
        Ok(output)
    });
    let second_stderr_drain = std::thread::spawn(move || -> io::Result<String> {
        let mut output = String::new();
        BufReader::new(second_stderr).read_to_string(&mut output)?;
        Ok(output)
    });
    let stale_stderr_drain = std::thread::spawn(move || -> io::Result<String> {
        let mut output = String::new();
        BufReader::new(stale_stderr).read_to_string(&mut output)?;
        Ok(output)
    });
    let mut first_input = first.take_stdin().unwrap();
    let mut second_input = second.take_stdin().unwrap();
    let mut stale_input = stale.take_stdin().unwrap();
    let stale_path = raw.join("app/native-queue-cancelled-create");
    assert!(
        !stale_path.exists(),
        "stale notification create starts absent"
    );
    let client = service.client();
    let mut first_gate = None;
    let mut stale_gate = None;
    let protocol = (|| -> io::Result<(u64, u64)> {
        native_event(&mut first_output, "NATIVE_CONCURRENT_READY")?;
        native_event(&mut second_output, "NATIVE_CONCURRENT_READY")?;
        native_event(&mut stale_output, "NATIVE_CANCELLED_CREATE_READY")?;
        let before = client.stats();
        first_gate = Some(service.block_next_path(c"/app/native-concurrent")?);
        first_input.write_all(b"go\n")?;
        first_gate
            .as_ref()
            .ok_or_else(|| io::Error::other("first stale-test gate was not armed"))?
            .wait_until_active()?;
        second_input.write_all(b"go\n")?;
        client.wait_for_admission(1, 0)?;
        stale_input.write_all(b"go\n")?;
        client.wait_for_admission(1, 1)?;
        stale_gate = Some(service.block_next_path(c"/app/native-queue-cancelled-create")?);
        first_gate
            .take()
            .ok_or_else(|| io::Error::other("first stale-test gate was released twice"))?
            .release()?;
        stale_gate
            .as_ref()
            .ok_or_else(|| io::Error::other("stale notification gate was not armed"))?
            .wait_until_active()?;
        stale.kill()?;
        wait_for_unreaped_exit(&stale)?;
        let sentinel = client.submit(native_request(c"/app/native-read", libc::O_RDONLY, 0))?;
        stale_gate
            .take()
            .ok_or_else(|| io::Error::other("stale notification gate was released twice"))?
            .release()?;
        let sentinel = sentinel.finish_blocking()?;
        assert_native_identity(&sentinel, &raw.join("app/native-read"));
        native_event(&mut first_output, "NATIVE_CONCURRENT_OK")?;
        native_event(&mut second_output, "NATIVE_CONCURRENT_OK")?;
        Ok(before)
    })();
    drop(first_gate.take());
    drop(stale_gate.take());
    let killed = if protocol.is_err() {
        Some((first.kill(), second.kill(), stale.kill()))
    } else {
        None
    };
    drop(first_input);
    drop(second_input);
    drop(stale_input);
    let outcome = (
        protocol,
        first.wait(),
        second.wait(),
        stale.wait(),
        first_stderr_drain.join(),
        second_stderr_drain.join(),
        stale_stderr_drain.join(),
    );
    match outcome {
        (
            Ok(before),
            Ok(first_status),
            Ok(second_status),
            Ok(stale_status),
            Ok(Ok(first_stderr)),
            Ok(Ok(second_stderr)),
            Ok(Ok(stale_stderr)),
        ) => {
            assert!(
                first_status.success(),
                "first stale-notification peer {first_status:?}\n{first_stderr}"
            );
            assert!(
                second_status.success(),
                "second stale-notification peer {second_status:?}\n{second_stderr}"
            );
            assert!(
                !stale_status.success(),
                "stale notification command unexpectedly succeeded: {stale_status:?}\n{stale_stderr}"
            );
            assert!(
                !stale_path.exists(),
                "stale notification create reached the projected filesystem"
            );
            assert_eq!(
                client.stats(),
                (before.0 + 3, before.1 + 3),
                "only two peers and the post-stale sentinel may acquire native files"
            );
            println!("NATIVE_STALE_NOTIFICATION_CANCEL_OK");
        }
        outcome => panic!(
            "native stale-notification protocol failed after reaping children; killed={killed:?}; outcome={outcome:?}"
        ),
    }
}

pub(super) fn recursive_view(source: &Path, target: &Path, readonly: bool) -> File {
    fs::create_dir(target).unwrap();
    let source = CString::new(source.as_os_str().as_bytes()).unwrap();
    let target_c = CString::new(target.as_os_str().as_bytes()).unwrap();
    checked(unsafe {
        libc::mount(
            source.as_ptr(),
            target_c.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        )
    })
    .unwrap();
    if readonly {
        #[repr(C)]
        struct MountAttr {
            attr_set: u64,
            attr_clr: u64,
            propagation: u64,
            userns_fd: u64,
        }
        let attr = MountAttr {
            attr_set: 0x0000_0001,
            attr_clr: 0,
            propagation: 0,
            userns_fd: 0,
        };
        checked(unsafe {
            libc::syscall(
                libc::SYS_mount_setattr,
                libc::AT_FDCWD,
                target_c.as_ptr(),
                libc::AT_RECURSIVE,
                &attr,
                std::mem::size_of::<MountAttr>(),
            ) as i32
        })
        .unwrap();
    }
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(target)
        .unwrap()
}

fn native_stalled_shutdown(service: NativeOpenService) {
    let client = service.client();
    let pid = client.resolver_tid() as libc::pid_t;
    checked(unsafe { libc::kill(pid, libc::SIGSTOP) }).unwrap();
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) },
        pid
    );
    assert!(libc::WIFSTOPPED(status));
    let (entered, active) = std::sync::mpsc::sync_channel(1);
    let pending = client
        .submit_cancellable(
            native_request(c"/app/native-read", libc::O_RDONLY, 0),
            Box::new(move || {
                entered.send(()).unwrap();
                true
            }),
            || false,
            |_| panic!("idle service must have capacity"),
        )
        .unwrap();
    active.recv_timeout(Duration::from_secs(5)).unwrap();
    // The raw child cannot reply while stopped. Shutdown must wake the request
    // worker and kill/reap its child before joining, not wait for that reply.
    let start = Instant::now();
    service.shutdown().unwrap();
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(pending.finish_blocking().is_err());
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    println!("NATIVE_STALLED_SERVICE_SHUTDOWN_REAPED");
}

fn native_provider(root: &Path) {
    unsafe {
        libc::alarm(45);
        libc::umask(0);
    }
    let rules: FsRuleSet =
        serde_json::from_slice(&fs::read(root.join("rules.json")).unwrap()).unwrap();
    let raw = root.join("raw");
    let rw = root.join("rw");
    let read = root.join("read");
    let rw_root = recursive_view(&raw, &rw, false);
    let read_root = recursive_view(&raw, &read, true);
    let projection = Projection::acquire_native(&rules, rw_root, read_root).unwrap();
    let fuse = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open("/dev/fuse")
        .expect("direct unprivileged /dev/fuse prerequisite");
    let view = root.join("view");
    let view_c = CString::new(view.as_os_str().as_bytes()).unwrap();
    let options = CString::new(format!(
        "fd={},rootmode=40000,user_id=0,group_id=0",
        fuse.as_raw_fd()
    ))
    .unwrap();
    checked(unsafe {
        libc::mount(
            c"nub-native-projection-test".as_ptr(),
            view_c.as_ptr(),
            c"fuse".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            options.as_ptr().cast(),
        )
    })
    .expect("direct FUSE mount prerequisite");
    let serve = projection.clone();
    let server = std::thread::spawn(move || serve.serve(fuse.into()));
    // Acquiring a reusable service from a short-lived library thread must not
    // bind the raw opener's PDEATHSIG lifetime to that thread.
    let acquire = projection.clone();
    let acquire_view = view.clone();
    let service = std::thread::spawn(move || acquire.native_opener(&acquire_view))
        .join()
        .unwrap()
        .unwrap();
    native_queue_contract(&service, &raw);
    println!("NATIVE_ACQUIRING_THREAD_EXIT_SURVIVED");
    native_simultaneous_command_contract(&service, &view_c);
    native_shared_service_cancellation_contract(&service, &view_c);
    native_admission_cancellation_contract(&service, &view_c, &raw);
    native_stale_notification_contract(&service, &view_c, &raw);
    // This provider thread is deliberately neither the registered resolver
    // thread nor seccomp-filtered. The FUSE callback therefore receives a
    // wrong Request.pid; EACCES proves callback authentication rather than the
    // command's generic ioctl ceiling.
    let wrong_thread = File::open(view.join("app/native-read")).unwrap();
    assert_eq!(
        unsafe { libc::ioctl(wrong_thread.as_raw_fd(), FORGED_EXPORT_IOCTL, 0) },
        -1,
        "unregistered provider thread must not obtain a native export"
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::EACCES),
        "export callback must reject the wrong Request.pid"
    );
    println!("NATIVE_EXPORT_CALLBACK_AUTH_DENIED");
    drop(wrong_thread);
    let exe = [
        CString::new("/app/run").unwrap(),
        CString::new("--exact").unwrap(),
        CString::new(HELPER).unwrap(),
        CString::new("--nocapture").unwrap(),
        CString::new("--test-threads=1").unwrap(),
    ];
    let env = [
        CString::new("PATH=/usr/bin:/bin").unwrap(),
        CString::new(format!("{ROLE}=native-command")).unwrap(),
        CString::new("NUB_PROJECTION_TEST_ROOT=/").unwrap(),
    ];
    let launch = SupervisedLaunch {
        argv: &exe,
        envp: &env,
        cwd: Some(c"/"),
        ruleset_fd: -1,
        seccomp_ceiling: None,
        stdin: SupervisedStdio::Piped,
        stdout: SupervisedStdio::Piped,
        stderr: SupervisedStdio::Piped,
        inherited_fds: &[],
    };
    let policy = EgressPolicy {
        self_proc: BTreeSet::new(),
        allow_all: false,
        allow: vec![],
        write_policy: None,
        proxy_port: None,
        proxy_token: None,
    };
    let opener = service.client();
    let stats = opener.clone();
    let mut child = spawn_supervised_projected(
        policy,
        launch,
        ProjectedLaunch::at_path(&view_c, opener).unwrap(),
    )
    .unwrap();
    let mut output = BufReader::new(child.take_stdout().unwrap());
    let stderr = child.take_stderr().unwrap();
    let stderr_drain = std::thread::spawn(move || -> io::Result<String> {
        let mut output = String::new();
        BufReader::new(stderr).read_to_string(&mut output)?;
        Ok(output)
    });
    let mut input = child.take_stdin().unwrap();
    let protocol = (|| -> io::Result<()> {
        native_event(&mut output, "NATIVE_HOST_WRITE_READY")?;
        let host_mapped = OpenOptions::new()
            .write(true)
            .open(raw.join("app/native-mapped-rw"))?;
        if unsafe { libc::pwrite(host_mapped.as_raw_fd(), b"host!!!!".as_ptr().cast(), 8, 32) } != 8
        {
            return Err(io::Error::last_os_error());
        }
        input.write_all(b"host-written\n")?;
        native_event(&mut output, "NATIVE_RETAINED_HANDLE_READY")?;
        fs::rename(
            raw.join("app/native-retained"),
            raw.join("app/native-retained-hidden"),
        )?;
        fs::write(raw.join("app/native-retained"), b"retained-replacement")?;
        input.write_all(b"replaced\n")?;
        native_event(&mut output, "NATIVE_CLIENT_ACCEPTANCE_OK mediated=true")?;
        Ok(())
    })();
    if let Err(error) = protocol {
        drop(input);
        let _ = child.kill();
        let status = child.wait();
        let stderr = stderr_drain.join().unwrap().unwrap();
        panic!("native client protocol failed: {error}; child={status:?}\n{stderr}");
    }
    drop(input);
    let status = child.wait().unwrap();
    let stderr = stderr_drain.join().unwrap().unwrap();
    assert!(status.success(), "native client {status:?}\n{stderr}");
    let (opened, exported) = stats.stats();
    assert!(
        opened > 0 && exported > 0,
        "native client did not exercise provider export"
    );
    assert_ne!(stats.resolver_tid(), 0, "native resolver has no TID");
    drop(stats);
    native_stalled_shutdown(service);
    drop(projection);
    unmount_projection(&view_c, server);
    for path in [&read, &rw] {
        let path_c = CString::new(path.as_os_str().as_bytes()).unwrap();
        checked(unsafe { libc::umount2(path_c.as_ptr(), 0) }).expect("normal backing-view unmount");
        fs::remove_dir(path).unwrap();
    }
    fs::remove_dir(&view).unwrap();
    println!("NATIVE_PROVIDER_NORMAL_UNMOUNT_OK");
}

fn native_supervisor(root: &Path) {
    unsafe {
        libc::alarm(90);
    }
    assert_ne!(
        unsafe { libc::getuid() },
        0,
        "probe must begin as ordinary user"
    );
    let exe = std::env::current_exe().unwrap();
    for (name, role) in [("raw", "native-raw"), ("mediated", "native-provider")] {
        let case = root.join(name);
        fs::create_dir(&case).unwrap();
        let rules = native_fixture(&case, &exe);
        fs::write(case.join("rules.json"), serde_json::to_vec(&rules).unwrap()).unwrap();
        let mut cmd = if role == "native-raw" {
            helper(&case.join("raw/app/run"), role, &case.join("raw"))
        } else {
            helper(&exe, role, &case)
        };
        if role == "native-provider" {
            namespace_launch(&mut cmd);
        }
        let before = snapshot(&case.join("raw/app/native-read"));
        let status = cmd.status().unwrap();
        assert!(status.success(), "{role}: {status:?}");
        let read = case.join("raw/app/native-read");
        if role == "native-raw" {
            assert_ne!(
                snapshot(&read),
                before,
                "raw O_RDONLY metadata-positive control did not mutate its canary"
            );
        } else {
            assert_unchanged(&read, &before, "mediated R-policy canary");
        }
        fs::remove_dir_all(&case).unwrap();
    }
    println!("NATIVE_PROJECTION_ACCEPTANCE_OK");
}

pub(super) fn run_role(role: &str, root: &Path) -> bool {
    match role {
        "native-supervisor" => native_supervisor(root),
        "native-provider" => native_provider(root),
        "native-raw" => native_client(root, false),
        "native-command" => native_client(root, true),
        "native-concurrent" => native_concurrent_client(root),
        "native-cancelled-create" => native_cancelled_create_client(root),
        _ => return false,
    }
    true
}

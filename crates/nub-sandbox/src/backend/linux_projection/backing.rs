use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use crate::policy::FsAccess;

pub(super) struct Backing {
    root: File,
    read: Option<File>,
}

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

pub(super) fn error(errno: i32) -> io::Error {
    io::Error::from_raw_os_error(errno)
}

pub(super) fn child_path(parent: &Path, name: &OsStr) -> io::Result<PathBuf> {
    valid_child_name(name)?;
    Ok(parent.join(name))
}

fn valid_child_name(name: &OsStr) -> io::Result<()> {
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes.contains(&0)
        || bytes.contains(&b'/')
        || bytes == b"."
        || bytes == b".."
    {
        return Err(error(libc::EINVAL));
    }
    Ok(())
}

fn child_name(name: &OsStr) -> io::Result<CString> {
    valid_child_name(name)?;
    CString::new(name.as_bytes()).map_err(|_| error(libc::EINVAL))
}

pub(super) fn same_object(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino() && a.mode() & libc::S_IFMT == b.mode() & libc::S_IFMT
}

pub(super) fn reopen_regular(pin: &File, flags: i32) -> io::Result<File> {
    if !pin.metadata()?.is_file() {
        return Err(error(libc::EACCES));
    }
    // This is a provider-owned descriptor, not a caller pathname. Reopening the
    // pinned object avoids activating a device substituted after a type check.
    // The namespace worker must retain its trusted procfs while serving requests.
    let path =
        CString::new(format!("/proc/self/fd/{}", pin.as_raw_fd())).map_err(io::Error::other)?;
    // SAFETY: path names the live pin; O_CREAT is not accepted by the caller.
    let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: open returned a newly owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn is_path_descriptor(file: &File) -> io::Result<bool> {
    // SAFETY: file is a live descriptor; F_GETFL takes no variadic argument.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(flags & libc::O_PATH != 0)
    }
}

const FCHMODAT2: libc::c_long = 452;
const EMPTY_PATH_NOFOLLOW: libc::c_int = libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW;

fn chmod_path_descriptor(pin: &File, mode: u32) -> io::Result<()> {
    // SAFETY: pin is a live descriptor and the empty C string remains live.
    let result = unsafe {
        libc::syscall(
            FCHMODAT2,
            pin.as_raw_fd(),
            c"".as_ptr(),
            mode,
            EMPTY_PATH_NOFOLLOW,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Require Linux's fchmodat2 empty-path form before exposing projection.
///
/// The invalid descriptor makes this an operation-free probe: a supported
/// kernel must reject it with EBADF before resolving or changing any file.
pub(super) fn require_metadata_support() -> io::Result<()> {
    // SAFETY: -1 is deliberately invalid and the empty C string remains live.
    let result = unsafe { libc::syscall(FCHMODAT2, -1, c"".as_ptr(), 0, EMPTY_PATH_NOFOLLOW) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EBADF) {
            Ok(())
        } else {
            Err(error)
        }
    } else {
        Err(error(libc::EOPNOTSUPP))
    }
}

pub(super) fn set_mode(pin: &File, mode: u32) -> io::Result<()> {
    if is_path_descriptor(pin)? {
        // fchmodat2 is Linux 6.6+. AT_EMPTY_PATH preserves the O_PATH pin's
        // exact identity without requiring read permission merely to chmod.
        // Both supported Nub Linux architectures use syscall number 452.
        chmod_path_descriptor(pin, mode)
    } else {
        // SAFETY: pin is a live ordinary descriptor and mode is from FUSE.
        if unsafe { libc::fchmod(pin.as_raw_fd(), mode) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

pub(super) fn set_owner(pin: &File, uid: u32, gid: u32) -> io::Result<()> {
    if is_path_descriptor(pin)? {
        // Empty-path fchownat operates on this exact O_PATH object, including
        // a symlink, without reopening procfs or requiring read access.
        let result =
            unsafe { libc::fchownat(pin.as_raw_fd(), c"".as_ptr(), uid, gid, EMPTY_PATH_NOFOLLOW) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    } else {
        // SAFETY: pin is a live descriptor and uid/gid are supplied by FUSE.
        if unsafe { libc::fchown(pin.as_raw_fd(), uid, gid) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

pub(super) fn set_times(pin: &File, times: &[libc::timespec; 2]) -> io::Result<()> {
    if is_path_descriptor(pin)? {
        // Empty-path utimensat targets this exact O_PATH object, including a
        // symlink, without reopening procfs or requiring read access.
        let result = unsafe {
            libc::utimensat(
                pin.as_raw_fd(),
                c"".as_ptr(),
                times.as_ptr(),
                EMPTY_PATH_NOFOLLOW,
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    } else {
        // SAFETY: pin and the fixed two-element timespec array remain live.
        if unsafe { libc::futimens(pin.as_raw_fd(), times.as_ptr()) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Backing {
    pub(super) fn new(root: File) -> io::Result<Self> {
        if !root.metadata()?.is_dir() {
            return Err(error(libc::ENOTDIR));
        }
        // A duplicated caller descriptor must not silently inherit into commands.
        // SAFETY: root is live, and F_SETFD takes an integer flag value.
        if unsafe { libc::fcntl(root.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let backing = Self { root, read: None };
        // Capability preflight: never replace openat2 with a racy pathname walk.
        backing.pin(Path::new("/"))?;
        Ok(backing)
    }

    pub(super) fn with_read_view(root: File, read: File) -> io::Result<Self> {
        let mut backing = Self::new(root)?;
        if !same_object(&backing.root.metadata()?, &read.metadata()?) {
            return Err(error(libc::EINVAL));
        }
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // The mount, not O_RDONLY, is the exported file's metadata ceiling.
        if unsafe { libc::fstatvfs(read.as_raw_fd(), &mut stat) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if stat.f_flag & libc::ST_RDONLY == 0 {
            return Err(error(libc::EROFS));
        }
        if unsafe { libc::fcntl(read.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        backing.read = Some(read);
        Ok(backing)
    }

    pub(super) fn supports_export(&self) -> bool {
        self.read.is_some()
    }

    pub(super) fn pin_authorized(&self, path: &Path, access: FsAccess) -> io::Result<File> {
        let root = if access == FsAccess::Read {
            self.read.as_ref().unwrap_or(&self.root)
        } else {
            &self.root
        };
        Self::open_from(root, path, libc::O_PATH, 0)
    }

    pub(super) fn open(&self, path: &Path, flags: i32, mode: u32) -> io::Result<File> {
        Self::open_from(&self.root, path, flags, mode)
    }

    fn open_from(root: &File, path: &Path, flags: i32, mode: u32) -> io::Result<File> {
        if !path.is_absolute()
            || path.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::CurDir | Component::Prefix(_)
                )
            })
        {
            return Err(error(libc::EINVAL));
        }
        let relative = path.strip_prefix("/").map_err(|_| error(libc::EINVAL))?;
        let relative = if relative.as_os_str().is_empty() {
            OsStr::new(".")
        } else {
            relative.as_os_str()
        };
        let name = CString::new(relative.as_bytes()).map_err(|_| error(libc::EINVAL))?;
        let how = OpenHow {
            flags: (flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
            mode: u64::from(mode),
            // BENEATH + NO_SYMLINKS + NO_MAGICLINKS: a backing lookup never
            // follows host-controlled redirects, even during concurrent renames.
            resolve: 0x08 | 0x04 | 0x02,
        };
        // SAFETY: name and how remain live throughout the syscall; the root is owned.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                root.as_raw_fd(),
                name.as_ptr(),
                &how,
                size_of::<OpenHow>(),
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: successful openat2 returns a fresh owned descriptor.
            Ok(unsafe { File::from_raw_fd(fd as i32) })
        }
    }

    pub(super) fn pin(&self, path: &Path) -> io::Result<File> {
        self.open(path, libc::O_PATH, 0)
    }

    pub(super) fn create_at(
        &self,
        parent: &File,
        name: &OsStr,
        flags: i32,
        mode: u32,
    ) -> io::Result<File> {
        let name = child_name(name)?;
        // An existing final component must return to projected lookup, never
        // acquire authority by being raced into a destructive backing open.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                (flags & !libc::O_TRUNC)
                    | libc::O_CREAT
                    | libc::O_EXCL
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC,
                mode,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    /// Pin exactly one child of a held directory without re-resolving the
    /// directory through the backing root. This protects the parent selected
    /// at validation; it intentionally cannot freeze unrelated host renames
    /// after that parent is acquired.
    pub(super) fn pin_child(&self, parent: &File, name: &OsStr) -> io::Result<File> {
        let name = child_name(name)?;
        // SAFETY: parent is held by the caller; name is one validated, NUL-free
        // component. O_NOFOLLOW pins a final symlink rather than its target.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: successful openat returns a fresh owned descriptor.
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    pub(super) fn mkdir_at(&self, parent: &File, name: &OsStr, mode: u32) -> io::Result<()> {
        let name = child_name(name)?;
        // SAFETY: parent and validated name remain live throughout the call.
        let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn unlink_at(&self, parent: &File, name: &OsStr, directory: bool) -> io::Result<()> {
        let name = child_name(name)?;
        let flags = directory.then_some(libc::AT_REMOVEDIR).unwrap_or(0);
        // SAFETY: parent and validated name remain live throughout the call.
        let result = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn symlink_at(&self, target: &Path, parent: &File, name: &OsStr) -> io::Result<()> {
        let target =
            CString::new(target.as_os_str().as_bytes()).map_err(|_| error(libc::EINVAL))?;
        let name = child_name(name)?;
        // SAFETY: parent, target and validated name remain live throughout the call.
        let result = unsafe { libc::symlinkat(target.as_ptr(), parent.as_raw_fd(), name.as_ptr()) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn rename_at(
        &self,
        old_parent: &File,
        old_name: &OsStr,
        new_parent: &File,
        new_name: &OsStr,
        flags: u32,
    ) -> io::Result<()> {
        let old_name = child_name(old_name)?;
        let new_name = child_name(new_name)?;
        // SAFETY: both parent descriptors and names remain live throughout the call.
        let result = if flags == 0 {
            unsafe {
                libc::renameat(
                    old_parent.as_raw_fd(),
                    old_name.as_ptr(),
                    new_parent.as_raw_fd(),
                    new_name.as_ptr(),
                )
            }
        } else {
            unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    old_parent.as_raw_fd(),
                    old_name.as_ptr(),
                    new_parent.as_raw_fd(),
                    new_name.as_ptr(),
                    flags,
                ) as i32
            }
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn link_at(
        &self,
        old_parent: &File,
        old_name: &OsStr,
        new_parent: &File,
        new_name: &OsStr,
    ) -> io::Result<()> {
        let old_name = child_name(old_name)?;
        let new_name = child_name(new_name)?;
        // SAFETY: both parent descriptors and names remain live throughout the call.
        let result = unsafe {
            libc::linkat(
                old_parent.as_raw_fd(),
                old_name.as_ptr(),
                new_parent.as_raw_fd(),
                new_name.as_ptr(),
                0,
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

pub(super) fn read_link(pin: &File) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; 4096];
    // SAFETY: an empty name addresses the pinned O_PATH symlink itself; the
    // kernel writes at most bytes.len() initialized buffer bytes.
    let size = unsafe {
        libc::readlinkat(
            pin.as_raw_fd(),
            c"".as_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
        )
    };
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    if size as usize == bytes.len() {
        return Err(error(libc::ENAMETOOLONG));
    }
    bytes.truncate(size as usize);
    Ok(bytes)
}

pub(super) fn directory_names(file: File, limit: usize) -> io::Result<Vec<OsString>> {
    use std::os::fd::IntoRawFd;
    let fd = file.into_raw_fd();
    // SAFETY: fd is transferred to DIR on success and closed here on failure.
    let dir = unsafe { libc::fdopendir(fd) };
    if dir.is_null() {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }
    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            // SAFETY: this guard uniquely owns a valid DIR.
            unsafe { libc::closedir(self.0) };
        }
    }
    let dir = Directory(dir);
    let mut names = Vec::new();
    loop {
        // SAFETY: errno is thread-local; dir is live and used on this thread only.
        unsafe { *libc::__errno_location() = 0 };
        let entry = unsafe { libc::readdir(dir.0) };
        if entry.is_null() {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(0) {
                return Err(err);
            }
            break;
        }
        // SAFETY: readdir supplies a NUL-terminated d_name, valid until the next call.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if names.len() == limit {
            return Err(error(libc::EOVERFLOW));
        }
        names.push(OsString::from_vec(name.to_vec()));
    }
    names.sort();
    Ok(names)
}

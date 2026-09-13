use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

pub(super) struct Backing(File);

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
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes.contains(&0)
        || bytes.contains(&b'/')
        || bytes == b"."
        || bytes == b".."
    {
        return Err(error(libc::EINVAL));
    }
    Ok(parent.join(name))
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
        let backing = Self(root);
        // Capability preflight: never replace openat2 with a racy pathname walk.
        backing.pin(Path::new("/"))?;
        Ok(backing)
    }

    pub(super) fn open(&self, path: &Path, flags: i32, mode: u32) -> io::Result<File> {
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
                self.0.as_raw_fd(),
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

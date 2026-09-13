//! Retained user and mount namespace identity for projected Linux launches.
//!
//! The library caller is allowed to already be multithreaded, so it never
//! changes namespaces. A one-shot raw child creates the pair, sends only the
//! two namespace descriptors back, and is synchronously reaped. Later raw
//! launch children enter the user namespace first and then the mount namespace.
//! That order is required by `setns(2)`: joining the user namespace supplies
//! the capabilities needed to join the mount namespace it owns.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(5);
const PACKET_LEN: usize = 8;
const CONTROL_LEN: usize = 64;
const CONTROL_FD: RawFd = 3;
const SUCCESS: u8 = 1;
const FAILURE: u8 = 2;
const ABORT_BUSY: u8 = 1;
const MOUNT_FD: RawFd = 4;
const MOUNT_ATTR_RDONLY: u64 = 0x0000_0001;
const CMSG_ALIGN: usize = std::mem::size_of::<usize>();
const CMSG_DATA_OFFSET: usize = align_cmsg(std::mem::size_of::<libc::cmsghdr>());

const fn align_cmsg(value: usize) -> usize {
    (value + CMSG_ALIGN - 1) & !(CMSG_ALIGN - 1)
}

#[repr(C)]
union Control {
    bytes: [u8; CONTROL_LEN],
    align: libc::cmsghdr,
}

/// Namespace descriptors kept alive by the projected-session owner.
pub(crate) struct NamespacePair {
    pub(super) user: File,
    pub(super) mount: File,
}

/// Precomputed paths used by the raw projected-mount bootstrap child.
///
/// All conversion and allocation happens before `fork`, so the child sees
/// stable C strings and performs only raw syscalls.
pub(crate) struct ProjectionMountPaths {
    source: CString,
    rw: CString,
    read: CString,
    view: CString,
}

impl ProjectionMountPaths {
    pub(crate) fn new(source: &Path, rw: &Path, read: &Path, view: &Path) -> io::Result<Self> {
        Ok(Self {
            source: path_c_string(source)?,
            rw: path_c_string(rw)?,
            read: path_c_string(read)?,
            view: path_c_string(view)?,
        })
    }
}

/// The private FUSE mount and descriptors established by a one-shot child.
///
/// `connection`, `rw_root`, and `read_root` are deliberately movable into the
/// parent-owned server/provider. The retained projected root remains here for
/// raw command launch, and the phased unmount operations retain this owner on
/// failure so cleanup can be retried after moved descriptors have stopped.
pub(crate) struct ProjectedMount {
    namespaces: Option<Arc<NamespacePair>>,
    root: Option<File>,
    connection: Option<OwnedFd>,
    rw_root: Option<File>,
    read_root: Option<File>,
    paths: ProjectionMountPaths,
    view_unmounted: bool,
}

impl ProjectedMount {
    pub(crate) fn namespaces(&self) -> &Arc<NamespacePair> {
        self.namespaces.as_ref().expect("live projected namespace")
    }

    pub(crate) fn root(&self) -> io::Result<&File> {
        self.root
            .as_ref()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))
    }

    pub(crate) fn take_connection(&mut self) -> io::Result<OwnedFd> {
        self.connection
            .take()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))
    }

    pub(crate) fn take_backing_roots(&mut self) -> io::Result<(File, File)> {
        if self.rw_root.is_none() || self.read_root.is_none() {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        let rw = self
            .rw_root
            .take()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
        let read = self
            .read_root
            .take()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
        Ok((rw, read))
    }

    /// Close the projected root and normally unmount the FUSE view. This is
    /// intentionally separate from [`Self::release_backing_namespace`]: the parent FUSE
    /// server cannot join until this mount has gone away.
    pub(crate) fn unmount_view(&mut self) -> io::Result<()> {
        if self.view_unmounted {
            return Ok(());
        }
        self.root.take();
        let UnmountResult::Unmounted =
            unmount_in_pair(self.namespaces(), &self.paths, UnmountPhase::View)?
        else {
            return Err(protocol_error());
        };
        self.view_unmounted = true;
        Ok(())
    }

    /// Force-abort the exact retained FUSE view when ordinary unmount cannot
    /// make progress. A successful forced unmount marks the view gone. An
    /// `EBUSY` result *from the force syscall itself* means FUSE has been
    /// aborted but the view remains mounted, so this returns `Ok(())` and the
    /// caller must join the server then retry [`Self::unmount_view`]. Every
    /// pre-unmount failure remains an error and leaves the owner retained.
    pub(crate) fn abort_view(&mut self) -> io::Result<()> {
        if self.view_unmounted {
            return Ok(());
        }
        self.root.take();
        match unmount_in_pair(self.namespaces(), &self.paths, UnmountPhase::AbortView)? {
            UnmountResult::Unmounted => {
                self.view_unmounted = true;
            }
            UnmountResult::ForceIssuedBusy => (),
        }
        Ok(())
    }

    /// Release the owned namespace and its recursive backing mounts. Call only
    /// after every command/raw opener has been reaped, the FUSE server joined,
    /// and all transferred backing and namespace descriptors have been closed.
    /// Ordinary unmount cannot remove recursive binds with source submounts;
    /// dropping the last namespace reference invokes the kernel's tree teardown.
    pub(crate) fn release_backing_namespace(&mut self) -> io::Result<()> {
        if !self.view_unmounted {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }
        self.connection.take();
        self.rw_root.take();
        self.read_root.take();
        if let Some(namespaces) = self.namespaces.take() {
            match Arc::try_unwrap(namespaces) {
                Ok(pair) => drop(pair),
                Err(shared) => {
                    self.namespaces = Some(shared);
                    return Err(io::Error::from_raw_os_error(libc::EBUSY));
                }
            }
        }
        Ok(())
    }
}

impl NamespacePair {
    /// Create a one-ID user namespace and a private mount namespace in a
    /// short-lived child, retaining only their descriptors in the caller.
    pub(crate) fn create() -> io::Result<Self> {
        Self::create_inner(BootstrapFault::None)
    }

    /// Create the retained namespace pair, bind the supplied backing views,
    /// and mount a private FUSE connection entirely in the one-ID child.
    pub(crate) fn mount_projected(paths: ProjectionMountPaths) -> io::Result<ProjectedMount> {
        let maps = Maps::current();
        let (socket, mut child) = spawn_projected_mount_child(&maps, &paths)?;
        let deadline = Instant::now() + BOOTSTRAP_TIMEOUT;
        let reply = receive_mount_reply(socket.as_raw_fd(), deadline, paths);
        let status = child.reap_until(deadline);
        match (reply, status) {
            (Ok(mount), Ok(0)) => Ok(mount),
            (Ok(mount), Ok(_)) => {
                close_unstarted_mount(mount);
                Err(protocol_error())
            }
            (Ok(mount), Err(error)) => {
                close_unstarted_mount(mount);
                Err(error)
            }
            (Err(error), _) => Err(error),
        }
    }

    /// Enter this pair in a dedicated, single-threaded raw launch child.
    ///
    /// # Safety
    /// The calling thread permanently changes user and mount namespace
    /// membership. It must not share `CLONE_FS` state, must be single-threaded,
    /// and must exit rather than continue normal Rust execution if either step
    /// fails. The caller owns that fork/exec boundary.
    pub(crate) unsafe fn enter(&self) -> io::Result<()> {
        if unsafe { libc::setns(self.user.as_raw_fd(), libc::CLONE_NEWUSER) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::setns(self.mount.as_raw_fd(), libc::CLONE_NEWNS) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn create_inner(fault: BootstrapFault) -> io::Result<Self> {
        // All allocation and identity formatting is deliberately complete
        // before `fork`: the child below uses only raw syscalls and plain data.
        let maps = Maps::current();
        let mut sockets = [0; 2];
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                sockets.as_mut_ptr(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }

        // Capture this before fork: if the parent dies in the small fork race,
        // the child must reject PID 1 rather than treating it as its owner.
        let owner = unsafe { libc::getpid() };
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            unsafe {
                libc::close(sockets[0]);
                libc::close(sockets[1]);
            }
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            unsafe {
                libc::close(sockets[0]);
                bootstrap_child(sockets[1], &maps, fault, owner);
            }
        }

        unsafe {
            libc::close(sockets[1]);
        }
        // SAFETY: the parent now owns this side of the successfully-created pair.
        let socket = unsafe { File::from_raw_fd(sockets[0]) };
        let mut child = BootstrapChild { pid, pidfd: None };
        child.pidfd = Some(pidfd_open(pid)?);
        let deadline = Instant::now() + BOOTSTRAP_TIMEOUT;
        let reply = receive_reply(socket.as_raw_fd(), deadline);
        let status = child.reap_until(deadline);

        match (reply, status) {
            (Ok(pair), Ok(0)) => Ok(pair),
            (Ok(_), Ok(_)) => Err(protocol_error()),
            (Ok(_), Err(error)) => Err(error),
            (Err(error), _) => Err(error),
        }
    }

    #[cfg(test)]
    pub(super) fn create_with_test_fault(fault: BootstrapFault) -> io::Result<Self> {
        Self::create_inner(fault)
    }
}

fn close_unstarted_mount(mount: ProjectedMount) {
    // A six-FD success packet is not accepted until the bootstrap exits zero.
    // This explicit drop is the only serverless cleanup owner on that rejected
    // path: it closes the sole transferred FUSE connection and every held root
    // and namespace descriptor. `BootstrapChild::drop` then kills/reaps a
    // timed-out child, so no private namespace or FUSE endpoint is detached.
    drop(mount);
}

struct Maps {
    uid: Vec<u8>,
    gid: Vec<u8>,
}

impl Maps {
    fn current() -> Self {
        // A direct one-ID map is the unprivileged path. No helper process or
        // host-policy repair participates in setup.
        Self {
            uid: format!("0 {} 1\n", unsafe { libc::geteuid() }).into_bytes(),
            gid: format!("0 {} 1\n", unsafe { libc::getegid() }).into_bytes(),
        }
    }
}

fn spawn_projected_mount_child(
    maps: &Maps,
    paths: &ProjectionMountPaths,
) -> io::Result<(File, BootstrapChild)> {
    let mut sockets = [0; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            sockets.as_mut_ptr(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let owner = unsafe { libc::getpid() };
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(sockets[0]);
            libc::close(sockets[1]);
        }
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        unsafe {
            libc::close(sockets[0]);
            projected_mount_child(sockets[1], maps, paths, owner);
        }
    }
    unsafe {
        libc::close(sockets[1]);
    }
    // SAFETY: the parent owns its socket endpoint after the child endpoint is
    // closed above.
    let socket = unsafe { File::from_raw_fd(sockets[0]) };
    let mut child = BootstrapChild { pid, pidfd: None };
    child.pidfd = Some(pidfd_open(pid)?);
    Ok((socket, child))
}

fn pidfd_open(pid: libc::pid_t) -> io::Result<File> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) as RawFd };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: pidfd_open returned a new descriptor owned by this process.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

struct BootstrapChild {
    pid: libc::pid_t,
    pidfd: Option<File>,
}

impl BootstrapChild {
    fn reap_until(&mut self, deadline: Instant) -> io::Result<i32> {
        let pidfd = self
            .pidfd
            .as_ref()
            .expect("bootstrap child always owns a pidfd")
            .as_raw_fd();
        loop {
            if Instant::now() >= deadline {
                return Err(io::Error::from_raw_os_error(libc::ETIMEDOUT));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let mut descriptor = libc::pollfd {
                fd: pidfd,
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout = remaining.as_millis().min(i32::MAX as u128) as i32;
            let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
            if result == 0 {
                return Err(io::Error::from_raw_os_error(libc::ETIMEDOUT));
            }
            if result < 0 {
                if raw_errno() == libc::EINTR {
                    continue;
                }
                return Err(io::Error::last_os_error());
            }
            if descriptor.revents & libc::POLLIN == 0 {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            loop {
                let mut status = 0;
                let waited = unsafe { libc::waitpid(self.pid, &mut status, 0) };
                if waited == self.pid {
                    self.pid = -1;
                    return Ok(status);
                }
                if waited < 0 && raw_errno() == libc::EINTR {
                    continue;
                }
                return Err(io::Error::last_os_error());
            }
        }
    }
}

impl Drop for BootstrapChild {
    fn drop(&mut self) {
        if self.pid <= 0 {
            return;
        }
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
        }
        loop {
            let mut status = 0;
            let result = unsafe { libc::waitpid(self.pid, &mut status, 0) };
            if result == self.pid || (result < 0 && raw_errno() == libc::ECHILD) {
                break;
            }
            if result < 0 && raw_errno() != libc::EINTR {
                break;
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum BootstrapFault {
    None,
    #[cfg(test)]
    AfterUserNamespace,
}

unsafe fn bootstrap_child(
    socket: RawFd,
    maps: &Maps,
    fault: BootstrapFault,
    owner: libc::pid_t,
) -> ! {
    // Between fork and _exit this is intentionally a straight-line raw-syscall
    // sequence: no allocation, locks, filesystem wrappers, or Rust callbacks.
    let mut control = socket;
    let mut error = unsafe { isolate_control_socket(&mut control) };
    if error == 0 {
        error = unsafe { arm_parent_death(owner) };
    }
    if error == 0 {
        error = unsafe { raw_unshare(libc::CLONE_NEWUSER) };
    }
    if error == 0 {
        error = unsafe { maybe_fault(fault) };
    }
    if error == 0 {
        error = unsafe { raw_write(c"/proc/self/setgroups", b"deny\n") };
    }
    if error == 0 {
        error = unsafe { raw_write(c"/proc/self/uid_map", &maps.uid) };
    }
    if error == 0 {
        error = unsafe { raw_write(c"/proc/self/gid_map", &maps.gid) };
    }
    if error == 0 {
        error = unsafe { raw_unshare(libc::CLONE_NEWNS) };
    }
    if error == 0 {
        error = unsafe {
            raw_checked(libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            ))
        };
    }
    // Mapping can reset the death signal. Re-arm it after the credential
    // transition before exposing namespace descriptors.
    if error == 0 {
        error = unsafe { arm_parent_death(owner) };
    }

    if error == 0 {
        error = unsafe { send_namespace_reply(control) };
    }
    if error != 0 {
        // Preserve the setup error as the child status even when the peer has
        // gone away and the bounded failure packet cannot be delivered.
        let _ = unsafe { send_reply(control, FAILURE, error, None) };
    }
    unsafe {
        libc::close(control);
        libc::_exit(if error == 0 { 0 } else { 1 });
    }
}

unsafe fn projected_mount_child(
    socket: RawFd,
    maps: &Maps,
    paths: &ProjectionMountPaths,
    owner: libc::pid_t,
) -> ! {
    // This has the same post-fork rules as the namespace-only bootstrap. Paths
    // and mount data are all precomputed above; no allocator, lock, or Rust IO
    // wrapper is reachable before `_exit`.
    let mut control = socket;
    let mut error = unsafe { isolate_control_socket(&mut control) };
    if error == 0 {
        error = unsafe { arm_parent_death(owner) };
    }
    if error == 0 {
        error = unsafe { raw_unshare(libc::CLONE_NEWUSER) };
    }
    if error == 0 {
        error = unsafe { raw_write(c"/proc/self/setgroups", b"deny\n") };
    }
    if error == 0 {
        error = unsafe { raw_write(c"/proc/self/uid_map", &maps.uid) };
    }
    if error == 0 {
        error = unsafe { raw_write(c"/proc/self/gid_map", &maps.gid) };
    }
    if error == 0 {
        error = unsafe { raw_unshare(libc::CLONE_NEWNS) };
    }
    if error == 0 {
        error = unsafe {
            raw_checked(libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            ))
        };
    }
    if error == 0 {
        error = unsafe { arm_parent_death(owner) };
    }
    if error == 0 {
        error = unsafe { raw_bind_mount(paths.source.as_ptr(), paths.rw.as_ptr()) };
    }
    if error == 0 {
        // When staging lies beneath source (notably source `/`), the next
        // recursive source snapshot would otherwise copy this private RW bind
        // into the read view. `copy_tree` skips an unbindable child mount, so
        // mark only this bootstrap-owned subtree before that second snapshot.
        error = unsafe { raw_make_unbindable(paths.rw.as_ptr()) };
    }
    if error == 0 {
        error = unsafe { raw_bind_mount(paths.source.as_ptr(), paths.read.as_ptr()) };
    }
    if error == 0 {
        error = unsafe { raw_make_readonly(paths.read.as_ptr()) };
    }
    let mut rw_root = -1;
    let mut read_root = -1;
    let mut fuse = -1;
    let mut root = -1;
    if error == 0 {
        // Normalize the device before opening transferable backing roots.
        // `dup3` closes its destination first, so this ordering makes it
        // impossible for device normalization to replace an already-open RW
        // root even if descriptor-isolation details change later.
        fuse = unsafe { libc::open(c"/dev/fuse".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fuse < 0 {
            error = raw_errno();
        } else if fuse != MOUNT_FD {
            if unsafe { libc::dup3(fuse, MOUNT_FD, libc::O_CLOEXEC) } < 0 {
                error = raw_errno();
            } else {
                unsafe { libc::close(fuse) };
                fuse = MOUNT_FD;
            }
        }
    }
    if error == 0 {
        rw_root = unsafe { raw_open_directory(paths.rw.as_ptr()) };
        if rw_root < 0 {
            error = raw_errno();
        }
    }
    if error == 0 {
        read_root = unsafe { raw_open_directory(paths.read.as_ptr()) };
        if read_root < 0 {
            error = raw_errno();
        }
    }
    if error == 0 {
        error = unsafe {
            raw_checked(libc::mount(
                c"nub-projection".as_ptr(),
                paths.view.as_ptr(),
                c"fuse".as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV,
                c"fd=4,rootmode=40000,user_id=0,group_id=0".as_ptr().cast(),
            ))
        };
    }
    if error == 0 {
        root = unsafe { raw_open_directory(paths.view.as_ptr()) };
        if root < 0 {
            error = raw_errno();
        }
    }
    if error == 0 {
        error = unsafe { send_projected_mount_reply(control, fuse, root, rw_root, read_root) };
    }
    if error != 0 {
        let _ = unsafe { send_reply(control, FAILURE, error, None) };
    }
    unsafe {
        close_if_open(root);
        close_if_open(fuse);
        close_if_open(rw_root);
        close_if_open(read_root);
        libc::close(control);
        libc::_exit(if error == 0 { 0 } else { 1 });
    }
}

unsafe fn raw_bind_mount(source: *const libc::c_char, target: *const libc::c_char) -> i32 {
    unsafe {
        raw_checked(libc::mount(
            source,
            target,
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        ))
    }
}

unsafe fn raw_make_unbindable(target: *const libc::c_char) -> i32 {
    unsafe {
        raw_checked(libc::mount(
            std::ptr::null(),
            target,
            std::ptr::null(),
            libc::MS_REC | libc::MS_UNBINDABLE,
            std::ptr::null(),
        ))
    }
}

#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

unsafe fn raw_make_readonly(target: *const libc::c_char) -> i32 {
    let attr = MountAttr {
        attr_set: MOUNT_ATTR_RDONLY,
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };
    unsafe {
        raw_checked(libc::syscall(
            libc::SYS_mount_setattr,
            libc::AT_FDCWD,
            target,
            libc::AT_RECURSIVE,
            &attr,
            std::mem::size_of::<MountAttr>(),
        ) as libc::c_int)
    }
}

unsafe fn raw_open_directory(path: *const libc::c_char) -> RawFd {
    unsafe { libc::open(path, libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) }
}

unsafe fn close_if_open(fd: RawFd) {
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
}

#[derive(Clone, Copy)]
enum UnmountPhase {
    View,
    AbortView,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum UnmountResult {
    Unmounted,
    ForceIssuedBusy,
}

fn unmount_in_pair(
    pair: &NamespacePair,
    paths: &ProjectionMountPaths,
    phase: UnmountPhase,
) -> io::Result<UnmountResult> {
    let mut sockets = [0; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            sockets.as_mut_ptr(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let owner = unsafe { libc::getpid() };
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(sockets[0]);
            libc::close(sockets[1]);
        }
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        unsafe {
            libc::close(sockets[0]);
            unmount_child(
                sockets[1],
                pair.user.as_raw_fd(),
                pair.mount.as_raw_fd(),
                paths,
                phase,
                owner,
            );
        }
    }
    unsafe { libc::close(sockets[1]) };
    // SAFETY: this is the endpoint retained by the parent after `fork`.
    let socket = unsafe { File::from_raw_fd(sockets[0]) };
    let mut child = BootstrapChild { pid, pidfd: None };
    child.pidfd = Some(pidfd_open(pid)?);
    let deadline = Instant::now() + BOOTSTRAP_TIMEOUT;
    let reply = receive_unmount_reply(socket.as_raw_fd(), deadline);
    let status = child.reap_until(deadline);
    match (reply, status) {
        (Ok(result), Ok(0)) => Ok(result),
        (Ok(_), Ok(_)) => Err(protocol_error()),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), _) => Err(error),
    }
}

unsafe fn unmount_child(
    socket: RawFd,
    user: RawFd,
    mount: RawFd,
    paths: &ProjectionMountPaths,
    phase: UnmountPhase,
    owner: libc::pid_t,
) -> ! {
    // Namespace descriptors must remain open through both setns calls, so
    // descriptor isolation occurs only after joining the retained pair.
    let mut error = unsafe { arm_parent_death(owner) };
    if error == 0 {
        error = unsafe { raw_checked(libc::setns(user, libc::CLONE_NEWUSER)) };
    }
    if error == 0 {
        error = unsafe { raw_checked(libc::setns(mount, libc::CLONE_NEWNS)) };
    }
    let mut control = socket;
    if error == 0 {
        error = unsafe { isolate_control_socket(&mut control) };
    }
    let mut force_busy = false;
    if error == 0 {
        let target = match phase {
            UnmountPhase::View | UnmountPhase::AbortView => paths.view.as_ptr(),
        };
        let flags = if matches!(phase, UnmountPhase::AbortView) {
            libc::MNT_FORCE
        } else {
            0
        };
        let result = unsafe { libc::umount2(target, flags) };
        if result < 0 {
            let errno = raw_errno();
            if matches!(phase, UnmountPhase::AbortView) && errno == libc::EBUSY {
                force_busy = true;
            } else {
                error = errno;
            }
        }
    }
    if error == 0 {
        error = if force_busy {
            unsafe { send_abort_busy_reply(control) }
        } else {
            unsafe { send_reply(control, SUCCESS, 0, None) }
        };
    }
    if error != 0 {
        let _ = unsafe { send_reply(control, FAILURE, error, None) };
    }
    unsafe {
        libc::close(control);
        libc::_exit(if error == 0 { 0 } else { 1 });
    }
}

unsafe fn isolate_control_socket(socket: &mut RawFd) -> i32 {
    // The raw bootstrapper must not retain backing, FUSE, provider, or command
    // descriptors inherited from an already-active session in its parent.
    if *socket != CONTROL_FD {
        if unsafe { libc::dup3(*socket, CONTROL_FD, libc::O_CLOEXEC) } < 0 {
            return raw_errno();
        }
        unsafe {
            libc::close(*socket);
        }
        *socket = CONTROL_FD;
    } else if unsafe { libc::fcntl(CONTROL_FD, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return raw_errno();
    }
    if unsafe { libc::syscall(libc::SYS_close_range, 0u32, 2u32, 0u32) } != 0 {
        return raw_errno();
    }
    if unsafe { libc::syscall(libc::SYS_close_range, 4u32, u32::MAX, 0u32) } != 0 {
        return raw_errno();
    }
    0
}

unsafe fn maybe_fault(fault: BootstrapFault) -> i32 {
    #[cfg(test)]
    if matches!(fault, BootstrapFault::AfterUserNamespace) {
        return libc::EIO;
    }
    let _ = fault;
    0
}

unsafe fn arm_parent_death(parent: libc::pid_t) -> i32 {
    let error = unsafe { raw_checked(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0)) };
    if error != 0 {
        return error;
    }
    if unsafe { libc::getppid() } == parent {
        0
    } else {
        libc::ESRCH
    }
}

unsafe fn raw_unshare(flags: i32) -> i32 {
    unsafe { raw_checked(libc::unshare(flags)) }
}

unsafe fn raw_checked(result: libc::c_int) -> i32 {
    if result < 0 { raw_errno() } else { 0 }
}

unsafe fn raw_write(path: &std::ffi::CStr, bytes: &[u8]) -> i32 {
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return raw_errno();
    }
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    let errno = if written == -1 {
        raw_errno()
    } else {
        libc::EIO
    };
    unsafe {
        libc::close(fd);
    }
    if written == bytes.len() as isize {
        0
    } else {
        errno
    }
}

unsafe fn send_namespace_reply(socket: RawFd) -> i32 {
    let user = unsafe {
        libc::open(
            c"/proc/self/ns/user".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if user < 0 {
        return raw_errno();
    }
    let mount = unsafe {
        libc::open(
            c"/proc/self/ns/mnt".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if mount < 0 {
        let errno = raw_errno();
        unsafe {
            libc::close(user);
        }
        return errno;
    }
    let result = unsafe {
        let result = send_reply(socket, SUCCESS, 0, Some(&[user, mount]));
        libc::close(user);
        libc::close(mount);
        result
    };
    result
}

unsafe fn send_projected_mount_reply(
    socket: RawFd,
    fuse: RawFd,
    root: RawFd,
    rw_root: RawFd,
    read_root: RawFd,
) -> i32 {
    let user = unsafe {
        libc::open(
            c"/proc/self/ns/user".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if user < 0 {
        return raw_errno();
    }
    let mount = unsafe {
        libc::open(
            c"/proc/self/ns/mnt".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if mount < 0 {
        let errno = raw_errno();
        unsafe { libc::close(user) };
        return errno;
    }
    let rights = [user, mount, fuse, root, rw_root, read_root];
    let result = unsafe { send_reply(socket, SUCCESS, 0, Some(&rights)) };
    unsafe {
        libc::close(user);
        libc::close(mount);
    }
    result
}

unsafe fn send_reply(socket: RawFd, status: u8, errno: i32, rights: Option<&[RawFd]>) -> i32 {
    unsafe { send_packet(socket, packet(status, errno), rights) }
}

unsafe fn send_abort_busy_reply(socket: RawFd) -> i32 {
    unsafe {
        send_packet(
            socket,
            packet_with_marker(FAILURE, libc::EBUSY, ABORT_BUSY),
            None,
        )
    }
}

unsafe fn send_packet(socket: RawFd, packet: [u8; PACKET_LEN], rights: Option<&[RawFd]>) -> i32 {
    let mut iov = libc::iovec {
        iov_base: packet.as_ptr().cast_mut().cast(),
        iov_len: packet.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    let mut control = Control {
        bytes: [0; CONTROL_LEN],
    };
    if let Some(rights) = rights {
        message.msg_control = (&mut control as *mut Control).cast();
        message.msg_controllen =
            unsafe { libc::CMSG_SPACE(std::mem::size_of_val(rights) as _) as _ };
        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        if header.is_null() {
            return libc::EPROTO;
        }
        unsafe {
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(rights) as _) as _;
            std::ptr::copy_nonoverlapping(
                rights.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(header),
                std::mem::size_of_val(rights),
            );
        }
    }
    // The peer is private and reads one bounded packet. MSG_NOSIGNAL avoids a
    // parent-close race changing the child-control path into SIGPIPE.
    let sent = loop {
        let sent = unsafe { libc::sendmsg(socket, &message, libc::MSG_NOSIGNAL) };
        if sent < 0 && raw_errno() == libc::EINTR {
            continue;
        }
        break sent;
    };
    if sent == packet.len() as isize {
        0
    } else if sent < 0 {
        raw_errno()
    } else {
        libc::EIO
    }
}

pub(super) fn receive_reply(socket: RawFd, deadline: Instant) -> io::Result<NamespacePair> {
    let rights = receive_fds(socket, deadline, 2)?;
    let [user, mount] = match <[RawFd; 2]>::try_from(rights) {
        Ok(rights) => rights,
        Err(rights) => {
            close_all(&rights);
            return Err(protocol_error());
        }
    };
    // SAFETY: SCM_RIGHTS transferred new descriptors, and MSG_CMSG_CLOEXEC
    // plus the explicit check below keeps them out of later execs.
    let user = unsafe { File::from_raw_fd(user) };
    let mount = unsafe { File::from_raw_fd(mount) };
    set_cloexec(&user)?;
    set_cloexec(&mount)?;
    Ok(NamespacePair { user, mount })
}

fn receive_mount_reply(
    socket: RawFd,
    deadline: Instant,
    paths: ProjectionMountPaths,
) -> io::Result<ProjectedMount> {
    let rights = receive_fds(socket, deadline, 6)?;
    let [user, mount, fuse, root, rw_root, read_root] = match <[RawFd; 6]>::try_from(rights) {
        Ok(rights) => rights,
        Err(rights) => {
            close_all(&rights);
            return Err(protocol_error());
        }
    };
    // SAFETY: every descriptor was installed by SCM_RIGHTS with CLOEXEC.
    let user = unsafe { File::from_raw_fd(user) };
    let mount = unsafe { File::from_raw_fd(mount) };
    let connection = unsafe { OwnedFd::from_raw_fd(fuse) };
    let root = unsafe { File::from_raw_fd(root) };
    let rw_root = unsafe { File::from_raw_fd(rw_root) };
    let read_root = unsafe { File::from_raw_fd(read_root) };
    set_cloexec(&user)?;
    set_cloexec(&mount)?;
    set_cloexec(&connection)?;
    set_cloexec(&root)?;
    set_cloexec(&rw_root)?;
    set_cloexec(&read_root)?;
    Ok(ProjectedMount {
        namespaces: Some(Arc::new(NamespacePair { user, mount })),
        root: Some(root),
        connection: Some(connection),
        rw_root: Some(rw_root),
        read_root: Some(read_root),
        paths,
        view_unmounted: false,
    })
}

fn receive_fds(
    socket: RawFd,
    deadline: Instant,
    expected_right_count: usize,
) -> io::Result<Vec<RawFd>> {
    let (bytes, rights) = receive_packet(socket, deadline)?;
    let reply = match decode_packet_expected(&bytes, rights.len(), expected_right_count) {
        Ok(reply) => reply,
        Err(error) => {
            close_all(&rights);
            return Err(error);
        }
    };
    match reply {
        Reply::Success => Ok(rights),
        Reply::Failure(errno) => {
            close_all(&rights);
            Err(io::Error::from_raw_os_error(errno))
        }
    }
}

fn receive_unmount_reply(socket: RawFd, deadline: Instant) -> io::Result<UnmountResult> {
    let (bytes, rights) = receive_packet(socket, deadline)?;
    if !rights.is_empty() {
        close_all(&rights);
        return Err(protocol_error());
    }
    decode_unmount_packet(&bytes)
}

fn receive_packet(socket: RawFd, deadline: Instant) -> io::Result<([u8; PACKET_LEN], Vec<RawFd>)> {
    wait_readable(socket, deadline)?;
    let mut bytes = [0; PACKET_LEN];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut control = Control {
        bytes: [0; CONTROL_LEN],
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = (&mut control as *mut Control).cast();
    message.msg_controllen = CONTROL_LEN;
    let received = loop {
        let result = unsafe { libc::recvmsg(socket, &mut message, libc::MSG_CMSG_CLOEXEC) };
        if result < 0 && raw_errno() == libc::EINTR {
            continue;
        }
        break result;
    };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }
    // recvmsg installs rights before it reports packet truncation. Parse them
    // first so every later packet-shape rejection can close what arrived.
    let rights = unsafe { received_rights(&message) }?;
    if received != PACKET_LEN as isize
        || message.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0
    {
        close_all(&rights);
        return Err(protocol_error());
    }

    Ok((bytes, rights))
}

unsafe fn received_rights(message: &libc::msghdr) -> io::Result<Vec<RawFd>> {
    let control_len = message.msg_controllen as usize;
    if control_len == 0 {
        return Ok(Vec::new());
    }
    if control_len > CONTROL_LEN {
        return Err(protocol_error());
    }
    let mut rights = Vec::new();
    let mut offset = 0;
    while offset < control_len {
        if control_len - offset < CMSG_DATA_OFFSET {
            close_all(&rights);
            return Err(protocol_error());
        }
        let header = unsafe {
            message
                .msg_control
                .cast::<u8>()
                .add(offset)
                .cast::<libc::cmsghdr>()
        };
        let header_len = unsafe { (*header).cmsg_len as usize };
        if header_len < CMSG_DATA_OFFSET || header_len > control_len - offset {
            close_all(&rights);
            return Err(protocol_error());
        }
        let data_len = header_len - CMSG_DATA_OFFSET;
        let next = align_cmsg(header_len);
        if data_len % std::mem::size_of::<RawFd>() != 0 || next > control_len - offset {
            close_all(&rights);
            return Err(protocol_error());
        }
        let valid = unsafe {
            (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS
        };
        if !valid {
            close_all(&rights);
            return Err(protocol_error());
        }
        for index in 0..data_len / std::mem::size_of::<RawFd>() {
            rights.push(unsafe {
                message
                    .msg_control
                    .cast::<u8>()
                    .add(offset + CMSG_DATA_OFFSET + index * std::mem::size_of::<RawFd>())
                    .cast::<RawFd>()
                    .read_unaligned()
            });
        }
        offset += next;
    }
    Ok(rights)
}

fn close_all(rights: &[RawFd]) {
    for &fd in rights {
        if fd >= 0 {
            unsafe {
                libc::close(fd);
            }
        }
    }
}

fn set_cloexec(file: &impl AsRawFd) -> io::Result<()> {
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn wait_readable(socket: RawFd, deadline: Instant) -> io::Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::from_raw_os_error(libc::ETIMEDOUT));
        }
        let mut descriptor = libc::pollfd {
            fd: socket,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout = remaining.as_millis().min(i32::MAX as u128) as i32;
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        if result > 0 {
            return Ok(());
        }
        if result == 0 {
            return Err(io::Error::from_raw_os_error(libc::ETIMEDOUT));
        }
        if raw_errno() != libc::EINTR {
            return Err(io::Error::last_os_error());
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Reply {
    Success,
    Failure(i32),
}

fn packet(status: u8, errno: i32) -> [u8; PACKET_LEN] {
    packet_with_marker(status, errno, 0)
}

fn packet_with_marker(status: u8, errno: i32, marker: u8) -> [u8; PACKET_LEN] {
    let errno = errno.to_ne_bytes();
    [
        b'N', b'P', status, marker, errno[0], errno[1], errno[2], errno[3],
    ]
}

pub(super) fn decode_packet(bytes: &[u8], right_count: usize) -> io::Result<Reply> {
    decode_packet_expected(bytes, right_count, 2)
}

pub(super) fn decode_packet_expected(
    bytes: &[u8],
    right_count: usize,
    expected_right_count: usize,
) -> io::Result<Reply> {
    if bytes.len() != PACKET_LEN || bytes[..2] != *b"NP" || bytes[3] != 0 {
        return Err(protocol_error());
    }
    let errno = i32::from_ne_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    match bytes[2] {
        SUCCESS if errno == 0 && right_count == expected_right_count => Ok(Reply::Success),
        FAILURE if errno > 0 && right_count == 0 => Ok(Reply::Failure(errno)),
        _ => Err(protocol_error()),
    }
}

pub(super) fn decode_unmount_packet(bytes: &[u8]) -> io::Result<UnmountResult> {
    if bytes.len() != PACKET_LEN || bytes[..2] != *b"NP" {
        return Err(protocol_error());
    }
    let errno = i32::from_ne_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    match (bytes[2], bytes[3], errno) {
        (SUCCESS, 0, 0) => Ok(UnmountResult::Unmounted),
        (FAILURE, ABORT_BUSY, libc::EBUSY) => Ok(UnmountResult::ForceIssuedBusy),
        (FAILURE, 0, errno) if errno > 0 => Err(io::Error::from_raw_os_error(errno)),
        _ => Err(protocol_error()),
    }
}

fn path_c_string(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

fn protocol_error() -> io::Error {
    io::Error::from_raw_os_error(libc::EPROTO)
}

fn raw_errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

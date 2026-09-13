//! Bounded raw child for projection-native opens.
//!
//! The FUSE provider and its backing descriptors stay in the multithreaded parent.  This child
//! is deliberately fork-only code: after `fork` it performs fixed-buffer syscall I/O, enters the
//! already-held namespaces, and has no Rust allocation/locking or provider state to inherit.

use super::super::namespace::NamespacePair;
use super::{Counters, NativeOpenRequest, Projection};
use std::fs::File;
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::time::{Duration, Instant};

const MAX_PATH: usize = 4096;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(1);
const KIND_REQUEST: u32 = 1;
const KIND_READY: u32 = 2;
const KIND_RESULT: u32 = 3;
const KIND_NEEDS_EXPORT: u32 = 4;
const KIND_ACK: u32 = 5;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Frame {
    kind: u32,
    errno: i32,
    path_len: u32,
    has_directory: u32,
    opened: u32,
    _reserved: u32,
    flags: u64,
    mode: u64,
    resolve: u64,
    umask: u32,
    _reserved_end: u32,
}

const FRAME_BYTES: usize = size_of::<Frame>();
const CMSG_ALIGN: usize = size_of::<usize>();
const fn align_cmsg(value: usize) -> usize {
    (value + CMSG_ALIGN - 1) & !(CMSG_ALIGN - 1)
}
const CMSG_DATA_OFFSET: usize = align_cmsg(size_of::<libc::cmsghdr>());
const CMSG_LEN_FD: usize = CMSG_DATA_OFFSET + size_of::<RawFd>();
const CMSG_SPACE_FD: usize = align_cmsg(CMSG_LEN_FD);

#[repr(C, align(8))]
struct CmsgBuffer([u8; CMSG_SPACE_FD]);

pub(super) struct RawOpenStart {
    root: File,
    user: Option<File>,
    mount: Option<File>,
}

impl RawOpenStart {
    pub(super) fn new(root: File, namespaces: Option<&NamespacePair>) -> io::Result<Self> {
        let (user, mount) = match namespaces {
            Some(namespaces) => (
                Some(namespaces.user.try_clone()?),
                Some(namespaces.mount.try_clone()?),
            ),
            None => (None, None),
        };
        Ok(Self { root, user, mount })
    }
}

/// A parent-owned, pidfd-addressed termination path. It is deliberately separate from the
/// request owner so service shutdown can kill a child stalled in FUSE before joining that worker.
pub(super) struct RawOpenTermination {
    control: Option<File>,
    pidfd: File,
}

impl RawOpenTermination {
    fn new(control: File, pid: libc::pid_t) -> io::Result<Self> {
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) as RawFd };
        if pidfd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            control: Some(control),
            // SAFETY: `pidfd_open` returned a fresh descriptor for this child task.
            pidfd: unsafe { File::from_raw_fd(pidfd) },
        })
    }

    pub(super) fn terminate(&mut self) {
        if let Some(control) = self.control.take() {
            // `try_clone` shares the worker endpoint's open-file description. Shutting it down
            // wakes a worker stuck in recvmsg even before SIGKILL tears down the child peer.
            unsafe {
                libc::shutdown(control.as_raw_fd(), libc::SHUT_RDWR);
            }
            drop(control);
        }
        // A pidfd names this exact task until it is reaped; unlike a saved numeric PID it cannot
        // signal a later, unrelated process after PID reuse. ESRCH means the owner already won.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0u32,
            );
        }
    }
}

pub(super) struct RawOpenProcess {
    control: Option<File>,
    pid: libc::pid_t,
}

impl RawOpenProcess {
    pub(super) fn start(start: RawOpenStart) -> io::Result<Self> {
        let mut sockets = [-1; 2];
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
        // Normalize every retained endpoint above stdio before fork. The raw child then closes
        // `0..=2` unconditionally instead of accidentally preserving a control endpoint that a
        // library caller happened to allocate in an empty descriptor table.
        let parent_control = match duplicate(sockets[0]) {
            Ok(fd) => fd,
            Err(error) => {
                close_fd(sockets[0]);
                close_fd(sockets[1]);
                return Err(error);
            }
        };
        let child_control = match duplicate(sockets[1]) {
            Ok(fd) => fd,
            Err(error) => {
                close_fd(parent_control);
                close_fd(sockets[0]);
                close_fd(sockets[1]);
                return Err(error);
            }
        };
        close_fd(sockets[0]);
        close_fd(sockets[1]);
        sockets = [parent_control, child_control];
        let root_fd = match duplicate(start.root.as_raw_fd()) {
            Ok(fd) => fd,
            Err(error) => {
                close_fd(sockets[0]);
                close_fd(sockets[1]);
                return Err(error);
            }
        };
        let user_fd = match start.user.as_ref() {
            Some(user) => match duplicate(user.as_raw_fd()) {
                Ok(fd) => fd,
                Err(error) => {
                    close_fd(root_fd);
                    close_fd(sockets[0]);
                    close_fd(sockets[1]);
                    return Err(error);
                }
            },
            None => -1,
        };
        let mount_fd = match start.mount.as_ref() {
            Some(mount) => match duplicate(mount.as_raw_fd()) {
                Ok(fd) => fd,
                Err(error) => {
                    close_fd(root_fd);
                    close_fd(user_fd);
                    close_fd(sockets[0]);
                    close_fd(sockets[1]);
                    return Err(error);
                }
            },
            None => -1,
        };
        let parent_pid = unsafe { libc::getpid() };
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            close_fd(root_fd);
            close_fd(user_fd);
            close_fd(mount_fd);
            close_fd(sockets[0]);
            close_fd(sockets[1]);
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            close_fd(sockets[0]);
            raw_child(sockets[1], root_fd, user_fd, mount_fd, parent_pid);
        }
        close_fd(sockets[1]);
        close_fd(root_fd);
        close_fd(user_fd);
        close_fd(mount_fd);
        let process = Self {
            // SAFETY: this is the sole remaining parent endpoint.
            control: Some(unsafe { File::from_raw_fd(sockets[0]) }),
            pid,
        };
        if let Err(error) = process.await_ready() {
            drop(process);
            return Err(error);
        }
        Ok(process)
    }

    pub(super) fn tid(&self) -> u32 {
        self.pid as u32
    }

    pub(super) fn termination_handle(&self) -> io::Result<RawOpenTermination> {
        let control = self.control()?.try_clone()?;
        RawOpenTermination::new(control, self.pid)
    }

    /// Parent-side state owns export arming and extraction.  It never retains the projection
    /// mutex while it is blocked in the control protocol or in the child FUSE ioctl.
    pub(super) fn open(
        &mut self,
        request: &NativeOpenRequest,
        projection: &Projection,
        counters: &Counters,
    ) -> io::Result<File> {
        let path = request.path.as_bytes_with_nul();
        if path.len() > MAX_PATH {
            return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
        }
        let request_frame = Frame {
            kind: KIND_REQUEST,
            path_len: path.len() as u32,
            has_directory: u32::from(request.directory.is_some()),
            flags: request.flags | libc::O_CLOEXEC as u64,
            mode: request.mode,
            resolve: request.resolve.unwrap_or(u64::MAX),
            umask: request.umask,
            ..Frame::default()
        };
        send_frame(
            self.control()?.as_raw_fd(),
            &request_frame,
            path,
            request.directory.as_ref().map(AsRawFd::as_raw_fd),
        )?;
        let (first, first_fd) = recv_frame(self.control()?.as_raw_fd())?;
        match first.kind {
            KIND_RESULT => finish_direct(first, first_fd, counters),
            KIND_NEEDS_EXPORT => {
                close_optional(first_fd);
                if first.opened != 0 {
                    counters
                        .opened
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                let arm = ArmedExport::arm(projection);
                let arm_errno = arm
                    .as_ref()
                    .err()
                    .and_then(io::Error::raw_os_error)
                    .unwrap_or(0);
                let ack = Frame {
                    kind: KIND_ACK,
                    errno: arm_errno,
                    ..Frame::default()
                };
                send_frame(self.control()?.as_raw_fd(), &ack, &[], None)?;
                let (result, result_fd) = recv_frame(self.control()?.as_raw_fd())?;
                close_optional(result_fd);
                if result.kind != KIND_RESULT {
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
                // `take_export` clears the one-shot slot on every terminal response, including
                // an ioctl failure that never reached FUSE.  Do this after the wait, not while
                // holding its mutex across it.
                let exported = match arm {
                    Ok(armed) => armed.take(),
                    Err(error) => Err(error),
                };
                if result.errno != 0 {
                    return Err(io::Error::from_raw_os_error(result.errno));
                }
                let file = exported?;
                counters
                    .exported
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(file)
            }
            _ => {
                close_optional(first_fd);
                Err(io::Error::from_raw_os_error(libc::EIO))
            }
        }
    }

    fn await_ready(&self) -> io::Result<()> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let mut pollfd = libc::pollfd {
            fd: self.control()?.as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::from_raw_os_error(libc::ETIMEDOUT));
            }
            let timeout = remaining.as_millis().min(i32::MAX as u128) as i32;
            let rc = unsafe { libc::poll(&mut pollfd, 1, timeout) };
            if rc > 0 {
                break;
            }
            if rc == 0 {
                continue;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        let (frame, fd) = recv_frame(self.control()?.as_raw_fd())?;
        close_optional(fd);
        if frame.kind != KIND_READY {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        if frame.errno != 0 {
            return Err(io::Error::from_raw_os_error(frame.errno));
        }
        Ok(())
    }

    fn terminate(&mut self) {
        // A stuck FUSE ioctl must not strand this request process or hold a namespace alive.
        // Closing the peer asks it to exit; SIGKILL bounds the parent-side shutdown path.
        drop(self.control.take());
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
        }
        wait_reap(self.pid);
    }

    fn control(&self) -> io::Result<&File> {
        self.control
            .as_ref()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ECANCELED))
    }
}

/// Clears a parent-owned one-shot export slot on every control-path exit. A failed transport
/// cannot leave `armed` set and turn all later native opens into `EBUSY`.
struct ArmedExport<'a> {
    projection: &'a Projection,
    armed: bool,
}

impl<'a> ArmedExport<'a> {
    fn arm(projection: &'a Projection) -> io::Result<Self> {
        projection.arm_export()?;
        Ok(Self {
            projection,
            armed: true,
        })
    }

    fn take(mut self) -> io::Result<File> {
        self.armed = false;
        self.projection.take_export()
    }
}

impl Drop for ArmedExport<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.projection.take_export();
        }
    }
}

impl Drop for RawOpenProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn finish_direct(frame: Frame, fd: Option<RawFd>, counters: &Counters) -> io::Result<File> {
    if frame.opened != 0 {
        counters
            .opened
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    if frame.errno != 0 {
        close_optional(fd);
        return Err(io::Error::from_raw_os_error(frame.errno));
    }
    let fd = fd.ok_or_else(|| io::Error::from_raw_os_error(libc::EIO))?;
    // SAFETY: SCM_RIGHTS produced a fresh owned descriptor for this process.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn duplicate(fd: RawFd) -> io::Result<RawFd> {
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(duplicate)
    }
}

fn raw_child(control: RawFd, root: RawFd, user: RawFd, mount: RawFd, parent_pid: libc::pid_t) -> ! {
    let setup = unsafe { raw_child_setup(control, root, user, mount, parent_pid) };
    match setup {
        Ok(()) => {
            let ready = Frame {
                kind: KIND_READY,
                ..Frame::default()
            };
            if send_frame(control, &ready, &[], None).is_err() {
                unsafe { libc::_exit(127) };
            }
        }
        Err(errno) => {
            let ready = Frame {
                kind: KIND_READY,
                errno,
                ..Frame::default()
            };
            let _ = send_frame(control, &ready, &[], None);
            unsafe { libc::_exit(127) };
        }
    }
    loop {
        if unsafe { raw_one_request(control) }.is_err() {
            unsafe { libc::_exit(0) };
        }
    }
}

/// All callers are post-fork. This is deliberately raw syscall-only code: no allocation, locks,
/// `File` construction, or Rust callbacks occur before the persistent control loop starts.
unsafe fn raw_child_setup(
    control: RawFd,
    root: RawFd,
    user: RawFd,
    mount: RawFd,
    parent_pid: libc::pid_t,
) -> Result<(), i32> {
    unsafe { arm_parent_death(parent_pid) }?;
    let mut keep = [control, root, user, mount];
    if unsafe { close_all_except(&mut keep) } != 0 {
        return Err(errno());
    }
    if user >= 0 && unsafe { libc::syscall(libc::SYS_setns, user, libc::CLONE_NEWUSER) } != 0 {
        return Err(errno());
    }
    if mount >= 0 && unsafe { libc::syscall(libc::SYS_setns, mount, libc::CLONE_NEWNS) } != 0 {
        return Err(errno());
    }
    // The direct owner/eUID namespace route is expected to retain the signal, but credential
    // transitions elsewhere can reset it. Re-arm and re-check defensively after both joins,
    // before the child keeps its projected-root control capability.
    unsafe { arm_parent_death(parent_pid) }?;
    if unsafe { libc::syscall(libc::SYS_fchdir, root) } != 0
        || unsafe { libc::syscall(libc::SYS_chroot, c".".as_ptr()) } != 0
        || unsafe { libc::syscall(libc::SYS_chdir, c"/".as_ptr()) } != 0
    {
        return Err(errno());
    }
    close_fd(root);
    close_fd(user);
    close_fd(mount);
    // This helper is explicitly fork-safe and consists of raw `prctl`/`capset` syscalls.
    if unsafe { super::super::super::linux_landlock::drop_all_capabilities() }.is_err() {
        return Err(errno());
    }
    Ok(())
}

unsafe fn arm_parent_death(parent_pid: libc::pid_t) -> Result<(), i32> {
    if unsafe { libc::syscall(libc::SYS_prctl, libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
        return Err(errno());
    }
    if unsafe { libc::getppid() } != parent_pid {
        return Err(libc::ECHILD);
    }
    Ok(())
}

unsafe fn raw_one_request(control: RawFd) -> Result<(), i32> {
    let mut bytes = [0u8; FRAME_BYTES + MAX_PATH];
    let (received, directory) = match recv_packet(control, &mut bytes) {
        Ok(value) => value,
        Err(error) => return Err(error.raw_os_error().unwrap_or(libc::EIO)),
    };
    if received < FRAME_BYTES {
        close_optional(directory);
        return Err(libc::EPROTO);
    }
    // SAFETY: the leading bytes are exactly `Frame` sized and Frame contains only integers.
    let request = unsafe { (bytes.as_ptr().cast::<Frame>()).read_unaligned() };
    if request.kind != KIND_REQUEST
        || request.path_len == 0
        || request.path_len as usize > MAX_PATH
        || (request.has_directory != 0) != directory.is_some()
        || received != FRAME_BYTES + request.path_len as usize
    {
        close_optional(directory);
        return Err(libc::EPROTO);
    }
    let path = &bytes[FRAME_BYTES..received];
    if path.last() != Some(&0) || path[..path.len() - 1].contains(&0) {
        close_optional(directory);
        return Err(libc::EPROTO);
    }
    let dirfd = directory.unwrap_or(libc::AT_FDCWD);
    unsafe { libc::syscall(libc::SYS_umask, request.umask) };
    let opened = unsafe {
        if request.resolve != u64::MAX {
            let how = OpenHow {
                flags: request.flags,
                mode: request.mode,
                resolve: request.resolve,
            };
            libc::syscall(
                libc::SYS_openat2,
                dirfd,
                path.as_ptr(),
                &how as *const OpenHow,
                size_of::<OpenHow>(),
            ) as RawFd
        } else {
            libc::syscall(
                libc::SYS_openat,
                dirfd,
                path.as_ptr(),
                request.flags as i32,
                request.mode as libc::mode_t,
            ) as RawFd
        }
    };
    close_optional(directory);
    if opened < 0 {
        return send_result(control, errno(), false, None);
    }
    let mut stat: libc::stat = unsafe { MaybeUninit::zeroed().assume_init() };
    if unsafe { libc::syscall(libc::SYS_fstat, opened, &mut stat as *mut libc::stat) } != 0 {
        let error = errno();
        close_fd(opened);
        return send_result(control, error, true, None);
    }
    if request.flags & libc::O_PATH as u64 != 0 || stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
        let result = send_result(control, 0, true, Some(opened));
        close_fd(opened);
        return result;
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        close_fd(opened);
        return send_result(control, libc::EACCES, true, None);
    }
    let needs_export = Frame {
        kind: KIND_NEEDS_EXPORT,
        opened: 1,
        ..Frame::default()
    };
    if let Err(error) = send_frame(control, &needs_export, &[], None) {
        close_fd(opened);
        return Err(error.raw_os_error().unwrap_or(libc::EIO));
    }
    let (ack, extra) = match recv_frame(control) {
        Ok(value) => value,
        Err(error) => {
            close_fd(opened);
            return Err(error.raw_os_error().unwrap_or(libc::EIO));
        }
    };
    close_optional(extra);
    if ack.kind != KIND_ACK {
        close_fd(opened);
        return Err(libc::EPROTO);
    }
    if ack.errno != 0 {
        close_fd(opened);
        return send_result(control, ack.errno, true, None);
    }
    let rc = unsafe { libc::syscall(libc::SYS_ioctl, opened, super::EXPORT_IOCTL, 0usize) };
    let result = if rc < 0 { errno() } else { 0 };
    close_fd(opened);
    send_result(control, result, true, None)
}

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

fn send_result(control: RawFd, errno: i32, opened: bool, passed: Option<RawFd>) -> Result<(), i32> {
    let result = Frame {
        kind: KIND_RESULT,
        errno,
        opened: u32::from(opened),
        ..Frame::default()
    };
    send_frame(control, &result, &[], passed)
        .map_err(|error| error.raw_os_error().unwrap_or(libc::EIO))
}

fn send_frame(fd: RawFd, frame: &Frame, path: &[u8], passed: Option<RawFd>) -> io::Result<()> {
    let mut header = *frame;
    header.path_len = path.len() as u32;
    let mut iov = [
        libc::iovec {
            iov_base: (&mut header as *mut Frame).cast(),
            iov_len: FRAME_BYTES,
        },
        libc::iovec {
            iov_base: path.as_ptr().cast_mut().cast(),
            iov_len: path.len(),
        },
    ];
    let mut control = CmsgBuffer([0; CMSG_SPACE_FD]);
    let mut message: libc::msghdr = unsafe { MaybeUninit::zeroed().assume_init() };
    message.msg_iov = iov.as_mut_ptr();
    message.msg_iovlen = if path.is_empty() { 1 } else { 2 };
    if let Some(passed) = passed {
        let header = control.0.as_mut_ptr().cast::<libc::cmsghdr>();
        unsafe {
            (*header).cmsg_len = CMSG_LEN_FD;
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (control.0.as_mut_ptr().add(CMSG_DATA_OFFSET).cast::<RawFd>()).write_unaligned(passed);
        }
        message.msg_control = control.0.as_mut_ptr().cast();
        message.msg_controllen = CMSG_SPACE_FD;
    }
    loop {
        let sent = unsafe { libc::sendmsg(fd, &message, libc::MSG_NOSIGNAL) };
        if sent == (FRAME_BYTES + path.len()) as isize {
            return Ok(());
        }
        if sent < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(if sent < 0 {
            io::Error::last_os_error()
        } else {
            io::Error::from_raw_os_error(libc::EIO)
        });
    }
}

fn recv_frame(fd: RawFd) -> io::Result<(Frame, Option<RawFd>)> {
    let mut bytes = [0u8; FRAME_BYTES];
    let (received, directory) = recv_packet(fd, &mut bytes)?;
    if received != FRAME_BYTES {
        close_optional(directory);
        return Err(io::Error::from_raw_os_error(libc::EPROTO));
    }
    // SAFETY: the byte array is exactly `Frame` sized and Frame contains only integer fields.
    let frame = unsafe { (bytes.as_ptr().cast::<Frame>()).read_unaligned() };
    Ok((frame, directory))
}

fn recv_packet(fd: RawFd, bytes: &mut [u8]) -> io::Result<(usize, Option<RawFd>)> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut control = CmsgBuffer([0; CMSG_SPACE_FD]);
    let mut message: libc::msghdr = unsafe { MaybeUninit::zeroed().assume_init() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.0.as_mut_ptr().cast();
    message.msg_controllen = CMSG_SPACE_FD;
    loop {
        let received = unsafe { libc::recvmsg(fd, &mut message, libc::MSG_CMSG_CLOEXEC) };
        if received == 0 {
            return Err(io::Error::from_raw_os_error(libc::EPIPE));
        }
        if received < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
            return Err(io::Error::from_raw_os_error(libc::EMSGSIZE));
        }
        let passed = if message.msg_controllen == 0 {
            None
        } else if message.msg_controllen >= CMSG_LEN_FD && message.msg_controllen <= CMSG_SPACE_FD {
            let header = control.0.as_ptr().cast::<libc::cmsghdr>();
            let valid = unsafe {
                (*header).cmsg_len == CMSG_LEN_FD
                    && (*header).cmsg_level == libc::SOL_SOCKET
                    && (*header).cmsg_type == libc::SCM_RIGHTS
            };
            if !valid {
                let received = unsafe {
                    (control.0.as_ptr().add(CMSG_DATA_OFFSET).cast::<RawFd>()).read_unaligned()
                };
                close_fd(received);
                return Err(io::Error::from_raw_os_error(libc::EPROTO));
            }
            Some(unsafe {
                (control.0.as_ptr().add(CMSG_DATA_OFFSET).cast::<RawFd>()).read_unaligned()
            })
        } else {
            return Err(io::Error::from_raw_os_error(libc::EPROTO));
        };
        return Ok((received as usize, passed));
    }
}

unsafe fn close_all_except(keep: &mut [RawFd; 4]) -> i32 {
    // A small insertion sort stays in fixed stack storage and makes the close-range gaps exact.
    for index in 1..keep.len() {
        let value = keep[index];
        let mut cursor = index;
        while cursor > 0 && keep[cursor - 1] > value {
            keep[cursor] = keep[cursor - 1];
            cursor -= 1;
        }
        keep[cursor] = value;
    }
    // The child has no stdio or inherited provider/backing endpoint: control is the sole
    // surviving communication capability after this point.
    let mut start = 0u32;
    for index in 0..keep.len() {
        if keep[index] < 3 {
            continue;
        }
        let fd = keep[index] as u32;
        if start < fd && unsafe { libc::syscall(libc::SYS_close_range, start, fd - 1, 0u32) } != 0 {
            return -1;
        }
        start = fd.saturating_add(1);
    }
    if unsafe { libc::syscall(libc::SYS_close_range, start, u32::MAX, 0u32) } != 0 {
        return -1;
    }
    0
}

fn close_optional(fd: Option<RawFd>) {
    if let Some(fd) = fd {
        close_fd(fd);
    }
}

fn close_fd(fd: RawFd) {
    if fd >= 0 {
        unsafe {
            libc::close(fd);
        }
    }
}

fn wait_reap(pid: libc::pid_t) {
    loop {
        let result = unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        if result == pid
            || (result < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
        {
            return;
        }
        if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
    }
}

fn errno() -> i32 {
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_is_fixed_and_path_bound_is_enforced() {
        assert_eq!(FRAME_BYTES, 56);
        assert!(MAX_PATH >= 4096);
        let frame = Frame {
            kind: KIND_REQUEST,
            path_len: MAX_PATH as u32,
            ..Frame::default()
        };
        assert_eq!(frame.path_len as usize, MAX_PATH);
    }

    #[test]
    fn control_rights_buffer_has_one_fd_capacity() {
        assert_eq!(CMSG_LEN_FD, CMSG_DATA_OFFSET + size_of::<RawFd>());
        assert!(CMSG_SPACE_FD >= CMSG_LEN_FD);
    }

    #[test]
    fn seqpacket_returns_one_cloexec_descriptor() {
        let mut sockets = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    sockets.as_mut_ptr(),
                )
            },
            0
        );
        let passed = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        assert!(passed >= 0);
        let frame = Frame {
            kind: KIND_RESULT,
            opened: 1,
            ..Frame::default()
        };
        send_frame(sockets[0], &frame, &[], Some(passed)).unwrap();
        let (received, descriptor) = recv_frame(sockets[1]).unwrap();
        assert_eq!(received.kind, KIND_RESULT);
        let descriptor = descriptor.expect("SCM_RIGHTS descriptor");
        assert_ne!(
            unsafe { libc::fcntl(descriptor, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        close_fd(descriptor);
        close_fd(passed);
        close_fd(sockets[0]);
        close_fd(sockets[1]);
    }

    #[test]
    fn cleanup_closes_every_unretained_descriptor_after_fork() {
        let mut pipe = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            let mut keep = [pipe[0], -1, -1, -1];
            let closed = unsafe { close_all_except(&mut keep) } == 0
                && unsafe { libc::fcntl(pipe[0], libc::F_GETFD) } >= 0
                && unsafe { libc::fcntl(pipe[1], libc::F_GETFD) } < 0;
            unsafe { libc::_exit(if closed { 0 } else { 1 }) };
        }
        close_fd(pipe[0]);
        close_fd(pipe[1]);
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
    }

    #[test]
    fn pidfd_termination_precedes_worker_join_for_stalled_child() {
        let mut sockets = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    sockets.as_mut_ptr(),
                )
            },
            0
        );
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            close_fd(sockets[0]);
            loop {
                unsafe { libc::pause() };
            }
        }
        close_fd(sockets[1]);
        let control = unsafe { File::from_raw_fd(sockets[0]) };
        let mut termination = RawOpenTermination::new(control, child).unwrap();
        termination.terminate();
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFSIGNALED(status));
        assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
    }

    #[test]
    fn pdeathsig_tracks_the_creating_thread_not_only_the_process() {
        let (reported, child_pid) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let mut ready = [-1; 2];
            assert_eq!(
                unsafe { libc::pipe2(ready.as_mut_ptr(), libc::O_CLOEXEC) },
                0
            );
            let child = unsafe { libc::fork() };
            assert!(child >= 0);
            if child == 0 {
                close_fd(ready[0]);
                let parent = unsafe { libc::getppid() };
                if unsafe { arm_parent_death(parent) }.is_err() {
                    unsafe { libc::_exit(1) };
                }
                let marker = [1u8];
                if unsafe { libc::write(ready[1], marker.as_ptr().cast(), marker.len()) } != 1 {
                    unsafe { libc::_exit(1) };
                }
                loop {
                    unsafe { libc::pause() };
                }
            }
            close_fd(ready[1]);
            let mut marker = [0u8];
            assert_eq!(
                unsafe { libc::read(ready[0], marker.as_mut_ptr().cast(), 1) },
                1
            );
            close_fd(ready[0]);
            reported.send(child).unwrap();
            // Returning terminates the creating thread while this process remains alive.
        });
        let child = child_pid.recv().unwrap();
        worker.join().unwrap();
        let mut status = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) } == child {
                break;
            }
            if std::time::Instant::now() >= deadline {
                unsafe {
                    libc::kill(child, libc::SIGKILL);
                    libc::waitpid(child, &mut status, 0);
                }
                panic!("PDEATHSIG did not follow the creating worker thread");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(libc::WIFSIGNALED(status));
        assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
    }
}

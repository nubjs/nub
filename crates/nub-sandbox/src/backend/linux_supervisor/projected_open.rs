//! Test-only native-file delivery through the actual projected kernel namespace.
use super::super::linux_projection::{NativeOpenClient, NativeOpenRequest};
use super::*;
use std::fs::File;
use std::io::Read;

pub(crate) struct ProjectedLaunch {
    pub root: CString,
    pub opener: NativeOpenClient,
}

pub(super) fn notifier() -> Vec<seccompiler::sock_filter> {
    let mut program = vec![
        stmt(BPF_LD | BPF_W | BPF_ABS, OFF_ARCH),
        jump(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_NATIVE, 1, 0),
        stmt(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        stmt(BPF_LD | BPF_W | BPF_ABS, OFF_NR),
    ];
    #[cfg(target_arch = "x86_64")]
    program.extend([
        jump(BPF_JMP | BPF_JGE | BPF_K, 0x4000_0000, 0, 1),
        stmt(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
    ]);
    // These test capabilities are absent, not delegated to a host pathname or
    // fd-import broker. Production notifier behavior is unchanged.
    for syscall in [
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_open_by_handle_at,
        libc::SYS_open_tree,
        libc::SYS_move_mount,
        libc::SYS_fsopen,
        libc::SYS_fsmount,
        libc::SYS_mount_setattr,
    ] {
        program.extend([
            jump(BPF_JMP | BPF_JEQ | BPF_K, syscall as u32, 0, 1),
            stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | libc::EPERM as u32),
        ]);
    }
    for syscall in open_syscalls() {
        program.extend([
            jump(BPF_JMP | BPF_JEQ | BPF_K, syscall as u32, 0, 1),
            stmt(BPF_RET | BPF_K, SECCOMP_RET_USER_NOTIF),
        ]);
    }
    program.extend([
        jump(BPF_JMP | BPF_JEQ | BPF_K, libc::SYS_socket as u32, 0, 4),
        stmt(BPF_LD | BPF_W | BPF_ABS, OFF_ARG0),
        jump(BPF_JMP | BPF_JEQ | BPF_K, libc::AF_UNIX as u32, 0, 1),
        stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | libc::EPERM as u32),
        stmt(BPF_LD | BPF_W | BPF_ABS, OFF_NR),
    ]);
    // Only scalar descriptor controls are needed by this fixture. In particular,
    // no filesystem-specific native mutation ioctl is a generic passthrough.
    program.push(jump(
        BPF_JMP | BPF_JEQ | BPF_K,
        libc::SYS_ioctl as u32,
        0,
        11,
    ));
    program.push(stmt(BPF_LD | BPF_W | BPF_ABS, 24));
    for command in [
        libc::FIONREAD as u32,
        libc::FIONBIO as u32,
        libc::FIOCLEX as u32,
        libc::FIONCLEX as u32,
    ] {
        program.extend([
            jump(BPF_JMP | BPF_JEQ | BPF_K, command, 0, 1),
            stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        ]);
    }
    program.push(stmt(
        BPF_RET | BPF_K,
        SECCOMP_RET_ERRNO | libc::EPERM as u32,
    ));
    // Restore nr after the ioctl argument branch before the common network filter.
    program.push(stmt(BPF_LD | BPF_W | BPF_ABS, OFF_NR));
    program.extend(notifier_program(false, false));
    program
}

fn open_syscalls() -> Vec<libc::c_long> {
    #[cfg(target_arch = "x86_64")]
    {
        vec![
            libc::SYS_openat,
            libc::SYS_openat2,
            libc::SYS_open,
            libc::SYS_creat,
        ]
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        vec![libc::SYS_openat, libc::SYS_openat2]
    }
}

fn is_open(nr: libc::c_long) -> bool {
    nr == libc::SYS_openat || nr == libc::SYS_openat2 || {
        #[cfg(target_arch = "x86_64")]
        {
            nr == libc::SYS_open || nr == libc::SYS_creat
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            false
        }
    }
}

fn scalar_path_only(req: &SeccompNotif) -> bool {
    let nr = req.data.nr as libc::c_long;
    let flags = if nr == libc::SYS_openat {
        req.data.args[2] as u32
    } else {
        #[cfg(target_arch = "x86_64")]
        if nr == libc::SYS_open {
            return req.data.args[1] as u32 & libc::O_PATH as u32 != 0;
        }
        return false;
    };
    flags & libc::O_PATH as u32 != 0
}

fn open_task_path(task: &File, path: &CString, flags: i32) -> io::Result<File> {
    let fd = unsafe { libc::openat(task.as_raw_fd(), path.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn capture(req: &SeccompNotif, client: &NativeOpenClient) -> io::Result<NativeOpenRequest> {
    let mem = open_child_mem(req.pid)?;
    let args = req.data.args;
    let nr = req.data.nr as libc::c_long;
    let mut dirfd = libc::AT_FDCWD;
    let (pointer, flags, mode, resolve) = if nr == libc::SYS_openat2 {
        if args[3] < 24 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        if args[3] > 4096 {
            return Err(io::Error::from_raw_os_error(libc::E2BIG));
        }
        let mut bytes = vec![0; args[3] as usize];
        child_pread(mem.as_raw_fd(), args[2], &mut bytes).map_err(io::Error::from_raw_os_error)?;
        if bytes[24..].iter().any(|byte| *byte != 0) {
            return Err(io::Error::from_raw_os_error(libc::E2BIG));
        }
        let word = |start| u64::from_ne_bytes(bytes[start..start + 8].try_into().unwrap());
        dirfd = args[0] as i32;
        (args[1], word(0), word(8), Some(word(16)))
    } else if nr == libc::SYS_openat {
        dirfd = args[0] as i32;
        (args[1], args[2] as u32 as u64, args[3] as u32 as u64, None)
    } else {
        #[cfg(target_arch = "x86_64")]
        {
            if nr == libc::SYS_creat {
                (
                    args[0],
                    (libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC) as u64,
                    args[1] as u32 as u64,
                    None,
                )
            } else {
                (args[0], args[1] as u32 as u64, args[2] as u32 as u64, None)
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            return Err(io::Error::from_raw_os_error(libc::ENOSYS));
        }
    };
    if flags & libc::O_TMPFILE as u64 == libc::O_TMPFILE as u64 {
        return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP));
    }
    if flags & libc::O_PATH as u64 != 0 {
        // ADDFD uses fget(), which excludes O_PATH. Unlike scalar flags,
        // open_how is mutable: continuing it could deliver an ordinary FUSE
        // file instead of a native descriptor. This route is still incomplete.
        return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP));
    }
    // Byte reads stop at NUL, including a string ending at a mapping boundary.
    // The bounded copied value is the only pathname used after this point.
    let mut path = Vec::with_capacity(128);
    for offset in 0..4096u64 {
        let mut byte = [0];
        let address = pointer
            .checked_add(offset)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?;
        child_pread(mem.as_raw_fd(), address, &mut byte).map_err(io::Error::from_raw_os_error)?;
        if byte[0] == 0 {
            break;
        }
        path.push(byte[0]);
    }
    if path.len() == 4096 {
        return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
    }
    let path = CString::new(path).map_err(io::Error::other)?;
    let task_name = CString::new(format!("/proc/{}", req.pid)).map_err(io::Error::other)?;
    let fd = unsafe {
        libc::open(
            task_name.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let task = unsafe { File::from_raw_fd(fd) };
    let mut status = String::new();
    open_task_path(&task, &CString::new("status").unwrap(), libc::O_RDONLY)?
        .take(65536)
        .read_to_string(&mut status)?;
    let umask = status
        .lines()
        .find_map(|line| line.strip_prefix("Umask:\t"))
        .and_then(|mask| u32::from_str_radix(mask.trim(), 8).ok())
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EACCES))?;
    for (key, expected) in [
        ("Uid:", unsafe { libc::geteuid() }),
        ("Gid:", unsafe { libc::getegid() }),
    ] {
        let ids: Vec<_> = status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EACCES))?
            .split_whitespace()
            .collect();
        if ids.len() != 4
            || ids
                .iter()
                .any(|id| id.parse::<u32>().ok() != Some(expected))
        {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }
    }
    let needs_dir =
        !path.as_bytes().starts_with(b"/") || resolve.is_some_and(|r| r & (0x08 | 0x10) != 0);
    let directory = if needs_dir {
        let name = if dirfd == libc::AT_FDCWD {
            "cwd".to_owned()
        } else {
            format!("fd/{dirfd}")
        };
        let dir = open_task_path(
            &task,
            &CString::new(name).unwrap(),
            libc::O_PATH | libc::O_DIRECTORY,
        )?;
        client.accepts_directory(&dir)?;
        Some(dir)
    } else {
        None
    };
    Ok(NativeOpenRequest {
        path,
        directory,
        flags,
        mode,
        resolve,
        umask,
    })
}

pub(super) fn handle(
    client: &NativeOpenClient,
    nfd: RawFd,
    req: &SeccompNotif,
    control: &WorkerControl,
) -> bool {
    if !is_open(req.data.nr as libc::c_long) {
        return false;
    }
    if !notification_is_live(nfd, req.id) {
        return true;
    }
    if scalar_path_only(req) {
        // Register flags are immutable; the kernel opens the path only once.
        // FUSE authorizes that actual lookup in the child's pinned chroot,
        // not a previously inspected host path or mutable open_how snapshot.
        reply_continue(nfd, req.id);
        return true;
    }
    let result = (|| {
        let request = capture(req, client)?;
        let flags = request.flags;
        if !notification_is_live(nfd, req.id) {
            return Err(io::Error::from_raw_os_error(libc::ECANCELED));
        }
        let pending = client.submit(request)?;
        if !control.wait(pending.readiness_fd(), libc::POLLIN, None)? {
            return Err(io::Error::from_raw_os_error(libc::ECANCELED));
        }
        let file = pending.finish()?;
        if !notification_is_live(nfd, req.id) {
            return Err(io::Error::from_raw_os_error(libc::ECANCELED));
        }
        let mut add = SeccompNotifAddfd {
            id: req.id,
            flags: 2,
            srcfd: file.as_raw_fd() as u32,
            newfd_flags: (flags & libc::O_CLOEXEC as u64) as u32,
            ..Default::default()
        };
        if ioctl_notif(
            nfd,
            notif_addfd(),
            (&mut add as *mut SeccompNotifAddfd).cast(),
        ) < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })();
    if let Err(error) = result {
        reply(nfd, req.id, -error.raw_os_error().unwrap_or(libc::EIO));
    }
    true
}

// Called after the child has consumed its ruleset and joined its guardian.
// No provider backing/fuse fd may keep an initial exec request alive.
pub(super) unsafe fn close_except(first: RawFd, second: RawFd) -> Result<(), i32> {
    let low = first.min(second) as u32;
    let high = first.max(second) as u32;
    for (start, end) in [(3, low - 1), (low + 1, high - 1), (high + 1, u32::MAX)] {
        if start <= end && unsafe { libc::syscall(libc::SYS_close_range, start, end, 0) } < 0 {
            return Err(errno());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_only_continuation_requires_scalar_register_flags() {
        let mut req: SeccompNotif = unsafe { std::mem::zeroed() };
        req.data.nr = libc::SYS_openat as i32;
        req.data.args[2] = (libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64;
        assert!(scalar_path_only(&req));
        req.data.args[2] = libc::O_RDONLY as u64;
        assert!(!scalar_path_only(&req));
        req.data.nr = libc::SYS_openat2 as i32;
        req.data.args[2] = libc::O_PATH as u64;
        assert!(!scalar_path_only(&req));
        #[cfg(target_arch = "x86_64")]
        {
            req.data.nr = libc::SYS_open as i32;
            req.data.args[1] = libc::O_PATH as u64;
            assert!(scalar_path_only(&req));
            req.data.nr = libc::SYS_creat as i32;
            assert!(!scalar_path_only(&req));
        }
    }

    fn verdict(nr: u32, arch: u32, args: [u64; 6]) -> u32 {
        let program = notifier();
        let mut words = [0u32; 16];
        words[0] = nr;
        words[1] = arch;
        for (i, arg) in args.into_iter().enumerate() {
            words[4 + 2 * i] = arg as u32;
            words[5 + 2 * i] = (arg >> 32) as u32;
        }
        let mut accumulator = 0;
        let mut pc = 0;
        while let Some(ins) = program.get(pc) {
            pc += 1;
            match ins.code {
                0x20 => accumulator = words[ins.k as usize / 4],
                0x54 => accumulator &= ins.k,
                0x15 => pc += usize::from(if accumulator == ins.k { ins.jt } else { ins.jf }),
                0x35 => pc += usize::from(if accumulator >= ins.k { ins.jt } else { ins.jf }),
                0x06 => return ins.k,
                code => panic!("unexpected BPF instruction {code}"),
            }
        }
        panic!("filter fell through")
    }

    #[test]
    fn projected_filter_notifies_reads_and_preserves_network_dispatch() {
        for syscall in open_syscalls() {
            assert_eq!(
                verdict(syscall as u32, AUDIT_ARCH_NATIVE, [0; 6]),
                SECCOMP_RET_USER_NOTIF
            );
        }
        assert_eq!(
            verdict(libc::SYS_connect as u32, AUDIT_ARCH_NATIVE, [0; 6]),
            SECCOMP_RET_USER_NOTIF
        );
        assert_eq!(
            verdict(libc::SYS_close as u32, AUDIT_ARCH_NATIVE, [0; 6]),
            SECCOMP_RET_ALLOW
        );
        assert_eq!(
            verdict(libc::SYS_openat as u32, 0, [0; 6]),
            SECCOMP_RET_KILL_PROCESS
        );
        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            verdict(
                0x4000_0000 | libc::SYS_openat as u32,
                AUDIT_ARCH_NATIVE,
                [0; 6]
            ),
            SECCOMP_RET_KILL_PROCESS
        );
    }

    #[test]
    fn projected_filter_does_not_import_host_namespaces_or_unix_sockets() {
        let deny = SECCOMP_RET_ERRNO | libc::EPERM as u32;
        for syscall in [
            libc::SYS_mount,
            libc::SYS_chroot,
            libc::SYS_unshare,
            libc::SYS_open_by_handle_at,
        ] {
            assert_eq!(verdict(syscall as u32, AUDIT_ARCH_NATIVE, [0; 6]), deny);
        }
        assert_eq!(
            verdict(
                libc::SYS_socket as u32,
                AUDIT_ARCH_NATIVE,
                [libc::AF_UNIX as u64, 0, 0, 0, 0, 0]
            ),
            deny
        );
        assert_eq!(
            verdict(
                libc::SYS_socket as u32,
                AUDIT_ARCH_NATIVE,
                [libc::AF_INET as u64, 0, 0, 0, 0, 0]
            ),
            SECCOMP_RET_USER_NOTIF
        );
        assert_eq!(
            verdict(
                libc::SYS_ioctl as u32,
                AUDIT_ARCH_NATIVE,
                [0, 0x4e80, 0, 0, 0, 0]
            ),
            deny
        );
        for command in [libc::FIONREAD, libc::FIONBIO, libc::FIOCLEX, libc::FIONCLEX] {
            assert_eq!(
                verdict(
                    libc::SYS_ioctl as u32,
                    AUDIT_ARCH_NATIVE,
                    [0, command as u64, 0, 0, 0, 0]
                ),
                SECCOMP_RET_ALLOW
            );
        }
    }
}

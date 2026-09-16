//! Explicit own-process metadata reads. Ordinary opens still run under Landlock.

use super::*;

const SECCOMP_ADDFD_FLAG_SEND: u32 = 2;

pub(super) fn check_atomic_addfd(nfd: RawFd) -> io::Result<()> {
    // A supported ioctl validates flags before looking up srcfd. An invalid
    // source descriptor probes SEND support without ever injecting a descriptor.
    let mut add = SeccompNotifAddfd {
        id: u64::MAX,
        flags: SECCOMP_ADDFD_FLAG_SEND,
        srcfd: u32::MAX,
        ..Default::default()
    };
    if ioctl_notif(nfd, notif_addfd(), &mut add as *mut _ as *mut libc::c_void) < 0
        && errno() == libc::EBADF
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "self-process metadata requires atomic seccomp FD injection (Linux 5.14+)",
        ))
    }
}

pub(super) fn handle_open(state: &SupState, nfd: RawFd, req: &SeccompNotif) -> bool {
    let nr = req.data.nr as libc::c_long;
    let args = &req.data.args;
    let (pointer, flags) = if nr == libc::SYS_openat {
        (args[1], Some(args[2]))
    } else if nr == libc::SYS_openat2 {
        let mut how = [0u8; 24];
        let read = unsafe { read_child_mem(req.pid, args[2], &mut how) };
        let flags = (args[3] == 24 && read == 24)
            .then(|| u64::from_ne_bytes(how[..8].try_into().unwrap()))
            .filter(|_| how[8..].iter().all(|byte| *byte == 0));
        (args[1], flags)
    } else {
        #[cfg(target_arch = "x86_64")]
        if nr == libc::SYS_open {
            return open_path(state, nfd, req, args[0], Some(args[1]));
        }
        return false;
    };
    open_path(state, nfd, req, pointer, flags)
}

fn open_path(
    state: &SupState,
    nfd: RawFd,
    req: &SeccompNotif,
    pointer: u64,
    flags: Option<u64>,
) -> bool {
    // Through `read_child_str` rather than a bare `read_child_mem` of a fixed buffer: that
    // buffer could span into an unmapped page, and `pread` on `/proc/<pid>/mem` fails the WHOLE
    // range when it does. A `/proc/self/maps` literal in rodata sits near a segment end often
    // enough that this was not theoretical — the failed read made the path look like a
    // NON-metadata one, which handed it to the fs broker, which has no rule for per-process
    // procfs (that capability is `fs.self_proc`, not an fs rule) and refused it.
    let raw = read_child_str(req.pid, pointer, 64);
    let selected = raw.as_deref().and_then(metadata_path);
    let Some((file, suffix)) = selected else {
        // Not a self-process metadata path, so this handler has no business deciding it: hand it
        // back to the fs broker whenever one is armed. CONTINUE here would let the child's own
        // syscall run and leave Landlock as the only authority — fine for a write-only broker,
        // because Landlock already refuses every write the policy denies, but WRONG once reads
        // are brokered: Landlock cannot subtract, so a CONTINUEd read of a denied path inside a
        // granted tree succeeds. The gate is therefore "is a broker armed", not "does this open
        // carry write intent".
        if state.write_matcher.is_some() {
            return false;
        }
        reply_continue(nfd, req.id);
        return true;
    };
    let permitted_flags = (libc::O_CLOEXEC
        | libc::O_LARGEFILE
        | libc::O_NONBLOCK
        | libc::O_NOCTTY
        | libc::O_NOFOLLOW
        | libc::O_DIRECTORY) as u64;
    let Some(flags) = flags.filter(|flags| flags & !permitted_flags == 0) else {
        reply(nfd, req.id, -libc::EACCES);
        return true;
    };
    if !state.self_proc.contains(&file) {
        reply(nfd, req.id, -libc::EACCES);
        return true;
    }
    let mut id = req.id;
    if ioctl_notif(
        nfd,
        notif_id_valid(),
        &mut id as *mut _ as *mut libc::c_void,
    ) < 0
    {
        return true;
    }
    // The notification's kernel-supplied TID identifies the requesting process,
    // never a PID supplied in the path. Revalidation after opening prevents PID
    // recycling from returning another process's file; ADDFD_SEND is atomic.
    let pid = std::fs::read_to_string(format!("/proc/{}/status", req.pid))
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix("Tgid:")
                    .and_then(|value| value.trim().parse::<u32>().ok())
            })
        });
    let Some(pid) = pid else {
        reply(nfd, req.id, -libc::ESRCH);
        return true;
    };
    // A numeric task component is resolved beneath this process's task directory;
    // a TID belonging to a different process is not present there. No path supplied
    // by the caller can select another process or traverse a procfs magic link.
    let path = cstr(&format!("/proc/{pid}/{suffix}"));
    let fd = unsafe { libc::open(path.as_ptr(), flags as i32 | libc::O_CLOEXEC) };
    if fd < 0 {
        reply(nfd, req.id, -errno());
        return true;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if ioctl_notif(
        nfd,
        notif_id_valid(),
        &mut id as *mut _ as *mut libc::c_void,
    ) < 0
    {
        return true;
    }
    let mut add = SeccompNotifAddfd {
        id: req.id,
        flags: SECCOMP_ADDFD_FLAG_SEND,
        srcfd: fd.as_raw_fd() as u32,
        newfd_flags: (flags & libc::O_CLOEXEC as u64) as u32,
        ..Default::default()
    };
    if ioctl_notif(nfd, notif_addfd(), &mut add as *mut _ as *mut libc::c_void) < 0 {
        reply(nfd, req.id, -errno());
    }
    true
}

fn metadata_path(path: &str) -> Option<(SelfProcFile, &str)> {
    let suffix = path.strip_prefix("/proc/self/")?;
    if let Some(file) = SelfProcFile::from_path(path)
        && !matches!(file, SelfProcFile::TaskStat | SelfProcFile::TaskStatus)
    {
        return Some((file, suffix));
    }
    let (tid, file) = suffix.strip_prefix("task/")?.split_once('/')?;
    if tid.is_empty() || !tid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let file = match file {
        "stat" => SelfProcFile::TaskStat,
        "status" => SelfProcFile::TaskStatus,
        _ => return None,
    };
    Some((file, suffix))
}

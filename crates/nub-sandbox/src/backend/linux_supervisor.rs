#![cfg(target_os = "linux")]
//! Zero-privilege transparent per-hostname TCP egress supervisor (Linux).
//!
//! This is a Rust port of the reviewed, measured C prototype `route.c`. It gives the
//! Landlock/seccomp mechanism a network axis WITHOUT a user namespace, a subprocess proxy,
//! or any capability: a seccomp `USER_NOTIF` filter installed in the child hands every
//! `connect`/`socket`/`send*` (and the DNS-socket `recv*`) to a supervisor thread in the
//! launching process, which owns the real syscalls.
//!
//! WHY A SUPERVISOR AND NOT ADDFD-ONLY. An ADDFD-only redirect discards the child's chosen
//! destination, so `ssh`/`git@`/databases lose their route. This design recovers the
//! destination and still has no TOCTOU: the supervisor reads the child's `sockaddr` ONCE,
//! confirms the notification is still live (`NOTIF_ID_VALID`), then DIALS THE DESTINATION
//! ITSELF. The kernel never re-reads the child's buffer after that, so a racing rewrite buys
//! nothing.
//!
//! HOSTNAME POLICY COMES FROM OBSERVED DNS. A UDP `connect` to port 53 is answered by
//! splicing in a supervisor-owned socket connected to the configured resolver. Reply peeking
//! records `name -> A/AAAA` for this launch. A later TCP `connect` to one
//! of those addresses is attributed to that name and checked against the allowlist.
//!
//! THE LISTENER FD IS HANDED OVER A PLAIN PIPE, NEVER `SCM_RIGHTS`. Once `sendmsg` is
//! filtered, an `SCM_RIGHTS` handoff deadlocks against the very supervisor that would service
//! it. The child instead `write`s the listener's fd NUMBER down an ordinary pipe (`write` is
//! unfiltered) and the parent recovers the descriptor with `pidfd_open` + `pidfd_getfd`.
//!
//! The library launch path owns the child, its streams and supervisor. Each supervisor owns
//! its own policy/DNS state and descriptors; cancellation wakes notification and network I/O.
#![allow(dead_code)]

use crate::matcher::path::PathMatcher;
use crate::policy::SelfProcFile;
use crate::policy::{Effect, FsAccess, FsRuleSet};
use std::collections::BTreeSet;

mod self_proc;
use std::cell::RefCell;
use std::ffi::CString;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, OnceLock};

/// Whether the supervisor's per-connection decision trace is enabled. The supervisor runs in the
/// nub PARENT, so its `eprintln!`s land on nub's own stderr (the user's terminal during `nub
/// install`) and name every destination the child reaches — useful when debugging egress, noise in
/// production. Gated behind `NUB_SANDBOX_SUP_DEBUG`; read once. (epic 5.1 L5)
fn sup_debug() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("NUB_SANDBOX_SUP_DEBUG").is_some())
}

/// `eprintln!` gated on [`sup_debug`]. Every supervisor decision line goes through this so the
/// default (unset) run emits nothing.
macro_rules! suplog {
    ($($arg:tt)*) => {
        if sup_debug() {
            eprintln!($($arg)*);
        }
    };
}

// ---------------------------------------------------------------------------
// seccomp USER_NOTIF ABI — hand-declared because `libc` does not expose the
// notification ioctls or their structs across the versions we target. Layouts
// mirror <linux/seccomp.h> exactly; a size mismatch changes the ioctl number and
// the kernel rejects the call, so these are load-bearing.
// ---------------------------------------------------------------------------

const SECCOMP_SET_MODE_FILTER: libc::c_uint = 1;
const SECCOMP_FILTER_FLAG_NEW_LISTENER: libc::c_ulong = 1 << 3;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;
/// SECCOMP_USER_NOTIF_FLAG_CONTINUE — let the kernel run the child's own syscall.
const SECCOMP_USER_NOTIF_FLAG_CONTINUE: u32 = 1;
/// SECCOMP_ADDFD_FLAG_SETFD — install at a caller-chosen descriptor number.
const SECCOMP_ADDFD_FLAG_SETFD: u32 = 1;

/// `struct seccomp_data` — the classic-BPF input record.
#[repr(C)]
#[derive(Clone, Copy)]
struct SeccompData {
    nr: libc::c_int,
    arch: u32,
    instruction_pointer: u64,
    args: [u64; 6],
}

/// `struct seccomp_notif`.
#[repr(C)]
#[derive(Clone, Copy)]
struct SeccompNotif {
    id: u64,
    pid: u32,
    flags: u32,
    data: SeccompData,
}

/// `struct seccomp_notif_resp`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SeccompNotifResp {
    id: u64,
    val: i64,
    error: i32,
    flags: u32,
}

/// `struct seccomp_notif_addfd`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SeccompNotifAddfd {
    id: u64,
    flags: u32,
    srcfd: u32,
    newfd: u32,
    newfd_flags: u32,
}

// _IOC(dir,type,nr,size) with '!' (0x21) as the type, matching the kernel's
// SECCOMP_IOR/IOW/IOWR helpers. dir bits: WRITE=1, READ=2.
const IOC_WRITE: libc::c_ulong = 1;
const IOC_READ: libc::c_ulong = 2;
const fn ioc(dir: libc::c_ulong, nr: libc::c_ulong, size: usize) -> libc::c_ulong {
    (dir << 30) | ((size as libc::c_ulong) << 16) | ((b'!' as libc::c_ulong) << 8) | nr
}
fn notif_recv() -> libc::c_ulong {
    ioc(IOC_READ | IOC_WRITE, 0, size_of::<SeccompNotif>())
}
fn notif_send() -> libc::c_ulong {
    ioc(IOC_READ | IOC_WRITE, 1, size_of::<SeccompNotifResp>())
}
fn notif_id_valid() -> libc::c_ulong {
    ioc(IOC_WRITE, 2, size_of::<u64>())
}
fn notif_addfd() -> libc::c_ulong {
    ioc(IOC_WRITE, 3, size_of::<SeccompNotifAddfd>())
}

// ---------------------------------------------------------------------------
// classic-BPF construction (seccompiler::sock_filter is `{code,jt,jf,k}`)
// ---------------------------------------------------------------------------

const BPF_LD: u16 = 0x00;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JMP: u16 = 0x05;
const BPF_JEQ: u16 = 0x10;
const BPF_JGE: u16 = 0x30;
const BPF_RET: u16 = 0x06;
const BPF_K: u16 = 0x00;
const BPF_ALU: u16 = 0x04;
const BPF_AND: u16 = 0x50;

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_NATIVE: u32 = 0xC000_003E; // AUDIT_ARCH_X86_64
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH_NATIVE: u32 = 0xC000_00B7; // AUDIT_ARCH_AARCH64

// offsets into `struct seccomp_data`
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_ARG0: u32 = 16;
#[cfg(target_arch = "x86_64")]
const OFF_ARG1: u32 = 24;
const OFF_ARG2: u32 = 32; // args[2] = 16 + 2*8; the openat flags word

// Supervisor-created DGRAM sockets are pinned into this descriptor window so the
// filter can cheaply decide which `recv*` calls are DNS sockets worth observing.
const DNS_FD_LO: u32 = 960;
const DNS_FD_HI: u32 = 1024;

fn stmt(code: u16, k: u32) -> seccompiler::sock_filter {
    seccompiler::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}
fn jump(code: u16, k: u32, jt: u8, jf: u8) -> seccompiler::sock_filter {
    seccompiler::sock_filter { code, jt, jf, k }
}

/// One symbolic classic-BPF instruction. Jump targets are LABELS resolved to forward skip
/// counts by [`assemble`], so adding or removing a dispatch entry can never desync a
/// hand-counted offset — the failure mode that makes a raw filter table so dangerous.
enum Ins {
    /// A non-jump instruction (`LD`, `RET`, `ALU`).
    Stmt(u16, u32),
    /// A conditional jump. `jt`/`jf` name labels; BOTH must resolve to a LATER instruction —
    /// classic BPF jumps forward only, and [`assemble`] panics if one does not.
    Jump(u16, u32, &'static str, &'static str),
    /// A label marker; emits no instruction, names the NEXT real instruction's index.
    Label(&'static str),
}

/// Resolve the symbolic program to `sock_filter`s, turning each jump's target labels into the
/// `(target − current − 1)` forward skip counts classic BPF uses.
fn assemble(prog: &[Ins]) -> Vec<seccompiler::sock_filter> {
    use std::collections::HashMap;
    let mut label_at: HashMap<&'static str, usize> = HashMap::new();
    let mut idx = 0usize;
    for ins in prog {
        match ins {
            Ins::Label(name) => {
                label_at.insert(name, idx);
            }
            _ => idx += 1,
        }
    }
    let resolve = |name: &'static str, cur: usize| -> u8 {
        let target = *label_at
            .get(name)
            .unwrap_or_else(|| panic!("bpf label `{name}` is never defined"));
        let skip = target
            .checked_sub(cur + 1)
            .unwrap_or_else(|| panic!("bpf label `{name}` is not a forward jump from {cur}"));
        u8::try_from(skip).unwrap_or_else(|_| panic!("bpf jump to `{name}` exceeds 255"))
    };
    let mut out = Vec::with_capacity(idx);
    let mut cur = 0usize;
    for ins in prog {
        match ins {
            Ins::Label(_) => {}
            Ins::Stmt(code, k) => {
                out.push(stmt(*code, *k));
                cur += 1;
            }
            Ins::Jump(code, k, t, f) => {
                out.push(jump(*code, *k, resolve(t, cur), resolve(f, cur)));
                cur += 1;
            }
        }
    }
    out
}

/// Write-intent open flags: an `open{at,at2}` carrying any of these can mutate, so it is
/// notified and brokered; a pure `O_RDONLY` open never leaves the kernel (the read-deny cost
/// is deliberately out of scope for this axis).
fn write_open_mask() -> u32 {
    (libc::O_WRONLY | libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC) as u32
}

/// The filesystem syscalls the broker mediates, in their `*at` spelling. Notified only when
/// `write_broker` is set (a policy carries deny carve-outs); `openat` is additionally gated on
/// its flags word unless reads are brokered too.
///
/// THE METADATA HALF IS HERE BECAUSE LANDLOCK HAS NO METADATA HOOK AT ANY ABI. `chmod`,
/// `chown`, `utimes` and the xattr calls take a PATH and are governed by nothing else, so
/// before this a confined command could `chmod 0777` any file its uid owns anywhere on the host.
///
/// THE MEASUREMENT THAT DECIDED THE SHAPE, kept here because the ceiling that used to hold it is
/// gone. A denial matrix over five real native installs (better-sqlite3, sqlite3, esbuild,
/// simple-git-hooks, bufferutil) on kernel 6.8 falsified a BLANKET seccomp deny: `chmod` breaks
/// 4 of 5 with EPERM, because node-gyp chmods the built addon from inside a make recipe, and
/// `utimensat` breaks sqlite3 and bufferutil in either the path or the fd form. Both had been
/// proposed off an strace showing zero calls. The xattr and `chown` families were free at BOTH
/// uid 1000 and root — nothing uses them, because they achieve little: chown to another uid
/// already fails under DAC and `user.*` xattrs are inert.
///
/// Per-path brokering is what that matrix could not reach: a chmod inside a write grant runs and
/// one outside it does not, so the workload that falsified the ceiling is unaffected and the
/// residual the ceiling left behind — host-wide mode and mtime rewriting — is closed.
///
/// The fd forms (`fchmod`, `fchown`, and `utimensat` with a NULL path) carry no path at all,
/// and the descriptor behind one proves only that an OPEN was allowed: a read-only grant hands
/// out a read-only fd these would then use to rewrite the file's mode, owner or timestamps.
/// They are resolved back to a path through `/proc/<tid>/fd/<n>` and judged as the path form.
///
/// THE XATTR FAMILY IS DELIBERATELY ABSENT, per the same matrix: `user.*` xattrs are inert, and
/// `security.*`/`trusted.*` need a capability the confined child does not hold, so there is
/// nothing for a rule to protect. Brokering them would mean copying an arbitrary value blob out
/// of child memory on every call — new unsafe surface bought for no enforcement.
const FS_INTENT_NRS: &[libc::c_long] = &[
    libc::SYS_openat,
    libc::SYS_openat2,
    libc::SYS_mkdirat,
    libc::SYS_unlinkat,
    libc::SYS_symlinkat,
    libc::SYS_linkat,
    libc::SYS_renameat,
    libc::SYS_renameat2,
    libc::SYS_truncate,
    libc::SYS_fchmodat,
    libc::SYS_fchmod,
    libc::SYS_utimensat,
    libc::SYS_fchownat,
    libc::SYS_fchown,
    libc::SYS_faccessat,
    libc::SYS_faccessat2,
];

/// The metadata syscalls whose FIRST argument is an open descriptor rather than a directory
/// fd plus a path. `utimensat` joins them dynamically when its path pointer is NULL — the
/// `futimens` spelling — which is why the set is consulted alongside a `pa == 0` test rather
/// than on its own.
const FD_FORM_NRS: &[libc::c_long] = &[libc::SYS_fchmod, libc::SYS_fchown];

/// ⛔ THE x86_64 LEGACY ENTRY POINTS, AND THEY ARE NOT COSMETIC. A seccomp filter matches a
/// syscall NUMBER, so a binary that issues `open(2)` rather than `openat(2)` — a static musl
/// build, a Go runtime, hand-written asm — walked straight past the broker. Landlock does not
/// cover the gap: it is syscall-agnostic, but it cannot express a DENY at all, so the `.env*`
/// floor is broker-only and `open("/project/.env", O_RDONLY)` under this number read the
/// secret while every test stayed green, because Rust and glibc both emit `openat`.
///
/// aarch64 has none of these numbers, so the list is empty there.
#[cfg(target_arch = "x86_64")]
const FS_INTENT_LEGACY_NRS: &[libc::c_long] = &[
    libc::SYS_open,
    libc::SYS_creat,
    libc::SYS_mkdir,
    libc::SYS_rmdir,
    libc::SYS_unlink,
    libc::SYS_symlink,
    libc::SYS_link,
    libc::SYS_rename,
    libc::SYS_chmod,
    libc::SYS_chown,
    libc::SYS_lchown,
    libc::SYS_access,
    libc::SYS_utime,
    libc::SYS_utimes,
    libc::SYS_futimesat,
];
#[cfg(not(target_arch = "x86_64"))]
const FS_INTENT_LEGACY_NRS: &[libc::c_long] = &[];

/// Build the notifier BPF program. `connect`/`socket`/`send{to,msg,mmsg}` become `USER_NOTIF`;
/// `io_uring_setup` becomes a scalar `EPERM` (its SQEs never re-enter this filter, so it cannot
/// be mediated per-op); `read`/`recv{from,msg}` are notified ONLY for descriptors in the DNS
/// window. When `write_broker` is set, the fs-intent syscalls above are also notified —
/// `openat` gated on its flags carrying a write bit — so the deny-inside-allow broker can
/// mediate them; otherwise those syscalls are never trapped and cost nothing.
fn notifier_program(
    write_broker: bool,
    read_broker: bool,
    self_proc: bool,
) -> Vec<seccompiler::sock_filter> {
    let nr = |n: libc::c_long| n as u32;
    let ld = BPF_LD | BPF_W | BPF_ABS;
    let jeq = BPF_JMP | BPF_JEQ | BPF_K;
    let jge = BPF_JMP | BPF_JGE | BPF_K;
    let mut p: Vec<Ins> = vec![
        Ins::Stmt(ld, OFF_ARCH),
        Ins::Jump(jeq, AUDIT_ARCH_NATIVE, "nr", "kill"),
        Ins::Label("kill"),
        Ins::Stmt(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        Ins::Label("nr"),
        Ins::Stmt(ld, OFF_NR),
        // Unconditional scalar EPERM denies (like io_uring, these cannot be mediated per-op):
        //  - io_uring_setup: its SQEs never re-enter this filter, so a submitted openat/connect
        //    would bypass every notifier.
        //  - ptrace / process_vm_readv / process_vm_writev: cross-process memory access — a
        //    confined child has no legitimate use and it is a direct route to tamper with a peer
        //    (or the supervisor). Also closes the `/proc/<pid>/mem`-via-ptrace path.
        //  - pidfd_getfd: steals an fd from another process (the supervisor's own listener among
        //    them); the child never needs it. The supervisor's own pidfd_getfd runs in the PARENT
        //    and is unaffected.
        Ins::Jump(jeq, nr(libc::SYS_io_uring_setup), "eperm", "h0"),
        Ins::Label("h0"),
        Ins::Jump(jeq, nr(libc::SYS_ptrace), "eperm", "h1"),
        Ins::Label("h1"),
        Ins::Jump(jeq, nr(libc::SYS_process_vm_readv), "eperm", "h2"),
        Ins::Label("h2"),
        Ins::Jump(jeq, nr(libc::SYS_process_vm_writev), "eperm", "h3"),
        Ins::Label("h3"),
        Ins::Jump(jeq, nr(libc::SYS_pidfd_getfd), "eperm", "n0"),
        Ins::Label("n0"),
        Ins::Jump(jeq, nr(libc::SYS_sendto), "notify", "n1"),
        Ins::Label("n1"),
        Ins::Jump(jeq, nr(libc::SYS_sendmsg), "notify", "n2"),
        Ins::Label("n2"),
        Ins::Jump(jeq, nr(libc::SYS_sendmmsg), "notify", "n3"),
        Ins::Label("n3"),
        Ins::Jump(jeq, nr(libc::SYS_connect), "notify", "n4"),
        Ins::Label("n4"),
        Ins::Jump(jeq, nr(libc::SYS_socket), "notify", "n5"),
        Ins::Label("n5"),
        Ins::Jump(jeq, nr(libc::SYS_read), "dnscheck", "n6"),
        Ins::Label("n6"),
        Ins::Jump(jeq, nr(libc::SYS_recvfrom), "dnscheck", "n7"),
        Ins::Label("n7"),
        Ins::Jump(jeq, nr(libc::SYS_recvmsg), "dnscheck", "n8"),
        Ins::Label("n8"),
    ];
    // Signal scoping rides every supervised launch; see `SIGNAL_INTENT_NRS` for why it carries no
    // gate of its own.
    for (i, syscall) in SIGNAL_INTENT_NRS.iter().enumerate() {
        let after: &'static str = SIGNAL_INTENT_LABELS[i];
        p.push(Ins::Jump(jeq, nr(*syscall), "notify", after));
        p.push(Ins::Label(after));
    }
    if self_proc {
        p.push(Ins::Jump(
            jeq,
            nr(libc::SYS_openat),
            "proc_openat_flags",
            "proc_openat2",
        ));
        p.push(Ins::Label("proc_openat2"));
        p.push(Ins::Jump(
            jeq,
            nr(libc::SYS_openat2),
            "notify",
            "proc_legacy",
        ));
        p.push(Ins::Label("proc_legacy"));
        #[cfg(target_arch = "x86_64")]
        {
            p.push(Ins::Jump(
                jeq,
                nr(libc::SYS_open),
                "proc_open_flags",
                "proc_end",
            ));
            p.push(Ins::Label("proc_end"));
        }
    }
    if write_broker {
        // `openat` routes to the flags check — unless reads are brokered too, in which case
        // every open is notified and the flags no longer decide anything.
        p.push(Ins::Jump(
            jeq,
            nr(libc::SYS_openat),
            if read_broker {
                "notify"
            } else {
                "openat_flags"
            },
            FS_INTENT_LABELS[0],
        ));
        p.push(Ins::Label(FS_INTENT_LABELS[0]));
        // The legacy numbers ride the same chain, and x86_64's `open` is notified
        // UNCONDITIONALLY rather than through the flags gate: its flags sit at a different
        // argument index, and a second flags block would buy nothing — the gate only matters
        // when reads are not brokered, which with the secret floor in place is the
        // measurement pin and nothing else.
        for (i, syscall) in FS_INTENT_NRS
            .iter()
            .chain(FS_INTENT_LEGACY_NRS)
            .enumerate()
            .skip(1)
        {
            let after: &'static str = FS_INTENT_LABELS[i];
            p.push(Ins::Jump(jeq, nr(*syscall), "notify", after));
            p.push(Ins::Label(after));
        }
    }
    // default: not a notified syscall → run it.
    p.push(Ins::Stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));
    // DNS-window check: fd < LO or fd >= HI → allow; LO <= fd < HI → notify.
    p.push(Ins::Label("dnscheck"));
    p.push(Ins::Stmt(ld, OFF_ARG0));
    p.push(Ins::Jump(jge, DNS_FD_LO, "dnschi", "allow"));
    p.push(Ins::Label("dnschi"));
    p.push(Ins::Jump(jge, DNS_FD_HI, "allow", "notify"));
    if write_broker && !read_broker {
        // openat flags: masked write bits == 0 → read-only → allow; else notify.
        p.push(Ins::Label("openat_flags"));
        p.push(Ins::Stmt(ld, OFF_ARG2));
        p.push(Ins::Stmt(BPF_ALU | BPF_AND | BPF_K, write_open_mask()));
        p.push(Ins::Jump(jeq, 0, "allow", "notify"));
    }
    if self_proc {
        let mut flags = |label, offset| {
            p.push(Ins::Label(label));
            p.push(Ins::Stmt(ld, offset));
            p.push(Ins::Stmt(BPF_ALU | BPF_AND | BPF_K, write_open_mask()));
            p.push(Ins::Jump(
                jeq,
                0,
                "notify",
                if write_broker { "notify" } else { "allow" },
            ));
        };
        flags("proc_openat_flags", OFF_ARG2);
        #[cfg(target_arch = "x86_64")]
        flags("proc_open_flags", OFF_ARG1);
    }
    // shared return tail — placed LAST so every jump above reaches it forward.
    p.push(Ins::Label("notify"));
    p.push(Ins::Stmt(BPF_RET | BPF_K, SECCOMP_RET_USER_NOTIF));
    p.push(Ins::Label("allow"));
    p.push(Ins::Stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));
    p.push(Ins::Label("eperm"));
    p.push(Ins::Stmt(
        BPF_RET | BPF_K,
        SECCOMP_RET_ERRNO | libc::EPERM as u32,
    ));
    assemble(&p)
}

/// Fall-through labels for the fs-intent dispatch chain, indexed rather than named: a parallel
/// list of hand-written names has to be edited in lockstep with the syscall tables above, and
/// a desync there silently drops a syscall out of the chain. The const assert below is what
/// makes the pool large enough a compile error, not a runtime panic.
const FS_INTENT_LABELS: &[&str] = &[
    "fs00", "fs01", "fs02", "fs03", "fs04", "fs05", "fs06", "fs07", "fs08", "fs09", "fs10", "fs11",
    "fs12", "fs13", "fs14", "fs15", "fs16", "fs17", "fs18", "fs19", "fs20", "fs21", "fs22", "fs23",
    "fs24", "fs25", "fs26", "fs27", "fs28", "fs29", "fs30", "fs31", "fs32", "fs33", "fs34", "fs35",
    "fs36", "fs37", "fs38", "fs39",
];
const _: () = assert!(FS_INTENT_NRS.len() + FS_INTENT_LEGACY_NRS.len() <= FS_INTENT_LABELS.len());

/// Install a pre-built notifier filter and return the listener descriptor (or `-errno`). The
/// filter is BUILT IN THE PARENT and passed in by reference: `fork` copies it into the child,
/// so this — which runs post-fork, pre-`execve` — allocates nothing. Must run AFTER
/// `PR_SET_NO_NEW_PRIVS`.
unsafe fn install_notifier(filter: &[seccompiler::sock_filter]) -> libc::c_int {
    let prog = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr() as *mut libc::sock_filter,
    };
    let rc = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            SECCOMP_FILTER_FLAG_NEW_LISTENER,
            &prog as *const libc::sock_fprog,
        )
    };
    rc as libc::c_int
}

// ---------------------------------------------------------------------------
// shared supervisor state
// ---------------------------------------------------------------------------

struct DnsEntry {
    fam: i32,
    addr: [u8; 16],
    name: String,
}

/// A socket the supervisor CREATED on the child's behalf, keyed by (tgid, fd).
/// Classification is by construction — no `pidfd_getfd`, no `/proc/net` lookup.
#[derive(Clone, Copy)]
struct SkEntry {
    tgid: u32,
    fd: i32,
    dom: i32,
    typ: i32,
    dns: bool,
    dup: RawFd,
}

struct SupState {
    self_proc: BTreeSet<SelfProcFile>,
    allow_all: bool,
    allow: Vec<String>,
    /// The full fs policy the write broker enforces. `Some` ⇒ the broker is THE write-intent
    /// authority (it performs opens outside Landlock, so it must apply the whole allow-only base
    /// AND the deny carve-outs, not just the denies — see A6). `None` ⇒ the broker is not armed
    /// and the filter traps no write-intent syscall.
    write_matcher: Option<Arc<PathMatcher>>,
    dns_map: Vec<DnsEntry>,
    sk: Vec<SkEntry>,
    upstream_addr_be: u32, // network-order IPv4 of the real upstream resolver
    /// `Some` ⇒ redirect an allowed TCP connect through the loopback egress proxy at this port
    /// (epic 5.1); `None` ⇒ dial the destination directly (the coarse path). Paired with
    /// [`Self::proxy_token`], copied from the [`EgressPolicy`] before the fork.
    proxy_port: Option<u16>,
    /// The bearer the supervisor presents to the loopback proxy. Present iff `proxy_port` is.
    proxy_token: Option<String>,
    /// The guardian's private process group — the confined tree, and the whole of it. Set after
    /// the guardian starts, so it is `None` only for a state that never reached a launch.
    tree_pgid: Option<libc::pid_t>,
}

impl SupState {
    fn record(&mut self, name: &str, fam: i32, addr: &[u8]) {
        if self.dns_map.len() < 512 {
            let mut a = [0u8; 16];
            let n = if fam == libc::AF_INET { 4 } else { 16 };
            a[..n].copy_from_slice(&addr[..n]);
            self.dns_map.push(DnsEntry {
                fam,
                addr: a,
                name: name.to_string(),
            });
        }
        suplog!("SUP DNS {name} -> {}", fmt_ip(fam, addr));
    }

    fn lookup(&self, fam: i32, addr: &[u8]) -> Option<String> {
        let n = if fam == libc::AF_INET { 4 } else { 16 };
        self.dns_map
            .iter()
            .rev()
            .find(|e| e.fam == fam && e.addr[..n] == addr[..n])
            .map(|e| e.name.clone())
    }

    fn allowed(&self, name: Option<&str>) -> bool {
        if self.allow_all {
            return true;
        }
        let Some(name) = name else { return false };
        for a in &self.allow {
            if a.eq_ignore_ascii_case(name) {
                return true;
            }
            // subdomain: name ends with ".<a>"
            if name.len() > a.len() + 1 {
                let (head, tail) = name.split_at(name.len() - a.len());
                if head.ends_with('.') && tail.eq_ignore_ascii_case(a) {
                    return true;
                }
            }
        }
        false
    }

    /// A cheap clone of the write matcher, taken under the lock so the (filesystem-touching)
    /// decision itself runs OUTSIDE it.
    fn write_matcher(&self) -> Option<Arc<PathMatcher>> {
        self.write_matcher.clone()
    }

    fn sk_put(&mut self, tgid: u32, fd: i32, dom: i32, typ: i32) {
        if let Some(e) = self.sk.iter_mut().find(|e| e.tgid == tgid && e.fd == fd) {
            if e.dup >= 0 {
                unsafe { libc::close(e.dup) };
            }
            *e = SkEntry {
                tgid,
                fd,
                dom,
                typ,
                dns: false,
                dup: -1,
            };
            return;
        }
        if self.sk.len() < 1024 {
            self.sk.push(SkEntry {
                tgid,
                fd,
                dom,
                typ,
                dns: false,
                dup: -1,
            });
        }
    }

    fn sk_get(&self, tgid: u32, fd: i32) -> Option<(i32, i32)> {
        self.sk
            .iter()
            .find(|e| e.tgid == tgid && e.fd == fd)
            .map(|e| (e.dom, e.typ))
    }
}

impl SupState {
    fn new(policy: EgressPolicy) -> Self {
        Self {
            self_proc: policy.self_proc,
            allow_all: policy.allow_all,
            allow: policy.allow,
            write_matcher: policy
                .fs_policy
                .as_ref()
                .map(|s| Arc::new(PathMatcher::new(s))),
            dns_map: Vec::new(),
            sk: Vec::new(),
            upstream_addr_be: upstream_resolver(),
            proxy_port: policy.proxy_port,
            proxy_token: policy.proxy_token,
            tree_pgid: None,
        }
    }
}

impl Drop for SupState {
    fn drop(&mut self) {
        for socket in &self.sk {
            if socket.dup >= 0 {
                unsafe { libc::close(socket.dup) };
            }
        }
    }
}

// ---------------------------------------------------------------------------
// small libc helpers
// ---------------------------------------------------------------------------

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Format an address for the decision log. `libc` exposes neither `inet_ntop` nor
/// `inet_pton`, so this and [`parse_ipv4`] are hand-rolled; the IPv6 form is uncompressed
/// (no `::`) because it feeds a human-readable log line, never a parser.
fn fmt_ip(fam: i32, addr: &[u8]) -> String {
    if fam == libc::AF_INET && addr.len() >= 4 {
        format!("{}.{}.{}.{}", addr[0], addr[1], addr[2], addr[3])
    } else if fam == libc::AF_INET6 && addr.len() >= 16 {
        (0..8)
            .map(|i| format!("{:x}", u16::from_be_bytes([addr[i * 2], addr[i * 2 + 1]])))
            .collect::<Vec<_>>()
            .join(":")
    } else {
        "?".into()
    }
}

/// Parse a dotted-quad into a network-order IPv4 word, or `None`.
fn parse_ipv4(s: &str) -> Option<u32> {
    let octets: Vec<u8> = s.split('.').filter_map(|p| p.parse::<u8>().ok()).collect();
    if octets.len() != 4 || s.split('.').count() != 4 {
        return None;
    }
    Some(u32::from_ne_bytes([
        octets[0], octets[1], octets[2], octets[3],
    ]))
}

/// Thread-group leader for a thread id (fds are shared across a thread group, so
/// `pidfd_open` and the sk key both want the tgid, not the calling tid).
fn tgid_of(tid: u32) -> u32 {
    let path = format!("/proc/{tid}/status");
    if let Ok(text) = std::fs::read_to_string(&path) {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("Tgid:")
                && let Ok(v) = rest.trim().parse::<u32>()
            {
                return v;
            }
        }
    }
    tid
}

thread_local! {
    /// One open `/proc/<tid>/mem` per thread the supervisor has seen.
    ///
    /// WHY A CACHE AT ALL: opening and closing this descriptor is two syscalls on the hottest
    /// path in the broker, and with reads brokered that path runs on EVERY `openat` the child
    /// makes. Measured at 20 000 opens from a confined `node`, the whole notification cost
    /// ~88 µs against ~6 µs unbrokered, so every syscall removed here is paid back 20 000 times.
    ///
    /// WHY IT IS SAFE: a procfs mem descriptor is bound to the task it was opened for and is
    /// never re-resolved, so a recycled tid cannot make it read a different process. It simply
    /// starts failing, and a failed read evicts the entry and reopens. The supervisor runs one
    /// notification at a time per thread, so the borrowed descriptor cannot be closed underneath
    /// a caller.
    static CHILD_MEM: RefCell<Vec<(u32, OwnedFd)>> = const { RefCell::new(Vec::new()) };
}

const CHILD_MEM_CAP: usize = 8;

fn child_mem_fd(tid: u32) -> Option<RawFd> {
    CHILD_MEM.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some((_, fd)) = cache.iter().find(|(seen, _)| *seen == tid) {
            return Some(fd.as_raw_fd());
        }
        let path = CString::new(format!("/proc/{tid}/mem")).ok()?;
        let raw = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if raw < 0 {
            return None;
        }
        if cache.len() >= CHILD_MEM_CAP {
            cache.remove(0);
        }
        cache.push((tid, unsafe { OwnedFd::from_raw_fd(raw) }));
        Some(raw)
    })
}

fn forget_child_mem(tid: u32) {
    CHILD_MEM.with(|cache| cache.borrow_mut().retain(|(seen, _)| *seen != tid));
}

/// `pread` the child's memory through the cached descriptor, reopening once if the cached one has
/// gone stale. Returns `-1` on failure, matching the raw `pread` contract its callers expect.
unsafe fn read_child_mem(pid: u32, off: u64, buf: &mut [u8]) -> isize {
    for attempt in 0..2 {
        let Some(fd) = child_mem_fd(pid) else {
            return -1;
        };
        let n = unsafe {
            libc::pread(
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                off as libc::off_t,
            )
        };
        // EIO/ESRCH means the descriptor outlived its task; a genuine EFAULT (an unmapped page)
        // is the caller's business and must not cost a reopen on every boundary read.
        if n >= 0 || attempt == 1 || !matches!(errno(), libc::EIO | libc::ESRCH) {
            return n;
        }
        forget_child_mem(pid);
    }
    -1
}

fn ioctl_notif(nfd: RawFd, req: libc::c_ulong, arg: *mut libc::c_void) -> libc::c_int {
    unsafe { libc::ioctl(nfd, req as _, arg) }
}

fn reply(nfd: RawFd, id: u64, err: i32) {
    let mut r = SeccompNotifResp {
        id,
        error: err,
        ..Default::default()
    };
    if ioctl_notif(nfd, notif_send(), &mut r as *mut _ as *mut libc::c_void) < 0 {
        suplog!("[sup] SEND: {}", io::Error::last_os_error());
    }
}

fn reply_value(nfd: RawFd, id: u64, value: i64) {
    let mut r = SeccompNotifResp {
        id,
        val: value,
        ..Default::default()
    };
    if ioctl_notif(nfd, notif_send(), &mut r as *mut _ as *mut libc::c_void) < 0 {
        suplog!("[sup] SEND: {}", io::Error::last_os_error());
    }
}

fn reply_continue(nfd: RawFd, id: u64) {
    let mut r = SeccompNotifResp {
        id,
        flags: SECCOMP_USER_NOTIF_FLAG_CONTINUE,
        ..Default::default()
    };
    ioctl_notif(nfd, notif_send(), &mut r as *mut _ as *mut libc::c_void);
}

// ---------------------------------------------------------------------------
// send* snapshot + replay
// ---------------------------------------------------------------------------

// USER_NOTIF exposes register arguments, not the pointed-to msghdr/iovec data.  Continuing a
// send after merely inspecting that data is explicitly racy: another child thread may replace an
// initially-null `msg_name` with a destination while the notifying thread waits.  Replay IP
// socket sends from these bounded, supervisor-owned snapshots instead.  The limits match the
// kernel's `UIO_MAXIOV` batch bound and keep a malicious request from making the supervisor
// allocate without limit.  Ordinary DNS, TLS, and package-registry writes are far below 16 MiB.
const MAX_SEND_IOV: usize = 1024;
const MAX_SEND_BYTES: usize = 16 * 1024 * 1024;

/// A connected IP send copied out of target memory.  USER_NOTIF cannot safely write the
/// `sendmmsg` result array, and cannot preserve AF_UNIX sender credentials, so those operations
/// return ENOSYS rather than being emulated against a different process identity.
#[derive(Debug)]
struct SendSnapshot {
    bytes: Vec<u8>,
}

/// Keep a `/proc/<tid>/mem` description open from the first ID check through every read.  This
/// binds reads to the notified address space even if a dead TID is quickly reused; the final
/// `NOTIF_ID_VALID` check below additionally proves the blocked syscall still belongs to it.
fn open_child_mem(tid: u32) -> io::Result<OwnedFd> {
    let path = CString::new(format!("/proc/{tid}/mem")).unwrap();
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn child_pread(mem: RawFd, off: u64, buf: &mut [u8]) -> Result<(), i32> {
    let got = unsafe {
        libc::pread(
            mem,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            off as libc::off_t,
        )
    };
    if got == buf.len() as isize {
        Ok(())
    } else {
        Err(if got < 0 { errno() } else { libc::EFAULT })
    }
}

fn child_struct<T: Copy>(mem: RawFd, off: u64) -> Result<T, i32> {
    let mut value = MaybeUninit::<T>::zeroed();
    let bytes =
        unsafe { std::slice::from_raw_parts_mut(value.as_mut_ptr() as *mut u8, size_of::<T>()) };
    child_pread(mem, off, bytes)?;
    Ok(unsafe { value.assume_init() })
}

fn duplicate_child_fd(tgid: u32, fd: RawFd) -> Result<OwnedFd, i32> {
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, tgid, 0) } as RawFd;
    if pidfd < 0 {
        return Err(errno());
    }
    let copy = unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd, fd, 0) } as RawFd;
    unsafe { libc::close(pidfd) };
    if copy < 0 {
        Err(errno())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(copy) })
    }
}

fn snapshot_msghdr(mem: RawFd, hdr: libc::msghdr) -> Result<SendSnapshot, i32> {
    if !hdr.msg_name.is_null() || hdr.msg_namelen != 0 {
        return Err(libc::EPERM);
    }
    if !hdr.msg_control.is_null() || hdr.msg_controllen != 0 {
        return Err(libc::EPERM);
    }
    let count = hdr.msg_iovlen;
    if count > MAX_SEND_IOV {
        return Err(libc::EINVAL);
    }
    if count != 0 && hdr.msg_iov.is_null() {
        return Err(libc::EFAULT);
    }
    let bytes_len = count
        .checked_mul(size_of::<libc::iovec>())
        .ok_or(libc::EFAULT)?;
    let mut iovecs = vec![unsafe { std::mem::zeroed::<libc::iovec>() }; count];
    if bytes_len != 0 {
        let bytes =
            unsafe { std::slice::from_raw_parts_mut(iovecs.as_mut_ptr() as *mut u8, bytes_len) };
        child_pread(mem, hdr.msg_iov as u64, bytes)?;
    }
    let total = iovecs.iter().try_fold(0usize, |total, iov| {
        total.checked_add(iov.iov_len).ok_or(libc::EFAULT)
    })?;
    if total > MAX_SEND_BYTES {
        return Err(libc::EMSGSIZE);
    }
    let mut payload = Vec::with_capacity(total);
    for iov in iovecs {
        if iov.iov_len == 0 {
            continue;
        }
        if iov.iov_base.is_null() {
            return Err(libc::EFAULT);
        }
        let begin = payload.len();
        payload.resize(begin + iov.iov_len, 0);
        child_pread(mem, iov.iov_base as u64, &mut payload[begin..])?;
    }
    Ok(SendSnapshot { bytes: payload })
}

fn snapshot_sendto(mem: RawFd, req: &SeccompNotif) -> Result<SendSnapshot, i32> {
    // sendto(fd, buf, len, flags, dest_addr, addrlen): a null destination is the connected form.
    let len = usize::try_from(req.data.args[2]).map_err(|_| libc::EFAULT)?;
    if len > MAX_SEND_BYTES {
        return Err(libc::EMSGSIZE);
    }
    if len != 0 && req.data.args[1] == 0 {
        return Err(libc::EFAULT);
    }
    let mut bytes = vec![0; len];
    if len != 0 {
        child_pread(mem, req.data.args[1], &mut bytes)?;
    }
    if req.data.args[4] != 0 || req.data.args[5] != 0 {
        return Err(libc::EPERM);
    }
    Ok(SendSnapshot { bytes })
}

fn notification_is_live(nfd: RawFd, id: u64) -> bool {
    ioctl_notif(nfd, notif_id_valid(), &mut { id } as *mut u64
        as *mut libc::c_void)
        >= 0
}

/// Duplicate and classify the target fd before a decision.  The duplicate keeps the same open
/// file description after another target thread closes or reuses its numeric fd, so replay uses
/// the identity the supervisor observed, not a later target-table occupant.
fn duplicate_child_socket(tgid: u32, fd: RawFd) -> Result<(OwnedFd, i32, i32), i32> {
    let copy = duplicate_child_fd(tgid, fd)?;
    let mut domain = -1;
    let mut kind = -1;
    let mut size = size_of::<i32>() as libc::socklen_t;
    let domain_ok = unsafe {
        libc::getsockopt(
            copy.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_DOMAIN,
            &mut domain as *mut _ as *mut libc::c_void,
            &mut size,
        )
    } == 0;
    size = size_of::<i32>() as libc::socklen_t;
    let kind_ok = unsafe {
        libc::getsockopt(
            copy.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            &mut kind as *mut _ as *mut libc::c_void,
            &mut size,
        )
    } == 0;
    if domain_ok && kind_ok {
        Ok((copy, domain, kind))
    } else {
        Err(libc::ENOTSOCK)
    }
}

enum SendReplayError {
    Errno(i32),
    Abandoned,
}

/// A finite cadence matters: a notification can be cancelled while the duplicated socket is
/// never writable. USER_NOTIF supplies no per-request wakeup, so revalidate the ID at this
/// interval and emit no response when it is gone. The final pre-send check is best-effort only;
/// cancellation and `send` cannot be made atomic through this ABI.
const SEND_REPLAY_POLL_CADENCE: std::time::Duration = std::time::Duration::from_millis(100);

/// Run one replayed connected send without allowing an abandoned target request to strand the
/// worker. A blocking target socket is retried with per-call `MSG_DONTWAIT`; target-created
/// nonblocking sockets keep their normal immediate EAGAIN behavior.
fn send_snapshot(
    control: &WorkerControl,
    listener: RawFd,
    notification_id: u64,
    fd: RawFd,
    snapshot: &SendSnapshot,
    flags: i32,
) -> Result<isize, SendReplayError> {
    let status = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if status < 0 {
        return Err(SendReplayError::Errno(errno()));
    }
    let target_nonblocking = status & libc::O_NONBLOCK != 0;
    let call_flags =
        replay_send_flags(flags, target_nonblocking).map_err(SendReplayError::Errno)?;
    loop {
        if !notification_is_live(listener, notification_id) {
            return Err(SendReplayError::Abandoned);
        }
        // The replay runs in Nub's parent, not the target.  Always suppress SIGPIPE here so an
        // EPIPE is returned to the target instead of signalling the host process.  This cannot
        // reproduce a target-installed SIGPIPE handler; that signal-delivery difference is an
        // explicit USER_NOTIF replay limitation.
        let result = unsafe {
            libc::send(
                fd,
                snapshot.bytes.as_ptr() as *const libc::c_void,
                snapshot.bytes.len(),
                call_flags,
            )
        };
        if result >= 0 {
            return Ok(result);
        }
        let error = errno();
        if target_nonblocking || (error != libc::EAGAIN && error != libc::EWOULDBLOCK) {
            return Err(SendReplayError::Errno(error));
        }
        let ready = control
            .wait(fd, libc::POLLOUT, Some(SEND_REPLAY_POLL_CADENCE))
            .map_err(|error| SendReplayError::Errno(error.raw_os_error().unwrap_or(libc::EIO)))?;
        if control.cancelled() {
            return Err(SendReplayError::Errno(libc::ECANCELED));
        }
        if !notification_is_live(listener, notification_id) {
            return Err(SendReplayError::Abandoned);
        }
        if !ready {
            continue;
        }
    }
}

fn replay_send_flags(flags: i32, target_nonblocking: bool) -> Result<i32, i32> {
    // Zero-copy retains the sender's pages after send returns. The supervisor's snapshot
    // must not outlive this call, and the target owns the socket's completion queue.
    // Reject rather than strip the flag: stripping would also lose promised completions.
    if flags & libc::MSG_ZEROCOPY != 0 {
        return Err(libc::EOPNOTSUPP);
    }
    Ok((if target_nonblocking {
        flags
    } else {
        flags | libc::MSG_DONTWAIT
    }) | libc::MSG_NOSIGNAL)
}

// ---------------------------------------------------------------------------
// DNS stub resolver
// ---------------------------------------------------------------------------

/// Decode a (possibly-compressed) DNS name starting at `off`; returns the offset just
/// past the name in the un-jumped stream, or `None` on a malformed record.
fn dns_name(p: &[u8], mut o: usize, out: &mut String) -> Option<usize> {
    let len = p.len();
    let mut ret: Option<usize> = None;
    let mut jumped = false;
    let mut hops = 0;
    out.clear();
    while o < len {
        let c = p[o] as usize;
        if c == 0 {
            o += 1;
            if !jumped {
                ret = Some(o);
            }
            break;
        }
        if c & 0xC0 == 0xC0 {
            if o + 1 >= len {
                return None;
            }
            let ptr = ((c & 0x3F) << 8) | p[o + 1] as usize;
            if !jumped {
                ret = Some(o + 2);
            }
            jumped = true;
            o = ptr;
            hops += 1;
            if hops > 20 {
                return None;
            }
            continue;
        }
        o += 1;
        if o + c > len {
            return None;
        }
        if !out.is_empty() {
            out.push('.');
        }
        for i in 0..c {
            out.push(p[o + i] as char);
        }
        o += c;
    }
    ret
}

/// Parse a DNS response and record every A/AAAA answer into the observed-DNS map.
fn parse_response(state: &mut SupState, p: &[u8]) {
    let len = p.len();
    if len < 12 {
        return;
    }
    let qd = ((p[4] as usize) << 8) | p[5] as usize;
    let an = ((p[6] as usize) << 8) | p[7] as usize;
    let mut off = 12usize;
    let mut qname = String::new();
    let mut tmp = String::new();
    for i in 0..qd {
        match dns_name(p, off, if i == 0 { &mut qname } else { &mut tmp }) {
            Some(v) => off = v + 4, // skip QTYPE+QCLASS
            None => return,
        }
    }
    for _ in 0..an {
        off = match dns_name(p, off, &mut tmp) {
            Some(v) => v,
            None => return,
        };
        if off + 10 > len {
            return;
        }
        let rtype = ((p[off] as usize) << 8) | p[off + 1] as usize;
        let rdlen = ((p[off + 8] as usize) << 8) | p[off + 9] as usize;
        off += 10;
        if off + rdlen > len {
            return;
        }
        if rtype == 1 && rdlen == 4 {
            state.record(&qname, libc::AF_INET, &p[off..off + 4]);
        } else if rtype == 28 && rdlen == 16 {
            state.record(&qname, libc::AF_INET6, &p[off..off + 16]);
        }
        off += rdlen;
    }
}

fn make_sockaddr_in(addr_be: u32, port: u16) -> libc::sockaddr_in {
    // SAFETY: sockaddr_in is plain-old-data.
    let mut a: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    a.sin_family = libc::AF_INET as libc::sa_family_t;
    a.sin_port = port.to_be();
    a.sin_addr.s_addr = addr_be;
    a
}

/// Dial the loopback egress proxy at `proxy_port` and complete the cooperative HTTP `CONNECT`
/// handshake on the confined child's behalf, returning a BLOCKING connected socket on success or
/// `-1` on any failure (the caller denies — never a direct-dial fallback, which would bypass the
/// proxy's SNI gate). `authority`/`dport` name the child's intended destination (the observed DNS
/// name when known, else the IP literal); the proxy re-resolves and applies its host + SNI gates.
/// `token` is presented as `Proxy-Authorization: Basic base64("<token>:")` — the same credential
/// `set_proxy_env` hands a cooperating client. Runs in the supervisor thread (unconfined), all raw
/// libc + blocking I/O; a 10s recv timeout guards the handshake read and is cleared before the
/// socket is handed back, so the tunnel the child inherits has no timeout. (epic 5.1)
fn proxy_connect_tcp(
    control: &WorkerControl,
    proxy_port: u16,
    token: &str,
    authority: &str,
    dport: u16,
) -> RawFd {
    use base64::Engine;
    let s = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if s < 0 {
        return -1;
    }
    let sa = make_sockaddr_in(u32::from_ne_bytes([127, 0, 0, 1]), proxy_port);
    if unsafe {
        connect_interruptible(
            control,
            s,
            &sa as *const _ as *const libc::sockaddr,
            size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    } != 0
    {
        unsafe { libc::close(s) };
        return -1;
    }
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{token}:"));
    let req = format!(
        "CONNECT {authority}:{dport} HTTP/1.1\r\nHost: {authority}:{dport}\r\nProxy-Authorization: Basic {auth}\r\n\r\n"
    );
    let bytes = req.as_bytes();
    let mut off = 0usize;
    while off < bytes.len() {
        if !control
            .wait(s, libc::POLLOUT, Some(std::time::Duration::from_secs(10)))
            .unwrap_or(false)
        {
            unsafe { libc::close(s) };
            return -1;
        }
        let n = unsafe {
            libc::send(
                s,
                bytes.as_ptr().add(off) as *const libc::c_void,
                bytes.len() - off,
                libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
            )
        };
        if n <= 0 {
            unsafe { libc::close(s) };
            return -1;
        }
        off += n as usize;
    }
    // Read the proxy's response header block (`...\r\n\r\n`), bounded. A `200` status opens the
    // tunnel; the child's subsequent ClientHello is then what the proxy reads for its SNI gate.
    // Anything else (403/407/timeout/short read) is a deny.
    let mut buf = [0u8; 1024];
    let mut len = 0usize;
    let ok = loop {
        if len >= buf.len() {
            break false;
        }
        if !control
            .wait(s, libc::POLLIN, Some(std::time::Duration::from_secs(10)))
            .unwrap_or(false)
        {
            break false;
        }
        let n = unsafe {
            libc::read(
                s,
                buf.as_mut_ptr().add(len) as *mut libc::c_void,
                buf.len() - len,
            )
        };
        if n <= 0 {
            break false;
        }
        len += n as usize;
        if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
            // Status line `HTTP/1.x <code> ...`; the code is the second space-separated token.
            let line_end = buf[..len]
                .windows(2)
                .position(|w| w == b"\r\n")
                .unwrap_or(len);
            break std::str::from_utf8(&buf[..line_end])
                .ok()
                .and_then(|l| l.split(' ').nth(1))
                .map(|code| code == "200")
                .unwrap_or(false);
        }
    };
    if ok {
        s
    } else {
        unsafe { libc::close(s) };
        -1
    }
}

/// The egress proxy is the one loopback peer that a fine-grained policy may dial without
/// re-entering the proxy path. Keep this intentionally narrower than a general loopback test:
/// a sibling service at another `127/8` address or port is still an egress destination and must
/// receive the proxy's target and SNI checks.
fn is_egress_proxy_endpoint(fam: i32, addr: &[u8], port: u16, proxy_port: u16) -> bool {
    fam == libc::AF_INET && addr == [127, 0, 0, 1] && port == proxy_port
}

/// A fine-policy child normally receives a supervisor-created socket. The egress proxy's own
/// authenticated listener is the sole exception: it has already been selected by its exact
/// address and port, so let a proxy-aware client reach it directly. A hostname observation is
/// neither available nor relevant for that transport hop.
fn direct_dial_allowed(proxy_endpoint: bool, observed_name_allowed: bool) -> bool {
    proxy_endpoint || observed_name_allowed
}

fn upstream_resolver() -> u32 {
    std::fs::read_to_string("/etc/resolv.conf")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let mut fields = line.split_whitespace();
                (fields.next()? == "nameserver")
                    .then(|| parse_ipv4(fields.next()?))
                    .flatten()
            })
        })
        .unwrap_or_else(|| u32::from_ne_bytes([127, 0, 0, 53]))
}

// ---------------------------------------------------------------------------
// write-intent broker (deny-inside-allow for writes) — ported from wdeny.c
// ---------------------------------------------------------------------------

/// The kernel `struct open_how` for `openat2`, defined locally so the port does not depend on
/// the libc crate exporting it. Field order and width are the stable kernel ABI.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}
const RESOLVE_NO_SYMLINKS: u64 = 0x04;

/// The signal-sending syscalls, notified on EVERY supervised launch and gated on nothing.
///
/// THE GAP THIS CLOSES: there is no PID namespace, so before this a confined command could signal
/// any process sharing its uid — the user's editor, a sibling agent, or the nub process that
/// launched it. Nothing else mediated them: the seccomp ceiling covers sockets and the keyring,
/// and Landlock is a filesystem LSM that never sees a signal. This is the affordable half of the
/// process isolation the mechanism deliberately does not otherwise claim (A2.3).
///
/// Unlike the fs set these are NOT conditioned on a policy carrying carve-outs. An fs deny is what
/// makes brokering opens meaningful; signal scoping is meaningful for every confining policy, and
/// signals are rare enough that the notification round trip never shows up in a workload.
const SIGNAL_INTENT_NRS: &[libc::c_long] = &[
    libc::SYS_kill,
    libc::SYS_tkill,
    libc::SYS_tgkill,
    libc::SYS_pidfd_send_signal,
    libc::SYS_rt_sigqueueinfo,
    libc::SYS_rt_tgsigqueueinfo,
];

/// Fall-through labels for the signal dispatch chain, indexed for the same reason
/// [`FS_INTENT_LABELS`] is: a hand-written parallel list desyncs silently.
const SIGNAL_INTENT_LABELS: &[&str] = &["sig0", "sig1", "sig2", "sig3", "sig4", "sig5", "sig6"];
const _: () = assert!(SIGNAL_INTENT_NRS.len() <= SIGNAL_INTENT_LABELS.len());

fn is_signal_intent(nr: libc::c_long) -> bool {
    SIGNAL_INTENT_NRS.contains(&nr)
}

fn is_fs_intent(nr: libc::c_long) -> bool {
    FS_INTENT_NRS.contains(&nr) || FS_INTENT_LEGACY_NRS.contains(&nr)
}

/// Whether the fs policy permits a WRITE at `canon` (an absolute canonical path): the last
/// matching rule is an Allow granting ReadWrite. A Deny, a read-only Allow, or no match (the
/// allow-only base's default Deny) all forbid the write. This is the WHOLE fs decision, because
/// the broker performs the op outside Landlock and so must enforce the base, not just the denies.
pub(crate) fn write_allowed(matcher: &PathMatcher, canon: &[u8]) -> bool {
    let d = matcher.decide(Path::new(std::ffi::OsStr::from_bytes(canon)));
    d.effect == Effect::Allow && d.access == FsAccess::ReadWrite
}

/// Whether the fs policy permits a READ at `canon`: the last matching rule is an Allow, at
/// either access. Only a Deny or no match at all refuses. Same whole-decision responsibility as
/// [`write_allowed`] — the broker opens outside Landlock, so the base is its job too.
///
/// This is the half Landlock structurally cannot do. Its rules UNION and never subtract, so a
/// `.env` inside a granted project tree is readable no matter what the policy says; the only
/// place that deny can be applied is here, per open, on the resolved canonical path.
pub(crate) fn read_allowed(matcher: &PathMatcher, canon: &[u8]) -> bool {
    matcher
        .decide(Path::new(std::ffi::OsStr::from_bytes(canon)))
        .effect
        == Effect::Allow
}

/// Read a NUL-terminated string at `addr` from the target's `/proc/<tid>/mem`, without the NUL.
/// `None` on a fault or no terminator within `max` bytes.
fn read_child_str(tid: u32, addr: u64, max: usize) -> Option<Vec<u8>> {
    // PAGE-BOUNDED, AND THAT IS THE WHOLE POINT. `pread` on `/proc/<pid>/mem` fails the ENTIRE
    // range with EFAULT if any byte of it is unmapped, so a single `PATH_MAX` read of a short
    // path sitting near the end of a mapping reads as a fault and the child gets EFAULT for a
    // perfectly valid open. Reading up to the next page boundary and stopping at the first NUL
    // touches only pages the string actually occupies.
    //
    // Latent until reads were brokered: write-intent opens are rare and their paths are usually
    // mid-heap, so the boundary case almost never came up. Every `openat` goes through here now.
    const PAGE: u64 = 4096;
    let fd = child_mem_fd(tid)?;
    let mut out: Vec<u8> = Vec::with_capacity(128);
    let mut cur = addr;
    let found = loop {
        if out.len() >= max {
            break None;
        }
        let chunk = ((PAGE - cur % PAGE) as usize).min(max - out.len());
        let mut buf = vec![0u8; chunk];
        let got = unsafe {
            libc::pread(
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                cur as libc::off_t,
            )
        };
        if got <= 0 {
            break None;
        }
        let got = got as usize;
        match buf[..got].iter().position(|&b| b == 0) {
            Some(end) => {
                out.extend_from_slice(&buf[..end]);
                break Some(());
            }
            None => {
                out.extend_from_slice(&buf[..got]);
                cur += got as u64;
            }
        }
    };
    found?;
    // BYTES, not a `String`. A filename is an arbitrary NUL-free byte string on Linux, and this
    // used to end in `String::from_utf8(...).ok()` — so a legal non-UTF-8 name read as a FAULT
    // and the child got EFAULT for a file it could perfectly well open. Harmless while only
    // write-intent syscalls were trapped and almost nothing hit it; a hard failure once every
    // read comes through here. Nothing downstream needs UTF-8: the matcher takes a `Path`, which
    // is bytes, and the only textual use is the decision log, which lossily converts for display.
    Some(out)
}

/// readlink(`/proc/self/fd/<fd>`) — where one of the SUPERVISOR's own fds really points.
pub(crate) fn fd_path(fd: RawFd) -> Option<Vec<u8>> {
    let link = CString::new(format!("/proc/self/fd/{fd}")).ok()?;
    let mut buf = vec![0u8; 4096];
    let n = unsafe {
        libc::readlink(
            link.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len() - 1,
        )
    };
    if n < 0 {
        return None;
    }
    Some(buf[..n as usize].to_vec())
}

/// Resolve `(tid, dirfd, path)` to a VERIFIED `O_PATH` fd of the PARENT directory, the parent's
/// canonical path, and the final component — or `Err(errno)`. The parent is `realpath`'d as a
/// hint, then reopened with `RESOLVE_NO_SYMLINKS` and its real target read back, so a symlink
/// swapped in after the hint either fails the open or is seen where it truly lands. The final
/// component is deliberately NOT followed here; the caller re-checks an opened fd's real path.
fn resolve_parent(tid: u32, dirfd: i32, path_in: &[u8]) -> Result<(RawFd, Vec<u8>, Vec<u8>), i32> {
    // "/proc/self/…" / "/proc/thread-self/…" name the CHILD's process, not the supervisor's.
    let strip = |prefix: &str| path_in.strip_prefix(prefix.as_bytes());
    let path: Vec<u8> =
        if let Some(rest) = strip("/proc/self/").or_else(|| strip("/proc/thread-self/")) {
            let mut v = format!("/proc/{tid}/").into_bytes();
            v.extend_from_slice(rest);
            v
        } else {
            path_in.to_vec()
        };
    let start: Vec<u8> = if path.first() == Some(&b'/') {
        b"/".to_vec()
    } else if dirfd == libc::AT_FDCWD {
        format!("/proc/{tid}/cwd").into_bytes()
    } else {
        format!("/proc/{tid}/fd/{dirfd}").into_bytes()
    };
    let (dirpart, base): (Vec<u8>, Vec<u8>) = match path.iter().rposition(|b| *b == b'/') {
        None => (b".".to_vec(), path.clone()),
        Some(0) => (b"/".to_vec(), path[1..].to_vec()),
        Some(i) => (path[..i].to_vec(), path[i + 1..].to_vec()),
    };
    if base.is_empty() || base == b"." || base == b".." {
        return Err(libc::EINVAL);
    }
    // FAST PATH. An ABSOLUTE directory part is a complete cache key on its own: it depends on
    // neither the child's cwd nor a descriptor number the child can close and reuse. The slow
    // path below costs a `realpath` (an lstat/readlink per component), an `openat2` and a
    // `readlink`; a hit costs a `readlink` and a `dup`, and a confined process opens thousands of
    // files from a handful of directories.
    //
    // The canonical path is RE-READ from the cached descriptor on every hit rather than stored,
    // so the policy is always checked against where that descriptor points RIGHT NOW. A child
    // that swaps a symlink under a cached directory can therefore only redirect the operation to
    // somewhere this broker also approves — never past it.
    if dirpart.first() == Some(&b'/')
        && let Some((fd, canon)) = cached_parent(&dirpart)
    {
        return Ok((fd, canon, base));
    }
    let dp = dirpart.strip_prefix(b"/".as_slice()).unwrap_or(&dirpart);
    let mut joined = start.clone();
    joined.push(b'/');
    joined.extend_from_slice(dp);
    let joined_c = CString::new(joined).map_err(|_| libc::EINVAL)?;
    let mut real = vec![0u8; libc::PATH_MAX as usize];
    let rp = unsafe { libc::realpath(joined_c.as_ptr(), real.as_mut_ptr() as *mut libc::c_char) };
    if rp.is_null() {
        return Err(errno());
    }
    let real_len = unsafe { libc::strlen(real.as_ptr() as *const libc::c_char) };
    let real_c = CString::new(real[..real_len].to_vec()).map_err(|_| libc::EIO)?;
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: RESOLVE_NO_SYMLINKS,
    };
    let pfd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            real_c.as_ptr(),
            &how as *const OpenHow,
            std::mem::size_of::<OpenHow>(),
        )
    } as RawFd;
    if pfd < 0 {
        return Err(errno());
    }
    match fd_path(pfd) {
        Some(canon) => {
            if dirpart.first() == Some(&b'/') {
                remember_parent(&dirpart, pfd);
            }
            Ok((pfd, canon, base))
        }
        None => {
            unsafe { libc::close(pfd) };
            Err(libc::EIO)
        }
    }
}

thread_local! {
    /// Resolved parent directories, keyed by the absolute directory path the child NAMED (not by
    /// its realpath — that is what the cached descriptor already encodes).
    static PARENT_DIRS: RefCell<Vec<(Vec<u8>, OwnedFd)>> = const { RefCell::new(Vec::new()) };
}

const PARENT_DIRS_CAP: usize = 64;

/// A fresh descriptor to a cached parent, plus where it points right now. `None` on a miss, and
/// on a stale entry — procfs spells a vanished directory with a ` (deleted)` suffix, and a
/// descriptor to a directory that has been replaced would otherwise keep answering for the old
/// inode forever.
fn cached_parent(dirpart: &[u8]) -> Option<(RawFd, Vec<u8>)> {
    PARENT_DIRS.with(|cache| {
        let mut cache = cache.borrow_mut();
        let index = cache.iter().position(|(key, _)| key == dirpart)?;
        let canon = fd_path(cache[index].1.as_raw_fd());
        match canon {
            Some(canon) if !canon.ends_with(b" (deleted)") => {
                let dup =
                    unsafe { libc::fcntl(cache[index].1.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
                if dup < 0 {
                    cache.remove(index);
                    return None;
                }
                Some((dup, canon))
            }
            _ => {
                cache.remove(index);
                None
            }
        }
    })
}

fn remember_parent(dirpart: &[u8], pfd: RawFd) {
    let dup = unsafe { libc::fcntl(pfd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return;
    }
    PARENT_DIRS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= PARENT_DIRS_CAP {
            cache.remove(0);
        }
        cache.push((dirpart.to_vec(), unsafe { OwnedFd::from_raw_fd(dup) }));
    });
}

/// Join a verified parent's canonical path and a final component into the full canonical path.
fn join_full(canon: &[u8], base: &[u8]) -> Vec<u8> {
    let mut out = if canon == b"/" {
        Vec::new()
    } else {
        canon.to_vec()
    };
    out.push(b'/');
    out.extend_from_slice(base);
    out
}

/// Service one write-intent notification: read the path(s) once from the child, resolve the
/// parent(s) to verified fds, apply the deny/protect policy to the CANONICAL path, and — when
/// allowed — perform the operation itself relative to the verified parent, splicing any opened
/// fd back with `ADDFD`. The child's memory is never consulted after the single read, so there
/// is nothing to race. Replies to `nfd` itself (errno on denial/failure, value on success).
/// Rewrite an x86_64 legacy entry point as its `*at` sibling, so the resolve / permit / perform
/// path below is written once. Every fold is an exact re-spelling: `AT_FDCWD` for the implicit
/// cwd, and the flag word the sibling takes explicitly where the legacy call took it by name
/// (`rmdir` is `unlinkat(AT_REMOVEDIR)`, `lchown` is `fchownat(AT_SYMLINK_NOFOLLOW)`, `creat`
/// is the three `open` flags). The time calls are NOT folded — their argument is a different
/// struct — so they keep their own number and get their own arm.
#[cfg(target_arch = "x86_64")]
fn fold_legacy(nr: libc::c_long, a: &[u64; 6]) -> (libc::c_long, [u64; 6]) {
    let at = libc::AT_FDCWD as u64;
    match nr {
        libc::SYS_open => (libc::SYS_openat, [at, a[0], a[1], a[2], 0, 0]),
        libc::SYS_creat => (
            libc::SYS_openat,
            [
                at,
                a[0],
                (libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC) as u64,
                a[1],
                0,
                0,
            ],
        ),
        libc::SYS_mkdir => (libc::SYS_mkdirat, [at, a[0], a[1], 0, 0, 0]),
        libc::SYS_rmdir => (
            libc::SYS_unlinkat,
            [at, a[0], libc::AT_REMOVEDIR as u64, 0, 0, 0],
        ),
        libc::SYS_unlink => (libc::SYS_unlinkat, [at, a[0], 0, 0, 0, 0]),
        // symlink(target, linkpath) -> symlinkat(target, AT_FDCWD, linkpath).
        libc::SYS_symlink => (libc::SYS_symlinkat, [a[0], at, a[1], 0, 0, 0]),
        libc::SYS_link => (libc::SYS_linkat, [at, a[0], at, a[1], 0, 0]),
        libc::SYS_rename => (libc::SYS_renameat, [at, a[0], at, a[1], 0, 0]),
        libc::SYS_chmod => (libc::SYS_fchmodat, [at, a[0], a[1], 0, 0, 0]),
        libc::SYS_chown => (libc::SYS_fchownat, [at, a[0], a[1], a[2], 0, 0]),
        libc::SYS_lchown => (
            libc::SYS_fchownat,
            [at, a[0], a[1], a[2], libc::AT_SYMLINK_NOFOLLOW as u64, 0],
        ),
        libc::SYS_access => (libc::SYS_faccessat, [at, a[0], a[1], 0, 0, 0]),
        _ => (nr, *a),
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn fold_legacy(nr: libc::c_long, a: &[u64; 6]) -> (libc::c_long, [u64; 6]) {
    (nr, *a)
}

/// The path a descriptor the CHILD holds currently names, read from the supervisor through
/// `/proc/<tid>/fd/<n>`. `None` when the descriptor names no filesystem object a policy rule
/// could be about — an anon inode, a socket, a pipe, a memfd, or a file already unlinked.
fn child_fd_path(tid: u32, fd: RawFd) -> Option<Vec<u8>> {
    let link = CString::new(format!("/proc/{tid}/fd/{fd}")).ok()?;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    let n = unsafe {
        libc::readlink(
            link.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len() - 1,
        )
    };
    if n < 0 {
        return None;
    }
    let out = buf[..n as usize].to_vec();
    // procfs spells a vanished or anonymous object as a NON-path: `socket:[…]`,
    // `anon_inode:…`, or a real path with a ` (deleted)` suffix. Neither is reachable by name,
    // so no filesystem rule names it either.
    if out.first() != Some(&b'/') || out.ends_with(b" (deleted)") {
        return None;
    }
    Some(out)
}

/// Refuse a signal aimed outside the confined process tree.
///
/// THE TREE IS EXACTLY THE GUARDIAN'S PROCESS GROUP, and that equivalence is what makes this
/// decidable at all: `linux_lifetime::program(true)` answers `setsid` and `setpgid` with EPERM,
/// and a seccomp filter is inherited across both fork and exec, so every descendant is placed in
/// the group once at launch and can never leave it. A target's pgid is therefore a sound answer
/// to "is this one of ours" — if that freeze is ever relaxed, this check goes with it.
///
/// AN ALLOWED SIGNAL IS ANSWERED WITH `CONTINUE`, NOT PERFORMED HERE, and that is safe for exactly
/// the reason the fs broker's `CONTINUE` is not (A6): every argument this decision reads is a
/// SCALAR captured in the notification at trap time, so there is no pointer for the kernel to
/// re-read out of child memory afterwards — which is the entirety of the fs TOCTOU. Performing the
/// signal here instead would also rewrite the delivered `si_pid` to the supervisor's, which job
/// control and anything else inspecting the sender would see.
///
/// ⚠️ RESIDUAL, stated rather than hidden: a target can exit and its pid be recycled between this
/// check and the kernel's delivery. `pidfd_open` pins `struct pid` across the lookup, so the
/// process judged is the process resolved; what is left is the window after the reply, and closing
/// it needs an attacker to land an OUT-OF-TREE process on one specific recycled pid — every
/// process it can fork itself inherits the group and is in-tree.
fn handle_signal_intent(state: &SupState, nfd: RawFd, req: &SeccompNotif) {
    let nr = req.data.nr as libc::c_long;
    let a = &req.data.args;
    let Some(tree) = state.tree_pgid else {
        // No private group means there is nothing to scope against, and refusing every signal
        // would break a command legitimately signalling its own children. Unreachable from
        // `spawn_supervised_with_ready`, which starts a guardian before handing the state over.
        reply_continue(nfd, req.id);
        return;
    };
    let in_tree = |pid: libc::pid_t| -> bool {
        if pid <= 0 {
            return false;
        }
        // Pin the target first: `pidfd_open` holds the `struct pid` the NUMBER is allocated from,
        // so the process cannot exit and have its pid reused underneath the `getpgid` below.
        let pinned = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::c_long, 0) };
        let answer = unsafe { libc::getpgid(pid) } == tree;
        if pinned >= 0 {
            unsafe { libc::close(pinned as RawFd) };
        }
        answer
    };
    let allowed = match nr {
        n if n == libc::SYS_kill => match a[0] as libc::pid_t {
            // The caller's own group, which the freeze above makes ours by construction.
            0 => true,
            // Every process the uid may signal. Nothing in tree needs it and nothing narrows it.
            -1 => false,
            target if target < -1 => -target == tree,
            target => in_tree(target),
        },
        // `pidfd_send_signal` names its target by DESCRIPTOR, so resolve it the way procfs
        // reports it; a descriptor naming no live process is refused rather than passed through.
        n if n == libc::SYS_pidfd_send_signal => {
            fdinfo_pid(req.pid, a[0] as RawFd).is_some_and(in_tree)
        }
        // `tkill`/`tgkill`/`rt_sigqueueinfo`/`rt_tgsigqueueinfo` all take a positive pid or tgid
        // in argument zero. Threads share their thread group's process group, so one lookup
        // answers the thread-directed forms too.
        _ => in_tree(a[0] as libc::pid_t),
    };
    if allowed {
        reply_continue(nfd, req.id);
    } else {
        suplog!("SUP DENY signal nr={nr} target={} -> EPERM", a[0] as i64);
        reply(nfd, req.id, -libc::EPERM);
    }
}

/// The pid a descriptor names, read out of the CHILD's `fdinfo`. `None` when the descriptor is not
/// a pidfd at all, or names a process already reaped — procfs spells that `Pid:\t-1`.
fn fdinfo_pid(tid: u32, fd: RawFd) -> Option<libc::pid_t> {
    let text = std::fs::read_to_string(format!("/proc/{tid}/fdinfo/{fd}")).ok()?;
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix("Pid:"))?
        .trim()
        .parse::<libc::pid_t>()
        .ok()?;
    (value > 0).then_some(value)
}

fn handle_fs_intent(state: &SupState, nfd: RawFd, req: &SeccompNotif) {
    let (nr, folded) = fold_legacy(req.data.nr as libc::c_long, &req.data.args);
    let a = &folded;
    // Per-syscall argument layout: which args hold (dirfd, path) and the optional second pair.
    // A `pa` of 0 marks a call carrying an open DESCRIPTOR instead, resolved below.
    let (dfd, pa, dfd2, pa2): (i32, u64, i32, u64) = match nr {
        n if n == libc::SYS_openat => (a[0] as i32, a[1], libc::AT_FDCWD, 0),
        n if n == libc::SYS_openat2 => (a[0] as i32, a[1], libc::AT_FDCWD, 0),
        n if n == libc::SYS_mkdirat => (a[0] as i32, a[1], libc::AT_FDCWD, 0),
        n if n == libc::SYS_unlinkat => (a[0] as i32, a[1], libc::AT_FDCWD, 0),
        // symlinkat(target, newdirfd, linkpath): pa2 is the target TEXT (not a path to resolve).
        n if n == libc::SYS_symlinkat => (a[1] as i32, a[2], libc::AT_FDCWD, a[0]),
        n if n == libc::SYS_linkat => (a[0] as i32, a[1], a[2] as i32, a[3]),
        n if n == libc::SYS_renameat => (a[0] as i32, a[1], a[2] as i32, a[3]),
        n if n == libc::SYS_renameat2 => (a[0] as i32, a[1], a[2] as i32, a[3]),
        n if n == libc::SYS_truncate => (libc::AT_FDCWD, a[0], libc::AT_FDCWD, 0),
        n if n == libc::SYS_fchmodat
            || n == libc::SYS_fchownat
            || n == libc::SYS_utimensat
            || n == libc::SYS_faccessat
            || n == libc::SYS_faccessat2 =>
        {
            (a[0] as i32, a[1], libc::AT_FDCWD, 0)
        }
        // The fd forms: arg 0 is the descriptor, and `pa` of 0 routes it through the readback.
        n if FD_FORM_NRS.contains(&n) => (libc::AT_FDCWD, 0, libc::AT_FDCWD, 0),
        #[cfg(target_arch = "x86_64")]
        n if n == libc::SYS_utime || n == libc::SYS_utimes => {
            (libc::AT_FDCWD, a[0], libc::AT_FDCWD, 0)
        }
        #[cfg(target_arch = "x86_64")]
        n if n == libc::SYS_futimesat => (a[0] as i32, a[1], libc::AT_FDCWD, 0),
        _ => {
            reply_continue(nfd, req.id);
            return;
        }
    };

    // A descriptor in place of a path: judged — and PERFORMED — on the path the descriptor
    // currently names. That is TOCTOU-safe in the direction that matters, because a child
    // racing `dup2` onto the descriptor can only redirect the operation to another path this
    // broker also approved, never to one it refused. A descriptor naming nothing reachable by
    // name is refused rather than passed through: `CONTINUE` would hand the kernel the fd
    // NUMBER back, which the child can still swap.
    // Whether this call names its target by DESCRIPTOR rather than by path. Declared, never
    // inferred from a null path pointer: a genuine `openat(AT_FDCWD, NULL, …)` must still answer
    // EFAULT, and reading its `AT_FDCWD` as a descriptor turned that into EPERM.
    let fd_form = FD_FORM_NRS.contains(&nr) || (nr == libc::SYS_utimensat && a[1] == 0);
    let (dfd, path) = if fd_form {
        let Some(resolved) = child_fd_path(req.pid, a[0] as RawFd) else {
            suplog!("SUP DENY fd-form nr={nr} fd={} -> EPERM", a[0]);
            reply(nfd, req.id, -libc::EPERM);
            return;
        };
        (libc::AT_FDCWD, resolved)
    } else {
        let Some(path) = read_child_str(req.pid, pa, libc::PATH_MAX as usize) else {
            reply(nfd, req.id, -libc::EFAULT);
            return;
        };
        (dfd, path)
    };
    let path2 = if pa2 != 0 {
        match read_child_str(req.pid, pa2, libc::PATH_MAX as usize) {
            Some(p) => Some(p),
            None => {
                reply(nfd, req.id, -libc::EFAULT);
                return;
            }
        }
    } else {
        None
    };

    // The notification must still be live before we act on a path read from the child.
    let mut valid_id = req.id;
    if ioctl_notif(
        nfd,
        notif_id_valid(),
        &mut valid_id as *mut _ as *mut libc::c_void,
    ) < 0
    {
        return; // child gone: nothing to answer
    }

    // The broker is THE authority for every op it traps (it performs them outside Landlock), so
    // it must apply the FULL fs policy — the allow base AND the deny carve-outs — not just the
    // denies. No matcher ⇒ not armed; let the child run.
    let Some(matcher) = state.write_matcher() else {
        reply_continue(nfd, req.id);
        return;
    };

    // Which direction to judge this op in. Every trapped syscall but `open{at,at2}` mutates by
    // definition; an open is whichever its flags say. When reads are not brokered the filter
    // never delivers a read-only open here at all, so this is simply always `true` then.
    //
    // `openat2` carries its flags in a `struct open_how` IN THE CHILD, so the fetch can fail. A
    // failed fetch judges the op as a WRITE, which is the strict direction: a read misjudged as
    // a write can only be refused, never wrongly allowed. The perform step below re-reads the
    // struct for its own use rather than threading this copy through, because the ID re-check
    // between here and there is what makes the second read the authoritative one.
    let wants_write = match nr {
        n if n == libc::SYS_openat => a[2] as u32 & write_open_mask() != 0,
        n if n == libc::SYS_openat2 => {
            let mut how = OpenHow::default();
            let got = unsafe {
                read_child_mem(
                    req.pid,
                    a[2],
                    std::slice::from_raw_parts_mut(
                        &mut how as *mut OpenHow as *mut u8,
                        std::mem::size_of::<OpenHow>(),
                    ),
                )
            };
            got != std::mem::size_of::<OpenHow>() as isize
                || how.flags as u32 & write_open_mask() != 0
        }
        // `access(W_OK)` asks a write question; `R_OK`/`X_OK`/`F_OK` ask a read one. Judging it
        // at all is what stops `test -r` reporting a secret-floor file as readable when an
        // actual open of it would be refused — the answer now comes from the same rule set the
        // open would consult, so the two can no longer disagree.
        n if n == libc::SYS_faccessat || n == libc::SYS_faccessat2 => a[2] as i32 & libc::W_OK != 0,
        _ => true,
    };
    // A RE-OPEN of a descriptor the child already holds — `/proc/self/fd/<n>`, which
    // `resolve_parent` rewrites to the child's own pid. It is not a location, so there is no
    // filesystem grant to check: whatever the descriptor points at, the child has it open
    // already and re-opening reaches nothing new. Landlock does not govern this either, which is
    // how the sealed CA-bundle memfd reaches a confined child at all — a memfd lives on an
    // internal mount that Landlock refuses as a rule target, so it is handed over as an
    // inherited fd and named `/proc/self/fd/<n>` in the child's environment.
    //
    // ⚠️ RESIDUAL, stated rather than hidden: a re-open may request a wider access MODE than the
    // descriptor was opened with, and for a regular file Landlock would judge that against the
    // underlying path where this does not. Narrow, and the alternative is refusing an fd the
    // child demonstrably already has.
    // Both spellings the child can use for its OWN descriptor directory. `resolve_parent`
    // rewrites `/proc/self/` to the notification's tid, and a child naming its tgid outright
    // reaches the same place; every OTHER pid stays a policy question, which is what keeps this
    // from becoming a way to read another process's descriptors.
    let own_fd_dirs = [
        format!("/proc/{}/fd", req.pid).into_bytes(),
        format!("/proc/{}/fd", tgid_of(req.pid)).into_bytes(),
    ];
    // The one policy predicate for this notification, so the pre-check and the post-open
    // readback below cannot drift into judging the same op two different ways.
    let permits = |canon: &[u8]| {
        if wants_write {
            write_allowed(&matcher, canon)
        } else {
            read_allowed(&matcher, canon)
        }
    };

    let resolves_second =
        nr == libc::SYS_linkat || nr == libc::SYS_renameat || nr == libc::SYS_renameat2;

    let mut pfd: RawFd = -1;
    let mut pfd2: RawFd = -1;
    let mut addfd_src: RawFd = -1;
    let mut cloexec = false;
    let mut err: i32 = 0;
    let mut val: i64 = 0;

    // ---- policy: resolve, then deny before performing anything ----
    'act: {
        let (p, canon, base) = match resolve_parent(req.pid, dfd, &path) {
            Ok(v) => v,
            Err(e) => {
                err = e;
                break 'act;
            }
        };
        pfd = p;
        let full = join_full(&canon, &base);
        // A RE-OPEN of a descriptor the child already holds is not a location, so the policy
        // has nothing to say about it — neither here nor at the readback below. Whatever the
        // descriptor points at, the child has it open already and re-opening reaches nothing
        // new. The readback half is the one that matters in practice: the sealed CA-bundle
        // memfd reads back as `/memfd:… (deleted)`, which no fs rule can ever name, so a
        // policy check there refuses the one fd the supervisor deliberately handed over.
        //
        // ⚠️ RESIDUAL, stated rather than hidden: a re-open may request a wider access MODE
        // than the descriptor was opened with, and for a regular file Landlock would judge that
        // against the underlying path where this does not. Narrow, and the alternative is
        // refusing an fd the child demonstrably already has.
        // A DESCRIPTOR under that directory, never the directory itself: `/proc/self/fd` is a
        // listing of what the child holds open, and a bare `starts_with` handed that listing
        // over — `production_readiness_linux` and the CA test both caught it.
        let own_fd = own_fd_dirs
            .iter()
            .any(|dir| full.starts_with(dir) && full.get(dir.len()) == Some(&b'/'));
        if !own_fd && !permits(&full) {
            // WHICH errno a refusal carries is part of the contract for `access`, and getting it
            // wrong broke `git config --global`. `access` is a QUESTION about permission whose
            // callers branch on the answer: git's `access_or_die` treats ENOENT and EACCES as
            // "no" and everything else as fatal, so answering EPERM turned a probe for a
            // `~/.gitconfig` that does not even exist into `fatal: unable to access`. Say what a
            // permission failure normally says — and when the path is genuinely absent, say
            // THAT, which is true and reveals nothing the policy protects (A2.3 already concedes
            // the mechanism cannot hide a path's existence). Every other trapped syscall keeps
            // EPERM: those PERFORM something, and a refusal to act is not a missing file.
            err = match nr {
                n if n == libc::SYS_faccessat || n == libc::SYS_faccessat2 => {
                    let absent = unsafe {
                        libc::faccessat(pfd, cstr(&base).as_ptr(), libc::F_OK, 0) < 0
                            && errno() == libc::ENOENT
                    };
                    if absent { libc::ENOENT } else { libc::EACCES }
                }
                _ => libc::EPERM,
            };
            suplog!(
                "SUP DENY {} {} -> {err}",
                if wants_write { "write" } else { "read" },
                String::from_utf8_lossy(&full)
            );
            break 'act;
        }

        let mut base2: Vec<u8> = Vec::new();
        if resolves_second {
            let path2_ref = path2.as_deref().unwrap_or_default();
            let (p2, canon2, b2) = match resolve_parent(req.pid, dfd2, path2_ref) {
                Ok(v) => v,
                Err(e) => {
                    err = e;
                    break 'act;
                }
            };
            pfd2 = p2;
            base2 = b2;
            let full2 = join_full(&canon2, &base2);
            if !write_allowed(&matcher, &full2) {
                suplog!(
                    "SUP DENY write {} -> EPERM",
                    String::from_utf8_lossy(&full2)
                );
                err = libc::EPERM;
                break 'act;
            }
            // linkat(AT_SYMLINK_FOLLOW) aliases the TARGET of a source symlink; the new hard link
            // grants write to whatever the source really points at, so that real path must ITSELF
            // be write-allowed (wdeny self-review 2026-09-04).
            if nr == libc::SYS_linkat && (a[4] as i32 & libc::AT_SYMLINK_FOLLOW) != 0 {
                let srcjoin = join_full(&canon, &base);
                let src_c = CString::new(srcjoin).ok();
                let mut srcreal = vec![0u8; libc::PATH_MAX as usize];
                let ok = src_c.as_ref().map(|c| unsafe {
                    !libc::realpath(c.as_ptr(), srcreal.as_mut_ptr() as *mut libc::c_char).is_null()
                });
                let refused = match ok {
                    Some(true) => {
                        let len = unsafe { libc::strlen(srcreal.as_ptr() as *const libc::c_char) };
                        !write_allowed(&matcher, &srcreal[..len])
                    }
                    _ => true, // could not resolve the source → refuse
                };
                if refused {
                    err = libc::EPERM;
                    break 'act;
                }
            }
        }

        // ---- allowed: perform it ourselves on the verified parent fd(s) ----
        match nr {
            n if n == libc::SYS_openat => {
                let flags = a[2] as i32;
                let mode = a[3] as libc::mode_t;
                let fd = unsafe {
                    libc::openat(
                        pfd,
                        cstr(&base).as_ptr(),
                        flags | libc::O_CLOEXEC,
                        mode as libc::c_uint,
                    )
                };
                if fd < 0 {
                    err = errno();
                    break 'act;
                }
                if !own_fd
                    && fd_path(fd)
                        .map(|w| !permits(&w))
                        // Fail CLOSED: a failed readback of the just-opened fd's real path
                        // cannot prove the open did not escape the allow-set through a
                        // final-component symlink swap, so refuse rather than addfd it.
                        .unwrap_or(true)
                {
                    unsafe { libc::close(fd) };
                    err = libc::EPERM;
                    break 'act;
                }
                addfd_src = fd;
                cloexec = flags & libc::O_CLOEXEC != 0;
            }
            n if n == libc::SYS_openat2 => {
                let mut how = OpenHow::default();
                let got = unsafe {
                    read_child_mem(
                        req.pid,
                        a[2],
                        std::slice::from_raw_parts_mut(
                            &mut how as *mut OpenHow as *mut u8,
                            std::mem::size_of::<OpenHow>(),
                        ),
                    )
                };
                if got != std::mem::size_of::<OpenHow>() as isize {
                    err = libc::EFAULT;
                    break 'act;
                }
                let fd = unsafe {
                    libc::syscall(
                        libc::SYS_openat2,
                        pfd,
                        cstr(&base).as_ptr(),
                        &how as *const OpenHow,
                        std::mem::size_of::<OpenHow>(),
                    )
                } as RawFd;
                if fd < 0 {
                    err = errno();
                    break 'act;
                }
                if !own_fd
                    && fd_path(fd)
                        .map(|w| !permits(&w))
                        // Fail CLOSED: a failed readback of the just-opened fd's real path
                        // cannot prove the open did not escape the allow-set through a
                        // final-component symlink swap, so refuse rather than addfd it.
                        .unwrap_or(true)
                {
                    unsafe { libc::close(fd) };
                    err = libc::EPERM;
                    break 'act;
                }
                addfd_src = fd;
                cloexec = how.flags & libc::O_CLOEXEC as u64 != 0;
            }
            n if n == libc::SYS_mkdirat => {
                if unsafe { libc::mkdirat(pfd, cstr(&base).as_ptr(), a[2] as libc::mode_t) } < 0 {
                    err = errno();
                }
            }
            n if n == libc::SYS_unlinkat => {
                if unsafe { libc::unlinkat(pfd, cstr(&base).as_ptr(), a[2] as i32) } < 0 {
                    err = errno();
                }
            }
            n if n == libc::SYS_symlinkat => {
                let target = cstr(path2.as_deref().unwrap_or_default());
                if unsafe { libc::symlinkat(target.as_ptr(), pfd, cstr(&base).as_ptr()) } < 0 {
                    err = errno();
                }
            }
            n if n == libc::SYS_linkat => {
                if unsafe {
                    libc::linkat(
                        pfd,
                        cstr(&base).as_ptr(),
                        pfd2,
                        cstr(&base2).as_ptr(),
                        a[4] as i32,
                    )
                } < 0
                {
                    err = errno();
                }
            }
            n if n == libc::SYS_renameat => {
                if unsafe { libc::renameat(pfd, cstr(&base).as_ptr(), pfd2, cstr(&base2).as_ptr()) }
                    < 0
                {
                    err = errno();
                }
            }
            n if n == libc::SYS_renameat2 => {
                if unsafe {
                    libc::syscall(
                        libc::SYS_renameat2,
                        pfd,
                        cstr(&base).as_ptr(),
                        pfd2,
                        cstr(&base2).as_ptr(),
                        a[4] as libc::c_uint,
                    )
                } < 0
                {
                    err = errno();
                }
            }
            n if n == libc::SYS_truncate => {
                let fd = unsafe {
                    libc::openat(
                        pfd,
                        cstr(&base).as_ptr(),
                        libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                        0,
                    )
                };
                if fd < 0 {
                    err = errno();
                    break 'act;
                }
                if !own_fd
                    && fd_path(fd)
                        .map(|w| !permits(&w))
                        // Fail CLOSED: a failed readback of the just-opened fd's real path
                        // cannot prove the open did not escape the allow-set through a
                        // final-component symlink swap, so refuse rather than addfd it.
                        .unwrap_or(true)
                {
                    unsafe { libc::close(fd) };
                    err = libc::EPERM;
                    break 'act;
                }
                if unsafe { libc::ftruncate(fd, a[1] as libc::off_t) } < 0 {
                    err = errno();
                }
                unsafe { libc::close(fd) };
            }
            // ---- the metadata arms: Landlock has no hook for any of these at any ABI ----
            // All of them perform against the PINNED parent fd and the verified final
            // component, so the path the policy approved is the path that gets mutated.
            n if n == libc::SYS_fchmodat || n == libc::SYS_fchmod => {
                // `fchmodat`'s flag word is dropped deliberately: the kernel's fchmodat takes
                // no `AT_SYMLINK_NOFOLLOW` (glibc emulates it through /proc), and following the
                // final symlink is what the unflagged call already does.
                let mode = if nr == libc::SYS_fchmod { a[1] } else { a[2] } as libc::mode_t;
                if unsafe { libc::fchmodat(pfd, cstr(&base).as_ptr(), mode, 0) } < 0 {
                    err = errno();
                }
            }
            n if n == libc::SYS_fchownat || n == libc::SYS_fchown => {
                let (uid, gid, flags) = if nr == libc::SYS_fchown {
                    (a[1] as libc::uid_t, a[2] as libc::gid_t, 0)
                } else {
                    (a[2] as libc::uid_t, a[3] as libc::gid_t, a[4] as i32)
                };
                if unsafe { libc::fchownat(pfd, cstr(&base).as_ptr(), uid, gid, flags) } < 0 {
                    err = errno();
                }
            }
            n if n == libc::SYS_utimensat => {
                // The `timespec[2]` lives in CHILD memory, so it is copied across before the
                // call; a NULL pointer means "now" and is passed through as NULL.
                let mut ts = [libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                }; 2];
                if a[2] != 0 {
                    let got = unsafe {
                        read_child_mem(
                            req.pid,
                            a[2],
                            std::slice::from_raw_parts_mut(
                                ts.as_mut_ptr() as *mut u8,
                                std::mem::size_of_val(&ts),
                            ),
                        )
                    };
                    if got != std::mem::size_of_val(&ts) as isize {
                        err = libc::EFAULT;
                        break 'act;
                    }
                }
                let times = if a[2] == 0 {
                    std::ptr::null()
                } else {
                    ts.as_ptr()
                };
                if unsafe { libc::utimensat(pfd, cstr(&base).as_ptr(), times, a[3] as i32) } < 0 {
                    err = errno();
                }
            }
            // x86_64's three pre-`utimensat` spellings, which carry a `utimbuf` or a
            // `timeval[2]` rather than a `timespec[2]`. Converted here instead of folded, since
            // a fold cannot change the shape of the struct the child wrote.
            #[cfg(target_arch = "x86_64")]
            n if n == libc::SYS_utime || n == libc::SYS_utimes || n == libc::SYS_futimesat => {
                let times_ptr = if n == libc::SYS_futimesat { a[2] } else { a[1] };
                let mut ts = [libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                }; 2];
                if times_ptr != 0 {
                    let mut raw = [0i64; 4];
                    let want = if n == libc::SYS_utime { 16 } else { 32 };
                    let got = unsafe {
                        read_child_mem(
                            req.pid,
                            times_ptr,
                            std::slice::from_raw_parts_mut(raw.as_mut_ptr() as *mut u8, want),
                        )
                    };
                    if got != want as isize {
                        err = libc::EFAULT;
                        break 'act;
                    }
                    if n == libc::SYS_utime {
                        // struct utimbuf { time_t actime; time_t modtime; }
                        ts[0].tv_sec = raw[0];
                        ts[1].tv_sec = raw[1];
                    } else {
                        // struct timeval[2] { time_t tv_sec; suseconds_t tv_usec; }
                        ts[0].tv_sec = raw[0];
                        ts[0].tv_nsec = raw[1] * 1000;
                        ts[1].tv_sec = raw[2];
                        ts[1].tv_nsec = raw[3] * 1000;
                    }
                }
                let times = if times_ptr == 0 {
                    std::ptr::null()
                } else {
                    ts.as_ptr()
                };
                if unsafe { libc::utimensat(pfd, cstr(&base).as_ptr(), times, 0) } < 0 {
                    err = errno();
                }
            }
            n if n == libc::SYS_faccessat || n == libc::SYS_faccessat2 => {
                // Answered from the real filesystem once the policy has allowed the question,
                // so a granted-but-genuinely-unreadable file still reports EACCES. The
                // supervisor runs as the same uid as the child, so the kernel's answer is the
                // child's answer. `faccessat2`'s flag word is honoured; `faccessat` has none.
                let flags = if nr == libc::SYS_faccessat2 {
                    a[3] as i32
                } else {
                    0
                };
                if unsafe {
                    libc::syscall(
                        libc::SYS_faccessat2,
                        pfd,
                        cstr(&base).as_ptr(),
                        a[2] as i32,
                        flags,
                    )
                } < 0
                {
                    err = errno();
                }
            }
            _ => {}
        }
    }

    // ---- reply: splice an opened fd, or return the errno/value ----
    if addfd_src >= 0 {
        let mut af = SeccompNotifAddfd {
            id: req.id,
            srcfd: addfd_src as u32,
            newfd_flags: if cloexec { libc::O_CLOEXEC as u32 } else { 0 },
            ..Default::default()
        };
        let newfd = ioctl_notif(nfd, notif_addfd(), &mut af as *mut _ as *mut libc::c_void);
        unsafe { libc::close(addfd_src) };
        if newfd < 0 {
            err = libc::EMFILE;
        } else {
            val = newfd as i64;
        }
    }
    let mut r = SeccompNotifResp {
        id: req.id,
        val: if err != 0 { 0 } else { val },
        error: if err != 0 { -err } else { 0 },
        ..Default::default()
    };
    if ioctl_notif(nfd, notif_send(), &mut r as *mut _ as *mut libc::c_void) < 0 {
        suplog!("[sup] SEND: {}", io::Error::last_os_error());
    }
    if pfd >= 0 {
        unsafe { libc::close(pfd) };
    }
    if pfd2 >= 0 {
        unsafe { libc::close(pfd2) };
    }
}

/// A `CString` for a path component, empty on an interior NUL (which cannot occur in a real
/// path component but keeps the perform step total).
fn cstr(s: &[u8]) -> CString {
    CString::new(s).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// the supervisor loop
// ---------------------------------------------------------------------------

/// One cancellation descriptor wakes both notification polling and outbound I/O. Keeping it
/// separate from the listener avoids closing a descriptor while another thread is using it.
struct WorkerControl(OwnedFd);

impl WorkerControl {
    fn new() -> io::Result<Self> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    fn cancel(&self) {
        let value = 1u64;
        // An already-readable eventfd remains cancelled; no thread drains it.
        unsafe {
            libc::write(
                self.0.as_raw_fd(),
                &value as *const _ as *const libc::c_void,
                8,
            )
        };
    }

    fn cancelled(&self) -> bool {
        let mut fd = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        (unsafe { libc::poll(&mut fd, 1, 0) }) > 0 && fd.revents != 0
    }

    fn wait(
        &self,
        fd: RawFd,
        events: i16,
        timeout: Option<std::time::Duration>,
    ) -> io::Result<bool> {
        let deadline = timeout.map(|timeout| std::time::Instant::now() + timeout);
        loop {
            let millis = deadline.map_or(-1, |end| {
                end.saturating_duration_since(std::time::Instant::now())
                    .as_millis()
                    .min(i32::MAX as u128) as i32
            });
            let mut fds = [
                libc::pollfd {
                    fd: self.0.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                },
            ];
            let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if result == 0 || fds[0].revents != 0 {
                return Ok(false);
            }
            // Deliver readable bytes even alongside HUP (e.g. the final proxy response).
            return Ok(fds[1].revents & events != 0);
        }
    }
}

/// Dial without blocking shutdown on the kernel's TCP retry budget. The returned socket keeps
/// its original blocking disposition; the caller later applies the target's socket flags.
unsafe fn connect_interruptible(
    control: &WorkerControl,
    fd: RawFd,
    addr: *const libc::sockaddr,
    len: libc::socklen_t,
) -> i32 {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return -1;
    }
    let result = unsafe { libc::connect(fd, addr, len) };
    let result = if result == 0 {
        0
    } else if errno() == libc::EINPROGRESS {
        if control
            .wait(fd, libc::POLLOUT, Some(std::time::Duration::from_secs(30)))
            .unwrap_or(false)
        {
            let mut error = 0i32;
            let mut size = size_of::<i32>() as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    &mut error as *mut _ as *mut libc::c_void,
                    &mut size,
                )
            } == 0
                && error == 0
            {
                0
            } else {
                unsafe { *libc::__errno_location() = if error == 0 { libc::EIO } else { error } };
                -1
            }
        } else {
            unsafe { *libc::__errno_location() = libc::ECANCELED };
            -1
        }
    } else {
        -1
    };
    let error = errno();
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } < 0 {
        return -1;
    }
    unsafe { *libc::__errno_location() = error };
    result
}

fn supervisor(listener: OwnedFd, mut state: SupState, control: Arc<WorkerControl>) {
    let nfd = listener.as_raw_fd();
    let mut dns_slot = 0u32;
    loop {
        // Wait for a notification OR a listener hangup. `poll` separates two events a bare
        // `NOTIF_RECV` conflates onto one ENOENT: "the target died" (exit — the filter is being
        // torn down) and "THIS notification's task was reaped between wake and RECV" (a transient
        // under load that MUST be re-blocked, not fatal). POLLHUP fires only for the first, giving
        // the one clean exit; a transient ENOENT after POLLIN just loops. The old loop returned on
        // any RECV error, which treats the transient as fatal — a documented race. NOTE: it did not
        // reproduce here — a control on kernel 6.17 ran 64 parallel curls + `npm install` clean
        // with the exit-on-error loop too (the earlier "0/8" was a test-harness `xargs -I _` bug,
        // not the supervisor). This is hardening against the race the kernel docs and the route.c
        // measurement (other kernels) describe, not a fix for a failure observed on this host.
        if !control.wait(nfd, libc::POLLIN, None).unwrap_or(false) {
            return;
        }
        let mut req: SeccompNotif = unsafe { std::mem::zeroed() };
        if ioctl_notif(nfd, notif_recv(), &mut req as *mut _ as *mut libc::c_void) < 0 {
            // EINTR: interrupted. ENOENT: the notifying task was reaped between `poll` and `RECV`
            // — transient under load, never a reason to tear the supervisor down. Either way,
            // re-block; a genuinely dead target surfaces as POLLHUP above, not here.
            continue;
        }
        let nr = req.data.nr as libc::c_long;
        let cfd = req.data.args[0] as i32;

        if !state.self_proc.is_empty() && self_proc::handle_open(&state, nfd, &req) {
            continue;
        }

        // Signal-sending syscalls: scoped to the guardian's process group. Trapped on every
        // supervised launch, so this arm is always live.
        if is_signal_intent(nr) {
            handle_signal_intent(&state, nfd, &req);
            continue;
        }

        // Filesystem syscalls: the deny-inside-allow broker, plus the metadata calls Landlock has
        // no hook for. Reached only when the filter was built with that branch — a policy carrying
        // carve-outs, which since the secret floor is every read-granting policy.
        if is_fs_intent(nr) {
            handle_fs_intent(&state, nfd, &req);
            continue;
        }

        // DNS-socket recv*: observe the reply without consuming it, then CONTINUE.
        if nr == libc::SYS_recvfrom || nr == libc::SYS_read || nr == libc::SYS_recvmsg {
            let tg = tgid_of(req.pid);
            let dup = {
                let st = &state;
                st.sk
                    .iter()
                    .find(|e| e.tgid == tg && e.fd == cfd && e.dns)
                    .map(|e| e.dup)
                    .unwrap_or(-1)
            };
            if dup >= 0 {
                let mut r = [0u8; 4096];
                let ready = control
                    .wait(dup, libc::POLLIN, Some(std::time::Duration::from_secs(3)))
                    .unwrap_or(false);
                if !ready {
                    reply_continue(nfd, req.id);
                    continue;
                }
                let rn = unsafe {
                    libc::recv(
                        dup,
                        r.as_mut_ptr() as *mut libc::c_void,
                        r.len(),
                        libc::MSG_PEEK | libc::MSG_DONTWAIT,
                    )
                };
                if rn > 0 {
                    parse_response(&mut state, &r[..rn as usize]);
                }
            }
            reply_continue(nfd, req.id);
            continue;
        }

        // `sendmmsg` writes `msg_len` output to target memory.  The USER_NOTIF ABI has no safe
        // way to make that write: a cancelled target may have already reused the address.  Give
        // callers ENOSYS so their usual sendmsg/sendto fallback remains available.
        if nr == libc::SYS_sendmmsg {
            reply(nfd, req.id, -libc::ENOSYS);
            continue;
        }

        // Replay connected IP sendto/sendmsg from supervisor-owned snapshots.  CONTINUE is
        // unsafe after fd classification because a concurrent dup2 can replace the numeric fd
        // before the kernel executes it.  Non-IP sockets deliberately return ENOSYS: replaying
        // AF_UNIX changes SO_PASSCRED/SO_PASSSEC sender identity, which this zero-privilege
        // supervisor cannot faithfully emulate.
        if nr == libc::SYS_sendto || nr == libc::SYS_sendmsg || nr == libc::SYS_sendmmsg {
            let tgid = tgid_of(req.pid);
            let (socket, domain, kind) = match duplicate_child_socket(tgid, cfd) {
                Ok(socket) => socket,
                // Emulate the observed non-socket result instead of continuing a mutable numeric
                // fd that could be replaced with an egress socket before the kernel runs it.
                Err(libc::ENOTSOCK) => {
                    reply(nfd, req.id, -libc::ENOTSOCK);
                    continue;
                }
                Err(error) => {
                    suplog!("SUP DENY send*: could not duplicate fd {cfd}: errno={error}");
                    reply(nfd, req.id, -libc::EPERM);
                    continue;
                }
            };
            let ip_socket = domain == libc::AF_INET || domain == libc::AF_INET6;
            if !ip_socket {
                reply(nfd, req.id, -libc::ENOSYS);
                continue;
            }
            if kind != libc::SOCK_DGRAM && kind != libc::SOCK_STREAM {
                reply(nfd, req.id, -libc::ENOSYS);
                continue;
            }

            let mem = match open_child_mem(req.pid) {
                Ok(mem) => mem,
                Err(_) => {
                    reply(nfd, req.id, -libc::EPERM);
                    continue;
                }
            };
            if !notification_is_live(nfd, req.id) {
                continue;
            }
            let snapshot = if nr == libc::SYS_sendto {
                snapshot_sendto(mem.as_raw_fd(), &req)
            } else {
                child_struct::<libc::msghdr>(mem.as_raw_fd(), req.data.args[1])
                    .and_then(|hdr| snapshot_msghdr(mem.as_raw_fd(), hdr))
            };
            let snapshot = match snapshot {
                Ok(snapshot) => snapshot,
                Err(libc::EPERM) => {
                    suplog!(
                        "SUP DENY send*: named IP destination or unreplayable credentials -> EPERM"
                    );
                    if notification_is_live(nfd, req.id) {
                        reply(nfd, req.id, -libc::EPERM);
                    }
                    continue;
                }
                Err(error) => {
                    if notification_is_live(nfd, req.id) {
                        reply(nfd, req.id, -error);
                    }
                    continue;
                }
            };
            if !notification_is_live(nfd, req.id) {
                continue;
            }
            let flags = if nr == libc::SYS_sendto {
                req.data.args[3] as i32
            } else {
                req.data.args[2] as i32
            };
            match send_snapshot(&control, nfd, req.id, socket.as_raw_fd(), &snapshot, flags) {
                Ok(sent) => {
                    // This check cannot close the final cancellation/send race, but it avoids a
                    // stale response when cancellation happened during the replay itself.
                    if notification_is_live(nfd, req.id) {
                        reply_value(nfd, req.id, sent as i64);
                    }
                }
                Err(SendReplayError::Errno(error)) => {
                    if notification_is_live(nfd, req.id) {
                        reply(nfd, req.id, -error);
                    }
                }
                Err(SendReplayError::Abandoned) => {}
            }
            continue;
        }

        // socket(): create it ourselves and ADDFD it into the child.
        if nr == libc::SYS_socket {
            let dom = req.data.args[0] as i32;
            let typ = req.data.args[1] as i32;
            let pro = req.data.args[2] as i32;
            if dom != libc::AF_INET && dom != libc::AF_INET6 {
                reply_continue(nfd, req.id);
                continue;
            }
            let s = unsafe { libc::socket(dom, typ | libc::SOCK_CLOEXEC, pro) };
            if s < 0 {
                reply(nfd, req.id, -errno());
                continue;
            }
            let mut af = SeccompNotifAddfd {
                id: req.id,
                srcfd: s as u32,
                newfd_flags: if typ & libc::SOCK_CLOEXEC != 0 {
                    libc::O_CLOEXEC as u32
                } else {
                    0
                },
                ..Default::default()
            };
            let is_dgram = (typ & 0xff) == libc::SOCK_DGRAM;
            if is_dgram {
                af.flags = SECCOMP_ADDFD_FLAG_SETFD;
                let slot = dns_slot % 64;
                dns_slot = dns_slot.wrapping_add(1);
                af.newfd = DNS_FD_LO + slot;
            }
            let mut newfd = ioctl_notif(nfd, notif_addfd(), &mut af as *mut _ as *mut libc::c_void);
            unsafe { libc::close(s) };
            if newfd >= 0 && af.flags & SECCOMP_ADDFD_FLAG_SETFD != 0 {
                newfd = af.newfd as libc::c_int;
            }
            if newfd < 0 {
                reply(nfd, req.id, -libc::EMFILE);
                continue;
            }
            {
                let st = &mut state;
                st.sk_put(tgid_of(req.pid), newfd, dom, typ & 0xff);
            }
            let mut r = SeccompNotifResp {
                id: req.id,
                val: newfd as i64,
                ..Default::default()
            };
            ioctl_notif(nfd, notif_send(), &mut r as *mut _ as *mut libc::c_void);
            continue;
        }

        // The remaining notified syscall is connect(): classify, read the destination
        // ONCE, confirm the notification is still live, then dial it ourselves.
        let tgid = tgid_of(req.pid);
        let (mut dom, mut typ): (i32, i32) = (-1, -1);
        let by_construction = {
            let st = &state;
            match st.sk_get(tgid, cfd) {
                Some((d, t)) => {
                    dom = d;
                    typ = t;
                    true
                }
                None => false,
            }
        };
        let mut pidfd: RawFd = -1;
        let sfd: RawFd = if by_construction {
            -2 // classified, no dup needed
        } else {
            pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, tgid, 0) } as RawFd;
            if pidfd >= 0 {
                unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd, cfd, 0) as RawFd }
            } else {
                -1
            }
        };
        if sfd >= 0 {
            let mut ol = size_of::<i32>() as libc::socklen_t;
            unsafe {
                libc::getsockopt(
                    sfd,
                    libc::SOL_SOCKET,
                    libc::SO_TYPE,
                    &mut typ as *mut _ as *mut libc::c_void,
                    &mut ol,
                );
                ol = size_of::<i32>() as libc::socklen_t;
                libc::getsockopt(
                    sfd,
                    libc::SOL_SOCKET,
                    libc::SO_DOMAIN,
                    &mut dom as *mut _ as *mut libc::c_void,
                    &mut ol,
                );
            }
        }

        // read the destination sockaddr ONCE from child memory
        let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let want = req.data.args[2] as usize;
        let alen = want.min(size_of::<libc::sockaddr_storage>());
        let ss_bytes = unsafe {
            std::slice::from_raw_parts_mut(
                &mut ss as *mut _ as *mut u8,
                size_of::<libc::sockaddr_storage>(),
            )
        };
        let got = unsafe { read_child_mem(req.pid, req.data.args[1], &mut ss_bytes[..alen]) };

        // notification must still be live, and the read must have yielded a sockaddr_in
        let id_live = ioctl_notif(nfd, notif_id_valid(), &mut { req.id } as *mut u64
            as *mut libc::c_void)
            >= 0;
        if !id_live || got < size_of::<libc::sockaddr_in>() as isize || sfd == -1 {
            suplog!("SUP DENY (unreadable: got={got} sfd={sfd}) -> EPERM");
            if sfd >= 0 {
                unsafe { libc::close(sfd) };
            }
            if pidfd >= 0 {
                unsafe { libc::close(pidfd) };
            }
            reply(nfd, req.id, -libc::EPERM);
            continue;
        }

        if dom != libc::AF_INET && dom != libc::AF_INET6 {
            suplog!("SUP CONTINUE dom={dom}");
            if sfd >= 0 {
                unsafe { libc::close(sfd) };
            }
            if pidfd >= 0 {
                unsafe { libc::close(pidfd) };
            }
            reply_continue(nfd, req.id);
            continue;
        }

        let fam = ss.ss_family as i32;
        let (port, addr): (u16, [u8; 16]) = if fam == libc::AF_INET {
            let a = unsafe { &*(&ss as *const _ as *const libc::sockaddr_in) };
            let mut buf = [0u8; 16];
            buf[..4].copy_from_slice(&a.sin_addr.s_addr.to_ne_bytes());
            (u16::from_be(a.sin_port), buf)
        } else {
            let a = unsafe { &*(&ss as *const _ as *const libc::sockaddr_in6) };
            (u16::from_be(a.sin6_port), a.sin6_addr.s6_addr)
        };
        let n = if fam == libc::AF_INET { 4 } else { 16 };
        let ip = fmt_ip(fam, &addr[..n]);

        let mut verdict_err = -libc::EPERM;
        let mut s: RawFd = -1;
        if typ == libc::SOCK_DGRAM {
            if port == 53 {
                let (upstream_be,) = {
                    let st = &state;
                    (st.upstream_addr_be,)
                };
                s = unsafe {
                    libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0)
                };
                let up = make_sockaddr_in(upstream_be, 53);
                if unsafe {
                    libc::connect(
                        s,
                        &up as *const _ as *const libc::sockaddr,
                        size_of::<libc::sockaddr_in>() as libc::socklen_t,
                    )
                } == 0
                {
                    verdict_err = 0;
                    suplog!(
                        "SUP DNS-UDP asked {ip}:{port} -> dialed upstream {}:53 (observed via peek)",
                        fmt_ip(libc::AF_INET, &upstream_be.to_ne_bytes())
                    );
                    let dupfd = unsafe { libc::fcntl(s, libc::F_DUPFD_CLOEXEC, 3) };
                    let st = &mut state;
                    if let Some(e) = st.sk.iter_mut().find(|e| e.tgid == tgid && e.fd == cfd) {
                        if e.dup >= 0 {
                            unsafe { libc::close(e.dup) };
                        }
                        e.dns = true;
                        e.dup = dupfd;
                    } else if dupfd >= 0 {
                        unsafe { libc::close(dupfd) };
                    }
                } else {
                    suplog!("SUP DENY UDP {ip}:{port} (dial upstream failed)");
                }
            } else {
                suplog!("SUP DENY UDP {ip}:{port}");
            }
        } else if typ == libc::SOCK_STREAM {
            let name = {
                let st = &state;
                st.lookup(fam, &addr[..n])
            };
            // The proxy config, copied from `EgressPolicy` before the fork (epic 5.1). `Some` ⇒ a
            // loopback SNI-inspecting proxy is authoritative for EVERY child connect except a dial
            // of its own exact `127.0.0.1:<port>` listener. That narrow exception prevents proxy
            // recursion; a general loopback carve-out would let a sibling relay carry a denied host
            // past the hostname policy. `None` ⇒ the coarse observed-name allowlist decides (epic
            // 1.5).
            let proxy = {
                let st = &state;
                st.proxy_port
                    .map(|p| (p, st.proxy_token.clone().unwrap_or_default()))
            };
            let proxy_endpoint = proxy
                .as_ref()
                .is_some_and(|(pport, _)| is_egress_proxy_endpoint(fam, &addr[..n], port, *pport));
            if let (false, Some((pport, ptoken))) = (proxy_endpoint, proxy) {
                // Transparent redirect: dial the loopback proxy and speak the cooperative HTTP
                // CONNECT on the child's behalf. The authority is the observed name when known
                // (exact for the common one-host-per-IP case), else the IP literal (the proxy's
                // host gate fail-closes a hardcoded-IP / DoH connect that has no allowlisted IP
                // rule). The proxy then reads the child's ClientHello SNI as the authoritative
                // per-host gate — closing the shared-IP leak 1.5 left (a DENIED host riding an
                // ALLOWED host's IP is dropped at the SNI gate). On ANY dial/handshake failure we
                // deny; never fall back to a direct dial, which would bypass the SNI gate. (5.1 L3/L4)
                let authority = name.clone().unwrap_or_else(|| ip.clone());
                s = proxy_connect_tcp(&control, pport, &ptoken, &authority, port);
                if s >= 0 {
                    verdict_err = 0;
                    suplog!("SUP PROXY {ip}:{port} authority={authority} -> 127.0.0.1:{pport}");
                } else {
                    verdict_err = -libc::EPERM;
                    suplog!("SUP DENY (proxy) {ip}:{port} authority={authority} -> EPERM");
                }
            } else {
                let allow = {
                    let st = &state;
                    st.allowed(name.as_deref())
                };
                if direct_dial_allowed(proxy_endpoint, allow) {
                    s = unsafe { libc::socket(fam, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
                    if unsafe {
                        connect_interruptible(
                            &control,
                            s,
                            &ss as *const _ as *const libc::sockaddr,
                            alen as libc::socklen_t,
                        )
                    } == 0
                    {
                        verdict_err = 0;
                        suplog!(
                            "SUP ALLOW {ip}:{port} name={}",
                            name.as_deref().unwrap_or("(none)")
                        );
                    } else {
                        verdict_err = -errno();
                        suplog!(
                            "SUP dial {ip}:{port} failed: {}",
                            io::Error::last_os_error()
                        );
                    }
                } else {
                    suplog!(
                        "SUP DENY {ip}:{port} name={} -> EPERM",
                        name.as_deref().unwrap_or("(none: no observed DNS)")
                    );
                }
            }
        } else {
            suplog!("SUP DENY type={typ} fam={fam}");
        }

        // Preserve the child's non-blocking disposition on the spliced socket. The child (curl,
        // Node, git — any event-loop client) sets O_NONBLOCK on its own socket before connect, but
        // ADDFD replaces that descriptor with our freshly-dialed BLOCKING socket, dropping the
        // flag. The child then issues a read expecting non-blocking semantics; on a blocking
        // socket it parks in the kernel and never returns to its own event loop to honor its
        // timeout — a hang (MEASURED: `curl https://` completes the TLS handshake, then blocks
        // forever in recvfrom on the spliced socket, past `--max-time`). `sfd` is our pidfd copy
        // of the child's original socket, so its flags are the child's actual flags at connect.
        if verdict_err == 0 && s >= 0 {
            // Read the child's ACTUAL socket flags. `sfd` is -2 on the common `by_construction`
            // path (the socket was supervisor-created), so grab a fresh dup of the child's fd —
            // the child set O_NONBLOCK on that shared description before connect.
            let mut flag_fd = sfd;
            let mut flag_pidfd = -1;
            if flag_fd < 0 {
                flag_pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, tgid, 0) } as RawFd;
                flag_fd = if flag_pidfd >= 0 {
                    unsafe { libc::syscall(libc::SYS_pidfd_getfd, flag_pidfd, cfd, 0) as RawFd }
                } else {
                    -1
                };
            }
            if flag_fd >= 0 {
                let child_flags = unsafe { libc::fcntl(flag_fd, libc::F_GETFL) };
                if child_flags >= 0 && child_flags & libc::O_NONBLOCK != 0 {
                    let before = unsafe { libc::fcntl(s, libc::F_GETFL) };
                    if before >= 0 {
                        unsafe { libc::fcntl(s, libc::F_SETFL, before | libc::O_NONBLOCK) };
                    }
                }
            }
            if flag_fd >= 0 && flag_fd != sfd {
                unsafe { libc::close(flag_fd) };
            }
            if flag_pidfd >= 0 {
                unsafe { libc::close(flag_pidfd) };
            }
        }

        if verdict_err == 0 {
            let mut af = SeccompNotifAddfd {
                id: req.id,
                flags: SECCOMP_ADDFD_FLAG_SETFD,
                srcfd: s as u32,
                newfd: cfd as u32,
                ..Default::default()
            };
            if ioctl_notif(nfd, notif_addfd(), &mut af as *mut _ as *mut libc::c_void) < 0 {
                suplog!("[sup] ADDFD: {}", io::Error::last_os_error());
                verdict_err = -libc::EPERM;
            }
        }
        if s >= 0 {
            unsafe { libc::close(s) };
        }
        if sfd >= 0 {
            unsafe { libc::close(sfd) };
        }
        if pidfd >= 0 {
            unsafe { libc::close(pidfd) };
        }
        reply(nfd, req.id, verdict_err);
    }
}

// ---------------------------------------------------------------------------
// public entry: run a command under the transparent egress supervisor
// ---------------------------------------------------------------------------

/// The egress allowlist plus (for a supervised launch that confines the filesystem) the full fs
/// policy the write broker enforces.
pub struct EgressPolicy {
    pub self_proc: BTreeSet<SelfProcFile>,
    pub allow_all: bool,
    pub allow: Vec<String>,
    /// `Some` ⇒ arm the fs broker as THE authority for the opens it traps, enforcing this whole
    /// fs policy (allow base + deny carve-outs). `None` ⇒ no filesystem confinement on this
    /// launch, so the filter traps no fs syscall at all (any net-only policy).
    pub fs_policy: Option<FsRuleSet>,
    /// `Some` ⇒ a loopback SNI-inspecting egress proxy is running; the connect-notifier redirects
    /// an allowed TCP connect THROUGH it (speaking the cooperative `CONNECT` handshake on the
    /// child's behalf) instead of dialing the destination directly, so per-host precision comes
    /// from the proxy's SNI gate rather than the coarse observed-IP allowlist. `None` ⇒ no proxy,
    /// so the notifier dials the destination directly (the coarse path, epic 1.5). Paired with
    /// [`Self::proxy_token`]. (epic 5.1)
    pub proxy_port: Option<u16>,
    /// The per-session bearer the supervisor presents to the loopback proxy (HTTP
    /// `Proxy-Authorization: Basic base64("<token>:")`). Always present when [`Self::proxy_port`]
    /// is. (epic 5.1)
    pub proxy_token: Option<String>,
}

impl EgressPolicy {
    /// Whether this policy arms the write broker. Governs whether the BPF filter includes the
    /// write-intent dispatch, so a launch that does not confine the filesystem pays nothing.
    fn write_broker(&self) -> bool {
        self.fs_policy.is_some()
    }

    /// Whether to trap READ-intent opens too. Gated on the policy actually carrying a Deny,
    /// because that is the only thing Landlock cannot already enforce: with no deny, Landlock's
    /// allow-only ruleset IS the read answer and every notification would be a round trip to
    /// reach the same verdict. With one, a read of the denied path is invisible to Landlock and
    /// this is the only layer that can refuse it.
    ///
    /// The cost is real and deliberate — every `openat` in the child becomes a supervisor round
    /// trip, where a write-only broker trapped almost nothing. That is the price of the `.env*`
    /// floor meaning anything for reads, which is the direction that matters for a secret.
    fn read_broker(&self) -> bool {
        // INTERNAL differential pin, for COST measurement only. Read-brokering costs a supervisor
        // round trip on every `openat`, and the only honest way to price that is to run the same
        // workload on ONE host with it on and off. Not a user knob, not documented as one, and it
        // can only ever make the sandbox WEAKER — so it is spelled as an opt-out that must be
        // typed exactly, never as a policy the compiler can reach.
        if std::env::var("NUB_SANDBOX_READ_BROKER").as_deref() == Ok("off") {
            return false;
        }
        self.fs_policy.as_ref().is_some_and(|set| {
            set.entries
                .iter()
                .any(|rule| rule.effect == crate::policy::Effect::Deny)
        })
    }
}

// ---------------------------------------------------------------------------
// The library launch path: a bespoke-fork confined child + its supervisor thread.
// ---------------------------------------------------------------------------

/// A confined child launched by [`spawn_supervised`]: the bespoke-forked pid plus the
/// supervisor thread servicing its `USER_NOTIF` listener. It mirrors the slice of
/// `std::process::Child` the sandbox launch path uses — [`id`](Self::id), [`wait`](Self::wait),
/// process-group signalling, and a kill-and-reap `Drop`.
///
/// WHY NOT `std::process::Child`. The listener-fd handoff needs a pre-`execve` BARRIER — the
/// child writes the listener fd number, then blocks until the parent has `pidfd_getfd`'d it —
/// and a blocking `pre_exec` deadlocks `Command::spawn`'s internal CLOEXEC exec-sync pipe.
/// Leaking the listener into the execve'd target instead is a sandbox ESCAPE (the target could
/// answer its own notifications with CONTINUE). So the supervised path forks directly;
/// `Command`/`pre_exec` stays for the no-listener path.
pub(super) struct SupervisedChild {
    pid: libc::pid_t,
    /// The `pidfd` opened to grab the listener; retained for a race-free kill in `Drop`.
    pidfd: RawFd,
    /// Joined after cancellation; the thread owns the notification and DNS descriptors.
    supervisor: Option<std::thread::JoinHandle<()>>,
    control: Arc<WorkerControl>,
    /// Set once `waitpid` has reaped `pid`, so `Drop` neither re-kills nor double-reaps.
    reaped: bool,
    guardian: Option<super::unix_guardian::UnixGuardian>,
    status: Option<std::process::ExitStatus>,
    pub(super) stdin: Option<std::process::ChildStdin>,
    pub(super) stdout: Option<std::process::ChildStdout>,
    pub(super) stderr: Option<std::process::ChildStderr>,
}

impl SupervisedChild {
    pub(super) fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.stdin.take()
    }
    pub(super) fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.stdout.take()
    }
    pub(super) fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.stderr.take()
    }

    pub(super) fn kill(&mut self) -> io::Result<()> {
        self.guardian.take();
        if self.reaped {
            return Ok(());
        }
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd,
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result < 0 && errno() != libc::ESRCH {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.reap(libc::WNOHANG)
    }

    pub(super) fn wait_for_exit_event(&self) -> io::Result<()> {
        let mut event = libc::pollfd {
            fd: self.pidfd,
            events: libc::POLLIN,
            revents: 0,
        };
        // The retained pidfd becomes readable at exit. The timeout bounds
        // cancellation latency without adding a fixed delay to short commands.
        let result = unsafe { libc::poll(&mut event, 1, 20) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        Ok(())
    }

    fn reap(&mut self, flags: i32) -> io::Result<Option<std::process::ExitStatus>> {
        use std::os::unix::process::ExitStatusExt;
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let mut status = 0;
        loop {
            match unsafe { libc::waitpid(self.pid, &mut status, flags) } {
                0 => return Ok(None),
                -1 => {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                _ => break,
            }
        }
        self.reaped = true;
        self.guardian.take();
        self.stop_supervisor();
        let status = std::process::ExitStatus::from_raw(status);
        self.status = Some(status);
        Ok(Some(status))
    }

    fn stop_supervisor(&mut self) {
        self.control.cancel();
        if let Some(worker) = self.supervisor.take() {
            let _ = worker.join();
        }
    }

    pub(super) fn id(&self) -> u32 {
        self.pid as u32
    }

    /// The child's process-GROUP id — `Some` only when it leads its own group, the sole state
    /// in which `-pid` names the child's tree and nothing else. See [`SupervisedChild`].
    pub(super) fn process_group_id(&self) -> Option<i32> {
        self.guardian
            .as_ref()
            .map(super::unix_guardian::UnixGuardian::process_group_id)
    }

    /// Reap the child and return its exit status. When it leads its own group, best-effort
    /// signals the group first — the supervised path has no PID namespace, so a signalled group
    /// is its only handle on a build tool the child backgrounded. Joins the supervisor thread
    /// after reaping (it has already returned once the target is gone).
    pub(super) fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        self.stdin.take();
        self.reap(0)?
            .ok_or_else(|| io::Error::other("blocking wait returned without a child status"))
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.kill();
            let _ = self.reap(0);
        }
        if self.pidfd >= 0 {
            unsafe { libc::close(self.pidfd) };
        }
        self.stop_supervisor();
    }
}

/// What a [`spawn_supervised`] child applies post-fork, pre-`execve`, IN ORDER. Every field is
/// prepared in the PARENT — allocation and lock acquisition are unsafe once forked.
pub(super) struct SupervisedLaunch<'a> {
    /// Fully-resolved argv. `argv[0]` MUST be an absolute program path: the child `execve`s
    /// (not `execvp`), because the environment is replaced by `envp` and a PATH search against
    /// the parent's `environ` would resolve against the wrong environment.
    pub argv: &'a [CString],
    /// The child's complete environment as `KEY=VALUE` entries; replaces the inherited env.
    pub envp: &'a [CString],
    /// `chdir` target, applied last before `execve`.
    pub cwd: Option<&'a std::ffi::CStr>,
    /// Landlock ruleset fd to `restrict_self` against, or `< 0` for no filesystem boundary.
    pub ruleset_fd: RawFd,
    /// The shared deny-ceiling filter (io_uring/keyctl/xattr/…), installed before the notifier.
    pub seccomp_ceiling: Option<&'a [seccompiler::sock_filter]>,
    pub stdin: SupervisedStdio,
    pub stdout: SupervisedStdio,
    pub stderr: SupervisedStdio,
    /// Internal callers normally pass an empty slice.  Tests use an explicit inherited socket to
    /// exercise post-exec notification handling; every other inherited descriptor stays CLOEXEC.
    pub inherited_fds: &'a [RawFd],
}

#[derive(Clone, Copy)]
pub(super) enum SupervisedStdio {
    Inherit,
    Null,
    Piped,
}

struct LaunchIo {
    child: Option<OwnedFd>,
    parent: Option<OwnedFd>,
}

impl LaunchIo {
    fn new(mode: SupervisedStdio, input: bool) -> io::Result<Self> {
        match mode {
            SupervisedStdio::Inherit => Ok(Self {
                child: None,
                parent: None,
            }),
            SupervisedStdio::Null => {
                let file = std::fs::OpenOptions::new()
                    .read(input)
                    .write(!input)
                    .open("/dev/null")?;
                Ok(Self {
                    child: Some(above_stdio(file.into())?),
                    parent: None,
                })
            }
            SupervisedStdio::Piped => {
                let (read, write) = pipe_owned()?;
                let (child, parent) = if input { (read, write) } else { (write, read) };
                Ok(Self {
                    child: Some(child),
                    parent: Some(parent),
                })
            }
        }
    }
}

fn pipe_owned() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    // The launching program may itself have closed a standard stream. Never let a pipe
    // occupy 0/1/2: dup2 of another stream would otherwise clobber a still-needed endpoint.
    Ok((above_stdio(read)?, above_stdio(write)?))
}

fn above_stdio(fd: OwnedFd) -> io::Result<OwnedFd> {
    if fd.as_raw_fd() >= 3 {
        return Ok(fd);
    }
    let moved = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if moved < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(moved) })
}

struct ForkGuard(libc::pid_t);

impl Drop for ForkGuard {
    fn drop(&mut self) {
        if self.0 <= 0 {
            return;
        }
        unsafe { libc::kill(self.0, libc::SIGKILL) };
        loop {
            if unsafe { libc::waitpid(self.0, std::ptr::null_mut(), 0) } >= 0
                || errno() != libc::EINTR
            {
                break;
            }
        }
    }
}

/// Fork `launch.argv` as a fully-confined child under the connect-notifier, start its supervisor
/// thread, and return a handle the caller reaps. The supervisor and pidfd handoff retain the
/// full child confinement a library launch needs
/// (guardian membership, `PDEATHSIG`, cloexec sweep, `no_new_privs`, capability drop, Landlock, the seccomp
/// ceiling) and returning rather than blocking on `waitpid`.
#[cfg(test)]
pub(super) fn spawn_supervised(
    policy: EgressPolicy,
    launch: SupervisedLaunch,
) -> io::Result<SupervisedChild> {
    spawn_supervised_with_ready(policy, launch, |_| Ok(()))
}

pub(super) fn spawn_supervised_with_ready(
    policy: EgressPolicy,
    launch: SupervisedLaunch,
    ready: impl FnOnce(i32) -> io::Result<()>,
) -> io::Result<SupervisedChild> {
    // Built in the PARENT and copied into the child by `fork`; the child installs it without
    // allocating. The write-intent dispatch is present only when the policy carries carve-outs.
    let filter = notifier_program(
        policy.write_broker(),
        policy.read_broker(),
        !policy.self_proc.is_empty(),
    );
    let mut state = SupState::new(policy);
    let control = Arc::new(WorkerControl::new()?);

    if launch.argv.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty supervised argv",
        ));
    }
    let guardian = super::unix_guardian::UnixGuardian::start()?;
    // The confined tree, handed to the supervisor so `handle_signal_intent` can scope to it. Read
    // after `start`, which is where the private group becomes real.
    state.tree_pgid = Some(guardian.process_group_id());
    let lifetime_filter = super::linux_lifetime::program(true)?;
    let (c2p_read, c2p_write) = pipe_owned()?;
    let (p2c_read, p2c_write) = pipe_owned()?;
    let c2p = [c2p_read.as_raw_fd(), c2p_write.as_raw_fd()];
    let p2c = [p2c_read.as_raw_fd(), p2c_write.as_raw_fd()];
    let mut stdin = LaunchIo::new(launch.stdin, true)?;
    let mut stdout = LaunchIo::new(launch.stdout, false)?;
    let mut stderr = LaunchIo::new(launch.stderr, false)?;

    let argv_ptrs: Vec<*const libc::c_char> = launch
        .argv
        .iter()
        .map(|a| a.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let envp_ptrs: Vec<*const libc::c_char> = launch
        .envp
        .iter()
        .map(|e| e.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let cwd_ptr = launch.cwd.map(|c| c.as_ptr());

    let owner_pid = unsafe { libc::getpid() };
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // ---- child ---- async-signal-safe only: raw syscalls, no allocation, no locks.
        unsafe {
            libc::close(c2p[0]);
            libc::close(p2c[1]);
            if guardian.child_join() != 0 {
                libc::_exit(10);
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                libc::_exit(11);
            }
            if libc::getppid() != owner_pid {
                libc::_exit(11);
            }
            for (stream, number) in [(&stdin, 0), (&stdout, 1), (&stderr, 2)] {
                if let Some(fd) = &stream.child
                    && libc::dup2(fd.as_raw_fd(), number) < 0
                {
                    libc::_exit(12);
                }
            }
            // FIRST, before any restriction: the sweep opens `/proc/self/fd`, which Landlock
            // below would make unreadable. It marks c2p[1]/p2c[0]/ruleset CLOEXEC too, which is
            // harmless — each stays usable until the explicit close before `execve`.
            if mark_inherited_fds_cloexec().is_err() {
                libc::_exit(12);
            }
            for fd in launch.inherited_fds {
                let flags = libc::fcntl(*fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    libc::_exit(12);
                }
            }
            // Gates Landlock and seccomp, both of which refuse a caller that could still gain
            // privileges through a setuid `execve`.
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                libc::_exit(13);
            }
            if super::linux_lifetime::install(&lifetime_filter).is_err() {
                libc::_exit(13);
            }
            if super::linux_landlock::drop_all_capabilities().is_err() {
                libc::_exit(14);
            }
            if launch.ruleset_fd >= 0
                && super::linux_landlock::restrict_self(launch.ruleset_fd).is_err()
            {
                libc::_exit(15);
            }
            if let Some(filter) = launch.seccomp_ceiling
                && install_target_seccomp(filter).is_err()
            {
                libc::_exit(16);
            }
            // The USER_NOTIF listener, LAST so the ceiling above never traps to this supervisor.
            // `nfd` is handed to the parent then CLOSED before `execve`, so it never leaks into
            // the target (which could otherwise service its own notifications with CONTINUE).
            let nfd = install_notifier(&filter);
            if nfd < 0 {
                libc::_exit(17);
            }
            let nfd_bytes = nfd.to_ne_bytes();
            if libc::write(c2p[1], nfd_bytes.as_ptr() as *const libc::c_void, 4) != 4 {
                libc::_exit(18);
            }
            let mut go = [0u8; 1];
            if libc::read(p2c[0], go.as_mut_ptr() as *mut libc::c_void, 1) != 1 {
                libc::_exit(19);
            }
            libc::close(nfd);
            libc::close(c2p[1]);
            libc::close(p2c[0]);
            if let Some(cwd) = cwd_ptr
                && libc::chdir(cwd) != 0
            {
                libc::_exit(20);
            }
            libc::execve(argv_ptrs[0], argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
            libc::_exit(127);
        }
    }

    // ---- parent ----
    use std::io::{Read, Write};
    let mut pending = ForkGuard(pid);
    drop(c2p_write);
    drop(p2c_read);
    stdin.child.take();
    stdout.child.take();
    stderr.child.take();
    let mut child_nfd_bytes = [0u8; 4];
    std::fs::File::from(c2p_read).read_exact(&mut child_nfd_bytes)?;
    let child_nfd = i32::from_ne_bytes(child_nfd_bytes);
    let pf = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as RawFd;
    let nfd = if pf >= 0 {
        unsafe { libc::syscall(libc::SYS_pidfd_getfd, pf, child_nfd, 0) as RawFd }
    } else {
        -1
    };
    if nfd < 0 {
        let e = io::Error::last_os_error();
        if pf >= 0 {
            unsafe { libc::close(pf) };
        }
        return Err(e);
    }
    // The thread owns the listener and all duplicated DNS sockets. Cancellation is separate
    // from listener hangup so shutdown also works when a descendant changed process groups.
    let listener = unsafe { OwnedFd::from_raw_fd(nfd) };
    let worker_control = Arc::clone(&control);
    let pidfd = unsafe { OwnedFd::from_raw_fd(pf) };
    if !state.self_proc.is_empty() {
        self_proc::check_atomic_addfd(nfd)?;
    }
    let sup_thread = std::thread::Builder::new()
        .name("sandbox-egress".into())
        .spawn(move || supervisor(listener, state, worker_control))?;
    use std::os::fd::IntoRawFd;
    let child = SupervisedChild {
        pid,
        pidfd: pidfd.into_raw_fd(),
        supervisor: Some(sup_thread),
        control,
        reaped: false,
        guardian: Some(guardian),
        status: None,
        stdin: stdin.parent.take().map(std::process::ChildStdin::from),
        stdout: stdout.parent.take().map(std::process::ChildStdout::from),
        stderr: stderr.parent.take().map(std::process::ChildStderr::from),
    };
    pending.0 = 0; // the returned handle now owns kill/reap, including a failed barrier write
    ready(
        child
            .process_group_id()
            .ok_or_else(|| io::Error::other("supervised command has no guardian"))?,
    )?;
    std::fs::File::from(p2c_write).write_all(b"g")?;
    Ok(child)
}

// ---------------------------------------------------------------------------
// Child-side confinement helpers the Landlock path calls (ported from the dropped
// `linux_monitor` module). Signatures match the call sites in `linux_landlock.rs`.
// ---------------------------------------------------------------------------

/// Mark every inherited descriptor (>= 3) close-on-exec so a confined child cannot use an
/// fd nub already holds open — a descriptor egresses BELOW both Landlock (which governs
/// `open`, not an open fd) and seccomp (which never sees a syscall for it). Marked rather
/// than closed so the child's exec-error report pipe still works; `execve` then closes the
/// whole marked range atomically. Prefers `close_range(CLOSE_RANGE_CLOEXEC)`, falling back
/// to a `/proc/self/fd` sweep.
pub(super) fn mark_inherited_fds_cloexec() -> io::Result<()> {
    const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
    let result =
        unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, CLOSE_RANGE_CLOEXEC) };
    if result >= 0 {
        return Ok(());
    }
    unsafe { mark_open_fds_cloexec_from_proc() }
}

unsafe fn mark_open_fds_cloexec_from_proc() -> io::Result<()> {
    const PROC_SUPER_MAGIC: libc::c_long = 0x9fa0;
    const DIRENT_HEADER: usize = 19;
    let directory = unsafe {
        libc::syscall(
            libc::SYS_openat,
            libc::AT_FDCWD,
            c"/proc/self/fd".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0,
        ) as RawFd
    };
    if directory < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut stat = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(directory, stat.as_mut_ptr()) } != 0
        || unsafe { stat.assume_init() }.f_type != PROC_SUPER_MAGIC
    {
        unsafe { libc::close(directory) };
        return Err(io::Error::other(
            "/proc/self/fd is not a procfs descriptor directory",
        ));
    }
    let mut buffer = [0u8; 8192];
    let mut saw_directory = false;
    loop {
        let count = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                directory,
                buffer.as_mut_ptr(),
                buffer.len(),
            ) as isize
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            unsafe { libc::close(directory) };
            return Err(error);
        }
        if count == 0 {
            break;
        }
        let count = count as usize;
        let mut offset = 0usize;
        while offset < count {
            if count - offset < DIRENT_HEADER {
                unsafe { libc::close(directory) };
                return Err(io::Error::other(
                    "procfs descriptor enumeration returned a truncated record",
                ));
            }
            let record = &buffer[offset..count];
            let reclen = u16::from_ne_bytes([record[16], record[17]]) as usize;
            if reclen < DIRENT_HEADER || offset + reclen > count {
                unsafe { libc::close(directory) };
                return Err(io::Error::other(
                    "procfs descriptor enumeration returned a malformed record",
                ));
            }
            let name = &record[DIRENT_HEADER..reclen];
            let Some(end) = name.iter().position(|byte| *byte == 0) else {
                unsafe { libc::close(directory) };
                return Err(io::Error::other(
                    "procfs descriptor enumeration returned an unterminated name",
                ));
            };
            let name = &name[..end];
            if name != b"." && name != b".." {
                if name.is_empty() || !name.iter().all(u8::is_ascii_digit) {
                    unsafe { libc::close(directory) };
                    return Err(io::Error::other(
                        "procfs descriptor enumeration returned a nonnumeric name",
                    ));
                }
                let mut fd: RawFd = 0;
                for byte in name {
                    fd = fd
                        .checked_mul(10)
                        .and_then(|value| value.checked_add(i32::from(*byte - b'0')))
                        .ok_or_else(|| {
                            io::Error::other(
                                "procfs descriptor enumeration overflowed a descriptor number",
                            )
                        })?;
                }
                if fd == directory {
                    saw_directory = true;
                } else if fd >= 3 {
                    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                    if flags < 0
                        || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
                    {
                        let error = io::Error::last_os_error();
                        unsafe { libc::close(directory) };
                        return Err(error);
                    }
                }
            }
            offset += reclen;
        }
    }
    unsafe { libc::close(directory) };
    if !saw_directory {
        return Err(io::Error::other(
            "procfs descriptor enumeration omitted its own descriptor",
        ));
    }
    Ok(())
}

/// Install `program` as the target's seccomp filter via `PR_SET_SECCOMP`. Runs after
/// `PR_SET_NO_NEW_PRIVS`; returns the child errno on failure so the caller can map it into
/// an `io::Error`.
pub(super) fn install_target_seccomp(
    program: &[seccompiler::sock_filter],
) -> Result<(), libc::c_int> {
    let len = u16::try_from(program.len()).map_err(|_| libc::E2BIG)?;
    let filter = libc::sock_fprog {
        len,
        filter: program.as_ptr().cast::<libc::sock_filter>().cast_mut(),
    };
    if unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &filter as *const libc::sock_fprog,
            0,
            0,
        )
    } != 0
    {
        return Err(unsafe { *libc::__errno_location() });
    }
    Ok(())
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::time::Duration;

    fn policy(host: &str) -> EgressPolicy {
        EgressPolicy {
            self_proc: BTreeSet::new(),
            allow_all: false,
            allow: vec![host.into()],
            fs_policy: None,
            proxy_port: None,
            proxy_token: None,
        }
    }

    /// This test owns the loopback listener and uses it as the one exact proxy endpoint the
    /// production supervisor permits directly. It does not restore a general loopback grant.
    fn loopback_proxy_endpoint_fixture_policy(port: u16) -> EgressPolicy {
        EgressPolicy {
            proxy_port: Some(port),
            proxy_token: Some("fixture-only".to_string()),
            ..policy("not-permitted.example")
        }
    }

    #[test]
    fn only_the_exact_v4_proxy_listener_skips_proxy_routing() {
        assert!(is_egress_proxy_endpoint(
            libc::AF_INET,
            &[127, 0, 0, 1],
            1234,
            1234
        ));
        for (family, address, port, proxy_port) in [
            (libc::AF_INET, vec![127, 0, 0, 2], 1234, 1234),
            (libc::AF_INET, vec![127, 0, 0, 1], 1235, 1234),
            (
                libc::AF_INET6,
                vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
                1234,
                1234,
            ),
        ] {
            assert!(
                !is_egress_proxy_endpoint(family, &address, port, proxy_port),
                "{address:?}:{port} must remain proxy-routed"
            );
        }
    }

    #[test]
    fn proxy_listener_direct_dial_needs_no_observed_dns_name() {
        assert!(direct_dial_allowed(true, false));
        assert!(direct_dial_allowed(false, true));
        assert!(!direct_dial_allowed(false, false));
    }

    #[test]
    fn policies_and_observed_dns_do_not_cross_launches() {
        let mut first = SupState::new(policy("first.example"));
        first.record("first.example", libc::AF_INET, &[192, 0, 2, 1]);
        let second = SupState::new(policy("second.example"));
        assert!(first.allowed(first.lookup(libc::AF_INET, &[192, 0, 2, 1]).as_deref()));
        assert!(!second.allowed(Some("first.example")));
        assert_eq!(second.lookup(libc::AF_INET, &[192, 0, 2, 1]), None);
    }

    #[test]
    fn replayed_send_always_suppresses_host_sigpipe() {
        for flags in [0, libc::MSG_MORE] {
            assert_ne!(
                replay_send_flags(flags, false).unwrap() & libc::MSG_NOSIGNAL,
                0,
                "blocking replay must not signal the Nub host"
            );
            assert_ne!(
                replay_send_flags(flags, true).unwrap() & libc::MSG_NOSIGNAL,
                0,
                "nonblocking replay must not signal the Nub host"
            );
        }
    }

    #[test]
    fn replayed_send_rejects_asynchronous_buffer_ownership() {
        for nonblocking in [false, true] {
            for flags in [libc::MSG_ZEROCOPY, libc::MSG_ZEROCOPY | libc::MSG_MORE] {
                assert_eq!(replay_send_flags(flags, nonblocking), Err(libc::EOPNOTSUPP));
            }
        }
    }

    #[test]
    fn ancillary_rights_are_rejected_without_any_fd_duplication() {
        // The payload is intentionally not readable: validation must refuse control data before
        // inspecting or duplicating even a 253/254-descriptor SCM_RIGHTS array.
        let hdr = libc::msghdr {
            msg_name: std::ptr::null_mut(),
            msg_namelen: 0,
            msg_iov: std::ptr::null_mut(),
            msg_iovlen: 0,
            msg_control: std::ptr::NonNull::<libc::c_void>::dangling().as_ptr(),
            msg_controllen: 64 * 1024,
            msg_flags: 0,
        };
        let mem = open_child_mem(std::process::id()).unwrap();
        assert_eq!(
            snapshot_msghdr(mem.as_raw_fd(), hdr).unwrap_err(),
            libc::EPERM
        );
    }

    #[test]
    fn mixed_address_sendmmsg_kernel_control_reaches_each_listener() {
        // First establish the kernel control on owned loopback UDP listeners: on a connected
        // socket, a normal batch can send its first element to the connected peer and a later
        // addressed element to a distinct recipient, recording each exact payload.
        let first = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let second = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        first
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        second
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let second_addr = make_sockaddr_in(
            u32::from_ne_bytes([127, 0, 0, 1]),
            second.local_addr().unwrap().port(),
        );
        sender.connect(first.local_addr().unwrap()).unwrap();
        let first_bytes = b"first-synthetic-datagram";
        let second_bytes = b"second-synthetic-datagram";
        let mut iovecs = [
            libc::iovec {
                iov_base: first_bytes.as_ptr() as *mut libc::c_void,
                iov_len: first_bytes.len(),
            },
            libc::iovec {
                iov_base: second_bytes.as_ptr() as *mut libc::c_void,
                iov_len: second_bytes.len(),
            },
        ];
        let mut control: [libc::mmsghdr; 2] = unsafe { std::mem::zeroed() };
        control[0].msg_hdr = libc::msghdr {
            msg_name: std::ptr::null_mut(),
            msg_namelen: 0,
            msg_iov: &mut iovecs[0],
            msg_iovlen: 1,
            msg_control: std::ptr::null_mut(),
            msg_controllen: 0,
            msg_flags: 0,
        };
        control[1].msg_hdr = libc::msghdr {
            msg_name: &second_addr as *const _ as *mut libc::c_void,
            msg_namelen: size_of::<libc::sockaddr_in>() as libc::socklen_t,
            msg_iov: &mut iovecs[1],
            msg_iovlen: 1,
            msg_control: std::ptr::null_mut(),
            msg_controllen: 0,
            msg_flags: 0,
        };
        assert_eq!(
            unsafe { libc::sendmmsg(sender.as_raw_fd(), control.as_mut_ptr(), 2, 0) },
            2
        );
        let mut received = [0u8; 64];
        let (len, _) = first.recv_from(&mut received).unwrap();
        assert_eq!(&received[..len], first_bytes);
        let (len, _) = second.recv_from(&mut received).unwrap();
        assert_eq!(&received[..len], second_bytes);
        assert_eq!(control[0].msg_len, first_bytes.len() as u32);
        assert_eq!(control[1].msg_len, second_bytes.len() as u32);

        // The supervised regression below instead returns ENOSYS for this entire ABI, avoiding
        // the unsafe target-memory `msg_len` writes that a transparent replay would require.
    }

    /// The child side of [`native_supervisor_replays_or_rejects_every_mmsg`].  Keeping this in
    /// the test binary makes the regression run after an actual `execve` with the real notifier,
    /// rather than only exercising the snapshot helper in the unconfined test process.
    #[test]
    fn supervised_sendmmsg_child() {
        let Ok(mode) = std::env::var("NUB_SUP_SENDMMSG_MODE") else {
            return;
        };
        let fd = std::env::var("NUB_SUP_SENDMMSG_FD")
            .ok()
            .and_then(|value| value.parse::<RawFd>().ok())
            .unwrap_or(-1);
        let second_port = std::env::var("NUB_SUP_SENDMMSG_SECOND_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(0);
        if mode == "cancel" {
            unsafe extern "C" fn signal_noop(_: libc::c_int) {}
            let sync_fd = std::env::var("NUB_SUP_SENDMMSG_SYNC_FD")
                .ok()
                .and_then(|value| value.parse::<RawFd>().ok())
                .unwrap_or(-1);
            let address = make_sockaddr_in(u32::from_ne_bytes([127, 0, 0, 1]), second_port);
            let fd =
                unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
            if fd < 0
                || unsafe {
                    libc::connect(
                        fd,
                        &address as *const _ as *const libc::sockaddr,
                        size_of::<libc::sockaddr_in>() as libc::socklen_t,
                    )
                } < 0
            {
                std::process::exit(94);
            }
            // The parent shrinks the accepted peer's receive window before releasing this
            // child.  That makes the following EAGAIN and cancelled replay deterministic
            // instead of relying on localhost's default autotuned receive buffer.
            if sync_fd < 0
                || unsafe { libc::write(sync_fd, b"R".as_ptr() as *const libc::c_void, 1) } != 1
            {
                std::process::exit(90);
            }
            let mut release = 0u8;
            if unsafe { libc::read(sync_fd, &mut release as *mut u8 as *mut libc::c_void, 1) } != 1
            {
                std::process::exit(89);
            }
            let small = 4096i32;
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    &small as *const _ as *const libc::c_void,
                    size_of::<i32>() as libc::socklen_t,
                );
            }
            let original = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            unsafe { libc::fcntl(fd, libc::F_SETFL, original | libc::O_NONBLOCK) };
            let fill = [0u8; 4096];
            while unsafe { libc::send(fd, fill.as_ptr() as *const libc::c_void, fill.len(), 0) }
                >= 0
            {}
            if errno() != libc::EAGAIN && errno() != libc::EWOULDBLOCK {
                std::process::exit(95);
            }
            unsafe { libc::fcntl(fd, libc::F_SETFL, original) };
            let action = libc::sigaction {
                sa_sigaction: signal_noop as *const () as usize,
                sa_mask: unsafe { std::mem::zeroed() },
                sa_flags: 0,
                sa_restorer: None,
            };
            unsafe { libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut()) };
            let tid = unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t };
            let pid = unsafe { libc::getpid() };
            let wake = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                unsafe { libc::syscall(libc::SYS_tgkill, pid, tid, libc::SIGUSR1) };
            });
            let bytes = b"must-not-send-after-cancel";
            let mut iov = libc::iovec {
                iov_base: bytes.as_ptr() as *mut libc::c_void,
                iov_len: bytes.len(),
            };
            let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
            hdr.msg_iov = &mut iov;
            hdr.msg_iovlen = 1;
            let result = unsafe { libc::sendmsg(fd, &hdr, 0) };
            let error = errno();
            wake.join().unwrap();
            unsafe { libc::close(fd) };
            // Keep the failure classes distinct in the parent assertion: the integration test
            // needs to distinguish an unexpected successful replay from a kernel error that did
            // not reflect the signal cancellation.
            std::process::exit(if result == -1 && error == libc::EINTR {
                0
            } else if result >= 0 {
                96
            } else if error == libc::EAGAIN || error == libc::EWOULDBLOCK {
                97
            } else if error == libc::ECANCELED {
                98
            } else {
                99
            });
        }
        if mode == "tcp" || mode == "dns" {
            let address = if mode == "tcp" {
                make_sockaddr_in(u32::from_ne_bytes([127, 0, 0, 1]), second_port)
            } else {
                make_sockaddr_in(u32::from_ne_bytes([8, 8, 8, 8]), 53)
            };
            let fd = unsafe {
                libc::socket(
                    libc::AF_INET,
                    if mode == "tcp" {
                        libc::SOCK_STREAM
                    } else {
                        libc::SOCK_DGRAM
                    } | libc::SOCK_CLOEXEC,
                    0,
                )
            };
            if fd < 0
                || unsafe {
                    libc::connect(
                        fd,
                        &address as *const _ as *const libc::sockaddr,
                        size_of::<libc::sockaddr_in>() as libc::socklen_t,
                    )
                } < 0
            {
                std::process::exit(92);
            }
            let query = if mode == "dns" {
                // Synthetic DNS A query for `example.com`; the assertion is only that the
                // resolver-bound connected datagram accepts a normal sendmsg.
                b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x07example\x03com\x00\x00\x01\x00\x01".as_slice()
            } else {
                b"supervised-tcp".as_slice()
            };
            let mut iov = libc::iovec {
                iov_base: query.as_ptr() as *mut libc::c_void,
                iov_len: query.len(),
            };
            let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
            hdr.msg_iov = &mut iov;
            hdr.msg_iovlen = 1;
            if mode == "tcp" {
                let enabled = 1i32;
                let configured = unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_ZEROCOPY,
                        &enabled as *const _ as *const libc::c_void,
                        size_of::<i32>() as libc::socklen_t,
                    )
                };
                if configured != 0 {
                    std::process::exit(87);
                }
                let sent = unsafe { libc::sendmsg(fd, &hdr, libc::MSG_ZEROCOPY) };
                if sent != -1 || errno() != libc::EOPNOTSUPP {
                    std::process::exit(88);
                }
            }
            let sent = unsafe { libc::sendmsg(fd, &hdr, 0) };
            unsafe { libc::close(fd) };
            std::process::exit(if sent == query.len() as isize { 0 } else { 93 });
        }
        let bytes = b"supervised-udp";
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr() as *mut libc::c_void,
            iov_len: bytes.len(),
        };
        let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
        hdr.msg_iov = &mut iov;
        hdr.msg_iovlen = 1;
        let correct = if mode == "batch" {
            let mut message: libc::mmsghdr = unsafe { std::mem::zeroed() };
            message.msg_hdr = hdr;
            (unsafe { libc::sendmmsg(fd, &mut message, 1, 0) }) == -1 && errno() == libc::ENOSYS
        } else {
            (unsafe { libc::sendmsg(fd, &hdr, 0) }) == bytes.len() as isize
        };
        std::process::exit(if correct { 0 } else { 91 });
    }

    #[test]
    #[ignore = "requires a Linux runner permitting unprivileged seccomp notification and pidfd_getfd"]
    fn native_supervisor_replays_or_rejects_every_mmsg() {
        fn launch(mode: &str) -> (std::net::UdpSocket, std::net::UdpSocket, SupervisedChild) {
            let first = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let second = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            first
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            second
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            sender.connect(first.local_addr().unwrap()).unwrap();
            // This checks the replay's nonblocking branch on a real post-exec child; ordinary
            // sends remain immediately writable while the parent owns the receiver.
            sender.set_nonblocking(true).unwrap();
            let exe = std::env::current_exe().unwrap();
            let argv = [
                CString::new(exe.as_os_str().as_encoded_bytes()).unwrap(),
                CString::new("supervised_sendmmsg_child").unwrap(),
            ];
            let envp = [
                CString::new(format!("NUB_SUP_SENDMMSG_MODE={mode}")).unwrap(),
                CString::new(format!("NUB_SUP_SENDMMSG_FD={}", sender.as_raw_fd())).unwrap(),
                CString::new(format!(
                    "NUB_SUP_SENDMMSG_SECOND_PORT={}",
                    second.local_addr().unwrap().port()
                ))
                .unwrap(),
            ];
            let inherited = [sender.as_raw_fd()];
            let child = spawn_supervised(
                policy("not-permitted.example"),
                SupervisedLaunch {
                    argv: &argv,
                    envp: &envp,
                    cwd: None,
                    ruleset_fd: -1,
                    seccomp_ceiling: None,
                    stdin: SupervisedStdio::Null,
                    stdout: SupervisedStdio::Null,
                    stderr: SupervisedStdio::Null,
                    inherited_fds: &inherited,
                },
            )
            .unwrap();
            (first, second, child)
        }

        let (first, second, mut child) = launch("udp");
        assert_eq!(child.wait().unwrap().code(), Some(0));
        let mut bytes = [0u8; 64];
        let (len, _) = first.recv_from(&mut bytes).unwrap();
        assert_eq!(&bytes[..len], b"supervised-udp");
        assert_eq!(
            second.recv_from(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );

        // sendmmsg falls back explicitly, without reading or writing its target mmsghdr array.
        let (first, second, mut child) = launch("batch");
        assert_eq!(child.wait().unwrap().code(), Some(0));
        assert_eq!(
            first.recv_from(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            second.recv_from(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );

        // Fresh child-created sockets exercise the regular connected routes too. A raw loopback
        // dial remains denied; the owned listener becomes the exact fixture proxy endpoint only
        // for the positive stream/cancellation cases. The port-53 UDP child retains the normal
        // deny policy and must still use the resolver-bound path.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let exe = std::env::current_exe().unwrap();
        let argv = [
            CString::new(exe.as_os_str().as_encoded_bytes()).unwrap(),
            CString::new("supervised_sendmmsg_child").unwrap(),
        ];
        let tcp_env = [
            CString::new("NUB_SUP_SENDMMSG_MODE=tcp").unwrap(),
            CString::new("NUB_SUP_SENDMMSG_FD=-1").unwrap(),
            CString::new(format!(
                "NUB_SUP_SENDMMSG_SECOND_PORT={}",
                listener.local_addr().unwrap().port()
            ))
            .unwrap(),
        ];
        let mut denied_child = spawn_supervised(
            policy("not-permitted.example"),
            SupervisedLaunch {
                argv: &argv,
                envp: &tcp_env,
                cwd: None,
                ruleset_fd: -1,
                seccomp_ceiling: None,
                stdin: SupervisedStdio::Null,
                stdout: SupervisedStdio::Null,
                stderr: SupervisedStdio::Null,
                inherited_fds: &[],
            },
        )
        .unwrap();
        assert_eq!(denied_child.wait().unwrap().code(), Some(92));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "the denied raw loopback dial must not reach the owned listener"
        );

        let mut child = spawn_supervised(
            loopback_proxy_endpoint_fixture_policy(listener.local_addr().unwrap().port()),
            SupervisedLaunch {
                argv: &argv,
                envp: &tcp_env,
                cwd: None,
                ruleset_fd: -1,
                seccomp_ceiling: None,
                stdin: SupervisedStdio::Null,
                stdout: SupervisedStdio::Null,
                stderr: SupervisedStdio::Null,
                inherited_fds: &[],
            },
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(value) => break value,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "TCP child did not connect"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("TCP listener failed: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut tcp = [0u8; 32];
        let len = stream.read(&mut tcp).unwrap();
        assert_eq!(&tcp[..len], b"supervised-tcp");
        assert_eq!(child.wait().unwrap().code(), Some(0));

        // Fill a fresh stream while this listener deliberately does not read it, then interrupt
        // the target's blocked sendmsg. The child must leave with EINTR within the finite
        // revalidation cadence; a stale supervisor must neither answer nor wait for writability.
        let receive = 1024i32;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    listener.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    &receive as *const _ as *const libc::c_void,
                    size_of::<i32>() as libc::socklen_t,
                )
            },
            0
        );
        let (mut sync_parent, sync_child) = std::os::unix::net::UnixStream::pair().unwrap();
        let inherited = [sync_child.as_raw_fd()];
        let cancel_env = [
            CString::new("NUB_SUP_SENDMMSG_MODE=cancel").unwrap(),
            CString::new("NUB_SUP_SENDMMSG_FD=-1").unwrap(),
            CString::new(format!(
                "NUB_SUP_SENDMMSG_SECOND_PORT={}",
                listener.local_addr().unwrap().port()
            ))
            .unwrap(),
            CString::new(format!(
                "NUB_SUP_SENDMMSG_SYNC_FD={}",
                sync_child.as_raw_fd()
            ))
            .unwrap(),
        ];
        let mut child = spawn_supervised(
            loopback_proxy_endpoint_fixture_policy(listener.local_addr().unwrap().port()),
            SupervisedLaunch {
                argv: &argv,
                envp: &cancel_env,
                cwd: None,
                ruleset_fd: -1,
                seccomp_ceiling: None,
                stdin: SupervisedStdio::Null,
                stdout: SupervisedStdio::Null,
                stderr: SupervisedStdio::Null,
                inherited_fds: &inherited,
            },
        )
        .unwrap();
        drop(sync_child);
        let mut ready = [0u8; 1];
        sync_parent.read_exact(&mut ready).unwrap();
        assert_eq!(ready, *b"R");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let (blocked_peer, _) = loop {
            match listener.accept() {
                Ok(value) => break value,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "cancel child did not connect"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("cancel listener failed: {error}"),
            }
        };
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    blocked_peer.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    &receive as *const _ as *const libc::c_void,
                    size_of::<i32>() as libc::socklen_t,
                )
            },
            0
        );
        sync_parent.write_all(b"G").unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(0));

        let dns_env = [
            CString::new("NUB_SUP_SENDMMSG_MODE=dns").unwrap(),
            CString::new("NUB_SUP_SENDMMSG_FD=-1").unwrap(),
            CString::new("NUB_SUP_SENDMMSG_SECOND_PORT=0").unwrap(),
        ];
        let mut child = spawn_supervised(
            policy("not-permitted.example"),
            SupervisedLaunch {
                argv: &argv,
                envp: &dns_env,
                cwd: None,
                ruleset_fd: -1,
                seccomp_ceiling: None,
                stdin: SupervisedStdio::Null,
                stdout: SupervisedStdio::Null,
                stderr: SupervisedStdio::Null,
                inherited_fds: &[],
            },
        )
        .unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(0));
    }

    #[test]
    fn cancellation_wakes_an_idle_worker_and_stays_cancelled() {
        let (read, _write) = pipe_owned().unwrap();
        let control = Arc::new(WorkerControl::new().unwrap());
        let worker_control = Arc::clone(&control);
        let worker =
            std::thread::spawn(move || worker_control.wait(read.as_raw_fd(), libc::POLLIN, None));
        control.cancel();
        assert!(control.cancelled());
        assert!(!worker.join().unwrap().unwrap());
        let (read, _write) = pipe_owned().unwrap();
        assert!(
            !control
                .wait(read.as_raw_fd(), libc::POLLIN, Some(Duration::ZERO))
                .unwrap()
        );
    }

    #[test]
    fn worker_observes_readiness_timeout_and_hangup() {
        let control = WorkerControl::new().unwrap();
        let (read, write) = pipe_owned().unwrap();
        let mut writer = std::fs::File::from(write);
        assert!(
            !control
                .wait(read.as_raw_fd(), libc::POLLIN, Some(Duration::ZERO))
                .unwrap()
        );
        writer.write_all(b"x").unwrap();
        assert!(
            control
                .wait(read.as_raw_fd(), libc::POLLIN, Some(Duration::ZERO))
                .unwrap()
        );
        std::fs::File::from(read).read_exact(&mut [0u8]).unwrap();
        let (read, write) = pipe_owned().unwrap();
        drop(write);
        assert!(!control.wait(read.as_raw_fd(), libc::POLLIN, None).unwrap());
    }

    #[test]
    fn launch_pipe_descriptors_are_private_and_close_on_exec() {
        let (read, write) = pipe_owned().unwrap();
        for fd in [read, write] {
            assert!(fd.as_raw_fd() >= 3);
            assert_ne!(
                unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
    }

    #[test]
    #[ignore = "requires a Linux runner permitting unprivileged seccomp notification and pidfd_getfd"]
    fn native_supervised_streams_status_and_repeated_launches() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("must-not-execute");
        let argv = [
            CString::new("/usr/bin/touch").unwrap(),
            CString::new(marker.as_os_str().as_encoded_bytes()).unwrap(),
        ];
        let launch = SupervisedLaunch {
            argv: &argv,
            envp: &[],
            cwd: None,
            ruleset_fd: -1,
            seccomp_ceiling: None,
            stdin: SupervisedStdio::Null,
            stdout: SupervisedStdio::Null,
            stderr: SupervisedStdio::Null,
            inherited_fds: &[],
        };
        let result = spawn_supervised_with_ready(policy("example.test"), launch, |_| {
            Err(io::Error::other("ready callback rejected launch"))
        });
        assert!(result.is_err());
        assert!(
            !marker.exists(),
            "ready failure must precede workload execution"
        );
        for _ in 0..24 {
            let argv = [
                CString::new("/bin/sh").unwrap(),
                CString::new("-c").unwrap(),
                CString::new("read line; printf 'out:%s' \"$line\"; printf err >&2; exit 7")
                    .unwrap(),
            ];
            let launch = SupervisedLaunch {
                argv: &argv,
                envp: &[],
                cwd: None,
                ruleset_fd: -1,
                seccomp_ceiling: None,
                stdin: SupervisedStdio::Piped,
                stdout: SupervisedStdio::Piped,
                stderr: SupervisedStdio::Piped,
                inherited_fds: &[],
            };
            let mut child = spawn_supervised(policy("example.test"), launch).unwrap();
            child.take_stdin().unwrap().write_all(b"hello\n").unwrap();
            let mut stdout = String::new();
            let mut stderr = String::new();
            child
                .take_stdout()
                .unwrap()
                .read_to_string(&mut stdout)
                .unwrap();
            child
                .take_stderr()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            assert_eq!(child.wait().unwrap().code(), Some(7));
            assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(7));
            assert_eq!(stdout, "out:hello");
            assert_eq!(stderr, "err");
        }
    }
}

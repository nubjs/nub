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
//! splicing in a supervisor-owned socket connected to an in-process stub resolver, which
//! forwards to the real resolver and records `name -> A/AAAA`. A later TCP `connect` to one
//! of those addresses is attributed to that name and checked against the allowlist.
//!
//! THE LISTENER FD IS HANDED OVER A PLAIN PIPE, NEVER `SCM_RIGHTS`. Once `sendmsg` is
//! filtered, an `SCM_RIGHTS` handoff deadlocks against the very supervisor that would service
//! it. The child instead `write`s the listener's fd NUMBER down an ordinary pipe (`write` is
//! unfiltered) and the parent recovers the descriptor with `pidfd_open` + `pidfd_getfd`.
//!
//! STATUS: this module is self-contained (it depends only on `libc`, `seccompiler`, and
//! `std`) and is NOT yet wired into the launch path — that integration, including how the
//! supervisor thread relates to the confined child in a library context, is owned by the
//! sandbox launch code. It also provides the two child-side confinement helpers the Landlock
//! path calls (`mark_inherited_fds_cloexec`, `install_target_seccomp`), ported from the
//! dropped `linux_monitor` module.
#![allow(dead_code)]

use crate::matcher::path::PathMatcher;
use crate::policy::{Effect, FsAccess, FsRuleSet};
use std::ffi::CString;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::RawFd;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

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

/// The write-intent syscalls the broker mediates. Notified only when `write_broker` is set
/// (a policy carries deny/protect carve-outs), so the build jail's write-heavy workload —
/// which has none — pays nothing. `openat` is notified conditionally on its flags word; the
/// rest are unconditional. Legacy non-`*at` entry points (`open`/`rename`/…) are not present
/// on aarch64 and are folded into these on x86_64 by glibc, so the `*at` set is the portable
/// floor; a production sweep of the remaining x86_64 legacy numbers is tracked in 5.2.
const WRITE_INTENT_NRS: &[libc::c_long] = &[
    libc::SYS_openat,
    libc::SYS_openat2,
    libc::SYS_mkdirat,
    libc::SYS_unlinkat,
    libc::SYS_symlinkat,
    libc::SYS_linkat,
    libc::SYS_renameat,
    libc::SYS_renameat2,
    libc::SYS_truncate,
];

/// Build the notifier BPF program. `connect`/`socket`/`send{to,msg,mmsg}` become `USER_NOTIF`;
/// `io_uring_setup` becomes a scalar `EPERM` (its SQEs never re-enter this filter, so it cannot
/// be mediated per-op); `read`/`recv{from,msg}` are notified ONLY for descriptors in the DNS
/// window. When `write_broker` is set, the write-intent syscalls above are also notified —
/// `openat` gated on its flags carrying a write bit — so the deny-inside-allow broker can
/// mediate them; otherwise those syscalls are never trapped and cost nothing.
fn notifier_program(write_broker: bool) -> Vec<seccompiler::sock_filter> {
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
    if write_broker {
        // `openat` routes to the flags check; the rest go straight to the notifier.
        p.push(Ins::Jump(
            jeq,
            nr(libc::SYS_openat),
            "openat_flags",
            "w_openat",
        ));
        p.push(Ins::Label("w_openat"));
        for (i, syscall) in WRITE_INTENT_NRS.iter().enumerate().skip(1) {
            let after: &'static str = WRITE_INTENT_LABELS[i];
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
    if write_broker {
        // openat flags: masked write bits == 0 → read-only → allow; else notify.
        p.push(Ins::Label("openat_flags"));
        p.push(Ins::Stmt(ld, OFF_ARG2));
        p.push(Ins::Stmt(BPF_ALU | BPF_AND | BPF_K, write_open_mask()));
        p.push(Ins::Jump(jeq, 0, "allow", "notify"));
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

/// Per-index fall-through labels for [`WRITE_INTENT_NRS`], so the dispatch loop can name the
/// instruction after each write-intent test without allocating a label string at runtime.
const WRITE_INTENT_LABELS: &[&str] = &[
    "w_openat",
    "w_openat2",
    "w_mkdirat",
    "w_unlinkat",
    "w_symlinkat",
    "w_linkat",
    "w_renameat",
    "w_renameat2",
    "w_truncate",
];

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
    stub_port: u16,
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
        eprintln!("SUP DNS {name} -> {}", fmt_ip(fam, addr));
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

fn state() -> &'static Mutex<SupState> {
    static STATE: OnceLock<Mutex<SupState>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(SupState {
            allow_all: false,
            allow: Vec::new(),
            write_matcher: None,
            dns_map: Vec::new(),
            sk: Vec::new(),
            upstream_addr_be: 0,
            stub_port: 0,
        })
    })
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

/// Read `len` bytes at `off` from the target's `/proc/<pid>/mem`.
unsafe fn read_child_mem(pid: u32, off: u64, buf: &mut [u8]) -> isize {
    let path = CString::new(format!("/proc/{pid}/mem")).unwrap();
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return -1;
    }
    let n = unsafe {
        libc::pread(
            fd,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            off as libc::off_t,
        )
    };
    unsafe { libc::close(fd) };
    n
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
        eprintln!("[sup] SEND: {}", io::Error::last_os_error());
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
fn parse_response(p: &[u8]) {
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
            let mut st = state().lock().unwrap();
            st.record(&qname, libc::AF_INET, &p[off..off + 4]);
        } else if rtype == 28 && rdlen == 16 {
            let mut st = state().lock().unwrap();
            st.record(&qname, libc::AF_INET6, &p[off..off + 16]);
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

/// Bind a loopback UDP socket, run the forward-and-record loop on it, and return the
/// bound port. `upstream_be` is the real resolver in network byte order.
fn start_stub() -> io::Result<()> {
    // pick the upstream: /etc/resolv.conf's first `nameserver`, else 127.0.0.53
    let mut upstream_be: u32 = u32::from_ne_bytes([127, 0, 0, 53]);
    if let Ok(text) = std::fs::read_to_string("/etc/resolv.conf") {
        for line in text.lines() {
            if let Some(ip) = line.strip_prefix("nameserver ")
                && let Some(be) = parse_ipv4(ip.trim())
            {
                upstream_be = be;
                break;
            }
        }
    }
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut bind_addr = make_sockaddr_in(u32::from_ne_bytes([127, 0, 0, 1]), 0);
    if unsafe {
        libc::bind(
            fd,
            &bind_addr as *const _ as *const libc::sockaddr,
            size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    } < 0
    {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    let mut sl = size_of::<libc::sockaddr_in>() as libc::socklen_t;
    unsafe { libc::getsockname(fd, &mut bind_addr as *mut _ as *mut libc::sockaddr, &mut sl) };
    let port = u16::from_be(bind_addr.sin_port);
    {
        let mut st = state().lock().unwrap();
        st.upstream_addr_be = upstream_be;
        st.stub_port = port;
    }
    eprintln!(
        "SUP stub resolver 127.0.0.1:{port} -> upstream {}",
        fmt_ip(libc::AF_INET, &upstream_be.to_ne_bytes())
    );
    std::thread::spawn(move || stub_loop(fd, upstream_be));
    Ok(())
}

fn stub_loop(fd: RawFd, upstream_be: u32) {
    let mut q = [0u8; 1500];
    let mut r = [0u8; 4096];
    loop {
        let mut from: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        let mut fl = size_of::<libc::sockaddr_in>() as libc::socklen_t;
        let n = unsafe {
            libc::recvfrom(
                fd,
                q.as_mut_ptr() as *mut libc::c_void,
                q.len(),
                0,
                &mut from as *mut _ as *mut libc::sockaddr,
                &mut fl,
            )
        };
        if n <= 0 {
            continue;
        }
        let n = n as usize;
        let u = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if u < 0 {
            continue;
        }
        let tv = libc::timeval {
            tv_sec: 3,
            tv_usec: 0,
        };
        unsafe {
            libc::setsockopt(
                u,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const _ as *const libc::c_void,
                size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        let up = make_sockaddr_in(upstream_be, 53);
        let connected = unsafe {
            libc::connect(
                u,
                &up as *const _ as *const libc::sockaddr,
                size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        } == 0;
        if connected
            && unsafe { libc::send(u, q.as_ptr() as *const libc::c_void, n, 0) } == n as isize
        {
            let rn = unsafe { libc::recv(u, r.as_mut_ptr() as *mut libc::c_void, r.len(), 0) };
            if rn > 0 {
                parse_response(&r[..rn as usize]);
                unsafe {
                    libc::sendto(
                        fd,
                        r.as_ptr() as *const libc::c_void,
                        rn as usize,
                        0,
                        &from as *const _ as *const libc::sockaddr,
                        fl,
                    )
                };
            }
        }
        unsafe { libc::close(u) };
    }
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

fn is_write_intent(nr: libc::c_long) -> bool {
    WRITE_INTENT_NRS.contains(&nr)
}

/// Whether the fs policy permits a WRITE at `canon` (an absolute canonical path): the last
/// matching rule is an Allow granting ReadWrite. A Deny, a read-only Allow, or no match (the
/// allow-only base's default Deny) all forbid the write. This is the WHOLE fs decision, because
/// the broker performs the op outside Landlock and so must enforce the base, not just the denies.
fn write_allowed(matcher: &PathMatcher, canon: &str) -> bool {
    let d = matcher.decide(Path::new(canon));
    d.effect == Effect::Allow && d.access == FsAccess::ReadWrite
}

/// Read a NUL-terminated string at `addr` from the target's `/proc/<tid>/mem`, without the NUL.
/// `None` on a fault or no terminator within `max` bytes.
fn read_child_str(tid: u32, addr: u64, max: usize) -> Option<String> {
    let mut buf = vec![0u8; max];
    let got = unsafe { read_child_mem(tid, addr, &mut buf) };
    if got <= 0 {
        return None;
    }
    let end = buf[..got as usize].iter().position(|&b| b == 0)?;
    String::from_utf8(buf[..end].to_vec()).ok()
}

/// readlink(`/proc/self/fd/<fd>`) — where one of the SUPERVISOR's own fds really points.
fn fd_path(fd: RawFd) -> Option<String> {
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
    String::from_utf8(buf[..n as usize].to_vec()).ok()
}

/// Resolve `(tid, dirfd, path)` to a VERIFIED `O_PATH` fd of the PARENT directory, the parent's
/// canonical path, and the final component — or `Err(errno)`. The parent is `realpath`'d as a
/// hint, then reopened with `RESOLVE_NO_SYMLINKS` and its real target read back, so a symlink
/// swapped in after the hint either fails the open or is seen where it truly lands. The final
/// component is deliberately NOT followed here; the caller re-checks an opened fd's real path.
fn resolve_parent(tid: u32, dirfd: i32, path_in: &str) -> Result<(RawFd, String, String), i32> {
    // "/proc/self/…" / "/proc/thread-self/…" name the CHILD's process, not the supervisor's.
    let path: String = if let Some(rest) = path_in.strip_prefix("/proc/self/") {
        format!("/proc/{tid}/{rest}")
    } else if let Some(rest) = path_in.strip_prefix("/proc/thread-self/") {
        format!("/proc/{tid}/{rest}")
    } else {
        path_in.to_string()
    };
    let start = if path.starts_with('/') {
        "/".to_string()
    } else if dirfd == libc::AT_FDCWD {
        format!("/proc/{tid}/cwd")
    } else {
        format!("/proc/{tid}/fd/{dirfd}")
    };
    let (dirpart, base) = match path.rfind('/') {
        None => (".".to_string(), path.clone()),
        Some(0) => ("/".to_string(), path[1..].to_string()),
        Some(i) => (path[..i].to_string(), path[i + 1..].to_string()),
    };
    if base.is_empty() || base == "." || base == ".." {
        return Err(libc::EINVAL);
    }
    let dp = dirpart.strip_prefix('/').unwrap_or(&dirpart);
    let joined = format!("{start}/{dp}");
    let joined_c = CString::new(joined).map_err(|_| libc::EINVAL)?;
    let mut real = vec![0u8; libc::PATH_MAX as usize];
    let rp = unsafe { libc::realpath(joined_c.as_ptr(), real.as_mut_ptr() as *mut libc::c_char) };
    if rp.is_null() {
        return Err(errno());
    }
    let real_len = unsafe { libc::strlen(real.as_ptr() as *const libc::c_char) };
    let real_str = String::from_utf8_lossy(&real[..real_len]).into_owned();
    let real_c = CString::new(real_str).map_err(|_| libc::EIO)?;
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
        Some(canon) => Ok((pfd, canon, base)),
        None => {
            unsafe { libc::close(pfd) };
            Err(libc::EIO)
        }
    }
}

/// Join a verified parent's canonical path and a final component into the full canonical path.
fn join_full(canon: &str, base: &str) -> String {
    if canon == "/" {
        format!("/{base}")
    } else {
        format!("{canon}/{base}")
    }
}

/// Service one write-intent notification: read the path(s) once from the child, resolve the
/// parent(s) to verified fds, apply the deny/protect policy to the CANONICAL path, and — when
/// allowed — perform the operation itself relative to the verified parent, splicing any opened
/// fd back with `ADDFD`. The child's memory is never consulted after the single read, so there
/// is nothing to race. Replies to `nfd` itself (errno on denial/failure, value on success).
fn handle_write_intent(nfd: RawFd, req: &SeccompNotif) {
    let nr = req.data.nr as libc::c_long;
    let a = &req.data.args;
    // Per-syscall argument layout: which args hold (dirfd, path) and the optional second pair.
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
        _ => {
            reply_continue(nfd, req.id);
            return;
        }
    };

    let Some(path) = read_child_str(req.pid, pa, libc::PATH_MAX as usize) else {
        reply(nfd, req.id, -libc::EFAULT);
        return;
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

    // The broker is THE write-intent authority for a supervised launch (it performs opens
    // outside Landlock), so it must apply the FULL fs write policy — the allow-only base AND the
    // deny carve-outs — not just the denies (A6). No matcher ⇒ not armed; let the child run.
    let Some(matcher) = state().lock().unwrap().write_matcher() else {
        reply_continue(nfd, req.id);
        return;
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
        if !write_allowed(&matcher, &full) {
            eprintln!("SUP DENY write {full} -> EPERM");
            err = libc::EPERM;
            break 'act;
        }

        let mut base2 = String::new();
        if resolves_second {
            let path2_ref = path2.as_deref().unwrap_or("");
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
                eprintln!("SUP DENY write {full2} -> EPERM");
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
                        let real = String::from_utf8_lossy(&srcreal[..len]).into_owned();
                        !write_allowed(&matcher, &real)
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
                        flags & !libc::O_CLOEXEC,
                        mode as libc::c_uint,
                    )
                };
                if fd < 0 {
                    err = errno();
                    break 'act;
                }
                if fd_path(fd)
                    .map(|w| !write_allowed(&matcher, &w))
                    .unwrap_or(false)
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
                if fd_path(fd)
                    .map(|w| !write_allowed(&matcher, &w))
                    .unwrap_or(false)
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
                let target = cstr(path2.as_deref().unwrap_or(""));
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
                        libc::O_WRONLY | libc::O_NOFOLLOW,
                        0,
                    )
                };
                if fd < 0 {
                    err = errno();
                    break 'act;
                }
                if fd_path(fd)
                    .map(|w| !write_allowed(&matcher, &w))
                    .unwrap_or(false)
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
        eprintln!("[sup] SEND: {}", io::Error::last_os_error());
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
fn cstr(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// the supervisor loop
// ---------------------------------------------------------------------------

static DNS_SLOT: AtomicU32 = AtomicU32::new(0);

fn supervisor(nfd: RawFd) {
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
        let mut pfd = libc::pollfd {
            fd: nfd,
            events: libc::POLLIN,
            revents: 0,
        };
        let pr = unsafe { libc::poll(&mut pfd, 1, -1) };
        if pr < 0 {
            if errno() == libc::EINTR {
                continue;
            }
            return; // the listener itself is unusable
        }
        if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return; // the target is gone and the filter is being torn down — the clean exit
        }
        if pfd.revents & libc::POLLIN == 0 {
            continue;
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

        // Write-intent syscalls: the deny-inside-allow broker. Only reached when the filter was
        // built with the write branch (a policy carried carve-outs), so this is inert for the
        // build jail.
        if is_write_intent(nr) {
            handle_write_intent(nfd, &req);
            continue;
        }

        // DNS-socket recv*: observe the reply without consuming it, then CONTINUE.
        if nr == libc::SYS_recvfrom || nr == libc::SYS_read || nr == libc::SYS_recvmsg {
            let tg = tgid_of(req.pid);
            let dup = {
                let st = state().lock().unwrap();
                st.sk
                    .iter()
                    .find(|e| e.tgid == tg && e.fd == cfd && e.dns)
                    .map(|e| e.dup)
                    .unwrap_or(-1)
            };
            if dup >= 0 {
                let mut r = [0u8; 4096];
                let tv = libc::timeval {
                    tv_sec: 3,
                    tv_usec: 0,
                };
                unsafe {
                    libc::setsockopt(
                        dup,
                        libc::SOL_SOCKET,
                        libc::SO_RCVTIMEO,
                        &tv as *const _ as *const libc::c_void,
                        size_of::<libc::timeval>() as libc::socklen_t,
                    )
                };
                let rn = unsafe {
                    libc::recv(
                        dup,
                        r.as_mut_ptr() as *mut libc::c_void,
                        r.len(),
                        libc::MSG_PEEK,
                    )
                };
                if rn > 0 {
                    parse_response(&r[..rn as usize]);
                }
            }
            reply_continue(nfd, req.id);
            continue;
        }

        // send*: CONTINUE for a connected socket (NULL addr), EPERM for an addressed one.
        if nr == libc::SYS_sendto || nr == libc::SYS_sendmsg || nr == libc::SYS_sendmmsg {
            let addr_ptr: u64 = if nr == libc::SYS_sendto {
                req.data.args[4]
            } else {
                // msghdr.msg_name is the first field; read the pointer from child mem.
                let mut buf = [0u8; 8];
                let got = unsafe { read_child_mem(req.pid, req.data.args[1], &mut buf) };
                if got == 8 {
                    u64::from_ne_bytes(buf)
                } else {
                    1 // treat as addressed on failure
                }
            };
            if addr_ptr == 0 {
                reply_continue(nfd, req.id); // connected: policed at connect()
            } else {
                eprintln!("SUP DENY UDP-send (addressed) -> EPERM");
                reply(nfd, req.id, -libc::EPERM);
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
            let s = unsafe { libc::socket(dom, typ, pro) };
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
                let slot = DNS_SLOT.fetch_add(1, Ordering::Relaxed) % 64;
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
                let mut st = state().lock().unwrap();
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
            let st = state().lock().unwrap();
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
            eprintln!("SUP DENY (unreadable: got={got} sfd={sfd}) -> EPERM");
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
            eprintln!("SUP CONTINUE dom={dom}");
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
                    let st = state().lock().unwrap();
                    (st.upstream_addr_be,)
                };
                s = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
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
                    eprintln!(
                        "SUP DNS-UDP asked {ip}:{port} -> dialed upstream {}:53 (observed via peek)",
                        fmt_ip(libc::AF_INET, &upstream_be.to_ne_bytes())
                    );
                    let dupfd = unsafe { libc::dup(s) };
                    let mut st = state().lock().unwrap();
                    if let Some(e) = st.sk.iter_mut().find(|e| e.tgid == tgid && e.fd == cfd) {
                        e.dns = true;
                        e.dup = dupfd;
                    }
                } else {
                    eprintln!("SUP DENY UDP {ip}:{port} (dial upstream failed)");
                }
            } else {
                eprintln!("SUP DENY UDP {ip}:{port}");
            }
        } else if typ == libc::SOCK_STREAM {
            let loopback = fam == libc::AF_INET && addr[0] == 127;
            let name = {
                let st = state().lock().unwrap();
                st.lookup(fam, &addr[..n])
            };
            let allow = {
                let st = state().lock().unwrap();
                st.allowed(name.as_deref())
            };
            if loopback || allow {
                s = unsafe { libc::socket(fam, libc::SOCK_STREAM, 0) };
                if unsafe {
                    libc::connect(
                        s,
                        &ss as *const _ as *const libc::sockaddr,
                        alen as libc::socklen_t,
                    )
                } == 0
                {
                    verdict_err = 0;
                    eprintln!(
                        "SUP ALLOW {ip}:{port} name={}",
                        name.as_deref()
                            .unwrap_or(if loopback { "(loopback)" } else { "(none)" })
                    );
                } else {
                    verdict_err = -errno();
                    eprintln!(
                        "SUP dial {ip}:{port} failed: {}",
                        io::Error::last_os_error()
                    );
                }
            } else {
                eprintln!(
                    "SUP DENY {ip}:{port} name={} -> EPERM",
                    name.as_deref().unwrap_or("(none: no observed DNS)")
                );
            }
        } else {
            eprintln!("SUP DENY type={typ} fam={fam}");
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
                eprintln!("[sup] ADDFD: {}", io::Error::last_os_error());
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
    pub allow_all: bool,
    pub allow: Vec<String>,
    /// `Some` ⇒ arm the write broker as THE write-intent authority, enforcing this whole fs
    /// policy (allow-only base + deny carve-outs). `None` ⇒ no filesystem confinement on this
    /// launch, so the filter traps no write-intent syscall (the build jail's coarse path, and any
    /// net-only policy).
    pub write_policy: Option<FsRuleSet>,
}

impl EgressPolicy {
    /// Whether this policy arms the write broker. Governs whether the BPF filter includes the
    /// write-intent dispatch, so a launch that does not confine the filesystem pays nothing.
    fn write_broker(&self) -> bool {
        self.write_policy.is_some()
    }
}

/// Fork `argv` under the connect-notifier, hand its listener fd to an in-process supervisor
/// thread over a plain pipe, and wait for it. Returns the child's exit code.
///
/// SAFETY / DESIGN NOTE: this is the standalone (route.c-shaped) driver — it forks and the
/// child performs only async-signal-safe raw syscalls before `execve`. Embedding this into a
/// library launch path (where the confined child is spawned via `Command`/`pre_exec` rather
/// than a bespoke fork) is deliberately left to the sandbox launch code.
pub fn run_supervised(policy: EgressPolicy, argv: &[CString]) -> io::Result<i32> {
    // Build the filter in the PARENT (allocation is fine here); `fork` copies it into the child,
    // which installs it with no post-fork allocation.
    let filter = notifier_program(policy.write_broker());
    {
        let mut st = state().lock().unwrap();
        st.allow_all = policy.allow_all;
        st.allow = policy.allow.clone();
        st.write_matcher = policy
            .write_policy
            .as_ref()
            .map(|s| Arc::new(PathMatcher::new(s)));
    }
    start_stub()?;

    // child->parent (fd number) and parent->child (go) plain pipes
    let mut c2p = [0i32; 2];
    let mut p2c = [0i32; 2];
    if unsafe { libc::pipe(c2p.as_mut_ptr()) } != 0 || unsafe { libc::pipe(p2c.as_mut_ptr()) } != 0
    {
        return Err(io::Error::last_os_error());
    }

    let argv_ptrs: Vec<*const libc::c_char> = argv
        .iter()
        .map(|a| a.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // ---- child ---- (async-signal-safe only)
        unsafe {
            libc::close(c2p[0]);
            libc::close(p2c[1]);
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0 {
                libc::_exit(2);
            }
            let nfd = install_notifier(&filter);
            if nfd < 0 {
                libc::_exit(3);
            }
            // write the listener fd NUMBER (never SCM_RIGHTS) down the pipe
            let nfd_bytes = nfd.to_ne_bytes();
            if libc::write(c2p[1], nfd_bytes.as_ptr() as *const libc::c_void, 4) != 4 {
                libc::_exit(4);
            }
            let mut go = [0u8; 1];
            if libc::read(p2c[0], go.as_mut_ptr() as *mut libc::c_void, 1) != 1 {
                libc::_exit(5);
            }
            libc::close(nfd);
            libc::close(c2p[1]);
            libc::close(p2c[0]);
            libc::execvp(argv_ptrs[0], argv_ptrs.as_ptr());
            libc::_exit(9);
        }
    }

    // ---- parent ----
    unsafe {
        libc::close(c2p[1]);
        libc::close(p2c[0]);
    }
    let mut child_nfd_bytes = [0u8; 4];
    let got = unsafe { libc::read(c2p[0], child_nfd_bytes.as_mut_ptr() as *mut libc::c_void, 4) };
    if got != 4 {
        let mut st = 0;
        unsafe { libc::waitpid(pid, &mut st, 0) };
        return Err(io::Error::other("child did not hand over its notifier fd"));
    }
    let child_nfd = i32::from_ne_bytes(child_nfd_bytes);
    let pf = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as RawFd;
    let nfd = if pf >= 0 {
        unsafe { libc::syscall(libc::SYS_pidfd_getfd, pf, child_nfd, 0) as RawFd }
    } else {
        -1
    };
    if nfd < 0 {
        let e = io::Error::last_os_error();
        let mut st = 0;
        unsafe { libc::waitpid(pid, &mut st, 0) };
        return Err(e);
    }
    std::thread::spawn(move || supervisor(nfd));
    let _ = unsafe { libc::write(p2c[1], b"g".as_ptr() as *const libc::c_void, 1) };
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    if pf >= 0 {
        unsafe { libc::close(pf) };
    }
    Ok(if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        -1
    })
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
    /// The supervisor loop thread, held only so it is DETACHED (not joined) on wait/drop — see
    /// [`SupervisedChild::wait`]. The thread self-terminates when the target dies (its filter is
    /// torn down and `NOTIF_RECV` returns ENOENT); joining is still avoided so a stuck target
    /// cannot block the reap.
    supervisor: Option<std::thread::JoinHandle<()>>,
    /// Set once `waitpid` has reaped `pid`, so `Drop` neither re-kills nor double-reaps.
    reaped: bool,
    /// The child leads its own session (`setsid`), so `-pid` names its whole descendant tree.
    group_leader: bool,
}

impl SupervisedChild {
    pub(super) fn id(&self) -> u32 {
        self.pid as u32
    }

    /// The child's process-GROUP id — `Some` only when it leads its own group, the sole state
    /// in which `-pid` names the child's tree and nothing else. See [`SupervisedChild`].
    pub(super) fn process_group_id(&self) -> Option<i32> {
        self.group_leader.then_some(self.pid)
    }

    /// Reap the child and return its exit status. When it leads its own group, best-effort
    /// signals the group first — the supervised path has no PID namespace, so a signalled group
    /// is its only handle on a build tool the child backgrounded. Joins the supervisor thread
    /// after reaping (it has already returned once the target is gone).
    pub(super) fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        use std::os::unix::process::ExitStatusExt;
        let mut status = 0;
        loop {
            if unsafe { libc::waitpid(self.pid, &mut status, 0) } < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            break;
        }
        self.reaped = true;
        if self.group_leader {
            unsafe { libc::kill(-self.pid, libc::SIGKILL) };
        }
        // DETACH, never join. The thread self-terminates: once the target exits, its seccomp
        // filter is torn down and the blocked `NOTIF_RECV` returns ENOENT, so the loop returns and
        // the thread ends on its own (measured: one `RECV: No such file or directory` per launch).
        // So this is not a leak — dropping the handle detaches a thread already on its way out. A
        // JOIN here is nonetheless avoided: a stuck target (e.g. the pre-1.3 O_NONBLOCK hang, where
        // the child never exited) would block the join forever, and reaping the child is the
        // caller's contract, not "wait for the supervisor to notice." (epic 1.4d)
        drop(self.supervisor.take());
        Ok(std::process::ExitStatus::from_raw(status))
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        if !self.reaped {
            let target = if self.group_leader {
                -self.pid
            } else {
                self.pid
            };
            unsafe {
                libc::kill(target, libc::SIGKILL);
                let mut status = 0;
                libc::waitpid(self.pid, &mut status, 0);
            }
        }
        if self.pidfd >= 0 {
            unsafe { libc::close(self.pidfd) };
        }
        // Detached, never joined — see [`SupervisedChild::wait`].
        drop(self.supervisor.take());
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
    /// Put the child in its own session (`setsid`): detaches the controlling terminal (the
    /// `TIOCSTI` defence) and gives the parent a group to reap.
    pub setsid: bool,
}

/// Fork `launch.argv` as a fully-confined child under the connect-notifier, start its supervisor
/// thread, and return a handle the caller reaps. This is [`run_supervised`]'s network machinery
/// (DNS stub + supervisor + pidfd handoff) with the FULL child confinement a library launch needs
/// (setsid, `PDEATHSIG`, cloexec sweep, `no_new_privs`, capability drop, Landlock, the seccomp
/// ceiling) and returning rather than blocking on `waitpid`.
pub(super) fn spawn_supervised(
    policy: EgressPolicy,
    launch: SupervisedLaunch,
) -> io::Result<SupervisedChild> {
    // Built in the PARENT and copied into the child by `fork`; the child installs it without
    // allocating. The write-intent dispatch is present only when the policy carries carve-outs.
    let filter = notifier_program(policy.write_broker());
    {
        let mut st = state().lock().unwrap();
        st.allow_all = policy.allow_all;
        st.allow = policy.allow.clone();
        st.write_matcher = policy
            .write_policy
            .as_ref()
            .map(|s| Arc::new(PathMatcher::new(s)));
    }
    start_stub()?;

    // child->parent (listener fd NUMBER) and parent->child ("go" barrier) plain pipes.
    let mut c2p = [0i32; 2];
    let mut p2c = [0i32; 2];
    if unsafe { libc::pipe(c2p.as_mut_ptr()) } != 0 || unsafe { libc::pipe(p2c.as_mut_ptr()) } != 0
    {
        return Err(io::Error::last_os_error());
    }

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

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // ---- child ---- async-signal-safe only: raw syscalls, no allocation, no locks.
        unsafe {
            libc::close(c2p[0]);
            libc::close(p2c[1]);
            if launch.setsid && libc::setsid() < 0 {
                libc::_exit(10);
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                libc::_exit(11);
            }
            // FIRST, before any restriction: the sweep opens `/proc/self/fd`, which Landlock
            // below would make unreadable. It marks c2p[1]/p2c[0]/ruleset CLOEXEC too, which is
            // harmless — each stays usable until the explicit close before `execve`.
            if mark_inherited_fds_cloexec().is_err() {
                libc::_exit(12);
            }
            // Gates Landlock and seccomp, both of which refuse a caller that could still gain
            // privileges through a setuid `execve`.
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
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
    unsafe {
        libc::close(c2p[1]);
        libc::close(p2c[0]);
    }
    let reap = |pid: libc::pid_t| {
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
    };
    let mut child_nfd_bytes = [0u8; 4];
    let got = unsafe { libc::read(c2p[0], child_nfd_bytes.as_mut_ptr() as *mut libc::c_void, 4) };
    unsafe { libc::close(c2p[0]) };
    if got != 4 {
        unsafe { libc::close(p2c[1]) };
        reap(pid);
        return Err(io::Error::other(
            "supervised child did not hand over its notifier fd",
        ));
    }
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
        unsafe { libc::close(p2c[1]) };
        reap(pid);
        return Err(e);
    }
    let sup_thread = std::thread::spawn(move || supervisor(nfd));
    // Release the barrier ONLY after the supervisor owns the listener — otherwise the child
    // could execve and issue a filtered connect before anything services the notification.
    let _ = unsafe { libc::write(p2c[1], b"g".as_ptr() as *const libc::c_void, 1) };
    unsafe { libc::close(p2c[1]) };
    Ok(SupervisedChild {
        pid,
        pidfd: pf,
        supervisor: Some(sup_thread),
        reaped: false,
        group_leader: launch.setsid,
    })
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

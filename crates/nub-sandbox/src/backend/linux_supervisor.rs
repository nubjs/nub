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

use std::ffi::CString;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

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

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_NATIVE: u32 = 0xC000_003E; // AUDIT_ARCH_X86_64
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH_NATIVE: u32 = 0xC000_00B7; // AUDIT_ARCH_AARCH64

// offsets into `struct seccomp_data`
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_ARG0: u32 = 16;

// Supervisor-created DGRAM sockets are pinned into this descriptor window so the
// filter can cheaply decide which `recv*` calls are DNS sockets worth observing.
const DNS_FD_LO: u32 = 960;
const DNS_FD_HI: u32 = 1024;

fn stmt(code: u16, k: u32) -> seccompiler::sock_filter {
    seccompiler::sock_filter { code, jt: 0, jf: 0, k }
}
fn jump(code: u16, k: u32, jt: u8, jf: u8) -> seccompiler::sock_filter {
    seccompiler::sock_filter { code, jt, jf, k }
}

/// Build the connect-notifier BPF program — a faithful transcription of `route.c`'s
/// `install_connect_notifier` filter table. `connect`/`socket`/`send{to,msg,mmsg}` become
/// `USER_NOTIF`; `io_uring_setup` becomes a scalar `EPERM`; `read`/`recv{from,msg}` are
/// notified ONLY for descriptors in the DNS window; everything else runs.
fn connect_notifier_program() -> Vec<seccompiler::sock_filter> {
    let nr = |n: libc::c_long| n as u32;
    vec![
        /*  0 */ stmt(BPF_LD | BPF_W | BPF_ABS, OFF_ARCH),
        /*  1 */ jump(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_NATIVE, 1, 0),
        /*  2 */ stmt(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        /*  3 */ stmt(BPF_LD | BPF_W | BPF_ABS, OFF_NR),
        /*  4 */ jump(BPF_JMP | BPF_JEQ | BPF_K, nr(libc::SYS_io_uring_setup), 14, 0),
        /*  5 */ jump(BPF_JMP | BPF_JEQ | BPF_K, nr(libc::SYS_sendto), 11, 0),
        /*  6 */ jump(BPF_JMP | BPF_JEQ | BPF_K, nr(libc::SYS_sendmsg), 10, 0),
        /*  7 */ jump(BPF_JMP | BPF_JEQ | BPF_K, nr(libc::SYS_sendmmsg), 9, 0),
        /*  8 */ jump(BPF_JMP | BPF_JEQ | BPF_K, nr(libc::SYS_connect), 8, 0),
        /*  9 */ jump(BPF_JMP | BPF_JEQ | BPF_K, nr(libc::SYS_socket), 7, 0),
        /* 10 */ jump(BPF_JMP | BPF_JEQ | BPF_K, nr(libc::SYS_read), 3, 0),
        /* 11 */ jump(BPF_JMP | BPF_JEQ | BPF_K, nr(libc::SYS_recvfrom), 2, 0),
        /* 12 */ jump(BPF_JMP | BPF_JEQ | BPF_K, nr(libc::SYS_recvmsg), 1, 0),
        /* 13 */ stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        /* 14 */ stmt(BPF_LD | BPF_W | BPF_ABS, OFF_ARG0),
        /* 15 */ jump(BPF_JMP | BPF_JGE | BPF_K, DNS_FD_LO, 0, 2),
        /* 16 */ jump(BPF_JMP | BPF_JGE | BPF_K, DNS_FD_HI, 1, 0),
        /* 17 */ stmt(BPF_RET | BPF_K, SECCOMP_RET_USER_NOTIF),
        /* 18 */ stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        /* 19 */ stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | libc::EPERM as u32),
    ]
}

/// Install the connect notifier and return the listener descriptor (or `-errno`).
/// Must run AFTER `PR_SET_NO_NEW_PRIVS`.
unsafe fn install_connect_notifier() -> libc::c_int {
    let program = connect_notifier_program();
    let prog = libc::sock_fprog {
        len: program.len() as u16,
        filter: program.as_ptr() as *mut libc::sock_filter,
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

    fn sk_put(&mut self, tgid: u32, fd: i32, dom: i32, typ: i32) {
        if let Some(e) = self.sk.iter_mut().find(|e| e.tgid == tgid && e.fd == fd) {
            if e.dup >= 0 {
                unsafe { libc::close(e.dup) };
            }
            *e = SkEntry { tgid, fd, dom, typ, dns: false, dup: -1 };
            return;
        }
        if self.sk.len() < 1024 {
            self.sk.push(SkEntry { tgid, fd, dom, typ, dns: false, dup: -1 });
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
    Some(u32::from_ne_bytes([octets[0], octets[1], octets[2], octets[3]]))
}

/// Thread-group leader for a thread id (fds are shared across a thread group, so
/// `pidfd_open` and the sk key both want the tgid, not the calling tid).
fn tgid_of(tid: u32) -> u32 {
    let path = format!("/proc/{tid}/status");
    if let Ok(text) = std::fs::read_to_string(&path) {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("Tgid:") {
                if let Ok(v) = rest.trim().parse::<u32>() {
                    return v;
                }
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
    let mut r = SeccompNotifResp { id, error: err, ..Default::default() };
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
            if let Some(ip) = line.strip_prefix("nameserver ") {
                if let Some(be) = parse_ipv4(ip.trim()) {
                    upstream_be = be;
                    break;
                }
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
        let tv = libc::timeval { tv_sec: 3, tv_usec: 0 };
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
// the supervisor loop
// ---------------------------------------------------------------------------

static DNS_SLOT: AtomicU32 = AtomicU32::new(0);

fn supervisor(nfd: RawFd) {
    loop {
        let mut req: SeccompNotif = unsafe { std::mem::zeroed() };
        if ioctl_notif(nfd, notif_recv(), &mut req as *mut _ as *mut libc::c_void) < 0 {
            if errno() == libc::EINTR {
                continue;
            }
            eprintln!("[sup] RECV: {}", io::Error::last_os_error());
            return;
        }
        let nr = req.data.nr as libc::c_long;
        let cfd = req.data.args[0] as i32;

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
                let tv = libc::timeval { tv_sec: 3, tv_usec: 0 };
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
                    libc::recv(dup, r.as_mut_ptr() as *mut libc::c_void, r.len(), libc::MSG_PEEK)
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
                newfd_flags: if typ & libc::SOCK_CLOEXEC != 0 { libc::O_CLOEXEC as u32 } else { 0 },
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
            let mut r = SeccompNotifResp { id: req.id, val: newfd as i64, ..Default::default() };
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
        let sfd: RawFd;
        let mut pidfd: RawFd = -1;
        if by_construction {
            sfd = -2; // classified, no dup needed
        } else {
            pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, tgid, 0) } as RawFd;
            sfd = if pidfd >= 0 {
                unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd, cfd, 0) as RawFd }
            } else {
                -1
            };
        }
        if sfd >= 0 {
            let mut ol = size_of::<i32>() as libc::socklen_t;
            unsafe {
                libc::getsockopt(sfd, libc::SOL_SOCKET, libc::SO_TYPE, &mut typ as *mut _ as *mut libc::c_void, &mut ol);
                ol = size_of::<i32>() as libc::socklen_t;
                libc::getsockopt(sfd, libc::SOL_SOCKET, libc::SO_DOMAIN, &mut dom as *mut _ as *mut libc::c_void, &mut ol);
            }
        }

        // read the destination sockaddr ONCE from child memory
        let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let want = req.data.args[2] as usize;
        let alen = want.min(size_of::<libc::sockaddr_storage>());
        let ss_bytes = unsafe {
            std::slice::from_raw_parts_mut(&mut ss as *mut _ as *mut u8, size_of::<libc::sockaddr_storage>())
        };
        let got = unsafe { read_child_mem(req.pid, req.data.args[1], &mut ss_bytes[..alen]) };

        // notification must still be live, and the read must have yielded a sockaddr_in
        let id_live = ioctl_notif(nfd, notif_id_valid(), &mut { req.id } as *mut u64 as *mut libc::c_void) >= 0;
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
                    libc::connect(s, &up as *const _ as *const libc::sockaddr, size_of::<libc::sockaddr_in>() as libc::socklen_t)
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
                    libc::connect(s, &ss as *const _ as *const libc::sockaddr, alen as libc::socklen_t)
                } == 0
                {
                    verdict_err = 0;
                    eprintln!(
                        "SUP ALLOW {ip}:{port} name={}",
                        name.as_deref().unwrap_or(if loopback { "(loopback)" } else { "(none)" })
                    );
                } else {
                    verdict_err = -errno();
                    eprintln!("SUP dial {ip}:{port} failed: {}", io::Error::last_os_error());
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

/// The egress allowlist for [`run_supervised`].
pub struct EgressPolicy {
    pub allow_all: bool,
    pub allow: Vec<String>,
}

/// Fork `argv` under the connect-notifier, hand its listener fd to an in-process supervisor
/// thread over a plain pipe, and wait for it. Returns the child's exit code.
///
/// SAFETY / DESIGN NOTE: this is the standalone (route.c-shaped) driver — it forks and the
/// child performs only async-signal-safe raw syscalls before `execve`. Embedding this into a
/// library launch path (where the confined child is spawned via `Command`/`pre_exec` rather
/// than a bespoke fork) is deliberately left to the sandbox launch code.
pub fn run_supervised(policy: EgressPolicy, argv: &[CString]) -> io::Result<i32> {
    {
        let mut st = state().lock().unwrap();
        st.allow_all = policy.allow_all;
        st.allow = policy.allow.clone();
    }
    start_stub()?;

    // child->parent (fd number) and parent->child (go) plain pipes
    let mut c2p = [0i32; 2];
    let mut p2c = [0i32; 2];
    if unsafe { libc::pipe(c2p.as_mut_ptr()) } != 0 || unsafe { libc::pipe(p2c.as_mut_ptr()) } != 0 {
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
            let nfd = install_connect_notifier();
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
    let got = unsafe {
        libc::read(c2p[0], child_nfd_bytes.as_mut_ptr() as *mut libc::c_void, 4)
    };
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
    /// [`SupervisedChild::wait`]. `NOTIF_RECV` does not reliably wake on target death, so joining
    /// would hang; the process reaps the detached thread at exit.
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
        // DETACH, never join: `SECCOMP_IOCTL_NOTIF_RECV` does not reliably return when the target
        // dies, so joining here blocks forever. Dropping the handle detaches the thread, which the
        // process reaps at exit. Clean per-launch shutdown (closing the listener to unblock RECV)
        // is a supervisor-lifecycle hardening item (epic 1.4).
        drop(self.supervisor.take());
        Ok(std::process::ExitStatus::from_raw(status))
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        if !self.reaped {
            let target = if self.group_leader { -self.pid } else { self.pid };
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
    {
        let mut st = state().lock().unwrap();
        st.allow_all = policy.allow_all;
        st.allow = policy.allow.clone();
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
            if let Some(filter) = launch.seccomp_ceiling {
                if install_target_seccomp(filter).is_err() {
                    libc::_exit(16);
                }
            }
            // The USER_NOTIF listener, LAST so the ceiling above never traps to this supervisor.
            // `nfd` is handed to the parent then CLOSED before `execve`, so it never leaks into
            // the target (which could otherwise service its own notifications with CONTINUE).
            let nfd = install_connect_notifier();
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
            if let Some(cwd) = cwd_ptr {
                if libc::chdir(cwd) != 0 {
                    libc::_exit(20);
                }
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
    let got =
        unsafe { libc::read(c2p[0], child_nfd_bytes.as_mut_ptr() as *mut libc::c_void, 4) };
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

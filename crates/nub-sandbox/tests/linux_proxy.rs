//! Linux per-host egress (C3) — REAL enforcement e2e: empty netns + UDS bridge + the
//! live egress proxy, under the actual Bubblewrap monitor.
//!
//! Proves the net axis end-to-end, each with a NEGATIVE CONTROL:
//!   - a proxy-aware child reaches an ALLOWED host through the bridge→proxy (200);
//!   - a DENIED host is refused at the proxy (403 / SNI drop);
//!   - a raw client that IGNORES the proxy fails CLOSED (the empty netns has no route);
//!   - the per-session token still gates the tunnel (tokenless → 407);
//!   - CROSS-LAYER: target-created AF_UNIX sockets are seccomp-denied, while socketpair(2)
//!     remains available for local in-process IPC and the trusted bridge remains functional;
//!   - LIFECYCLE: after the child exits (normally AND killed) no bridge survives and the
//!     per-run socket dir is cleaned.
//!
//! Linux-only; skips where Bubblewrap cannot create an unprivileged user namespace. The
//! child is a tiny C probe (compiled with `cc`; skips if unavailable). Upstreams are
//! throwaway loopback echo servers in this parent process — reachable by the parent-side
//! proxy but NOT by the child's empty netns, which is exactly the confinement under test.
#![cfg(target_os = "linux")]

use nub_sandbox::compiler::{CompileCtx, ScopeCapabilities, ShellRunner};
use nub_sandbox::matcher::Homes;
use nub_sandbox::{
    CommandSpec, PreparedSignalTarget, RuntimeCapability, apply_with_runtime, compile,
};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn bwrap_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        ["/usr/bin/bwrap", "/bin/bwrap"].iter().any(|program| {
            std::process::Command::new(program)
                .args([
                    "--die-with-parent",
                    "--unshare-user",
                    "--unshare-net",
                    "--ro-bind",
                    "/",
                    "/",
                    "--",
                    "/bin/true",
                ])
                .status()
                .is_ok_and(|status| status.success())
        })
    })
}

fn skip_without_bwrap() -> bool {
    if bwrap_available() {
        return false;
    }
    let required = matches!(
        std::env::var("NUB_SANDBOX_REQUIRE_BWRAP").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    );
    assert!(
        !required,
        "NUB_SANDBOX_REQUIRE_BWRAP set but Bubblewrap cannot create the required user namespace"
    );
    true
}

fn runtime() -> &'static RuntimeCapability {
    static RUNTIME: std::sync::OnceLock<RuntimeCapability> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        RuntimeCapability::from_verified_executable(env!(
            "CARGO_BIN_EXE_nub-sandbox-monitor-harness"
        ))
        .expect("verify the purpose-built sandbox monitor harness")
    })
}

/// A loopback echo server (throwaway upstream reached by the parent-side proxy).
fn echo_server() -> SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            std::thread::spawn(move || {
                let mut b = [0u8; 4096];
                while let Ok(n) = s.read(&mut b) {
                    if n == 0 || s.write_all(&b[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

struct Fixture {
    _tmp: TempDir,
    proj: PathBuf,
    home: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::Builder::new()
        .prefix("nub-lproxy-")
        .tempdir()
        .unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let proj = root.join("proj");
    let home = root.join("home");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    Fixture {
        _tmp: tmp,
        proj,
        home,
    }
}

impl Fixture {
    fn ctx(&self) -> CompileCtx {
        CompileCtx {
            homes: Homes {
                home: self.home.clone(),
                tmp: std::env::temp_dir(),
                cache: self.home.join(".cache"),
                project: self.proj.clone(),
            },
            cwd: self.proj.clone(),
            caps: ScopeCapabilities::approved(),
            ambient_env: Default::default(),
            runner: Box::new(ShellRunner),
        }
    }

    /// Compile+apply+run the probe under `surface`, returning its exit code. `status()`
    /// holds the proxy + bridge alive for the child's whole run.
    fn run(&self, surface: Value, probe: &Path, args: &[&str]) -> i32 {
        let policy = compile(&surface, &self.ctx()).expect("compiles");
        let spec = CommandSpec::new(probe)
            .args(args.iter().copied())
            .cwd(&self.proj);
        let prepared = apply_with_runtime(&policy, spec, runtime()).expect("apply");
        prepared.status().expect("spawn").code().unwrap_or(-1)
    }
}

fn compile_probe(dir: &Path) -> Option<PathBuf> {
    let src = dir.join("probe.c");
    std::fs::write(&src, PROBE_C).ok()?;
    let bin = dir.join("probe");
    let ok = std::process::Command::new("cc")
        .arg(&src)
        .arg("-o")
        .arg(&bin)
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?
        .success();
    ok.then_some(bin)
}

fn ip(a: SocketAddr) -> String {
    a.ip().to_string()
}

/// Per-host policy: admit the loopback CIDR (so the proxy admits the loopback echo target)
/// + an SNI host glob. Relaxed fs so the NET axis is isolated.
fn net_policy() -> Value {
    json!({ "fs": true, "net": ["127.0.0.0/8", "*.allowed.example"] })
}

#[test]
fn allowed_host_reachable_denied_host_blocked_through_bridge() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let Some(probe) = compile_probe(&f.proj) else {
        return;
    };
    let echo = echo_server();
    let port = echo.port().to_string();

    // A proxy-aware child reaches an admitted target through the empty-netns bridge → the
    // parent-side proxy → the loopback echo (200).
    assert_eq!(
        f.run(net_policy(), &probe, &["proxyget", "127.0.0.1", &port]),
        0,
        "an admitted target returns 200 through the in-netns bridge and proxy"
    );
    // A target the policy denies is refused (403) at the proxy before it connects.
    assert_eq!(
        f.run(net_policy(), &probe, &["proxyget", "203.0.113.1", "443"]),
        3,
        "a denied target is refused (403) by the proxy"
    );
    // gate 2: an allowed SNI to the admitted target forwards; a denied SNI is dropped.
    assert_eq!(
        f.run(
            net_policy(),
            &probe,
            &["proxysni", "127.0.0.1", &port, "api.allowed.example"]
        ),
        0,
        "an allowed SNI forwards end-to-end through the bridge"
    );
    assert_eq!(
        f.run(
            net_policy(),
            &probe,
            &["proxysni", "127.0.0.1", &port, "evil.example"]
        ),
        5,
        "a denied SNI is dropped even though the target IP is admitted"
    );
}

#[test]
fn raw_client_ignoring_proxy_fails_closed() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let Some(probe) = compile_probe(&f.proj) else {
        return;
    };
    let echo = echo_server();
    let port = echo.port().to_string();

    // A non-cooperating client that dials the upstream DIRECTLY hits the empty netns — the
    // parent's loopback echo is in a DIFFERENT network namespace, so the connect fails. This
    // is the DEFINED per-host semantics: a program that bypasses the proxy env fails closed.
    assert_eq!(
        f.run(net_policy(), &probe, &["rawconnect", &ip(echo), &port]),
        1,
        "a raw connect that ignores the proxy is blocked by the empty netns"
    );
    // Negative control: with net unenforced there is no netns unshare, so the same direct
    // connect succeeds — proving the block above is the confinement, not a dead upstream.
    assert_eq!(
        f.run(
            json!({ "fs": true, "net": true }),
            &probe,
            &["rawconnect", &ip(echo), &port]
        ),
        0,
        "neg-control: unenforced net reaches the upstream directly"
    );
}

#[test]
fn per_host_requires_the_per_session_token() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let Some(probe) = compile_probe(&f.proj) else {
        return;
    };
    let echo = echo_server();
    let port = echo.port().to_string();

    // The bridge is a dumb pump — the parent proxy still enforces the per-session token. A
    // CONNECT without it is refused (407) even to an admitted target.
    assert_eq!(
        f.run(net_policy(), &probe, &["proxynoauth", "127.0.0.1", &port]),
        4,
        "a tokenless CONNECT through the bridge is refused (407)"
    );
    // Positive control: the child's own token (from HTTP_PROXY) forwards.
    assert_eq!(
        f.run(net_policy(), &probe, &["proxyget", "127.0.0.1", &port]),
        0,
        "the child's own token still forwards through the bridge"
    );
}

#[test]
fn per_host_denies_target_unix_sockets_but_permits_socketpair() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let Some(probe) = compile_probe(&f.proj) else {
        return;
    };

    // The target may not create AF_UNIX sockets: both abstract and filesystem-path UDS can
    // cross the netns boundary. The proxy's relay is trusted infrastructure outside this
    // target seccomp filter.
    assert_eq!(
        f.run(net_policy(), &probe, &["abstract"]),
        1,
        "per-host denies target-created abstract AF_UNIX sockets"
    );
    let ipc = f.proj.join("app.sock");
    let _ipc = UnixListener::bind(&ipc).unwrap();
    assert_eq!(
        f.run(net_policy(), &probe, &["udsconnect", ipc.to_str().unwrap()]),
        1,
        "per-host denies target-created filesystem AF_UNIX sockets"
    );

    // socketpair(2) creates a local connected pair without opening a host-addressable
    // AF_UNIX endpoint, so it remains available for target-local IPC.
    assert_eq!(
        f.run(net_policy(), &probe, &["socketpair"]),
        0,
        "per-host retains socketpair for target-local IPC"
    );

    // Negative control: without net enforcement, the same filesystem UDS remains available.
    assert_eq!(
        f.run(
            json!({ "fs": true, "net": true }),
            &probe,
            &["udsconnect", ipc.to_str().unwrap()]
        ),
        0,
        "neg-control: unenforced net does not install the AF_UNIX seccomp deny"
    );
}

#[test]
fn per_host_denies_netns_unconfined_socket_families() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let Some(probe) = compile_probe(&f.proj) else {
        return;
    };

    // The empty netns confines IP traffic but NOT AF_VSOCK — vsock CID addressing reaches the
    // hypervisor across the netns. Per-host keeps AF_VSOCK (and the other netns-unconfined
    // families) seccomp-denied, so it is not a weakening of coarse-deny. Guard on vsock being
    // creatable when UNENFORCED (the neg-control): where the kernel lacks vsock the probe
    // can't distinguish a seccomp block from an unsupported family, so skip.
    let unenforced = f.run(json!({ "fs": true, "net": true }), &probe, &["vsock"]);
    if unenforced != 0 {
        return; // no usable AF_VSOCK transport here — inconclusive, skip
    }
    assert_eq!(
        f.run(net_policy(), &probe, &["vsock"]),
        1,
        "per-host must deny AF_VSOCK (the empty netns does not confine it)"
    );
}

#[test]
fn no_bridge_survives_and_socket_dir_is_cleaned() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let Some(probe) = compile_probe(&f.proj) else {
        return;
    };

    // Each case captures ITS OWN bridge's exact socket-dir path via the `#[doc(hidden)]`
    // debug accessor and asserts that exact path is gone — not a scan of the shared OS
    // temp dir by this process's pid. Sibling tests in this binary run concurrently and
    // share one pid, so `nub-net-<pid>-*` names collide across tests (only the trailing
    // nonce differs); a temp-dir-wide scan sees live sibling bridges too and flakes.

    // NORMAL exit: a per-host child runs to completion; afterward the per-run socket dir is
    // gone (host bridge torn down on Drop) — no orphaned path to the proxy remains. The
    // in-netns bridge process died with the collapsed sandbox PID namespace.
    let policy = compile(&net_policy(), &f.ctx()).expect("compiles");
    let spec = CommandSpec::new(&probe).args(["socketpair"]).cwd(&f.proj);
    let prepared = apply_with_runtime(&policy, spec, runtime()).expect("apply");
    let normal_dir = prepared
        .debug_net_bridge_dir()
        .expect("per-host net policy starts a bridge")
        .to_path_buf();
    assert_eq!(
        prepared.status().expect("spawn").code().unwrap_or(-1),
        0,
        "a per-host child runs to completion"
    );
    assert!(
        !normal_dir.exists(),
        "the per-run socket dir must be cleaned after a normal exit"
    );

    // KILLED mid-run: spawn a sleeping per-host child, SIGKILL it, wait. The fail-closed
    // cleanup reaps the in-netns bridge and the host bridge Drop removes the socket dir.
    let policy = compile(&net_policy(), &f.ctx()).expect("compiles");
    let spec = CommandSpec::new(&probe).args(["sleep", "10"]).cwd(&f.proj);
    let prepared = apply_with_runtime(&policy, spec, runtime()).expect("apply");
    let killed_dir = prepared
        .debug_net_bridge_dir()
        .expect("per-host net policy starts a bridge")
        .to_path_buf();
    let mut child = prepared
        .spawn_with_signal_target(|target| {
            if let PreparedSignalTarget::Direct(pid) = target {
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                });
            }
            Ok(())
        })
        .expect("spawn");
    let _ = child.wait();
    drop(child);
    assert!(
        !killed_dir.exists(),
        "the per-run socket dir must be cleaned after the child is killed"
    );
}

// Exit codes: rawconnect 0=ok 1=fail; proxyget 0=200 3=refused(403) 1=err; proxynoauth
// 4=refused-407 0=leaked 1=err; proxysni 0=forwarded 5=dropped 1=err; udsconnect 0=ok
// 1=fail; abstract 0=ok 1=fail; sleep <secs>. Reads HTTP_PROXY for the in-netns bridge port
// AND the per-session token (`http://<token>@host:port`).
const PROBE_C: &str = r#"
#include <arpa/inet.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/time.h>
#include <unistd.h>
#ifndef AF_VSOCK
#define AF_VSOCK 40
#endif

static int dial(const char* ip, int port) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    struct timeval tv = { .tv_sec = 3, .tv_usec = 0 };
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof tv);
    struct sockaddr_in a; memset(&a, 0, sizeof a);
    a.sin_family = AF_INET; a.sin_port = htons(port);
    inet_pton(AF_INET, ip, &a.sin_addr);
    if (connect(fd, (struct sockaddr*)&a, sizeof a) != 0) { close(fd); return -1; }
    return fd;
}

static int proxy_port(void) {
    const char* p = getenv("HTTP_PROXY");
    if (!p) return 0;
    const char* c = strrchr(p, ':');
    return c ? atoi(c + 1) : 0;
}

static int proxy_token(char* out, int cap) {
    const char* p = getenv("HTTP_PROXY"); if (!p) return 0;
    const char* s = strstr(p, "//"); if (!s) return 0; s += 2;
    const char* at = strchr(s, '@'); if (!at) return 0;
    int n = (int)(at - s); if (n <= 0 || n >= cap) return 0;
    memcpy(out, s, n); out[n] = 0; return 1;
}

static const char B64[] = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
static void b64enc(const unsigned char* in, int len, char* out) {
    int o = 0, i = 0;
    for (; i + 3 <= len; i += 3) {
        unsigned v = (in[i] << 16) | (in[i+1] << 8) | in[i+2];
        out[o++] = B64[(v>>18)&63]; out[o++] = B64[(v>>12)&63];
        out[o++] = B64[(v>>6)&63];  out[o++] = B64[v&63];
    }
    int rem = len - i;
    if (rem == 1) { unsigned v = in[i] << 16; out[o++]=B64[(v>>18)&63]; out[o++]=B64[(v>>12)&63]; out[o++]='='; out[o++]='='; }
    else if (rem == 2) { unsigned v=(in[i]<<16)|(in[i+1]<<8); out[o++]=B64[(v>>18)&63]; out[o++]=B64[(v>>12)&63]; out[o++]=B64[(v>>6)&63]; out[o++]='='; }
    out[o] = 0;
}

static int auth_header(char* hdr, int cap) {
    char tok[128]; if (!proxy_token(tok, sizeof tok)) { hdr[0] = 0; return 0; }
    char cred[160]; int n = snprintf(cred, sizeof cred, "%s:", tok);
    char enc[256]; b64enc((const unsigned char*)cred, n, enc);
    snprintf(hdr, cap, "Proxy-Authorization: Basic %s\r\n", enc);
    return 1;
}

static int build_hello(const char* sni, unsigned char* out) {
    unsigned char body[1024]; int b = 0;
    body[b++]=0x03; body[b++]=0x03;
    for (int i=0;i<32;i++) body[b++]=0;
    body[b++]=0;
    body[b++]=0x00; body[b++]=0x02; body[b++]=0x13; body[b++]=0x01;
    body[b++]=0x01; body[b++]=0x00;
    int slen = strlen(sni);
    int entry = 1 + 2 + slen;
    int extdata = 2 + entry;
    int exttotal = 4 + extdata;
    body[b++]=(exttotal>>8)&0xff; body[b++]=exttotal&0xff;
    body[b++]=0x00; body[b++]=0x00;
    body[b++]=(extdata>>8)&0xff; body[b++]=extdata&0xff;
    body[b++]=(entry>>8)&0xff; body[b++]=entry&0xff;
    body[b++]=0x00;
    body[b++]=(slen>>8)&0xff; body[b++]=slen&0xff;
    memcpy(body+b, sni, slen); b+=slen;
    int o=0;
    out[o++]=0x16; out[o++]=0x03; out[o++]=0x01;
    int hs = 4 + b;
    out[o++]=(hs>>8)&0xff; out[o++]=hs&0xff;
    out[o++]=0x01; out[o++]=(b>>16)&0xff; out[o++]=(b>>8)&0xff; out[o++]=b&0xff;
    memcpy(out+o, body, b); o+=b;
    return o;
}

static int proxy_connect(const char* tgt, int tport, int with_auth) {
    int pp = proxy_port(); if (!pp) return -1;
    int fd = dial("127.0.0.1", pp); if (fd < 0) return -1;
    char hdr[256]; hdr[0] = 0;
    if (with_auth) auth_header(hdr, sizeof hdr);
    char req[512];
    int n = snprintf(req, sizeof req, "CONNECT %s:%d HTTP/1.1\r\nHost: %s\r\n%s\r\n", tgt, tport, tgt, hdr);
    if (write(fd, req, n) != n) { close(fd); return -1; }
    char resp[512]; int got = 0;
    while (got < 4 || memcmp(resp+got-4, "\r\n\r\n", 4)) {
        int r = read(fd, resp+got, 1);
        if (r <= 0) { close(fd); return -1; }
        got += r; if (got >= (int)sizeof resp) break;
    }
    if (strncmp(resp, "HTTP/1.1 200", 12) != 0) { close(fd); return -2; }
    return fd;
}

/* Connect to a filesystem-path AF_UNIX socket. 0 = connected, 1 = failed. */
static int uds_connect(const char* path) {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return 1;
    struct sockaddr_un a; memset(&a, 0, sizeof a);
    a.sun_family = AF_UNIX;
    strncpy(a.sun_path, path, sizeof a.sun_path - 1);
    int r = connect(fd, (struct sockaddr*)&a, sizeof a);
    close(fd);
    return r == 0 ? 0 : 1;
}

/* Bind + connect an ABSTRACT-namespace AF_UNIX socket within this process. 0 = works. */
static int socketpair_ok(void) {
    int fd[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, fd) != 0) return 1;
    close(fd[0]); close(fd[1]);
    return 0;
}

static int abstract_ok(void) {
    int lfd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (lfd < 0) return 1;
    struct sockaddr_un a; memset(&a, 0, sizeof a);
    a.sun_family = AF_UNIX;
    const char* name = "nub-xlayer-test";
    a.sun_path[0] = 0; /* leading NUL = abstract namespace */
    memcpy(a.sun_path + 1, name, strlen(name));
    socklen_t len = offsetof(struct sockaddr_un, sun_path) + 1 + strlen(name);
    if (bind(lfd, (struct sockaddr*)&a, len) != 0) { close(lfd); return 1; }
    if (listen(lfd, 1) != 0) { close(lfd); return 1; }
    int cfd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (cfd < 0) { close(lfd); return 1; }
    int r = connect(cfd, (struct sockaddr*)&a, len);
    close(cfd); close(lfd);
    return r == 0 ? 0 : 1;
}

int main(int argc, char** argv) {
    if (argc < 2) return 1;
    if (!strcmp(argv[1], "rawconnect")) {
        int fd = dial(argv[2], atoi(argv[3]));
        if (fd < 0) return 1; close(fd); return 0;
    }
    if (!strcmp(argv[1], "proxyget")) {
        int fd = proxy_connect(argv[2], atoi(argv[3]), 1);
        if (fd == -2) return 3;
        if (fd < 0) return 1;
        close(fd); return 0;
    }
    if (!strcmp(argv[1], "proxynoauth")) {
        int fd = proxy_connect(argv[2], atoi(argv[3]), 0);
        if (fd == -2) return 4;
        if (fd < 0) return 1;
        close(fd); return 0;
    }
    if (!strcmp(argv[1], "proxysni")) {
        int fd = proxy_connect(argv[2], atoi(argv[3]), 1);
        if (fd < 0) return 1;
        unsigned char hello[1024]; int hl = build_hello(argv[4], hello);
        if (write(fd, hello, hl) != hl) { close(fd); return 1; }
        unsigned char echo[64];
        int r = read(fd, echo, sizeof echo);
        close(fd);
        return (r > 0) ? 0 : 5;
    }
    if (!strcmp(argv[1], "udsconnect")) return uds_connect(argv[2]);
    if (!strcmp(argv[1], "abstract")) return abstract_ok();
    if (!strcmp(argv[1], "socketpair")) return socketpair_ok();
    /* Create an AF_VSOCK socket. 0 = created (reaches the hypervisor transport), 1 = the
       socket() was refused (seccomp EPERM, or the family is unsupported). */
    if (!strcmp(argv[1], "vsock")) {
        int fd = socket(AF_VSOCK, SOCK_STREAM, 0);
        if (fd < 0) return 1;
        close(fd); return 0;
    }
    if (!strcmp(argv[1], "sleep")) { sleep(atoi(argv[2])); return 0; }
    return 1;
}
"#;

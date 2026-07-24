//! macOS per-host egress — REAL enforcement e2e (Seatbelt + the live egress proxy).
//!
//! Proves the net axis end-to-end under the actual kernel sandbox, each with a
//! NEGATIVE CONTROL (lift enforcement → the blocked thing succeeds):
//!   - the child can reach the proxy's loopback port (the narrowed carve works);
//!   - a DIRECT connect bypassing the proxy is BLOCKED by Seatbelt;
//!   - an arbitrary loopback sibling listener is DENIED (all-loopback carve closed);
//!   - through the proxy: an allowed SNI FORWARDS, a denied SNI is DROPPED.
//!
//! The child is a tiny C probe (compiled with `cc`; the suite skips if unavailable).
//! Upstreams are throwaway loopback echo servers in this parent process. The proxy is
//! started inside `apply()`; the child discovers its port from `HTTP_PROXY`.
#![cfg(target_os = "macos")]

use nub_sandbox::compiler::{CompileCtx, ScopeCapabilities, ShellRunner};
use nub_sandbox::matcher::Homes;
use nub_sandbox::{CommandSpec, apply, compile};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tempfile::TempDir;

/// A loopback echo server (throwaway upstream). Reflects bytes until EOF.
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

/// A project + fake home under `/private/tmp` (canonical, write-confineable).
struct Fixture {
    _tmp: TempDir,
    proj: PathBuf,
    home: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::Builder::new()
        .prefix("nub-proxy-")
        .tempdir_in("/private/tmp")
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

    /// Run the probe with `args` under `surface`, returning its exit code. `status()`
    /// holds the proxy alive for the child's whole run (the probe prints nothing, so
    /// inherited stdio is fine).
    fn run(&self, surface: Value, probe: &Path, args: &[&str]) -> i32 {
        let policy = compile(&surface, &self.ctx()).expect("compiles");
        let spec = CommandSpec::new(probe)
            .args(args.iter().copied())
            .cwd(&self.proj);
        let prepared = apply(&policy, spec).expect("apply");
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
        .stderr(Stdio::null())
        .status()
        .ok()?
        .success();
    ok.then_some(bin)
}

// Exit codes: rawconnect 0=ok 1=fail; proxyget 0=200 3=refused 1=err; proxynoauth
// 4=refused-407 0=leaked 1=err; proxysni 0=forwarded 5=dropped 1=err. The probe reads
// HTTP_PROXY for the proxy port AND the per-session token (`http://<token>@host:port`).
const PROBE_C: &str = r#"
#include <arpa/inet.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

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

/* Extract the per-session token from HTTP_PROXY (`http://<token>@127.0.0.1:<port>`),
   between "//" and "@", into `out`. Returns 1 on success. */
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

/* Build the `Proxy-Authorization: Basic <b64(token:)>\r\n` header line into `hdr`.
   Returns 1 if a token was present (header written), else 0 (hdr emptied). */
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
    int entry = 1 + 2 + slen;      /* name_type + name_len + name */
    int extdata = 2 + entry;        /* server_name_list_len prefix + entry */
    int exttotal = 4 + extdata;     /* ext type(2) + ext_len(2) + ext_data */
    body[b++]=(exttotal>>8)&0xff; body[b++]=exttotal&0xff; /* extensions total len */
    body[b++]=0x00; body[b++]=0x00;                         /* ext type server_name */
    body[b++]=(extdata>>8)&0xff; body[b++]=extdata&0xff;    /* ext_len */
    body[b++]=(entry>>8)&0xff; body[b++]=entry&0xff;        /* server_name_list len */
    body[b++]=0x00;                                   /* host_name */
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

/* CONNECT tgt:port through the proxy. `with_auth` toggles sending the per-session
   Proxy-Authorization header. Returns the fd on a 200, -2 on a non-200 (e.g. 407/403),
   -1 on a transport error. */
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

int main(int argc, char** argv) {
    if (argc < 2) return 1;
    if (!strcmp(argv[1], "rawconnect")) {
        int fd = dial(argv[2], atoi(argv[3]));
        if (fd < 0) return 1; close(fd); return 0;
    }
    if (!strcmp(argv[1], "proxyget")) {
        int fd = proxy_connect(argv[2], atoi(argv[3]), 1);
        if (fd == -2) return 3;   /* proxy refused (403) */
        if (fd < 0) return 1;     /* couldn't reach proxy */
        close(fd); return 0;      /* 200 */
    }
    /* Like proxyget but WITHOUT the token — proves the per-session gate: a co-resident
       process that lacks the token is refused. 4=refused (407, expected), 0=leaked
       through (200, a bug), 1=couldn't reach proxy. */
    if (!strcmp(argv[1], "proxynoauth")) {
        int fd = proxy_connect(argv[2], atoi(argv[3]), 0);
        if (fd == -2) return 4;   /* 407 — correctly refused */
        if (fd < 0) return 1;
        close(fd); return 0;      /* got a 200 without a token — leak */
    }
    if (!strcmp(argv[1], "proxysni")) {
        int fd = proxy_connect(argv[2], atoi(argv[3]), 1);
        if (fd < 0) return 1;
        unsigned char hello[1024]; int hl = build_hello(argv[4], hello);
        if (write(fd, hello, hl) != hl) { close(fd); return 1; }
        unsigned char echo[64];
        int r = read(fd, echo, sizeof echo);
        close(fd);
        return (r > 0) ? 0 : 5;   /* forwarded (echoed) vs dropped */
    }
    return 1;
}
"#;

fn ip(a: SocketAddr) -> String {
    a.ip().to_string()
}

// Policy allowing the loopback CIDR (so the proxy admits the loopback echo target) +
// the SNI host glob. Relaxed fs so the NET axis is isolated (probe execs freely).
fn net_policy() -> Value {
    json!({ "fs": true, "net": ["127.0.0.0/8", "*.allowed.example"], "proxy": "auto" })
}

#[test]
fn proxy_port_reachable_but_siblings_and_direct_blocked() {
    let f = fixture();
    let Some(probe) = compile_probe(&f.proj) else {
        return;
    };
    let echo = echo_server();
    let sibling = echo_server(); // a second loopback service (docker-like)

    // Under enforcement: a DIRECT connect to the echo upstream is BLOCKED by Seatbelt
    // (only localhost:<proxyport> is carved) — the probe cannot reach it.
    assert_eq!(
        f.run(
            net_policy(),
            &probe,
            &["rawconnect", &ip(echo), &echo.port().to_string()]
        ),
        1,
        "direct connect to the upstream must be blocked by Seatbelt"
    );
    // A sibling loopback listener on another port is likewise denied (the all-loopback
    // carve is closed — this is the local-exfil fix).
    assert_eq!(
        f.run(
            net_policy(),
            &probe,
            &["rawconnect", "127.0.0.1", &sibling.port().to_string()]
        ),
        1,
        "arbitrary loopback sibling must be denied"
    );
    // Negative control: net relaxed → the same direct connect succeeds. Under the
    // complete-statement model net must be named `true` explicitly (an unlisted
    // axis now FLOORS to deny-all, so `{ fs: true }` alone would confine egress).
    assert_eq!(
        f.run(
            json!({ "fs": true, "net": true }),
            &probe,
            &["rawconnect", &ip(echo), &echo.port().to_string()]
        ),
        0,
        "neg-control: unenforced net reaches the upstream directly"
    );
}

#[test]
fn allowed_sni_forwards_denied_sni_dropped_through_proxy() {
    let f = fixture();
    let Some(probe) = compile_probe(&f.proj) else {
        return;
    };
    let echo = echo_server();
    let port = echo.port().to_string();

    // Through the proxy (the child reaches localhost:<proxyport>, allowed by Seatbelt):
    // an allowed SNI to the admitted loopback target forwards end-to-end.
    assert_eq!(
        f.run(
            net_policy(),
            &probe,
            &["proxysni", "127.0.0.1", &port, "api.allowed.example"]
        ),
        0,
        "allowed SNI must forward through the proxy under Seatbelt"
    );
    // A denied SNI to the SAME admitted target is dropped by the proxy's SNI gate.
    assert_eq!(
        f.run(
            net_policy(),
            &probe,
            &["proxysni", "127.0.0.1", &port, "evil.example"]
        ),
        5,
        "denied SNI must be dropped even though the target IP is admitted"
    );
    // gate-1 (target host) through the real proxy under Seatbelt: an admitted loopback
    // target gets a 200; a target the policy denies gets a 403. Use a non-loopback IP
    // literal the CIDR does NOT cover for the denied case.
    assert_eq!(
        f.run(net_policy(), &probe, &["proxyget", "127.0.0.1", &port]),
        0,
        "admitted target returns 200 via the proxy"
    );
    assert_eq!(
        f.run(net_policy(), &probe, &["proxyget", "203.0.113.1", "443"]),
        3,
        "a denied target is refused (403) by the proxy before connecting"
    );
}

#[test]
fn proxy_requires_the_per_session_token() {
    // Defense-in-depth: the loopback proxy is reachable (carved) but a caller WITHOUT the
    // per-session token is refused (407). The child holds the token in its own env — the
    // `proxynoauth` mode deliberately omits it, standing in for a co-resident same-user
    // process that never learned it. The paired positive control (WITH the token) forwards.
    let f = fixture();
    let Some(probe) = compile_probe(&f.proj) else {
        return;
    };
    let echo = echo_server();
    let port = echo.port().to_string();

    // Without the token → 407, even though the target IP is admitted at gate 1.
    assert_eq!(
        f.run(net_policy(), &probe, &["proxynoauth", "127.0.0.1", &port]),
        4,
        "a tokenless CONNECT to an admitted target must be refused (407)"
    );
    // Positive control: the same target WITH the token gets a 200.
    assert_eq!(
        f.run(net_policy(), &probe, &["proxyget", "127.0.0.1", &port]),
        0,
        "the child's own token must still forward"
    );
}

#[test]
fn ssrf_metadata_target_dropped_even_when_policy_admits_it() {
    // SSRF guard: a policy that ADMITS the link-local /16 at gate 1 still cannot reach the
    // cloud-metadata endpoint — connect_upstream refuses `169.254.169.254`. The negative
    // control (an admitted loopback echo under the SAME policy forwards) proves this is a
    // targeted egress block, not a broken tunnel. (That the drop is the GUARD and not a
    // dead-IP timeout is proven deterministically by the connect_upstream unit test; a
    // link-local endpoint cannot be stood up to show a would-be-reachable pre-fix path.)
    let f = fixture();
    let Some(probe) = compile_probe(&f.proj) else {
        return;
    };
    let echo = echo_server();
    let port = echo.port().to_string();
    // Admit BOTH the link-local range (so gate 1 passes for the metadata IP) and loopback.
    let policy = json!({ "fs": true, "net": ["169.254.0.0/16", "127.0.0.0/8", "*.allowed.example"], "proxy": "auto" });

    assert_eq!(
        f.run(
            policy.clone(),
            &probe,
            &["proxysni", "169.254.169.254", "80", "api.allowed.example"]
        ),
        5,
        "metadata target must be dropped at connect even though gate 1 admits it"
    );
    assert_eq!(
        f.run(
            policy,
            &probe,
            &["proxysni", "127.0.0.1", &port, "api.allowed.example"]
        ),
        0,
        "neg-control: an admitted loopback target still forwards under the same policy"
    );
}

//! Proves PER-HOST egress through the loopback SNI-inspecting proxy (epic 5.1), driving
//! nub-sandbox's REAL public API (`compile` → `apply` → `status`) on all three OSes. A
//! fine-grained `net` allowlist derives `ProxyMode::Auto`, so `apply` starts the proxy; how the
//! child reaches it differs by OS, so the arms do too:
//!
//! LINUX (transparent redirect). The child is NON-cooperative — no `HTTP_PROXY`, `--noproxy '*'`
//! besides — and the seccomp supervisor redirects every TCP connect except its own proxy endpoint by
//! speaking the cooperative CONNECT on its behalf. A block is the OS interception, never client
//! good-behavior. The SNI gate is isolated by a same-IP discriminator (arms 3 vs 4).
//!
//! MACOS (deny-all-but-proxy). Seatbelt allows the child ONLY `localhost:<proxy_port>`; a
//! cooperative client honors the injected `https_proxy` and reaches the proxy, a non-cooperative
//! one dials direct and Seatbelt denies it — for EVERY host, allowed or not (the accepted
//! compatibility cost of having no transparent redirect on macOS). Arms 3+4 both fail ⇒
//! non-cooperative egress is blocked regardless of host = never leaked (A1).

#[cfg(target_os = "macos")]
use nub_sandbox::Sandbox;
use nub_sandbox::{
    apply, compile, CommandSpec, CompileCtx, Homes, SandboxPolicy, ScopeCapabilities,
};
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use serde_json::json;
use serde_json::Value;
use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::io::{Read, Write};
#[cfg(target_os = "macos")]
use std::net::TcpStream;
#[cfg(target_os = "linux")]
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
#[cfg(target_os = "linux")]
use std::thread;

fn policy(surface: Value) -> SandboxPolicy {
    let root = std::env::temp_dir();
    let homes = Homes {
        home: root.clone(),
        tmp: root.clone(),
        cache: root.clone(),
        project: root.clone(),
    };
    let mut env = BTreeMap::new();
    // The Windows co-package helper receives the compiled environment rather than the parent
    // environment. Keep only OS startup roots in this fixture; no secret-bearing ambient values
    // are relevant to the proxy contract.
    for key in [
        "PATH",
        "HOME",
        "SystemRoot",
        "SYSTEMROOT",
        "WINDIR",
        "TEMP",
        "TMP",
        "LOCALAPPDATA",
        "USERPROFILE",
        "ComSpec",
    ] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.to_string(), value);
        }
    }
    let ctx = CompileCtx::new(homes, root, ScopeCapabilities::approved(), env);
    compile(&surface, &ctx).expect("compile net policy")
}

/// Run `curl` under `policy`, return its exit code. `noproxy` removes every common proxy variable
/// and adds `--noproxy '*'`, so curl dials the destination DIRECTLY — the non-cooperative case.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn curl(label: &str, policy: &SandboxPolicy, noproxy: bool, curl_args: &str) -> i32 {
    curl_family(label, policy, noproxy, "-4", curl_args)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn curl_family(
    label: &str,
    policy: &SandboxPolicy,
    noproxy: bool,
    family: &str,
    curl_args: &str,
) -> i32 {
    let curl = if noproxy {
        "env -u https_proxy -u HTTPS_PROXY -u http_proxy -u HTTP_PROXY -u all_proxy -u ALL_PROXY -u no_proxy -u NO_PROXY curl --noproxy '*'"
    } else {
        "curl"
    };
    let script =
        format!("{curl} {family} -sS -o /dev/null --connect-timeout 8 --max-time 20 {curl_args}");
    eprintln!(">>> {label}: sh -c {script:?}");
    let spec = CommandSpec::new("/bin/sh").arg("-c").arg(&script);
    let code = apply(policy, spec)
        .expect("apply policy")
        .status()
        .expect("run confined child")
        .code()
        .unwrap_or(-1);
    eprintln!("<<< {label}: exited {code}");
    code
}

/// Separate listener availability from Seatbelt authorization. The parent is unconfined, while
/// the first socket is a child submitted through the same retained public [`Sandbox`] session.
/// That child reports only the port and `raw_os_error`; then the parent makes the liveness control.
/// Neither path prints a proxy URL or bearer token.
#[cfg(target_os = "macos")]
fn retained_proxy_socket_probe(sandbox: &Sandbox) -> bool {
    let script = r#"import os, socket, sys, urllib.parse
port = urllib.parse.urlsplit(os.environ["HTTPS_PROXY"]).port
try:
    socket.create_connection(("127.0.0.1", port), timeout=2).close()
except OSError as error:
    print(f"port={port} raw_os_error={error.errno}")
    sys.exit(1)
print(f"port={port} connected")"#;
    let prepared = match sandbox.prepare(CommandSpec::new("python3").args(["-c", script])) {
        Ok(prepared) => prepared,
        Err(error) => {
            eprintln!(
                "retained proxy confined socket: preparation failed, unavailable axes: {:?}",
                error.lost
            );
            return false;
        }
    };
    let output = match prepared.output() {
        Ok(output) => output,
        Err(error) => {
            eprintln!(
                "retained proxy confined socket: launch failed raw_os_error={:?}",
                error.raw_os_error()
            );
            return false;
        }
    };
    let detail = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    eprintln!(
        "retained proxy first confined socket: exited {} {}",
        output.status,
        if detail.is_empty() {
            "(no output)"
        } else {
            &detail
        }
    );
    let Some(port) = detail
        .split_whitespace()
        .next()
        .and_then(|part| part.strip_prefix("port="))
        .and_then(|port| port.parse::<u16>().ok())
    else {
        eprintln!("retained proxy parent socket: no diagnostic port");
        return false;
    };
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let parent_connected =
        match TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2)) {
            Ok(_) => {
                eprintln!("retained proxy parent socket: connected");
                true
            }
            Err(error) => {
                eprintln!(
                    "retained proxy parent socket: failed raw_os_error={:?}",
                    error.raw_os_error()
                );
                false
            }
        };
    output.status.success() && detail.ends_with(" connected") && parent_connected
}

/// Each sample owns a new [`Sandbox`] and therefore a distinct proxy listener. A failed sample
/// stays failed: this is an availability sample, not a retry loop that stops at its first green
/// result. The first child connection and the parent connection remain the paired controls from
/// [`retained_proxy_socket_probe`].
#[cfg(target_os = "macos")]
fn fresh_proxy_startup_sweep(policy: &SandboxPolicy) -> bool {
    const SAMPLES: usize = 32;
    let mut passed = 0;
    for sample in 1..=SAMPLES {
        let sample_passed = match Sandbox::new(policy) {
            Ok(sandbox) => retained_proxy_socket_probe(&sandbox),
            Err(error) => {
                eprintln!(
                    "fresh startup sample {sample:02}: acquisition failed, unavailable axes: {:?}",
                    error.lost
                );
                false
            }
        };
        eprintln!(
            "fresh startup sample {sample:02}: {}",
            if sample_passed { "PASS" } else { "FAIL" }
        );
        passed += if sample_passed { 1 } else { 0 };
    }
    println!("fresh proxy startup samples: {passed}/{SAMPLES} [want {SAMPLES}/{SAMPLES}]");
    passed == SAMPLES
}

/// A reachable listener alone is not a successful egress result. This deliberately omits the
/// session bearer, so the first proxy response must be the fixed 407 rejection before host policy
/// or any upstream connection is considered. It prevents a connection-only failure endpoint from
/// being recorded as a green request.
#[cfg(target_os = "macos")]
fn unauthenticated_proxy_cannot_read_green(sandbox: &Sandbox) -> bool {
    let script = r#"import os, socket, sys, time, urllib.parse
port = urllib.parse.urlsplit(os.environ["HTTPS_PROXY"]).port
try:
    with socket.create_connection(("127.0.0.1", port), timeout=2) as stream:
        stream.settimeout(2)
        stream.sendall(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n")
        deadline = time.monotonic() + 2
        reply = b""
        while b"\r\n" not in reply and len(reply) < 128:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("proxy response deadline")
            stream.settimeout(remaining)
            part = stream.recv(128 - len(reply))
            if not part:
                break
            reply += part
        reply = reply.split(b"\r\n", 1)[0]
except OSError as error:
    print(f"port={port} raw_os_error={error.errno}")
    sys.exit(1)
print(f"port={port} reply={reply.decode('ascii', 'replace')}")
sys.exit(0 if reply == b"HTTP/1.1 407 Proxy Authentication Required" else 1)"#;
    let prepared = match sandbox.prepare(CommandSpec::new("python3").args(["-c", script])) {
        Ok(prepared) => prepared,
        Err(error) => {
            eprintln!(
                "unauthenticated proxy control: preparation failed, unavailable axes: {:?}",
                error.lost
            );
            return false;
        }
    };
    let output = match prepared.output() {
        Ok(output) => output,
        Err(error) => {
            eprintln!(
                "unauthenticated proxy control: launch failed raw_os_error={:?}",
                error.raw_os_error()
            );
            return false;
        }
    };
    let detail = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    eprintln!(
        "unauthenticated proxy control: exited {} {}",
        output.status,
        if detail.is_empty() {
            "(no output)"
        } else {
            &detail
        }
    );
    output.status.success() && detail.ends_with("reply=HTTP/1.1 407 Proxy Authentication Required")
}

#[cfg(target_os = "macos")]
fn curl_in_session(label: &str, sandbox: &Sandbox, noproxy: bool, curl_args: &str) -> i32 {
    let curl = if noproxy {
        "env -u https_proxy -u HTTPS_PROXY -u http_proxy -u HTTP_PROXY -u all_proxy -u ALL_PROXY -u no_proxy -u NO_PROXY curl --noproxy '*'"
    } else {
        "curl"
    };
    let script =
        format!("{curl} -4 -sS -o /dev/null --connect-timeout 8 --max-time 20 {curl_args}");
    eprintln!(">>> {label}: retained sh -c {script:?}");
    let code = sandbox
        .prepare(CommandSpec::new("/bin/sh").arg("-c").arg(&script))
        .expect("prepare retained confined child")
        .status()
        .expect("run retained confined child")
        .code()
        .unwrap_or(-1);
    eprintln!("<<< {label}: exited {code}");
    code
}

#[cfg(target_os = "linux")]
fn loopback_probe(
    policy: &SandboxPolicy,
    bind: &str,
    url: &str,
    family: &str,
    relay: bool,
) -> bool {
    // A host-local service is a separate egress channel from the proxy. This listener is the
    // positive control: only reply with its canary after it has opened a TCP connection to the
    // denied host. A confined curl reaching it therefore proves a local relay can cross the
    // hostname policy; it is not merely a test that loopback sockets exist.
    let listener = TcpListener::bind(bind).expect("bind loopback probe control");
    let addr = listener.local_addr().expect("loopback probe address");
    let url = url.replace("{port}", &addr.port().to_string());
    let relay = thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .expect("make loopback control nonblocking");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut client = loop {
            match listener.accept() {
                Ok((client, _)) => break client,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return false;
                    }
                    thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("accept confined loopback request: {error}"),
            }
        };
        let mut request = [0_u8; 1024];
        let _ = client.read(&mut request);
        let upstream = relay
            && "www.google.com:443"
                .to_socket_addrs()
                .ok()
                .and_then(|mut addrs| addrs.next())
                .is_some_and(|addr| {
                    TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(8)).is_ok()
                });
        let body = if upstream {
            "RELAY_CANARY"
        } else if relay {
            "UPSTREAM_UNREACHABLE"
        } else {
            "LOCAL_CANARY"
        };
        write!(
            client,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("write loopback relay response");
        !relay || upstream
    });
    let client = curl_family("loopback-probe", policy, true, family, &url);
    let served = relay.join().expect("join loopback probe");
    client == 0 && served
}

#[cfg(target_os = "linux")]
fn run() -> bool {
    unsafe { std::env::set_var("NUB_SANDBOX_SUP_DEBUG", "1") };
    let allow = policy(json!({ "fs": true, "net": ["example.com"] }));
    // Non-cooperative throughout: the supervisor's transparent redirect is the only thing routing.
    let compat = curl("compat      ", &allow, true, "https://example.com/");
    let attack_deny = curl("attack-deny ", &allow, true, "https://www.google.com/");
    let attack_sni = curl(
        "attack-sni  ",
        &allow,
        true,
        "--connect-to www.google.com:443:example.com:443 https://www.google.com/",
    );
    let control_sni = curl(
        "control-sni ",
        &allow,
        true,
        "--connect-to example.com:443:example.com:443 https://example.com/",
    );
    let loopback_relay_v4 = loopback_probe(
        &allow,
        "127.0.0.1:0",
        "http://127.0.0.1:{port}/",
        "-4",
        true,
    );
    let loopback_relay_alias = loopback_probe(
        &allow,
        "127.0.0.2:0",
        "http://127.0.0.2:{port}/",
        "-4",
        true,
    );
    let loopback_relay_v6 = loopback_probe(&allow, "[::1]:0", "http://[::1]:{port}/", "-6", true);
    let explicit_v4 = policy(json!({ "fs": true, "net": ["127.0.0.1"] }));
    let explicit_v4_ok = loopback_probe(
        &explicit_v4,
        "127.0.0.1:0",
        "http://127.0.0.1:{port}/",
        "-4",
        false,
    );
    let explicit_cidr = policy(json!({ "fs": true, "net": ["127.0.0.0/8"] }));
    let explicit_cidr_ok = loopback_probe(
        &explicit_cidr,
        "127.0.0.2:0",
        "http://127.0.0.2:{port}/",
        "-4",
        false,
    );
    let explicit_v6 = policy(json!({ "fs": true, "net": ["::1"] }));
    let explicit_v6_ok =
        loopback_probe(&explicit_v6, "[::1]:0", "http://[::1]:{port}/", "-6", false);
    let localhost = policy(json!({ "fs": true, "net": ["localhost"] }));
    let localhost_is_distinct = !loopback_probe(
        &localhost,
        "127.0.0.1:0",
        "http://localhost:{port}/",
        "-4",
        false,
    );
    println!();
    println!("1 compat      (allow example.com, GET example.com)  -> exit={compat}   [want 0]");
    println!(
        "2 attack-deny (allow example.com, GET google)       -> exit={attack_deny}   [want != 0]"
    );
    println!(
        "3 attack-sni  (example.com IP, SNI=google)          -> exit={attack_sni}   [want != 0]"
    );
    println!(
        "4 control-sni (example.com IP, SNI=example.com)     -> exit={control_sni}   [want 0]"
    );
    println!(
        "5 loopback relay (denied google via 127.0.0.1)      -> reached={loopback_relay_v4} [want false]"
    );
    println!(
        "6 loopback relay (denied google via 127.0.0.2)      -> reached={loopback_relay_alias} [want false]"
    );
    println!(
        "7 loopback relay (denied google via ::1)            -> reached={loopback_relay_v6} [want false]"
    );
    println!(
        "8 explicit IP 127.0.0.1 local service            -> reached={explicit_v4_ok} [want true]"
    );
    println!(
        "9 explicit CIDR 127/8 local service               -> reached={explicit_cidr_ok} [want true]"
    );
    println!(
        "10 explicit IP ::1 local service                  -> reached={explicit_v6_ok} [want true]"
    );
    println!(
        "11 hostname localhost is distinct from 127.0.0.1  -> blocked={localhost_is_distinct} [want true]"
    );
    compat == 0
        && attack_deny != 0
        && attack_sni != 0
        && control_sni == 0
        && !loopback_relay_v4
        && !loopback_relay_alias
        && !loopback_relay_v6
        && explicit_v4_ok
        && explicit_cidr_ok
        && explicit_v6_ok
        && localhost_is_distinct
}

#[cfg(target_os = "macos")]
fn run() -> bool {
    let allow = policy(json!({ "fs": true, "net": ["example.com"] }));
    // Preserve the original one-shot public path. Its proxy begins on this first curl, so a
    // retained session below cannot pre-warm or hide a startup/lifetime failure here.
    let fresh_allow = curl("fresh-allow ", &allow, false, "https://example.com/");
    let fresh_deny = curl("fresh-deny  ", &allow, false, "https://www.google.com/");
    // Thirty-two independent sessions sample proxy startup without turning an individual failure
    // into a retry-to-green. Every sample's child is the first connection to its own proxy port.
    let fresh_startup = fresh_proxy_startup_sweep(&allow);
    // A reusable session starts one egress proxy. Keep it alive through a parent socket control,
    // a first confined socket control, and the allow/deny curls; a one-shot `apply` per curl
    // cannot distinguish a listener-lifetime fault from a distinct-session failure.
    let sandbox = Sandbox::new(&allow).expect("acquire retained host-filter session");
    let retained_proxy = retained_proxy_socket_probe(&sandbox);
    let unauthenticated_proxy = unauthenticated_proxy_cannot_read_green(&sandbox);
    // Cooperative (honors the injected https_proxy) — the proxy's per-host gate decides.
    let coop_allow = curl_in_session("coop-allow  ", &sandbox, false, "https://example.com/");
    let coop_deny = curl_in_session("coop-deny   ", &sandbox, false, "https://www.google.com/");
    // Non-cooperative (dials direct) — Seatbelt denies ALL direct egress, allowed or not. Names
    // fail at DNS (the resolver is off-limits too); the hardcoded-IP arm proves the block is at
    // connect, not merely resolution — a client that needs no DNS still cannot leave.
    let noncoop_deny = curl_in_session("noncoop-deny", &sandbox, true, "https://www.google.com/");
    let noncoop_allow = curl_in_session("noncoop-allw", &sandbox, true, "https://example.com/");
    let noncoop_ip = curl_in_session("noncoop-ip  ", &sandbox, true, "https://1.1.1.1/");
    println!();
    println!("0 retained proxy listener + Seatbelt socket -> passed={retained_proxy} [want true]");
    println!("1 fresh-allow  (one-shot proxy, GET example)  -> exit={fresh_allow}  [want 0]");
    println!("2 fresh-deny   (one-shot proxy, GET google)   -> exit={fresh_deny}   [want != 0]");
    println!(
        "3 fresh startup sweep (32 independent sessions)-> passed={fresh_startup} [want true]"
    );
    println!("4 tokenless proxy CONNECT rejects 407          -> passed={unauthenticated_proxy} [want true]");
    println!("5 coop-allow   (retained proxy, GET example)  -> exit={coop_allow}   [want 0]");
    println!("6 coop-deny    (retained proxy, GET google)   -> exit={coop_deny}   [want != 0]");
    println!("7 noncoop-deny (--noproxy, GET google)        -> exit={noncoop_deny}   [want != 0]");
    println!("8 noncoop-allw (--noproxy, GET example.com)   -> exit={noncoop_allow}   [want != 0]");
    println!("9 noncoop-ip   (--noproxy, GET 1.1.1.1)       -> exit={noncoop_ip}   [want != 0]");
    // 1/5 vs 2/6: the proxy's per-host gate works on both public lifecycles. 7/8/9 all blocked:
    // non-cooperative egress is denied regardless of host or DNS — never leaked.
    retained_proxy
        && fresh_allow == 0
        && fresh_deny != 0
        && fresh_startup
        && unauthenticated_proxy
        && coop_allow == 0
        && coop_deny != 0
        && noncoop_deny != 0
        && noncoop_allow != 0
        && noncoop_ip != 0
}

#[cfg(target_os = "windows")]
fn windows_curl(label: &str, policy: &SandboxPolicy, noproxy: bool, url: &str) -> i32 {
    let mut spec = CommandSpec::new(r"C:\Windows\System32\curl.exe")
        .args([
            "-4",
            "-sS",
            "--head",
            "--connect-timeout",
            "8",
            "--max-time",
            "20",
        ])
        .cwd(r"C:\Windows\System32");
    if noproxy {
        spec = spec.args(["--noproxy", "*"]);
    }
    spec = spec.arg(url);
    eprintln!(">>> {label}: curl {url} (noproxy={noproxy})");
    let code = apply(policy, spec)
        .expect("apply Windows host-filter policy with registered helper")
        .status()
        .expect("run confined Windows curl")
        .code()
        .unwrap_or(-1);
    eprintln!("<<< {label}: exited {code}");
    code
}

#[cfg(target_os = "windows")]
fn run() -> bool {
    // Mirror the nub-cli embedder: the helper is this co-package binary's hidden re-entry. The
    // same executable serves the proxy below, so this probes the actual registered-helper path
    // rather than the library's intentionally fail-closed unregistered state.
    nub_sandbox::set_windows_egress_helper_command(vec![
        std::env::current_exe()
            .expect("current proxy probe executable")
            .into_os_string(),
        "--windows-egress-helper".into(),
    ]);
    // Keep the filesystem axis relaxed so a failed curl cannot be misattributed to an absent file
    // grant. The network axis is still an AppContainer funnel: the command has no Internet
    // capability and can reach egress only through the same-SID helper's injected proxy.
    let allow = policy(json!({ "fs": true, "net": ["example.com"] }));
    let coop_allow = windows_curl("coop-allow  ", &allow, false, "https://example.com/");
    let coop_deny = windows_curl("coop-deny   ", &allow, false, "https://www.google.com/");
    // These deliberately ignore the helper-injected proxy environment. An allowlisted hostname
    // still must not direct-dial, and an IP literal has no host rule to admit it.
    let noncoop_allow = windows_curl("noncoop-allw", &allow, true, "https://example.com/");
    let noncoop_ip = windows_curl("noncoop-ip  ", &allow, true, "https://1.1.1.1/");
    println!();
    println!("1 coop-allow   (helper proxy, GET example.com)  -> exit={coop_allow}   [want 0]");
    println!("2 coop-deny    (helper proxy, GET google)       -> exit={coop_deny}   [want != 0]");
    println!(
        "3 noncoop-allw (--noproxy, GET example.com)    -> exit={noncoop_allow}   [want != 0]"
    );
    println!("4 noncoop-ip   (--noproxy, GET 1.1.1.1)         -> exit={noncoop_ip}   [want != 0]");
    coop_allow == 0 && coop_deny != 0 && noncoop_allow != 0 && noncoop_ip != 0
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn run() -> bool {
    eprintln!("per-host egress enforcement is unsupported on this OS");
    true
}

fn main() {
    #[cfg(target_os = "windows")]
    if std::env::args().nth(1).as_deref() == Some("--windows-egress-helper") {
        nub_sandbox::serve_windows_egress_helper();
    }
    let pass = run();
    println!("RESULT: {}", if pass { "PASS" } else { "FAIL" });
    std::process::exit(if pass { 0 } else { 1 });
}

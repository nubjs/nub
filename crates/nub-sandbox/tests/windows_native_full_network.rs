//! Focused, ordinary-user Windows gate for `fixtures/windows/native-full-network.cpp`.
//!
//! This test is ignored outside the source-controlled fixture-compilation workflow.  That
//! workflow supplies `NUB_WINDOWS_NATIVE_FULL_NETWORK_FIXTURE`, an absolute path to the
//! MSVC-built, hash-retained fixture binary; tests never compile native source at runtime.
#![cfg(windows)]

#[path = "common/tool_output.rs"]
mod tool_output;

use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::Duration;

const FIXTURE: &str = "NUB_WINDOWS_NATIVE_FULL_NETWORK_FIXTURE";
const REQUEST: &[u8] = b"nub-full-network-request";
const REPLY: &[u8] = b"nub-full-network-reply";
const DEADLINE: Duration = Duration::from_secs(5);
static SERIAL: Mutex<()> = Mutex::new(());
static DNS_NONCE: AtomicU64 = AtomicU64::new(0);

fn standard_user() {
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, SC_MANAGER_CREATE_SERVICE,
    };
    let manager = unsafe {
        OpenSCManagerW(
            std::ptr::null(),
            std::ptr::null(),
            SC_MANAGER_CREATE_SERVICE,
        )
    };
    if !manager.is_null() {
        unsafe { CloseServiceHandle(manager) };
        panic!("focused native-network fixture requires an ordinary Windows user");
    }
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(5),
        "ordinary-user oracle"
    );
    let relay = PathBuf::from(
        std::env::var_os("NUB_WINDOWS_RELAY_FIXTURE")
            .expect("focused gate must supply the standalone relay fixture"),
    );
    assert!(
        relay.is_absolute() && relay.is_file(),
        "invalid relay fixture: {relay:?}"
    );
    // Libtest's progress output would corrupt the helper's binary stdout transport.
    nub_sandbox::set_windows_egress_helper_command(vec![relay.into(), "--relay".into()]);
}

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os(FIXTURE).expect("focused gate must supply precompiled fixture path"),
    );
    assert!(
        path.is_file(),
        "fixture path is not a file: {}",
        path.display()
    );
    path
}

fn policy(
    root: &Path,
    fixture: &Path,
    net: serde_json::Value,
    endpoint: Option<&str>,
    dns_name: Option<&str>,
) -> nub_sandbox::SandboxPolicy {
    let project = root.join("project");
    let ambient: BTreeMap<String, String> = std::env::vars()
        .filter(|(key, _)| {
            ["PATH", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT"]
                .contains(&key.to_ascii_uppercase().as_str())
        })
        .collect();
    let ctx = CompileCtx::new(
        Homes {
            home: root.join("home"),
            cache: root.join("cache"),
            tmp: root.join("tmp"),
            project: project.clone(),
        },
        project.clone(),
        ScopeCapabilities::approved(),
        ambient.clone(),
    );
    let mut result = compile(
        &json!({"fs": {
        (project.to_string_lossy()): "rw", (fixture.to_string_lossy()): "r", "$tmp": "rw"
    }, "net": net}),
        &ctx,
    )
    .expect("fixture policy compiles");
    result.env.constructed = ambient;
    result.env.constructed.insert(
        "NUB_FULL_NETWORK_FS_CANARY".into(),
        root.join("withheld/canary").to_string_lossy().into_owned(),
    );
    if let Some(endpoint) = endpoint {
        result
            .env
            .constructed
            .insert("NUB_FULL_NETWORK_ENDPOINT".into(), endpoint.into());
    }
    if let Some(name) = dns_name {
        result
            .env
            .constructed
            .insert("NUB_FULL_NETWORK_DNS_NAME".into(), name.into());
    }
    result
}

fn command(root: &Path, fixture: &Path, case: &str) -> CommandSpec {
    CommandSpec::new(fixture)
        .cwd(root.join("project"))
        .args([case])
        .redact_stdout(true)
        .redact_stderr(true)
}

fn output(session: &Sandbox, root: &Path, fixture: &Path, case: &str) -> std::process::Output {
    let prepared = session
        .prepare(command(root, fixture, case))
        .expect("fixture prepares");
    assert!(prepared.degradation.is_full(), "{:?}", prepared.degradation);
    tool_output::output(prepared)
}

fn read_all(mut pipe: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes).expect("fixture output drains");
    bytes
}

fn plain_output(mut command: std::process::Command) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("plain fixture launches");
    let stdout = child.stdout.take().expect("plain fixture stdout is piped");
    let stderr = child.stderr.take().expect("plain fixture stderr is piped");
    let cancelled = AtomicBool::new(false);
    let (done, deadline) = mpsc::channel();
    std::thread::scope(|scope| {
        let cancelled = &cancelled;
        let timer = scope.spawn(move || {
            if deadline.recv_timeout(DEADLINE).is_err() {
                cancelled.store(true, Ordering::Release);
            }
        });
        let stdout = scope.spawn(move || read_all(stdout));
        let stderr = scope.spawn(move || read_all(stderr));
        let status = wait_plain_cancellable(&mut child, &cancelled);
        let _ = done.send(());
        timer.join().expect("plain fixture deadline thread joins");
        let stdout = stdout.join().expect("plain fixture stdout thread joins");
        let stderr = stderr.join().expect("plain fixture stderr thread joins");
        let status = status.unwrap_or_else(|error| {
            panic!(
                "plain fixture failed or exceeded its {DEADLINE:?} deadline: {error}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            )
        });
        Output {
            status,
            stdout,
            stderr,
        }
    })
}

fn wait_plain_cancellable(
    child: &mut Child,
    cancelled: &AtomicBool,
) -> std::io::Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if cancelled.load(Ordering::Acquire) {
            if let Err(error) = child.kill() {
                if child.try_wait()?.is_none() {
                    return Err(error);
                }
            }
            let _ = child.wait()?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "plain fixture deadline expired",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn plain_command(fixture: &Path, case: &str, endpoint: Option<&str>) -> std::process::Command {
    let mut command = std::process::Command::new(fixture);
    command.arg(case);
    if let Some(endpoint) = endpoint {
        command.env("NUB_FULL_NETWORK_ENDPOINT", endpoint);
    }
    command
}

fn assert_marker(output: &std::process::Output, name: &str, expected: &str) {
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.lines()
            .any(|line| line == format!("{name}={expected}")),
        "missing {name}={expected}: {text}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// The peer's internal socket deadlines make cleanup safe even if a fixture assertion unwinds.
struct Peer(Option<std::thread::JoinHandle<Result<(), String>>>);

impl Peer {
    fn join(mut self) {
        self.0
            .take()
            .expect("peer has not already joined")
            .join()
            .expect("peer thread joins")
            .unwrap_or_else(|error| panic!("peer oracle failed: {error}"));
    }
}

fn tcp_peer(listener: TcpListener, connections: usize) -> Peer {
    Peer(Some(std::thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("configuring TCP peer: {error}"))?;
        for connection in 0..connections {
            let deadline = std::time::Instant::now() + DEADLINE;
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() >= deadline {
                            return Err(format!(
                                "TCP peer timed out waiting for connection {connection}"
                            ));
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => return Err(format!("accepting TCP peer: {error}")),
                }
            };
            stream
                .set_read_timeout(Some(DEADLINE))
                .map_err(|error| error.to_string())?;
            stream
                .set_write_timeout(Some(DEADLINE))
                .map_err(|error| error.to_string())?;
            let mut request = [0; REQUEST.len()];
            stream
                .read_exact(&mut request)
                .map_err(|error| error.to_string())?;
            if request != REQUEST {
                return Err(format!("TCP request differed on connection {connection}"));
            }
            stream.write_all(REPLY).map_err(|error| error.to_string())?;
        }
        Ok(())
    })))
}

impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(peer) = self.0.take() {
            let _ = peer.join();
        }
    }
}

fn udp_peer(socket: UdpSocket) -> Peer {
    Peer(Some(std::thread::spawn(move || {
        socket
            .set_read_timeout(Some(DEADLINE))
            .map_err(|error| error.to_string())?;
        let mut request = [0; REQUEST.len()];
        let (length, peer) = socket
            .recv_from(&mut request)
            .map_err(|error| error.to_string())?;
        if &request[..length] != REQUEST {
            return Err("UDP request differed".to_owned());
        }
        socket
            .send_to(REPLY, peer)
            .map_err(|error| error.to_string())?;
        Ok(())
    })))
}

fn assert_positive_client(output: &Output, mode: &str, case: &str) {
    eprintln!("FULL_NETWORK_CLIENT mode={mode} case={case} output={output:?}");
    assert!(output.status.success(), "{mode} {case}: {output:?}");
    if case == "concurrent4" {
        assert_marker(output, "FULL_NETWORK_CONCURRENT", "12");
    } else {
        assert_marker(output, "FULL_NETWORK_PEER", "1");
    }
}

fn positive_client(root: &Path, fixture: &Path, case: &str, udp: bool, connections: usize) {
    let bind = if case.ends_with('6') {
        "[::1]:0"
    } else {
        "127.0.0.1:0"
    };
    if udp {
        let peer = UdpSocket::bind(bind).unwrap();
        let address = peer.local_addr().unwrap().to_string();
        let peer = udp_peer(peer);
        let result = plain_output(plain_command(fixture, case, Some(&address)));
        assert_positive_client(&result, "plain", case);
        peer.join();

        let peer = UdpSocket::bind(bind).unwrap();
        let address = peer.local_addr().unwrap().to_string();
        let policy = policy(root, fixture, json!(true), Some(&address), None);
        let session = Sandbox::with_windows_native_compat(&policy).unwrap();
        let peer = udp_peer(peer);
        let result = output(&session, root, fixture, case);
        assert_positive_client(&result, "native", case);
        peer.join();
    } else {
        let peer = TcpListener::bind(bind).unwrap();
        let address = peer.local_addr().unwrap().to_string();
        let peer = tcp_peer(peer, connections);
        let result = plain_output(plain_command(fixture, case, Some(&address)));
        assert_positive_client(&result, "plain", case);
        peer.join();

        let peer = TcpListener::bind(bind).unwrap();
        let address = peer.local_addr().unwrap().to_string();
        let policy = policy(root, fixture, json!(true), Some(&address), None);
        let session = Sandbox::with_windows_native_compat(&policy).unwrap();
        let peer = tcp_peer(peer, connections);
        let result = output(&session, root, fixture, case);
        assert_positive_client(&result, "native", case);
        peer.join();
        if case == "descendant4" {
            assert_token_attestation(&result);
        }
    }
}

fn negative_client(
    root: &Path,
    fixture: &Path,
    net: serde_json::Value,
    native: bool,
    net_label: &str,
) {
    let peer = TcpListener::bind("127.0.0.1:0").unwrap();
    peer.set_nonblocking(true).unwrap();
    let endpoint = peer.local_addr().unwrap().to_string();
    let policy = policy(root, fixture, net, Some(&endpoint), None);
    let session = if native {
        Sandbox::with_windows_native_compat(&policy)
    } else {
        Sandbox::new(&policy)
    }
    .expect("negative session");
    let token = output(&session, root, fixture, "token-report");
    assert!(token.status.success(), "{net_label} token query: {token:?}");
    assert_marker(
        &token,
        "FULL_NETWORK_TOKEN",
        "appcontainer=1:capabilities=0:internet-client=0:admin=0",
    );
    let fs = output(&session, root, fixture, "fs-canary");
    assert!(fs.status.success(), "{net_label} filesystem canary: {fs:?}");
    assert_marker(&fs, "FULL_NETWORK_FS_CANARY", "read=5:write=5");
    let result = output(&session, root, fixture, "tcp4");
    assert!(
        !result.status.success(),
        "{net_label} unexpectedly succeeded: {result:?}"
    );
    assert!(
        String::from_utf8_lossy(&result.stdout).contains("FULL_NETWORK_ROOT_BROKER_SOCKET="),
        "missing root diagnostic: {result:?}"
    );
    assert_marker(&result, "FULL_NETWORK_PEER", "0");
    match peer.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(error) => panic!("{net_label} peer oracle failed: {error}"),
        Ok(_) => panic!("{net_label} exposed peer traffic to the child"),
    }
}

enum ListenerChild {
    Plain(Child),
    Confined(nub_sandbox::PreparedChild),
}

impl ListenerChild {
    fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        match self {
            Self::Plain(child) => child.stdout.take(),
            Self::Confined(child) => child.take_stdout(),
        }
    }

    fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        match self {
            Self::Plain(child) => child.stderr.take(),
            Self::Confined(child) => child.take_stderr(),
        }
    }

    fn wait_cancellable(&mut self, cancelled: &AtomicBool) -> std::io::Result<ExitStatus> {
        match self {
            Self::Plain(child) => wait_plain_cancellable(child, cancelled),
            Self::Confined(child) => child.wait_cancellable(cancelled),
        }
    }
}

impl Drop for ListenerChild {
    fn drop(&mut self) {
        if let Self::Plain(child) = self {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn relay_listener_stdout(
    stdout: std::process::ChildStdout,
    lines: mpsc::Sender<String>,
) -> std::io::Result<Vec<u8>> {
    let mut stdout = BufReader::new(stdout);
    let mut bytes = Vec::new();
    loop {
        let mut line = Vec::new();
        let read = stdout.read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&line);
        let _ = lines.send(String::from_utf8_lossy(&line).trim().to_owned());
    }
}

fn receive_listener_line(
    ready: &mpsc::Receiver<String>,
    deadline: std::time::Instant,
    marker: &str,
) -> Result<String, String> {
    let remaining = deadline
        .checked_duration_since(std::time::Instant::now())
        .ok_or_else(|| format!("{marker} exceeded {DEADLINE:?}"))?;
    ready
        .recv_timeout(remaining)
        .map_err(|error| format!("{marker}: {error}"))
}

fn listener_output(mut child: ListenerChild) -> Output {
    let stdout = child.take_stdout().expect("listener stdout is piped");
    let stderr = child.take_stderr().expect("listener stderr is piped");
    let cancelled = AtomicBool::new(false);
    let (done, deadline) = mpsc::channel();
    let (lines, ready) = mpsc::channel();
    std::thread::scope(|scope| {
        let cancelled = &cancelled;
        let timer = scope.spawn(move || {
            if deadline.recv_timeout(DEADLINE).is_err() {
                cancelled.store(true, Ordering::Release);
            }
        });
        let stdout = scope.spawn(move || relay_listener_stdout(stdout, lines));
        let stderr = scope.spawn(move || read_all(stderr));
        let marker_deadline = std::time::Instant::now() + DEADLINE;
        let root = receive_listener_line(&ready, marker_deadline, "listener root marker");
        let address = root
            .and_then(|line| {
                (line == "FULL_NETWORK_ROOT_BROKER_SOCKET=ok")
                    .then_some(())
                    .ok_or_else(|| format!("listener root marker was {line:?}"))
            })
            .and_then(|()| receive_listener_line(&ready, marker_deadline, "listener ready marker"))
            .and_then(|line| {
                line.strip_prefix("FULL_NETWORK_READY=")
                    .map(str::to_owned)
                    .ok_or_else(|| format!("listener ready marker was {line:?}"))
            });
        let exchange = address.and_then(|address| {
            let address: SocketAddr = address
                .parse()
                .map_err(|error| format!("listener address {address:?}: {error}"))?;
            let mut peer = TcpStream::connect_timeout(&address, DEADLINE)
                .map_err(|error| format!("owned listener connect {address}: {error}"))?;
            peer.set_read_timeout(Some(DEADLINE))
                .map_err(|error| error.to_string())?;
            peer.set_write_timeout(Some(DEADLINE))
                .map_err(|error| error.to_string())?;
            peer.write_all(REQUEST).map_err(|error| error.to_string())?;
            let mut reply = [0; REPLY.len()];
            peer.read_exact(&mut reply)
                .map_err(|error| error.to_string())?;
            (reply == REPLY)
                .then_some(())
                .ok_or_else(|| "listener reply bytes differed".to_owned())
        });
        if exchange.is_err() {
            cancelled.store(true, Ordering::Release);
        }
        let status = child.wait_cancellable(&cancelled);
        drop(child);
        let _ = done.send(());
        timer.join().expect("listener deadline thread joins");
        let stdout = stdout
            .join()
            .expect("listener stdout thread joins")
            .expect("listener stdout drains");
        let stderr = stderr.join().expect("listener stderr thread joins");
        let status = status.unwrap_or_else(|error| {
            panic!(
                "listener failed or exceeded its {DEADLINE:?} deadline: {error}; exchange={exchange:?}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            )
        });
        exchange.unwrap_or_else(|error| {
            panic!(
                "listener exchange failed: {error}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            )
        });
        Output {
            status,
            stdout,
            stderr,
        }
    })
}

fn owner_listener_address(mut child: nub_sandbox::PreparedChild) -> String {
    let stdout = child.take_stdout().expect("owner listener stdout is piped");
    let stderr = child.take_stderr().expect("owner listener stderr is piped");
    let (lines, ready) = mpsc::channel();
    std::thread::scope(|scope| {
        let stdout = scope.spawn(move || relay_listener_stdout(stdout, lines));
        let stderr = scope.spawn(move || read_all(stderr));
        let marker_deadline = std::time::Instant::now() + DEADLINE;
        let result = receive_listener_line(&ready, marker_deadline, "owner root marker")
            .and_then(|line| {
                (line == "FULL_NETWORK_ROOT_BROKER_SOCKET=ok")
                    .then_some(())
                    .ok_or_else(|| format!("owner root marker was {line:?}"))
            })
            .and_then(|()| receive_listener_line(&ready, marker_deadline, "owner ready marker"))
            .and_then(|line| {
                line.strip_prefix("FULL_NETWORK_READY=")
                    .map(str::to_owned)
                    .ok_or_else(|| format!("owner ready marker was {line:?}"))
            })
            .and_then(|address| {
                // Restricted networking can block a connection even while a
                // listener is alive. Prove that it holds the port before drop.
                match TcpListener::bind(address.as_str()) {
                    Err(_) => Ok(address),
                    Ok(_) => Err(format!("owner listener did not reserve {address}")),
                }
            });
        drop(child);
        let stdout = stdout
            .join()
            .expect("owner listener stdout thread joins")
            .expect("owner listener stdout drains");
        let stderr = stderr.join().expect("owner listener stderr thread joins");
        result.unwrap_or_else(|error| {
            panic!(
                "owner listener did not become ready: {error}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            )
        })
    })
}

fn listener_case(root: &Path, fixture: &Path, case: &str) {
    let plain = ListenerChild::Plain(
        plain_command(fixture, case, None)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("plain listener launches"),
    );
    let plain = listener_output(plain);
    assert!(
        plain.status.success(),
        "plain {case} listener failed: {plain:?}"
    );
    assert_marker(&plain, "FULL_NETWORK_PEER", "1");

    let policy = policy(root, fixture, json!(true), None, None);
    let session = Sandbox::with_windows_native_compat(&policy).unwrap();
    let prepared = session
        .prepare(command(root, fixture, case))
        .expect("listener prepares");
    let native = listener_output(ListenerChild::Confined(
        prepared.spawn().expect("listener launches"),
    ));
    assert!(
        native.status.success(),
        "native {case} listener failed: {native:?}"
    );
    assert_marker(&native, "FULL_NETWORK_PEER", "1");
}

fn fresh_dns_name(api: &str, mode: &str) -> String {
    let nonce = DNS_NONCE.fetch_add(1, Ordering::Relaxed)
        ^ std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos() as u64;
    let label = format!("nub-{api}-{mode}-{nonce:016x}");
    assert!(label.len() <= 63, "DNS label exceeds 63 bytes: {label}");
    format!("{label}.1.1.1.1.sslip.io")
}

fn assert_token_attestation(output: &Output) {
    assert_marker(
        output,
        "FULL_NETWORK_TOKEN",
        "appcontainer=1:capabilities=1:internet-client=1:admin=0",
    );
}

#[test]
#[ignore = "requires NUB_WINDOWS_NATIVE_FULL_NETWORK_DNS_OPT_IN=1 and the precompiled focused fixture"]
fn native_adapter_full_network_dns_opt_in() {
    assert_eq!(
        std::env::var_os("NUB_WINDOWS_NATIVE_FULL_NETWORK_DNS_OPT_IN").as_deref(),
        Some(std::ffi::OsStr::new("1")),
        "this ignored DNS gate must be explicitly enabled rather than silently skipped"
    );
    let _serial = SERIAL.lock().unwrap();
    standard_user();
    let fixture = fixture_path();
    let root = tempfile::Builder::new()
        .prefix("nub-native-network-dns-")
        .tempdir_in(std::env::var_os("USERPROFILE").unwrap())
        .unwrap();
    for name in ["project", "home", "cache", "tmp", "withheld"] {
        std::fs::create_dir(root.path().join(name)).unwrap();
    }
    std::fs::write(root.path().join("withheld/canary"), b"withheld").unwrap();
    let mut results = Vec::new();
    let mut denied_results = Vec::new();
    for api in ["getaddrinfo", "dnsqueryex"] {
        let plain_name = fresh_dns_name(api, "plain");
        let mut plain_command = plain_command(&fixture, api, None);
        plain_command.env("NUB_FULL_NETWORK_DNS_NAME", &plain_name);
        let plain = plain_output(plain_command);
        let native_name = fresh_dns_name(api, "native");
        let native_policy = policy(root.path(), &fixture, json!(true), None, Some(&native_name));
        let native = Sandbox::with_windows_native_compat(&native_policy)
            .expect("native full-network DNS session");
        let adapted = output(&native, root.path(), &fixture, api);
        eprintln!("FULL_NETWORK_DNS_RESULT api={api} plain={plain:?} native={adapted:?}");
        results.push((api, plain_name, native_name, plain, adapted));
        for (mode, net, native) in [
            ("raw-deny", json!(false), false),
            ("native-deny", json!(false), true),
            ("native-host", json!(["example.com"]), true),
        ] {
            let denied_policy = policy(
                root.path(),
                &fixture,
                net,
                None,
                Some(&fresh_dns_name(api, mode)),
            );
            let denied = if native {
                Sandbox::with_windows_native_compat(&denied_policy)
            } else {
                Sandbox::new(&denied_policy)
            }
            .expect("restricted DNS session");
            let token = output(&denied, root.path(), &fixture, "token-report");
            let observed = output(&denied, root.path(), &fixture, api);
            eprintln!(
                "FULL_NETWORK_DNS_DENIED api={api} mode={mode} token={token:?} result={observed:?}"
            );
            denied_results.push((api, mode, token, observed));
        }
    }
    // Run both APIs before assertions so one failure does not hide the other.
    for (api, plain_name, native_name, plain, adapted) in results {
        assert!(
            plain.status.success(),
            "plain {api} setup/provider failure for {plain_name}: {plain:?}"
        );
        assert_marker(&plain, "FULL_NETWORK_DNS", &format!("{api}:0:1.1.1.1"));
        assert!(
            adapted.status.success(),
            "adapter {api} failed for {native_name}: {adapted:?}"
        );
        assert_marker(&adapted, "FULL_NETWORK_DNS", &format!("{api}:0:1.1.1.1"));
    }
    for (api, mode, token, observed) in denied_results {
        assert!(
            token.status.success(),
            "{api} {mode} token query: {token:?}"
        );
        assert_marker(
            &token,
            "FULL_NETWORK_TOKEN",
            "appcontainer=1:capabilities=0:internet-client=0:admin=0",
        );
        assert!(
            !observed.status.success(),
            "{api} {mode} resolved a fresh disallowed hostname: {observed:?}"
        );
        assert!(
            String::from_utf8_lossy(&observed.stdout).contains(&format!("FULL_NETWORK_DNS={api}:")),
            "{api} {mode} did not reach the resolver: {observed:?}"
        );
    }
}

#[test]
#[ignore = "requires the source-controlled MSVC native-full-network fixture and an ordinary Windows user"]
fn native_adapter_drop_reaps_pending_listener_and_closes_port() {
    owner_drop_case(json!(true), "full-network");
}

#[test]
#[ignore = "requires the source-controlled MSVC native-full-network fixture and an ordinary Windows user"]
fn native_adapter_drop_reaps_pending_broker_session_and_closes_port() {
    // A hostname allow derives the capability-free parent-owned egress broker.
    // Dropping the only command must still tear down that broker session while
    // reaping the pending child listener; no private helper handle is inspected.
    owner_drop_case(json!(["allowed.invalid"]), "hostname-broker");
}

fn owner_drop_case(net: serde_json::Value, label: &str) {
    let _serial = SERIAL.lock().unwrap();
    standard_user();
    let fixture = fixture_path();
    let root = tempfile::Builder::new()
        .prefix("nub-native-network-owner-")
        .tempdir_in(std::env::var_os("USERPROFILE").unwrap())
        .unwrap();
    for name in ["project", "home", "cache", "tmp", "withheld"] {
        std::fs::create_dir(root.path().join(name)).unwrap();
    }
    std::fs::write(root.path().join("withheld/canary"), b"withheld").unwrap();
    let policy = policy(root.path(), &fixture, net, None, None);
    let session = Sandbox::with_windows_native_compat(&policy).unwrap();
    let child = session
        .prepare(command(root.path(), &fixture, "owner-hold4"))
        .unwrap()
        .spawn()
        .unwrap();
    let address = owner_listener_address(child);
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        TcpStream::connect_timeout(&address.parse().unwrap(), DEADLINE).is_err(),
        "dropped {label} command owner left listener reachable"
    );
    let _rebound = TcpListener::bind(address.as_str())
        .unwrap_or_else(|error| panic!("dropped {label} owner retained port {address}: {error}"));
}

#[test]
#[ignore = "requires the source-controlled MSVC native-full-network fixture and an ordinary Windows user"]
fn native_adapter_full_network_has_peer_oracles_and_retained_policy_separation() {
    let _serial = SERIAL.lock().unwrap();
    standard_user();
    let fixture = fixture_path();
    let root = tempfile::Builder::new()
        .prefix("nub-native-full-network-")
        .tempdir_in(std::env::var_os("USERPROFILE").unwrap())
        .unwrap();
    for name in ["project", "home", "cache", "tmp", "withheld"] {
        std::fs::create_dir(root.path().join(name)).unwrap();
    }
    std::fs::write(root.path().join("withheld/canary"), b"withheld").unwrap();

    // Plain proves the fixture and withheld path controls themselves are meaningful.
    let mut plain_command = plain_command(&fixture, "fs-canary", None);
    plain_command.env(
        "NUB_FULL_NETWORK_FS_CANARY",
        root.path().join("withheld/canary"),
    );
    let plain = plain_output(plain_command);
    assert!(!plain.status.success());
    assert_marker(&plain, "FULL_NETWORK_FS_CANARY", "read=0:write=0");

    let token_policy = policy(root.path(), &fixture, json!(true), None, None);
    let token = Sandbox::with_windows_native_compat(&token_policy).unwrap();
    let token = output(&token, root.path(), &fixture, "token-attest");
    assert!(
        token.status.success(),
        "native root token attestation failed: {token:?}"
    );
    assert_token_attestation(&token);

    positive_client(root.path(), &fixture, "tcp4", false, 1);
    positive_client(root.path(), &fixture, "tcp6", false, 1);
    positive_client(root.path(), &fixture, "udp4", true, 1);
    positive_client(root.path(), &fixture, "udp6", true, 1);
    positive_client(root.path(), &fixture, "connectex4", false, 1);
    positive_client(root.path(), &fixture, "concurrent4", false, 12);
    positive_client(root.path(), &fixture, "descendant4", false, 1);
    listener_case(root.path(), &fixture, "listen4");
    listener_case(root.path(), &fixture, "listen6");
    listener_case(root.path(), &fixture, "acceptex4");
    let fs_policy = policy(root.path(), &fixture, json!(true), None, None);
    let native = Sandbox::with_windows_native_compat(&fs_policy).unwrap();
    let fs = output(&native, root.path(), &fixture, "fs-canary");
    assert!(fs.status.success());
    assert_marker(&fs, "FULL_NETWORK_FS_CANARY", "read=5:write=5");
    drop(native);

    // These run after the positive lease with identical literal fs grants, targeting profile-key aliasing.
    negative_client(root.path(), &fixture, json!(false), false, "raw net:false");
    negative_client(
        root.path(),
        &fixture,
        json!(false),
        true,
        "native net:false",
    );
    negative_client(
        root.path(),
        &fixture,
        json!(["allowed.invalid"]),
        true,
        "hostname-restricted native",
    );
}

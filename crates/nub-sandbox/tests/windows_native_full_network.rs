//! Focused, ordinary-user Windows gate for `fixtures/windows/native-full-network.cpp`.
//!
//! This test is ignored outside the source-controlled fixture-compilation workflow.  That
//! workflow supplies `NUB_WINDOWS_NATIVE_FULL_NETWORK_FIXTURE`, an absolute path to the
//! MSVC-built, hash-retained fixture binary; tests never compile native source at runtime.
#![cfg(windows)]

use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const FIXTURE: &str = "NUB_WINDOWS_NATIVE_FULL_NETWORK_FIXTURE";
const REQUEST: &[u8] = b"nub-full-network-request";
const REPLY: &[u8] = b"nub-full-network-reply";
const DEADLINE: Duration = Duration::from_secs(5);
static SERIAL: Mutex<()> = Mutex::new(());

fn standard_user() {
    use windows_sys::Win32::System::Services::{CloseServiceHandle, OpenSCManagerW, SC_MANAGER_CREATE_SERVICE};
    let manager = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CREATE_SERVICE) };
    if !manager.is_null() {
        unsafe { CloseServiceHandle(manager) };
        panic!("focused native-network fixture requires an ordinary Windows user");
    }
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(5), "ordinary-user oracle");
}

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(std::env::var_os(FIXTURE).expect("focused gate must supply precompiled fixture path"));
    assert!(path.is_file(), "fixture path is not a file: {}", path.display());
    path
}

fn policy(root: &Path, fixture: &Path, net: serde_json::Value, endpoint: Option<&str>) -> nub_sandbox::SandboxPolicy {
    let project = root.join("project");
    let ambient: BTreeMap<String, String> = std::env::vars().filter(|(key, _)| {
        ["PATH", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT"].contains(&key.to_ascii_uppercase().as_str())
    }).collect();
    let ctx = CompileCtx::new(Homes { home: root.join("home"), cache: root.join("cache"), tmp: root.join("tmp"), project: project.clone() }, project.clone(), ScopeCapabilities::approved(), ambient.clone());
    let mut result = compile(&json!({"fs": {
        (project.to_string_lossy()): "rw", (fixture.to_string_lossy()): "r", "$tmp": "rw"
    }, "net": net}), &ctx).expect("fixture policy compiles");
    result.env.constructed = ambient;
    result.env.constructed.insert("NUB_FULL_NETWORK_FS_CANARY".into(), root.join("withheld/canary").to_string_lossy().into_owned());
    if let Some(endpoint) = endpoint { result.env.constructed.insert("NUB_FULL_NETWORK_ENDPOINT".into(), endpoint.into()); }
    result
}

fn command(fixture: &Path, case: &str) -> CommandSpec {
    CommandSpec::new(fixture).args([case]).redact_stdout(true).redact_stderr(true)
}

fn output(session: &Sandbox, fixture: &Path, case: &str) -> std::process::Output {
    let prepared = session.prepare(command(fixture, case)).expect("fixture prepares");
    assert!(prepared.degradation.is_full(), "{:?}", prepared.degradation);
    prepared.spawn().expect("fixture launches").wait_with_output().expect("fixture reaps")
}

fn assert_marker(output: &std::process::Output, name: &str, expected: &str) {
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.lines().any(|line| line == format!("{name}={expected}")), "missing {name}={expected}: {text}\nstderr: {}", String::from_utf8_lossy(&output.stderr));
}

fn tcp_peer(listener: TcpListener, connections: usize) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || for _ in 0..connections {
        let (mut stream, _) = listener.accept().expect("peer accept");
        stream.set_read_timeout(Some(DEADLINE)).unwrap();
        stream.set_write_timeout(Some(DEADLINE)).unwrap();
        let mut request = [0; REQUEST.len()]; stream.read_exact(&mut request).unwrap(); assert_eq!(request, REQUEST);
        stream.write_all(REPLY).unwrap();
    })
}

fn udp_peer(socket: UdpSocket) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        socket.set_read_timeout(Some(DEADLINE)).unwrap();
        let mut request = [0; REQUEST.len()]; let (length, peer) = socket.recv_from(&mut request).expect("peer datagram");
        assert_eq!(&request[..length], REQUEST); socket.send_to(REPLY, peer).unwrap();
    })
}

fn positive_client(root: &Path, fixture: &Path, case: &str, udp: bool, connections: usize) {
    if udp {
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap(); let address = peer.local_addr().unwrap().to_string();
        let policy = policy(root, fixture, json!(true), Some(&address)); let session = Sandbox::with_windows_native_compat(&policy).unwrap();
        let peer = udp_peer(peer); let result = output(&session, fixture, case); peer.join().unwrap();
        assert!(result.status.success(), "{result:?}"); assert_marker(&result, "FULL_NETWORK_PEER", "1");
    } else {
        let peer = TcpListener::bind("127.0.0.1:0").unwrap(); let address = peer.local_addr().unwrap().to_string();
        let policy = policy(root, fixture, json!(true), Some(&address)); let session = Sandbox::with_windows_native_compat(&policy).unwrap();
        let peer = tcp_peer(peer, connections); let result = output(&session, fixture, case); peer.join().unwrap();
        assert!(result.status.success(), "{result:?}"); assert_marker(&result, "FULL_NETWORK_PEER", "1");
    }
}

fn negative_client(root: &Path, fixture: &Path, net: serde_json::Value, native: bool, net_label: &str) {
    let peer = TcpListener::bind("127.0.0.1:0").unwrap(); peer.set_nonblocking(true).unwrap();
    let endpoint = peer.local_addr().unwrap().to_string(); let policy = policy(root, fixture, net, Some(&endpoint));
    let session = if native { Sandbox::with_windows_native_compat(&policy) } else { Sandbox::new(&policy) }.expect("negative session");
    let result = output(&session, fixture, "tcp4");
    assert!(!result.status.success(), "{net_label} unexpectedly succeeded: {result:?}");
    if native { assert_marker(&result, "FULL_NETWORK_ROOT_BROKER_SOCKET", "failed:10013"); } else { assert_marker(&result, "FULL_NETWORK_PEER", "0"); }
    assert!(peer.accept().is_err(), "{net_label} exposed a broad socket to the child");
}

fn listener_case(root: &Path, fixture: &Path, case: &str) {
    let policy = policy(root, fixture, json!(true), None); let session = Sandbox::with_windows_native_compat(&policy).unwrap();
    let prepared = session.prepare(command(fixture, case)).expect("listener prepares");
    let mut child = prepared.spawn().expect("listener launches");
    let stdout = child.take_stdout().expect("listener stdout");
    let stderr = child.take_stderr().expect("listener stderr");
    let stderr = std::thread::spawn(move || { let mut bytes = Vec::new(); BufReader::new(stderr).read_to_end(&mut bytes).unwrap(); bytes });
    let mut stdout = BufReader::new(stdout); let mut line = String::new(); stdout.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "FULL_NETWORK_ROOT_BROKER_SOCKET=ok", "listener root marker");
    line.clear(); stdout.read_line(&mut line).unwrap(); let address = line.trim().strip_prefix("FULL_NETWORK_READY=").expect("listener ready marker");
    let mut peer = TcpStream::connect(address).expect("owned listener connect"); peer.set_read_timeout(Some(DEADLINE)).unwrap();
    peer.write_all(REQUEST).unwrap(); let mut reply = [0; REPLY.len()]; peer.read_exact(&mut reply).unwrap(); assert_eq!(reply, REPLY);
    let mut rest = Vec::new(); stdout.read_to_end(&mut rest).unwrap(); let status = child.wait().unwrap();
    assert!(status.success(), "listener failed: {}\nstderr: {}", String::from_utf8_lossy(&rest), String::from_utf8_lossy(&stderr.join().unwrap()));
}

#[test]
#[ignore = "requires the source-controlled MSVC native-full-network fixture and an ordinary Windows user"]
fn native_adapter_full_network_has_peer_oracles_and_retained_policy_separation() {
    let _serial = SERIAL.lock().unwrap(); standard_user();
    let fixture = fixture_path(); let root = tempfile::Builder::new().prefix("nub-native-full-network-").tempdir_in(std::env::var_os("USERPROFILE").unwrap()).unwrap();
    for name in ["project", "home", "cache", "tmp", "withheld"] { std::fs::create_dir(root.path().join(name)).unwrap(); }
    std::fs::write(root.path().join("withheld/canary"), b"withheld").unwrap();

    // Plain proves the fixture and withheld path controls themselves are meaningful.
    let plain = std::process::Command::new(&fixture).arg("fs-canary").env("NUB_FULL_NETWORK_FS_CANARY", root.path().join("withheld/canary")).output().unwrap();
    assert!(!plain.status.success()); assert_marker(&plain, "FULL_NETWORK_FS_CANARY", "read=0:write=0");

    positive_client(root.path(), &fixture, "tcp4", false, 1); positive_client(root.path(), &fixture, "udp4", true, 1);
    positive_client(root.path(), &fixture, "connectex4", false, 1); positive_client(root.path(), &fixture, "concurrent4", false, 12);
    positive_client(root.path(), &fixture, "descendant4", false, 1); listener_case(root.path(), &fixture, "listen4"); listener_case(root.path(), &fixture, "acceptex4");
    let fs_policy = policy(root.path(), &fixture, json!(true), None); let native = Sandbox::with_windows_native_compat(&fs_policy).unwrap(); let fs = output(&native, &fixture, "fs-canary"); assert!(!fs.status.success()); assert_marker(&fs, "FULL_NETWORK_FS_CANARY", "read=5:write=5"); drop(native);

    // These run after the positive lease with identical literal fs grants, targeting profile-key aliasing.
    negative_client(root.path(), &fixture, json!(false), false, "raw net:false");
    negative_client(root.path(), &fixture, json!(["allowed.invalid"]), true, "hostname-restricted native");
}

//! Public-session controls for Windows confinement and persistent resource reuse.
#![cfg(windows)]

#[path = "common/tool_output.rs"]
mod tool_output;

use nub_sandbox::{
    CommandSpec, CompileCtx, Homes, Sandbox, SandboxPolicy, ScopeCapabilities, compile,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_APPCONTAINER_INFORMATION, TOKEN_QUERY, TOKEN_USER,
    TokenAppContainerSid, TokenUser,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_DUP_HANDLE,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_VM_READ, WaitForSingleObject,
};

const CHILD: &str = "production_windows_child";
const MODE: &str = "SANDBOX_PRODUCTION_MODE";
const CONFIG: &str = "SANDBOX_PRODUCTION_CONFIG";
const MARKER: &str = "WINDOWS_PRODUCTION_RESULT ";
const DEADLINE: Duration = Duration::from_secs(30);
static SERIAL: Mutex<()> = Mutex::new(());

fn sid(package: bool) -> Option<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    let mut token = std::ptr::null_mut();
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0
    );
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut buffer = [0usize; 256];
    let mut length = 0;
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                if package {
                    TokenAppContainerSid
                } else {
                    TokenUser
                },
                buffer.as_mut_ptr().cast(),
                std::mem::size_of_val(&buffer) as u32,
                &mut length,
            )
        },
        0
    );
    let sid = unsafe {
        if package {
            (*buffer.as_ptr().cast::<TOKEN_APPCONTAINER_INFORMATION>()).TokenAppContainer
        } else {
            (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid
        }
    };
    if sid.is_null() {
        return None;
    }
    let mut text = std::ptr::null_mut();
    assert_ne!(unsafe { ConvertSidToStringSidW(sid, &mut text) }, 0);
    let mut length = 0;
    unsafe {
        while *text.add(length) != 0 {
            length += 1;
        }
    }
    let result = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
    unsafe { LocalFree(text.cast()) };
    Some(result)
}

fn unprivileged() {
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, SC_MANAGER_CREATE_SERVICE,
    };
    assert_ne!(
        sid(false).as_deref(),
        Some("S-1-5-18"),
        "SYSTEM is not a valid oracle"
    );
    let manager = unsafe {
        OpenSCManagerW(
            std::ptr::null(),
            std::ptr::null(),
            SC_MANAGER_CREATE_SERVICE,
        )
    };
    if !manager.is_null() {
        unsafe { CloseServiceHandle(manager) };
        panic!("run these controls as a non-administrative Windows user");
    }
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(5),
        "administrative-access oracle failed for a reason other than access denial"
    );
}

fn fixture() -> tempfile::TempDir {
    unprivileged();
    let root = tempfile::Builder::new()
        .prefix("sandbox-production-")
        .tempdir_in(std::env::var_os("USERPROFILE").expect("runner home"))
        .unwrap();
    for name in ["project", "readable", "omitted", "cache", "tmp"] {
        std::fs::create_dir(root.path().join(name)).unwrap();
    }
    std::fs::write(root.path().join("omitted/canary"), b"outside-canary").unwrap();
    std::fs::write(root.path().join("readable/input"), b"readable-canary").unwrap();
    root
}

fn policy(root: &Path, project: &Path, mode: &str, config: Value) -> SandboxPolicy {
    let environment: BTreeMap<String, String> = std::env::vars()
        .filter(|(key, _)| {
            ["PATH", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT"]
                .contains(&key.to_ascii_uppercase().as_str())
        })
        .collect();
    let ctx = CompileCtx::new(
        Homes {
            home: root.join("omitted"),
            cache: root.join("cache"),
            tmp: root.join("tmp"),
            project: project.into(),
        },
        project.into(),
        ScopeCapabilities::approved(),
        environment.clone(),
    );
    let mut result = compile(
        &json!({"fs": {
        (project.to_string_lossy()): "rw",
        (root.join("readable").to_string_lossy()): "r",
        (std::env::current_exe().unwrap().to_string_lossy()): "r",
        "$tmp": "rw"
    }, "net": false}),
        &ctx,
    )
    .unwrap();
    result.env.constructed = environment;
    result.env.constructed.insert(MODE.into(), mode.into());
    result
        .env
        .constructed
        .insert(CONFIG.into(), config.to_string());
    result
}

fn config(root: &Path) -> Value {
    json!({"root": root, "host": std::process::id()})
}

fn session(policy: &SandboxPolicy, native: bool) -> Sandbox {
    if native {
        Sandbox::with_windows_native_compat(policy)
    } else {
        Sandbox::acquire(policy)
    }
    .unwrap()
}

fn command(project: &Path) -> CommandSpec {
    CommandSpec::new(std::env::current_exe().unwrap())
        .args(["--exact", CHILD, "--nocapture"])
        .cwd(project)
        .redact_stdout(true)
        .redact_stderr(true)
}

fn output(session: &Sandbox, project: &Path) -> Value {
    let prepared = session.prepare(command(project)).unwrap();
    assert!(prepared.degradation.is_full(), "{:?}", prepared.degradation);
    record(&tool_output::output(prepared))
}

fn record(output: &Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once(MARKER))
        .map(|(_, text)| serde_json::from_str(text).unwrap())
        .next_back()
        .unwrap_or_else(|| panic!("missing fixture result: {output:?}"))
}

struct Owner(Child);
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn plain(policy: &SandboxPolicy, project: &Path) -> Value {
    let mut child = Owner(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD, "--nocapture"])
            .env_clear()
            .envs(&policy.env.constructed)
            .current_dir(project)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let stderr = child.0.stderr.take().unwrap();
    std::thread::scope(|scope| {
        let drain = |mut pipe: Box<dyn Read + Send>| {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).unwrap();
            bytes
        };
        let stdout = scope.spawn(move || drain(Box::new(stdout)));
        let stderr = scope.spawn(move || drain(Box::new(stderr)));
        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            if start.elapsed() >= DEADLINE {
                child.0.kill().unwrap();
                let _ = child.0.wait();
                panic!("plain fixture exceeded deadline");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        record(&Output {
            status,
            stdout: stdout.join().unwrap(),
            stderr: stderr.join().unwrap(),
        })
    })
}

fn opens_process(pid: u32, access: u32) -> bool {
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        return false;
    }
    drop(unsafe { OwnedHandle::from_raw_handle(handle) });
    true
}

fn loaded_adapter() -> Option<PathBuf> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
    for name in ["compat-x64.dll", "compat-arm64.dll"] {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let handle = unsafe { GetModuleHandleW(wide.as_ptr()) };
        if handle.is_null() {
            continue;
        }
        let mut path = vec![0u16; 32768];
        let length =
            unsafe { GetModuleFileNameW(handle, path.as_mut_ptr(), path.len() as u32) } as usize;
        assert!(length > 0 && length < path.len());
        return Some(PathBuf::from(std::ffi::OsString::from_wide(
            &path[..length],
        )));
    }
    None
}

fn probe(config: &Value) -> Value {
    let root = Path::new(config["root"].as_str().unwrap());
    let host = config["host"].as_u64().unwrap() as u32;
    let adapter = loaded_adapter();
    let assets = adapter.as_ref().map(|path| {
        let directory = path.parent().unwrap();
        json!({
            "read": std::fs::read(path).is_ok(),
            "write": OpenOptions::new().write(true).open(path).is_ok(),
            "create": OpenOptions::new().write(true).create_new(true).open(directory.join("fixture-tamper")).is_ok(),
            "journal": File::open(directory.parent().unwrap().join("registry.json")).is_ok()
        })
    });
    json!({
        "sid": sid(true), "tmp": std::env::temp_dir(),
        "value": std::env::var("SANDBOX_COMMAND_VALUE").ok(),
        "ambient": std::env::var("SANDBOX_AMBIENT_ONLY").ok(),
        "readable": std::fs::read(root.join("readable/input")).ok(),
        "outside_read": File::open(root.join("omitted/canary")).is_ok(),
        "outside_write": OpenOptions::new().write(true).open(root.join("omitted/canary")).is_ok(),
        "host_read": opens_process(host, PROCESS_VM_READ),
        "host_duplicate": opens_process(host, PROCESS_DUP_HANDLE),
        "adapter": adapter, "assets": assets
    })
}

#[test]
fn production_windows_child() {
    let Ok(mode) = std::env::var(MODE) else {
        return;
    };
    let config: Value = serde_json::from_str(&std::env::var(CONFIG).unwrap()).unwrap();
    let result = match mode.as_str() {
        "report" => probe(&config),
        "nested" => {
            let mut child = Owner(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", CHILD, "--nocapture"])
                    .env(MODE, "report")
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let stdout = child.0.stdout.take().unwrap();
            let stderr = child.0.stderr.take().unwrap();
            let output = std::thread::scope(|scope| {
                let a = scope.spawn(|| {
                    let mut bytes = Vec::new();
                    std::io::BufReader::new(stdout)
                        .read_to_end(&mut bytes)
                        .unwrap();
                    bytes
                });
                let b = scope.spawn(|| {
                    let mut bytes = Vec::new();
                    std::io::BufReader::new(stderr)
                        .read_to_end(&mut bytes)
                        .unwrap();
                    bytes
                });
                let start = Instant::now();
                let status = loop {
                    if let Some(status) = child.0.try_wait().unwrap() {
                        break status;
                    }
                    if start.elapsed() >= Duration::from_secs(10) {
                        child.0.kill().unwrap();
                        let _ = child.0.wait();
                        panic!("nested fixture exceeded deadline");
                    }
                    std::thread::sleep(Duration::from_millis(20));
                };
                Output {
                    status,
                    stdout: a.join().unwrap(),
                    stderr: b.join().unwrap(),
                }
            });
            json!({"parent": probe(&config), "child": record(&output)})
        }
        "handle" => {
            use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_DISK, GetFileType, ReadFile};
            let handle = config["handle"].as_u64().unwrap() as usize as HANDLE;
            let mut buffer = [0u8; 64];
            let mut length = 0;
            let ok = if unsafe { GetFileType(handle) } == FILE_TYPE_DISK {
                unsafe {
                    ReadFile(
                        handle,
                        buffer.as_mut_ptr(),
                        buffer.len() as u32,
                        &mut length,
                        std::ptr::null_mut(),
                    )
                }
            } else {
                0
            };
            json!({"ok": ok != 0, "bytes": &buffer[..length as usize]})
        }
        "network" => {
            let address: std::net::SocketAddr =
                config["address"].as_str().unwrap().parse().unwrap();
            let tcp =
                std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_ok();
            let udp = std::net::UdpSocket::bind("[::1]:0")
                .and_then(|socket| socket.send_to(b"production-datagram", address));
            json!({"tcp": tcp, "udp": udp.is_ok()})
        }
        "environment-owner" => {
            assert_eq!(
                std::env::var("SANDBOX_AMBIENT_ONLY").unwrap(),
                "parent-only"
            );
            let root = Path::new(config["root"].as_str().unwrap());
            let project = root.join("project");
            let policy = policy(root, &project, "report", config.clone());
            json!({
                "raw": output(&session(&policy, false), &project),
                "native": output(&session(&policy, true), &project)
            })
        }
        "owner" => {
            unprivileged();
            let root = Path::new(config["root"].as_str().unwrap());
            let project = root.join("project");
            let policy = policy(root, &project, "hold", config.clone());
            let sandbox = session(&policy, config["native"].as_bool().unwrap());
            let _command = sandbox.prepare(command(&project)).unwrap().spawn().unwrap();
            loop {
                std::thread::park();
            }
        }
        "hold" => {
            let root = Path::new(config["root"].as_str().unwrap());
            std::fs::write(
                root.join("project")
                    .join(format!("ready-{}", config["tag"].as_str().unwrap())),
                json!({"pid": std::process::id(), "probe": probe(&config)}).to_string(),
            )
            .unwrap();
            loop {
                std::thread::park();
            }
        }
        _ => panic!("unknown fixture mode {mode}"),
    };
    println!("{MARKER}{result}");
}

#[test]
#[ignore = "requires a dedicated non-administrative Windows user"]
fn public_sessions_reuse_resolved_identity_without_reusing_command_environment() {
    let _serial = SERIAL.lock().unwrap();
    let root = fixture();
    let project = root.path().join("project");
    for native in [false, true] {
        let mut first = policy(root.path(), &project, "report", config(root.path()));
        first
            .env
            .constructed
            .insert("SANDBOX_COMMAND_VALUE".into(), "first-private-value".into());
        let mut second = policy(
            root.path(),
            &project.join("."),
            "report",
            config(root.path()),
        );
        second.env.constructed.insert(
            "SANDBOX_COMMAND_VALUE".into(),
            "second-private-value".into(),
        );
        let a = session(&first, native);
        let b = session(&second, native);
        let one = output(&a, &project);
        let two = output(&b, &project);
        assert!(one["sid"].as_str().unwrap().starts_with("S-1-15-2-"));
        assert_eq!(one["sid"], two["sid"]);
        assert_eq!(one["tmp"], two["tmp"]);
        assert_eq!(one["value"], "first-private-value");
        assert_eq!(two["value"], "second-private-value");
        let marker = Path::new(one["tmp"].as_str().unwrap()).join("idle-reuse-marker");
        std::fs::write(&marker, b"retained-private-state").unwrap();
        a.close();
        assert_eq!(output(&b, &project)["value"], "second-private-value");
        b.close();
        assert_eq!(
            output(&session(&first, native), &project)["sid"],
            one["sid"]
        );
        assert_eq!(std::fs::read(&marker).unwrap(), b"retained-private-state");
        let other = root.path().join("other-project");
        std::fs::create_dir_all(&other).unwrap();
        let different = output(
            &session(
                &policy(root.path(), &other, "report", config(root.path())),
                native,
            ),
            &other,
        );
        assert_ne!(different["sid"], one["sid"]);
        assert_ne!(different["tmp"], one["tmp"]);
    }
    let mut owner = policy(
        root.path(),
        &project,
        "environment-owner",
        config(root.path()),
    );
    owner
        .env
        .constructed
        .insert("SANDBOX_AMBIENT_ONLY".into(), "parent-only".into());
    owner
        .env
        .constructed
        .insert("ProgramData".into(), std::env::var("ProgramData").unwrap());
    let result = plain(&owner, &project);
    assert!(result["raw"]["ambient"].is_null(), "{result}");
    assert!(result["native"]["ambient"].is_null(), "{result}");
}

#[test]
#[ignore = "requires a dedicated non-administrative Windows user"]
fn nested_native_adapter_preserves_file_and_host_process_boundaries() {
    let _serial = SERIAL.lock().unwrap();
    let root = fixture();
    let project = root.path().join("project");
    let policy = policy(root.path(), &project, "nested", config(root.path()));
    let control = plain(&policy, &project);
    for depth in ["parent", "child"] {
        assert_eq!(control[depth]["outside_read"], true);
        assert_eq!(control[depth]["outside_write"], true);
        assert_eq!(control[depth]["host_read"], true);
        assert_eq!(control[depth]["host_duplicate"], true);
        assert!(control[depth]["adapter"].is_null());
    }
    let mut identities = Vec::new();
    for native in [false, true] {
        let result = output(&session(&policy, native), &project);
        for depth in ["parent", "child"] {
            let report = &result[depth];
            assert_eq!(report["readable"], json!(b"readable-canary".as_slice()));
            for denied in [
                "outside_read",
                "outside_write",
                "host_read",
                "host_duplicate",
            ] {
                assert_eq!(
                    report[denied], false,
                    "native={native} depth={depth}: {result}"
                );
            }
            assert_eq!(!report["adapter"].is_null(), native, "{result}");
            if native {
                assert_eq!(
                    report["assets"],
                    json!({"read": true, "write": false, "create": false, "journal": false})
                );
            }
        }
        assert_eq!(result["parent"]["sid"], result["child"]["sid"]);
        identities.push(result["parent"]["sid"].clone());
    }
    assert_ne!(
        identities[0], identities[1],
        "raw and adapter policies share an identity"
    );
}

#[test]
#[ignore = "requires a dedicated non-administrative Windows user"]
fn unrelated_inheritable_file_handle_never_crosses_a_public_launch() {
    let _serial = SERIAL.lock().unwrap();
    let root = fixture();
    let project = root.path().join("project");
    let mut file = File::open(root.path().join("omitted/canary")).unwrap();
    assert_ne!(
        unsafe {
            SetHandleInformation(
                file.as_raw_handle(),
                HANDLE_FLAG_INHERIT,
                HANDLE_FLAG_INHERIT,
            )
        },
        0
    );
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"outside-canary");
    let mut config = config(root.path());
    config["handle"] = json!(file.as_raw_handle() as usize);
    let policy = policy(root.path(), &project, "handle", config);
    for native in [false, true] {
        file.seek(SeekFrom::Start(0)).unwrap();
        let result = output(&session(&policy, native), &project);
        assert_ne!(
            result["bytes"],
            json!(b"outside-canary".as_slice()),
            "inherited file capability bypassed the policy: {result}"
        );
    }
}

#[test]
#[ignore = "requires a dedicated non-administrative Windows user with IPv6 loopback"]
fn net_false_blocks_ipv6_tcp_and_actual_udp_delivery() {
    let _serial = SERIAL.lock().unwrap();
    let root = fixture();
    let project = root.path().join("project");
    let tcp =
        std::net::TcpListener::bind("[::1]:0").expect("IPv6 loopback is a fixture prerequisite");
    let address = tcp.local_addr().unwrap();
    tcp.set_nonblocking(true).unwrap();
    let udp = std::net::UdpSocket::bind(address).unwrap();
    udp.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let mut config = config(root.path());
    config["address"] = json!(address.to_string());
    let policy = policy(root.path(), &project, "network", config);
    let control = plain(&policy, &project);
    assert_eq!(control["tcp"], true);
    assert!(tcp.accept().is_ok());
    let mut bytes = [0u8; 64];
    let length = udp.recv(&mut bytes).unwrap();
    assert_eq!(&bytes[..length], b"production-datagram");
    for native in [false, true] {
        let result = output(&session(&policy, native), &project);
        assert_eq!(result["tcp"], false, "{result}");
        assert_eq!(
            tcp.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let error = udp
            .recv(&mut bytes)
            .expect_err("net:false delivered an IPv6 datagram");
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
            "{error}"
        );
    }
}

fn ready(root: &Path, tag: &str) -> Value {
    let path = root.join("project").join(format!("ready-{tag}"));
    let start = Instant::now();
    loop {
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(value) = serde_json::from_slice(&bytes) {
                return value;
            }
        }
        assert!(start.elapsed() < DEADLINE, "owner never published {path:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn process_handle(pid: u32) -> OwnedHandle {
    let handle = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    assert!(
        !handle.is_null(),
        "cannot observe process {pid}: {}",
        std::io::Error::last_os_error()
    );
    unsafe { OwnedHandle::from_raw_handle(handle) }
}

#[test]
#[ignore = "requires a dedicated non-administrative Windows user"]
fn public_independent_owner_death_leaves_equivalent_survivor_running() {
    let _serial = SERIAL.lock().unwrap();
    let root = fixture();
    for native in [false, true] {
        let owner = |tag: &str| {
            let mut config = config(root.path());
            config["native"] = json!(native);
            config["tag"] = json!(tag);
            Owner(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", CHILD, "--nocapture"])
                    .env(MODE, "owner")
                    .env(CONFIG, config.to_string())
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .unwrap(),
            )
        };
        let a_tag = format!("a-{native}");
        let b_tag = format!("b-{native}");
        let a = owner(&a_tag);
        let b = owner(&b_tag);
        let a_ready = ready(root.path(), &a_tag);
        let b_ready = ready(root.path(), &b_tag);
        assert_eq!(a_ready["probe"]["sid"], b_ready["probe"]["sid"]);
        let a_process = process_handle(a_ready["pid"].as_u64().unwrap() as u32);
        let b_process = process_handle(b_ready["pid"].as_u64().unwrap() as u32);
        drop(a);
        assert_eq!(
            unsafe { WaitForSingleObject(a_process.as_raw_handle(), 30_000) },
            0,
            "dead owner's command survived"
        );
        nub_sandbox::cleanup().unwrap();
        assert_eq!(
            unsafe { WaitForSingleObject(b_process.as_raw_handle(), 0) },
            258,
            "cleanup killed the other owner"
        );
        drop(b);
        assert_eq!(
            unsafe { WaitForSingleObject(b_process.as_raw_handle(), 30_000) },
            0
        );
    }
}

#[test]
#[ignore = "requires a dedicated Windows user and a serialized persistent-cache sweep"]
fn unique_policy_churn_preserves_live_lease_and_bounds_retained_private_roots() {
    let _serial = SERIAL.lock().unwrap();
    let root = fixture();
    let project = root.path().join("project");
    let active = session(
        &policy(root.path(), &project, "report", config(root.path())),
        true,
    );
    let initial = output(&active, &project);
    let active_tmp = PathBuf::from(initial["tmp"].as_str().unwrap());
    std::fs::write(active_tmp.join("live-marker"), b"live").unwrap();
    let mut private_roots = BTreeSet::new();
    for index in 0..66 {
        let project = root.path().join(format!("project-{index}"));
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("caller-output"), b"caller").unwrap();
        let sandbox = session(
            &policy(root.path(), &project, "report", config(root.path())),
            true,
        );
        let report = output(&sandbox, &project);
        private_roots.insert(PathBuf::from(report["tmp"].as_str().unwrap()));
        sandbox.close();
        assert!(
            private_roots.iter().filter(|path| path.exists()).count() <= 64,
            "idle profile roots exceeded 64 after policy {index}"
        );
        assert_eq!(
            std::fs::read(active_tmp.join("live-marker")).unwrap(),
            b"live"
        );
    }
    assert_eq!(
        private_roots.len(),
        66,
        "unique policies unexpectedly shared private data"
    );
    assert_eq!(output(&active, &project)["sid"], initial["sid"]);
    active.close();
    nub_sandbox::cleanup().unwrap();
    for path in private_roots {
        assert!(!path.exists(), "cleanup retained {path:?}");
    }
    for index in 0..66 {
        assert_eq!(
            std::fs::read(root.path().join(format!("project-{index}/caller-output"))).unwrap(),
            b"caller"
        );
    }
}

#[test]
#[ignore = "requires a dedicated non-administrative Windows user"]
fn cleanup_retries_a_locked_private_file_without_deleting_caller_outputs() {
    let _serial = SERIAL.lock().unwrap();
    let root = fixture();
    let project = root.path().join("project");
    let sandbox = session(
        &policy(root.path(), &project, "report", config(root.path())),
        true,
    );
    let report = output(&sandbox, &project);
    let tmp = PathBuf::from(report["tmp"].as_str().unwrap());
    let path = tmp.join("locked-output");
    std::fs::write(&path, b"locked").unwrap();
    std::fs::write(project.join("caller-output"), b"caller").unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&path)
        .unwrap();
    sandbox.close();
    let first = nub_sandbox::cleanup();
    eprintln!("locked cleanup: {first:?}");
    assert!(
        tmp.exists(),
        "locked private data disappeared before its handle closed"
    );
    drop(lock);
    nub_sandbox::cleanup().unwrap();
    assert!(
        !tmp.exists(),
        "cleanup failed to retry the private-file sharing violation"
    );
    assert_eq!(
        std::fs::read(project.join("caller-output")).unwrap(),
        b"caller"
    );
}

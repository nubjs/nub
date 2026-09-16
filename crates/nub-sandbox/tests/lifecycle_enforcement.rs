//! Native children are the enforcement oracle; matcher success alone does not pass these tests.
use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;

const CASE: &str = "SANDBOX_FIXTURE_CASE";

#[test]
fn native_child() {
    let Ok(case) = std::env::var(CASE) else {
        return;
    };
    let root = PathBuf::from(std::env::var("SANDBOX_FIXTURE_ROOT").unwrap());
    match case.as_str() {
        #[cfg(unix)]
        "tmp-owner" => {
            let session = sandbox(&root, "tmp");
            run(&session, &root, "tmp");
            std::process::exit(91);
        }
        #[cfg(target_os = "linux")]
        "lifetime" => {
            for syscall in [libc::SYS_setsid, libc::SYS_setpgid] {
                assert_eq!(unsafe { libc::syscall(syscall, 0, 0) }, -1);
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EPERM)
                );
            }
            for request in [0x5412, 0x541c] {
                assert_eq!(unsafe { libc::ioctl(-1, request, 0) }, -1);
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EPERM)
                );
            }
            assert_eq!(unsafe { libc::ioctl(-1, libc::TIOCGWINSZ, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EBADF)
            );
        }
        #[cfg(unix)]
        "owner" => {
            let session = sandbox(&root, "descendants");
            let _child = session
                .prepare(
                    CommandSpec::new(std::env::current_exe().unwrap())
                        .args(["--exact", "native_child", "--nocapture"])
                        .cwd(root.join("project")),
                )
                .unwrap()
                .spawn()
                .unwrap();
            loop {
                std::thread::park();
            }
        }
        #[cfg(unix)]
        "descendants" => {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "native_child", "--nocapture"])
                .env(CASE, "sleep")
                .spawn()
                .unwrap();
            std::fs::write(root.join("project/descendant-pid"), child.id().to_string()).unwrap();
            child.wait().unwrap();
        }
        #[cfg(unix)]
        "sleep" => loop {
            std::thread::park();
        },
        "files" => {
            assert_eq!(
                std::fs::read(root.join("readable/input")).unwrap(),
                b"readable"
            );
            assert!(std::fs::write(root.join("readable/input"), b"forbidden").is_err());
            assert!(std::fs::read(root.join("omitted/.ssh/key")).is_err());
            assert!(std::fs::write(root.join("omitted/.ssh/key"), b"forbidden").is_err());
            std::fs::write(root.join("project/created"), b"created").unwrap();
            std::fs::create_dir_all(root.join("project/later/nested")).unwrap();
            std::fs::write(root.join("project/later/nested/output"), b"later").unwrap();
            assert!(std::env::var_os("SANDBOX_PARENT_SECRET").is_none());
        }
        "tooldirs" => {
            assert_eq!(
                PathBuf::from(std::env::var("UV_CACHE_DIR").unwrap()),
                root.join("resolved-cache")
            );
            for path in tool_roots(&root) {
                let output = path.join("created");
                std::fs::write(&output, b"tool-state").unwrap();
                assert_eq!(std::fs::read(&output).unwrap(), b"tool-state");
                std::fs::remove_file(&output).unwrap();
            }
            for path in [
                "omitted/.ssh/key",
                "relocated-cache/unrelated/canary",
                "previous-cache/canary",
            ] {
                assert!(std::fs::read(root.join(path)).is_err());
                assert!(std::fs::write(root.join(path), b"forbidden").is_err());
            }
        }
        #[cfg(target_os = "macos")]
        "sharedtmp" => {
            let canary = std::fs::read_to_string(root.join("project/canary-path")).unwrap();
            assert!(std::fs::read(&canary).is_err());
            assert!(std::fs::write(&canary, b"forbidden").is_err());
        }
        "tmp" => {
            let tmp = std::env::temp_dir();
            #[cfg(unix)]
            {
                let lease = tmp.parent().unwrap().join("lease");
                assert!(std::fs::read(&lease).is_err());
                assert!(std::fs::write(&lease, b"forged").is_err());
            }
            let marker = tmp.join("session-marker");
            if marker.exists() {
                assert_eq!(std::fs::read(&marker).unwrap(), b"shared");
                std::fs::write(root.join("project/reused"), b"yes").unwrap();
            } else {
                std::fs::write(&marker, b"shared").unwrap();
                std::fs::write(
                    root.join("project/tmp-path"),
                    tmp.to_string_lossy().as_bytes(),
                )
                .unwrap();
            }
        }
        "network" => {
            let address = std::fs::read_to_string(root.join("project/address"))
                .unwrap()
                .parse()
                .unwrap();
            assert!(
                std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(2))
                    .is_err(),
                "net:false allowed a direct TCP connection"
            );
            if let Ok(socket) = std::net::UdpSocket::bind("127.0.0.1:0") {
                // Windows can accept a datagram into the send queue and drop it
                // at network isolation. The parent checks actual delivery.
                let result = socket.send_to(b"confined-datagram", address);
                eprintln!("UDP send result: {result:?}");
            }
        }
        "proc" => {
            #[cfg(target_os = "linux")]
            {
                let parent = std::env::var("SANDBOX_FIXTURE_PARENT").unwrap();
                for file in ["environ", "mem"] {
                    assert!(
                        std::fs::File::open(format!("/proc/{parent}/{file}")).is_err(),
                        "parent {file} was exposed"
                    );
                }
            }
        }
        _ => panic!("unknown native fixture {case}"),
    }
    println!("SANDBOX_NATIVE_OK:{case}");
}

fn fixture() -> tempfile::TempDir {
    // Host tmp may be part of the platform runtime baseline. Put the omitted canary outside it.
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap();
    tempfile::Builder::new()
        .prefix(".sandbox-enforcement-")
        .tempdir_in(home)
        .unwrap()
}

fn sandbox(root: &std::path::Path, case: &str) -> Sandbox {
    let project = root.join("project");
    let mut ctx = CompileCtx::new(
        Homes {
            home: root.join("omitted"),
            cache: root.join("cache"),
            tmp: root.join("tmp"),
            project: project.clone(),
        },
        project,
        ScopeCapabilities::approved(),
        // ⛔ Seeded, not empty, for the reason spelled out in `self_proc.rs`: the confined child
        // asserts `SANDBOX_PARENT_SECRET` is absent, and with an empty map that assertion could
        // never fail — no `vars` entry admits this name, so its absence now means the env axis
        // withheld it rather than that nobody ever set it.
        BTreeMap::from([(
            "SANDBOX_PARENT_SECRET".to_string(),
            "must-not-reach-the-child".to_string(),
        )]),
    );
    let permissions = if case == "tooldirs" {
        for (key, suffix) in [
            ("XDG_CACHE_HOME", "relocated-cache"),
            ("XDG_DATA_HOME", "relocated-data"),
            ("XDG_CONFIG_HOME", "relocated-config"),
            ("NPM_CONFIG_GLOBAL_VIRTUAL_STORE_DIR", "relocated-store"),
            ("LOCALAPPDATA", "local-app-data"),
            ("APPDATA", "roaming-app-data"),
            ("UV_CACHE_DIR", "previous-cache"),
        ] {
            ctx.ambient_env
                .insert(key.into(), root.join(suffix).to_string_lossy().into());
        }
        struct CachePath(PathBuf);
        impl nub_sandbox::CommandRunner for CachePath {
            fn run(&self, command: &str) -> Result<String, String> {
                assert_eq!(command, "tool-cache-location");
                Ok(self.0.to_string_lossy().into_owned())
            }
        }
        ctx.runner = Box::new(CachePath(root.join("resolved-cache")));
        json!({"fs": ["./", "$tooldirs", "$tmp"], "net": false, "vars": {"UV_CACHE_DIR": "$(tool-cache-location)"}})
    } else {
        json!({"fs": {(root.join("project").to_string_lossy()): "rw", (root.join("readable").to_string_lossy()): "r", "$tmp": "rw"}, "net": false})
    };
    let mut policy = compile(&permissions, &ctx).unwrap();
    for key in [
        "PATH",
        "SystemRoot",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
    ] {
        if let Ok(value) = std::env::var(key) {
            policy.env.constructed.insert(key.into(), value);
        }
    }
    policy.env.constructed.insert(CASE.into(), case.into());
    policy
        .env
        .constructed
        .insert("SANDBOX_FIXTURE_ROOT".into(), root.to_string_lossy().into());
    policy.env.constructed.insert(
        "SANDBOX_FIXTURE_PARENT".into(),
        std::process::id().to_string(),
    );
    Sandbox::new(&policy).unwrap()
}

fn tool_roots(root: &std::path::Path) -> Vec<PathBuf> {
    let roots = [
        "omitted/.cache/nub",
        "omitted/.config/nub",
        "omitted/.cache/git",
        "relocated-cache/nub",
        "relocated-cache/git",
        "relocated-data/nub/store",
        "relocated-config/nub",
        "relocated-config/go",
        "relocated-store",
        "resolved-cache",
    ];
    #[cfg(windows)]
    let roots = roots
        .into_iter()
        .chain(["local-app-data/nub", "roaming-app-data/nub"]);
    roots.into_iter().map(|path| root.join(path)).collect()
}

#[test]
fn tooldirs_grants_nub_storage_without_granting_its_parents() {
    let root = fixture();
    for path in tool_roots(root.path()) {
        std::fs::create_dir_all(path).unwrap();
    }
    for path in [
        "project",
        "omitted/.ssh",
        "relocated-cache/unrelated",
        "previous-cache",
    ] {
        std::fs::create_dir_all(root.path().join(path)).unwrap();
    }
    for path in [
        "omitted/.ssh/key",
        "relocated-cache/unrelated/canary",
        "previous-cache/canary",
    ] {
        std::fs::write(root.path().join(path), b"not-granted").unwrap();
    }
    let sandbox = sandbox(root.path(), "tooldirs");
    run(&sandbox, root.path(), "tooldirs");
}

fn run(sandbox: &Sandbox, root: &std::path::Path, case: &str) {
    let command = CommandSpec::new(std::env::current_exe().unwrap())
        .args(["--exact", "native_child", "--nocapture"])
        .cwd(root.join("project"));
    let prepared = sandbox.prepare(command).unwrap();
    assert!(
        prepared.degradation.lost.is_empty(),
        "native fixture degraded: {:?}",
        prepared.degradation
    );
    let output = prepared.output().unwrap();
    assert!(
        output.status.success(),
        "{case}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("SANDBOX_NATIVE_OK:{case}")));
}

#[test]
fn positive_filesystem_grants_are_enforced_by_native_children() {
    let root = fixture();
    for path in ["project", "readable", "omitted/.ssh", "cache"] {
        std::fs::create_dir_all(root.path().join(path)).unwrap();
    }
    std::fs::write(root.path().join("readable/input"), b"readable").unwrap();
    std::fs::write(root.path().join("omitted/.ssh/key"), b"not-a-secret").unwrap();
    let sandbox = sandbox(root.path(), "files");
    for _ in 0..3 {
        run(&sandbox, root.path(), "files");
    }
    assert_eq!(
        std::fs::read(root.path().join("project/created")).unwrap(),
        b"created"
    );
    assert_eq!(
        std::fs::read(root.path().join("omitted/.ssh/key")).unwrap(),
        b"not-a-secret"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn native_child_cannot_read_the_host_process_environment() {
    let root = fixture();
    for path in ["project", "readable", "cache"] {
        std::fs::create_dir_all(root.path().join(path)).unwrap();
    }
    let sandbox = sandbox(root.path(), "proc");
    run(&sandbox, root.path(), "proc");
}

#[test]
fn managed_temp_is_shared_by_commands_until_session_close() {
    let root = fixture();
    for path in ["project", "readable", "cache"] {
        std::fs::create_dir_all(root.path().join(path)).unwrap();
    }
    let sandbox = sandbox(root.path(), "tmp");
    run(&sandbox, root.path(), "tmp");
    run(&sandbox, root.path(), "tmp");
    assert_eq!(
        std::fs::read(root.path().join("project/reused")).unwrap(),
        b"yes"
    );
    let tmp = PathBuf::from(std::fs::read_to_string(root.path().join("project/tmp-path")).unwrap());
    assert!(tmp.join("session-marker").exists());
    sandbox.close();
    #[cfg(unix)]
    assert!(
        !tmp.exists(),
        "closed Unix session retained its managed temp"
    );
}

#[cfg(unix)]
#[test]
fn cleanup_recovers_temp_after_a_native_command_owner_crashes() {
    let root = fixture();
    for path in ["project", "readable", "cache"] {
        std::fs::create_dir_all(root.path().join(path)).unwrap();
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "native_child", "--nocapture"])
        .env(CASE, "tmp-owner")
        .env("SANDBOX_FIXTURE_ROOT", root.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(91));
    let tmp = PathBuf::from(std::fs::read_to_string(root.path().join("project/tmp-path")).unwrap());
    // Another parallel test may already have run opportunistic recovery.
    nub_sandbox::cleanup().unwrap();
    assert!(
        !tmp.exists(),
        "cleanup retained the crashed owner's private data"
    );
    assert!(root.path().join("project/tmp-path").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn private_temp_does_not_grant_the_shared_darwin_scratch_directory() {
    let root = fixture();
    for path in ["project", "readable", "cache"] {
        std::fs::create_dir_all(root.path().join(path)).unwrap();
    }
    let canary = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(canary.path(), b"unchanged").unwrap();
    std::fs::write(
        root.path().join("project/canary-path"),
        canary.path().to_string_lossy().as_bytes(),
    )
    .unwrap();
    run(&sandbox(root.path(), "sharedtmp"), root.path(), "sharedtmp");
    assert_eq!(std::fs::read(canary.path()).unwrap(), b"unchanged");
}

#[test]
fn net_false_blocks_native_tcp_and_udp() {
    let root = fixture();
    for path in ["project", "readable", "cache"] {
        std::fs::create_dir_all(root.path().join(path)).unwrap();
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let control = std::net::TcpStream::connect(address).unwrap();
    let _accepted = listener.accept().unwrap();
    drop(control);
    let datagrams = std::net::UdpSocket::bind(address).unwrap();
    datagrams
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    sender.send_to(b"control", address).unwrap();
    let mut received = [0_u8; 64];
    let (length, _) = datagrams.recv_from(&mut received).unwrap();
    assert_eq!(&received[..length], b"control");
    std::fs::write(root.path().join("project/address"), address.to_string()).unwrap();
    let sandbox = sandbox(root.path(), "network");
    run(&sandbox, root.path(), "network");
    let error = datagrams
        .recv_from(&mut received)
        .expect_err("net:false delivered a UDP datagram to the unconfined receiver");
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_commands_cannot_detach_or_inject_terminal_input() {
    let root = fixture();
    for path in ["project", "readable", "cache"] {
        std::fs::create_dir_all(root.path().join(path)).unwrap();
    }
    for request in [0x5412, 0x541c] {
        assert_eq!(unsafe { libc::ioctl(-1, request, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF)
        );
    }
    let sandbox = sandbox(root.path(), "lifetime");
    run(&sandbox, root.path(), "lifetime");
}

#[cfg(unix)]
fn process_running(pid: i32) -> bool {
    #[cfg(target_os = "linux")]
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        return stat
            .rsplit_once(") ")
            .is_some_and(|(_, tail)| !tail.starts_with('Z'));
    }
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(unix)]
#[test]
fn killing_the_owner_terminates_the_native_descendant_tree() {
    let root = fixture();
    for path in ["project", "readable", "cache"] {
        std::fs::create_dir_all(root.path().join(path)).unwrap();
    }
    let mut owner = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "native_child", "--nocapture"])
        .env(CASE, "owner")
        .env("SANDBOX_FIXTURE_ROOT", root.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let pid_file = root.path().join("project/descendant-pid");
    while !pid_file.exists() && std::time::Instant::now() < deadline {
        assert!(
            owner.try_wait().unwrap().is_none(),
            "owner exited before creating descendant"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    owner.kill().unwrap();
    owner.wait().unwrap();
    let pid: i32 = std::fs::read_to_string(pid_file)
        .expect("descendant readiness")
        .parse()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while process_running(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let survived = process_running(pid);
    if survived {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    assert!(!survived, "descendant survived owner SIGKILL");
}

//! A confined command may listen on loopback and nowhere else.
//!
//! The socket ceiling is a FAMILY gate, so the moment a policy admits any egress it admits
//! `AF_INET`/`AF_INET6` — and nothing then stopped the command binding `0.0.0.0` and serving. That
//! is an inbound channel the `net` axis never granted: that axis names egress DESTINATIONS, and no
//! host grant is a statement about who may reach in.
//!
//! Loopback stays allowed on purpose. A confined command running a dev server on `127.0.0.1:3000`
//! is the ordinary case for an agent sandbox, and a loopback listener is reachable only from the
//! host already running the command.
//!
//! The third case is the one that decides whether the check is real. `listen` on an UNBOUND socket
//! implicitly binds to the wildcard address, so a `bind`-only guard would let a command open an
//! externally-reachable port without ever calling the syscall being guarded.
#![cfg(target_os = "linux")]

#[path = "common/tool_output.rs"]
mod tool_output;

use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

const CASE: &str = "NUB_INBOUND_CASE";

fn tcp_socket() -> i32 {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    assert!(
        fd >= 0,
        "an IP socket must be creatable under a net-granting policy"
    );
    fd
}

/// `bind` to `addr:0`, returning the errno on failure. `addr` is in network byte order.
fn bind_errno(fd: i32, addr: [u8; 4]) -> Option<i32> {
    let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sa.sin_family = libc::AF_INET as libc::sa_family_t;
    sa.sin_port = 0;
    sa.sin_addr.s_addr = u32::from_ne_bytes(addr);
    let rc = unsafe {
        libc::bind(
            fd,
            &sa as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    (rc != 0).then(|| std::io::Error::last_os_error().raw_os_error().unwrap())
}

fn listen_errno(fd: i32) -> Option<i32> {
    let rc = unsafe { libc::listen(fd, 1) };
    (rc != 0).then(|| std::io::Error::last_os_error().raw_os_error().unwrap())
}

#[test]
fn inbound_socket_child() {
    if std::env::var(CASE).is_err() {
        return;
    }

    // POSITIVE CONTROL. Without it every refusal below would pass just as well if the broker
    // refused every bind, which would break the dev-server case the design exists to preserve.
    let good = tcp_socket();
    assert_eq!(
        bind_errno(good, [127, 0, 0, 1]),
        None,
        "a loopback bind must be allowed — a dev server is the ordinary case",
    );
    assert_eq!(
        listen_errno(good),
        None,
        "a loopback listener must be allowed"
    );

    let wild = tcp_socket();
    assert_eq!(
        bind_errno(wild, [0, 0, 0, 0]),
        Some(libc::EPERM),
        "a wildcard bind must be refused — it is reachable from every interface",
    );

    // THE DECIDING CASE: never bound, so the kernel would bind the wildcard implicitly.
    let implicit = tcp_socket();
    assert_eq!(
        listen_errno(implicit),
        Some(libc::EPERM),
        "listen on an unbound socket must be refused; the implicit bind is the wildcard",
    );

    for fd in [good, wild, implicit] {
        unsafe { libc::close(fd) };
    }
}

#[test]
fn a_confined_command_can_listen_on_loopback_and_nowhere_else() {
    let root = tempfile::tempdir().expect("fixture root");
    let project = root.path().join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("index.js"), "ordinary-source").unwrap();

    // A host grant is what lifts the socket ceiling to the IP families, so this policy is the one
    // where the gap existed at all. With `net: false` no IP socket can be created to bind.
    let mut policy = compile(
        &json!({
            "fs": {(project.to_string_lossy()): "rw"},
            "net": ["registry.npmjs.org"],
        }),
        &ctx(root.path()),
    )
    .expect("a host-granting policy compiles");
    policy.env.constructed.insert(CASE.into(), "1".into());

    let sandbox = Sandbox::new(&policy).expect("the supervised backend acquires");
    let prepared = sandbox
        .prepare(
            CommandSpec::new(std::env::current_exe().unwrap())
                .args(["--exact", "inbound_socket_child", "--nocapture"])
                .cwd(&project)
                .redact_stdout(true)
                .redact_stderr(true),
        )
        .expect("the child prepares");
    assert!(prepared.degradation.is_full(), "{:?}", prepared.degradation);
    let output = tool_output::output(prepared);
    assert!(
        output.status.success(),
        "inbound socket scoping failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn ctx(root: &Path) -> CompileCtx {
    let project = root.join("project");
    CompileCtx::new(
        Homes {
            home: root.join("home"),
            cache: root.join("cache"),
            tmp: root.join("tmp"),
            project: project.clone(),
        },
        project,
        ScopeCapabilities::approved(),
        BTreeMap::new(),
    )
}

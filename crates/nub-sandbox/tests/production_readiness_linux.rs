//! Real-kernel adversarial probes for the reusable Linux sandbox.
//!
//! These intentionally use the public `Sandbox` / `CommandSpec` API and re-enter this
//! integration-test executable.  The assertions exercise the post-`execve` process, not the
//! matcher or the backend's unit-test helpers.
#![cfg(target_os = "linux")]

#[path = "common/tool_output.rs"]
mod tool_output;

use nub_sandbox::policy::{CanonGlob, Effect, FsAccess, FsOrigin, FsRule};
use nub_sandbox::{
    CommandSpec, CompileCtx, Homes, Sandbox, SandboxPolicy, ScopeCapabilities, compile,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

const CASE: &str = "NUB_PRODUCTION_LINUX_CASE";
const ROOT: &str = "NUB_PRODUCTION_LINUX_ROOT";
const PARENT_PID: &str = "NUB_PRODUCTION_LINUX_PARENT_PID";
const INHERITED_FD: &str = "NUB_PRODUCTION_LINUX_INHERITED_FD";
const COUNT_OWNER: &str = "NUB_PRODUCTION_LINUX_COUNT_OWNER";
const EXECUTABLE: &str = "NUB_PRODUCTION_LINUX_EXECUTABLE";
const DYNAMIC_FIRST: &str = "NUB_PRODUCTION_LINUX_DYNAMIC_FIRST";
const PARENT_SECRET_FD: &str = "NUB_PRODUCTION_LINUX_PARENT_SECRET_FD";

#[test]
fn linux_production_child() {
    let Some(case) = std::env::var_os(CASE) else {
        return;
    };
    let root = PathBuf::from(std::env::var_os(ROOT).expect("child root"));
    match case.to_string_lossy().as_ref() {
        "filesystem" => filesystem_child(&root),
        "inherited-fd" => inherited_fd_child(),
        "proc" => proc_child(),
        "sockets" => sockets_child(),
        "self-proc-race" => self_proc_race_child(),
        "late-speculative" => assert_unavailable(root.join("late-speculative/secret")),
        "dynamic-exec" => dynamic_exec_child(),
        "wait" => {
            std::fs::write(root.join("project/ready"), b"ready").unwrap();
            loop {
                std::thread::park();
            }
        }
        "owner" => owner_child(&root),
        "basic" => {
            assert_eq!(std::fs::read_to_string("allowed").unwrap(), "ALLOWED");
            std::fs::write("command-ran", b"yes").unwrap();
        }
        other => panic!("unknown Linux production probe {other}"),
    }
    println!("LINUX_PRODUCTION_READY:{}", case.to_string_lossy());
}

fn fixture() -> tempfile::TempDir {
    let home = std::env::var_os("HOME").expect("HOME is set for integration tests");
    let root = tempfile::Builder::new()
        .prefix(".nub-production-linux-")
        .tempdir_in(home)
        .unwrap();
    for directory in ["project", "granted/nested", "withheld"] {
        std::fs::create_dir_all(root.path().join(directory)).unwrap();
    }
    std::fs::write(root.path().join("project/allowed"), "ALLOWED").unwrap();
    std::fs::write(root.path().join("granted/node"), "GRANTED").unwrap();
    std::fs::write(root.path().join("granted/nested/node"), "DESCENDANT").unwrap();
    std::fs::write(root.path().join("withheld/secret"), "WITHHELD").unwrap();
    symlink(
        root.path().join("granted"),
        root.path().join("project/alias"),
    )
    .unwrap();
    root
}

fn policy(root: &Path, self_stat: bool) -> SandboxPolicy {
    let project = root.join("project");
    let mut fs = Map::new();
    fs.insert(project.display().to_string(), json!("rw"));
    fs.insert(root.join("granted").display().to_string(), json!("r"));
    fs.insert("$tmp".into(), json!("rw"));
    fs.insert(
        std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .display()
            .to_string(),
        json!("r"),
    );
    if self_stat {
        fs.insert("/proc/self/stat".into(), json!("r"));
    }
    let mut input = Map::new();
    input.insert("fs".into(), Value::Object(fs));
    input.insert("net".into(), Value::Bool(false));
    let ctx = CompileCtx::new(
        Homes {
            home: root.join("withheld-home"),
            cache: root.join("withheld-cache"),
            tmp: root.join("tmp"),
            project: project.clone(),
        },
        project,
        ScopeCapabilities::approved(),
        BTreeMap::new(),
    );
    compile(&Value::Object(input), &ctx).expect("production policy compiles")
}

fn session(
    mut policy: SandboxPolicy,
    root: &Path,
    case: &str,
    extra_env: &[(&str, String)],
) -> Sandbox {
    policy.env.constructed.extend([
        (CASE.into(), case.into()),
        (ROOT.into(), root.display().to_string()),
        (PARENT_PID.into(), std::process::id().to_string()),
        (
            EXECUTABLE.into(),
            std::env::current_exe().unwrap().display().to_string(),
        ),
    ]);
    policy.env.constructed.extend(
        extra_env
            .iter()
            .map(|(key, value)| ((*key).into(), value.clone())),
    );
    Sandbox::new(&policy).expect("Linux Landlock/seccomp enforcement is available")
}

fn sandbox(root: &Path, case: &str, self_stat: bool, extra_env: &[(&str, String)]) -> Sandbox {
    session(policy(root, self_stat), root, case, extra_env)
}

fn command(root: &Path) -> CommandSpec {
    command_with_program(root, std::env::current_exe().unwrap())
}

fn command_with_program(root: &Path, program: impl Into<PathBuf>) -> CommandSpec {
    CommandSpec::new(program.into())
        .args(["--exact", "linux_production_child", "--nocapture"])
        .cwd(root.join("project"))
        .redact_stdout(true)
        .redact_stderr(true)
}

fn output(sandbox: &Sandbox, root: &Path) -> Output {
    let prepared = sandbox
        .prepare(command(root))
        .expect("prepares confined child");
    assert!(
        prepared.degradation.lost.is_empty(),
        "{:#?}",
        prepared.degradation
    );
    let output = tool_output::output(prepared);
    assert!(
        output.status.success(),
        "child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn assert_unavailable(path: impl AsRef<Path>) {
    assert!(
        std::fs::read(path.as_ref()).is_err(),
        "sandbox exposed {}",
        path.as_ref().display()
    );
}

fn assert_directory_unavailable(path: impl AsRef<Path>) {
    assert!(
        std::fs::read_dir(path.as_ref()).is_err(),
        "sandbox listed {}",
        path.as_ref().display()
    );
}

fn filesystem_child(root: &Path) {
    if root.join("project/renamed-original-ready").exists() {
        assert_eq!(
            std::fs::read_to_string(root.join("displaced/node")).unwrap(),
            "GRANTED"
        );
        return;
    }
    if root.join("project/replacement-ready").exists() {
        // The host replaced the pathname after acquisition.  The old O_PATH rule must not turn a
        // newly substituted hierarchy into a grant, even though its spelling is unchanged.
        assert_eq!(
            std::fs::read_to_string(root.join("displaced/node")).unwrap(),
            "GRANTED"
        );
        assert_unavailable(root.join("granted/secret"));
        return;
    }
    // A grant must cover its own node, descendants, and an alias resolving to that hierarchy.
    assert_eq!(
        std::fs::read_to_string(root.join("granted/node")).unwrap(),
        "GRANTED"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("granted/nested/node")).unwrap(),
        "DESCENDANT"
    );
    assert!(std::fs::metadata(root.join("granted")).is_ok());
    assert_eq!(std::fs::read_to_string("alias/node").unwrap(), "GRANTED");
    assert_unavailable(root.join("withheld/secret"));
    std::fs::write("created", b"only-project-is-writable").unwrap();
    assert!(std::fs::write(root.join("granted/node"), b"forbidden").is_err());
}

fn inherited_fd_child() {
    let fd: RawFd = std::env::var(INHERITED_FD).unwrap().parse().unwrap();
    assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EBADF)
    );
    assert_unavailable(format!("/proc/self/fd/{fd}"));
}

fn proc_child() {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("explicit self stat grant");
    assert_eq!(
        stat.split_whitespace().next().unwrap(),
        std::process::id().to_string()
    );
    // `/proc/self/exe` is a magic alias for this already read-granted test executable, not an
    // independently injected procfs capability.  Confirm its identity rather than treating a
    // read of a deliberately granted inode as a credential leak.
    let expected = std::fs::metadata(std::env::var(EXECUTABLE).unwrap()).unwrap();
    let proc_exe = std::fs::metadata("/proc/self/exe").expect("read-granted executable alias");
    assert_eq!(
        (proc_exe.dev(), proc_exe.ino()),
        (expected.dev(), expected.ino())
    );
    // `cwd` is another magic alias, but this one targets the explicitly granted project
    // hierarchy.  Its visibility is therefore expected; what matters is that resolving
    // through it cannot traverse to a withheld sibling or create a file there.
    let project = std::fs::metadata(root_path("project")).unwrap();
    let proc_cwd = std::fs::metadata("/proc/self/cwd").expect("granted cwd alias");
    assert_eq!(
        (proc_cwd.dev(), proc_cwd.ino()),
        (project.dev(), project.ino())
    );
    assert_unavailable("/proc/self/cwd/../withheld/secret");
    assert!(
        std::fs::write("/proc/self/cwd/../withheld/proc-alias-write", b"denied").is_err(),
        "sandbox wrote to withheld data through /proc/self/cwd"
    );
    for path in [
        "/proc/self/environ",
        "/proc/self/maps",
        "/proc/self/mem",
        "/proc/thread-self/stat",
    ] {
        assert_unavailable(path);
    }
    for path in ["/proc/self/fd", "/proc/self/fdinfo", "/proc/self/root"] {
        assert_directory_unavailable(path);
    }
    let parent = std::env::var(PARENT_PID).unwrap();
    for leaf in ["environ", "maps", "stat"] {
        assert_unavailable(format!("/proc/{parent}/{leaf}"));
    }
    assert_directory_unavailable(format!("/proc/{parent}/fd"));
    let secret_fd = std::env::var(PARENT_SECRET_FD).unwrap();
    assert_unavailable(format!("/proc/{parent}/fd/{secret_fd}"));
}

fn root_path(leaf: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(ROOT).expect("child root")).join(leaf)
}

fn sockets_child() {
    // `net:false` must reject TCP, UDP, loopback, raw-packet and host-daemon families before a
    // connection can use an unmediated address.  A kernel without a particular optional family
    // may return EAFNOSUPPORT instead; any failure is the required result.
    for (family, kind) in [
        (libc::AF_INET, libc::SOCK_STREAM),
        (libc::AF_INET, libc::SOCK_DGRAM),
        (libc::AF_INET6, libc::SOCK_STREAM),
        (libc::AF_UNIX, libc::SOCK_STREAM),
        (libc::AF_PACKET, libc::SOCK_RAW),
        (libc::AF_VSOCK, libc::SOCK_STREAM),
    ] {
        let fd = unsafe { libc::socket(family, kind | libc::SOCK_CLOEXEC, 0) };
        assert_eq!(fd, -1, "net:false created family {family}, kind {kind}");
    }
    assert!(
        std::net::TcpStream::connect_timeout(
            &"127.0.0.1:9".parse().unwrap(),
            Duration::from_millis(100)
        )
        .is_err(),
        "net:false connected to loopback"
    );
    assert_eq!(
        unsafe {
            libc::syscall(
                libc::SYS_io_uring_setup,
                1_u32,
                std::ptr::null::<libc::c_void>(),
            )
        },
        -1,
        "net:false left io_uring available for socket creation"
    );
}

fn self_proc_race_child() {
    let expected = std::process::id().to_string();
    std::thread::scope(|scope| {
        for _ in 0..12 {
            let expected = &expected;
            scope.spawn(move || {
                for _ in 0..24 {
                    let stat = std::fs::read_to_string("/proc/self/stat")
                        .expect("supervisor injected the requesting process's stat fd");
                    assert_eq!(stat.split_whitespace().next().unwrap(), expected.as_str());
                    assert_unavailable("/proc/self/environ");
                }
            });
        }
    });
}

fn dynamic_exec_child() {
    let first = PathBuf::from(std::env::var_os(DYNAMIC_FIRST).unwrap());
    if std::fs::metadata(std::env::current_exe().unwrap())
        .unwrap()
        .ino()
        == std::fs::metadata(&first).unwrap().ino()
    {
        return;
    }
    assert_unavailable(first);
}

fn owner_child(root: &Path) {
    let session = sandbox(root, "wait", false, &[]);
    let child = session
        .prepare(command(root))
        .expect("prepares owner descendant")
        .spawn()
        .expect("spawns owner descendant");
    std::fs::write(root.join("project/descendant-pid"), child.id().to_string()).unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn grants_cover_alias_node_and_subtree_without_following_replacements() {
    let root = fixture();
    let session = sandbox(root.path(), "filesystem", false, &[]);
    output(&session, root.path());
    assert_eq!(
        std::fs::read(root.path().join("project/created")).unwrap(),
        b"only-project-is-writable"
    );

    std::fs::rename(root.path().join("granted"), root.path().join("displaced")).unwrap();
    std::fs::write(root.path().join("project/renamed-original-ready"), b"yes").unwrap();
    output(&session, root.path());
    std::fs::remove_file(root.path().join("project/renamed-original-ready")).unwrap();

    std::fs::create_dir(root.path().join("granted")).unwrap();
    std::fs::write(root.path().join("granted/secret"), "REPLACEMENT").unwrap();
    std::fs::write(root.path().join("project/replacement-ready"), b"yes").unwrap();
    output(&session, root.path());

    std::fs::remove_dir_all(root.path().join("granted")).unwrap();
    symlink(root.path().join("withheld"), root.path().join("granted")).unwrap();
    output(&session, root.path());
}

#[test]
fn absent_speculative_grant_stays_absent_until_reacquisition() {
    let root = fixture();
    let absent = root.path().join("late-speculative");
    let mut policy = policy(root.path(), false);
    policy.fs.rules.entries.push(FsRule {
        matcher: CanonGlob(absent.display().to_string()),
        effect: Effect::Allow,
        access: FsAccess::Read,
        origin: FsOrigin::Speculative,
    });
    let session = session(policy, root.path(), "late-speculative", &[]);
    symlink(root.path().join("withheld"), &absent).unwrap();
    output(&session, root.path());
}

#[test]
fn command_executable_grants_do_not_cross_reused_session_commands() {
    let root = fixture();
    let executables = root.path().join("executables");
    std::fs::create_dir(&executables).unwrap();
    let first = executables.join("first");
    let second = executables.join("second");
    let source = std::env::current_exe().unwrap();
    for destination in [&first, &second] {
        std::fs::copy(&source, destination).unwrap();
        std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let session = sandbox(
        root.path(),
        "dynamic-exec",
        false,
        &[(DYNAMIC_FIRST, first.display().to_string())],
    );
    let first_output = tool_output::output(
        session
            .prepare(command_with_program(root.path(), &first))
            .expect("prepares first dynamic executable"),
    );
    assert!(
        first_output.status.success(),
        "first dynamic executable failed"
    );
    let second_output = tool_output::output(
        session
            .prepare(command_with_program(root.path(), &second))
            .expect("prepares second dynamic executable"),
    );
    assert!(
        second_output.status.success(),
        "second dynamic executable retained the first command's grant:\n{}",
        String::from_utf8_lossy(&second_output.stderr)
    );
}

#[test]
fn inherited_parent_descriptors_do_not_cross_exec() {
    let root = fixture();
    let secret = CString::new(
        root.path()
            .join("withheld/secret")
            .as_os_str()
            .as_encoded_bytes(),
    )
    .unwrap();
    let original = unsafe { libc::open(secret.as_ptr(), libc::O_RDONLY) };
    assert!(
        original >= 0,
        "opens descriptor canary: {}",
        std::io::Error::last_os_error()
    );
    let inherited = unsafe { libc::fcntl(original, libc::F_DUPFD, 900) };
    unsafe { libc::close(original) };
    assert!(
        inherited >= 900,
        "duplicates descriptor high enough to avoid loader reuse"
    );
    let canary = unsafe { std::fs::File::from_raw_fd(inherited) };
    let session = sandbox(
        root.path(),
        "inherited-fd",
        false,
        &[(INHERITED_FD, inherited.to_string())],
    );
    output(&session, root.path());
    drop(canary);
}

#[test]
fn procfs_injection_exposes_only_the_requested_self_file() {
    let root = fixture();
    let secret = std::fs::File::open(root.path().join("withheld/secret")).unwrap();
    let session = sandbox(
        root.path(),
        "proc",
        true,
        &[(PARENT_SECRET_FD, secret.as_raw_fd().to_string())],
    );
    output(&session, root.path());
}

#[test]
fn net_false_closes_socket_and_io_uring_bypasses() {
    let root = fixture();
    let session = sandbox(root.path(), "sockets", false, &[]);
    output(&session, root.path());
}

#[test]
fn repeated_self_proc_notifications_are_process_bound() {
    let root = fixture();
    let session = sandbox(root.path(), "self-proc-race", true, &[]);
    output(&session, root.path());
}

#[test]
fn cancellation_reaps_a_supervised_child() {
    let root = fixture();
    let session = sandbox(root.path(), "wait", false, &[]);
    let mut child = session
        .prepare(command(root.path()))
        .unwrap()
        .spawn()
        .expect("spawns cancellable child");
    wait_for(&root.path().join("project/ready"), Duration::from_secs(10));
    let pid = child.id() as i32;
    let error = child.wait_cancellable(&AtomicBool::new(true)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
    drop(child);
    wait_for_exit(pid, Duration::from_secs(5));
}

#[test]
fn owner_death_reaps_the_command_tree_without_reusing_its_resources() {
    let root = fixture();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "linux_production_child", "--nocapture"])
        .env(CASE, "owner")
        .env(ROOT, root.path())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    // Keep this ordinary test-host child under RAII too: a readiness assertion may panic before
    // the deliberate owner-loss signal below.
    let mut owner = ReapedChild::spawn(&mut command);
    let pid_path = root.path().join("project/descendant-pid");
    wait_for(&pid_path, Duration::from_secs(10));
    let descendant: i32 = std::fs::read_to_string(&pid_path).unwrap().parse().unwrap();
    owner.kill_and_wait();
    wait_for_exit(descendant, Duration::from_secs(5));
}

#[test]
fn reused_session_releases_per_command_workers_and_descriptors() {
    if std::env::var_os(COUNT_OWNER).is_none() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "reused_session_releases_per_command_workers_and_descriptors",
                "--nocapture",
            ])
            .env(COUNT_OWNER, "1")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let mut child = ReapedChild::spawn(&mut command);
        assert!(
            child.wait_with_deadline(Duration::from_secs(30)).success(),
            "isolated resource-count process failed"
        );
        return;
    }
    let root = fixture();
    let session = sandbox(root.path(), "basic", false, &[]);
    let baseline = resource_counts();
    for _ in 0..24 {
        output(&session, root.path());
    }
    assert_eq!(
        resource_counts(),
        baseline,
        "per-command resources leaked across reuse"
    );
    session.close();
}

fn wait_for(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn process_running(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(") ")
                .map(|(_, tail)| !tail.starts_with('Z'))
        })
        .unwrap_or(false)
}

fn wait_for_exit(pid: i32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while process_running(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if process_running(pid) {
        unsafe { libc::kill(pid, libc::SIGKILL) };
        panic!("sandbox command {pid} survived owner loss/cancellation");
    }
}

fn resource_counts() -> (usize, usize) {
    (
        std::fs::read_dir("/proc/self/fd").unwrap().count(),
        std::fs::read_dir("/proc/self/task").unwrap().count(),
    )
}

/// A test-owned ordinary process.  Unlike `PreparedChild`, `std::process::Child` does not reap
/// itself on drop; this guard therefore makes every early assertion path terminate and collect it.
struct ReapedChild {
    child: Option<Child>,
}

impl ReapedChild {
    fn spawn(command: &mut Command) -> Self {
        Self {
            child: Some(command.spawn().expect("spawns ordinary test child")),
        }
    }

    fn wait_with_deadline(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            let child = self.child.as_mut().expect("child is still owned");
            if let Some(status) = child.try_wait().expect("checks ordinary child") {
                self.child = None;
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "ordinary child exceeded {timeout:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn kill_and_wait(&mut self) {
        let mut child = self.child.take().expect("child is still owned");
        if child
            .try_wait()
            .expect("checks owner before kill")
            .is_none()
        {
            child.kill().expect("kills test owner");
        }
        child.wait().expect("reaps test owner");
    }
}

impl Drop for ReapedChild {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

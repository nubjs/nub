#![cfg(target_os = "macos")]

//! Black-box macOS production-readiness probes for the reusable public sandbox API.
//!
//! This intentionally uses only `compile`, `Sandbox`, and `CommandSpec`: the child is a
//! fresh invocation of this integration-test binary, so every assertion below exercises the
//! compiled Seatbelt profile rather than private profile-builder seams.

#[path = "common/tool_output.rs"]
mod tool_output;

use base64::Engine as _;
use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CASE: &str = "NUB_PRODUCTION_MACOS_CASE";
const ROOT: &str = "NUB_PRODUCTION_MACOS_ROOT";
const PARENT_PID: &str = "NUB_PRODUCTION_MACOS_PARENT_PID";
const ENV_CANARY: &str = "NUB_PRODUCTION_MACOS_ENV_CANARY";
const ENV_CANARY_VALUE: &str = "macos-seatbelt-procargs2-canary";

#[test]
fn production_macos_child_reentry() {
    let Some(case) = std::env::var_os(CASE) else {
        return;
    };
    let root = PathBuf::from(std::env::var_os(ROOT).expect("fixture root"));
    match case.to_string_lossy().as_ref() {
        "filesystem" => child_filesystem(&root),
        "tmp-alias" => child_tmp_alias(&root),
        "repeat" => child_repeat(&root),
        "network-deny" => child_network_deny(&root),
        "network-proxy" => child_network_proxy(&root),
        "worker" => child_worker(&root),
        "owner-crash" => child_owner_crash(&root),
        "tmp-owner" => child_tmp_owner(&root),
        "secret-holder" => child_secret_holder(&root),
        "sleep" => loop {
            std::thread::park();
        },
        "environment" => child_environment(&root),
        other => panic!("unknown macOS production fixture {other}"),
    }
}

#[test]
fn public_api_enforces_filesystem_alias_environment_and_network_contracts() {
    let root = fixture();
    let project = root.path().join("project");
    let exact = root.path().join("exact-file");
    let sibling = root.path().join("sibling-file");
    let exact_dir = root.path().join("exact-dir");
    let tree = root.path().join("tree");
    let withheld = root.path().join("withheld");
    let secret = withheld.join("secret");
    for path in [&project, &exact_dir, &tree, &withheld] {
        fs::create_dir_all(path).unwrap();
    }
    fs::write(&exact, b"exact").unwrap();
    fs::write(&sibling, b"sibling").unwrap();
    fs::write(exact_dir.join("inside"), b"inside").unwrap();
    fs::write(tree.join("existing"), b"tree").unwrap();
    fs::write(&secret, b"not-granted").unwrap();
    std::os::unix::fs::symlink(&secret, tree.join("secret-link")).unwrap();
    fs::hard_link(&secret, tree.join("secret-hard-link")).unwrap();
    std::os::unix::fs::symlink(&secret, tree.join("replaceable")).unwrap();

    let filesystem_policy = json!({
        (project.to_string_lossy()): "rw",
        (exact.to_string_lossy()): "rw",
        (exact_dir.to_string_lossy()): "rw",
        (tree.to_string_lossy()): "rw",
    });
    let sandbox = Sandbox::acquire(&policy(
        root.path(),
        filesystem_policy,
        json!(false),
        "filesystem",
    ))
    .expect("filesystem sandbox");
    run(&sandbox, root.path(), "filesystem");
    assert_eq!(
        fs::read(tree.join("later/nested/output")).unwrap(),
        b"later"
    );
    assert_eq!(fs::read(&secret).unwrap(), b"not-granted");

    // Reuse must not re-open a spelling the host replaced after acquisition.  Linux binds this
    // through an O_PATH rule; macOS emits the policy's already-canonicalized path, so exercise the
    // same public contract against Seatbelt rather than assuming that representation is equivalent.
    fs::rename(&tree, root.path().join("displaced-tree")).unwrap();
    std::os::unix::fs::symlink(&withheld, &tree).unwrap();
    fs::write(project.join("replacement-ready"), b"yes").unwrap();
    run(&sandbox, root.path(), "filesystem");

    // `$TMPDIR` aliases `/private/tmp` through a firmlink.  The compiler must normalize the
    // policy spelling before Seatbelt evaluates the path, in both directions.
    let first = tempfile::Builder::new()
        .prefix("nub-production-macos-private-")
        .tempfile_in("/private/tmp")
        .expect("private tmp fixture");
    let second = tempfile::Builder::new()
        .prefix("nub-production-macos-tmp-")
        .tempfile_in("/private/tmp")
        .expect("tmp fixture");
    fs::write(first.path(), b"private-spelling").unwrap();
    fs::write(second.path(), b"tmp-spelling").unwrap();
    fs::write(
        project.join("tmp-alias-paths"),
        format!(
            "/tmp/{}\n/private/tmp/{}\n",
            first.path().file_name().unwrap().to_string_lossy(),
            second.path().file_name().unwrap().to_string_lossy()
        ),
    )
    .unwrap();
    let aliases = json!({
        (project.to_string_lossy()): "rw",
        (format!("/tmp/{}", first.path().file_name().unwrap().to_string_lossy())): "r",
        (format!("/private/tmp/{}", second.path().file_name().unwrap().to_string_lossy())): "r",
    });
    let alias_sandbox = Sandbox::new(&policy(root.path(), aliases, json!(false), "tmp-alias"))
        .expect("alias sandbox");
    run(&alias_sandbox, root.path(), "tmp-alias");

    let network_policy = json!({(project.to_string_lossy()): "rw"});
    let deny = Sandbox::new(&policy(
        root.path(),
        network_policy.clone(),
        json!(false),
        "network-deny",
    ))
    .expect("net:false sandbox");
    let listener = TcpListener::bind("127.0.0.1:0").expect("local listener");
    fs::write(
        project.join("network-address"),
        listener.local_addr().unwrap().to_string(),
    )
    .unwrap();
    run(&deny, root.path(), "network-deny");

    let allowed = Sandbox::new(&policy(
        root.path(),
        network_policy,
        json!(["localhost", "!blocked.invalid"]),
        "network-proxy",
    ))
    .expect("host-filtered sandbox");
    fs::write(
        project.join("network-address"),
        listener.local_addr().unwrap().to_string(),
    )
    .unwrap();
    run(&allowed, root.path(), "network-proxy");

    let secret_holder = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "production_macos_child_reentry", "--nocapture"])
        .env(CASE, "secret-holder")
        .env(ROOT, root.path())
        .env(ENV_CANARY, ENV_CANARY_VALUE)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("secret-holder starts");
    let secret_holder = ReapChild(secret_holder);
    wait_for_file(
        &project.join("secret-holder-ready"),
        "secret-holder readiness",
    );
    let parent_secret = format!("{ENV_CANARY}={ENV_CANARY_VALUE}");
    assert!(
        contains_bytes(
            &procargs2(secret_holder.0.id() as i32).expect("host procargs2 positive control"),
            parent_secret.as_bytes(),
        ),
        "host procargs2 control did not expose its same-uid environment canary",
    );

    let mut environment_policy = policy(
        root.path(),
        json!({(project.to_string_lossy()): "rw"}),
        json!(false),
        "environment",
    );
    environment_policy
        .env
        .constructed
        .insert(PARENT_PID.into(), secret_holder.0.id().to_string());
    let environment = Sandbox::new(&environment_policy).expect("environment sandbox");
    run(&environment, root.path(), "environment");
}

#[test]
fn public_api_retains_session_resources_and_reaps_normal_descendants() {
    let root = fixture();
    let project = root.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let session_fs = json!({(project.to_string_lossy()): "rw", "$tmp": "rw"});
    let repeat_policy = policy(root.path(), session_fs.clone(), json!(false), "repeat");

    let first = Sandbox::acquire(&repeat_policy).expect("first session");
    run(&first, root.path(), "repeat");
    let first_tmp = temp_path(&project);
    run(&first, root.path(), "repeat");
    assert_eq!(
        temp_path(&project),
        first_tmp,
        "one session must retain its private temp"
    );

    let second = Sandbox::acquire(&repeat_policy).expect("second session");
    run(&second, root.path(), "repeat");
    let second_tmp = temp_path(&project);
    assert_ne!(
        first_tmp, second_tmp,
        "independent Unix sessions must not share private temp"
    );
    first.close();
    assert!(!first_tmp.exists(), "closed session retained private temp");
    assert!(
        second_tmp.exists(),
        "closing one session removed another session's temp"
    );
    second.close();
    assert!(
        !second_tmp.exists(),
        "second closed session retained private temp"
    );

    let owner_policy = policy(root.path(), session_fs, json!(false), "worker");
    let owner = Sandbox::new(&owner_policy).expect("owner sandbox");
    let worker = owner
        .prepare(command(root.path(), "worker"))
        .expect("worker prepares")
        .spawn()
        .expect("worker starts");
    let pid_path = project.join("normal-descendant-pid");
    let pid = wait_for_pid(&pid_path, "normal descendant readiness");
    drop(worker); // cancellation must terminate the launch-owned process group.
    assert_dead(
        pid,
        "normal descendant survived prepared-child cancellation",
    );
    owner.close();

    fs::remove_file(&pid_path).expect("remove prior descendant readiness");
    let crashed_owner = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "production_macos_child_reentry", "--nocapture"])
        .env(CASE, "owner-crash")
        .env(ROOT, root.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("crash owner starts");
    let mut crashed_owner = ReapChild(crashed_owner);
    let owner_pid = wait_for_pid(&pid_path, "owner-loss descendant readiness");
    crashed_owner.kill_and_wait();
    assert_dead(owner_pid, "normal descendant survived owner loss");

    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "production_macos_child_reentry", "--nocapture"])
        .env(CASE, "tmp-owner")
        .env(ROOT, root.path())
        .status()
        .expect("crashed temp owner starts");
    assert_eq!(status.code(), Some(91));
    let crashed_tmp = temp_path(&project);
    nub_sandbox::cleanup().expect("private temp recovery");
    assert!(
        !crashed_tmp.exists(),
        "cleanup retained crashed session private temp"
    );
}

fn child_filesystem(root: &Path) {
    let project = root.join("project");
    if project.join("replacement-ready").exists() {
        assert!(
            fs::read(root.join("tree/secret")).is_err(),
            "post-acquisition replacement exposed withheld secret"
        );
        return;
    }
    assert_eq!(fs::read(root.join("exact-file")).unwrap(), b"exact");
    assert!(fs::write(root.join("exact-file"), b"updated").is_ok());
    assert!(
        fs::read(root.join("sibling-file")).is_err(),
        "exact file granted sibling"
    );
    // Public grammar expands an ordinary directory spelling to the node plus its subtree.
    assert!(
        fs::metadata(root.join("exact-dir")).is_ok(),
        "directory node denied"
    );
    assert_eq!(fs::read(root.join("exact-dir/inside")).unwrap(), b"inside");
    assert_eq!(fs::read(root.join("tree/existing")).unwrap(), b"tree");
    fs::create_dir_all(root.join("tree/later/nested")).unwrap();
    fs::write(root.join("tree/later/nested/output"), b"later").unwrap();
    assert!(
        fs::read(root.join("tree/secret-link")).is_err(),
        "symlink escaped tree grant"
    );
    // A hard link is a permitted name in the granted tree; it is not a path-resolution escape.
    assert_eq!(
        fs::read(root.join("tree/secret-hard-link")).unwrap(),
        b"not-granted"
    );
    assert!(
        fs::read(root.join("tree/replaceable")).is_err(),
        "replacement symlink escaped tree grant"
    );
    assert!(fs::write(project.join("proof"), b"project").is_ok());
}

fn child_tmp_alias(root: &Path) {
    let paths = fs::read_to_string(root.join("project/tmp-alias-paths")).unwrap();
    let mut paths = paths.lines();
    assert_eq!(
        fs::read(paths.next().unwrap()).unwrap(),
        b"private-spelling"
    );
    assert_eq!(fs::read(paths.next().unwrap()).unwrap(), b"tmp-spelling");
}

fn child_repeat(root: &Path) {
    let tmp = std::env::temp_dir();
    let marker = tmp.join("production-readiness-marker");
    if marker.exists() {
        assert_eq!(fs::read(&marker).unwrap(), b"retained");
    } else {
        fs::write(&marker, b"retained").unwrap();
    }
    fs::write(
        root.join("project/private-tmp-path"),
        tmp.to_string_lossy().as_bytes(),
    )
    .unwrap();
}

fn child_network_deny(root: &Path) {
    let target = fs::read_to_string(root.join("project/network-address")).unwrap();
    assert!(
        TcpStream::connect_timeout(&target.parse().unwrap(), Duration::from_secs(2)).is_err(),
        "net:false permitted direct loopback TCP"
    );
}

fn child_network_proxy(root: &Path) {
    let target = fs::read_to_string(root.join("project/network-address")).unwrap();
    assert!(
        TcpStream::connect_timeout(&target.parse().unwrap(), Duration::from_secs(2)).is_err(),
        "host-filtered policy permitted bypassing its proxy"
    );
    let proxy = std::env::var("HTTP_PROXY").expect("host-filtered policy supplies HTTP proxy");
    let userinfo_and_address = proxy.strip_prefix("http://").expect("HTTP proxy URL");
    let (token, address) = userinfo_and_address
        .split_once('@')
        .expect("proxy URL carries its session bearer");
    let address = address.parse().expect("proxy socket address");
    let authorization = base64::engine::general_purpose::STANDARD.encode(format!("{token}:"));
    let mut allowed =
        TcpStream::connect_timeout(&address, Duration::from_secs(2)).expect("proxy reachable");
    set_socket_deadline(&allowed);
    allowed
        .write_all(format!("CONNECT localhost:{port} HTTP/1.1\r\nHost: localhost:{port}\r\nProxy-Authorization: Basic {authorization}\r\n\r\n", port = target.rsplit_once(':').unwrap().1).as_bytes())
        .unwrap();
    let mut response = [0; 512];
    let length = allowed.read(&mut response).unwrap();
    assert!(
        std::str::from_utf8(&response[..length])
            .unwrap()
            .starts_with("HTTP/1.1 200"),
        "allowed host was not tunneled"
    );
    let mut denied = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .expect("proxy still reachable");
    set_socket_deadline(&denied);
    denied
        .write_all(format!("CONNECT blocked.invalid:443 HTTP/1.1\r\nHost: blocked.invalid:443\r\nProxy-Authorization: Basic {authorization}\r\n\r\n").as_bytes())
        .unwrap();
    let length = denied.read(&mut response).unwrap();
    assert!(
        !std::str::from_utf8(&response[..length])
            .unwrap()
            .starts_with("HTTP/1.1 200"),
        "denied host was tunneled"
    );
}

fn child_environment(_root: &Path) {
    assert!(
        std::env::var_os("HOME").is_none(),
        "unlisted HOME reached child environment"
    );
    let parent = std::env::var(PARENT_PID).unwrap();
    let parent_secret = format!("{ENV_CANARY}={ENV_CANARY_VALUE}");
    match procargs2(parent.parse().expect("parent pid")) {
        Ok(bytes) => assert!(
            !contains_bytes(&bytes, parent_secret.as_bytes()),
            "child recovered an unlisted parent environment value through procargs2"
        ),
        Err(error) => assert_eq!(
            error.kind(),
            io::ErrorKind::PermissionDenied,
            "procargs2 must either deny the sibling environment read or omit its canary: {error}"
        ),
    }
}

fn child_secret_holder(root: &Path) {
    assert_eq!(std::env::var(ENV_CANARY).unwrap(), ENV_CANARY_VALUE);
    fs::write(root.join("project/secret-holder-ready"), b"ready").unwrap();
    loop {
        std::thread::park();
    }
}

fn child_worker(root: &Path) {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "production_macos_child_reentry", "--nocapture"])
        .env(CASE, "sleep")
        .spawn()
        .expect("normal descendant starts");
    fs::write(
        root.join("project/normal-descendant-pid"),
        child.id().to_string(),
    )
    .unwrap();
    child.wait().expect("normal descendant is reaped");
    panic!("normal descendant exited before its owner was terminated");
}

fn child_owner_crash(root: &Path) {
    let project = root.join("project");
    let policy = policy(
        root,
        json!({(project.to_string_lossy()): "rw", "$tmp": "rw"}),
        json!(false),
        "worker",
    );
    let sandbox = Sandbox::new(&policy).expect("owner sandbox");
    let _worker = sandbox
        .prepare(command(root, "worker"))
        .expect("owner worker prepares")
        .spawn()
        .expect("owner worker starts");
    loop {
        std::thread::park();
    }
}

fn child_tmp_owner(root: &Path) {
    let project = root.join("project");
    let policy = policy(
        root,
        json!({(project.to_string_lossy()): "rw", "$tmp": "rw"}),
        json!(false),
        "repeat",
    );
    let sandbox = Sandbox::new(&policy).expect("temp owner sandbox");
    run(&sandbox, root, "repeat");
    std::process::exit(91);
}

fn fixture() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("nub-production-macos-")
        .tempdir_in(std::env::var_os("HOME").expect("HOME"))
        .unwrap()
}

fn policy(root: &Path, fs: Value, net: Value, case: &str) -> nub_sandbox::SandboxPolicy {
    let project = root.join("project");
    let ambient = BTreeMap::from([
        (CASE.into(), case.into()),
        (ROOT.into(), root.to_string_lossy().into_owned()),
        (PARENT_PID.into(), std::process::id().to_string()),
        (
            "HOME".into(),
            std::env::var("HOME").expect("parent HOME in ambient snapshot"),
        ),
    ]);
    let ctx = CompileCtx::new(
        Homes {
            home: root.join("home"),
            cache: root.join("cache"),
            tmp: root.join("tmp"),
            project: project.clone(),
        },
        project,
        ScopeCapabilities::approved(),
        ambient,
    );
    compile(
        &json!({"fs": fs, "net": net, "vars": {(CASE): true, (ROOT): true, (PARENT_PID): true}}),
        &ctx,
    )
    .expect("production policy compiles")
}

fn command(root: &Path, case: &str) -> CommandSpec {
    let mut command = CommandSpec::new(std::env::current_exe().unwrap())
        .args(["--exact", "production_macos_child_reentry", "--nocapture"])
        .cwd(root.join("project"))
        .redact_stdout(true)
        .redact_stderr(true);
    command = command.reap_descendants(true);
    command.audit_label(format!("production-macos-{case}"))
}

fn run(sandbox: &Sandbox, root: &Path, case: &str) {
    let prepared = sandbox
        .prepare(command(root, case))
        .expect("command prepares");
    assert!(
        prepared.degradation.is_full(),
        "macOS command degraded: {:?}",
        prepared.degradation
    );
    let output = tool_output::output(prepared);
    assert!(
        output.status.success(),
        "{case} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn set_socket_deadline(stream: &TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("proxy socket read deadline");
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("proxy socket write deadline");
}

fn procargs2(pid: i32) -> io::Result<Vec<u8>> {
    // CTL_KERN/KERN_PROCARGS2 are the Darwin MIB used by `ps` to obtain argv+environment.
    const CTL_KERN: libc::c_int = 1;
    const KERN_PROCARGS2: libc::c_int = 49;
    let mut mib = [CTL_KERN, KERN_PROCARGS2, pid];
    let mut len = 0usize;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut bytes = vec![0; len];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            bytes.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    bytes.truncate(len);
    Ok(bytes)
}

fn contains_bytes(bytes: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && bytes.windows(needle.len()).any(|window| window == needle)
}

struct ReapChild(std::process::Child);

impl ReapChild {
    fn kill_and_wait(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for ReapChild {
    fn drop(&mut self) {
        self.kill_and_wait();
    }
}

fn temp_path(project: &Path) -> PathBuf {
    PathBuf::from(fs::read_to_string(project.join("private-tmp-path")).unwrap())
}

fn wait_for_pid(path: &Path, what: &str) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    fs::read_to_string(path)
        .unwrap_or_else(|_| panic!("{what}"))
        .trim()
        .parse()
        .unwrap()
}

fn wait_for_file(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(path.exists(), "{what}");
}

fn assert_dead(pid: i32, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    if alive {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    assert!(!alive, "{message}");
}

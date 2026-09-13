//! Real Sandbox -> Prepared -> PreparedChild ownership fixture for a private
//! mounted projection. This is deliberately one narrow vertical: policy
//! admission remains disabled and the fixture requires the namespace/FUSE
//! capabilities its mount actually uses.

use super::ProjectedSession;
use super::namespace::{NamespacePair, ProjectionMountPaths};
use super::session::AcquireFault;
use crate::backend::{CommandSpec, Prepared, PreparedChild, PreparedSignalTarget, Sandbox};
use crate::policy::{
    CanonGlob, Effect, EnvPolicy, FsAccess, FsOrigin, FsRule, FsRuleSet, NetPolicy, SandboxPolicy,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

const HELPER: &str = "backend::linux_projection::session_tests::projected_session_helper";
const ROLE: &str = "NUB_PROJECTED_SESSION_ROLE";
const ENV_MARKER: &str = "NUB_PROJECTED_SESSION_ENV";
const ENV_VALUE: &str = "constructed-session-env";
const TOPOLOGY_ALLOWED: &str = "NUB_PROJECTED_TOPOLOGY_ALLOWED";
const TOPOLOGY_DENIED: &str = "NUB_PROJECTED_TOPOLOGY_DENIED";

struct Fixture {
    root: tempfile::TempDir,
    source: PathBuf,
    rules: FsRuleSet,
}

fn rule(path: impl Into<String>, access: FsAccess) -> FsRule {
    FsRule {
        matcher: CanonGlob(path.into()),
        effect: Effect::Allow,
        access,
        origin: FsOrigin::Authored,
    }
}

fn copy(source: &Path, destination: &Path) {
    fs::create_dir_all(destination.parent().expect("fixture destination parent"))
        .expect("fixture destination directory");
    fs::copy(source, destination).expect("fixture copy");
}

fn set_test_xattr(path: &Path, value: &[u8]) -> io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: both strings are terminated and value remains live throughout.
    let result = unsafe {
        libc::lsetxattr(
            path.as_ptr(),
            c"user.projected_session".as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn get_test_xattr(path: &Path) -> io::Result<Vec<u8>> {
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let mut bytes = vec![0u8; 32];
    // SAFETY: terminated strings and a live writable buffer of the given size.
    let count = unsafe {
        libc::lgetxattr(
            path.as_ptr(),
            c"user.projected_session".as_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
        )
    };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    bytes.truncate(count as usize);
    Ok(bytes)
}

fn executable_closure(executable: &Path) -> BTreeSet<PathBuf> {
    let mut closure = BTreeSet::new();
    let mut pending = vec![executable.to_owned()];
    while let Some(binary) = pending.pop() {
        let output = Command::new("ldd")
            .arg(&binary)
            .output()
            .expect("fixture ldd invocation");
        assert!(
            output.status.success(),
            "ldd {}: {output:?}",
            binary.display()
        );
        for word in String::from_utf8(output.stdout)
            .expect("ldd output is utf-8")
            .split_whitespace()
        {
            if word.starts_with('/') {
                let library = PathBuf::from(word);
                if closure.insert(library.clone()) {
                    pending.push(library);
                }
            }
        }
    }
    closure
}

fn fixture_path_targets(grants: &mut BTreeSet<PathBuf>, path: &Path, links_left: u8) {
    assert!(path.is_absolute(), "fixture closure path must be absolute");
    let mut current = PathBuf::from("/");
    let mut components = path.components();
    while let Some(component) = components.next() {
        match component {
            Component::RootDir | Component::CurDir => continue,
            Component::ParentDir => {
                current.pop();
                continue;
            }
            Component::Normal(name) => current.push(name),
            Component::Prefix(_) => unreachable!("Linux fixture path prefix"),
        }
        if fs::symlink_metadata(&current)
            .expect("fixture closure component")
            .file_type()
            .is_symlink()
        {
            assert!(links_left > 0, "fixture closure symlink limit");
            grants.insert(current.clone());
            let target = fs::read_link(&current).expect("fixture closure symlink");
            let mut target = if target.is_absolute() {
                target
            } else {
                current.parent().unwrap().join(target)
            };
            target.push(components.as_path());
            fixture_path_targets(grants, &target, links_left - 1);
            return;
        }
    }
    grants.insert(current);
}

#[test]
fn fixture_path_grants_include_symlink_components_and_relative_targets() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(root.path()).unwrap();
    fs::create_dir_all(root.join("usr/lib64")).unwrap();
    fs::create_dir_all(root.join("usr/lib")).unwrap();
    fs::write(root.join("usr/lib/loader"), b"loader").unwrap();
    symlink("usr/lib64", root.join("lib64")).unwrap();
    symlink("../lib/loader", root.join("usr/lib64/loader")).unwrap();
    let mut grants = BTreeSet::new();
    fixture_path_targets(&mut grants, &root.join("lib64/loader"), 40);
    assert_eq!(
        grants,
        BTreeSet::from([
            root.join("lib64"),
            root.join("usr/lib64/loader"),
            root.join("usr/lib/loader"),
        ])
    );
}

fn fixture(executable: &Path) -> Fixture {
    let root = tempfile::tempdir().expect("fixture root");
    let source = root.path().join("source");
    let app = source.join("app");
    fs::create_dir_all(&app).expect("fixture app directory");

    // This is a disposable test-binary closure, not executable discovery or a
    // production policy grant. The copied binary reaches its interpreter and
    // shared objects only through the projected namespace.
    let closure = executable_closure(executable);

    copy(executable, &app.join("run"));
    fs::set_permissions(app.join("run"), fs::Permissions::from_mode(0o755))
        .expect("fixture executable mode");
    let mut entries = vec![rule("/app/run", FsAccess::Read)];
    for library in closure {
        copy(
            &library,
            &source.join(library.strip_prefix("/").expect("absolute library")),
        );
        entries.push(rule(library.to_string_lossy().into_owned(), FsAccess::Read));
    }
    for (name, bytes, access) in [
        ("allowed.txt", b"allowed-path".as_slice(), FsAccess::Read),
        (
            "writable.txt",
            b"writable-path".as_slice(),
            FsAccess::ReadWrite,
        ),
        ("denied.txt", b"denied-path".as_slice(), FsAccess::Read),
        ("role", b"idle".as_slice(), FsAccess::Read),
        ("release", b"hold".as_slice(), FsAccess::Read),
    ] {
        fs::write(app.join(name), bytes).expect("fixture leaf");
        if name != "denied.txt" {
            entries.push(rule(format!("/app/{name}"), access));
        }
    }
    for name in ["allowed.txt", "writable.txt"] {
        set_test_xattr(&app.join(name), b"raw-control").expect("raw host xattr control");
        assert_eq!(get_test_xattr(&app.join(name)).unwrap(), b"raw-control");
    }

    Fixture {
        root,
        source,
        rules: FsRuleSet {
            entries,
            default_effect: Effect::Deny,
        },
    }
}

fn source_for_fault() -> Fixture {
    let root = tempfile::tempdir().expect("fault fixture root");
    let source = root.path().join("source");
    fs::create_dir_all(source.join("app")).expect("fault fixture app");
    fs::write(source.join("app/allowed.txt"), b"allowed-path").expect("fault fixture leaf");
    Fixture {
        root,
        source,
        rules: FsRuleSet {
            entries: vec![rule("/app/allowed.txt", FsAccess::Read)],
            default_effect: Effect::Deny,
        },
    }
}

fn policy(rules: FsRuleSet) -> SandboxPolicy {
    let mut constructed = BTreeMap::new();
    constructed.insert("PATH".into(), "/usr/bin:/bin".into());
    constructed.insert(ROLE.into(), "mounted-session".into());
    constructed.insert(ENV_MARKER.into(), ENV_VALUE.into());
    let mut policy = SandboxPolicy::default();
    policy.fs.rules = rules;
    policy.env = EnvPolicy::resolved(constructed);
    policy.env.enforce = true;
    policy.net = NetPolicy {
        enforce: true,
        default_effect: Effect::Deny,
        ..NetPolicy::default()
    };
    policy
}

fn root_topology_policy(
    executable: &Path,
    allowed: &Path,
    denied: &Path,
    staging_root: &Path,
) -> SandboxPolicy {
    // The staging entry is allocated by ProjectedSession. This explicit grant
    // lets the child use it as cwd without granting the denied sentinel.
    let staging = staging_root.to_string_lossy();
    let mut entries = vec![
        rule(executable.to_string_lossy().into_owned(), FsAccess::Read),
        rule(staging.to_string(), FsAccess::Read),
        rule(
            format!("{}/**", staging.trim_end_matches('/')),
            FsAccess::Read,
        ),
        rule(allowed.to_string_lossy().into_owned(), FsAccess::Read),
    ];
    assert!(
        !entries
            .iter()
            .any(|entry| entry.matcher.as_str() == denied.to_string_lossy()),
        "root topology policy accidentally grants denied sentinel"
    );
    // Unlike the copied fixture, source=/ preserves the host's symlink topology.
    // Grant only this ELF closure's link nodes and targets, not system subtrees.
    let mut closure = executable_closure(executable);
    closure.insert(executable.to_owned());
    let mut targets = BTreeSet::new();
    for path in &closure {
        fixture_path_targets(&mut targets, path, 40);
    }
    closure.extend(targets);
    entries.extend(
        closure
            .into_iter()
            .map(|path| rule(path.to_string_lossy().into_owned(), FsAccess::Read)),
    );

    let mut constructed = BTreeMap::new();
    constructed.insert("PATH".into(), "/usr/bin:/bin".into());
    constructed.insert(ROLE.into(), "root-topology".into());
    constructed.insert(
        TOPOLOGY_ALLOWED.into(),
        allowed.to_string_lossy().into_owned(),
    );
    constructed.insert(
        TOPOLOGY_DENIED.into(),
        denied.to_string_lossy().into_owned(),
    );
    let mut policy = SandboxPolicy::default();
    policy.fs.rules = FsRuleSet {
        entries,
        default_effect: Effect::Deny,
    };
    policy.env = EnvPolicy::resolved(constructed);
    policy.env.enforce = true;
    policy.net = NetPolicy {
        enforce: true,
        default_effect: Effect::Deny,
        ..NetPolicy::default()
    };
    policy
}

fn command() -> CommandSpec {
    CommandSpec::new("/app/run")
        .args(["--exact", HELPER, "--nocapture", "--test-threads=1"])
        .cwd("/")
        .redact_stdout(true)
        .redact_stderr(true)
}

fn root_topology_command(executable: &Path, staging: &Path) -> CommandSpec {
    CommandSpec::new(executable)
        .args(["--exact", HELPER, "--nocapture", "--test-threads=1"])
        // The projected launch applies cwd after chroot. This gives the helper
        // the precise dynamically allocated staging path without expanding the
        // command/environment surface just for test instrumentation.
        .cwd(staging)
        .redact_stdout(true)
        .redact_stderr(true)
}

fn write_role(fixture: &Fixture, role: &str) {
    fs::write(fixture.source.join("app/role"), role).expect("fixture role");
}

fn read_marker(reader: &mut impl BufRead, marker: &str) {
    loop {
        let mut line = String::new();
        assert_ne!(
            reader.read_line(&mut line).expect("child stdout"),
            0,
            "missing {marker}"
        );
        if line.trim() == marker {
            return;
        }
    }
}

fn spawn_ready(prepared: Prepared) -> PreparedChild {
    let mut target = None;
    let child = prepared
        .spawn_with_signal_target(|signal| {
            let PreparedSignalTarget::Direct(group) = signal;
            assert!(group < 0, "projected child needs guardian process group");
            target = Some(group);
            Ok(())
        })
        .expect("projected Prepared spawn");
    assert!(
        target.is_some(),
        "ready barrier did not expose guardian target"
    );
    child
}

fn parent_network_control() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("parent listener");
    listener
        .set_nonblocking(true)
        .expect("parent listener nonblocking");
    let address = listener.local_addr().expect("parent listener address");
    thread::scope(|scope| {
        let server = scope.spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut accepted = loop {
                match listener.accept() {
                    Ok((accepted, _)) => break accepted,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return Err(io::Error::new(io::ErrorKind::TimedOut, "parent accept"));
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => return Err(error),
                }
            };
            accepted.set_write_timeout(Some(Duration::from_secs(2)))?;
            accepted
                .write_all(b"parent-network-control")
                .map_err(|error| io::Error::new(error.kind(), format!("parent reply: {error}")))
        });
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
            .expect("parent allowed network control");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("parent control read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .expect("parent control write timeout");
        let mut bytes = String::new();
        stream
            .read_to_string(&mut bytes)
            .expect("parent control read");
        server
            .join()
            .expect("parent network thread")
            .expect("parent network control");
        assert_eq!(bytes, "parent-network-control");
    });
    println!("PROJECTED_SESSION_PARENT_NETWORK_CONTROL_OK");
}

fn denied_network() {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd >= 0 {
        unsafe {
            libc::close(fd);
        }
        panic!("deny-all projected child unexpectedly created an IP socket");
    }
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::EPERM),
        "deny-all projected child network errno"
    );
}

fn child_contract() {
    assert_eq!(std::env::var(ENV_MARKER).as_deref(), Ok(ENV_VALUE));
    assert!(std::env::var("NUB_PROJECTED_SESSION_UNSET").is_err());
    assert!(std::env::var_os("HOME").is_none(), "ambient HOME leaked");
    assert_eq!(
        fs::read("/app/allowed.txt").expect("allowed projected leaf"),
        b"allowed-path"
    );
    let denied = fs::read("/app/denied.txt").expect_err("denied projected leaf opened");
    assert!(
        matches!(denied.raw_os_error(), Some(libc::EACCES | libc::ENOENT)),
        "denied projected leaf errno: {denied}"
    );
    assert_eq!(
        get_test_xattr(Path::new("/app/allowed.txt")).expect("R path xattr read"),
        b"raw-control"
    );
    let denied = set_test_xattr(Path::new("/app/allowed.txt"), b"denied")
        .expect_err("R path xattr mutation succeeded");
    assert_eq!(denied.raw_os_error(), Some(libc::EACCES));
    set_test_xattr(Path::new("/app/writable.txt"), b"projected-write")
        .expect("RW path xattr mutation");
    assert_eq!(
        get_test_xattr(Path::new("/app/writable.txt")).expect("RW path xattr read"),
        b"projected-write"
    );
    denied_network();
}

fn root_topology_contract() {
    let allowed = PathBuf::from(std::env::var_os(TOPOLOGY_ALLOWED).expect("topology allowed path"));
    let denied = PathBuf::from(std::env::var_os(TOPOLOGY_DENIED).expect("topology denied path"));
    assert_eq!(
        fs::read(&allowed).expect("root topology allowed sentinel"),
        b"allowed"
    );
    let denied_error = fs::read(&denied).expect_err("root topology denied sentinel opened");
    assert!(
        matches!(
            denied_error.raw_os_error(),
            Some(libc::EACCES | libc::ENOENT)
        ),
        "root topology denied sentinel errno: {denied_error}"
    );
}

/// Bounded subprocess role run from the copied test ELF inside the projection.
#[test]
fn projected_session_helper() {
    match std::env::var(ROLE).as_deref() {
        Ok("mounted-session") => {
            child_contract();
            match fs::read_to_string("/app/role")
                .expect("projected role")
                .trim()
            {
                "hold" => {
                    // libtest's in-progress prefix has no trailing newline.
                    println!("\nPROJECTED_SESSION_HELD_READY");
                    io::stdout().flush().expect("held marker flush");
                    loop {
                        unsafe {
                            libc::pause();
                        }
                    }
                }
                "survive" => {
                    println!("\nPROJECTED_SESSION_SURVIVOR_READY");
                    io::stdout().flush().expect("survivor marker flush");
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while fs::read("/app/release").expect("projected release") != b"release" {
                        assert!(
                            Instant::now() < deadline,
                            "projected survivor release timed out"
                        );
                        thread::sleep(Duration::from_millis(10));
                    }
                    eprintln!("PROJECTED_SESSION_STDERR_OK");
                    println!("PROJECTED_SESSION_SURVIVOR_REAP_OK");
                }
                "final" => {
                    eprintln!("PROJECTED_SESSION_FINAL_STDERR_OK");
                    println!("PROJECTED_SESSION_FINAL_REAP_OK");
                }
                role => panic!("unknown projected session role {role:?}"),
            }
        }
        Ok("root-topology") => {
            root_topology_contract();
            eprintln!("PROJECTED_SESSION_ROOT_TOPOLOGY_STDERR_OK");
            println!("PROJECTED_SESSION_ROOT_TOPOLOGY_OK");
        }
        _ => (),
    }
}

fn assert_cleanup_ok(observer: &std::sync::Arc<std::sync::Mutex<Option<Result<(), String>>>>) {
    let cleanup = observer.lock().expect("cleanup observer").clone();
    assert!(
        matches!(cleanup, Some(Ok(()))),
        "cleanup result: {cleanup:?}"
    );
}

fn startup_faults_cleanup() {
    for fault in [AcquireFault::AfterMount, AcquireFault::AfterServerReady] {
        let fixture = source_for_fault();
        let (result, observer) =
            ProjectedSession::acquire_with_fault(&fixture.rules, &fixture.source, fault);
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("injected projected acquisition fault unexpectedly succeeded"),
        };
        assert_eq!(
            error.raw_os_error(),
            Some(libc::EIO),
            "fault arm must reach injected EIO"
        );
        assert_cleanup_ok(&observer);
        drop(fixture.root);
    }
    println!("PROJECTED_SESSION_STARTUP_FAULT_CLEANUP_OK");
}

fn failed_cleanup_retains_then_retries() {
    let fixture = source_for_fault();
    let session =
        ProjectedSession::acquire(&fixture.rules, &fixture.source).expect("projected acquisition");
    let observer = session.cleanup_observer();
    // This root descriptor keeps the FUSE mount busy across ordinary and forced
    // cleanup. The retained registry must reject a new acquisition until retry.
    let held = session.launch().expect("held projected launch");
    drop(session);
    let initial = observer.lock().expect("cleanup observer").clone();
    assert!(
        matches!(initial, Some(Err(_))),
        "busy mounted cleanup did not retain its owner: {initial:?}"
    );
    let next = match ProjectedSession::acquire(&fixture.rules, &fixture.source) {
        Err(error) => error,
        Ok(_) => panic!("failed cleanup unexpectedly admitted a new projected session"),
    };
    assert!(
        next.to_string().contains("cleanup requires retry"),
        "new session was refused for the wrong reason: {next}"
    );
    drop(held);
    ProjectedSession::retry_failed_cleanup().expect("explicit cleanup retry");
    assert_cleanup_ok(&observer);
    drop(fixture.root);
    println!("PROJECTED_SESSION_FAILED_CLEANUP_RETRY_OK");
}

fn staging_journal_mismatch_retains_then_retries() {
    let fixture = source_for_fault();
    let mut session =
        ProjectedSession::acquire(&fixture.rules, &fixture.source).expect("projected acquisition");
    let observer = session.cleanup_observer();
    let staging = session.staging_path().to_owned();
    let entry = staging.parent().expect("private staging entry").to_owned();
    let lease = entry.join("lease");
    let original_lease = fs::read(&lease).expect("private staging lease");
    fs::write(&lease, b"[0,0]\n").expect("mismatched staging lease");

    let error = session
        .shutdown()
        .expect_err("mismatched staging journal cleaned");
    assert!(
        error.to_string().contains("ownership identity changed"),
        "staging journal returned the wrong cleanup error: {error}"
    );
    match session.launch() {
        Err(error) => assert_eq!(
            error.raw_os_error(),
            Some(libc::EBADF),
            "mount teardown did not complete before staging journal failure"
        ),
        Ok(_) => panic!("cleanup failure retained a live projected root"),
    }
    assert!(
        staging.exists(),
        "journal failure removed the owned staging before retry"
    );

    fs::write(&lease, original_lease).expect("restore private staging lease");
    session
        .shutdown()
        .expect("restored staging retry after mount teardown");
    assert!(!entry.exists(), "restored owned staging entry remained");
    assert_cleanup_ok(&observer);
    drop(fixture.root);
    println!("PROJECTED_SESSION_STAGING_JOURNAL_RETRY_OK");
}

fn prepared_children_retain_the_mounted_lease() {
    assert!(
        std::env::var_os("HOME").is_some(),
        "ambient environment control"
    );
    parent_network_control();
    let fixture = fixture(&std::env::current_exe().expect("test executable"));
    let sandbox = Sandbox::test_projected(&policy(fixture.rules.clone()), &fixture.source)
        .expect("private projected sandbox acquisition");
    let observer = sandbox.test_projected_cleanup_observer();
    let staging = sandbox.test_projected_staging_path().to_owned();
    assert!(
        staging.is_dir(),
        "projected staging must exist after acquisition"
    );

    let held_prepared = sandbox.prepare(command()).expect("held preparation");
    let survivor_prepared = sandbox.prepare(command()).expect("survivor preparation");
    let final_prepared = sandbox.prepare(command()).expect("retained preparation");

    write_role(&fixture, "hold");
    let mut held = spawn_ready(held_prepared);
    let mut held_stdout = BufReader::new(held.take_stdout().expect("held stdout pipe"));
    read_marker(&mut held_stdout, "PROJECTED_SESSION_HELD_READY");
    drop(held_stdout);

    write_role(&fixture, "survive");
    let mut survivor = spawn_ready(survivor_prepared);
    let mut survivor_stdout = BufReader::new(survivor.take_stdout().expect("survivor stdout pipe"));
    let survivor_stderr = survivor.take_stderr().expect("survivor stderr pipe");
    let stderr_drain = thread::spawn(move || -> io::Result<String> {
        let mut text = String::new();
        BufReader::new(survivor_stderr).read_to_string(&mut text)?;
        Ok(text)
    });
    read_marker(&mut survivor_stdout, "PROJECTED_SESSION_SURVIVOR_READY");

    // The source lease is gone while two child leases and a Prepared lease remain.
    drop(sandbox);
    assert!(
        staging.exists(),
        "Prepared/child leases released projection too early"
    );
    assert!(observer.lock().expect("cleanup observer").is_none());

    // Killing one prepared child cannot dismantle the session used by its sibling.
    drop(held);
    fs::write(fixture.source.join("app/release"), b"release").expect("survivor release");
    assert!(survivor.wait().expect("survivor reap").success());
    let mut survivor_output = String::new();
    survivor_stdout
        .read_to_string(&mut survivor_output)
        .expect("survivor stdout drain");
    let survivor_stderr = stderr_drain
        .join()
        .expect("survivor stderr thread")
        .expect("survivor stderr drain");
    assert!(survivor_output.contains("PROJECTED_SESSION_SURVIVOR_REAP_OK"));
    assert!(survivor_stderr.contains("PROJECTED_SESSION_STDERR_OK"));

    write_role(&fixture, "final");
    let final_output = spawn_ready(final_prepared)
        .wait_with_output()
        .expect("prepared lease final output");
    assert!(final_output.status.success(), "final prepared child failed");
    assert!(
        String::from_utf8_lossy(&final_output.stdout).contains("PROJECTED_SESSION_FINAL_REAP_OK")
    );
    assert!(
        String::from_utf8_lossy(&final_output.stderr).contains("PROJECTED_SESSION_FINAL_STDERR_OK")
    );

    assert!(
        !staging.exists(),
        "final PreparedChild lease did not clean staging"
    );
    assert_cleanup_ok(&observer);
    assert_eq!(
        get_test_xattr(&fixture.source.join("app/allowed.txt")).unwrap(),
        b"raw-control"
    );
    assert_eq!(
        get_test_xattr(&fixture.source.join("app/writable.txt")).unwrap(),
        b"projected-write"
    );
    println!("PROJECTED_SESSION_XATTR_CONTROL_OK");
    println!("PROJECTED_SESSION_PREPARED_LEASE_STDIO_READY_REAP_OK");
}

#[test]
#[ignore = "requires an ordinary Linux user with direct user namespaces and /dev/fuse"]
fn projected_session_real_lifecycle_and_cleanup() {
    // This is deliberately one sequential fixture: project-wide ownership slots
    // make parallel mounted fault/lifetime probes invalid evidence.
    startup_faults_cleanup();
    failed_cleanup_retains_then_retries();
    staging_journal_mismatch_retains_then_retries();
    prepared_children_retain_the_mounted_lease();
}

/// Real `source=/` command, policy, and cleanup coverage.
#[test]
#[ignore = "requires an ordinary Linux user with direct user namespaces and /dev/fuse"]
fn projected_session_source_root_topology_and_cleanup() {
    let fixture = tempfile::tempdir().expect("root topology fixture");
    let staging_root =
        std::env::temp_dir().join(format!("nub-sandbox-tmp-{}", unsafe { libc::geteuid() }));
    assert_ne!(
        staging_root,
        Path::new("/"),
        "root topology fixture needs a bounded private staging root"
    );
    let allowed = fixture.path().join("allowed");
    let denied = PathBuf::from("/etc/passwd");
    assert!(
        !denied.starts_with(&staging_root),
        "denied sentinel must remain outside the explicit staging grant"
    );
    fs::write(&allowed, b"allowed").expect("root topology allowed sentinel");

    // These reads prove the sentinel is real and the parent remains raw. The
    // child receives only the explicit policy above, which omits /etc/passwd.
    assert_eq!(
        fs::read(&allowed).expect("raw allowed sentinel"),
        b"allowed"
    );
    assert!(!fs::read(&denied).expect("raw denied sentinel").is_empty());
    let raw_mountinfo = fs::read_to_string("/proc/self/mountinfo").expect("raw source mountinfo");
    for mount in ["/dev", "/proc"] {
        assert!(
            raw_mountinfo
                .lines()
                .any(|line| line.split_whitespace().nth(4) == Some(mount)),
            "source=/ raw topology lacks nested {mount} mount"
        );
    }

    let executable = std::env::current_exe().expect("test executable");
    let sandbox = Sandbox::test_projected(
        &root_topology_policy(&executable, &allowed, &denied, &staging_root),
        Path::new("/"),
    )
    .expect("private source=/ projected sandbox acquisition");
    let observer = sandbox.test_projected_cleanup_observer();
    let staging = sandbox.test_projected_staging_path().to_owned();
    assert!(
        staging.starts_with(&staging_root),
        "session staging escaped its explicitly granted private root: {}",
        staging.display()
    );

    let output = spawn_ready(
        sandbox
            .prepare(root_topology_command(&executable, &staging))
            .expect("source=/ prepared command"),
    )
    .wait_with_output()
    .expect("source=/ command reap");
    assert!(
        output.status.success(),
        "source=/ command failed: {output:?}"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("PROJECTED_SESSION_ROOT_TOPOLOGY_OK"));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("PROJECTED_SESSION_ROOT_TOPOLOGY_STDERR_OK")
    );

    drop(sandbox);
    assert!(
        !staging.exists(),
        "source=/ session retained staging after its final command closed"
    );
    assert_cleanup_ok(&observer);
    println!("PROJECTED_SESSION_SOURCE_ROOT_TOPOLOGY_CLEANUP_OK");
}

/// Serverless read-backing discriminator for the recursive source-root snapshot.
#[test]
#[ignore = "requires an ordinary Linux user with direct user namespaces and /dev/fuse"]
fn projected_session_source_root_read_backing_prunes_rw_clone() {
    let staging = tempfile::tempdir().expect("source-root backing staging");
    let rw = staging.path().join("rw");
    let read = staging.path().join("read");
    let view = staging.path().join("view");
    for path in [&rw, &read, &view] {
        fs::create_dir(path).expect("source-root backing mountpoint");
    }
    let mut mount = NamespacePair::mount_projected(
        ProjectionMountPaths::new(Path::new("/"), &rw, &read, &view)
            .expect("source-root mount paths"),
    )
    .expect("source-root projected mount");
    let connection = mount
        .take_connection()
        .expect("source-root FUSE connection");
    let (rw_root, read_root) = mount
        .take_backing_roots()
        .expect("source-root backing roots");
    let relative_staging = staging
        .path()
        .strip_prefix("/")
        .expect("absolute source-root staging");
    let copied_rw_escape = relative_staging.join("rw/etc/passwd");
    let copied_rw_escape =
        CString::new(copied_rw_escape.as_os_str().as_bytes()).expect("source-root escape path");
    let fd = unsafe {
        libc::openat(
            read_root.as_raw_fd(),
            copied_rw_escape.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if fd >= 0 {
        unsafe {
            libc::close(fd);
        }
        panic!("read backing cloned the private RW source-root mount");
    }
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ENOENT),
        "read backing escape returned the wrong errno"
    );

    drop(read_root);
    drop(rw_root);
    drop(connection);
    mount.unmount_view().expect("source-root view unmount");
    mount
        .release_backing_namespace()
        .expect("source-root recursive backing release");
    println!("PROJECTED_SESSION_SOURCE_ROOT_READ_BACKING_PRUNED_OK");
}

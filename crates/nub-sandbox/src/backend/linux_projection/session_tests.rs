//! Real Sandbox -> Prepared -> PreparedChild ownership fixture for a private
//! mounted projection. This is deliberately one narrow vertical: policy
//! admission remains disabled and the fixture requires the namespace/FUSE
//! capabilities its mount actually uses.

use super::ProjectedSession;
use super::session::AcquireFault;
use crate::backend::{CommandSpec, Prepared, PreparedChild, PreparedSignalTarget, Sandbox};
use crate::policy::{
    CanonGlob, Effect, EnvPolicy, FsAccess, FsOrigin, FsRule, FsRuleSet, NetPolicy, SandboxPolicy,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

const HELPER: &str = "backend::linux_projection::session_tests::projected_session_helper";
const ROLE: &str = "NUB_PROJECTED_SESSION_ROLE";
const ENV_MARKER: &str = "NUB_PROJECTED_SESSION_ENV";
const ENV_VALUE: &str = "constructed-session-env";

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

fn fixture(executable: &Path) -> Fixture {
    let root = tempfile::tempdir().expect("fixture root");
    let source = root.path().join("source");
    let app = source.join("app");
    fs::create_dir_all(&app).expect("fixture app directory");

    // This is a disposable test-binary closure, not executable discovery or a
    // production policy grant. The copied binary reaches its interpreter and
    // shared objects only through the projected namespace.
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
        ("denied.txt", b"denied-path".as_slice(), FsAccess::Read),
        ("role", b"idle".as_slice(), FsAccess::Read),
        ("release", b"hold".as_slice(), FsAccess::Read),
    ] {
        fs::write(app.join(name), bytes).expect("fixture leaf");
        if name != "denied.txt" {
            entries.push(rule(format!("/app/{name}"), access));
        }
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

fn command() -> CommandSpec {
    CommandSpec::new("/app/run")
        .args(["--exact", HELPER, "--nocapture", "--test-threads=1"])
        .cwd("/")
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
    let address = listener.local_addr().expect("parent listener address");
    let server = thread::spawn(move || {
        let (mut accepted, _) = listener.accept().expect("parent accept");
        accepted
            .write_all(b"parent-network-control")
            .expect("parent reply");
    });
    let mut stream = TcpStream::connect(address).expect("parent allowed network control");
    let mut bytes = String::new();
    stream
        .read_to_string(&mut bytes)
        .expect("parent control read");
    server.join().expect("parent network thread");
    assert_eq!(bytes, "parent-network-control");
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
    denied_network();
}

/// Bounded subprocess role run from the copied test ELF inside the projection.
#[test]
fn projected_session_helper() {
    if std::env::var(ROLE).as_deref() != Ok("mounted-session") {
        return;
    }
    child_contract();
    match fs::read_to_string("/app/role")
        .expect("projected role")
        .trim()
    {
        "hold" => {
            println!("PROJECTED_SESSION_HELD_READY");
            io::stdout().flush().expect("held marker flush");
            loop {
                unsafe {
                    libc::pause();
                }
            }
        }
        "survive" => {
            println!("PROJECTED_SESSION_SURVIVOR_READY");
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
    println!("PROJECTED_SESSION_PREPARED_LEASE_STDIO_READY_REAP_OK");
}

#[test]
#[ignore = "requires an ordinary Linux user with direct user namespaces and /dev/fuse"]
fn projected_session_real_lifecycle_and_cleanup() {
    // This is deliberately one sequential fixture: project-wide ownership slots
    // make parallel mounted fault/lifetime probes invalid evidence.
    startup_faults_cleanup();
    failed_cleanup_retains_then_retries();
    prepared_children_retain_the_mounted_lease();
}

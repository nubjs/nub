use super::*;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

const FIXTURE: &str = "backend::windows::native_child_tests::windows_native_child_fixture";
const MODE: &str = "__NUB_WINDOWS_NATIVE_FIXTURE";

pub(super) fn plan(root: &Path, mode: &str) -> AppContainerLaunch {
    let program = std::env::current_exe().unwrap();
    let mut env: BTreeMap<String, String> = std::env::vars()
        .filter(|(key, _)| {
            [
                "SYSTEMROOT",
                "WINDIR",
                "TEMP",
                "TMP",
                "LOCALAPPDATA",
                "USERPROFILE",
                "PATH",
            ]
            .contains(&key.to_ascii_uppercase().as_str())
        })
        .collect();
    env.insert(MODE.to_string(), mode.to_string());
    env.insert(
        "__NUB_WINDOWS_FIXTURE_ROOT".to_string(),
        root.display().to_string(),
    );
    AppContainerLaunch {
        program: program.clone().into_os_string(),
        args: crate::backend::CommandArgs::Argv(vec![
            "--exact".into(),
            FIXTURE.into(),
            "--nocapture".into(),
            "--test-threads=1".into(),
        ]),
        cwd: Some(root.to_path_buf()),
        read_grants: vec![program],
        read_node_grants: Vec::new(),
        write_grants: vec![root.to_path_buf()],
        publishable_grants: Vec::new(),
        env: Some(env),
        allow_internet: false,
        egress_funnel: None,
        proxy_context: None,
        tmp_mode: crate::policy::TmpMode::Shared,
        native_compat: std::env::var_os("NUB_NATIVE_EMBEDDED_ADAPTER").is_some(),
        native_full_network: false,
        stdout: WindowsStdio::Piped,
        stderr: WindowsStdio::Piped,
    }
}

fn plain_plan(root: &Path, mode: &str) -> WindowsLaunch {
    let plan = plan(root, mode);
    let mut spec = crate::CommandSpec::new(plan.program);
    spec.args = plan.args;
    spec.cwd = plan.cwd;
    spec.redact_stdout = true;
    spec.redact_stderr = true;
    WindowsLaunch::plain(spec, plan.env.unwrap())
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "child did not create {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn is_running(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
    let handle = unsafe { OpenProcess(0x0010_0000, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let running = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
    unsafe {
        CloseHandle(handle);
    }
    running
}

struct OwnerFixture(std::process::Child);

impl Drop for OwnerFixture {
    fn drop(&mut self) {
        // A readiness/assertion failure must not strand an owner waiting on
        // stdin or acquiring a resource, with the CI log stream still open.
        self.0.stdin.take();
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn windows_native_child_fixture() {
    let Ok(mode) = std::env::var(MODE) else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("__NUB_WINDOWS_FIXTURE_ROOT").unwrap());
    match mode.as_str() {
        "echo" => {
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input).unwrap();
            println!("native-stdout:{input}");
            eprintln!("native-stderr");
            println!("{}", super::windows_token_report());
        }
        "scrubbed-env" => {
            assert!(std::env::var_os("LOCALAPPDATA").is_some());
            assert!(std::env::var_os("USERPROFILE").is_none());
            assert!(std::env::var_os("PATH").is_none());
            println!("scrubbed-env-ok");
        }
        "read-package" => {
            println!(
                "package:{}",
                std::fs::read_to_string(root.join("package.json")).unwrap()
            );
        }
        "cached-env" => {
            println!(
                "command-env:{}",
                std::env::var("__NUB_COMMAND_VALUE").unwrap()
            );
        }
        "tmp" => {
            let tmp = std::env::var("TMP").unwrap();
            assert_eq!(std::env::var("TEMP").unwrap(), tmp);
            assert_eq!(std::env::var("TMPDIR").unwrap(), tmp);
            let path = std::env::temp_dir().join("managed-marker");
            std::fs::write(&path, b"managed-slot").unwrap();
            println!("managed-temp:{}", path.display());
            println!("{}", super::windows_token_report());
        }
        "tree" | "tree-hold" => {
            // Deliberately orphan this helper to test Job ownership after its
            // immediate parent exits; waiting here would defeat the regression.
            #[allow(clippy::zombie_processes)]
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", FIXTURE, "--nocapture", "--test-threads=1"])
                .env(MODE, "hold")
                .spawn()
                .unwrap();
            std::fs::write(root.join("descendant.pending"), child.id().to_string()).unwrap();
            std::fs::rename(root.join("descendant.pending"), root.join("descendant")).unwrap();
            // Return without waiting: the native Job, not this direct child, owns the tree.
            if mode == "tree-hold" {
                loop {
                    std::thread::sleep(Duration::from_secs(60));
                }
            }
        }
        "hold" => {
            std::fs::write(root.join(format!("ready-{}", std::process::id())), b"ready").unwrap();
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        "owner" => {
            let mut launch = plan(&root, "hold");
            launch.tmp_mode = crate::policy::TmpMode::Private;
            let resource = launch.acquire().unwrap();
            let child = resource
                .spawn_with_stdio(WindowsStdio::Null, WindowsStdio::Null, WindowsStdio::Null)
                .unwrap();
            wait_for_file(&root.join(format!("ready-{}", child.id())));
            let record = root.join(format!("owner-{}", std::process::id()));
            let pending = record.with_extension("pending");
            std::fs::write(
                &pending,
                format!(
                    "{}\n{}\n{}",
                    resource.profile_name(),
                    child.id(),
                    resource.private_tmp().unwrap().display()
                ),
            )
            .unwrap();
            std::fs::rename(pending, record).unwrap();
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input).unwrap();
            drop(child);
        }
        "plain-owner-suspended" => {
            let resource = plain_plan(&root, "hold").acquire().unwrap();
            assert!(resource.identity().is_none());
            assert!(resource.lease().is_none());
            let _child = resource
                .spawn_before_resume(
                    WindowsStdio::Null,
                    WindowsStdio::Null,
                    WindowsStdio::Null,
                    |pid| {
                        let pending = root.join("suspended.pending");
                        std::fs::write(&pending, pid.to_string())?;
                        std::fs::rename(pending, root.join("suspended"))?;
                        let mut gate = String::new();
                        std::io::stdin().read_to_string(&mut gate)?;
                        Ok(())
                    },
                )
                .unwrap();
        }
        _ => panic!("unexpected fixture mode"),
    }
}

#[test]
fn windows_native_streams_are_caller_owned_and_status_is_cached() {
    let root = tempfile::tempdir().unwrap();
    let resource = plan(root.path(), "echo").acquire().unwrap();
    let mut child = resource
        .spawn_with_stdio(
            WindowsStdio::Piped,
            WindowsStdio::Piped,
            WindowsStdio::Piped,
        )
        .unwrap();
    assert!(
        child.try_wait().unwrap().is_none(),
        "stdin keeps the native child live"
    );
    let mut input = child.take_stdin().unwrap();
    let payload = "pipe-contract".repeat(16 * 1024);
    eprintln!("NATIVE_PIPE_PHASE streams: writing stdin");
    input.write_all(payload.as_bytes()).unwrap();
    drop(input);
    eprintln!("NATIVE_PIPE_PHASE streams: stdin closed");
    let mut out = child.take_stdout().unwrap();
    let mut err = child.take_stderr().unwrap();
    let stdout = std::thread::spawn(move || {
        let mut bytes = String::new();
        out.read_to_string(&mut bytes).unwrap();
        bytes
    });
    let stderr = std::thread::spawn(move || {
        let mut bytes = String::new();
        err.read_to_string(&mut bytes).unwrap();
        bytes
    });
    drop(resource);
    eprintln!("NATIVE_PIPE_PHASE streams: waiting for Job");
    let status = child.wait().unwrap();
    eprintln!("NATIVE_PIPE_PHASE streams: Job reaped, joining readers");
    assert!(status.success());
    assert_eq!(child.try_wait().unwrap(), Some(status));
    let stdout = stdout.join().unwrap();
    assert!(stdout.contains(&format!("native-stdout:{payload}")));
    assert!(stdout.contains("is_appcontainer=true"));
    assert!(stderr.join().unwrap().contains("native-stderr"));
    eprintln!("NATIVE_PIPE_PHASE streams: readers joined");
}

#[test]
fn windows_native_equivalent_resources_reuse_identity_but_not_jobs() {
    let root = tempfile::tempdir().unwrap();
    let first = plan(root.path(), "hold").acquire().unwrap();
    let second = plan(root.path(), "hold").acquire().unwrap();
    assert_eq!(first.profile_name(), second.profile_name());
    let profile = first.profile_name().to_string();
    let mut a = first
        .spawn_with_stdio(WindowsStdio::Null, WindowsStdio::Null, WindowsStdio::Null)
        .unwrap();
    let mut b = second
        .spawn_with_stdio(WindowsStdio::Null, WindowsStdio::Null, WindowsStdio::Null)
        .unwrap();
    wait_for_file(&root.path().join(format!("ready-{}", a.id())));
    wait_for_file(&root.path().join(format!("ready-{}", b.id())));
    a.kill().unwrap();
    assert!(!a.wait().unwrap().success());
    assert!(b.try_wait().unwrap().is_none());
    assert!(is_running(b.id()));
    let pid = b.id();
    drop(b);
    assert!(!is_running(pid), "Drop must reap its native Job");
    drop(a);
    drop(first);
    drop(second);
    assert_eq!(
        plan(root.path(), "hold").acquire().unwrap().profile_name(),
        profile
    );
}

#[test]
fn windows_native_drop_reaps_a_handed_off_descendant() {
    for mode in ["tree", "tree-hold"] {
        let root = tempfile::tempdir().unwrap();
        let resource = plan(root.path(), mode).acquire().unwrap();
        let mut child = resource
            .spawn_with_stdio(WindowsStdio::Null, WindowsStdio::Null, WindowsStdio::Null)
            .unwrap();
        let pid = child.id();
        wait_for_file(&root.path().join("descendant"));
        let descendant: u32 = std::fs::read_to_string(root.path().join("descendant"))
            .unwrap()
            .parse()
            .unwrap();
        wait_for_file(&root.path().join(format!("ready-{descendant}")));
        assert!(
            child.try_wait().unwrap().is_none(),
            "a root exit is not Job completion"
        );
        drop(child);
        assert!(
            !is_running(descendant),
            "Drop left a live descendant: {mode}"
        );
        assert!(!is_running(pid), "Drop left a live root: {mode}");
    }
}

#[test]
fn windows_native_spawn_failure_does_not_poison_the_resource() {
    let root = tempfile::tempdir().unwrap();
    let good = plan(root.path(), "hold");
    let mut bad = good.clone();
    bad.program = root.path().join("not-an-executable.exe").into_os_string();
    let resource = bad.acquire().unwrap();
    assert!(resource.spawn().is_err());
    let resource = good.acquire().unwrap();
    let mut child = resource
        .spawn_with_stdio(WindowsStdio::Null, WindowsStdio::Null, WindowsStdio::Null)
        .unwrap();
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
}

#[test]
fn windows_native_independent_owner_death_reaps_only_its_command() {
    let root = tempfile::tempdir().unwrap();
    let owner = || {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", FIXTURE, "--nocapture", "--test-threads=1"])
            .env(MODE, "owner")
            .env("__NUB_WINDOWS_FIXTURE_ROOT", root.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap()
    };
    let mut a = OwnerFixture(owner());
    let mut b = OwnerFixture(owner());
    let record = |owner: &std::process::Child| {
        let path = root.path().join(format!("owner-{}", owner.id()));
        wait_for_file(&path);
        let text = std::fs::read_to_string(path).unwrap();
        let mut lines = text.lines();
        (
            lines.next().unwrap().to_string(),
            lines.next().unwrap().parse::<u32>().unwrap(),
            PathBuf::from(lines.next().unwrap()),
        )
    };
    let (profile_a, child_a, tmp_a) = record(&a.0);
    let (profile_b, child_b, tmp_b) = record(&b.0);
    assert_eq!(profile_a, profile_b);
    assert_eq!(tmp_a, tmp_b);
    a.0.kill().unwrap();
    a.0.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while is_running(child_a) {
        assert!(
            Instant::now() < deadline,
            "owner death left its command running"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        is_running(child_b),
        "another owner's same-policy Job must remain alive"
    );
    drop(b.0.stdin.take());
    assert!(b.0.wait().unwrap().success());
    assert!(!is_running(child_b));
    let mut launch = plan(root.path(), "hold");
    launch.tmp_mode = crate::policy::TmpMode::Private;
    let resource = launch.acquire().unwrap();
    assert_eq!(resource.profile_name(), profile_a);
    assert_eq!(resource.private_tmp(), Some(tmp_a.as_path()));
}

#[test]
fn windows_native_owner_guard_reaps_on_fixture_panic() {
    let root = tempfile::tempdir().unwrap();
    let owner = OwnerFixture(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", FIXTURE, "--nocapture", "--test-threads=1"])
            .env(MODE, "hold")
            .env("__NUB_WINDOWS_FIXTURE_ROOT", root.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let pid = owner.0.id();
    wait_for_file(&root.path().join(format!("ready-{pid}")));
    let failure = std::panic::catch_unwind(move || {
        let _owner = owner;
        panic!("simulated owner readiness assertion failure");
    });
    assert!(failure.is_err());
    assert!(
        !is_running(pid),
        "fixture panic must kill and reap its owner"
    );
}

#[test]
fn windows_native_scrubbed_environment_supplies_only_required_profile_metadata() {
    let root = tempfile::tempdir().unwrap();
    let mut launch = plan(root.path(), "scrubbed-env");
    launch.env.as_mut().unwrap().retain(|key, _| {
        !["LOCALAPPDATA", "USERPROFILE", "PATH"]
            .iter()
            .any(|name| key.eq_ignore_ascii_case(name))
    });
    let resource = launch.acquire().expect("acquire with scrubbed environment");
    let mut child = resource
        .spawn_with_stdio(
            WindowsStdio::Null,
            WindowsStdio::Piped,
            WindowsStdio::Inherit,
        )
        .expect("spawn with scrubbed environment");
    let mut output = String::new();
    child
        .take_stdout()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    assert!(child.wait().unwrap().success(), "{output}");
    assert!(output.contains("scrubbed-env-ok"), "{output}");
}

#[test]
fn windows_native_retained_resource_keeps_lease_but_not_command_environment() {
    let root = tempfile::tempdir().unwrap();
    let mut first = plan(root.path(), "cached-env");
    first
        .env
        .as_mut()
        .unwrap()
        .insert("__NUB_COMMAND_VALUE".into(), "first".into());
    let mut second = first.clone();
    second
        .env
        .as_mut()
        .unwrap()
        .insert("__NUB_COMMAND_VALUE".into(), "second".into());
    let first = first.acquire().unwrap();
    let lease = first.lease().unwrap();
    let retained = BTreeMap::from([(first.identity().unwrap().to_string(), lease.clone())]);
    drop(first);
    let second = second.acquire_reusing(&retained).unwrap();
    assert!(lease.shares_resource(&second.lease().unwrap()));
    let mut child = second
        .spawn_with_stdio(
            WindowsStdio::Null,
            WindowsStdio::Piped,
            WindowsStdio::Inherit,
        )
        .unwrap();
    let mut output = String::new();
    child
        .take_stdout()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    assert!(child.wait().unwrap().success(), "{output}");
    assert!(output.contains("command-env:second"), "{output}");
    assert!(!output.contains("command-env:first"), "{output}");
    assert!(lease.is_live());
}

#[test]
fn windows_native_retained_resource_refuses_replaced_grant_and_private_root() {
    let root = tempfile::tempdir().unwrap();
    let grant = root.path().join("grant");
    std::fs::create_dir(&grant).unwrap();
    let mut launch = plan(&grant, "hold");
    launch.tmp_mode = crate::policy::TmpMode::Private;
    let resource = launch.clone().acquire().unwrap();
    let retained = BTreeMap::from([(
        resource.identity().unwrap().to_string(),
        resource.lease().unwrap(),
    )]);
    let private = resource.private_tmp().unwrap().to_path_buf();
    for path in [grant, private] {
        let original = path.with_extension("cache-original");
        std::fs::rename(&path, &original).unwrap();
        std::fs::create_dir(&path).unwrap();
        let result = launch.clone().acquire_reusing(&retained);
        // Restore the owned object before assertions or resource cleanup.
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&original, &path).unwrap();
        assert!(result.is_err(), "replaced {} was reused", path.display());
    }
}

#[test]
fn windows_native_tmp_deny_validator_accepts_exact_read_and_write_unions() {
    const READ_EXECUTE: u32 = 0x8000_0000 | 0x2000_0000;
    const READ_WRITE_EXECUTE_DELETE: u32 = READ_EXECUTE | 0x4000_0000 | 0x0001_0000;

    let root = tempfile::tempdir().unwrap();
    let mut launch = plan(root.path(), "hold");
    launch.tmp_mode = crate::policy::TmpMode::Deny;
    let resource = launch.acquire().unwrap();
    let profile = resource.profile_name().to_string();
    let temp = super::launch::test_profile_storage_temp(&profile).unwrap();

    assert!(super::launch::test_profile_storage_matches(&profile, &temp, 0).unwrap());
    super::launch::test_add_profile_storage_ace(&profile, &temp, READ_EXECUTE, false).unwrap();
    assert!(super::launch::test_profile_storage_matches(&profile, &temp, READ_EXECUTE).unwrap());
    super::launch::test_add_profile_storage_ace(&profile, &temp, READ_WRITE_EXECUTE_DELETE, false)
        .unwrap();
    assert!(
        !super::launch::test_profile_storage_matches(&profile, &temp, READ_EXECUTE).unwrap(),
        "read-write storage must not validate a read-only grant"
    );
    assert!(
        super::launch::test_profile_storage_matches(&profile, &temp, READ_WRITE_EXECUTE_DELETE)
            .unwrap()
    );

    // Windows may split a generic inheritable grant on a container into an
    // effective mapped ACE and an inherit-only generic ACE for descendants.
    let root = tempfile::tempdir().unwrap();
    let mut launch = plan(root.path(), "hold");
    launch.tmp_mode = crate::policy::TmpMode::Deny;
    let resource = launch.acquire().unwrap();
    let profile = resource.profile_name().to_string();
    let temp = super::launch::test_profile_storage_temp(&profile).unwrap();
    super::launch::test_add_profile_storage_ace(&profile, &temp, READ_EXECUTE, true).unwrap();
    super::launch::test_add_profile_storage_effective_ace(&profile, &temp, READ_EXECUTE).unwrap();
    assert!(
        super::launch::test_profile_storage_matches(&profile, &temp, READ_EXECUTE).unwrap(),
        "equivalent effective and inherit-only split ACEs must validate"
    );
}

#[test]
fn windows_native_tmp_deny_retained_resource_rejects_overgrant_and_inherit_only_ace() {
    const READ_EXECUTE: u32 = 0x8000_0000 | 0x2000_0000;
    const READ_WRITE_EXECUTE_DELETE: u32 = READ_EXECUTE | 0x4000_0000 | 0x0001_0000;

    let root = tempfile::tempdir().unwrap();
    let mut launch = plan(root.path(), "hold");
    launch.tmp_mode = crate::policy::TmpMode::Deny;
    let resource = launch.clone().acquire().unwrap();
    let profile = resource.profile_name().to_string();
    let temp = super::launch::test_profile_storage_temp(&profile).unwrap();
    let retained = BTreeMap::from([(
        resource.identity().unwrap().to_string(),
        resource.lease().unwrap(),
    )]);
    super::launch::test_add_profile_storage_ace(&profile, &temp, READ_WRITE_EXECUTE_DELETE, false)
        .unwrap();
    assert!(
        !super::launch::test_profile_storage_matches(&profile, &temp, 0).unwrap(),
        "a package SID grant must invalidate tmp:false"
    );
    assert!(
        launch.acquire_reusing(&retained).is_err(),
        "a retained tmp:false resource reused a widened storage DACL"
    );

    let root = tempfile::tempdir().unwrap();
    let mut launch = plan(root.path(), "hold");
    launch.tmp_mode = crate::policy::TmpMode::Deny;
    let resource = launch.acquire().unwrap();
    let profile = resource.profile_name().to_string();
    let temp = super::launch::test_profile_storage_temp(&profile).unwrap();
    super::launch::test_add_profile_storage_ace(&profile, &temp, READ_EXECUTE, true).unwrap();
    assert!(
        !super::launch::test_profile_storage_matches(&profile, &temp, READ_EXECUTE).unwrap(),
        "an inherit-only ACE must not validate object-effective profile storage access"
    );
}

#[test]
fn windows_native_retained_resource_misses_for_different_resolved_grants() {
    let root = tempfile::tempdir().unwrap();
    let first_root = root.path().join("first");
    let second_root = root.path().join("second");
    std::fs::create_dir(&first_root).unwrap();
    std::fs::create_dir(&second_root).unwrap();
    let first = plan(&first_root, "hold").acquire().unwrap();
    let retained = BTreeMap::from([(
        first.identity().unwrap().to_string(),
        first.lease().unwrap(),
    )]);
    let second = plan(&second_root, "hold")
        .acquire_reusing(&retained)
        .unwrap();
    assert_ne!(first.identity(), second.identity());
    assert!(
        !first
            .lease()
            .unwrap()
            .shares_resource(&second.lease().unwrap())
    );
}

#[test]
fn windows_native_retained_resource_owns_independent_command_jobs() {
    let root = tempfile::tempdir().unwrap();
    let launch = plan(root.path(), "hold");
    let first = launch.clone().acquire().unwrap();
    let retained = BTreeMap::from([(
        first.identity().unwrap().to_string(),
        first.lease().unwrap(),
    )]);
    let second = launch.acquire_reusing(&retained).unwrap();
    assert!(
        first
            .lease()
            .unwrap()
            .shares_resource(&second.lease().unwrap())
    );
    let a = first
        .spawn_with_stdio(WindowsStdio::Null, WindowsStdio::Null, WindowsStdio::Null)
        .unwrap();
    let mut b = second
        .spawn_with_stdio(WindowsStdio::Null, WindowsStdio::Null, WindowsStdio::Null)
        .unwrap();
    wait_for_file(&root.path().join(format!("ready-{}", a.id())));
    wait_for_file(&root.path().join(format!("ready-{}", b.id())));
    let a_id = a.id();
    drop(a);
    assert!(!is_running(a_id));
    assert!(is_running(b.id()));
    assert!(b.try_wait().unwrap().is_none());
    b.kill().unwrap();
    b.wait().unwrap();
}

#[test]
fn windows_native_managed_tmp_reuses_slot_without_retaining_command_environment() {
    let root = tempfile::tempdir().unwrap();
    let mut first = plan(root.path(), "tmp");
    first.tmp_mode = crate::policy::TmpMode::Private;
    let mut second = first.clone();
    for (launch, value) in [(&mut first, "caller-one"), (&mut second, "caller-two")] {
        launch
            .env
            .as_mut()
            .unwrap()
            .insert("tMp".into(), value.into());
        launch
            .env
            .as_mut()
            .unwrap()
            .insert("TEMP".into(), value.into());
    }
    let first = first.acquire().unwrap();
    let slot = first.private_tmp().unwrap().to_path_buf();
    let identity = first.identity().unwrap().to_string();
    let keepalive = first.lease().unwrap();
    drop(first);
    assert!(
        keepalive.is_live(),
        "session retains the native registry lease"
    );
    assert!(
        slot.is_dir(),
        "session lease protects a resource between commands"
    );
    let second = second.acquire().unwrap();
    assert_eq!(second.identity(), Some(identity.as_str()));
    assert_eq!(second.private_tmp(), Some(slot.as_path()));
    let mut child = second
        .spawn_with_stdio(
            WindowsStdio::Null,
            WindowsStdio::Piped,
            WindowsStdio::Inherit,
        )
        .unwrap();
    let mut stdout = String::new();
    eprintln!("NATIVE_PIPE_PHASE managed-tmp: reading stdout");
    child
        .take_stdout()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    eprintln!("NATIVE_PIPE_PHASE managed-tmp: stdout EOF, waiting for Job");
    assert!(child.wait().unwrap().success());
    eprintln!("NATIVE_PIPE_PHASE managed-tmp: Job reaped");
    assert!(stdout.contains("is_appcontainer=true"));
    assert_eq!(
        std::fs::read(slot.join("managed-marker")).unwrap(),
        b"managed-slot"
    );
    drop(child);
    drop(second);
    drop(keepalive);
}

#[test]
fn windows_native_explicit_grant_changes_never_share_managed_tmp() {
    let root = tempfile::tempdir().unwrap();
    let unique = tempfile::tempdir().unwrap();
    let mut first = plan(root.path(), "hold");
    first.tmp_mode = crate::policy::TmpMode::Private;
    let mut second = first.clone();
    second.read_grants.push(unique.path().to_path_buf());
    let first = first.acquire().unwrap();
    let second = second.acquire().unwrap();
    assert_ne!(first.identity(), second.identity());
    assert_ne!(first.private_tmp(), second.private_tmp());
}

#[test]
fn windows_plain_streams_preserve_output_without_a_profile() {
    let root = tempfile::tempdir().unwrap();
    let launch = plain_plan(root.path(), "echo");
    assert!(!launch.is_appcontainer());
    let resource = launch.acquire().unwrap();
    assert!(resource.identity().is_none());
    assert!(resource.lease().is_none());
    assert!(resource.private_tmp().is_none());
    let mut child = resource
        .spawn_with_stdio(
            WindowsStdio::Piped,
            WindowsStdio::Piped,
            WindowsStdio::Piped,
        )
        .unwrap();
    let mut input = child.take_stdin().unwrap();
    let payload = "plain-pipe".repeat(16 * 1024);
    input.write_all(payload.as_bytes()).unwrap();
    drop(input);
    let mut stdout = child.take_stdout().unwrap();
    let mut stderr = child.take_stderr().unwrap();
    let out = std::thread::spawn(move || {
        let mut text = String::new();
        stdout.read_to_string(&mut text).unwrap();
        text
    });
    let err = std::thread::spawn(move || {
        let mut text = String::new();
        stderr.read_to_string(&mut text).unwrap();
        text
    });
    assert!(child.wait().unwrap().success());
    let out = out.join().unwrap();
    assert!(out.contains(&payload));
    assert!(out.contains("is_appcontainer=false"));
    assert!(err.join().unwrap().contains("native-stderr"));
}

#[test]
fn windows_plain_verbatim_args_and_exit_code_survive_native_launch() {
    let root = tempfile::tempdir().unwrap();
    let mut spec = crate::CommandSpec::new("cmd.exe");
    spec.args =
        crate::backend::CommandArgs::Verbatim(r#"/d /s /c "echo raw marker& exit /b 7""#.into());
    spec.cwd = Some(root.path().to_path_buf());
    spec.redact_stdout = true;
    let launch = WindowsLaunch::plain(spec, std::env::vars().collect());
    let resource = launch.acquire().unwrap();
    let mut child = resource.spawn().unwrap();
    let mut output = String::new();
    child
        .take_stdout()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(7));
    assert!(output.contains("raw marker"));
}

#[test]
fn windows_plain_owner_death_before_resume_reaps_the_suspended_child() {
    let root = tempfile::tempdir().unwrap();
    let mut owner = OwnerFixture(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", FIXTURE, "--nocapture", "--test-threads=1"])
            .env(MODE, "plain-owner-suspended")
            .env("__NUB_WINDOWS_FIXTURE_ROOT", root.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    wait_for_file(&root.path().join("suspended"));
    let pid: u32 = std::fs::read_to_string(root.path().join("suspended"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(is_running(pid));
    assert!(
        !root.path().join(format!("ready-{pid}")).exists(),
        "child is still before ResumeThread"
    );
    owner.0.kill().unwrap();
    owner.0.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while is_running(pid) {
        assert!(
            Instant::now() < deadline,
            "owner death left the suspended child alive"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!root.path().join(format!("ready-{pid}")).exists());
}

#[test]
fn windows_plain_batch_args_and_child_path_match_standard_command() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("probe.cmd"),
        "@echo off\r\necho [%1]\r\necho [%2]\r\necho [%3]\r\necho [%4]\r\nexit /b 9\r\n",
    )
    .unwrap();
    let args = ["plain", "two words", "percent%PATH%", "tail\\"];
    let reference = std::process::Command::new("probe.cmd")
        .args(args)
        .current_dir(root.path())
        .env("PATH", root.path())
        .output()
        .unwrap();
    let mut spec = crate::CommandSpec::new("probe.cmd");
    spec.args = crate::backend::CommandArgs::Argv(args.into_iter().map(Into::into).collect());
    spec.cwd = Some(root.path().to_path_buf());
    let mut env: BTreeMap<_, _> = std::env::vars().collect();
    env.retain(|key, _| !key.eq_ignore_ascii_case("PATH"));
    env.insert("PATH".into(), root.path().to_string_lossy().into_owned());
    let resource = WindowsLaunch::plain(spec, env).acquire().unwrap();
    let mut child = resource
        .spawn_with_stdio(WindowsStdio::Null, WindowsStdio::Piped, WindowsStdio::Piped)
        .unwrap();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .take_stdout()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    child
        .take_stderr()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    assert_eq!(child.wait().unwrap(), reference.status);
    assert_eq!(stdout, reference.stdout);
    assert_eq!(stderr, reference.stderr);
}

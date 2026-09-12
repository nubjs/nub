//! Native Windows lifecycle regressions.  These deliberately drive a second test
//! process: a process-local cache cannot prove the persistent registry's lease and
//! cleanup rules.

use super::*;
use crate::backend::CommandArgs;
use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, CloseWindowStation, CreateDesktopW, CreateWindowStationW,
    GetProcessWindowStation, SetProcessWindowStation,
};

const FIXTURE: &str = "backend::windows::windows_cleanup_tests::windows_cleanup_fixture";
const MODE: &str = "__NUB_WINDOWS_CLEANUP_FIXTURE";
const ROOT: &str = "__NUB_WINDOWS_CLEANUP_ROOT";
const FAULT: &str = "__NUB_WINDOWS_CLEANUP_FAULT";

const WINSTA_ALL_ACCESS: u32 = 0x000F_037F;
const DESKTOP_ALL_ACCESS: u32 = 0x000F_01FF;

struct TestWindowObjects {
    station: HANDLE,
    desktop: HANDLE,
    object: windows_registry::WindowObject,
}

impl Drop for TestWindowObjects {
    fn drop(&mut self) {
        unsafe {
            CloseDesktop(self.desktop);
            CloseWindowStation(self.station);
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Build an independently named station/desktop pair so a child can leave a journaled object
/// behind and its parent can recreate precisely those names. The explicit handles keep each
/// generation alive only for the test that owns it.
fn create_test_window_objects(
    station_name: &str,
    desktop_name: &str,
) -> std::io::Result<TestWindowObjects> {
    let session = crate::backend::windows_ace::current_objects()?[0].session;
    let previous = unsafe { GetProcessWindowStation() };
    if previous.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let station_wide = wide(station_name);
    let station = unsafe {
        CreateWindowStationW(
            station_wide.as_ptr(),
            0,
            WINSTA_ALL_ACCESS,
            std::ptr::null_mut(),
        )
    };
    if station.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { SetProcessWindowStation(station) } == 0 {
        unsafe { CloseWindowStation(station) };
        return Err(std::io::Error::last_os_error());
    }
    let desktop_wide = wide(desktop_name);
    let desktop = unsafe {
        CreateDesktopW(
            desktop_wide.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            DESKTOP_ALL_ACCESS,
            std::ptr::null_mut(),
        )
    };
    let restored = unsafe { SetProcessWindowStation(previous) };
    if desktop.is_null() {
        unsafe { CloseWindowStation(station) };
        return Err(std::io::Error::last_os_error());
    }
    if restored == 0 {
        unsafe {
            CloseDesktop(desktop);
            CloseWindowStation(station);
        }
        return Err(std::io::Error::last_os_error());
    }
    Ok(TestWindowObjects {
        station,
        desktop,
        object: windows_registry::WindowObject {
            session,
            station: station_name.to_string(),
            desktop: Some(desktop_name.to_string()),
        },
    })
}

fn plan(root: &Path, mode: &str) -> AppContainerLaunch {
    let program = std::env::current_exe().unwrap();
    let mut env: BTreeMap<String, String> = std::env::vars()
        .filter(|(key, _)| {
            [
                "SYSTEMROOT",
                "WINDIR",
                "TEMP",
                "TMP",
                "LOCALAPPDATA",
                "PATH",
            ]
            .contains(&key.to_ascii_uppercase().as_str())
        })
        .collect();
    env.insert(MODE.into(), mode.into());
    env.insert(ROOT.into(), root.display().to_string());
    AppContainerLaunch {
        program: program.clone().into_os_string(),
        args: CommandArgs::Argv(vec![
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
        private_tmp: true,
        native_compat: std::env::var_os("NUB_NATIVE_EMBEDDED_ADAPTER").is_some(),
        stdout: WindowsStdio::Null,
        stderr: WindowsStdio::Null,
    }
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn running(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
    let handle = unsafe { OpenProcess(0x0010_0000, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let live = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
    unsafe { CloseHandle(handle) };
    live
}

struct OwnerFixture(std::process::Child);

impl Drop for OwnerFixture {
    fn drop(&mut self) {
        self.0.stdin.take();
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Keeps an unrelated SID ACE installed while cleanup removes the resource's ACE.
/// The synthetic profile need not be registered: Windows derives its SID from its
/// deterministic name, which makes this a genuinely distinct DACL entry.
struct ForeignProfileAce {
    profile: String,
    path: PathBuf,
}

impl ForeignProfileAce {
    fn grant(path: &Path) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let profile = format!("nub-test-foreign-{}-{stamp}", std::process::id());
        super::launch::test_set_profile_ace(&profile, path, true).unwrap();
        let owned = Self {
            profile,
            path: path.to_path_buf(),
        };
        assert!(
            super::launch::test_profile_has_ace(&owned.profile, path).unwrap(),
            "test setup did not install unrelated SID ACE"
        );
        owned
    }
}

impl Drop for ForeignProfileAce {
    fn drop(&mut self) {
        let _ = super::launch::test_set_profile_ace(&self.profile, &self.path, false);
    }
}

struct CleanupAfterTest;

impl Drop for CleanupAfterTest {
    fn drop(&mut self) {
        let _ = cleanup_resources();
    }
}

#[test]
fn windows_cleanup_fixture() {
    let Ok(mode) = std::env::var(MODE) else {
        return;
    };
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    match mode.as_str() {
        "hold" => {
            std::fs::write(root.join(format!("ready-{}", std::process::id())), b"ready").unwrap();
            // Do not share the fixture's inherited stdin with the confined child:
            // the parent fixture owns that pipe as its deterministic lifetime gate.
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        "owner" => {
            let start = Instant::now();
            let stage = |label: &str| {
                eprintln!(
                    "CLEANUP_OWNER {} {label} {:?}",
                    std::process::id(),
                    start.elapsed()
                );
            };
            stage("acquiring");
            let resource = plan(&root, "hold").acquire().unwrap();
            stage("acquired");
            let child = resource
                .spawn_with_stdio(WindowsStdio::Null, WindowsStdio::Null, WindowsStdio::Null)
                .unwrap();
            stage("spawned");
            let pid = child.id();
            wait_for(&root.join(format!("ready-{pid}")));
            stage("child-ready");
            std::fs::write(
                root.join(format!("owner-{}", std::process::id())),
                format!("{}\n{pid}", resource.profile_name()),
            )
            .unwrap();
            stage("owner-ready");
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input).unwrap();
            drop(child);
        }
        "fault-acquire" => {
            let _resource = plan(&root, "hold").acquire().unwrap();
            panic!("acquisition did not reach the requested crash transition");
        }
        "fault-cleanup" => {
            cleanup_resources().unwrap();
            panic!("cleanup did not reach the requested crash transition");
        }
        "station-recovery" => crate::backend::windows_ace::test_revoke_from_noncurrent_station()
            .expect("station-specific ACE recovery"),
        "station-replacement" => {
            crate::backend::windows_ace::test_revoke_rejects_same_name_desktop_replacement()
                .expect("replacement desktop must not authorize cleanup")
        }
        "station-concurrent-launch" => concurrent_station_launch(&root),
        "crash-transitions" => crash_transitions(&root),
        "station-journal-crash" => station_journal_crash(&root),
        "cleanup-retry" => interrupted_cleanup(&root),
        "cleanup-window-retry" => interrupted_window_cleanup(&root),
        "cleanup-window-save-failure" => window_revoke_journal_save_failure(&root),
        "window-witness-replacement-fault" => witness_before_intent_fault(&root),
        "cleanup-junction" => cleanup_junction(&root),
        other => panic!("unknown cleanup fixture mode {other}"),
    }
}

fn fixture_command(root: &Path, mode: &str) -> std::process::Command {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", FIXTURE, "--nocapture", "--test-threads=1"])
        .env(MODE, mode)
        .env(ROOT, root)
        .env_remove(FAULT)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());
    command
}

fn isolated_scenario(mode: &str) {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("registry-parent");
    let caller = root.path().join("caller");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(&caller).unwrap();
    let shared_acl_lock = windows_registry::test_registry_root().unwrap();
    let mut owner = OwnerFixture(
        fixture_command(&caller, mode)
            // Only this scenario and its subprocesses see this registry. Other
            // native tests cannot sweep the abandoned entry before inspection.
            .env("ProgramData", state)
            .env("__NUB_WINDOWS_CLEANUP_ACL_LOCK_ROOT", shared_acl_lock)
            .spawn()
            .unwrap(),
    );
    let status = owner.0.wait().unwrap();
    if !status.success() {
        // Preserve the journal if a failure leaves recovery work behind.
        let path = root.keep();
        panic!(
            "{mode} failed with {status}; retained fixture registry at {}",
            path.display()
        );
    }
}

fn crash_owner(root: &Path, mode: &str, stage: &str) -> (String, PathBuf) {
    let mut owner = OwnerFixture(
        fixture_command(root, mode)
            .env(FAULT, stage)
            .spawn()
            .unwrap(),
    );
    let status = owner.0.wait().unwrap();
    assert_eq!(status.code(), Some(91), "fault stage {stage}: {status}");
    let (observed, profile, private): (String, String, PathBuf) =
        serde_json::from_slice(&std::fs::read(root.join("crash-transition.json")).unwrap())
            .unwrap();
    assert_eq!(observed, stage);
    (profile, private)
}

fn assert_recovered(profile: &str, private: &Path, caller: &Path, foreign: &ForeignProfileAce) {
    cleanup_resources().unwrap();
    assert!(windows_registry::test_entry(profile).unwrap().is_none());
    assert!(
        !private.exists(),
        "owned private root survived recovery: {}",
        private.display()
    );
    assert!(!super::launch::test_profile_has_ace(profile, caller).unwrap());
    assert!(
        !windows_registry::native_assets_path(profile)
            .unwrap()
            .exists()
    );
    assert!(super::launch::test_profile_has_ace(&foreign.profile, caller).unwrap());
    assert_eq!(
        std::fs::read(caller.join("caller-owned.txt")).unwrap(),
        b"caller-owned"
    );
    cleanup_resources().unwrap();
    assert_eq!(
        std::fs::read(caller.join("caller-owned.txt")).unwrap(),
        b"caller-owned"
    );
}

fn crash_transitions(root: &Path) {
    let _cleanup = CleanupAfterTest;
    let mut stages = vec![
        "profile-created",
        "private-root-created",
        "acl-installed-before-ready",
    ];
    if std::env::var_os("NUB_NATIVE_EMBEDDED_ADAPTER").is_some() {
        stages.extend([
            "native-assets-journaled",
            "native-assets-before-identity",
            "native-assets-created",
            "native-asset-written",
            "native-assets-installed",
            "native-assets-granted",
        ]);
    }
    for stage in stages {
        let caller = root.join(stage);
        std::fs::create_dir(&caller).unwrap();
        std::fs::write(caller.join("caller-owned.txt"), b"caller-owned").unwrap();
        let foreign = ForeignProfileAce::grant(&caller);
        let (profile, private) = crash_owner(&caller, "fault-acquire", stage);
        let entry = windows_registry::test_entry(&profile)
            .unwrap()
            .expect("crashed owner lost its journal");
        assert_eq!(entry.state, windows_registry::EntryState::Preparing);
        assert!(
            !entry.leases.is_empty(),
            "journal must retain the dead owner's lease until recovery"
        );
        assert_eq!(private.is_dir(), stage != "native-assets-journaled");
        if stage == "acl-installed-before-ready" {
            assert!(super::launch::test_profile_has_ace(&profile, &caller).unwrap());
        }
        if stage == "native-assets-before-identity" {
            assert!(cleanup_resources().is_err());
            let retained = windows_registry::test_entry(&profile).unwrap().unwrap();
            assert_eq!(retained.state, windows_registry::EntryState::RecoveryNeeded);
            assert!(
                private.is_dir(),
                "an unidentified directory must not be deleted"
            );
            // Only the fixture knows that this is its freshly created empty leaf.
            // Recovery must retain the journal until that ambiguity is resolved.
            std::fs::remove_dir(&private).unwrap();
        }
        assert_recovered(&profile, &private, &caller, &foreign);
    }
}

/// The owner process reaches the real AppContainer acquisition path, journals its current
/// station/desktop grants, then exits without destructors. A later process must find and remove
/// those grants through the durable registry, not a process-local cache.
fn station_journal_crash(root: &Path) {
    let _cleanup = CleanupAfterTest;
    let caller = root.join("station-journal");
    std::fs::create_dir(&caller).unwrap();
    std::fs::write(caller.join("caller-owned.txt"), b"caller-owned").unwrap();
    let foreign = ForeignProfileAce::grant(&caller);
    let (profile, _private) = crash_owner(&caller, "fault-acquire", "acl-installed-before-ready");
    let entry = windows_registry::test_entry(&profile)
        .unwrap()
        .expect("crashed owner lost its station journal");
    assert_eq!(entry.state, windows_registry::EntryState::Preparing);
    assert!(
        !entry.window_objects.is_empty(),
        "the owner did not persist its window-object grants"
    );
    for object in &entry.window_objects {
        assert!(
            super::launch::test_profile_has_window_grant(&profile, object).unwrap(),
            "owner's journaled grant was not present before later-process recovery: {object:?}"
        );
    }

    cleanup_resources().unwrap();
    assert!(windows_registry::test_entry(&profile).unwrap().is_none());
    for object in &entry.window_objects {
        assert!(
            !super::launch::test_profile_has_window_grant(&profile, object).unwrap(),
            "later-process recovery retained the journaled grant: {object:?}"
        );
    }
    assert!(super::launch::test_profile_has_ace(&foreign.profile, &caller).unwrap());
    assert_eq!(
        std::fs::read(caller.join("caller-owned.txt")).unwrap(),
        b"caller-owned"
    );
}

/// Keep a real confined child launch behind the recovery station switch. The launch resource is
/// acquired first so the measured wait is the production `CreateProcessW` station guard, not an
/// earlier current-object observation.
fn concurrent_station_launch(root: &Path) {
    let _cleanup = CleanupAfterTest;
    let resource = plan(root, "hold").acquire().unwrap();
    crate::backend::windows_ace::test_spawn_blocks_during_noncurrent_recovery(|| {
        let mut child = resource.spawn_with_stdio(
            WindowsStdio::Null,
            WindowsStdio::Null,
            WindowsStdio::Null,
        )?;
        wait_for(&root.join(format!("ready-{}", child.id())));
        child.kill()?;
        child.wait()?;
        Ok(())
    })
    .expect("AppContainer child launch must wait for station recovery");
    drop(resource);
    cleanup_resources().unwrap();
}

fn interrupted_cleanup(root: &Path) {
    let _cleanup = CleanupAfterTest;
    std::fs::write(root.join("caller-owned.txt"), b"caller-owned").unwrap();
    let foreign = ForeignProfileAce::grant(root);
    let resource = plan(root, "hold").acquire().unwrap();
    let profile = resource.profile_name().to_string();
    let private = resource.private_tmp().unwrap().to_path_buf();
    std::fs::write(private.join("owned-file"), b"owned").unwrap();
    drop(resource);
    let (crashed_profile, removed) = crash_owner(root, "fault-cleanup", "cleanup-private-removed");
    assert_eq!(crashed_profile, profile);
    let entry = windows_registry::test_entry(&profile)
        .unwrap()
        .expect("interrupted cleanup discarded ownership");
    assert_eq!(entry.state, windows_registry::EntryState::Closing);
    assert!(
        !removed.exists(),
        "fault did not occur after private deletion"
    );
    assert_recovered(&profile, &private, root, &foreign);
}

/// Crash between a successful native window-object revoke and its durable journal completion,
/// then prove a later process consumes the persisted in-progress marker without touching a
/// replacement DACL or replaying the already-removed grant.
fn interrupted_window_cleanup(root: &Path) {
    let _cleanup = CleanupAfterTest;
    std::fs::write(root.join("caller-owned.txt"), b"caller-owned").unwrap();
    let foreign = ForeignProfileAce::grant(root);
    let resource = plan(root, "hold").acquire().unwrap();
    let profile = resource.profile_name().to_string();
    let private = resource.private_tmp().unwrap().to_path_buf();
    drop(resource);
    let (crashed_profile, _) = crash_owner(root, "fault-cleanup", "cleanup-window-object-revoked");
    assert_eq!(crashed_profile, profile);
    let entry = windows_registry::test_entry(&profile)
        .unwrap()
        .expect("interrupted window cleanup discarded ownership");
    assert_eq!(entry.state, windows_registry::EntryState::Closing);
    assert_eq!(entry.window_objects.len(), 2);
    assert!(
        entry.window_object_revoke.is_some(),
        "window revoke intent was not durable before the native mutation"
    );
    assert_recovered(&profile, &private, root, &foreign);
}

/// A failed journal completion after the native DACL mutation must retain independent progress,
/// even though `finish_recovery` records the ordinary error in `recovery_error`.
fn window_revoke_journal_save_failure(root: &Path) {
    let _cleanup = CleanupAfterTest;
    std::fs::write(root.join("caller-owned.txt"), b"caller-owned").unwrap();
    let foreign = ForeignProfileAce::grant(root);
    let resource = plan(root, "hold").acquire().unwrap();
    let profile = resource.profile_name().to_string();
    let private = resource.private_tmp().unwrap().to_path_buf();
    drop(resource);

    unsafe { std::env::set_var(FAULT, "cleanup-window-object-journal-save") };
    let error = cleanup_resources().expect_err("injected journal completion save must fail");
    unsafe { std::env::remove_var(FAULT) };
    assert!(
        error
            .to_string()
            .contains("injected window-object cleanup journal save failure")
    );
    let entry = windows_registry::test_entry(&profile)
        .unwrap()
        .expect("journal save failure discarded ownership");
    assert_eq!(entry.state, windows_registry::EntryState::RecoveryNeeded);
    assert!(
        entry.window_object_revoke.is_some(),
        "ordinary recovery-error recording erased native-revoke progress"
    );
    assert!(
        entry.recovery_error.is_some(),
        "the injected journal failure was not recorded for diagnosis"
    );
    assert_recovered(&profile, &private, root, &foreign);
}

/// The witness is checked before progress is persisted. Crash at that boundary, recreate the
/// recorded names with a narrower same-SID grant, and prove the later cleanup keeps both the
/// replacement ACE and the ownership journal rather than treating absent witness as completion.
fn witness_before_intent_fault(root: &Path) {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let profile = format!("nub-test-window-witness-{}-{stamp}", std::process::id());
    let station_name = format!("nub-witness-station-{}-{stamp}", std::process::id());
    let desktop_name = format!("nub-witness-desktop-{}-{stamp}", std::process::id());
    let objects = create_test_window_objects(&station_name, &desktop_name).unwrap();
    let sid = super::launch::SidGuard(super::launch::derive_appcontainer(&profile).unwrap());
    crate::backend::windows_ace::grant_persistent(&objects.object, sid.0).unwrap();
    windows_registry::test_insert_window_object_recovery(&profile, objects.object.clone()).unwrap();
    cleanup_resources().unwrap();
    panic!("cleanup did not reach witness-before-intent crash transition");
}

fn window_witness_crash_does_not_retire_a_replacement(root: &Path) {
    let _cleanup = CleanupAfterTest;
    let (profile, _) = crash_owner(
        root,
        "window-witness-replacement-fault",
        "cleanup-window-object-witness-checked",
    );
    let entry = windows_registry::test_entry(&profile)
        .unwrap()
        .expect("witness-boundary crash discarded the journal");
    assert!(
        entry.window_object_revoke.is_none(),
        "witness-boundary crash persisted revoke authority before ownership was established"
    );
    assert_eq!(
        entry.window_objects.len(),
        1,
        "fixture must journal exactly one replacement candidate"
    );
    let object = entry.window_objects[0].clone();
    let replacement =
        create_test_window_objects(&object.station, object.desktop.as_deref().unwrap())
            .expect("same-name replacement window objects");
    let sid = super::launch::SidGuard(super::launch::derive_appcontainer(&profile).unwrap());
    crate::backend::windows_ace::test_grant_narrow_desktop_ace(&object, sid.0).unwrap();

    let error = cleanup_resources().expect_err("fresh replacement must retain the journal");
    assert!(error.to_string().contains("ownership witness is absent"));
    assert!(
        crate::backend::windows_ace::test_has_narrow_desktop_ace(&object, sid.0).unwrap(),
        "cleanup changed the same-SID replacement ACE"
    );
    let retained = windows_registry::test_entry(&profile)
        .unwrap()
        .expect("fresh replacement incorrectly retired its journal");
    assert_eq!(retained.state, windows_registry::EntryState::RecoveryNeeded);
    assert_eq!(retained.window_objects, vec![object]);
    assert!(retained.window_object_revoke.is_none());
    drop(replacement);
    windows_registry::test_remove_entry(&profile).unwrap();
}

fn cleanup_junction(root: &Path) {
    let _cleanup = CleanupAfterTest;
    let caller = root.join("project");
    let target = root.join("caller-target");
    std::fs::create_dir(&caller).unwrap();
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("survivor.txt"), b"junction-target").unwrap();
    let foreign = ForeignProfileAce::grant(&target);
    let resource = plan(&caller, "hold").acquire().unwrap();
    let profile = resource.profile_name().to_string();
    let private = resource.private_tmp().unwrap().to_path_buf();
    let junction = private.join("caller-junction");
    let output = std::process::Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(&junction)
        .arg(&target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "unprivileged junction creation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(junction.join("survivor.txt")).unwrap(),
        b"junction-target"
    );
    drop(resource);
    cleanup_resources().unwrap();
    assert!(windows_registry::test_entry(&profile).unwrap().is_none());
    assert!(!private.exists());
    assert!(target.is_dir());
    assert_eq!(
        std::fs::read(target.join("survivor.txt")).unwrap(),
        b"junction-target"
    );
    assert!(super::launch::test_profile_has_ace(&foreign.profile, &target).unwrap());
}

#[test]
fn windows_cleanup_recovers_abrupt_acquisition_transitions() {
    isolated_scenario("crash-transitions");
}

#[test]
fn windows_cleanup_retries_after_abrupt_private_root_removal() {
    isolated_scenario("cleanup-retry");
}

#[test]
fn windows_cleanup_retries_after_window_revoke_before_journal_completion() {
    isolated_scenario("cleanup-window-retry");
}

#[test]
fn windows_cleanup_retries_after_window_revoke_journal_save_failure() {
    isolated_scenario("cleanup-window-save-failure");
}

#[test]
fn windows_cleanup_witness_crash_does_not_retire_same_name_replacement() {
    isolated_scenario("window-witness-replacement-fault");
}

#[test]
fn windows_cleanup_never_follows_private_junction_into_caller_data() {
    isolated_scenario("cleanup-junction");
}

#[test]
fn windows_cleanup_recovers_aces_from_a_noncurrent_window_station() {
    // A process-wide station change belongs in a short-lived fixture process.  The fixture creates
    // a second station/desktop, restores its original station, then exercises the journal cleanup
    // against the recorded objects.  This models successive SSH logons in session 0 without
    // allowing the test to perturb the libtest process's own desktop attachment.
    isolated_scenario("station-recovery");
}

#[test]
fn windows_cleanup_rejects_same_name_window_object_replacement() {
    isolated_scenario("station-replacement");
}

#[test]
fn windows_cleanup_recovers_window_object_journal_from_later_process() {
    isolated_scenario("station-journal-crash");
}

#[test]
fn windows_cleanup_serializes_real_appcontainer_launch_with_station_recovery() {
    isolated_scenario("station-concurrent-launch");
}

#[test]
fn windows_cleanup_skips_a_live_independent_lease_and_preserves_caller_files() {
    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join("caller-owned.txt");
    std::fs::write(&marker, b"must survive cleanup").unwrap();
    let mut owner = OwnerFixture(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", FIXTURE, "--nocapture", "--test-threads=1"])
            .env(MODE, "owner")
            .env(ROOT, root.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let record = root.path().join(format!("owner-{}", owner.0.id()));
    wait_for(&record);
    let lines = std::fs::read_to_string(record).unwrap();
    let pid: u32 = lines.lines().nth(1).unwrap().parse().unwrap();
    cleanup_resources().unwrap();
    assert!(
        running(pid),
        "explicit cleanup evicted a live cross-process lease"
    );
    assert_eq!(std::fs::read(&marker).unwrap(), b"must survive cleanup");
    drop(owner.0.stdin.take());
    assert!(owner.0.wait().unwrap().success());
}

#[test]
fn windows_cleanup_reuses_equivalent_profile_across_processes_without_killing_survivor() {
    let root = tempfile::tempdir().unwrap();
    let owner = || {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", FIXTURE, "--nocapture", "--test-threads=1"])
            .env(MODE, "owner")
            .env(ROOT, root.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    };
    let mut first = OwnerFixture(owner());
    let mut second = OwnerFixture(owner());
    let read = |child: &mut std::process::Child| {
        let p = root.path().join(format!("owner-{}", child.id()));
        let deadline = Instant::now() + Duration::from_secs(30);
        while !p.exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "owner {} exited before readiness; acquisition diagnostics are on stderr",
                child.id()
            );
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {}",
                p.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::read_to_string(p).unwrap()
    };
    let a = read(&mut first.0);
    let b = read(&mut second.0);
    assert_eq!(
        a.lines().next(),
        b.lines().next(),
        "equivalent policies must share a profile"
    );
    let survivor: u32 = b.lines().nth(1).unwrap().parse().unwrap();
    first.0.kill().unwrap();
    first.0.wait().unwrap();
    cleanup_resources().unwrap();
    assert!(
        running(survivor),
        "cleanup after one owner died killed another owner’s command"
    );
    drop(second.0.stdin.take());
    assert!(second.0.wait().unwrap().success());
}

#[test]
#[ignore = "concurrent independent-owner acquisition stress"]
fn windows_cleanup_acquisition_stress() {
    for round in 0..8 {
        eprintln!("WINDOWS_ACQUIRE_STRESS round={round}");
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(
                windows_cleanup_reuses_equivalent_profile_across_processes_without_killing_survivor
            )
                })
                .collect();
            for worker in workers {
                worker.join().unwrap();
            }
        });
    }
    cleanup_resources().unwrap();
}

#[test]
fn windows_idle_cleanup_removes_only_owned_private_state_and_aces() {
    let root = tempfile::tempdir().unwrap();
    let caller_marker = root.path().join("caller-output.txt");
    std::fs::write(&caller_marker, b"caller-owned").unwrap();
    let _cleanup = CleanupAfterTest;
    let foreign = ForeignProfileAce::grant(root.path());

    let resource = plan(root.path(), "hold").acquire().unwrap();
    let profile = resource.profile_name().to_string();
    let owned_temp = resource.private_tmp().unwrap().to_path_buf();
    assert!(
        super::launch::test_profile_has_ace(&profile, root.path()).unwrap(),
        "resource acquisition did not install its tracked root ACE"
    );
    let mut child = resource
        .spawn_with_stdio(WindowsStdio::Null, WindowsStdio::Null, WindowsStdio::Null)
        .unwrap();
    wait_for(&root.path().join(format!("ready-{}", child.id())));
    child.kill().unwrap();
    let _ = child.wait().unwrap();
    drop(child);
    drop(resource);

    cleanup_resources().unwrap();
    assert!(
        !super::launch::test_profile_has_ace(&profile, root.path()).unwrap(),
        "idle cleanup retained the resource ACE for {profile}"
    );
    assert!(
        super::launch::test_profile_has_ace(&foreign.profile, root.path()).unwrap(),
        "idle cleanup removed an unrelated external ACE"
    );
    assert!(
        !owned_temp.exists(),
        "idle cleanup retained the registered private directory for {profile}: {}",
        owned_temp.display()
    );
    assert_eq!(std::fs::read(&caller_marker).unwrap(), b"caller-owned");
}

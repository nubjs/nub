#[cfg(target_os = "linux")]
#[path = "common/resource_counts.rs"]
mod resource_counts;

use nub_sandbox::policy::SelfProcFile;
use nub_sandbox::{CompileCtx, Homes, SandboxPolicy, ScopeCapabilities, compile};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

fn context(root: &Path) -> CompileCtx {
    CompileCtx::new(
        Homes {
            home: root.into(),
            cache: root.into(),
            tmp: root.into(),
            project: root.into(),
        },
        root.into(),
        ScopeCapabilities::approved(),
        // ⛔ THE AMBIENT MAP MUST CONTAIN THE WITHHELD SECRET OR THE ASSERTION ABOUT IT IS A
        // TEST THAT CANNOT FAIL. `WITHHELD_AMBIENT_SECRET` is asserted absent from the confined
        // child below; with an EMPTY map here nothing ever offered it to the compiler, so that
        // assertion held identically whether or not the env axis enforced anything at all — it
        // was checking that a variable nobody sets is unset. Seeded here, it is a real ambient
        // value that no `vars` entry admits, so the child sees it only if filtering breaks.
        BTreeMap::from([(
            "WITHHELD_AMBIENT_SECRET".to_string(),
            "must-not-reach-the-child".to_string(),
        )]),
    )
}

#[test]
fn metadata_grants_are_explicit_resolved_capabilities() {
    let root = tempfile::tempdir().unwrap();
    let value = json!({"fs": {"/proc/self/maps": "r", "/proc/self/stat": "r", "/proc/self/cmdline": "r"}, "net": false});
    let policy = compile(&value, &context(root.path())).unwrap();
    assert_eq!(
        policy.fs.self_proc,
        [
            SelfProcFile::Maps,
            SelfProcFile::Stat,
            SelfProcFile::Cmdline
        ]
        .into()
    );
    assert!(
        policy.fs.rules.entries.is_empty(),
        "no compiler-process PID in ordinary grants"
    );
    let encoded = serde_json::to_value(&policy).unwrap();
    let decoded: SandboxPolicy = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded.fs.self_proc, policy.fs.self_proc);
    let plain = compile(&json!({"fs": false}), &context(root.path())).unwrap();
    assert!(plain.fs.self_proc.is_empty());
    assert!(
        serde_json::to_value(&plain).unwrap()["fs"]
            .get("self_proc")
            .is_none()
    );
}

#[test]
fn writable_metadata_is_rejected_including_reused_grants() {
    let root = tempfile::tempdir().unwrap();
    for value in [
        json!({"fs": {"/proc/self/maps": "rw"}}),
        json!({"fs": {"/proc/self/stat": true}}),
        json!({"fs": {"/proc/self/cmdline": "rw"}}),
        json!({"fs": {"/proc/self/task/*/stat": "rw"}}),
        json!({"fs": ["/proc/self/maps"]}),
        json!({"shared": {"/proc/self/stat": "rw"}, "fs": {"...:#/shared": true}}),
    ] {
        assert!(compile(&value, &context(root.path())).is_err(), "{value}");
    }
    let document =
        json!({"shared": {"/proc/self/stat": "r"}, "sandbox": {"fs": {"...:#/shared": true}}});
    let policy = compile(
        &document["sandbox"],
        &context(root.path()).with_document(document.clone()),
    )
    .unwrap();
    assert_eq!(policy.fs.self_proc, [SelfProcFile::Stat].into());
}

#[test]
fn shared_tool_state_is_initialized_once_and_survives_session_close() {
    use nub_sandbox::policy::{CanonGlob, Effect, FsAccess, FsOrigin, FsRule};
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("coordination");
    let mut policy = compile(&json!({"fs": false}), &context(root.path())).unwrap();
    policy.fs.rules.entries.push(FsRule {
        matcher: CanonGlob(state.to_string_lossy().into_owned()),
        effect: Effect::Allow,
        access: FsAccess::Read,
        origin: FsOrigin::SharedToolState,
    });
    nub_sandbox::Sandbox::acquire(&policy).unwrap().close();
    assert!(
        !state.exists(),
        "read-only acquisition must not create shared state"
    );
    policy.fs.rules.entries[0].access = FsAccess::ReadWrite;
    for _ in 0..2 {
        nub_sandbox::Sandbox::acquire(&policy).unwrap().close();
        assert!(
            state.is_dir(),
            "shared tool state is not private cleanup data"
        );
    }
}

#[cfg(unix)]
#[test]
fn tool_directory_listing_leaf() {
    let Ok(root) = std::env::var("TOOL_LISTING_ROOT") else {
        return;
    };
    let root = Path::new(&root);
    let confined = std::env::var("TOOL_LISTING_CONFINED").unwrap() == "1";
    let check = |result: std::io::Result<()>| {
        if confined {
            assert_eq!(
                result.unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
        } else {
            result.unwrap();
        }
    };
    assert!(
        std::fs::read_dir(root)
            .unwrap()
            .any(|entry| entry.unwrap().file_name() == "secret")
    );
    assert!(std::fs::read_dir("/tmp").is_ok());
    for path in [root.join("secret"), root.join("sibling/secret")] {
        check(std::fs::read(&path).map(|_| ()));
        check(std::fs::write(&path, "changed"));
        check(std::fs::remove_file(&path));
    }
    check(std::fs::write(root.join("new-file"), "new"));
    std::fs::write(root.join("project/allowed"), "allowed").unwrap();
}

#[cfg(unix)]
#[test]
fn tool_directory_listing_does_not_grant_sibling_contents_or_writes() {
    use nub_sandbox::{CommandSpec, Sandbox};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let project = root.join("project");
    for dir in [&project, &root.join("sibling")] {
        std::fs::create_dir(dir).unwrap();
    }
    let exe = std::env::current_exe().unwrap();
    let mut ctx = context(&project);
    ctx.homes.home = root.join("home");
    ctx.homes.cache = root.join("cache");
    let mut fs = json!({"./": "rw", "$tooldirs": "rw", "$tmp": "rw"});
    fs[exe.parent().unwrap().to_str().unwrap()] = json!("r");
    let mut policy = compile(&json!({"fs": fs, "net": false}), &ctx).unwrap();
    policy
        .env
        .constructed
        .insert("TOOL_LISTING_ROOT".into(), root.display().to_string());
    let args = ["--exact", "tool_directory_listing_leaf", "--nocapture"];
    for confined in [false, true] {
        for path in [root.join("secret"), root.join("sibling/secret")] {
            std::fs::write(path, "withheld").unwrap();
        }
        policy.env.constructed.insert(
            "TOOL_LISTING_CONFINED".into(),
            if confined { "1" } else { "0" }.into(),
        );
        let output = if confined {
            let sandbox = Sandbox::acquire(&policy).unwrap();
            let prepared = sandbox
                .prepare(CommandSpec::new(&exe).args(args).cwd(&project))
                .unwrap();
            assert!(
                prepared.degradation.lost.is_empty(),
                "{:?}",
                prepared.degradation
            );
            let output = prepared.output().unwrap();
            sandbox.close();
            output
        } else {
            std::process::Command::new(&exe)
                .args(args)
                .envs(&policy.env.constructed)
                .output()
                .unwrap()
        };
        assert!(output.status.success(), "confined={confined}: {output:?}");
        if !confined {
            std::fs::remove_file(root.join("new-file")).unwrap();
        }
    }
}

#[cfg(not(target_os = "linux"))]
#[test]
fn unsupported_hosts_refuse_metadata_at_acquisition() {
    let root = tempfile::tempdir().unwrap();
    let policy = compile(
        &json!({"fs": {"/proc/self/stat": "r"}}),
        &context(root.path()),
    )
    .unwrap();
    let error = nub_sandbox::Sandbox::acquire(&policy)
        .err()
        .expect("must refuse");
    assert_eq!(error.lost, ["fs-self-proc"]);
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use nub_sandbox::{CommandSpec, Sandbox};
    use std::ffi::CString;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn fixture(files: &[&str]) -> (tempfile::TempDir, Sandbox) {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(root.path().join("secret"), "WITHHELD").unwrap();
        std::fs::write(project.join("allowed"), "ALLOWED").unwrap();
        std::fs::write(
            project.join(std::ffi::OsStr::from_bytes(b"nonutf8-\xff")),
            "BYTES",
        )
        .unwrap();
        let exe = std::env::current_exe().unwrap();
        let mut fs = json!({"./": "rw", "$tmp": "rw"});
        fs[exe.parent().unwrap().to_string_lossy().as_ref()] = json!("r");
        for file in files {
            fs[format!("/proc/self/{file}")] = json!("r");
        }
        let mut policy = compile(&json!({"fs": fs, "net": false}), &context(&project)).unwrap();
        policy.env.constructed.extend([
            ("SELF_PROC_CASE".into(), files.join(",")),
            ("SELF_PROC_ROOT".into(), root.path().display().to_string()),
            ("SELF_PROC_EXE".into(), exe.display().to_string()),
            ("SELF_PROC_OWNER".into(), std::process::id().to_string()),
            ("SELF_PROC_LOOP".into(), "1".into()),
        ]);
        (root, Sandbox::acquire(&policy).unwrap())
    }

    fn spec(root: &Path, name: &str) -> CommandSpec {
        CommandSpec::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .cwd(root.join("project"))
    }

    fn check_metadata() {
        let selected = std::env::var("SELF_PROC_CASE").unwrap();
        for name in ["maps", "stat", "cmdline"] {
            let read = std::fs::read_to_string(format!("/proc/self/{name}"));
            if selected.split(',').any(|file| file == name) {
                let text = read.unwrap();
                if name == "stat" {
                    assert_eq!(
                        text.split_whitespace().next().unwrap(),
                        std::process::id().to_string()
                    );
                } else if name == "maps" {
                    assert!(
                        text.contains("self_proc"),
                        "maps describes this executable: {text}"
                    );
                } else {
                    assert_eq!(
                        text.split('\0').next().unwrap(),
                        std::env::args().next().unwrap()
                    );
                    assert!(text.contains("linux::metadata_child"));
                }
            } else {
                assert_eq!(
                    read.unwrap_err().kind(),
                    std::io::ErrorKind::PermissionDenied
                );
            }
        }
    }

    #[test]
    fn metadata_child() {
        if std::env::var_os("SELF_PROC_CASE").is_none() {
            return;
        }
        check_metadata();
        std::thread::spawn(check_metadata).join().unwrap();
        let root = std::path::PathBuf::from(std::env::var_os("SELF_PROC_ROOT").unwrap());
        let owner = std::env::var("SELF_PROC_OWNER").unwrap();
        for path in [
            root.join("secret"),
            "/proc/self/environ".into(),
            "/proc/thread-self/stat".into(),
            format!("/proc/{owner}/maps").into(),
            format!("/proc/{owner}/environ").into(),
            format!("/proc/{owner}/cmdline").into(),
            format!("/proc/{}/stat", std::process::id()).into(),
        ] {
            assert_eq!(
                std::fs::read(&path).unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied,
                "{}",
                path.display()
            );
        }
        assert!(std::env::var_os("WITHHELD_AMBIENT_SECRET").is_none());
        assert_eq!(std::fs::read_to_string("allowed").unwrap(), "ALLOWED");
        assert_eq!(
            std::fs::read(std::ffi::OsStr::from_bytes(b"nonutf8-\xff")).unwrap(),
            b"BYTES"
        );
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open("/proc/self/stat")
                .is_err()
        );
        if std::env::var_os("SELF_PROC_GRANDCHILD").is_none() {
            let output = Command::new(std::env::var_os("SELF_PROC_EXE").unwrap())
                .args(["--exact", "linux::metadata_child", "--nocapture"])
                .env("SELF_PROC_GRANDCHILD", "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "grandchild: {output:?}");
        }
    }

    #[test]
    fn retained_metadata_is_per_command_thread_and_descendant() {
        for files in [
            &[][..],
            &["maps"][..],
            &["stat"][..],
            &["cmdline"][..],
            &["maps", "stat", "cmdline"][..],
        ] {
            let (root, sandbox) = fixture(files);
            for _ in 0..3 {
                let output = sandbox
                    .prepare(spec(root.path(), "linux::metadata_child"))
                    .unwrap()
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{files:?}: {output:?}");
            }
            sandbox.close();
        }
    }

    #[test]
    fn task_metadata_child() {
        if std::env::var_os("SELF_PROC_CASE").is_none() {
            return;
        }
        let check = || {
            let tid = unsafe { libc::syscall(libc::SYS_gettid) };
            let tasks = std::fs::read_dir("/proc/self/task")
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            assert!(
                tasks
                    .iter()
                    .any(|entry| entry.to_string_lossy() == tid.to_string())
            );
            let stat = std::fs::read_to_string(format!("/proc/self/task/{tid}/stat")).unwrap();
            assert_eq!(stat.split_whitespace().next().unwrap(), tid.to_string());
            let status = std::fs::read_to_string(format!("/proc/self/task/{tid}/status")).unwrap();
            assert!(status.contains(&format!("Tgid:\t{}\n", std::process::id())));
            assert!(
                std::fs::read_to_string("/proc/self/status")
                    .unwrap()
                    .contains("VmRSS:")
            );
            assert!(
                !std::fs::read_to_string("/proc/self/statm")
                    .unwrap()
                    .is_empty()
            );
            let owner = std::env::var("SELF_PROC_OWNER").unwrap();
            for path in [
                format!("/proc/self/task/{tid}/environ"),
                format!("/proc/self/task/{owner}/stat"),
                format!("/proc/self/task/../../{owner}/cmdline"),
                "/proc/self/environ".into(),
                "/proc/self/mem".into(),
            ] {
                assert!(std::fs::read(&path).is_err(), "unexpected read: {path}");
            }
            let dir = std::fs::File::open("/proc/self/task").unwrap();
            let escape = CString::new(format!("../../{owner}/environ")).unwrap();
            assert_eq!(
                unsafe { libc::openat(dir.as_raw_fd(), escape.as_ptr(), libc::O_RDONLY) },
                -1
            );
        };
        check();
        std::thread::spawn(check).join().unwrap();
    }

    #[test]
    fn task_metadata_and_tool_bundle_preserve_process_boundaries() {
        let files = ["status", "statm", "task", "task/*/stat", "task/*/status"];
        let (root, sandbox) = fixture(&files);
        let output = sandbox
            .prepare(spec(root.path(), "linux::task_metadata_child"))
            .unwrap()
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        for fs in [
            json!({"$tooldirs": "rw"}),
            json!({"$tooldirs": "r"}),
            json!(["$tooldirs"]),
        ] {
            let bundle = compile(&json!({"fs": fs}), &context(root.path())).unwrap();
            assert_eq!(bundle.fs.self_proc.len(), 8);
            // The self-proc metadata (the 8 files above) lands in `self_proc`, never as an
            // ordinary GRANT — so no /proc path is ALLOWED in `rules.entries`. The cross-process
            // secret floor does add /proc DENY rules here (it rides every grants-read policy);
            // those are the floor, not a leak, so this boundary check is for allow rules only.
            assert!(bundle.fs.rules.entries.iter().all(|rule| {
                rule.effect != nub_sandbox::policy::Effect::Allow
                    || !rule.matcher.as_str().starts_with("/proc/")
            }));
        }
    }

    #[test]
    fn syscall_child() {
        if std::env::var_os("SELF_PROC_CASE").is_none() {
            return;
        }
        let path = CString::new("/proc/self/stat").unwrap();
        for (close_exec, nonblock) in [(0, 0), (libc::O_CLOEXEC, libc::O_NONBLOCK)] {
            let flags = close_exec | nonblock;
            let how = [flags as u64, 0, 0];
            // `SYS_open` is x86_64-only, so only that arch pushes onto this.
            #[cfg_attr(not(target_arch = "x86_64"), allow(unused_mut))]
            let mut fds = vec![
                unsafe { libc::syscall(libc::SYS_openat, libc::AT_FDCWD, path.as_ptr(), flags, 0) },
                unsafe {
                    libc::syscall(
                        libc::SYS_openat2,
                        libc::AT_FDCWD,
                        path.as_ptr(),
                        how.as_ptr(),
                        24,
                    )
                },
            ];
            #[cfg(target_arch = "x86_64")]
            fds.push(unsafe { libc::syscall(libc::SYS_open, path.as_ptr(), flags, 0) });
            for fd in fds {
                assert!(fd >= 0, "open: {}", std::io::Error::last_os_error());
                let mut file = unsafe { std::fs::File::from_raw_fd(fd as i32) };
                let status = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
                assert_eq!(status & libc::O_NONBLOCK, nonblock);
                let descriptor = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
                assert_eq!(descriptor & libc::FD_CLOEXEC, i32::from(close_exec != 0));
                let mut text = String::new();
                file.read_to_string(&mut text).unwrap();
                assert_eq!(
                    text.split_whitespace().next().unwrap(),
                    std::process::id().to_string()
                );
            }
        }
        for flags in [libc::O_PATH, libc::O_DIRECTORY, libc::O_RDWR, libc::O_CREAT] {
            let fd = unsafe { libc::open(path.as_ptr(), flags, 0o600) };
            assert_eq!(fd, -1, "unsupported flags: {flags}");
        }
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat,
                libc::AT_FDCWD,
                std::ptr::null::<u8>(),
                0,
                0,
            )
        };
        assert_eq!(fd, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EFAULT)
        );
    }

    #[test]
    fn read_only_syscalls_preserve_descriptor_flags() {
        let (root, sandbox) = fixture(&["stat"]);
        let output = sandbox
            .prepare(spec(root.path(), "linux::syscall_child"))
            .unwrap()
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    #[test]
    fn reading_child() {
        if std::env::var_os("SELF_PROC_CASE").is_none() {
            return;
        }
        std::fs::write("ready", "ready").unwrap();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(30) {
            check_metadata();
        }
    }

    #[test]
    fn cancellable_metadata_wait_preserves_exit_and_reaps_cancellation() {
        use std::sync::atomic::AtomicBool;
        let (root, sandbox) = fixture(&["maps", "stat"]);
        for _ in 0..8 {
            let mut child = sandbox
                .prepare(CommandSpec::new("/bin/true"))
                .unwrap()
                .spawn()
                .unwrap();
            assert!(
                child
                    .wait_cancellable(&AtomicBool::new(false))
                    .unwrap()
                    .success()
            );
        }
        let mut child = sandbox
            .prepare(spec(root.path(), "linux::reading_child"))
            .unwrap()
            .spawn()
            .unwrap();
        let pid = child.id();
        let started = Instant::now();
        while !root.path().join("project/ready").exists() {
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(10));
        }
        let error = child.wait_cancellable(&AtomicBool::new(true)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "cancelled child reaped"
        );
    }

    #[test]
    fn cancellation_reclaims_metadata_workers_and_descriptors() {
        if std::env::var_os("SELF_PROC_COUNTER_OWNER").is_none() {
            // Whole-process counts need an isolated test host, not sibling tests'
            // concurrently opening descriptors and creating supervisor threads.
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "linux::cancellation_reclaims_metadata_workers_and_descriptors",
                    "--nocapture",
                ])
                .env("SELF_PROC_COUNTER_OWNER", "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }
        let (root, sandbox) = fixture(&["maps", "stat"]);
        let counts = crate::resource_counts::settled;
        // Warm the process-global owner guardian before establishing the baseline.
        assert!(
            sandbox
                .prepare(CommandSpec::new("/bin/true"))
                .unwrap()
                .status()
                .unwrap()
                .success()
        );
        let baseline = counts();
        for _ in 0..20 {
            let ready = root.path().join("project/ready");
            let _ = std::fs::remove_file(&ready);
            let child = sandbox
                .prepare(spec(root.path(), "linux::reading_child"))
                .unwrap()
                .spawn()
                .unwrap();
            let start = Instant::now();
            while !ready.exists() {
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "reader never became ready"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            drop(child);
        }
        assert_eq!(
            counts(),
            baseline,
            "all per-command resources return to baseline"
        );
    }
}

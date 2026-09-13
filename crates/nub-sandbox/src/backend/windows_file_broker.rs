//! Command-owned file authority. Production admission remains disabled until
//! the complete namespace and native-execution contract is implemented.

#[cfg(target_env = "msvc")]
use crate::matcher::path::PathMatcher;

#[cfg(target_env = "msvc")]
struct Authority {
    _matcher: PathMatcher,
    #[cfg(test)]
    blocking: Option<std::sync::Arc<TestBlockedAuthorization>>,
    #[cfg(test)]
    process: usize,
}

pub(super) struct FileBroker {
    #[cfg(target_env = "msvc")]
    native: std::ptr::NonNull<std::ffi::c_void>,
    #[cfg(target_env = "msvc")]
    pub(super) endpoint: Vec<u16>,
    // Stable allocation borrowed by native workers until Drop has joined them.
    #[cfg(target_env = "msvc")]
    _authority: Box<Authority>,
}

// SAFETY: the native owner synchronizes cancellation, and its immutable matcher
// allocation is stable across moves. Drop joins workers before freeing it.
unsafe impl Send for FileBroker {}

impl FileBroker {
    pub(super) fn cancel(&self) {
        #[cfg(target_env = "msvc")]
        {
            unsafe extern "C" {
                fn sandbox_file_broker_cancel(broker: *mut std::ffi::c_void);
            }
            // SAFETY: this only signals the owned workers. Drop joins them
            // before freeing the native owner or its borrowed matcher.
            unsafe { sandbox_file_broker_cancel(self.native.as_ptr()) };
        }
    }
}

impl Drop for FileBroker {
    fn drop(&mut self) {
        #[cfg(target_env = "msvc")]
        {
            unsafe extern "C" {
                fn sandbox_file_broker_stop(broker: *mut std::ffi::c_void);
            }
            // SAFETY: this is the unique allocation returned by start. The
            // borrowed Job remains owned by WindowsChild throughout this call.
            unsafe { sandbox_file_broker_stop(self.native.as_ptr()) };
        }
    }
}

#[cfg(all(test, target_env = "msvc"))]
thread_local! {
    static TEST_RULES: std::cell::RefCell<Option<TestRules>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, target_env = "msvc"))]
#[derive(Clone)]
struct TestRules {
    rules: crate::policy::FsRuleSet,
    blocking: Option<std::sync::Arc<TestBlockedAuthorization>>,
}

#[cfg(all(test, target_env = "msvc"))]
struct TestBlockedAuthorization {
    entered: std::os::windows::io::OwnedHandle,
    result: std::sync::atomic::AtomicU32,
    skip: std::sync::atomic::AtomicU32,
}

#[cfg(all(test, target_env = "msvc"))]
impl TestBlockedAuthorization {
    fn wait_for_child_exit(&self, process: usize) {
        use std::os::windows::io::AsRawHandle as _;
        use std::sync::atomic::Ordering;
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, SetEvent, WaitForSingleObject,
        };
        if self
            .skip
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return;
        }
        // SAFETY: WindowsChild retains its process handle until this broker's
        // workers have joined. The event is owned by this shared test state.
        let result = unsafe {
            if SetEvent(self.entered.as_raw_handle()) == 0 {
                3
            } else if WaitForSingleObject(process as _, 30_000) != WAIT_OBJECT_0 {
                2
            } else {
                let mut code = 0;
                if GetExitCodeProcess(process as _, &mut code) != 0 && code == 1 {
                    1
                } else {
                    4
                }
            }
        };
        self.result.store(result, Ordering::Release);
    }
}

#[cfg(all(test, target_env = "msvc"))]
pub(super) fn with_test_rules<T>(rules: crate::policy::FsRuleSet, run: impl FnOnce() -> T) -> T {
    with_test_config(
        TestRules {
            rules,
            blocking: None,
        },
        run,
    )
}

#[cfg(all(test, target_env = "msvc"))]
fn with_test_config<T>(rules: TestRules, run: impl FnOnce() -> T) -> T {
    struct Reset(Option<TestRules>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_RULES.set(self.0.take());
        }
    }
    let _reset = Reset(TEST_RULES.replace(Some(rules)));
    run()
}

#[cfg(all(test, target_env = "msvc"))]
pub(super) fn start_for_test(
    process: *mut std::ffi::c_void,
    job: *mut std::ffi::c_void,
) -> std::io::Result<Option<FileBroker>> {
    use crate::policy::{Effect, FsAccess};
    use std::io;

    unsafe extern "C" fn authorize(
        context: *const std::ffi::c_void,
        path: *const u16,
        length: u32,
    ) -> u32 {
        if context.is_null() || path.is_null() || length == 0 || length >= 1024 {
            return 0;
        }
        // SAFETY: native resolver supplies its bounded UTF-16 name and the
        // immutable boxed matcher retained until all workers have stopped.
        let (authority, name) = unsafe {
            (
                &*context.cast::<Authority>(),
                std::slice::from_raw_parts(path, length as usize),
            )
        };
        let Ok(name) = String::from_utf16(name) else {
            return 0;
        };
        let decision = authority._matcher.decide_verified_name(&name);
        if decision.effect == Effect::Allow
            && let Some(blocking) = &authority.blocking
        {
            blocking.wait_for_child_exit(authority.process);
        }
        match (decision.effect, decision.access) {
            (Effect::Allow, FsAccess::Read) => 1,
            (Effect::Allow, FsAccess::ReadWrite) => 3,
            (Effect::Deny, _) => 0,
        }
    }

    let Some(rules) = TEST_RULES.with_borrow(Clone::clone) else {
        return Ok(None);
    };
    unsafe extern "C" {
        fn sandbox_file_broker_start(
            process: *mut std::ffi::c_void,
            job: *mut std::ffi::c_void,
            endpoint: *const u16,
            authorize: unsafe extern "C" fn(*const std::ffi::c_void, *const u16, u32) -> u32,
            context: *const std::ffi::c_void,
            broker: *mut *mut std::ffi::c_void,
        ) -> u32;
    }
    let authority = Box::new(Authority {
        _matcher: PathMatcher::new(&rules.rules),
        blocking: rules.blocking,
        process: process as usize,
    });
    let mut nonce = [0u8; 16];
    getrandom::getrandom(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
    let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
    let endpoint: Vec<u16> = format!(r"\\.\pipe\LOCAL\sandbox-files-{nonce}")
        .encode_utf16()
        .chain([0])
        .collect();
    let mut native = std::ptr::null_mut();
    // SAFETY: called only while WindowsChild owns the suspended target and Job;
    // no worker may outlive the boxed matcher or that Job.
    let error = unsafe {
        sandbox_file_broker_start(
            process,
            job,
            endpoint.as_ptr(),
            authorize,
            (&*authority as *const Authority).cast(),
            &mut native,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let native = std::ptr::NonNull::new(native)
        .ok_or_else(|| io::Error::other("file broker returned no owner"))?;
    Ok(Some(FileBroker {
        native,
        endpoint,
        _authority: authority,
    }))
}

#[cfg(all(test, target_env = "msvc"))]
mod tests {
    use super::*;
    use crate::policy::{CanonGlob, Effect, FsAccess, FsOrigin, FsRule, FsRuleSet};

    const CHILD: &str = "backend::windows_file_broker::tests::file_broker_native_child";

    #[test]
    fn file_broker_cancellation_child() {
        let Some(path) = std::env::var_os("NUB_FILE_BROKER_CANCELLATION_FILE") else {
            return;
        };
        // A client IPC timeout must not release the synthetic blocked worker by
        // exiting the process. Only Job termination should produce exit code 1.
        match std::env::var("NUB_FILE_BROKER_CANCELLATION_OPERATION")
            .unwrap()
            .as_str()
        {
            "remove" => {
                let _ = std::fs::remove_file(path);
            }
            "create" | "write" => {
                let _ = std::fs::write(path, b"must not be written after cancellation");
            }
            "mkdir" => {
                let _ = std::fs::create_dir(path);
            }
            "read" => {
                let _ = std::fs::File::open(path);
            }
            operation => panic!("unknown cancellation operation: {operation}"),
        }
        std::thread::sleep(std::time::Duration::from_secs(120));
        panic!("cancellation fixture survived its command Job deadline");
    }

    #[test]
    #[ignore = "requires an ordinary-user native Windows acceptance run"]
    fn file_broker_kills_job_before_joining_blocked_worker() {
        for operation in ["read", "create", "write", "mkdir"] {
            cancellation_control(operation);
        }
    }

    #[test]
    #[ignore = "requires an ordinary-user native Windows acceptance run"]
    fn file_broker_cancels_namespace_before_mutation() {
        cancellation_control("remove");
    }

    fn cancellation_control(operation: &str) {
        use super::super::windows::WindowsStdio;
        use crate::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
        use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
        use std::sync::{
            Arc,
            atomic::{AtomicU32, Ordering},
        };
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

        const CHILD: &str = "backend::windows_file_broker::tests::file_broker_cancellation_child";
        let binary = std::env::current_exe().unwrap();
        let root = tempfile::Builder::new()
            .prefix("sandbox-file-cancellation-")
            .tempdir_in(std::env::var_os("USERPROFILE").unwrap())
            .unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let file = root.path().join("blocked.json");
        std::fs::write(&file, b"ordinary caller open succeeds").unwrap();
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"ordinary caller open succeeds"
        );
        let initially_absent = matches!(operation, "create" | "mkdir");
        if initially_absent {
            std::fs::remove_file(&file).unwrap();
        }
        // SAFETY: unnamed, noninheritable manual-reset event, uniquely owned
        // below and retained until the native worker has returned.
        let entered = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        assert!(!entered.is_null(), "{}", std::io::Error::last_os_error());
        let blocking = Arc::new(TestBlockedAuthorization {
            // SAFETY: successful CreateEventW returned this unique handle.
            entered: unsafe { OwnedHandle::from_raw_handle(entered) },
            result: AtomicU32::new(0),
            skip: AtomicU32::new(u32::from(operation == "remove")),
        });
        let ctx = CompileCtx::new(
            Homes {
                home: root.path().join("home"),
                cache: root.path().join("cache"),
                tmp: root.path().join("tmp"),
                project: project.clone(),
            },
            project.clone(),
            ScopeCapabilities::approved(),
            std::env::vars().collect(),
        );
        let mut policy = compile(
            &serde_json::json!({"fs": {"./": "r", "$tmp": "rw",
            binary.parent().unwrap().to_str().unwrap(): "r"}, "net": false}),
            &ctx,
        )
        .unwrap();
        policy.env.constructed.insert(
            "NUB_FILE_BROKER_CANCELLATION_FILE".into(),
            file.to_str().unwrap().into(),
        );
        policy.env.constructed.insert(
            "NUB_FILE_BROKER_CANCELLATION_OPERATION".into(),
            operation.into(),
        );
        policy
            .env
            .constructed
            .insert("NUB_JAIL_DUMP_POLICY".into(), "1".into());
        let sandbox = Sandbox::with_windows_native_compat(&policy).unwrap();
        let mut prepared = sandbox
            .prepare(
                CommandSpec::new(&binary)
                    .args(["--exact", CHILD, "--nocapture"])
                    .cwd(&project),
            )
            .unwrap();
        assert!(
            prepared.degradation.lost.is_empty(),
            "{:?}",
            prepared.degradation
        );
        let launch = prepared.launch.take().unwrap();
        let resource = prepared.acquire_windows_resource(launch).unwrap();
        let rules = TestRules {
            rules: FsRuleSet {
                default_effect: Effect::Deny,
                entries: vec![rule(
                    file.to_str().unwrap(),
                    if operation == "read" {
                        FsAccess::Read
                    } else {
                        FsAccess::ReadWrite
                    },
                )],
            },
            blocking: Some(blocking.clone()),
        };
        let mut child = with_test_config(rules, || {
            resource.spawn_before_resume(
                WindowsStdio::Null,
                WindowsStdio::Inherit,
                WindowsStdio::Inherit,
                |_| Ok(()),
            )
        })
        .unwrap();
        // Entry occurs only after the native client authenticated, the real
        // resolver pinned the file or parent, and the immutable matcher allowed it.
        // Always terminate/reap before asserting, including a missing-entry failure.
        let entered = unsafe { WaitForSingleObject(blocking.entered.as_raw_handle(), 30_000) };
        let live = child.try_wait().unwrap().is_none();
        let kill = child.kill();
        let status = child.wait().unwrap();
        let result = blocking.result.load(Ordering::Acquire);
        drop(child);
        drop(resource);
        drop(prepared);
        sandbox.close();
        assert_eq!(entered, WAIT_OBJECT_0, "broker callback was never entered");
        assert!(
            live,
            "target exited before the parent requested Job termination"
        );
        kill.unwrap();
        assert_eq!(status.code(), Some(1), "unexpected target exit: {status}");
        assert_eq!(
            result, 1,
            "worker did not observe Job-killed child before join: 0=not run, 2=deadline, 3=event failure, 4=other exit"
        );
        if initially_absent {
            assert!(!file.exists(), "cancelled {operation} created the target");
        } else {
            assert_eq!(
                std::fs::read(&file).unwrap(),
                b"ordinary caller open succeeds",
                "cancelled {operation} changed the target"
            );
        }
        println!("FILE_BROKER_CANCELLED_OPERATION={operation}");
        println!("FILE_BROKER_BLOCKED_WORKER_ENTERED");
        println!("FILE_BROKER_WORKER_OBSERVED_JOB_EXIT=1");
        println!("FILE_BROKER_KILL_BEFORE_JOIN_OK");
    }

    #[test]
    fn file_broker_exact_frames_and_foreign_job_fail_closed() {
        unsafe extern "C" {
            fn sandbox_file_broker_test_frames(name: *const u16) -> u32;
            fn sandbox_file_broker_test_foreign_client(name: *const u16) -> u32;
        }
        for foreign in [false, true] {
            let mut nonce = [0u8; 16];
            getrandom::getrandom(&mut nonce).unwrap();
            let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
            let name: Vec<u16> = format!(r"\\.\pipe\LOCAL\sandbox-file-test-{nonce}")
                .encode_utf16()
                .chain([0])
                .collect();
            // SAFETY: native fixtures own and drain their pipe I/O, and join
            // every worker before the borrowed context and empty Job release.
            let result = unsafe {
                if foreign {
                    sandbox_file_broker_test_foreign_client(name.as_ptr())
                } else {
                    sandbox_file_broker_test_frames(name.as_ptr())
                }
            };
            assert_eq!(result, 0, "foreign={foreign}: {result}");
        }
    }

    fn rule(pattern: &str, access: FsAccess) -> FsRule {
        FsRule {
            matcher: CanonGlob(pattern.replace('\\', "/")),
            effect: Effect::Allow,
            access,
            origin: FsOrigin::Authored,
        }
    }

    #[test]
    fn file_broker_verified_name_uses_positive_union_without_namespace_reopen() {
        use crate::{CompileCtx, Homes, ScopeCapabilities, compile};

        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let output = root.path().join("output");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&output).unwrap();
        let future = output.join("future.json");
        std::fs::write(&future, b"held object").unwrap();
        let ctx = CompileCtx::new(
            Homes {
                home: root.path().join("home"),
                cache: root.path().join("cache"),
                tmp: root.path().join("tmp"),
                project: project.clone(),
            },
            project,
            ScopeCapabilities::approved(),
            std::env::vars().collect(),
        );
        // Exercise the public JSON grammar used by the native fixture. The
        // narrower read overlaps the globbed read-write grant, so the held
        // name must retain the unioned read-write authority.
        let policy = compile(
            &serde_json::json!({"fs": {
                "./": "r",
                "$tmp": false,
                format!("{}/*.json", output.display()): "rw",
                future.to_str().unwrap(): "r",
            }, "net": false}),
            &ctx,
        )
        .unwrap();
        let matcher = PathMatcher::new(&policy.fs.rules);
        let held = std::fs::canonicalize(&future).unwrap();
        let decision = matcher.decide_verified_name(held.to_str().unwrap());
        assert_eq!(
            (decision.effect, decision.access),
            (Effect::Allow, FsAccess::ReadWrite)
        );
        assert_eq!(
            matcher
                .decide_verified_name(
                    std::fs::canonicalize(&output)
                        .unwrap()
                        .join("near.txt")
                        .to_str()
                        .unwrap()
                )
                .effect,
            Effect::Deny
        );
        assert_eq!(
            matcher
                .decide_verified_name(
                    std::fs::canonicalize(&output)
                        .unwrap()
                        .join("nested")
                        .join("future.json")
                        .to_str()
                        .unwrap()
                )
                .effect,
            Effect::Deny
        );
    }

    #[repr(C)]
    #[derive(Clone)]
    struct Request {
        version: u32,
        size: u32,
        operation: u32,
        access: u32,
        share: u32,
        disposition: u32,
        options: u32,
        attributes: u32,
        length: u32,
        path: [u16; 1024],
        source_low: u32,
        source_high: u32,
    }
    fn request(path: &str) -> Request {
        let mut request = Request {
            version: 2,
            size: 2092,
            operation: 1,
            access: 0x80000000,
            share: 3,
            disposition: 1,
            options: 0x60,
            attributes: 0,
            length: 0,
            path: [0; 1024],
            source_low: 0,
            source_high: 0,
        };
        let path: Vec<_> = path.encode_utf16().collect();
        request.length = path.len() as u32;
        request.path[..path.len()].copy_from_slice(&path);
        request
    }
    fn validate(request: &Request) -> i32 {
        unsafe extern "C" {
            fn sandbox_file_broker_validate(request: *const Request) -> i32;
        }
        assert_eq!(std::mem::size_of::<Request>(), 2092);
        // SAFETY: pointer-free fixed-width layout matches the native protocol.
        unsafe { sandbox_file_broker_validate(request) }
    }
    fn normalize_directory_capture(request: &mut Request) -> bool {
        unsafe extern "C" {
            fn sandbox_file_broker_normalize_directory_capture(request: *mut Request) -> i32;
        }
        // SAFETY: pointer-free fixed-width request is exclusively borrowed for
        // the native capture normalization regression.
        unsafe { sandbox_file_broker_normalize_directory_capture(request) != 0 }
    }

    #[test]
    fn file_broker_protocol_rejects_authority_and_namespace_expansion() {
        unsafe extern "C" {
            fn sandbox_file_broker_test_quality() -> u32;
        }
        // SAFETY: native helper exercises only private, pointer-free QoS
        // validation cases and returns a scalar verdict.
        assert_eq!(unsafe { sandbox_file_broker_test_quality() }, 0);
        let valid = request(r"C:\output\future.json");
        assert_eq!(validate(&valid), 0);
        // Windows CreateFileW adds SYNCHRONIZE and FILE_READ_ATTRIBUTES to
        // ordinary generic opens; keep the private protocol compatible with
        // the exact shapes that reached the native capture control.
        let standard_open = Request {
            access: 0x8010_0080,
            ..valid.clone()
        };
        assert_eq!(validate(&standard_open), 0);
        let standard_arm_open = Request {
            options: standard_open.options | 0x0002_0000,
            ..standard_open.clone()
        };
        assert_eq!(
            validate(&standard_arm_open),
            0,
            "FILE_DISALLOW_EXCLUSIVE is emitted by the ARM Win32 request"
        );
        let standard_create = Request {
            operation: 2,
            access: 0x4010_0080,
            disposition: 3,
            ..valid.clone()
        };
        assert_eq!(validate(&standard_create), 0);
        assert_eq!(
            validate(&Request {
                options: standard_create.options | 0x0002_0000,
                ..standard_create.clone()
            }),
            0,
            "FILE_DISALLOW_EXCLUSIVE is emitted by the ARM Win32 create"
        );
        assert_eq!(
            validate(&Request {
                options: standard_create.options | 0x4000,
                ..standard_create.clone()
            }),
            0,
            "directory operations carry backup intent, which is not forwarded to the host open"
        );
        assert_ne!(
            validate(&Request {
                options: standard_create.options | 0x0004_0000,
                ..standard_create.clone()
            }),
            0,
            "session-aware opens are outside the private broker contract"
        );
        for access in [
            0x10000000, 0x02000000, 0x00040000, 0x00080000, 0x01000000, 0x40,
        ] {
            assert_ne!(
                validate(&Request {
                    access: valid.access | access,
                    ..valid.clone()
                }),
                0
            );
        }
        for options in [0x1, 0x1000, 0x2000, 0x400000] {
            assert_ne!(
                validate(&Request {
                    options: valid.options | options,
                    ..valid.clone()
                }),
                0
            );
        }
        for path in [
            r"C:\output\..\canary",
            r"C:\output\a:stream",
            r"C:\OUTPU~1\a",
            r"\\server\share\a",
            r"C:\output\a.\b",
            r"C:\output\a ",
            r"C:\output\\a",
        ] {
            assert_ne!(validate(&request(path)), 0, "{path}");
        }
        assert_ne!(
            validate(&request(r"C:\output\listing.dir\")),
            0,
            "the broker protocol receives only canonical non-root leaves"
        );
        let mut directory = request(r"C:\output\listing.dir\");
        directory.options |= 1;
        assert!(normalize_directory_capture(&mut directory));
        assert_eq!(
            validate(&directory),
            0,
            "directory capture normalizes one separator"
        );
        let mut regular_file = request(r"C:\output\future.json\");
        assert!(!normalize_directory_capture(&mut regular_file));
        assert_ne!(
            validate(&regular_file),
            0,
            "a regular-file trailing separator is rejected before broker exchange"
        );
        for path in [r"C:\output\listing.dir\\", r"C:\"] {
            let mut rejected = request(path);
            rejected.options |= 1;
            let _ = normalize_directory_capture(&mut rejected);
            assert_ne!(validate(&rejected), 0, "{path}");
        }
        for disposition in [1, 2, 3, 4, 5] {
            assert_eq!(
                validate(&Request {
                    operation: 2,
                    access: 0x40000000,
                    disposition,
                    ..valid.clone()
                }),
                0
            );
        }
        assert_ne!(
            validate(&Request {
                operation: 2,
                disposition: 0,
                ..valid.clone()
            }),
            0
        );
        assert_ne!(
            validate(&Request {
                version: 1,
                ..valid.clone()
            }),
            0
        );
        assert_ne!(
            validate(&Request {
                size: 2091,
                ..valid.clone()
            }),
            0
        );
        let mut tail = valid.clone();
        tail.path[1023] = 1;
        assert_ne!(validate(&tail), 0);
        for (operation, access, disposition, options) in [
            (2, 0x0010_0001, 2, 0x0020_4021),
            (1, 0x0011_0080, 1, 0x0020_4021),
            (1, 0x0010_0001, 1, 0x0000_4021),
            (1, 0x0011_0080, 1, 0x0020_4020),
            (1, 0x0001_0080, 1, 0x0020_4040),
        ] {
            assert_eq!(
                validate(&Request {
                    operation,
                    access,
                    disposition,
                    options,
                    ..valid.clone()
                }),
                0
            );
        }
        for operation in [6, 7] {
            let mutation = Request {
                operation,
                access: 0,
                share: 0,
                disposition: 0,
                options: 0,
                source_low: 4,
                ..valid.clone()
            };
            assert_eq!(validate(&mutation), 0);
            assert_ne!(
                validate(&Request {
                    source_low: 0,
                    ..mutation.clone()
                }),
                0
            );
            assert_ne!(
                validate(&Request {
                    attributes: 1,
                    ..mutation
                }),
                0
            );
        }
        let remove = Request {
            operation: 5,
            access: 0,
            share: 0,
            disposition: 0,
            options: 0,
            source_low: 4,
            length: 0,
            path: [0; 1024],
            ..valid.clone()
        };
        for attributes in [0, 1, 3, 7] {
            assert_eq!(
                validate(&Request {
                    attributes,
                    ..remove.clone()
                }),
                0
            );
        }
        for attributes in [2, 5, 8, 0x10] {
            assert_ne!(
                validate(&Request {
                    attributes,
                    ..remove.clone()
                }),
                0,
                "unobserved delete-disposition bits remain outside the protocol"
            );
        }
        assert_ne!(
            validate(&Request {
                source_low: 4,
                ..valid
            }),
            0
        );
    }

    #[test]
    fn file_broker_native_child() {
        let Some(root) = std::env::var_os("NUB_FILE_BROKER_TEST_ROOT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let allowed = std::env::var("NUB_FILE_BROKER_TEST_MODE").unwrap() != "raw";
        if std::env::var_os("NUB_FILE_BROKER_TEST_NAMESPACE").is_some() {
            native_namespace(&root, allowed);
            let directory = root.join("win32.dir");
            assert_eq!(std::fs::create_dir(&directory).is_ok(), allowed);
            assert!(std::fs::create_dir(root.join("forbidden-folder.txt")).is_err());
            assert!(
                std::fs::read_dir(root.join("private-folder.txt")).is_err(),
                "the unmatched directory remains denied in {mode} mode"
            );
            let listing = std::fs::read_dir(root.join("listing.dir"));
            assert_eq!(
                listing.is_ok(),
                allowed,
                "ordinary Rust read_dir is raw-denied and broker-allowed"
            );
            if allowed {
                let entries: Vec<_> = listing
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect();
                assert!(entries.contains(&"entry.txt".into()));
                assert!(
                    !root.join("force-image.json").exists(),
                    "the observed delete-disposition shape reaches the broker"
                );
            }
            let linked = root.join("win32-link.json");
            assert_eq!(
                std::fs::hard_link(root.join("win32-source.json"), &linked).is_ok(),
                allowed
            );
            assert_eq!(
                std::fs::remove_file(root.join("win32-remove.json")).is_ok(),
                allowed
            );
            assert_eq!(std::fs::remove_dir(&directory).is_ok(), allowed);
            if allowed {
                assert_eq!(std::fs::read(&linked).unwrap(), b"namespace fixture");
            }
            println!("FILE_BROKER_NATIVE_NAMESPACE_OK");
            return;
        }
        unsafe extern "C" {
            fn sandbox_file_broker_test_four_calls(
                path: *const u16,
                allowed: i32,
                statuses: *mut i32,
            ) -> u32;
        }
        for name in ["existing.json", "near.txt"] {
            let path: Vec<u16> = root
                .join(name)
                .to_str()
                .unwrap()
                .encode_utf16()
                .chain([0])
                .collect();
            // SAFETY: terminated path and scalar expectation; native fixture
            // owns its returned handles and calls the real four NT entrypoints.
            let error = unsafe {
                sandbox_file_broker_test_four_calls(
                    path.as_ptr(),
                    i32::from(allowed && name.ends_with("json")),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(error, 0, "four-call failure bits for {name}: {error:#x}");
        }
        let future = root.join("future.json");
        let write = std::fs::write(&future, b"created through generic write");
        assert_eq!(write.is_ok(), allowed, "future create: {write:?}");
        if allowed {
            assert_eq!(
                std::fs::read(&future).unwrap(),
                b"created through generic write"
            );
            std::fs::write(&future, b"overwrite").unwrap();
            assert_eq!(std::fs::read(&future).unwrap(), b"overwrite");
            let mutated = root.join("mutated.json");
            let path: Vec<u16> = mutated
                .to_str()
                .unwrap()
                .encode_utf16()
                .chain([0])
                .collect();
            unsafe extern "C" {
                fn sandbox_file_broker_test_reparse_after_open(path: *const u16) -> u32;
            }
            // SAFETY: a disposable fixture file is created and tagged through
            // its own returned handle. Failed setup is not a passing control.
            let result = unsafe { sandbox_file_broker_test_reparse_after_open(path.as_ptr()) };
            assert_eq!(result, 0, "post-transfer reparse setup failed: {result}");
            assert!(std::fs::read(&mutated).is_err());
        }
        assert!(std::fs::write(root.join("future.txt"), b"denied").is_err());
        // Authority belongs to each explicitly matching hardlink spelling.
        assert_eq!(std::fs::read(root.join("linked.json")).is_ok(), allowed);
        assert!(std::fs::read(root.join("existing-alias.txt")).is_err());
        assert!(std::fs::remove_file(root.join("near.txt")).is_err());
        assert!(std::fs::rename(root.join("existing.json"), root.join("renamed.txt")).is_err());
        assert!(std::fs::hard_link(root.join("existing.json"), root.join("alias.txt")).is_err());
        if allowed && std::env::var_os("NUB_FILE_BROKER_TEST_LOADER").is_none() {
            use std::os::windows::fs::OpenOptionsExt as _;
            let exclusive = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(0)
                .open(root.join("existing.json"))
                .unwrap();
            drop(exclusive);
            // Closed recipient handles must not consume a cumulative quota.
            for _ in 0..4097 {
                drop(std::fs::File::open(root.join("existing.json")).unwrap());
            }
            println!("FILE_BROKER_REPEATED_OPENS=4097");
        }
        if std::env::var_os("NUB_FILE_BROKER_TEST_LOADER").is_some() {
            let path: Vec<u16> = root
                .join("loader-fixture.dll")
                .to_str()
                .unwrap()
                .encode_utf16()
                .chain([0])
                .collect();
            unsafe extern "C" {
                fn sandbox_file_broker_test_loader(path: *const u16, allowed: i32) -> u32;
            }
            // SAFETY: terminated fixture path. The helper loads and unloads its
            // own module and checks both DllMain and the known fixture export.
            eprintln!("FILE_BROKER_LOADER_BEGIN");
            let result =
                unsafe { sandbox_file_broker_test_loader(path.as_ptr(), i32::from(allowed)) };
            eprintln!("FILE_BROKER_LOADER_END result={result}");
            assert_eq!(result, 0, "loader failed: {result}");
            println!("FILE_BROKER_NATIVE_LOADER_OK");
        }
        println!("FILE_BROKER_NATIVE_CHILD_OK");
    }

    #[test]
    #[ignore = "requires an ordinary-user native Windows acceptance run"]
    fn file_broker_native_open_create_metadata_with_raw_control() {
        native_control(None, false);
    }

    #[test]
    #[ignore = "requires a matching-architecture file_broker_fixture.c DLL"]
    fn file_broker_native_loader_with_raw_control() {
        let dll = std::env::var_os("NUB_FILE_BROKER_TEST_DLL")
            .expect("compile native/file_broker_fixture.c and supply its absolute DLL path");
        native_control(Some(std::path::PathBuf::from(dll)), false);
    }

    #[test]
    #[ignore = "requires an ordinary-user native Windows acceptance run"]
    fn file_broker_native_namespace_with_raw_control() {
        native_control(None, true);
    }

    fn namespace_fixture(root: &std::path::Path) {
        std::fs::create_dir(root.join("listing.dir")).unwrap();
        std::fs::create_dir(root.join("private-folder.txt")).unwrap();
        std::fs::write(root.join("listing.dir").join("entry.txt"), b"entry").unwrap();
        for name in [
            "source.json",
            "remove.json",
            "force-image.json",
            "readonly.txt",
            "win32-source.json",
            "win32-remove.json",
        ] {
            std::fs::write(root.join(name), b"namespace fixture").unwrap();
        }
    }

    fn native_namespace(root: &std::path::Path, allowed: bool) {
        unsafe extern "C" {
            fn sandbox_file_broker_test_namespace(root: *const u16, allowed: i32) -> u32;
        }
        let root: Vec<u16> = root.to_str().unwrap().encode_utf16().chain([0]).collect();
        // SAFETY: terminated fixture path; helper owns and closes its handles.
        let result =
            unsafe { sandbox_file_broker_test_namespace(root.as_ptr(), i32::from(allowed)) };
        assert_eq!(result, 0, "native namespace step failed: {result}");
    }

    fn native_control(dll: Option<std::path::PathBuf>, namespace: bool) {
        use super::super::windows::WindowsStdio;
        use crate::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
        use std::io::Read as _;
        let binary = std::env::current_exe().unwrap();
        let root = tempfile::Builder::new()
            .prefix("sandbox-file-broker-")
            .tempdir_in(std::env::var_os("USERPROFILE").unwrap())
            .unwrap();
        let project = root.path().join("project");
        let files = root.path().join("ungranted");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&files).unwrap();
        std::fs::write(files.join("existing.json"), b"original").unwrap();
        std::fs::write(files.join("near.txt"), b"canary").unwrap();
        std::fs::hard_link(files.join("near.txt"), files.join("linked.json")).unwrap();
        std::fs::hard_link(
            files.join("existing.json"),
            files.join("existing-alias.txt"),
        )
        .unwrap();
        if namespace {
            namespace_fixture(&files);
            let control = root.path().join("unconfined-namespace");
            std::fs::create_dir(&control).unwrap();
            namespace_fixture(&control);
            native_namespace(&control, true);
            assert!(control.join("renamed.json").is_file());
            assert!(control.join("new-link.json").is_file());
            assert!(!control.join("source.json").exists());
            assert!(!control.join("remove.json").exists());
            assert!(!control.join("created.dir").exists());
        }
        unsafe extern "C" {
            fn sandbox_file_broker_test_reparse_after_open(path: *const u16) -> u32;
        }
        let reparse_control = root.path().join("unconfined-reparse-control");
        let path: Vec<u16> = reparse_control
            .to_str()
            .unwrap()
            .encode_utf16()
            .chain([0])
            .collect();
        // SAFETY: this test owns the disposable tagged file and the native
        // helper closes its only handle before this scope continues.
        assert_eq!(
            unsafe { sandbox_file_broker_test_reparse_after_open(path.as_ptr()) },
            0
        );
        if let Some(dll) = &dll {
            std::fs::copy(dll, files.join("loader-fixture.dll")).unwrap();
        }
        // Unconfined positive control uses the same native entrypoints and
        // original fixture files before any sandbox acquires permissions.
        unsafe extern "C" {
            fn sandbox_file_broker_test_four_calls(
                path: *const u16,
                allowed: i32,
                statuses: *mut i32,
            ) -> u32;
            fn sandbox_file_broker_test_loader(path: *const u16, allowed: i32) -> u32;
        }
        for name in ["existing.json", "near.txt"] {
            let path: Vec<u16> = files
                .join(name)
                .to_str()
                .unwrap()
                .encode_utf16()
                .chain([0])
                .collect();
            // SAFETY: native fixture owns every temporary handle.
            let mut statuses = [0; 6];
            let result = unsafe {
                sandbox_file_broker_test_four_calls(path.as_ptr(), 1, statuses.as_mut_ptr())
            };
            println!("FILE_BROKER_UNCONFINED_NTSTATUS={statuses:x?}");
            assert_eq!(
                result, 0,
                "unconfined four-call failure bits: {result:#x}; statuses [generic-open, generic-create, synchronized-open, synchronized-create, basic, full]: {statuses:x?}"
            );
        }
        if dll.is_some() {
            let path: Vec<u16> = files
                .join("loader-fixture.dll")
                .to_str()
                .unwrap()
                .encode_utf16()
                .chain([0])
                .collect();
            // SAFETY: this fixture library is loaded, called and unloaded here.
            assert_eq!(
                unsafe { sandbox_file_broker_test_loader(path.as_ptr(), 1) },
                0
            );
        }
        let ctx = CompileCtx::new(
            Homes {
                home: root.path().join("home"),
                cache: root.path().join("cache"),
                tmp: root.path().join("tmp"),
                project: project.clone(),
            },
            project.clone(),
            ScopeCapabilities::approved(),
            std::env::vars().collect(),
        );
        // This is the authority that the private test-only broker uses. It is
        // compiled through the public JSON grammar rather than hand-built IR.
        // `files` remains a sibling of the project grant, so its globbed
        // entries cannot acquire an inherited AppContainer ACL.
        assert!(!files.starts_with(&project));
        let mut authority_fs: serde_json::Map<String, serde_json::Value> = [
            ("./".to_string(), serde_json::json!("r")),
            ("$tmp".to_string(), serde_json::json!(false)),
            (
                format!("{}/*.json", files.display()),
                serde_json::json!("rw"),
            ),
        ]
        .into_iter()
        .collect();
        if dll.is_some() {
            authority_fs.insert(
                format!("{}/loader-*.dll", files.display()),
                serde_json::json!("r"),
            );
        }
        if namespace {
            authority_fs.insert(
                format!("{}/*.dir", files.display()),
                serde_json::json!("rw"),
            );
            authority_fs.insert(
                format!("{}/readonly.txt", files.display()),
                serde_json::json!("r"),
            );
        }
        let authority_policy =
            compile(&serde_json::json!({"fs": authority_fs, "net": false}), &ctx).unwrap();
        let derived = super::super::windows::derive_grants(&authority_policy.fs);
        assert!(
            derived
                .read
                .iter()
                .chain(&derived.read_nodes)
                .chain(&derived.write)
                .all(|grant| !files.starts_with(grant)),
            "globbed file authority must not materialize an inherited grant covering {files:?}: read={:?}, nodes={:?}, write={:?}",
            derived.read,
            derived.read_nodes,
            derived.write,
        );

        // The ordinary launch remains on the currently-admissible direct ACL
        // policy. The compiled glob authority above reaches only the private
        // test hook; it does not admit a live public file-broker grammar.
        let config = serde_json::json!({"fs": {"./": "r", "$tmp": "rw",
            binary.parent().unwrap().to_str().unwrap(): "r"}, "net": false});
        for mode in ["raw", "broker"] {
            let mut policy = compile(&config, &ctx).unwrap();
            policy.env.constructed.insert(
                "NUB_FILE_BROKER_TEST_ROOT".into(),
                files.to_str().unwrap().into(),
            );
            policy
                .env
                .constructed
                .insert("NUB_JAIL_DUMP_POLICY".into(), "1".into());
            policy
                .env
                .constructed
                .insert("NUB_FILE_BROKER_TEST_MODE".into(), mode.into());
            if dll.is_some() {
                policy
                    .env
                    .constructed
                    .insert("NUB_FILE_BROKER_TEST_LOADER".into(), "1".into());
            }
            if namespace {
                policy
                    .env
                    .constructed
                    .insert("NUB_FILE_BROKER_TEST_NAMESPACE".into(), "1".into());
            }
            let sandbox = if mode == "raw" {
                Sandbox::new(&policy)
            } else {
                Sandbox::with_windows_native_compat(&policy)
            }
            .unwrap();
            let mut prepared = sandbox
                .prepare(
                    CommandSpec::new(&binary)
                        .args(["--exact", CHILD, "--nocapture"])
                        .cwd(&project),
                )
                .unwrap();
            assert!(
                prepared.degradation.lost.is_empty(),
                "{:?}",
                prepared.degradation
            );
            let launch = prepared.launch.take().unwrap();
            let resource = prepared.acquire_windows_resource(launch).unwrap();
            let mut child = with_test_rules(authority_policy.fs.rules.clone(), || {
                resource.spawn_before_resume(
                    WindowsStdio::Null,
                    WindowsStdio::Piped,
                    WindowsStdio::Piped,
                    |_| Ok(()),
                )
            })
            .unwrap();
            let mut stdout = child.take_stdout().unwrap();
            let mut stderr = child.take_stderr().unwrap();
            let stdout = std::thread::spawn(move || {
                let mut s = String::new();
                stdout.read_to_string(&mut s).unwrap();
                s
            });
            let stderr = std::thread::spawn(move || {
                let mut s = String::new();
                stderr.read_to_string(&mut s).unwrap();
                s
            });
            let status = child.wait().unwrap();
            let stdout = stdout.join().unwrap();
            let stderr = stderr.join().unwrap();
            assert!(status.success(), "{mode}: {stdout}\n{stderr}");
            assert!(
                stdout.contains(if namespace {
                    "FILE_BROKER_NATIVE_NAMESPACE_OK"
                } else {
                    "FILE_BROKER_NATIVE_CHILD_OK"
                }),
                "{mode}: {stdout}"
            );
            if dll.is_some() {
                assert!(
                    stdout.contains("FILE_BROKER_NATIVE_LOADER_OK"),
                    "{mode}: {stdout}"
                );
            } else if mode == "broker" && !namespace {
                assert!(
                    stdout.contains("FILE_BROKER_REPEATED_OPENS=4097"),
                    "{stdout}"
                );
            }
            println!("FILE_BROKER_NATIVE_MODE={mode}\n{stdout}");
            drop(child);
            drop(resource);
            drop(prepared);
            sandbox.close();
        }
        assert_eq!(std::fs::read(files.join("near.txt")).unwrap(), b"canary");
        assert!(!files.join("future.txt").is_file());
        if namespace {
            assert!(files.join("renamed.json").is_file());
            assert!(files.join("new-link.json").is_file());
            assert!(!files.join("source.json").exists());
            assert!(!files.join("remove.json").exists());
            assert!(!files.join("created.dir").exists());
            assert!(!files.join("forbidden.txt").exists());
            assert!(!files.join("amplified.json").exists());
            assert_eq!(
                std::fs::read(files.join("readonly.txt")).unwrap(),
                b"namespace fixture"
            );
        }
    }
}

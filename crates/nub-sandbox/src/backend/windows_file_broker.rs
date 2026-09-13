//! Command-owned file authority. Production admission remains disabled until
//! the complete namespace and native-execution contract is implemented.

#[cfg(target_env = "msvc")]
use crate::matcher::path::PathMatcher;

pub(super) struct FileBroker {
    #[cfg(target_env = "msvc")]
    native: std::ptr::NonNull<std::ffi::c_void>,
    #[cfg(target_env = "msvc")]
    pub(super) endpoint: Vec<u16>,
    // Stable allocation borrowed by native workers until Drop has joined them.
    #[cfg(target_env = "msvc")]
    _matcher: Box<PathMatcher>,
}

// SAFETY: the native owner synchronizes cancellation, and its immutable matcher
// allocation is stable across moves. Drop joins workers before freeing it.
unsafe impl Send for FileBroker {}

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
    static TEST_RULES: std::cell::RefCell<Option<crate::policy::FsRuleSet>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, target_env = "msvc"))]
pub(super) fn with_test_rules<T>(rules: crate::policy::FsRuleSet, run: impl FnOnce() -> T) -> T {
    struct Reset(Option<crate::policy::FsRuleSet>);
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
        let (matcher, name) = unsafe {
            (
                &*context.cast::<PathMatcher>(),
                std::slice::from_raw_parts(path, length as usize),
            )
        };
        let Ok(name) = String::from_utf16(name) else {
            return 0;
        };
        let decision = matcher.decide_verified_name(&name);
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
    let matcher = Box::new(PathMatcher::new(&rules));
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
            (&*matcher as *const PathMatcher).cast(),
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
        _matcher: matcher,
    }))
}

#[cfg(all(test, target_env = "msvc"))]
mod tests {
    use super::*;
    use crate::policy::{CanonGlob, Effect, FsAccess, FsOrigin, FsRule, FsRuleSet};

    const CHILD: &str = "backend::windows_file_broker::tests::file_broker_native_child";

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
        let matcher = PathMatcher::new(&FsRuleSet {
            default_effect: Effect::Deny,
            entries: vec![
                rule("C:/output/*.json", FsAccess::ReadWrite),
                rule("C:/output/future.json", FsAccess::Read),
            ],
        });
        let decision = matcher.decide_verified_name(r"C:\output\future.json");
        assert_eq!(
            (decision.effect, decision.access),
            (Effect::Allow, FsAccess::ReadWrite)
        );
        assert_eq!(
            matcher.decide_verified_name(r"C:\output\near.txt").effect,
            Effect::Deny
        );
        assert_eq!(
            matcher
                .decide_verified_name(r"C:\output\nested\future.json")
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
    }
    fn request(path: &str) -> Request {
        let mut request = Request {
            version: 1,
            size: 2084,
            operation: 1,
            access: 0x80000000,
            share: 3,
            disposition: 1,
            options: 0x60,
            attributes: 0,
            length: 0,
            path: [0; 1024],
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
        assert_eq!(std::mem::size_of::<Request>(), 2084);
        // SAFETY: pointer-free fixed-width layout matches the native protocol.
        unsafe { sandbox_file_broker_validate(request) }
    }

    #[test]
    fn file_broker_protocol_rejects_authority_and_namespace_expansion() {
        let valid = request(r"C:\output\future.json");
        assert_eq!(validate(&valid), 0);
        for access in [
            0x10000000, 0x02000000, 0x00010000, 0x00040000, 0x00080000, 0x01000000, 0x40,
        ] {
            assert_ne!(
                validate(&Request {
                    access: valid.access | access,
                    ..valid.clone()
                }),
                0
            );
        }
        for options in [0x1, 0x1000, 0x2000, 0x200000, 0x4000, 0x400000] {
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
                version: 2,
                ..valid.clone()
            }),
            0
        );
        assert_ne!(
            validate(&Request {
                size: 2083,
                ..valid.clone()
            }),
            0
        );
        let mut tail = valid.clone();
        tail.path[1023] = 1;
        assert_ne!(validate(&tail), 0);
    }

    #[test]
    fn file_broker_native_child() {
        let Some(root) = std::env::var_os("NUB_FILE_BROKER_TEST_ROOT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let allowed = std::env::var("NUB_FILE_BROKER_TEST_MODE").unwrap() != "raw";
        unsafe extern "C" {
            fn sandbox_file_broker_test_four_calls(path: *const u16, allowed: i32) -> u32;
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
        assert!(std::fs::read(root.join("linked.json")).is_err());
        assert!(std::fs::remove_file(root.join("existing.json")).is_err());
        assert!(std::fs::rename(root.join("existing.json"), root.join("renamed.txt")).is_err());
        assert!(std::fs::hard_link(root.join("existing.json"), root.join("alias.txt")).is_err());
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
            let result =
                unsafe { sandbox_file_broker_test_loader(path.as_ptr(), i32::from(allowed)) };
            assert_eq!(result, 0, "loader failed: {result}");
            println!("FILE_BROKER_NATIVE_LOADER_OK");
        }
        println!("FILE_BROKER_NATIVE_CHILD_OK");
    }

    #[test]
    #[ignore = "requires an ordinary-user native Windows acceptance run"]
    fn file_broker_native_open_create_metadata_with_raw_control() {
        native_control(None);
    }

    #[test]
    #[ignore = "requires a matching-architecture file_broker_fixture.c DLL"]
    fn file_broker_native_loader_with_raw_control() {
        let dll = std::env::var_os("NUB_FILE_BROKER_TEST_DLL")
            .expect("compile native/file_broker_fixture.c and supply its absolute DLL path");
        native_control(Some(std::path::PathBuf::from(dll)));
    }

    fn native_control(dll: Option<std::path::PathBuf>) {
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
            fn sandbox_file_broker_test_four_calls(path: *const u16, allowed: i32) -> u32;
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
            assert_eq!(
                unsafe { sandbox_file_broker_test_four_calls(path.as_ptr(), 1) },
                0
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
                .insert("NUB_FILE_BROKER_TEST_MODE".into(), mode.into());
            if dll.is_some() {
                policy
                    .env
                    .constructed
                    .insert("NUB_FILE_BROKER_TEST_LOADER".into(), "1".into());
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
            let rules = FsRuleSet {
                default_effect: Effect::Deny,
                entries: vec![
                    rule(&format!("{}/*.json", files.display()), FsAccess::ReadWrite),
                    rule(&format!("{}/loader-*.dll", files.display()), FsAccess::Read),
                ],
            };
            let mut child = with_test_rules(rules, || {
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
                stdout.contains("FILE_BROKER_NATIVE_CHILD_OK"),
                "{mode}: {stdout}"
            );
            drop(child);
            drop(resource);
            drop(prepared);
            sandbox.close();
        }
        assert_eq!(std::fs::read(files.join("near.txt")).unwrap(), b"canary");
        assert!(!files.join("future.txt").is_file());
    }
}

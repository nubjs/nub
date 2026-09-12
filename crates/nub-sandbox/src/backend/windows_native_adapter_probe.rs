//! Native compatibility controls through both the isolated probe and embedded adapter.

use super::*;
use crate::{CompileCtx, Homes, ScopeCapabilities, compile};
use serde_json::json;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

const CHILD: &str = "backend::windows_native_adapter_probe::native_adapter_child";

#[path = "windows_mount_query_probe.rs"]
mod mount_query_probe;
#[path = "windows_null_device_probe.rs"]
mod null_device_probe;

fn private_object_permissions() -> std::io::Result<()> {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::Foundation::GENERIC_ALL;
    use windows_sys::Win32::Security::{
        ACL, ACL_REVISION, AddAccessAllowedAce, DACL_SECURITY_INFORMATION, GetTokenInformation,
        InitializeAcl, InitializeSecurityDescriptor, SECURITY_DESCRIPTOR, SetKernelObjectSecurity,
        SetSecurityDescriptorDacl, SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_DEFAULT_DACL,
        TOKEN_QUERY, TOKEN_USER, TokenDefaultDacl, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_INFORMATION,
        PROCESS_SYNCHRONIZE,
    };
    let check = |ok| {
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    let mut token = std::ptr::null_mut();
    check(unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_ADJUST_DEFAULT,
            &mut token,
        )
    })?;
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut user = [0usize; 64];
    let mut needed = 0;
    check(unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            user.as_mut_ptr().cast(),
            std::mem::size_of_val(&user) as u32,
            &mut needed,
        )
    })?;
    if std::env::var_os("NUB_ADAPTER_PRIVATE_CHILD").is_some() {
        return Ok(()); // A child of the modified default DACL can query its own token.
    }
    let sid = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut bytes = [0u32; 128];
    let acl = bytes.as_mut_ptr().cast::<ACL>();
    let mut descriptor: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let sd = std::ptr::addr_of_mut!(descriptor).cast();
    check(unsafe { InitializeAcl(acl, std::mem::size_of_val(&bytes) as u32, ACL_REVISION) })?;
    check(unsafe { AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_ALL, sid) })?;
    check(unsafe { InitializeSecurityDescriptor(sd, 1) })?;
    check(unsafe { SetSecurityDescriptorDacl(sd, 1, acl, 0) })?;
    let default = TOKEN_DEFAULT_DACL { DefaultDacl: acl };
    // Reproduce MSYS's user-only token default and process DACL, not a Git-specific API.
    check(unsafe {
        SetTokenInformation(
            token.as_raw_handle(),
            TokenDefaultDacl,
            std::ptr::addr_of!(default).cast(),
            std::mem::size_of_val(&default) as u32,
        )
    })?;
    check(unsafe { SetKernelObjectSecurity(GetCurrentProcess(), DACL_SECURITY_INFORMATION, sd) })?;
    drop(token);
    let reopen = |pid| {
        let handle =
            unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_INFORMATION, 0, pid) };
        if handle.is_null() {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
        }
    };
    drop(reopen(std::process::id())?);
    let mut child = Command::new(std::env::current_exe()?)
        .args(["--exact", CHILD, "--nocapture"])
        .env("NUB_ADAPTER_PRIVATE_CHILD", "1")
        .spawn()?;
    // Retain and reap the child even when reopening fails, as in Git's waitpid.
    let reopened = reopen(child.id());
    let status = child.wait()?;
    drop(reopened?);
    if !status.success() {
        return Err(std::io::Error::other("private-object child failed"));
    }
    Ok(())
}

fn anonymous_pipe_bytes() -> std::io::Result<Vec<u8>> {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use windows_sys::Win32::Security::{
        InitializeSecurityDescriptor, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
        SetSecurityDescriptorDacl,
    };
    use windows_sys::Win32::System::Pipes::{CreatePipe, GetNamedPipeInfo};
    let mut descriptor: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let descriptor = std::ptr::addr_of_mut!(descriptor).cast();
    if unsafe { InitializeSecurityDescriptor(descriptor, 1) } == 0
        || unsafe { SetSecurityDescriptorDacl(descriptor, 1, std::ptr::null(), 0) } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let mut reader = std::ptr::null_mut();
    let mut writer = std::ptr::null_mut();
    if unsafe { CreatePipe(&mut reader, &mut writer, &attributes, 16) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut reader = unsafe { std::fs::File::from_raw_handle(reader) };
    let mut writer = unsafe { std::fs::File::from_raw_handle(writer) };
    for pipe in [&reader, &writer] {
        let mut flags = 0;
        if unsafe {
            GetNamedPipeInfo(
                pipe.as_raw_handle(),
                &mut flags,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    writer.write_all(b"pipe")?;
    drop(writer);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[derive(Clone, Copy)]
enum PipeSecurity {
    NullAttributes,
    NullDescriptor,
    UserOnlyDescriptor,
}

impl PipeSecurity {
    fn label(self) -> &'static str {
        match self {
            Self::NullAttributes => "null-attributes",
            Self::NullDescriptor => "null-descriptor",
            Self::UserOnlyDescriptor => "user-only-descriptor",
        }
    }
}

/// Exercise the handle handoff used by Go's `os/exec`: no child opens the
/// pipe by name.  It receives stdin/stdout by CreateProcess handle inheritance.
fn inherited_pipe_roundtrip(security: PipeSecurity) -> std::io::Result<Vec<u8>> {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::Foundation::GENERIC_ALL;
    use windows_sys::Win32::Security::{
        ACL, ACL_REVISION, AddAccessAllowedAce, GetTokenInformation, InitializeAcl,
        InitializeSecurityDescriptor, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
        SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let check = |ok| {
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    let mut descriptor: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let mut acl_bytes = [0u32; 128];
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let attributes = match security {
        // The API itself returns non-inheritable handles here. Rust's Windows
        // process launcher duplicates precisely its standard handles as
        // inheritable before CreateProcess, matching Go's handoff shape.
        PipeSecurity::NullAttributes => std::ptr::null(),
        PipeSecurity::NullDescriptor => std::ptr::addr_of!(attributes),
        PipeSecurity::UserOnlyDescriptor => {
            let mut token = std::ptr::null_mut();
            check(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) })?;
            let token = unsafe { OwnedHandle::from_raw_handle(token) };
            let mut user = [0usize; 64];
            let mut needed = 0;
            check(unsafe {
                GetTokenInformation(
                    token.as_raw_handle(),
                    TokenUser,
                    user.as_mut_ptr().cast(),
                    std::mem::size_of_val(&user) as u32,
                    &mut needed,
                )
            })?;
            let sid = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
            let acl = acl_bytes.as_mut_ptr().cast::<ACL>();
            check(unsafe {
                InitializeAcl(acl, std::mem::size_of_val(&acl_bytes) as u32, ACL_REVISION)
            })?;
            check(unsafe { AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_ALL, sid) })?;
            let descriptor = std::ptr::addr_of_mut!(descriptor).cast();
            check(unsafe { InitializeSecurityDescriptor(descriptor, 1) })?;
            check(unsafe { SetSecurityDescriptorDacl(descriptor, 1, acl, 0) })?;
            attributes.lpSecurityDescriptor = descriptor;
            std::ptr::addr_of!(attributes)
        }
    };
    let pipe = |attributes: *const SECURITY_ATTRIBUTES| -> std::io::Result<(std::fs::File, std::fs::File)> {
        let mut reader = std::ptr::null_mut();
        let mut writer = std::ptr::null_mut();
        check(unsafe { CreatePipe(&mut reader, &mut writer, attributes, 23) })?;
        Ok(unsafe {
            (
                std::fs::File::from_raw_handle(reader),
                std::fs::File::from_raw_handle(writer),
            )
        })
    };
    let (child_stdin, mut parent_stdin) = pipe(attributes)?;
    let (mut parent_stdout, child_stdout) = pipe(attributes)?;
    let mut child = Command::new(std::env::current_exe()?)
        .args(["--exact", CHILD, "--nocapture"])
        .env("NUB_ADAPTER_PIPE_ROUNDTRIP", security.label())
        .stdin(Stdio::from(child_stdin))
        .stdout(Stdio::from(child_stdout))
        .stderr(Stdio::inherit())
        .spawn()?;
    let input = [0, 0xff, b'n', b'u', b'b', 0, b'\n'];
    parent_stdin.write_all(&input)?;
    drop(parent_stdin);
    let mut output = Vec::new();
    parent_stdout.read_to_end(&mut output)?;
    let status = child.wait()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "{} inherited-pipe child failed: {status}",
            security.label()
        )));
    }
    Ok(output)
}

fn inherited_pipe_roundtrips() -> std::io::Result<()> {
    let input = [0, 0xff, b'n', b'u', b'b', 0, b'\n'];
    let mut expected = b"pipe-reply:".to_vec();
    expected.extend(input);
    for security in [
        PipeSecurity::NullAttributes,
        PipeSecurity::NullDescriptor,
        PipeSecurity::UserOnlyDescriptor,
    ] {
        let output = inherited_pipe_roundtrip(security)?;
        // The Rust test harness writes its own progress before and after the
        // child's bytes.  The contiguous reply must nevertheless appear once,
        // preserving every byte of the transport payload including both NULs.
        if output
            .windows(expected.len())
            .filter(|window| *window == expected)
            .count()
            != 1
        {
            return Err(std::io::Error::other(format!(
                "{} inherited-pipe reply mismatch: {output:?}",
                security.label()
            )));
        }
    }
    Ok(())
}

fn embedded_assets_protected() -> Option<bool> {
    use std::os::windows::ffi::OsStringExt as _;
    use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
    for name in ["compat-x64.dll", "compat-arm64.dll"] {
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let module = unsafe { GetModuleHandleW(name.as_ptr()) };
        if module.is_null() {
            continue;
        }
        let mut path = vec![0u16; 32768];
        let length = unsafe { GetModuleFileNameW(module, path.as_mut_ptr(), path.len() as u32) };
        assert!(length > 0 && (length as usize) < path.len());
        let path =
            std::path::PathBuf::from(std::ffi::OsString::from_wide(&path[..length as usize]));
        let root = path.parent().unwrap();
        let registry = root.parent().unwrap().join("registry.json");
        let denied = |result: std::io::Result<std::fs::File>| {
            result.is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
        };
        let protected = std::fs::read(&path).is_ok()
            && denied(
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(root.join("compat-x64.dll")),
            )
            && denied(
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(root.join("tamper")),
            )
            && denied(std::fs::File::open(registry));
        return Some(protected);
    }
    None
}

pub(crate) fn inject_probe(pid: u32) -> std::io::Result<()> {
    let adapter = std::env::var_os("NUB_NATIVE_ADAPTER_PROBE_DIR")
        .ok_or_else(|| std::io::Error::other("native adapter directory missing"))?;
    let output = Command::new(Path::new(&adapter).join("injector.exe"))
        .arg(pid.to_string())
        .arg(&adapter)
        .output()?;
    eprintln!("ADAPTER_INJECT {output:?}");
    if !output.status.success() {
        return Err(std::io::Error::other("native adapter injection failed"));
    }
    Ok(())
}

#[test]
fn native_adapter_child() {
    if std::env::var_os("NUB_ADAPTER_PIPE_ROUNDTRIP").is_some() {
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input).unwrap();
        assert_eq!(input, [0, 0xff, b'n', b'u', b'b', 0, b'\n']);
        std::io::stdout().write_all(b"pipe-reply:").unwrap();
        std::io::stdout().write_all(&input).unwrap();
        return;
    }
    if std::env::var_os("NUB_ADAPTER_PRIVATE_CHILD").is_some() {
        private_object_permissions().unwrap();
        return;
    }
    let Ok(file) = std::env::var("NUB_ADAPTER_PROBE_FILE") else {
        return;
    };
    let canary = std::env::var("NUB_ADAPTER_PROBE_CANARY").unwrap();
    let nul_read = std::fs::File::open("NUL").and_then(|mut f| f.read(&mut [0u8]));
    let nul_write = std::fs::OpenOptions::new()
        .write(true)
        .open("NUL")
        .and_then(|mut f| f.write(b"discarded"));
    let canonical = std::fs::canonicalize(&file);
    let mount_query = mount_query_probe::query_for_file(Path::new(&file), false);
    let mount_create_query = mount_query_probe::query_for_file(Path::new(&file), true);
    let native_null = null_device_probe::probe();
    let directory_read = mount_query_probe::read_directory_node(
        Path::new(&file).parent().unwrap().parent().unwrap(),
    );
    let absolute_nul = Path::new(&file).parent().unwrap().join("NUL");
    let absolute_nul = std::fs::OpenOptions::new()
        .write(true)
        .open(absolute_nul)
        .and_then(|mut f| f.write(b"discarded"));
    let denied = std::fs::read(&canary);
    let assets = embedded_assets_protected();
    let mut nested = None;
    if std::env::var_os("NUB_ADAPTER_PROBE_NESTED").is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD, "--nocapture"])
            .env("NUB_ADAPTER_PROBE_NESTED", "1")
            .output();
        eprintln!("ADAPTER_NESTED {output:?}");
        nested = Some(output.is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("ADAPTER_PRIMITIVES")
        }));
    }
    let private_objects = private_object_permissions();
    eprintln!("ADAPTER_PRIVATE_OBJECTS {private_objects:?}");
    let pipe = anonymous_pipe_bytes();
    let inherited_pipes = inherited_pipe_roundtrips();
    eprintln!("ADAPTER_ANONYMOUS_PIPE {pipe:?}");
    eprintln!("ADAPTER_INHERITED_PIPES {inherited_pipes:?}");
    let denied_after = std::fs::read(&canary);
    let host_denied = {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, GetLastError};
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_VM_READ};
        let pid = std::env::var("NUB_ADAPTER_PROBE_HOST_PID")
            .unwrap()
            .parse()
            .unwrap();
        let handle = unsafe { OpenProcess(PROCESS_VM_READ, 0, pid) };
        if handle.is_null() {
            unsafe { GetLastError() == ERROR_ACCESS_DENIED }
        } else {
            unsafe {
                CloseHandle(handle);
            }
            false
        }
    };
    if std::env::var_os("NUB_ADAPTER_PROBE_REQUIRE").is_some() {
        null_device_probe::assert_mode(&native_null, "embedded");
    }
    println!(
        "ADAPTER_PRIMITIVES {}",
        json!({
            "nul_read": nul_read.as_ref().is_ok_and(|n| *n == 0),
            "nul_write": nul_write.as_ref().is_ok_and(|n| *n == 9),
            "absolute_nul": absolute_nul.as_ref().is_ok_and(|n| *n == 9),
            "canonical": canonical.is_ok(),
            "mount_query": mount_query,
            "mount_create_query": mount_create_query,
            "native_null": native_null,
            "directory_read": directory_read,
            "canary_denied": denied.as_ref().is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied),
            "nested": nested,
            "assets_protected": assets,
            "anonymous_pipe": pipe.as_ref().is_ok_and(|bytes| bytes == b"pipe"),
            "inherited_pipes": inherited_pipes.is_ok(),
            "private_objects": private_objects.is_ok(),
            "host_process_denied": host_denied,
            "canary_after_object_changes": denied_after.as_ref().is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied),
        })
    );
    eprintln!("ADAPTER_ERRORS read={nul_read:?} write={nul_write:?} canonical={canonical:?}");
    if std::env::var_os("NUB_ADAPTER_PROBE_REQUIRE").is_some() {
        assert!(nul_read.is_ok_and(|n| n == 0));
        assert!(nul_write.is_ok_and(|n| n == 9));
        assert!(absolute_nul.is_ok_and(|n| n == 9));
        assert!(canonical.is_ok());
        assert!(denied.is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied));
        assert!(nested.is_none_or(|ok| ok));
        assert!(assets.is_none_or(|ok| ok));
        assert!(pipe.is_ok_and(|bytes| bytes == b"pipe"));
        assert!(inherited_pipes.is_ok());
        assert!(private_objects.is_ok());
        assert!(host_denied);
        assert!(
            denied_after.is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
        );
    }
}

#[test]
#[ignore = "requires the separately built native API probe DLL and injector"]
fn native_adapter_primitives_with_raw_and_plain_controls() {
    native_adapter_primitives(true);
}

#[test]
fn embedded_native_adapter_primitives_with_raw_and_plain_controls() {
    native_adapter_primitives(false);
}

fn native_adapter_primitives(probe: bool) {
    use super::windows::WindowsStdio;
    let adapter = probe.then(|| std::env::var("NUB_NATIVE_ADAPTER_PROBE_DIR").unwrap());
    let binary = std::env::current_exe().unwrap();
    let root = tempfile::Builder::new()
        .prefix("sandbox-native-adapter-")
        .tempdir_in(std::env::var_os("USERPROFILE").unwrap())
        .unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let file = project.join("allowed");
    std::fs::write(&file, "allowed").unwrap();
    let canary = root.path().join("withheld-NUL");
    std::fs::write(&canary, "withheld").unwrap();
    let mut ambient: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    ambient.insert(
        "NUB_ADAPTER_PROBE_FILE".into(),
        file.to_string_lossy().into_owned(),
    );
    ambient.insert(
        "NUB_ADAPTER_PROBE_CANARY".into(),
        canary.to_string_lossy().into_owned(),
    );
    ambient.insert(
        "NUB_ADAPTER_PROBE_HOST_PID".into(),
        std::process::id().to_string(),
    );
    let ctx = CompileCtx::new(
        Homes {
            home: root.path().join("home"),
            cache: root.path().join("cache"),
            tmp: root.path().join("tmp"),
            project: project.clone(),
        },
        project.clone(),
        ScopeCapabilities::approved(),
        ambient,
    );
    let mut raw_native_null = None;
    for mode in ["plain", "raw", if probe { "adapter" } else { "embedded" }] {
        let mut config = json!({
            "fs": {"./": "rw", "$tmp": "rw", binary.parent().unwrap().to_str().unwrap(): "r"},
            "vars": {"NUB_ADAPTER_PROBE_FILE": true, "NUB_ADAPTER_PROBE_CANARY": true, "NUB_ADAPTER_PROBE_HOST_PID": true},
            "net": false,
        });
        if let Some(adapter) = &adapter {
            config["fs"][adapter] = json!("r");
        }
        let mut policy = compile(&config, &ctx).unwrap();
        if matches!(mode, "adapter" | "embedded") {
            policy
                .env
                .constructed
                .insert("NUB_ADAPTER_PROBE_REQUIRE".into(), "1".into());
        }
        let output = if mode == "plain" {
            Command::new(&binary)
                .args(["--exact", CHILD, "--nocapture"])
                .env_clear()
                .envs(&policy.env.constructed)
                .current_dir(&project)
                .output()
                .unwrap()
        } else {
            let sandbox = if mode == "embedded" {
                Sandbox::with_windows_native_compat(&policy)
            } else {
                Sandbox::acquire(&policy)
            }
            .unwrap();
            let mut prepared = sandbox
                .prepare(
                    CommandSpec::new(&binary)
                        .args(["--exact", CHILD, "--nocapture"])
                        .cwd(&project),
                )
                .unwrap();
            assert!(prepared.degradation.lost.is_empty());
            let launch = prepared.launch.take().unwrap();
            let resource = prepared.acquire_windows_resource(launch).unwrap();
            let assets = (mode == "embedded").then(|| {
                let second = Sandbox::with_windows_native_compat(&policy).unwrap();
                let mut pending = second
                    .prepare(CommandSpec::new(&binary).cwd(&project))
                    .unwrap();
                let plan = pending.launch.take().unwrap();
                let shared = pending.acquire_windows_resource(plan).unwrap();
                assert_eq!(resource.profile_name(), shared.profile_name());
                let path =
                    super::windows_native_compat::asset_path(resource.profile_name()).unwrap();
                assert!(path.join("compat-x64.dll").is_file());
                drop(shared);
                drop(pending);
                second.close();
                assert!(path.is_dir());
                path
            });
            let mut child = resource
                .spawn_before_resume(
                    WindowsStdio::Null,
                    WindowsStdio::Piped,
                    WindowsStdio::Piped,
                    |pid| {
                        if mode == "adapter" {
                            inject_probe(pid)?;
                        }
                        Ok(())
                    },
                )
                .unwrap();
            let stdout = child.take_stdout().unwrap();
            let stderr = child.take_stderr().unwrap();
            let read = |mut stream: Box<dyn Read + Send>| {
                let mut bytes = Vec::new();
                stream.read_to_end(&mut bytes).unwrap();
                bytes
            };
            let stdout = std::thread::spawn(move || read(Box::new(stdout)));
            let stderr = std::thread::spawn(move || read(Box::new(stderr)));
            let start = std::time::Instant::now();
            let mut timed_out = false;
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if start.elapsed() > std::time::Duration::from_secs(30) {
                    child.kill().unwrap();
                    timed_out = true;
                    break child.wait().unwrap();
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            };
            drop(child);
            drop(resource);
            drop(prepared);
            sandbox.close();
            if let Some(path) = &assets {
                assert!(path.is_dir(), "idle asset retention");
            }
            cleanup().unwrap();
            if let Some(path) = &assets {
                assert!(!path.exists(), "explicit cleanup removes owned adapters");
            }
            let stdout = stdout.join().unwrap();
            let stderr = stderr.join().unwrap();
            assert!(
                !timed_out,
                "native adapter child timed out: {}",
                String::from_utf8_lossy(&stderr)
            );
            std::process::Output {
                status,
                stdout,
                stderr,
            }
        };
        eprintln!("ADAPTER_CONTROL {mode} {output:?}");
        assert!(output.status.success(), "{mode}: {output:?}");
        let text = String::from_utf8_lossy(&output.stdout);
        let marker = text
            .split("ADAPTER_PRIMITIVES ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap();
        let result: serde_json::Value = serde_json::from_str(marker).unwrap();
        if mode == "raw" {
            raw_native_null = Some(result["native_null"].clone());
        } else if mode != "plain" {
            null_device_probe::assert_unsupported_matches(
                &result["native_null"],
                raw_native_null.as_ref().unwrap(),
            );
        }
        assert_eq!(result["canary_denied"], mode != "plain");
        assert_eq!(result["canary_after_object_changes"], mode != "plain");
        assert_eq!(result["host_process_denied"], mode != "plain");
        if mode == "embedded" {
            assert_eq!(result["assets_protected"], true);
        }
        if matches!(mode, "plain" | "embedded") {
            assert_eq!(result["directory_read"], 0, "{mode}: {result}");
            assert_eq!(result["mount_query"]["matched"], true, "{mode}: {result}");
            assert_eq!(
                result["mount_create_query"]["matched"], true,
                "{mode}: {result}"
            );
            for property in ["mount_query", "mount_create_query"] {
                assert_eq!(
                    result[property]["handles_balanced"], true,
                    "{mode}: {result}"
                );
                for operation in ["duplicated_query", "original_after_duplicate"] {
                    let status = result[property][operation].as_i64().unwrap();
                    if mode == "plain" {
                        assert_eq!(status, 0, "{mode}: {result}");
                    } else {
                        assert!(status < 0, "{mode}: {result}");
                    }
                }
            }
        }
        if mode == "raw" {
            assert_eq!(result["directory_read"], 0xc0000022u32 as i32);
            assert_eq!(result["canonical"], false);
            assert_eq!(result["mount_query"]["open"], 0xc0000022u32 as i32);
            assert_eq!(result["mount_create_query"]["open"], 0xc0000022u32 as i32);
        }
        null_device_probe::assert_mode(&result["native_null"], mode);
        if mode != "raw" {
            for property in [
                "nul_read",
                "nul_write",
                "absolute_nul",
                "canonical",
                "nested",
                "anonymous_pipe",
                "inherited_pipes",
                "private_objects",
            ] {
                assert_eq!(result[property], true, "{mode} {property}: {result}");
            }
        }
    }
}

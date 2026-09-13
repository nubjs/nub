//! Windows AppContainer temporary-storage contract.
//!
//! `$tmp: false` withholds the AppContainer's implicit profile storage without subtracting an
//! explicit filesystem grant that happens to cover it. The child derives its profile storage
//! from its AppContainer SID rather than inherited `TEMP`: CI's launcher environment is not a
//! storage-authority signal.

#![cfg(target_os = "windows")]

#[path = "common/tool_output.rs"]
mod tool_output;

use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::Isolation::GetAppContainerFolderPath;
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_APPCONTAINER_INFORMATION, TOKEN_QUERY, TokenAppContainerSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const CHILD: &str = "appcontainer_tmp_child_records_profile_storage";
const MODE: &str = "NUB_SANDBOX_TMP_POLICY_CHILD";
const PROJECT: &str = "NUB_SANDBOX_TMP_POLICY_PROJECT";
const MARKER: &str = "WINDOWS_TMP_POLICY_ACCESS ";

fn os_environment() -> BTreeMap<String, String> {
    std::env::vars()
        .filter(|(key, _)| {
            ["PATH", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT"]
                .contains(&key.to_ascii_uppercase().as_str())
        })
        .collect()
}

fn fixture() -> tempfile::TempDir {
    standard_user();
    let home = std::env::var_os("USERPROFILE").expect("Windows user profile");
    let root = tempfile::Builder::new()
        .prefix("nub-appcontainer-tmp-")
        .tempdir_in(home)
        .expect("fixture beneath standard-user profile");
    std::fs::create_dir(root.path().join("project")).unwrap();
    std::fs::create_dir(root.path().join("home")).unwrap();
    std::fs::create_dir(root.path().join("cache")).unwrap();
    std::fs::create_dir(root.path().join("tmp")).unwrap();
    root
}

/// The filesystem result must be from an ordinary user token, not an administrator whose
/// writable profile/DACL surface would make the AppContainer comparison meaningless.
fn standard_user() {
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, SC_MANAGER_CREATE_SERVICE,
    };

    let manager = unsafe {
        OpenSCManagerW(
            std::ptr::null(),
            std::ptr::null(),
            SC_MANAGER_CREATE_SERVICE,
        )
    };
    if !manager.is_null() {
        unsafe { CloseServiceHandle(manager) };
        panic!("run this AppContainer filesystem probe as a non-administrative Windows user");
    }
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(5),
        "administrative-access oracle failed for a reason other than access denial"
    );
}

fn policy(root: &Path, tmp: Value, broad_home: Option<&str>) -> nub_sandbox::SandboxPolicy {
    let project = root.join("project");
    let environment = os_environment();
    let context = CompileCtx::new(
        Homes {
            home: root.join("home"),
            cache: root.join("cache"),
            tmp: root.join("tmp"),
            project: project.clone(),
        },
        project.clone(),
        ScopeCapabilities::approved(),
        environment.clone(),
    );
    let mut fs = serde_json::Map::new();
    fs.insert(project.to_string_lossy().into_owned(), json!("rw"));
    fs.insert(
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        json!("r"),
    );
    if let Some(access) = broad_home {
        fs.insert(
            std::env::var("USERPROFILE").expect("Windows user profile"),
            json!(access),
        );
    }
    fs.insert("$tmp".into(), tmp);
    let mut result =
        compile(&json!({"fs": fs, "net": false}), &context).expect("approved tmp grammar compiles");
    result.env.constructed = environment;
    result.env.constructed.insert(MODE.into(), "1".into());
    result
        .env
        .constructed
        .insert(PROJECT.into(), project.to_string_lossy().into_owned());
    result
}

fn command(project: &Path) -> CommandSpec {
    CommandSpec::new(std::env::current_exe().unwrap())
        .args(["--exact", CHILD, "--nocapture"])
        .cwd(project)
        .redact_stdout(true)
        .redact_stderr(true)
}

fn record(output: &std::process::Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once(MARKER))
        .map(|(_, value)| serde_json::from_str(value).expect("child JSON"))
        .next_back()
        .unwrap_or_else(|| panic!("missing tmp-policy child output: {output:?}"))
}

fn assert_denied_profile_storage(access: &Value) {
    assert!(
        access["token"]
            .as_str()
            .unwrap_or_default()
            .contains("is_appcontainer=true"),
        "child was not an AppContainer: {access}"
    );
    for storage in ["package", "appcontainer_folder", "temp"] {
        assert!(!access[storage]["read"].as_bool().unwrap(), "{access}");
        assert_eq!(access[storage]["read_error"], 5, "{access}");
        assert!(!access[storage]["enumerate"].as_bool().unwrap(), "{access}");
        assert_eq!(access[storage]["enumerate_error"], 5, "{access}");
        assert!(!access[storage]["create"].as_bool().unwrap(), "{access}");
        assert_eq!(access[storage]["create_error"], 5, "{access}");
    }
    assert!(access["project"]["create"].as_bool().unwrap(), "{access}");
}

#[test]
fn appcontainer_tmp_false_denies_owned_profile_storage_and_preserves_project_grant() {
    let root = fixture();
    let project = root.path().join("project");
    let denied = policy(root.path(), json!(false), None);
    let session = Sandbox::acquire(&denied).expect("tmp:false session acquires");
    let prepared = session
        .prepare(command(&project))
        .expect("tmp:false launch prepares");
    assert!(prepared.degradation.is_full(), "{:?}", prepared.degradation);
    let access = record(&tool_output::output(prepared));

    assert_denied_profile_storage(&access);
    eprintln!("{MARKER}{access}");

    // A second command keeps the session's live resource and therefore exercises the reuse
    // validation path, not a fresh profile creation.
    let prepared = session
        .prepare(command(&project))
        .expect("reused tmp:false launch prepares");
    let access = record(&tool_output::output(prepared));
    assert_denied_profile_storage(&access);
    eprintln!("{MARKER}{access}");
}

#[test]
fn appcontainer_private_tmp_records_profile_storage_and_project_access() {
    let root = fixture();
    let project = root.path().join("project");
    let private = policy(root.path(), json!("rw"), None);
    let session = Sandbox::acquire(&private).expect("private tmp session acquires");
    let prepared = session
        .prepare(command(&project))
        .expect("private tmp launch prepares");
    assert!(prepared.degradation.is_full(), "{:?}", prepared.degradation);
    let access = record(&tool_output::output(prepared));

    assert!(
        access["token"]
            .as_str()
            .unwrap_or_default()
            .contains("is_appcontainer=true"),
        "child was not an AppContainer: {access}"
    );
    assert!(access["temp"]["create"].as_bool().unwrap(), "{access}");
    assert!(access["project"]["create"].as_bool().unwrap(), "{access}");
    assert!(
        access["temp"]["path"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("\\ac\\temp"),
        "private tmp must be the AppContainer AC\\Temp path: {access}"
    );

    eprintln!("{MARKER}{access}");
}

#[test]
fn appcontainer_tmp_false_reapplies_explicit_home_rw_to_owned_profile_storage() {
    let root = fixture();
    let project = root.path().join("project");
    let denied = policy(root.path(), json!(false), Some("rw"));
    let session = Sandbox::acquire(&denied).expect("broad tmp:false session acquires");
    let prepared = session
        .prepare(command(&project))
        .expect("broad tmp:false launch prepares");
    let access = record(&tool_output::output(prepared));
    for storage in ["package", "appcontainer_folder", "temp"] {
        assert!(access[storage]["read"].as_bool().unwrap(), "{access}");
        assert!(access[storage]["enumerate"].as_bool().unwrap(), "{access}");
        assert!(access[storage]["create"].as_bool().unwrap(), "{access}");
    }
    assert!(access["project"]["create"].as_bool().unwrap(), "{access}");
    eprintln!("{MARKER}{access}");
}

#[test]
fn appcontainer_tmp_false_preserves_explicit_home_read_without_write() {
    let root = fixture();
    let project = root.path().join("project");
    let denied = policy(root.path(), json!(false), Some("r"));
    let session = Sandbox::acquire(&denied).expect("read-only home session acquires");
    for _ in 0..2 {
        let prepared = session
            .prepare(command(&project))
            .expect("read-only home prepares");
        let access = record(&tool_output::output(prepared));
        for storage in ["package", "appcontainer_folder", "temp"] {
            assert!(access[storage]["read"].as_bool().unwrap(), "{access}");
            assert!(access[storage]["enumerate"].as_bool().unwrap(), "{access}");
            assert!(!access[storage]["create"].as_bool().unwrap(), "{access}");
            assert_eq!(access[storage]["create_error"], 5, "{access}");
        }
        assert!(access["project"]["create"].as_bool().unwrap(), "{access}");
        eprintln!("{MARKER}{access}");
    }
}

#[test]
fn appcontainer_tmp_false_withholds_redirected_profile_storage() {
    let root = fixture();
    let project = root.path().join("project");
    let mut denied = policy(root.path(), json!(false), None);
    let redirected = root.path().join("local-appdata");
    std::fs::create_dir(&redirected).unwrap();
    denied.env.constructed.insert(
        "LOCALAPPDATA".into(),
        redirected.to_string_lossy().into_owned(),
    );
    let session = Sandbox::acquire(&denied).expect("redirected storage session acquires");
    for _ in 0..2 {
        let prepared = session
            .prepare(command(&project))
            .expect("redirected storage prepares");
        let access = record(&tool_output::output(prepared));
        assert_denied_profile_storage(&access);
        let profile = Path::new(access["package"]["path"].as_str().unwrap())
            .file_name()
            .expect("profile name");
        let package = redirected.join("Packages").join(profile);
        for (name, path) in [
            ("redirected_package", package.clone()),
            ("redirected_ac", package.join("AC")),
            ("redirected_temp", package.join("AC").join("Temp")),
        ] {
            assert!(
                path.is_dir(),
                "redirected storage must actually exist: {}",
                path.display()
            );
            for operation in ["read", "enumerate", "create"] {
                assert!(!access[name][operation].as_bool().unwrap(), "{access}");
                assert_eq!(access[name][format!("{operation}_error")], 5, "{access}");
            }
        }
        eprintln!("{MARKER}{access}");
    }
}

fn appcontainer_storage() -> PathBuf {
    let mut token = std::ptr::null_mut();
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0
    );
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut bytes = 0u32;
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenAppContainerSid,
            std::ptr::null_mut(),
            0,
            &mut bytes,
        );
    }
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(std::mem::size_of::<usize>())];
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenAppContainerSid,
                buffer.as_mut_ptr().cast(),
                bytes,
                &mut bytes,
            )
        },
        0
    );
    let sid =
        unsafe { (*buffer.as_ptr().cast::<TOKEN_APPCONTAINER_INFORMATION>()).TokenAppContainer };
    assert!(!sid.is_null(), "child token has no AppContainer SID");
    let mut text = std::ptr::null_mut();
    assert_ne!(unsafe { ConvertSidToStringSidW(sid, &mut text) }, 0);
    let mut path = std::ptr::null_mut();
    assert_eq!(unsafe { GetAppContainerFolderPath(text, &mut path) }, 0);
    unsafe { LocalFree(text.cast()) };
    let mut length = 0usize;
    unsafe {
        while *path.add(length) != 0 {
            length += 1;
        }
    }
    let folder = PathBuf::from(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(path, length)
    }));
    unsafe { CoTaskMemFree(path.cast()) };
    folder
}

fn directory_read(path: &Path) -> Result<(), std::io::Error> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    unsafe { CloseHandle(handle) };
    Ok(())
}

fn probe_path(path: PathBuf, nonce: &str) -> Value {
    let read = directory_read(&path);
    let read_error = read.as_ref().err().and_then(std::io::Error::raw_os_error);
    let enumerated = std::fs::read_dir(&path);
    let enumerate_error = enumerated
        .as_ref()
        .err()
        .and_then(std::io::Error::raw_os_error);
    let file = path.join(format!("nub-tmp-policy-{nonce}"));
    let created = OpenOptions::new().write(true).create_new(true).open(&file);
    let create_error = created
        .as_ref()
        .err()
        .and_then(std::io::Error::raw_os_error);
    json!({
        "path": path,
        "read": read.is_ok(),
        "read_error": read_error,
        "enumerate": enumerated.is_ok(),
        "enumerate_error": enumerate_error,
        "file": file,
        "create": created.is_ok(),
        "create_error": create_error,
    })
}

#[test]
fn appcontainer_tmp_child_records_profile_storage() {
    if std::env::var_os(MODE).is_none() {
        return;
    }
    let ac_folder = appcontainer_storage();
    let package = ac_folder.parent().expect("package parent of AC folder");
    let temp = ac_folder.join("Temp");
    let project = PathBuf::from(std::env::var_os(PROJECT).expect("project path"));
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after Unix epoch")
            .as_nanos()
    );
    let mut access = json!({
        "token": nub_sandbox::windows_token_report(),
        "package": probe_path(package.to_path_buf(), &nonce),
        "appcontainer_folder": probe_path(ac_folder.clone(), &nonce),
        "temp": probe_path(temp.clone(), &nonce),
        "package_local_state": probe_path(package.join("LocalState"), &nonce),
        "appcontainer_local_state": probe_path(ac_folder.join("LocalState"), &nonce),
        "project": probe_path(project, &nonce),
    });
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let redirected = PathBuf::from(local)
            .join("Packages")
            .join(package.file_name().unwrap());
        access["redirected_package"] = probe_path(redirected.clone(), &nonce);
        access["redirected_ac"] = probe_path(redirected.join("AC"), &nonce);
        access["redirected_temp"] = probe_path(redirected.join("AC").join("Temp"), &nonce);
    }
    println!("{MARKER}{access}");
}

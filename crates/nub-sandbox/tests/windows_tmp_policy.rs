//! Windows AppContainer temporary-storage contract.
//!
//! `$tmp: false` is an approved grammar spelling.  Windows currently reports it as
//! `tmp-deny` but still launches the AppContainer, which leaves the requested deny unverified.
//! The deny arm therefore requires a fail-closed rejection until a real implementation exists.
//! The positive control runs the same compiled policy with `$tmp: "rw"` and records the actual
//! standard-user access matrix for the package parent, AppContainer folder, `AC\\Temp`, both
//! candidate `LocalState` paths, and the explicitly granted project directory.

#![cfg(target_os = "windows")]

#[path = "common/tool_output.rs"]
mod tool_output;

use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, apply, compile};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

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

fn policy(root: &Path, tmp: Value) -> nub_sandbox::SandboxPolicy {
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
    let mut result = compile(
        &json!({"fs": {
            (project.to_string_lossy()): "rw",
            (std::env::current_exe().unwrap().to_string_lossy()): "r",
            "$tmp": tmp,
        }, "net": false}),
        &context,
    )
    .expect("approved tmp grammar compiles");
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

#[test]
fn appcontainer_tmp_false_fails_closed_before_profile_storage_is_acquired() {
    let root = fixture();
    let project = root.path().join("project");
    let denied = policy(root.path(), json!(false));

    let error = match apply(&denied, command(&project)) {
        Ok(_) => {
            panic!("Windows cannot enforce `$tmp: false` after AppContainer profile storage exists")
        }
        Err(error) => error,
    };
    assert_eq!(error.lost, ["tmp-deny"], "{error:?}");
    assert!(
        error
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("temporary storage"),
        "{error:?}"
    );
}

#[test]
fn appcontainer_private_tmp_records_profile_storage_and_project_access() {
    let root = fixture();
    let project = root.path().join("project");
    let private = policy(root.path(), json!("rw"));
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

    // Keep the package/AC/LocalState values in CI output as direct OS observations.  No current
    // backend implementation denies these profile locations for `$tmp: false`; the deny arm
    // instead preserves the honest fail-closed contract until such implementation exists.
    eprintln!("{MARKER}{access}");
}

fn probe_path(path: PathBuf, nonce: &str) -> Value {
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
    let temp = std::env::temp_dir();
    // `GetAppContainerFolderPath` is the AC folder; the backend appends `Temp` to it.
    let ac_folder = temp.parent().expect("AC folder parent of Temp");
    let package = ac_folder.parent().expect("package parent of AC folder");
    let project = PathBuf::from(std::env::var_os(PROJECT).expect("project path"));
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after Unix epoch")
            .as_nanos()
    );
    let access = json!({
        "token": nub_sandbox::windows_token_report(),
        "package": probe_path(package.to_path_buf(), &nonce),
        "appcontainer_folder": probe_path(ac_folder.to_path_buf(), &nonce),
        "temp": probe_path(temp.clone(), &nonce),
        "package_local_state": probe_path(package.join("LocalState"), &nonce),
        "appcontainer_local_state": probe_path(ac_folder.join("LocalState"), &nonce),
        "project": probe_path(project, &nonce),
    });
    println!("{MARKER}{access}");
}

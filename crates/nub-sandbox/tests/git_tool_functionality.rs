//! Native Git operations under unconfined, exact-grant, and `$tooldirs` controls.
//!
//! These tests intentionally use a synthetic HOME outside the OS temporary directory. They are
//! opt-in native tests: the dedicated CI job runs them after selecting the runner's Git binary.

#[path = "common/tool_output.rs"]
mod tool_output;
#[path = "common/tool_sandbox.rs"]
mod tool_sandbox;

use nub_sandbox::{CommandSpec, CompileCtx, Homes, ScopeCapabilities, compile};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Control {
    Unconfined,
    Exact,
    ToolDirs,
}

impl Control {
    fn name(self) -> &'static str {
        match self {
            Self::Unconfined => "unconfined",
            Self::Exact => "exact grants",
            Self::ToolDirs => "$tooldirs",
        }
    }
}

fn fixture() -> tempfile::TempDir {
    let runner_home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .expect("runner home");
    let root = tempfile::Builder::new()
        .prefix("sandbox-git-tool-")
        .tempdir_in(runner_home)
        .expect("fixture root");
    for path in ["home", "project", "cache"] {
        std::fs::create_dir_all(root.path().join(path)).expect("fixture directory");
    }
    root
}

fn environment(root: &Path, extra: &[(&str, String)]) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for key in [
        "PATH",
        "SystemRoot",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
    ] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.to_owned(), value);
        }
    }
    env.insert("HOME".into(), root.join("home").to_string_lossy().into());
    env.insert(
        "USERPROFILE".into(),
        root.join("home").to_string_lossy().into(),
    );
    env.insert(
        "XDG_CONFIG_HOME".into(),
        root.join("home/.config").to_string_lossy().into(),
    );
    for (key, value) in extra {
        env.insert((*key).to_owned(), value.clone());
    }
    env.insert("GIT_TRACE".into(), "1".into());
    env
}

fn policy(
    root: &Path,
    control: Control,
    extra_env: &[(&str, String)],
    extra_exact: &[(&Path, &str)],
    standard_global_access: Option<&str>,
    needs_global_lock: bool,
) -> nub_sandbox::SandboxPolicy {
    let home = root.join("home");
    let remote = root.join("remote.git");
    let mut grants = match control {
        Control::Unconfined => unreachable!("an unconfined control has no policy"),
        Control::Exact => Value::Object(Map::new()),
        Control::ToolDirs => {
            let mut paths = Map::new();
            paths.insert("$tooldirs".into(), Value::String("rw".into()));
            Value::Object(paths)
        }
    };
    let Value::Object(ref mut entries) = grants else {
        unreachable!("all Git grants are object-form");
    };
    if std::env::var_os("NUB_NATIVE_ADAPTER_PROBE_ENABLE").is_some() {
        let adapter = std::env::var("NUB_NATIVE_ADAPTER_PROBE_DIR").unwrap();
        entries.insert(adapter, Value::String("r".into()));
    }
    // Config-only cases have no local remote. Do not turn that absent fixture path into a
    // backend mount-source failure; a real bare remote remains an explicit narrow grant.
    if remote.exists() {
        entries.insert(
            remote.to_string_lossy().into_owned(),
            Value::String("rw".into()),
        );
    }
    if let Some(access) = standard_global_access {
        entries.insert(
            home.join(".gitconfig").to_string_lossy().into_owned(),
            Value::String(access.into()),
        );
    }
    if needs_global_lock {
        // Git replaces the conventional global config through this adjacent lock. This is an
        // explicit capability test, so its absent source cannot block unrelated Git operations.
        entries.insert(
            home.join(".gitconfig.lock").to_string_lossy().into_owned(),
            Value::String("rw".into()),
        );
    }
    for (path, access) in extra_exact {
        entries.insert(
            path.to_string_lossy().into_owned(),
            Value::String((*access).into()),
        );
    }
    entries.insert("./".into(), Value::String("rw".into()));
    entries.insert("$tmp".into(), Value::String("rw".into()));

    let env = environment(root, extra_env);
    let homes = Homes {
        home,
        cache: root.join("cache"),
        tmp: root.join("tmp"),
        project: root.join("project"),
    };
    let ctx = CompileCtx::new(
        homes,
        root.join("project"),
        ScopeCapabilities::approved(),
        env.clone(),
    );
    let mut policy =
        compile(&json!({"fs": grants, "net": false}), &ctx).expect("Git fixture policy compiles");
    policy.env.constructed = env;
    policy
}

fn output_message(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(phase: &str, output: Output) -> Output {
    assert!(
        output.status.success(),
        "{phase} failed:\n{}",
        output_message(&output)
    );
    output
}

fn host(root: &Path, cwd: &Path, args: &[&str]) -> Output {
    eprintln!("SETUP git {args:?}");
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(environment(root, &[]));
    command.output().expect("Git setup command launches")
}

fn invoke(
    root: &Path,
    cwd: &Path,
    args: &[&str],
    control: Control,
    policy: Option<&nub_sandbox::SandboxPolicy>,
    extra_env: &[(&str, String)],
) -> Output {
    match control {
        Control::Unconfined => {
            eprintln!("UNCONFINED git {args:?}");
            let mut command = Command::new("git");
            command
                .args(args)
                .current_dir(cwd)
                .env_clear()
                .envs(environment(root, extra_env));
            command.output().expect("unconfined Git command launches")
        }
        Control::Exact | Control::ToolDirs => {
            eprintln!("CONFINED {} git {args:?}", control.name());
            let sandbox = tool_sandbox::acquire(policy.expect("confined Git policy"))
                .expect("Git sandbox acquires");
            let prepared = sandbox
                .prepare(
                    CommandSpec::new("git")
                        .args(args)
                        .cwd(cwd)
                        .redact_stdout(true)
                        .redact_stderr(true),
                )
                .expect("Git command prepares without degradation");
            assert!(
                prepared.degradation.lost.is_empty(),
                "{} Git fixture degraded: {:?}",
                control.name(),
                prepared.degradation
            );
            tool_output::output(prepared)
        }
    }
}

fn require_git() {
    let output = Command::new("git")
        .arg("--version")
        .output()
        .expect("standard GitHub runner supplies Git");
    let output = assert_success("git --version", output);
    eprintln!(
        "native Git: {}",
        String::from_utf8_lossy(&output.stdout).trim()
    );
}

fn git_runtime_paths() -> Vec<PathBuf> {
    let executable = if cfg!(windows) { "git.exe" } else { "git" };
    let program = std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|path| path.join(executable))
                .find(|candidate| candidate.is_file())
        })
        .expect("Git executable must be discoverable on PATH for an explicit runtime grant");
    let output = Command::new("git")
        .arg("--exec-path")
        .output()
        .expect("Git reports its helper directory");
    let output = assert_success("git --exec-path", output);
    let exec_path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    assert!(
        exec_path.is_dir(),
        "Git helper directory is missing: {}",
        exec_path.display()
    );

    let mut paths = vec![
        program.clone(),
        program.parent().unwrap().to_path_buf(),
        exec_path.clone(),
    ];
    // Git for Windows dispatches from `cmd/git.exe` into `mingw64/bin/git.exe`; Unix layouts
    // resolve this to `/usr/bin`, already a narrow executable directory. Include only when it
    // exists, never a broad system root.
    if let Some(bin) = exec_path
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join("bin"))
        .filter(|path| path.is_dir())
    {
        paths.push(bin);
    }
    paths.sort();
    paths.dedup();
    paths
}

fn prepare_remote(root: &Path) {
    let remote = root.join("remote.git");
    let publisher = root.join("publisher");
    assert_success(
        "create bare remote",
        host(root, root, &["init", "--bare", remote.to_str().unwrap()]),
    );
    assert_success(
        "create publisher",
        host(root, root, &["init", publisher.to_str().unwrap()]),
    );
    std::fs::write(publisher.join("README.md"), "initial\n").expect("initial source");
    assert_success(
        "stage initial source",
        host(root, &publisher, &["add", "README.md"]),
    );
    assert_success(
        "commit initial source",
        host(
            root,
            &publisher,
            &[
                "-c",
                "user.name=Sandbox Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "initial",
            ],
        ),
    );
    assert_success(
        "add fixture remote",
        host(
            root,
            &publisher,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        ),
    );
    assert_success(
        "push initial source",
        host(root, &publisher, &["push", "origin", "HEAD:main"]),
    );
    assert_success(
        "set fixture remote default branch",
        host(
            root,
            root,
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "symbolic-ref",
                "HEAD",
                "refs/heads/main",
            ],
        ),
    );
}

fn advance_remote(root: &Path) {
    let remote = root.join("remote.git");
    let upstream = root.join("upstream");
    assert_success(
        "clone upstream fixture",
        host(
            root,
            root,
            &[
                "clone",
                remote.to_str().unwrap(),
                upstream.to_str().unwrap(),
            ],
        ),
    );
    std::fs::write(upstream.join("upstream.txt"), "upstream\n").expect("upstream source");
    assert_success(
        "stage upstream source",
        host(root, &upstream, &["add", "upstream.txt"]),
    );
    assert_success(
        "commit upstream source",
        host(
            root,
            &upstream,
            &[
                "-c",
                "user.name=Sandbox Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "upstream",
            ],
        ),
    );
    assert_success(
        "push upstream source",
        host(root, &upstream, &["push", "origin", "HEAD:main"]),
    );
}

fn conventional_global_config(root: &Path) -> PathBuf {
    let config = root.join("home/.gitconfig");
    std::fs::write(
        &config,
        "[user]\n\tname = Sandbox Fixture\n\temail = fixture@example.invalid\n",
    )
    .expect("conventional global config fixture");
    config
}

fn run_conventional_config_write(control: Control) {
    require_git();
    let runtime = git_runtime_paths();
    let runtime: Vec<_> = runtime.iter().map(|path| (path.as_path(), "r")).collect();
    let root = fixture();
    let config = conventional_global_config(root.path());
    let policy = (control != Control::Unconfined)
        .then(|| policy(root.path(), control, &[], &runtime, Some("rw"), true));
    assert_success(
        "write conventional global config through its adjacent lock",
        invoke(
            root.path(),
            &root.path().join("project"),
            &[
                "config",
                "--global",
                "user.email",
                "updated@example.invalid",
            ],
            control,
            policy.as_ref(),
            &[],
        ),
    );
    assert!(
        std::fs::read_to_string(&config)
            .unwrap()
            .contains("updated@example.invalid")
    );
    assert!(
        !config.with_extension("gitconfig.lock").exists(),
        "Git must clean up its adjacent lock"
    );
}

fn run_operations(control: Control) {
    run_operations_with_runtime(control, git_runtime_paths());
}

fn run_operations_with_runtime(control: Control, runtime: Vec<PathBuf>) {
    require_git();
    let runtime: Vec<_> = runtime.iter().map(|path| (path.as_path(), "r")).collect();
    let root = fixture();
    prepare_remote(root.path());
    conventional_global_config(root.path());
    let policy = (control != Control::Unconfined)
        .then(|| policy(root.path(), control, &[], &runtime, Some("r"), false));
    let project = root.path().join("project");
    let remote = root.path().join("remote.git");
    let clone = project.join("clone");

    let configured = assert_success(
        "read conventional global config",
        invoke(
            root.path(),
            &project,
            &["config", "--global", "--get", "user.email"],
            control,
            policy.as_ref(),
            &[],
        ),
    );
    assert_eq!(
        String::from_utf8_lossy(&configured.stdout).trim(),
        "fixture@example.invalid"
    );
    assert!(root.path().join("home/.gitconfig").exists());

    assert_success(
        "local clone",
        invoke(
            root.path(),
            &project,
            &["clone", remote.to_str().unwrap(), clone.to_str().unwrap()],
            control,
            policy.as_ref(),
            &[],
        ),
    );
    std::fs::write(clone.join("change.txt"), "change\n").expect("untracked fixture source");
    let status = assert_success(
        "status untracked source",
        invoke(
            root.path(),
            &clone,
            &["status", "--porcelain"],
            control,
            policy.as_ref(),
            &[],
        ),
    );
    assert!(String::from_utf8_lossy(&status.stdout).contains("?? change.txt"));
    assert_success(
        "stage source",
        invoke(
            root.path(),
            &clone,
            &["add", "change.txt"],
            control,
            policy.as_ref(),
            &[],
        ),
    );
    assert_success(
        "commit source",
        invoke(
            root.path(),
            &clone,
            &["commit", "-m", "sandbox change"],
            control,
            policy.as_ref(),
            &[],
        ),
    );
    assert_success(
        "push source",
        invoke(
            root.path(),
            &clone,
            &["push", "origin", "HEAD:main"],
            control,
            policy.as_ref(),
            &[],
        ),
    );

    advance_remote(root.path());
    assert_success(
        "fetch remote advance",
        invoke(
            root.path(),
            &clone,
            &["fetch", "origin"],
            control,
            policy.as_ref(),
            &[],
        ),
    );

    let linked = project.join("linked");
    assert_success(
        "create linked worktree",
        invoke(
            root.path(),
            &clone,
            &[
                "worktree",
                "add",
                "-b",
                "sandbox-linked",
                linked.to_str().unwrap(),
            ],
            control,
            policy.as_ref(),
            &[],
        ),
    );
    assert!(
        linked.join(".git").is_file(),
        "linked worktree must use a .git indirection file"
    );
    assert!(
        std::fs::read_dir(clone.join(".git/worktrees"))
            .expect("linked-worktree common directory")
            .next()
            .is_some(),
        "linked worktree must create common-dir metadata"
    );
    assert_success(
        "linked worktree status",
        invoke(
            root.path(),
            &linked,
            &["status", "--porcelain"],
            control,
            policy.as_ref(),
            &[],
        ),
    );
}

#[test]
#[ignore = "requires native Git tool functionality job"]
fn git_native_unconfined_conventional_global_config_write() {
    run_conventional_config_write(Control::Unconfined);
}

#[test]
#[ignore = "requires native Git tool functionality job"]
fn git_native_exact_conventional_global_config_write() {
    run_conventional_config_write(Control::Exact);
}

#[test]
#[ignore = "requires native Git tool functionality job"]
fn git_native_tooldirs_conventional_global_config_write() {
    run_conventional_config_write(Control::ToolDirs);
}

#[test]
#[ignore = "requires native Git tool functionality job"]
fn git_native_unconfined_status_add_commit_clone_fetch_push_and_worktree() {
    run_operations(Control::Unconfined);
}

#[test]
#[ignore = "requires native Git tool functionality job"]
fn git_native_exact_status_add_commit_clone_fetch_push_and_worktree() {
    run_operations(Control::Exact);
}

#[test]
#[ignore = "requires native Git tool functionality job"]
fn git_native_tooldirs_status_add_commit_clone_fetch_push_and_worktree() {
    run_operations(Control::ToolDirs);
}

#[test]
#[ignore = "requires native Git tool functionality job"]
fn git_documented_global_config_relocation_needs_an_explicit_grant() {
    require_git();
    let runtime = git_runtime_paths();
    let mut runtime: Vec<_> = runtime.iter().map(|path| (path.as_path(), "r")).collect();
    let root = fixture();
    let project = root.path().join("project");
    let config = root.path().join("explicit/global.gitconfig");
    std::fs::create_dir_all(config.parent().unwrap()).expect("explicit config parent");
    std::fs::write(&config, "[user]\n\tname = Before\n").expect("explicit config fixture");
    let env = [("GIT_CONFIG_GLOBAL", config.to_string_lossy().into_owned())];
    let args = [
        "config",
        "--global",
        "user.email",
        "fixture@example.invalid",
    ];

    assert_success(
        "unconfined relocated global config",
        invoke(
            root.path(),
            &project,
            &args,
            Control::Unconfined,
            None,
            &env,
        ),
    );
    runtime.push((config.parent().unwrap(), "rw"));
    let exact = policy(root.path(), Control::Exact, &env, &runtime, None, false);
    assert_success(
        "explicitly granted relocated global config",
        invoke(
            root.path(),
            &project,
            &args,
            Control::Exact,
            Some(&exact),
            &env,
        ),
    );
    assert!(
        std::fs::read_to_string(&config)
            .unwrap()
            .contains("fixture@example.invalid")
    );
    assert!(!config.with_extension("gitconfig.lock").exists());
}

fn git_lfs_program() -> PathBuf {
    let executable = if cfg!(windows) {
        "git-lfs.exe"
    } else {
        "git-lfs"
    };
    let available = Command::new("git")
        .args(["lfs", "version"])
        .output()
        .expect("Git LFS must be provisioned for the native Git fixture");
    let available = assert_success(
        "git lfs version (required native fixture provision)",
        available,
    );
    eprintln!(
        "native Git LFS: {}",
        String::from_utf8_lossy(&available.stdout).trim()
    );

    let path_candidate = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|path| path.join(executable))
            .find(|candidate| candidate.is_file())
    });
    let exec_path_candidate = Command::new("git")
        .arg("--exec-path")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
        .map(|path| path.join(executable))
        .filter(|candidate| candidate.is_file());
    path_candidate
        .or(exec_path_candidate)
        .expect("Git LFS is available but its executable is not discoverable for an exact grant")
}

fn run_lfs(control: Control) {
    run_lfs_with_runtime(control, git_runtime_paths());
}

#[cfg(windows)]
fn lfs_hook_startup_ladder(root: &Path, clone: &Path, policy: &nub_sandbox::SandboxPolicy) {
    use std::io::Read;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
    };

    struct RestoreHook {
        path: PathBuf,
        original: Vec<u8>,
    }
    impl Drop for RestoreHook {
        fn drop(&mut self) {
            if let Err(error) = std::fs::write(&self.path, &self.original) {
                eprintln!("LFS_HOOK_RESTORE_FAILED {}: {error}", self.path.display());
            }
        }
    }
    let hook = clone.join(".git/hooks/pre-push");
    let restore = RestoreHook {
        original: std::fs::read(&hook).expect("original LFS hook"),
        path: hook,
    };
    let marker = clone.join(".git/sandbox-lfs-diagnostic-marker");
    let trace = clone.join(".git/sandbox-lfs-diagnostic-trace.json");
    let nested = clone.join(".git/sandbox-lfs-diagnostic-child");
    std::fs::write(
        &nested,
        b"#!/bin/sh\nprintf 'nested-entered\\n' >> .git/sandbox-lfs-diagnostic-marker\nexit 37\n",
    )
    .expect("nested diagnostic script");
    let stages = [
        ("builtin", ""),
        (
            "lookup",
            "command -v git-lfs\nprintf 'lookup=%s\\n' \"$?\" >> .git/sandbox-lfs-diagnostic-marker\n",
        ),
        (
            "null-redirect",
            ": >/dev/null\nprintf 'null=%s\\n' \"$?\" >> .git/sandbox-lfs-diagnostic-marker\n",
        ),
        (
            "lfs-version",
            "git-lfs version\nprintf 'lfs-version=%s\\n' \"$?\" >> .git/sandbox-lfs-diagnostic-marker\n",
        ),
        (
            "nested-shell",
            "sh .git/sandbox-lfs-diagnostic-child\nprintf 'nested-status=%s\\n' \"$?\" >> .git/sandbox-lfs-diagnostic-marker\n",
        ),
        (
            "fork-wait",
            "(printf 'fork-entered\\n' >> .git/sandbox-lfs-diagnostic-marker) &\nchild=$!\nwait \"$child\"\nprintf 'fork-status=%s\\n' \"$?\" >> .git/sandbox-lfs-diagnostic-marker\n",
        ),
        ("exec-shell", "exec sh .git/sandbox-lfs-diagnostic-child\n"),
        (
            "lfs-pre-push",
            "git lfs pre-push \"$@\"\nprintf 'lfs-pre-push=%s\\n' \"$?\" >> .git/sandbox-lfs-diagnostic-marker\n",
        ),
    ];
    for (stage, body) in stages {
        // The sentinel makes hook execution observable; --dry-run protects refs even
        // when a broken shell exits successfully without evaluating the script.
        std::fs::write(&restore.path, format!("#!/bin/sh\nprintf 'entered\\n' > .git/sandbox-lfs-diagnostic-marker\n{body}exit 37\n"))
            .expect("diagnostic hook");
        for mode in ["plain", "embedded"] {
            let _ = std::fs::remove_file(&marker);
            let _ = std::fs::remove_file(&trace);
            let extra = [("GIT_TRACE2_EVENT", trace.to_string_lossy().into_owned())];
            let run = || -> Result<Output, String> {
                if mode == "embedded" {
                    let mut policy = policy.clone();
                    policy.env.constructed.extend(
                        extra
                            .iter()
                            .map(|(key, value)| ((*key).into(), value.clone())),
                    );
                    let sandbox = nub_sandbox::Sandbox::with_windows_native_compat(&policy)
                        .map_err(|error| format!("acquire: {error:?}"))?;
                    let prepared = sandbox
                        .prepare(
                            CommandSpec::new("git")
                                .args(["push", "--dry-run", "origin", "HEAD:main"])
                                .cwd(clone)
                                .redact_stdout(true)
                                .redact_stderr(true),
                        )
                        .map_err(|error| format!("prepare: {error:?}"))?;
                    return std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        tool_output::output(prepared)
                    }))
                    .map_err(|_| {
                        "embedded launch/output failed or exceeded its 30-second deadline".into()
                    });
                }

                // Suspend before assignment so even an immediate shell descendant belongs
                // to this control's kill-on-close job. This does not change its token.
                let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
                if handle.is_null() {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                let job = unsafe { OwnedHandle::from_raw_handle(handle) };
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
                    unsafe { std::mem::zeroed() };
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if unsafe {
                    SetInformationJobObject(
                        job.as_raw_handle(),
                        JobObjectExtendedLimitInformation,
                        std::ptr::addr_of!(limits).cast(),
                        std::mem::size_of_val(&limits) as u32,
                    )
                } == 0
                {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                let mut child = Command::new("git")
                    .args(["push", "--dry-run", "origin", "HEAD:main"])
                    .current_dir(clone)
                    .env_clear()
                    .envs(environment(root, &extra))
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .creation_flags(CREATE_SUSPENDED)
                    .spawn()
                    .map_err(|error| error.to_string())?;
                let activate = || -> Result<(), String> {
                    if unsafe {
                        AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle())
                    } == 0
                    {
                        return Err(format!(
                            "job assignment: {}",
                            std::io::Error::last_os_error()
                        ));
                    }
                    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
                    if snapshot == INVALID_HANDLE_VALUE {
                        return Err(std::io::Error::last_os_error().to_string());
                    }
                    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
                    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
                    entry.dwSize = std::mem::size_of_val(&entry) as u32;
                    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
                    while found != 0 {
                        if entry.th32OwnerProcessID == child.id() {
                            let thread =
                                unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                            if thread.is_null() {
                                return Err(std::io::Error::last_os_error().to_string());
                            }
                            let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
                            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                                return Err(std::io::Error::last_os_error().to_string());
                            }
                            return Ok(());
                        }
                        found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
                    }
                    Err("suspended Git primary thread missing".into())
                };
                if let Err(error) = activate() {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("plain control activation: {error}"));
                }
                let stdout = child.stdout.take().unwrap();
                let stderr = child.stderr.take().unwrap();
                std::thread::scope(|scope| {
                    let stdout = scope.spawn(move || {
                        let mut bytes = Vec::new();
                        std::io::BufReader::new(stdout)
                            .read_to_end(&mut bytes)
                            .unwrap();
                        bytes
                    });
                    let stderr = scope.spawn(move || {
                        let mut bytes = Vec::new();
                        std::io::BufReader::new(stderr)
                            .read_to_end(&mut bytes)
                            .unwrap();
                        bytes
                    });
                    let start = Instant::now();
                    let status = loop {
                        match child.try_wait() {
                            Ok(Some(status)) => break Ok(status),
                            Ok(None) if start.elapsed() < Duration::from_secs(30) => {
                                std::thread::sleep(Duration::from_millis(20))
                            }
                            Ok(None) => {
                                break Err("plain control exceeded its 30-second deadline".into());
                            }
                            Err(error) => break Err(error.to_string()),
                        }
                    };
                    drop(job);
                    let _ = child.wait();
                    let stdout = stdout.join().unwrap();
                    let stderr = stderr.join().unwrap();
                    let status = status.map_err(|error| {
                        format!(
                            "{error}; stdout={:?}; stderr={:?}",
                            String::from_utf8_lossy(&stdout),
                            String::from_utf8_lossy(&stderr)
                        )
                    })?;
                    Ok(Output {
                        status,
                        stdout,
                        stderr,
                    })
                })
            };
            let result = run().map(|output| json!({"status": output.status.code(),
                "stdout": String::from_utf8_lossy(&output.stdout), "stderr": String::from_utf8_lossy(&output.stderr)}));
            eprintln!(
                "LFS_HOOK_LADDER {}",
                json!({"stage": stage, "mode": mode,
                "result": result, "marker": std::fs::read_to_string(&marker).ok(),
                "trace2": std::fs::read_to_string(&trace).ok()})
            );
        }
    }
    std::fs::write(&restore.path, &restore.original).expect("restore original LFS hook");
    for path in [marker, trace, nested] {
        let _ = std::fs::remove_file(path);
    }
}

fn run_lfs_with_runtime(control: Control, mut runtime: Vec<PathBuf>) {
    require_git();
    let lfs = git_lfs_program();
    runtime.push(lfs);
    runtime.sort();
    runtime.dedup();
    let runtime: Vec<_> = runtime.iter().map(|path| (path.as_path(), "r")).collect();
    let root = fixture();
    prepare_remote(root.path());
    conventional_global_config(root.path());
    let policy = (control != Control::Unconfined)
        .then(|| policy(root.path(), control, &[], &runtime, Some("r"), false));
    let project = root.path().join("project");
    let clone = project.join("clone");
    assert_success(
        "clone LFS fixture",
        invoke(
            root.path(),
            &project,
            &[
                "clone",
                root.path().join("remote.git").to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
            control,
            policy.as_ref(),
            &[],
        ),
    );
    assert_success(
        "install local LFS hooks",
        invoke(
            root.path(),
            &clone,
            &["lfs", "install", "--local"],
            control,
            policy.as_ref(),
            &[],
        ),
    );
    assert_success(
        "track LFS fixture",
        invoke(
            root.path(),
            &clone,
            &["lfs", "track", "*.bin"],
            control,
            policy.as_ref(),
            &[],
        ),
    );
    let binary = clone.join("fixture.bin");
    let payload = b"sandbox Git LFS payload\n";
    std::fs::write(&binary, payload).expect("LFS object fixture");
    assert_success(
        "stage LFS object",
        invoke(
            root.path(),
            &clone,
            &["add", ".gitattributes", "fixture.bin"],
            control,
            policy.as_ref(),
            &[],
        ),
    );
    assert_success(
        "commit LFS object",
        invoke(
            root.path(),
            &clone,
            &["commit", "-m", "LFS fixture"],
            control,
            policy.as_ref(),
            &[],
        ),
    );
    let pushed = invoke(
        root.path(),
        &clone,
        &["push", "origin", "HEAD:main"],
        control,
        policy.as_ref(),
        &[],
    );
    if !pushed.status.success() {
        eprintln!(
            "LFS_PRE_PUSH_HOOK {:?}",
            std::fs::read_to_string(clone.join(".git/hooks/pre-push"))
        );
        let remote_path = root.path().join("remote.git");
        for args in [
            vec!["lfs", "logs", "last"],
            vec![
                "-c",
                "alias.sandbox-hook=!sh -x .git/hooks/pre-push",
                "sandbox-hook",
                "origin",
                remote_path.to_str().unwrap(),
            ],
        ] {
            let output = invoke(root.path(), &clone, &args, control, policy.as_ref(), &[]);
            eprintln!("LFS_HOOK_DIAGNOSTIC {args:?} {output:?}");
        }
        #[cfg(windows)]
        if std::env::var_os("NUB_NATIVE_EMBEDDED_ADAPTER").is_some()
            && let Some(policy) = policy.as_ref()
        {
            lfs_hook_startup_ladder(root.path(), &clone, policy);
        }
    }
    assert_success("push LFS object", pushed);
    let consumer = project.join("lfs-consumer");
    assert_success(
        "clone and fetch LFS object",
        invoke(
            root.path(),
            &project,
            &[
                "clone",
                root.path().join("remote.git").to_str().unwrap(),
                consumer.to_str().unwrap(),
            ],
            control,
            policy.as_ref(),
            &[],
        ),
    );
    assert_success(
        "fetch LFS object",
        invoke(
            root.path(),
            &consumer,
            &["lfs", "fetch", "origin", "main"],
            control,
            policy.as_ref(),
            &[],
        ),
    );
    assert_success(
        "checkout fetched LFS object",
        invoke(
            root.path(),
            &consumer,
            &["lfs", "checkout", "fixture.bin"],
            control,
            policy.as_ref(),
            &[],
        ),
    );
    assert!(
        std::fs::read_to_string(clone.join(".gitattributes"))
            .unwrap()
            .contains("*.bin filter=lfs")
    );
    assert_eq!(
        std::fs::read(consumer.join("fixture.bin")).unwrap(),
        payload
    );
}

#[test]
#[ignore = "requires native Git tool functionality job"]
fn git_lfs_native_unconfined_when_available() {
    run_lfs(Control::Unconfined);
}

#[test]
#[ignore = "requires native Git tool functionality job"]
fn git_lfs_native_exact_when_available() {
    run_lfs(Control::Exact);
}

#[test]
#[ignore = "requires native Git tool functionality job"]
fn git_lfs_native_tooldirs_when_available() {
    run_lfs(Control::ToolDirs);
}

#[cfg(windows)]
fn complete_git_runtime_paths() -> Vec<PathBuf> {
    let mut paths = git_runtime_paths();
    let installation = paths
        .iter()
        .flat_map(|path| path.ancestors())
        .find(|path| path.join("cmd/git.exe").is_file() && path.join("usr/bin/sh.exe").is_file())
        .expect("Git for Windows installation containing both Git and its shell")
        .to_path_buf();
    eprintln!("EXPLICIT GIT INSTALLATION READ {}", installation.display());
    paths.push(installation);
    paths
}

#[cfg(windows)]
macro_rules! complete_runtime_case {
    ($name:ident, $control:ident, $run:ident) => {
        #[test]
        #[ignore = "requires native Git tool functionality job"]
        fn $name() {
            $run(Control::$control, complete_git_runtime_paths());
        }
    };
}

#[cfg(windows)]
complete_runtime_case!(
    git_windows_complete_runtime_plain,
    Unconfined,
    run_operations_with_runtime
);
#[cfg(windows)]
complete_runtime_case!(
    git_windows_complete_runtime_exact,
    Exact,
    run_operations_with_runtime
);
#[cfg(windows)]
complete_runtime_case!(
    git_windows_complete_runtime_tooldirs,
    ToolDirs,
    run_operations_with_runtime
);
#[cfg(windows)]
complete_runtime_case!(
    git_windows_complete_runtime_lfs_plain,
    Unconfined,
    run_lfs_with_runtime
);
#[cfg(windows)]
complete_runtime_case!(
    git_windows_complete_runtime_lfs_exact,
    Exact,
    run_lfs_with_runtime
);
#[cfg(windows)]
complete_runtime_case!(
    git_windows_complete_runtime_lfs_tooldirs,
    ToolDirs,
    run_lfs_with_runtime
);

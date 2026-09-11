//! Native pip and uv operations with local-wheel, exact-grant, and `$tooldirs` controls.

use nub_sandbox::{CommandSpec, CompileCtx, Homes, ScopeCapabilities, compile};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixture() -> tempfile::TempDir {
    // Keep the synthetic home outside OS temporary paths. The sandbox's private tmp is
    // intentionally the only temporary location a confined tool receives.
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .expect("runner home");
    let root = tempfile::Builder::new()
        .prefix("sandbox-python-tool-")
        .tempdir_in(home)
        .expect("fixture root");
    for path in ["home", "project", "cache"] {
        std::fs::create_dir_all(root.path().join(path)).expect("fixture directory");
    }
    root
}

fn path_separator() -> &'static str {
    if cfg!(windows) { ";" } else { ":" }
}

fn env_for(root: &Path, tool: &Tool, paths: &ConfiguredPaths) -> BTreeMap<String, String> {
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
            env.insert(key.to_string(), value);
        }
    }
    let home = root.join("home");
    let project_site = root.join("project/site");
    env.insert("HOME".into(), home.to_string_lossy().into());
    env.insert("USERPROFILE".into(), home.to_string_lossy().into());
    env.insert(
        "XDG_CACHE_HOME".into(),
        home.join("cache-home").to_string_lossy().into(),
    );
    env.insert(
        "XDG_CONFIG_HOME".into(),
        home.join("config-home").to_string_lossy().into(),
    );
    env.insert(
        "XDG_DATA_HOME".into(),
        home.join("data-home").to_string_lossy().into(),
    );
    env.insert(
        "LOCALAPPDATA".into(),
        home.join("AppData/Local").to_string_lossy().into(),
    );
    env.insert(
        "APPDATA".into(),
        home.join("AppData/Roaming").to_string_lossy().into(),
    );
    env.insert(
        "PIP_CACHE_DIR".into(),
        paths.pip_cache.to_string_lossy().into(),
    );
    env.insert(
        "PIP_CONFIG_FILE".into(),
        paths.pip_config.to_string_lossy().into(),
    );
    env.insert("PIP_DISABLE_PIP_VERSION_CHECK".into(), "1".into());
    // The fixture is self-contained: dependency resolution must not consult an index.
    env.insert("PIP_NO_INDEX".into(), "1".into());
    env.insert(
        "PYTHONUSERBASE".into(),
        paths.user_base.to_string_lossy().into(),
    );
    env.insert(
        "UV_CACHE_DIR".into(),
        paths.uv_cache.to_string_lossy().into(),
    );
    env.insert(
        "UV_TOOL_DIR".into(),
        paths.uv_tools.to_string_lossy().into(),
    );
    env.insert(
        "UV_TOOL_BIN_DIR".into(),
        paths.uv_bin.to_string_lossy().into(),
    );
    // `UV_OFFLINE` is uv's documented `--offline` equivalent.
    env.insert("UV_OFFLINE".into(), "1".into());
    env.insert(
        "PYTHONPATH".into(),
        format!(
            "{}{}{}",
            tool.tool_root.display(),
            path_separator(),
            project_site.display()
        ),
    );
    env
}

fn policy(root: &Path, fs: Value, env: BTreeMap<String, String>) -> nub_sandbox::SandboxPolicy {
    let mut fs = match fs {
        Value::Object(entries) => entries,
        _ => panic!("fixture filesystem policy must be an object"),
    };
    tool_msys::grant(&mut fs);
    if std::env::var_os("NUB_NATIVE_ADAPTER_PROBE_ENABLE").is_some() {
        let adapter = std::env::var("NUB_NATIVE_ADAPTER_PROBE_DIR").unwrap();
        fs.insert(adapter, Value::String("r".into()));
    }
    fs.insert("./".into(), Value::String("rw".into()));
    fs.insert("$tmp".into(), Value::String("rw".into()));
    let homes = Homes {
        home: root.join("home"),
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
        compile(&json!({"fs": fs, "net": false}), &ctx).expect("Python policy compiles");
    policy.env.constructed = env;
    policy
}

fn exact_grants(paths: &[(&Path, &str)]) -> Value {
    let mut entries = Map::new();
    for (path, access) in paths {
        entries.insert(
            path.to_string_lossy().into_owned(),
            Value::String((*access).into()),
        );
    }
    Value::Object(entries)
}

#[path = "common/tool_msys.rs"]
mod tool_msys;
#[path = "common/tool_output.rs"]
mod tool_output;
#[path = "common/tool_sandbox.rs"]
mod tool_sandbox;

fn confined(
    program: &Path,
    args: &[String],
    root: &Path,
    policy: &nub_sandbox::SandboxPolicy,
) -> Output {
    eprintln!("CONFINED {} {args:?}", program.display());
    let (program, args) = tool_msys::command(program, args.to_vec(), &root.join("project"));
    let sandbox = tool_sandbox::acquire(policy).expect("Python sandbox acquires");
    let prepared = sandbox
        .prepare(
            CommandSpec::new(program.to_string_lossy().into_owned())
                .args(args)
                .redact_stdout(true)
                .redact_stderr(true)
                .cwd(root.join("project")),
        )
        .expect("Python command prepares without degradation");
    assert!(
        prepared.degradation.lost.is_empty(),
        "native Python fixture degraded: {:?}",
        prepared.degradation
    );
    tool_output::output(prepared)
}

fn unconfined(
    program: &Path,
    args: &[String],
    root: &Path,
    env: &BTreeMap<String, String>,
) -> Output {
    eprintln!("UNCONFINED {} {args:?}", program.display());
    let (program, args) = tool_msys::command(program, args.to_vec(), &root.join("project"));
    let mut command = Command::new(program);
    command.args(args).current_dir(root.join("project"));
    command.env_clear();
    command.envs(env);
    command
        .output()
        .expect("unconfined Python command launches")
}

#[derive(Deserialize)]
struct Tool {
    name: String,
    spec: String,
    program: PathBuf,
    prefix: Vec<String>,
    #[serde(rename = "toolRoot")]
    tool_root: PathBuf,
    #[serde(rename = "runtimeRoot")]
    runtime_root: PathBuf,
    python: PathBuf,
    version: String,
    wheel: PathBuf,
}

fn tool(name: &str) -> Tool {
    let matrix = std::env::var("NUB_SANDBOX_PYTHON_TOOL_MATRIX_FILE").expect(
        "Python tool matrix missing: run `node scripts/sandbox-python-fixtures.mjs` before native tests",
    );
    let text = std::fs::read_to_string(&matrix)
        .unwrap_or_else(|error| panic!("Python tool matrix `{matrix}` unreadable: {error}"));
    let tools: Vec<Tool> = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("Python tool matrix `{matrix}` is invalid: {error}"));
    let expected_version = match name {
        "pip" => "26.2.1",
        "uv" => "0.12.11",
        _ => unreachable!("only pinned Python tools have fixture cases"),
    };
    let tool = tools
        .into_iter()
        .find(|tool| tool.name == name)
        .unwrap_or_else(|| panic!("Python tool matrix omitted {name}"));
    assert_eq!(tool.version, expected_version, "{name} fixture pin drifted");
    assert!(
        tool.program.exists(),
        "{name} program missing: {}",
        tool.program.display()
    );
    assert!(
        tool.wheel.exists(),
        "local wheel missing: {}",
        tool.wheel.display()
    );
    tool
}

struct ConfiguredPaths {
    pip_cache: PathBuf,
    pip_config: PathBuf,
    user_base: PathBuf,
    uv_cache: PathBuf,
    uv_tools: PathBuf,
    uv_bin: PathBuf,
}

fn configured_paths(root: &Path) -> ConfiguredPaths {
    let data = root.join("home/data-home");
    ConfiguredPaths {
        pip_cache: root.join("home/cache-home/pip"),
        pip_config: root.join("home/config-home/pip/pip.conf"),
        // Keep this distinct from XDG_DATA_HOME: `$tooldirs` must expand the documented
        // PYTHONUSERBASE override rather than incidentally covering it through XDG.
        user_base: root.join("custom-python-user-base"),
        uv_cache: root.join("home/cache-home/uv"),
        uv_tools: data.join("uv/tools"),
        uv_bin: root.join("home/tool-bin"),
    }
}

fn initialize_configured_roots(paths: &ConfiguredPaths) {
    for path in [
        &paths.pip_cache,
        &paths.user_base,
        &paths.uv_cache,
        &paths.uv_tools,
        &paths.uv_bin,
    ] {
        std::fs::create_dir_all(path).expect("configured tool root precondition");
    }
    std::fs::create_dir_all(paths.pip_config.parent().expect("pip config parent"))
        .expect("pip config parent precondition");
    std::fs::write(&paths.pip_config, "[global]\n").expect("pip config precondition");
}

fn grant_policy(
    tool: &Tool,
    root: &Path,
    paths: &ConfiguredPaths,
    env: BTreeMap<String, String>,
    tooldirs: bool,
) -> nub_sandbox::SandboxPolicy {
    let fs = if tooldirs {
        let mut entries = Map::new();
        entries.insert("$tooldirs".into(), Value::String("rw".into()));
        entries.insert(
            tool.tool_root.to_string_lossy().into_owned(),
            Value::String("r".into()),
        );
        entries.insert(
            tool.runtime_root.to_string_lossy().into_owned(),
            Value::String("r".into()),
        );
        entries.insert(
            tool.wheel
                .parent()
                .expect("fixture wheel has a parent")
                .to_string_lossy()
                .into_owned(),
            Value::String("r".into()),
        );
        Value::Object(entries)
    } else {
        exact_grants(&[
            (&paths.pip_cache, "rw"),
            // Ordinary operations only read this fixture-owned configuration file. A future
            // `pip config set` fixture needs an explicit writable parent directory because pip
            // creates it before directly opening the file for write.
            (&paths.pip_config, "r"),
            (&paths.user_base, "rw"),
            (&paths.uv_cache, "rw"),
            (&paths.uv_tools, "rw"),
            (&paths.uv_bin, "rw"),
            (&tool.tool_root, "r"),
            (&tool.runtime_root, "r"),
            (
                tool.wheel.parent().expect("fixture wheel has a parent"),
                "r",
            ),
        ])
    };
    policy(root, fs, env)
}

fn tool_args(tool: &Tool, tail: &[&str]) -> Vec<String> {
    tool.prefix
        .iter()
        .cloned()
        .chain(tail.iter().map(|arg| (*arg).to_string()))
        .collect()
}

fn invoke(
    tool: &Tool,
    tail: &[&str],
    root: &Path,
    env: &BTreeMap<String, String>,
    policy: Option<&nub_sandbox::SandboxPolicy>,
) -> Output {
    let args = tool_args(tool, tail);
    match policy {
        Some(policy) => confined(&tool.program, &args, root, policy),
        None => unconfined(&tool.program, &args, root, env),
    }
}

fn invoke_python(
    tool: &Tool,
    args: &[&str],
    root: &Path,
    env: &BTreeMap<String, String>,
    policy: Option<&nub_sandbox::SandboxPolicy>,
) -> Output {
    let args = args
        .iter()
        .map(|arg| (*arg).to_string())
        .collect::<Vec<_>>();
    match policy {
        Some(policy) => confined(&tool.python, &args, root, policy),
        None => unconfined(&tool.python, &args, root, env),
    }
}

fn assert_success(tool: &Tool, phase: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{} {} failed:\nstdout:\n{}\nstderr:\n{}",
        tool.name,
        phase,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_fixture_runs(tool: &Tool, output: &Output) {
    assert_success(tool, "fixture import/run", output);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        stdout.contains("sandbox-python-fixture-ok"),
        "{} did not import and run the local wheel: {stdout}",
        tool.name
    );
}

fn assert_cache_path(tool: &Tool, output: &Output, expected: &str) {
    assert_success(tool, "configured cache directory", output);
    let actual = String::from_utf8_lossy(&output.stdout);
    let actual = Path::new(actual.trim());
    let actual = std::fs::canonicalize(actual).unwrap_or_else(|error| {
        panic!(
            "{} reported an unusable cache path `{actual:?}`: {error}",
            tool.name
        )
    });
    let expected = std::fs::canonicalize(expected).unwrap_or_else(|error| {
        panic!(
            "{} configured cache path `{expected}` is unavailable: {error}",
            tool.name
        )
    });
    #[cfg(windows)]
    let matches = actual
        .to_string_lossy()
        .eq_ignore_ascii_case(&expected.to_string_lossy());
    #[cfg(not(windows))]
    let matches = actual == expected;
    assert!(
        matches,
        "{} did not select the configured cache: reported {}, expected {}",
        tool.name,
        actual.display(),
        expected.display()
    );
}

fn run_pip_operations(
    tool: &Tool,
    root: &Path,
    env: &BTreeMap<String, String>,
    policy: Option<&nub_sandbox::SandboxPolicy>,
) {
    let wheel = tool.wheel.to_string_lossy().into_owned();
    let target = root.join("project/site").to_string_lossy().into_owned();
    assert_success(
        tool,
        "local-wheel install",
        &invoke(
            tool,
            &["install", "--no-deps", "--target", &target, &wheel],
            root,
            env,
            policy,
        ),
    );
    assert_success(
        tool,
        "warm local-wheel reinstall",
        &invoke(
            tool,
            &[
                "install",
                "--no-deps",
                "--upgrade",
                "--target",
                &target,
                &wheel,
            ],
            root,
            env,
            policy,
        ),
    );
    assert_fixture_runs(
        tool,
        &invoke_python(
            tool,
            &[
                "-c",
                "import sandbox_python_fixture; sandbox_python_fixture.main()",
            ],
            root,
            env,
            policy,
        ),
    );
    assert_success(
        tool,
        "configured user install",
        &invoke(
            tool,
            &[
                "install",
                "--no-deps",
                "--user",
                "--force-reinstall",
                "--break-system-packages",
                &wheel,
            ],
            root,
            env,
            policy,
        ),
    );
    let cache = invoke(tool, &["cache", "dir"], root, env, policy);
    assert_cache_path(
        tool,
        &cache,
        env.get("PIP_CACHE_DIR").expect("pip cache env"),
    );
    assert_success(
        tool,
        "configured cache prune",
        &invoke(tool, &["cache", "purge"], root, env, policy),
    );
}

fn venv_python(root: &Path) -> PathBuf {
    if cfg!(windows) {
        root.join("project/.venv/Scripts/python.exe")
    } else {
        root.join("project/.venv/bin/python")
    }
}

fn run_uv_operations(
    tool: &Tool,
    root: &Path,
    env: &BTreeMap<String, String>,
    policy: Option<&nub_sandbox::SandboxPolicy>,
    expected_cache: &Path,
) {
    let wheel = tool.wheel.to_string_lossy().into_owned();
    let python = tool.python.to_string_lossy().into_owned();
    let venv = venv_python(root);
    let venv_text = venv.to_string_lossy().into_owned();
    assert_success(
        tool,
        "project environment creation",
        &invoke(
            tool,
            &["venv", ".venv", "--python", &python],
            root,
            env,
            policy,
        ),
    );
    assert_success(
        tool,
        "local-wheel install",
        &invoke(
            tool,
            &["pip", "install", "--python", &venv_text, &wheel],
            root,
            env,
            policy,
        ),
    );
    assert_success(
        tool,
        "warm local-wheel reinstall",
        &invoke(
            tool,
            &["pip", "install", "--python", &venv_text, &wheel],
            root,
            env,
            policy,
        ),
    );
    assert_fixture_runs(
        tool,
        &match policy {
            Some(policy) => confined(
                &venv,
                &[
                    "-c".into(),
                    "import sandbox_python_fixture; sandbox_python_fixture.main()".into(),
                ],
                root,
                policy,
            ),
            None => unconfined(
                &venv,
                &[
                    "-c".into(),
                    "import sandbox_python_fixture; sandbox_python_fixture.main()".into(),
                ],
                root,
                env,
            ),
        },
    );
    assert_success(
        tool,
        "configured tool install",
        &invoke(
            tool,
            &["tool", "install", "--python", &python, &wheel],
            root,
            env,
            policy,
        ),
    );
    assert_fixture_runs(
        tool,
        &invoke(
            tool,
            &["tool", "run", "sandbox-python-fixture"],
            root,
            env,
            policy,
        ),
    );
    let cache = invoke(tool, &["cache", "dir"], root, env, policy);
    assert_cache_path(tool, &cache, expected_cache.to_str().unwrap());
    assert_success(
        tool,
        "configured cache prune",
        &invoke(tool, &["cache", "prune"], root, env, policy),
    );
}

fn run_tool_control(name: &str, label: &str, tooldirs: Option<bool>) {
    let tool = tool(if name == "uv-default" { "uv" } else { name });
    let root = fixture();
    let mut paths = configured_paths(root.path());
    if name == "uv-default" {
        paths.uv_cache = root.path().join("home/.cache/uv");
    }
    // Existing configured roots are a native-backend precondition. A separate report records
    // absent-root behavior rather than treating it as a normal-operation pass.
    initialize_configured_roots(&paths);
    let mut env = env_for(root.path(), &tool, &paths);
    if name == "uv-default" {
        env.remove("UV_CACHE_DIR");
        env.remove("XDG_CACHE_HOME");
    }
    let policy =
        tooldirs.map(|tooldirs| grant_policy(&tool, root.path(), &paths, env.clone(), tooldirs));
    eprintln!(
        "PYTHON TOOL {} {} ({}) {label} control",
        tool.name, tool.version, tool.spec
    );
    match name {
        "pip" => run_pip_operations(&tool, root.path(), &env, policy.as_ref()),
        "uv" | "uv-default" => {
            run_uv_operations(&tool, root.path(), &env, policy.as_ref(), &paths.uv_cache)
        }
        _ => unreachable!("tool matrix was validated above"),
    }
}

#[cfg(windows)]
fn run_python_adapter(name: &str, tooldirs: bool, readable_ancestors: bool) {
    let tool = tool(name);
    let root = fixture();
    let paths = configured_paths(root.path());
    initialize_configured_roots(&paths);
    let mut env = env_for(root.path(), &tool, &paths);
    let startup = root.path().join("project/python-startup");
    std::fs::create_dir(&startup).unwrap();
    std::fs::write(
        startup.join("sitecustomize.py"),
        nub_sandbox::windows_python_compat_source(),
    )
    .unwrap();
    let existing = env.get("PYTHONPATH").unwrap();
    env.insert(
        "PYTHONPATH".into(),
        format!("{};{existing}", startup.display()),
    );
    let mut policy = grant_policy(&tool, root.path(), &paths, env.clone(), tooldirs);
    if readable_ancestors {
        use nub_sandbox::policy::{CanonGlob, Effect, FsAccess, FsOrigin, FsRule};
        let mut ancestors = std::collections::BTreeSet::new();
        for rule in &policy.fs.rules.entries {
            let path = rule.matcher.as_str().trim_end_matches("/**");
            if !path.contains('*') {
                ancestors.extend(Path::new(path).ancestors().skip(1).map(Path::to_path_buf));
            }
        }
        // A bounded diagnostic: read directory nodes, never their descendants.
        // Distinguish missing ancestor metadata from native canonicalization limits.
        for path in ancestors {
            if !path.as_os_str().is_empty() {
                policy.fs.rules.entries.push(FsRule {
                    matcher: CanonGlob(path.to_string_lossy().into_owned()),
                    effect: Effect::Allow,
                    access: FsAccess::Read,
                    origin: FsOrigin::Speculative,
                });
            }
        }
    }
    let retained = tool_sandbox::acquire(&policy).unwrap();
    // Separate one-shot acquisitions share this live resource throughout the sequence.
    let probe = r#"import os, pathlib, tempfile
assert getattr(os.mkdir, '_appcontainer_compatible', False)
p = pathlib.Path(tempfile.mkdtemp())
(p / 'allowed').write_text('OK')
assert (p / 'allowed').read_text() == 'OK'
try:
    os.mkdir(p, 0o700)
except FileExistsError:
    pass
else:
    raise AssertionError('existing private directory must fail')
try:
    os.mkdir('bad\0path', 0o700)
except ValueError:
    pass
else:
    raise AssertionError('embedded NUL must fail')
os.mkdir(p / 'nested', 0o700)
(p / 'nested' / 'allowed').write_text('NESTED')
os.mkdir(p / 'ordinary', 0o777)
import ctypes
from ctypes import wintypes
api = ctypes.WinDLL('advapi32', use_last_error=True)
api.GetNamedSecurityInfoW.argtypes = [wintypes.LPWSTR, ctypes.c_int, wintypes.DWORD,
    ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p,
    ctypes.POINTER(ctypes.c_void_p)]
api.ConvertSecurityDescriptorToStringSecurityDescriptorW.argtypes = [ctypes.c_void_p,
    wintypes.DWORD, wintypes.DWORD, ctypes.POINTER(wintypes.LPWSTR), ctypes.c_void_p]
free = ctypes.WinDLL('kernel32').LocalFree
free.argtypes = [ctypes.c_void_p]
descriptor = ctypes.c_void_p()
assert api.GetNamedSecurityInfoW(str(p), 1, 4, None, None, None, None, ctypes.byref(descriptor)) == 0
text = wintypes.LPWSTR()
try:
    assert api.ConvertSecurityDescriptorToStringSecurityDescriptorW(descriptor, 1, 4, ctypes.byref(text), None)
    acl = text.value
    assert acl.startswith('D:P'), acl
    assert acl.count('(A;') == 4, acl
    assert ';;;AC)' not in acl and ';;;S-1-15-2-1)' not in acl, acl
    assert ';;;S-1-15-2-' in acl, acl
    print('PRIVATE_DIRECTORY_ACL', acl)
finally:
    if text: free(text)
    free(descriptor)
print('PRIVATE_DIRECTORY_OK')
"#;
    assert_success(
        &tool,
        "private directory adapter",
        &invoke_python(&tool, &["-c", probe], root.path(), &env, Some(&policy)),
    );
    match name {
        "pip" => run_pip_operations(&tool, root.path(), &env, Some(&policy)),
        "uv" => run_uv_operations(&tool, root.path(), &env, Some(&policy), &paths.uv_cache),
        _ => unreachable!(),
    }
    let secret = root.path().join("denied-secret");
    std::fs::write(&secret, "WITHHELD").unwrap();
    let canary = format!(
        "from pathlib import Path\ntry:\n Path({}).read_text()\nexcept PermissionError:\n print('CANARY_DENIED')\nelse:\n raise AssertionError('canary exposed')",
        serde_json::to_string(secret.to_str().unwrap()).unwrap()
    );
    assert_success(
        &tool,
        "adapter permission canary",
        &invoke_python(&tool, &["-c", &canary], root.path(), &env, Some(&policy)),
    );
    retained.close();
    nub_sandbox::cleanup().unwrap();
}

#[cfg(windows)]
#[test]
#[ignore = "requires the pinned native Python tool matrix"]
fn windows_adapter_pip_exact() {
    run_python_adapter("pip", false, false);
}

#[cfg(windows)]
#[test]
#[ignore = "requires the pinned native Python tool matrix"]
fn windows_adapter_pip_tooldirs() {
    run_python_adapter("pip", true, false);
}

#[cfg(windows)]
#[test]
#[ignore = "requires the pinned native Python tool matrix"]
fn windows_adapter_uv_exact() {
    run_python_adapter("uv", false, false);
}

#[cfg(windows)]
#[test]
#[ignore = "requires the pinned native Python tool matrix"]
fn windows_adapter_uv_tooldirs() {
    run_python_adapter("uv", true, false);
}

#[cfg(windows)]
#[test]
#[ignore = "requires the pinned native Python tool matrix"]
fn windows_adapter_uv_readable_ancestors() {
    run_python_adapter("uv", true, true);
}

macro_rules! python_tool_test {
    ($name:ident, $tool:literal, $label:literal, $tooldirs:expr) => {
        #[test]
        #[ignore = "requires the pinned native Python tool matrix"]
        fn $name() {
            run_tool_control($tool, $label, $tooldirs);
        }
    };
}

python_tool_test!(pip_unconfined, "pip", "unconfined", None);
python_tool_test!(pip_exact_grants, "pip", "exact", Some(false));
python_tool_test!(pip_tooldirs, "pip", "$tooldirs", Some(true));
python_tool_test!(uv_unconfined, "uv", "unconfined", None);
python_tool_test!(uv_exact_grants, "uv", "exact", Some(false));
python_tool_test!(uv_tooldirs, "uv", "$tooldirs", Some(true));

#[cfg(unix)]
python_tool_test!(
    uv_default_cache_unconfined,
    "uv-default",
    "unconfined",
    None
);
#[cfg(unix)]
python_tool_test!(uv_default_cache_exact, "uv-default", "exact", Some(false));
#[cfg(unix)]
python_tool_test!(
    uv_default_cache_tooldirs,
    "uv-default",
    "$tooldirs",
    Some(true)
);

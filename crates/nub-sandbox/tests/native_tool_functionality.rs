//! Native Cargo/rustup, Go, JVM, NuGet, and Composer tool-directory controls.

#[path = "common/tool_msys.rs"]
mod tool_msys;
#[path = "common/tool_output.rs"]
mod tool_output;
#[path = "common/tool_sandbox.rs"]
mod tool_sandbox;

use nub_sandbox::{CommandSpec, CompileCtx, Homes, ScopeCapabilities, compile};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[derive(Deserialize)]
struct Tool {
    name: String,
    program: PathBuf,
    prefix: Vec<String>,
    version: String,
    #[serde(rename = "toolRoot")]
    tool_root: PathBuf,
    #[serde(rename = "runtimeRoots")]
    runtime_roots: Vec<PathBuf>,
    #[serde(rename = "toolEnv", default)]
    tool_env: BTreeMap<String, String>,
    #[serde(default)]
    shell: bool,
    #[serde(rename = "mavenSeed")]
    maven_seed: Option<PathBuf>,
}

fn tool(name: &str) -> Tool {
    let matrix = std::env::var("NUB_SANDBOX_NATIVE_TOOL_MATRIX_FILE").expect(
        "native tool matrix missing: run `node scripts/sandbox-native-tool-fixtures.mjs` before native tests",
    );
    let text = std::fs::read_to_string(&matrix)
        .unwrap_or_else(|error| panic!("native tool matrix `{matrix}` unreadable: {error}"));
    serde_json::from_str::<Vec<Tool>>(&text)
        .unwrap_or_else(|error| panic!("native tool matrix `{matrix}` is invalid: {error}"))
        .into_iter()
        .find(|tool| tool.name == name)
        .unwrap_or_else(|| panic!("native tool matrix omitted {name}"))
}

fn fixture() -> tempfile::TempDir {
    let parent = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .expect("runner home");
    let root = tempfile::Builder::new()
        .prefix("sandbox-native-tool-")
        .tempdir_in(parent)
        .expect("fixture root");
    for path in ["home", "home/.m2", "project/java-tmp", "cache", "tmp"] {
        std::fs::create_dir_all(root.path().join(path)).expect("fixture directory");
    }
    root
}

fn env_for(root: &Path, tool: &Tool) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for key in [
        "PATH",
        "SystemRoot",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramData",
        "ALLUSERSPROFILE",
    ] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.into(), value);
        }
    }
    let home = root.join("home");
    for (key, path) in [
        ("HOME", home.clone()),
        ("USERPROFILE", home.clone()),
        ("APPDATA", home.join("AppData/Roaming")),
        ("LOCALAPPDATA", home.join("AppData/Local")),
        ("XDG_CACHE_HOME", home.join("cache")),
        ("XDG_CONFIG_HOME", home.join("config")),
        ("XDG_DATA_HOME", home.join("data")),
        ("CARGO_HOME", home.join("cargo-home")),
        ("RUSTUP_HOME", home.join("rustup-home")),
        ("CARGO_TARGET_DIR", root.join("cargo-target")),
        ("GOMODCACHE", home.join("go-mod")),
        ("GOCACHE", home.join("go-cache")),
        ("GOBIN", home.join("go-bin")),
        ("GOTMPDIR", root.join("go-tmp")),
        ("GRADLE_USER_HOME", home.join("gradle")),
        ("NUGET_PACKAGES", home.join(".nuget/packages")),
        ("NUGET_HTTP_CACHE_PATH", home.join(".nuget/http-cache")),
        ("NUGET_SCRATCH", home.join(".nuget/scratch")),
        (
            "NUGET_PLUGINS_CACHE_PATH",
            home.join(".nuget/plugins-cache"),
        ),
        ("COMPOSER_HOME", home.join("composer-home")),
        ("COMPOSER_CACHE_DIR", home.join("composer-cache")),
        ("COMPOSER_VENDOR_DIR", root.join("project/vendor")),
        ("COMPOSER_BIN_DIR", root.join("project/vendor/bin")),
        ("DOTNET_CLI_HOME", home.join("dotnet")),
    ] {
        std::fs::create_dir_all(&path).expect("configured root precondition");
        env.insert(key.into(), path.to_string_lossy().into());
    }
    env.insert("GOPROXY".into(), "off".into());
    env.insert("GOSUMDB".into(), "off".into());
    env.insert(
        "JAVA_TOOL_OPTIONS".into(),
        format!(
            "-Djava.io.tmpdir={} -Duser.home={}",
            root.join("project/java-tmp").display(),
            home.display()
        ),
    );
    env.extend(tool.tool_env.clone());
    if tool.name == "nuget" {
        std::fs::create_dir_all(nuget_config(root)).expect("NuGet user configuration root");
    }
    if tool.name == "maven" {
        let rc = if cfg!(windows) {
            "@echo loaded> \"%USERPROFILE%\\..\\project\\mavenrc-loaded\"\r\n"
        } else {
            "printf loaded > \"$HOME/../project/mavenrc-loaded\"\n"
        };
        std::fs::write(maven_config(root), rc).expect("Maven user startup file");
    }
    if tool.name == "go" {
        std::fs::create_dir_all(go_config(root)).expect("Go user configuration root");
    }
    if let Some(seed) = &tool.maven_seed {
        copy_directory(seed, &home.join(".m2/repository"));
    }
    env
}

fn nuget_config(root: &Path) -> PathBuf {
    root.join(if cfg!(windows) {
        "home/AppData/Roaming/NuGet"
    } else {
        "home/.nuget/NuGet"
    })
}

fn maven_config(root: &Path) -> PathBuf {
    root.join(if cfg!(windows) {
        "home/mavenrc_pre.cmd"
    } else {
        "home/.mavenrc"
    })
}

fn go_config(root: &Path) -> PathBuf {
    root.join(if cfg!(windows) {
        "home/AppData/Roaming/go"
    } else if cfg!(target_os = "macos") {
        "home/Library/Application Support/go"
    } else {
        "home/config/go"
    })
}

fn copy_directory(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("Maven seed destination");
    for entry in std::fs::read_dir(source).expect("Maven seed source") {
        let entry = entry.expect("Maven seed entry");
        let destination = destination.join(entry.file_name());
        if entry.file_type().expect("Maven seed type").is_dir() {
            copy_directory(&entry.path(), &destination);
        } else {
            std::fs::copy(entry.path(), destination).expect("Maven seed file");
        }
    }
}

fn policy(
    root: &Path,
    tool: &Tool,
    env: BTreeMap<String, String>,
    tooldirs: bool,
) -> nub_sandbox::SandboxPolicy {
    let mut fs = Map::new();
    tool_msys::grant(&mut fs);
    if std::env::var_os("NUB_NATIVE_ADAPTER_PROBE_ENABLE").is_some() {
        let adapter = std::env::var("NUB_NATIVE_ADAPTER_PROBE_DIR").unwrap();
        fs.insert(adapter, Value::String("r".into()));
    }
    if tooldirs {
        fs.insert("$tooldirs".into(), Value::String("rw".into()));
    } else {
        for key in [
            "CARGO_HOME",
            "RUSTUP_HOME",
            "CARGO_TARGET_DIR",
            "GOMODCACHE",
            "GOCACHE",
            "GOBIN",
            "GOTMPDIR",
            "GRADLE_USER_HOME",
            "NUGET_PACKAGES",
            "NUGET_HTTP_CACHE_PATH",
            "NUGET_SCRATCH",
            "NUGET_PLUGINS_CACHE_PATH",
            "COMPOSER_HOME",
            "COMPOSER_CACHE_DIR",
            "COMPOSER_VENDOR_DIR",
            "COMPOSER_BIN_DIR",
            "DOTNET_CLI_HOME",
        ] {
            // NuGet clears and recreates these children; its stable parent grant
            // below covers them without pinning an authored, now-absent child.
            if tool.name == "nuget" && key.starts_with("NUGET_") {
                continue;
            }
            if let Some(path) = env.get(key) {
                fs.insert(path.clone(), Value::String("rw".into()));
            }
        }
        fs.insert(
            root.join("home/.m2").to_string_lossy().into(),
            Value::String("rw".into()),
        );
        if tool.name == "nuget" {
            fs.insert(
                root.join("home/.nuget").to_string_lossy().into(),
                Value::String("rw".into()),
            );
            fs.insert(
                nuget_config(root).to_string_lossy().into(),
                Value::String("rw".into()),
            );
        }
        if tool.name == "maven" {
            insert_read(&mut fs, &maven_config(root));
        }
        if tool.name == "go" {
            fs.insert(
                go_config(root).to_string_lossy().into(),
                Value::String("rw".into()),
            );
        }
    }
    fs.insert("./".into(), Value::String("rw".into()));
    fs.insert("$tmp".into(), Value::String("rw".into()));
    insert_read(&mut fs, &tool.tool_root);
    for root in &tool.runtime_roots {
        insert_read(&mut fs, root);
    }
    insert_read(&mut fs, tool.program.parent().expect("tool program parent"));
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
    // Gradle's file-lock service uses sockets even for an offline local build.
    // This matrix tests filesystem grants; network enforcement has separate tests.
    let network = tool.name == "gradle";
    if network {
        println!("NATIVE TOOL CAPABILITY gradle network=true (file-lock service)");
    }
    let mut policy =
        compile(&json!({"fs": fs, "net": network}), &ctx).expect("native tool policy compiles");
    policy.env.constructed = env;
    policy
}

fn insert_read(fs: &mut Map<String, Value>, path: &Path) {
    fs.entry(path.to_string_lossy().into_owned())
        .or_insert_with(|| Value::String("r".into()));
}

fn run(
    tool: &Tool,
    args: &[&str],
    root: &Path,
    env: &BTreeMap<String, String>,
    policy: Option<&nub_sandbox::SandboxPolicy>,
) -> Output {
    let args = tool
        .prefix
        .iter()
        .cloned()
        .chain(args.iter().map(|arg| (*arg).to_string()))
        .collect::<Vec<_>>();
    let (program, args) = if tool.shell {
        (tool.program.clone(), args)
    } else {
        tool_msys::command(&tool.program, args, &root.join("project"))
    };
    match policy {
        Some(policy) => {
            let sandbox = tool_sandbox::acquire(policy).expect("sandbox acquires");
            let spec = if tool.shell {
                CommandSpec::new(std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into()))
                    .verbatim_command_line(format!(
                        "/d /s /c \"{}\"",
                        command_line(&tool.program, &args)
                    ))
                    .cwd(root.join("project"))
                    .redact_stdout(true)
                    .redact_stderr(true)
            } else {
                CommandSpec::new(program.to_string_lossy().into_owned())
                    .args(args.iter().cloned())
                    .cwd(root.join("project"))
                    .redact_stdout(true)
                    .redact_stderr(true)
            };
            let prepared = sandbox.prepare(spec).expect("tool prepares");
            assert!(
                prepared.degradation.lost.is_empty(),
                "{} degraded: {:?}",
                tool.name,
                prepared.degradation
            );
            tool_output::output(prepared)
        }
        None => {
            let mut command = if tool.shell {
                let mut command =
                    Command::new(std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into()));
                #[cfg(windows)]
                command.raw_arg(format!(
                    "/d /s /c \"{}\"",
                    command_line(&tool.program, &args)
                ));
                #[cfg(not(windows))]
                command.args(["/d", "/s", "/c", &command_line(&tool.program, &args)]);
                command
            } else {
                let mut command = Command::new(&program);
                command.args(&args);
                command
            };
            command
                .current_dir(root.join("project"))
                .env_clear()
                .envs(env);
            command.output().expect("unconfined tool launches")
        }
    }
}

fn command_line(program: &Path, args: &[String]) -> String {
    std::iter::once(program.to_string_lossy().into_owned())
        .chain(args.iter().cloned())
        .map(|arg| format!("\"{}\"", arg.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn assert_ok(tool: &Tool, phase: &str, output: Output) {
    assert!(
        output.status.success(),
        "{} {phase} failed:\nstdout:\n{}\nstderr:\n{}",
        tool.name,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_projects(root: &Path) {
    let project = root.join("project");
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"native_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("src/main.rs"),
        "fn main() { println!(\"ok\"); }\n",
    )
    .unwrap();
    std::fs::write(
        project.join("go.mod"),
        "module example.invalid/nativefixture\ngo 1.25\n",
    )
    .unwrap();
    std::fs::write(project.join("main.go"), "package main\nfunc main() {}\n").unwrap();
    std::fs::write(
        project.join("build.gradle"),
        "tasks.register('fixture') { doLast { println 'ok' } }\n",
    )
    .unwrap();
    std::fs::write(
        project.join("settings.gradle"),
        "rootProject.name = 'native-fixture'\n",
    )
    .unwrap();
    std::fs::write(
        project.join("global.json"),
        "{\"sdk\":{\"version\":\"10.0.100\",\"rollForward\":\"disable\"}}\n",
    )
    .unwrap();
    std::fs::write(project.join("pom.xml"), "<project xmlns=\"http://maven.apache.org/POM/4.0.0\"><modelVersion>4.0.0</modelVersion><groupId>example</groupId><artifactId>fixture</artifactId><version>1</version><build><plugins><plugin><groupId>org.apache.maven.plugins</groupId><artifactId>maven-clean-plugin</artifactId><version>3.4.1</version></plugin></plugins></build></project>\n").unwrap();
    std::fs::write(project.join("fixture.csproj"), "<Project Sdk=\"Microsoft.NET.Sdk\"><PropertyGroup><TargetFramework>net10.0</TargetFramework><OutputType>Exe</OutputType><ImplicitUsings>enable</ImplicitUsings></PropertyGroup></Project>\n").unwrap();
    std::fs::write(
        project.join("Program.cs"),
        "System.Console.WriteLine(\"ok\");\n",
    )
    .unwrap();
    std::fs::write(
        project.join("composer.json"),
        "{\"name\":\"example/native-fixture\",\"require\":{}}\n",
    )
    .unwrap();
}

fn operations(
    name: &str,
    tool: &Tool,
    root: &Path,
    env: &BTreeMap<String, String>,
    policy: Option<&nub_sandbox::SandboxPolicy>,
) {
    match name {
        "cargo" => {
            assert_ok(
                tool,
                "cold build",
                run(tool, &["build", "--offline"], root, env, policy),
            );
            assert_ok(
                tool,
                "warm build",
                run(tool, &["build", "--offline"], root, env, policy),
            );
            assert_ok(
                tool,
                "target cleanup",
                run(tool, &["clean"], root, env, policy),
            );
        }
        "rustup" => assert_ok(
            tool,
            "home query",
            run(tool, &["show", "home"], root, env, policy),
        ),
        "go" => {
            assert_ok(
                tool,
                "user configuration write",
                run(
                    tool,
                    &["env", "-w", "GONOPROXY=example.invalid"],
                    root,
                    env,
                    policy,
                ),
            );
            let config = run(tool, &["env", "GONOPROXY"], root, env, policy);
            assert!(config.status.success());
            assert_eq!(
                String::from_utf8_lossy(&config.stdout).trim(),
                "example.invalid"
            );
            assert_ok(tool, "cold build", run(tool, &["build"], root, env, policy));
            assert_ok(tool, "warm build", run(tool, &["build"], root, env, policy));
            assert_ok(
                tool,
                "user install",
                run(tool, &["install"], root, env, policy),
            );
            assert_ok(
                tool,
                "cache cleanup",
                run(tool, &["clean", "-cache"], root, env, policy),
            );
        }
        "gradle" => {
            assert_ok(
                tool,
                "cold task",
                run(
                    tool,
                    &["--offline", "--no-daemon", "fixture"],
                    root,
                    env,
                    policy,
                ),
            );
            assert_ok(
                tool,
                "warm task",
                run(
                    tool,
                    &["--offline", "--no-daemon", "fixture"],
                    root,
                    env,
                    policy,
                ),
            );
            assert_ok(
                tool,
                "daemon cleanup",
                run(tool, &["--stop"], root, env, policy),
            );
        }
        "maven" => {
            assert_ok(
                tool,
                "cold validate",
                run(tool, &["-o", "validate"], root, env, policy),
            );
            let marker = root.join("project/mavenrc-loaded");
            assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "loaded");
            std::fs::remove_file(&marker).unwrap();
            assert_ok(
                tool,
                "warm clean",
                run(tool, &["-o", "clean"], root, env, policy),
            );
            assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "loaded");
        }
        "nuget" => {
            let selected = run(tool, &["--version"], root, env, policy);
            let version = String::from_utf8_lossy(&selected.stdout).trim().to_owned();
            assert_ok(tool, "SDK selection", selected);
            assert_eq!(version, "10.0.100", "fixture must use its pinned SDK");
            eprintln!("NATIVE TOOL SELECTED SDK {version}");
            assert_ok(
                tool,
                "offline restore",
                run(
                    tool,
                    &["restore", "--ignore-failed-sources"],
                    root,
                    env,
                    policy,
                ),
            );
            assert_ok(
                tool,
                "build",
                run(tool, &["build", "--no-restore"], root, env, policy),
            );
            assert_ok(
                tool,
                "cache cleanup",
                run(
                    tool,
                    &["nuget", "locals", "all", "--clear"],
                    root,
                    env,
                    policy,
                ),
            );
        }
        "composer" => {
            #[cfg(windows)]
            if std::env::var_os("NUB_NATIVE_ADAPTER_PROBE_DIR").is_some() {
                // Symfony suppresses proc_open warnings; retain the underlying PHP error.
                let diagnostic = root.join("project/proc-open.php");
                std::fs::write(&diagnostic, r#"<?php
foreach (['pipes', 'nul', 'files'] as $mode) {
    $descriptors = [['pipe', 'r'], ['pipe', 'w'], ['pipe', 'w']];
    if ($mode === 'nul') $descriptors = [['pipe', 'r'], ['file', 'NUL', 'w'], ['file', 'NUL', 'w']];
    if ($mode === 'files') $descriptors = [['pipe', 'r'], ['file', 'proc-out', 'w'], ['file', 'proc-err', 'w']];
    error_clear_last();
    $process = proc_open([PHP_BINARY, '-r', 'echo 42;'], $descriptors, $pipes);
    echo json_encode(['mode' => $mode, 'created' => is_resource($process), 'error' => error_get_last()]), PHP_EOL;
    if (is_resource($process)) {
        foreach ($pipes as $pipe) fclose($pipe);
        echo 'EXIT ', proc_close($process), PHP_EOL;
    }
}
"#).unwrap();
                let php = Tool {
                    name: "php-proc-open".into(),
                    program: tool.program.clone(),
                    prefix: vec![],
                    version: tool.version.clone(),
                    tool_root: tool.tool_root.clone(),
                    runtime_roots: tool.runtime_roots.clone(),
                    tool_env: tool.tool_env.clone(),
                    shell: false,
                    maven_seed: None,
                };
                let output = run(&php, &[diagnostic.to_str().unwrap()], root, env, policy);
                eprintln!(
                    "PHP_PROC_OPEN status={} stdout={} stderr={}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            assert_ok(
                tool,
                "cold install",
                run(
                    tool,
                    &[
                        "install",
                        "--no-interaction",
                        "--no-plugins",
                        "--no-scripts",
                    ],
                    root,
                    env,
                    policy,
                ),
            );
            assert_ok(
                tool,
                "warm install",
                run(
                    tool,
                    &[
                        "install",
                        "--no-interaction",
                        "--no-plugins",
                        "--no-scripts",
                    ],
                    root,
                    env,
                    policy,
                ),
            );
            assert_ok(
                tool,
                "cache cleanup",
                run(
                    tool,
                    &["clear-cache", "--no-interaction"],
                    root,
                    env,
                    policy,
                ),
            );
        }
        _ => unreachable!("matrix tool name"),
    }
}

fn run_case(name: &str, tooldirs: Option<bool>) {
    let tool = tool(name);
    eprintln!("NATIVE TOOL {} {}", tool.name, tool.version);
    let root = fixture();
    let env = env_for(root.path(), &tool);
    write_projects(root.path());
    let policy = tooldirs.map(|value| policy(root.path(), &tool, env.clone(), value));
    #[cfg(windows)]
    if let Some(policy) = &policy {
        let secret = root.path().join("withheld");
        std::fs::write(&secret, "WITHHELD").unwrap();
        let sandbox = tool_sandbox::acquire(policy).unwrap();
        let output = tool_output::output(
            sandbox
                .prepare(
                    CommandSpec::new(std::env::var_os("COMSPEC").unwrap())
                        .args(["/d", "/c", "type", secret.to_str().unwrap()])
                        .cwd(root.path().join("project"))
                        .redact_stdout(true)
                        .redact_stderr(true),
                )
                .unwrap(),
        );
        assert!(!output.status.success(), "canary exposed: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Access is denied"),
            "{output:?}"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("WITHHELD"));
        eprintln!("NATIVE_CANARY_DENIED {output:?}");
    }
    #[cfg(windows)]
    if name == "cargo" && std::env::var_os("NUB_SANDBOX_TOOL_MSYS_ROOT").is_some() {
        msys_execution_count(root.path(), &env, tooldirs);
    }
    operations(name, &tool, root.path(), &env, policy.as_ref());
}

#[cfg(windows)]
#[test]
#[ignore = "child process for the MSYS execution-count control"]
fn msys_execution_count_child() {
    let root =
        std::env::var_os("SANDBOX_MSYS_EXECUTION_COUNT").expect("execution-count child marker");
    std::fs::write(
        Path::new(&root).join(format!("{}.txt", std::process::id())),
        "one native execution",
    )
    .unwrap();
}

#[cfg(windows)]
fn msys_execution_count(root: &Path, env: &BTreeMap<String, String>, tooldirs: Option<bool>) {
    let binary = std::env::current_exe().unwrap();
    let tool = Tool {
        name: "MSYS execution-count control".into(),
        tool_root: binary.parent().unwrap().to_owned(),
        program: binary,
        prefix: vec![
            "--exact".into(),
            "native_tool_functionality_probe::msys_execution_count_child".into(),
            "--ignored".into(),
            "--nocapture".into(),
        ],
        version: "current test binary".into(),
        runtime_roots: vec![],
        tool_env: BTreeMap::new(),
        shell: false,
        maven_seed: None,
    };
    for sample in 0..3 {
        let count_root = root.join(format!("project/exec-count-{sample}"));
        std::fs::create_dir(&count_root).unwrap();
        let mut env = env.clone();
        env.insert(
            "SANDBOX_MSYS_EXECUTION_COUNT".into(),
            count_root.to_string_lossy().into_owned(),
        );
        let policy = tooldirs.map(|value| policy(root, &tool, env.clone(), value));
        let output = run(&tool, &[], root, &env, policy.as_ref());
        let executions: Vec<_> = std::fs::read_dir(&count_root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        eprintln!(
            "MSYS_EXECUTION_COUNT sample={sample} executions={executions:?} output={output:?}"
        );
        assert_ok(&tool, "single execution", output);
        assert_eq!(
            executions.len(),
            1,
            "MSYS executed a command more than once: {executions:?}"
        );
    }
}

#[cfg(unix)]
fn run_nuget_self_proc(tooldirs: bool) {
    let tool = tool("nuget");
    let root = fixture();
    let env = env_for(root.path(), &tool);
    write_projects(root.path());
    let mut policy = policy(root.path(), &tool, env, tooldirs);
    if !tooldirs {
        #[cfg(target_os = "linux")]
        policy.fs.self_proc.extend([
            nub_sandbox::policy::SelfProcFile::Maps,
            nub_sandbox::policy::SelfProcFile::Stat,
            nub_sandbox::policy::SelfProcFile::Cmdline,
            nub_sandbox::policy::SelfProcFile::Status,
            nub_sandbox::policy::SelfProcFile::Task,
            nub_sandbox::policy::SelfProcFile::TaskStat,
            nub_sandbox::policy::SelfProcFile::TaskStatus,
        ]);
        // Named .NET mutexes use this shared path even with a private TMPDIR.
        // The exact-grant control spells out what the tool bundle supplies.
        std::fs::create_dir_all("/tmp/.dotnet/shm").unwrap();
        let ctx = CompileCtx::new(
            Homes {
                home: root.path().join("home"),
                cache: root.path().join("cache"),
                tmp: root.path().join("tmp"),
                project: root.path().join("project"),
            },
            root.path().join("project"),
            ScopeCapabilities::approved(),
            BTreeMap::new(),
        );
        let shared = compile(&json!({"fs": {"/tmp/.dotnet/shm": "rw"}}), &ctx).unwrap();
        policy.fs.rules.entries.extend(shared.fs.rules.entries);
    }
    let sandbox = tool_sandbox::acquire(&policy).unwrap();
    for tail in [
        &["--version"][..],
        &["restore", "--ignore-failed-sources"][..],
        &["build", "--no-restore"][..],
        &["nuget", "locals", "all", "--clear"][..],
        &["restore", "--ignore-failed-sources"][..],
    ] {
        let argv = tool
            .prefix
            .iter()
            .cloned()
            .chain(tail.iter().map(|arg| (*arg).to_owned()));
        let prepared = sandbox
            .prepare(
                CommandSpec::new(&tool.program)
                    .args(argv)
                    .cwd(root.path().join("project"))
                    .redact_stdout(true)
                    .redact_stderr(true),
            )
            .unwrap();
        assert!(
            prepared.degradation.lost.is_empty(),
            "{:?}",
            prepared.degradation
        );
        let output = tool_output::output(prepared);
        let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        assert_ok(&tool, &format!("self-metadata {tail:?}"), output);
        if tail == ["--version"] {
            assert_eq!(version, "10.0.100");
        }
    }
    let secret = root.path().join("secret");
    std::fs::write(&secret, "WITHHELD").unwrap();
    let script = format!(
        "for p in '{}' /proc/self/environ /proc/{}/environ; do if cat \"$p\" >/dev/null; then exit 91; fi; done",
        secret.display(),
        std::process::id()
    );
    let output = sandbox
        .prepare(
            CommandSpec::new("/bin/sh")
                .args(["-c", &script])
                .cwd(root.path().join("project")),
        )
        .unwrap()
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("WITHHELD"));
    sandbox.close();
    nub_sandbox::cleanup().unwrap();
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires pinned .NET SDK"]
fn linux_self_proc_nuget_exact() {
    run_nuget_self_proc(false);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires pinned .NET SDK"]
fn linux_self_proc_nuget_tooldirs() {
    run_nuget_self_proc(true);
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires pinned .NET SDK"]
fn macos_self_proc_nuget_tooldirs() {
    run_nuget_self_proc(true);
}

#[cfg(windows)]
#[test]
#[ignore = "requires pinned Gradle; diagnoses the explicitly acknowledged net-full limit"]
fn windows_gradle_with_limited_network() {
    let tool = tool("gradle");
    let root = fixture();
    let env = env_for(root.path(), &tool);
    write_projects(root.path());
    let policy = policy(root.path(), &tool, env.clone(), true);
    let sandbox = tool_sandbox::acquire(&policy).expect("limited-network sandbox acquires");
    for tail in [
        &["--offline", "--no-daemon", "--stacktrace", "fixture"][..],
        &["--offline", "--no-daemon", "--stacktrace", "fixture"][..],
        &["--stop"][..],
    ] {
        let args: Vec<_> = tool
            .prefix
            .iter()
            .cloned()
            .chain(tail.iter().map(|arg| (*arg).to_owned()))
            .collect();
        let spec = CommandSpec::new(std::env::var_os("COMSPEC").unwrap())
            .verbatim_command_line(format!(
                "/d /s /c \"{}\"",
                command_line(&tool.program, &args)
            ))
            .cwd(root.path().join("project"))
            .redact_stdout(true)
            .redact_stderr(true);
        let prepared = sandbox
            .prepare(spec)
            .expect("limited-network command prepares");
        // This diagnostic explicitly accepts a narrower network capability than
        // net:true requested. The strict raw fixture above still refuses it.
        assert_eq!(prepared.degradation.lost, vec!["net-full".to_owned()]);
        eprintln!("GRADLE_LIMITED_NETWORK {:?} {tail:?}", prepared.degradation);
        let output = tool_output::output(prepared);
        assert_ok(&tool, "limited-network execution", output);
    }
    sandbox.close();
    nub_sandbox::cleanup().expect("limited-network resources are reclaimed");
}

fn cargo_project_target(tooldirs: Option<bool>) {
    let tool = tool("cargo");
    eprintln!(
        "NATIVE TOOL {} {} default project target",
        tool.name, tool.version
    );
    let root = fixture();
    let mut env = env_for(root.path(), &tool);
    env.remove("CARGO_TARGET_DIR");
    write_projects(root.path());
    let policy = tooldirs.map(|value| policy(root.path(), &tool, env.clone(), value));
    operations("cargo", &tool, root.path(), &env, policy.as_ref());
    assert!(!root.path().join("project/target").exists());
}

#[test]
#[ignore = "requires the pinned native tool matrix"]
fn cargo_default_target_unconfined() {
    cargo_project_target(None);
}

#[test]
#[ignore = "requires the pinned native tool matrix"]
fn cargo_default_target_exact() {
    cargo_project_target(Some(false));
}

#[test]
#[ignore = "requires the pinned native tool matrix"]
fn cargo_default_target_tooldirs() {
    cargo_project_target(Some(true));
}

macro_rules! tool_cases {
    ($name:literal, $u:ident, $e:ident, $d:ident) => {
        #[test]
        #[ignore = "requires the pinned native tool matrix"]
        fn $u() {
            run_case($name, None);
        }
        #[test]
        #[ignore = "requires the pinned native tool matrix"]
        fn $e() {
            run_case($name, Some(false));
        }
        #[test]
        #[ignore = "requires the pinned native tool matrix"]
        fn $d() {
            run_case($name, Some(true));
        }
    };
}
tool_cases!("cargo", cargo_unconfined, cargo_exact, cargo_tooldirs);
tool_cases!("rustup", rustup_unconfined, rustup_exact, rustup_tooldirs);
tool_cases!("go", go_unconfined, go_exact, go_tooldirs);
tool_cases!("gradle", gradle_unconfined, gradle_exact, gradle_tooldirs);
tool_cases!("maven", maven_unconfined, maven_exact, maven_tooldirs);
tool_cases!("nuget", nuget_unconfined, nuget_exact, nuget_tooldirs);
tool_cases!(
    "composer",
    composer_unconfined,
    composer_exact,
    composer_tooldirs
);

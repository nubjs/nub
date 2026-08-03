//! End-to-end coverage for handing the environment to an external loader.
//!
//! The loader here is a STUB executable, not real varlock. What nub owes is a
//! contract — stop loading `.env*`, put the loader in front of Node, hand it the
//! node command unchanged, and do not wrap twice — and a stub exercises all of it
//! hermetically, with no network and no coupling to a loader version. Whether the
//! loader resolves a graph correctly is its own test suite's job.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

/// A project that reports what reached the child.
fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        "probe.mjs",
        r#"console.log(JSON.stringify({
            FROM_DOTENV: process.env.FROM_DOTENV ?? null,
            FROM_LOADER: process.env.FROM_LOADER ?? null,
            SAW_NODE: process.env.LOADER_SAW_NODE ?? null,
            LOADER_PATH: process.env.LOADER_PATH || null,
            SAW_PRELOAD: /preload\.(c|m)js/.test(process.env.NODE_OPTIONS ?? "") ? "yes" : "no",
        }));"#,
    );
    write(
        dir.path(),
        "package.json",
        r#"{"name":"fx","version":"1.0.0"}"#,
    );
    for (path, contents) in files {
        write(dir.path(), path, contents);
    }
    dir
}

fn write(root: &Path, path: &str, contents: &str) {
    let full = root.join(path);
    std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
    std::fs::write(full, contents).expect("write");
}

/// A stand-in for the loader CLI, accepting the one shape nub uses:
/// `<loader> run -- <command…>`.
///
/// It records that it ran, notes whether the thing it was handed was Node, then
/// spawns that command — which is what `varlock run` does around its own
/// resolution and stream piping.
///
/// The `#!/usr/bin/env node` shebang is deliberate and load-bearing, not
/// incidental: it is what varlock's real bin carries, and it means the loader's
/// OWN interpreter resolves `node` through nub's PATH shim and re-enters nub.
/// A `#!/bin/sh` stub would run green against a nub that had lost its
/// re-entrancy guard, because the recursion channel would never open.
#[cfg(unix)]
fn install_stub_loader(root: &Path, tally: &Path) {
    let bin = root.join("node_modules/.bin/varlock");
    std::fs::create_dir_all(bin.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &bin,
        format!(
            r#"#!/usr/bin/env node
const {{ appendFileSync }} = require("node:fs");
const {{ spawnSync }} = require("node:child_process");
appendFileSync({tally:?}, "ran\n");
const argv = process.argv.slice(2);
if (argv[0] !== "run") {{
  console.error(`stub loader: expected 'run', got '${{argv[0]}}'`);
  process.exit(64);
}}
const sep = argv.indexOf("--");
if (sep < 0) {{
  console.error("stub loader: no '--' separator");
  process.exit(64);
}}
const flags = argv.slice(1, sep);
const cmd = argv.slice(sep + 1);
const pathIdx = flags.indexOf("--path");
const res = spawnSync(cmd[0], cmd.slice(1), {{
  stdio: "inherit",
  env: {{
    ...process.env,
    FROM_LOADER: "yes",
    LOADER_SAW_NODE: /node/.test(cmd[0]) ? "yes" : "no",
    LOADER_PATH: pathIdx < 0 ? "" : flags[pathIdx + 1],
  }},
}});
process.exit(res.status ?? 1);
"#,
            tally = tally.to_string_lossy()
        ),
    )
    .expect("write stub");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

struct Run {
    stdout: String,
    stderr: String,
}

impl Run {
    fn var(&self, key: &str) -> Option<String> {
        let value: serde_json::Value = serde_json::from_str(self.stdout.trim())
            .unwrap_or_else(|err| panic!("probe stdout was not JSON ({err}): {}", self.stdout));
        value.get(key).and_then(|v| v.as_str()).map(str::to_string)
    }
}

/// `PATH` is load-bearing for detection — the loader lookup falls back to it — so
/// every run gets a controlled one. `node` must stay reachable for the child.
fn run(dir: &Path) -> Run {
    run_args(dir, &["probe.mjs"])
}

fn run_args(dir: &Path, args: &[&str]) -> Run {
    let node_dir = which_node_dir();
    let output = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env("PATH", &node_dir)
        .env_remove("APP_ENV")
        .env_remove("NODE_ENV")
        .env_remove("NODE_OPTIONS")
        .output()
        .expect("spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "nub exited {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    Run { stdout, stderr }
}

fn which_node_dir() -> PathBuf {
    let out = Command::new("sh")
        .args(["-c", "command -v node"])
        .output()
        .expect("locate node");
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string())
        .parent()
        .expect("node has a parent dir")
        .to_path_buf()
}

#[test]
fn without_a_schema_nub_loads_env_files_as_before() {
    let dir = project(&[(".env", "FROM_DOTENV=yes\n")]);
    let run = run(dir.path());
    assert_eq!(
        run.var("FROM_DOTENV").as_deref(),
        Some("yes"),
        "a project with no schema must keep nub's own .env loading"
    );
}

#[cfg(unix)]
#[test]
fn a_schema_with_the_loader_puts_it_in_front_of_node() {
    let dir = project(&[
        (".env", "FROM_DOTENV=leaked\n"),
        (".env.schema", "# ---\nA=1\n"),
    ]);
    let tally = dir.path().join("tally");
    install_stub_loader(dir.path(), &tally);
    let run = run(dir.path());

    assert_eq!(
        run.var("FROM_LOADER").as_deref(),
        Some("yes"),
        "the loader must be the thing that launched Node. stderr: {}",
        run.stderr
    );
    assert_eq!(
        run.var("SAW_NODE").as_deref(),
        Some("yes"),
        "the loader must be handed the NODE command, not something else. stderr: {}",
        run.stderr
    );
    assert_eq!(
        run.var("FROM_DOTENV"),
        None,
        "nub must contribute no environment of its own — the loader owns it, and \
         nub's values would layer over the loader's. stderr: {}",
        run.stderr
    );
}

#[cfg(unix)]
#[test]
fn a_workspace_member_points_the_loader_at_the_root_schema() {
    // The loader infers its entry point from cwd, which for a member is a
    // directory with no schema in it — so it fails with "no .env files found" at
    // a schema nub demonstrably found. nub walked up to decide the loader owns
    // this project at all, so it hands over the directory it walked to.
    let dir = project(&[
        (
            "package.json",
            r#"{"name":"ws","version":"1.0.0","workspaces":["pkgs/*"]}"#,
        ),
        (".env.schema", "# ---\nA=1\n"),
        ("pkgs/web/package.json", r#"{"name":"web"}"#),
    ]);
    let tally = dir.path().join("tally");
    install_stub_loader(dir.path(), &tally);
    std::fs::copy(
        dir.path().join("probe.mjs"),
        dir.path().join("pkgs/web/probe.mjs"),
    )
    .expect("probe into the member");

    let run = run(&dir.path().join("pkgs/web"));
    let handed = run
        .var("LOADER_PATH")
        .expect("the loader must be given a --path");
    assert_eq!(
        std::fs::canonicalize(&handed).expect("canonicalize --path"),
        std::fs::canonicalize(dir.path()).expect("canonicalize root"),
        "a member must point the loader at the workspace root, where the schema \
         is. stderr: {}",
        run.stderr
    );
}

#[cfg(unix)]
#[test]
fn nub_watch_also_puts_the_loader_in_front_of_node() {
    // `nub watch` assembles its own `node --watch` command instead of going
    // through `spawn_node`, so it does not inherit the wrap — but detection has
    // already stood nub's own cascade down by the time it spawns. That
    // combination has now produced a silent, total loss of environment twice:
    // every variable arrives `undefined`, with no diagnostic anywhere.
    let dir = project(&[(".env.schema", "# ---\nA=1\n")]);
    let tally = dir.path().join("tally");
    install_stub_loader(dir.path(), &tally);

    let mut child = Command::new(nub_binary())
        .args(["watch", "probe.mjs"])
        .current_dir(dir.path())
        .env("PATH", which_node_dir())
        .env_remove("NODE_OPTIONS")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn nub watch");

    // The watcher never exits on its own, so read the first program run and kill.
    let stdout = child.stdout.take().expect("piped stdout");
    let first_line = std::thread::spawn(move || {
        use std::io::BufRead;
        std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .find(|line| line.contains("FROM_LOADER"))
    });
    let line = wait_for(first_line, std::time::Duration::from_secs(60));
    let _ = child.kill();
    let _ = child.wait();

    let line = line.expect("nub watch printed no probe output within 60s");
    assert!(
        line.contains(r#""FROM_LOADER":"yes""#),
        "nub watch must run Node behind the loader; got: {line}"
    );
}

/// Join a reader thread with a deadline, so a hung watcher fails the test rather
/// than the suite.
#[cfg(unix)]
fn wait_for<T: Send + 'static>(
    handle: std::thread::JoinHandle<Option<T>>,
    timeout: std::time::Duration,
) -> Option<T> {
    let deadline = std::time::Instant::now() + timeout;
    while !handle.is_finished() {
        if std::time::Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    handle.join().ok().flatten()
}

#[cfg(unix)]
#[test]
fn the_node_command_reaches_the_loader_intact() {
    // nub's augmentation has to survive being handed through the loader, or
    // TypeScript and the module hooks quietly stop working behind a schema.
    let dir = project(&[(".env.schema", "# ---\nA=1\n")]);
    let tally = dir.path().join("tally");
    install_stub_loader(dir.path(), &tally);
    let run = run(dir.path());
    assert_eq!(
        run.var("SAW_PRELOAD").as_deref(),
        Some("yes"),
        "nub's preload must still reach Node through the loader — without it the \
         transpiler is gone. stderr: {}",
        run.stderr
    );
}

#[cfg(unix)]
#[test]
fn the_loader_is_invoked_exactly_once() {
    // The loader's real bin is a `#!/usr/bin/env node` script, so its own
    // interpreter resolves through nub's PATH shim and re-enters nub. Without the
    // wrapped-marker that nub detects the same project and wraps again, forever.
    let dir = project(&[(".env.schema", "# ---\nA=1\n")]);
    let tally = dir.path().join("tally");
    install_stub_loader(dir.path(), &tally);
    let _ = run(dir.path());
    let runs = std::fs::read_to_string(&tally)
        .unwrap_or_default()
        .lines()
        .count();
    assert_eq!(
        runs, 1,
        "the loader must be invoked exactly once; {runs} means nub wrapped a \
         command that was already behind the loader"
    );
}

#[cfg(unix)]
#[test]
fn a_nested_nub_behind_the_loader_does_not_load_env_files() {
    // Inside the wrap the values are already present. A nested nub that reloaded
    // `.env*` would layer its own answer over the loader's.
    let dir = project(&[
        (".env", "FROM_DOTENV=leaked\n"),
        (".env.schema", "# ---\nA=1\n"),
    ]);
    let tally = dir.path().join("tally");
    install_stub_loader(dir.path(), &tally);
    let node_dir = which_node_dir();
    let output = Command::new(nub_binary())
        .arg("probe.mjs")
        .current_dir(dir.path())
        .env("PATH", &node_dir)
        .env("__NUB_ENV_OWNER_WRAPPED", "1")
        .env_remove("NODE_OPTIONS")
        .output()
        .expect("spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let run = Run {
        stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    assert_eq!(
        run.var("FROM_DOTENV"),
        None,
        "a nub already behind the loader must stay stood down. stderr: {}",
        run.stderr
    );
    assert!(
        !tally.exists(),
        "and must not invoke the loader a second time"
    );
}

#[test]
fn a_schema_without_the_loader_keeps_loading_and_warns() {
    // A schema with nothing to read it is not evidence that anything owns this
    // project, so standing down would leave the child with no environment at all.
    let dir = project(&[
        (".env", "FROM_DOTENV=yes\n"),
        (".env.schema", "# ---\nA=1\n"),
    ]);
    let run = run(dir.path());
    assert_eq!(
        run.var("FROM_DOTENV").as_deref(),
        Some("yes"),
        "with no loader installed nub must keep loading. stderr: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("nub add -D varlock"),
        "the user must be told the schema was not applied, and how to fix it — \
         as a DEV dependency, which is what the loader's own docs prescribe; \
         stderr was: {}",
        run.stderr
    );
}

#[test]
fn a_project_using_a_rival_schema_tool_is_not_warned_at() {
    // `dotenv-extended` claims this filename too. A project that declares it has
    // a tool applying its schema already, so telling it the schema "was not
    // applied" is false, and recommending another package is noise. The file
    // sniff cannot settle this one: the schema here IS @env-spec shaped.
    let dir = project(&[
        (
            "package.json",
            r#"{"name":"f","version":"1.0.0","devDependencies":{"dotenv-extended":"^2.9.0"}}"#,
        ),
        (".env", "FROM_DOTENV=yes\n"),
        (".env.schema", "# ---\nA=1\n"),
    ]);
    let run = run(dir.path());
    assert!(
        !run.stderr.contains("varlock"),
        "a project that declares a rival schema tool must not be advertised at; \
         stderr was: {}",
        run.stderr
    );
    assert_eq!(
        run.var("FROM_DOTENV").as_deref(),
        Some("yes"),
        "and nub must still load its own .env files. stderr: {}",
        run.stderr
    );
}

#[test]
fn a_foreign_env_schema_is_left_alone_silently() {
    // `.env.schema` is not this format's name — dotenv-extended has defaulted to
    // it since 2016 for an incompatible format. Warning on the filename told those
    // projects their schema "was not applied" while another tool was applying it.
    let dir = project(&[
        (".env", "FROM_DOTENV=yes\n"),
        (".env.schema", "# Server\nPORT=\nAPI_URL=\n"),
    ]);
    let run = run(dir.path());
    assert_eq!(
        run.var("FROM_DOTENV").as_deref(),
        Some("yes"),
        "a foreign schema must not disturb nub's own loading. stderr: {}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("varlock"),
        "nub must not name another tool at a project that never asked for it; \
         stderr was: {}",
        run.stderr
    );
}

#[cfg(unix)]
#[test]
fn an_explicit_env_file_setting_overrides_the_stand_down() {
    // Explicit beats inferred: a user who spells out `envFile` has asked for those
    // files regardless of what a schema implies.
    let dir = project(&[
        (".env.schema", "# ---\nA=1\n"),
        ("custom.env", "FROM_DOTENV=explicit\n"),
        ("nub.jsonc", r#"{ "envFile": "custom.env" }"#),
    ]);
    let tally = dir.path().join("tally");
    install_stub_loader(dir.path(), &tally);
    let run = run(dir.path());
    assert_eq!(
        run.var("FROM_DOTENV").as_deref(),
        Some("explicit"),
        "an explicit envFile must still load. stderr: {}",
        run.stderr
    );
}

#[cfg(unix)]
#[test]
fn compat_mode_does_no_owner_handling_at_all() {
    // `--node` is vanilla Node: no augmentation, and nothing put in front of it.
    let dir = project(&[(".env.schema", "# ---\nA=1\n")]);
    let tally = dir.path().join("tally");
    install_stub_loader(dir.path(), &tally);
    let run = run_args(dir.path(), &["--node", "probe.mjs"]);
    assert_eq!(
        run.var("FROM_LOADER"),
        None,
        "compat mode must not put the loader in front of Node. stderr: {}",
        run.stderr
    );
    assert!(
        !tally.exists(),
        "and must not invoke the loader at all in compat mode"
    );
}

//! End-to-end coverage for standing down when an external loader owns env.
//!
//! The loader here is a STUB package, not real varlock. What nub owes is a
//! contract — stop loading `.env*`, hand the child a marker and the adapter, warn
//! when nothing loaded — and a stub exercises all of it hermetically, with no
//! network, no install, and no coupling to a loader version. Whether varlock
//! itself resolves a graph correctly is varlock's test suite, not nub's.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

/// A project that prints the variables under test as JSON.
fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let probe = r#"console.log(JSON.stringify({
        FROM_DOTENV: process.env.FROM_DOTENV ?? null,
        FROM_LOADER: process.env.FROM_LOADER ?? null,
        FROM_AUTO_LOAD: process.env.FROM_AUTO_LOAD ?? null,
    }));"#;
    write(dir.path(), "probe.mjs", probe);
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

/// A minimal stand-in for the loader package: sets a value and the sentinel the
/// verification pass looks for, exactly as a real loader would.
fn install_stub_loader(root: &Path) {
    // Mirrors the real package's shape: a root export that resolves the graph, and
    // a separate `auto-load` entry that applies the protections. nub primes the
    // first and hands off to the second, so a stub missing either would not
    // exercise the adapter.
    write(
        root,
        "node_modules/varlock/package.json",
        r#"{"name":"varlock","version":"0.0.0","type":"module",
            "exports":{".":"./index.js","./auto-load":"./auto-load.js"}}"#,
    );
    write(
        root,
        "node_modules/varlock/index.js",
        r#"export async function load() {
             process.env.FROM_LOADER = "yes";
             process.env.__VARLOCK_ENV = "{}";
           }
           export function patchGlobalConsole() {}"#,
    );
    write(
        root,
        "node_modules/varlock/auto-load.js",
        r#"// The real entry reuses an already-populated blob and installs its guards;
           // the stub records that nub handed off to it at all.
           process.env.FROM_AUTO_LOAD = "yes";"#,
    );
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

fn run(dir: &Path) -> Run {
    run_with_path(dir, None)
}

/// `PATH` is load-bearing for detection: `find_loader_cli` falls back to it, so a
/// developer machine with varlock installed would turn a `Missing` fixture into a
/// `Cli` one and fail. Every run therefore gets a CONTROLLED `PATH` — empty by
/// default, or exactly the directory a test wants probed.
fn run_with_path(dir: &Path, path: Option<&Path>) -> Run {
    let mut command = Command::new(nub_binary());
    command
        .arg("probe.mjs")
        .current_dir(dir)
        .env_remove("APP_ENV")
        .env_remove("NODE_ENV")
        .env_remove("NODE_OPTIONS");
    match path {
        Some(dir) => command.env("PATH", dir),
        None => command.env("PATH", ""),
    };
    let output = command.output().expect("spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "nub exited {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    Run { stdout, stderr }
}

#[test]
fn without_a_schema_nub_loads_env_files_as_before() {
    let dir = project(&[(".env", "FROM_DOTENV=yes\n")]);
    let run = run(dir.path());
    assert_eq!(
        run.var("FROM_DOTENV").as_deref(),
        Some("yes"),
        "a project with no .env.schema must keep nub's own .env loading"
    );
}

#[test]
fn a_schema_plus_loader_makes_nub_stand_down() {
    let dir = project(&[
        (".env", "FROM_DOTENV=leaked\n"),
        (".env.schema", "# ---\nA=1\n"),
    ]);
    install_stub_loader(dir.path());
    let run = run(dir.path());
    assert_eq!(
        run.var("FROM_DOTENV"),
        None,
        "with a loader owning env, nub must NOT inject its own .env values — \
         they would override what the loader resolved. stderr: {}",
        run.stderr
    );
    assert_eq!(
        run.var("FROM_LOADER").as_deref(),
        Some("yes"),
        "the loader adapter must run and populate the environment. stderr: {}",
        run.stderr
    );
    assert_eq!(
        run.var("FROM_AUTO_LOAD").as_deref(),
        Some("yes"),
        "nub must hand off to the loader's own unified entry rather than stopping \
         at the graph — that entry is what installs the protections and applies \
         schema settings nub does not model. stderr: {}",
        run.stderr
    );
}

#[test]
fn a_schema_with_no_loader_keeps_loading_and_warns() {
    // `.env.schema` may be committed for an editor extension or another tool, so
    // its mere presence must not leave the process with no environment at all.
    let dir = project(&[
        (".env", "FROM_DOTENV=yes\n"),
        (".env.schema", "# ---\nA=1\n"),
    ]);
    let run = run(dir.path());
    assert_eq!(
        run.var("FROM_DOTENV").as_deref(),
        Some("yes"),
        "with no loader installed, nub must keep loading .env rather than \
         standing down into an empty environment. stderr: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains(".env.schema"),
        "the user must be told the schema was not applied; stderr was: {}",
        run.stderr
    );
}

#[test]
fn the_verification_pass_warns_when_nothing_loaded() {
    // The loader package resolves but never sets the sentinel — the shape of a
    // loader that failed early, or one invoked where it could not see the schema.
    let dir = project(&[(".env.schema", "# ---\nA=1\n")]);
    write(
        dir.path(),
        "node_modules/varlock/package.json",
        r#"{"name":"varlock","version":"0.0.0","type":"module",
            "exports":{".":"./index.js","./auto-load":"./auto-load.js"}}"#,
    );
    write(
        dir.path(),
        "node_modules/varlock/index.js",
        "export async function load() {}\nexport function patchGlobalConsole() {}",
    );
    write(dir.path(), "node_modules/varlock/auto-load.js", "");
    let run = run(dir.path());
    assert!(
        run.stderr.contains("never loaded the environment"),
        "a loader that ran but applied nothing must produce a warning, \
         not silence; stderr was: {}",
        run.stderr
    );
}

#[test]
fn an_explicit_env_file_setting_overrides_the_stand_down() {
    // Explicit beats inferred: a user who spells out `envFile` in nub.jsonc has
    // asked for those files regardless of what a schema implies.
    let dir = project(&[
        (".env.schema", "# ---\nA=1\n"),
        ("custom.env", "FROM_DOTENV=explicit\n"),
        ("nub.jsonc", r#"{ "envFile": "custom.env" }"#),
    ]);
    install_stub_loader(dir.path());
    let run = run(dir.path());
    assert_eq!(
        run.var("FROM_DOTENV").as_deref(),
        Some("explicit"),
        "an explicit envFile must still load even when a loader owns discovery. \
         stderr: {}",
        run.stderr
    );
}

/// An executable loader stub with NO package directory, so detection lands on the
/// CLI path. Emits a fixed `json-full` payload, which is all nub parses.
#[cfg(unix)]
fn install_stub_cli(root: &Path, dir: &str) -> PathBuf {
    let bin = root.join(dir).join("varlock");
    std::fs::create_dir_all(bin.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &bin,
        "#!/bin/sh\nprintf '%s' '{\"config\":{\"FROM_LOADER\":{\"value\":\"yes\",\
         \"isSensitive\":false}}}'\n",
    )
    .expect("write stub");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    bin
}

#[cfg(unix)]
#[test]
fn a_cli_only_loader_resolves_the_graph_and_suppresses_env_files() {
    // The whole CLI branch had no end-to-end coverage: every other fixture writes
    // a package directory and therefore lands on the in-process path.
    let dir = project(&[
        (".env", "FROM_DOTENV=leaked\n"),
        (".env.schema", "# ---\nA=1\n"),
    ]);
    install_stub_cli(dir.path(), "node_modules/.bin");
    let run = run(dir.path());
    assert_eq!(
        run.var("FROM_LOADER").as_deref(),
        Some("yes"),
        "nub must inject the values the loader CLI reported. stderr: {}",
        run.stderr
    );
    assert_eq!(
        run.var("FROM_DOTENV"),
        None,
        "the CLI path must still suppress nub's own .env cascade. stderr: {}",
        run.stderr
    );
}

#[cfg(unix)]
#[test]
fn a_cli_only_loader_is_found_on_path_not_just_in_node_modules() {
    // A Homebrew or curl install lands on PATH with no node_modules entry at all.
    let dir = project(&[(".env.schema", "# ---\nA=1\n")]);
    let bin = install_stub_cli(dir.path(), "tools");
    let run = run_with_path(dir.path(), Some(bin.parent().expect("parent")));
    assert_eq!(
        run.var("FROM_LOADER").as_deref(),
        Some("yes"),
        "a standalone loader on PATH must be found and used. stderr: {}",
        run.stderr
    );
}

#[cfg(unix)]
#[test]
fn a_node_shebang_loader_cli_does_not_re_enter_nub() {
    // Regression: nub's PATH shim made a `#!/usr/bin/env node` loader resolve
    // `node` back to nub, which re-detected the same owner and ran the loader
    // again — unbounded, and before the stub's own body ever executed. The stub
    // records every invocation so a regression fails loudly instead of hanging.
    let dir = project(&[(".env.schema", "# ---\nA=1\n")]);
    let calls = dir.path().join("calls.log");
    let bin = dir.path().join("node_modules/.bin/varlock");
    std::fs::create_dir_all(bin.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &bin,
        format!(
            "#!/usr/bin/env node\n\
             const fs = require('node:fs');\n\
             fs.appendFileSync({calls:?}, 'x');\n\
             if (fs.readFileSync({calls:?}, 'utf8').length > 3) process.exit(9);\n\
             process.stdout.write(JSON.stringify({{config:{{FROM_LOADER:{{value:'yes'}}}}}}));\n",
            calls = calls.to_str().expect("utf8 path")
        ),
    )
    .expect("write stub");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    // A real `node` must be reachable for the shebang; that is the point.
    let node_dir = which_node_dir();
    let run = run_with_path(dir.path(), Some(&node_dir));
    assert_eq!(
        run.var("FROM_LOADER").as_deref(),
        Some("yes"),
        "the node-shebang loader must run and be parsed. stderr: {}",
        run.stderr
    );
    let invocations = std::fs::read_to_string(&calls).unwrap_or_default().len();
    assert_eq!(
        invocations, 1,
        "the loader CLI must be invoked exactly once; {invocations} invocations means \
         nub re-entered itself through the node shim"
    );
}

#[cfg(unix)]
fn which_node_dir() -> PathBuf {
    let out = Command::new("sh")
        .args(["-c", "command -v node"])
        .output()
        .expect("locate node");
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    path.parent().expect("node has a parent dir").to_path_buf()
}

#[test]
fn compat_mode_does_no_owner_handling_at_all() {
    // `--node` is vanilla Node: no augmentation, so no adapter can run and nub
    // must not pretend otherwise.
    let dir = project(&[
        (".env", "FROM_DOTENV=yes\n"),
        (".env.schema", "# ---\nA=1\n"),
    ]);
    install_stub_loader(dir.path());
    let output = Command::new(nub_binary())
        .args(["--node", "probe.mjs"])
        .current_dir(dir.path())
        .env_remove("NODE_OPTIONS")
        .output()
        .expect("spawn nub --node");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|err| panic!("probe stdout was not JSON ({err}): {stdout}"));
    assert!(
        value["FROM_LOADER"].is_null(),
        "compat mode must not run the loader adapter, got {stdout}"
    );
}

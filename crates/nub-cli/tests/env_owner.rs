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
    write(
        root,
        "node_modules/varlock/package.json",
        r#"{"name":"varlock","version":"0.0.0","type":"module","exports":{".":"./index.js"}}"#,
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
    let output = Command::new(nub_binary())
        .arg("probe.mjs")
        .current_dir(dir)
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
        r#"{"name":"varlock","version":"0.0.0","type":"module","exports":{".":"./index.js"}}"#,
    );
    write(
        dir.path(),
        "node_modules/varlock/index.js",
        "export async function load() {}\nexport function patchGlobalConsole() {}",
    );
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

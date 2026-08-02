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
        SEEN_PATH: process.env.SEEN_PATH ?? null,
        SEEN_NODE_OPTIONS: process.env.SEEN_NODE_OPTIONS ?? null,
        LIVE_PATH: process.env.PATH ?? null,
        LIVE_NODE_OPTIONS: process.env.NODE_OPTIONS ?? null,
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
            "exports":{".":"./index.js","./auto-load":"./auto-load.js","./init-server":"./init-server.js"}}"#,
    );
    write(
        root,
        "node_modules/varlock/index.js",
        // Records what the environment looked like AT LOAD TIME. The real loader
        // snapshots process.env when it is imported and hands that snapshot to any
        // subprocess it spawns, so the scrub is only effective if it has already
        // happened by this point — which is exactly what these two capture.
        // `load()` reports whether it is running for the FIRST time in this env.
        // A second resolution overwrites the value with a marker, which is how the
        // Worker test detects a re-resolve that would clobber inherited values.
        r#"process.env.SEEN_PATH = process.env.PATH ?? "";
           process.env.SEEN_NODE_OPTIONS = process.env.NODE_OPTIONS ?? "";
           export async function load() {
             process.env.FROM_LOADER =
               process.env.FROM_LOADER === undefined ? "yes" : "re-resolved";
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
    write(
        root,
        "node_modules/varlock/init-server.js",
        // The blob-consuming entry a Worker takes: installs guards from an
        // inherited __VARLOCK_ENV without resolving or spawning anything.
        r#"process.env.FROM_INIT_SERVER = "yes";"#,
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
fn the_loader_is_imported_with_a_scrubbed_environment_that_is_then_restored() {
    // The loader snapshots process.env when it is imported and hands THAT to any
    // subprocess it spawns. If nub's shim is still on PATH there, the loader's CLI
    // resolves `node` back to nub and recurses without bound — which is exactly
    // what `_VARLOCK_FILTER` triggered, since it disables the reuse fast-path.
    // So the scrub has to be in place by import time, not merely before a spawn.
    let dir = project(&[(".env.schema", "# ---\nA=1\n")]);
    install_stub_loader(dir.path());

    let shim = dir.path().join("nub-node-shim-test");
    std::fs::create_dir_all(&shim).expect("mkdir");
    let mut command = Command::new(nub_binary());
    command
        .arg("probe.mjs")
        .current_dir(dir.path())
        .env("PATH", &shim)
        .env("NODE_OPTIONS", "");
    let output = command.output().expect("spawn nub");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "nub exited {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    let run = Run { stdout, stderr };

    let seen_path = run.var("SEEN_PATH").unwrap_or_default();
    assert!(
        !seen_path.contains("nub-node-shim-"),
        "the loader must not see nub's shim on PATH at import time, or its own \
         CLI would resolve `node` back to nub; saw {seen_path:?}"
    );
    let seen_options = run.var("SEEN_NODE_OPTIONS").unwrap_or_default();
    assert!(
        !seen_options.contains("env-owner"),
        "the loader must not see nub's env-owner preload tokens, or a subprocess \
         would re-run the adapter; saw {seen_options:?}"
    );

    let live_path = run.var("LIVE_PATH").unwrap_or_default();
    assert!(
        live_path.contains("nub-node-shim-"),
        "the scrub exists only to make the loader's subprocess safe — user code \
         must get the shim back, so a `node` the app spawns stays augmented; \
         saw {live_path:?}"
    );
    let live_options = run.var("LIVE_NODE_OPTIONS").unwrap_or_default();
    assert!(
        live_options.contains("env-owner"),
        "NODE_OPTIONS must be restored too, or a `node` the app spawns loses \
         env-owner handling entirely; saw {live_options:?}"
    );
}

#[test]
fn a_worker_inherits_the_environment_instead_of_re_resolving_it() {
    // Regression, and it was silent. A Worker inherits NODE_OPTIONS and re-ran the
    // adapter; `process.chdir` throws in a Worker, so the cwd hop was skipped and a
    // Worker started from a workspace member resolved from a directory with no
    // schema — producing an empty graph while __VARLOCK_ENV stayed set, so the
    // verification pass saw a load and said nothing.
    let dir = project(&[(".env.schema", "# ---\nA=1\n")]);
    install_stub_loader(dir.path());
    write(
        dir.path(),
        "worker.mjs",
        r#"import { Worker, isMainThread, parentPort } from "node:worker_threads";
           if (isMainThread) {
             const w = new Worker(new URL(import.meta.url));
             w.on("message", (m) => { console.log(JSON.stringify(m)); w.terminate(); });
           } else {
             parentPort.postMessage({
               FROM_LOADER: process.env.FROM_LOADER ?? null,
               FROM_INIT_SERVER: process.env.FROM_INIT_SERVER ?? null,
             });
           }"#,
    );

    let output = Command::new(nub_binary())
        .arg("worker.mjs")
        .current_dir(dir.path())
        .env("PATH", "")
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
    let seen: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|err| panic!("worker output was not JSON ({err}): {stdout}"));
    assert_eq!(
        seen["FROM_LOADER"].as_str(),
        Some("yes"),
        "the Worker must inherit the resolved value, not re-resolve it — \
         \"re-resolved\" means the adapter ran again inside the Worker, which is \
         where the cwd hop cannot work. stderr: {stderr}"
    );
    assert_eq!(
        seen["FROM_INIT_SERVER"].as_str(),
        Some("yes"),
        "the Worker must still install the loader's guards from the inherited \
         blob — skipping the module wholesale leaves @sensitive values printing \
         in the clear there, which `varlock run` does not do. stderr: {stderr}"
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

#[cfg(unix)]
#[test]
fn every_launch_path_that_injects_the_adapter_also_stamps_its_markers() {
    // Regression, and it was total and silent. `nub watch` injected the preload
    // but stamped no markers, so the adapter — which keys on the root marker —
    // no-opped. Detection had already stood nub's own cascade down, so the
    // watched process got NO environment at all, and the verification pass was
    // equally silent because its own marker was missing too.
    //
    // Watch is the reachable proof; the same omission applied to `nubx`/exec and
    // lifecycle scripts, which are harder to drive from a test.
    let dir = project(&[(".env.schema", "# ---\nA=1\n")]);
    install_stub_loader(dir.path());
    write(
        dir.path(),
        "watched.mjs",
        r#"console.log(JSON.stringify({ FROM_LOADER: process.env.FROM_LOADER ?? null }));
           process.exit(0);"#,
    );

    let mut child = Command::new(nub_binary())
        .args(["watch", "watched.mjs"])
        .current_dir(dir.path())
        .env("PATH", "")
        .env_remove("NODE_OPTIONS")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn nub watch");

    // `nub watch` never exits on its own, so read the first run's output and kill.
    let mut stdout = child.stdout.take().expect("stdout");
    let seen = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let mut chunk = [0u8; 512];
        while let Ok(n) = stdout.read(&mut chunk) {
            if n == 0 {
                break;
            }
            buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
            if buf.contains("FROM_LOADER") {
                break;
            }
        }
        buf
    })
    .join()
    .expect("reader thread");
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        seen.contains(r#""FROM_LOADER":"yes""#),
        "the watched process must get the loader's environment — a null here means \
         the adapter was injected without its markers and silently did nothing, \
         while nub's own cascade was already suppressed. Saw: {seen}"
    );
}

#[cfg(unix)]
#[test]
fn a_cli_owner_is_resolved_on_paths_that_do_not_run_the_child_env_builder() {
    // Regression: watch, exec and the lifecycle overlay stamped the ownership
    // markers without resolving anything, because only `runtime_child_env`
    // resolves a CLI owner. The child was told an owner was in charge while
    // nothing would ever load — so the environment came back empty AND the
    // verification pass told a user who HAD installed the loader to install it.
    let dir = project(&[(".env.schema", "# ---\nA=1\n")]);
    install_stub_cli(dir.path(), "tools");
    write(
        dir.path(),
        "watched.mjs",
        r#"console.log(JSON.stringify({ FROM_LOADER: process.env.FROM_LOADER ?? null }));
           process.exit(0);"#,
    );

    let mut child = Command::new(nub_binary())
        .args(["watch", "watched.mjs"])
        .current_dir(dir.path())
        .env("PATH", dir.path().join("tools"))
        .env_remove("NODE_OPTIONS")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn nub watch");

    let mut stdout = child.stdout.take().expect("stdout");
    let seen = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let mut chunk = [0u8; 512];
        while let Ok(n) = stdout.read(&mut chunk) {
            if n == 0 {
                break;
            }
            buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
            if buf.contains("FROM_LOADER") {
                break;
            }
        }
        buf
    })
    .join()
    .expect("reader thread");
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        seen.contains(r#""FROM_LOADER":"yes""#),
        "a CLI-only owner must be resolved on the watch path too, not merely \
         announced — a null here is the shape where nub suppressed its own \
         cascade and then loaded nothing. Saw: {seen}"
    );
}

#[test]
fn a_foreign_env_schema_is_left_alone_without_advertising_anything() {
    // `.env.schema` is not @env-spec's name. dotenv-extended has defaulted to it
    // since 2016 — nine years earlier — for an incompatible format: bare `NAME=`
    // lines, with the values still in `.env`. Warning on the filename told those
    // projects their schema "was not applied" while another tool was applying it
    // perfectly well, and recommended installing something they never asked for.
    let dir = project(&[
        (".env", "FROM_DOTENV=yes\n"),
        // No `# ---` divider and no `# @decorator`: not this format.
        (".env.schema", "# Server\nNODE_ENV=\nPORT=\n"),
    ]);
    let run = run(dir.path());
    assert_eq!(
        run.var("FROM_DOTENV").as_deref(),
        Some("yes"),
        "a foreign schema must not disturb nub's own .env loading. stderr: {}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("varlock"),
        "nub must not name another tool at a project that never asked for it; \
         stderr was: {}",
        run.stderr
    );
}

#[test]
fn an_env_spec_schema_without_the_loader_still_warns() {
    // The control for the test above: a genuinely `@env-spec` schema — it has the
    // header divider — still earns the hint, so the sniff has not simply muted it.
    let dir = project(&[
        (".env", "FROM_DOTENV=yes\n"),
        (".env.schema", "# @defaultSensitive=false\n# ---\nA=1\n"),
    ]);
    let run = run(dir.path());
    assert!(
        run.stderr.contains("@env-spec"),
        "an @env-spec schema with no loader installed must still say so; \
         stderr was: {}",
        run.stderr
    );
}

#[test]
fn silent_suppresses_the_verification_warning() {
    // The warning is on by default because it reports a run with no environment
    // at all, but `--silent` has to mean silent.
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

    let noisy = run(dir.path());
    assert!(
        noisy.stderr.contains("never loaded the environment"),
        "control: the warning must fire without --silent, or this test proves \
         nothing; stderr was: {}",
        noisy.stderr
    );

    let quiet = Command::new(nub_binary())
        .args(["--silent", "probe.mjs"])
        .current_dir(dir.path())
        .env("PATH", "")
        .env_remove("NODE_OPTIONS")
        .output()
        .expect("spawn nub");
    let quiet_stderr = String::from_utf8_lossy(&quiet.stderr);
    // Assert the run SUCCEEDED before reading anything into its silence. A
    // negative assertion alone passes on any failure of the invocation — a
    // rejected flag position, a spawn error, an early exit — so it would prove
    // nothing about suppression.
    assert!(
        quiet.status.success(),
        "the --silent run must succeed, or its silence means nothing; \
         exited {:?}, stderr: {quiet_stderr}",
        quiet.status.code()
    );
    assert!(
        !quiet_stderr.contains("never loaded the environment"),
        "--silent must suppress the verification warning; stderr was: {quiet_stderr}"
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
        "#!/bin/sh\nprintf '%s' '{\"config\":{\"FROM_LOADER\":{\"value\":\"yes\",\"isSensitive\":false}}}'\n",
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

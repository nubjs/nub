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

#[cfg(unix)]
#[test]
fn a_watcher_inside_the_loader_does_not_load_config_sources() {
    // `run_watch` builds its own env instead of going through `runtime_child_env`,
    // so gating that function's `Sources` arm left this copy of the same hole open.
    // Reachable because the wrap marker is INHERITED: any `nub watch` started by a
    // program already running behind the loader arrives here owned, and used to
    // load `envFile` sources the outer refusal had no chance to see.
    let dir = project(&[
        (".env.schema", "# ---\nA=1\n"),
        ("custom.env", "FROM_DOTENV=smuggled\n"),
        ("nub.jsonc", r#"{ "envFile": ["custom.env"] }"#),
    ]);
    let root = std::fs::canonicalize(dir.path()).expect("canonicalize");

    let mut child = Command::new(nub_binary())
        .args(["watch", "probe.mjs"])
        .current_dir(dir.path())
        .env("PATH", which_node_dir())
        .env("__NUB_ENV_OWNER_WRAPPED", &root)
        .env_remove("NODE_OPTIONS")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn nub watch");

    let stdout = child.stdout.take().expect("piped stdout");
    let first_line = std::thread::spawn(move || {
        use std::io::BufRead;
        std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .find(|line| line.contains("FROM_DOTENV"))
    });
    let line = wait_for(first_line, std::time::Duration::from_secs(60));
    let _ = child.kill();
    let _ = child.wait();

    let line = line.expect("nub watch printed no probe output within 60s");
    assert!(
        line.contains(r#""FROM_DOTENV":null"#),
        "a watcher behind the loader must not load explicit envFile sources; got: {line}"
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
        // The marker carries the wrapped project's root, so a nested nub can tell
        // "this project is already behind the loader" from "some other one is".
        .env("__NUB_ENV_OWNER_WRAPPED", dir.path())
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
fn a_declared_but_missing_loader_is_fatal() {
    // A project that asks for the loader in its manifest and cannot resolve it has
    // a BROKEN TREE — a pruned `--prod` install, a partial `node_modules`. Falling
    // back to `.env*` would hand the program an environment it never asked for: no
    // defaults, no validation, no providers, and for a schema-only project, nothing
    // at all. It also used to advise `nub add -D varlock` at a project that already
    // declares it.
    let dir = project(&[
        (
            "package.json",
            r#"{"name":"f","version":"1.0.0","devDependencies":{"varlock":"^1.16.0"}}"#,
        ),
        (".env", "FROM_DOTENV=yes\n"),
        (".env.schema", "# ---\nA=1\n"),
    ]);
    let node_dir = which_node_dir();
    let output = Command::new(nub_binary())
        .arg("probe.mjs")
        .current_dir(dir.path())
        .env("PATH", &node_dir)
        .env_remove("NODE_OPTIONS")
        .output()
        .expect("spawn nub");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "a broken loader install must not run the program; exit was {:?}",
        output.status.code()
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).trim().is_empty(),
        "the program must not have run at all; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("nub install"),
        "the fix for a declared-but-missing loader is an install, not an add; \
         stderr: {stderr}"
    );
    assert!(
        !stderr.contains("nub add"),
        "must not tell a project that already declares the loader to add it; \
         stderr: {stderr}"
    );
}

#[test]
fn a_schema_without_any_loader_is_fatal() {
    // Falling back to `.env*` is a DIFFERENT answer, not a softer one: no defaults,
    // no validation, no providers. A schema the project has not disclaimed says the
    // environment is schema-resolved, so nub refuses rather than running the
    // program on an environment it never asked for.
    let dir = project(&[
        (".env", "FROM_DOTENV=yes\n"),
        (".env.schema", "# ---\nA=1\n"),
    ]);
    let node_dir = which_node_dir();
    let output = Command::new(nub_binary())
        .arg("probe.mjs")
        .current_dir(dir.path())
        .env("PATH", &node_dir)
        .env_remove("NODE_OPTIONS")
        .output()
        .expect("spawn nub");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "an unreadable schema must not run the program; exit was {:?}",
        output.status.code()
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).trim().is_empty(),
        "the program must not have run at all; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("nub add -D varlock"),
        "the fix for a schema with no loader declared anywhere is an add — as a \
         DEV dependency, which is what the loader's own docs prescribe; \
         stderr: {stderr}"
    );
}

#[test]
fn a_project_using_a_rival_schema_tool_is_not_warned_at() {
    // `dotenv-extended` claims this filename too. A project that declares it has
    // a tool applying its schema already, so telling it the schema "was not
    // applied" is false, and recommending another package is noise. Its manifest
    // is the only thing that can say so — nub never reads the schema itself, so
    // the file here is deliberately @env-spec shaped.
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

#[cfg(unix)]
#[test]
fn a_declared_rival_tool_blocks_the_hand_over_even_with_the_loader_installed() {
    // The carve-out gates the HAND-OVER, not just the diagnostic. Gating only the
    // warning leaves the worse half live: a `dotenv-extended` project where any
    // varlock happens to be reachable would have nub suppress its own cascade and
    // route the whole run through `varlock run` against a schema written for
    // another format — silently, since the warning is what is being suppressed.
    let dir = project(&[
        (
            "package.json",
            r#"{"name":"f","version":"1.0.0","devDependencies":{"dotenv-extended":"^2.9.0"}}"#,
        ),
        (".env", "FROM_DOTENV=yes\n"),
        (".env.schema", "# Server\nPORT=\nAPI_URL=\n"),
    ]);
    let tally = dir.path().join("tally");
    install_stub_loader(dir.path(), &tally);
    let run = run(dir.path());

    assert_eq!(
        run.var("FROM_LOADER"),
        None,
        "a declared rival must not put the loader in front of Node. stderr: {}",
        run.stderr
    );
    assert!(!tally.exists(), "and must not invoke the loader at all");
    assert_eq!(
        run.var("FROM_DOTENV").as_deref(),
        Some("yes"),
        "nub must keep loading its own .env files. stderr: {}",
        run.stderr
    );
}

#[cfg(unix)]
#[test]
fn a_different_project_inside_the_wrap_still_wraps_its_own() {
    // The marker records WHICH project is wrapped, not merely that something is.
    // A bare flag made any project reached from inside the wrap stand down — so a
    // second schema-owned project ran on the outer project's environment, its own
    // schema never resolved and no warning, since the diagnostic is gated on the
    // same flag.
    let dir = project(&[(".env.schema", "# ---\nA=1\n")]);
    let tally = dir.path().join("tally");
    install_stub_loader(dir.path(), &tally);

    let node_dir = which_node_dir();
    let output = Command::new(nub_binary())
        .arg("probe.mjs")
        .current_dir(dir.path())
        // A parent nub wrapped a DIFFERENT project.
        .env("__NUB_ENV_OWNER_WRAPPED", "/somewhere/else/entirely")
        .env("PATH", &node_dir)
        .env_remove("NODE_OPTIONS")
        .output()
        .expect("spawn nub");
    let run = Run {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    assert_eq!(
        run.var("FROM_LOADER").as_deref(),
        Some("yes"),
        "a different project's wrap must not suppress this project's own. \
         stderr: {}",
        run.stderr
    );
}

/// Run without asserting success — for the cases whose point IS the exit code.
#[cfg(unix)]
fn try_run(dir: &Path, args: &[&str]) -> (bool, String, String) {
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
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[cfg(unix)]
#[test]
fn an_explicit_env_file_alongside_the_loader_is_refused() {
    // Two deliberate instructions that contradict: load THESE files, and the loader
    // owns the environment. Whichever nub picked, the other would vanish silently —
    // nothing prints which files did or did not arrive. So it refuses and names
    // both. Previously the explicit setting simply won, which is the silent half.
    let dir = project(&[
        (".env.schema", "# ---\nA=1\n"),
        ("custom.env", "FROM_DOTENV=explicit\n"),
        ("nub.jsonc", r#"{ "envFile": "custom.env" }"#),
    ]);
    let tally = dir.path().join("tally");
    install_stub_loader(dir.path(), &tally);

    let (ok, stdout, stderr) = try_run(dir.path(), &["probe.mjs"]);
    assert!(!ok, "the contradiction must not run; stdout: {stdout}");
    assert!(
        stderr.contains("envFile") && stderr.contains("varlock"),
        "the error must name BOTH sides, so the user knows which two things collided; \
         stderr: {stderr}"
    );
    assert!(
        !tally.exists(),
        "and must refuse BEFORE spawning the loader"
    );

    // Same collision from the command line, which is the likelier way to hit it.
    let (ok, _, stderr) = try_run(dir.path(), &["--env-file=custom.env", "probe.mjs"]);
    assert!(!ok, "an explicit --env-file must be refused too");
    assert!(
        stderr.contains("--env-file"),
        "the error must name the flag the user actually typed, not the config field; \
         stderr: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn an_explicit_env_file_is_fine_without_a_loader() {
    // The control for the test above: same flag, no schema. Without this, that test
    // would pass just as well if `--env-file` were broken outright.
    let dir = project(&[("custom.env", "FROM_DOTENV=explicit\n")]);
    let run = run_args(dir.path(), &["--env-file=custom.env", "probe.mjs"]);
    assert_eq!(
        run.var("FROM_DOTENV").as_deref(),
        Some("explicit"),
        "with no loader in play an explicit env file must still load. stderr: {}",
        run.stderr
    );
}

#[cfg(unix)]
#[test]
fn no_env_file_does_not_smuggle_config_sources_past_the_loader() {
    // The one path the outer refusal cannot cover. `--no-env-file` is classified as
    // a non-conflict, so nub hands over — but the flag does not survive the spawn
    // while the config snapshot does, so the nested nub inside the loader used to
    // load `envFile` sources with nothing left to suppress them. Asking for LESS
    // loading produced MORE than the default, silently.
    let dir = project(&[
        (".env.schema", "# ---\nA=1\n"),
        ("custom.env", "FROM_DOTENV=smuggled\n"),
        ("nub.jsonc", r#"{ "envFile": ["custom.env"] }"#),
    ]);
    let tally = dir.path().join("tally");
    install_stub_loader(dir.path(), &tally);
    let run = run_args(dir.path(), &["--no-env-file", "probe.mjs"]);
    assert_eq!(
        run.var("FROM_DOTENV"),
        None,
        "an explicit envFile source must not reach the child once the loader owns \
         the environment, from any process. stderr: {}",
        run.stderr
    );
    assert_eq!(
        run.var("FROM_LOADER").as_deref(),
        Some("yes"),
        "and the hand-over must still have happened. stderr: {}",
        run.stderr
    );
}

#[cfg(unix)]
#[test]
fn the_conflict_names_the_if_exists_flag_when_that_is_what_was_typed() {
    // Both spellings set one presence flag, so the message used to say `--env-file`
    // to a user who typed `--env-file-if-exists` — sending them to look for a flag
    // that is not in their command.
    let dir = project(&[
        (".env.schema", "# ---\nA=1\n"),
        ("custom.env", "FROM_DOTENV=explicit\n"),
    ]);
    let tally = dir.path().join("tally");
    install_stub_loader(dir.path(), &tally);
    let (ok, _, stderr) = try_run(
        dir.path(),
        &["--env-file-if-exists=custom.env", "probe.mjs"],
    );
    assert!(!ok, "the conflict must be refused for either spelling");
    assert!(
        stderr.contains("--env-file-if-exists"),
        "the error must name the spelling the user typed; stderr: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn loading_nothing_does_not_conflict_with_the_loader() {
    // `--no-env-file` asks nub to load nothing, which is exactly what standing down
    // for the loader already does. Only a LOAD instruction contradicts a hand-over,
    // so this must still run — and still run THROUGH the loader.
    let dir = project(&[(".env.schema", "# ---\nA=1\n")]);
    let tally = dir.path().join("tally");
    install_stub_loader(dir.path(), &tally);
    let run = run_args(dir.path(), &["--no-env-file", "probe.mjs"]);
    assert_eq!(
        run.var("FROM_LOADER").as_deref(),
        Some("yes"),
        "--no-env-file must not disturb the hand-over. stderr: {}",
        run.stderr
    );
}

// @lat: [[compat-mode-tests#Compat mode#Compat mode never puts the loader in front of Node]]
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

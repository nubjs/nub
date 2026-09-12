//! End-to-end coverage for the `prefix` field of `nub.jsonc`: a command nub puts
//! in front of a file run, a script, and a watch.
//!
//! The wrapper is a STUB, not dotenvx. What nub owes is a contract — resolve the
//! program from the project's `.bin` chain, put it in front of the launch with
//! its arguments intact, keep loading `.env*`, wrap a script as a whole, and
//! never wrap the same project twice — and a stub exercises all of it
//! hermetically. Unix-only: the stub is a `#!/usr/bin/env node` script, the
//! shape a wrapper installed from npm actually has.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

/// A project whose probe reports what reached the child.
fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        "probe.mjs",
        r#"console.log(JSON.stringify({
            FROM_WRAPPER: process.env.FROM_WRAPPER ?? null,
            FROM_DOTENV: process.env.FROM_DOTENV ?? null,
            WRAPPER_FLAGS: process.env.WRAPPER_FLAGS ?? null,
            WRAPPED_PROGRAM: process.env.WRAPPED_PROGRAM ?? null,
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

/// A stand-in for a wrapper CLI with dotenvx's shape: `wrap [flags…] -- <command…>`.
///
/// It records that it ran, notes its own flags and the program it was handed,
/// then spawns that command with the marker variables set. The
/// `#!/usr/bin/env node` shebang is what a real wrapper's bin carries, so its
/// own interpreter resolves `node` and would re-enter nub through the PATH shim
/// inside a script — the re-entrancy channel the marker exists to close.
fn install_stub_wrapper(path: &Path, tally: &Path) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        path,
        format!(
            r#"#!/usr/bin/env node
const {{ appendFileSync }} = require("node:fs");
const {{ spawnSync }} = require("node:child_process");
appendFileSync({tally:?}, "ran\n");
const argv = process.argv.slice(2);
const sep = argv.indexOf("--");
if (sep < 0) {{
  console.error("stub wrapper: no '--' separator");
  process.exit(64);
}}
const cmd = argv.slice(sep + 1);
const res = spawnSync(cmd[0], cmd.slice(1), {{
  stdio: "inherit",
  env: {{
    ...process.env,
    FROM_WRAPPER: "yes",
    WRAPPER_FLAGS: argv.slice(0, sep).join("|"),
    WRAPPED_PROGRAM: require("node:path").basename(cmd[0]),
  }},
}});
process.exit(res.status ?? 1);
"#,
            tally = tally.to_string_lossy()
        ),
    )
    .expect("write stub");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

fn install_local_wrapper(root: &Path, tally: &Path) {
    install_stub_wrapper(&root.join("node_modules/.bin/wrap"), tally);
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

struct Run {
    status: std::process::ExitStatus,
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

/// `PATH` holds Node and the system shell only, so a wrapper resolves through
/// the project's `.bin` chain or not at all.
fn run(dir: &Path, args: &[&str]) -> Run {
    let output = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env("PATH", test_path())
        .env_remove("NODE_OPTIONS")
        .output()
        .expect("spawn nub");
    Run {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Node's directory plus the system shell's: a script wrapper re-spawns `sh`
/// through the `PATH` nub hands it, exactly as the shell is found in any real
/// environment.
fn test_path() -> std::ffi::OsString {
    std::env::join_paths([which_node_dir(), "/usr/bin".into(), "/bin".into()]).expect("join PATH")
}

/// Join a reader thread within `timeout`, or give up and report `None`.
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

fn runs(tally: &Path) -> usize {
    std::fs::read_to_string(tally).map_or(0, |s| s.lines().count())
}

#[test]
fn a_string_prefix_wraps_the_file_run_and_keeps_env_files() {
    let dir = project(&[
        (".env", "FROM_DOTENV=1\n"),
        ("nub.jsonc", r#"{ "prefix": "wrap --flag 'two words' --" }"#),
    ]);
    let tally = dir.path().join("tally");
    install_local_wrapper(dir.path(), &tally);
    let run = run(dir.path(), &["probe.mjs"]);
    assert!(run.status.success(), "stderr: {}", run.stderr);

    assert_eq!(
        run.var("FROM_WRAPPER").as_deref(),
        Some("yes"),
        "stderr: {}",
        run.stderr
    );
    assert_eq!(
        run.var("WRAPPED_PROGRAM").as_deref(),
        Some("node"),
        "the wrapper must be handed the node command"
    );
    assert_eq!(
        run.var("WRAPPER_FLAGS").as_deref(),
        Some("--flag|two words"),
        "the string form splits like a shell, so a quoted argument stays one word"
    );
    assert_eq!(
        run.var("FROM_DOTENV").as_deref(),
        Some("1"),
        "nub keeps loading env files; the wrapper starts with them set"
    );
    assert_eq!(runs(&tally), 1);
}

#[test]
fn a_script_runs_behind_the_prefix_once() {
    // The outer `nub run` wraps the shell. The nested `nub run` and the `node`
    // inside it re-enter nub in the same project and must not wrap again.
    let nub = nub_binary();
    let dir = project(&[
        ("nub.jsonc", r#"{ "prefix": ["wrap", "--"] }"#),
        (
            "package.json",
            &format!(
                r#"{{"name":"fx","version":"1.0.0","scripts":{{"probe":"node probe.mjs","outer":"{} run probe"}}}}"#,
                nub.display()
            ),
        ),
    ]);
    let tally = dir.path().join("tally");
    install_local_wrapper(dir.path(), &tally);
    let run = run(dir.path(), &["run", "outer"]);
    assert!(run.status.success(), "stderr: {}", run.stderr);

    assert_eq!(
        run.var("FROM_WRAPPER").as_deref(),
        Some("yes"),
        "stderr: {}",
        run.stderr
    );
    assert_eq!(
        run.var("WRAPPED_PROGRAM").as_deref(),
        Some("sh"),
        "a script is wrapped as a whole: the wrapper sees the shell, not node"
    );
    assert_eq!(
        runs(&tally),
        1,
        "the nested nub run and the inner node must not wrap the project again. stderr: {}",
        run.stderr
    );
}

#[test]
fn nub_watch_puts_the_prefix_in_front_of_node() {
    let dir = project(&[("nub.jsonc", r#"{ "prefix": "wrap --" }"#)]);
    let tally = dir.path().join("tally");
    install_local_wrapper(dir.path(), &tally);

    let mut child = Command::new(nub_binary())
        .args(["watch", "probe.mjs"])
        .current_dir(dir.path())
        .env("PATH", test_path())
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
            .find(|line| line.contains("FROM_WRAPPER"))
    });
    let line = wait_for(first_line, std::time::Duration::from_secs(60));
    let _ = child.kill();
    let _ = child.wait();

    let line = line.expect("nub watch printed no probe output within 60s");
    assert!(
        line.contains(r#""FROM_WRAPPER":"yes""#) && line.contains(r#""WRAPPED_PROGRAM":"node""#),
        "nub watch must run Node behind the wrapper; got: {line}"
    );
}

#[test]
fn a_relative_prefix_anchors_to_its_config_file() {
    // The file at the project root names `./tools/wrap`; the run is from a
    // subdirectory, where that path would not exist relative to the cwd.
    let dir = project(&[
        ("nub.jsonc", r#"{ "prefix": ["./tools/wrap", "--"] }"#),
        ("sub/probe.mjs", "import '../probe.mjs';"),
    ]);
    let tally = dir.path().join("tally");
    install_stub_wrapper(&dir.path().join("tools/wrap"), &tally);
    let run = run(&dir.path().join("sub"), &["probe.mjs"]);
    assert!(run.status.success(), "stderr: {}", run.stderr);
    assert_eq!(
        run.var("FROM_WRAPPER").as_deref(),
        Some("yes"),
        "stderr: {}",
        run.stderr
    );
}

#[test]
fn a_prefix_that_resolves_to_nothing_is_refused_by_name() {
    let dir = project(&[("nub.jsonc", r#"{ "prefix": "no-such-wrapper --" }"#)]);
    let refused = run(dir.path(), &["probe.mjs"]);
    assert!(!refused.status.success(), "stdout: {}", refused.stdout);
    assert!(
        refused.stderr.contains("ERR_NUB_PREFIX_NOT_FOUND")
            && refused.stderr.contains("no-such-wrapper"),
        "the error must name the code and the program: {}",
        refused.stderr
    );
    // Compat mode is the escape hatch: a bare spawn that never resolves the
    // wrapper, so a broken prefix cannot take `--node` down with it.
    let compat = run(dir.path(), &["--node", "probe.mjs"]);
    assert!(compat.status.success(), "stderr: {}", compat.stderr);
    assert_eq!(compat.var("FROM_WRAPPER"), None);
}

#[test]
fn nubx_does_not_take_the_prefix() {
    // A local bin is someone else's CLI, and `nubx` carries its own config.
    let dir = project(&[("nub.jsonc", r#"{ "prefix": "wrap --" }"#)]);
    let tally = dir.path().join("tally");
    install_local_wrapper(dir.path(), &tally);
    let bin = dir.path().join("node_modules/.bin/probe-bin");
    std::fs::write(
        &bin,
        "#!/usr/bin/env node\nconsole.log(JSON.stringify({ FROM_WRAPPER: process.env.FROM_WRAPPER ?? null }));\n",
    )
    .expect("write bin");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let run = run(dir.path(), &["exec", "probe-bin"]);
    assert!(run.status.success(), "stderr: {}", run.stderr);
    assert_eq!(run.var("FROM_WRAPPER"), None, "stderr: {}", run.stderr);
    assert!(!tally.exists(), "the wrapper must not run for a local bin");
}

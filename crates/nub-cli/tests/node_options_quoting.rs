//! A project `nub.jsonc` `nodeOptions` entry containing a double quote must
//! reach the child intact on EVERY surface that composes `NODE_OPTIONS`.
//!
//! Four independent producers build that variable — the direct file run and the
//! script/exec augmentation in `nub_core::node::spawn`, plus two more inside
//! `nub watch` — so fixing one says nothing about the other three, and all four
//! are exercised here. The CLI validator rejects whitespace and NUL but NOT a
//! quote, and `--title` is in Node's `allowedNodeEnvironmentFlags`, so
//! `--title=a"b` passes validation and reaches Node's tokenizer with an
//! unmatched quote: a hard startup abort (`invalid value for NODE_OPTIONS
//! (unterminated string)`, exit 9) rather than a degraded run.

#![cfg(unix)]

use std::io::BufRead;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// The entry the validator lets through: a quote, no whitespace.
const TITLE: &str = r#"a"b"#;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

fn host_node_usable() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

struct Fixture {
    _temp: tempfile::TempDir,
    project: PathBuf,
    xdg_config: PathBuf,
    cache: PathBuf,
    nubx: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let nubx = temp.path().join("nubx");
        std::fs::create_dir_all(project.join("node_modules/.bin")).unwrap();
        std::os::unix::fs::symlink(nub_binary(), &nubx).unwrap();

        std::fs::write(
            project.join("nub.jsonc"),
            r#"{ "nodeOptions": ["--title=a\"b"] }"#,
        )
        .unwrap();
        std::fs::write(
            project.join("probe.js"),
            "console.log(JSON.stringify({ title: process.title, nodeOptions: process.env.NODE_OPTIONS || \"\" }));\n",
        )
        .unwrap();
        std::fs::write(
            project.join("package.json"),
            r#"{ "name": "q", "private": true, "scripts": { "probe": "node probe.js" } }"#,
        )
        .unwrap();
        // A `#!/usr/bin/env node` bin is what re-enters nub's `node` PATH shim,
        // which is the only channel augmentation reaches a launched tool through.
        let bin = project.join("node_modules/.bin/probe");
        std::fs::write(&bin, "#!/usr/bin/env node\nrequire('../../probe.js');\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        Self {
            xdg_config: temp.path().join("config"),
            cache: temp.path().join("cache"),
            _temp: temp,
            project,
            nubx,
        }
    }

    fn command_for(&self, binary: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(binary);
        command
            .current_dir(&self.project)
            .env("XDG_CONFIG_HOME", &self.xdg_config)
            .env("XDG_CACHE_HOME", &self.cache);
        command
    }

    fn command(&self) -> Command {
        self.command_for(nub_binary())
    }
}

/// Run a surface that exits on its own and assert the title round-tripped.
fn assert_surface(surface: &str, mut command: Command) {
    let output = command.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "[{surface}] exited {:?} instead of running — a raw (unquoted) \
         nodeOptions entry aborts Node before it executes a line.\nstderr: {stderr}",
        output.status.code()
    );
    assert_title(surface, &String::from_utf8_lossy(&output.stdout));
}

/// Assert the probe's JSON line reports a byte-identical title. The observed
/// `NODE_OPTIONS` rides the failure message: a red here means one of the four
/// producers drifted, and the variable's contents name which.
fn assert_title(surface: &str, stdout: &str) {
    let line = stdout
        .lines()
        .find(|l| l.starts_with('{'))
        .unwrap_or_else(|| panic!("[{surface}] no JSON line in output: {stdout:?}"));
    let value: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(
        value["title"].as_str(),
        Some(TITLE),
        "[{surface}] title did not survive NODE_OPTIONS.\nNODE_OPTIONS was: {}",
        value["nodeOptions"].as_str().unwrap_or_default()
    );
}

#[test]
fn quoted_node_option_survives_every_node_options_producer() {
    if !host_node_usable() {
        eprintln!("skipping node-options quoting: no usable node on PATH");
        return;
    }
    let fixture = Fixture::new();

    let mut file_run = fixture.command();
    file_run.arg("probe.js");
    assert_surface("nub <file>", file_run);

    let mut script_run = fixture.command();
    script_run.args(["run", "probe"]);
    assert_surface("nub run", script_run);

    let mut nubx = fixture.command_for(&fixture.nubx);
    nubx.arg("probe");
    assert_surface("nubx", nubx);
}

/// `nub watch` gets its own test because it never exits: the assertion is on
/// the first line the watched child emits, not on an exit status. It is also
/// the surface that stayed broken after the other three were fixed, so its
/// coverage is the point rather than a completeness gesture.
#[test]
fn quoted_node_option_survives_the_watch_producer() {
    if !host_node_usable() {
        eprintln!("skipping node-options quoting (watch): no usable node on PATH");
        return;
    }
    let fixture = Fixture::new();
    let mut child = fixture
        .command()
        .args(["watch", "probe.js"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = std::io::BufReader::new(stdout).lines();
        let found = lines.by_ref().find_map(|l| match l {
            Ok(line) if line.starts_with('{') => Some(line),
            _ => None,
        });
        let _ = tx.send(found);
        for _ in lines {} // drain so the child never blocks on a full pipe
    });

    // 30s, double the sibling watch tests' budget: the failing shape is an
    // IMMEDIATE exit-9 abort, so a generous ceiling costs nothing on red and
    // only buys headroom for a slow watch boot on a contended runner.
    let line = rx.recv_timeout(Duration::from_secs(30)).ok().flatten();
    let _ = child.kill();
    let stderr = child
        .wait_with_output()
        .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
        .unwrap_or_default();
    let line = line.unwrap_or_else(|| {
        panic!("[nub watch] watched child produced no output — a raw nodeOptions entry aborts it at startup.\nstderr: {stderr}")
    });
    assert_title("nub watch", &line);
}

//! `nub --test --experimental-test-coverage` must report the same set of files
//! plain `node` does — the user's own test files excluded.
//!
//! Node applies its default test-file exclusion (`kDefaultPattern` in
//! `lib/internal/test_runner/utils.js`) only when NO `--test-coverage-exclude`
//! is set. nub injects one of its own to keep the preloaded runtime out of the
//! report, which silently turned that default off and folded every `*.test.js`
//! back into the user's coverage. The compensation must be conditional: when the
//! user supplies their own exclude, node drops the default too, and so must nub.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

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
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("logic.js"), "exports.add = (a, b) => a + b;\n").unwrap();
        std::fs::write(
            project.join("logic.test.js"),
            "const t = require('node:test');\n\
             const a = require('node:assert');\n\
             t('add', () => { a.strictEqual(require('./logic.js').add(1, 2), 3); });\n",
        )
        .unwrap();
        Self {
            project,
            _temp: temp,
        }
    }

    fn command(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .current_dir(&self.project)
            .env("XDG_CONFIG_HOME", self._temp.path().join("config"))
            .env("XDG_CACHE_HOME", self._temp.path().join("cache"))
            .env_remove("NODE_OPTIONS")
            .args([
                "--test",
                "--experimental-test-coverage",
                "--test-reporter=tap",
            ]);
        command
    }
}

/// The TAP reporter's coverage table, without the surrounding test output — so a
/// `logic.test.js` mention in a test *name* or a stack frame can never be read as
/// a coverage row.
fn coverage_report(runner: &str, mut command: Command) -> String {
    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "[{runner}] exited {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    let start = stdout
        .find("# start of coverage report")
        .unwrap_or_else(|| panic!("[{runner}] no coverage report in output:\n{stdout}"));
    let end = stdout[start..]
        .find("# end of coverage report")
        .map(|i| start + i)
        .unwrap_or(stdout.len());
    stdout[start..end].to_string()
}

/// nub and plain node must agree on which files the report lists. Comparing
/// against the live `node` rather than a hardcoded expectation keeps the test
/// honest across Node versions, whose default pattern is version-dependent.
#[test]
fn coverage_excludes_the_users_test_files_like_node() {
    if !host_node_usable() {
        eprintln!("skipping coverage default-exclusion: no usable node on PATH");
        return;
    }
    let fixture = Fixture::new();

    let node = coverage_report("node", fixture.command(Path::new("node")));
    assert!(
        node.contains("logic.js") && !node.contains("logic.test.js"),
        "precondition broken — this Node does not apply the default test-file \
         exclusion, so the nub assertion below would pass vacuously:\n{node}"
    );

    let nub = coverage_report("nub", fixture.command(&nub_binary()));
    assert!(
        nub.contains("logic.js"),
        "nub dropped the file under test from the coverage report:\n{nub}"
    );
    assert!(
        !nub.contains("logic.test.js"),
        "nub reported the user's own test file; node does not. nub's injected \
         --test-coverage-exclude turned off Node's default exclusion:\n{nub}"
    );
}

/// A user-supplied `--test-coverage-exclude` disables Node's default exclusion
/// for node too, so nub must not re-add it — otherwise nub silently excludes
/// test files a user asked to see.
#[test]
fn a_user_supplied_exclude_still_disables_the_default_like_node() {
    if !host_node_usable() {
        eprintln!("skipping coverage user-exclude parity: no usable node on PATH");
        return;
    }
    let fixture = Fixture::new();
    let user_exclude = "--test-coverage-exclude=**/no-such-dir/**";

    let mut node_cmd = fixture.command(Path::new("node"));
    node_cmd.arg(user_exclude);
    let node = coverage_report("node", node_cmd);
    assert!(
        node.contains("logic.test.js"),
        "precondition broken — a user exclude no longer disables Node's default:\n{node}"
    );

    let mut nub_cmd = fixture.command(&nub_binary());
    nub_cmd.arg(user_exclude);
    let nub = coverage_report("nub", nub_cmd);
    assert!(
        nub.contains("logic.test.js"),
        "nub applied Node's default exclusion on top of a user-supplied one; \
         node reports the test file here:\n{nub}"
    );
}

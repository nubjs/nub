//! Under `--experimental-test-coverage`, nub must report exactly the files the
//! host `node` reports.
//!
//! Node applies its default test-file exclusion (`kDefaultPattern` in
//! `lib/internal/test_runner/utils.js`) only when NO `--test-coverage-exclude` is
//! set, and only from 23.5.0 — it was never backported to 22.x. nub injects an
//! exclude of its own to keep the preloaded runtime out of the report, which
//! silently switched that default off.
//!
//! The contract is parity with whatever `node` this host resolved, so these tests
//! diff nub's table against the live node's rather than against a fixed
//! expectation. That is what makes them version-proof: on 26 both exclude the test
//! file, on 22.15 both include it, and a hardcoded table would be wrong on one of
//! the two.

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

/// Whether the host node has `--test-coverage-exclude` at all (22.5+). Below it the
/// flag is a bad option and node exits 9, so the user-exclude case has no control to
/// compare against and is skipped rather than asserted.
fn host_node_has_coverage_exclude() -> bool {
    Command::new("node")
        .args(["--test-coverage-exclude=probe/**", "-e", ""])
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
        // ESM, not CJS. A CJS `require('node:test')` trips a separate defect on the
        // 22.15 tier — upstream's convertCJSFilenameToURL mishandles scheme-only
        // builtin ids once any sync resolve hook is registered; fixed by PR #803 —
        // and the fixture itself throwing would red this test for a reason it does
        // not govern.
        std::fs::write(
            project.join("package.json"),
            "{ \"name\": \"cov\", \"private\": true, \"type\": \"module\" }\n",
        )
        .unwrap();
        std::fs::write(
            project.join("logic.js"),
            "export const add = (a, b) => a + b;\n",
        )
        .unwrap();
        std::fs::write(
            project.join("logic.test.js"),
            "import t from 'node:test';\n\
             import a from 'node:assert';\n\
             import { add } from './logic.js';\n\
             t('add', () => { a.strictEqual(add(1, 2), 3); });\n",
        )
        .unwrap();
        Self {
            project,
            _temp: temp,
        }
    }

    fn command(&self, binary: &Path, extra: &[&str]) -> Command {
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
            ])
            .args(extra);
        command
    }
}

/// The fixture file names the coverage table lists, in report order. Reducing to
/// names drops the percentages, and keeping only the fixture's own files drops nub's
/// runtime rows — so the comparison is about WHICH of the user's files are reported,
/// which is what the exclusion decides, and is not perturbed by the preload shifting
/// a branch denominator.
fn reported_files(runner: &str, mut command: Command) -> Vec<String> {
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
    stdout[start..end]
        .lines()
        .filter_map(|line| line.strip_prefix("# "))
        .filter_map(|row| row.split('|').next())
        .map(str::trim)
        .filter(|name| *name == "logic.js" || *name == "logic.test.js")
        .map(str::to_string)
        .collect()
}

/// Both runners see the same fixture and the same flags, so any difference is nub's
/// injected exclude changing which files Node decides to report.
#[test]
fn coverage_reports_the_same_files_as_the_host_node() {
    if !host_node_usable() {
        eprintln!("skipping coverage default-exclusion: no usable node on PATH");
        return;
    }
    let fixture = Fixture::new();

    let node = reported_files("node", fixture.command(Path::new("node"), &[]));
    let nub = reported_files("nub", fixture.command(&nub_binary(), &[]));

    // Positive control: an empty report on both sides would satisfy the equality
    // below while proving nothing. The file under test is covered on every version.
    assert!(
        node.contains(&"logic.js".to_string()),
        "control failed — node reported no coverage for the file under test: {node:?}"
    );
    assert_eq!(
        nub, node,
        "nub and node disagree on which files the coverage report lists. nub's \
         injected --test-coverage-exclude either turned off Node's default \
         test-file exclusion, or re-stated it on a Node that has none."
    );
}

/// A user-supplied `--test-coverage-exclude` disables Node's default exclusion for
/// node too, so nub must not re-add it — otherwise nub silently excludes test files
/// a user asked to see.
#[test]
fn a_user_supplied_exclude_matches_the_host_node() {
    if !host_node_usable() {
        eprintln!("skipping coverage user-exclude parity: no usable node on PATH");
        return;
    }
    if !host_node_has_coverage_exclude() {
        eprintln!(
            "skipping coverage user-exclude parity: this node predates \
             --test-coverage-exclude (22.5), so there is no control to diff against"
        );
        return;
    }
    let fixture = Fixture::new();
    let user_exclude = ["--test-coverage-exclude=**/no-such-dir/**"];

    let node = reported_files("node", fixture.command(Path::new("node"), &user_exclude));
    let nub = reported_files("nub", fixture.command(&nub_binary(), &user_exclude));

    // Positive control: this exclude matches nothing, so node must still report the
    // test file — which is exactly what a re-stated default pattern would take away.
    assert!(
        node.contains(&"logic.test.js".to_string()),
        "control failed — a no-op user exclude should leave node's default \
         exclusion off, so the test file stays in the report: {node:?}"
    );
    assert_eq!(
        nub, node,
        "nub applied an exclusion node did not, on top of a user-supplied \
         --test-coverage-exclude"
    );
}

/// A grandchild the fixture spawns through `process.execPath` never passes through
/// nub but inherits nub's NODE_OPTIONS. Node excludes a file when ANY exclude glob
/// matches it, so a default pattern carried in NODE_OPTIONS could not be undone by
/// the grandchild's own negated glob — nub therefore restates the default only on
/// its own argv. The grandchild here negates everything, so node lists both files.
#[test]
fn a_grandchild_s_own_exclude_matches_the_host_node() {
    if !host_node_usable() {
        eprintln!("skipping coverage grandchild-exclude parity: no usable node on PATH");
        return;
    }
    if !host_node_has_coverage_exclude() {
        eprintln!(
            "skipping coverage grandchild-exclude parity: this node predates \
             --test-coverage-exclude (22.5), so there is no control to diff against"
        );
        return;
    }
    let fixture = Fixture::new();
    std::fs::write(
        fixture.project.join("spawn.mjs"),
        "import { spawnSync } from 'node:child_process';\n\
         const r = spawnSync(process.execPath, ['--test', '--experimental-test-coverage',\n\
         '--test-reporter=tap', '--test-coverage-exclude=!**/*.js', 'logic.test.js'],\n\
         { stdio: 'inherit' });\n\
         process.exit(r.status ?? 1);\n",
    )
    .unwrap();
    let outer = |binary: &Path| {
        let mut command = Command::new(binary);
        command
            .current_dir(&fixture.project)
            .env("XDG_CONFIG_HOME", fixture._temp.path().join("config"))
            .env("XDG_CACHE_HOME", fixture._temp.path().join("cache"))
            .env_remove("NODE_OPTIONS")
            .arg("spawn.mjs");
        command
    };

    let node = reported_files("node", outer(Path::new("node")));
    let nub = reported_files("nub", outer(&nub_binary()));

    // Positive control: the negated glob turns node's default off, so the test file
    // is in node's report — exactly the row a NODE_OPTIONS default would remove.
    assert!(
        node.contains(&"logic.test.js".to_string()),
        "control failed — a grandchild negating every exclude should list its test \
         file under node: {node:?}"
    );
    assert_eq!(
        nub, node,
        "a grandchild's own --test-coverage-exclude was overridden by an exclude nub \
         carried in NODE_OPTIONS"
    );
}

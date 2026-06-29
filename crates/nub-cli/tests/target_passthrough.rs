//! Target-token termination + verbatim arg/flag passthrough — the durable,
//! oracle-grounded contract across every nub command that takes a target token
//! (`nub <file>`, `nub run`, `nub exec`, `nub watch`). The model is node's own
//! grammar: the first resolved target token ENDS runner/flag parsing, and
//! everything after it is the target's, forwarded byte-for-byte. A future
//! regression in any command's split — dropping a flag, stealing a trailing
//! flag, mis-binding a value flag's value as the target — fails here.
//!
//! The post-target `--` is KEPT VERBATIM by every command (decided 2026-06-28,
//! Option A): the target token ends runner parsing, and `--` is neither required
//! nor special-cased — uniform across file/run/exec/watch and byte-identical to
//! `node` and `pnpm 10`. (npm/yarn/bun strip it; nub deliberately does not.)
//!
//! Golden values are captured from the real reference tools (offline, so the
//! tests stay deterministic and CI-cheap):
//!   - node 26.2.0  — oracle for the file runner and `nub watch` (file-runs)
//!   - pnpm 10.15.1 — oracle for `nub run` / `nub exec` (also keeps `--`)

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/ or release/
    path.push("nub");
    path
}

fn fixture_dir() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    Path::new(&manifest).join("../../tests/fixtures/target-passthrough")
}

fn svec(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// Pull the fixture's `ARGV:[...]` line out of captured output and parse the
/// JSON array — the exact argv the executed target received.
fn parse_argv(stdout: &str, stderr: &str) -> Vec<String> {
    let line = stdout
        .lines()
        .find_map(|l| l.strip_prefix("ARGV:"))
        .unwrap_or_else(|| {
            panic!("fixture never emitted an ARGV line\nstdout: {stdout:?}\nstderr: {stderr:?}")
        });
    serde_json::from_str(line).expect("fixture must emit a JSON argv array")
}

/// Run `nub <args>` in the fixture dir and return the argv the target received.
/// Used for the file runner, `nub run`, and `nub exec` — all of which exit
/// cleanly after echoing.
fn forwarded_argv(args: &[&str]) -> Vec<String> {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(fixture_dir())
        .output()
        .expect("spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    parse_argv(&stdout, &stderr)
}

/// `nub watch` never exits (it re-runs on change), and its own control output
/// races stdout — so the watched run writes its argv to a sentinel file (via
/// ARGV_OUT) which we poll for, then kill the watcher. Same approach as the
/// other `nub watch` integration test.
fn watch_argv(args: &[&str]) -> Vec<String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let sentinel = std::env::temp_dir().join(format!(
        "nub-ttph-watch-{}-{}.json",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&sentinel);

    let mut child = Command::new(nub_binary())
        .args(args)
        .current_dir(fixture_dir())
        .env("ARGV_OUT", &sentinel)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn nub watch");

    let mut contents = None;
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(&sentinel)
            && s.starts_with("ARGV:")
        {
            contents = Some(s);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&sentinel);

    let s = contents.expect("`nub watch` probe never ran (no sentinel written)");
    parse_argv(&s, "")
}

/// The file runner is the gold standard: node-identical, ALWAYS keeps `--`,
/// NEVER requires it. (node 26.2.0 oracle, captured.)
#[test]
fn file_runner_forwards_verbatim_like_node() {
    let cases: &[(&[&str], &[&str])] = &[
        // post-target `--` is kept (the property the runners diverge on)
        (
            &["echo-argv.js", "--", "--foo", "bar"],
            &["--", "--foo", "bar"],
        ),
        // flags after the file pass straight through — `--` not required
        (
            &["echo-argv.js", "--foo", "bar", "-x"],
            &["--foo", "bar", "-x"],
        ),
        // every `--` is literal: repeated separators all survive
        (
            &["echo-argv.js", "--", "a", "--", "b"],
            &["--", "a", "--", "b"],
        ),
        // a lone trailing `--` survives (node keeps it)
        (&["echo-argv.js", "--"], &["--"]),
    ];
    for (input, want) in cases {
        assert_eq!(
            forwarded_argv(input),
            svec(want),
            "file runner argv for `nub {}`",
            input.join(" ")
        );
    }
}

/// `nub run` and `nub exec` forward everything after the target VERBATIM, keeping
/// the post-target `--` — byte-identical to `pnpm 10 run`/`pnpm 10 exec` and to
/// the file runner (Option A). `--` is never required.
#[test]
fn run_and_exec_keep_post_target_dashdash_like_pnpm() {
    for &(verb, target) in &[("run", "echo"), ("exec", "echo-argv")] {
        // Flags after the target reach the target — no `--` required.
        assert_eq!(
            forwarded_argv(&[verb, target, "--foo", "bar"]),
            svec(&["--foo", "bar"]),
            "{verb}: flags after the target must pass through"
        );

        // The post-target `--` is kept verbatim (= node / pnpm 10).
        assert_eq!(
            forwarded_argv(&[verb, target, "--", "--foo", "bar"]),
            svec(&["--", "--foo", "bar"]),
            "{verb}: post-target `--` must be kept"
        );

        // Every `--` is literal — a repeated separator survives in full.
        assert_eq!(
            forwarded_argv(&[verb, target, "--", "a", "--", "b"]),
            svec(&["--", "a", "--", "b"]),
            "{verb}: a repeated `--` is literal and kept"
        );
    }
}

/// The target boundary is resolved correctly before the passthrough suffix
/// begins — consumed/`=`-joined runner flags and value flags whose value looks
/// like the target don't shift where forwarding starts.
#[test]
fn target_boundary_is_resolved_before_passthrough() {
    // file: a consumed `=`-joined nub flag before the file is not forwarded.
    assert_eq!(
        forwarded_argv(&["--color=always", "echo-argv.js", "q"]),
        svec(&["q"]),
        "a consumed `=`-joined nub flag must not leak into argv"
    );

    // file: a value flag (`--env-file`) whose VALUE looks like the target binds
    // the value; the real file positional follows, so only `x` is forwarded.
    assert_eq!(
        forwarded_argv(&["--env-file", "echo-argv.js", "echo-argv.js", "x"]),
        svec(&["x"]),
        "a value-flag value that looks like the target must bind to the flag"
    );

    // file: a REPEATED value flag before the target — each occurrence consumes its
    // own value (both fixture files exist), and the target is the token after the
    // last one. Only `z` is forwarded.
    assert_eq!(
        forwarded_argv(&[
            "--env-file",
            "echo-argv.js",
            "--env-file",
            "package.json",
            "echo-argv.js",
            "z",
        ]),
        svec(&["z"]),
        "a repeated value flag must consume each value; the target follows the last"
    );

    // run: a `=`-joined consumed runner flag before the script doesn't leak.
    assert_eq!(
        forwarded_argv(&["run", "--reporter=silent", "echo", "aa", "bb"]),
        svec(&["aa", "bb"]),
        "a consumed `=`-joined run flag must not leak into the script's argv"
    );

    // run: a script forced via the LEADING `--` separator (before the target) —
    // that `--` ends runner options and is consumed (pnpm 10 + node), distinct
    // from a post-target `--`, which is kept.
    assert_eq!(
        forwarded_argv(&["run", "--", "echo", "zz"]),
        svec(&["zz"]),
        "a leading `--` ends runner options and is consumed; the script gets the rest"
    );
}

/// A target whose name LOOKS like a flag is still the target, not parsed as one.
#[test]
fn target_named_like_a_flag() {
    // file: a path whose basename looks like a flag (`./-x.js`). The `./` marks
    // it a path; node-identical, `--` kept. Built in a temp dir to avoid
    // committing a `-`-prefixed filename.
    let dir = std::env::temp_dir().join(format!("nub-ttph-dashfile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("-x.js"),
        "console.log(\"ARGV:\" + JSON.stringify(process.argv.slice(2)));\n",
    )
    .unwrap();
    let out = Command::new(nub_binary())
        .args(["./-x.js", "--", "p"])
        .current_dir(&dir)
        .output()
        .expect("spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        parse_argv(&stdout, &String::from_utf8_lossy(&out.stderr)),
        svec(&["--", "p"]),
        "a file path whose name looks like a flag is the target, `--` kept"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// No target at all: `nub run` lists scripts and exits 0 (pnpm parity) — it must
/// not treat a later token as a phantom target or error.
#[test]
fn no_target_lists_and_succeeds() {
    let out = Command::new(nub_binary())
        .arg("run")
        .current_dir(fixture_dir())
        .output()
        .expect("spawn nub");
    assert_eq!(
        out.status.code(),
        Some(0),
        "bare `nub run` must list scripts and exit 0\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The watch fix: `nub watch <file>` (subcommand) and `nub --watch <file>`
/// (flag) are two spellings of the same file-run and MUST agree — both keep
/// `--` verbatim, byte-identical to `node --watch <file> -- …`. The subcommand
/// form previously stripped the first `--` via the shared runner split; watch is
/// now exempt because it is a file-run, not a pnpm-style runner.
#[test]
fn watch_subcommand_and_flag_keep_dashdash_like_node() {
    let want = svec(&["--", "--foo", "bar"]);
    assert_eq!(
        watch_argv(&["watch", "echo-argv.js", "--", "--foo", "bar"]),
        want,
        "`nub watch <file>` (subcommand) must keep `--`"
    );
    assert_eq!(
        watch_argv(&["--watch", "echo-argv.js", "--", "--foo", "bar"]),
        want,
        "`nub --watch <file>` (flag) must keep `--` — and equal the subcommand form"
    );
}

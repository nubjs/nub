//! A `node` that leads back to nub must end in one error, never a chain of nubs.
//!
//! On 2026-11-04 a test bin dir held `node -> <nub's node shim>`; every nub that
//! resolved `node` found that link, probed it, and the probe was nub again,
//! about 7,000 levels deep before the test exited. Each earlier fix recognised
//! one more such shape. These tests use shapes nothing recognises: `bin/node`
//! is a shell script that runs nub as `node`, so path and identity checks see an
//! ordinary script. With `exec` it keeps the pid nub spawned, as a symlink, hard
//! link or copy of nub does; without it the shell stays in between, as a
//! wrapper or a spawning version manager does.
//!
//! Every wrapper counts its invocations and refuses past a cap, so a regression
//! fails here instead of filling the process table.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const WRAPPER_CAP: usize = 200;

/// `nub_core::node::nesting::MAX_DEPTH`; the CLI's integration tests do not link
/// nub-core.
const MAX_DEPTH: usize = 64;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

fn tmp(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-launch-loop-{tag}-{}-{nanos:x}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// A real Node binary from the inherited PATH, never one of nub's own shims.
fn real_node() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|dir| !dir.to_string_lossy().contains("node-shim"))
        .map(|dir| dir.join("node"))
        .find(|node| node.is_file())
        .and_then(|node| node.canonicalize().ok())
}

#[derive(Clone, Copy, Debug)]
enum Hop {
    /// `exec`s nub, keeping the pid: what a link or copy of nub amounts to.
    Exec,
    /// Runs nub as a child of the shell.
    Spawn,
    /// Answers the probes itself, naming another binary as `process.execPath` the
    /// way a shim's Node would, and runs nub for everything else: every probe
    /// passes and only a launch comes back to nub.
    AnswerProbes,
}

struct Fixture {
    root: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new(tag: &str, hop: Hop) -> Self {
        let root = tmp(tag);
        let bin = root.join("bin");
        let nub_as_node = root.join("nub-as-node");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&nub_as_node).unwrap();
        std::os::unix::fs::symlink(nub_binary(), nub_as_node.join("node")).unwrap();
        let log = root.join("calls.log");
        let run = format!("'{}' \"$@\"", nub_as_node.join("node").display());
        let body = match hop {
            Hop::Exec => format!("exec {run}"),
            Hop::Spawn => run,
            Hop::AnswerProbes => format!(
                "case \"$1\" in\n\
                 --version) echo v26.0.0; exit 0 ;;\n\
                 -e) printf 'v26.0.0\\n/nonexistent/node'; exit 0 ;;\n\
                 esac\n{run}"
            ),
        };
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{log}'\n\
             if [ \"$(wc -l < '{log}')\" -gt {WRAPPER_CAP} ]; then echo 'wrapper cap' >&2; exit 99; fi\n\
             {body}\n",
            log = log.display(),
        );
        let node = bin.join("node");
        std::fs::write(&node, script).unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { root, bin }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.root.join("calls.log")).unwrap_or_default()
    }

    fn calls(&self) -> usize {
        self.log().lines().count()
    }

    /// Runs nub in the fixture with `bin/` first on PATH, then `later` dirs.
    fn run(&self, later: &[PathBuf], args: &[&str], manifest: &str) -> Output {
        std::fs::write(self.root.join("package.json"), manifest).unwrap();
        let path = std::env::join_paths(
            std::iter::once(self.bin.clone())
                .chain(later.iter().cloned())
                .chain([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]),
        )
        .unwrap();
        Command::new(nub_binary())
            .args(args)
            .current_dir(&self.root)
            .env("PATH", path)
            // A fresh version cache, so every probe through the wrapper runs.
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("HOME", self.root.join("home"))
            .env_remove("NODE_COMPAT")
            .env_remove("NODE_OPTIONS")
            .env_remove("NODE_EXECUTABLE")
            .env_remove("__NUB_NESTING")
            .output()
            .expect("spawn nub")
    }
}

const PLAIN: &str = r#"{"name":"p","private":true}"#;

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn a_node_that_starts_nub_again_is_skipped_for_the_next_node_on_path() {
    let Some(node) = real_node() else {
        eprintln!("skipping: no Node on PATH");
        return;
    };
    for hop in [Hop::Exec, Hop::Spawn] {
        let fx = Fixture::new("skip", hop);
        std::fs::write(fx.root.join("run.js"), "console.log(process.execPath)").unwrap();
        let out = fx.run(&[node.parent().unwrap().to_path_buf()], &["run.js"], PLAIN);
        assert_eq!(out.status.code(), Some(0), "{hop:?}: {}", stderr(&out));
        assert_eq!(
            Path::new(String::from_utf8_lossy(&out.stdout).trim()),
            node,
            "{hop:?}: the run lands on the Node after the wrapper"
        );
        assert_eq!(
            fx.calls(),
            1,
            "{hop:?}: only the version probe reaches the wrapper"
        );
    }
}

/// The incident's shape: a lifecycle script runs `node --version` with nothing
/// but the looping `node` on PATH.
#[test]
fn a_lifecycle_script_through_such_a_node_fails_once_instead_of_recursing() {
    let manifest = r#"{"name":"p","version":"1.0.0","private":true,"scripts":{"postinstall":"node --version"}}"#;
    for hop in [Hop::Exec, Hop::Spawn] {
        let fx = Fixture::new("lifecycle", hop);
        let out = fx.run(&[], &["install"], manifest);
        let err = stderr(&out);
        assert_ne!(out.status.code(), Some(0), "{hop:?}: {err}");
        assert_eq!(
            err.matches("ERR_NUB_NODE_SHIM_LOOP").count(),
            1,
            "{hop:?}: one error naming the loop: {err}"
        );
        assert!(
            err.contains(&fx.bin.join("node").display().to_string()),
            "{hop:?}: the error names the looping `node`: {err}"
        );
        // A probe or two per nub process, never one per level of a chain.
        assert!(fx.calls() < 10, "{hop:?}: {} wrapper calls", fx.calls());
    }
}

/// A `node` whose probe passes but whose every launch comes back to nub is
/// invisible to the probe; the recursion bound ends it.
#[test]
fn a_launch_loop_stops_at_the_recursion_bound_with_one_error() {
    let fx = Fixture::new("launch", Hop::AnswerProbes);
    std::fs::write(fx.root.join("run.js"), "console.log('ran')").unwrap();
    // `--node` keeps every level vanilla, so each one is a launch and nothing else.
    let out = fx.run(&[], &["--node", "run.js"], PLAIN);
    let err = stderr(&out);
    assert_ne!(out.status.code(), Some(0), "{err}");
    assert_eq!(
        err.matches("ERR_NUB_LAUNCH_LOOP").count(),
        1,
        "one error, not one per level: {err}"
    );
    assert!(
        err.contains(&fx.bin.join("node").display().to_string()),
        "the error names the `node` that led back to nub: {err}"
    );
    let launches = fx.log().lines().filter(|l| l.contains("run.js")).count();
    assert!(
        (MAX_DEPTH - 2..=MAX_DEPTH + 2).contains(&launches),
        "the chain stops at the bound, not at the wrapper's cap: {launches} launches"
    );
}

/// A real Node between two nubs is nesting, so scripts can nest `node` to any depth
/// and a daemon that restarts itself through `node` never accumulates a count.
#[test]
fn launching_a_real_node_clears_the_count() {
    let Some(node) = real_node() else {
        eprintln!("skipping: no Node on PATH");
        return;
    };
    let root = tmp("clears");
    std::fs::write(root.join("package.json"), PLAIN).unwrap();
    std::fs::write(
        root.join("level.js"),
        "const n = Number(process.argv[2]);\n\
         if (n < 3) process.stdout.write(require('child_process').execSync(`node level.js ${n + 1}`));\n\
         else console.log(`depth ${n}`);\n",
    )
    .unwrap();
    let path = std::env::join_paths([
        node.parent().unwrap().to_path_buf(),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ])
    .unwrap();
    // One level below the bound: every nested nub past the first would refuse if the
    // count survived the Node above it.
    let out = Command::new(nub_binary())
        .args(["level.js", "0"])
        .current_dir(&root)
        .env("PATH", path)
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("__NUB_NESTING", (MAX_DEPTH - 1).to_string())
        .env_remove("NODE_COMPAT")
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_EXECUTABLE")
        .output()
        .expect("spawn nub");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "depth 3");
}

/// A pin names the Node to use, so a `node` that leads back to nub is no reason to
/// stop. The mirror refuses the connection: nub reporting that download failing is
/// the proof it went on to provision, with no network in the test.
#[test]
fn a_pinned_project_provisions_past_a_node_that_leads_back_to_nub() {
    let fx = Fixture::new("pinned", Hop::Spawn);
    std::fs::write(fx.root.join(".node-version"), "26.0.0\n").unwrap();
    std::fs::write(
        fx.root.join(".npmrc"),
        "node-mirror:release=http://127.0.0.1:1/\n",
    )
    .unwrap();
    let out = fx.run(&[], &["-p", "process.version"], PLAIN);
    let err = stderr(&out);
    assert!(
        err.contains("ERR_NUB_NODE_PROVISION_FAILED") && !err.contains("ERR_NUB_NODE_SHIM_LOOP"),
        "exit {:?}; stderr:\n{err}",
        out.status.code()
    );
}

/// `process.execve` keeps the pid, so a real Node that re-execs through nub's `node`
/// re-enters nub as the direct child of the nub that launched it. That Node is still
/// the one to run; nothing here leads back to nub.
#[test]
fn a_real_node_that_re_execs_itself_through_nubs_node_stays_on_that_node() {
    let Some(node) = real_node() else {
        eprintln!("skipping: no Node on PATH");
        return;
    };
    let root = tmp("execve");
    std::fs::write(root.join("package.json"), PLAIN).unwrap();
    std::fs::write(
        root.join("reexec.js"),
        "if (process.argv[2]) { console.log(`again on ${process.execPath}`); process.exit(0); }\n\
         if (!process.execve) { console.log('no execve'); process.exit(0); }\n\
         const node = require('child_process').execSync('command -v node', { encoding: 'utf8' }).trim();\n\
         console.log(`through ${node}`);\n\
         process.execve(node, ['node', __filename, 'again'], process.env);\n",
    )
    .unwrap();
    let path = std::env::join_paths([
        node.parent().unwrap().to_path_buf(),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ])
    .unwrap();
    let out = Command::new(nub_binary())
        .arg("reexec.js")
        .current_dir(&root)
        .env("PATH", path)
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env_remove("NODE_COMPAT")
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_EXECUTABLE")
        .env_remove("__NUB_NESTING")
        .output()
        .expect("spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim() == "no execve" {
        eprintln!("skipping: this Node has no process.execve");
        return;
    }
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stdout.contains("through ") && stdout.contains("nub-node-shim-"),
        "the re-exec must go through nub's own `node`; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!("again on {}", node.display())),
        "stdout:\n{stdout}"
    );
}

#[test]
fn nub_refuses_to_start_one_level_past_the_bound() {
    let at_bound = Command::new(nub_binary())
        .arg("--version")
        .env("__NUB_NESTING", MAX_DEPTH.to_string())
        .output()
        .unwrap();
    assert_ne!(at_bound.status.code(), Some(0));
    assert!(
        stderr(&at_bound).contains("ERR_NUB_LAUNCH_LOOP"),
        "{}",
        stderr(&at_bound)
    );

    let below = Command::new(nub_binary())
        .arg("--version")
        .env("__NUB_NESTING", (MAX_DEPTH - 1).to_string())
        .output()
        .unwrap();
    assert_eq!(below.status.code(), Some(0), "{}", stderr(&below));
}

#[test]
fn an_explicit_node_executable_that_starts_nub_is_refused_by_name() {
    let fx = Fixture::new("override", Hop::Spawn);
    std::fs::write(fx.root.join("run.js"), "console.log('ran')").unwrap();
    std::fs::write(fx.root.join("package.json"), PLAIN).unwrap();
    let out = Command::new(nub_binary())
        .arg("run.js")
        .current_dir(&fx.root)
        .env("NODE_EXECUTABLE", fx.bin.join("node"))
        .env("XDG_CACHE_HOME", fx.root.join("cache"))
        .env_remove("NODE_COMPAT")
        .env_remove("__NUB_NESTING")
        .output()
        .unwrap();
    let err = stderr(&out);
    assert_ne!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("ERR_NUB_NODE_EXECUTABLE_SELF"), "{err}");
    assert_eq!(fx.calls(), 1, "refused after one probe");
}

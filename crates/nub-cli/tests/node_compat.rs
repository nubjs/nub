//! Node.js compatibility tests.
//! Reads tests/node-compat-config.jsonc, runs each test through nub,
//! and asserts exit code 0.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Per-test wall-clock budget. Some upstream Node tests block forever when run
/// outside Node's own `tools/test.py` harness (which supplies its own timeout):
/// they open a server/socket or read stdin and wait. Run sequentially with no
/// timeout, one such test stalls the WHOLE suite indefinitely — locally and in
/// CI (where the only backstop is the 6h job timeout). A timed-out test is
/// killed and counted as a FAILURE, not a silent skip, so a genuine hang
/// surfaces loudly instead of masquerading as green. 30s is well above the
/// runtime of any non-hanging entry while bounding the worst case.
const PER_TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Outcome of running one compat entry under the timeout budget.
enum RunOutcome {
    Passed,
    Failed(String),
    TimedOut,
}

/// Spawn `nub <test>` and wait up to `timeout`, killing the child if it
/// overruns. Polls `try_wait` rather than blocking on `output()` so a hung
/// child can't wedge the suite. stdout is discarded; stderr is captured for the
/// failure report.
///
/// `tmp` is a per-worker scratch directory exported as `TMPDIR`: the suite runs
/// across several worker threads (below), and many upstream Node tests write to
/// a shared `os.tmpdir()` path keyed only by the test name — so two workers
/// running different tests can still collide on the same scratch file. Giving
/// each worker its own `TMPDIR` isolates that scratch and removes the
/// false-positive failures documented in tests/node-compat-failures/parallel.md
/// ("54 were false positives from tmpdir collisions in 20-way parallel
/// execution"). `serial_id` is exported as `TEST_SERIAL_ID`, which is what
/// `test/common/tmpdir.js` actually keys its `.tmp.<id>` directory on — without
/// it every worker shares `.tmp.0` and each `tmpdir.refresh()` wipes a sibling's.
///
/// `flags` are the test's own `// Flags:` tokens, passed to nub exactly as
/// tests/cross-runtime/run.mjs passes them, with `NODE_SKIP_FLAG_CHECK=1` so
/// `test/common` does not re-spawn the test itself. That re-spawn goes through
/// `process.execPath`, which under nub is the real `node` binary — so without
/// this every flagged test silently ran WITHOUT nub's augmentation and the gate
/// could not see a regression in it. `cwd` is the suite ROOT (the Node checkout,
/// not its `test/` dir), as in Node's own runner: flag paths such as
/// `--experimental-loader ./test/fixtures/...` are written relative to it.
fn run_with_timeout(
    nub: &Path,
    test_path: &Path,
    flags: &[String],
    cwd: &Path,
    tmp: &Path,
    serial_id: usize,
    timeout: Duration,
) -> RunOutcome {
    let mut cmd = Command::new(nub);
    cmd.args(flags)
        .arg(test_path)
        .current_dir(cwd)
        .env("NODE_TEST_KNOWN_GLOBALS", "0")
        .env("NODE_SKIP_FLAG_CHECK", "1")
        .env("NO_COLOR", "1")
        .env("TMPDIR", tmp)
        .env("TEST_SERIAL_ID", serial_id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // Each test runs in a session of its own, so `reap_session` can find what it
    // leaves behind. A process group is not enough: Nub puts Node in a group of
    // its own (`group_on_spawn`), so the servers, workers and `process.execPath`
    // children a test starts sit in Node's group, not Nub's, but they all stay in
    // this session. Killing only the leader orphaned them to PID 1, where they
    // kept running.
    #[cfg(unix)]
    // SAFETY: setsid(2) is async-signal-safe and touches no parent state.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut cmd, || {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return RunOutcome::Failed(format!("spawn error: {e}")),
    };
    let mut stderr = StderrDrain::new(child.stderr.take());

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // A clean leader exit can still leave a server or worker running.
                reap_session(&child);
                let stderr = stderr.finish();
                if status.success() {
                    return RunOutcome::Passed;
                }
                let code = status.code().unwrap_or(-1);
                let snippet: String = stderr.chars().take(200).collect();
                let snippet = if snippet.is_empty() {
                    snippet
                } else {
                    format!("\n  {snippet}")
                };
                return RunOutcome::Failed(format!("exit {code}{snippet}"));
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    reap_session(&child);
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stderr.finish();
                    return RunOutcome::TimedOut;
                }
                stderr.pump();
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return RunOutcome::Failed(format!("wait error: {e}")),
        }
    }
}

/// A test's stderr, read WHILE it runs: reading only after exit deadlocks a
/// chatty test, which blocks on its next write once the pipe fills and is then
/// misreported as a hang (test-stream2-large-read-stall.js logs every push and
/// read).
///
/// On unix the pipe is non-blocking and read from the poll loop, then once more
/// after the reap, and closed. A blocking read would last until every holder of
/// the pipe exits, and one `reap_session` cannot reach (a `detached: true`
/// child starts its own session, as test-listen-fd-detached-inherit.js does
/// with stderr inherited) would hold up the whole suite, or leave a reader
/// thread behind per test.
#[cfg(unix)]
struct StderrDrain {
    pipe: Option<std::process::ChildStderr>,
    buf: Vec<u8>,
}

#[cfg(unix)]
impl StderrDrain {
    fn new(pipe: Option<std::process::ChildStderr>) -> Self {
        if let Some(pipe) = &pipe {
            use std::os::fd::AsRawFd;
            let fd = pipe.as_raw_fd();
            // SAFETY: fcntl(2) on a descriptor this drain owns.
            unsafe {
                libc::fcntl(
                    fd,
                    libc::F_SETFL,
                    libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK,
                );
            }
        }
        Self {
            pipe,
            buf: Vec::new(),
        }
    }

    /// Read whatever is buffered without waiting for more.
    fn pump(&mut self) {
        let Some(pipe) = &mut self.pipe else { return };
        let mut chunk = [0u8; 8192];
        let open = loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break false,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => break e.kind() == std::io::ErrorKind::WouldBlock,
            }
        };
        if !open {
            self.pipe = None;
        }
    }

    fn finish(mut self) -> String {
        self.pump();
        String::from_utf8_lossy(&self.buf).into_owned()
    }
}

/// Off unix there is no session to reap, so stderr is read on a thread and
/// joined after exit.
#[cfg(not(unix))]
struct StderrDrain(Option<std::thread::JoinHandle<String>>);

#[cfg(not(unix))]
impl StderrDrain {
    fn new(pipe: Option<std::process::ChildStderr>) -> Self {
        Self(pipe.map(|mut pipe| {
            std::thread::spawn(move || {
                let mut stderr = String::new();
                let _ = pipe.read_to_string(&mut stderr);
                stderr
            })
        }))
    }

    fn pump(&mut self) {}

    fn finish(self) -> String {
        self.0.and_then(|h| h.join().ok()).unwrap_or_default()
    }
}

/// SIGKILL every process in the test's session: the child is the session leader,
/// so this is its whole subtree and nothing outside it. A process can fork
/// between a scan and its kill, so scan again until a pass turns up nothing new.
/// Off unix the caller's `child.kill()` is the only cleanup.
fn reap_session(child: &std::process::Child) {
    #[cfg(unix)]
    {
        let sid = child.id() as libc::pid_t;
        let mut killed = std::collections::HashSet::new();
        for _ in 0..10 {
            let fresh: Vec<libc::pid_t> = all_pids()
                .into_iter()
                // SAFETY: getsid(2) and kill(2) take no pointers.
                .filter(|&pid| unsafe { libc::getsid(pid) } == sid && killed.insert(pid))
                .collect();
            if fresh.is_empty() {
                break;
            }
            for pid in fresh {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }
    #[cfg(not(unix))]
    let _ = child;
}

#[cfg(target_os = "macos")]
fn all_pids() -> Vec<libc::pid_t> {
    // SAFETY: a null buffer only asks for the count; the second call writes at
    // most `buf.len()` pids, and both return a count of pids, not bytes.
    unsafe {
        let n = libc::proc_listallpids(std::ptr::null_mut(), 0).max(0) as usize;
        let mut buf: Vec<libc::pid_t> = vec![0; n + 64];
        let size = (buf.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        let got = libc::proc_listallpids(buf.as_mut_ptr().cast(), size).max(0) as usize;
        buf.truncate(got);
        buf
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn all_pids() -> Vec<libc::pid_t> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect()
}

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

fn suite_dir() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    Path::new(&manifest).join("../../tests/node-suite/test")
}

fn config_path() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    Path::new(&manifest).join("../../tests/node-compat-config.jsonc")
}

/// The tokens of the test's first `// Flags:` directive, if any — Node's own
/// runner passes these to the binary, and so does the cross-runtime harness
/// that generated the config, so the gate must too or it judges a different
/// program than the one the config entry describes.
fn test_flags(test_path: &Path) -> Vec<String> {
    let content = fs::read_to_string(test_path).unwrap_or_default();
    content
        .lines()
        .find_map(|line| line.strip_prefix("// Flags: "))
        .map(|rest| rest.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default()
}

struct TestEntry {
    path: String,
    /// A classified divergence (nub fails, node passes) with its reason.
    ignore: bool,
    /// A divergence the generator could not classify. Skipped like `ignore`,
    /// but counted and reported apart so it cannot hide among the classified.
    untriaged: bool,
}

fn load_config() -> Vec<TestEntry> {
    let content = fs::read_to_string(config_path()).unwrap_or_default();
    let stripped: String = content
        .lines()
        .map(|line| {
            if let Some(pos) = line.find("//") {
                &line[..pos]
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let parsed: serde_json::Value = serde_json::from_str(&stripped).unwrap_or_default();
    let obj = parsed.as_object().unwrap();

    let flag = |opts: &serde_json::Value, key: &str| {
        opts.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
    };
    obj.iter()
        .map(|(path, opts)| TestEntry {
            path: path.clone(),
            ignore: flag(opts, "ignore"),
            untriaged: flag(opts, "untriaged"),
        })
        .collect()
}

/// The full Node-suite compatibility corpus — ~5,200 black-box `nub` spawns.
///
/// `#[ignore]` by design: this is a CI-scale gate, not a unit test, and running
/// it inline would turn every `cargo test` (and every workflow build gate) into
/// a multi-minute job that blows past ordinary timeouts. The default `cargo
/// test` therefore runs only the fast unit + integration path; the gates ride on
/// that. Run the corpus explicitly:
///   cargo test -p nub-cli --test node_compat -- --ignored --nocapture
/// or via tests/run-node-compat.sh. Requires the
/// tests/node-suite submodule (`git submodule update --init --checkout --depth 1
/// tests/node-suite`).
#[test]
#[ignore = "heavyweight Node-suite compat corpus — run via `cargo test -p nub-cli --test node_compat -- --ignored` (needs the tests/node-suite submodule)"]
fn node_compat_suite() {
    let suite = suite_dir();
    // Fail LOUDLY when the suite is absent. A silent `return;` here let a
    // missing submodule read as a green test — the compat gate would "pass"
    // having checked nothing. Nothing in CI populates tests/node-suite (its
    // corpus job was removed 2026-06-03); run `git submodule update --init
    // --checkout --depth 1 tests/node-suite` first. A missing suite is a setup
    // error, never a pass.
    assert!(
        suite.exists(),
        "Node compat suite missing at {suite:?}. The compat gate cannot run. \
         Initialize the submodule: `git submodule update --init --checkout --depth 1 tests/node-suite`. \
         (Refusing to skip silently — a vacuous pass would hide real compat regressions.)"
    );

    let entries = load_config();
    let nub = nub_binary();
    let suite_root = suite
        .parent()
        .expect("suite dir has a parent")
        .to_path_buf();

    // Resolve which entries actually run (and tally skips) before fanning out,
    // so the parallel section only does spawn work.
    let mut runnable: Vec<String> = Vec::new();
    let mut skipped = 0usize;
    let mut untriaged = 0usize;
    for entry in &entries {
        if entry.ignore {
            skipped += 1;
            continue;
        }
        if entry.untriaged {
            untriaged += 1;
            continue;
        }
        let test_path = suite.join(&entry.path);
        if !test_path.exists() {
            eprintln!("SKIP {}: not found", entry.path);
            skipped += 1;
            continue;
        }
        runnable.push(entry.path.clone());
    }

    // Fan the corpus across worker threads. Sequential, this is the runtime of
    // ~5,200 process spawns summed; parallel it's bounded by the slowest worker.
    // Each worker owns an isolated TMPDIR (see run_with_timeout) so the suite's
    // shared-tmpdir tests can't cross-collide. Within a worker, entries run
    // sequentially and each refreshes its own scratch, so reuse is safe.
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 16);
    let mut buckets: Vec<Vec<String>> = (0..workers).map(|_| Vec::new()).collect();
    for (i, path) in runnable.into_iter().enumerate() {
        buckets[i % workers].push(path);
    }

    // Pass 1 — parallel SCAN. Fast, but parallelism is load-bearing only for
    // throughput: timing/resource-sensitive entries (fs-watch, child-process,
    // anything racing a port or the scheduler) can spuriously fail under N-way
    // load even with isolated TMPDIRs. So a scan "failure" is a SUSPECT, not a
    // verdict — see tests/node-compat-failures/parallel.md, which records 54 such
    // false positives from a 20-way run that vanished in isolation.
    let (scan_passed, suspects) = std::thread::scope(|scope| {
        let handles: Vec<_> = buckets
            .into_iter()
            .enumerate()
            .map(|(wid, bucket)| {
                let nub = &nub;
                let suite = &suite;
                let suite_root = &suite_root;
                scope.spawn(move || {
                    let tmp = std::env::temp_dir()
                        .join(format!("nub-compat-{}-{wid}", std::process::id()));
                    let _ = fs::create_dir_all(&tmp);
                    let mut p = 0usize;
                    let mut suspect: Vec<String> = Vec::new();
                    for rel in &bucket {
                        let test_path = suite.join(rel);
                        let flags = test_flags(&test_path);
                        match run_with_timeout(
                            nub,
                            &test_path,
                            &flags,
                            suite_root,
                            &tmp,
                            wid,
                            PER_TEST_TIMEOUT,
                        ) {
                            RunOutcome::Passed => p += 1,
                            RunOutcome::Failed(_) | RunOutcome::TimedOut => {
                                suspect.push(rel.clone())
                            }
                        }
                    }
                    let _ = fs::remove_dir_all(&tmp);
                    (p, suspect)
                })
            })
            .collect();
        let mut total_pass = 0usize;
        let mut all_suspects: Vec<String> = Vec::new();
        for h in handles {
            let (p, s) = h.join().expect("a compat worker thread panicked");
            total_pass += p;
            all_suspects.extend(s);
        }
        (total_pass, all_suspects)
    });

    // Pass 2 — sequential RE-VERIFY. Each suspect runs ALONE on the full machine
    // (single TMPDIR, serial id 0); only a failure that reproduces in isolation is a
    // real failure. A suspect that now passes was a parallel-load false positive
    // and is credited as passed. This is the same parallel-scan-then-confirm
    // protocol the parallel.md investigation used by hand — kept honest in code so
    // the gate is fast AND can't fail green on a load artifact.
    let reverify_tmp =
        std::env::temp_dir().join(format!("nub-compat-reverify-{}", std::process::id()));
    let _ = fs::create_dir_all(&reverify_tmp);
    let mut passed = scan_passed;
    let mut failed = 0usize;
    let mut timed_out = 0usize;
    let mut false_positives = 0usize;
    if !suspects.is_empty() {
        eprintln!(
            "Re-verifying {} parallel-scan suspect(s) sequentially…",
            suspects.len()
        );
    }
    for rel in &suspects {
        let test_path = suite.join(rel);
        let flags = test_flags(&test_path);
        match run_with_timeout(
            &nub,
            &test_path,
            &flags,
            &suite_root,
            &reverify_tmp,
            0,
            PER_TEST_TIMEOUT,
        ) {
            RunOutcome::Passed => {
                passed += 1;
                false_positives += 1;
            }
            RunOutcome::Failed(detail) => {
                eprintln!("FAIL {rel}: {detail}");
                failed += 1;
            }
            RunOutcome::TimedOut => {
                eprintln!(
                    "TIMEOUT {rel}: exceeded {}s alone and was killed (real hang — counted as a \
                     failure, not silently passed)",
                    PER_TEST_TIMEOUT.as_secs()
                );
                timed_out += 1;
            }
        }
    }
    let _ = fs::remove_dir_all(&reverify_tmp);

    eprintln!(
        "\n=== Node compat: {passed}/{} passed ({failed} failed, {timed_out} timed out, \
         {skipped} skipped as classified divergences, {untriaged} skipped as UNTRIAGED \
         divergences — see tests/node-compat-config.jsonc) [{workers}-way scan, \
         {false_positives} parallel false positive(s) reclassified on sequential re-verify] ===",
        passed + failed + timed_out
    );
    assert_eq!(
        failed + timed_out,
        0,
        "{failed} Node compat tests failed, {timed_out} timed out (confirmed in isolation)"
    );
}

/// The reap reaches what a test leaves behind even though Nub runs Node in a
/// process group of its own: a child still holding the test's stderr, after a
/// clean exit and after a timeout. Without it, the exit case waited on that
/// child for as long as it lived.
#[cfg(unix)]
#[test]
fn leftover_processes_are_killed_when_a_test_exits_or_times_out() {
    let dir = std::env::temp_dir().join(format!("nub-compat-reap-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    for (name, ending, expected) in [
        ("exits.js", "c.unref();", "passed"),
        ("hangs.js", "setInterval(() => {}, 1000);", "timed out"),
    ] {
        let script = dir.join(name);
        fs::write(
            &script,
            format!(
                "const c = require('child_process').spawn(process.execPath, \
                 ['-e', 'setInterval(() => {{}}, 1000)'], {{ stdio: ['ignore', 'ignore', 'inherit'] }});\n\
                 require('fs').writeFileSync(__filename + '.pid', String(c.pid));\n{ending}\n"
            ),
        )
        .unwrap();

        let outcome = run_with_timeout(
            &nub_binary(),
            &script,
            &[],
            &dir,
            &dir,
            0,
            // Far above startup on a loaded machine, so only hangs.js reaches it.
            Duration::from_secs(10),
        );
        let pid: libc::pid_t = fs::read_to_string(dir.join(format!("{name}.pid")))
            .expect("the fixture never wrote its child's pid")
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while running(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let survived = running(pid);
        // SAFETY: kill(2) takes no pointers.
        unsafe { libc::kill(pid, libc::SIGKILL) };

        let got = match outcome {
            RunOutcome::Passed => "passed".to_string(),
            RunOutcome::Failed(detail) => format!("failed: {detail}"),
            RunOutcome::TimedOut => "timed out".to_string(),
        };
        assert_eq!(got, expected, "{name}: wrong outcome");
        assert!(
            !survived,
            "{name}: the child it left (pid {pid}) is still running"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

/// Alive and not a zombie: a killed orphan can wait a moment for its new parent
/// to reap it.
#[cfg(unix)]
fn running(pid: libc::pid_t) -> bool {
    let zombie = fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
        s.rsplit(')')
            .next()
            .is_some_and(|r| r.trim_start().starts_with('Z'))
    });
    // SAFETY: kill(2) with signal 0 only checks that the process exists.
    !zombie && unsafe { libc::kill(pid, 0) } == 0
}

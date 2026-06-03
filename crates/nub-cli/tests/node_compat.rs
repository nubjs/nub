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

/// Spawn `nub <test>` and wait up to [`PER_TEST_TIMEOUT`], killing the child if
/// it overruns. Polls `try_wait` rather than blocking on `output()` so a hung
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
/// execution"). `fork_id` additionally namespaces Node's own `.tmp.<id>` dir.
fn run_with_timeout(
    nub: &Path,
    test_path: &Path,
    cwd: &Path,
    tmp: &Path,
    fork_id: usize,
) -> RunOutcome {
    let mut child = match Command::new(nub)
        .arg(test_path)
        .current_dir(cwd)
        .env("NODE_TEST_KNOWN_GLOBALS", "0")
        .env("TMPDIR", tmp)
        .env("NODE_TEST_FORK_ID", fork_id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return RunOutcome::Failed(format!("spawn error: {e}")),
    };

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stderr = String::new();
                if let Some(mut s) = child.stderr.take() {
                    let _ = s.read_to_string(&mut stderr);
                }
                if status.success() {
                    return RunOutcome::Passed;
                }
                let code = status.code().unwrap_or(-1);
                let snippet = if stderr.len() > 200 {
                    format!("\n  {}", &stderr[..200])
                } else if !stderr.is_empty() {
                    format!("\n  {stderr}")
                } else {
                    String::new()
                };
                return RunOutcome::Failed(format!("exit {code}{snippet}"));
            }
            Ok(None) => {
                if start.elapsed() >= PER_TEST_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return RunOutcome::TimedOut;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return RunOutcome::Failed(format!("wait error: {e}")),
        }
    }
}

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    path.pop();
    path.push("nub");
    path
}

fn suite_dir() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    Path::new(&manifest).join("../../tests/node-suite/test")
}

fn config_path() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    Path::new(&manifest).join("../../tests/node-compat-config.jsonc")
}

fn has_internal_flags(test_path: &Path) -> bool {
    let content = fs::read_to_string(test_path).unwrap_or_default();
    let header: String = content.lines().take(20).collect::<Vec<_>>().join("\n");
    header.contains("--expose-internals")
        || header.contains("--allow-natives-syntax")
        || header.contains("--expose-externalize-string")
        || header.contains("--expose-gc")
}

struct TestEntry {
    path: String,
    ignore: bool,
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

    obj.iter()
        .map(|(path, opts)| TestEntry {
            path: path.clone(),
            ignore: opts
                .get("ignore")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        })
        .collect()
}

/// The full Node-suite compatibility corpus — ~2,554 black-box `nub` spawns.
///
/// `#[ignore]` by design: this is a CI-scale gate, not a unit test, and running
/// it inline would turn every `cargo test` (and every workflow build gate) into
/// a multi-minute job that blows past ordinary timeouts. The default `cargo
/// test` therefore runs only the fast unit + integration path; the gates ride on
/// that. Run the corpus explicitly:
///   cargo test -p nub-cli --test node_compat -- --ignored --nocapture
/// or via tests/run-node-compat.sh / the CI `compat` job. Requires the
/// tests/node-suite submodule (`git submodule update --init --depth 1
/// tests/node-suite`).
#[test]
#[ignore = "heavyweight Node-suite compat corpus — run via `cargo test -p nub-cli --test node_compat -- --ignored` or the CI compat job (needs the tests/node-suite submodule)"]
fn node_compat_suite() {
    let suite = suite_dir();
    // Fail LOUDLY when the suite is absent. A silent `return;` here let a
    // missing submodule read as a green test — the compat gate would "pass"
    // having checked nothing. CI initializes tests/node-suite (a registered
    // submodule); locally, run `git submodule update --init --depth 1
    // tests/node-suite`. A missing suite is a setup error, never a pass.
    assert!(
        suite.exists(),
        "Node compat suite missing at {suite:?}. The compat gate cannot run. \
         Initialize the submodule: `git submodule update --init --depth 1 tests/node-suite`. \
         (Refusing to skip silently — a vacuous pass would hide real compat regressions.)"
    );

    let entries = load_config();
    let nub = nub_binary();

    // Resolve which entries actually run (and tally skips) before fanning out,
    // so the parallel section only does spawn work.
    let mut runnable: Vec<String> = Vec::new();
    let mut skipped = 0usize;
    for entry in &entries {
        if entry.ignore {
            skipped += 1;
            continue;
        }
        let test_path = suite.join(&entry.path);
        if !test_path.exists() {
            eprintln!("SKIP {}: not found", entry.path);
            skipped += 1;
            continue;
        }
        if has_internal_flags(&test_path) {
            eprintln!("SKIP {}: internal-only flags", entry.path);
            skipped += 1;
            continue;
        }
        runnable.push(entry.path.clone());
    }

    // Fan the corpus across worker threads. Sequential, this is the runtime of
    // ~2,554 process spawns summed; parallel it's bounded by the slowest worker.
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
                scope.spawn(move || {
                    let tmp = std::env::temp_dir()
                        .join(format!("nub-compat-{}-{wid}", std::process::id()));
                    let _ = fs::create_dir_all(&tmp);
                    let mut p = 0usize;
                    let mut suspect: Vec<String> = Vec::new();
                    for rel in &bucket {
                        match run_with_timeout(nub, &suite.join(rel), suite, &tmp, wid) {
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
    // (single TMPDIR, fork id 0); only a failure that reproduces in isolation is a
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
        match run_with_timeout(&nub, &suite.join(rel), &suite, &reverify_tmp, 0) {
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
         {skipped} skipped) [{workers}-way scan, {false_positives} parallel false positive(s) \
         reclassified on sequential re-verify] ===",
        passed + failed + timed_out
    );
    assert_eq!(
        failed + timed_out,
        0,
        "{failed} Node compat tests failed, {timed_out} timed out (confirmed in isolation)"
    );
}

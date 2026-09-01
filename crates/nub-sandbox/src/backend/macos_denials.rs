//! Read back what Seatbelt refused a confined launch, so a failed lifecycle script can name the
//! path instead of only reporting an exit code.
//!
//! ⛔ THE PREMISE, AND IT REVERSES A LONG-STANDING ONE. The build jail's diagnostic used to say
//! nub "never observes the attempt" because the kernel denies from inside the child's own process.
//! That is still true of the SYSCALL — the script gets its `EPERM` with nub nowhere in the loop —
//! but it was never true of the RECORD: the Sandbox kext writes every denial to the unified log,
//! carrying the operation, the resolved path, and the offending process. `/usr/bin/log show` reads
//! it back as an ordinary user. Measured on Darwin 25.6.0, uid 501, no sudo and no setup step,
//! which is what makes it admissible under the jail's totally-unprivileged constraint.
//!
//! ATTRIBUTION IS THE HARD PART AND SBPL SOLVES IT FOR FREE. A `(with message "…")` modifier on
//! the profile's `(deny default)` is echoed verbatim on every record that rule produces —
//! including from grandchildren, which is what a `node-gyp` → `make` → `cc` tree needs. nub
//! already compiles one profile per launch, so the label costs one string.
//!
//! ⛔ macOS ONLY, and not for want of trying elsewhere: Linux Landlock's audit channel needs
//! kernel 6.15 plus audit privilege, and Windows LowBox Permissive Learning Mode needs
//! administrator AND stops enforcing while it learns. Neither has an unprivileged twin, so this
//! stays a one-platform diagnostic rather than an abstraction with two empty implementations.
//!
//! ⛔ FAILURE PATH ONLY. Every entry point here is called after a confined script has already
//! exited non-zero. A passing install spawns nothing from this module — see the cost note on
//! [`for_launch`].

use std::path::Path;
use std::time::Duration;

/// The stock reader for the unified log. Absolute by necessity, not by style: `log` is also a zsh
/// builtin, so a bare `log show` from any shell-mediated context dies `too many arguments` — a
/// wrong answer that looks exactly like "no denials were recorded".
#[cfg(target_os = "macos")]
const LOG_BINARY: &str = "/usr/bin/log";

/// How far back to ask for. The refusal that killed a script is the last thing that happened to
/// it, so a bounded lookback finds it while keeping the scan cheap on a machine with a busy log.
/// A script that ran longer than this loses only its early, survived denials.
const MAX_LOOKBACK: Duration = Duration::from_secs(300);

/// Added to the script's runtime before asking.
///
/// ⛔ NOT ROUNDING SLOP — WITHOUT IT THE WINDOW IS SHORT BY CONSTRUCTION. `--last N` is measured
/// back from when `log show` runs, which is strictly AFTER the child exited, so a window of
/// exactly the child's runtime already misses its first moments; whole-second truncation of a
/// 1.9 s run then asks for 1 s and loses nearly half. Both errors are silent — a short window
/// returns an empty list, which is indistinguishable from "nothing was refused".
const LOOKBACK_SLACK: Duration = Duration::from_secs(2);

/// Give up rather than hold an install open. A diagnostic that can itself hang a failing install
/// is worse than no diagnostic; measured cost of the real call is ~0.9–1.2 s, so this is slack for
/// a contended host and not a budget anyone should reach.
#[cfg(target_os = "macos")]
const DEADLINE: Duration = Duration::from_secs(10);

/// Refusals past this are dropped. The cap is a readability bound, not a correctness one: a script
/// that provoked hundreds is not diagnosed by listing them.
const MAX_DENIALS: usize = 32;

/// One refusal the kernel recorded against a confined launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    /// The Seatbelt operation, as the kext spells it — `file-read-data`, `file-write-create`, …
    pub operation: String,
    /// The resolved path the kernel refused. Always absolute: a record whose argument is not a
    /// path (a sysctl name, a mach service) is dropped before it becomes a `Denial`.
    pub path: String,
}

/// Refusals EVERY confined process provokes at startup, on a jail that is working correctly.
///
/// ⛔ MEASURED ON A *SUCCEEDING* RUN, which is the only evidence that admits an entry here: a real
/// `nub install` whose confined `postinstall` exited 0 produced both of these. Something a passing
/// script was refused cannot be what made a failing one fail, and listing it would put the same
/// dead path at the top of every package's diagnostic — which is how a reader learns to skip the
/// section.
///
/// Exact pairs, never a prefix rule, so an unmeasured refusal nearby still reaches the user.
const STARTUP_NOISE_PATHS: &[(&str, &str)] = &[("file-read-metadata", "/System/Cryptexes/OS")];

/// The same, for a refusal whose DIRECTORY varies. CoreFoundation reads `$HOME/.CFUserTextEncoding`
/// during startup, so every Node lifecycle script produces one — and the jail redirects `$HOME`,
/// so the path differs between the real home and the throwaway one and cannot be matched exactly.
const STARTUP_NOISE_NAMES: &[(&str, &str)] = &[("file-read-data", ".CFUserTextEncoding")];

/// The label characters that may reach a `log show` predicate.
///
/// ⛔ A GATE, NOT A SANITIZER: the label is interpolated into predicate SOURCE, so anything
/// outside this set aborts the query rather than being escaped or stripped. npm names, semver
/// versions and nub's own launch nonce are all inside it, so the gate never fires in practice —
/// it exists so that a future label format cannot quietly turn into predicate injection.
fn label_is_safe(label: &str) -> bool {
    !label.is_empty()
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@/.:_-+".contains(&b))
}

/// What the jail refused `label`'s launch during the `ran_for` window that just ended.
///
/// ⛔ COSTS NOTHING ON A PASSING INSTALL — the only caller is the failure path, so the `log show`
/// child is spawned once per FAILED script and never otherwise.
///
/// Silent on every failure of its own: no `/usr/bin/log`, a non-macOS host, a malformed label, a
/// query that outruns its deadline, unparseable output — each returns an empty list. The caller
/// prints nothing extra and the pre-existing diagnostic is unchanged.
///
/// ⛔ PRIVACY: the paths come back because the SCRIPT touched them, and they may name the user's
/// own files. They are for that user's terminal and their own project-local log. Never send them
/// anywhere, and never write them to a lockfile or anything committed.
pub fn for_launch(label: &str, ran_for: Duration) -> Vec<Denial> {
    raw_for_launch(label, ran_for)
        .map(|raw| parse(&raw))
        .unwrap_or_default()
}

/// The kernel records for `label` BEFORE parsing, or `None` when the host answered with nothing.
///
/// ⛔ IT EXISTS TO SEPARATE TWO FAILURES [`for_launch`] CONFLATES. An empty `Vec` means either the
/// kernel never delivered a record — the unified log drops and delays under load, which is why this
/// whole channel is documented best-effort — or it delivered one and the parser missed it. The first
/// is the environment and is not assertable; the second is a regression and is the only reason to
/// have a test here at all. A caller that cannot tell them apart must either flake or assert
/// nothing.
pub(crate) fn raw_for_launch(label: &str, ran_for: Duration) -> Option<String> {
    if !label_is_safe(label) {
        return None;
    }
    read_log(label, (ran_for + LOOKBACK_SLACK).min(MAX_LOOKBACK))
}

/// Run the query, or `None` if the host cannot answer it within [`DEADLINE`].
///
/// ⛔ OUTPUT GOES TO A FILE, NEVER A PIPE. A deadline needs `try_wait`, and `try_wait` on a child
/// whose stdout is an unread pipe deadlocks the moment the log exceeds the pipe buffer — which a
/// 300 s window on a busy machine does easily. A temp file has no such bound and is discarded with
/// the handle.
#[cfg(target_os = "macos")]
fn read_log(label: &str, lookback: Duration) -> Option<String> {
    use std::process::{Command, Stdio};

    let sink = tempfile::NamedTempFile::new().ok()?;
    let mut child = Command::new(LOG_BINARY)
        .arg("show")
        .arg("--style")
        .arg("ndjson")
        .arg("--last")
        .arg(format!("{}s", lookback.as_secs()))
        .arg("--predicate")
        // `process == "kernel"` is not merely a narrowing: without it the predicate matches the
        // `log show` invocation itself, whose own arguments contain the label.
        .arg(format!(
            "process == \"kernel\" AND eventMessage CONTAINS \"{label}\""
        ))
        .stdout(sink.reopen().ok()?)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = std::time::Instant::now() + DEADLINE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            // Over the deadline, or the wait itself failed: kill and give up. Reaping keeps a
            // long install from accumulating zombies across several failed scripts.
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    std::fs::read_to_string(sink.path()).ok()
}

#[cfg(not(target_os = "macos"))]
fn read_log(_label: &str, _lookback: Duration) -> Option<String> {
    // Seatbelt's `(with message …)` has no unprivileged twin on Linux or Windows (module doc), so
    // there is nothing to read. Compiled everywhere anyway so the parser below — which is pure —
    // is exercised by the test suite on every platform rather than only on a macOS runner.
    None
}

/// Turn `log show --style ndjson` output into the refusals worth showing a user.
///
/// Deduplicated on `(operation, path)` in first-seen order: a build loop retrying one denied path
/// produces the same record hundreds of times, and the count answers no question the reader has.
pub(crate) fn parse(ndjson: &str) -> Vec<Denial> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in ndjson.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(message) = record.get("eventMessage").and_then(|v| v.as_str()) else {
            continue;
        };
        // The label rides on its own trailing line of the same message; only the first line
        // carries the operation and the path.
        let Some(denial) = parse_message(message.lines().next().unwrap_or_default()) else {
            continue;
        };
        if seen.insert((denial.operation.clone(), denial.path.clone())) {
            out.push(denial);
            if out.len() == MAX_DENIALS {
                break;
            }
        }
    }
    out
}

/// Parse one kext message: `Sandbox: <proc>(<pid>) deny(1) <operation> <argument>`.
///
/// The kext coalesces bursts behind an `N duplicate report for ` prefix, which carries a real
/// denial the caller would otherwise lose — strip it rather than skipping the line.
fn parse_message(message: &str) -> Option<Denial> {
    let body = message
        .split_once("Sandbox: ")
        .map(|(_, rest)| rest)
        .unwrap_or(message);
    let (_process, rest) = body.split_once(") deny(")?;
    let (_flags, rest) = rest.split_once(") ")?;
    let (operation, argument) = rest.split_once(' ')?;
    // A non-file operation's argument is a sysctl name or a mach service, not a path. Those are
    // real refusals, but naming one tells a user nothing they can act on — and the jail's own
    // grants are expressed in paths, so a path is what a remedy would have to name.
    if !operation.starts_with("file-") || !Path::new(argument).is_absolute() {
        return None;
    }
    if STARTUP_NOISE_PATHS.contains(&(operation, argument)) {
        return None;
    }
    let name = Path::new(argument).file_name()?.to_str()?;
    if STARTUP_NOISE_NAMES.contains(&(operation, name)) {
        return None;
    }
    Some(Denial {
        operation: operation.to_string(),
        path: argument.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(message: &str) -> String {
        serde_json::json!({ "processID": 0, "eventMessage": message }).to_string()
    }

    #[test]
    fn parses_operation_and_path_out_of_a_kext_denial() {
        let got = parse(&record(
            "Sandbox: node(1234) deny(1) file-write-create /Users/x/.ssh/known_hosts\nNUBPKG:p@1.0.0:7-1",
        ));
        assert_eq!(
            got,
            vec![Denial {
                operation: "file-write-create".into(),
                path: "/Users/x/.ssh/known_hosts".into(),
            }],
            "the first message line carries op + path; the label rides the second"
        );
    }

    #[test]
    fn keeps_a_coalesced_duplicate_report() {
        let got = parse(&record(
            "1 duplicate report for Sandbox: cc(9) deny(1) file-read-data /opt/x/lib.h\nNUBPKG:p@1.0.0:7-1",
        ));
        assert_eq!(
            got.len(),
            1,
            "the kext's duplicate-report prefix wraps a real denial, not a summary line: {got:?}"
        );
        assert_eq!(got[0].path, "/opt/x/lib.h");
    }

    #[test]
    fn drops_records_that_name_no_path() {
        let got = parse(&record(
            "Sandbox: sh(1) deny(1) sysctl-read kern.bootargs\nNUBPKG:p@1.0.0:7-1",
        ));
        assert!(
            got.is_empty(),
            "a sysctl name is not a path a remedy could grant: {got:?}"
        );
    }

    #[test]
    fn drops_the_startup_probes_every_confined_process_makes() {
        for noise in [
            "Sandbox: sh(1) deny(1) file-read-metadata /System/Cryptexes/OS",
            // $HOME is jail-redirected, so this one is matched by file name at either spelling.
            "Sandbox: node(1) deny(1) file-read-data /Users/x/.CFUserTextEncoding",
            "Sandbox: node(1) deny(1) file-read-data /var/folders/t/nub-home/.CFUserTextEncoding",
        ] {
            let got = parse(&record(&format!("{noise}\nNUBPKG:p@1.0.0:7-1")));
            assert!(
                got.is_empty(),
                "measured on a SUCCEEDING confined run, so it cannot explain a failure: {got:?}"
            );
        }
        // …but only those exact operations. A different one is a real refusal, not noise.
        for real in [
            "Sandbox: sh(1) deny(1) file-read-data /System/Cryptexes/OS/x",
            "Sandbox: node(1) deny(1) file-write-create /Users/x/.CFUserTextEncoding",
        ] {
            let got = parse(&record(&format!("{real}\nNUBPKG:p@1.0.0:7-1")));
            assert_eq!(
                got.len(),
                1,
                "the noise filter is exact, not a prefix: {real}"
            );
        }
    }

    #[test]
    fn collapses_a_repeated_refusal_to_one_line() {
        let line = record("Sandbox: cc(9) deny(1) file-read-data /opt/x/lib.h\nNUBPKG:p@1.0.0:7-1");
        let got = parse(&format!("{line}\n{line}\n{line}"));
        assert_eq!(
            got.len(),
            1,
            "a retry loop denies the same path repeatedly; the count answers nothing: {got:?}"
        );
    }

    #[test]
    fn a_label_that_could_escape_the_predicate_is_refused_outright() {
        for bad in ["", "p\" OR 1==1 OR \"", "p 1", "p\n1"] {
            assert!(
                !label_is_safe(bad),
                "{bad:?} reaches log show as predicate source and must not be queried"
            );
        }
        assert!(label_is_safe(
            "NUBPKG:@scope/pkg@1.2.3-beta.4+build.5:701-2"
        ));
    }

    #[test]
    fn a_refused_label_never_reaches_the_query() {
        assert!(
            for_launch("p\" OR \"", Duration::from_secs(5)).is_empty(),
            "the gate must short-circuit before any child is spawned"
        );
    }
}

//! End-to-end output redaction for `nub run --sandbox`.
//!
//! Drives the REAL `nub run --sandbox <policy> -- <probe> env-print …` seam and asserts
//! that a granted `secrets` value is scrubbed to `<redacted:NAME>` on nub's forwarded
//! stdout while a non-secret `vars` value passes through unchanged. The child is the
//! std-only `nub-sandbox-probe` binary (a NON-Node program), which also proves the scrub
//! is language-agnostic. Output is captured via a pipe (`Command::output`), so nub's own
//! stdout is non-TTY and redaction engages — exactly the log/CI case the feature targets.

use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

/// A sibling binary in the same target dir as this test (cargo builds nub-cli's bins —
/// `nub` and `nub-sandbox-probe` — before its integration tests).
fn sibling_bin(name: &str) -> PathBuf {
    let mut p = std::env::current_exe().expect("current_exe");
    p.pop(); // deps/
    p.pop(); // profile dir
    p.push(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(p.exists(), "expected built binary at {}", p.display());
    p
}

/// Linux env-enforcing policies confine via Bubblewrap; skip the e2e where stock bwrap
/// cannot run (a non-Linux host, or a Linux host without a working bwrap). Redaction
/// itself is a Nub-side transform, but the sandbox APPLY is its precondition here.
#[cfg(target_os = "linux")]
fn can_run() -> bool {
    Command::new("bwrap")
        .args(["--dev-bind", "/", "/", "true"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
#[cfg(not(target_os = "linux"))]
fn can_run() -> bool {
    true
}

/// Run the probe under a policy, returning its (stdout, stderr, exit-code-is-zero).
fn run(policy: &str, env: &[(&str, &str)], probe_args: &[&str]) -> (String, String, bool) {
    let tree = TempDir::new().unwrap();
    let policy_path = tree.path().join("policy.json");
    std::fs::write(&policy_path, policy).unwrap();

    let mut cmd = Command::new(sibling_bin("nub"));
    cmd.arg("run").arg("--sandbox").arg(&policy_path).arg("--");
    cmd.arg(sibling_bin("nub-sandbox-probe"));
    cmd.args(probe_args);
    cmd.current_dir(tree.path());
    // Homes come from the ambient env inside nub; anchor them at the temp tree so nothing
    // touches the developer's real `~`.
    cmd.env("HOME", tree.path())
        .env("USERPROFILE", tree.path())
        .env("XDG_CACHE_HOME", tree.path().join(".cache"));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn nub");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !(stderr.contains("did not compile") || stderr.contains("could not be applied")),
        "HARNESS ERROR — nub failed to set up the sandbox:\n{stderr}"
    );
    (stdout, stderr, out.status.code() == Some(0))
}

#[test]
fn secret_value_is_redacted_and_non_secret_var_passes_through() {
    if !can_run() {
        eprintln!("skipping: sandbox cannot be enforced on this host");
        return;
    }
    let policy = r#"{ "secrets": ["SECRET"], "vars": ["MODE"] }"#;
    let (stdout, _stderr, ok) = run(
        policy,
        &[("SECRET", "abc123xyz"), ("MODE", "production")],
        &["env-print", "SECRET", "MODE"],
    );
    assert!(ok, "the probe should run and exit 0; stdout=\n{stdout}");
    assert!(
        !stdout.contains("abc123xyz"),
        "the secret value must be scrubbed from the forwarded stream; stdout=\n{stdout}"
    );
    assert!(
        stdout.contains("SECRET=<redacted:SECRET>"),
        "the secret is replaced by its named token; stdout=\n{stdout}"
    );
    assert!(
        stdout.contains("MODE=production"),
        "a non-secret var is untouched; stdout=\n{stdout}"
    );
}

#[test]
fn a_policy_with_no_secrets_leaves_output_untouched() {
    if !can_run() {
        eprintln!("skipping: sandbox cannot be enforced on this host");
        return;
    }
    // Zero-secret regression: a `vars`-only policy has no sensitive keys, so stdio is
    // inherited (no pipe, no scan) and the value reaches the stream verbatim.
    let policy = r#"{ "vars": ["TOKENISH"] }"#;
    let (stdout, _stderr, ok) = run(
        policy,
        &[("TOKENISH", "not-a-secret-value")],
        &["env-print", "TOKENISH"],
    );
    assert!(ok, "the probe should run and exit 0; stdout=\n{stdout}");
    assert!(
        stdout.contains("TOKENISH=not-a-secret-value"),
        "a non-secret value is never redacted; stdout=\n{stdout}"
    );
    assert!(
        !stdout.contains("<redacted"),
        "nothing is redacted when there are no secrets; stdout=\n{stdout}"
    );
}

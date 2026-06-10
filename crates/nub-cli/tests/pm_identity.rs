//! The PM identity decision table, behaviorally, through the binary
//! (spec: wiki/commands/pm/identity-policy.md). Identity resolution is the
//! engine's declaration-aware policy (pin-over-inference, Axiom 1), wired
//! into nub's engine preflight; the contradiction/ambiguity rows render
//! nub-side with the rewritten stable codes and the `nub pm use` remedy.
//!
//! All rows run OFFLINE: the lockfile-writing rows use empty-dependency
//! manifests (nothing to resolve, but the lockfile still lands — pointing
//! the registry at a dead port proves no network is involved), and the
//! error rows fail in preflight before any resolution.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

/// A unique temp project dir under the system temp root (never under $HOME,
/// so manifest/lockfile walk-ups can't escape into stray ancestors). The
/// `.npmrc` dead-port registry makes any accidental network use fail loudly.
fn project(tag: &str, manifest: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-pm-identity-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("package.json"), manifest).unwrap();
    std::fs::write(dir.join(".npmrc"), "registry=http://127.0.0.1:1/\n").unwrap();
    dir
}

/// Spawn `nub <args>` in `dir` with the engine store/cache isolated to fresh
/// temp roots.
fn run(dir: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(nub_binary())
        .args(args)
        .current_dir(dir)
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("XDG_CACHE_HOME", dir.join("xdg-cache"))
        .output()
        .expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

const EMPTY_PNPM: &str = r#"{"name":"app","version":"1.0.0","packageManager":"pnpm@9.1.0"}"#;

/// Rows "none|none → pnpm format" (Axiom 4) and "declared X|none → X's
/// format" (the fresh-with-pin row): an empty-deps install writes the
/// identity's lockfile without any network.
#[test]
fn fresh_projects_write_the_identity_format_declared_first_else_pnpm() {
    // none + none → pnpm-lock.yaml (the portable default).
    let dir = project("fresh-default", r#"{"name":"app","version":"1.0.0"}"#);
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        dir.join("pnpm-lock.yaml").is_file(),
        "undeclared fresh install must write pnpm-lock.yaml"
    );

    // declared npm + none → package-lock.json, NOT the pnpm default.
    let dir = project(
        "fresh-npm",
        r#"{"name":"app","version":"1.0.0","packageManager":"npm@11.0.0"}"#,
    );
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        dir.join("package-lock.json").is_file(),
        "declared-npm fresh install must write package-lock.json: {stderr}"
    );
    assert!(
        !dir.join("pnpm-lock.yaml").exists(),
        "the declaration must outrank the pnpm fresh default"
    );
}

/// Row "none|exactly one → that identity": an undeclared project keeps its
/// single lockfile's format, and a declared project keeps its own lockfile
/// even with a stray other-format file next to it (declaration wins; the
/// stray is ignored, not adopted).
#[test]
fn a_single_lockfile_infers_the_identity_and_a_declaration_outranks_strays() {
    let npm_lock = r#"{"name":"app","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{"":{"name":"app","version":"1.0.0"}}}"#;

    let dir = project("infer-npm", r#"{"name":"app","version":"1.0.0"}"#);
    std::fs::write(dir.join("package-lock.json"), npm_lock).unwrap();
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        dir.join("package-lock.json").is_file() && !dir.join("pnpm-lock.yaml").exists(),
        "a lone package-lock.json keeps the npm identity: {stderr}"
    );

    // Declared pnpm + pnpm-lock.yaml + stray package-lock.json → pnpm wins,
    // the stray is left alone (removal is `nub pm use`'s job, not install's).
    let dir = project("declared-vs-stray", EMPTY_PNPM);
    std::fs::write(
        dir.join("pnpm-lock.yaml"),
        "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n",
    )
    .unwrap();
    std::fs::write(dir.join("package-lock.json"), npm_lock).unwrap();
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        dir.join("pnpm-lock.yaml").is_file() && dir.join("package-lock.json").is_file(),
        "the declared format is used; the stray is not deleted by install"
    );
}

/// Row "X|only a different PM's lockfile → error": the contradiction is loud,
/// carries the rewritten stable code, and names the `nub pm use` remedy.
#[test]
fn a_declaration_contradicted_by_the_lockfile_errors_with_code_and_remedy() {
    let dir = project("contradiction", EMPTY_PNPM);
    std::fs::write(
        dir.join("package-lock.json"),
        r#"{"name":"app","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{}}"#,
    )
    .unwrap();
    let (stdout, stderr, code) = run(&dir, &["install"]);
    assert_ne!(code, 0, "a contradicted project must refuse to install");
    assert!(
        stderr.contains("ERR_NUB_LOCKFILE_DECLARATION_MISMATCH"),
        "the stable code must be present (rewritten): {stderr}"
    );
    assert!(
        stderr.contains("set the declaration: nub pm use <pm> — or remove the stale lockfile"),
        "the remedy must be nub's: {stderr}"
    );
    assert!(
        !stderr.contains("aube") && !stderr.contains("AUBE"),
        "no engine branding may leak: {stderr}"
    );
    assert!(
        !dir.join("pnpm-lock.yaml").exists() && !dir.join("node_modules").exists(),
        "nothing may be written past the contradiction: {stdout}"
    );
}

/// Row "none|multiple → error": two lockfiles and no declaration is an
/// ambiguity nub refuses to guess through — same code/remedy contract.
#[test]
fn undeclared_multi_lockfile_projects_error_as_ambiguous() {
    let dir = project("ambiguous", r#"{"name":"app","version":"1.0.0"}"#);
    std::fs::write(
        dir.join("package-lock.json"),
        r#"{"name":"app","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("yarn.lock"), "# yarn lockfile v1\n").unwrap();
    let (_, stderr, code) = run(&dir, &["install"]);
    assert_ne!(code, 0, "an ambiguous project must refuse to install");
    assert!(
        stderr.contains("ERR_NUB_LOCKFILE_AMBIGUOUS"),
        "the stable code must be present (rewritten): {stderr}"
    );
    assert!(
        stderr.contains("package-lock.json") && stderr.contains("yarn.lock"),
        "the error must name the conflicting files: {stderr}"
    );
    assert!(
        stderr.contains("set the declaration: nub pm use <pm> — or remove the stale lockfile"),
        "the remedy must be nub's: {stderr}"
    );
}

/// The declared-yarn corner of the fresh row: identity resolves to yarn with
/// no yarn.lock on disk, and the first install would CREATE yarn.lock — the
/// gated write. Refused with the gate message, nothing written.
#[test]
fn a_fresh_declared_yarn_project_hits_the_write_gate_not_a_pnpm_lockfile() {
    let dir = project(
        "yarn-fresh",
        r#"{"name":"app","version":"1.0.0","packageManager":"yarn@1.22.19"}"#,
    );
    let (_, stderr, code) = run(&dir, &["install"]);
    assert_ne!(code, 0, "a fresh declared-yarn install must refuse");
    assert!(
        stderr.contains("refusing to modify yarn.lock") && stderr.contains("yarn install"),
        "the refusal must be the yarn gate with its remedy: {stderr}"
    );
    assert!(
        !dir.join("pnpm-lock.yaml").exists() && !dir.join("yarn.lock").exists(),
        "no lockfile of any format may be written past the gate"
    );
}

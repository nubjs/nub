#![cfg(target_os = "macos")]

//! Live Seatbelt coverage for the public positive filesystem grammar.
//!
//! Each child is this test binary re-entered through [`Sandbox`], so the assertions cover
//! compiler expansion, macOS profile emission, and the kernel decision together. Every glob arm
//! writes a path that did not exist when its policy was compiled, then attempts its nearest
//! non-matching sibling. The policies contain only positive grants: a root read grant plus a
//! narrower rw grant, which also proves that `r` never subtracts the overlapping `rw` capability.

#[path = "common/tool_output.rs"]
mod tool_output;

use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const CASE: &str = "NUB_MACOS_POSITIVE_GRAMMAR_CASE";
const ALLOWED: &str = "NUB_MACOS_POSITIVE_GRAMMAR_ALLOWED";
const DENIED: &str = "NUB_MACOS_POSITIVE_GRAMMAR_DENIED";
const READ_ALLOWED_EXISTING: &str = "NUB_MACOS_POSITIVE_GRAMMAR_READ_ALLOWED_EXISTING";
const READ_ALLOWED_FUTURE: &str = "NUB_MACOS_POSITIVE_GRAMMAR_READ_ALLOWED_FUTURE";
const READ_DENIED_EXISTING: &str = "NUB_MACOS_POSITIVE_GRAMMAR_READ_DENIED_EXISTING";
const READ_DENIED_FUTURE: &str = "NUB_MACOS_POSITIVE_GRAMMAR_READ_DENIED_FUTURE";

#[test]
fn macos_positive_grammar_child() {
    let Some(case) = std::env::var_os(CASE) else {
        return;
    };
    if case.to_string_lossy().starts_with("read-") {
        child_reads(&case);
        return;
    }
    let allowed = PathBuf::from(std::env::var_os(ALLOWED).expect("allowed canary"));
    assert!(
        !allowed.exists(),
        "{case:?} allowed canary existed before the confined child ran"
    );
    fs::write(&allowed, b"allowed future canary").expect("allowed future canary write");
    assert_eq!(fs::read(&allowed).unwrap(), b"allowed future canary");

    if let Some(denied) = std::env::var_os(DENIED) {
        let denied = PathBuf::from(denied);
        assert!(
            !denied.exists(),
            "{case:?} denied canary existed before the confined child ran"
        );
        assert!(
            fs::write(&denied, b"denied future canary").is_err(),
            "{case:?} nearest non-matching future canary was writable: {}",
            denied.display()
        );
        assert!(
            !denied.exists(),
            "{case:?} denied future canary was created: {}",
            denied.display()
        );
    }
}

fn child_reads(case: &std::ffi::OsStr) {
    for name in [READ_ALLOWED_EXISTING, READ_ALLOWED_FUTURE] {
        let path = PathBuf::from(std::env::var_os(name).expect("allowed read canary"));
        assert_eq!(
            fs::read(&path).unwrap(),
            b"allowed readable canary",
            "{case:?} allowed read was denied: {}",
            path.display()
        );
    }
    for name in [READ_DENIED_EXISTING, READ_DENIED_FUTURE] {
        let path = PathBuf::from(std::env::var_os(name).expect("denied read canary"));
        assert!(
            fs::read(&path).is_err(),
            "{case:?} nearest non-matching readable canary was exposed: {}",
            path.display()
        );
    }
}

#[test]
fn positive_union_enforces_full_live_glob_grammar() {
    let root = fixture();
    let root = root.path();
    fs::create_dir_all(root.join("case")).unwrap();
    let cases = [
        (
            "asterisk",
            root.join("asterisk/allow-*"),
            root.join("asterisk/allow-future"),
            root.join("asterisk/denied-future"),
        ),
        (
            "recursive",
            root.join("recursive/**/allow"),
            root.join("recursive/deep/nested/allow"),
            root.join("recursive/deep/nested/deny"),
        ),
        (
            "question",
            root.join("question/allow-?"),
            root.join("question/allow-a"),
            root.join("question/allow-aa"),
        ),
        (
            "class",
            root.join("class/allow-[ab]"),
            root.join("class/allow-a"),
            root.join("class/allow-c"),
        ),
        (
            "nested-braces",
            root.join("brace/{allow,{also,again}}-file"),
            root.join("brace/also-file"),
            root.join("brace/all-file"),
        ),
        // APFS resolves this existing parent case-insensitively, while the final filename is
        // created after policy compilation. This keeps the backend honest about the case behavior
        // that the PathMatcher promises without assuming how Seatbelt canonicalizes the lookup.
        (
            "case-variant",
            root.join("case/allow-*"),
            root.join("CASE/ALLOW-FUTURE"),
            root.join("CASE/DENIED-FUTURE"),
        ),
    ];

    for (label, pattern, allowed, denied) in cases {
        fs::create_dir_all(allowed.parent().unwrap()).unwrap();
        fs::create_dir_all(denied.parent().unwrap()).unwrap();
        run_case(root, label, pattern, allowed, Some(denied));
    }
}

#[test]
fn read_only_globs_enforce_existing_and_post_prepare_canaries() {
    let root = fixture();
    let root = root.path();
    fs::create_dir_all(root.join("case")).unwrap();
    let cases = [
        (
            "asterisk",
            root.join("asterisk/allow-*"),
            root.join("asterisk/allow-existing"),
            root.join("asterisk/allow-future"),
            root.join("asterisk/denied-existing"),
            root.join("asterisk/denied-future"),
        ),
        (
            "recursive",
            root.join("recursive/**/allow"),
            root.join("recursive/existing/allow"),
            root.join("recursive/future/deep/allow"),
            root.join("recursive/existing/deny"),
            root.join("recursive/future/deep/deny"),
        ),
        (
            "question",
            root.join("question/allow-?"),
            root.join("question/allow-a"),
            root.join("question/allow-b"),
            root.join("question/allow-aa"),
            root.join("question/allow-cc"),
        ),
        (
            "class",
            root.join("class/allow-[ab]"),
            root.join("class/allow-a"),
            root.join("class/allow-b"),
            root.join("class/allow-c"),
            root.join("class/allow-d"),
        ),
        (
            "nested-braces",
            root.join("brace/{allow,{also,again}}-file"),
            root.join("brace/allow-file"),
            root.join("brace/again-file"),
            root.join("brace/all-file"),
            root.join("brace/other-file"),
        ),
        (
            "case-variant",
            root.join("case/allow-*"),
            root.join("CASE/ALLOW-EXISTING"),
            root.join("CASE/ALLOW-FUTURE"),
            root.join("CASE/DENIED-EXISTING"),
            root.join("CASE/DENIED-FUTURE"),
        ),
    ];

    for (label, pattern, allowed_existing, allowed_future, denied_existing, denied_future) in cases
    {
        for path in [
            &allowed_existing,
            &allowed_future,
            &denied_existing,
            &denied_future,
        ] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        run_read_case(
            root,
            label,
            pattern,
            allowed_existing,
            allowed_future,
            denied_existing,
            denied_future,
        );
    }
}

#[test]
fn explicit_broad_write_roots_reach_only_harmless_temp_canaries() {
    for (label, broad_root) in [("whole-filesystem", "/"), ("private-root", "/private")] {
        let root = tempfile::Builder::new()
            .prefix("nub-macos-broad-write-")
            .tempdir_in("/private/tmp")
            .expect("private tmp fixture");
        let canary = root.path().join("broad-root-future-canary");
        run_case(
            root.path(),
            label,
            PathBuf::from(broad_root),
            canary.clone(),
            None,
        );
        assert_eq!(fs::read(canary).unwrap(), b"allowed future canary");
    }
}

fn run_case(root: &Path, label: &str, pattern: PathBuf, allowed: PathBuf, denied: Option<PathBuf>) {
    let mut fs = Map::new();
    // `r` over the fixture supplies the child cwd and overlaps the `rw` matcher below. The
    // child write demonstrates that positive `r` and `rw` entries union rather than subtract.
    fs.insert(root.to_string_lossy().into_owned(), json!("r"));
    fs.insert(pattern.to_string_lossy().into_owned(), json!("rw"));
    let policy = policy(root, Value::Object(fs), label, &allowed, denied.as_deref());
    let sandbox = Sandbox::new(&policy).expect("positive grammar sandbox");
    let prepared = sandbox
        .prepare(command(root, label))
        .expect("positive grammar command prepares");
    assert!(
        prepared.degradation.is_full(),
        "{label}: macOS command degraded: {:?}",
        prepared.degradation
    );
    let output = tool_output::output(prepared);
    assert!(
        output.status.success(),
        "{label} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_read_case(
    root: &Path,
    label: &str,
    pattern: PathBuf,
    allowed_existing: PathBuf,
    allowed_future: PathBuf,
    denied_existing: PathBuf,
    denied_future: PathBuf,
) {
    fs::write(&allowed_existing, b"allowed readable canary").unwrap();
    fs::write(&denied_existing, b"denied readable canary").unwrap();

    let test_exe = std::env::current_exe().unwrap();
    let fs = json!({
        (test_exe.to_string_lossy()): "r",
        (pattern.to_string_lossy()): "r",
    });
    let policy = read_policy(
        root,
        fs,
        label,
        &allowed_existing,
        &allowed_future,
        &denied_existing,
        &denied_future,
    );
    let sandbox = Sandbox::new(&policy).expect("read-only grammar sandbox");
    // These two paths become visible after both acquisition and profile preparation. Seatbelt
    // must still apply the compiled grammar when the prepared child opens them.
    let prepared = sandbox
        .prepare(command(Path::new("/"), &format!("read-{label}")))
        .expect("read-only grammar command prepares");
    assert!(
        prepared.degradation.is_full(),
        "read-{label}: macOS command degraded: {:?}",
        prepared.degradation
    );
    fs::write(&allowed_future, b"allowed readable canary").unwrap();
    fs::write(&denied_future, b"denied readable canary").unwrap();
    let output = tool_output::output(prepared);
    assert!(
        output.status.success(),
        "read-{label} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn policy(
    root: &Path,
    fs: Value,
    case: &str,
    allowed: &Path,
    denied: Option<&Path>,
) -> nub_sandbox::SandboxPolicy {
    let mut ambient = BTreeMap::from([
        (CASE.into(), case.into()),
        (ALLOWED.into(), allowed.to_string_lossy().into_owned()),
    ]);
    if let Some(denied) = denied {
        ambient.insert(DENIED.into(), denied.to_string_lossy().into_owned());
    }
    let ctx = CompileCtx::new(
        Homes {
            home: root.join("home"),
            cache: root.join("cache"),
            tmp: root.join("tmp"),
            project: root.to_path_buf(),
        },
        root.to_path_buf(),
        ScopeCapabilities::approved(),
        ambient,
    );
    let mut vars = Map::from_iter([(CASE.into(), json!(true)), (ALLOWED.into(), json!(true))]);
    if denied.is_some() {
        vars.insert(DENIED.into(), json!(true));
    }
    compile(&json!({"fs": fs, "net": false, "vars": vars}), &ctx)
        .expect("positive grammar policy compiles")
}

fn read_policy(
    root: &Path,
    fs: Value,
    label: &str,
    allowed_existing: &Path,
    allowed_future: &Path,
    denied_existing: &Path,
    denied_future: &Path,
) -> nub_sandbox::SandboxPolicy {
    let ambient = BTreeMap::from([
        (CASE.into(), format!("read-{label}")),
        (
            READ_ALLOWED_EXISTING.into(),
            allowed_existing.to_string_lossy().into_owned(),
        ),
        (
            READ_ALLOWED_FUTURE.into(),
            allowed_future.to_string_lossy().into_owned(),
        ),
        (
            READ_DENIED_EXISTING.into(),
            denied_existing.to_string_lossy().into_owned(),
        ),
        (
            READ_DENIED_FUTURE.into(),
            denied_future.to_string_lossy().into_owned(),
        ),
    ]);
    let ctx = CompileCtx::new(
        Homes {
            home: root.join("home"),
            cache: root.join("cache"),
            tmp: root.join("tmp"),
            project: root.to_path_buf(),
        },
        root.to_path_buf(),
        ScopeCapabilities::approved(),
        ambient,
    );
    compile(
        &json!({
            "fs": fs,
            "net": false,
            "vars": {
                (CASE): true,
                (READ_ALLOWED_EXISTING): true,
                (READ_ALLOWED_FUTURE): true,
                (READ_DENIED_EXISTING): true,
                (READ_DENIED_FUTURE): true,
            },
        }),
        &ctx,
    )
    .expect("read-only grammar policy compiles")
}

fn command(root: &Path, label: &str) -> CommandSpec {
    CommandSpec::new(std::env::current_exe().unwrap())
        .args(["--exact", "macos_positive_grammar_child", "--nocapture"])
        .cwd(root)
        .redact_stdout(true)
        .redact_stderr(true)
        .audit_label(format!("macos-positive-grammar-{label}"))
}

fn fixture() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("nub-macos-positive-grammar-")
        .tempdir_in(std::env::var_os("HOME").expect("HOME"))
        .unwrap()
}

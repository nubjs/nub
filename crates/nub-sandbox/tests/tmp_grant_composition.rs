//! Native regression probes for `$tmp` composed with a broad positive filesystem grant.
//!
//! The canary is created by the unconfined parent in the OS temporary directory. The confined
//! child only reads it; its sole write (in the private case) is below the session-owned tempdir.
#![cfg(any(target_os = "linux", target_os = "macos"))]

#[path = "common/tool_output.rs"]
mod tool_output;

use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const CASE: &str = "NUB_TMP_GRANT_COMPOSITION_CASE";
const HOST_CANARY: &str = "NUB_TMP_GRANT_COMPOSITION_HOST_CANARY";
const HOST_TMP_ROOT: &str = "NUB_TMP_GRANT_COMPOSITION_HOST_TMP_ROOT";
const HOST_VISIBLE: &str = "NUB_TMP_GRANT_COMPOSITION_HOST_VISIBLE";
const PROJECT_CANARY: &str = "NUB_TMP_GRANT_COMPOSITION_PROJECT_CANARY";

#[test]
fn tmp_grant_composition_child() {
    let Some(case) = std::env::var_os(CASE) else {
        return;
    };
    let case = case.to_string_lossy();

    let host_canary = PathBuf::from(std::env::var_os(HOST_CANARY).expect("host tmp canary"));
    let project_canary =
        PathBuf::from(std::env::var_os(PROJECT_CANARY).expect("policy-readable project canary"));
    assert_eq!(
        fs::read_to_string(&project_canary).unwrap(),
        "project-grant-is-still-live"
    );
    let host_visible = std::env::var(HOST_VISIBLE).expect("host-canary access expectation");
    if host_visible == "true" {
        assert_eq!(
            fs::read_to_string(&host_canary).unwrap(),
            "host-temp-grant-canary"
        );
    } else {
        assert!(
            fs::read_to_string(&host_canary).is_err(),
            "a policy without a host-temp grant exposed {}",
            host_canary.display(),
        );
    }

    if case == "private" {
        let private = std::env::temp_dir();
        let host_tmp_root =
            PathBuf::from(std::env::var_os(HOST_TMP_ROOT).expect("parent host temp root"));
        assert_ne!(
            private, host_tmp_root,
            "private mode must redirect the child to a session-owned subdirectory"
        );
        let write = private.join(format!("nub-private-tmp-{}", std::process::id()));
        fs::write(&write, b"private-temp-write").expect("private temp is writable");
        assert_eq!(fs::read(&write).unwrap(), b"private-temp-write");
        fs::remove_file(write).unwrap();
        assert!(
            fs::write(
                project_canary.with_file_name("outside-private-tmp"),
                b"must-fail"
            )
            .is_err(),
            "a broad read grant must not make the fixture project writable"
        );
    } else {
        assert_eq!(case, "deny", "unknown composition fixture case");
    }
}

#[test]
fn tmp_modes_preserve_explicit_positive_grants_and_private_writes() {
    let root = fixture();
    let host_canary = tempfile::NamedTempFile::new().expect("host temporary canary");
    fs::write(host_canary.path(), b"host-temp-grant-canary").unwrap();

    // `$tmp` selects managed storage; it does not subtract an authored positive grant. The
    // narrow controls deny the host canary, while an explicit whole-root read admits it in both
    // modes. Private mode adds exactly the redirected temp directory as a writable location.
    run(&root, host_canary.path(), "deny", false, false);
    run(&root, host_canary.path(), "deny", true, true);
    run(&root, host_canary.path(), "private", false, false);
    run(&root, host_canary.path(), "private", true, true);
}

#[test]
fn raw_whole_disk_write_grammar_preserves_the_tmp_deny_mode() {
    let root = fixture();
    let policy = policy(root.path(), "deny", true, "rw");
    let matcher = nub_sandbox::matcher::path::PathMatcher::new(&policy.fs.rules);
    for path in [Path::new("/"), root.path()] {
        let decision = matcher.decide(path);
        assert_eq!(decision.effect, nub_sandbox::policy::Effect::Allow);
        assert_eq!(decision.access, nub_sandbox::policy::FsAccess::ReadWrite);
    }
    assert_eq!(
        policy.fs.tmp,
        nub_sandbox::policy::TmpMode::Deny,
        "a broad positive filesystem rule must not erase `$tmp: false`"
    );
}

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("fixture root");
    fs::create_dir(root.path().join("project")).unwrap();
    fs::write(
        root.path().join("project/canary"),
        "project-grant-is-still-live",
    )
    .unwrap();
    root
}

fn policy(
    root: &Path,
    case: &str,
    broad_root: bool,
    root_access: &str,
) -> nub_sandbox::SandboxPolicy {
    let project = root.join("project");
    let mut fs = if broad_root {
        json!({"/": root_access})
    } else {
        json!({(project.to_string_lossy()): "r"})
    };
    fs["$tmp"] = match case {
        "private" => json!("rw"),
        "deny" => json!(false),
        _ => panic!("unknown temporary storage case: {case}"),
    };
    let context = CompileCtx::new(
        Homes {
            home: root.join("home"),
            cache: root.join("cache"),
            // `$tmp` is a mode consumed during the fold; the backend-owned private directory is
            // allocated from the process OS temp location, not this CompileCtx anchor.
            tmp: root.join("compile-context-tmp"),
            project: project.clone(),
        },
        project,
        ScopeCapabilities::approved(),
        BTreeMap::new(),
    );
    let mut policy = compile(&json!({"fs": fs, "net": false}), &context)
        .expect("raw tmp composition policy compiles");
    policy.env.constructed.insert(CASE.into(), case.into());
    policy
}

fn command(root: &Path) -> CommandSpec {
    CommandSpec::new(std::env::current_exe().unwrap())
        .args(["--exact", "tmp_grant_composition_child", "--nocapture"])
        .cwd(root.join("project"))
        .redact_stdout(true)
        .redact_stderr(true)
}

fn run(
    root: &tempfile::TempDir,
    host_canary: &Path,
    case: &str,
    broad_root: bool,
    host_visible: bool,
) {
    let mut policy = policy(root.path(), case, broad_root, "r");
    policy.env.constructed.insert(
        HOST_CANARY.into(),
        host_canary.to_string_lossy().into_owned(),
    );
    policy.env.constructed.insert(
        PROJECT_CANARY.into(),
        root.path()
            .join("project/canary")
            .to_string_lossy()
            .into_owned(),
    );
    policy.env.constructed.insert(
        HOST_TMP_ROOT.into(),
        host_canary
            .parent()
            .expect("host temporary canary parent")
            .to_string_lossy()
            .into_owned(),
    );
    policy
        .env
        .constructed
        .insert(HOST_VISIBLE.into(), host_visible.to_string());
    let sandbox = Sandbox::new(&policy).expect("native sandbox is available");
    let prepared = sandbox
        .prepare(command(root.path()))
        .expect("child prepares");
    assert!(prepared.degradation.is_full(), "{:?}", prepared.degradation);
    let output = tool_output::output(prepared);
    assert!(
        output.status.success(),
        "{case} broad_root={broad_root} host_visible={host_visible} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

//! The `.env*` / `.npmrc` secret floor and the policy-file self-exclusion, enforced against a
//! real confined child.
//!
//! These are the only two deny-producing bands on the fs axis — the public grammar has no deny
//! form — so they are also the only thing that exercises deny-inside-allow at all. Landlock
//! cannot express either: its rules union and never subtract, so a `.env` inside a granted
//! project tree is readable to it no matter what the policy says. The refusal can only come
//! from the seccomp `USER_NOTIF` broker, per open, on the resolved canonical path.
//!
//! Every case here grants the project tree WHOLE and then expects one file inside it to be
//! refused. A test that passed because the tree was not granted would be measuring nothing, so
//! each child reads an ordinary sibling file first: that read is the positive control, and it
//! fails loudly if the grant is not live.
#![cfg(target_os = "linux")]

#[path = "common/tool_output.rs"]
mod tool_output;

use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

const CASE: &str = "NUB_SECRET_FLOOR_CASE";
const PROJECT: &str = "NUB_SECRET_FLOOR_PROJECT";
/// Set only for the whole-disk case, where the child owes one extra control.
const WIDE: &str = "NUB_SECRET_FLOOR_WHOLE_DISK";

/// The confined half. Reads the control, then asserts the denied path is refused.
#[test]
fn secret_floor_child() {
    let Ok(case) = std::env::var(CASE) else {
        return;
    };
    let project = std::env::var(PROJECT).expect("project root");
    let project = Path::new(&project);

    // POSITIVE CONTROL, first and unconditionally: the grant this whole file depends on. If the
    // project tree were not readable, every assertion below would pass for the wrong reason.
    assert_eq!(
        fs::read_to_string(project.join("index.js")).expect("the granted project tree is readable"),
        "ordinary-source",
    );

    let denied: &[&str] = match case.as_str() {
        "env" => &[".env", ".env.local"],
        "npmrc" => &[".npmrc"],
        "env-dir" => &[".env.d/production"],
        "policy-file" => &["sandbox.json"],
        // Uppercase spellings a case-sensitive glob would read through — the floor is
        // case-insensitive so it catches them, matching the env-VARIABLE floor.
        "env-case" => &[".ENV", ".Env", ".NPMRC"],
        other => panic!("unknown secret-floor case: {other}"),
    };
    // THE WHOLE-DISK CASE OWES A THIRD CONTROL, and it is the one that makes that case mean
    // anything. Under `fs: {"/": "rw"}` Landlock binds `/` read-write and contributes NOTHING —
    // its rules union, so every floor deny is dropped on the way in and the broker is the only
    // thing left refusing. Reading a file OUTSIDE the project is what proves the grant really is
    // whole-disk: a narrower grant would refuse this read, and then the refusals below would be
    // the grant's doing rather than the floor's.
    if std::env::var_os(WIDE).is_some() {
        let outside = project.parent().expect("fixture root").join("outside.txt");
        assert_eq!(
            fs::read_to_string(&outside).unwrap_or_else(|e| panic!(
                "{} must be readable under a whole-disk grant: {e}",
                outside.display()
            )),
            "outside-the-project",
        );
    }

    // SECOND POSITIVE CONTROL, and the one the WRITE half below rests on: the tree is granted
    // read-WRITE, so an ordinary write inside it must succeed. Without this, a tree that was
    // merely read-only would make every write refusal below pass for the wrong reason — the
    // floor would look enforced while nothing but the missing grant was stopping anything.
    fs::write(project.join("scratch.txt"), "ordinary-write")
        .expect("the granted project tree is writable");

    for leaf in denied {
        let path = project.join(leaf);
        let read = fs::read_to_string(&path);
        assert!(
            read.is_err(),
            "{} was readable inside a granted project tree: {:?}",
            path.display(),
            read,
        );

        // THE WRITE HALF. A floor that only refused reads would leave the more damaging half
        // open: `fs::write` is `O_WRONLY|O_CREAT|O_TRUNC`, so a command that cannot READ
        // `.npmrc` could still TRUNCATE it, or replace it with a registry line pointing at an
        // attacker's host for whatever runs next. Landlock alone cannot refuse this — the tree
        // is granted `rw` and its rules only ever union — so the refusal has to come from the
        // broker, exactly as it does for the read.
        let wrote = fs::write(&path, "overwritten-by-the-confined-command");
        assert!(
            wrote.is_err(),
            "{} was WRITABLE inside a granted project tree: the floor refused the read and let \
             the overwrite through",
            path.display(),
        );
    }
}

/// `.env` and a dotted variant, both inside the granted tree, both refused.
#[test]
fn a_granted_project_tree_still_refuses_its_dotenv_files() {
    run("env");
}

/// A project-local `.npmrc` can hardcode a registry token, so it rides the same band.
#[test]
fn a_granted_project_tree_still_refuses_a_project_npmrc() {
    run("npmrc");
}

/// The floor is case-INSENSITIVE, matching the env-VARIABLE floor's reasoning ("an uppercase
/// can't slip past"). A checkout made on a case-insensitive host (macOS/Windows) can carry
/// `.ENV`/`.NPMRC`; a case-sensitive glob would read them through on Linux. Reverting
/// `case_fold_glob` in the compiler defaults turns exactly this red.
#[test]
fn a_granted_project_tree_refuses_uppercase_secret_file_variants() {
    run("env-case");
}

/// The SUBTREE half: a `.env.d/`-style directory of per-target secrets. The leaf band matches
/// the directory; this proves its CONTENTS are refused too, which is a separate glob.
#[test]
fn a_dotenv_named_directory_refuses_its_contents() {
    run("env-dir");
}

/// The policy file itself — a confined command must not be able to read the rules confining it.
#[test]
fn a_confined_command_cannot_read_the_policy_that_confines_it() {
    run("policy-file");
}

/// The floor is not a deny-everything: a policy that confines nothing (`fs: true`, the explicit
/// whole-disk escape hatch) must not acquire one, or the escape hatch would not be an escape.
/// Asserted on the compiled IR rather than a child, because the point is that no rule EXISTS.
#[test]
fn the_whole_disk_escape_hatch_takes_no_floor() {
    let root = tempfile::tempdir().expect("fixture root");
    let policy = compile(&json!({"fs": true, "net": false}), &ctx(root.path()))
        .expect("the escape hatch compiles");
    assert!(
        policy.fs.rules.entries.is_empty(),
        "`fs: true` must stay an unconditional allow: {:?}",
        policy.fs.rules.entries,
    );
}

/// A deny-all fs axis takes no floor either — there is nothing for a deny to sit inside, so the
/// band would be inert noise in every dump and snapshot.
#[test]
fn a_deny_all_axis_takes_no_floor() {
    let root = tempfile::tempdir().expect("fixture root");
    let policy = compile(&json!({"fs": false, "net": false}), &ctx(root.path()))
        .expect("a deny-all axis compiles");
    assert!(
        policy.fs.rules.entries.is_empty(),
        "a deny-all axis must carry no entries: {:?}",
        policy.fs.rules.entries,
    );
}

fn ctx(root: &Path) -> CompileCtx {
    let project = root.join("project");
    CompileCtx::new(
        Homes {
            home: root.join("home"),
            cache: root.join("cache"),
            tmp: root.join("tmp"),
            project: project.clone(),
        },
        project,
        ScopeCapabilities::approved(),
        BTreeMap::new(),
    )
}

/// A project tree holding one ordinary file and every secret shape the floor covers.
fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("fixture root");
    let project = root.path().join("project");
    fs::create_dir_all(project.join(".env.d")).unwrap();
    fs::write(project.join("index.js"), "ordinary-source").unwrap();
    fs::write(project.join(".env"), "TOKEN=secret").unwrap();
    fs::write(project.join(".env.local"), "TOKEN=secret").unwrap();
    fs::write(project.join(".npmrc"), "//registry/:_authToken=secret").unwrap();
    fs::write(project.join(".env.d/production"), "TOKEN=secret").unwrap();
    fs::write(project.join("sandbox.json"), "{}").unwrap();
    // Uppercase spellings for the case-insensitivity case. Distinct files from their lowercase
    // siblings on Linux's case-sensitive fs, so the floor must match every case to catch them.
    fs::write(project.join(".ENV"), "TOKEN=secret").unwrap();
    fs::write(project.join(".Env"), "TOKEN=secret").unwrap();
    fs::write(project.join(".NPMRC"), "//registry/:_authToken=secret").unwrap();
    fs::write(root.path().join("outside.txt"), "outside-the-project").unwrap();
    root
}

/// A whole-disk grant is the shape where Landlock stops helping and the broker is alone.
///
/// `compile_mount_plan` binds `/` and DROPS every deny, because Landlock cannot subtract from a
/// root grant — a refusal it used to make outright, and now delegates. `linux_grants.rs` records
/// that the two are coupled and that losing the broker's arming would silently take the floor
/// with it; this is what notices. Every other case here grants a project subtree, where Landlock
/// is also constraining, so none of them can tell a live broker from a narrow grant.
#[test]
fn a_whole_disk_grant_still_refuses_the_secret_floor() {
    run_with("env", Some("/"));
    run_with("npmrc", Some("/"));
}

fn run(case: &str) {
    run_with(case, None);
}

/// `grant` names the fs root to hand the child: `None` ⇒ the project tree, `Some(path)` ⇒ that
/// path, which the case above uses to grant the whole disk.
fn run_with(case: &str, grant: Option<&str>) {
    let root = fixture();
    let project = root.path().join("project");
    let mut ctx = ctx(root.path());
    if case == "policy-file" {
        ctx = ctx.with_policy_files(vec![project.join("sandbox.json")]);
    }
    // The tree granted WHOLE and read-write: the floor has to win over a grant strictly broader
    // than the files it denies, which is the case a merely-narrower grant would never test.
    let granted = grant.map_or_else(|| project.to_string_lossy().into_owned(), str::to_string);
    let mut policy = compile(&json!({"fs": {granted: "rw"}, "net": false}), &ctx)
        .expect("a project grant compiles");
    policy.env.constructed.insert(CASE.into(), case.into());
    if grant.is_some() {
        policy.env.constructed.insert(WIDE.into(), "1".into());
    }
    policy
        .env
        .constructed
        .insert(PROJECT.into(), project.to_string_lossy().into_owned());

    let sandbox = Sandbox::new(&policy).expect("the supervised backend acquires");
    let prepared = sandbox
        .prepare(
            CommandSpec::new(std::env::current_exe().unwrap())
                .args(["--exact", "secret_floor_child", "--nocapture"])
                .cwd(&project)
                .redact_stdout(true)
                .redact_stderr(true),
        )
        .expect("the child prepares");
    assert!(prepared.degradation.is_full(), "{:?}", prepared.degradation);
    let output = tool_output::output(prepared);
    assert!(
        output.status.success(),
        "secret-floor case `{case}` failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

//! macOS move/rename secret-relocation bypass — REAL enforcement test.
//!
//! A policy protects a secret with a write-DENY keyed to its PATH. Two macOS holes let a
//! child relocate the bytes past that path-keyed deny and read them at the new path:
//!   - Hole #1 (default-reachable): the backend emits a trailing confstr
//!     `(allow file-write* <$TMPDIR>)` scratch grant (so the Apple toolchain can write
//!     `xcrun_db`). SBPL is last-match-wins, so that grant re-opens unlink/rename on a
//!     denied secret living under `$TMPDIR` → `mv .env leaked` then read `leaked`. Reachable
//!     even for a plain generous-read policy when the secret sits under `$TMPDIR`.
//!   - Hole #2 (anchored user denies): a LITERAL deny `<root>/proj/.env` blocks the file
//!     `mv`, but `mv proj proj2` renames the rw-granted container out from under the anchor.
//!
//! The fix (`emit_move_block` in `backend/macos.rs`) re-asserts the unlink/create denies
//! AFTER the confstr grant, plus an ancestor-dir move-block for anchored denies. Every
//! assertion is paired with an UNCONFINED negative control that LEAKS, so a pass cannot be
//! hollow (it proves the attack is real and the fixture holds a genuine secret), and each
//! case checks the non-regression that legit scratch/in-dir writes still succeed.
//! macOS-only; other OSes skip cleanly.
#![cfg(target_os = "macos")]

use nub_sandbox::policy::{
    CanonGlob, Effect, EnvPolicy, FsAccess, FsRule, FsRuleSet, SandboxPolicy,
};
use nub_sandbox::{CommandSpec, apply};
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

/// A value that appears only in the planted secret, so finding it in a child's stdout is
/// unambiguous proof the child relocated-and-read the secret.
const MARKER: &str = "TOPSECRETxMOVExRELOCATExMARKER";

fn fs_policy(entries: Vec<FsRule>) -> SandboxPolicy {
    let mut p = SandboxPolicy {
        env: EnvPolicy::resolved(std::env::vars().collect()),
        ..Default::default()
    };
    p.fs.rules = FsRuleSet {
        entries,
        default_effect: Effect::Deny,
    };
    p
}

fn rule(m: &str, effect: Effect, access: FsAccess) -> FsRule {
    FsRule {
        matcher: CanonGlob(m.to_string()),
        effect,
        access,
    }
}

/// Run `sh -c script` in `cwd` under `policy` via the REAL `apply()` path; trimmed stdout.
fn run_confined(policy: &SandboxPolicy, cwd: &Path, script: &str) -> String {
    let spec = CommandSpec::new("/bin/sh").args(["-c", script]).cwd(cwd);
    let out = apply(policy, spec)
        .expect("apply")
        .output()
        .expect("spawn confined child");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The same script with no sandbox — the negative control proving the relocation leaks.
fn run_unconfined(cwd: &Path, script: &str) -> String {
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .stderr(Stdio::null())
        .output()
        .expect("spawn unconfined child");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The canonical DARWIN confstr temp dir (`realpath` of `getconf DARWIN_USER_TEMP_DIR`) —
/// where the backend's trailing scratch grant lands, so a secret placed here exercises
/// hole #1. `None` if it can't be resolved (skip, never a hollow pass).
fn darwin_temp_dir() -> Option<std::path::PathBuf> {
    let raw = Command::new("getconf")
        .arg("DARWIN_USER_TEMP_DIR")
        .output()
        .ok()?;
    let p = String::from_utf8_lossy(&raw.stdout).trim().to_string();
    std::fs::canonicalize(p).ok()
}

#[test]
fn tmpdir_secret_cannot_be_relocated_by_file_mv() {
    // HOLE #1 (default-reachable): generous-read + `**/.env` deny; the secret lives under
    // `$TMPDIR`, where the confstr scratch grant would re-open write. `mv .env leaked` must
    // be BLOCKED so the secret can't be read at the relocated path.
    let Some(temp) = darwin_temp_dir() else {
        eprintln!("skipping: DARWIN_USER_TEMP_DIR unresolved");
        return;
    };
    let dir = TempDir::new_in(&temp).expect("tmp under $TMPDIR");
    std::fs::write(dir.path().join(".env"), MARKER).expect("plant secret");

    let policy = fs_policy(vec![
        rule("**", Effect::Allow, FsAccess::Read),
        rule("**/.env", Effect::Deny, FsAccess::Read),
    ]);

    let script = "/bin/mv .env leaked.txt 2>/dev/null; /bin/cat leaked.txt 2>/dev/null";

    // NEGATIVE CONTROL: unconfined, the relocation leaks — the attack is real.
    let control = run_unconfined(dir.path(), script);
    // Restore the secret name for the confined run (the control moved it).
    let _ = std::fs::rename(dir.path().join("leaked.txt"), dir.path().join(".env"));
    let confined = run_confined(&policy, dir.path(), script);

    assert!(
        control.contains(MARKER),
        "neg-control: unconfined `mv .env leaked; cat leaked` must leak the secret (got {control:?})"
    );
    assert!(
        !confined.contains(MARKER),
        "confined: the $TMPDIR-resident secret must NOT be relocatable past its deny (got {confined:?})"
    );
}

#[test]
fn tmpdir_scratch_write_still_works_under_the_move_block() {
    // NON-REGRESSION for hole #1's fix: re-asserting the `.env` deny must NOT re-deny the
    // whole `$TMPDIR` — a legit scratch write (the xcrun_db shape) still succeeds.
    let Some(temp) = darwin_temp_dir() else {
        eprintln!("skipping: DARWIN_USER_TEMP_DIR unresolved");
        return;
    };
    let dir = TempDir::new_in(&temp).expect("tmp under $TMPDIR");
    std::fs::write(dir.path().join(".env"), MARKER).expect("plant secret");

    let policy = fs_policy(vec![
        rule("**", Effect::Allow, FsAccess::Read),
        rule("**/.env", Effect::Deny, FsAccess::Read),
    ]);

    // Write a non-secret scratch file into $TMPDIR (not a `.env`), then read it back.
    let scratch = dir.path().join("scratch_probe.txt");
    let script = format!(
        "echo OKSCRATCH > {p:?}; /bin/cat {p:?}",
        p = scratch.to_string_lossy()
    );
    let out = run_confined(&policy, dir.path(), &script);
    assert!(
        out.contains("OKSCRATCH"),
        "a non-secret $TMPDIR scratch write must still succeed under the move-block (got {out:?})"
    );
}

#[test]
fn anchored_deny_secret_cannot_be_relocated_by_ancestor_rename() {
    // HOLE #2: a LITERAL deny `<root>/proj/.env` inside an rw-granted `<root>`. The direct
    // file `mv` is already blocked, but `mv proj proj2` relocates the container. Both must
    // be BLOCKED; a legit write inside the granted dir must still succeed.
    let root = TempDir::new_in("/private/tmp").expect("tmp outside $TMPDIR");
    let root_c = std::fs::canonicalize(root.path()).expect("canon root");
    std::fs::create_dir(root_c.join("proj")).expect("mkdir proj");
    std::fs::write(root_c.join("proj/.env"), MARKER).expect("plant secret");

    let root_s = root_c.to_string_lossy().to_string();
    let policy = fs_policy(vec![
        rule("**", Effect::Allow, FsAccess::Read),
        rule(&root_s, Effect::Allow, FsAccess::ReadWrite),
        rule(&format!("{root_s}/proj/.env"), Effect::Deny, FsAccess::Read),
    ]);

    let relocate = "/bin/mv proj proj2 2>/dev/null; /bin/cat proj2/.env 2>/dev/null";

    // NEGATIVE CONTROL: unconfined, the ancestor rename relocates and leaks.
    let control = run_unconfined(&root_c, relocate);
    let _ = std::fs::rename(root_c.join("proj2"), root_c.join("proj"));
    assert!(
        control.contains(MARKER),
        "neg-control: unconfined `mv proj proj2; cat proj2/.env` must leak (got {control:?})"
    );

    // CONFINED: the container rename is blocked → no relocation, no leak.
    let confined = run_confined(&policy, &root_c, relocate);
    assert!(
        !confined.contains(MARKER),
        "confined: an ancestor-dir rename must NOT relocate the anchored secret (got {confined:?})"
    );

    // And the direct file mv stays blocked too.
    let direct = run_confined(
        &policy,
        &root_c,
        "cd proj && /bin/mv .env leaked.txt 2>/dev/null; /bin/cat leaked.txt 2>/dev/null",
    );
    assert!(
        !direct.contains(MARKER),
        "confined: the direct file mv must stay blocked (got {direct:?})"
    );

    // NON-REGRESSION: a legit write inside the granted project dir still works.
    let write_ok = run_confined(
        &policy,
        &root_c,
        "echo OKWRITE > proj/other.txt; /bin/cat proj/other.txt",
    );
    assert!(
        write_ok.contains("OKWRITE"),
        "a legit write inside the rw-granted dir must still succeed (got {write_ok:?})"
    );
}

#[test]
fn regex_dir_pinning_deny_secret_cannot_be_relocated_by_ancestor_rename() {
    // HOLE #2, regex twin: a user directory-pinning glob deny `<root>/secrets/*.key` (from
    // `!secrets/*.key`) is a REGEX, so the re-asserted deny blocks the leaf `*.key` files but
    // not their literal container `<root>/secrets` — `mv secrets secretz` relocates the whole
    // dir out from under the pattern. The literal-prefix move-block must pin `<root>/secrets`
    // so the container rename is blocked; a legit write inside secrets/ must still succeed.
    let root = TempDir::new_in("/private/tmp").expect("tmp outside $TMPDIR");
    let root_c = std::fs::canonicalize(root.path()).expect("canon root");
    std::fs::create_dir(root_c.join("secrets")).expect("mkdir secrets");
    std::fs::write(root_c.join("secrets/topsecret.key"), MARKER).expect("plant secret");

    let root_s = root_c.to_string_lossy().to_string();
    let policy = fs_policy(vec![
        rule("**", Effect::Allow, FsAccess::Read),
        rule(&root_s, Effect::Allow, FsAccess::ReadWrite),
        rule(
            &format!("{root_s}/secrets/*.key"),
            Effect::Deny,
            FsAccess::Read,
        ),
    ]);

    let relocate =
        "/bin/mv secrets secretz 2>/dev/null; /bin/cat secretz/topsecret.key 2>/dev/null";

    // NEGATIVE CONTROL: unconfined, the container rename relocates and leaks the leaf.
    let control = run_unconfined(&root_c, relocate);
    let _ = std::fs::rename(root_c.join("secretz"), root_c.join("secrets"));
    assert!(
        control.contains(MARKER),
        "neg-control: unconfined `mv secrets secretz; cat secretz/*.key` must leak (got {control:?})"
    );

    // CONFINED: the container rename is blocked → no relocation, no leak.
    let confined = run_confined(&policy, &root_c, relocate);
    assert!(
        !confined.contains(MARKER),
        "confined: an ancestor-dir rename must NOT relocate the regex-pinned secret (got {confined:?})"
    );

    // NON-REGRESSION: a legit write INSIDE secrets/ still works (the pin is exact-path on the
    // dir, never a subpath, so it blocks renaming secrets/ but not writing under it).
    let write_ok = run_confined(
        &policy,
        &root_c,
        "echo OKINSIDE > secrets/note.txt; /bin/cat secrets/note.txt",
    );
    assert!(
        write_ok.contains("OKINSIDE"),
        "a legit write inside the pinned dir must still succeed (got {write_ok:?})"
    );
}

// ── documented OPEN residuals (LIMITATIONS.md) ──────────────────────────────────
// The two closed shapes above (literal `(subpath)` deny + regex-dir-prefix deny) ARE
// move-blocked and are the positive controls. The two tests below BOUND the residual:
// a deny whose relocation-sensitive container has NO absolute literal directory prefix
// to anchor stays relocatable by an ancestor rename. Each asserts the precise bound —
// the leaf read-deny still holds (only the container rename escapes) — and pairs an
// UNCONFINED control proving the relocation is a real leak. These pass by REPRODUCING
// the residual; when a future change closes it, the `confined … must still leak`
// assertion flips and this test must be updated to a `must NOT leak` (like the two
// above), turning the residual's closure into a visible, deliberate edit.

#[test]
fn floating_name_subtree_deny_stays_relocatable_residual() {
    // `!**/secrets/**` → a subtree deny whose matched dir name FLOATS (no fixed absolute
    // anchor). `regex_literal_dir_prefix` finds no literal prefix, so no ancestor is
    // pinned. The leaf read-deny holds, but `mv secrets elsewhere` moves the tree to a
    // path the pattern no longer matches → readable at the new path.
    let root = TempDir::new_in("/private/tmp").expect("tmp outside $TMPDIR");
    let root_c = std::fs::canonicalize(root.path()).expect("canon root");
    std::fs::create_dir(root_c.join("secrets")).expect("mkdir secrets");
    std::fs::write(root_c.join("secrets/x.key"), MARKER).expect("plant secret");

    let root_s = root_c.to_string_lossy().to_string();
    let policy = fs_policy(vec![
        rule("**", Effect::Allow, FsAccess::Read),
        rule(&root_s, Effect::Allow, FsAccess::ReadWrite),
        rule("**/secrets/**", Effect::Deny, FsAccess::Read),
    ]);

    // BOUND: the direct read at the original path IS denied (the rule's primary read-deny
    // holds — proving the sandbox applies, so the relocation leak is a genuine residual,
    // not a non-enforcing sandbox).
    let direct = run_confined(&policy, &root_c, "/bin/cat secrets/x.key 2>/dev/null");
    assert!(
        !direct.contains(MARKER),
        "confined: the leaf read-deny must hold at the original path (got {direct:?})"
    );

    let relocate = "/bin/mv secrets elsewhere 2>/dev/null; /bin/cat elsewhere/x.key 2>/dev/null";
    let control = run_unconfined(&root_c, relocate);
    let _ = std::fs::rename(root_c.join("elsewhere"), root_c.join("secrets"));
    assert!(
        control.contains(MARKER),
        "neg-control: unconfined `mv secrets elsewhere; cat` must leak (got {control:?})"
    );

    // RESIDUAL: no literal anchor → the container rename is NOT blocked → the secret is
    // readable at the relocated path. (If this stops leaking, the residual closed —
    // update to `must NOT leak`.)
    let confined = run_confined(&policy, &root_c, relocate);
    assert!(
        confined.contains(MARKER),
        "residual: a floating-name subtree deny stays relocatable — expected the leak to \
         reproduce (got {confined:?}); if it no longer leaks the residual closed"
    );
}

#[test]
fn partial_component_glob_deny_stays_relocatable_residual() {
    // `!sec*/x.key` → a PARTIAL glob in a non-leaf component. The literal directory
    // prefix is only `<root>` (the pin stops above `<root>/secrets`), so the container
    // `<root>/secrets` — matched by the partial `sec*` — is below the anchor and unpinned.
    // `mv secrets elsewhere` renames it to a name outside `sec*` → the deny no longer
    // matches → readable.
    let root = TempDir::new_in("/private/tmp").expect("tmp outside $TMPDIR");
    let root_c = std::fs::canonicalize(root.path()).expect("canon root");
    std::fs::create_dir(root_c.join("secrets")).expect("mkdir secrets");
    std::fs::write(root_c.join("secrets/x.key"), MARKER).expect("plant secret");

    let root_s = root_c.to_string_lossy().to_string();
    let policy = fs_policy(vec![
        rule("**", Effect::Allow, FsAccess::Read),
        rule(&root_s, Effect::Allow, FsAccess::ReadWrite),
        rule(
            &format!("{root_s}/sec*/x.key"),
            Effect::Deny,
            FsAccess::Read,
        ),
    ]);

    // BOUND: the direct read at the original path IS denied (the rule's primary read-deny
    // holds — proving the sandbox applies, so the relocation leak is a genuine residual).
    let direct = run_confined(&policy, &root_c, "/bin/cat secrets/x.key 2>/dev/null");
    assert!(
        !direct.contains(MARKER),
        "confined: the leaf read-deny must hold at the original path (got {direct:?})"
    );

    let relocate = "/bin/mv secrets elsewhere 2>/dev/null; /bin/cat elsewhere/x.key 2>/dev/null";
    let control = run_unconfined(&root_c, relocate);
    let _ = std::fs::rename(root_c.join("elsewhere"), root_c.join("secrets"));
    assert!(
        control.contains(MARKER),
        "neg-control: unconfined `mv secrets elsewhere; cat` must leak (got {control:?})"
    );

    // RESIDUAL: the `sec*`-matched container is below the literal anchor → unpinned →
    // relocatable. (If this stops leaking, the residual closed — update to `must NOT leak`.)
    let confined = run_confined(&policy, &root_c, relocate);
    assert!(
        confined.contains(MARKER),
        "residual: a partial-component glob deny stays relocatable — expected the leak to \
         reproduce (got {confined:?}); if it no longer leaks the residual closed"
    );
}

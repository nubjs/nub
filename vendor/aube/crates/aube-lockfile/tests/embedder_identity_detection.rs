//! Integration test for the embedder-parameterized detection carve-outs:
//! custom self-names + strict canonical-lockfile coexistence.
//!
//! Lives in its own integration-test binary — i.e. its own process —
//! because the active embedder identity is once-per-process: registering a
//! custom profile here would poison the in-crate unit tests that assert the
//! default (`aube`) identity. The profile is selected the way every embedder
//! selects one — a single `aube_util::set_embedder(&PROFILE)` — rather than
//! the old per-seam runtime setters, which the compile-time `Embedder` model
//! replaced.

use aube_lockfile::{Error, LockfileKind, ResolvedLockfileKind, resolve_project_lockfile_kind};
use aube_util::Embedder;

/// A strict-identity embedder: its canonical lockfile is `lock.yaml`, it
/// answers to the self-name `mytool`, and — unlike standalone aube — it does
/// *not* let its canonical lockfile silently win beside a foreign one
/// (`canonical_lockfile_always_wins: false`). The remaining fields are
/// irrelevant to detection and just reproduce a plausible profile.
static MYTOOL: Embedder = Embedder {
    name: "mytool",
    display_name: "mytool",
    vendor: None,
    version: "1.0.0",
    user_agent: "mytool/1.0.0",
    self_names: &["mytool"],
    compatible_names: &["pnpm"],
    lockfile_basename: "lock.yaml",
    workspace_yaml: Some("mytool-workspace.yaml"),
    manifest_namespace: "mytool",
    env_prefix: Some("MYTOOL"),
    config_env_prefix: Some("MYTOOL"),
    cache_namespace: "mytool",
    data_namespace: "mytool",
    canonical_lockfile_always_wins: false,
    runtime_switching: true,
    self_engines_check: true,
    self_update_enabled: true,
    warm_store_verify: true,
    no_churn_lockfile_write: false,
    read_branded_settings_env: true,
    primer_ttl: None,
};

fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in files {
        std::fs::write(dir.path().join(name), body).unwrap();
    }
    dir
}

#[test]
fn strict_embedder_identity_decision_rows() {
    aube_util::set_embedder(&MYTOOL);

    // The canonical lockfile (the profile's `lock.yaml`) + no declaration →
    // the canonical kind (embedder identity), even with always-wins off:
    // nothing foreign sits beside it to trigger the ambiguity rule.
    let d = project(&[
        ("package.json", r#"{"name":"t"}"#),
        ("lock.yaml", "lockfileVersion: '9.0'\n"),
    ]);
    assert_eq!(
        resolve_project_lockfile_kind(d.path()).unwrap(),
        ResolvedLockfileKind::Existing(LockfileKind::Aube)
    );

    // lock.yaml beside a foreign lockfile, no declaration → loud ambiguity
    // (the upstream always-wins carve-out is demoted under strict identity).
    let d = project(&[
        ("package.json", r#"{"name":"t"}"#),
        ("lock.yaml", "lockfileVersion: '9.0'\n"),
        ("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"),
    ]);
    let err = resolve_project_lockfile_kind(d.path()).unwrap_err();
    let Error::AmbiguousLockfiles { found } = &err else {
        panic!("expected AmbiguousLockfiles, got {err:?}");
    };
    assert!(
        found.contains("lock.yaml") && found.contains("pnpm-lock.yaml"),
        "ambiguity must name both files: {found}"
    );

    // Declared pnpm + only lock.yaml → contradiction naming the file.
    let d = project(&[
        (
            "package.json",
            r#"{"name":"t","packageManager":"pnpm@10.0.0"}"#,
        ),
        ("lock.yaml", "lockfileVersion: '9.0'\n"),
    ]);
    let err = resolve_project_lockfile_kind(d.path()).unwrap_err();
    let Error::DeclarationMismatch { found, .. } = &err else {
        panic!("expected DeclarationMismatch, got {err:?}");
    };
    assert_eq!(found, "lock.yaml");

    // The registered self-name behaves exactly like a declared `aube`
    // upstream: accepts an existing foreign format, pins the canonical
    // format when fresh.
    let d = project(&[
        (
            "package.json",
            r#"{"name":"t","packageManager":"mytool@1.0.0"}"#,
        ),
        ("package-lock.json", "{}"),
    ]);
    assert_eq!(
        resolve_project_lockfile_kind(d.path()).unwrap(),
        ResolvedLockfileKind::Existing(LockfileKind::Npm)
    );
    let d = project(&[(
        "package.json",
        r#"{"name":"t","packageManager":"mytool@1.0.0"}"#,
    )]);
    assert_eq!(
        resolve_project_lockfile_kind(d.path()).unwrap(),
        ResolvedLockfileKind::DeclaredFresh(LockfileKind::Aube)
    );
}

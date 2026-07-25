//! Cancellation rollback of the embed `add` API for a host mid-lockfile-rename.
//!
//! A host that renames its lockfile carries the prior basename in
//! `lockfile_legacy_basenames` (nub: `lock.yaml`, superseded by `nub.lock`), and
//! the first real write during an add MIGRATES it — writing the current name and
//! deleting the legacy file. So `add_to_project`'s cancellation rollback has to
//! restore the legacy name too, exactly as the `--no-save` paths do; restoring
//! only the current name drops the host's checked-in lockfile.
//!
//! Its own test binary because the embedder profile is process-global and
//! first-write-wins: `embed.rs`'s host has no legacy basenames (standalone
//! aube's list is empty, so none of this is reachable there) and is deliberately
//! shaped to mirror upstream's `Host`.

use aube::embed::{AUBE, Host, InstallControl, InstallEvent, InstallReporter};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const LEGACY_LOCKFILE: &str = "renamehost-legacy-lock.yaml";
/// Minimal parseable lockfile in the pnpm-v9 form aube's canonical writer emits.
const LOCKFILE_BYTES: &str = "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n";

static RENAME_HOST: Host = Host {
    name: "renamehost",
    display_name: "renamehost",
    version: "1.0.0",
    user_agent: "renamehost/1.0.0",
    self_names: &["renamehost"],
    lockfile_basename: "renamehost-lock.yaml",
    lockfile_legacy_basenames: &[LEGACY_LOCKFILE],
    workspace_yaml: None,
    manifest_namespace: "renamehost",
    env_prefix: None,
    config_env_prefix: None,
    cache_namespace: "renamehost",
    data_namespace: "renamehost",
    runtime_switching: false,
    self_engines_check: false,
    self_update_enabled: false,
    ..AUBE
};

/// Stands in for the migrating lockfile write. A real add deletes the legacy
/// file from inside `write_lockfile_as`, but the install pipeline's last
/// cancellation checkpoint sits at the head of the link phase — ahead of every
/// event a reporter can observe after that write — so the write-then-cancel
/// ordering cannot be driven from outside the pipeline. Deleting the file at a
/// point where cancellation *is* honored reproduces the on-disk state the
/// rollback has to undo.
struct MigrateThenCancel {
    legacy: PathBuf,
    control: Mutex<Option<InstallControl>>,
}

impl InstallReporter for MigrateThenCancel {
    fn report(&self, _event: InstallEvent) {
        if let Some(control) = self.control.lock().unwrap().take() {
            std::fs::remove_file(&self.legacy).expect("legacy lockfile must exist to be migrated");
            control.cancel();
        }
    }
}

#[tokio::test]
async fn cancelled_add_restores_migrated_legacy_lockfile() {
    aube::embed::initialize(
        &RENAME_HOST,
        vec![("minimumReleaseAge".to_string(), "0".to_string())],
    );

    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0"}
"#,
    )
    .unwrap();
    std::fs::create_dir_all(project.path().join("localdep")).unwrap();
    std::fs::write(
        project.path().join("localdep/package.json"),
        r#"{"name":"localdep","version":"1.0.0"}
"#,
    )
    .unwrap();
    let legacy = project.path().join(LEGACY_LOCKFILE);
    std::fs::write(&legacy, LOCKFILE_BYTES).unwrap();

    let reporter = Arc::new(MigrateThenCancel {
        legacy: legacy.clone(),
        control: Mutex::new(None),
    });
    let control = InstallControl::events(reporter.clone());
    *reporter.control.lock().unwrap() = Some(control.clone());

    let error = aube::embed::add(
        project.path(),
        &["localdep@file:./localdep".to_string()],
        aube::embed::AddToProjectOptions {
            offline: true,
            control,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();

    assert_eq!(
        aube::embed::error_code(&error).as_deref(),
        Some(aube_codes::errors::ERR_AUBE_INSTALL_CANCELLED),
        "the add must fail as cancelled, not for some unrelated reason"
    );
    assert_eq!(
        std::fs::read_to_string(&legacy).ok().as_deref(),
        Some(LOCKFILE_BYTES),
        "cancellation must restore {} — the legacy-named lockfile the add migrated away",
        legacy.display()
    );
}

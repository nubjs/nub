//! Every DEPENDENCY lifecycle phase is confined by nub's build-jail — not just
//! `postinstall`.
//!
//! The jail's POLICY is phase-agnostic by construction: `compile_build_jail` takes no
//! script name, so a per-phase *enforcement* test would be three copies of one
//! assertion. Enforcement is covered once, end-to-end, by
//! `nub-sandbox/tests/build_jail_enforcement.rs`. What can actually regress is
//! ROUTING — aube decides per dep hook whether to hand the spawn to the embedder's
//! confiner, so a phase could silently start spawning unconfined. This pins that every
//! phase in `DEP_LIFECYCLE_HOOKS` reaches the hook, and that none escapes it.
//!
//! Root-package scripts are deliberately NOT routed here (they are user-authored and
//! trusted); that exemption is asserted in `pm_engine::build_jail`'s module doc and is
//! out of scope for this file, which covers dependency scripts only.

use std::sync::{Arc, Mutex};

/// Stands in for the confiner: records the phase it was handed and reports success
/// WITHOUT spawning. Every fixture script is `exit 1`, so if a phase ever bypasses the
/// hook the real spawn runs, exits non-zero, and `run_dep_hook` fails — the bypass is
/// caught even if the recorder somehow missed it.
#[derive(Debug, Default)]
struct Recorder {
    phases: Mutex<Vec<String>>,
    /// What the opt-out gate was asked about. The identity has to survive the whole
    /// `run_dep_hook` → `SandboxScope` → `confines` path or nub's per-package opt-out
    /// silently degrades to "no package ever opts out".
    gated: Mutex<Vec<Option<String>>>,
}

impl aube_util::LifecycleSandbox for Recorder {
    fn confines(&self, package_name: Option<&str>, _project_root: &std::path::Path) -> bool {
        self.gated
            .lock()
            .expect("recorder lock")
            .push(package_name.map(str::to_string));
        true
    }

    fn run(
        &self,
        spawn: aube_util::LifecycleSandboxSpawn,
    ) -> std::io::Result<std::process::ExitStatus> {
        let phase = spawn
            .env_delta
            .iter()
            .find(|(k, _)| k == "npm_lifecycle_event")
            .and_then(|(_, v)| v.clone())
            .and_then(|v| v.into_string().ok())
            .unwrap_or_else(|| "<unstamped>".to_string());
        self.phases.lock().expect("recorder lock").push(phase);
        Ok(success())
    }
}

fn success() -> std::process::ExitStatus {
    #[cfg(unix)]
    {
        std::os::unix::process::ExitStatusExt::from_raw(0)
    }
    #[cfg(windows)]
    {
        std::os::windows::process::ExitStatusExt::from_raw(0)
    }
}

/// Minimal profile that opts into embedder-owned lifecycle confinement — the one field
/// that drives the routing under test. Mirrors nub's real `NUB` profile for that field;
/// the rest are inert defaults. Declared locally because nub-cli's `NUB` is crate-private.
///
/// This file must stay a SINGLE test. It leaves two process globals dirty — the
/// `set_embedder` `OnceLock` (first write wins, so a second test could not re-point it)
/// and the engine context's installed hook — and cargo gives an integration-test file its
/// own process, which is the whole isolation. A second test added here would silently
/// inherit both.
static JAILED: aube_util::Embedder = aube_util::Embedder {
    name: "jailtest",
    display_name: "jailtest",
    vendor: None,
    version: "0.0.0",
    user_agent: "jailtest/0.0.0",
    self_names: &["jailtest"],
    compatible_names: &[],
    lockfile_basename: "jailtest-lock.yaml",
    lockfile_legacy_basenames: &[],
    workspace_yaml: None,
    manifest_namespace: "",
    env_prefix: None,
    config_env_prefix: None,
    diag_env_prefix: None,
    cache_namespace: "jailtest",
    data_namespace: "jailtest",
    virtual_store_subdir: "virtual-store",
    managed_config_system_dir: None,
    config_namespace: None,
    canonical_lockfile_always_wins: false,
    runtime_switching: false,
    self_engines_check: false,
    self_update_enabled: false,
    warm_store_verify: false,
    no_churn_lockfile_write: true,
    read_branded_settings_env: false,
    gvs_incompatible_warning: false,
    gvs_over_default_hoist: true,
    primer_ttl: None,
    cpu_budget: None,
    tty_progress: false,
    rich_update_picker: false,
    strict_unsupported_source: false,
    warm_trust_revalidate: false,
    trust_policy_ignore_after_default: None,
    extra_settings_fingerprint: None,
    embedder_owns_lifecycle_sandbox: true,
};

#[test]
fn preinstall_install_and_postinstall_all_reach_the_build_jail() {
    aube_util::set_embedder(&JAILED);
    let recorder = Arc::new(Recorder::default());
    aube_util::update_engine_context(|c| c.lifecycle_sandbox = Some(recorder.clone()));

    let project = tempfile::tempdir().expect("tempdir");
    let package_dir = project.path().join("node_modules/evil");
    let dep_modules_dir = package_dir.join("node_modules");
    std::fs::create_dir_all(&dep_modules_dir).expect("create package dir");
    let manifest = aube_manifest::PackageJson::parse(
        &package_dir.join("package.json"),
        r#"{
            "name": "evil",
            "version": "1.0.0",
            "scripts": {
                "preinstall": "exit 1",
                "install": "exit 1",
                "postinstall": "exit 1"
            }
        }"#
        .to_string(),
    )
    .expect("parse fixture manifest");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    for hook in aube_scripts::DEP_LIFECYCLE_HOOKS {
        let phase = hook.script_name();
        let ran = rt
            .block_on(aube_scripts::run_dep_hook(
                &package_dir,
                project.path(),
                "node_modules",
                &manifest,
                hook,
                &[],
                None,
                Some("evil"),
            ))
            .unwrap_or_else(|e| match e {
                // The fixture scripts are `exit 1`, so a non-zero exit means the real
                // spawn ran — the phase bypassed the confiner. Any other error is a
                // harness failure and must not be reported as a confinement breach.
                aube_scripts::Error::NonZeroExit { .. } => {
                    panic!("`{phase}` escaped the build-jail and spawned unconfined: {e}")
                }
                other => panic!("`{phase}` failed to run at all (test harness fault): {other}"),
            });
        assert!(ran, "`{phase}` is declared in the fixture but did not run");
    }

    assert_eq!(
        recorder.gated.lock().expect("recorder lock").clone(),
        vec![
            Some("evil".to_string()),
            Some("evil".to_string()),
            Some("evil".to_string())
        ],
        "the resolved package identity must reach the opt-out gate on every phase; \
         `None` here means nub can no longer tell which package it is being asked about \
         and every opt-out silently stops working"
    );

    let seen = recorder.phases.lock().expect("recorder lock").clone();
    assert_eq!(
        seen,
        vec!["preinstall", "install", "postinstall"],
        "every dependency lifecycle phase must be handed to the build-jail, in order; \
         a missing entry is a phase spawning unconfined, an extra one is a new phase \
         that needs its confinement reviewed"
    );
}

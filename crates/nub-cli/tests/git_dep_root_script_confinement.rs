//! A git dependency's ROOT lifecycle scripts are confined by nub's build-jail; the
//! user's own root scripts stay exempt.
//!
//! A git dep carrying `prepare` is built by a nested install in which the fetched
//! checkout IS the root package, so it used to inherit the root exemption — third-party
//! code running with the user's full ambient authority across all four phases. The
//! exemption is grounded in AUTHORSHIP, and `RootProvenance` is what carries that fact
//! to the spawn.
//!
//! What can regress here is ROUTING, not policy: enforcement is covered end-to-end by
//! `nub-sandbox/tests/build_jail_enforcement.rs`, and `compile_build_jail` takes no
//! script name, so the policy cannot differ per phase. This pins that every phase of a
//! fetched checkout reaches the confiner AND that a user-authored root still bypasses
//! it — the second half is what stops the fix from over-widening into a regression.

use std::sync::{Arc, Mutex};

/// Stands in for the confiner: records the phase it was handed and reports success
/// WITHOUT spawning. The fetched-checkout fixture's scripts are all `exit 1`, so a phase
/// that bypasses the hook runs for real, exits non-zero, and fails the call — the bypass
/// is caught even if the recorder somehow missed it.
#[derive(Debug, Default)]
struct Recorder {
    phases: Mutex<Vec<String>>,
}

impl Recorder {
    fn seen(&self) -> Vec<String> {
        self.phases.lock().expect("recorder lock").clone()
    }
}

impl aube_util::LifecycleSandbox for Recorder {
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
/// own process, which is the whole isolation.
static JAILED: aube_util::Embedder = aube_util::Embedder {
    name: "gitdeptest",
    display_name: "gitdeptest",
    vendor: None,
    version: "0.0.0",
    user_agent: "gitdeptest/0.0.0",
    self_names: &["gitdeptest"],
    compatible_names: &[],
    lockfile_basename: "gitdeptest-lock.yaml",
    lockfile_legacy_basenames: &[],
    workspace_yaml: None,
    manifest_namespace: "",
    env_prefix: None,
    config_env_prefix: None,
    diag_env_prefix: None,
    cache_namespace: "gitdeptest",
    data_namespace: "gitdeptest",
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

/// Every root lifecycle phase, in the order an install runs them. `prepare` is the one
/// that makes a git dep get prepared at all, and it is a root-only hook — absent from
/// `DEP_LIFECYCLE_HOOKS` — so it is reachable ONLY through this path.
const ROOT_PHASES: [aube_scripts::LifecycleHook; 4] = [
    aube_scripts::LifecycleHook::PreInstall,
    aube_scripts::LifecycleHook::Install,
    aube_scripts::LifecycleHook::PostInstall,
    aube_scripts::LifecycleHook::Prepare,
];

fn manifest_with(dir: &std::path::Path, name: &str, body: &str) -> aube_manifest::PackageJson {
    let scripts: Vec<String> = ROOT_PHASES
        .iter()
        .map(|h| format!("\"{}\": \"{body}\"", h.script_name()))
        .collect();
    aube_manifest::PackageJson::parse(
        &dir.join("package.json"),
        format!(
            r#"{{ "name": "{name}", "version": "1.0.0", "scripts": {{ {} }} }}"#,
            scripts.join(", ")
        ),
    )
    .expect("parse fixture manifest")
}

#[test]
fn a_fetched_checkouts_root_scripts_are_confined_and_a_user_authored_roots_are_not() {
    aube_util::set_embedder(&JAILED);
    let recorder = Arc::new(Recorder::default());
    aube_util::update_engine_context(|c| c.lifecycle_sandbox = Some(recorder.clone()));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    // A git dependency being prepared: every phase must reach the confiner. The scripts
    // are `exit 1` so an unconfined spawn is loud rather than silent.
    let fetched = tempfile::tempdir().expect("tempdir");
    let fetched_manifest = manifest_with(fetched.path(), "fetched-dep", "exit 1");
    for hook in ROOT_PHASES {
        let phase = hook.script_name();
        let ran = rt
            .block_on(aube_scripts::run_root_script_by_name(
                fetched.path(),
                "node_modules",
                &fetched_manifest,
                phase,
                aube_scripts::RootProvenance::Fetched,
            ))
            .unwrap_or_else(|e| match e {
                aube_scripts::Error::NonZeroExit { .. } => panic!(
                    "a fetched git checkout's `{phase}` escaped the build-jail and spawned \
                     unconfined: {e}"
                ),
                other => panic!("`{phase}` failed to run at all (test harness fault): {other}"),
            });
        assert!(ran, "`{phase}` is declared in the fixture but did not run");
    }
    assert_eq!(
        recorder.seen(),
        vec!["preinstall", "install", "postinstall", "prepare"],
        "every root lifecycle phase of a FETCHED checkout must be handed to the \
         build-jail, in order; a missing entry is a phase running with the user's full \
         authority, an extra one is a new phase whose confinement needs review"
    );

    // The user's own project: the trusted-root exemption must survive. These scripts
    // `exit 0`, so the real (unconfined) spawn succeeds — the assertion is that the
    // confiner was NOT consulted. This is also the non-vacuousness control for the
    // block above: the recorder demonstrably distinguishes the two provenances.
    let owned = tempfile::tempdir().expect("tempdir");
    let owned_manifest = manifest_with(owned.path(), "my-project", "exit 0");
    for hook in ROOT_PHASES {
        let phase = hook.script_name();
        let ran = rt
            .block_on(aube_scripts::run_root_script_by_name(
                owned.path(),
                "node_modules",
                &owned_manifest,
                phase,
                aube_scripts::RootProvenance::UserAuthored,
            ))
            .unwrap_or_else(|e| panic!("user-authored `{phase}` should run cleanly: {e}"));
        assert!(ran, "`{phase}` is declared in the fixture but did not run");
    }
    assert_eq!(
        recorder.seen(),
        vec!["preinstall", "install", "postinstall", "prepare"],
        "a user-authored root script must NOT be routed to the dependency build-jail; \
         the recorder gained an entry, so the fix over-widened and is now confining the \
         user's own project scripts"
    );
}

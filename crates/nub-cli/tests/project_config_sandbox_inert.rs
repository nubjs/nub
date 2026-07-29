//! A restrictive sandbox value in the active project config layer must be
//! parsed but INERT during a real offline install lifecycle: everything a
//! lifecycle script can observe matches a no-sandbox baseline. The runtime/dlx
//! consumers get the same treatment via their existing e2e fixtures
//! (project_runtime_config.rs / project_config_dlx.rs, which carry restrictive
//! sandbox values through all their assertions); this file owns the install
//! lifecycle.
//!
//! An inertness comparison proves nothing if the config never loaded, so each
//! test pairs its comparisons with a liveness control — see
//! [`assert_config_reaches_install_path`].

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

/// Every sandbox position a PROJECT file may hold, all fully restrictive. The
/// third position, `dlx.sandbox`, is global-only and belongs to the global
/// sibling below.
const RESTRICTIVE_PROJECT: &str = r#"{
  "sandbox": { "fs": false, "net": false, "env": false },
  "install": { "sandbox": { "fs": false, "net": false, "env": false } }
}"#;

/// [`RESTRICTIVE_PROJECT`] with the liveness control's live install knob added.
const RESTRICTIVE_PROJECT_PROBED: &str = r#"{
  "sandbox": { "fs": false, "net": false, "env": false },
  "install": {
    "linker": "pnp",
    "sandbox": { "fs": false, "net": false, "env": false }
  }
}"#;

const RESTRICTIVE_GLOBAL: &str = r#"{
  "sandbox": { "fs": false, "net": false, "env": false },
  "install": { "sandbox": { "fs": false, "net": false, "env": false } },
  "dlx": { "sandbox": { "fs": false, "net": false, "env": false } }
}"#;

/// [`RESTRICTIVE_GLOBAL`] with the liveness control's live install knob added.
const RESTRICTIVE_GLOBAL_PROBED: &str = r#"{
  "sandbox": { "fs": false, "net": false, "env": false },
  "install": {
    "linker": "pnp",
    "sandbox": { "fs": false, "net": false, "env": false }
  },
  "dlx": { "sandbox": { "fs": false, "net": false, "env": false } }
}"#;

/// One hermetic project + config home, installed offline. `project_config`
/// plants a project `nub.jsonc`; `None` is the no-config baseline. The root
/// postinstall records what a lifecycle script can observe on the axes a sandbox
/// would restrict — an inherited env var, and a write OUTSIDE the project root
/// (the marker itself lives above the project dir, so writing it at all is that
/// observation).
fn offline_lifecycle_install(
    root: &Path,
    project_config: Option<&str>,
    global_config: Option<&str>,
) -> (Output, PathBuf) {
    let project = root.join("project");
    let config_home = root.join("config");
    let marker = root.join("marker.json");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(config_home.join("nub")).unwrap();
    if let Some(body) = project_config {
        std::fs::write(project.join("nub.jsonc"), body).unwrap();
    }
    if let Some(body) = global_config {
        std::fs::write(config_home.join("nub/nub.jsonc"), body).unwrap();
    }
    std::fs::write(
        project.join("package.json"),
        r#"{
          "name": "lifecycle-fixture",
          "version": "1.0.0",
          "scripts": {
            "postinstall": "node -e \"require('fs').writeFileSync(process.env.MARKER_PATH, JSON.stringify({ canary: process.env.SANDBOX_CANARY ?? null, outsideWrite: true }))\""
          }
        }"#,
    )
    .unwrap();

    let output = Command::new(nub_binary())
        .args(["install", "--offline"])
        .current_dir(&project)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("MARKER_PATH", &marker)
        .env("SANDBOX_CANARY", "visible")
        .env_remove("CI")
        .env_remove("NODE_OPTIONS")
        .output()
        .expect("run offline install");
    (output, marker)
}

fn run_offline_lifecycle_install(
    root: &Path,
    project_config: Option<&str>,
    global_config: Option<&str>,
) -> (i32, serde_json::Value) {
    let (output, marker) = offline_lifecycle_install(root, project_config, global_config);
    let code = output.status.code().unwrap_or(-1);
    let text = std::fs::read_to_string(&marker).unwrap_or_else(|_| {
        panic!(
            "postinstall must run and write the marker (exit {code}); stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (code, serde_json::from_str(&text).expect("marker is JSON"))
}

/// The liveness half: re-plant the SAME config body with one live install knob
/// added — `install.linker: "pnp"`, rejected while the install lowers config
/// into engine settings — and require the install to abort on it. A file at this
/// exact path is therefore both parsed and consulted by the install path, which
/// is what stops the "nothing changed" comparisons from being vacuous. Reusing
/// the body is what makes it a control: a body that failed to parse (the silent
/// degrade the best-effort global reader can take) would not abort here either.
///
/// Out of band rather than in-run because nothing left in `nub.jsonc` is
/// observable from inside a lifecycle script's environment — the engine exports
/// no config-derived `npm_config_*`, and `install.nodeOptions`, which used to
/// steer the script's `Error.stackTraceLimit` in-run, is gone.
fn assert_config_reaches_install_path(
    root: &Path,
    project_config: Option<&str>,
    global_config: Option<&str>,
) {
    let (output, _) = offline_lifecycle_install(root, project_config, global_config);
    let reported = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert_ne!(
        output.status.code(),
        Some(0),
        "a live install knob in the config under test must abort the install, \
         or the config never reached the install path and the inertness \
         assertions prove nothing; output: {reported}"
    );
    assert!(
        reported.contains("ERR_NUB_CONFIG_UNSUPPORTED"),
        "the install must abort on the config's `linker: \"pnp\"`, \
         not on something else; output: {reported}"
    );
}

#[test]
fn restrictive_install_sandbox_config_is_inert_for_an_offline_lifecycle_install() {
    let temp = tempfile::tempdir().unwrap();
    let (baseline_code, baseline) =
        run_offline_lifecycle_install(&temp.path().join("baseline"), None, None);
    assert_eq!(
        baseline_code, 0,
        "the baseline offline install must succeed"
    );
    assert_eq!(baseline["canary"], "visible", "baseline env visibility");
    assert_eq!(
        baseline["outsideWrite"], true,
        "baseline outside-root write"
    );

    // If any consumer activated a posture, the env canary or the outside-project
    // write would diverge from that baseline.
    let (code, observed) = run_offline_lifecycle_install(
        &temp.path().join("configured"),
        Some(RESTRICTIVE_PROJECT),
        None,
    );
    assert_eq!(code, baseline_code, "exit code must match the baseline");
    assert_eq!(
        observed["canary"], baseline["canary"],
        "a restrictive env axis must not strip the lifecycle environment"
    );
    assert_eq!(
        observed["outsideWrite"], baseline["outsideWrite"],
        "a restrictive fs axis must not block writes outside the project root"
    );

    assert_config_reaches_install_path(
        &temp.path().join("liveness"),
        Some(RESTRICTIVE_PROJECT_PROBED),
        None,
    );
}

#[test]
fn restrictive_global_sandbox_config_is_inert_for_an_offline_lifecycle_install() {
    let temp = tempfile::tempdir().unwrap();
    let (baseline_code, baseline) =
        run_offline_lifecycle_install(&temp.path().join("baseline"), None, None);
    assert_eq!(
        baseline_code, 0,
        "the baseline offline install must succeed"
    );

    let (code, observed) = run_offline_lifecycle_install(
        &temp.path().join("configured"),
        None,
        Some(RESTRICTIVE_GLOBAL),
    );
    assert_eq!(code, baseline_code, "exit code must match the baseline");
    assert_eq!(observed["canary"], baseline["canary"]);
    assert_eq!(observed["outsideWrite"], baseline["outsideWrite"]);

    assert_config_reaches_install_path(
        &temp.path().join("liveness"),
        None,
        Some(RESTRICTIVE_GLOBAL_PROBED),
    );
}

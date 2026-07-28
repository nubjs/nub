//! A restrictive sandbox value in the active project config layer must be
//! parsed but INERT during a real offline install lifecycle: everything a
//! lifecycle script can observe matches a no-sandbox baseline. The runtime/dlx
//! consumers get the same treatment via their existing e2e fixtures
//! (project_runtime_config.rs / project_config_dlx.rs, which carry restrictive
//! sandbox values through all their assertions); this file owns the install
//! lifecycle.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

/// One hermetic project + config home. `project_config` plants a project
/// `nub.jsonc`; `None` is the no-config baseline. The root postinstall records
/// what a lifecycle script can observe on the axes a sandbox would restrict —
/// an inherited env var and a write OUTSIDE the project root — plus
/// `Error.stackTraceLimit`, which the config's `install.nodeOptions` steers and
/// therefore proves the global layer was actually loaded (the global reader is
/// best-effort, so without this the inertness comparison could be vacuous).
fn run_offline_lifecycle_install(
    root: &Path,
    project_config: Option<&str>,
    global_config: Option<&str>,
) -> (i32, serde_json::Value) {
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
            "postinstall": "node -e \"require('fs').writeFileSync(process.env.MARKER_PATH, JSON.stringify({ canary: process.env.SANDBOX_CANARY ?? null, outsideWrite: true, stack: Error.stackTraceLimit }))\""
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
    let code = output.status.code().unwrap_or(-1);
    let text = std::fs::read_to_string(&marker).unwrap_or_else(|_| {
        panic!(
            "postinstall must run and write the marker (exit {code}); stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (code, serde_json::from_str(&text).expect("marker is JSON"))
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

    // Fully-restrictive values at every sandbox position a project file may
    // hold, alongside the `install.nodeOptions` liveness probe. If any consumer
    // activated a posture, the env canary or the outside-project write would
    // diverge. The third position, `dlx.sandbox`, is global-only and is covered
    // by the global-config sibling below.
    let restrictive = r#"{
      "sandbox": { "fs": false, "net": false, "env": false },
      "install": {
        "nodeOptions": ["--stack-trace-limit=19"],
        "sandbox": { "fs": false, "net": false, "env": false }
      }
    }"#;
    let (code, observed) =
        run_offline_lifecycle_install(&temp.path().join("configured"), Some(restrictive), None);
    assert_eq!(code, baseline_code, "exit code must match the baseline");
    assert_eq!(
        observed["stack"], 19,
        "the install.nodeOptions probe must reach the lifecycle script — \
         otherwise the project config never loaded and this test proves nothing"
    );
    assert_eq!(
        observed["canary"], baseline["canary"],
        "a restrictive env axis must not strip the lifecycle environment"
    );
    assert_eq!(
        observed["outsideWrite"], baseline["outsideWrite"],
        "a restrictive fs axis must not block writes outside the project root"
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

    let restrictive = r#"{
      "sandbox": { "fs": false, "net": false, "env": false },
      "install": {
        "nodeOptions": ["--stack-trace-limit=29"],
        "sandbox": { "fs": false, "net": false, "env": false }
      },
      "dlx": { "sandbox": { "fs": false, "net": false, "env": false } }
    }"#;
    let (code, observed) =
        run_offline_lifecycle_install(&temp.path().join("configured"), None, Some(restrictive));
    assert_eq!(code, baseline_code, "exit code must match the baseline");
    assert_eq!(
        observed["stack"], 29,
        "the global install.nodeOptions probe must reach the lifecycle script"
    );
    assert_eq!(observed["canary"], baseline["canary"]);
    assert_eq!(observed["outsideWrite"], baseline["outsideWrite"]);
}

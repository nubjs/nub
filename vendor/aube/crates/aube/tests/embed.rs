use aube::embed::{AUBE, Host, InstallControl, InstallOptions};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Once};

static TEST_HOST: Host = Host {
    name: "testhost",
    display_name: "Test Host",
    vendor: None,
    version: "1.0.0",
    user_agent: "testhost/1.0.0",
    self_names: &["testhost"],
    compatible_names: &["pnpm"],
    lockfile_basename: "testhost-lock.yaml",
    workspace_yaml: None,
    manifest_namespace: "testhost",
    env_prefix: None,
    config_env_prefix: None,
    cache_namespace: "testhost",
    data_namespace: "testhost",
    canonical_lockfile_always_wins: true,
    runtime_switching: false,
    self_engines_check: false,
    self_update_enabled: false,
    // nub's fork adds embedder-fixed fields upstream's `Host` does not have.
    // Inheriting them from `AUBE` keeps this host on standalone-aube behavior
    // for every field upstream doesn't name, and survives either side adding
    // more.
    ..AUBE
};
static INIT: Once = Once::new();

fn initialize_test_host() {
    INIT.call_once(|| {
        aube::embed::initialize(
            &TEST_HOST,
            vec![("minimumReleaseAge".to_string(), "0".to_string())],
        );
    });
}

fn workspace_fixture() -> (tempfile::TempDir, PathBuf) {
    let workspace = tempfile::tempdir().unwrap();
    let app = workspace.path().join("packages/app");
    let library = workspace.path().join("packages/library");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&library).unwrap();
    std::fs::write(
        workspace.path().join("package.json"),
        r#"{"private":true}
"#,
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("pnpm-workspace.yaml"),
        "packages:\n  - packages/*\n",
    )
    .unwrap();
    std::fs::write(
        app.join("package.json"),
        r#"{"name":"app"}
"#,
    )
    .unwrap();
    std::fs::write(
        library.join("package.json"),
        r#"{"name":"library","version":"1.0.0"}
"#,
    )
    .unwrap();
    (workspace, app)
}

struct CancelOnOutput(Mutex<Option<InstallControl>>);

impl aube::embed::InstallReporter for CancelOnOutput {
    fn report(&self, event: aube::embed::InstallEvent) {
        if matches!(event, aube::embed::InstallEvent::Output { .. })
            && let Some(control) = self.0.lock().unwrap().take()
        {
            control.cancel();
        }
    }
}

#[tokio::test]
async fn facade_initializes_host_and_runs_install() {
    initialize_test_host();
    assert_eq!(aube::embed::host().name, "testhost");

    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("package.json"), "{}\n").unwrap();

    let mut options = InstallOptions::new(project.path());
    options.ignore_scripts = true;
    options.network_mode = aube::embed::NetworkMode::Offline;
    options.control = InstallControl::silent();
    aube::embed::install(options).await.unwrap();

    assert!(project.path().join("testhost-lock.yaml").is_file());
}

#[tokio::test]
async fn facade_adds_local_package_to_workspace_member() {
    initialize_test_host();
    let (workspace, app) = workspace_fixture();

    aube::embed::add(
        &app,
        &["library@workspace:*".to_string()],
        aube::embed::AddToProjectOptions {
            save_dev: true,
            ignore_scripts: true,
            offline: true,
            control: InstallControl::silent(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let manifest = std::fs::read_to_string(app.join("package.json")).unwrap();
    assert!(manifest.contains(r#""devDependencies""#));
    assert!(manifest.contains(r#""library": "workspace:*""#));
    assert!(workspace.path().join("testhost-lock.yaml").is_file());
    assert!(!app.join("testhost-lock.yaml").exists());
}

#[tokio::test]
async fn cancelled_manifest_mutation_is_rolled_back() {
    initialize_test_host();
    let (workspace, app) = workspace_fixture();
    let original_manifest = std::fs::read(app.join("package.json")).unwrap();
    let reporter = Arc::new(CancelOnOutput(Mutex::new(None)));
    let control = InstallControl::events(reporter.clone());
    *reporter.0.lock().unwrap() = Some(control.clone());

    let error = aube::embed::add(
        &app,
        &["library@workspace:*".to_string()],
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
        Some(aube_codes::errors::ERR_AUBE_INSTALL_CANCELLED)
    );
    assert_eq!(
        std::fs::read(app.join("package.json")).unwrap(),
        original_manifest
    );
    assert!(!workspace.path().join("testhost-lock.yaml").exists());
}

#[test]
fn error_code_reads_structured_diagnostic_code() {
    let error = miette::miette!(code = "ERR_AUBE_TEST", "test failure");
    assert_eq!(
        aube::embed::error_code(&error).as_deref(),
        Some("ERR_AUBE_TEST")
    );
}

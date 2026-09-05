use aube::embed::{AUBE, Host, InstallControl, InstallOptions, NetworkMode};

static TEST_HOST: Host = Host {
    name: "embed-diag-test",
    display_name: "Embed Diagnostics Test",
    vendor: None,
    version: "1.0.0",
    user_agent: "embed-diag-test/1.0.0",
    self_names: &[],
    compatible_names: &["pnpm"],
    lockfile_basename: "aube-lock.yaml",
    workspace_yaml: None,
    manifest_namespace: "",
    env_prefix: None,
    config_env_prefix: None,
    cache_namespace: "embed-diag-test",
    data_namespace: "embed-diag-test",
    canonical_lockfile_always_wins: false,
    runtime_switching: false,
    self_engines_check: false,
    self_update_enabled: false,
    // nub's fork adds embedder-fixed fields upstream's `Host` does not have;
    // inherit them from `AUBE` so this host stays on standalone-aube behavior.
    ..AUBE
};

#[test]
fn embedded_install_honors_env_driven_diagnostics() {
    aube::embed::initialize(&TEST_HOST, Vec::new());
    assert!(aube::embed::host().env_prefix.is_none());
    let sandbox = tempfile::tempdir().unwrap();
    let project = sandbox.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("package.json"), "{}\n").unwrap();
    let trace = sandbox.path().join("embed-diag.jsonl");

    // Safety: this integration-test binary contains one test and has not
    // created the Tokio runtime or any other threads yet.
    unsafe {
        std::env::set_var("AUBE_DIAG_FILE", &trace);
        std::env::set_var("AUBE_DIAG_FLUSH", "1");
        std::env::set_var("AUBE_DIAG_KERNEL", "1");
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut options = InstallOptions::new(&project);
        options.ignore_scripts = true;
        options.network_mode = NetworkMode::Offline;
        options.control = InstallControl::silent();
        aube::embed::install_with_overrides(
            options,
            aube::embed::EmbedderInstallOverrides {
                use_global_virtual_store: Some(false),
                cache_dir: Some(sandbox.path().join("cache")),
                store_dir: Some(sandbox.path().join("store")),
            },
        )
        .await
        .unwrap();
    });

    let bytes_after_install = std::fs::metadata(&trace).unwrap().len();
    runtime.block_on(async {
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    });
    assert_eq!(
        std::fs::metadata(&trace).unwrap().len(),
        bytes_after_install,
        "the operation-scoped sampler must stop when the embedded install returns"
    );

    let contents = std::fs::read_to_string(&trace).unwrap();
    assert!(contents.contains(r#""cat":"install","name":"begin""#));
    #[cfg(target_os = "linux")]
    assert!(contents.contains(r#""rss_current":"#));
}

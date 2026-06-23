//! Integration test for the embedder identity's user-agent on the
//! registry side.
//!
//! Lives in its own integration-test binary — i.e. its own process —
//! because the active identity is once-per-process and the assembled
//! header is cached in a `OnceLock`: registering an identity here must
//! not leak into the unit tests that exercise the default `aube/<version>`
//! identity.

use aube_registry::client::RegistryClient;
use aube_registry::config::NpmConfig;
use aube_util::Embedder;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

static MYTOOL: Embedder = Embedder {
    name: "mytool",
    display_name: "mytool",
    vendor: None,
    version: "2.1.0",
    user_agent: "mytool/2.1.0",
    self_names: &["mytool"],
    compatible_names: &["pnpm"],
    lockfile_basename: "mytool-lock.yaml",
    workspace_yaml: Some("mytool-workspace.yaml"),
    manifest_namespace: "mytool",
    env_prefix: Some("MYTOOL"),
    config_env_prefix: Some("MYTOOL"),
    cache_namespace: "mytool",
    data_namespace: "mytool",
    canonical_lockfile_always_wins: true,
    runtime_switching: true,
    self_engines_check: true,
    self_update_enabled: true,
    warm_store_verify: true,
    no_churn_lockfile_write: false,
    read_branded_settings_env: true,
    primer_ttl: None,
};

#[tokio::test]
async fn registered_identity_leads_the_registry_user_agent_header() {
    aube_util::set_embedder(&MYTOOL);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "demo",
            "versions": {},
            "dist-tags": {},
        })))
        .mount(&server)
        .await;

    let client = RegistryClient::from_config(NpmConfig {
        registry: format!("{}/", server.uri()),
        ..Default::default()
    });
    client
        .fetch_packument_json_fresh("demo")
        .await
        .expect("mock packument fetch should succeed");

    let requests = server
        .received_requests()
        .await
        .expect("request recording is enabled by default");
    let ua = requests[0]
        .headers
        .get("user-agent")
        .expect("registry requests must carry a User-Agent header")
        .to_str()
        .expect("UA header should be valid UTF-8");
    assert!(
        ua.starts_with("mytool/2.1.0 ("),
        "identity user-agent must lead the registry UA, got: {ua}"
    );
    assert!(
        !ua.contains("aube/"),
        "default user-agent must be fully replaced, got: {ua}"
    );
}

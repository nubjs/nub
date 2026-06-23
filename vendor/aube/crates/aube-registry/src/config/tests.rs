use super::*;
use crate::config::types::NpmrcSource;
use base64::Engine as _;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Serializes the auth.ini tests that touch the process-global
/// `engine_context().read_branded_pnpm_config` gate, so the toggle test's
/// disabled-window can't race a concurrent auth.ini read that assumes the
/// upstream default (enabled). Restored to `true` by the toggle test
/// inside the same critical section. Poison is ignored — these tests don't
/// share mutable state beyond the gate.
static AUTH_INI_GATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct ScopedEnvVars(&'static [&'static str]);

impl Drop for ScopedEnvVars {
    fn drop(&mut self) {
        for name in self.0 {
            unsafe { std::env::remove_var(name) };
        }
    }
}

#[test]
fn parse_npmrc_strips_utf8_bom() {
    let dir = tempfile::tempdir().unwrap();
    let rc = dir.path().join(".npmrc");
    std::fs::write(&rc, "\u{feff}registry=https://r.example.com\n").unwrap();
    let entries = parse_npmrc(&rc).unwrap();
    assert_eq!(
        entries,
        vec![("registry".to_string(), "https://r.example.com".to_string())]
    );
}

#[test]
fn scoped_registry_lookup_is_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "@MyOrg:registry=https://myorg.example.com/\n",
    )
    .unwrap();
    let cfg = NpmConfig::load_isolated(dir.path());
    assert_eq!(cfg.registry_for("@myorg/pkg"), "https://myorg.example.com/");
}

#[test]
fn yarnrc_translates_registry_scope_auth_and_linker_subset() {
    let entries = translate_yarnrc_content(
        r#"
npmRegistryServer: "https://registry.yarn.example"
npmAuthToken: top-token
npmRegistries:
  "https://registry.yarn.example":
    npmAuthIdent: "user:pass"
npmScopes:
  myorg:
    npmRegistryServer: "https://npm.myorg.example"
    npmAuthToken: scope-token
nodeLinker: node-modules
"#,
    );

    assert!(entries.contains(&(
        "registry".to_string(),
        "https://registry.yarn.example/".to_string()
    )));
    assert!(entries.contains(&(
        "//registry.yarn.example/:_authToken".to_string(),
        "top-token".to_string()
    )));
    assert!(entries.contains(&(
        "//registry.yarn.example/:_auth".to_string(),
        base64::engine::general_purpose::STANDARD.encode("user:pass")
    )));
    assert!(entries.contains(&(
        "@myorg:registry".to_string(),
        "https://npm.myorg.example/".to_string()
    )));
    assert!(entries.contains(&(
        "//npm.myorg.example/:_authToken".to_string(),
        "scope-token".to_string()
    )));
    assert!(entries.contains(&("nodeLinker".to_string(), "hoisted".to_string())));
}

#[test]
fn yarnrc_does_not_widen_non_representable_scope_auth() {
    let entries = translate_yarnrc_content(
        r#"
npmRegistryServer: "https://registry.yarn.example"
npmScopes:
  noCustomRegistry:
    npmAuthToken: no-custom-token
  first:
    npmRegistryServer: "https://shared.example"
    npmAuthToken: first-token
  second:
    npmRegistryServer: "https://shared.example"
    npmAuthToken: second-token
  third:
    npmRegistryServer: "https://third-shared.example"
  fourth:
    npmRegistryServer: "https://third-shared.example"
    npmAuthToken: fourth-token
  registryConfigured:
    npmRegistryServer: "https://registry-configured.example"
    npmAuthToken: registry-configured-token
npmRegistries:
  "https://registry-configured.example":
    npmAuthToken: registry-token
"#,
    );

    assert!(entries.contains(&(
        "@noCustomRegistry:registry".to_string(),
        "https://registry.yarn.example/".to_string()
    )));
    assert!(entries.contains(&(
        "@first:registry".to_string(),
        "https://shared.example/".to_string()
    )));
    assert!(entries.contains(&(
        "@second:registry".to_string(),
        "https://shared.example/".to_string()
    )));
    assert!(entries.contains(&(
        "@registryConfigured:registry".to_string(),
        "https://registry-configured.example/".to_string()
    )));

    assert!(
        !entries.iter().any(|(_, v)| v == "no-custom-token"
            || v == "first-token"
            || v == "second-token"
            || v == "fourth-token"
            || v == "registry-configured-token"),
        "scope auth must not be converted to registry-wide auth unless the scope owns a unique custom registry"
    );
    assert!(entries.contains(&(
        "//registry-configured.example/:_authToken".to_string(),
        "registry-token".to_string()
    )));
}

#[test]
fn yarnrc_maps_pnpm_linker_to_existing_isolated_linker_and_leaves_pnp_untranslated() {
    let entries = translate_yarnrc_content("nodeLinker: pnpm\n");
    assert_eq!(
        entries,
        vec![("nodeLinker".to_string(), "isolated".to_string())]
    );

    let entries = translate_yarnrc_content("nodeLinker: pnp\n");
    assert!(
        entries.iter().all(|(k, _)| k != "nodeLinker"),
        "PnP install generation is out of scope; the translator must leave it to nub's warning/refusal path"
    );
}

#[test]
fn yarnrc_translates_package_extensions_to_a_json_object_entry() {
    let entries = translate_yarnrc_content(
        r#"
packageExtensions:
  "react-redux@1":
    peerDependencies:
      react-dom: "*"
  "is-even@*":
    dependencies:
      is-odd: "^1.0.0"
    peerDependenciesMeta:
      is-odd:
        optional: true
"#,
    );

    let (_, raw) = entries
        .iter()
        .find(|(k, _)| k == "packageExtensions")
        .expect("packageExtensions must be emitted as a settings entry");
    let parsed: serde_json::Value =
        serde_json::from_str(raw).expect("emitted packageExtensions must be a JSON object string");

    // The selector key is carried through verbatim (Yarn requires `name@range`),
    // and the dependencies/peerDependencies/peerDependenciesMeta shapes mirror
    // the resolver's PackageExtension model 1:1.
    assert_eq!(
        parsed["react-redux@1"]["peerDependencies"]["react-dom"],
        serde_json::json!("*")
    );
    assert_eq!(
        parsed["is-even@*"]["dependencies"]["is-odd"],
        serde_json::json!("^1.0.0")
    );
    assert_eq!(
        parsed["is-even@*"]["peerDependenciesMeta"]["is-odd"]["optional"],
        serde_json::json!(true)
    );
}

#[test]
fn yarnrc_without_package_extensions_emits_no_entry() {
    let entries = translate_yarnrc_content("nodeLinker: node-modules\n");
    assert!(
        entries.iter().all(|(k, _)| k != "packageExtensions"),
        "absent packageExtensions must not synthesize an empty settings entry"
    );
}

#[test]
fn yarnrc_translates_top_level_ca_proxy_and_strict_ssl() {
    let entries = translate_yarnrc_content(
        r#"
httpsCaFilePath: /etc/ssl/corp-bundle.pem
httpProxy: "http://proxy.example:3128"
httpsProxy: "http://proxy.example:3129"
enableStrictSsl: false
"#,
    );
    assert!(entries.contains(&("cafile".to_string(), "/etc/ssl/corp-bundle.pem".to_string())));
    assert!(entries.contains(&(
        "http-proxy".to_string(),
        "http://proxy.example:3128".to_string()
    )));
    assert!(entries.contains(&(
        "https-proxy".to_string(),
        "http://proxy.example:3129".to_string()
    )));
    assert!(entries.contains(&("strict-ssl".to_string(), "false".to_string())));
}

#[test]
fn yarnrc_translates_per_host_ca_and_skips_glob_hosts() {
    let entries = translate_yarnrc_content(
        r#"
networkSettings:
  "registry.example.com":
    httpsCaFilePath: /etc/ssl/host-ca.pem
  "*.cdn.example.com":
    httpsCaFilePath: /etc/ssl/glob-ca.pem
"#,
    );
    // Literal host → the per-registry `//host/:cafile` form the .npmrc
    // consumer already understands.
    assert!(entries.contains(&(
        "//registry.example.com/:cafile".to_string(),
        "/etc/ssl/host-ca.pem".to_string()
    )));
    // A glob host pattern has no exact-prefix equivalent and must be dropped
    // rather than mis-scoped to a literal `//*.cdn.example.com/` key.
    assert!(
        entries.iter().all(|(_, v)| v != "/etc/ssl/glob-ca.pem"),
        "glob networkSettings host must not be translated to a per-registry cafile"
    );
}

#[test]
fn yarnrc_translates_top_level_mtls_cert_and_key_paths() {
    // Yarn Berry expresses the client cert/key as file PATHS; the cert/key
    // consumer takes inline PEM, so the reader loads both files and emits
    // the inline `cert` / `key` npmrc keys.
    let dir = tempfile::tempdir().unwrap();
    let cert_path = dir.path().join("client.crt");
    let key_path = dir.path().join("client.key");
    std::fs::write(&cert_path, "-----BEGIN CERTIFICATE-----\nCERTBODY\n").unwrap();
    std::fs::write(&key_path, "-----BEGIN PRIVATE KEY-----\nKEYBODY\n").unwrap();
    let entries = translate_yarnrc_content(&format!(
        "httpsCertFilePath: {}\nhttpsKeyFilePath: {}\n",
        cert_path.display(),
        key_path.display()
    ));
    assert!(
        entries
            .iter()
            .any(|(k, v)| k == "cert" && v.contains("CERTBODY"))
    );
    assert!(
        entries
            .iter()
            .any(|(k, v)| k == "key" && v.contains("KEYBODY"))
    );
}

#[test]
fn yarnrc_skips_mtls_when_cert_or_key_path_missing() {
    // A half-identity (cert without key, or an unreadable path) is never a
    // usable mTLS pair, so neither inline key is emitted.
    let dir = tempfile::tempdir().unwrap();
    let cert_path = dir.path().join("client.crt");
    std::fs::write(&cert_path, "-----BEGIN CERTIFICATE-----\nCERTBODY\n").unwrap();
    let entries = translate_yarnrc_content(&format!(
        "httpsCertFilePath: {}\nhttpsKeyFilePath: /nonexistent/client.key\n",
        cert_path.display(),
    ));
    assert!(entries.iter().all(|(k, _)| k != "cert" && k != "key"));
}

#[test]
fn yarnrc_translates_per_host_mtls_cert_and_key_paths() {
    let dir = tempfile::tempdir().unwrap();
    let cert_path = dir.path().join("host.crt");
    let key_path = dir.path().join("host.key");
    std::fs::write(&cert_path, "-----BEGIN CERTIFICATE-----\nHOSTCERT\n").unwrap();
    std::fs::write(&key_path, "-----BEGIN PRIVATE KEY-----\nHOSTKEY\n").unwrap();
    let entries = translate_yarnrc_content(&format!(
        r#"
networkSettings:
  "registry.example.com":
    httpsCertFilePath: {}
    httpsKeyFilePath: {}
"#,
        cert_path.display(),
        key_path.display()
    ));
    assert!(
        entries
            .iter()
            .any(|(k, v)| k == "//registry.example.com/:cert" && v.contains("HOSTCERT"))
    );
    assert!(
        entries
            .iter()
            .any(|(k, v)| k == "//registry.example.com/:key" && v.contains("HOSTKEY"))
    );
}

#[test]
fn yarnrc_translates_supported_architectures_to_object_setting() {
    // Yarn `supportedArchitectures` mirrors pnpm's shape and rides the same
    // object-setting channel as packageExtensions: a JSON object string
    // under the `supportedArchitectures` key.
    let entries = translate_yarnrc_content(
        r#"
supportedArchitectures:
  os:
    - current
    - linux
  cpu:
    - arm64
  libc:
    - glibc
"#,
    );
    let (_, json) = entries
        .iter()
        .find(|(k, _)| k == "supportedArchitectures")
        .expect("supportedArchitectures entry must be emitted");
    let value: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(value["os"], serde_json::json!(["current", "linux"]));
    assert_eq!(value["cpu"], serde_json::json!(["arm64"]));
    assert_eq!(value["libc"], serde_json::json!(["glibc"]));
}

#[test]
fn yarnrc_translates_npm_always_auth_top_level_and_per_registry() {
    // Top-level `npmAlwaysAuth` scopes to the default registry; a
    // per-registry `npmRegistries.<url>.npmAlwaysAuth` scopes to that host.
    let entries = translate_yarnrc_content(
        r#"
npmRegistryServer: "https://registry.example.com/"
npmAlwaysAuth: true
npmRegistries:
  "//npm.other.com/":
    npmAuthToken: secret-token
    npmAlwaysAuth: true
"#,
    );
    assert!(
        entries
            .iter()
            .any(|(k, v)| k == "//registry.example.com/:always-auth" && v == "true"),
        "default-registry always-auth: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .any(|(k, v)| k == "//npm.other.com/:always-auth" && v == "true"),
        "per-registry always-auth: {entries:?}"
    );
}

#[test]
fn yarn_env_translates_top_level_ca_proxy_and_strict_ssl() {
    let entries = yarn_env_entries_from(&[
        (
            "YARN_HTTPS_CA_FILE_PATH".to_string(),
            "/etc/ssl/env-ca.pem".to_string(),
        ),
        ("YARN_HTTP_PROXY".to_string(), "http://e:1".to_string()),
        ("YARN_HTTPS_PROXY".to_string(), "http://e:2".to_string()),
        ("YARN_ENABLE_STRICT_SSL".to_string(), "false".to_string()),
    ]);
    assert!(entries.contains(&("cafile".to_string(), "/etc/ssl/env-ca.pem".to_string())));
    assert!(entries.contains(&("http-proxy".to_string(), "http://e:1".to_string())));
    assert!(entries.contains(&("https-proxy".to_string(), "http://e:2".to_string())));
    assert!(entries.contains(&("strict-ssl".to_string(), "false".to_string())));
}

#[test]
fn user_yarnrc_proxy_is_honored_but_project_yarnrc_proxy_is_rejected() {
    // Proxy/strict-ssl are subprocess-affecting settings: a user-level
    // `~/.yarnrc.yml` is trusted, a project-committed `.yarnrc.yml` is not —
    // identical to the `.npmrc` trust gate. The yarnrc entries inherit it for
    // free by flowing through `apply_tagged`.
    let _gate = AUTH_INI_GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".yarnrc.yml"),
        "httpsProxy: http://user-proxy.example\n",
    )
    .unwrap();
    std::fs::write(
        project.path().join(".yarnrc.yml"),
        "httpProxy: http://project-proxy.example\n",
    )
    .unwrap();

    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = true);
    let tagged = crate::config::load::merge_yarnrc_tagged_entries(
        load_npmrc_entries_tagged_with_home(Some(home.path()), None, project.path(), None),
        Some(home.path()),
        project.path(),
    );
    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = false);
    let mut config = NpmConfig::default();
    config.apply_tagged(tagged);

    assert_eq!(
        config.https_proxy.as_deref(),
        Some("http://user-proxy.example"),
        "user ~/.yarnrc.yml proxy must be honored"
    );
    assert!(
        config.http_proxy.as_deref() != Some("http://project-proxy.example"),
        "project .yarnrc.yml must not set a registry proxy (untrusted source)"
    );
}

#[test]
fn yarnrc_project_overrides_global_yarnrc_entries() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".yarnrc.yml"),
        "npmRegistryServer: https://global.yarn.example\nnodeLinker: pnpm\n",
    )
    .unwrap();
    std::fs::write(
        project.path().join(".yarnrc.yml"),
        "npmRegistryServer: https://project.yarn.example\nnodeLinker: node-modules\n",
    )
    .unwrap();

    let split = load_yarnrc_entries_split_with_home(Some(home.path()), project.path());
    let mut merged = split.user;
    merged.extend(split.project);
    let mut cfg = NpmConfig::default();
    cfg.apply(merged);

    assert_eq!(cfg.registry, "https://project.yarn.example/");
}

#[test]
fn yarnrc_walks_ancestor_rc_files_with_nearest_file_winning() {
    let root = tempfile::tempdir().unwrap();
    let child = root.path().join("packages/app");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::write(
        root.path().join(".yarnrc.yml"),
        "npmRegistryServer: https://root.yarn.example\n",
    )
    .unwrap();
    std::fs::write(
        child.join(".yarnrc.yml"),
        "npmRegistryServer: https://child.yarn.example\n",
    )
    .unwrap();

    let split = load_yarnrc_entries_split_with_home(None, &child);
    let mut cfg = NpmConfig::default();
    cfg.apply(split.project);

    assert_eq!(cfg.registry, "https://child.yarn.example/");
}

#[test]
fn yarnrc_merges_package_extensions_across_ancestor_rc_files_child_wins() {
    // Map-typed `packageExtensions` is shallow-merged across every rc file in
    // the ancestor walk, not last-file-wins (which is correct only for scalar
    // settings like registry/linker). Selectors unique to the root file must
    // survive alongside the child's, and a selector set in BOTH must resolve to
    // the child (nearest) file's value.
    let root = tempfile::tempdir().unwrap();
    let child = root.path().join("packages/app");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::write(
        root.path().join(".yarnrc.yml"),
        r#"
packageExtensions:
  "root-only@*":
    dependencies:
      left-pad: "^1.0.0"
  "shared@*":
    dependencies:
      from-root: "^1.0.0"
"#,
    )
    .unwrap();
    std::fs::write(
        child.join(".yarnrc.yml"),
        r#"
packageExtensions:
  "child-only@*":
    dependencies:
      right-pad: "^2.0.0"
  "shared@*":
    dependencies:
      from-child: "^2.0.0"
"#,
    )
    .unwrap();

    let split = load_yarnrc_entries_split_with_home(None, &child);

    // A single merged entry — not one per file.
    let pkg_ext_entries: Vec<_> = split
        .project
        .iter()
        .filter(|(k, _)| k == "packageExtensions")
        .collect();
    assert_eq!(
        pkg_ext_entries.len(),
        1,
        "ancestor packageExtensions must collapse into a single merged entry"
    );

    let parsed: serde_json::Value = serde_json::from_str(&pkg_ext_entries[0].1).unwrap();

    // Both files' unique selectors survive the merge.
    assert_eq!(
        parsed["root-only@*"]["dependencies"]["left-pad"],
        serde_json::json!("^1.0.0")
    );
    assert_eq!(
        parsed["child-only@*"]["dependencies"]["right-pad"],
        serde_json::json!("^2.0.0")
    );
    // The overlapping selector resolves to the nearest (child) file's value.
    assert_eq!(
        parsed["shared@*"]["dependencies"],
        serde_json::json!({ "from-child": "^2.0.0" }),
        "child file must win on a duplicate selector key"
    );
}

#[test]
fn yarnrc_load_is_incumbent_gated_for_registry_config() {
    let _gate = AUTH_INI_GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".yarnrc.yml"),
        r#"
npmRegistryServer: https://yarn-only.example
npmScopes:
  myorg:
    npmRegistryServer: https://npm.myorg.example
    npmAuthToken: scope-token
"#,
    )
    .unwrap();

    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = false);
    let disabled = NpmConfig::load_with_env(project.path(), &[]);
    assert_eq!(disabled.registry, "https://registry.npmjs.org/");
    assert_eq!(
        disabled.registry_for("@myorg/pkg"),
        "https://registry.npmjs.org/"
    );
    assert_eq!(disabled.auth_token_for("https://npm.myorg.example/"), None);

    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = true);
    let enabled = NpmConfig::load_with_env(project.path(), &[]);
    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = false);

    assert_eq!(enabled.registry, "https://yarn-only.example/");
    assert_eq!(
        enabled.registry_for("@myorg/pkg"),
        "https://npm.myorg.example/"
    );
    assert_eq!(
        enabled.auth_token_for("https://npm.myorg.example/"),
        Some("scope-token")
    );
}

#[test]
fn yarnrc_node_linker_reaches_split_settings_sources_only_when_gated_on() {
    let _gate = AUTH_INI_GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let disabled_project = tempfile::tempdir().unwrap();
    std::fs::write(
        disabled_project.path().join(".yarnrc.yml"),
        "nodeLinker: node-modules\n",
    )
    .unwrap();
    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = false);
    let disabled = load_npmrc_entries_split(disabled_project.path());
    assert!(disabled.project.iter().all(|(key, _)| key != "nodeLinker"));

    let enabled_project = tempfile::tempdir().unwrap();
    std::fs::write(
        enabled_project.path().join(".yarnrc.yml"),
        "nodeLinker: node-modules\n",
    )
    .unwrap();
    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = true);
    let enabled = load_npmrc_entries_split(enabled_project.path());
    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = false);
    assert!(
        enabled
            .project
            .contains(&("nodeLinker".to_string(), "hoisted".to_string()))
    );
}

#[test]
fn classic_yarnrc_translates_core_registry_scope_and_auth_fields() {
    let entries = translate_classic_yarnrc_content(
        r#"
# a comment line
registry "https://registry.yarnpkg.com"
"@myscope:registry" "https://npm.pkg.github.com"
"//npm.pkg.github.com/:_authToken" "GH_TOKEN"
_authToken NPM_TOKEN
network-timeout 60000
--install.production true
"#,
    );

    assert!(entries.contains(&(
        "registry".to_string(),
        "https://registry.yarnpkg.com/".to_string()
    )));
    assert!(entries.contains(&(
        "@myscope:registry".to_string(),
        "https://npm.pkg.github.com/".to_string()
    )));
    assert!(entries.contains(&(
        "//npm.pkg.github.com/:_authToken".to_string(),
        "GH_TOKEN".to_string()
    )));
    assert!(entries.contains(&("_authToken".to_string(), "NPM_TOKEN".to_string())));
    // Out-of-scope keys and `--flag` arg lines are ignored.
    assert!(entries.iter().all(|(key, _)| key != "network-timeout"));
    assert!(entries.iter().all(|(key, _)| !key.starts_with("--")));
}

#[test]
fn classic_yarnrc_load_is_incumbent_and_classic_gated() {
    let _gate = AUTH_INI_GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".yarnrc"),
        "registry \"https://classic-yarn.example\"\n",
    )
    .unwrap();

    // No Yarn incumbent → not read.
    aube_util::update_engine_context(|ctx| {
        ctx.read_yarn_config = false;
        ctx.yarn_is_classic = false;
    });
    let disabled = NpmConfig::load_with_env(project.path(), &[]);
    assert_eq!(disabled.registry, "https://registry.npmjs.org/");

    // Yarn incumbent but BERRY (classic gate off) → `.yarnrc` is NOT read. A
    // Berry project's stray legacy `.yarnrc` must not leak into config, since
    // Berry itself ignores it. This is the gating-bug regression guard.
    aube_util::update_engine_context(|ctx| {
        ctx.read_yarn_config = true;
        ctx.yarn_is_classic = false;
    });
    let berry = NpmConfig::load_with_env(project.path(), &[]);
    assert_eq!(berry.registry, "https://registry.npmjs.org/");

    // Classic (v1) Yarn incumbent → `.yarnrc` IS read.
    aube_util::update_engine_context(|ctx| {
        ctx.read_yarn_config = true;
        ctx.yarn_is_classic = true;
    });
    let enabled = NpmConfig::load_with_env(project.path(), &[]);
    aube_util::update_engine_context(|ctx| {
        ctx.read_yarn_config = false;
        ctx.yarn_is_classic = false;
    });
    assert_eq!(enabled.registry, "https://classic-yarn.example/");
}

#[test]
fn classic_yarnrc_nearest_file_wins_along_ancestor_walk() {
    let _gate = AUTH_INI_GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join(".yarnrc"),
        "registry \"https://root.example\"\n",
    )
    .unwrap();
    let child = root.path().join("packages").join("app");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::write(
        child.join(".yarnrc"),
        "registry \"https://child.example\"\n",
    )
    .unwrap();

    aube_util::update_engine_context(|ctx| {
        ctx.read_yarn_config = true;
        ctx.yarn_is_classic = true;
    });
    let cfg = NpmConfig::load_with_env(&child, &[]);
    aube_util::update_engine_context(|ctx| {
        ctx.read_yarn_config = false;
        ctx.yarn_is_classic = false;
    });
    assert_eq!(cfg.registry, "https://child.example/");
}

#[test]
fn npm_config_env_still_outranks_yarnrc_file_config() {
    let _gate = AUTH_INI_GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".yarnrc.yml"),
        "npmRegistryServer: https://from-yarnrc.example\n",
    )
    .unwrap();

    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = true);
    let cfg = NpmConfig::load_with_env(
        project.path(),
        &[(
            "NPM_CONFIG_REGISTRY".to_string(),
            "https://from-env.example".to_string(),
        )],
    );
    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = false);

    assert_eq!(cfg.registry, "https://from-env.example/");
}

#[test]
fn yarn_env_subset_is_translated_above_yarnrc_files() {
    let _gate = AUTH_INI_GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".yarnrc.yml"),
        "npmRegistryServer: https://from-yarnrc.example\nnodeLinker: node-modules\n",
    )
    .unwrap();

    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = true);
    let cfg = NpmConfig::load_with_env(
        project.path(),
        &[(
            "YARN_NPM_REGISTRY_SERVER".to_string(),
            "https://from-yarn-env.example".to_string(),
        )],
    );
    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = false);

    assert_eq!(cfg.registry, "https://from-yarn-env.example/");
    assert!(
        yarn_env_entries_from(&[("YARN_NODE_LINKER".to_string(), "pnpm".to_string())])
            .contains(&("nodeLinker".to_string(), "isolated".to_string()))
    );
}

#[test]
fn mixed_yarnrc_npmrc_order_keeps_project_sources_above_user_global_sources() {
    let _gate = AUTH_INI_GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".yarnrc.yml"),
        "npmRegistryServer: https://global-yarn.example\n",
    )
    .unwrap();
    std::fs::write(
        project.path().join(".npmrc"),
        "registry=https://project-npmrc.example\n",
    )
    .unwrap();

    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = true);
    let mut cfg = NpmConfig::default();
    cfg.apply(load_npmrc_entries_with_home(
        Some(home.path()),
        None,
        project.path(),
        None,
    ));
    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = false);

    assert_eq!(cfg.registry, "https://project-npmrc.example/");
}

#[test]
fn home_yarnrc_is_not_promoted_to_project_precedence_under_home() {
    let _gate = AUTH_INI_GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    let project = home.path().join("work/project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        home.path().join(".yarnrc.yml"),
        "npmRegistryServer: https://home-yarn.example\n",
    )
    .unwrap();
    std::fs::write(
        project.join(".npmrc"),
        "registry=https://project-npmrc.example\n",
    )
    .unwrap();

    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = true);
    let mut cfg = NpmConfig::default();
    cfg.apply(load_npmrc_entries_with_home(
        Some(home.path()),
        None,
        &project,
        None,
    ));
    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = false);

    assert_eq!(cfg.registry, "https://project-npmrc.example/");
}

#[test]
fn project_yarnrc_outranks_project_npmrc_for_supported_yarn_incumbent_subset() {
    let _gate = AUTH_INI_GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".npmrc"),
        "registry=https://project-npmrc.example\n",
    )
    .unwrap();
    std::fs::write(
        project.path().join(".yarnrc.yml"),
        "npmRegistryServer: https://project-yarnrc.example\n",
    )
    .unwrap();

    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = true);
    let loaded = NpmConfig::load_with_env(project.path(), &[]);
    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = false);

    assert_eq!(loaded.registry, "https://project-yarnrc.example/");
}

#[test]
fn split_loader_cache_is_sensitive_to_yarn_gate() {
    let _gate = AUTH_INI_GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".yarnrc.yml"),
        "nodeLinker: node-modules\n",
    )
    .unwrap();

    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = false);
    let disabled = load_npmrc_entries_split(project.path());
    assert!(disabled.project.iter().all(|(key, _)| key != "nodeLinker"));

    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = true);
    let enabled = load_npmrc_entries_split(project.path());
    aube_util::update_engine_context(|ctx| ctx.read_yarn_config = false);

    assert!(
        enabled
            .project
            .contains(&("nodeLinker".to_string(), "hoisted".to_string())),
        "same project_dir must not reuse a gate-disabled cached split"
    );
}

#[test]
fn test_parse_npmrc_basic() {
    let dir = tempfile::tempdir().unwrap();
    let rc = dir.path().join(".npmrc");
    std::fs::write(
        &rc,
        "registry=https://registry.example.com\n_authToken=secret123\n",
    )
    .unwrap();

    let entries = parse_npmrc(&rc).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0],
        (
            "registry".to_string(),
            "https://registry.example.com".to_string()
        )
    );
    assert_eq!(
        entries[1],
        ("_authToken".to_string(), "secret123".to_string())
    );
}

#[test]
fn test_parse_npmrc_comments_and_blanks() {
    let dir = tempfile::tempdir().unwrap();
    let rc = dir.path().join(".npmrc");
    std::fs::write(
        &rc,
        "# comment\n\n; another comment\nregistry=https://r.com\n",
    )
    .unwrap();

    let entries = parse_npmrc(&rc).unwrap();
    assert_eq!(entries.len(), 1);
}

#[test]
fn test_substitute_env() {
    // Use a unique var name and unsafe block (required in edition 2024)
    unsafe { std::env::set_var("AUBE_TEST_TOKEN_CFG", "mytoken") };
    assert_eq!(substitute_env("${AUBE_TEST_TOKEN_CFG}"), "mytoken");
    assert_eq!(
        substitute_env("prefix-${AUBE_TEST_TOKEN_CFG}-suffix"),
        "prefix-mytoken-suffix"
    );
    assert_eq!(substitute_env("no-vars-here"), "no-vars-here");
    unsafe { std::env::remove_var("AUBE_TEST_TOKEN_CFG") };
}

#[test]
fn test_substitute_env_missing_var() {
    assert_eq!(substitute_env("${AUBE_DEFINITELY_NOT_SET}"), "");
}

#[test]
fn parse_npmrc_strips_surrounding_quotes() {
    let dir = tempfile::tempdir().unwrap();
    let rc = dir.path().join(".npmrc");
    std::fs::write(
        &rc,
        "//artifactory.example.com/api/npm/virtual-npm/:_auth=\"token==\"\n\
             //registry.example.com/:_authToken='single-quoted'\n\
             registry=\"https://r.example.com/\"\n\
             unmatched=\"only-leading\n\
             plain=value\n",
    )
    .unwrap();

    let entries = parse_npmrc(&rc).unwrap();
    assert_eq!(
        entries,
        vec![
            (
                "//artifactory.example.com/api/npm/virtual-npm/:_auth".to_string(),
                "token==".to_string()
            ),
            (
                "//registry.example.com/:_authToken".to_string(),
                "single-quoted".to_string()
            ),
            ("registry".to_string(), "https://r.example.com/".to_string()),
            ("unmatched".to_string(), "\"only-leading".to_string()),
            ("plain".to_string(), "value".to_string()),
        ]
    );
}

#[test]
fn parse_npmrc_expands_env_in_keys_for_per_uri_auth() {
    // Regression for jdx/aube#519. Nexus / Artifactory setups
    // commonly template the registry-prefix portion of per-URI
    // auth keys via env vars injected by sops/CI:
    //
    //     ${NEXUS_NPM_AUTH_URL}:_auth=${NEXUS_NPM_TOKEN}
    //
    // pnpm/npm both expand `${VAR}` on the key side as well as
    // the value side, so the entry lands in `auth_by_uri` keyed
    // by the real host. Without key-side expansion the entry was
    // stored under the literal `${NEXUS_NPM_AUTH_URL}` and the
    // tarball request never picked up the basic-auth credential.
    //
    // RAII guard so a panic between `set_var` and the manual
    // cleanup can't leak these names into the rest of the test
    // run (the harness runs cases in parallel threads on shared
    // process-wide env).
    let _vars = ScopedEnvVars(&["AUBE_TEST_NEXUS_HOST_CFG", "AUBE_TEST_NEXUS_TOKEN_CFG"]);
    unsafe {
        std::env::set_var(
            "AUBE_TEST_NEXUS_HOST_CFG",
            "//nexus.example.com/repository/npm/",
        );
        std::env::set_var("AUBE_TEST_NEXUS_TOKEN_CFG", "dXNlcjpwYXNz");
    }

    let dir = tempfile::tempdir().unwrap();
    let rc = dir.path().join(".npmrc");
    std::fs::write(
        &rc,
        "${AUBE_TEST_NEXUS_HOST_CFG}:_auth=${AUBE_TEST_NEXUS_TOKEN_CFG}\n",
    )
    .unwrap();

    let entries = parse_npmrc(&rc).unwrap();

    assert_eq!(
        entries,
        vec![(
            "//nexus.example.com/repository/npm/:_auth".to_string(),
            "dXNlcjpwYXNz".to_string(),
        )]
    );

    let mut config = NpmConfig::default();
    config.apply(entries);
    assert_eq!(
        config
            .basic_auth_for("https://nexus.example.com/repository/npm/@scope/pkg/-/pkg-1.0.0.tgz"),
        Some("dXNlcjpwYXNz".to_string()),
        "tarball URL under the env-templated host must pick up _auth",
    );
}

#[test]
fn parse_npmrc_untrusted_preserves_env_refs_in_keys_and_values() {
    let _vars = ScopedEnvVars(&["AUBE_TEST_PROJECT_HOST_CFG", "AUBE_TEST_PROJECT_TOKEN_CFG"]);
    unsafe {
        std::env::set_var("AUBE_TEST_PROJECT_HOST_CFG", "//registry.example.com/");
        std::env::set_var("AUBE_TEST_PROJECT_TOKEN_CFG", "secret-token");
    }

    let dir = tempfile::tempdir().unwrap();
    let rc = dir.path().join(".npmrc");
    std::fs::write(
        &rc,
        "${AUBE_TEST_PROJECT_HOST_CFG}:_authToken=${AUBE_TEST_PROJECT_TOKEN_CFG}\n",
    )
    .unwrap();

    let entries = parse_npmrc_untrusted(&rc).unwrap();

    assert_eq!(
        entries,
        vec![(
            "${AUBE_TEST_PROJECT_HOST_CFG}:_authToken".to_string(),
            "${AUBE_TEST_PROJECT_TOKEN_CFG}".to_string(),
        )]
    );
}

#[test]
fn project_npmrc_does_not_expand_env_into_auth() {
    let _vars = ScopedEnvVars(&["AUBE_TEST_PROJECT_TOKEN_CFG"]);
    unsafe { std::env::set_var("AUBE_TEST_PROJECT_TOKEN_CFG", "secret-token") };

    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".npmrc"),
        "//registry.example.com/:_authToken=${AUBE_TEST_PROJECT_TOKEN_CFG}\n",
    )
    .unwrap();

    let mut config = NpmConfig::default();
    config.apply_tagged(load_npmrc_entries_tagged_with_home(
        Some(home.path()),
        None,
        project.path(),
        None,
    ));

    assert_eq!(
        config.auth_token_for("https://registry.example.com/"),
        None,
        "project .npmrc auth env refs must be ignored instead of stored literally",
    );
}

#[test]
fn user_npmrc_still_expands_env_into_auth() {
    let _vars = ScopedEnvVars(&["AUBE_TEST_USER_TOKEN_CFG"]);
    unsafe { std::env::set_var("AUBE_TEST_USER_TOKEN_CFG", "secret-token") };

    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".npmrc"),
        "//registry.example.com/:_authToken=${AUBE_TEST_USER_TOKEN_CFG}\n",
    )
    .unwrap();

    let mut config = NpmConfig::default();
    config.apply_tagged(load_npmrc_entries_tagged_with_home(
        Some(home.path()),
        None,
        project.path(),
        None,
    ));

    assert_eq!(
        config.auth_token_for("https://registry.example.com/"),
        Some("secret-token"),
        "user .npmrc keeps trusted env substitution",
    );
}

#[test]
fn npmrc_auth_file_does_not_expand_env_into_auth() {
    let _vars = ScopedEnvVars(&["AUBE_TEST_AUTH_FILE_TOKEN_CFG"]);
    unsafe { std::env::set_var("AUBE_TEST_AUTH_FILE_TOKEN_CFG", "secret-token") };

    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let auth_file = project.path().join("auth.npmrc");
    std::fs::write(
        &auth_file,
        "//registry.example.com/:_authToken=${AUBE_TEST_AUTH_FILE_TOKEN_CFG}\n",
    )
    .unwrap();
    std::fs::write(
        project.path().join(".npmrc"),
        format!("npmrc-auth-file={}\n", auth_file.display()),
    )
    .unwrap();

    let mut config = NpmConfig::default();
    config.apply_tagged(load_npmrc_entries_tagged_with_home(
        Some(home.path()),
        None,
        project.path(),
        None,
    ));

    assert_eq!(
        config.auth_token_for("https://registry.example.com/"),
        None,
        "auth file loaded through project config inherits project trust and ignores auth env refs",
    );
}

#[test]
fn user_declared_npmrc_auth_file_expands_env_into_auth() {
    let _vars = ScopedEnvVars(&["AUBE_TEST_USER_AUTH_FILE_TOKEN_CFG"]);
    unsafe { std::env::set_var("AUBE_TEST_USER_AUTH_FILE_TOKEN_CFG", "secret-token") };

    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let auth_file = home.path().join("auth.npmrc");
    std::fs::write(
        &auth_file,
        "//registry.example.com/:_authToken=${AUBE_TEST_USER_AUTH_FILE_TOKEN_CFG}\n",
    )
    .unwrap();
    std::fs::write(
        home.path().join(".npmrc"),
        format!("npmrc-auth-file={}\n", auth_file.display()),
    )
    .unwrap();

    let mut config = NpmConfig::default();
    config.apply_tagged(load_npmrc_entries_tagged_with_home(
        Some(home.path()),
        None,
        project.path(),
        None,
    ));

    assert_eq!(
        config.auth_token_for("https://registry.example.com/"),
        Some("secret-token"),
        "auth file loaded through user config keeps trusted env substitution",
    );
}

#[test]
fn user_declared_npmrc_auth_file_loses_to_project_npmrc() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let auth_file = home.path().join("auth.npmrc");
    std::fs::write(
        &auth_file,
        "//registry.example.com/:_authToken=user-auth-file-token\n",
    )
    .unwrap();
    std::fs::write(
        home.path().join(".npmrc"),
        format!("npmrc-auth-file={}\n", auth_file.display()),
    )
    .unwrap();
    std::fs::write(
        project.path().join(".npmrc"),
        "//registry.example.com/:_authToken=project-token\n",
    )
    .unwrap();

    let entries = load_npmrc_entries_with_home(Some(home.path()), None, project.path(), None);
    let mut config = NpmConfig::default();
    config.apply(entries);

    assert_eq!(
        config.auth_token_for("https://registry.example.com/"),
        Some("project-token"),
        "user-declared auth file entries should stay in the user precedence layer",
    );
}

/// The full npm config-file cascade — builtin < global < user < project —
/// resolves with the right precedence: each scope overrides the one below
/// it, and the project `.npmrc` still wins overall. Drives the tagged
/// walker directly with injected home + global paths so the developer's
/// real `~/.npmrc` and `NPM_CONFIG_*` env can't perturb the assertion.
#[test]
fn npmrc_cascade_orders_builtin_global_user_project() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let global_dir = tempfile::tempdir().unwrap();

    let builtin_rc = global_dir.path().join("builtin-npmrc");
    let global_rc = global_dir.path().join("global-npmrc");

    // Each scope sets `registry` (so we can see who wins overall) plus a
    // scope-unique `@<scope>:registry` (so we can see every scope was read).
    std::fs::write(
        &builtin_rc,
        "registry=https://builtin.example/\n@builtin:registry=https://builtin.example/\n",
    )
    .unwrap();
    std::fs::write(
        &global_rc,
        "registry=https://global.example/\n@global:registry=https://global.example/\n",
    )
    .unwrap();
    std::fs::write(
        home.path().join(".npmrc"),
        "registry=https://user.example/\n@user:registry=https://user.example/\n",
    )
    .unwrap();
    std::fs::write(
        project.path().join(".npmrc"),
        "registry=https://project.example/\n@project:registry=https://project.example/\n",
    )
    .unwrap();

    let globals = GlobalNpmrcPaths {
        builtin: Some(builtin_rc),
        global: Some(global_rc),
    };
    let tagged = load_npmrc_entries_tagged_with_globals(
        Some(home.path()),
        None,
        project.path(),
        None,
        &globals,
    );

    // The `registry` entries appear in cascade order, lowest first.
    let registry_order: Vec<(NpmrcSource, &str)> = tagged
        .iter()
        .filter(|(_, k, _)| k == "registry")
        .map(|(s, _, v)| (*s, v.as_str()))
        .collect();
    assert_eq!(
        registry_order,
        vec![
            (NpmrcSource::Builtin, "https://builtin.example/"),
            (NpmrcSource::Global, "https://global.example/"),
            (NpmrcSource::User, "https://user.example/"),
            (NpmrcSource::Project, "https://project.example/"),
        ],
        "registry entries must be emitted builtin < global < user < project",
    );

    let mut config = NpmConfig::default();
    config.apply_tagged(tagged);

    // Last-write-wins → project's default registry is the resolved one.
    assert_eq!(
        config.registry, "https://project.example/",
        "project .npmrc registry wins the full cascade",
    );
    // Every scope's scoped-registry override survived (proving all four
    // files were read, not just the winner).
    assert_eq!(
        config.registry_for("@builtin/pkg"),
        "https://builtin.example/"
    );
    assert_eq!(
        config.registry_for("@global/pkg"),
        "https://global.example/"
    );
    assert_eq!(config.registry_for("@user/pkg"), "https://user.example/");
    assert_eq!(
        config.registry_for("@project/pkg"),
        "https://project.example/"
    );
}

/// Trust posture: the global `npmrc` is admin/system-controlled, so it may
/// set a subprocess-spawning `tokenHelper`; a project `.npmrc` (attacker-
/// controlled on a hostile clone) may not. Mirrors npm's trust model where
/// global config ranks at/above user.
#[test]
fn global_npmrc_may_set_token_helper_project_may_not() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let global_dir = tempfile::tempdir().unwrap();
    let global_rc = global_dir.path().join("global-npmrc");

    // A bare absolute path passes sanitize_token_helper.
    std::fs::write(
        &global_rc,
        "//registry.example.com/:tokenHelper=/usr/local/bin/get-token\n",
    )
    .unwrap();
    // The project tries to override it with its own helper.
    std::fs::write(
        project.path().join(".npmrc"),
        "//registry.example.com/:tokenHelper=/tmp/evil\n",
    )
    .unwrap();

    let globals = GlobalNpmrcPaths {
        builtin: None,
        global: Some(global_rc),
    };
    let mut config = NpmConfig::default();
    config.apply_tagged(load_npmrc_entries_tagged_with_globals(
        Some(home.path()),
        None,
        project.path(),
        None,
        &globals,
    ));

    assert_eq!(
        config.token_helper_for("https://registry.example.com/"),
        Some("/usr/local/bin/get-token"),
        "global tokenHelper is trusted; the project's attempt to override is rejected",
    );
}

/// `resolve_global_npmrc_paths` derives the global path from
/// `NPM_CONFIG_PREFIX` (`$PREFIX/etc/npmrc`) and honors an explicit
/// `NPM_CONFIG_GLOBALCONFIG`. With no prefix and no override, both scopes
/// are absent — aube does not fabricate a path.
#[test]
fn resolve_global_npmrc_paths_follows_npm_rules() {
    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    // Prefix → $PREFIX/etc/npmrc, builtin under the npm package.
    let from_prefix = resolve_global_npmrc_paths(lookup(&[("NPM_CONFIG_PREFIX", "/opt/node")]));
    assert_eq!(
        from_prefix.global.as_deref(),
        Some(Path::new("/opt/node/etc/npmrc")),
    );
    assert!(
        from_prefix
            .builtin
            .as_deref()
            .is_some_and(|p| p.starts_with("/opt/node") && p.ends_with("npmrc")),
        "builtin path is derived under the npm install prefix",
    );

    // Explicit GLOBALCONFIG overrides the prefix-derived global path.
    let explicit = resolve_global_npmrc_paths(lookup(&[
        ("NPM_CONFIG_PREFIX", "/opt/node"),
        ("NPM_CONFIG_GLOBALCONFIG", "/etc/corp/npmrc"),
    ]));
    assert_eq!(
        explicit.global.as_deref(),
        Some(Path::new("/etc/corp/npmrc")),
    );

    // No prefix, no override → no fabricated paths.
    let empty = resolve_global_npmrc_paths(lookup(&[]));
    assert!(empty.global.is_none() && empty.builtin.is_none());
}

/// End-to-end through the public `NpmConfig` loader: a scoped registry set
/// ONLY in the global `npmrc` (pointed at by `NPM_CONFIG_GLOBALCONFIG` in
/// the captured env) is resolved by the registry client. Before global
/// npmrc was read this scope resolved to the default registry. A scope the
/// developer's real `~/.npmrc` won't define keeps the assertion robust
/// against the process `$HOME` that `load_with_env` reads.
#[test]
fn npm_config_picks_up_registry_from_global_npmrc_only() {
    let global_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap(); // empty: no project .npmrc
    let global_rc = global_dir.path().join("npmrc");
    std::fs::write(
        &global_rc,
        "@globalonly:registry=https://global-only.example/\n",
    )
    .unwrap();

    let env = vec![(
        "NPM_CONFIG_GLOBALCONFIG".to_string(),
        global_rc.to_string_lossy().into_owned(),
    )];
    let config = NpmConfig::load_with_env(project.path(), &env);

    assert_eq!(
        config.registry_for("@globalonly/pkg"),
        "https://global-only.example/",
        "registry-client resolves a scope defined only in the global npmrc",
    );
}

#[test]
fn test_package_scope() {
    assert_eq!(package_scope("@myorg/pkg"), Some("@myorg"));
    assert_eq!(package_scope("lodash"), None);
    assert_eq!(package_scope("@types/node"), Some("@types"));
}

#[test]
fn test_registry_uri_key() {
    assert_eq!(
        registry_uri_key("https://registry.example.com/"),
        "//registry.example.com/"
    );
    assert_eq!(
        registry_uri_key("http://localhost:4873/"),
        "//localhost:4873/"
    );
}

#[test]
fn test_registry_uri_key_strips_default_port() {
    // https default port collapses
    assert_eq!(
        registry_uri_key("https://registry.example.com:443/"),
        "//registry.example.com/"
    );
    // http default port collapses
    assert_eq!(
        registry_uri_key("http://registry.example.com:80/artifactory/npm/"),
        "//registry.example.com/artifactory/npm/"
    );
    // Non-default port is preserved
    assert_eq!(
        registry_uri_key("https://registry.example.com:8443/"),
        "//registry.example.com:8443/"
    );
}

#[test]
fn test_registry_uri_key_only_strips_matching_default_port() {
    // https on the http default port (rare but valid) is a *different
    // server* from https on its own default — don't collapse them.
    assert_eq!(registry_uri_key("https://host:80/x/"), "//host:80/x/",);
    // Symmetric case: http on https default port stays distinct.
    assert_eq!(registry_uri_key("http://host:443/x/"), "//host:443/x/",);
}

#[test]
fn test_lookup_by_uri_prefix_longest_match() {
    // Path-scoped auth entry. A tarball URL that lives under the
    // same path should resolve, while an unrelated path should not.
    let mut map: BTreeMap<String, &'static str> = BTreeMap::new();
    map.insert("//host/artifactory/npm/".to_string(), "scoped-token");
    map.insert("//host/".to_string(), "root-token");

    // Full tarball path finds the path-scoped key.
    assert_eq!(
        lookup_by_uri_prefix(&map, "//host/artifactory/npm/lodash/-/lodash-4.17.21.tgz"),
        Some(&"scoped-token"),
    );
    // A request outside the scope falls through to the host root.
    assert_eq!(
        lookup_by_uri_prefix(&map, "//host/other/pkg.tgz"),
        Some(&"root-token"),
    );
    // Different host does not leak root-token.
    assert_eq!(lookup_by_uri_prefix(&map, "//other/foo"), None);
}

#[test]
fn auth_token_resolves_for_path_scoped_registry_with_default_port() {
    // End-to-end: `.npmrc` configures path-scoped auth under a
    // reverse-proxy path, tarball URLs carry an explicit `:443`.
    // Before the fix this 401'd because the lookup key
    // `//host:443/artifactory/npm/lodash/-/lodash-4.17.21.tgz`
    // never matched the stored `//host/artifactory/npm/` key.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "registry=https://registry.example.com/artifactory/npm/\n\
             //registry.example.com/artifactory/npm/:_authToken=scoped-secret\n",
    )
    .unwrap();

    let config = NpmConfig::load_isolated(dir.path());

    assert_eq!(
        config.auth_token_for(
            "https://registry.example.com:443/artifactory/npm/lodash/-/lodash-4.17.21.tgz"
        ),
        Some("scoped-secret"),
    );
    assert_eq!(
        config.auth_token_for(
            "https://registry.example.com/artifactory/npm/lodash/-/lodash-4.17.21.tgz"
        ),
        Some("scoped-secret"),
    );
}

#[test]
fn scoped_auth_token_resolves_for_full_tarball_url_under_path_registry() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "registry=https://registry.example.com/\n\
             //registry.example.com/:_authToken=registry-token\n\
             //registry.example.com/npm:@myorg:_authToken=scoped-token\n",
    )
    .unwrap();

    let config = NpmConfig::load_isolated(dir.path());

    assert_eq!(
        config.auth_token_for_package(
            "https://registry.example.com/npm/@myorg/pkg/-/pkg-1.0.0.tgz",
            "@myorg/pkg",
        ),
        Some("scoped-token"),
    );
    assert_eq!(
        config.auth_token_for_package(
            "https://registry.example.com/npm-release/@myorg/pkg/-/pkg-1.0.0.tgz",
            "@myorg/pkg",
        ),
        Some("registry-token"),
    );
}

#[test]
fn npmrc_key_with_default_port_is_normalized_on_ingest() {
    // User wrote `:443` explicitly in `.npmrc`. Lookups that don't
    // carry the port must still resolve.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "//registry.example.com:443/:_authToken=via-443\n",
    )
    .unwrap();

    let config = NpmConfig::load_isolated(dir.path());
    assert_eq!(
        config.auth_token_for("https://registry.example.com/"),
        Some("via-443"),
    );
}

#[test]
fn test_normalize_registry_url() {
    assert_eq!(normalize_registry_url("https://r.com"), "https://r.com/");
    assert_eq!(normalize_registry_url("https://r.com/"), "https://r.com/");
}

#[test]
fn test_config_load_project_npmrc() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "registry=https://custom.registry.com\n\
             @myorg:registry=https://myorg.registry.com\n\
             //myorg.registry.com/:_authToken=org-secret\n\
             //custom.registry.com/:_authToken=custom-secret\n",
    )
    .unwrap();

    // HOME + env isolation via `load_isolated`: `NpmConfig::load`
    // would layer the developer's real `~/.npmrc` and
    // `NPM_CONFIG_REGISTRY` env var on top of the project file,
    // either of which can shadow the `registry=` we're asserting on.
    let config = NpmConfig::load_isolated(dir.path());

    assert_eq!(config.registry, "https://custom.registry.com/");
    assert_eq!(
        config.registry_for("@myorg/pkg"),
        "https://myorg.registry.com/"
    );
    assert_eq!(
        config.registry_for("lodash"),
        "https://custom.registry.com/"
    );
    assert_eq!(
        config.auth_token_for("https://myorg.registry.com/"),
        Some("org-secret")
    );
    assert_eq!(
        config.auth_token_for("https://custom.registry.com/"),
        Some("custom-secret")
    );
}

#[test]
fn split_username_password_auth_resolves_to_basic_header_payload() {
    let dir = tempfile::tempdir().unwrap();
    let encoded_password = base64::engine::general_purpose::STANDARD.encode("s3cr3t");
    std::fs::write(
        dir.path().join(".npmrc"),
        format!(
            "//registry.example.com/:username=alice\n\
                 //registry.example.com/:_password={encoded_password}\n"
        ),
    )
    .unwrap();

    let config = NpmConfig::load_isolated(dir.path());
    let expected = base64::engine::general_purpose::STANDARD.encode("alice:s3cr3t");
    assert_eq!(
        config.basic_auth_for("https://registry.example.com/"),
        Some(expected),
    );
}

#[test]
fn token_helper_from_project_npmrc_is_refused_kebab_case() {
    // Same regression as `token_helper_from_project_npmrc_is_refused`
    // but using the `token-helper` kebab-case alias that
    // `apply_tagged` also accepts. Confirms the gate fires for
    // both spellings, not just the camelCase key.
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".npmrc"),
        "//registry.example.com/:token-helper=/tmp/evil.sh\n",
    )
    .unwrap();

    let home = tempfile::tempdir().unwrap();
    let mut config = NpmConfig::default();
    config.apply_tagged(load_npmrc_entries_tagged_with_home(
        Some(home.path()),
        None,
        project.path(),
        None,
    ));
    assert_eq!(
        config.token_helper_for("https://registry.example.com/"),
        None,
        "project-scope token-helper (kebab-case) must be refused"
    );
}

#[test]
fn token_helper_from_project_npmrc_is_refused() {
    // Regression for the CVE-2025-69262 class: a project-scope
    // `.npmrc` that a hostile repo can commit used to be able
    // to set `tokenHelper`, which aube then spawned via
    // `sh -c <value>` at the next authed registry request.
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".npmrc"),
        "//registry.example.com/:tokenHelper=/tmp/evil.sh\n",
    )
    .unwrap();

    let home = tempfile::tempdir().unwrap();
    let mut config = NpmConfig::default();
    config.apply_tagged(load_npmrc_entries_tagged_with_home(
        Some(home.path()),
        None,
        project.path(),
        None,
    ));
    assert_eq!(
        config.token_helper_for("https://registry.example.com/"),
        None,
        "project-scope tokenHelper must be refused"
    );
}

#[test]
fn untrusted_scoped_token_helper_does_not_shadow_registry_token() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".npmrc"),
        "//npm.pkg.github.com/:_authToken=broad-token\n",
    )
    .unwrap();

    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".npmrc"),
        "//npm.pkg.github.com/:@myorg:tokenHelper=/tmp/evil.sh\n",
    )
    .unwrap();

    let mut config = NpmConfig::default();
    config.apply_tagged(load_npmrc_entries_tagged_with_home(
        Some(home.path()),
        None,
        project.path(),
        None,
    ));

    assert_eq!(
        config.auth_token_for_package("https://npm.pkg.github.com/", "@myorg/pkg"),
        Some("broad-token"),
        "ignored project tokenHelper must not create an empty scoped auth entry"
    );
}

#[test]
fn token_helper_from_user_npmrc_is_accepted() {
    // The user's own `~/.npmrc` is the only file trusted to
    // configure subprocess execution. A valid bare absolute
    // path passes the sanitizer and reaches `token_helper_for`.
    let home = tempfile::tempdir().unwrap();
    let helper_path = if cfg!(windows) {
        "C:\\opt\\aube\\helper.exe"
    } else {
        "/opt/aube/helper"
    };
    std::fs::write(
        home.path().join(".npmrc"),
        format!("//registry.example.com/:tokenHelper={helper_path}\n"),
    )
    .unwrap();

    let project = tempfile::tempdir().unwrap();
    let mut config = NpmConfig::default();
    config.apply_tagged(load_npmrc_entries_tagged_with_home(
        Some(home.path()),
        None,
        project.path(),
        None,
    ));
    assert_eq!(
        config.token_helper_for("https://registry.example.com/"),
        Some(helper_path)
    );
}

#[test]
fn token_helper_from_npmrc_auth_file_is_refused() {
    // `npmrc-auth-file` lets a user point aube at a sidecar
    // `.npmrc` for auth. The path itself can be set from a
    // project `.npmrc`, so the file's contents inherit the
    // project trust level and must not be allowed to set
    // `tokenHelper` either.
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let auth = project.path().join("auth.rc");
    std::fs::write(&auth, "//registry.example.com/:tokenHelper=/tmp/evil.sh\n").unwrap();
    std::fs::write(
        project.path().join(".npmrc"),
        format!(
            "npmrc-auth-file={}\n",
            auth.to_string_lossy().replace('\\', "/")
        ),
    )
    .unwrap();

    let mut config = NpmConfig::default();
    config.apply_tagged(load_npmrc_entries_tagged_with_home(
        Some(home.path()),
        None,
        project.path(),
        None,
    ));
    assert_eq!(
        config.token_helper_for("https://registry.example.com/"),
        None,
        "tokenHelper from an auth file reachable via project `.npmrc` must be refused"
    );
}

#[test]
fn sanitize_token_helper_accepts_absolute_path() {
    assert_eq!(
        sanitize_token_helper("/usr/local/bin/aws-npm-helper"),
        Some("/usr/local/bin/aws-npm-helper".to_string())
    );
    assert_eq!(
        sanitize_token_helper("C:\\Program.Files\\auth.exe"),
        Some("C:\\Program.Files\\auth.exe".to_string())
    );
    assert_eq!(
        sanitize_token_helper("C:/tools/auth.exe"),
        Some("C:/tools/auth.exe".to_string())
    );
    // UNC paths are absolute on Windows.
    assert_eq!(
        sanitize_token_helper("\\\\server\\share\\auth.exe"),
        Some("\\\\server\\share\\auth.exe".to_string())
    );
}

#[test]
fn sanitize_token_helper_rejects_relative_path() {
    assert!(sanitize_token_helper("aws-helper").is_none());
    assert!(sanitize_token_helper("./aws-helper").is_none());
    assert!(sanitize_token_helper("bin/aws-helper").is_none());
}

#[test]
fn sanitize_token_helper_rejects_shell_metacharacters() {
    // `sh -c` / `cmd /C` would otherwise reinterpret any of
    // these as a pipeline separator or substitution marker.
    for v in [
        "/bin/helper;rm",
        "/bin/helper|rm",
        "/bin/helper&rm",
        "/bin/helper`rm`",
        "/bin/helper$(rm)",
        "/bin/helper>log",
        "/bin/helper<log",
        "/bin/helper*glob",
        "/bin/helper?glob",
        "/bin/helper\"evil",
        "/bin/helper'evil",
    ] {
        assert!(sanitize_token_helper(v).is_none(), "should reject {v:?}");
    }
}

#[test]
fn sanitize_token_helper_rejects_whitespace() {
    // Arguments must not be smuggled into the value. pnpm's
    // tokenHelper contract is a path to an executable, so any
    // extra tokens have to go in a wrapper script.
    assert!(sanitize_token_helper("/bin/helper --flag").is_none());
    assert!(sanitize_token_helper("/bin/helper\targ").is_none());
    assert!(sanitize_token_helper("/bin/helper\nevil").is_none());
}

#[test]
fn sanitize_token_helper_rejects_empty_and_nul() {
    assert!(sanitize_token_helper("").is_none());
    assert!(sanitize_token_helper("   ").is_none());
    assert!(sanitize_token_helper("/bin/helper\0evil").is_none());
}

#[test]
fn sanitize_token_helper_rejects_env_substitution_markers() {
    // `${VAR}` and `$VAR` both fail because `$` is in the
    // metacharacter rejection set. This matches pnpm 10.27.0
    // throwing on env-var tokens in the value.
    assert!(sanitize_token_helper("/bin/helper-${EVIL}").is_none());
    assert!(sanitize_token_helper("/bin/$EVIL").is_none());
}

#[test]
fn per_registry_tls_config_is_parsed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
            dir.path().join(".npmrc"),
            "//registry.example.com/:ca=-----BEGIN CERTIFICATE-----\\nca\\n-----END CERTIFICATE-----\n\
             //registry.example.com/:cafile=corp-ca.pem\n\
             //registry.example.com/:cert=-----BEGIN CERTIFICATE-----\\nclient\\n-----END CERTIFICATE-----\n\
             //registry.example.com/:key=-----BEGIN PRIVATE KEY-----\\nkey\\n-----END PRIVATE KEY-----\n",
        )
        .unwrap();

    let config = NpmConfig::load_isolated(dir.path());
    let tls = &config
        .registry_config_for("https://registry.example.com/")
        .expect("registry config")
        .tls;
    assert_eq!(tls.ca.len(), 1);
    assert!(tls.ca[0].contains("\nca\n"));
    assert!(!tls.ca[0].contains("\\n"));
    assert_eq!(tls.cafile.as_deref(), Some(Path::new("corp-ca.pem")));
    assert!(tls.cert.as_deref().unwrap().contains("\nclient\n"));
    assert!(tls.key.as_deref().unwrap().contains("\nkey\n"));
}

#[test]
fn top_level_cafile_and_ca_are_parsed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "cafile=/etc/ssl/corp-bundle.pem\n\
             ca=-----BEGIN CERTIFICATE-----\\nfirst\\n-----END CERTIFICATE-----\n\
             ca[]=-----BEGIN CERTIFICATE-----\\nsecond\\n-----END CERTIFICATE-----\n",
    )
    .unwrap();

    let config = NpmConfig::load_isolated(dir.path());
    assert_eq!(
        config.cafile.as_deref(),
        Some(Path::new("/etc/ssl/corp-bundle.pem"))
    );
    assert_eq!(config.ca.len(), 2);
    assert!(config.ca[0].contains("\nfirst\n"));
    assert!(config.ca[1].contains("\nsecond\n"));
    // Top-level keys must not leak into per-registry config.
    assert!(
        config
            .registry_config_for("https://registry.npmjs.org/")
            .is_none()
    );
}

#[test]
fn unscoped_auth_token_is_pinned_to_same_source_registry() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".npmrc"), "_authToken=global-token\n").unwrap();

    // Isolate from the host's real `~/.npmrc`: a developer or CI
    // runner with `//registry.npmjs.org/:_authToken=...` already
    // logged in would otherwise affect this assertion.
    let config = NpmConfig::load_isolated(dir.path());
    // Unscoped auth is pinned to npmjs at load time. It must not
    // remain a floating fallback.
    assert_eq!(
        config.auth_token_for("https://registry.npmjs.org/"),
        Some("global-token")
    );
    assert_eq!(config.auth_token_for("https://registry.example.com/"), None);
}

#[test]
fn unscoped_auth_uses_registry_from_same_source() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".npmrc"),
        "registry=https://registry.npmjs.org/\n_authToken=user-token\n",
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".npmrc"),
        "registry=https://registry.internal.example/\n",
    )
    .unwrap();

    let mut config = NpmConfig::default();
    config.apply_tagged(load_npmrc_entries_tagged_with_home(
        Some(home.path()),
        None,
        project.path(),
        None,
    ));

    assert_eq!(
        config.registry, "https://registry.internal.example/",
        "project registry still wins as the effective default"
    );
    assert_eq!(
        config.auth_token_for("https://registry.npmjs.org/"),
        Some("user-token")
    );
    assert_eq!(
        config.auth_token_for("https://registry.internal.example/"),
        None,
        "project registry must not inherit user source's unscoped token"
    );
}

#[test]
fn unscoped_auth_uses_same_source_registry_regardless_of_order() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "_authToken=private-token\nregistry=https://registry.private.example/\n",
    )
    .unwrap();

    let config = NpmConfig::load_isolated(dir.path());
    assert_eq!(
        config.auth_token_for("https://registry.private.example/"),
        Some("private-token")
    );
    assert_eq!(config.auth_token_for("https://registry.npmjs.org/"), None);
}

#[test]
fn uri_scoped_auth_beats_later_rescoped_bare_auth() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".npmrc"),
        "//registry.npmjs.org/:_authToken=user-token\n",
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join(".npmrc"), "_authToken=project-token\n").unwrap();

    let mut config = NpmConfig::default();
    config.apply_tagged(load_npmrc_entries_tagged_with_home(
        Some(home.path()),
        None,
        project.path(),
        None,
    ));

    assert_eq!(
        config.auth_token_for("https://registry.npmjs.org/"),
        Some("user-token")
    );
}

#[test]
fn later_bare_auth_can_override_earlier_bare_auth() {
    let mut config = NpmConfig::default();
    config.apply_tagged(vec![
        (
            NpmrcSource::User,
            "_authToken".to_string(),
            "user-token".to_string(),
        ),
        (
            NpmrcSource::Env,
            "_authToken".to_string(),
            "env-token".to_string(),
        ),
    ]);

    assert_eq!(
        config.auth_token_for("https://registry.npmjs.org/"),
        Some("env-token")
    );
}

#[test]
fn later_uri_scoped_auth_can_override_earlier_bare_auth() {
    let mut config = NpmConfig::default();
    config.apply_tagged(vec![
        (
            NpmrcSource::User,
            "_authToken".to_string(),
            "user-token".to_string(),
        ),
        (
            NpmrcSource::Project,
            "//registry.npmjs.org/:_authToken".to_string(),
            "project-token".to_string(),
        ),
    ]);

    assert_eq!(
        config.auth_token_for("https://registry.npmjs.org/"),
        Some("project-token")
    );
}

#[test]
fn unscoped_tls_client_credentials_are_registry_scoped() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "registry=https://registry.example.com/\n\
         cert=-----BEGIN CERTIFICATE-----\\nclient\\n-----END CERTIFICATE-----\n\
         key=-----BEGIN PRIVATE KEY-----\\nkey\\n-----END PRIVATE KEY-----\n",
    )
    .unwrap();

    let config = NpmConfig::load_isolated(dir.path());
    let tls = &config
        .registry_config_for("https://registry.example.com/")
        .unwrap()
        .tls;
    assert!(tls.cert.as_deref().unwrap().contains("\nclient\n"));
    assert!(tls.key.as_deref().unwrap().contains("\nkey\n"));
    assert!(
        config
            .registry_config_for("https://registry.npmjs.org/")
            .is_none()
    );
}

#[test]
fn test_config_defaults() {
    let dir = tempfile::tempdir().unwrap();
    // No .npmrc at all. Same HOME isolation rationale as
    // `unscoped_auth_token_is_pinned_to_same_source_registry` —
    // without it this assertion flakes on any developer box whose
    // `~/.npmrc` has ever been touched by `npm login`.
    let config = NpmConfig::load_isolated(dir.path());
    assert_eq!(config.registry, "https://registry.npmjs.org/");
    assert!(
        config
            .auth_token_for("https://registry.npmjs.org/")
            .is_none()
    );
}

#[test]
fn test_config_scoped_registry_without_auth() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "@private:registry=https://private.registry.com\n",
    )
    .unwrap();

    let config = NpmConfig::load_isolated(dir.path());
    assert_eq!(
        config.registry_for("@private/my-lib"),
        "https://private.registry.com/"
    );
    assert!(
        config
            .auth_token_for("https://private.registry.com/")
            .is_none()
    );
}

#[test]
fn test_http_proxy_inherits_https_proxy() {
    // pnpm's fallback: `httpProxy` inherits whatever `httpsProxy`
    // resolved to when no HTTP-specific value is configured,
    // so a single `https-proxy=` line configures both schemes.
    //
    // We scrub the proxy env vars inside the `apply_proxy_env`
    // helper's view by staging the field value directly: the
    // real resolver is pure once `https_proxy` is already set,
    // so `env_any` is never consulted for the HTTPS half and
    // this assertion can't race a developer's shell.
    let mut config = NpmConfig {
        https_proxy: Some("http://corp.proxy:8080".to_string()),
        ..Default::default()
    };
    // Drop any ambient `HTTP_PROXY` so the second `or_else` in
    // `apply_proxy_env` can't beat us to the fallback. We can't
    // use `std::env::remove_var` safely across parallel tests;
    // instead, pre-populate `http_proxy` to `None` and rely on
    // the field-level fallback only.
    // Since `https_proxy` is already `Some`, the resolver takes
    // that branch first — `env_any("HTTP_PROXY", ...)` is never
    // called.
    config.apply_proxy_env();
    assert_eq!(
        config.http_proxy.as_deref(),
        Some("http://corp.proxy:8080"),
        "http_proxy must inherit https_proxy"
    );
}

#[test]
fn test_npmrc_proxy_key_feeds_https_proxy() {
    // pnpm treats `.npmrc proxy=` as the fallback for
    // `httpsProxy`, not as a direct alias for `httpProxy`.
    let mut config = NpmConfig {
        npmrc_proxy: Some("http://legacy:3128".to_string()),
        ..Default::default()
    };
    config.apply_proxy_env();
    assert_eq!(
        config.https_proxy.as_deref(),
        Some("http://legacy:3128"),
        "legacy `proxy=` key must resolve into https_proxy"
    );
    assert_eq!(
        config.http_proxy.as_deref(),
        Some("http://legacy:3128"),
        "http_proxy then inherits the resolved https_proxy"
    );
}

#[test]
fn test_explicit_https_proxy_wins_over_npmrc_proxy() {
    let mut config = NpmConfig {
        https_proxy: Some("http://explicit:1".to_string()),
        npmrc_proxy: Some("http://fallback:2".to_string()),
        ..Default::default()
    };
    config.apply_proxy_env();
    assert_eq!(config.https_proxy.as_deref(), Some("http://explicit:1"));
}

#[test]
fn test_default_strict_ssl_is_true() {
    // Regression: `NpmConfig::default()` must not leave
    // `strict_ssl = false` (bool::default), because
    // `RegistryClient::new` spreads the default and would
    // otherwise silently disable TLS cert validation.
    let c = NpmConfig::default();
    assert!(c.strict_ssl);
}

#[test]
fn test_parses_proxy_and_ssl_settings() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "https-proxy=http://proxy.example.com:8080\n\
             proxy=http://plain.example.com:3128\n\
             noproxy=localhost,.internal\n\
             strict-ssl=false\n\
             local-address=127.0.0.1\n\
             maxsockets=12\n",
    )
    .unwrap();

    // Isolate from the developer's real ~/.npmrc
    let home = tempfile::tempdir().unwrap();
    let mut config = NpmConfig {
        registry: "https://registry.npmjs.org/".to_string(),
        strict_ssl: true,
        ..Default::default()
    };
    config.apply(load_npmrc_entries_with_home(
        Some(home.path()),
        None,
        dir.path(),
        None,
    ));

    assert_eq!(
        config.https_proxy.as_deref(),
        Some("http://proxy.example.com:8080")
    );
    // `.npmrc proxy=` stores into `npmrc_proxy`, which feeds
    // `https_proxy`/`http_proxy` only via `apply_proxy_env`. We
    // called raw `apply` here, so the field is still the
    // verbatim legacy key.
    assert_eq!(
        config.npmrc_proxy.as_deref(),
        Some("http://plain.example.com:3128")
    );
    assert!(config.http_proxy.is_none());
    assert_eq!(config.no_proxy.as_deref(), Some("localhost,.internal"));
    assert!(!config.strict_ssl);
    assert_eq!(
        config.local_address,
        Some("127.0.0.1".parse::<std::net::IpAddr>().unwrap())
    );
    assert_eq!(config.max_sockets, Some(12));
}

#[test]
fn test_strict_ssl_default_true() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".npmrc"), "").unwrap();
    let mut config = NpmConfig {
        strict_ssl: true,
        ..Default::default()
    };
    config.apply(load_npmrc_entries_with_home(
        Some(home.path()),
        None,
        dir.path(),
        None,
    ));
    assert!(config.strict_ssl);
}

#[test]
fn test_camel_case_proxy_aliases() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "httpsProxy=http://a\nhttpProxy=http://b\nnoProxy=foo\nstrictSsl=false\nlocalAddress=::1\n",
    )
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    let mut config = NpmConfig {
        strict_ssl: true,
        ..Default::default()
    };
    config.apply(load_npmrc_entries_with_home(
        Some(home.path()),
        None,
        dir.path(),
        None,
    ));
    assert_eq!(config.https_proxy.as_deref(), Some("http://a"));
    assert_eq!(config.http_proxy.as_deref(), Some("http://b"));
    assert_eq!(config.no_proxy.as_deref(), Some("foo"));
    assert!(!config.strict_ssl);
    assert_eq!(
        config.local_address,
        Some("::1".parse::<std::net::IpAddr>().unwrap())
    );
}

#[test]
fn test_invalid_proxy_values_dropped() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "local-address=not-an-ip\nmaxsockets=zero\nstrict-ssl=perhaps\n",
    )
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    let mut config = NpmConfig {
        strict_ssl: true,
        ..Default::default()
    };
    config.apply(load_npmrc_entries_with_home(
        Some(home.path()),
        None,
        dir.path(),
        None,
    ));
    assert!(config.local_address.is_none());
    assert!(config.max_sockets.is_none());
    // Garbage boolean leaves the previous value in place.
    assert!(config.strict_ssl);
}

// `auto-install-peers` parsing lives in aube's settings_values
// module now — see tests there. NpmConfig only knows about
// registry-client config (URL, auth, scopes).

#[test]
fn test_load_npmrc_entries_orders_user_before_project() {
    // The downstream settings resolver iterates the returned Vec in
    // reverse to give project-level entries priority, so the
    // invariant this test pins is specifically the ordering: user
    // entries MUST appear before project entries for the same key.
    //
    // Uses `load_npmrc_entries_with_home` (test-only helper) to
    // inject a fake user home rather than mutating `$HOME` on the
    // process, which would race with any other test reading env.
    let home_dir = tempfile::tempdir().unwrap();
    let proj_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        home_dir.path().join(".npmrc"),
        "auto-install-peers=true\nfoo=user-only\n",
    )
    .unwrap();
    std::fs::write(
        proj_dir.path().join(".npmrc"),
        "auto-install-peers=false\nbar=project-only\n",
    )
    .unwrap();

    let entries = load_npmrc_entries_with_home(Some(home_dir.path()), None, proj_dir.path(), None);

    // Both keys from each file are present.
    assert!(entries.iter().any(|(k, v)| k == "foo" && v == "user-only"));
    assert!(
        entries
            .iter()
            .any(|(k, v)| k == "bar" && v == "project-only")
    );

    // The shared key appears twice, in the right order.
    let positions: Vec<_> = entries
        .iter()
        .filter(|(k, _)| k == "auto-install-peers")
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(
        positions.len(),
        2,
        "expected both user and project entries for shared key: {entries:?}"
    );
    assert_eq!(
        positions[0], "true",
        "user entry must come first (precedence is last-write-wins downstream)"
    );
    assert_eq!(
        positions[1], "false",
        "project entry must come second so it overrides the user entry"
    );
}

#[test]
fn pnpm_global_auth_ini_loads_and_overrides_user_rc() {
    // `~/.config/pnpm/auth.ini` is pnpm's out-of-band credential
    // file. Aube needs to read it so users who stash tokens there
    // (to keep them out of `~/.npmrc`) don't get "401 Unauthorized"
    // on a fresh clone. It should beat `~/.npmrc` for the same
    // key, since the entire reason to use it is to override
    // whatever npm-side tooling writes to `.npmrc`.
    let _gate = AUTH_INI_GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home_dir = tempfile::tempdir().unwrap();
    let proj_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        home_dir.path().join(".npmrc"),
        "//registry.example.com/:_authToken=stale-npmrc\n",
    )
    .unwrap();
    // Place auth.ini at pnpm's per-OS config dir (no XDG override), not a
    // flat `~/.config/pnpm` — the latter is correct only on Linux, so the
    // file must land where `pnpm_config_dir_with` resolves on the test host.
    let auth_ini = aube_util::env::pnpm_config_dir_with(Some(home_dir.path()), None)
        .unwrap()
        .join("auth.ini");
    std::fs::create_dir_all(auth_ini.parent().unwrap()).unwrap();
    std::fs::write(
        &auth_ini,
        "//registry.example.com/:_authToken=fresh-auth-ini\n\
             //other.example.com/:_authToken=other-token\n",
    )
    .unwrap();

    let entries = load_npmrc_entries_with_home(Some(home_dir.path()), None, proj_dir.path(), None);
    let mut cfg = NpmConfig::default();
    cfg.apply(entries);
    assert_eq!(
        cfg.auth_token_for("https://registry.example.com/"),
        Some("fresh-auth-ini"),
        "auth.ini token should override stale ~/.npmrc token",
    );
    assert_eq!(
        cfg.auth_token_for("https://other.example.com/"),
        Some("other-token"),
        "additional auth.ini entries should be picked up",
    );
}

#[test]
fn pnpm_global_auth_ini_honors_xdg_config_home_override() {
    // When `XDG_CONFIG_HOME` is set, pnpm reads
    // `$XDG_CONFIG_HOME/pnpm/auth.ini` instead of
    // `$HOME/.config/pnpm/auth.ini`. Aube must match, or a user
    // with a custom XDG layout will see pnpm and aube disagree on
    // where credentials live. The injected override here is the
    // same value `load_npmrc_entries` reads from the real env var.
    let _gate = AUTH_INI_GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home_dir = tempfile::tempdir().unwrap();
    let xdg_dir = tempfile::tempdir().unwrap();
    let proj_dir = tempfile::tempdir().unwrap();

    let auth_ini = xdg_dir.path().join("pnpm/auth.ini");
    std::fs::create_dir_all(auth_ini.parent().unwrap()).unwrap();
    std::fs::write(&auth_ini, "//registry.example.com/:_authToken=xdg-token\n").unwrap();
    // Decoy at the default `$HOME/.config/pnpm/auth.ini` location
    // to prove the XDG override replaces the fallback instead of
    // being merged alongside it.
    let decoy = home_dir.path().join(".config/pnpm/auth.ini");
    std::fs::create_dir_all(decoy.parent().unwrap()).unwrap();
    std::fs::write(&decoy, "//registry.example.com/:_authToken=decoy\n").unwrap();

    let entries = load_npmrc_entries_with_home(
        Some(home_dir.path()),
        Some(xdg_dir.path()),
        proj_dir.path(),
        None,
    );
    let mut cfg = NpmConfig::default();
    cfg.apply(entries);
    assert_eq!(
        cfg.auth_token_for("https://registry.example.com/"),
        Some("xdg-token"),
    );
}

#[test]
fn pnpm_global_auth_ini_loses_to_project_npmrc() {
    // Project `.npmrc` pins still win — per-repo configuration is
    // the most specific layer, and a user's global auth.ini
    // must not clobber a token a project explicitly set.
    let _gate = AUTH_INI_GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home_dir = tempfile::tempdir().unwrap();
    let proj_dir = tempfile::tempdir().unwrap();

    let auth_ini = aube_util::env::pnpm_config_dir_with(Some(home_dir.path()), None)
        .unwrap()
        .join("auth.ini");
    std::fs::create_dir_all(auth_ini.parent().unwrap()).unwrap();
    std::fs::write(
        &auth_ini,
        "//registry.example.com/:_authToken=global-auth-ini\n",
    )
    .unwrap();
    std::fs::write(
        proj_dir.path().join(".npmrc"),
        "//registry.example.com/:_authToken=project-pin\n",
    )
    .unwrap();

    let entries = load_npmrc_entries_with_home(Some(home_dir.path()), None, proj_dir.path(), None);
    let mut cfg = NpmConfig::default();
    cfg.apply(entries);
    assert_eq!(
        cfg.auth_token_for("https://registry.example.com/"),
        Some("project-pin"),
    );
}

#[test]
fn pnpm_global_auth_ini_not_read_when_gate_disabled() {
    // The pnpm-NAMED GLOBAL `~/.config/pnpm/auth.ini` is gated by the
    // GLOBAL-scope `read_pnpm_global_config` posture — NOT the project-scope
    // `read_branded_pnpm_config` (asymmetric read/write model, 2026-06-20): a
    // GLOBAL file has no project incumbent, so its read must not ride the
    // cwd-derived project gate. With the global gate OFF the auth.ini token is
    // never applied (the `~/.npmrc` user source is untouched, so the stale
    // npmrc token survives); with it ON (the upstream default), auth.ini wins.
    // Restores the gate to the default (`true`) inside the lock so other
    // auth.ini tests see the normal behavior.
    let _gate = AUTH_INI_GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home_dir = tempfile::tempdir().unwrap();
    let proj_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        home_dir.path().join(".npmrc"),
        "//registry.example.com/:_authToken=npmrc-token\n",
    )
    .unwrap();
    // Per-OS config dir (no XDG override), so the fixture matches where
    // `pnpm_global_auth_ini_path` looks on the test host.
    let auth_ini = aube_util::env::pnpm_config_dir_with(Some(home_dir.path()), None)
        .unwrap()
        .join("auth.ini");
    std::fs::create_dir_all(auth_ini.parent().unwrap()).unwrap();
    std::fs::write(
        &auth_ini,
        "//registry.example.com/:_authToken=auth-ini-token\n",
    )
    .unwrap();

    aube_util::update_engine_context(|ctx| ctx.read_pnpm_global_config = false);
    let disabled = load_npmrc_entries_with_home(Some(home_dir.path()), None, proj_dir.path(), None);
    aube_util::update_engine_context(|ctx| ctx.read_pnpm_global_config = true);

    let mut cfg = NpmConfig::default();
    cfg.apply(disabled);
    assert_eq!(
        cfg.auth_token_for("https://registry.example.com/"),
        Some("npmrc-token"),
        "auth.ini token must not be applied when the global pnpm gate is off",
    );

    // The other direction in the same fixture: with the global gate ON (the
    // upstream default), auth.ini is read and its token wins over ~/.npmrc —
    // and this holds INDEPENDENT of the project gate (here left at its
    // default), the whole point of decoupling global from cwd incumbency.
    let enabled = load_npmrc_entries_with_home(Some(home_dir.path()), None, proj_dir.path(), None);
    let mut cfg = NpmConfig::default();
    cfg.apply(enabled);
    assert_eq!(
        cfg.auth_token_for("https://registry.example.com/"),
        Some("auth-ini-token"),
        "with the global gate on, pnpm auth.ini is read and overrides ~/.npmrc",
    );
}

#[test]
fn npmrc_auth_file_overrides_user_token() {
    // The whole point of `npmrcAuthFile`: a token declared in the
    // out-of-tree auth file must beat the same token in `~/.npmrc`,
    // so CI can mount a secret-bearing file at a fixed path and
    // know it wins regardless of any leftover entries in user rc.
    let home_dir = tempfile::tempdir().unwrap();
    let proj_dir = tempfile::tempdir().unwrap();
    let auth_file = proj_dir.path().join("auth.npmrc");

    std::fs::write(
        home_dir.path().join(".npmrc"),
        "//registry.example.com/:_authToken=stale-user-token\n",
    )
    .unwrap();
    std::fs::write(
        &auth_file,
        "//registry.example.com/:_authToken=fresh-from-auth-file\n",
    )
    .unwrap();
    std::fs::write(
        proj_dir.path().join(".npmrc"),
        format!("npmrc-auth-file={}\n", auth_file.display()),
    )
    .unwrap();

    let entries = load_npmrc_entries_with_home(Some(home_dir.path()), None, proj_dir.path(), None);
    let mut cfg = NpmConfig::default();
    cfg.apply(entries);
    assert_eq!(
        cfg.auth_token_for("https://registry.example.com/"),
        Some("fresh-from-auth-file"),
    );
}

#[test]
fn npmrc_auth_file_resolves_relative_to_project_root() {
    // A relative `npmrc-auth-file` path should resolve against the
    // project root, NOT the cwd of the test runner — same convention
    // as the storeDir setting.
    let home_dir = tempfile::tempdir().unwrap();
    let proj_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(proj_dir.path().join("secrets")).unwrap();
    std::fs::write(
        proj_dir.path().join("secrets/npm"),
        "//registry.example.com/:_authToken=relative-path-token\n",
    )
    .unwrap();
    std::fs::write(
        proj_dir.path().join(".npmrc"),
        "npmrc-auth-file=secrets/npm\n",
    )
    .unwrap();

    let entries = load_npmrc_entries_with_home(Some(home_dir.path()), None, proj_dir.path(), None);
    assert!(
        entries
            .iter()
            .any(|(k, v)| k == "//registry.example.com/:_authToken" && v == "relative-path-token"),
        "auth file entries missing — got {entries:?}",
    );
}

#[test]
fn npmrc_auth_file_camel_case_alias_works() {
    // The kebab-case spelling is exercised by the other tests; pin
    // the camelCase alias separately so a future tweak to the
    // `matches!` arm can't silently drop one of the spellings.
    let home_dir = tempfile::tempdir().unwrap();
    let proj_dir = tempfile::tempdir().unwrap();
    let auth_file = proj_dir.path().join("auth.npmrc");

    std::fs::write(
        &auth_file,
        "//registry.example.com/:_authToken=camel-token\n",
    )
    .unwrap();
    std::fs::write(
        proj_dir.path().join(".npmrc"),
        format!("npmrcAuthFile={}\n", auth_file.display()),
    )
    .unwrap();

    let entries = load_npmrc_entries_with_home(Some(home_dir.path()), None, proj_dir.path(), None);
    assert!(
        entries
            .iter()
            .any(|(k, v)| k == "//registry.example.com/:_authToken" && v == "camel-token"),
        "camelCase alias did not load auth file — got {entries:?}",
    );
}

#[test]
fn npmrc_auth_file_expands_tilde_against_home() {
    // `~/secrets/npm` should expand to `<home>/secrets/npm`, mirroring
    // the storeDir / pnpm convention.
    let home_dir = tempfile::tempdir().unwrap();
    let proj_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home_dir.path().join("secrets")).unwrap();
    std::fs::write(
        home_dir.path().join("secrets/npm"),
        "//registry.example.com/:_authToken=tilde-token\n",
    )
    .unwrap();
    std::fs::write(
        proj_dir.path().join(".npmrc"),
        "npmrc-auth-file=~/secrets/npm\n",
    )
    .unwrap();

    let entries = load_npmrc_entries_with_home(Some(home_dir.path()), None, proj_dir.path(), None);
    assert!(
        entries
            .iter()
            .any(|(k, v)| k == "//registry.example.com/:_authToken" && v == "tilde-token"),
        "tilde expansion failed — got {entries:?}",
    );
}

#[test]
fn userconfig_override_replaces_default_user_npmrc() {
    // `NPM_CONFIG_USERCONFIG` moves the user rc off the default
    // `$HOME/.npmrc` (XDG setups, CI secret mounts, etc.). When
    // the override is set, the default path must be skipped
    // entirely — matching npm/pnpm, which treat the env var as
    // "this is the user rc," not "also read it on top of the
    // default."
    let home_dir = tempfile::tempdir().unwrap();
    let proj_dir = tempfile::tempdir().unwrap();
    let override_dir = tempfile::tempdir().unwrap();
    let override_rc = override_dir.path().join("npmrc");

    // Decoy at the default location — must NOT be loaded.
    std::fs::write(
        home_dir.path().join(".npmrc"),
        "registry=https://decoy.example/\n",
    )
    .unwrap();
    std::fs::write(&override_rc, "registry=https://override.example/\n").unwrap();

    let entries = load_npmrc_entries_with_home(
        Some(home_dir.path()),
        None,
        proj_dir.path(),
        Some(&override_rc),
    );
    assert!(
        entries
            .iter()
            .any(|(k, v)| k == "registry" && v == "https://override.example/"),
        "override file was not loaded — got {entries:?}",
    );
    assert!(
        !entries.iter().any(|(_, v)| v == "https://decoy.example/"),
        "default ~/.npmrc must be skipped when override is set — got {entries:?}",
    );
}

#[test]
fn expand_userconfig_path_handles_tilde_absolute_and_empty() {
    let home = PathBuf::from("/fake/home");
    assert_eq!(
        expand_userconfig_path("~/config/npm/npmrc", Some(&home)),
        Some(PathBuf::from("/fake/home/config/npm/npmrc"))
    );
    assert_eq!(
        expand_userconfig_path("~", Some(&home)),
        Some(PathBuf::from("/fake/home"))
    );
    // Absolute paths pass through unchanged; tilde without a home
    // can't resolve, so callers see `None` and skip the load.
    assert_eq!(
        expand_userconfig_path("/etc/npmrc", Some(&home)),
        Some(PathBuf::from("/etc/npmrc"))
    );
    assert_eq!(expand_userconfig_path("~/x", None), None);
    // Trimmed-empty values are rejected so an accidentally-empty
    // export doesn't probe the process cwd.
    assert_eq!(expand_userconfig_path("", Some(&home)), None);
    assert_eq!(expand_userconfig_path("   ", Some(&home)), None);
}

#[test]
fn userconfig_override_from_env_prefers_screaming_casing() {
    // npm documents both `NPM_CONFIG_USERCONFIG` and the
    // lowercase form. We match on either so a shell that exports
    // the lowercase variant (direnv, mise, etc.) still relocates
    // the user rc.
    let home = PathBuf::from("/h");
    let upper = vec![(
        "NPM_CONFIG_USERCONFIG".to_string(),
        "/tmp/upper-rc".to_string(),
    )];
    assert_eq!(
        userconfig_override_from_env(&upper, Some(&home)),
        Some(PathBuf::from("/tmp/upper-rc"))
    );
    let lower = vec![(
        "npm_config_userconfig".to_string(),
        "/tmp/lower-rc".to_string(),
    )];
    assert_eq!(
        userconfig_override_from_env(&lower, Some(&home)),
        Some(PathBuf::from("/tmp/lower-rc"))
    );
    // Both set → the SCREAMING form wins regardless of slice
    // position. Positional ordering can't be the tiebreaker
    // because the production caller builds the slice from
    // `std::env::vars()`, which iterates in HashMap order.
    // Explicit casing precedence keeps the two public entry
    // points (`load_npmrc_entries` and `NpmConfig::load_with_env`)
    // from resolving to different files on the same host.
    let upper_first = vec![
        (
            "NPM_CONFIG_USERCONFIG".to_string(),
            "/tmp/upper".to_string(),
        ),
        (
            "npm_config_userconfig".to_string(),
            "/tmp/lower".to_string(),
        ),
    ];
    assert_eq!(
        userconfig_override_from_env(&upper_first, Some(&home)),
        Some(PathBuf::from("/tmp/upper")),
    );
    // Lowercase appearing first must not change the outcome.
    let lower_first = vec![
        (
            "npm_config_userconfig".to_string(),
            "/tmp/lower".to_string(),
        ),
        (
            "NPM_CONFIG_USERCONFIG".to_string(),
            "/tmp/upper".to_string(),
        ),
    ];
    assert_eq!(
        userconfig_override_from_env(&lower_first, Some(&home)),
        Some(PathBuf::from("/tmp/upper")),
        "SCREAMING form must win regardless of slice position",
    );
    // Nothing userconfig-shaped in the env → no override.
    let none_case = vec![("HOME".to_string(), "/h".to_string())];
    assert_eq!(userconfig_override_from_env(&none_case, Some(&home)), None);
}

#[test]
fn load_with_env_honors_npm_config_userconfig() {
    // End-to-end: set `NPM_CONFIG_USERCONFIG` in the captured env
    // slice and a token only present in the overridden file
    // should reach `auth_token_for`. Uses a test-specific host so
    // the developer's real `~/.npmrc` can't plausibly carry the
    // same key and skew the assertion.
    let proj_dir = tempfile::tempdir().unwrap();
    let override_dir = tempfile::tempdir().unwrap();
    let override_rc = override_dir.path().join("custom-npmrc");
    std::fs::write(
        &override_rc,
        "//userconfig-test.example/:_authToken=from-userconfig-file\n",
    )
    .unwrap();
    let env = vec![(
        "NPM_CONFIG_USERCONFIG".to_string(),
        override_rc.display().to_string(),
    )];
    let config = NpmConfig::load_with_env(proj_dir.path(), &env);
    assert_eq!(
        config.auth_token_for("https://userconfig-test.example/"),
        Some("from-userconfig-file"),
    );
}

#[test]
fn fetch_policy_default_matches_settings_toml_declared_defaults() {
    // `settings.toml` declares these defaults; `FetchPolicy::default`
    // must match them verbatim so callers that skip
    // `FetchPolicy::from_ctx` still get the same behavior.
    let p = FetchPolicy::default();
    assert_eq!(p.timeout_ms, 300_000);
    assert_eq!(p.retries, 2);
    assert_eq!(p.retry_factor, 10);
    assert_eq!(p.retry_min_timeout_ms, 10_000);
    assert_eq!(p.retry_max_timeout_ms, 60_000);
}

#[test]
fn fetch_policy_backoff_sequence_matches_make_fetch_happen() {
    // Defaults: min=10s, factor=10, max=60s. Sequence:
    //   attempt 1 → 10s  (10 * 10^0 = 10)
    //   attempt 2 → 60s  (10 * 10^1 = 100 → clamped to 60)
    //   attempt 3 → 60s  (10 * 10^2 = 1000 → clamped to 60)
    let p = FetchPolicy::default();
    assert_eq!(
        p.backoff_for_attempt(1),
        std::time::Duration::from_millis(10_000)
    );
    assert_eq!(
        p.backoff_for_attempt(2),
        std::time::Duration::from_millis(60_000)
    );
    assert_eq!(
        p.backoff_for_attempt(3),
        std::time::Duration::from_millis(60_000)
    );
}

#[test]
fn fetch_policy_backoff_clamps_on_huge_factor() {
    // Saturating math: even `factor=u32::MAX` doesn't panic; the
    // first retry hits the max ceiling and stays there.
    let p = FetchPolicy {
        timeout_ms: 60_000,
        retries: 5,
        retry_factor: u32::MAX,
        retry_min_timeout_ms: 100,
        retry_max_timeout_ms: 5_000,
        ..FetchPolicy::default()
    };
    assert_eq!(
        p.backoff_for_attempt(1),
        std::time::Duration::from_millis(100),
        "first attempt is the min (no multiplier applied yet)",
    );
    assert_eq!(
        p.backoff_for_attempt(2),
        std::time::Duration::from_millis(5_000),
    );
    assert_eq!(
        p.backoff_for_attempt(10),
        std::time::Duration::from_millis(5_000),
        "deep retries still clamp; no overflow panic",
    );
}

#[test]
fn fetch_policy_from_ctx_reads_npmrc_overrides() {
    // Full precedence chain is tested in `aube_settings`; this test
    // just proves the composite struct wires each field through to
    // the right generated accessor.
    let entries = vec![
        ("fetch-timeout".to_string(), "1234".to_string()),
        ("fetch-retries".to_string(), "5".to_string()),
        ("fetch-retry-factor".to_string(), "3".to_string()),
        ("fetch-retry-mintimeout".to_string(), "250".to_string()),
        ("fetch-retry-maxtimeout".to_string(), "9_999".to_string()),
    ];
    let ws: std::collections::BTreeMap<String, yaml_serde::Value> =
        std::collections::BTreeMap::new();
    let ctx = aube_settings::ResolveCtx::files_only(&entries, &ws);
    let p = FetchPolicy::from_ctx(&ctx);
    assert_eq!(p.timeout_ms, 1234);
    assert_eq!(p.retries, 5);
    assert_eq!(p.retry_factor, 3);
    assert_eq!(p.retry_min_timeout_ms, 250);
    // `9_999` with the underscore doesn't parse as u64 under the
    // generic `str::parse`; the accessor falls through to the
    // declared default. Assert that to lock the behavior.
    assert_eq!(p.retry_max_timeout_ms, 60_000);
}

#[test]
fn fetch_policy_from_ctx_reads_warn_timeout_and_min_speed() {
    // Pin the wiring for the two observability knobs. `from_ctx`
    // must route each through its generated accessor or a later
    // rename in the build script will silently fall back to the
    // declared default.
    let entries = vec![
        ("fetchWarnTimeoutMs".to_string(), "500".to_string()),
        ("fetchMinSpeedKiBps".to_string(), "123".to_string()),
    ];
    let ws: std::collections::BTreeMap<String, yaml_serde::Value> =
        std::collections::BTreeMap::new();
    let ctx = aube_settings::ResolveCtx::files_only(&entries, &ws);
    let p = FetchPolicy::from_ctx(&ctx);
    assert_eq!(p.warn_timeout_ms, 500);
    assert_eq!(p.min_speed_kibps, 123);
}

#[test]
fn fetch_policy_default_includes_observability_thresholds() {
    // Regression lock: the `settings.toml` defaults for the two
    // observability knobs (10s warn threshold, 50 KiB/s floor) must
    // remain reflected in `FetchPolicy::default()` so callers that
    // skip `from_ctx` still behave like a default-configured pnpm.
    let p = FetchPolicy::default();
    assert_eq!(p.warn_timeout_ms, 10_000);
    assert_eq!(p.min_speed_kibps, 50);
}

#[test]
fn translate_npm_config_env_maps_default_registry() {
    // Both the lowercase and SCREAMING_SNAKE spellings must land
    // on the canonical `.npmrc` key `registry`. The docs promise
    // `NPM_CONFIG_REGISTRY aube install` works; this is the hook
    // that makes it true.
    assert_eq!(
        translate_npm_config_env("NPM_CONFIG_REGISTRY", "https://r.example/"),
        Some(("registry".to_string(), "https://r.example/".to_string()))
    );
    assert_eq!(
        translate_npm_config_env("npm_config_registry", "https://r.example/"),
        Some(("registry".to_string(), "https://r.example/".to_string()))
    );
    // Non-npm env vars are ignored so the entry list stays tight
    // and `apply` isn't fed noise.
    assert_eq!(translate_npm_config_env("HOME", "/tmp"), None);
}

#[test]
fn translate_npm_config_env_maps_proxy_and_tls_knobs() {
    // Multi-word env suffix → hyphenated `.npmrc` key. Pins the
    // mapping for every registry-client knob that's exposed via
    // an env alias so future regressions show up as test
    // failures, not silent drops.
    let cases = [
        ("NPM_CONFIG_HTTPS_PROXY", "http://p:8", "https-proxy"),
        ("NPM_CONFIG_HTTP_PROXY", "http://p:9", "http-proxy"),
        ("NPM_CONFIG_PROXY", "http://p:0", "proxy"),
        ("NPM_CONFIG_NOPROXY", "localhost,.internal", "noproxy"),
        ("NPM_CONFIG_STRICT_SSL", "false", "strict-ssl"),
        ("NPM_CONFIG_LOCAL_ADDRESS", "127.0.0.1", "local-address"),
        ("NPM_CONFIG_MAXSOCKETS", "16", "maxsockets"),
    ];
    for (name, value, expected_key) in cases {
        assert_eq!(
            translate_npm_config_env(name, value),
            Some((expected_key.to_string(), value.to_string())),
            "mapping failed for {name}"
        );
    }
}

#[test]
fn translate_npm_config_env_maps_scoped_registry() {
    // `NPM_CONFIG_@MYORG:REGISTRY` should normalise to the
    // lowercase canonical `@myorg:registry` key that `apply`
    // matches via `strip_suffix(":registry")`.
    assert_eq!(
        translate_npm_config_env("NPM_CONFIG_@MYORG:REGISTRY", "https://r.mycorp/"),
        Some((
            "@myorg:registry".to_string(),
            "https://r.mycorp/".to_string()
        ))
    );
    assert_eq!(
        translate_npm_config_env("npm_config_@myorg:registry", "https://r.mycorp/"),
        Some((
            "@myorg:registry".to_string(),
            "https://r.mycorp/".to_string()
        ))
    );
}

#[test]
fn translate_npm_config_env_passes_uri_auth_through_verbatim() {
    // Per-URI auth keys carry `.npmrc` syntax in the env name.
    // Passthrough preserves the `_authToken` casing that `apply`
    // matches inside its `starts_with("//")` branch.
    assert_eq!(
        translate_npm_config_env(
            "NPM_CONFIG_//registry.example.com/:_authToken",
            "secret-token"
        ),
        Some((
            "//registry.example.com/:_authToken".to_string(),
            "secret-token".to_string()
        ))
    );
}

#[test]
fn translate_npm_config_env_passes_pnpm_uri_auth_through_verbatim() {
    assert_eq!(
        translate_npm_config_env(
            "PNPM_CONFIG_//registry.example.com/:_authToken",
            "secret-token"
        ),
        Some((
            "//registry.example.com/:_authToken".to_string(),
            "secret-token".to_string()
        ))
    );
}

#[test]
fn npm_config_env_entries_pnpm_uri_auth_wins_over_npm_uri_auth() {
    let entries = npm_config_env_entries_from(&[
        (
            "npm_config_//registry.example.com/:_authToken".to_string(),
            "npm-token".to_string(),
        ),
        (
            "pnpm_config_//registry.example.com/:_authToken".to_string(),
            "pnpm-token".to_string(),
        ),
    ]);
    let mut config = NpmConfig::default();
    config.apply(entries);
    assert_eq!(
        config.auth_token_for("https://registry.example.com/"),
        Some("pnpm-token")
    );
}

#[test]
fn translate_npm_config_env_ignores_uri_token_helper() {
    assert_eq!(
        translate_npm_config_env(
            "pnpm_config_//registry.example.com/:tokenHelper",
            "/tmp/helper"
        ),
        None
    );
}

/// Serializes the tests that flip the process-global
/// `engine_context().read_bun_config` gate so a disabled-window can't race a
/// concurrent load assuming the upstream default (off).
static BUN_CONFIG_GATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn bun_env_entries_map_registry_and_token() {
    // `BUN_CONFIG_REGISTRY` becomes the default `registry`; `BUN_CONFIG_TOKEN`
    // becomes an unscoped `_authToken` that `apply_tagged` pins to that
    // registry (mirrors Bun's `this.scope.token`). Mapping is a pure function
    // of the captured env — no `std::env` mutation.
    let entries = super::env::bun_env_entries_from(&[
        (
            "BUN_CONFIG_REGISTRY".to_string(),
            "https://bun.registry.example/".to_string(),
        ),
        ("BUN_CONFIG_TOKEN".to_string(), "bun-token".to_string()),
    ]);
    assert_eq!(
        entries,
        vec![
            (
                "registry".to_string(),
                "https://bun.registry.example/".to_string()
            ),
            ("_authToken".to_string(), "bun-token".to_string()),
        ]
    );
}

#[test]
fn bun_env_entries_ignore_non_http_registry_and_empty_values() {
    // Bun only accepts an `http(s)://` registry value and skips empty vars.
    let entries = super::env::bun_env_entries_from(&[
        (
            "BUN_CONFIG_REGISTRY".to_string(),
            "registry.example.com".to_string(),
        ),
        ("BUN_CONFIG_TOKEN".to_string(), String::new()),
    ]);
    assert!(entries.is_empty());
}

#[test]
fn load_with_env_bun_config_registry_and_token_pin_to_env_registry() {
    let _guard = BUN_CONFIG_GATE_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "registry=https://file.registry.example/\n",
    )
    .unwrap();
    let env = vec![
        (
            "BUN_CONFIG_REGISTRY".to_string(),
            "https://bun.registry.example/".to_string(),
        ),
        ("BUN_CONFIG_TOKEN".to_string(), "bun-token".to_string()),
    ];

    // Off (upstream default): Bun env vars are another tool's state, ignored.
    aube_util::update_engine_context(|ctx| ctx.read_bun_config = false);
    let disabled = NpmConfig::load_with_env(dir.path(), &env);
    assert_eq!(disabled.registry, "https://file.registry.example/");
    assert_eq!(
        disabled.auth_token_for("https://bun.registry.example/"),
        None
    );

    // On (Bun incumbent): `BUN_CONFIG_REGISTRY` wins over the project file and
    // the token pins to that registry.
    aube_util::update_engine_context(|ctx| ctx.read_bun_config = true);
    let enabled = NpmConfig::load_with_env(dir.path(), &env);
    aube_util::update_engine_context(|ctx| ctx.read_bun_config = false);

    assert_eq!(enabled.registry, "https://bun.registry.example/");
    assert_eq!(
        enabled.auth_token_for("https://bun.registry.example/"),
        Some("bun-token")
    );
}

#[test]
fn load_with_env_bun_config_registry_outranks_npm_config_registry() {
    // Bun checks `BUN_CONFIG_REGISTRY` before `NPM_CONFIG_REGISTRY`, so the
    // Bun spelling must win when both are present.
    let _guard = BUN_CONFIG_GATE_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let env = vec![
        (
            "NPM_CONFIG_REGISTRY".to_string(),
            "https://npm.registry.example/".to_string(),
        ),
        (
            "BUN_CONFIG_REGISTRY".to_string(),
            "https://bun.registry.example/".to_string(),
        ),
    ];

    aube_util::update_engine_context(|ctx| ctx.read_bun_config = true);
    let config = NpmConfig::load_with_env(dir.path(), &env);
    aube_util::update_engine_context(|ctx| ctx.read_bun_config = false);

    assert_eq!(config.registry, "https://bun.registry.example/");
}

#[test]
fn load_with_env_npm_config_registry_overrides_project_file() {
    // Integration-ish: `load_with_env` stitches file config and
    // env together. Project `.npmrc` sets one registry URL; the
    // captured env carries `NPM_CONFIG_REGISTRY` with another.
    // The env value must win so the code path a user exercises
    // with `NPM_CONFIG_REGISTRY=... aube install` really does
    // route traffic to the configured host.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "registry=https://file.registry.example/\n",
    )
    .unwrap();
    let env = vec![(
        "NPM_CONFIG_REGISTRY".to_string(),
        "https://env.registry.example/".to_string(),
    )];
    let config = NpmConfig::load_with_env(dir.path(), &env);
    assert_eq!(config.registry, "https://env.registry.example/");
}

#[test]
fn env_registry_overrides_project_npmrc() {
    // End-to-end: `apply` consumes the synthesised env entry last,
    // so a `NPM_CONFIG_REGISTRY` value beats whatever the project
    // `.npmrc` declares. This is the behaviour the user-facing
    // docs (`docs/package-manager/configuration.md`) guarantee.
    //
    // Driven through `apply` directly to avoid racing other tests
    // on the process-wide env (edition 2024 requires `unsafe` for
    // `set_var`, and the test harness runs cases in parallel).
    let mut config = NpmConfig {
        registry: "https://registry.npmjs.org/".to_string(),
        ..Default::default()
    };
    config.apply(vec![(
        "registry".to_string(),
        "https://file.registry/".to_string(),
    )]);
    assert_eq!(config.registry, "https://file.registry/");
    // Emulate the `load_npm_config_env_entries` output for
    // `NPM_CONFIG_REGISTRY=https://env.registry/`.
    let env = translate_npm_config_env("NPM_CONFIG_REGISTRY", "https://env.registry/")
        .map(|e| vec![e])
        .unwrap_or_default();
    config.apply(env);
    assert_eq!(
        config.registry, "https://env.registry/",
        "env var must override file-based registry"
    );
}

#[test]
fn is_public_npmjs_matches_canonical_and_normalised_urls() {
    // The same npmjs.org URL spelled different ways should all
    // resolve to "yes, public" — supply-chain gates skip on a
    // mismatch and we don't want capitalization / trailing slash
    // drift between npm, pnpm, and aube's own normalisation to
    // accidentally flip a public package into "private" mode.
    for url in [
        "https://registry.npmjs.org/",
        "https://registry.npmjs.org",
        "https://Registry.NPMJS.org/",
        "http://registry.npmjs.org/",
        "//registry.npmjs.org/",
        // URI schemes are case-insensitive per RFC 3986. A
        // user-typed `HTTPS://...` in `.npmrc` must not silently
        // fall through and disable the supply-chain gates.
        "HTTPS://registry.npmjs.org/",
        "Http://registry.npmjs.org/",
    ] {
        assert!(is_public_npmjs_url(url), "expected public for {url}");
    }
}

#[test]
fn is_public_npmjs_rejects_private_and_mirror_urls() {
    // Anything other than the canonical npmjs host counts as
    // private from the gate's perspective: mirrors, internal
    // Verdaccio installs, Artifactory proxies, and unrelated
    // hosts that happen to embed `npmjs.org` as a subpath.
    for url in [
        "https://internal.example.com/",
        "https://npm.pkg.github.com/",
        "https://registry.yarnpkg.com/",
        "https://example.com/registry.npmjs.org/",
        "ftp://registry.npmjs.org/",
    ] {
        assert!(!is_public_npmjs_url(url), "expected private for {url}");
    }
}

#[test]
fn is_public_npmjs_does_not_panic_on_multibyte_char_at_prefix_boundary() {
    // `https:/ñ...` — the `ñ` straddles byte offset 8 (the
    // length of `"https://"`). A naive `split_at(8)` would
    // panic; the helper has to use `split_at_checked` and
    // return `None` so `aube add` doesn't crash on a typo'd
    // `.npmrc` value.
    assert!(!is_public_npmjs_url("https:/ñregistry.npmjs.org/"));
    // Also exercise a multi-byte char inside what looks like a
    // valid scheme — we should reject this as non-public
    // without crashing.
    assert!(!is_public_npmjs_url("htñps://registry.npmjs.org/"));
}

#[test]
fn is_public_npmjs_via_npm_config_uses_scope_override() {
    // A scoped registry override flips the per-package answer
    // even though the default registry is still npmjs. Verifies
    // the gate filter respects scope→registry mapping rather
    // than only looking at the global `registry=` field.
    let mut cfg = NpmConfig {
        registry: "https://registry.npmjs.org/".to_string(),
        ..Default::default()
    };
    cfg.scoped_registries.insert(
        "@myorg".to_string(),
        "https://npm.internal.example/".to_string(),
    );
    assert!(cfg.is_public_npmjs("lodash"));
    assert!(!cfg.is_public_npmjs("@myorg/utils"));
}

#[test]
fn fetch_policy_clamps_giant_retry_counts_into_u32() {
    // A user writing `fetch-retries=99999999999` should not panic;
    // the retry loop just caps at u32::MAX attempts.
    let entries = vec![("fetch-retries".to_string(), "99999999999999".to_string())];
    let ws: std::collections::BTreeMap<String, yaml_serde::Value> =
        std::collections::BTreeMap::new();
    let ctx = aube_settings::ResolveCtx::files_only(&entries, &ws);
    let p = FetchPolicy::from_ctx(&ctx);
    assert_eq!(p.retries, u32::MAX);
}

#[test]
fn scoped_auth_token_overrides_registry_token_for_matching_scope() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "registry=https://npm.pkg.github.com/\n\
         //npm.pkg.github.com/:_authToken=registry-token\n\
         //npm.pkg.github.com/:@org-a:_authToken=org-a-token\n",
    )
    .unwrap();

    let config = NpmConfig::load_isolated(dir.path());

    assert_eq!(
        config.auth_token_for_package("https://npm.pkg.github.com/", "@org-a/pkg"),
        Some("org-a-token")
    );
    assert_eq!(
        config.auth_token_for_package("https://npm.pkg.github.com/", "@org-b/pkg"),
        Some("registry-token")
    );
}

#[test]
fn scoped_tls_config_does_not_shadow_registry_token() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "registry=https://npm.pkg.github.com/\n\
         //npm.pkg.github.com/:_authToken=registry-token\n\
         //npm.pkg.github.com/:@org-a:ca=-----BEGIN CERTIFICATE-----\\nca\\n-----END CERTIFICATE-----\n",
    )
    .unwrap();

    let config = NpmConfig::load_isolated(dir.path());

    assert_eq!(
        config.auth_token_for_package("https://npm.pkg.github.com/", "@org-a/pkg"),
        Some("registry-token")
    );
}

#[test]
fn longer_scoped_tls_config_does_not_shadow_shorter_scoped_token() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "registry=https://registry.example.com/npm/\n\
         //registry.example.com/:_authToken=registry-token\n\
         //registry.example.com/:@org-a:_authToken=org-a-token\n\
         //registry.example.com/npm/:@org-a:cafile=/etc/ssl/org-a.pem\n",
    )
    .unwrap();

    let config = NpmConfig::load_isolated(dir.path());

    assert_eq!(
        config.auth_token_for_package(
            "https://registry.example.com/npm/@org-a/pkg/-/pkg-1.0.0.tgz",
            "@org-a/pkg"
        ),
        Some("org-a-token")
    );
}

#[test]
fn scoped_auth_token_honors_path_scoped_registry_prefix() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "@org-a:registry=https://registry.example.com/npm/\n\
         //registry.example.com/npm/:@org-a:_authToken=org-a-token\n\
         //registry.example.com/:_authToken=root-token\n",
    )
    .unwrap();

    let config = NpmConfig::load_isolated(dir.path());

    assert_eq!(
        config.auth_token_for_package(
            "https://registry.example.com/npm/@org-a/pkg/-/pkg-1.0.0.tgz",
            "@org-a/pkg"
        ),
        Some("org-a-token")
    );
    assert_eq!(
        config.auth_token_for_package(
            "https://registry.example.com/other/@org-a/pkg/-/pkg-1.0.0.tgz",
            "@org-a/pkg"
        ),
        Some("root-token")
    );
}

#[test]
fn scoped_auth_token_requires_path_prefix_boundary() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "//registry.example.com/npm:@org-a:_authToken=org-a-token\n\
         //registry.example.com/:_authToken=root-token\n",
    )
    .unwrap();

    let config = NpmConfig::load_isolated(dir.path());

    assert_eq!(
        config.auth_token_for_package(
            "https://registry.example.com/npm/@org-a/pkg/-/pkg-1.0.0.tgz",
            "@org-a/pkg"
        ),
        Some("org-a-token")
    );
    assert_eq!(
        config.auth_token_for_package(
            "https://registry.example.com/npm-release/@org-a/pkg/-/pkg-1.0.0.tgz",
            "@org-a/pkg"
        ),
        Some("root-token")
    );
}

#[test]
fn scoped_auth_lookup_checks_shorter_prefixes_before_unscoped_fallback() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "//registry.example.com/npm/private/:_authToken=private-root-token\n\
         //registry.example.com/npm/:@org-a:_authToken=org-a-token\n",
    )
    .unwrap();

    let config = NpmConfig::load_isolated(dir.path());

    assert_eq!(
        config.auth_token_for_package(
            "https://registry.example.com/npm/private/@org-a/pkg/-/pkg-1.0.0.tgz",
            "@org-a/pkg"
        ),
        Some("org-a-token")
    );
}

#[test]
fn home_npmrc_is_user_trusted_when_cwd_is_home_not_a_committed_project_source() {
    // Regression: when the project dir IS the home dir, `~/.npmrc` was
    // read TWICE — once as User (trusted) and once as Project (untrusted,
    // via `project_dir.join(".npmrc")` resolving to the same file) — so
    // its `https-proxy` / `strict-ssl=false` tripped the
    // untrusted-committed-`.npmrc` gate. The user's own global config must
    // stay user-trusted regardless of cwd.
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".npmrc"),
        "https-proxy=http://proxy.example:8080/\nstrict-ssl=false\n",
    )
    .unwrap();

    // No Project-tagged entries should come out — the same-file read is
    // deduped, not re-tagged as an untrusted committed source.
    let tagged = load_npmrc_entries_tagged_with_home(Some(home.path()), None, home.path(), None);
    assert!(
        !tagged
            .iter()
            .any(|(src, _, _)| matches!(src, NpmrcSource::Project)),
        "~/.npmrc read with cwd==home must not be tagged Project (untrusted): {tagged:?}",
    );

    // And the user-trusted proxy / strict-ssl settings actually apply.
    let mut config = NpmConfig::default();
    config.apply_tagged(tagged);
    assert_eq!(
        config.https_proxy.as_deref(),
        Some("http://proxy.example:8080/"),
        "user ~/.npmrc https-proxy must apply from $HOME (not refused as untrusted)",
    );
    assert!(
        !config.strict_ssl,
        "user ~/.npmrc strict-ssl=false must apply from $HOME (not refused as untrusted)",
    );
}

#[test]
fn genuine_committed_project_npmrc_in_nested_dir_stays_untrusted() {
    // Counter-case proving the security gate is intact: a real committed
    // `.npmrc` in a nested project dir (a different file from `~/.npmrc`)
    // is still classified Project/untrusted, so its proxy / strict-ssl
    // disable are refused.
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".npmrc"),
        "https-proxy=http://evil.example:8080/\nstrict-ssl=false\n",
    )
    .unwrap();

    let tagged = load_npmrc_entries_tagged_with_home(Some(home.path()), None, project.path(), None);
    assert!(
        tagged
            .iter()
            .any(|(src, _, _)| matches!(src, NpmrcSource::Project)),
        "a genuine committed project .npmrc must be tagged Project (untrusted): {tagged:?}",
    );

    let mut config = NpmConfig::default();
    config.apply_tagged(tagged);
    assert_eq!(
        config.https_proxy, None,
        "committed project .npmrc must not set an https-proxy (untrusted)",
    );
    assert!(
        config.strict_ssl,
        "committed project .npmrc must not disable strict-ssl (untrusted)",
    );
}

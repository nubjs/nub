use super::*;
use aube_registry::{Dist, Packument, VersionMetadata};
use miette::Diagnostic;

#[test]
fn no_match_help_renders_context() {
    let err = Error::NoMatch(Box::new(NoMatchDetails {
        name: "bisection".into(),
        range: "^9.9.9".into(),
        importer: "packages/app".into(),
        ancestors: vec![("parent-pkg".into(), "1.2.3".into())],
        original_spec: Some("catalog:evens".into()),
        available: vec!["1.0.1".into(), "1.0.0".into(), "0.1.0".into()],
        total_versions: 3,
        only_prereleases: false,
    }));
    let help = err.help().expect("help set").to_string();
    assert!(help.contains("importer: packages/app"));
    assert!(help.contains("chain: parent-pkg@1.2.3 > bisection"));
    assert!(help.contains("original spec: `catalog:evens`"));
    assert!(help.contains("available versions: 1.0.1, 1.0.0, 0.1.0"));
}

#[test]
fn no_match_help_flags_empty_packument() {
    let err = Error::NoMatch(Box::new(NoMatchDetails {
        name: "ghost".into(),
        range: "^1".into(),
        importer: ".".into(),
        ancestors: vec![],
        original_spec: None,
        available: vec![],
        total_versions: 0,
        only_prereleases: false,
    }));
    let help = err.help().expect("help set").to_string();
    assert!(help.contains("packument has no versions"));
    assert!(!help.contains("importer:"));
}

#[test]
fn no_match_help_flags_prerelease_only_packument() {
    let err = Error::NoMatch(Box::new(NoMatchDetails {
        name: "bleeding".into(),
        range: "^1".into(),
        importer: ".".into(),
        ancestors: vec![],
        original_spec: None,
        available: vec!["2.0.0-rc.3".into(), "2.0.0-rc.2".into()],
        total_versions: 2,
        only_prereleases: true,
    }));
    let help = err.help().expect("help set").to_string();
    assert!(help.contains("no stable versions published"));
    assert!(help.contains("2.0.0-rc.3"));
    assert!(help.contains("bleeding@2.0.0-rc.3"));
    assert!(help.contains("`next` dist-tag"));
}

#[test]
fn build_age_gate_resolves_dist_tag_range() {
    let packument = make_packument("foo", &["1.0.0", "2.0.0", "3.0.0"], "3.0.0");
    let task = ResolveTask {
        name: "foo".into(),
        range: "latest".into(),
        dep_type: DepType::Production,
        is_root: true,
        parent: None,
        importer: ".".into(),
        original_specifier: None,
        real_name: None,
        ancestors: Vec::new(),
        range_from_override: false,
    };
    let d = build_age_gate(&task, &packument, 60);
    // `latest` → 3.0.0; the exact-version range only matches 3.0.0.
    assert_eq!(d.gated, vec!["3.0.0".to_string()]);
}

#[test]
fn build_no_match_falls_back_to_prereleases() {
    let packument = make_packument(
        "alpha",
        &["1.0.0-alpha.1", "1.0.0-alpha.2"],
        "1.0.0-alpha.2",
    );
    let task = ResolveTask {
        name: "alpha".into(),
        range: "^2".into(),
        dep_type: DepType::Production,
        is_root: true,
        parent: None,
        importer: ".".into(),
        original_specifier: None,
        real_name: None,
        ancestors: Vec::new(),
        range_from_override: false,
    };
    let d = build_no_match(&task, &packument);
    assert!(d.only_prereleases);
    assert_eq!(d.total_versions, 2);
    assert_eq!(
        d.available,
        vec!["1.0.0-alpha.2".to_string(), "1.0.0-alpha.1".to_string()]
    );
}

#[test]
fn classify_registry_error_is_case_insensitive() {
    assert!(matches!(
        classify_registry_error("fetch https://reg.example: HTTP 403"),
        RegistryErrorKind::Fetch
    ));
    assert!(matches!(
        classify_registry_error("fetch https://reg.example: http 403"),
        RegistryErrorKind::Fetch
    ));
    assert!(matches!(
        classify_registry_error("tarball https://x/y.tgz: Integrity mismatch"),
        RegistryErrorKind::Tarball
    ));
    assert!(matches!(
        classify_registry_error("readPackage hook: TypeError"),
        RegistryErrorKind::Hook
    ));
    assert!(matches!(
        classify_registry_error("READPACKAGE hook: error"),
        RegistryErrorKind::Hook
    ));
}

#[test]
fn classify_registry_error_prefers_hook_over_http_url() {
    // `readPackage hook:` messages can embed an HTTPS URL from the
    // hook's own error payload — must land in Hook, not Fetch.
    assert!(matches!(
        classify_registry_error(
            "readPackage hook: Error: failed to fetch https://internal.example/thing"
        ),
        RegistryErrorKind::Hook
    ));
    assert!(matches!(
        classify_registry_error("readPackage hook: TypeError: Cannot read property"),
        RegistryErrorKind::Hook
    ));
}

#[test]
fn unknown_catalog_entry_help_explains_chained_value() {
    // Chained-catalog case: the help path suggests a concrete semver
    // range instead of listing siblings (which would match the user's
    // own input and produce a bogus "did you mean `react`?").
    let err = Error::UnknownCatalogEntry(Box::new(CatalogDetails {
        name: "react".into(),
        spec: "catalog:".into(),
        catalog: "default (value catalog:other is itself a catalog: reference, catalogs \
                 cannot chain)"
            .into(),
        available: Vec::new(),
        chained_value: Some("catalog:other".into()),
    }));
    let help = err.help().expect("help set").to_string();
    assert!(!help.contains("did you mean"));
    assert!(!help.contains("is empty"));
    assert!(help.contains("catalogs cannot chain"));
    assert!(help.contains("catalog:other"));
    assert!(help.contains("concrete semver range"));
}

#[test]
fn classify_registry_error_prefers_git_over_http_url() {
    // `git resolve {range}: ...` with an https:// or git+https:// range
    // must land in Git, not Fetch — the substring `http` inside the URL
    // would otherwise steal it into the Fetch bucket.
    assert!(matches!(
        classify_registry_error("git resolve https://github.com/foo/bar.git#v1: auth failed"),
        RegistryErrorKind::Git
    ));
    assert!(matches!(
        classify_registry_error("git resolve git+https://host/x.git: ref not found"),
        RegistryErrorKind::Git
    ));
    assert!(matches!(
        classify_registry_error("git task panicked: join error"),
        RegistryErrorKind::Git
    ));
    assert!(matches!(
        classify_registry_error("git dep https://github.com/...: nested install failed"),
        RegistryErrorKind::Git
    ));
}

#[test]
fn age_gate_help_lists_gated_versions_and_bypass() {
    let err = Error::AgeGate(Box::new(AgeGateDetails {
        name: "lodash".into(),
        range: "^4".into(),
        minutes: 60,
        importer: "packages/app".into(),
        ancestors: vec![("parent".into(), "1.0.0".into())],
        gated: vec!["4.17.21".into(), "4.17.20".into()],
    }));
    let help = err.help().expect("help set").to_string();
    assert!(help.contains("importer: packages/app"));
    assert!(help.contains("chain: parent@1.0.0 > lodash"));
    assert!(help.contains("blocked by age gate: 4.17.21, 4.17.20"));
    assert!(help.contains("minimumReleaseAgeStrict=false"));
    assert!(help.contains("minimumReleaseAgeExclude"));
}

#[test]
fn registry_help_classifies_common_subtypes() {
    let tarball = format_registry_help("lodash", "tarball https://x/y.tgz: eof");
    assert!(tarball.contains("aube store prune"));
    let fetch = format_registry_help("lodash", "fetch https://registry.npmjs.org: 403");
    assert!(fetch.contains("registry URL"));
    let git = format_registry_help("some-pkg", "git resolve git+ssh://...: auth");
    assert!(git.contains("git dep"));
    let local = format_registry_help("pkg", "unparseable local specifier: file:../x");
    assert!(local.contains("local specifier"));
    let hook = format_registry_help("pkg", "readPackage hook: TypeError");
    assert!(hook.contains("readPackage"));
    let bug = format_registry_help("(resolver)", "3 transitives still deferred");
    assert!(bug.contains("report at"));
}

#[test]
fn unknown_catalog_help_lists_defined() {
    let err = Error::UnknownCatalog(Box::new(CatalogDetails {
        name: "react".into(),
        spec: "catalog:missing".into(),
        catalog: "missing".into(),
        available: vec!["default".into(), "evens".into()],
        chained_value: None,
    }));
    let help = err.help().expect("help set").to_string();
    assert!(help.contains("defined catalogs: default, evens"));
}

#[test]
fn unknown_catalog_help_when_none_defined() {
    let err = Error::UnknownCatalog(Box::new(CatalogDetails {
        name: "react".into(),
        spec: "catalog:".into(),
        catalog: "default".into(),
        available: vec![],
        chained_value: None,
    }));
    let help = err.help().expect("help set").to_string();
    assert!(help.contains("no catalogs are defined"));
}

#[test]
fn unknown_catalog_entry_help_suggests_similar() {
    let err = Error::UnknownCatalogEntry(Box::new(CatalogDetails {
        name: "reactt".into(),
        spec: "catalog:".into(),
        catalog: "default".into(),
        available: vec!["react".into(), "react-dom".into()],
        chained_value: None,
    }));
    let help = err.help().expect("help set").to_string();
    assert!(help.contains("defines: react, react-dom"));
    assert!(help.contains("did you mean `react`"));
}

#[test]
fn exotic_subdep_help_shows_chain_and_fix() {
    let err = Error::BlockedExoticSubdep(Box::new(ExoticSubdepDetails {
        name: "xlsx".into(),
        spec: "https://cdn.sheetjs.com/xlsx-0.20.3.tgz".into(),
        parent: "some-pkg@1.0.0".into(),
        ancestors: vec![("some-pkg".into(), "1.0.0".into())],
        importer: ".".into(),
    }));
    let help = err.help().expect("help set").to_string();
    assert!(help.contains("chain: some-pkg@1.0.0 > xlsx"));
    assert!(help.contains("pin `xlsx`"));
    assert!(help.contains("blockExoticSubdeps=false"));
}

#[test]
fn test_version_satisfies() {
    assert!(version_satisfies("4.17.21", "^4.17.0"));
    assert!(version_satisfies("4.17.21", "^4.0.0"));
    assert!(!version_satisfies("3.10.0", "^4.0.0"));
    assert!(version_satisfies("1.0.0", ">=1.0.0"));
    assert!(version_satisfies("2.0.0", ">=1.0.0 <3.0.0"));
}

#[test]
fn test_version_satisfies_exact() {
    assert!(version_satisfies("1.0.0", "1.0.0"));
    assert!(!version_satisfies("1.0.1", "1.0.0"));
}

#[test]
fn test_version_satisfies_tilde() {
    assert!(version_satisfies("1.2.3", "~1.2.0"));
    assert!(version_satisfies("1.2.9", "~1.2.0"));
    assert!(!version_satisfies("1.3.0", "~1.2.0"));
}

#[test]
fn test_version_satisfies_star() {
    assert!(version_satisfies("1.0.0", "*"));
    assert!(version_satisfies("99.99.99", "*"));
}

#[test]
fn test_version_satisfies_invalid() {
    assert!(!version_satisfies("notaversion", "^1.0.0"));
    assert!(!version_satisfies("1.0.0", "notarange"));
}

#[test]
fn test_version_satisfies_empty_range_is_any() {
    // `hashring@0.0.8` in the wild declares `"bisection": ""`.
    // npm / pnpm / yarn treat empty and whitespace-only ranges as
    // `"*"`; aube must match.
    assert!(version_satisfies("0.0.3", ""));
    assert!(version_satisfies("99.99.99", ""));
    assert!(version_satisfies("1.2.3", "   "));
}

#[test]
fn dependency_policy_default_blocks_exotic_subdeps() {
    assert!(DependencyPolicy::default().block_exotic_subdeps);
}

#[test]
fn exotic_subdeps_from_local_parents_are_allowed() {
    let task = ResolveTask {
        name: "xlsx".to_string(),
        range: "https://cdn.sheetjs.com/xlsx-0.20.3/xlsx-0.20.3.tgz".to_string(),
        dep_type: DepType::Production,
        is_root: false,
        parent: Some("pi-web-ui@file+abc123".to_string()),
        importer: ".".to_string(),
        original_specifier: None,
        real_name: None,
        ancestors: Vec::new(),
        range_from_override: false,
    };
    let mut resolved = BTreeMap::new();
    resolved.insert(
        "pi-web-ui@file+abc123".to_string(),
        LockedPackage {
            name: "pi-web-ui".to_string(),
            version: "0.68.1".to_string(),
            dep_path: "pi-web-ui@file+abc123".to_string(),
            local_source: Some(LocalSource::Directory(PathBuf::from("packages/web-ui"))),
            ..Default::default()
        },
    );

    assert!(!should_block_exotic_subdep(&task, &resolved, true));
}

#[test]
fn exotic_subdeps_from_unknown_parents_stay_blocked() {
    let task = ResolveTask {
        name: "xlsx".to_string(),
        range: "https://cdn.sheetjs.com/xlsx-0.20.3/xlsx-0.20.3.tgz".to_string(),
        dep_type: DepType::Production,
        is_root: false,
        parent: Some("pi-web-ui@file+missing".to_string()),
        importer: ".".to_string(),
        original_specifier: None,
        real_name: None,
        ancestors: Vec::new(),
        range_from_override: false,
    };

    assert!(should_block_exotic_subdep(&task, &BTreeMap::new(), true));
}

#[test]
fn exotic_subdeps_from_registry_parents_stay_blocked() {
    let task = ResolveTask {
        name: "xlsx".to_string(),
        range: "https://cdn.sheetjs.com/xlsx-0.20.3/xlsx-0.20.3.tgz".to_string(),
        dep_type: DepType::Production,
        is_root: false,
        parent: Some("pi-web-ui@0.68.1".to_string()),
        importer: ".".to_string(),
        original_specifier: None,
        real_name: None,
        ancestors: Vec::new(),
        range_from_override: false,
    };
    let mut resolved = BTreeMap::new();
    resolved.insert(
        "pi-web-ui@0.68.1".to_string(),
        LockedPackage {
            name: "pi-web-ui".to_string(),
            version: "0.68.1".to_string(),
            dep_path: "pi-web-ui@0.68.1".to_string(),
            ..Default::default()
        },
    );

    assert!(should_block_exotic_subdep(&task, &resolved, true));
}

#[test]
fn strip_alias_prefix_extracts_version_tail() {
    assert_eq!(strip_alias_prefix("npm:bar@1.2.3"), "1.2.3");
    assert_eq!(
        strip_alias_prefix("npm:@descript/immer@6.0.9-patched.1"),
        "6.0.9-patched.1"
    );
    assert_eq!(strip_alias_prefix("jsr:@std/fmt@1.0.0"), "1.0.0");
    assert_eq!(strip_alias_prefix("^1.2.3"), "^1.2.3");
    // Edge cases: alias without a version tail falls through.
    assert_eq!(strip_alias_prefix("npm:bar"), "bar");
    assert_eq!(strip_alias_prefix("jsr:^1.0.0"), "^1.0.0");
}

#[test]
fn pick_override_spec_respects_aliased_version_tail() {
    use override_rule::compile;
    // Override `immer@>=7.0.0 <9.0.6`, real dep is
    // `npm:@descript/immer@6.0.9-patched.1`. The version tail is
    // outside the selector's range, so the override must NOT fire
    // (pnpm parity). Regression for #174.
    let mut raw = BTreeMap::new();
    raw.insert("immer@>=7.0.0 <9.0.6".to_string(), "11.1.4".to_string());
    let rules = compile(&raw);
    assert_eq!(
        pick_override_spec(&rules, "immer", "npm:@descript/immer@6.0.9-patched.1", &[]),
        None,
    );
    // A matching version tail still fires.
    assert_eq!(
        pick_override_spec(&rules, "immer", "npm:@descript/immer@8.0.0", &[]),
        Some("11.1.4".to_string()),
    );
}

#[test]
fn package_extension_selector_matches_scoped_and_versioned_names() {
    assert!(package_selector_matches(
        "@scope/pkg@^1",
        "@scope/pkg",
        "1.2.3"
    ));
    assert!(package_selector_matches("plain", "plain", "9.0.0"));
    assert!(!package_selector_matches(
        "@scope/pkg@^2",
        "@scope/pkg",
        "1.2.3"
    ));
}

#[test]
fn package_extension_selector_matches_wildcard_on_unparseable_version() {
    // A `name@*` selector must match even when the version isn't valid
    // semver — git/tarball packages can carry odd version strings, and
    // `*` means "any version" regardless. Regression guard for
    // packageExtensions targeting git deps (e.g. injecting a runtime
    // connector into a github: package) being silently skipped.
    assert!(package_selector_matches("host@*", "host", "not-a-semver"));
    assert!(package_selector_matches("host@*", "host", "2.59.3"));
    assert!(package_selector_matches("host", "host", "whatever-ref"));
    assert!(!package_selector_matches("host@*", "other", "1.0.0"));
}

#[test]
fn package_extensions_inject_into_flat_dep_map() {
    // The non-registry (git/tarball/dir) resolve path carries a flat
    // `name -> range` dep map. A matching `@*` extension must inject its
    // `dependencies` so they get resolved + linked as siblings.
    let mut deps = BTreeMap::new();
    deps.insert("existing".to_string(), "1.0.0".to_string());
    let extension = PackageExtension {
        selector: "juggler@*".to_string(),
        dependencies: [
            (
                "connector".to_string(),
                "github:org/connector#abc".to_string(),
            ),
            ("existing".to_string(), "9.9.9".to_string()),
        ]
        .into_iter()
        .collect(),
        optional_dependencies: BTreeMap::new(),
        peer_dependencies: BTreeMap::new(),
        peer_dependencies_meta: BTreeMap::new(),
    };

    apply_package_extensions_to_deps("juggler", "0.0.0-git", &mut deps, &[extension]);

    // Injected dep is added...
    assert_eq!(deps.get("connector").unwrap(), "github:org/connector#abc");
    // ...but the package's own declared dep stays authoritative.
    assert_eq!(deps.get("existing").unwrap(), "1.0.0");
}

#[test]
fn package_extensions_skip_flat_dep_map_on_selector_mismatch() {
    let mut deps = BTreeMap::new();
    let extension = PackageExtension {
        selector: "other@*".to_string(),
        dependencies: [("connector".to_string(), "1.0.0".to_string())]
            .into_iter()
            .collect(),
        optional_dependencies: BTreeMap::new(),
        peer_dependencies: BTreeMap::new(),
        peer_dependencies_meta: BTreeMap::new(),
    };
    apply_package_extensions_to_deps("juggler", "1.0.0", &mut deps, &[extension]);
    assert!(deps.is_empty());
}

#[test]
fn package_extensions_merge_dependency_maps() {
    let mut pkg = make_version("host", "1.0.0");
    let extension = PackageExtension {
        selector: "host@1".to_string(),
        dependencies: [("missing".to_string(), "^2.0.0".to_string())]
            .into_iter()
            .collect(),
        optional_dependencies: BTreeMap::new(),
        peer_dependencies: [("peer".to_string(), "^3.0.0".to_string())]
            .into_iter()
            .collect(),
        peer_dependencies_meta: [(
            "peer".to_string(),
            aube_registry::PeerDepMeta { optional: true },
        )]
        .into_iter()
        .collect(),
    };

    apply_package_extensions(&mut pkg, &[extension]);

    assert_eq!(pkg.dependencies.get("missing").unwrap(), "^2.0.0");
    assert_eq!(pkg.peer_dependencies.get("peer").unwrap(), "^3.0.0");
    assert!(pkg.peer_dependencies_meta.get("peer").unwrap().optional);
}

#[test]
fn package_extensions_do_not_overwrite_existing_dependency_maps() {
    let mut pkg = make_version("host", "1.0.0");
    pkg.dependencies
        .insert("dep".to_string(), "^1.0.0".to_string());
    pkg.optional_dependencies
        .insert("optional".to_string(), "^2.0.0".to_string());
    pkg.peer_dependencies
        .insert("peer".to_string(), "^3.0.0".to_string());
    pkg.peer_dependencies_meta.insert(
        "peer".to_string(),
        aube_registry::PeerDepMeta { optional: false },
    );

    let extension = PackageExtension {
        selector: "host".to_string(),
        dependencies: [
            ("dep".to_string(), "^9.0.0".to_string()),
            ("missing".to_string(), "^4.0.0".to_string()),
        ]
        .into_iter()
        .collect(),
        optional_dependencies: [
            ("optional".to_string(), "^9.0.0".to_string()),
            ("missing-optional".to_string(), "^5.0.0".to_string()),
        ]
        .into_iter()
        .collect(),
        peer_dependencies: [
            ("peer".to_string(), "^9.0.0".to_string()),
            ("missing-peer".to_string(), "^6.0.0".to_string()),
        ]
        .into_iter()
        .collect(),
        peer_dependencies_meta: [
            (
                "peer".to_string(),
                aube_registry::PeerDepMeta { optional: true },
            ),
            (
                "missing-peer".to_string(),
                aube_registry::PeerDepMeta { optional: true },
            ),
        ]
        .into_iter()
        .collect(),
    };

    apply_package_extensions(&mut pkg, &[extension]);

    assert_eq!(pkg.dependencies.get("dep").unwrap(), "^1.0.0");
    assert_eq!(pkg.dependencies.get("missing").unwrap(), "^4.0.0");
    assert_eq!(pkg.optional_dependencies.get("optional").unwrap(), "^2.0.0");
    assert_eq!(
        pkg.optional_dependencies.get("missing-optional").unwrap(),
        "^5.0.0"
    );
    assert_eq!(pkg.peer_dependencies.get("peer").unwrap(), "^3.0.0");
    assert_eq!(pkg.peer_dependencies.get("missing-peer").unwrap(), "^6.0.0");
    assert!(!pkg.peer_dependencies_meta.get("peer").unwrap().optional);
    assert!(
        pkg.peer_dependencies_meta
            .get("missing-peer")
            .unwrap()
            .optional
    );
}

#[test]
fn allowed_deprecated_versions_match_package_ranges() {
    let allowed = [("old".to_string(), "<2".to_string())]
        .into_iter()
        .collect();

    assert!(is_deprecation_allowed("old", "1.9.0", &allowed));
    assert!(!is_deprecation_allowed("old", "2.0.0", &allowed));
    assert!(!is_deprecation_allowed("other", "1.0.0", &allowed));
}

#[test]
fn test_dep_path_for() {
    assert_eq!(dep_path_for("lodash", "4.17.21"), "lodash@4.17.21");
    assert_eq!(dep_path_for("@babel/core", "7.24.0"), "@babel/core@7.24.0");
}

fn make_version(name: &str, version: &str) -> VersionMetadata {
    VersionMetadata {
        name: name.to_string(),
        version: version.to_string(),
        dependencies: BTreeMap::new(),
        dev_dependencies: BTreeMap::new(),
        peer_dependencies: BTreeMap::new(),
        peer_dependencies_meta: BTreeMap::new(),
        optional_dependencies: BTreeMap::new(),
        bundled_dependencies: None,
        dist: Some(Dist {
            tarball: format!("https://registry.npmjs.org/{name}/-/{name}-{version}.tgz"),
            integrity: Some(format!("sha512-fake-{name}-{version}")),
            shasum: None,
            unpacked_size: None,
            attestations: None,
        }),
        os: vec![],
        cpu: vec![],
        libc: vec![],
        engines: BTreeMap::new(),
        license: None,
        funding_url: None,
        bin: BTreeMap::new(),
        has_install_script: false,
        deprecated: None,
        approver: None,
        npm_user: None,
    }
}

fn make_packument(name: &str, versions: &[&str], latest: &str) -> Packument {
    let mut ver_map = BTreeMap::new();
    for v in versions {
        ver_map.insert(v.to_string(), make_version(name, v));
    }
    let mut dist_tags = BTreeMap::new();
    dist_tags.insert("latest".to_string(), latest.to_string());
    Packument {
        name: name.to_string(),
        modified: None,
        versions: ver_map,
        dist_tags,
        time: BTreeMap::new(),
    }
}

#[test]
fn test_pick_version_highest_match() {
    // `latest=2.0.0` does NOT satisfy `^1.0.0` (`<2.0.0`), so the
    // dist-tag preference doesn't apply and we fall through to the
    // strictly-highest version inside the range — 1.2.0.
    let packument = make_packument("foo", &["1.0.0", "1.1.0", "1.2.0", "2.0.0"], "2.0.0");
    let result = pick_version(&packument, "^1.0.0", None, false, None, false).unwrap();
    assert_eq!(result.version, "1.2.0");
}

#[test]
fn test_pick_version_prefers_dist_tag_latest_when_in_range() {
    // npm/pnpm parity: when `dist-tags.latest` falls inside the
    // user's range, return the publisher's tagged build instead of
    // the highest version — the publisher used `latest` to anchor
    // the canonical install; a stray higher version inside the
    // range (hotfix on an old line, withdrawn experimental publish,
    // mid-rollback intermediary) shouldn't silently win.
    //
    // Regression for the pnpm_update.bats `add_dist_tag latest 100.0.0
    // -> aube add foo@^100.0.0` flow, which expects the lockfile to
    // pin 100.0.0 even though 100.1.0 is available.
    let packument = make_packument("foo", &["1.0.0", "1.1.0", "1.2.0"], "1.0.0");
    let result = pick_version(&packument, "^1.0.0", None, false, None, false).unwrap();
    assert_eq!(result.version, "1.0.0");
}

#[test]
fn test_pick_version_falls_through_when_latest_outside_range() {
    // `latest=2.0.0` is outside the user's `^1.0.0`, so the dist-tag
    // preference is a no-op; the strictly-highest matching version
    // (1.1.0) wins.
    let packument = make_packument("foo", &["1.0.0", "1.1.0", "2.0.0"], "2.0.0");
    let result = pick_version(&packument, "^1.0.0", None, false, None, false).unwrap();
    assert_eq!(result.version, "1.1.0");
}

#[test]
fn test_pick_version_lowest_ignores_dist_tag_preference() {
    // TimeBased mode (`pick_lowest=true`) wants the floor of the
    // range, not whatever the publisher tagged latest. Confirm the
    // dist-tag preference is suppressed when pick_lowest is set.
    let packument = make_packument("foo", &["1.0.0", "1.1.0", "1.2.0"], "1.2.0");
    let result = pick_version(&packument, "^1.0.0", None, true, None, false).unwrap();
    assert_eq!(result.version, "1.0.0");
}

#[test]
fn test_pick_version_exact() {
    let packument = make_packument("foo", &["1.0.0", "1.1.0"], "1.1.0");
    let result = pick_version(&packument, "1.0.0", None, false, None, false).unwrap();
    assert_eq!(result.version, "1.0.0");
}

#[test]
fn test_pick_version_no_match() {
    let packument = make_packument("foo", &["1.0.0", "1.1.0"], "1.1.0");
    let result = pick_version(&packument, "^2.0.0", None, false, None, false);
    assert!(matches!(result, PickResult::NoMatch));
}

#[test]
fn test_pick_version_strict_distinguishes_age_gate_from_no_match() {
    // A version satisfies the range but is filtered by the cutoff.
    // Strict mode should report `AgeGated`, not `NoMatch`, so the
    // caller can surface a meaningful error message.
    let mut packument = make_packument("foo", &["1.0.0", "1.1.0"], "1.1.0");
    packument
        .time
        .insert("1.0.0".into(), "2024-01-01T00:00:00.000Z".into());
    packument
        .time
        .insert("1.1.0".into(), "2024-06-01T00:00:00.000Z".into());
    let cutoff = "2020-01-01T00:00:00.000Z";
    let result = pick_version(&packument, "^1.0.0", None, false, Some(cutoff), true);
    assert!(matches!(result, PickResult::AgeGated));

    // No version satisfies the range at all → still NoMatch even
    // in strict mode.
    let result = pick_version(&packument, "^9.0.0", None, false, Some(cutoff), true);
    assert!(matches!(result, PickResult::NoMatch));
}

#[test]
fn test_pick_version_prefers_locked() {
    let packument = make_packument("foo", &["1.0.0", "1.1.0", "1.2.0"], "1.2.0");
    let result = pick_version(&packument, "^1.0.0", Some("1.1.0"), false, None, false).unwrap();
    assert_eq!(result.version, "1.1.0");
}

#[test]
fn classify_regime_hard_frozen_when_higher_minor_in_same_major() {
    // Picking 1.0.0 while 1.1.0 / 1.2.0 exist: the same-major line has
    // moved on, so history below is settled — a refetch can't surface a
    // newer satisfying answer for `~1.0`.
    use crate::semver_util::{Regime, classify_regime};
    let packument = make_packument("foo", &["1.0.0", "1.1.0", "1.2.0", "2.0.0"], "2.0.0");
    assert_eq!(classify_regime(&packument, "1.0.0"), Regime::HardFrozen);
}

#[test]
fn classify_regime_soft_frozen_when_only_higher_major_exists() {
    // Picking the top of the 1.x line while a 2.x exists: no higher
    // minor in-major, but a whole newer major — maintenance line.
    use crate::semver_util::{Regime, classify_regime};
    let packument = make_packument("foo", &["1.0.0", "1.2.0", "2.0.0", "2.1.0"], "2.1.0");
    assert_eq!(classify_regime(&packument, "1.2.0"), Regime::SoftFrozen);
}

#[test]
fn classify_regime_current_at_the_frontier() {
    // Picking the highest stable version: nothing newer is visible, so
    // a newer upstream publish the offline seed can't see is possible.
    use crate::semver_util::{Regime, classify_regime};
    let packument = make_packument("foo", &["1.0.0", "1.1.0", "1.2.0"], "1.2.0");
    assert_eq!(classify_regime(&packument, "1.2.0"), Regime::Current);
}

#[test]
fn classify_regime_ignores_prereleases() {
    // A dangling `2.0.0-rc.1` must not make a stable 1.x pick look
    // soft-frozen, nor should it count as a higher major: the frontier
    // for regime purposes is the highest *stable* release.
    use crate::semver_util::{Regime, classify_regime};
    let packument = make_packument("foo", &["1.0.0", "1.1.0", "2.0.0-rc.1"], "1.1.0");
    assert_eq!(classify_regime(&packument, "1.1.0"), Regime::Current);
}

#[test]
fn classify_regime_unparseable_pick_is_current() {
    // Total + conservative: an unparseable pick can't be proven frozen,
    // so it defaults to Current (keeps the freshness gate).
    use crate::semver_util::{Regime, classify_regime};
    let packument = make_packument("foo", &["1.0.0"], "1.0.0");
    assert_eq!(
        classify_regime(&packument, "not-a-version"),
        Regime::Current
    );
}

#[test]
fn test_pick_version_locked_out_of_range() {
    let packument = make_packument("foo", &["1.0.0", "2.0.0"], "2.0.0");
    // Locked version doesn't satisfy range, should pick highest match
    let result = pick_version(&packument, "^2.0.0", Some("1.0.0"), false, None, false).unwrap();
    assert_eq!(result.version, "2.0.0");
}

#[test]
fn test_pick_version_dist_tag() {
    let packument = make_packument("foo", &["1.0.0", "2.0.0-beta.1"], "1.0.0");
    let result = pick_version(&packument, "latest", None, false, None, false).unwrap();
    assert_eq!(result.version, "1.0.0");
}

#[test]
fn test_pick_version_lowest_picks_smallest_satisfying() {
    let packument = make_packument("foo", &["1.0.0", "1.1.0", "1.2.0", "2.0.0"], "2.0.0");
    let result = pick_version(&packument, "^1.0.0", None, true, None, false).unwrap();
    assert_eq!(result.version, "1.0.0");
}

#[test]
fn test_pick_version_cutoff_filters_future_versions() {
    let mut packument = make_packument("foo", &["1.0.0", "1.1.0", "1.2.0"], "1.2.0");
    packument
        .time
        .insert("1.0.0".into(), "2020-01-01T00:00:00.000Z".into());
    packument
        .time
        .insert("1.1.0".into(), "2021-01-01T00:00:00.000Z".into());
    packument
        .time
        .insert("1.2.0".into(), "2023-01-01T00:00:00.000Z".into());
    // Highest pick, but cutoff forbids 1.2.0 → fall back to 1.1.0.
    let cutoff = "2022-06-01T00:00:00.000Z";
    let result = pick_version(&packument, "^1.0.0", None, false, Some(cutoff), false).unwrap();
    assert_eq!(result.version, "1.1.0");
}

#[test]
fn test_pick_version_lenient_falls_back_to_lowest_when_cutoff_excludes_all() {
    // Mirrors pnpm's lenient `pickPackageFromMetaUsingTime`: when
    // every satisfying version is younger than the cutoff, fall
    // back to the lowest satisfying version (ignoring the cutoff).
    let mut packument = make_packument("foo", &["1.0.0", "1.1.0", "1.2.0"], "1.2.0");
    packument
        .time
        .insert("1.0.0".into(), "2024-01-01T00:00:00.000Z".into());
    packument
        .time
        .insert("1.1.0".into(), "2024-06-01T00:00:00.000Z".into());
    packument
        .time
        .insert("1.2.0".into(), "2025-01-01T00:00:00.000Z".into());
    let cutoff = "2020-01-01T00:00:00.000Z";
    let result = pick_version(&packument, "^1.0.0", None, false, Some(cutoff), false).unwrap();
    assert_eq!(result.version, "1.0.0");
}

#[test]
fn test_pick_version_strict_returns_age_gated_when_cutoff_excludes_all() {
    let mut packument = make_packument("foo", &["1.0.0", "1.1.0"], "1.1.0");
    packument
        .time
        .insert("1.0.0".into(), "2024-01-01T00:00:00.000Z".into());
    packument
        .time
        .insert("1.1.0".into(), "2024-06-01T00:00:00.000Z".into());
    let cutoff = "2020-01-01T00:00:00.000Z";
    let result = pick_version(&packument, "^1.0.0", None, false, Some(cutoff), true);
    assert!(matches!(result, PickResult::AgeGated));
}

#[test]
fn test_minimum_release_age_cutoff_format() {
    let mra = MinimumReleaseAge {
        minutes: 60,
        ..Default::default()
    };
    let cutoff = mra.cutoff().expect("non-zero minutes produces a cutoff");
    // Sanity-check the shape; the actual instant depends on now().
    assert_eq!(cutoff.len(), 24, "ISO-8601 with millis is 24 chars");
    assert!(cutoff.ends_with("Z"));
    assert_eq!(&cutoff[4..5], "-");
    assert_eq!(&cutoff[10..11], "T");
}

#[test]
fn test_minimum_release_age_zero_disables() {
    let mra = MinimumReleaseAge::default();
    assert!(mra.cutoff().is_none());
}

#[tokio::test]
async fn minimum_release_age_fetches_full_packument_directly() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut packument = make_packument("foo", &["1.0.0"], "1.0.0");
    packument.modified = Some("2024-01-01T00:00:00.000Z".to_string());
    let body = serde_json::to_vec(&packument).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registry = format!("http://{}/", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let request_count = requests.clone();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            request_count.fetch_add(1, Ordering::Relaxed);
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = [0_u8; 2048];
                let _ = socket.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            });
        }
    });

    let base = std::env::temp_dir().join(format!(
        "aube-resolver-mra-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cache_dir = base.join("packuments");
    let full_cache_dir = base.join("packuments-full");
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::create_dir_all(&full_cache_dir).unwrap();

    let client = Arc::new(aube_registry::client::RegistryClient::new(&registry));
    let mut resolver = Resolver::new(client)
        .with_packument_cache(cache_dir)
        .with_packument_full_cache(full_cache_dir)
        .with_minimum_release_age(Some(MinimumReleaseAge {
            minutes: 60,
            ..Default::default()
        }));
    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("foo".to_string(), "1.0.0".to_string());

    let graph = resolver.resolve(&manifest, None).await.unwrap();

    assert!(graph_has_package(&graph, "foo", "1.0.0"));
    assert_eq!(requests.load(Ordering::Relaxed), 1);
    server.abort();
    let _ = std::fs::remove_dir_all(base);
}

/// Regression guard for the upstream #892 merge-sync hazard: under the
/// DEFAULT `resolution-mode=highest` with `minimumReleaseAge` active,
/// the resolver must still keep the picked versions' publish times in
/// the in-memory `graph.times`. #892 narrowed the in-memory recording
/// gate to time-based mode only, which left `graph.times` empty on the
/// default install — failing the embedder's `defaultTrust` cooling-window
/// floor closed (allowlisted native packages got denied). The fix
/// decoupled in-memory recording (kept wide: time-based OR
/// `minimumReleaseAge` OR `trustPolicy=no-downgrade`) from lockfile
/// `time:` PERSISTENCE (still time-based-only, gated at the write site).
/// A future `git merge upstream/main` that re-narrows
/// `should_keep_in_memory_times` would re-break the floor; this test
/// catches it. Uses a direct mock registry (no primer path) so it
/// exercises the normal pick-site recording, not the primer-refetch
/// heal the sibling test covers.
#[tokio::test]
async fn highest_mode_with_minimum_release_age_keeps_in_memory_times() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut packument = make_packument("foo", &["1.0.0"], "1.0.0");
    packument.modified = Some("2024-01-01T00:00:00.000Z".to_string());
    packument
        .time
        .insert("1.0.0".to_string(), "2024-01-01T00:00:00.000Z".to_string());
    let body = serde_json::to_vec(&packument).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registry = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = [0_u8; 2048];
                let _ = socket.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            });
        }
    });

    let base = std::env::temp_dir().join(format!(
        "aube-resolver-mra-times-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cache_dir = base.join("packuments");
    let full_cache_dir = base.join("packuments-full");
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::create_dir_all(&full_cache_dir).unwrap();

    let client = Arc::new(aube_registry::client::RegistryClient::new(&registry));
    // Default resolution mode is Highest — intentionally NOT time-based.
    // `minimumReleaseAge` is the only policy active, so the in-memory
    // recording must come from `should_keep_in_memory_times`'s
    // `minimum_release_age.is_some()` branch.
    let mut resolver = Resolver::new(client)
        .with_packument_cache(cache_dir)
        .with_packument_full_cache(full_cache_dir)
        .with_minimum_release_age(Some(MinimumReleaseAge {
            minutes: 60,
            ..Default::default()
        }));
    assert_eq!(
        resolver.resolution_mode,
        ResolutionMode::Highest,
        "test must run in the default Highest mode to guard the regression"
    );
    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("foo".to_string(), "1.0.0".to_string());

    let graph = resolver.resolve(&manifest, None).await.unwrap();

    assert!(graph_has_package(&graph, "foo", "1.0.0"));
    assert_eq!(
        graph.times.get("foo@1.0.0").map(String::as_str),
        Some("2024-01-01T00:00:00.000Z"),
        "Highest mode + minimumReleaseAge must keep the publish time in \
         graph.times for the defaultTrust floor; got {:?}",
        graph.times
    );
    server.abort();
    let _ = std::fs::remove_dir_all(base);
}

/// Regression: a primer-seeded pick that satisfies the range must still
/// record the package's publish time when `minimumReleaseAge` is active.
/// The bundled primer's `time` data is sparse, so a primer hit could
/// leave `graph.times` empty for an aged stable package — silently
/// disabling every `time:` consumer (the `defaultTrust` build floor, the
/// lockfile round-trip). The resolver must refetch the full packument to
/// recover the time. Found via the brand-sweep gate (esbuild floored on
/// the warm install only, ignored on the fresh one) 2026-06-10.
#[tokio::test]
async fn primer_seeded_pick_records_publish_time_for_the_age_floor() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Pick a real primer entry whose seed carries NO publish time for
    // its latest version AND has no runtime deps for it — exactly the
    // shape that strands the floor, while staying resolvable against a
    // single-package mock registry. An empty primer or an
    // all-times-present primer leaves nothing to regress, so skip
    // honestly (matching `bundled_primer_loads`).
    let Some((name, version)) = crate::primer::names().find_map(|name| {
        let pkt = crate::primer::get(name)?.packument();
        let latest = pkt.dist_tags.get("latest")?;
        let meta = pkt.versions.get(latest)?;
        (!pkt.time.contains_key(latest)
            && meta.dependencies.is_empty()
            && meta.optional_dependencies.is_empty()
            && meta.peer_dependencies.is_empty())
        .then(|| (name.to_string(), latest.clone()))
    }) else {
        return;
    };

    // The mock registry serves a full packument for that exact
    // (name, version) WITH a time entry — the data the resolver must
    // recover by refetching past the time-less primer seed.
    let mut full = make_packument(&name, &[&version], &version);
    full.modified = Some("2024-01-01T00:00:00.000Z".to_string());
    full.time
        .insert(version.clone(), "2024-01-01T00:00:00.000Z".to_string());
    let full_body = serde_json::to_vec(&full).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registry = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let full_body = full_body.clone();
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 4096];
                let _ = socket.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    full_body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(&full_body).await.unwrap();
            });
        }
    });

    let base = std::env::temp_dir().join(format!(
        "aube-resolver-primer-time-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(base.join("packuments")).unwrap();
    std::fs::create_dir_all(base.join("packuments-full")).unwrap();

    let client = Arc::new(aube_registry::client::RegistryClient::new(&registry));
    // `force_metadata_primer` routes the non-default mock registry
    // through the primer; a long window keeps the cutoff old enough
    // that the primer is eligible (covers_cutoff) so the primer seeds
    // the pick — the exact path that strands the time.
    let mut resolver = Resolver::new(client)
        .with_packument_cache(base.join("packuments"))
        .with_packument_full_cache(base.join("packuments-full"))
        .with_force_metadata_primer(true)
        .with_minimum_release_age(Some(MinimumReleaseAge {
            minutes: 10 * 365 * 24 * 60,
            ..Default::default()
        }));
    let mut manifest = PackageJson::default();
    manifest.dependencies.insert(name.clone(), version.clone());

    let graph = resolver.resolve(&manifest, None).await.expect("resolve");

    assert!(
        graph
            .times
            .values()
            .any(|t| t.as_str() == "2024-01-01T00:00:00.000Z"),
        "primer-seeded pick of {name}@{version} dropped its publish time \
         from graph.times (the defaultTrust floor would fail closed): {:?}",
        graph.times
    );

    server.abort();
    let _ = std::fs::remove_dir_all(base);
}

/// Empirical proof that the cold-install "24h self-disable" regression is
/// fixed by the always-on pick-site gate + default-unlimited primer TTL,
/// observed via *registry hit count* against a request-counting mock
/// registry. The bundled primer's `AUBE_PRIMER_GENERATED_AT` is baked at
/// build time (the primer data file's mtime). The active `minimumReleaseAge`
/// cutoff is `now - minutes`, so:
///
///   - a LARGE window (cutoff far in the past) → simulates a freshly-built
///     binary (cutoff predates the build);
///   - a 24h window (cutoff = now - 1 day) → once the binary is older than
///     ~24h, `now - 1day` lands AFTER `generated_at` → simulates the aged
///     binary, the case the legacy fetch-site gate used to self-disable on.
///
/// Both cases must now serve a FROZEN pick from the primer with NO registry
/// hit: the pick-site gate is the always-on correctness layer, and the
/// default primer TTL is unlimited, so an aged binary's frozen picks stay
/// offline (evergreen) — the cold-install fix.
#[tokio::test]
async fn primer_serves_frozen_pick_offline_even_on_an_aged_binary() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Pick a real primer entry that is FROZEN (a newer version exists past
    // the version a `^picked` range would resolve to) and resolves
    // standalone (latest has no runtime/peer/optional deps). Frozen is
    // what the pick-gate serves offline; standalone keeps the mock
    // single-package. Skip honestly on an empty primer.
    let Some((name, range, picked)) = crate::primer::names().find_map(|name| {
        let pkt = crate::primer::get(name)?.packument();
        // Collect parseable stable versions, ascending.
        let mut stable: Vec<node_semver::Version> = pkt
            .versions
            .keys()
            .filter_map(|v| node_semver::Version::parse(v).ok())
            .filter(|v| v.pre_release.is_empty())
            .collect();
        stable.sort();
        // Need a version in the SAME major with a strictly-higher minor
        // *present in the primer's slice* — so `classify_regime` (which
        // runs against the primer packument under the gate) sees the
        // pick as HardFrozen. Pin the range to that exact lower version
        // (`=lower`) so the pick is deterministic and leaves the higher
        // minor sitting above it.
        let lower = stable.iter().find(|low| {
            stable
                .iter()
                .any(|hi| hi.major == low.major && hi.minor > low.minor)
        })?;
        let picked_str = lower.to_string();
        let meta = pkt.versions.get(&picked_str)?;
        (meta.dependencies.is_empty()
            && meta.optional_dependencies.is_empty()
            && meta.peer_dependencies.is_empty())
        .then(|| (name.to_string(), format!("={picked_str}"), picked_str))
    }) else {
        return;
    };

    // A request-counting registry. Any request that reaches it means the
    // primer did NOT serve the pick. The body is a full packument the
    // resolver could resolve `range` against, so the test never errors —
    // it only measures whether the network was touched.
    fn spawn_counting_registry(
        full_body: Vec<u8>,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = hits.clone();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let registry = format!("http://{addr}/");
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                seen.fetch_add(1, Ordering::Relaxed);
                let full_body = full_body.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 8192];
                    let _ = socket.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        full_body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&full_body).await;
                });
            }
        });
        (registry, hits, server)
    }

    // Build a full packument that carries both the picked version and a
    // strictly-higher one (so the range resolves regardless of which path
    // is taken) plus a `time` entry for the pick (so the needs_time floor
    // doesn't force a refetch and contaminate the measurement).
    let make_full = || {
        let higher = {
            let p = node_semver::Version::parse(&picked).unwrap();
            format!("{}.{}.0", p.major, p.minor + 1)
        };
        let mut full = make_packument(&name, &[&picked, &higher], &higher);
        full.modified = Some("2024-01-01T00:00:00.000Z".to_string());
        full.time
            .insert(picked.clone(), "2024-01-01T00:00:00.000Z".to_string());
        full.time
            .insert(higher.clone(), "2024-01-02T00:00:00.000Z".to_string());
        serde_json::to_vec(&full).unwrap()
    };

    let temp_base = |tag: &str| {
        let base = std::env::temp_dir().join(format!(
            "aube-resolver-primer-disable-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(base.join("packuments")).unwrap();
        std::fs::create_dir_all(base.join("packuments-full")).unwrap();
        base
    };

    let run = |minutes: u64, base: std::path::PathBuf, registry: String| {
        let name = name.clone();
        let range = range.clone();
        async move {
            let client = Arc::new(aube_registry::client::RegistryClient::new(&registry));
            let mut resolver = Resolver::new(client)
                .with_packument_cache(base.join("packuments"))
                .with_packument_full_cache(base.join("packuments-full"))
                .with_force_metadata_primer(true)
                // Time field on: keeps `needs_time` false so the only
                // reason to touch the registry is the primer being
                // *skipped* — isolating the covers_cutoff gate from the
                // unrelated sparse-time refetch heal. Cooling is still
                // fully applied (cutoff computed + filtered in pick_version
                // against the primer's own time map).
                .with_registry_supports_time_field(true)
                .with_minimum_release_age(Some(MinimumReleaseAge {
                    minutes,
                    ..Default::default()
                }));
            let mut manifest = PackageJson::default();
            manifest.dependencies.insert(name.clone(), range);
            let graph = resolver.resolve(&manifest, None).await.expect("resolve");
            (graph, base)
        }
    };

    // Sanity: the bundled `generated_at` must be in the past relative to a
    // 24h-ago cutoff (i.e. the binary is "older than a day"). If a brand
    // new build is under test (generated_at within the last day), the
    // legacy gate wouldn't fire yet — skip honestly rather than assert a
    // false negative.
    let day_ago = MinimumReleaseAge {
        minutes: 1440,
        ..Default::default()
    }
    .cutoff()
    .unwrap();
    if crate::primer::covers_cutoff(&day_ago) {
        // Binary built within the last 24h — self-disable not yet active.
        return;
    }

    // (A) Fresh-binary simulation: 10-year window → cutoff far in the past
    // → covers_cutoff TRUE → primer serves → ZERO registry hits.
    // The pick-path determines which version is chosen (primer slice vs
    // mock registry), so we measure *only* the hit count, not the version.
    let (reg_a, hits_a, srv_a) = spawn_counting_registry(make_full());
    let (graph_a, base_a) = run(10 * 365 * 24 * 60, temp_base("fresh"), reg_a).await;
    srv_a.abort();
    assert!(
        graph_has_package_named(&graph_a, &name),
        "fresh-window resolve did not resolve {name} at all"
    );
    let fresh_hits = hits_a.load(Ordering::Relaxed);

    // (B) Aged-binary simulation: 24h window → cutoff is AFTER generated_at.
    // Under the old legacy fetch-site gate this would self-disable the primer
    // and hit the registry; under the always-on pick-site gate with the
    // default unlimited TTL, a FROZEN pick is served from the primer offline
    // even on an aged binary — no registry hit.
    let (reg_b, hits_b, srv_b) = spawn_counting_registry(make_full());
    let (graph_b, base_b) = run(1440, temp_base("aged"), reg_b).await;
    srv_b.abort();
    assert!(
        graph_has_package_named(&graph_b, &name),
        "aged-window resolve did not resolve {name} at all"
    );
    let aged_hits = hits_b.load(Ordering::Relaxed);

    let _ = std::fs::remove_dir_all(base_a);
    let _ = std::fs::remove_dir_all(base_b);

    // Evergreen-by-default: the pick-site gate is always on and the default
    // primer TTL is unlimited, so a FROZEN pick is served from the primer in
    // BOTH the fresh and aged cases — no registry hit either way. This is the
    // cold-install fix; the old "aged binary self-disables to the network"
    // regression is gone.
    assert_eq!(
        fresh_hits, 0,
        "fresh window should serve the frozen pick from the primer"
    );
    assert_eq!(
        aged_hits, 0,
        "evergreen default: aged binary must STILL serve the frozen pick \
         offline (unlimited TTL + always-on pick-gate), but the registry was hit"
    );
}

/// Measurement (not a regression — `#[ignore]` so CI skips it): of the
/// bundled primer's entries, what fraction of `latest`-tag picks are
/// FROZEN — i.e. a strictly-newer stable version already exists past the
/// pick, so the resolution is immutable and stays correct no matter how
/// old the binary is. Run with `cargo test -p aube-resolver
/// primer_frozen_fraction -- --ignored --nocapture` to print the split.
///
/// Note: `latest`-tag picks under-report the frozen fraction, because the
/// latest tag is by definition near the live frontier. A realistic cold
/// install resolves *caret ranges that pin older lines*, which the
/// pick-gate classifies as frozen far more often. This number is a
/// conservative floor on how much permanently-valid data the 24h cutoff
/// throws away.
#[test]
#[ignore = "measurement, not a regression"]
fn primer_frozen_fraction() {
    use crate::semver_util::{Regime, classify_regime};

    // (1) `latest`-tag proxy — the live-frontier pick. A FLOOR on the
    // frozen fraction: latest is by definition near `Current`, so this
    // badly under-counts what a real install (resolving caret ranges that
    // pin older lines) classifies as frozen.
    let (mut lh, mut ls, mut lc, mut lt) = (0u32, 0u32, 0u32, 0u32);
    // (2) Every stable version in every primer slice, classified against
    // its own packument. This is the population a cold install draws from
    // when its lockfile / caret ranges pin historical lines — the realistic
    // shape. Each version is one would-be primer pick.
    let (mut ah, mut as_, mut ac, mut at) = (0u32, 0u32, 0u32, 0u32);
    for name in crate::primer::names() {
        let Some(pkt) = crate::primer::get(name).map(|s| s.packument()) else {
            continue;
        };
        if let Some(latest) = pkt.dist_tags.get("latest") {
            lt += 1;
            match classify_regime(&pkt, latest) {
                Regime::HardFrozen => lh += 1,
                Regime::SoftFrozen => ls += 1,
                Regime::Current => lc += 1,
            }
        }
        for ver in pkt.versions.keys() {
            // Skip prereleases: caret ranges don't resolve to them.
            if node_semver::Version::parse(ver).is_ok_and(|v| !v.pre_release.is_empty()) {
                continue;
            }
            at += 1;
            match classify_regime(&pkt, ver) {
                Regime::HardFrozen => ah += 1,
                Regime::SoftFrozen => as_ += 1,
                Regime::Current => ac += 1,
            }
        }
    }
    let lfrozen = lh + ls;
    let afrozen = ah + as_;
    eprintln!(
        "[latest-tag, FLOOR] {lt} pkgs: HardFrozen={lh} SoftFrozen={ls} \
         Current={lc} => frozen={lfrozen} ({:.1}%)",
        100.0 * f64::from(lfrozen) / f64::from(lt.max(1)),
    );
    eprintln!(
        "[all-stable-versions, realistic] {at} versions: HardFrozen={ah} \
         SoftFrozen={as_} Current={ac} => frozen(immutable)={afrozen} \
         ({:.1}%), current(live)={ac} ({:.1}%)",
        100.0 * f64::from(afrozen) / f64::from(at.max(1)),
        100.0 * f64::from(ac) / f64::from(at.max(1)),
    );
}

/// Regression: a primer hit must not poison the full-packument cache with
/// the registry's validators. The bundled primer is a *truncated* slice
/// (newest `version_cap` versions) but carries the full document's real
/// ETag/Last-Modified. If the primer seeds those into the full-packument
/// cache, the driver's range-miss heal (a satisfying version lives outside
/// the primer window) sends `If-None-Match`, the registry answers
/// `304 Not Modified`, and the *truncated* body is resurrected as
/// authoritative — so a range the registry can plainly satisfy fails with
/// `ERR_AUBE_NO_MATCHING_VERSION`. Surfaced by the time-bearing primer,
/// which sorts the kept window by publish time and so drops older stable
/// lines for high-churn packages (e.g. `eslint-plugin-react-hooks`, whose
/// `^5` stable sits 700+ publishes behind a flood of experimental builds).
/// The seed must drop the validators so the heal is an unconditional GET.
#[tokio::test]
async fn primer_range_miss_heals_past_a_304_on_the_seeded_full_cache() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Any real primer entry will do — we ask for a version the primer
    // never carries (`99999.0.0`) so the pick is always a range miss that
    // must heal against the registry. Skip honestly on an empty primer.
    let Some(name) = crate::primer::names().next().map(str::to_string) else {
        return;
    };
    let healed_version = "99999.0.0";
    let mut full = make_packument(&name, &[healed_version], healed_version);
    full.modified = Some("2024-01-01T00:00:00.000Z".to_string());
    full.time.insert(
        healed_version.to_string(),
        "2024-01-01T00:00:00.000Z".to_string(),
    );
    let full_body = serde_json::to_vec(&full).unwrap();

    // The registry 304s any *conditional* request (`If-None-Match` /
    // `If-Modified-Since`) — i.e. it claims "unchanged since the primer
    // built" — and serves the full body only to an unconditional GET.
    // A heal that wrongly carries the primer's validators gets the 304
    // and never sees `99999.0.0`.
    let conditional_hits = Arc::new(AtomicUsize::new(0));
    let conditional_seen = conditional_hits.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registry = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let full_body = full_body.clone();
            let conditional_seen = conditional_seen.clone();
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                let conditional =
                    req.contains("if-none-match:") || req.contains("if-modified-since:");
                if conditional {
                    conditional_seen.fetch_add(1, Ordering::Relaxed);
                    let response = "HTTP/1.1 304 Not Modified\r\netag: \"primer\"\r\nconnection: close\r\n\r\n";
                    let _ = socket.write_all(response.as_bytes()).await;
                } else {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\netag: \"live\"\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        full_body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&full_body).await;
                }
            });
        }
    });

    let base = std::env::temp_dir().join(format!(
        "aube-resolver-primer-304-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(base.join("packuments")).unwrap();
    std::fs::create_dir_all(base.join("packuments-full")).unwrap();

    let client = Arc::new(aube_registry::client::RegistryClient::new(&registry));
    // `force_metadata_primer` routes the mock registry through the primer
    // seed; a long window keeps the cutoff old enough that the primer is
    // eligible, so the primer seeds the (truncated) full cache — the exact
    // setup that strands the heal behind a 304.
    let mut resolver = Resolver::new(client)
        .with_packument_cache(base.join("packuments"))
        .with_packument_full_cache(base.join("packuments-full"))
        .with_force_metadata_primer(true)
        .with_minimum_release_age(Some(MinimumReleaseAge {
            minutes: 10 * 365 * 24 * 60,
            ..Default::default()
        }));
    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert(name.clone(), format!("={healed_version}"));

    let graph = resolver
        .resolve(&manifest, None)
        .await
        .unwrap_or_else(|e| panic!("range-miss heal failed for {name}@{healed_version}: {e}"));

    assert!(
        graph_has_package(&graph, &name, healed_version),
        "heal did not resolve {name}@{healed_version}; the seeded full cache \
         was resurrected behind a 304 ({} conditional requests seen)",
        conditional_hits.load(Ordering::Relaxed)
    );

    server.abort();
    let _ = std::fs::remove_dir_all(base);
}

/// Regression: when both `minimumReleaseAge` and `trustPolicy=NoDowngrade`
/// are active, the resolver must use a full packument with `time`;
/// using an abbreviated corgi packument would make the trust check fail
/// with a spurious `TrustCheckMissingTime` for every version. Reported
/// by Cursor Bugbot on PR #333.
#[tokio::test]
async fn trust_policy_disables_minimum_release_age_short_circuit() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Two packument bodies sharing one name. The corgi body (served to
    // requests carrying the corgi Accept header) has an empty `time`
    // map — exactly what a real npmjs.org corgi response looks like.
    // The full body has the same versions but a populated `time` map.
    // The resolver should go straight to the full fetch, the trust
    // check should see a populated `time`, find no prior versions to
    // compare against, and resolve cleanly.
    let mut corgi = make_packument("foo", &["1.0.0"], "1.0.0");
    corgi.modified = Some("2024-01-01T00:00:00.000Z".to_string());
    let corgi_body = serde_json::to_vec(&corgi).unwrap();

    let mut full = make_packument("foo", &["1.0.0"], "1.0.0");
    full.modified = Some("2024-01-01T00:00:00.000Z".to_string());
    full.time
        .insert("1.0.0".to_string(), "2024-01-01T00:00:00.000Z".to_string());
    let full_body = serde_json::to_vec(&full).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registry = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let corgi_body = corgi_body.clone();
            let full_body = full_body.clone();
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let body = if request.contains("application/vnd.npm.install-v1+json") {
                    corgi_body
                } else {
                    full_body
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            });
        }
    });

    let base = std::env::temp_dir().join(format!(
        "aube-resolver-trust-mra-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(base.join("packuments")).unwrap();
    std::fs::create_dir_all(base.join("packuments-full")).unwrap();

    let client = Arc::new(aube_registry::client::RegistryClient::new(&registry));
    let policy = crate::DependencyPolicy {
        trust_policy: crate::TrustPolicy::NoDowngrade,
        ..crate::DependencyPolicy::default()
    };
    let mut resolver = Resolver::new(client)
        .with_packument_cache(base.join("packuments"))
        .with_packument_full_cache(base.join("packuments-full"))
        .with_minimum_release_age(Some(MinimumReleaseAge {
            minutes: 60,
            ..Default::default()
        }))
        .with_dependency_policy(policy);
    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("foo".to_string(), "1.0.0".to_string());

    let result = resolver.resolve(&manifest, None).await;
    assert!(
        !matches!(result, Err(Error::TrustCheckMissingTime(_))),
        "shortcircuit must be suppressed when trustPolicy=NoDowngrade — got {result:?}"
    );
    let graph = result.expect("clean resolve");
    assert!(graph_has_package(&graph, "foo", "1.0.0"));

    server.abort();
    let _ = std::fs::remove_dir_all(base);
}

#[tokio::test]
async fn trust_policy_no_downgrade_blocks_downgraded_install() {
    use aube_registry::Attestations;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Three versions of `foo`. 2.0.0 has provenance attestation;
    // 3.0.0 lost it (the supply-chain incident shape pnpm's check is
    // designed for). With trustPolicy=no-downgrade, picking 3.0.0
    // must fail with Error::TrustDowngrade.
    let mut packument = make_packument("foo", &["1.0.0", "2.0.0", "3.0.0"], "3.0.0");
    packument
        .time
        .insert("1.0.0".to_string(), "2025-01-01T00:00:00.000Z".to_string());
    packument
        .time
        .insert("2.0.0".to_string(), "2025-02-01T00:00:00.000Z".to_string());
    packument
        .time
        .insert("3.0.0".to_string(), "2025-03-01T00:00:00.000Z".to_string());
    let v2 = packument.versions.get_mut("2.0.0").unwrap();
    v2.dist.as_mut().unwrap().attestations = Some(Attestations {
        provenance: Some(serde_json::json!({
            "predicateType": "https://slsa.dev/provenance/v1"
        })),
    });
    let body = serde_json::to_vec(&packument).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registry = format!("http://{}/", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let request_count = requests.clone();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            request_count.fetch_add(1, Ordering::Relaxed);
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = [0_u8; 2048];
                let _ = socket.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            });
        }
    });

    let base = std::env::temp_dir().join(format!(
        "aube-resolver-trust-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(base.join("packuments")).unwrap();
    std::fs::create_dir_all(base.join("packuments-full")).unwrap();

    let client = Arc::new(aube_registry::client::RegistryClient::new(&registry));
    let policy = crate::DependencyPolicy {
        trust_policy: crate::TrustPolicy::NoDowngrade,
        ..crate::DependencyPolicy::default()
    };
    let mut resolver = Resolver::new(client)
        .with_packument_cache(base.join("packuments"))
        .with_packument_full_cache(base.join("packuments-full"))
        .with_dependency_policy(policy);
    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("foo".to_string(), "3.0.0".to_string());

    let err = resolver
        .resolve(&manifest, None)
        .await
        .expect_err("3.0.0 must be rejected as a trust downgrade");
    match err {
        Error::TrustDowngrade(d) => {
            assert_eq!(d.name, "foo");
            assert_eq!(d.picked_version, "3.0.0");
            assert_eq!(d.prior_version, "2.0.0");
            assert!(matches!(
                d.prior_evidence,
                crate::trust::TrustEvidence::Provenance
            ));
            assert!(d.current_evidence.is_none());
        }
        other => panic!("expected TrustDowngrade, got {other:?}"),
    }

    // Verify the suggested fix path actually unblocks the install.
    let mut excluded_policy = crate::DependencyPolicy {
        trust_policy: crate::TrustPolicy::NoDowngrade,
        ..crate::DependencyPolicy::default()
    };
    excluded_policy.trust_policy_exclude = crate::TrustExcludeRules::parse(["foo@3.0.0"]).unwrap();
    let mut resolver = Resolver::new(Arc::new(aube_registry::client::RegistryClient::new(
        &registry,
    )))
    .with_packument_cache(base.join("packuments"))
    .with_packument_full_cache(base.join("packuments-full"))
    .with_dependency_policy(excluded_policy);
    let graph = resolver
        .resolve(&manifest, None)
        .await
        .expect("excluded version installs cleanly");
    assert!(graph_has_package(&graph, "foo", "3.0.0"));

    server.abort();
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn test_format_iso8601_known_epoch() {
    // 2024-01-01T00:00:00Z = 1704067200
    assert_eq!(
        format_iso8601_utc(1_704_067_200),
        "2024-01-01T00:00:00.000Z"
    );
    // 1970-01-01T00:00:00Z = 0
    assert_eq!(format_iso8601_utc(0), "1970-01-01T00:00:00.000Z");
}

#[test]
fn test_pick_version_cutoff_allows_missing_time_entries() {
    let packument = make_packument("foo", &["1.0.0", "1.1.0"], "1.1.0");
    // Packument has no `time` entries at all — cutoff must not
    // remove every candidate, or the resolver can never make
    // progress on abbreviated-packument registries.
    let cutoff = "2000-01-01T00:00:00.000Z";
    let result = pick_version(&packument, "^1.0.0", None, false, Some(cutoff), false).unwrap();
    assert_eq!(result.version, "1.1.0");
}

#[test]
fn test_pick_version_with_deps() {
    let mut packument = make_packument("foo", &["1.0.0"], "1.0.0");
    packument
        .versions
        .get_mut("1.0.0")
        .unwrap()
        .dependencies
        .insert("bar".to_string(), "^2.0.0".to_string());

    let result = pick_version(&packument, "^1.0.0", None, false, None, false).unwrap();
    assert_eq!(result.dependencies.get("bar").unwrap(), "^2.0.0");
}

fn mk_locked(
    name: &str,
    version: &str,
    deps: &[(&str, &str)],
    peer_deps: &[(&str, &str)],
) -> LockedPackage {
    let mut dependencies = BTreeMap::new();
    for (n, v) in deps {
        dependencies.insert((*n).to_string(), (*v).to_string());
    }
    let mut peer_dependencies = BTreeMap::new();
    for (n, r) in peer_deps {
        peer_dependencies.insert((*n).to_string(), (*r).to_string());
    }
    LockedPackage {
        name: name.to_string(),
        version: version.to_string(),
        integrity: None,
        dependencies,
        peer_dependencies,
        peer_dependencies_meta: BTreeMap::new(),
        dep_path: format!("{name}@{version}"),
        ..Default::default()
    }
}

fn graph_has_package(graph: &LockfileGraph, name: &str, version: &str) -> bool {
    graph
        .packages
        .values()
        .any(|pkg| pkg.name == name && pkg.version == version)
}

fn graph_has_package_named(graph: &LockfileGraph, name: &str) -> bool {
    graph.packages.values().any(|pkg| pkg.name == name)
}

// Regression guard for the cycle-break branch in `visit_peer_context`
// flagged by greptile on #40. Two packages peer-depend on each other:
//
//     a@1.0.0 -> dep=b@1.0.0, peer=b@^1
//     b@1.0.0 -> dep=a@1.0.0, peer=a@^1
//
// Starting the DFS from importer root `a`, we should:
//   1. Visit `a`, recurse into `b`
//   2. Visit `b`, recurse into `a` (cycle hit — `visiting` guard fires)
//   3. Cycle branch returns `a`'s contextualized dep_path WITHOUT
//      waiting for the in-progress insertion to land
//   4. `b` completes, gets inserted
//   5. `a` completes, gets inserted
//
// By the time the function returns, every dep_path referenced from
// any `dependencies` tail must exist as a key in `out_packages`.
#[test]
fn apply_peer_contexts_handles_mutual_peer_cycle() {
    let a = mk_locked("a", "1.0.0", &[("b", "1.0.0")], &[("b", "^1")]);
    let b = mk_locked("b", "1.0.0", &[("a", "1.0.0")], &[("a", "^1")]);

    let mut packages = BTreeMap::new();
    packages.insert("a@1.0.0".to_string(), a);
    packages.insert("b@1.0.0".to_string(), b);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "a".to_string(),
            dep_path: "a@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let canonical = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let out = apply_peer_contexts(canonical, &PeerContextOptions::default())
        .expect("test graph should converge");

    // Both packages got contextualized dep_paths with each other's
    // resolved version baked in.
    let a_key = "a@1.0.0(b@1.0.0)";
    let b_key = "b@1.0.0(a@1.0.0)";
    assert!(
        out.packages.contains_key(a_key),
        "expected {a_key} in {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );
    assert!(
        out.packages.contains_key(b_key),
        "expected {b_key} in {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );

    // Every referenced dependency tail resolves to a real entry in
    // out_packages — the cycle-break branch didn't leak a dangling
    // reference.
    for pkg in out.packages.values() {
        for (child_name, child_tail) in &pkg.dependencies {
            let child_key = format!("{child_name}@{child_tail}");
            assert!(
                out.packages.contains_key(&child_key),
                "dangling dep_path {child_key} referenced from {}",
                pkg.dep_path
            );
        }
    }

    // Importer's direct dep now points at the contextualized `a`.
    let root = out.importers.get(".").unwrap();
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].dep_path, a_key);
}

// When a declared peer has its own resolved peers, the outer
// package's suffix must carry the *nested* form — this is what
// pnpm writes for React ecosystem projects where
// `@testing-library/react` peers on both `react` and `react-dom`,
// and `react-dom` itself peers on `react`. The expected snapshot
// key is `@testing-library/react@14(react@18)(react-dom@18(react@18))`.
//
// This test uses a simplified three-package fixture
// (consumer → adapter → core) where `core` is only a peer and
// `adapter` peers on `core`. The `consumer` peers on both and
// should serialize the `adapter` entry in its suffix with the
// nested `(core@...)` tail.
#[test]
fn apply_peer_contexts_produces_nested_peer_suffixes() {
    // consumer declares peers [adapter, core]. adapter declares
    // peer [core]. core has no deps or peers.
    let mut consumer = mk_locked(
        "consumer",
        "1.0.0",
        &[("adapter", "1.0.0"), ("core", "1.0.0")],
        &[("adapter", "^1"), ("core", "^1")],
    );
    consumer.dep_path = "consumer@1.0.0".to_string();

    let mut adapter = mk_locked("adapter", "1.0.0", &[("core", "1.0.0")], &[("core", "^1")]);
    adapter.dep_path = "adapter@1.0.0".to_string();

    let core = mk_locked("core", "1.0.0", &[], &[]);

    let mut packages = BTreeMap::new();
    packages.insert("consumer@1.0.0".to_string(), consumer);
    packages.insert("adapter@1.0.0".to_string(), adapter);
    packages.insert("core@1.0.0".to_string(), core);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "consumer".to_string(),
            dep_path: "consumer@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let out = apply_peer_contexts(graph, &PeerContextOptions::default())
        .expect("test graph should converge");

    // adapter's standalone key should have just its own peer (core).
    assert!(
        out.packages.contains_key("adapter@1.0.0(core@1.0.0)"),
        "expected nested adapter variant: {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );

    // consumer's key should reference adapter's NESTED tail, i.e.
    // `(adapter@1.0.0(core@1.0.0))(core@1.0.0)` — that's the pnpm
    // byte-identical shape.
    let consumer_key = "consumer@1.0.0(adapter@1.0.0(core@1.0.0))(core@1.0.0)";
    assert!(
        out.packages.contains_key(consumer_key),
        "expected nested consumer key {consumer_key} in {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );

    // Every referenced dependency tail must resolve to a real entry.
    for pkg in out.packages.values() {
        for (child_name, child_tail) in &pkg.dependencies {
            let child_key = format!("{child_name}@{child_tail}");
            assert!(
                out.packages.contains_key(&child_key),
                "dangling dep_path {child_key} referenced from {}",
                pkg.dep_path
            );
        }
    }
}

// Repro for the johnpyp/aube-vite-peer-variant case: a workspace
// importer pins a peer version that DOESN'T satisfy a sibling's
// declared peer range, while the workspace ROOT pins a satisfying
// one. The peer context must follow node_modules-resolution order
// — closest-ancestor-wins, even when its version misses the range
// — and emit an unmet-peer warning rather than reaching past the
// importer to grab a more-distant matching version. Matches what
// pnpm and bun produce for the same shape.
#[test]
fn apply_peer_contexts_prefers_incompatible_ancestor_over_root() {
    // consumer peers on dep@^5. Workspace `app` directly depends on
    // BOTH consumer and dep@8 (out of range). Root pins dep@5 (in
    // range). The expected pick is the closest provider — `app`'s
    // dep@8 — not root's dep@5.
    let mut consumer = mk_locked("consumer", "1.0.0", &[], &[("dep", "^5")]);
    consumer.dep_path = "consumer@1.0.0".to_string();
    let dep5 = mk_locked("dep", "5.0.0", &[], &[]);
    let dep8 = mk_locked("dep", "8.0.0", &[], &[]);

    let mut packages = BTreeMap::new();
    packages.insert("consumer@1.0.0".to_string(), consumer);
    packages.insert("dep@5.0.0".to_string(), dep5);
    packages.insert("dep@8.0.0".to_string(), dep8);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "dep".to_string(),
            dep_path: "dep@5.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("5.0.0".to_string()),
        }],
    );
    importers.insert(
        "packages/app".to_string(),
        vec![
            DirectDep {
                name: "consumer".to_string(),
                dep_path: "consumer@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            },
            DirectDep {
                name: "dep".to_string(),
                dep_path: "dep@8.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("8.0.0".to_string()),
            },
        ],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let out = apply_peer_contexts(graph, &PeerContextOptions::default())
        .expect("test graph should converge");

    // consumer must follow `app`'s dep@8 (its actual node_modules
    // sibling), even though dep@8 doesn't satisfy `^5`.
    assert!(
        out.packages.contains_key("consumer@1.0.0(dep@8.0.0)"),
        "consumer must pick app's incompatible dep@8 over root's compatible dep@5: {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );
    assert!(
        !out.packages.contains_key("consumer@1.0.0(dep@5.0.0)"),
        "consumer was incorrectly pinned to root's dep@5: {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );

    // detect_unmet_peers should flag the mismatch so the CLI prints
    // a warning, matching pnpm/bun behavior.
    let unmet = detect_unmet_peers(&out);
    assert!(
        unmet
            .iter()
            .any(|u| u.peer_name == "dep" && u.found.as_deref() == Some("8.0.0")),
        "expected unmet-peer warning for consumer's dep peer: {unmet:?}"
    );
}

// Per-peer-range cross-subtree satisfaction: two sibling packages
// that declare peer react with INCOMPATIBLE ranges should each
// end up pinned to the version satisfying their own range, even
// if an ancestor scope carries the wrong version. This is pnpm's
// "duplicate package per peer context" behavior.
//
// The fixture mirrors the real React/Testing-Library case: the
// user pins `react@17` at the root (which is what the hoist
// propagates into every child's ancestor scope), but a sibling
// dep declares `peer react: ^18`. That sibling must resolve to
// `react@18.x`, not `react@17`.
#[test]
fn apply_peer_contexts_per_range_satisfaction() {
    // consumer17 wants react@^17. consumer18 wants react@^18.
    // Both peer on react. The graph has BOTH versions in play
    // (the BFS resolver already emits both when the ranges
    // conflict — see `resolved_versions` dedupe logic).
    let mut consumer17 = mk_locked(
        "consumer17",
        "1.0.0",
        &[("react", "17.0.2")],
        &[("react", "^17")],
    );
    consumer17.dep_path = "consumer17@1.0.0".to_string();

    let mut consumer18 = mk_locked(
        "consumer18",
        "1.0.0",
        &[("react", "18.2.0")],
        &[("react", "^18")],
    );
    consumer18.dep_path = "consumer18@1.0.0".to_string();

    let react17 = mk_locked("react", "17.0.2", &[], &[]);
    let react18 = mk_locked("react", "18.2.0", &[], &[]);

    let mut packages = BTreeMap::new();
    packages.insert("consumer17@1.0.0".to_string(), consumer17);
    packages.insert("consumer18@1.0.0".to_string(), consumer18);
    packages.insert("react@17.0.2".to_string(), react17);
    packages.insert("react@18.2.0".to_string(), react18);

    // Importer has BOTH consumers plus react@17 hoisted (the hoist
    // pass picks the first-encountered version, matching what
    // happens live when a user pins the older version).
    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "consumer17".to_string(),
                dep_path: "consumer17@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            },
            DirectDep {
                name: "consumer18".to_string(),
                dep_path: "consumer18@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            },
            DirectDep {
                name: "react".to_string(),
                dep_path: "react@17.0.2".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^17".to_string()),
            },
        ],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let out = apply_peer_contexts(graph, &PeerContextOptions::default())
        .expect("test graph should converge");

    // consumer17 should be suffixed with react@17 (satisfies ^17).
    assert!(
        out.packages.contains_key("consumer17@1.0.0(react@17.0.2)"),
        "consumer17 must pick react@17: {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );
    // consumer18 must NOT reuse the root's react@17.0.2 — its own
    // declared range `^18` rejects it, so the peer-context pass
    // should fall back to the BFS-resolved react@18.2.0.
    assert!(
        out.packages.contains_key("consumer18@1.0.0(react@18.2.0)"),
        "consumer18 must fall back to react@18: {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );
    // And specifically must NOT have been glued to react@17 just
    // because the ancestor scope happened to have it.
    assert!(
        !out.packages.contains_key("consumer18@1.0.0(react@17.0.2)"),
        "consumer18 was incorrectly pinned to react@17: {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );
}

// Regression for greptile feedback on #67: the `from_graph_scan`
// fallback in `visit_peer_context` must return the full dep_path
// TAIL, not just `p.version`. On Pass 2+ of the fixed-point loop
// the input graph's keys carry peer suffixes — e.g. `react-dom`
// lives at `react-dom@18.2.0(react@18.2.0)` — and downstream
// lookups that reconstruct `format!("{name}@{tail}")` need the
// tail to match the actual key. Returning `p.version` would give
// `react-dom@18.2.0`, which Pass 2 lookups would miss, silently
// dropping the peer from `new_dependencies`.
//
// The scenario: consumer peers on a package (helper) whose own
// peer context already exists in the graph's suffixed form.
// Neither ancestor scope nor the consumer's own `pkg.dependencies`
// has helper (so the scan path is actually reached), forcing
// `from_graph_scan` to be the resolution source. The resulting
// `consumer` entry must reference the suffixed `helper` tail.
#[test]
fn from_graph_scan_returns_full_dep_path_tail() {
    // helper@1.0.0 has its own peer `core`. `consumer` peers on
    // helper but has no entry for it in its `pkg.dependencies`,
    // so the scan is the only resolution source.
    let mut consumer = mk_locked("consumer", "1.0.0", &[], &[("helper", "^1")]);
    consumer.dep_path = "consumer@1.0.0".to_string();

    // `helper@1.0.0(core@1.0.0)` — already contextualized as it
    // would be after one iteration of the fixed-point loop.
    let mut helper = mk_locked("helper", "1.0.0", &[("core", "1.0.0")], &[("core", "^1")]);
    helper.dep_path = "helper@1.0.0(core@1.0.0)".to_string();

    let mut core = mk_locked("core", "1.0.0", &[], &[]);
    core.dep_path = "core@1.0.0".to_string();

    let mut packages = BTreeMap::new();
    packages.insert("consumer@1.0.0".to_string(), consumer);
    packages.insert("helper@1.0.0(core@1.0.0)".to_string(), helper);
    packages.insert("core@1.0.0".to_string(), core);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "consumer".to_string(),
            dep_path: "consumer@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let out = apply_peer_contexts(graph, &PeerContextOptions::default())
        .expect("test graph should converge");

    // consumer's key must reference helper with its CONTEXTUALIZED
    // tail. Returning `p.version` would have produced
    // `consumer@1.0.0(helper@1.0.0)` and then silently dropped
    // `helper` from new_dependencies when the lookup missed.
    assert!(
        out.packages
            .contains_key("consumer@1.0.0(helper@1.0.0(core@1.0.0))"),
        "consumer must reference helper's contextualized tail: {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );

    // And consumer.new_dependencies[helper] must be a non-dangling
    // reference into out_packages.
    let consumer_out = out
        .packages
        .get("consumer@1.0.0(helper@1.0.0(core@1.0.0))")
        .unwrap();
    let helper_tail = consumer_out
        .dependencies
        .get("helper")
        .expect("consumer must wire helper as a dep");
    assert_eq!(helper_tail, "1.0.0(core@1.0.0)");
    let helper_key = format!("helper@{helper_tail}");
    assert!(
        out.packages.contains_key(&helper_key),
        "consumer.dependencies[helper] must resolve to an existing package key"
    );
}

// `dedupe-peer-dependents=true` (the pnpm default) should collapse
// two importer dependents that peer on the same name and resolve
// to the same peer version into a single variant. Here two
// consumers (consumer-a, consumer-b) both peer on react and both
// end up with react@18.0.0 — the peer-context pass should emit a
// single canonical consumer-a key and a single canonical
// consumer-b key, but crucially when two *different ancestor
// subtrees* pin the same peer version we still collapse to one
// variant rather than keeping one per subtree.
#[test]
fn dedupe_peer_dependents_merges_equivalent_subtrees() {
    // Two sibling middle packages that each peer on react. The
    // importer has react@18.0.0 available, and the middle
    // packages' shared declared peer range (^18) would match.
    // Without dedupe-peer-dependents, the outer fixed-point loop
    // can emit duplicate variants for the same peer resolution
    // when the middle packages are reached via different sibling
    // paths. With the flag on, `dedupe_peer_variants` merges them.
    let mut consumer_a = mk_locked(
        "consumer",
        "1.0.0",
        &[("react", "18.0.0")],
        &[("react", "^18")],
    );
    consumer_a.dep_path = "consumer@1.0.0".to_string();
    let react = mk_locked("react", "18.0.0", &[], &[]);

    let mut packages = BTreeMap::new();
    // Seed two peer-suffixed keys manually to simulate mid-fixpoint
    // state where distinct subtrees produced the same peer
    // resolution. The dedupe pass should merge them.
    packages.insert(
        "consumer@1.0.0(react@18.0.0)".to_string(),
        LockedPackage {
            dep_path: "consumer@1.0.0(react@18.0.0)".to_string(),
            dependencies: {
                let mut m = BTreeMap::new();
                m.insert("react".to_string(), "18.0.0".to_string());
                m
            },
            ..consumer_a.clone()
        },
    );
    // A second variant with identical peer resolution but a
    // different suffix encoding — simulating a stale subtree from
    // an earlier fixpoint iteration.
    let mut variant = consumer_a.clone();
    variant.dep_path = "consumer@1.0.0(react@18.0.0)(react@18.0.0)".to_string();
    variant
        .dependencies
        .insert("react".to_string(), "18.0.0".to_string());
    packages.insert(
        "consumer@1.0.0(react@18.0.0)(react@18.0.0)".to_string(),
        variant,
    );
    packages.insert("react@18.0.0".to_string(), react);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "consumer".to_string(),
            dep_path: "consumer@1.0.0(react@18.0.0)(react@18.0.0)".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let out = dedupe_peer_variants(graph);

    // Only one consumer variant should survive — the
    // lexicographically smallest key.
    let consumer_keys: Vec<_> = out
        .packages
        .keys()
        .filter(|k| k.starts_with("consumer@"))
        .collect();
    assert_eq!(
        consumer_keys.len(),
        1,
        "expected single canonical consumer variant after dedupe, got: {:?}",
        consumer_keys
    );
    assert_eq!(
        consumer_keys[0], "consumer@1.0.0(react@18.0.0)",
        "canonical should be lex-smallest key"
    );

    // Importer reference was rewritten to the canonical dep_path.
    let root = out.importers.get(".").unwrap();
    assert_eq!(root[0].dep_path, "consumer@1.0.0(react@18.0.0)");
}

// `dedupe-peer-dependents=false` should preserve every distinct
// peer-suffixed variant, even when they would merge under the
// default `true` setting. `apply_peer_contexts` is the only call
// gated by the flag, so the meaningful assertion is that calling
// `dedupe_peer_variants` explicitly merges the two variants, and
// skipping the call (the flag-off codepath) leaves both intact.
#[test]
fn dedupe_peer_dependents_disabled_keeps_variants() {
    let consumer_a = mk_locked(
        "consumer",
        "1.0.0",
        &[("react", "18.0.0")],
        &[("react", "^18")],
    );
    let react = mk_locked("react", "18.0.0", &[], &[]);

    let mut packages = BTreeMap::new();
    packages.insert(
        "consumer@1.0.0(react@18.0.0)".to_string(),
        LockedPackage {
            dep_path: "consumer@1.0.0(react@18.0.0)".to_string(),
            dependencies: {
                let mut m = BTreeMap::new();
                m.insert("react".to_string(), "18.0.0".to_string());
                m
            },
            ..consumer_a.clone()
        },
    );
    let mut variant = consumer_a.clone();
    variant.dep_path = "consumer@1.0.0(react@18.0.0)(react@18.0.0)".to_string();
    variant
        .dependencies
        .insert("react".to_string(), "18.0.0".to_string());
    packages.insert(
        "consumer@1.0.0(react@18.0.0)(react@18.0.0)".to_string(),
        variant,
    );
    packages.insert("react@18.0.0".to_string(), react);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "consumer".to_string(),
            dep_path: "consumer@1.0.0(react@18.0.0)".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    // Flag-off codepath: dedupe_peer_variants is never called,
    // so both variants survive untouched.
    let consumer_keys_off: Vec<_> = graph
        .packages
        .keys()
        .filter(|k| k.starts_with("consumer@"))
        .cloned()
        .collect();
    assert_eq!(
        consumer_keys_off.len(),
        2,
        "expected both variants to survive with dedupe_peer_dependents=false, got: {:?}",
        consumer_keys_off
    );

    // Flag-on codepath (for comparison): dedupe_peer_variants
    // collapses the two peer-equivalent variants into one.
    let merged = dedupe_peer_variants(graph);
    let consumer_keys_on: Vec<_> = merged
        .packages
        .keys()
        .filter(|k| k.starts_with("consumer@"))
        .cloned()
        .collect();
    assert_eq!(
        consumer_keys_on.len(),
        1,
        "expected single canonical variant with dedupe_peer_dependents=true, got: {:?}",
        consumer_keys_on
    );
}

// `dedupe-peers=true` should emit suffixes as `(version)` instead
// of `(name@version)`. The `parse_dep_path` function in
// aube-lockfile handles both forms (splits on the first `(`), so
// round-tripping the key still gives back the package name and
// canonical version.
#[test]
fn dedupe_peers_suffix_is_version_only() {
    let mut consumer = mk_locked(
        "consumer",
        "1.0.0",
        &[("react", "18.2.0")],
        &[("react", "^18")],
    );
    consumer.dep_path = "consumer@1.0.0".to_string();
    let react = mk_locked("react", "18.2.0", &[], &[]);

    let mut packages = BTreeMap::new();
    packages.insert("consumer@1.0.0".to_string(), consumer);
    packages.insert("react@18.2.0".to_string(), react);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "consumer".to_string(),
            dep_path: "consumer@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let options = PeerContextOptions {
        dedupe_peers: true,
        ..PeerContextOptions::default()
    };
    let out = apply_peer_contexts(graph, &options).expect("test graph should converge");

    // Suffix should be `(18.2.0)`, not `(react@18.2.0)`.
    assert!(
        out.packages.contains_key("consumer@1.0.0(18.2.0)"),
        "expected version-only suffix: {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );
    assert!(
        !out.packages.contains_key("consumer@1.0.0(react@18.2.0)"),
        "name-based suffix should not appear under dedupe-peers=true"
    );
}

// `resolve-peers-from-workspace-root=true` should satisfy an
// unresolved peer from the root importer's direct deps BEFORE the
// graph-wide scan tier. Fixture: workspace importer `packages/app`
// directly depends on `consumer`, which peers on react@>=17.
// `packages/app` itself has no react in its deps. Root importer
// pins `react@17.0.2`; the graph also contains `react@18.2.0`
// reachable via some other path. Because ancestor_scope for
// consumer is built from `packages/app`'s direct deps (NOT root's),
// react is missing from the ancestor chain — so only the
// root-tier and graph-scan tiers can satisfy it, and they resolve
// to different versions. Paired on/off assertions distinguish
// which tier ran.
#[test]
fn resolve_peers_from_workspace_root_prefers_root() {
    let build_graph = || {
        let mut consumer = mk_locked("consumer", "1.0.0", &[], &[("react", ">=17")]);
        consumer.dep_path = "consumer@1.0.0".to_string();
        let react17 = mk_locked("react", "17.0.2", &[], &[]);
        let react18 = mk_locked("react", "18.2.0", &[], &[]);

        let mut packages = BTreeMap::new();
        packages.insert("consumer@1.0.0".to_string(), consumer);
        packages.insert("react@17.0.2".to_string(), react17);
        packages.insert("react@18.2.0".to_string(), react18);

        let mut importers = BTreeMap::new();
        // Root importer: pins react@17.0.2. Feeds root_scope.
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "react".to_string(),
                dep_path: "react@17.0.2".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^17".to_string()),
            }],
        );
        // Workspace importer: depends on consumer, but does NOT
        // have react in its own direct deps. Consumer's
        // ancestor_scope therefore does not include react, forcing
        // peer resolution down to the root-or-scan tiers.
        importers.insert(
            "packages/app".to_string(),
            vec![DirectDep {
                name: "consumer".to_string(),
                dep_path: "consumer@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            }],
        );

        LockfileGraph {
            importers,
            packages,
            ..Default::default()
        }
    };

    let options_on = PeerContextOptions {
        resolve_from_workspace_root: true,
        ..PeerContextOptions::default()
    };
    let out_on =
        apply_peer_contexts(build_graph(), &options_on).expect("test graph should converge");
    assert!(
        out_on.packages.contains_key("consumer@1.0.0(react@17.0.2)"),
        "with flag on, consumer should resolve peer from workspace root (17.0.2): {:?}",
        out_on.packages.keys().collect::<Vec<_>>()
    );

    let options_off = PeerContextOptions {
        resolve_from_workspace_root: false,
        ..PeerContextOptions::default()
    };
    let out_off =
        apply_peer_contexts(build_graph(), &options_off).expect("test graph should converge");
    assert!(
        out_off
            .packages
            .contains_key("consumer@1.0.0(react@18.2.0)"),
        "with flag off, consumer should fall through to graph-wide scan (18.2.0): {:?}",
        out_off.packages.keys().collect::<Vec<_>>()
    );
}

// Mutual-peer cycle fixture with `dedupe-peers=true` should still
// converge without hitting MAX_ITERATIONS. The cycle-break
// handling in `contains_canonical_back_ref` uses the `name@version`
// form of the canonical base, but when `dedupe_peers=true` the
// suffix uses just `version` — the check still succeeds because
// nested tails reach back to the same `canonical_base` computed
// from the input key (which is still `name@version`).
#[test]
fn dedupe_peers_cycle_break_still_converges() {
    let a = mk_locked("a", "1.0.0", &[("b", "1.0.0")], &[("b", "^1")]);
    let b = mk_locked("b", "1.0.0", &[("a", "1.0.0")], &[("a", "^1")]);

    let mut packages = BTreeMap::new();
    packages.insert("a@1.0.0".to_string(), a);
    packages.insert("b@1.0.0".to_string(), b);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "a".to_string(),
            dep_path: "a@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let canonical = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let options = PeerContextOptions {
        dedupe_peers: true,
        ..PeerContextOptions::default()
    };
    let out = apply_peer_contexts(canonical, &options).expect("test graph should converge");

    // Under dedupe_peers=true the keys collapse to version-only
    // suffixes.
    let a_key = "a@1.0.0(1.0.0)";
    let b_key = "b@1.0.0(1.0.0)";
    assert!(
        out.packages.contains_key(a_key),
        "expected {a_key} in {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );
    assert!(
        out.packages.contains_key(b_key),
        "expected {b_key} in {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );

    // Every referenced dependency tail resolves to a real entry
    // — proves the cycle break didn't strand references.
    for pkg in out.packages.values() {
        for (child_name, child_tail) in &pkg.dependencies {
            let child_key = format!("{child_name}@{child_tail}");
            assert!(
                out.packages.contains_key(&child_key),
                "dangling dep_path {child_key} referenced from {}",
                pkg.dep_path
            );
        }
    }
}

// Regression: under `dedupe-peers=true`, a package whose canonical
// version coincidentally matches a nested peer's version in an
// unrelated subtree must NOT collide. Cycle detection runs against
// the full `name@version` form during the fixed-point loop, and
// `dedupe_peer_suffixes` rewrites the suffix to version-only as a
// purely cosmetic post-pass — so A@1.0.0's cycle check against
// B's tail `2.0.0(c@1.0.0)` distinguishes "C at 1.0.0" from
// "back-ref to A at 1.0.0".
#[test]
fn dedupe_peers_no_false_positive_on_version_collision() {
    // A@1.0.0 peers on B. B@2.0.0 peers on C. C@1.0.0 has no peers.
    // A and C share version 1.0.0 but are otherwise unrelated.
    // Under `dedupe_peers=true` B's deduped tail is `(2.0.0(1.0.0))`
    // — the inner `1.0.0` is C's peer, not a back-ref to A.
    let a = mk_locked("a", "1.0.0", &[("b", "2.0.0")], &[("b", "^2")]);
    let b = mk_locked("b", "2.0.0", &[("c", "1.0.0")], &[("c", "^1")]);
    let c = mk_locked("c", "1.0.0", &[], &[]);

    let mut packages = BTreeMap::new();
    packages.insert("a@1.0.0".to_string(), a);
    packages.insert("b@2.0.0".to_string(), b);
    packages.insert("c@1.0.0".to_string(), c);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "a".to_string(),
            dep_path: "a@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let options = PeerContextOptions {
        dedupe_peers: true,
        ..PeerContextOptions::default()
    };
    let out = apply_peer_contexts(graph, &options).expect("test graph should converge");

    // A's key must carry B's full nested tail including C's peer.
    // If cycle detection false-positived on the bare version, B's
    // tail would collapse to `(2.0.0)` (dropping `(1.0.0)`) and
    // we'd see `a@1.0.0(2.0.0)` instead.
    assert!(
        out.packages.contains_key("a@1.0.0(2.0.0(1.0.0))"),
        "expected A's key to preserve B's nested peer chain: {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );
    assert!(
        !out.packages.contains_key("a@1.0.0(2.0.0)"),
        "false-positive cycle break would produce the truncated form"
    );
}

// Unit test for the dedupe-peers post-pass: given a key with
// `name@version` suffix segments, produce the version-only form.
#[test]
fn apply_dedupe_peers_to_key_strips_names_in_suffix() {
    assert_eq!(
        apply_dedupe_peers_to_key("react-dom@18.2.0(react@18.2.0)"),
        "react-dom@18.2.0(18.2.0)"
    );
    assert_eq!(
        apply_dedupe_peers_to_key("a@1.0.0(b@2.0.0(c@3.0.0))"),
        "a@1.0.0(2.0.0(3.0.0))"
    );
    // No parens = no change.
    assert_eq!(apply_dedupe_peers_to_key("react@18.2.0"), "react@18.2.0");
    // Already deduped (no `name@` inside parens) = no change.
    assert_eq!(
        apply_dedupe_peers_to_key("a@1.0.0(18.2.0)"),
        "a@1.0.0(18.2.0)"
    );
}

// Regression: two peer-variant keys that differ only in which peer
// NAME they declared (but whose peer versions coincide) must not
// silently collapse into each other when `dedupe_peers=true`.
// `apply_dedupe_peers_to_key` strips peer names, so naive insertion
// into a `BTreeMap` would drop one variant. `dedupe_peer_suffixes`
// detects the collision and keeps both sides in full form.
#[test]
fn dedupe_peer_suffixes_preserves_full_form_on_name_collision() {
    // Construct two distinct variants that would collide after
    // naive suffix rewriting:
    //   consumer@1.0.0(foo@1.0.0)  and  consumer@1.0.0(bar@1.0.0)
    let consumer_foo = {
        let mut pkg = mk_locked("consumer", "1.0.0", &[("foo", "1.0.0")], &[("foo", "^1")]);
        pkg.dep_path = "consumer@1.0.0(foo@1.0.0)".to_string();
        pkg
    };
    let consumer_bar = {
        let mut pkg = mk_locked("consumer", "1.0.0", &[("bar", "1.0.0")], &[("bar", "^1")]);
        pkg.dep_path = "consumer@1.0.0(bar@1.0.0)".to_string();
        pkg
    };
    let foo = mk_locked("foo", "1.0.0", &[], &[]);
    let bar = mk_locked("bar", "1.0.0", &[], &[]);

    let mut packages = BTreeMap::new();
    packages.insert("consumer@1.0.0(foo@1.0.0)".to_string(), consumer_foo);
    packages.insert("consumer@1.0.0(bar@1.0.0)".to_string(), consumer_bar);
    packages.insert("foo@1.0.0".to_string(), foo);
    packages.insert("bar@1.0.0".to_string(), bar);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "consumer".to_string(),
                dep_path: "consumer@1.0.0(foo@1.0.0)".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            },
            DirectDep {
                name: "consumer".to_string(),
                dep_path: "consumer@1.0.0(bar@1.0.0)".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            },
        ],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let out = dedupe_peer_suffixes(graph);

    // Both variants must survive: colliding keys fall back to the
    // original full-form keys instead of silently overwriting each
    // other.
    let consumer_keys: BTreeSet<_> = out
        .packages
        .keys()
        .filter(|k| k.starts_with("consumer@"))
        .cloned()
        .collect();
    assert_eq!(
        consumer_keys.len(),
        2,
        "both consumer variants must survive collision: {consumer_keys:?}"
    );
    assert!(consumer_keys.contains("consumer@1.0.0(foo@1.0.0)"));
    assert!(consumer_keys.contains("consumer@1.0.0(bar@1.0.0)"));

    // Importer references to the full-form keys must stay pointing
    // at the preserved variants.
    let importer_keys: BTreeSet<_> = out
        .importers
        .get(".")
        .unwrap()
        .iter()
        .map(|d| d.dep_path.clone())
        .collect();
    assert!(importer_keys.contains("consumer@1.0.0(foo@1.0.0)"));
    assert!(importer_keys.contains("consumer@1.0.0(bar@1.0.0)"));
}

// Scoped packages have two `@` chars (scope prefix + version
// separator); the version separator is the rightmost one, so the
// suffix-stripper must use `rfind('@')`. Regression for a bug
// where `find('@')` returned the scope's leading `@` and produced
// malformed keys like `(types/react@18.2.0)`.
#[test]
fn apply_dedupe_peers_to_key_handles_scoped_packages() {
    assert_eq!(
        apply_dedupe_peers_to_key("consumer@1.0.0(@types/react@18.2.0)"),
        "consumer@1.0.0(18.2.0)"
    );
    // Scoped head and scoped peer.
    assert_eq!(
        apply_dedupe_peers_to_key("@foo/bar@1.0.0(@types/react@18.2.0)"),
        "@foo/bar@1.0.0(18.2.0)"
    );
    // Nested scoped peers.
    assert_eq!(
        apply_dedupe_peers_to_key("a@1.0.0(@types/react@18.2.0(@babel/core@7.0.0))"),
        "a@1.0.0(18.2.0(7.0.0))"
    );
}

// Cycle helper sanity check: a value that contains the canonical
// back-ref should be recognized only at proper boundaries, not
// inside longer version strings.
#[test]
fn contains_canonical_back_ref_respects_boundaries() {
    assert!(contains_canonical_back_ref("1.0.0(a@1.0.0)", "a@1.0.0"));
    assert!(contains_canonical_back_ref(
        "1.0.0(a@1.0.0(b@1.0.0))",
        "a@1.0.0"
    ));
    // False positive guard: "a@1.0" should NOT match inside
    // "a@1.0.5" because the following char ('.') is not a boundary.
    assert!(!contains_canonical_back_ref("1.0.0(a@1.0.5)", "a@1.0"));
    // No match when the canonical isn't inside a peer suffix at all.
    assert!(!contains_canonical_back_ref("1.0.0", "a@1.0.0"));
}

// A package whose only dep is another package that declares a peer
// should hoist that peer to the importer — matching pnpm's
// `auto-install-peers=true` default. The hoisted DirectDep carries
// the declared peer range as its specifier.
#[test]
fn hoist_auto_installed_peers_hoists_unmet_peers_to_importer() {
    // consumer declares `peer react: ^17 || ^18` and already has
    // `react@18.2.0` wired via its auto-install dependencies map.
    let mut consumer = mk_locked(
        "consumer",
        "1.0.0",
        &[("react", "18.2.0")],
        &[("react", "^17 || ^18")],
    );
    consumer.dep_path = "consumer@1.0.0".to_string();

    let react = mk_locked("react", "18.2.0", &[], &[]);

    let mut packages = BTreeMap::new();
    packages.insert("consumer@1.0.0".to_string(), consumer);
    packages.insert("react@18.2.0".to_string(), react);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "consumer".to_string(),
            dep_path: "consumer@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let (hoisted, _) = hoist_auto_installed_peers(graph);
    let root = hoisted.importers.get(".").unwrap();

    // Sorted by name → [consumer, react].
    assert_eq!(root.len(), 2);
    assert_eq!(root[0].name, "consumer");
    assert_eq!(root[1].name, "react");
    assert_eq!(root[1].dep_path, "react@18.2.0");
    assert_eq!(root[1].dep_type, DepType::Production);
    // Specifier carries the declared peer range verbatim.
    assert_eq!(root[1].specifier.as_deref(), Some("^17 || ^18"));
}

// Peers declared by transitive dependencies are still resolved and
// sibling-linked by the peer-context pass, but pnpm does not expose
// them as root importer deps or top-level node_modules entries.
#[test]
fn hoist_auto_installed_peers_does_not_hoist_transitive_peers_to_importer() {
    let parent = mk_locked("parent", "1.0.0", &[("consumer", "1.0.0")], &[]);
    let consumer = mk_locked(
        "consumer",
        "1.0.0",
        &[("react", "18.2.0")],
        &[("react", "^17 || ^18")],
    );
    let react = mk_locked("react", "18.2.0", &[], &[]);

    let mut packages = BTreeMap::new();
    packages.insert("parent@1.0.0".to_string(), parent);
    packages.insert("consumer@1.0.0".to_string(), consumer);
    packages.insert("react@18.2.0".to_string(), react);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "parent".to_string(),
            dep_path: "parent@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let (hoisted, _) = hoist_auto_installed_peers(graph);
    let root = hoisted.importers.get(".").unwrap();

    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "parent");
}

// pnpm 11 keeps peers of auto-installed peers contextual to the
// virtual store. If direct dep `consumer` peers on `plugin`, and the
// auto-installed `plugin` peers on `host`, only `plugin` becomes an
// importer dep; `host` is wired later by `apply_peer_contexts`.
#[test]
fn hoist_auto_installed_peers_does_not_hoist_auto_peer_peers_to_importer() {
    let consumer = mk_locked(
        "consumer",
        "1.0.0",
        &[("plugin", "2.0.0")],
        &[("plugin", "^2")],
    );
    let plugin = mk_locked("plugin", "2.0.0", &[("host", "3.0.0")], &[("host", "^3")]);
    let host = mk_locked("host", "3.0.0", &[], &[]);

    let mut packages = BTreeMap::new();
    packages.insert("consumer@1.0.0".to_string(), consumer);
    packages.insert("plugin@2.0.0".to_string(), plugin);
    packages.insert("host@3.0.0".to_string(), host);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "consumer".to_string(),
            dep_path: "consumer@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let (hoisted, _) = hoist_auto_installed_peers(graph);
    let root = hoisted.importers.get(".").unwrap();

    assert_eq!(root.len(), 2);
    assert_eq!(root[0].name, "consumer");
    assert_eq!(root[1].name, "plugin");
    assert_eq!(root[1].dep_path, "plugin@2.0.0");
}

// If the peer is already in the importer's direct deps, hoist is a
// no-op — we don't duplicate or shadow the user's own specifier.
#[test]
fn hoist_auto_installed_peers_leaves_already_satisfied_peers_alone() {
    let consumer = mk_locked(
        "consumer",
        "1.0.0",
        &[("react", "17.0.2")],
        &[("react", "^17 || ^18")],
    );
    let react = mk_locked("react", "17.0.2", &[], &[]);

    let mut packages = BTreeMap::new();
    packages.insert("consumer@1.0.0".to_string(), consumer);
    packages.insert("react@17.0.2".to_string(), react);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "consumer".to_string(),
                dep_path: "consumer@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            },
            DirectDep {
                name: "react".to_string(),
                dep_path: "react@17.0.2".to_string(),
                dep_type: DepType::Production,
                specifier: Some("17.0.2".to_string()),
            },
        ],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let (hoisted, _) = hoist_auto_installed_peers(graph);
    let root = hoisted.importers.get(".").unwrap();

    // Still just the two original entries — no extra react snuck in.
    assert_eq!(root.len(), 2);
    let react_dep = root.iter().find(|d| d.name == "react").unwrap();
    // The user's own pin (17.0.2) survives — not clobbered by the
    // peer range.
    assert_eq!(react_dep.specifier.as_deref(), Some("17.0.2"));
}

// The hoist is scaffolding for the peer-context pass; once stripped,
// the importer must mirror the manifest exactly — pnpm 10 never writes
// auto-installed peers as importer specifiers (`pnpm install
// --frozen-lockfile` rejects a lockfile that has them), and the
// resolved peer must survive in `packages` for snapshot wiring.
#[test]
fn remove_auto_installed_peers_restores_manifest_shaped_importers() {
    let mut consumer = mk_locked(
        "consumer",
        "1.0.0",
        &[("react", "18.2.0")],
        &[("react", "^17 || ^18")],
    );
    consumer.dep_path = "consumer@1.0.0".to_string();
    let react = mk_locked("react", "18.2.0", &[], &[]);

    let mut packages = BTreeMap::new();
    packages.insert("consumer@1.0.0".to_string(), consumer);
    packages.insert("react@18.2.0".to_string(), react);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "consumer".to_string(),
            dep_path: "consumer@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let (mut hoisted, auto_installed) = hoist_auto_installed_peers(graph);

    // The hoist reports exactly what it added: react into ".".
    assert_eq!(auto_installed.len(), 1, "expected one importer touched");
    assert!(auto_installed.get(".").unwrap().contains("react"));
    assert_eq!(hoisted.importers.get(".").unwrap().len(), 2);

    remove_auto_installed_peers(&mut hoisted, &auto_installed);

    // Importer is back to the manifest's single direct dep…
    let root = hoisted.importers.get(".").unwrap();
    assert_eq!(root.len(), 1, "auto-installed peer must not stay hoisted");
    assert_eq!(root[0].name, "consumer");
    // …while the resolved peer stays in the package graph for the
    // peer-context/snapshot wiring.
    assert!(hoisted.packages.contains_key("react@18.2.0"));
}

// `detect_unmet_peers` should flag a package whose declared peer
// range isn't satisfied by whatever the graph ends up providing.
// This is the core case: user pins `react@15.7.0`, a consumer
// declares `peer react: ^18`, and we need a warning so the user
// knows their runtime will break.
#[test]
fn detect_unmet_peers_flags_version_mismatch() {
    let mut consumer = mk_locked(
        "consumer",
        "1.0.0",
        &[("react", "15.7.0")],
        &[("react", "^18")],
    );
    consumer.dep_path = "consumer@1.0.0(react@15.7.0)".to_string();

    let mut packages = BTreeMap::new();
    packages.insert(consumer.dep_path.clone(), consumer);

    let graph = LockfileGraph {
        importers: BTreeMap::new(),
        packages,
        ..Default::default()
    };

    let unmet = detect_unmet_peers(&graph);
    assert_eq!(unmet.len(), 1, "expected one unmet peer, got {unmet:?}");
    let u = &unmet[0];
    assert_eq!(u.from_name, "consumer");
    assert_eq!(u.peer_name, "react");
    assert_eq!(u.declared, "^18");
    assert_eq!(u.found.as_deref(), Some("15.7.0"));
}

// When the resolved version *does* satisfy the declared range, no
// warning should fire.
#[test]
fn detect_unmet_peers_silent_when_satisfied() {
    let mut consumer = mk_locked(
        "consumer",
        "1.0.0",
        &[("react", "18.2.0")],
        &[("react", "^17 || ^18")],
    );
    consumer.dep_path = "consumer@1.0.0(react@18.2.0)".to_string();

    let mut packages = BTreeMap::new();
    packages.insert(consumer.dep_path.clone(), consumer);

    let graph = LockfileGraph {
        importers: BTreeMap::new(),
        packages,
        ..Default::default()
    };
    assert!(detect_unmet_peers(&graph).is_empty());
}

// Peer declared but completely absent from `pkg.dependencies` —
// exercises the `found: None` branch that drives the "missing
// required peer" display path in `check_unmet_peers`. Rare in
// practice because the BFS peer walk usually drags *some* version
// in, but possible for corner cases (registry fetch failure, etc).
#[test]
fn detect_unmet_peers_flags_completely_missing_peer() {
    let mut consumer = mk_locked("consumer", "1.0.0", &[], &[("react", "^18")]);
    consumer.dep_path = "consumer@1.0.0".to_string();

    let mut packages = BTreeMap::new();
    packages.insert(consumer.dep_path.clone(), consumer);

    let graph = LockfileGraph {
        importers: BTreeMap::new(),
        packages,
        ..Default::default()
    };

    let unmet = detect_unmet_peers(&graph);
    assert_eq!(unmet.len(), 1);
    let u = &unmet[0];
    assert_eq!(u.from_name, "consumer");
    assert_eq!(u.peer_name, "react");
    assert_eq!(u.declared, "^18");
    assert_eq!(u.found, None);
}

// Optional peers are suppressed even when they would otherwise be
// flagged — matches pnpm's `peerDependenciesMeta.optional` behavior
// with `auto-install-peers=true`.
#[test]
fn detect_unmet_peers_skips_optional_peers() {
    let mut consumer = mk_locked(
        "consumer",
        "1.0.0",
        &[("react", "15.7.0")],
        &[("react", "^18")],
    );
    consumer.dep_path = "consumer@1.0.0(react@15.7.0)".to_string();
    consumer.peer_dependencies_meta.insert(
        "react".to_string(),
        aube_lockfile::PeerDepMeta { optional: true },
    );

    let mut packages = BTreeMap::new();
    packages.insert(consumer.dep_path.clone(), consumer);

    let graph = LockfileGraph {
        importers: BTreeMap::new(),
        packages,
        ..Default::default()
    };
    assert!(detect_unmet_peers(&graph).is_empty());
}

// Mutual dependency cycles must not hang the BFS resolver. The
// walker dedupes on `name@version`, so the second time the cycle
// brings us back to a package we already resolved, we wire the
// parent edge but skip recursing into its transitives.
//
//     cycle-a@1.0.0 -> cycle-b@1.0.0
//     cycle-b@1.0.0 -> cycle-a@1.0.0
#[tokio::test]
async fn resolve_terminates_on_dependency_cycle() {
    let mut a = make_packument("cycle-a", &["1.0.0"], "1.0.0");
    a.versions
        .get_mut("1.0.0")
        .unwrap()
        .dependencies
        .insert("cycle-b".to_string(), "1.0.0".to_string());
    let mut b = make_packument("cycle-b", &["1.0.0"], "1.0.0");
    b.versions
        .get_mut("1.0.0")
        .unwrap()
        .dependencies
        .insert("cycle-a".to_string(), "1.0.0".to_string());

    // The RegistryClient is never hit because we pre-seed the cache.
    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    resolver.cache.insert("cycle-a".to_string(), a);
    resolver.cache.insert("cycle-b".to_string(), b);

    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("cycle-a".to_string(), "1.0.0".to_string());

    let graph = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        resolver.resolve(&manifest, None),
    )
    .await
    .expect("resolver hung on dependency cycle")
    .expect("resolve failed");

    assert!(graph.packages.contains_key("cycle-a@1.0.0"));
    assert!(graph.packages.contains_key("cycle-b@1.0.0"));
    assert_eq!(
        graph.packages["cycle-a@1.0.0"].dependencies.get("cycle-b"),
        Some(&"1.0.0".to_string())
    );
    assert_eq!(
        graph.packages["cycle-b@1.0.0"].dependencies.get("cycle-a"),
        Some(&"1.0.0".to_string())
    );
}

// pnpm v11.5.0 fixed a dep-path calculation hang for an npm alias
// participating in a peer cycle. Keep the fixture small but preserve
// the shape: root installs `alias-a -> npm:real-a`, `real-a` peers on
// `b`, and `b` peers back on the alias name while depending on the
// same alias.
#[tokio::test]
async fn resolve_terminates_on_npm_alias_peer_cycle() {
    let mut real_a = make_packument("real-a", &["1.0.0"], "1.0.0");
    let real_a_meta = real_a.versions.get_mut("1.0.0").unwrap();
    real_a_meta
        .dependencies
        .insert("b".to_string(), "1.0.0".to_string());
    real_a_meta
        .peer_dependencies
        .insert("b".to_string(), "^1.0.0".to_string());

    let mut b = make_packument("b", &["1.0.0"], "1.0.0");
    let b_meta = b.versions.get_mut("1.0.0").unwrap();
    b_meta
        .dependencies
        .insert("alias-a".to_string(), "npm:real-a@1.0.0".to_string());
    b_meta
        .peer_dependencies
        .insert("alias-a".to_string(), "1.0.0".to_string());

    // The RegistryClient is never hit because we pre-seed the cache.
    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    resolver.cache.insert("real-a".to_string(), real_a);
    resolver.cache.insert("b".to_string(), b);

    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("alias-a".to_string(), "npm:real-a@1.0.0".to_string());
    manifest
        .dependencies
        .insert("b".to_string(), "1.0.0".to_string());

    let graph = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        resolver.resolve(&manifest, None),
    )
    .await
    .expect("resolver hung on npm-alias peer cycle")
    .expect("resolve failed");

    let alias_dep_path = graph
        .importers
        .get(".")
        .and_then(|deps| deps.iter().find(|dep| dep.name == "alias-a"))
        .map(|dep| dep.dep_path.as_str())
        .expect("root alias dependency should be present");
    let alias_pkg = graph
        .packages
        .get(alias_dep_path)
        .expect("alias package should be keyed by the alias dep_path");
    assert_eq!(alias_pkg.name, "alias-a");
    assert_eq!(alias_pkg.version, "1.0.0");
    assert_eq!(alias_pkg.alias_of.as_deref(), Some("real-a"));
    assert_eq!(alias_pkg.registry_name(), "real-a");
    assert!(
        graph
            .packages
            .keys()
            .all(|dep_path| !dep_path.starts_with("real-a@")),
        "alias package should not leak a real-name dep_path"
    );
    assert!(
        graph.packages.values().any(|pkg| pkg.name == "b"),
        "peer package should resolve"
    );
}

#[tokio::test]
async fn auto_install_peers_installs_missing_required_peer() {
    let mut consumer = make_packument("consumer", &["1.0.0"], "1.0.0");
    consumer
        .versions
        .get_mut("1.0.0")
        .unwrap()
        .peer_dependencies
        .insert("react".to_string(), "^18".to_string());
    let react = make_packument("react", &["18.2.0"], "18.2.0");

    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    resolver.cache.insert("consumer".to_string(), consumer);
    resolver.cache.insert("react".to_string(), react);

    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("consumer".to_string(), "1.0.0".to_string());

    let graph = resolver
        .resolve(&manifest, None)
        .await
        .expect("resolve failed");

    assert!(graph_has_package(&graph, "consumer", "1.0.0"));
    assert!(
        graph_has_package(&graph, "react", "18.2.0"),
        "missing required peer should be auto-installed"
    );
}

#[tokio::test]
async fn importer_peer_dependencies_are_seeded_as_direct_deps() {
    let zod = make_packument("zod", &["3.22.0"], "3.22.0");

    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    resolver.cache.insert("zod".to_string(), zod);

    let root = PackageJson::default();
    let mut ui = PackageJson::default();
    ui.peer_dependencies
        .insert("zod".to_string(), "^3.22.0".to_string());

    let graph = resolver
        .resolve_workspace(
            &[(".".to_string(), root), ("packages/ui".to_string(), ui)],
            None,
            &std::collections::HashMap::new(),
        )
        .await
        .expect("resolve failed");

    let ui_deps = graph
        .importers
        .get("packages/ui")
        .expect("packages/ui importer");
    let zod_dep = ui_deps
        .iter()
        .find(|dep| dep.name == "zod")
        .expect("peer dependency should be a direct dep");
    assert_eq!(zod_dep.dep_type, DepType::Production);
    assert_eq!(zod_dep.specifier.as_deref(), Some("^3.22.0"));
    assert_eq!(zod_dep.dep_path, "zod@3.22.0");
}

#[tokio::test]
async fn auto_install_peers_false_skips_importer_peer_dependencies() {
    let zod = make_packument("zod", &["3.22.0"], "3.22.0");

    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client).with_auto_install_peers(false);
    resolver.cache.insert("zod".to_string(), zod);

    let mut ui = PackageJson::default();
    ui.peer_dependencies
        .insert("zod".to_string(), "^3.22.0".to_string());

    let graph = resolver
        .resolve_workspace(
            &[("packages/ui".to_string(), ui)],
            None,
            &std::collections::HashMap::new(),
        )
        .await
        .expect("resolve failed");

    assert!(graph.importers["packages/ui"].is_empty());
    assert!(
        !graph_has_package(&graph, "zod", "3.22.0"),
        "auto-install-peers=false should not install importer peers"
    );
}

#[tokio::test]
async fn importer_optional_peer_dependencies_are_not_seeded() {
    let zod = make_packument("zod", &["3.22.0"], "3.22.0");

    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    resolver.cache.insert("zod".to_string(), zod);

    let mut ui = PackageJson::default();
    ui.peer_dependencies
        .insert("zod".to_string(), "^3.22.0".to_string());
    ui.extra.insert(
        "peerDependenciesMeta".to_string(),
        serde_json::json!({"zod": {"optional": true}}),
    );

    let graph = resolver
        .resolve_workspace(
            &[("packages/ui".to_string(), ui)],
            None,
            &std::collections::HashMap::new(),
        )
        .await
        .expect("resolve failed");

    assert!(graph.importers["packages/ui"].is_empty());
    assert!(
        !graph_has_package(&graph, "zod", "3.22.0"),
        "optional importer peers should not be auto-installed"
    );
}

#[tokio::test]
async fn importer_peer_dependencies_do_not_duplicate_declared_deps() {
    let zod = make_packument("zod", &["3.22.0", "3.23.0"], "3.23.0");

    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    resolver.cache.insert("zod".to_string(), zod);

    let mut ui = PackageJson::default();
    ui.dependencies
        .insert("zod".to_string(), "3.22.0".to_string());
    ui.peer_dependencies
        .insert("zod".to_string(), "^3.0.0".to_string());

    let graph = resolver
        .resolve_workspace(
            &[("packages/ui".to_string(), ui)],
            None,
            &std::collections::HashMap::new(),
        )
        .await
        .expect("resolve failed");

    let ui_deps = graph.importers.get("packages/ui").unwrap();
    assert_eq!(ui_deps.len(), 1);
    assert_eq!(ui_deps[0].name, "zod");
    assert_eq!(ui_deps[0].specifier.as_deref(), Some("3.22.0"));
    assert_eq!(ui_deps[0].dep_path, "zod@3.22.0");
}

#[tokio::test]
async fn importer_peer_dependencies_resolve_per_importer_range() {
    let host = make_packument("host", &["1.0.0", "2.0.0"], "2.0.0");

    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    resolver.cache.insert("host".to_string(), host);

    let mut one = PackageJson::default();
    one.peer_dependencies
        .insert("host".to_string(), "^1.0.0".to_string());
    let mut two = PackageJson::default();
    two.peer_dependencies
        .insert("host".to_string(), "^2.0.0".to_string());

    let graph = resolver
        .resolve_workspace(
            &[
                ("packages/one".to_string(), one),
                ("packages/two".to_string(), two),
            ],
            None,
            &std::collections::HashMap::new(),
        )
        .await
        .expect("resolve failed");

    assert_eq!(graph.importers["packages/one"][0].dep_path, "host@1.0.0");
    assert_eq!(graph.importers["packages/two"][0].dep_path, "host@2.0.0");
}

#[tokio::test]
async fn auto_install_peers_uses_importer_declared_peer_name_without_extra_version() {
    let mut plugin = make_packument("plugin", &["1.0.0"], "1.0.0");
    plugin
        .versions
        .get_mut("1.0.0")
        .unwrap()
        .peer_dependencies
        .insert("eslint".to_string(), "^8.56.0".to_string());
    let eslint = make_packument("eslint", &["8.57.1", "9.0.0"], "9.0.0");

    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    resolver.cache.insert("plugin".to_string(), plugin);
    resolver.cache.insert("eslint".to_string(), eslint);

    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("eslint".to_string(), "^9".to_string());
    manifest
        .dependencies
        .insert("plugin".to_string(), "1.0.0".to_string());

    let graph = resolver
        .resolve(&manifest, None)
        .await
        .expect("resolve failed");

    assert!(graph_has_package(&graph, "eslint", "9.0.0"));
    assert!(graph_has_package(&graph, "plugin", "1.0.0"));
    assert!(
        !graph_has_package(&graph, "eslint", "8.57.1"),
        "importer-declared peer name should not pull a second compatible peer tree"
    );
    let unmet = detect_unmet_peers(&graph);
    assert!(
        unmet.iter().any(|unmet| unmet.from_name == "plugin"
            && unmet.peer_name == "eslint"
            && unmet.declared == "^8.56.0"
            && unmet.found.as_deref() == Some("9.0.0")),
        "incompatible importer peer should surface as a version-mismatch warning"
    );
}

#[tokio::test]
async fn auto_install_peers_skips_unrequested_optional_peer_alternatives() {
    let mut loader = make_packument("loader", &["1.0.0"], "1.0.0");
    let loader_meta = loader.versions.get_mut("1.0.0").unwrap();
    loader_meta
        .peer_dependencies
        .insert("sass".to_string(), "^1".to_string());
    loader_meta
        .peer_dependencies
        .insert("webpack".to_string(), "^5".to_string());
    loader_meta
        .peer_dependencies
        .insert("@rspack/core".to_string(), "^1".to_string());
    loader_meta
        .peer_dependencies
        .insert("node-sass".to_string(), "^9".to_string());
    loader_meta.peer_dependencies_meta.insert(
        "@rspack/core".to_string(),
        aube_registry::PeerDepMeta { optional: true },
    );
    loader_meta.peer_dependencies_meta.insert(
        "node-sass".to_string(),
        aube_registry::PeerDepMeta { optional: true },
    );

    let sass = make_packument("sass", &["1.69.0"], "1.69.0");
    let webpack = make_packument("webpack", &["5.0.0"], "5.0.0");
    let rspack = make_packument("@rspack/core", &["1.0.0"], "1.0.0");
    let node_sass = make_packument("node-sass", &["9.0.0"], "9.0.0");

    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    resolver.cache.insert("loader".to_string(), loader);
    resolver.cache.insert("sass".to_string(), sass);
    resolver.cache.insert("webpack".to_string(), webpack);
    resolver.cache.insert("@rspack/core".to_string(), rspack);
    resolver.cache.insert("node-sass".to_string(), node_sass);

    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("loader".to_string(), "1.0.0".to_string());
    manifest
        .dependencies
        .insert("sass".to_string(), "^1".to_string());
    manifest
        .dependencies
        .insert("webpack".to_string(), "^5".to_string());

    let graph = resolver
        .resolve(&manifest, None)
        .await
        .expect("resolve failed");

    assert!(graph_has_package(&graph, "loader", "1.0.0"));
    assert!(graph_has_package(&graph, "sass", "1.69.0"));
    assert!(graph_has_package(&graph, "webpack", "5.0.0"));
    assert!(
        !graph_has_package(&graph, "@rspack/core", "1.0.0"),
        "optional peer alternative should not be auto-installed"
    );
    assert!(
        !graph_has_package(&graph, "node-sass", "9.0.0"),
        "optional peer alternative should not be auto-installed"
    );
}

// Scenario test for the bug Cursor Bugbot flagged on #142:
// lockfile has `dep-a@1.0.0`; manifest wants both `dep-a@^1`
// (matches lockfile) AND `other-a@^2` (fresh); `other-a@2.0.0`
// declares a transitive `dep-a@^2` that no lockfile entry
// satisfies.
//
// Correct behavior: resolver picks dep-a@1.0.0 for the direct
// dep (via lockfile reuse) and dep-a@2.0.0 for the transitive
// (via the fetch path).
//
// The original bug: `ensure_fetch!` wrongly skipped the spawn
// when `resolved_versions[dep-a]` was non-empty, regardless of
// whether the packument was actually in `self.cache`. The
// lockfile-reuse path populates `resolved_versions` without
// ever caching the packument, so the transitive dep-a@^2 task
// fell through to the fetch-wait loop, called `ensure_fetch!`,
// got skipped, and panicked with "packument fetch disappeared
// before completing". The fix removes the `resolved_versions`
// guard from `ensure_fetch!` — the macro now checks only
// in-flight + cache, and prefetch gating on lockfile-covered
// names is done by callers via an explicit `existing_names`
// check.
//
// Note: this test pre-seeds the resolver cache with both
// packuments, so the wait-for-fetch loop exits immediately
// without actually calling `ensure_fetch!` — which means the
// test passes with or without the fix. It's kept as an
// end-to-end scenario assertion (resolver produces the
// expected two-version graph) rather than a direct regression
// test for the `ensure_fetch!` bug itself. Triggering the
// actual bug requires a real registry mock that returns the
// packument during the wait loop, which the unit-test harness
// doesn't have; the BATS suite covers the end-to-end path
// through a local Verdaccio registry.
#[tokio::test]
async fn resolve_handles_lockfile_reused_name_with_incompatible_transitive_range() {
    // Packument for `dep-a` has both a 1.x and a 2.x line; only
    // 1.0.0 is in the (fake) lockfile, so the fetch path has to
    // cover the 2.x case.
    let dep_a = make_packument("dep-a", &["1.0.0", "2.0.0"], "2.0.0");
    // `other-a@2.0.0` is the package that triggers the
    // transitive `dep-a@^2` task.
    let mut other_a = make_packument("other-a", &["2.0.0"], "2.0.0");
    other_a
        .versions
        .get_mut("2.0.0")
        .unwrap()
        .dependencies
        .insert("dep-a".to_string(), "^2".to_string());

    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    // Pre-seed the in-memory packument cache so the resolver
    // never needs to touch the fake registry URL.
    resolver.cache.insert("dep-a".to_string(), dep_a);
    resolver.cache.insert("other-a".to_string(), other_a);

    // Existing lockfile: has `dep-a@1.0.0` (the lockfile-reuse
    // hit) but nothing else. `other-a@^2` is a fresh dep that
    // won't lockfile-reuse.
    let mut existing_pkgs: BTreeMap<String, LockedPackage> = BTreeMap::new();
    existing_pkgs.insert(
        "dep-a@1.0.0".to_string(),
        LockedPackage {
            name: "dep-a".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "dep-a@1.0.0".to_string(),
            ..Default::default()
        },
    );
    let existing = LockfileGraph {
        packages: existing_pkgs,
        importers: BTreeMap::new(),
        settings: Default::default(),
        overrides: BTreeMap::new(),
        package_extensions_checksum: None,
        pnpmfile_checksum: None,
        ignored_optional_dependencies: BTreeSet::new(),
        times: BTreeMap::new(),
        skipped_optional_dependencies: BTreeMap::new(),
        catalogs: BTreeMap::new(),
        bun_config_version: None,
        patched_dependencies: BTreeMap::new(),
        patched_dependency_hashes: BTreeMap::new(),
        trusted_dependencies: Vec::new(),
        runtimes: BTreeMap::new(),
        extra_fields: BTreeMap::new(),
        workspace_extra_fields: BTreeMap::new(),
    };

    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("dep-a".to_string(), "^1".to_string());
    manifest
        .dependencies
        .insert("other-a".to_string(), "^2".to_string());

    let graph = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        resolver.resolve(&manifest, Some(&existing)),
    )
    .await
    .expect("resolver hung")
    .expect("resolve failed");

    // Both versions of dep-a should be in the resolved graph:
    // 1.0.0 from lockfile-reuse, 2.0.0 from the fetch path.
    assert!(
        graph.packages.contains_key("dep-a@1.0.0"),
        "dep-a@1.0.0 missing (lockfile reuse)"
    );
    assert!(
        graph.packages.contains_key("dep-a@2.0.0"),
        "dep-a@2.0.0 missing (transitive fetch fell through the ensure_fetch guard)"
    );
    assert!(graph.packages.contains_key("other-a@2.0.0"));
}

#[tokio::test]
async fn lockfile_reuse_preserves_transitive_optional_edges() {
    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);

    let mut existing_pkgs: BTreeMap<String, LockedPackage> = BTreeMap::new();
    existing_pkgs.insert(
        "host@1.0.0".to_string(),
        LockedPackage {
            name: "host".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "host@1.0.0".to_string(),
            dependencies: [("native".to_string(), "1.0.0".to_string())].into(),
            optional_dependencies: [("native".to_string(), "1.0.0".to_string())].into(),
            ..Default::default()
        },
    );
    existing_pkgs.insert(
        "native@1.0.0".to_string(),
        LockedPackage {
            name: "native".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "native@1.0.0".to_string(),
            ..Default::default()
        },
    );
    let existing = LockfileGraph {
        packages: existing_pkgs,
        importers: BTreeMap::new(),
        settings: Default::default(),
        overrides: BTreeMap::new(),
        package_extensions_checksum: None,
        pnpmfile_checksum: None,
        ignored_optional_dependencies: BTreeSet::new(),
        times: BTreeMap::new(),
        skipped_optional_dependencies: BTreeMap::new(),
        catalogs: BTreeMap::new(),
        bun_config_version: None,
        patched_dependencies: BTreeMap::new(),
        patched_dependency_hashes: BTreeMap::new(),
        trusted_dependencies: Vec::new(),
        runtimes: BTreeMap::new(),
        extra_fields: BTreeMap::new(),
        workspace_extra_fields: BTreeMap::new(),
    };

    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("host".to_string(), "1.0.0".to_string());

    let graph = resolver
        .resolve(&manifest, Some(&existing))
        .await
        .expect("resolve failed");

    let host = graph.packages.get("host@1.0.0").unwrap();
    assert_eq!(host.dependencies.get("native").unwrap(), "1.0.0");
    assert_eq!(
        host.optional_dependencies.get("native").unwrap(),
        "1.0.0",
        "lockfile reuse must keep the optional edge metadata for write()"
    );
}

// Bun and yarn parsers store transitive deps in `pkg.dependencies`
// using the full dep_path form (`is-number@6.0.0`), while pnpm uses
// bare versions (`6.0.0`). The resolver's lockfile-reuse path
// previously used the dep value verbatim as a semver range, which
// hard-failed on bun/yarn lockfiles with a malformed range like
// `is-number@6.0.0`. Strip the `name@` prefix before treating the
// value as a range.
#[tokio::test]
async fn lockfile_reuse_handles_name_at_version_dep_form() {
    let is_number = make_packument("is-number", &["6.0.0", "7.0.0"], "7.0.0");
    let mut is_odd = make_packument("is-odd", &["3.0.1"], "3.0.1");
    is_odd
        .versions
        .get_mut("3.0.1")
        .unwrap()
        .dependencies
        .insert("is-number".to_string(), "^6.0.0".to_string());

    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    resolver.cache.insert("is-number".to_string(), is_number);
    resolver.cache.insert("is-odd".to_string(), is_odd);

    // Mimic the bun/yarn parser: `dependencies` value is the full
    // dep_path, not a bare version.
    let mut existing_pkgs: BTreeMap<String, LockedPackage> = BTreeMap::new();
    existing_pkgs.insert(
        "is-odd@3.0.1".to_string(),
        LockedPackage {
            name: "is-odd".to_string(),
            version: "3.0.1".to_string(),
            dep_path: "is-odd@3.0.1".to_string(),
            dependencies: [("is-number".to_string(), "is-number@6.0.0".to_string())].into(),
            ..Default::default()
        },
    );
    existing_pkgs.insert(
        "is-number@6.0.0".to_string(),
        LockedPackage {
            name: "is-number".to_string(),
            version: "6.0.0".to_string(),
            dep_path: "is-number@6.0.0".to_string(),
            ..Default::default()
        },
    );
    let existing = LockfileGraph {
        packages: existing_pkgs,
        ..Default::default()
    };

    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("is-odd".to_string(), "3.0.1".to_string());

    let graph = resolver
        .resolve(&manifest, Some(&existing))
        .await
        .expect("resolve failed");

    assert!(graph_has_package(&graph, "is-odd", "3.0.1"));
    assert!(
        graph_has_package(&graph, "is-number", "6.0.0"),
        "transitive must reuse the locked 6.0.0, not fail or pick 7.0.0"
    );
}

// Regression for the pnpm-11 monorepo install blocker (vuejs/core): a
// registry package whose peer is satisfied by a WORKSPACE member is
// recorded by pnpm as a `link:` transitive in the parent's lockfile
// deps (e.g. `@vitejs/plugin-vue`'s `vue: link:packages/vue`). On
// lockfile reuse that `link:` task reaches the resolver as a transitive
// from a registry parent — the default-on `blockExoticSubdeps` guard
// wrongly treated it as an untrusted exotic dep and HARD-FAILED the
// whole install. A `link:`/`portal:` whose NAME is a workspace member
// is a first-party workspace link (the workspace is the trust
// boundary), so it must bind to the local member, not be blocked.
#[tokio::test]
async fn workspace_link_transitive_from_registry_parent_is_not_blocked_as_exotic() {
    let plugin_vue = make_packument("@vitejs/plugin-vue", &["6.0.7"], "6.0.7");

    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    // Default policy: block_exotic_subdeps = true (the trip wire).
    assert!(resolver.dependency_policy.block_exotic_subdeps);
    resolver
        .cache
        .insert("@vitejs/plugin-vue".to_string(), plugin_vue);

    // Existing lockfile: the registry plugin-vue resolved its `vue` peer
    // to the workspace member, serialized as `link:packages/vue`.
    let mut existing_pkgs: BTreeMap<String, LockedPackage> = BTreeMap::new();
    existing_pkgs.insert(
        "@vitejs/plugin-vue@6.0.7".to_string(),
        LockedPackage {
            name: "@vitejs/plugin-vue".to_string(),
            version: "6.0.7".to_string(),
            dep_path: "@vitejs/plugin-vue@6.0.7".to_string(),
            dependencies: [("vue".to_string(), "link:packages/vue".to_string())].into(),
            ..Default::default()
        },
    );
    let existing = LockfileGraph {
        packages: existing_pkgs,
        ..Default::default()
    };

    // Root imports plugin-vue; `vue` is a workspace member.
    let root = PackageJson {
        dependencies: [("@vitejs/plugin-vue".to_string(), "6.0.7".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let vue_member = PackageJson {
        name: Some("vue".to_string()),
        version: Some("3.5.38".to_string()),
        ..Default::default()
    };

    let mut workspace_packages = std::collections::HashMap::new();
    workspace_packages.insert("vue".to_string(), "3.5.38".to_string());

    let graph = resolver
        .resolve_workspace(
            &[
                (".".to_string(), root),
                ("packages/vue".to_string(), vue_member),
            ],
            Some(&existing),
            &workspace_packages,
        )
        .await
        .expect("workspace link peer must not be blocked as an exotic subdep");

    assert!(
        graph_has_package(&graph, "@vitejs/plugin-vue", "6.0.7"),
        "plugin-vue should resolve"
    );
    // The `vue` peer must bind to the workspace member version, not error.
    let plugin = graph
        .packages
        .get("@vitejs/plugin-vue@6.0.7")
        .expect("plugin-vue in graph");
    assert_eq!(
        plugin.dependencies.get("vue").map(String::as_str),
        Some("3.5.38"),
        "plugin-vue's vue peer must bind to the workspace member version"
    );
}

// A fresh resolve must stash the packument's `deprecated` reason on the
// LockedPackage (via `extra_meta`) so the pnpm/aube lockfile writer can
// emit pnpm's `deprecated:` field. Without this the reason is dropped
// and aube's lockfile drifts from pnpm's for every deprecated dep.
#[tokio::test]
async fn fresh_resolve_records_deprecated_reason_on_extra_meta() {
    let mut foo = make_packument("foo", &["1.0.0"], "1.0.0");
    foo.versions.get_mut("1.0.0").unwrap().deprecated = Some("use bar instead".to_string());

    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    resolver.cache.insert("foo".to_string(), foo);

    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("foo".to_string(), "1.0.0".to_string());

    let graph = resolver
        .resolve(&manifest, None)
        .await
        .expect("resolve failed");

    let foo_pkg = graph.packages.get("foo@1.0.0").expect("foo resolved");
    assert_eq!(
        foo_pkg
            .extra_meta
            .get("deprecated")
            .and_then(|v| v.as_str()),
        Some("use bar instead"),
        "fresh resolve must record the deprecation reason on extra_meta"
    );
}

// `allowedDeprecatedVersions` only silences the install *warning* — it
// must not strip the `deprecated:` field from the lockfile. pnpm keeps
// recording the reason on the package entry regardless, so the resolver
// stores the raw packument message rather than the warning-gated one.
#[tokio::test]
async fn deprecated_reason_recorded_even_when_warning_suppressed() {
    let mut foo = make_packument("foo", &["1.0.0"], "1.0.0");
    foo.versions.get_mut("1.0.0").unwrap().deprecated = Some("use bar instead".to_string());

    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client).with_dependency_policy(DependencyPolicy {
        allowed_deprecated_versions: [("foo".to_string(), "*".to_string())].into_iter().collect(),
        ..Default::default()
    });
    resolver.cache.insert("foo".to_string(), foo);

    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("foo".to_string(), "1.0.0".to_string());

    let graph = resolver
        .resolve(&manifest, None)
        .await
        .expect("resolve failed");

    let foo_pkg = graph.packages.get("foo@1.0.0").expect("foo resolved");
    assert_eq!(
        foo_pkg
            .extra_meta
            .get("deprecated")
            .and_then(|v| v.as_str()),
        Some("use bar instead"),
        "lockfile must record deprecated even when the warning is suppressed"
    );
}

// ===== peersSuffixMaxLength =====
//
// Helpers exercised directly: `effective_peer_suffix` for the pnpm
// format invariant; `apply_peer_contexts` for the integration path
// that reads the cap and decides whether to swap the suffix.

#[test]
fn effective_peer_suffix_hashes_to_pnpm_parenthesized_form() {
    // Cap of 0 forces hashing of any non-empty body. pnpm's
    // `createPeerDepGraphHash` wraps the short hash in a single
    // `(...)`, never a bare `_<hex>` marker.
    let out = effective_peer_suffix("(react@18.2.0)", 0);
    assert!(
        out.starts_with('(') && out.ends_with(')'),
        "parens: {out:?}"
    );
    let inner = &out[1..out.len() - 1];
    // `createShortHash` = sha256 hex truncated to 32 chars.
    assert_eq!(inner.len(), 32, "expected 32 hex chars: {out:?}");
    assert!(
        inner
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "expected lowercase hex inside parens: {out:?}"
    );
    // The hash input is the body (suffix without the outer parens),
    // matching pnpm's `dirName = peers.join(')(')`.
    assert_eq!(
        out,
        effective_peer_suffix("(react@18.2.0)", 0),
        "stable output"
    );
    assert!(is_hashed_peer_suffix(&out), "must be recognized as hashed");
}

#[test]
fn effective_peer_suffix_is_identity_within_cap() {
    // Within the cap the suffix is returned verbatim (the common case).
    let suffix = "(react@18.2.0)(redux@4.0.5)";
    assert_eq!(effective_peer_suffix(suffix, 1000), suffix);
    assert!(!is_hashed_peer_suffix(suffix), "real peers aren't hashed");
}

// Small cap forces the suffix to collapse to `(<short-hash>)`. Uses the
// nested-peer fixture that already proves correct behavior at the
// default cap — same fixture, different cap, different output.
#[test]
fn peer_suffix_is_hashed_when_exceeding_cap() {
    let mut consumer = mk_locked(
        "consumer",
        "1.0.0",
        &[("adapter", "1.0.0"), ("core", "1.0.0")],
        &[("adapter", "^1"), ("core", "^1")],
    );
    consumer.dep_path = "consumer@1.0.0".to_string();
    let mut adapter = mk_locked("adapter", "1.0.0", &[("core", "1.0.0")], &[("core", "^1")]);
    adapter.dep_path = "adapter@1.0.0".to_string();
    let core = mk_locked("core", "1.0.0", &[], &[]);

    let mut packages = BTreeMap::new();
    packages.insert("consumer@1.0.0".to_string(), consumer);
    packages.insert("adapter@1.0.0".to_string(), adapter);
    packages.insert("core@1.0.0".to_string(), core);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "consumer".to_string(),
            dep_path: "consumer@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    // Cap of 10 bytes is smaller than any realistic suffix.
    let options = PeerContextOptions {
        peers_suffix_max_length: 10,
        ..PeerContextOptions::default()
    };
    let out = apply_peer_contexts(graph, &options).expect("test graph should converge");

    // At least one package should have a hashed suffix. The outer
    // `consumer` package is the one most likely to overflow (nested
    // suffix `(adapter@1.0.0(core@1.0.0))(core@1.0.0)` = 42 bytes).
    let consumer_key = out
        .packages
        .keys()
        .find(|k| k.starts_with("consumer@1.0.0"))
        .cloned()
        .expect("consumer@1.0.0 variant missing");
    let suffix = consumer_key.strip_prefix("consumer@1.0.0").unwrap();
    assert!(
        is_hashed_peer_suffix(suffix),
        "expected parenthesized hashed suffix (<32-hex>), got {suffix:?} from {consumer_key:?}"
    );
}

// Default cap leaves the nested form byte-identical to pre-cap output.
// Regression guard: the wiring must not change behavior when the cap
// isn't hit — which is the overwhelmingly common case.
#[test]
fn peer_suffix_unchanged_when_within_cap() {
    let mut consumer = mk_locked(
        "consumer",
        "1.0.0",
        &[("adapter", "1.0.0"), ("core", "1.0.0")],
        &[("adapter", "^1"), ("core", "^1")],
    );
    consumer.dep_path = "consumer@1.0.0".to_string();
    let mut adapter = mk_locked("adapter", "1.0.0", &[("core", "1.0.0")], &[("core", "^1")]);
    adapter.dep_path = "adapter@1.0.0".to_string();
    let core = mk_locked("core", "1.0.0", &[], &[]);

    let mut packages = BTreeMap::new();
    packages.insert("consumer@1.0.0".to_string(), consumer);
    packages.insert("adapter@1.0.0".to_string(), adapter);
    packages.insert("core@1.0.0".to_string(), core);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "consumer".to_string(),
            dep_path: "consumer@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let out = apply_peer_contexts(graph, &PeerContextOptions::default())
        .expect("test graph should converge");

    // The nested-peer test's expected key must still be produced.
    assert!(
        out.packages
            .contains_key("consumer@1.0.0(adapter@1.0.0(core@1.0.0))(core@1.0.0)"),
        "default cap corrupted output: {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );
}

// Fresh resolve: when the root manifest carries
// `"odd-alias": "npm:is-odd@3.0.1"`, the resolver must emit the
// graph keyed by the *alias* and stash the real registry name in
// `alias_of`. Before this fix, `task.name` was clobbered to
// `is-odd` at the `npm:` rewrite site, which collapsed
// `node_modules/odd-alias/` to `node_modules/is-odd/` and broke
// `require("odd-alias")` at runtime.
#[tokio::test]
async fn fresh_resolve_preserves_npm_alias_as_folder_name() {
    let is_odd = make_packument("is-odd", &["3.0.1"], "3.0.1");

    // Pre-seed the cache under the *real* package name — the
    // whole point of the fix is that the registry fetch keys by
    // the real name (`is-odd`), not the alias-qualified
    // `odd-alias` that would 404 the registry.
    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    resolver.cache.insert("is-odd".to_string(), is_odd);

    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("odd-alias".to_string(), "npm:is-odd@3.0.1".to_string());

    let graph = resolver
        .resolve(&manifest, None)
        .await
        .expect("alias resolve failed");

    // Graph key and `LockedPackage.name` both carry the alias —
    // that's what the linker drops into `node_modules/` and what
    // any `require("odd-alias")` walks to.
    let pkg = graph
        .packages
        .get("odd-alias@3.0.1")
        .expect("aliased package must be keyed by the alias dep_path");
    assert_eq!(pkg.name, "odd-alias");
    assert_eq!(pkg.version, "3.0.1");
    assert_eq!(pkg.alias_of.as_deref(), Some("is-odd"));
    assert_eq!(pkg.registry_name(), "is-odd");

    // No stray `is-odd@3.0.1` entry from the rewrite leaking the
    // real name past the alias boundary.
    assert!(!graph.packages.contains_key("is-odd@3.0.1"));

    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "odd-alias");
    assert_eq!(root[0].dep_path, "odd-alias@3.0.1");
}

// Catalog-aliased dep + selector override targeting the original
// (alias) name with a bare-range replacement. Reproduces the
// pnpm/pnpm `js-yaml: npm:@zkochan/js-yaml@0.0.11` + `js-yaml@<3.14.2:
// ^3.14.2` shape: the catalog rewrites js-yaml to the @zkochan
// package, then the override fires by user-facing name and
// replaces the range with `^3.14.2`. Without clearing
// `task.real_name` in the override path, the resolver kept fetching
// `@zkochan/js-yaml`'s packument and bailed with "no version of
// js-yaml matches range ^3.14.2".
#[tokio::test]
async fn override_with_bare_range_undoes_prior_catalog_alias() {
    let real_js_yaml = make_packument("js-yaml", &["3.14.2"], "3.14.2");
    let aliased = make_packument("@zkochan/js-yaml", &["0.0.11"], "0.0.11");

    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut catalogs: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    catalogs.entry("default".to_string()).or_default().insert(
        "js-yaml".to_string(),
        "npm:@zkochan/js-yaml@0.0.11".to_string(),
    );
    let mut overrides: BTreeMap<String, String> = BTreeMap::new();
    overrides.insert("js-yaml@<3.14.2".to_string(), "^3.14.2".to_string());

    let mut resolver = Resolver::new(client)
        .with_catalogs(catalogs)
        .with_overrides(overrides);
    resolver.cache.insert("js-yaml".to_string(), real_js_yaml);
    resolver
        .cache
        .insert("@zkochan/js-yaml".to_string(), aliased);

    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("js-yaml".to_string(), "catalog:".to_string());

    let graph = resolver
        .resolve(&manifest, None)
        .await
        .expect("override should redirect back to real js-yaml");

    let pkg = graph
        .packages
        .get("js-yaml@3.14.2")
        .expect("override target must resolve to real js-yaml@3.14.2");
    assert_eq!(pkg.name, "js-yaml");
    assert_eq!(pkg.version, "3.14.2");
    assert!(
        pkg.alias_of.is_none(),
        "bare-range override must clear the prior npm: alias, got alias_of={:?}",
        pkg.alias_of,
    );
    assert!(!graph.packages.contains_key("js-yaml@0.0.11"));
    assert!(!graph.packages.contains_key("@zkochan/js-yaml@0.0.11"));
}

#[tokio::test]
async fn fresh_resolve_preserves_jsr_name_as_folder_name() {
    let jsr_collections = make_packument("@jsr/std__collections", &["1.1.6"], "1.1.6");

    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    resolver
        .cache
        .insert("@jsr/std__collections".to_string(), jsr_collections);

    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("@std/collections".to_string(), "jsr:^1.1.6".to_string());

    let graph = resolver
        .resolve(&manifest, None)
        .await
        .expect("jsr resolve failed");

    let pkg = graph
        .packages
        .get("@std/collections@1.1.6")
        .expect("jsr package must be keyed by the user-facing dep_path");
    assert_eq!(pkg.name, "@std/collections");
    assert_eq!(pkg.version, "1.1.6");
    assert_eq!(pkg.alias_of.as_deref(), Some("@jsr/std__collections"));
    assert_eq!(pkg.registry_name(), "@jsr/std__collections");
    assert!(
        pkg.tarball_url
            .as_deref()
            .is_some_and(|url| url.contains("@jsr/std__collections")),
        "JSR resolver output must preserve dist.tarball"
    );
    assert!(!graph.packages.contains_key("@jsr/std__collections@1.1.6"));

    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "@std/collections");
    assert_eq!(root[0].dep_path, "@std/collections@1.1.6");
}

// A package listed in both `dependencies` and `devDependencies`
// must appear in the resolved importer's direct-dep list exactly
// once, with `dep_type = Production` (matches pnpm: production
// wins, dev entry is silently dropped). Without dedupe the linker
// sees the same name twice and parallel step 2 races to create
// the shared `node_modules/<name>` symlink, producing EEXIST.
#[tokio::test]
async fn same_dep_in_dependencies_and_dev_dependencies_dedupes() {
    let pmap = make_packument("p-map", &["7.0.4"], "7.0.4");

    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    resolver.cache.insert("p-map".to_string(), pmap);

    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("p-map".to_string(), "7.0.4".to_string());
    manifest
        .dev_dependencies
        .insert("p-map".to_string(), "7.0.4".to_string());

    let graph = resolver
        .resolve(&manifest, None)
        .await
        .expect("resolve failed");

    let root = graph.importers.get(".").unwrap();
    assert_eq!(
        root.len(),
        1,
        "p-map must appear once in root deps, got {root:?}"
    );
    assert_eq!(root[0].name, "p-map");
    assert_eq!(root[0].dep_type, DepType::Production);
}

// `dependencies` also wins over `optionalDependencies` when the
// same name appears in both — same race hazard, same fix.
#[tokio::test]
async fn same_dep_in_dependencies_and_optional_dependencies_dedupes() {
    let pmap = make_packument("p-map", &["7.0.4"], "7.0.4");

    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    resolver.cache.insert("p-map".to_string(), pmap);

    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("p-map".to_string(), "7.0.4".to_string());
    manifest
        .optional_dependencies
        .insert("p-map".to_string(), "7.0.4".to_string());

    let graph = resolver
        .resolve(&manifest, None)
        .await
        .expect("resolve failed");

    let root = graph.importers.get(".").unwrap();
    assert_eq!(
        root.len(),
        1,
        "p-map must appear once in root deps, got {root:?}"
    );
    assert_eq!(root[0].name, "p-map");
    assert_eq!(root[0].dep_type, DepType::Production);
}

// With no `dependencies` entry, `devDependencies` wins over
// `optionalDependencies`. Covers the remaining overlap branch.
#[tokio::test]
async fn same_dep_in_dev_and_optional_dependencies_dedupes() {
    let pmap = make_packument("p-map", &["7.0.4"], "7.0.4");

    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    resolver.cache.insert("p-map".to_string(), pmap);

    let mut manifest = PackageJson::default();
    manifest
        .dev_dependencies
        .insert("p-map".to_string(), "7.0.4".to_string());
    manifest
        .optional_dependencies
        .insert("p-map".to_string(), "7.0.4".to_string());

    let graph = resolver
        .resolve(&manifest, None)
        .await
        .expect("resolve failed");

    let root = graph.importers.get(".").unwrap();
    assert_eq!(
        root.len(),
        1,
        "p-map must appear once in root deps, got {root:?}"
    );
    assert_eq!(root[0].name, "p-map");
    assert_eq!(root[0].dep_type, DepType::Dev);
}

#[test]
fn pick_version_exact_pin_not_hijacked_by_dist_tag() {
    let mut packument = make_packument("foo", &["1.0.0", "1.5.0"], "1.5.0");
    packument
        .dist_tags
        .insert("1.0.0".to_string(), "1.5.0".to_string());
    let result = pick_version(&packument, "1.0.0", None, false, None, false).unwrap();
    assert_eq!(result.version, "1.0.0");
}

fn assert_protocol_hijack_blocked(spec: &str) {
    let mut packument = make_packument("@victim/utils", &["1.0.0"], "1.0.0");
    packument
        .dist_tags
        .insert(spec.to_string(), "1.0.0".to_string());
    let result = pick_version(&packument, spec, None, false, None, false);
    assert!(
        matches!(result, super::semver_util::PickResult::NoMatch),
        "protocol-prefixed range {spec:?} reached dist-tag fallback",
    );
}

#[test]
fn cve_audit_protocol_dist_tag_hijack_blocked() {
    assert_protocol_hijack_blocked("workspace:*");
    assert_protocol_hijack_blocked("catalog:");
    assert_protocol_hijack_blocked("npm:other-pkg@1.0.0");
    assert_protocol_hijack_blocked("Workspace:*");
    assert_protocol_hijack_blocked("GIT+FILE:/local");
}

// The enumerated-prefix guard above only blocks the ~18 known
// protocols. An attacker can pick a scheme aube never enumerated —
// `evil:steal`, `patch:foo`, `tarball:x` — register a dist-tag of that
// literal name on a package they control, and aube would resolve the
// colon-scheme spec straight to it (dependency-confusion: npm forbids
// colons in dist-tag names, but the registry does not enforce that).
// npm rejects with EUNSUPPORTEDPROTOCOL, pnpm with
// ERR_PNPM_SPEC_NOT_SUPPORTED_BY_ANY_RESOLVER, bun rejects too — none
// resolve a colon-scheme spec to a dist-tag. Any URL-scheme-shaped
// prefix must be blocked, not just the enumerated allowlist.
#[test]
fn cve_audit_unenumerated_scheme_dist_tag_hijack_blocked() {
    assert_protocol_hijack_blocked("evil:steal");
    assert_protocol_hijack_blocked("patch:foo");
    assert_protocol_hijack_blocked("tarball:x");
    assert_protocol_hijack_blocked("x+y.z-1:payload");
}

// The guard must reject scheme-shaped specs WITHOUT swallowing
// legitimate colon-less dist-tags. A real custom tag (`nightly`) still
// resolves through the dist-tag fallback, and so does the `latest`
// special case — npm forbids colons in tag names, so a colon-less name
// is unambiguously a tag, never a protocol selector.
#[test]
fn colonless_dist_tag_still_resolves_after_scheme_guard() {
    let mut packument = make_packument("foo", &["1.0.0", "2.0.0"], "2.0.0");
    packument
        .dist_tags
        .insert("nightly".to_string(), "1.0.0".to_string());
    let result = pick_version(&packument, "nightly", None, false, None, false).unwrap();
    assert_eq!(result.version, "1.0.0");

    let result = pick_version(&packument, "latest", None, false, None, false).unwrap();
    assert_eq!(result.version, "2.0.0");
}

// pnpm-parity: a non-peer-declaring package between an importer and a
// peer-declaring descendant must carry the descendant's
// `(peer@version)` suffix on its own dep_path, AND on the importer
// row's `version:` field. Aube's `propagate_peer_suffixes_to_ancestors`
// post-pass closes the gap that previously only tagged the
// peer-declarer itself. Mirrors the assertion at
// `pnpm/test/monorepo/dedupePeers.test.ts:191`.
#[test]
fn peer_suffix_propagates_through_non_peer_intermediary() {
    // parent (no peers) -> leaf (peers on `peer-a`).
    let parent = mk_locked("parent", "1.0.0", &[("leaf", "1.0.0")], &[]);
    let leaf = mk_locked("leaf", "1.0.0", &[("peer-a", "1.0.0")], &[("peer-a", "^1")]);
    let peer_a = mk_locked("peer-a", "1.0.0", &[], &[]);

    let mut packages = BTreeMap::new();
    packages.insert("parent@1.0.0".to_string(), parent);
    packages.insert("leaf@1.0.0".to_string(), leaf);
    packages.insert("peer-a@1.0.0".to_string(), peer_a);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "parent".to_string(),
            dep_path: "parent@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let out = apply_peer_contexts(graph, &PeerContextOptions::default())
        .expect("test graph should converge");

    let parent_key = "parent@1.0.0(peer-a@1.0.0)";
    assert!(
        out.packages.contains_key(parent_key),
        "parent's snapshot key must propagate the descendant peer suffix; got {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );
    let leaf_key = "leaf@1.0.0(peer-a@1.0.0)";
    assert!(
        out.packages.contains_key(leaf_key),
        "leaf keeps its self-peer suffix; got {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );
    let importer_dep_path = &out.importers["."][0].dep_path;
    assert_eq!(
        importer_dep_path, parent_key,
        "importer DirectDep.dep_path tracks the propagated parent key"
    );
}

// pnpm-parity regression: a package that DECLARES its own peer must
// STILL absorb a *descendant* peer it does not declare. pnpm's
// `resolvePeersOfNode` unions children's resolved peers
// (`unknownResolvedPeersOfChildren`) with the package's own resolved
// peers (`allResolvedPeers`) — there is no "has own peers ⇒ stop
// propagating" branch. The previous aube code early-returned on
// `has_own_peers`, dropping the descendant peer and producing a
// snapshot key that diverged from pnpm's (→ `node_modules/.pnpm` path
// mismatch → lockfile churn). This guards that the descendant peer
// bubbles into a peer-declarer's key alongside its own.
#[test]
fn peer_suffix_propagation_unions_descendant_peer_onto_peer_declarer() {
    // mid declares its OWN peer (own-peer) and depends on leaf.
    // leaf declares a DIFFERENT peer (desc-peer) that mid does not
    // declare. After resolution mid's key must carry BOTH suffixes.
    let mut mid = mk_locked(
        "mid",
        "1.0.0",
        &[("own-peer", "1.0.0"), ("leaf", "1.0.0")],
        &[("own-peer", "^1")],
    );
    mid.dep_path = "mid@1.0.0".to_string();
    let leaf = mk_locked(
        "leaf",
        "1.0.0",
        &[("desc-peer", "1.0.0")],
        &[("desc-peer", "^1")],
    );
    let own_peer = mk_locked("own-peer", "1.0.0", &[], &[]);
    let desc_peer = mk_locked("desc-peer", "1.0.0", &[], &[]);

    let mut packages = BTreeMap::new();
    packages.insert("mid@1.0.0".to_string(), mid);
    packages.insert("leaf@1.0.0".to_string(), leaf);
    packages.insert("own-peer@1.0.0".to_string(), own_peer);
    packages.insert("desc-peer@1.0.0".to_string(), desc_peer);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "mid".to_string(),
            dep_path: "mid@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let out = apply_peer_contexts(graph, &PeerContextOptions::default())
        .expect("test graph should converge");

    // mid's key is the UNION of its own resolved peer and the
    // descendant peer bubbled up from leaf — sorted by peer name
    // (`desc-peer` < `own-peer`).
    let mid_key = "mid@1.0.0(desc-peer@1.0.0)(own-peer@1.0.0)";
    assert!(
        out.packages.contains_key(mid_key),
        "peer-declarer must union the descendant peer (desc-peer) with its own (own-peer); got {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );
    // The importer row tracks the unioned key.
    assert_eq!(
        &out.importers["."][0].dep_path, mid_key,
        "importer DirectDep.dep_path tracks the unioned mid key"
    );
    // leaf keeps its own self-peer suffix.
    assert!(
        out.packages.contains_key("leaf@1.0.0(desc-peer@1.0.0)"),
        "leaf keeps its self-peer suffix; got {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );
}

// pnpm-parity (direct-dependency filter): a descendant peer that the
// peer-declaring node ALSO directly depends on must NOT bubble onto the
// node's key — it is resolved locally. pnpm's `resolvePeersOfChildren`
// builds the bubbling set (`unknownResolvedPeersOfChildren`) by keeping
// only child-resolved peers whose alias is NOT a direct child of the
// node (`if (!children[alias])`,
// `installing/deps-resolver/src/resolvePeers.ts`). Without this filter
// aube over-unions: it would emit `mid@1.0.0(desc-peer@…)(own-peer@…)`
// where pnpm emits `mid@1.0.0(own-peer@…)`. This is the peer-declarer
// shape of the over-union bug.
#[test]
fn peer_suffix_propagation_suppresses_direct_dep_peer_on_peer_declarer() {
    // mid declares its OWN peer (own-peer) and directly depends on BOTH
    // leaf and desc-peer. leaf declares desc-peer as a peer — but since
    // mid directly depends on desc-peer, that peer is resolved locally
    // and must NOT bubble onto mid's key. mid's key carries only its
    // own resolved peer.
    let mut mid = mk_locked(
        "mid",
        "1.0.0",
        &[
            ("own-peer", "1.0.0"),
            ("leaf", "1.0.0"),
            ("desc-peer", "1.0.0"),
        ],
        &[("own-peer", "^1")],
    );
    mid.dep_path = "mid@1.0.0".to_string();
    let leaf = mk_locked(
        "leaf",
        "1.0.0",
        &[("desc-peer", "1.0.0")],
        &[("desc-peer", "^1")],
    );
    let own_peer = mk_locked("own-peer", "1.0.0", &[], &[]);
    let desc_peer = mk_locked("desc-peer", "1.0.0", &[], &[]);

    let mut packages = BTreeMap::new();
    packages.insert("mid@1.0.0".to_string(), mid);
    packages.insert("leaf@1.0.0".to_string(), leaf);
    packages.insert("own-peer@1.0.0".to_string(), own_peer);
    packages.insert("desc-peer@1.0.0".to_string(), desc_peer);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "mid".to_string(),
            dep_path: "mid@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let out = apply_peer_contexts(graph, &PeerContextOptions::default())
        .expect("test graph should converge");

    // mid carries ONLY its own resolved peer — desc-peer is suppressed
    // because mid directly depends on it.
    let mid_key = "mid@1.0.0(own-peer@1.0.0)";
    assert!(
        out.packages.contains_key(mid_key),
        "peer-declarer must NOT absorb a descendant peer it directly depends on; got {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );
    assert!(
        !out.packages
            .contains_key("mid@1.0.0(desc-peer@1.0.0)(own-peer@1.0.0)"),
        "desc-peer is mid's direct dep → must not over-union onto mid's key"
    );
    assert_eq!(
        &out.importers["."][0].dep_path, mid_key,
        "importer DirectDep.dep_path tracks mid's own-peer-only key"
    );
}

// pnpm-parity (direct-dependency filter, peer-less shape): a peer-less
// intermediary that directly depends on a peer its child resolves must
// NOT carry that peer on its own key. mid → {leaf, shared}, leaf peers
// on shared. Because mid directly depends on shared, pnpm resolves it
// locally (`if (!children[alias])` drops it from the bubbling set) and
// mid's key stays bare `mid@1.0.0`. This is the peer-less shape of the
// over-union bug — pre-existing before the Q2 patch, same root cause.
#[test]
fn peer_suffix_propagation_suppresses_direct_dep_peer_on_peer_less_intermediary() {
    // mid (no own peers) depends on leaf AND shared. leaf peers on
    // shared. shared is mid's direct child → resolved locally → does
    // not bubble onto mid.
    let mut mid = mk_locked(
        "mid",
        "1.0.0",
        &[("leaf", "1.0.0"), ("shared", "1.0.0")],
        &[],
    );
    mid.dep_path = "mid@1.0.0".to_string();
    let leaf = mk_locked("leaf", "1.0.0", &[("shared", "1.0.0")], &[("shared", "^1")]);
    let shared = mk_locked("shared", "1.0.0", &[], &[]);

    let mut packages = BTreeMap::new();
    packages.insert("mid@1.0.0".to_string(), mid);
    packages.insert("leaf@1.0.0".to_string(), leaf);
    packages.insert("shared@1.0.0".to_string(), shared);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "mid".to_string(),
            dep_path: "mid@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let out = apply_peer_contexts(graph, &PeerContextOptions::default())
        .expect("test graph should converge");

    // mid stays bare — shared is its own direct dep, not a bubbled peer.
    assert!(
        out.packages.contains_key("mid@1.0.0"),
        "peer-less intermediary that directly depends on the peer keeps a bare key; got {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );
    assert!(
        !out.packages.contains_key("mid@1.0.0(shared@1.0.0)"),
        "shared is mid's direct dep → must not over-union onto mid's key"
    );
    // leaf still carries its own resolved peer suffix.
    assert!(
        out.packages.contains_key("leaf@1.0.0(shared@1.0.0)"),
        "leaf keeps its self-peer suffix; got {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        &out.importers["."][0].dep_path, "mid@1.0.0",
        "importer DirectDep.dep_path tracks mid's bare key"
    );
}

// Mutual peer cycle (a -> peer b, b -> peer a). The propagation post-
// pass must NOT lift `(a@…)` onto a's own dep_path — a node listing
// itself as a peer is not valid pnpm shape. The cycle break suppresses
// the package's own canonical name from cumulative additions.
#[test]
fn peer_suffix_propagation_skips_self_in_mutual_cycle() {
    let a = mk_locked("a", "1.0.0", &[("b", "1.0.0")], &[("b", "^1")]);
    let b = mk_locked("b", "1.0.0", &[("a", "1.0.0")], &[("a", "^1")]);

    let mut packages = BTreeMap::new();
    packages.insert("a@1.0.0".to_string(), a);
    packages.insert("b@1.0.0".to_string(), b);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "a".to_string(),
            dep_path: "a@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let out = apply_peer_contexts(graph, &PeerContextOptions::default())
        .expect("test graph should converge");

    // No `(a@…)` self-reference on a, no `(b@…)` self-reference on b.
    assert!(
        out.packages.contains_key("a@1.0.0(b@1.0.0)"),
        "a keeps its existing (b@…) suffix only; got {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );
    assert!(
        !out.packages.contains_key("a@1.0.0(a@1.0.0)(b@1.0.0)"),
        "propagation must NOT lift a's name back onto itself"
    );
    assert!(
        out.packages.contains_key("b@1.0.0(a@1.0.0)"),
        "b keeps its existing (a@…) suffix only"
    );
    assert!(
        !out.packages.contains_key("b@1.0.0(a@1.0.0)(b@1.0.0)"),
        "propagation must NOT lift b's name back onto itself"
    );
}

// Nested self-suffix: when a package's own peer suffix already encodes
// a peer name transitively (e.g. `consumer@1(helper@1(core@1))`), the
// propagation must not double-emit `(core@…)` as a separate flat
// segment. `peer_names_in_segments_recursive` covers names at any
// depth in the self set.
#[test]
fn peer_suffix_propagation_dedupes_nested_self_segments() {
    // consumer peers on helper. helper peers on core. consumer's
    // suffix nests as `(helper@1(core@1))`; we must NOT add a flat
    // `(core@1)` from the descendant scan.
    let mut consumer = mk_locked("consumer", "1.0.0", &[], &[("helper", "^1")]);
    consumer.dep_path = "consumer@1.0.0".to_string();
    let mut helper = mk_locked("helper", "1.0.0", &[("core", "1.0.0")], &[("core", "^1")]);
    helper.dep_path = "helper@1.0.0(core@1.0.0)".to_string();
    let mut core = mk_locked("core", "1.0.0", &[], &[]);
    core.dep_path = "core@1.0.0".to_string();

    let mut packages = BTreeMap::new();
    packages.insert("consumer@1.0.0".to_string(), consumer);
    packages.insert("helper@1.0.0(core@1.0.0)".to_string(), helper);
    packages.insert("core@1.0.0".to_string(), core);

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "consumer".to_string(),
            dep_path: "consumer@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let out = apply_peer_contexts(graph, &PeerContextOptions::default())
        .expect("test graph should converge");

    // Consumer's nested self-suffix is preserved exactly — no flat
    // `(core@1.0.0)` is appended.
    assert!(
        out.packages
            .contains_key("consumer@1.0.0(helper@1.0.0(core@1.0.0))"),
        "consumer's nested self-suffix is preserved; got {:?}",
        out.packages.keys().collect::<Vec<_>>()
    );
    assert!(
        !out.packages
            .contains_key("consumer@1.0.0(core@1.0.0)(helper@1.0.0(core@1.0.0))"),
        "propagation must not double-emit a peer name already covered transitively in the self suffix"
    );
}

// A peer suffix must stop at the package that *supplies* the peer.
// Real-world chain: `xml2json -> node-expat -> node-gyp -> tinyglobby ->
// fdir`, where `fdir` declares an OPTIONAL `picomatch` peer and
// `tinyglobby` lists `picomatch` in its own `dependencies`. pnpm tags
// only `fdir@6.5.0(picomatch@4.0.4)`; every ancestor — including the
// supplier `tinyglobby` — stays bare. The
// `propagate_peer_suffixes_to_ancestors` post-pass used to leak
// `(picomatch@4.0.4)` all the way up to `xml2json@0.12.0`; suppressing a
// peer that the absorbing package supplies itself restores pnpm parity.
#[test]
fn peer_suffix_stops_at_supplier_of_the_peer() {
    let xml2json = mk_locked("xml2json", "0.12.0", &[("node-expat", "2.4.3")], &[]);
    let node_expat = mk_locked("node-expat", "2.4.3", &[("node-gyp", "12.3.0")], &[]);
    let node_gyp = mk_locked("node-gyp", "12.3.0", &[("tinyglobby", "0.2.17")], &[]);
    // tinyglobby supplies `picomatch` (resolving fdir's optional peer).
    let tinyglobby = mk_locked(
        "tinyglobby",
        "0.2.17",
        &[("fdir", "6.5.0"), ("picomatch", "4.0.4")],
        &[],
    );
    // fdir only *declares* the optional peer — it doesn't supply it.
    let mut fdir = mk_locked("fdir", "6.5.0", &[], &[("picomatch", "^3 || ^4")]);
    fdir.peer_dependencies_meta.insert(
        "picomatch".to_string(),
        aube_lockfile::PeerDepMeta { optional: true },
    );
    let picomatch = mk_locked("picomatch", "4.0.4", &[], &[]);

    let mut packages = BTreeMap::new();
    for p in [xml2json, node_expat, node_gyp, tinyglobby, fdir, picomatch] {
        packages.insert(p.dep_path.clone(), p);
    }

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "xml2json".to_string(),
            dep_path: "xml2json@0.12.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^0.12.0".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let out = apply_peer_contexts(graph, &PeerContextOptions::default())
        .expect("test graph should converge");

    let keys: Vec<&String> = out.packages.keys().collect();
    // Only fdir — the peer *declarer* — carries the suffix.
    assert!(
        out.packages.contains_key("fdir@6.5.0(picomatch@4.0.4)"),
        "fdir keeps its own optional-peer suffix; got {keys:?}"
    );
    // The supplier and every ancestor above it stay bare (pnpm parity).
    for bare in [
        "tinyglobby@0.2.17",
        "node-gyp@12.3.0",
        "node-expat@2.4.3",
        "xml2json@0.12.0",
    ] {
        assert!(
            out.packages.contains_key(bare),
            "{bare} must stay bare; got {keys:?}"
        );
    }
    assert!(
        !out.packages
            .contains_key("tinyglobby@0.2.17(picomatch@4.0.4)"),
        "tinyglobby supplies picomatch, so the suffix must stop there; got {keys:?}"
    );
    assert!(
        !out.packages
            .contains_key("xml2json@0.12.0(picomatch@4.0.4)"),
        "xml2json must not inherit a transitive optional peer; got {keys:?}"
    );
}

// A META-ONLY optional peer (declared in `peerDependenciesMeta` but absent
// from `peerDependencies`, exactly how `follow-redirects` declares `debug`)
// must NOT be eagerly bound from a distant ancestor's *regular* dependency.
// pnpm treats such a peer as resolvable but then collapses the binding back
// out via `dedupe-peer-dependents` whenever a peer-free path exists, so the
// realistic lockfile keeps the whole chain bare:
//   provider        (carries debug@3.2.7 as a plain dep)
//     mid_a          (bare)
//       mid_b        (bare)
//         mid_c      (bare)
//           axios    (bare)
//             fr     (bare — follow-redirects; debug stays a transitive peer)
// Binding `(debug@3.2.7)` onto that chain (as a since-reverted experiment did)
// produced suffixed variants that aube's dedupe pass — which only collapses
// *declared*-peer variants — never merged. The same subtree then hashed
// differently per install scope (whole-workspace vs single-member), splitting
// a shared global-virtual-store singleton in two and surfacing at runtime as a
// duplicate-instance "Cannot find module". This test pins the bare shape so
// the over-binding can't regress.
#[test]
fn meta_only_optional_peer_stays_bare_to_keep_singleton_shared() {
    // provider carries debug as a plain (regular) dependency.
    let provider = mk_locked(
        "provider",
        "1.0.0",
        &[("mid_a", "1.0.0"), ("debug", "3.2.7")],
        &[],
    );
    let mid_a = mk_locked("mid_a", "1.0.0", &[("mid_b", "1.0.0")], &[]);
    let mid_b = mk_locked("mid_b", "1.0.0", &[("mid_c", "1.0.0")], &[]);
    let mid_c = mk_locked("mid_c", "1.0.0", &[("axios", "1.0.0")], &[]);
    let axios = mk_locked("axios", "1.0.0", &[("fr", "1.0.0")], &[]);
    // fr = follow-redirects: debug ONLY in meta (optional), NOT in peer_deps.
    let mut fr = mk_locked("fr", "1.0.0", &[], &[]);
    fr.peer_dependencies_meta.insert(
        "debug".to_string(),
        aube_lockfile::PeerDepMeta { optional: true },
    );
    let debug = mk_locked("debug", "3.2.7", &[], &[]);

    let mut packages = BTreeMap::new();
    for p in [provider, mid_a, mid_b, mid_c, axios, fr, debug] {
        packages.insert(p.dep_path.clone(), p);
    }
    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "provider".to_string(),
            dep_path: "provider@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1".to_string()),
        }],
    );
    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let out = apply_peer_contexts(graph, &PeerContextOptions::default()).expect("should converge");
    let mut keys: Vec<&String> = out.packages.keys().collect();
    keys.sort();
    // Expected: the meta-only optional `debug` peer is NOT folded into any
    // dep_path, so every node on the chain has a single bare instance and the
    // subtree hashes identically regardless of install scope.
    for bare in [
        "provider@1.0.0",
        "mid_a@1.0.0",
        "mid_b@1.0.0",
        "mid_c@1.0.0",
        "axios@1.0.0",
        "fr@1.0.0",
    ] {
        assert!(
            out.packages.contains_key(bare),
            "{bare} must exist as the single bare instance; got {keys:#?}"
        );
    }
    for suffixed in [
        "fr@1.0.0(debug@3.2.7)",
        "axios@1.0.0(debug@3.2.7)",
        "mid_c@1.0.0(debug@3.2.7)",
        "mid_b@1.0.0(debug@3.2.7)",
        "mid_a@1.0.0(debug@3.2.7)",
    ] {
        assert!(
            !out.packages.contains_key(suffixed),
            "{suffixed} must NOT be created from a meta-only optional peer; got {keys:#?}"
        );
    }
}

// ---------------------------------------------------------------------------
// direct_dep_info
// ---------------------------------------------------------------------------

fn direct_dep_info_resolver() -> Resolver {
    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    Resolver::new(client)
}

fn direct_dep_info_graph(
    direct: &[(&str, &str, &str)],
    packages: &[LockedPackage],
) -> LockfileGraph {
    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        direct
            .iter()
            .map(|(name, dep_path, spec)| DirectDep {
                name: (*name).to_string(),
                dep_path: (*dep_path).to_string(),
                dep_type: DepType::Production,
                specifier: Some((*spec).to_string()),
            })
            .collect(),
    );
    let mut pkg_map = BTreeMap::new();
    for pkg in packages {
        pkg_map.insert(pkg.dep_path.clone(), pkg.clone());
    }
    LockfileGraph {
        importers,
        packages: pkg_map,
        ..Default::default()
    }
}

#[test]
fn direct_dep_info_flags_deprecated_resolved_version() {
    let mut packument = make_packument("foo", &["1.0.0", "1.0.1"], "1.0.1");
    packument.versions.get_mut("1.0.0").unwrap().deprecated = Some("use 1.0.1 instead".to_string());

    let mut resolver = direct_dep_info_resolver();
    resolver.cache.insert("foo".to_string(), packument);

    let pkg = mk_locked("foo", "1.0.0", &[], &[]);
    let graph = direct_dep_info_graph(&[("foo", "foo@1.0.0", "^1")], &[pkg]);

    let info = resolver.direct_dep_info(&graph);
    let entry = info.get("foo@1.0.0").expect("foo@1.0.0 should have info");
    assert!(entry.deprecated, "resolved version is deprecated");
    assert_eq!(entry.latest.as_deref(), Some("1.0.1"));
}

#[test]
fn direct_dep_info_omits_latest_when_already_on_latest() {
    let packument = make_packument("bar", &["2.0.0"], "2.0.0");
    let mut resolver = direct_dep_info_resolver();
    resolver.cache.insert("bar".to_string(), packument);

    let pkg = mk_locked("bar", "2.0.0", &[], &[]);
    let graph = direct_dep_info_graph(&[("bar", "bar@2.0.0", "^2")], &[pkg]);

    let info = resolver.direct_dep_info(&graph);
    // No deprecated and latest == current → no entry at all.
    assert!(
        !info.contains_key("bar@2.0.0"),
        "got unexpected entry: {info:?}"
    );
}

#[test]
fn direct_dep_info_skips_local_source_deps() {
    // file: / link: / git: deps don't have packuments and shouldn't be
    // probed for "latest" even if a same-name packument happens to be
    // cached (e.g. a transitive registry sibling). Local-source deps
    // get no badge.
    let packument = make_packument("baz", &["9.9.9"], "9.9.9");
    let mut resolver = direct_dep_info_resolver();
    resolver.cache.insert("baz".to_string(), packument);

    let mut pkg = mk_locked("baz", "0.0.0", &[], &[]);
    pkg.local_source = Some(LocalSource::Directory(std::path::PathBuf::from(
        "./vendor/baz",
    )));
    let graph = direct_dep_info_graph(&[("baz", "baz@0.0.0", "file:./vendor/baz")], &[pkg]);

    let info = resolver.direct_dep_info(&graph);
    assert!(
        info.is_empty(),
        "local-source dep should be skipped: {info:?}"
    );
}

#[test]
fn direct_dep_info_uses_registry_name_for_aliased_dep() {
    // `"h3-v2": "npm:h3@2.0.1-rc.20"` — the manifest alias is `h3-v2`
    // and the packument lives under the registry name `h3`. The
    // resolver's cache is keyed by registry name, so direct_dep_info
    // must follow `LockedPackage::registry_name()` (not `name`) to
    // find the packument.
    let mut packument = make_packument("h3", &["2.0.0", "2.0.1"], "2.0.1");
    packument.versions.get_mut("2.0.0").unwrap().deprecated = Some("rc only".to_string());
    let mut resolver = direct_dep_info_resolver();
    resolver.cache.insert("h3".to_string(), packument);

    let mut pkg = mk_locked("h3-v2", "2.0.0", &[], &[]);
    pkg.alias_of = Some("h3".to_string());
    pkg.dep_path = "h3-v2@2.0.0".to_string();
    let graph = direct_dep_info_graph(&[("h3-v2", "h3-v2@2.0.0", "npm:h3@2.0.0")], &[pkg]);

    let info = resolver.direct_dep_info(&graph);
    let entry = info
        .get("h3-v2@2.0.0")
        .expect("aliased entry should resolve via registry_name");
    assert!(entry.deprecated, "aliased resolved version is deprecated");
    assert_eq!(entry.latest.as_deref(), Some("2.0.1"));
}

#[test]
fn direct_dep_info_empty_when_no_packument_cached() {
    // Frozen-lockfile reuse: the resolver never fetches packuments, so
    // direct_dep_info should produce no entries rather than blowing up.
    let resolver = direct_dep_info_resolver();
    let pkg = mk_locked("ghost", "1.0.0", &[], &[]);
    let graph = direct_dep_info_graph(&[("ghost", "ghost@1.0.0", "^1")], &[pkg]);

    let info = resolver.direct_dep_info(&graph);
    assert!(info.is_empty(), "no packument cached → empty map");
}

#[tokio::test]
async fn optional_dep_is_skipped_while_required_dep_resolves() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registry = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0_u8; 2048];
                let _ = socket.read(&mut buf).await;
                let response =
                    "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                socket.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });

    // Pre-seed cache for the required dep so it never hits the network.
    let pmap = make_packument("p-map", &["7.0.4"], "7.0.4");
    let client = Arc::new(aube_registry::client::RegistryClient::new(&registry));
    let mut resolver = Resolver::new(client);
    resolver.cache.insert("p-map".to_string(), pmap);

    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("p-map".to_string(), "7.0.4".to_string());
    manifest
        .optional_dependencies
        .insert("missing-optional".to_string(), "1.0.0".to_string());

    let graph = resolver
        .resolve(&manifest, None)
        .await
        .expect("optional dep 404 must not fail the resolve");
    assert!(graph_has_package(&graph, "p-map", "7.0.4"));
    assert!(!graph_has_package(&graph, "missing-optional", "1.0.0"));
    // pnpm parity: fetch-failed optional deps leave no trace in the
    // lockfile — no skippedOptionalDependencies entry.
    assert!(
        graph
            .skipped_optional_dependencies
            .get(".")
            .is_none_or(|skipped| !skipped.contains_key("missing-optional"))
    );

    server.abort();
}

#[tokio::test]
async fn required_dep_propagates_registry_error() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registry = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0_u8; 2048];
                let _ = socket.read(&mut buf).await;
                let response =
                    "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                socket.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });

    let client = Arc::new(aube_registry::client::RegistryClient::new(&registry));
    let mut resolver = Resolver::new(client);

    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("missing-required".to_string(), "1.0.0".to_string());

    let err = resolver
        .resolve(&manifest, None)
        .await
        .expect_err("required dep 404 must propagate");
    match err {
        Error::Registry(name, _) => {
            assert_eq!(name, "missing-required");
        }
        other => panic!("expected Error::Registry, got {other:?}"),
    }

    server.abort();
}

#[tokio::test]
async fn optional_dep_with_both_fetches_in_flight() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Server returns 200 for p-map and 404 for missing-optional,
    // so BOTH deps trigger real concurrent fetches.
    let pkg_body = serde_json::to_vec(&make_packument("p-map", &["7.0.4"], "7.0.4")).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registry = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let body = pkg_body.clone();
            tokio::spawn(async move {
                let mut buf = [0_u8; 2048];
                let _ = socket.read(&mut buf).await;
                let path = std::str::from_utf8(&buf)
                    .ok()
                    .and_then(|s| s.split_whitespace().nth(1))
                    .unwrap_or("/");
                if path.contains("missing-optional") {
                    let response =
                        "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                    socket.write_all(response.as_bytes()).await.unwrap();
                } else {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                }
            });
        }
    });

    let client = Arc::new(aube_registry::client::RegistryClient::new(&registry));
    let mut resolver = Resolver::new(client);

    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("p-map".to_string(), "7.0.4".to_string());
    manifest
        .optional_dependencies
        .insert("missing-optional".to_string(), "1.0.0".to_string());

    let graph = resolver
        .resolve(&manifest, None)
        .await
        .expect("optional dep 404 must not fail even with concurrent fetches");
    assert!(graph_has_package(&graph, "p-map", "7.0.4"));
    assert!(!graph_has_package(&graph, "missing-optional", "1.0.0"));
    // pnpm parity: fetch-failed optional deps leave no trace in the
    // lockfile — no skippedOptionalDependencies entry.
    assert!(
        graph
            .skipped_optional_dependencies
            .get(".")
            .is_none_or(|skipped| !skipped.contains_key("missing-optional"))
    );

    server.abort();
}

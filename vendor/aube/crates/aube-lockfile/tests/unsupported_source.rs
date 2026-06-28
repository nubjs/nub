//! Integration test for the abort-eagerly policy on unresolvable lockfile
//! sources (the `strict_unsupported_source` embedder toggle).
//!
//! Lives in its own integration-test binary — its own process — because the
//! active embedder is once-per-process. The in-crate unit tests assert the
//! default (lenient: warn+drop for berry, reclassify for classic) behavior, so
//! the STRICT profile is registered only here, where every parse exercises the
//! nub-style "fatal on a non-optional unresolvable source, warn+skip an
//! optional one" path.

use aube_lockfile::{Error, LockfileGraph, parse_lockfile};
use aube_util::Embedder;

/// A strict profile: identical to standalone `aube` except it opts INTO the
/// eager unsupported-source refusal (`strict_unsupported_source: true`), the
/// way nub's profile does.
static STRICT: Embedder = Embedder {
    name: "aube",
    display_name: "aube",
    vendor: None,
    version: "1.0.0",
    user_agent: "aube/1.0.0",
    self_names: &["aube"],
    compatible_names: &["pnpm"],
    lockfile_basename: "aube-lock.yaml",
    workspace_yaml: Some("aube-workspace.yaml"),
    manifest_namespace: "aube",
    env_prefix: Some("AUBE"),
    config_env_prefix: Some("AUBE"),
    diag_env_prefix: Some("AUBE"),
    cache_namespace: "aube",
    data_namespace: "aube",
    managed_config_system_dir: Some("aube"),
    canonical_lockfile_always_wins: true,
    runtime_switching: true,
    self_engines_check: true,
    self_update_enabled: true,
    warm_store_verify: true,
    no_churn_lockfile_write: false,
    read_branded_settings_env: true,
    gvs_incompatible_warning: true,
    primer_ttl: None,
    cpu_budget: None,
    tty_progress: false,
    strict_unsupported_source: true,
};

fn parse(files: &[(&str, &str)]) -> Result<LockfileGraph, Error> {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in files {
        std::fs::write(dir.path().join(name), body).unwrap();
    }
    let manifest = aube_manifest::PackageJson::from_path(&dir.path().join("package.json")).unwrap();
    parse_lockfile(dir.path(), &manifest)
}

fn assert_unsupported(result: Result<LockfileGraph, Error>, want_protocol: &str) {
    match result {
        Err(Error::UnsupportedSource { protocol, .. }) => assert_eq!(
            protocol, want_protocol,
            "wrong protocol in UnsupportedSource error"
        ),
        other => panic!("expected UnsupportedSource({want_protocol}), got {other:?}"),
    }
}

const BERRY_HEADER: &str = "__metadata:\n  version: 8\n  cacheKey: 10c0\n\n";

#[test]
fn strict_must_register_before_other_tests() {
    // The whole binary shares one embedder; register it once up front.
    aube_util::set_embedder(&STRICT);
    assert!(aube_util::embedder().strict_unsupported_source);
}

#[test]
fn berry_jsr_dep_is_a_fatal() {
    aube_util::set_embedder(&STRICT);
    let lock = format!(
        "{BERRY_HEADER}\"foo@jsr:^1.0.0\":\n  version: 1.0.0\n  resolution: \"foo@jsr:1.0.0\"\n  languageName: node\n  linkType: hard\n"
    );
    let r = parse(&[
        (
            "package.json",
            r#"{"name":"t","dependencies":{"foo":"jsr:^1.0.0"}}"#,
        ),
        ("yarn.lock", &lock),
    ]);
    assert_unsupported(r, "jsr");
}

#[test]
fn berry_git_dep_still_resolves() {
    // Berry HANDLES git sources (LocalSource::Git), so a git dep is NOT
    // unsupported — only protocols berry can't represent (jsr, unknown) are.
    aube_util::set_embedder(&STRICT);
    let lock = format!(
        "{BERRY_HEADER}\"foo@https://github.com/u/r.git#commit=abc123\":\n  version: 1.0.0\n  resolution: \"foo@https://github.com/u/r.git#commit=abc123\"\n  languageName: node\n  linkType: hard\n"
    );
    let graph = parse(&[
        (
            "package.json",
            r#"{"name":"t","dependencies":{"foo":"https://github.com/u/r.git#commit=abc123"}}"#,
        ),
        ("yarn.lock", &lock),
    ])
    .expect("a berry git dep must resolve, not fatal");
    assert!(graph.importers["."].iter().any(|d| d.name == "foo"));
}

#[test]
fn classic_git_protocol_dep_resolves() {
    // #217 regression fix: classic HANDLES git sources (like berry above), so a
    // git dep pinned by its `resolved` URL is NOT unsupported — it resolves to a
    // `LocalSource::Git`. Pre-fix it was mis-classified `Unsupported` and
    // aborted the frozen install npm/pnpm/bun all accept (the lockfile-roundtrip
    // nightly `git-dep × yarn` regression). Only a git source with NO pinned
    // commit is still a fatal (see `classic_github_shorthand_dep_is_a_fatal`).
    aube_util::set_embedder(&STRICT);
    let lock = "# yarn lockfile v1\n\n\"foo@git+https://github.com/u/r.git#abc123\":\n  version \"1.0.0\"\n  resolved \"git+https://github.com/u/r.git#abc123\"\n";
    let graph = parse(&[
        (
            "package.json",
            r#"{"name":"t","dependencies":{"foo":"git+https://github.com/u/r.git#abc123"}}"#,
        ),
        ("yarn.lock", lock),
    ])
    .expect("a classic git dep pinned by `resolved` must resolve, not fatal");
    assert!(graph.importers["."].iter().any(|d| d.name == "foo"));
    let pkg = graph
        .packages
        .values()
        .find(|p| p.name == "foo")
        .expect("foo must be in the graph");
    assert!(
        matches!(
            &pkg.local_source,
            Some(aube_lockfile::LocalSource::Git(_))
        ),
        "expected git LocalSource, got {:?}",
        pkg.local_source
    );
}

#[test]
fn classic_github_shorthand_dep_is_a_fatal() {
    aube_util::set_embedder(&STRICT);
    let lock = "# yarn lockfile v1\n\n\"foo@user/repo#abc123\":\n  version \"1.0.0\"\n  resolved \"https://codeload.github.com/user/repo/tar.gz/abc123\"\n";
    let r = parse(&[
        (
            "package.json",
            r#"{"name":"t","dependencies":{"foo":"user/repo#abc123"}}"#,
        ),
        ("yarn.lock", lock),
    ]);
    assert_unsupported(r, "git");
}

#[test]
fn classic_optional_unsupported_dep_warns_and_is_skipped() {
    // decision #3 + B3: an OPTIONAL unresolvable dep is NOT fatal — it's
    // dropped (the install continues) and recorded as a skipped optional so a
    // frozen install's drift check tolerates the absent dep.
    aube_util::set_embedder(&STRICT);
    let lock =
        "# yarn lockfile v1\n\n\"foo@user/repo#abc123\":\n  version \"1.0.0\"\n  resolved \"x\"\n";
    let graph = parse(&[
        (
            "package.json",
            r#"{"name":"t","optionalDependencies":{"foo":"user/repo#abc123"}}"#,
        ),
        ("yarn.lock", lock),
    ])
    .expect("an optional unsupported dep must NOT be fatal");
    assert!(
        !graph.importers["."].iter().any(|d| d.name == "foo"),
        "the optional unsupported dep should be dropped from the importer"
    );
    assert_eq!(
        graph.skipped_optional_dependencies["."]
            .get("foo")
            .map(String::as_str),
        Some("user/repo#abc123"),
        "the dropped optional must be recorded as skipped for drift tolerance"
    );
}

#[test]
fn clean_classic_lockfile_does_not_fatal() {
    // The critical anti-regression: a lockfile with only resolvable sources
    // must parse cleanly under the strict profile (no false-positive fatal).
    aube_util::set_embedder(&STRICT);
    let lock = "# yarn lockfile v1\n\nlodash@^4.17.0:\n  version \"4.17.21\"\n  resolved \"https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz\"\n  integrity sha512-aaa\n\nlink-dep@link:./local:\n  version \"0.0.0\"\n";
    let graph = parse(&[
        (
            "package.json",
            r#"{"name":"t","dependencies":{"lodash":"^4.17.0","link-dep":"link:./local"}}"#,
        ),
        ("yarn.lock", lock),
    ])
    .expect("a clean lockfile must not fatal under strict");
    assert!(graph.importers["."].iter().any(|d| d.name == "lodash"));
    assert!(graph.importers["."].iter().any(|d| d.name == "link-dep"));
}

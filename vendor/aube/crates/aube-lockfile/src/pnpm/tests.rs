use super::{
    dep_path::{parse_dep_path, version_to_dep_path},
    parse, write,
};
use crate::{
    CatalogEntry, DepType, DirectDep, GitSource, LocalSource, LockedPackage, LockfileGraph,
};
use aube_manifest::PackageJson;
use std::collections::BTreeMap;
use std::path::Path;

#[test]
fn test_parse_dep_path_simple() {
    let (name, version) = parse_dep_path("lodash@4.17.21").unwrap();
    assert_eq!(name, "lodash");
    assert_eq!(version, "4.17.21");
}

#[test]
fn test_parse_dep_path_scoped() {
    let (name, version) = parse_dep_path("@babel/core@7.24.0").unwrap();
    assert_eq!(name, "@babel/core");
    assert_eq!(version, "7.24.0");
}

#[test]
fn test_parse_dep_path_scoped_nested() {
    let (name, version) = parse_dep_path("@types/node@20.11.0").unwrap();
    assert_eq!(name, "@types/node");
    assert_eq!(version, "20.11.0");
}

#[test]
fn test_parse_dep_path_with_leading_slash() {
    let (name, version) = parse_dep_path("/lodash@4.17.21").unwrap();
    assert_eq!(name, "lodash");
    assert_eq!(version, "4.17.21");
}

#[test]
fn test_parse_dep_path_with_peer_suffix() {
    let (name, version) = parse_dep_path("foo@1.0.0(react@18.0.0)").unwrap();
    assert_eq!(name, "foo");
    assert_eq!(version, "1.0.0");
}

#[test]
fn test_parse_dep_path_with_multiple_peer_suffixes() {
    let (name, version) = parse_dep_path("foo@2.0.0(react@18.0.0)(react-dom@18.0.0)").unwrap();
    assert_eq!(name, "foo");
    assert_eq!(version, "2.0.0");
}

#[test]
fn test_parse_dep_path_prerelease() {
    let (name, version) = parse_dep_path("foo@1.0.0-beta.1").unwrap();
    assert_eq!(name, "foo");
    assert_eq!(version, "1.0.0-beta.1");
}

#[test]
fn test_parse_dep_path_no_at() {
    assert!(parse_dep_path("invalid").is_none());
}

#[test]
fn test_version_to_dep_path() {
    assert_eq!(version_to_dep_path("foo", "1.0.0"), "foo@1.0.0");
    assert_eq!(
        version_to_dep_path("@scope/pkg", "2.0.0"),
        "@scope/pkg@2.0.0"
    );
}

#[test]
fn test_parse_fixture_lockfile() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/basic/pnpm-lock.yaml");
    if !fixture.exists() {
        return;
    }

    let graph = parse(&fixture).unwrap();

    // Check importers
    let root_deps = graph.importers.get(".").unwrap();
    assert_eq!(root_deps.len(), 2);
    assert!(root_deps.iter().any(|d| d.name == "is-odd"));
    assert!(root_deps.iter().any(|d| d.name == "is-even"));

    // Check packages
    assert_eq!(graph.packages.len(), 7);
    assert!(graph.packages.contains_key("is-odd@3.0.1"));
    assert!(graph.packages.contains_key("is-even@1.0.0"));
    assert!(graph.packages.contains_key("is-buffer@1.1.6"));

    // Check dependencies in snapshots
    let is_odd = graph.packages.get("is-odd@3.0.1").unwrap();
    assert_eq!(is_odd.dependencies.get("is-number").unwrap(), "6.0.0");

    let is_even = graph.packages.get("is-even@1.0.0").unwrap();
    assert_eq!(is_even.dependencies.get("is-odd").unwrap(), "0.1.2");

    // Check integrity hashes exist
    assert!(is_odd.integrity.is_some());
}

#[test]
fn test_parse_fixture_dep_types() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/basic/pnpm-lock.yaml");
    if !fixture.exists() {
        return;
    }

    let graph = parse(&fixture).unwrap();
    let root_deps = graph.importers.get(".").unwrap();

    // Both deps in basic fixture are production deps
    for dep in root_deps {
        assert_eq!(dep.dep_type, DepType::Production);
    }
}

#[test]
fn test_parse_fixture_transitive_chain() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/basic/pnpm-lock.yaml");
    if !fixture.exists() {
        return;
    }

    let graph = parse(&fixture).unwrap();

    // is-odd@3.0.1 -> is-number@6.0.0 (no further deps)
    let is_odd = graph.packages.get("is-odd@3.0.1").unwrap();
    assert_eq!(is_odd.dependencies.len(), 1);
    let is_number_6 = graph.packages.get("is-number@6.0.0").unwrap();
    assert!(is_number_6.dependencies.is_empty());

    // is-even@1.0.0 -> is-odd@0.1.2 -> is-number@3.0.0 -> kind-of@3.2.2 -> is-buffer@1.1.6
    let is_even = graph.packages.get("is-even@1.0.0").unwrap();
    assert_eq!(is_even.dependencies.get("is-odd").unwrap(), "0.1.2");

    let is_odd_old = graph.packages.get("is-odd@0.1.2").unwrap();
    assert_eq!(is_odd_old.dependencies.get("is-number").unwrap(), "3.0.0");

    let is_number_3 = graph.packages.get("is-number@3.0.0").unwrap();
    assert_eq!(is_number_3.dependencies.get("kind-of").unwrap(), "3.2.2");

    let kind_of = graph.packages.get("kind-of@3.2.2").unwrap();
    assert_eq!(kind_of.dependencies.get("is-buffer").unwrap(), "1.1.6");
}

#[test]
fn parse_normalizes_empty_root_importer_key() {
    // Some pnpm v9 lockfiles in the wild (e.g. npmx.dev) write the
    // root importer as `''` (empty key) rather than `'.'`. Both
    // mean "workspace root" — we must normalize so the linker's
    // `importers.get(".")` lookup still hits.
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &lockfile_path,
        r#"
lockfileVersion: '9.0'

importers:
  '':
    dependencies:
      host:
        specifier: 1.0.0
        version: 1.0.0

packages:
  host@1.0.0:
    resolution: {integrity: sha512-host}

snapshots:
  host@1.0.0: {}
"#,
    )
    .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let root = graph
        .importers
        .get(".")
        .expect("empty-string importer should normalize to `.`");
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "host");
    assert!(!graph.importers.contains_key(""));
}

#[test]
fn parse_handles_both_empty_and_dot_root_importer_keys() {
    // Degenerate case pnpm itself never emits: a lockfile with
    // *both* `''` and `'.'` as separate YAML keys for root. The
    // BTreeMap visits `''` first; without the collision guard
    // the real `'.'` entry silently overwrites the normalized
    // empty-key entry and its deps disappear. First-key wins is
    // arbitrary but deterministic; the important behavior is
    // that no deps get silently dropped on the floor.
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &lockfile_path,
        r#"
lockfileVersion: '9.0'

importers:
  '':
    dependencies:
      from-empty:
        specifier: 1.0.0
        version: 1.0.0
  '.':
    dependencies:
      from-dot:
        specifier: 1.0.0
        version: 1.0.0

packages:
  from-empty@1.0.0:
    resolution: {integrity: sha512-empty}
  from-dot@1.0.0:
    resolution: {integrity: sha512-dot}

snapshots:
  from-empty@1.0.0: {}
  from-dot@1.0.0: {}
"#,
    )
    .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let root = graph.importers.get(".").expect("`.` importer present");
    let names: Vec<&str> = root.iter().map(|d| d.name.as_str()).collect();
    // The empty-key entry is visited first and wins; the `.`
    // entry's deps are ignored (rather than silently clobbering).
    assert_eq!(names, vec!["from-empty"]);
    assert!(!graph.importers.contains_key(""));
}

#[test]
fn parse_snapshot_optional_dependencies_as_edges() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &lockfile_path,
        r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      host:
        specifier: 1.0.0
        version: 1.0.0

packages:
  host@1.0.0:
    resolution: {integrity: sha512-host}

  native@1.0.0:
    resolution: {integrity: sha512-native}
    cpu: [arm64]
    os: [darwin]

snapshots:
  host@1.0.0:
    optionalDependencies:
      native: 1.0.0

  native@1.0.0: {}
"#,
    )
    .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let host = graph.packages.get("host@1.0.0").unwrap();
    assert_eq!(host.dependencies.get("native").unwrap(), "1.0.0");
    assert_eq!(host.optional_dependencies.get("native").unwrap(), "1.0.0");
}

#[test]
fn parse_package_platform_fields_accept_scalar_strings() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &lockfile_path,
        r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      sass-embedded-linux-arm64:
        specifier: 1.99.0
        version: 1.99.0

packages:
  sass-embedded-linux-arm64@1.99.0:
    resolution: {integrity: sha512-native}
    engines: {node: '>=14.0.0'}
    cpu: arm64
    os: linux
    libc: glibc

snapshots:
  sass-embedded-linux-arm64@1.99.0: {}
"#,
    )
    .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let pkg = graph
        .packages
        .get("sass-embedded-linux-arm64@1.99.0")
        .unwrap();
    assert_eq!(pkg.os.as_slice(), &["linux".to_string()]);
    assert_eq!(pkg.cpu.as_slice(), &["arm64".to_string()]);
    assert_eq!(pkg.libc.as_slice(), &["glibc".to_string()]);
}

#[test]
fn parse_local_snapshot_optional_dependencies_as_edges() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &lockfile_path,
        r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      local-host:
        specifier: file:./local-host
        version: file:./local-host

packages:
  local-host@file:./local-host:
    resolution: {directory: ./local-host, type: directory}

  native@1.0.0:
    resolution: {integrity: sha512-native}
    cpu: [arm64]
    os: [darwin]

snapshots:
  local-host@file:./local-host:
    optionalDependencies:
      native: 1.0.0

  native@1.0.0: {}
"#,
    )
    .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let local = graph
        .packages
        .values()
        .find(|pkg| pkg.name == "local-host")
        .unwrap();
    assert_eq!(local.dependencies.get("native").unwrap(), "1.0.0");
    assert_eq!(local.optional_dependencies.get("native").unwrap(), "1.0.0");
}

#[test]
fn parse_workspace_local_snapshot_keys_do_not_duplicate_rebased_packages() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &lockfile_path,
        r#"
lockfileVersion: '9.0'

importers:
  .: {}

  pkg-a:
    dependencies:
      pkg-b:
        specifier: link:../gems/pkg-b-parent/pkg-b
        version: link:../gems/pkg-b-parent/pkg-b

packages:
  pkg-b@link:../gems/pkg-b-parent/pkg-b:
    resolution: {directory: ../gems/pkg-b-parent/pkg-b, type: directory}

snapshots:
  pkg-b@link:../gems/pkg-b-parent/pkg-b: {}
"#,
    )
    .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let pkg_b_entries: Vec<_> = graph
        .packages
        .values()
        .filter(|pkg| pkg.name == "pkg-b")
        .collect();
    assert_eq!(pkg_b_entries.len(), 1);
    assert_eq!(
        pkg_b_entries[0].local_source,
        Some(LocalSource::Link("gems/pkg-b-parent/pkg-b".into()))
    );
    assert_eq!(
        graph.importers["pkg-a"][0].dep_path,
        pkg_b_entries[0].dep_path
    );
}

#[test]
fn parse_multi_importer_local_snapshot_keys_do_not_create_orphans() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &lockfile_path,
        r#"
lockfileVersion: '9.0'

importers:
  .: {}

  pkg-a:
    dependencies:
      pkg-b:
        specifier: link:../gems/pkg-b-parent/pkg-b
        version: link:../gems/pkg-b-parent/pkg-b

  packages/deep-app:
    dependencies:
      pkg-b:
        specifier: link:../../gems/pkg-b-parent/pkg-b
        version: link:../../gems/pkg-b-parent/pkg-b

snapshots:
  pkg-b@link:../gems/pkg-b-parent/pkg-b: {}
  pkg-b@link:../../gems/pkg-b-parent/pkg-b: {}
"#,
    )
    .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let pkg_b_entries: Vec<_> = graph
        .packages
        .values()
        .filter(|pkg| pkg.name == "pkg-b")
        .collect();
    assert_eq!(pkg_b_entries.len(), 1);
    assert_eq!(
        pkg_b_entries[0].local_source,
        Some(LocalSource::Link("gems/pkg-b-parent/pkg-b".into()))
    );
    assert_eq!(
        graph.importers["pkg-a"][0].dep_path,
        pkg_b_entries[0].dep_path
    );
    assert_eq!(
        graph.importers["packages/deep-app"][0].dep_path,
        pkg_b_entries[0].dep_path
    );
}

#[test]
fn parse_workspace_protocol_link_versions_are_rebased_from_importer() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &lockfile_path,
        r#"
lockfileVersion: '9.0'

importers:
  .: {}

  pkg-a:
    dependencies:
      pkg-b:
        specifier: workspace:*
        version: link:../pkg-b

  pkg-b: {}
"#,
    )
    .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let pkg_b = graph
        .packages
        .values()
        .find(|pkg| pkg.name == "pkg-b")
        .expect("pkg-b");
    assert_eq!(pkg_b.local_source, Some(LocalSource::Link("pkg-b".into())));
    assert_eq!(graph.importers["pkg-a"][0].dep_path, pkg_b.dep_path);
    assert_eq!(
        graph.importers["pkg-a"][0].specifier.as_deref(),
        Some("workspace:*")
    );
}

#[test]
fn parse_semver_range_resolved_to_workspace_link_is_rebased_from_importer() {
    // pnpm resolves a plain semver-range dep to a local workspace sibling
    // when the range satisfies the sibling's version (default
    // link-workspace-packages). The lockfile then records the importer-relative
    // `version: link:../pkg-b` under a NON-`workspace:` specifier (`^1.0.0`).
    // That importer-relative path must be rebased to root just like the
    // `workspace:*` case — otherwise the linker materializes a symlink with one
    // extra `..` per importer-depth segment and the package is unresolvable.
    // Real-world repro: dub's `@dub/embed-react` -> `@dub/embed-core: ^0.0.18`.
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &lockfile_path,
        r#"
lockfileVersion: '9.0'

importers:
  .: {}

  packages/embeds/react:
    dependencies:
      embed-core:
        specifier: ^0.0.18
        version: link:../core

  packages/embeds/core: {}
"#,
    )
    .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let core = graph
        .packages
        .values()
        .find(|pkg| pkg.name == "embed-core")
        .expect("embed-core");
    assert_eq!(
        core.local_source,
        Some(LocalSource::Link("packages/embeds/core".into())),
        "a semver range resolving to a workspace link must rebase the importer-relative path to root"
    );
    assert_eq!(
        graph.importers["packages/embeds/react"][0].dep_path,
        core.dep_path
    );
}

#[test]
fn parse_aube_written_workspace_local_paths_are_not_rebased_twice() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &lockfile_path,
        r#"
lockfileVersion: '9.0'

importers:
  .: {}

  pkg-a:
    dependencies:
      pkg-b:
        specifier: link:../gems/pkg-b-parent/pkg-b
        version: link:gems/pkg-b-parent/pkg-b

      pkg-c:
        specifier: file:../gems/pkg-c
        version: file:gems/pkg-c

packages:
  pkg-c@file:gems/pkg-c:
    resolution: {directory: gems/pkg-c, type: directory}

snapshots:
  pkg-c@file:gems/pkg-c: {}
"#,
    )
    .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let pkg_b = graph
        .packages
        .values()
        .find(|pkg| pkg.name == "pkg-b")
        .expect("pkg-b");
    assert_eq!(
        pkg_b.local_source,
        Some(LocalSource::Link("gems/pkg-b-parent/pkg-b".into()))
    );
    let pkg_c = graph
        .packages
        .values()
        .find(|pkg| pkg.name == "pkg-c")
        .expect("pkg-c");
    assert_eq!(
        pkg_c.local_source,
        Some(LocalSource::Directory("gems/pkg-c".into()))
    );
}

#[test]
fn parse_transitive_url_entry_uses_pnpm_version_field() {
    // Regression: pnpm writes non-registry transitive entries with
    // the tarball URL in the dep-path key and the real semver in a
    // `version:` field. Parsing used the URL as the `version`
    // itself, and the install path's store-content cross-check then
    // compared the URL against the tarball's declared `2.4.1` and
    // failed every override'd github dep.
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
            &lockfile_path,
            r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      xml2json:
        specifier: ^0.12.0
        version: 0.12.0

packages:
  xml2json@0.12.0:
    resolution: {integrity: sha512-xxx}

  node-expat@https://codeload.github.com/astro/node-expat/tar.gz/78e559baa908942097330f7967dfbf623ebc2529:
    resolution: {tarball: https://codeload.github.com/astro/node-expat/tar.gz/78e559baa908942097330f7967dfbf623ebc2529}
    version: 2.4.1

snapshots:
  xml2json@0.12.0:
    dependencies:
      node-expat: https://codeload.github.com/astro/node-expat/tar.gz/78e559baa908942097330f7967dfbf623ebc2529

  node-expat@https://codeload.github.com/astro/node-expat/tar.gz/78e559baa908942097330f7967dfbf623ebc2529: {}
"#,
        )
        .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let url = "https://codeload.github.com/astro/node-expat/tar.gz/78e559baa908942097330f7967dfbf623ebc2529";
    // Transitive remote-tarball deps are keyed under the canonical
    // `name@url+<hash>` form — the same form `push_direct` uses for
    // direct deps and a fresh resolve uses for all of them. The raw-URL
    // key must NOT survive: the linker derives the parent's sibling
    // symlink target via `shared_local_dep_path` (the canonical form),
    // so a URL-keyed package would leave that symlink dangling
    // (`Cannot find module 'node-expat'`).
    let canonical =
        crate::shared_local_dep_path("node-expat", url).expect("tarball url canonicalizes");
    assert!(
        canonical.starts_with("node-expat@url+"),
        "unexpected canonical key: {canonical}"
    );
    assert!(
        !graph.packages.contains_key(&format!("node-expat@{url}")),
        "raw-URL key must be canonicalized away, got keys: {:?}",
        graph.packages.keys().collect::<Vec<_>>()
    );
    let pkg = graph
        .packages
        .get(&canonical)
        .expect("transitive remote-tarball entry present under canonical key");
    assert_eq!(pkg.name, "node-expat");
    // pnpm's `version:` field, not the URL.
    assert_eq!(pkg.version, "2.4.1");
    // The URL drives the fetch path via `tarball_url`.
    assert_eq!(pkg.tarball_url.as_deref(), Some(url));
    // The parent records the dep by URL; that reference must canonicalize
    // to the child's key so the sibling symlink resolves.
    let parent = graph
        .packages
        .get("xml2json@0.12.0")
        .expect("xml2json present");
    let child_ref = parent
        .dependencies
        .get("node-expat")
        .expect("node-expat dep recorded");
    assert_eq!(
        crate::shared_local_dep_path("node-expat", child_ref).as_deref(),
        Some(canonical.as_str()),
        "parent reference must canonicalize to the child's key"
    );
}

#[test]
fn url_dep_path_round_trips_with_pnpm_version_field() {
    // Write-side companion: the URL has to stay in the canonical
    // key and the `version:` field has to reappear in the written
    // output so tooling reading the file back sees the same shape
    // pnpm wrote.
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    let src = r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:

  .:
    dependencies:
      xml2json:
        specifier: ^0.12.0
        version: 0.12.0

packages:

  node-expat@https://codeload.github.com/astro/node-expat/tar.gz/78e559baa908942097330f7967dfbf623ebc2529:
    resolution: {tarball: https://codeload.github.com/astro/node-expat/tar.gz/78e559baa908942097330f7967dfbf623ebc2529}
    version: 2.4.1

  xml2json@0.12.0:
    resolution: {integrity: sha512-xxx}

snapshots:

  node-expat@https://codeload.github.com/astro/node-expat/tar.gz/78e559baa908942097330f7967dfbf623ebc2529: {}

  xml2json@0.12.0:
    dependencies:
      node-expat: https://codeload.github.com/astro/node-expat/tar.gz/78e559baa908942097330f7967dfbf623ebc2529
"#;
    std::fs::write(&lockfile_path, src).unwrap();
    let graph = parse(&lockfile_path).unwrap();

    let manifest = PackageJson {
        name: Some("root".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: [("xml2json".to_string(), "^0.12.0".to_string())]
            .into_iter()
            .collect(),
        ..PackageJson::default()
    };
    let out_path = dir.path().join("round-trip.yaml");
    write(&out_path, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&out_path).unwrap();
    assert!(
            written.contains("node-expat@https://codeload.github.com/astro/node-expat/tar.gz/78e559baa908942097330f7967dfbf623ebc2529:"),
            "URL canonical key missing from output: {written}"
        );
    assert!(
        written.contains("    version: 2.4.1"),
        "`version:` field missing from output: {written}"
    );
    // Round-trip must preserve the `resolution: {tarball: …}` block.
    // URL-keyed transitives typically have no integrity, so gating
    // the block on `pkg.integrity` would silently drop the tarball
    // URL and a re-parse would have no way to fetch the package.
    // Hosted git tarballs also carry pnpm's `gitHosted` marker.
    assert!(
            written.contains("resolution: {gitHosted: true, tarball: https://codeload.github.com/astro/node-expat/tar.gz/78e559baa908942097330f7967dfbf623ebc2529}"),
            "`resolution: {{tarball: …}}` missing from output: {written}"
        );
    // Re-parse the written lockfile and assert the tarball URL
    // makes it all the way back onto `LockedPackage.tarball_url`.
    let reparsed = parse(&out_path).unwrap();
    let url = "https://codeload.github.com/astro/node-expat/tar.gz/78e559baa908942097330f7967dfbf623ebc2529";
    // The written lockfile carries the URL key (pnpm parity, asserted
    // above), but on re-parse it canonicalizes back to `name@url+<hash>`.
    let canonical =
        crate::shared_local_dep_path("node-expat", url).expect("tarball url canonicalizes");
    let pkg = reparsed
        .packages
        .get(&canonical)
        .expect("URL-keyed entry survives round-trip under canonical key");
    assert_eq!(pkg.version, "2.4.1");
    assert_eq!(pkg.tarball_url.as_deref(), Some(url));
}

/// Regression for the transitive remote-tarball "Cannot find module"
/// crash. A *transitive* remote-tarball dep is recorded in the pnpm
/// lockfile by its resolved URL — both as its own `packages:`/`snapshots:`
/// key and inside its parent's `dependencies:` map. The linker derives
/// each parent's sibling symlink target, and the graph hasher derives its
/// child lookups, by canonicalizing that URL via `shared_local_dep_path`
/// to the FS-safe `name@url+<hash>` form. The reader must therefore key
/// the package under that same canonical form (mirroring `push_direct`
/// and a fresh resolve). Before the fix it kept the raw URL key, so the
/// package materialized at the escaped `https+++…` dir while every
/// parent's symlink targeted `url+<hash>` — the link dangled and the
/// child's content/engine taint never reached the parent's GVS hash.
#[test]
fn transitive_tarball_child_keyed_canonically_so_parent_symlink_resolves() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    let url =
        "https://codeload.github.com/acme/tardep/tar.gz/9504d1f8f3293df7bfa4de72bd52df615f9f399c";
    std::fs::write(
        &lockfile_path,
        format!(
            r#"lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      app:
        specifier: ^1.0.0
        version: 1.0.0

packages:
  app@1.0.0:
    resolution: {{integrity: sha512-app==}}

  tardep@{url}:
    resolution: {{gitHosted: true, integrity: sha512-tardep==, tarball: {url}}}
    version: 2.42.0

snapshots:
  app@1.0.0:
    dependencies:
      tardep: {url}

  tardep@{url}: {{}}
"#
        ),
    )
    .unwrap();

    let graph = parse(&lockfile_path).unwrap();

    // 1. The transitive tarball is keyed under the canonical hashed form,
    //    and the raw-URL key is gone.
    let canonical = crate::shared_local_dep_path("tardep", url).expect("url canonicalizes");
    assert!(canonical.starts_with("tardep@url+"), "got {canonical}");
    assert!(
        !graph.packages.contains_key(&format!("tardep@{url}")),
        "raw-URL key leaked: {:?}",
        graph.packages.keys().collect::<Vec<_>>()
    );
    assert!(graph.packages.contains_key(&canonical));

    // 2. The structural invariant the linker + hasher rely on: every
    //    child reference that canonicalizes resolves to a real package
    //    key. Before the fix `app`'s `tardep: <url>` canonicalized to a
    //    key that didn't exist, so the sibling symlink dangled.
    for (key, pkg) in &graph.packages {
        for (alias, tail) in &pkg.dependencies {
            if let Some(child) = crate::shared_local_dep_path(alias, tail) {
                assert!(
                    graph.packages.contains_key(&child),
                    "{key}'s dep {alias}@{tail} -> {child} is missing from the graph"
                );
            }
        }
    }

    // 3. The hasher no longer skips the child: a change to the tarball's
    //    content fingerprint must cascade into the parent's hash. Before
    //    the fix the URL-keyed child was invisible to `app`'s deps-hash,
    //    so its fingerprint never moved `app`'s GVS path.
    let with_fp = crate::graph_hash::compute_graph_hashes_full(
        &graph,
        &|_| false,
        None,
        &|_, _| None,
        &|dp| (dp == canonical.as_str()).then(|| "fp".to_string()),
    );
    let without_fp = crate::graph_hash::compute_graph_hashes_full(
        &graph,
        &|_| false,
        None,
        &|_, _| None,
        &|_| None,
    );
    assert_ne!(
        with_fp.node_hash["app@1.0.0"], without_fp.node_hash["app@1.0.0"],
        "transitive tarball child fingerprint must cascade into the parent's graph hash"
    );
}

/// Fresh-resolve companion to `url_dep_path_round_trips_with_pnpm_version_field`.
///
/// When the resolver promotes a hosted-git dep to a codeload tarball it
/// stores the package under the *hashed* `name@url+<hash>` dep_path (not
/// the bare URL), so the writer's `url_keyed` heuristic is false. pnpm
/// still records these as `name@<codeload-url>` with a `version:` field
/// and `resolution: {gitHosted: true, integrity, tarball}`. This drives
/// a resolver-shaped graph through `write` and asserts that shape —
/// without it a fresh `aube install` emits the `<url>.git#<sha>` /
/// `type: git` form and drifts from pnpm.
#[test]
fn fresh_resolved_codeload_tarball_writes_pnpm_version_and_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let codeload = "https://codeload.github.com/xmppo/node-expat/tar.gz/78e559baa908942097330f7967dfbf623ebc2529".to_string();
    let remote = LocalSource::RemoteTarball(crate::RemoteTarballSource {
        url: codeload.clone(),
        integrity: "sha512-nodeexpat==".to_string(),
        git_hosted: true,
    });
    // The resolver keys the package by the hashed `name@url+<hash>` form.
    let node_expat_dp = remote.dep_path("node-expat");
    assert!(
        node_expat_dp.starts_with("node-expat@url+"),
        "sanity: resolver dep_path is the hashed url form, got {node_expat_dp}"
    );
    // The hashed tail is what the parent records as its dep value, and
    // what must NOT leak into the written lockfile.
    let node_expat_tail = node_expat_dp
        .strip_prefix("node-expat@")
        .unwrap()
        .to_string();

    let mut packages = BTreeMap::new();
    packages.insert(
        node_expat_dp.clone(),
        LockedPackage {
            name: "node-expat".to_string(),
            version: "2.4.3".to_string(),
            dep_path: node_expat_dp.clone(),
            local_source: Some(remote),
            ..Default::default()
        },
    );
    packages.insert(
        "xml2json@0.12.0".to_string(),
        LockedPackage {
            name: "xml2json".to_string(),
            version: "0.12.0".to_string(),
            integrity: Some("sha512-xml2json==".to_string()),
            dep_path: "xml2json@0.12.0".to_string(),
            dependencies: BTreeMap::from([("node-expat".to_string(), node_expat_tail.clone())]),
            ..Default::default()
        },
    );

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
    let manifest = PackageJson {
        name: Some("root".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: BTreeMap::from([("xml2json".to_string(), "^0.12.0".to_string())]),
        ..PackageJson::default()
    };

    write(&lockfile_path, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&lockfile_path).unwrap();

    // Keyed by the bare codeload URL (pnpm parity), never the hashed
    // form and never the `.git#<sha>` form.
    assert!(
        written.contains(&format!("node-expat@{codeload}:")),
        "expected codeload-keyed entry, got:\n{written}"
    );
    assert!(
        !written.contains(&node_expat_tail),
        "internal hashed dep_path leaked into the lockfile:\n{written}"
    );
    assert!(
        !written.contains(".git#"),
        "git-url form must be promoted to codeload tarball:\n{written}"
    );
    // pnpm records the real semver next to the tarball resolution.
    assert!(
        written.contains("    version: 2.4.3"),
        "missing `version:` on the codeload entry:\n{written}"
    );
    assert!(
        written.contains(&format!(
            "resolution: {{gitHosted: true, integrity: sha512-nodeexpat==, tarball: {codeload}}}"
        )),
        "missing/incorrect codeload resolution block:\n{written}"
    );
    // The parent references it by the bare codeload URL.
    assert!(
        written.contains(&format!("node-expat: {codeload}")),
        "parent must reference the codeload URL:\n{written}"
    );

    // And it survives a re-parse onto `tarball_url` (drift-free). The
    // written lockfile carries the codeload URL key (asserted above), but
    // re-parse canonicalizes it back to the hashed `name@url+<hash>` form
    // the resolver originally produced — so the round-trip graph matches a
    // fresh resolve and the parent's sibling symlink resolves.
    let reparsed = parse(&lockfile_path).unwrap();
    let canonical =
        crate::shared_local_dep_path("node-expat", &codeload).expect("codeload url canonicalizes");
    assert_eq!(
        canonical, node_expat_dp,
        "re-parse must reproduce the resolver's hashed dep_path"
    );
    let pkg = reparsed
        .packages
        .get(&canonical)
        .expect("codeload entry survives round-trip under canonical key");
    assert_eq!(pkg.version, "2.4.3");
    assert_eq!(pkg.tarball_url.as_deref(), Some(codeload.as_str()));
}

/// A git / remote-tarball dep that is *also* used as a peer must render
/// its peer suffix as the resolved spec, never aube's internal FS-safe
/// hashed dep_path. pnpm writes
/// `request-promise-core@1.1.4(request@https://codeload.…/tar.gz/<sha>)`,
/// not `(request@url+<hash>)` — the latter is only an in-memory key. The
/// writer translates hashed→spec and the reader re-derives spec→hashed so
/// a round-trip is a fixed point (a divergence would re-key every install
/// and churn the lockfile).
#[test]
fn git_tarball_peer_suffix_renders_as_spec_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let codeload =
        "https://codeload.github.com/owner/request/tar.gz/abcdef1234567890abcdef1234567890abcdef12"
            .to_string();
    let remote = LocalSource::RemoteTarball(crate::RemoteTarballSource {
        url: codeload.clone(),
        integrity: "sha512-request==".to_string(),
        git_hosted: true,
    });
    // Resolver-internal keys: the tarball is hashed, registry packages that
    // peer with it carry the hashed suffix.
    let request_dp = remote.dep_path("request");
    let request_tail = request_dp.strip_prefix("request@").unwrap().to_string();
    assert!(
        request_tail.starts_with("url+"),
        "sanity: hashed tarball tail, got {request_tail}"
    );
    let core_key = format!("request-promise-core@1.1.4({request_dp})");
    let rp_key = format!("request-promise@4.2.6({request_dp})");

    let mut packages = BTreeMap::new();
    packages.insert(
        request_dp.clone(),
        LockedPackage {
            name: "request".to_string(),
            version: "2.88.2".to_string(),
            dep_path: request_dp.clone(),
            local_source: Some(remote),
            ..Default::default()
        },
    );
    packages.insert(
        core_key.clone(),
        LockedPackage {
            name: "request-promise-core".to_string(),
            version: "1.1.4".to_string(),
            integrity: Some("sha512-core==".to_string()),
            dep_path: core_key.clone(),
            peer_dependencies: BTreeMap::from([("request".to_string(), "^2.34".to_string())]),
            dependencies: BTreeMap::from([("request".to_string(), request_tail.clone())]),
            ..Default::default()
        },
    );
    packages.insert(
        rp_key.clone(),
        LockedPackage {
            name: "request-promise".to_string(),
            version: "4.2.6".to_string(),
            integrity: Some("sha512-rp==".to_string()),
            dep_path: rp_key.clone(),
            peer_dependencies: BTreeMap::from([("request".to_string(), "^2.34".to_string())]),
            dependencies: BTreeMap::from([
                ("request".to_string(), request_tail.clone()),
                // pnpm references the peer-bearing sibling by its
                // contextualized (hashed, in-memory) tail.
                (
                    "request-promise-core".to_string(),
                    format!("1.1.4({request_dp})"),
                ),
            ]),
            ..Default::default()
        },
    );

    let graph = LockfileGraph {
        packages,
        ..Default::default()
    };
    let manifest = PackageJson {
        name: Some("root".to_string()),
        version: Some("0.0.0".to_string()),
        ..PackageJson::default()
    };

    write(&lockfile_path, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&lockfile_path).unwrap();

    // Snapshot keys + embedded dep values render the suffix as the spec.
    assert!(
        written.contains(&format!("request-promise-core@1.1.4(request@{codeload}):")),
        "core snapshot key suffix not rendered as spec:\n{written}"
    );
    assert!(
        written.contains(&format!("request-promise@4.2.6(request@{codeload}):")),
        "request-promise snapshot key suffix not rendered as spec:\n{written}"
    );
    assert!(
        written.contains(&format!("request-promise-core: 1.1.4(request@{codeload})")),
        "embedded dep-value suffix not rendered as spec:\n{written}"
    );
    // The internal hashed form must never leak into the file.
    assert!(
        !written.contains(&request_tail),
        "internal hashed dep_path leaked into the lockfile:\n{written}"
    );

    // Reader re-derives the hashed suffix: keys match a fresh resolve, so a
    // re-install reads its own (or pnpm's) lockfile as a fixed point.
    let reparsed = parse(&lockfile_path).unwrap();
    assert!(
        reparsed.packages.contains_key(&core_key),
        "reader must normalize the spec suffix back to the hashed key; got keys: {:?}",
        reparsed.packages.keys().collect::<Vec<_>>()
    );
    assert!(
        reparsed.packages.contains_key(&rp_key),
        "reader must normalize request-promise's hashed key"
    );
    let rp = reparsed.packages.get(&rp_key).unwrap();
    assert_eq!(
        rp.dependencies
            .get("request-promise-core")
            .map(String::as_str),
        Some(format!("1.1.4({request_dp})").as_str()),
        "reader must normalize the embedded dep-value suffix: {:?}",
        rp.dependencies
    );
}

#[test]
fn direct_url_importer_strips_peer_suffix_from_fetch_url() {
    // Regression: when a direct dep's importer `version:` is a
    // tarball URL *with* a pnpm peer-context suffix
    // (`(peer@ver)`), the parser used to bake the whole string
    // into `RemoteTarballSource.url`, so the install path fetched
    // `…/tar.gz/SHA(peer@ver)` and hit a 404.
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
            &lockfile_path,
            r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      dep-a:
        specifier: github:owner/dep-a#abcdef1234567890abcdef1234567890abcdef12
        version: https://codeload.github.com/owner/dep-a/tar.gz/abcdef1234567890abcdef1234567890abcdef12(encoding@0.1.13)

packages:
  dep-a@https://codeload.github.com/owner/dep-a/tar.gz/abcdef1234567890abcdef1234567890abcdef12:
    resolution: {tarball: https://codeload.github.com/owner/dep-a/tar.gz/abcdef1234567890abcdef1234567890abcdef12}
    version: 1.0.0

  encoding@0.1.13:
    resolution: {integrity: sha512-enc}

snapshots:
  dep-a@https://codeload.github.com/owner/dep-a/tar.gz/abcdef1234567890abcdef1234567890abcdef12(encoding@0.1.13):
    dependencies:
      encoding: 0.1.13

  encoding@0.1.13: {}
"#,
        )
        .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let clean_url =
        "https://codeload.github.com/owner/dep-a/tar.gz/abcdef1234567890abcdef1234567890abcdef12";

    let dep_a = graph
        .packages
        .values()
        .find(|pkg| pkg.name == "dep-a")
        .expect("dep-a present after parse");
    match dep_a.local_source.as_ref() {
        Some(LocalSource::RemoteTarball(t)) => {
            assert_eq!(
                t.url, clean_url,
                "peer suffix leaked into RemoteTarballSource.url — fetch would 404"
            );
        }
        other => panic!("expected RemoteTarball, got {other:?}"),
    }
    // The snapshot carrying the peer suffix shouldn't produce a
    // second entry — that would round-trip as a stray packages
    // block.
    let dep_a_entries: Vec<_> = graph
        .packages
        .values()
        .filter(|p| p.name == "dep-a")
        .collect();
    assert_eq!(
        dep_a_entries.len(),
        1,
        "exactly one dep-a entry expected (suffix'd snapshot should fold into the local)"
    );
    // Transitive deps declared on the peer-context'd snapshot flow
    // onto the local package.
    assert_eq!(
        dep_a.dependencies.get("encoding"),
        Some(&"0.1.13".to_string())
    );
}

#[test]
fn test_write_and_reparse_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    // Build a graph
    let mut packages = BTreeMap::new();
    let mut foo_deps = BTreeMap::new();
    foo_deps.insert("bar".to_string(), "2.0.0".to_string());
    packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-abc123==".to_string()),
            dependencies: foo_deps,
            dep_path: "foo@1.0.0".to_string(),
            ..Default::default()
        },
    );
    packages.insert(
        "bar@2.0.0".to_string(),
        LockedPackage {
            name: "bar".to_string(),
            version: "2.0.0".to_string(),
            integrity: Some("sha512-def456==".to_string()),
            dependencies: BTreeMap::new(),
            dep_path: "bar@2.0.0".to_string(),
            ..Default::default()
        },
    );

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "foo".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1.0.0".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let mut deps = BTreeMap::new();
    deps.insert("foo".to_string(), "^1.0.0".to_string());
    let manifest = PackageJson {
        name: Some("test".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: deps,
        dev_dependencies: BTreeMap::new(),
        peer_dependencies: BTreeMap::new(),
        optional_dependencies: BTreeMap::new(),
        update_config: None,
        scripts: BTreeMap::new(),
        engines: BTreeMap::new(),
        dev_engines: None,
        workspaces: None,
        bundled_dependencies: None,
        extra: BTreeMap::new(),
    };

    write(&lockfile_path, &graph, &manifest).unwrap();

    // Re-parse and verify
    let reparsed = parse(&lockfile_path).unwrap();
    assert_eq!(reparsed.packages.len(), 2);
    assert_eq!(
        reparsed.packages.get("foo@1.0.0").unwrap().integrity,
        Some("sha512-abc123==".to_string())
    );
    assert_eq!(
        reparsed
            .packages
            .get("foo@1.0.0")
            .unwrap()
            .dependencies
            .get("bar")
            .unwrap(),
        "2.0.0"
    );

    let root_deps = reparsed.importers.get(".").unwrap();
    assert_eq!(root_deps.len(), 1);
    assert_eq!(root_deps[0].name, "foo");
    assert_eq!(root_deps[0].dep_type, DepType::Production);
}

/// pnpm strips engines entries whose value is exactly `*` and omits the
/// field when nothing survives (verified against pnpm v11 + its
/// `updateLockfile.ts`: `if (version === '*') continue`). Everything
/// else is kept verbatim — including the array-shaped
/// `{'0': node >=0.6.0}` pnpm emits for packages that declared `engines`
/// as an array, so the filter must key off the *value*, not the shape.
#[test]
fn engines_star_values_are_dropped_like_pnpm() {
    let mk = |name: &str, engines: &[(&str, &str)]| {
        (
            format!("{name}@1.0.0"),
            LockedPackage {
                name: name.to_string(),
                version: "1.0.0".to_string(),
                integrity: Some(format!("sha512-{name}")),
                dep_path: format!("{name}@1.0.0"),
                engines: engines
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                ..Default::default()
            },
        )
    };

    let packages = BTreeMap::from([
        mk("arrayform", &[("0", "node >=0.6.0")]),
        mk("real", &[("node", ">=14")]),
        mk("starnode", &[("node", "*")]),
        mk("starplusnpm", &[("node", "*"), ("npm", ">=6")]),
    ]);

    let graph = LockfileGraph {
        packages,
        ..Default::default()
    };
    let manifest = PackageJson {
        name: Some("eng".to_string()),
        version: Some("0.0.0".to_string()),
        ..Default::default()
    };

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("pnpm-lock.yaml");
    write(&out, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&out).unwrap();

    // `{node: '*'}` collapses to nothing → exactly three engines lines
    // survive (starplusnpm keeps only npm, plus arrayform + real).
    assert_eq!(
        yaml.matches("    engines: {").count(),
        3,
        "expected 3 engines lines (starnode fully dropped):\n{yaml}"
    );
    assert!(
        yaml.contains("engines: {npm: '>=6'}"),
        "node:'*' dropped but npm kept:\n{yaml}"
    );
    assert!(
        yaml.contains("engines: {'0': node >=0.6.0}"),
        "array-shaped engines must be preserved (pnpm keeps them):\n{yaml}"
    );
    assert!(
        yaml.contains("engines: {node: '>=14'}"),
        "ordinary engines must be preserved:\n{yaml}"
    );

    // Round-trip: the fully-dropped entry stays empty, survivors reparse.
    let reparsed = parse(&out).unwrap();
    assert!(
        reparsed.packages["starnode@1.0.0"].engines.is_empty(),
        "starnode must round-trip with no engines"
    );
    assert_eq!(
        reparsed.packages["starplusnpm@1.0.0"]
            .engines
            .get("npm")
            .map(String::as_str),
        Some(">=6")
    );
    assert!(
        !reparsed.packages["starplusnpm@1.0.0"]
            .engines
            .contains_key("node"),
        "node:'*' must not come back on reparse"
    );
    assert_eq!(
        reparsed.packages["arrayform@1.0.0"]
            .engines
            .get("0")
            .map(String::as_str),
        Some("node >=0.6.0")
    );
}

/// pnpm records the registry `deprecated:` reason on `packages:`
/// entries, placed after `engines`/`cpu`/`os`/`libc` and before
/// `hasBin` (verified against pnpm v11 output for `coffee-script` /
/// `fsevents` / `request`). aube carries the reason on
/// `LockedPackage::extra_meta["deprecated"]` so the reader and writer
/// round-trip it instead of dropping the field on a parse/write cycle.
#[test]
fn deprecated_message_round_trips_in_pnpm_field_order() {
    let yaml = r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:

  .:
    dependencies:
      coffee-script:
        specifier: 1.12.7
        version: 1.12.7

packages:

  coffee-script@1.12.7:
    resolution: {integrity: sha512-coffee}
    engines: {node: '>=0.8.0'}
    deprecated: CoffeeScript on NPM has moved to "coffeescript" (no hyphen)
    hasBin: true

snapshots:

  coffee-script@1.12.7: {}
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(&path, yaml).unwrap();

    // Reader: the deprecation reason lands on extra_meta verbatim.
    let graph = parse(&path).unwrap();
    let pkg = graph.packages.get("coffee-script@1.12.7").unwrap();
    assert_eq!(
        pkg.extra_meta.get("deprecated").and_then(|v| v.as_str()),
        Some(r#"CoffeeScript on NPM has moved to "coffeescript" (no hyphen)"#),
        "reader must capture deprecated into extra_meta"
    );

    // Writer: re-emit it as a plain scalar (embedded quotes and all,
    // matching pnpm), positioned after engines and before hasBin.
    let manifest = PackageJson {
        name: Some("dep-test".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: [("coffee-script".to_string(), "1.12.7".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = dir.path().join("out.yaml");
    write(&out, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&out).unwrap();

    assert!(
        written.contains(
            "    deprecated: CoffeeScript on NPM has moved to \"coffeescript\" (no hyphen)\n"
        ),
        "writer must emit the deprecated line verbatim:\n{written}"
    );
    let engines_at = written.find("engines:").expect("engines:");
    let deprecated_at = written.find("deprecated:").expect("deprecated:");
    let has_bin_at = written.find("hasBin:").expect("hasBin:");
    assert!(
        engines_at < deprecated_at && deprecated_at < has_bin_at,
        "deprecated must sit after engines and before hasBin:\n{written}"
    );

    // Round-trip: the field survives a parse → write → parse cycle.
    let reparsed = parse(&out).unwrap();
    assert_eq!(
        reparsed
            .packages
            .get("coffee-script@1.12.7")
            .unwrap()
            .extra_meta
            .get("deprecated")
            .and_then(|v| v.as_str()),
        Some(r#"CoffeeScript on NPM has moved to "coffeescript" (no hyphen)"#),
        "deprecated reason must survive a full round-trip"
    );
}

#[test]
fn test_write_prunes_time_to_direct_importer_deps() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut packages = BTreeMap::new();
    packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-foo==".to_string()),
            dependencies: [("bar".to_string(), "2.0.0".to_string())]
                .into_iter()
                .collect(),
            dep_path: "foo@1.0.0".to_string(),
            ..Default::default()
        },
    );
    packages.insert(
        "bar@2.0.0".to_string(),
        LockedPackage {
            name: "bar".to_string(),
            version: "2.0.0".to_string(),
            integrity: Some("sha512-bar==".to_string()),
            dep_path: "bar@2.0.0".to_string(),
            ..Default::default()
        },
    );

    let graph = LockfileGraph {
        importers: [(
            ".".to_string(),
            vec![DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1.0.0".to_string()),
            }],
        )]
        .into_iter()
        .collect(),
        packages,
        times: [
            (
                "foo@1.0.0".to_string(),
                "2026-01-01T00:00:00.000Z".to_string(),
            ),
            (
                "bar@2.0.0".to_string(),
                "2026-01-02T00:00:00.000Z".to_string(),
            ),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    write(&lockfile_path, &graph, &PackageJson::default()).unwrap();
    let written = std::fs::read_to_string(&lockfile_path).unwrap();

    assert!(written.contains("\n  foo@1.0.0: 2026-01-01T00:00:00.000Z\n"));
    assert!(!written.contains("\n  bar@2.0.0: 2026-01-02T00:00:00.000Z\n"));
}

#[test]
fn test_write_preserves_real_name_time_for_aube_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("aube-lock.yaml");

    let graph = LockfileGraph {
        importers: [(
            ".".to_string(),
            vec![DirectDep {
                name: "alias-pkg".to_string(),
                dep_path: "alias-pkg@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("npm:real-pkg@^1.0.0".to_string()),
            }],
        )]
        .into_iter()
        .collect(),
        packages: [(
            "alias-pkg@1.0.0".to_string(),
            LockedPackage {
                name: "alias-pkg".to_string(),
                version: "1.0.0".to_string(),
                integrity: Some("sha512-alias==".to_string()),
                dep_path: "alias-pkg@1.0.0".to_string(),
                alias_of: Some("real-pkg".to_string()),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
        times: [(
            "real-pkg@1.0.0".to_string(),
            "2026-01-01T00:00:00.000Z".to_string(),
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    write(&lockfile_path, &graph, &PackageJson::default()).unwrap();
    let written = std::fs::read_to_string(&lockfile_path).unwrap();

    assert!(written.contains("\n  alias-pkg@1.0.0: 2026-01-01T00:00:00.000Z\n"));
    assert!(!written.contains("\n  real-pkg@1.0.0: 2026-01-01T00:00:00.000Z\n"));
}

#[test]
fn writer_preserves_workspace_importer_specifiers() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut packages = BTreeMap::new();
    packages.insert(
        "@dev/build-tools@1.0.0".to_string(),
        LockedPackage {
            name: "@dev/build-tools".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "@dev/build-tools@1.0.0".to_string(),
            ..Default::default()
        },
    );

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "@dev/build-tools".to_string(),
            dep_path: "@dev/build-tools@1.0.0".to_string(),
            dep_type: DepType::Dev,
            specifier: Some("^1.0.0".to_string()),
        }],
    );
    importers.insert(
        "packages/public/umd/babylonjs".to_string(),
        vec![DirectDep {
            name: "@dev/build-tools".to_string(),
            dep_path: "@dev/build-tools@1.0.0".to_string(),
            dep_type: DepType::Dev,
            specifier: Some("1.0.0".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let mut root_dev_dependencies = BTreeMap::new();
    root_dev_dependencies.insert("@dev/build-tools".to_string(), "^1.0.0".to_string());
    let manifest = PackageJson {
        name: Some("root".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: BTreeMap::new(),
        dev_dependencies: root_dev_dependencies,
        peer_dependencies: BTreeMap::new(),
        optional_dependencies: BTreeMap::new(),
        update_config: None,
        scripts: BTreeMap::new(),
        engines: BTreeMap::new(),
        dev_engines: None,
        workspaces: None,
        bundled_dependencies: None,
        extra: BTreeMap::new(),
    };

    write(&lockfile_path, &graph, &manifest).unwrap();

    let reparsed = parse(&lockfile_path).unwrap();
    let workspace_deps = reparsed
        .importers
        .get("packages/public/umd/babylonjs")
        .unwrap();
    assert_eq!(workspace_deps[0].specifier.as_deref(), Some("1.0.0"));
}

#[test]
fn overrides_round_trip_through_pnpm_lock_yaml() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut overrides = BTreeMap::new();
    overrides.insert("lodash".to_string(), "4.17.21".to_string());
    overrides.insert("foo".to_string(), "npm:bar@^2".to_string());

    let graph = LockfileGraph {
        importers: BTreeMap::new(),
        packages: BTreeMap::new(),
        overrides,
        ..Default::default()
    };

    let manifest = PackageJson {
        name: Some("test".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: BTreeMap::new(),
        dev_dependencies: BTreeMap::new(),
        peer_dependencies: BTreeMap::new(),
        optional_dependencies: BTreeMap::new(),
        update_config: None,
        scripts: BTreeMap::new(),
        engines: BTreeMap::new(),
        dev_engines: None,
        workspaces: None,
        bundled_dependencies: None,
        extra: BTreeMap::new(),
    };

    write(&lockfile_path, &graph, &manifest).unwrap();

    // The serialized YAML must contain an `overrides:` block — guard
    // against a future serde change silently dropping the field.
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(
        yaml.contains("overrides:"),
        "expected `overrides:` block in:\n{yaml}"
    );

    let reparsed = parse(&lockfile_path).unwrap();
    assert_eq!(reparsed.overrides.len(), 2);
    assert_eq!(reparsed.overrides.get("lodash").unwrap(), "4.17.21");
    assert_eq!(reparsed.overrides.get("foo").unwrap(), "npm:bar@^2");
}

/// Top-level blocks must follow pnpm's `sortLockfileKeys` ROOT_KEYS
/// order: `catalogs:` → `overrides:` → … → `patchedDependencies:`.
/// pnpm writes `catalogs:` right after `settings:` (before `overrides:`)
/// and `patchedDependencies:` after the checksums; any other position
/// produces a gratuitous diff against pnpm's output on every install.
#[test]
fn catalogs_overrides_patched_dependencies_match_pnpm_order() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut overrides = BTreeMap::new();
    overrides.insert("lodash".to_string(), "4.17.21".to_string());
    let mut patched_dependencies = BTreeMap::new();
    patched_dependencies.insert(
        "lodash@4.17.21".to_string(),
        "patches/lodash@4.17.21.patch".to_string(),
    );
    let mut default_catalog = BTreeMap::new();
    default_catalog.insert(
        "react".to_string(),
        CatalogEntry {
            specifier: "^18.2.0".to_string(),
            version: "18.2.0".to_string(),
        },
    );
    let mut catalogs = BTreeMap::new();
    catalogs.insert("default".to_string(), default_catalog);

    let graph = LockfileGraph {
        overrides,
        patched_dependencies,
        catalogs,
        ..Default::default()
    };

    let manifest = PackageJson {
        name: Some("test".to_string()),
        ..Default::default()
    };

    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();

    let catalogs_at = yaml.find("catalogs:").expect("catalogs:");
    let overrides_at = yaml.find("overrides:").expect("overrides:");
    let patched_at = yaml
        .find("patchedDependencies:")
        .expect("patchedDependencies:");
    assert!(
        catalogs_at < overrides_at && overrides_at < patched_at,
        "expected order: catalogs < overrides < patchedDependencies, got\n{yaml}"
    );
}

/// A patched dependency must serialize the way pnpm 9+ writes it
/// (ground-truthed against a pnpm@10.15.1 lockfile: a `{ hash, path }`
/// object, e.g. `is-odd@3.0.1:\n    hash: dcac…\n    path:
/// patches/is-odd@3.0.1.patch`). The importer's resolved version and
/// the `snapshots:` key carry a `(patch_hash=<hash>)` suffix, and the
/// `packages:` key stays the clean `name@version`. pnpm rejects a
/// lockfile that records the patch as a bare hash scalar or a hash-only
/// object with `ERR_PNPM_LOCKFILE_CONFIG_MISMATCH` (see
/// `patched_dependency_roundtrips_through_real_pnpm` for the captured
/// real-pnpm byte-for-byte form).
#[test]
fn patched_dependency_writes_pnpm10_hash_and_suffix_shape() {
    const HASH: &str = "82ff0b4d1c20272cdb11684045f28947472d5b8a10a04c0d972102d14815e536";
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut packages = BTreeMap::new();
    packages.insert(
        "ms@2.1.3".to_string(),
        LockedPackage {
            name: "ms".to_string(),
            version: "2.1.3".to_string(),
            integrity: Some("sha512-6FlzubTLZG3J2a/NVCAleEhjzq5oxgHyaCU9yYXvcLsvoVaHJq/s5xXI6/XXP6tz7R9xAOtHnSO/tXtF3WRTlA==".to_string()),
            dep_path: "ms@2.1.3".to_string(),
            ..Default::default()
        },
    );
    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "ms".to_string(),
            dep_path: "ms@2.1.3".to_string(),
            dep_type: DepType::Production,
            specifier: Some("2.1.3".to_string()),
        }],
    );
    let mut patched_dependencies = BTreeMap::new();
    patched_dependencies.insert("ms@2.1.3".to_string(), "patches/ms@2.1.3.patch".to_string());
    let mut patched_dependency_hashes = BTreeMap::new();
    patched_dependency_hashes.insert("ms@2.1.3".to_string(), HASH.to_string());

    let graph = LockfileGraph {
        importers,
        packages,
        patched_dependencies,
        patched_dependency_hashes,
        ..Default::default()
    };
    let mut deps = BTreeMap::new();
    deps.insert("ms".to_string(), "2.1.3".to_string());
    let manifest = PackageJson {
        name: Some("test".to_string()),
        dependencies: deps,
        ..Default::default()
    };

    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();

    // pnpm 9+ writes a `{ hash, path }` object as the
    // `patchedDependencies` value; a frozen install rejects anything
    // else (a bare hash scalar or a hash-only object) with
    // `ERR_PNPM_LOCKFILE_CONFIG_MISMATCH`.
    assert!(
        yaml.contains(&format!(
            "patchedDependencies:\n  ms@2.1.3:\n    hash: {HASH}\n    path: patches/ms@2.1.3.patch"
        )),
        "expected a {{ hash, path }} patchedDependencies entry in:\n{yaml}"
    );
    assert!(
        yaml.contains(&format!("version: 2.1.3(patch_hash={HASH})")),
        "expected importer version to carry the patch_hash suffix in:\n{yaml}"
    );
    assert!(
        yaml.contains(&format!("  ms@2.1.3(patch_hash={HASH}): {{}}")),
        "expected the snapshots key to carry the patch_hash suffix in:\n{yaml}"
    );
    assert!(
        yaml.contains("\n  ms@2.1.3:\n    resolution:"),
        "expected the packages key to stay the clean name@version in:\n{yaml}"
    );

    // Round-trip: the hash survives a re-parse. The path map is empty
    // for pnpm — the value on disk is a hash, not a path.
    let reparsed = parse(&lockfile_path).unwrap();
    assert!(
        reparsed.patched_dependencies.is_empty(),
        "pnpm reparse should leave the path map empty (the value is a hash): {:?}",
        reparsed.patched_dependencies
    );
    assert_eq!(
        reparsed.patched_dependency_hashes.get("ms@2.1.3").unwrap(),
        HASH
    );
}

/// Byte-for-byte parity with real pnpm@10.15.1's `patchedDependencies:`
/// block. The expected text was captured by running
/// `corepack pnpm@10.15.1 install` on the `patched-deps` conformance
/// fixture (`is-odd@3.0.1` patched via `patches/is-odd@3.0.1.patch`,
/// declared in `pnpm-workspace.yaml`) and reading the resulting
/// `pnpm-lock.yaml`. A bare-hash scalar (the form aube emitted before
/// this fix) made `pnpm install --frozen-lockfile` reject the lockfile
/// with `ERR_PNPM_LOCKFILE_CONFIG_MISMATCH`; the `{ hash, path }` object
/// below is the only shape pnpm accepts. This closes the gap the prior
/// nub→nub unit test missed (it asserted the bare-hash form).
#[test]
fn patched_dependency_roundtrips_through_real_pnpm() {
    // Captured from `corepack pnpm@10.15.1 install` on the patched-deps
    // fixture — the exact bytes real pnpm wrote.
    const HASH: &str = "dcac38e61b21e4c1fbc036fbd04c2c57fc5aca4d595709258e1654cf8529c5c1";
    const EXPECTED_BLOCK: &str =
        "patchedDependencies:\n  is-odd@3.0.1:\n    hash: dcac38e61b21e4c1fbc036fbd04c2c57fc5aca4d595709258e1654cf8529c5c1\n    path: patches/is-odd@3.0.1.patch";

    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut packages = BTreeMap::new();
    packages.insert(
        "is-odd@3.0.1".to_string(),
        LockedPackage {
            name: "is-odd".to_string(),
            version: "3.0.1".to_string(),
            integrity: Some("sha512-CQpnWPrDwmP1+SMHXZhtLtJv90yiyVfluGsX5iNCVkrhQtU3TQHsUWPG9wkdk9Lgd5yNpAg9jQEo90CBaXgWMA==".to_string()),
            dep_path: "is-odd@3.0.1".to_string(),
            ..Default::default()
        },
    );
    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "is-odd".to_string(),
            dep_path: "is-odd@3.0.1".to_string(),
            dep_type: DepType::Production,
            specifier: Some("3.0.1".to_string()),
        }],
    );
    let mut patched_dependencies = BTreeMap::new();
    patched_dependencies.insert(
        "is-odd@3.0.1".to_string(),
        "patches/is-odd@3.0.1.patch".to_string(),
    );
    let mut patched_dependency_hashes = BTreeMap::new();
    patched_dependency_hashes.insert("is-odd@3.0.1".to_string(), HASH.to_string());

    let graph = LockfileGraph {
        importers,
        packages,
        patched_dependencies,
        patched_dependency_hashes,
        ..Default::default()
    };
    let mut deps = BTreeMap::new();
    deps.insert("is-odd".to_string(), "3.0.1".to_string());
    let manifest = PackageJson {
        name: Some("conform-patched-deps".to_string()),
        dependencies: deps,
        ..Default::default()
    };

    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();

    assert!(
        yaml.contains(EXPECTED_BLOCK),
        "emitted patchedDependencies block diverges from real pnpm@10.15.1; expected to find:\n{EXPECTED_BLOCK}\nin:\n{yaml}"
    );
}

/// Cross-tool (issue #15): a pnpm-authored v9 lockfile records the
/// `patchedDependencies` value as a *bare per-file hash string* (the
/// shape the current pnpm CLI writes, e.g.
/// `graceful-fs@4.2.11: 68ebc232…`). aube must read it as a hash —
/// landing in `patched_dependency_hashes`, leaving the path map empty
/// — so a `--frozen-lockfile` install does not false-drift. Modeled on
/// a real pnpm-lock.yaml block.
#[test]
fn pnpm_authored_bare_hash_patched_dependency_reads_as_hash() {
    const HASH: &str = "68ebc232025360cb3dcd3081f4067f4e9fc022ab6b6f71a3230e86c7a5b337d1";
    let yaml = format!(
        "lockfileVersion: '9.0'\n\
         \n\
         patchedDependencies:\n  \
         graceful-fs@4.2.11: {HASH}\n\
         \n\
         importers:\n\n  .:\n    dependencies:\n      \
         graceful-fs:\n        specifier: 4.2.11\n        version: 4.2.11\n\
         \n\
         packages:\n\n  \
         graceful-fs@4.2.11:\n    resolution: {{integrity: sha512-AA==}}\n\
         \n\
         snapshots:\n\n  graceful-fs@4.2.11: {{}}\n"
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(&path, &yaml).unwrap();

    let graph = parse(&path).unwrap();
    assert!(
        graph.patched_dependencies.is_empty(),
        "pnpm's value is a hash, not a path — path map must stay empty: {:?}",
        graph.patched_dependencies
    );
    assert_eq!(
        graph
            .patched_dependency_hashes
            .get("graceful-fs@4.2.11")
            .unwrap(),
        HASH
    );

    // Re-emitting a graph parsed straight from a lockfile (no install,
    // so the patch path was never resolved from disk) writes a
    // hash-only object — the honest output when the path is unknown. A
    // real install resolves the path first via `record_patches_on_graph`
    // and writes the full `{ hash, path }` object pnpm's frozen check
    // requires (see `patched_dependency_roundtrips_through_real_pnpm`);
    // this path-less re-emit is not the frozen-round-trip flow.
    let manifest = PackageJson {
        name: Some("t".into()),
        dependencies: [("graceful-fs".to_string(), "4.2.11".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = dir.path().join("out.yaml");
    write(&out, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&out).unwrap();
    assert!(
        written.contains(&format!(
            "patchedDependencies:\n  graceful-fs@4.2.11:\n    hash: {HASH}"
        )),
        "expected hash-only object re-emit:\n{written}"
    );
}

/// Cross-tool: the legacy pnpm 9.x `{ hash, path }` object form is read
/// for its hash (the authoritative field — pnpm's `migratePatchedDependencies`
/// keeps only `.hash`). The path is discarded; the path map stays empty.
#[test]
fn pnpm_legacy_object_patched_dependency_reads_hash_only() {
    let yaml = "lockfileVersion: '9.0'\n\
         \n\
         patchedDependencies:\n  \
         is-positive@3.1.0:\n    \
         hash: b27bbf2d83e68cac4491a38dd8b846aadd55d9c7bf8a4971139465c4de3566ce\n    \
         path: patches/is-positive@3.1.0.patch\n\
         \n\
         importers:\n\n  .:\n    dependencies:\n      \
         is-positive:\n        specifier: 3.1.0\n        version: 3.1.0\n\
         \n\
         packages:\n\n  \
         is-positive@3.1.0:\n    resolution: {integrity: sha512-AA==}\n\
         \n\
         snapshots:\n\n  is-positive@3.1.0: {}\n";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(&path, yaml).unwrap();

    let graph = parse(&path).unwrap();
    assert!(graph.patched_dependencies.is_empty());
    assert_eq!(
        graph
            .patched_dependency_hashes
            .get("is-positive@3.1.0")
            .unwrap(),
        "b27bbf2d83e68cac4491a38dd8b846aadd55d9c7bf8a4971139465c4de3566ce"
    );
}

#[test]
fn empty_overrides_block_omitted_from_yaml() {
    // Default-empty overrides should not introduce an `overrides:` key
    // in the lockfile — important for byte-identical parity with pnpm
    // on the no-overrides path.
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    let graph = LockfileGraph::default();
    let manifest = PackageJson {
        name: Some("test".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: BTreeMap::new(),
        dev_dependencies: BTreeMap::new(),
        peer_dependencies: BTreeMap::new(),
        optional_dependencies: BTreeMap::new(),
        update_config: None,
        scripts: BTreeMap::new(),
        engines: BTreeMap::new(),
        dev_engines: None,
        workspaces: None,
        bundled_dependencies: None,
        extra: BTreeMap::new(),
    };
    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(
        !yaml.contains("overrides:"),
        "unexpected overrides block:\n{yaml}"
    );
}

/// `packageExtensionsChecksum:` / `pnpmfileChecksum:` must round-trip
/// verbatim and land right after `overrides:` and before `importers:`,
/// each as its own blank-line-separated top-level scalar — exactly
/// where pnpm writes them. Any other shape produces a gratuitous diff
/// against pnpm's output (and a wrong/absent value makes pnpm re-resolve
/// or abort a frozen install).
#[test]
fn config_checksums_round_trip_in_pnpm_order() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut overrides = BTreeMap::new();
    overrides.insert("lodash".to_string(), "4.17.21".to_string());

    let graph = LockfileGraph {
        overrides,
        package_extensions_checksum: Some(
            "sha256-9yDK//Ix13a8CrWmJGIeVC0z1tCnQxNHOLTw47oh10s=".to_string(),
        ),
        pnpmfile_checksum: Some("sha256-EOT4Rq2KGdwdUwAI9FuL2HmoawSWgN2C+QLiGsRhY20=".to_string()),
        ..Default::default()
    };

    let manifest = PackageJson {
        name: Some("test".to_string()),
        version: Some("0.0.0".to_string()),
        ..Default::default()
    };

    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();

    // Exact lines pnpm emits (unquoted scalars, `sha256-` prefix kept).
    assert!(
        yaml.contains(
            "packageExtensionsChecksum: sha256-9yDK//Ix13a8CrWmJGIeVC0z1tCnQxNHOLTw47oh10s="
        ),
        "missing packageExtensionsChecksum line:\n{yaml}"
    );
    assert!(
        yaml.contains("pnpmfileChecksum: sha256-EOT4Rq2KGdwdUwAI9FuL2HmoawSWgN2C+QLiGsRhY20="),
        "missing pnpmfileChecksum line:\n{yaml}"
    );

    // Order: overrides < packageExtensionsChecksum < pnpmfileChecksum < importers.
    let overrides_at = yaml.find("overrides:").expect("overrides:");
    let pe_at = yaml
        .find("packageExtensionsChecksum:")
        .expect("packageExtensionsChecksum:");
    let pf_at = yaml.find("pnpmfileChecksum:").expect("pnpmfileChecksum:");
    let importers_at = yaml.find("importers:").expect("importers:");
    assert!(
        overrides_at < pe_at && pe_at < pf_at && pf_at < importers_at,
        "expected order overrides < packageExtensionsChecksum < pnpmfileChecksum < importers, got:\n{yaml}"
    );

    // Each checksum is its own blank-line-separated top-level section,
    // matching pnpm's spacing.
    assert!(
        yaml.contains(
            "\n\npackageExtensionsChecksum: sha256-9yDK//Ix13a8CrWmJGIeVC0z1tCnQxNHOLTw47oh10s=\n\n"
        ),
        "packageExtensionsChecksum not blank-line separated:\n{yaml}"
    );
    assert!(
        yaml.contains(
            "\n\npnpmfileChecksum: sha256-EOT4Rq2KGdwdUwAI9FuL2HmoawSWgN2C+QLiGsRhY20=\n\n"
        ),
        "pnpmfileChecksum not blank-line separated:\n{yaml}"
    );

    let reparsed = parse(&lockfile_path).unwrap();
    assert_eq!(
        reparsed.package_extensions_checksum.as_deref(),
        Some("sha256-9yDK//Ix13a8CrWmJGIeVC0z1tCnQxNHOLTw47oh10s=")
    );
    assert_eq!(
        reparsed.pnpmfile_checksum.as_deref(),
        Some("sha256-EOT4Rq2KGdwdUwAI9FuL2HmoawSWgN2C+QLiGsRhY20=")
    );
}

/// A graph with no config checksums must not introduce either key —
/// byte-identical parity with pnpm on the no-extensions / no-pnpmfile
/// path.
#[test]
fn absent_config_checksums_are_omitted_from_yaml() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    let graph = LockfileGraph::default();
    let manifest = PackageJson {
        name: Some("test".to_string()),
        version: Some("0.0.0".to_string()),
        ..Default::default()
    };
    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(
        !yaml.contains("packageExtensionsChecksum:"),
        "unexpected packageExtensionsChecksum:\n{yaml}"
    );
    assert!(
        !yaml.contains("pnpmfileChecksum:"),
        "unexpected pnpmfileChecksum:\n{yaml}"
    );
}

#[test]
fn test_write_dev_and_optional_deps() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut packages = BTreeMap::new();
    for (name, ver) in [("foo", "1.0.0"), ("bar", "2.0.0"), ("baz", "3.0.0")] {
        packages.insert(
            format!("{name}@{ver}"),
            LockedPackage {
                name: name.to_string(),
                version: ver.to_string(),
                integrity: None,
                dependencies: BTreeMap::new(),
                dep_path: format!("{name}@{ver}"),
                ..Default::default()
            },
        );
    }

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1.0.0".to_string()),
            },
            DirectDep {
                name: "bar".to_string(),
                dep_path: "bar@2.0.0".to_string(),
                dep_type: DepType::Dev,
                specifier: Some("^2.0.0".to_string()),
            },
            DirectDep {
                name: "baz".to_string(),
                dep_path: "baz@3.0.0".to_string(),
                dep_type: DepType::Optional,
                specifier: Some("^3.0.0".to_string()),
            },
        ],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let mut deps = BTreeMap::new();
    deps.insert("foo".to_string(), "^1.0.0".to_string());
    let mut dev_deps = BTreeMap::new();
    dev_deps.insert("bar".to_string(), "^2.0.0".to_string());
    let mut opt_deps = BTreeMap::new();
    opt_deps.insert("baz".to_string(), "^3.0.0".to_string());

    let manifest = PackageJson {
        name: Some("test".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: deps,
        dev_dependencies: dev_deps,
        peer_dependencies: BTreeMap::new(),
        optional_dependencies: opt_deps,
        update_config: None,
        scripts: BTreeMap::new(),
        engines: BTreeMap::new(),
        dev_engines: None,
        workspaces: None,
        bundled_dependencies: None,
        extra: BTreeMap::new(),
    };

    write(&lockfile_path, &graph, &manifest).unwrap();

    let reparsed = parse(&lockfile_path).unwrap();
    let root_deps = reparsed.importers.get(".").unwrap();
    assert_eq!(root_deps.len(), 3);

    let bar = root_deps.iter().find(|d| d.name == "bar").unwrap();
    assert_eq!(bar.dep_type, DepType::Dev);

    let baz = root_deps.iter().find(|d| d.name == "baz").unwrap();
    assert_eq!(baz.dep_type, DepType::Optional);
}

#[test]
fn test_catalogs_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut default_cat = BTreeMap::new();
    default_cat.insert(
        "react".to_string(),
        CatalogEntry {
            specifier: "^18.0.0".to_string(),
            version: "18.2.0".to_string(),
        },
    );
    let mut catalogs = BTreeMap::new();
    catalogs.insert("default".to_string(), default_cat);

    let graph = LockfileGraph {
        catalogs,
        ..Default::default()
    };
    let manifest = PackageJson {
        name: Some("test".to_string()),
        version: Some("0.0.0".to_string()),
        ..Default::default()
    };
    write(&lockfile_path, &graph, &manifest).unwrap();

    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(
        yaml.contains("catalogs:"),
        "missing catalogs section: {yaml}"
    );
    assert!(yaml.contains("react"), "missing entry: {yaml}");

    let reparsed = parse(&lockfile_path).unwrap();
    let entry = reparsed
        .catalogs
        .get("default")
        .and_then(|c| c.get("react"))
        .expect("react catalog entry");
    assert_eq!(entry.specifier, "^18.0.0");
    assert_eq!(entry.version, "18.2.0");
}

#[test]
fn ignored_optional_dependencies_section_matches_pnpm_order() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut ignored_optional_dependencies = std::collections::BTreeSet::new();
    ignored_optional_dependencies.insert("fsevents".to_string());

    let mut default_cat = BTreeMap::new();
    default_cat.insert(
        "react".to_string(),
        CatalogEntry {
            specifier: "^18.0.0".to_string(),
            version: "18.2.0".to_string(),
        },
    );
    let mut catalogs = BTreeMap::new();
    catalogs.insert("default".to_string(), default_cat);

    let graph = LockfileGraph {
        ignored_optional_dependencies,
        catalogs,
        ..Default::default()
    };
    let manifest = PackageJson {
        name: Some("test".to_string()),
        version: Some("0.0.0".to_string()),
        ..Default::default()
    };
    write(&lockfile_path, &graph, &manifest).unwrap();

    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    let catalogs = yaml.find("\ncatalogs:").expect("missing catalogs");
    let importers = yaml.find("\nimporters:").expect("missing importers");
    let packages = yaml.find("\npackages:").expect("missing packages");
    let ignored = yaml
        .find("\nignoredOptionalDependencies:")
        .expect("missing ignoredOptionalDependencies");
    let snapshots = yaml.find("\nsnapshots:").expect("missing snapshots");

    assert!(
        catalogs < importers && importers < packages && packages < ignored && ignored < snapshots,
        "unexpected pnpm section order:\n{yaml}"
    );
}

// Build a graph with one `link:` dep and one registry dep, write it
// with `excludeLinksFromLockfile: true`, and confirm the `link:`
// entry vanishes from the importer's `dependencies:` map while the
// registry dep survives. Guards the filter in the importer loop.
#[test]
fn exclude_links_from_lockfile_drops_link_deps_from_importer() {
    use crate::{LocalSource, LockfileSettings};
    use std::path::PathBuf;

    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut packages = BTreeMap::new();
    packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-abc==".to_string()),
            dep_path: "foo@1.0.0".to_string(),
            ..Default::default()
        },
    );
    packages.insert(
        "sibling@link:../sibling".to_string(),
        LockedPackage {
            name: "sibling".to_string(),
            version: "0.0.0".to_string(),
            dep_path: "sibling@link:../sibling".to_string(),
            local_source: Some(LocalSource::Link(PathBuf::from("../sibling"))),
            ..Default::default()
        },
    );

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1.0.0".to_string()),
            },
            DirectDep {
                name: "sibling".to_string(),
                dep_path: "sibling@link:../sibling".to_string(),
                dep_type: DepType::Production,
                specifier: Some("link:../sibling".to_string()),
            },
        ],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        settings: LockfileSettings {
            auto_install_peers: true,
            exclude_links_from_lockfile: true,
            lockfile_include_tarball_url: false,
        },
        ..Default::default()
    };

    let mut deps = BTreeMap::new();
    deps.insert("foo".to_string(), "^1.0.0".to_string());
    deps.insert("sibling".to_string(), "link:../sibling".to_string());
    let manifest = PackageJson {
        name: Some("root".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: deps,
        dev_dependencies: BTreeMap::new(),
        peer_dependencies: BTreeMap::new(),
        optional_dependencies: BTreeMap::new(),
        update_config: None,
        scripts: BTreeMap::new(),
        engines: BTreeMap::new(),
        dev_engines: None,
        workspaces: None,
        bundled_dependencies: None,
        extra: BTreeMap::new(),
    };

    write(&lockfile_path, &graph, &manifest).unwrap();

    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(
        yaml.contains("excludeLinksFromLockfile: true"),
        "settings header must record the flag: {yaml}"
    );
    assert!(
        !yaml.contains("sibling:"),
        "sibling link dep should be filtered out of importers: {yaml}"
    );
    assert!(
        yaml.contains("foo:"),
        "registry dep foo must still appear: {yaml}"
    );

    // Sanity: with the flag off, the same graph keeps the link dep.
    let graph_off = LockfileGraph {
        settings: LockfileSettings::default(),
        ..graph
    };
    write(&lockfile_path, &graph_off, &manifest).unwrap();
    let yaml_off = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(
        yaml_off.contains("sibling:"),
        "with flag off, sibling must reappear: {yaml_off}"
    );
}

#[test]
fn writer_uses_pnpm_resolution_types_for_portal_and_exec() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let portal_source = LocalSource::Portal(std::path::PathBuf::from("./packages/portal"));
    let exec_source = LocalSource::Exec(std::path::PathBuf::from("./scripts/generate-exec.js"));
    let portal_dep_path = portal_source.dep_path("portal-pkg");
    let exec_dep_path = exec_source.dep_path("exec-pkg");

    let mut packages = BTreeMap::new();
    packages.insert(
        portal_dep_path.clone(),
        LockedPackage {
            name: "portal-pkg".to_string(),
            version: "1.0.0".to_string(),
            dep_path: portal_dep_path.clone(),
            local_source: Some(portal_source),
            ..Default::default()
        },
    );
    packages.insert(
        exec_dep_path.clone(),
        LockedPackage {
            name: "exec-pkg".to_string(),
            version: "2.0.0".to_string(),
            dep_path: exec_dep_path.clone(),
            local_source: Some(exec_source),
            ..Default::default()
        },
    );

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "portal-pkg".to_string(),
                dep_path: portal_dep_path,
                dep_type: DepType::Production,
                specifier: Some("portal:./packages/portal".to_string()),
            },
            DirectDep {
                name: "exec-pkg".to_string(),
                dep_path: exec_dep_path,
                dep_type: DepType::Production,
                specifier: Some("exec:./scripts/generate-exec.js".to_string()),
            },
        ],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let manifest = PackageJson {
        name: Some("root".to_string()),
        version: Some("0.0.0".to_string()),
        ..Default::default()
    };

    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();

    assert!(
        yaml.contains("portal-pkg@portal:./packages/portal:"),
        "portal package should be written:\n{yaml}"
    );
    assert!(
        yaml.contains("resolution: {directory: ./packages/portal, type: directory}"),
        "portal should use pnpm's directory resolution type:\n{yaml}"
    );
    assert!(
        !yaml.contains("exec-pkg@exec:./scripts/generate-exec.js:"),
        "exec packages should be omitted from pnpm packages entries:\n{yaml}"
    );
    assert!(
        !yaml.contains("type: portal") && !yaml.contains("type: exec"),
        "pnpm lockfiles must not contain non-standard local source types:\n{yaml}"
    );
}

#[test]
fn test_parse_invalid_yaml() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(&path, "{{{{not yaml").unwrap();
    assert!(parse(&path).is_err());
}

#[test]
fn test_parse_nonexistent_file() {
    let path = Path::new("/nonexistent/pnpm-lock.yaml");
    assert!(parse(path).is_err());
}

// Byte-parity with a real pnpm-lock.yaml. The fixture was produced by
// `pnpm install` against a `{ chalk, picocolors, semver }` manifest and
// lightly pinned — if pnpm's own output format drifts in a future
// release, regenerate the fixture rather than loosening the assertion.
// The test guards against silent regressions in the four churn sources
// we fixed: stray `time:`, block-form `resolution:`, missing blank
// lines, and dropped `engines:` / `hasBin:`.
#[test]
fn test_write_byte_identical_to_native_pnpm() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pnpm-native.yaml");
    // Windows' `core.autocrlf=true` rewrites checked-out files to
    // CRLF even when `.gitattributes` asks for LF; normalize both
    // sides before comparing so a misconfigured checkout gets a
    // meaningful failure rather than a line-ending false positive.
    let original = std::fs::read_to_string(&fixture)
        .unwrap()
        .replace("\r\n", "\n");

    let graph = parse(&fixture).unwrap();
    let manifest = PackageJson {
        name: Some("aube-lockfile-stability".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [
            ("chalk".to_string(), "^4.1.2".to_string()),
            ("picocolors".to_string(), "^1.1.1".to_string()),
            ("semver".to_string(), "^7.6.3".to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("pnpm-lock.yaml");
    write(&out, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&out).unwrap();

    if written != original {
        // pretty-print a short contextual diff so CI logs are actionable.
        let diff = similar_diff(&original, &written);
        panic!(
            "pnpm writer drifted from native pnpm output:\n{diff}\n\n--- full written output ---\n{written}"
        );
    }
}

// pnpm never records workspace members in `packages:`/`snapshots:` —
// the consuming importer carries `version: link:<dir>` with the path
// relative to *that importer* and the manifest's `workspace:` specifier
// preserved (verified against pnpm 10.15.1). The resolver's
// workspace-link path records such deps with a registry-style
// `name@version` dep_path and no package entry, so the writer has to
// recover the member's directory from the sibling importer's manifest.
#[test]
fn workspace_deps_write_importer_relative_link_versions() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    for (rel, body) in [
        ("packages/app", r#"{"name": "@ws/app", "version": "1.0.0"}"#),
        (
            "packages/core",
            r#"{"name": "@ws/core", "version": "1.0.0"}"#,
        ),
    ] {
        let pkg_dir = dir.path().join(rel);
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("package.json"), body).unwrap();
    }

    let mut packages = BTreeMap::new();
    packages.insert(
        "ms@2.1.3".to_string(),
        LockedPackage {
            name: "ms".to_string(),
            version: "2.1.3".to_string(),
            integrity: Some("sha512-abc==".to_string()),
            dep_path: "ms@2.1.3".to_string(),
            ..Default::default()
        },
    );
    let mut importers = BTreeMap::new();
    importers.insert(".".to_string(), vec![]);
    importers.insert(
        "packages/app".to_string(),
        vec![
            DirectDep {
                name: "@ws/core".to_string(),
                dep_path: "@ws/core@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("workspace:^".to_string()),
            },
            DirectDep {
                name: "ms".to_string(),
                dep_path: "ms@2.1.3".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^2.1.3".to_string()),
            },
        ],
    );
    importers.insert("packages/core".to_string(), vec![]);
    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let manifest = PackageJson {
        name: Some("ws-root".to_string()),
        version: Some("1.0.0".to_string()),
        ..Default::default()
    };

    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();

    assert!(
        yaml.contains("version: link:../core"),
        "workspace dep must render as link relative to the consuming importer: {yaml}"
    );
    assert!(
        !yaml.contains("version: 1.0.0"),
        "workspace dep must not be recorded as a registry version: {yaml}"
    );
    assert!(
        !yaml.contains("'@ws/core@1.0.0'") && !yaml.contains("@ws/core@1.0.0:"),
        "workspace member must not appear in packages/snapshots: {yaml}"
    );

    // The reader synthesizes the member back as a root-relative link,
    // and a re-write stays byte-stable.
    let reparsed = parse(&lockfile_path).unwrap();
    let member = reparsed
        .packages
        .values()
        .find(|p| p.name == "@ws/core")
        .expect("reparsed graph must carry the workspace member as a link");
    assert_eq!(
        member.local_source,
        Some(LocalSource::Link("packages/core".into()))
    );
    let rewrite_path = dir.path().join("pnpm-lock-2.yaml");
    write(&rewrite_path, &reparsed, &manifest).unwrap();
    assert_eq!(
        yaml,
        std::fs::read_to_string(&rewrite_path).unwrap(),
        "fresh-resolve and reparsed graphs must serialize identically"
    );
}

// Same property at the whole-file level: a workspace lockfile written
// by native pnpm 10.15.1 (fixture generated from a real install) must
// survive parse → write byte-identically — in particular the
// importer-relative `link:../<dir>` versions, which the graph stores
// root-relative internally.
#[test]
fn workspace_lockfile_round_trips_byte_identical_to_native_pnpm() {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pnpm-native-workspace.yaml");
    let original = std::fs::read_to_string(&fixture)
        .unwrap()
        .replace("\r\n", "\n");

    let graph = parse(&fixture).unwrap();
    let manifest = PackageJson {
        name: Some("workspace-fixture".to_string()),
        version: Some("1.0.0".to_string()),
        ..Default::default()
    };

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("pnpm-lock.yaml");
    write(&out, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&out).unwrap();

    if written != original {
        let diff = similar_diff(&original, &written);
        panic!("pnpm writer drifted from native pnpm workspace output:\n{diff}");
    }
}

// Minimal line diff for the byte-parity test failure message. We don't
// pull in a diff crate just for this — the lockfile is small enough
// that a line-by-line comparison is readable.
/// Line-aligned diff with a bounded lookahead so a single
/// insertion doesn't flag every following line as "modified".
/// When sides diverge at `(i, j)`, scan up to `LOOKAHEAD` steps in
/// both directions for the nearest `al[ii] == bl[jj]` and emit the
/// skipped-over ranges as `- …` / `+ …` runs; that keeps the
/// failure output readable for the ≤100-line fixtures this test
/// exercises without pulling in a full LCS dependency.
fn similar_diff(a: &str, b: &str) -> String {
    const LOOKAHEAD: usize = 8;
    let al: Vec<&str> = a.lines().collect();
    let bl: Vec<&str> = b.lines().collect();
    let mut out = String::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < al.len() || j < bl.len() {
        if i < al.len() && j < bl.len() && al[i] == bl[j] {
            i += 1;
            j += 1;
            continue;
        }
        // Find the nearest resync point within the lookahead
        // window. `k` is the combined distance from `(i, j)`;
        // smaller `k` wins, matching how a developer eyeballs
        // the diff.
        let mut sync: Option<(usize, usize)> = None;
        'outer: for k in 1..=LOOKAHEAD {
            for dx in 0..=k {
                let dy = k - dx;
                let ii = i + dx;
                let jj = j + dy;
                if ii < al.len() && jj < bl.len() && al[ii] == bl[jj] {
                    sync = Some((ii, jj));
                    break 'outer;
                }
            }
        }
        match sync {
            Some((ii, jj)) => {
                for line in &al[i..ii] {
                    out.push_str(&format!("  - {line:?}\n"));
                }
                for line in &bl[j..jj] {
                    out.push_str(&format!("  + {line:?}\n"));
                }
                i = ii;
                j = jj;
            }
            None => {
                // No sync in the window — dump the rest and stop.
                for line in &al[i..] {
                    out.push_str(&format!("  - {line:?}\n"));
                }
                for line in &bl[j..] {
                    out.push_str(&format!("  + {line:?}\n"));
                }
                break;
            }
        }
    }
    out
}

#[test]
fn parse_multi_document_lockfile_picks_project_doc() {
    // pnpm v11 emits two YAML documents in one file: a bootstrap
    // doc for `packageManagerDependencies` and the real project
    // lockfile. We want the latter.
    let yaml = r#"---
lockfileVersion: '9.0'

importers:

  .:
    packageManagerDependencies:
      pnpm:
        specifier: 11.0.0-rc.1
        version: 11.0.0-rc.1

packages:

  'pnpm@11.0.0-rc.1':
    resolution: {integrity: sha512-aaa}

snapshots:

  'pnpm@11.0.0-rc.1': {}

---
lockfileVersion: '9.0'

settings:
  autoInstallPeers: true

importers:

  .:
    dependencies:
      lodash:
        specifier: ^4.17.0
        version: 4.17.21

packages:

  'lodash@4.17.21':
    resolution: {integrity: sha512-bbb}

snapshots:

  'lodash@4.17.21': {}
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(&path, yaml).unwrap();
    let graph = parse(&path).expect("multi-doc lockfile should parse");
    let root = graph.importers.get(".").expect("root importer");
    let names: Vec<_> = root.iter().map(|d| d.name.as_str()).collect();
    assert!(
        names.contains(&"lodash"),
        "expected lodash from project doc, got {names:?}"
    );
    assert!(
        !names.contains(&"pnpm"),
        "bootstrap doc's packageManagerDependencies should not leak in, got {names:?}"
    );
}

#[test]
fn snapshot_optional_and_transitive_peer_deps_roundtrip() {
    let yaml = r#"lockfileVersion: '9.0'
settings:
  autoInstallPeers: true
importers:
  .:
    dependencies:
      '@reflink/reflink':
        specifier: ^0.1.19
        version: 0.1.19
      '@babel/generator':
        specifier: ^7.29.1
        version: 7.29.1
packages:
  '@reflink/reflink-darwin-arm64@0.1.19':
    resolution: {integrity: sha512-darwin}
    cpu: [arm64]
    os: [darwin]
  '@reflink/reflink@0.1.19':
    resolution: {integrity: sha512-reflink}
  '@babel/generator@7.29.1':
    resolution: {integrity: sha512-gen}
  '@babel/parser@7.29.2':
    resolution: {integrity: sha512-parser}
snapshots:
  '@reflink/reflink-darwin-arm64@0.1.19':
    optional: true
  '@reflink/reflink@0.1.19':
    optionalDependencies:
      '@reflink/reflink-darwin-arm64': 0.1.19
  '@babel/generator@7.29.1':
    dependencies:
      '@babel/parser': 7.29.2
    transitivePeerDependencies:
      - supports-color
  '@babel/parser@7.29.2': {}
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(&path, yaml).unwrap();

    let graph = parse(&path).unwrap();
    let darwin = graph
        .packages
        .get("@reflink/reflink-darwin-arm64@0.1.19")
        .expect("darwin snapshot present");
    assert!(darwin.optional, "optional: true must round-trip");

    let generator = graph
        .packages
        .get("@babel/generator@7.29.1")
        .expect("generator snapshot present");
    assert_eq!(
        generator.transitive_peer_dependencies,
        vec!["supports-color".to_string()],
    );

    let parser_pkg = graph.packages.get("@babel/parser@7.29.2").unwrap();
    assert!(!parser_pkg.optional);
    assert!(parser_pkg.transitive_peer_dependencies.is_empty());

    let manifest = PackageJson {
        name: Some("rt".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: [
            ("@reflink/reflink".to_string(), "^0.1.19".to_string()),
            ("@babel/generator".to_string(), "^7.29.1".to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let out_path = dir.path().join("out.yaml");
    write(&out_path, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&out_path).unwrap();

    assert!(
        written.contains("optional: true"),
        "writer must emit optional: true; got:\n{written}"
    );
    assert!(
        written.contains("transitivePeerDependencies:"),
        "writer must emit transitivePeerDependencies; got:\n{written}"
    );
    assert!(
        written.contains("- supports-color"),
        "writer must list bubbled peers; got:\n{written}"
    );

    // Field order within a snapshot must match pnpm's
    // `LockfilePackageSnapshot` emit order so a round-trip stays
    // diff-clean against pnpm's own output: dependencies →
    // optionalDependencies → transitivePeerDependencies → optional.
    // The `@babel/generator` snapshot has `dependencies` followed
    // by `transitivePeerDependencies`, which is the pair Greptile
    // flagged as ordered wrong.
    let deps_line = "\n    dependencies:\n";
    let tpd_line = "\n    transitivePeerDependencies:\n";
    let deps_at = written.find(deps_line).expect("dependencies line emitted");
    let tpd_at = written
        .find(tpd_line)
        .expect("transitivePeerDependencies line emitted");
    assert!(
        deps_at < tpd_at,
        "dependencies must precede transitivePeerDependencies; got:\n{written}"
    );

    let reparsed = parse(&out_path).unwrap();
    assert!(
        reparsed
            .packages
            .get("@reflink/reflink-darwin-arm64@0.1.19")
            .unwrap()
            .optional
    );
    assert_eq!(
        reparsed
            .packages
            .get("@babel/generator@7.29.1")
            .unwrap()
            .transitive_peer_dependencies,
        vec!["supports-color".to_string()]
    );
}

#[test]
fn adversarial_native_pnpm_features_roundtrip_together() {
    let yaml = r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: false
  excludeLinksFromLockfile: false
  lockfileIncludeTarballUrl: true

overrides:
  is-number: 6.0.0
  react: 'catalog:'

patchedDependencies:
  is-odd@3.0.1:
    path: patches/is-odd@3.0.1.patch
    hash: sha256-deadbeef

catalogs:
  default:
    react:
      specifier: ^18.2.0
      version: 18.2.0
  evens:
    is-even:
      specifier: ^1.0.0
      version: 1.0.0

importers:

  .:
    dependencies:
      odd-alias:
        specifier: npm:is-odd@3.0.1
        version: is-odd@3.0.1
      react:
        specifier: 'catalog:'
        version: 18.2.0
    devDependencies:
      peer-host:
        specifier: 1.0.0
        version: 1.0.0(@types/node@20.11.0)
    optionalDependencies:
      fsevents:
        specifier: ^2.3.3
        version: 2.3.3
    skippedOptionalDependencies:
      optional-native:
        specifier: ^1.0.0
        version: 1.0.0

packages:

  '@types/node@20.11.0':
    resolution: {integrity: sha512-types}

  fsevents@2.3.3:
    resolution: {integrity: sha512-fsevents, tarball: https://registry.npmjs.org/fsevents/-/fsevents-2.3.3.tgz}
    os: [darwin]
    cpu: [x64]

  is-number@6.0.0:
    resolution: {integrity: sha512-number}

  is-odd@3.0.1:
    resolution: {integrity: sha512-odd, tarball: https://registry.npmjs.org/is-odd/-/is-odd-3.0.1.tgz}

  peer-host@1.0.0(@types/node@20.11.0):
    resolution: {integrity: sha512-peer}
    peerDependencies:
      '@types/node': '>=20'
    peerDependenciesMeta:
      '@types/node':
        optional: true

  react@18.2.0:
    resolution: {integrity: sha512-react}

ignoredOptionalDependencies:
  - optional-native

snapshots:

  '@types/node@20.11.0': {}

  fsevents@2.3.3:
    optional: true

  is-number@6.0.0: {}

  is-odd@3.0.1:
    dependencies:
      is-number: 6.0.0
    transitivePeerDependencies:
      - '@types/node'

  peer-host@1.0.0(@types/node@20.11.0): {}

  react@18.2.0: {}
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(&path, yaml).unwrap();

    let graph = parse(&path).unwrap();

    assert!(!graph.settings.auto_install_peers);
    assert!(graph.settings.lockfile_include_tarball_url);
    assert_eq!(graph.overrides.get("react").unwrap(), "catalog:");
    // pnpm's patch value is a hash; the legacy `{ path, hash }` object
    // is read for its hash only, and the path map stays empty.
    assert!(graph.patched_dependencies.is_empty());
    assert_eq!(
        graph
            .patched_dependency_hashes
            .get("is-odd@3.0.1")
            .unwrap(),
        "sha256-deadbeef"
    );
    assert_eq!(
        graph.catalogs["evens"]["is-even"].specifier, "^1.0.0",
        "named catalogs must survive parse"
    );
    assert!(
        graph
            .ignored_optional_dependencies
            .contains("optional-native")
    );
    assert_eq!(
        graph.skipped_optional_dependencies["."]["optional-native"],
        "^1.0.0"
    );

    let root = graph.importers.get(".").expect("root importer");
    let alias_dep = root.iter().find(|d| d.name == "odd-alias").unwrap();
    assert_eq!(alias_dep.dep_path, "odd-alias@3.0.1");
    assert_eq!(alias_dep.specifier.as_deref(), Some("npm:is-odd@3.0.1"));
    let peer_dep = root.iter().find(|d| d.name == "peer-host").unwrap();
    assert_eq!(peer_dep.dep_type, DepType::Dev);
    let optional_dep = root.iter().find(|d| d.name == "fsevents").unwrap();
    assert_eq!(optional_dep.dep_type, DepType::Optional);

    let alias_pkg = graph.packages.get("odd-alias@3.0.1").unwrap();
    assert_eq!(alias_pkg.alias_of.as_deref(), Some("is-odd"));
    assert_eq!(
        alias_pkg
            .transitive_peer_dependencies
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["@types/node"]
    );
    let fsevents = graph.packages.get("fsevents@2.3.3").unwrap();
    assert!(fsevents.optional);
    assert_eq!(fsevents.os.as_slice(), ["darwin"]);
    assert_eq!(fsevents.cpu.as_slice(), ["x64"]);
    assert_eq!(
        fsevents.tarball_url.as_deref(),
        Some("https://registry.npmjs.org/fsevents/-/fsevents-2.3.3.tgz")
    );
    let peer_host = graph
        .packages
        .get("peer-host@1.0.0(@types/node@20.11.0)")
        .unwrap();
    assert_eq!(peer_host.peer_dependencies["@types/node"], ">=20");
    assert!(peer_host.peer_dependencies_meta["@types/node"].optional);

    let manifest = PackageJson {
        name: Some("adversarial-native-pnpm".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [
            ("odd-alias".to_string(), "npm:is-odd@3.0.1".to_string()),
            ("react".to_string(), "catalog:".to_string()),
        ]
        .into_iter()
        .collect(),
        dev_dependencies: [("peer-host".to_string(), "1.0.0".to_string())]
            .into_iter()
            .collect(),
        optional_dependencies: [("fsevents".to_string(), "^2.3.3".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = dir.path().join("out.yaml");
    write(&out, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&out).unwrap();

    for needle in [
        "lockfileIncludeTarballUrl: true",
        "overrides:",
        "patchedDependencies:",
        "catalogs:",
        "skippedOptionalDependencies:",
        "ignoredOptionalDependencies:",
        "aliasOf: is-odd",
        "peerDependencies:",
        "peerDependenciesMeta:",
        "transitivePeerDependencies:",
        "optional: true",
        "tarball: https://registry.npmjs.org/fsevents/-/fsevents-2.3.3.tgz",
    ] {
        assert!(
            written.contains(needle),
            "missing {needle:?} in:\n{written}"
        );
    }

    let catalogs_at = written.find("\ncatalogs:").expect("catalogs");
    let overrides_at = written.find("\noverrides:").expect("overrides");
    let patched_at = written
        .find("\npatchedDependencies:")
        .expect("patchedDependencies");
    let importers_at = written.find("\nimporters:").expect("importers");
    assert!(
        catalogs_at < overrides_at && overrides_at < patched_at && patched_at < importers_at,
        "pnpm top-level section order drifted:\n{written}"
    );
    let packages_at = written.find("\npackages:").expect("packages");
    let ignored_at = written
        .find("\nignoredOptionalDependencies:")
        .expect("ignored optional");
    let snapshots_at = written.find("\nsnapshots:").expect("snapshots");
    assert!(
        packages_at < ignored_at && ignored_at < snapshots_at,
        "ignoredOptionalDependencies must stay between packages and snapshots:\n{written}"
    );

    let reparsed = parse(&out).unwrap();
    assert_eq!(
        reparsed
            .patched_dependency_hashes
            .get("is-odd@3.0.1")
            .unwrap_or_else(|| panic!("patched dep hash lost after reparse:\n{written}")),
        "sha256-deadbeef"
    );
    assert_eq!(reparsed.catalogs["default"]["react"].version, "18.2.0");
    assert_eq!(
        reparsed
            .packages
            .get("odd-alias@3.0.1")
            .unwrap_or_else(|| panic!("alias package lost after reparse:\n{written}"))
            .alias_of
            .as_deref(),
        Some("is-odd")
    );
    assert!(reparsed.packages.get("fsevents@2.3.3").unwrap().optional);
    assert_eq!(
        reparsed.skipped_optional_dependencies["."]["optional-native"],
        "^1.0.0"
    );
}

#[test]
fn write_pnpm_lockfile_uses_native_alias_shape() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    let manifest = PackageJson {
        name: Some("alias-native-pnpm".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("odd-alias".to_string(), "npm:is-odd@3.0.1".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let graph = LockfileGraph {
        importers: [(
            ".".to_string(),
            vec![DirectDep {
                name: "odd-alias".to_string(),
                dep_path: "odd-alias@3.0.1".to_string(),
                dep_type: DepType::Production,
                specifier: Some("npm:is-odd@3.0.1".to_string()),
            }],
        )]
        .into_iter()
        .collect(),
        packages: [
            (
                "odd-alias@3.0.1".to_string(),
                LockedPackage {
                    name: "odd-alias".to_string(),
                    version: "3.0.1".to_string(),
                    integrity: Some("sha512-odd".to_string()),
                    dep_path: "odd-alias@3.0.1".to_string(),
                    alias_of: Some("is-odd".to_string()),
                    ..Default::default()
                },
            ),
            (
                "consumer@1.0.0".to_string(),
                LockedPackage {
                    name: "consumer".to_string(),
                    version: "1.0.0".to_string(),
                    integrity: Some("sha512-consumer".to_string()),
                    dep_path: "consumer@1.0.0".to_string(),
                    dependencies: [(
                        "odd-alias".to_string(),
                        "3.0.1(peer-host@1.0.0)".to_string(),
                    )]
                    .into_iter()
                    .collect(),
                    ..Default::default()
                },
            ),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    write(&path, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.contains("version: is-odd@3.0.1"), "{written}");
    assert!(written.contains("is-odd@3.0.1:"), "{written}");
    assert!(
        written.contains("odd-alias: is-odd@3.0.1(peer-host@1.0.0)"),
        "{written}"
    );
    assert!(!written.contains("aliasOf:"), "{written}");

    let reparsed = parse(&path).unwrap();
    let alias_pkg = reparsed.packages.get("odd-alias@3.0.1").unwrap();
    assert_eq!(alias_pkg.alias_of.as_deref(), Some("is-odd"));
}

#[test]
fn parse_synthesizes_npm_alias_from_pnpm_v9_lockfile() {
    // pnpm v9 encodes npm-aliases implicitly (importer key is the
    // alias, `version:` is `<real>@<resolved>`, no `aliasOf:`
    // field on the package entry). The reader must reconstruct
    // an alias-keyed LockedPackage with `alias_of=Some(real)` so
    // the linker creates `node_modules/<alias>` correctly.
    // Repro: https://github.com/rubnogueira/aube-exotic-bug
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      express-fork:
        specifier: npm:express@^4.22.1
        version: express@4.22.1

packages:
  express@4.22.1:
    resolution: {integrity: sha512-fake}
    engines: {node: '>= 0.10.0'}

snapshots:
  express@4.22.1: {}
"#,
    )
    .unwrap();

    let graph = parse(&path).unwrap();

    let root = graph.importers.get(".").expect("root importer");
    assert_eq!(root.len(), 1);
    let dep = &root[0];
    assert_eq!(dep.name, "express-fork", "DirectDep keeps the alias name");
    assert_eq!(
        dep.dep_path, "express-fork@4.22.1",
        "DirectDep dep_path is alias-keyed (not the malformed express-fork@express@4.22.1)"
    );
    assert_eq!(dep.specifier.as_deref(), Some("npm:express@^4.22.1"));

    let pkg = graph
        .packages
        .get("express-fork@4.22.1")
        .expect("synthesized alias-keyed package");
    assert_eq!(pkg.name, "express-fork");
    assert_eq!(pkg.alias_of.as_deref(), Some("express"));
    assert_eq!(pkg.dep_path, "express-fork@4.22.1");
    // Real-keyed entry stays in place — other importers may
    // reference the package directly, and the canonical entry is
    // needed for byte-identical round-trips back to pnpm format.
    let real = graph.packages.get("express@4.22.1").expect("real entry");
    assert_eq!(real.name, "express");
    assert!(real.alias_of.is_none());
}

#[test]
fn parse_synthesizes_npm_alias_from_pnpm_lockfile_catalog_specifier() {
    // pnpm-resolved catalog aliases keep `specifier: 'catalog:'`
    // in the importer block while the `version:` field already
    // carries the resolved alias (`<real>@<resolved>`). The
    // reader must detect the alias from the version shape alone
    // — gating on `specifier.starts_with("npm:")` would silently
    // drop the dep and leave node_modules empty.
    // Repro:
    //   https://github.com/jdx/aube/discussions/383#discussioncomment-16759640
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"
lockfileVersion: '9.0'

catalogs:
  default:
    beamcoder:
      specifier: npm:beamcoder-prebuild@0.7.1-rc.18
      version: 0.7.1-rc.18

importers:
  packages/app:
    dependencies:
      beamcoder:
        specifier: 'catalog:'
        version: beamcoder-prebuild@0.7.1-rc.18

packages:
  beamcoder-prebuild@0.7.1-rc.18:
    resolution: {integrity: sha512-fake}

snapshots:
  beamcoder-prebuild@0.7.1-rc.18: {}
"#,
    )
    .unwrap();

    let graph = parse(&path).unwrap();
    let app = graph
        .importers
        .get("packages/app")
        .expect("packages/app importer");
    assert_eq!(app.len(), 1, "alias-resolved catalog dep must be parsed");
    let dep = &app[0];
    assert_eq!(dep.name, "beamcoder", "DirectDep keeps the alias name");
    assert_eq!(
        dep.dep_path, "beamcoder@0.7.1-rc.18",
        "DirectDep dep_path is alias-keyed"
    );
    assert_eq!(dep.specifier.as_deref(), Some("catalog:"));

    let pkg = graph
        .packages
        .get("beamcoder@0.7.1-rc.18")
        .expect("synthesized alias-keyed package");
    assert_eq!(pkg.name, "beamcoder");
    assert_eq!(pkg.alias_of.as_deref(), Some("beamcoder-prebuild"));
}

#[test]
fn parse_synthesizes_npm_alias_when_real_name_is_scoped() {
    // Scoped real package + non-scoped alias: `parse_dep_path` must
    // correctly split `@scope/pkg` from the version when the
    // version field is `@scope/pkg@1.0.0`.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      types-alias:
        specifier: npm:@types/node@^20.0.0
        version: '@types/node@20.11.0'

packages:
  '@types/node@20.11.0':
    resolution: {integrity: sha512-fake}

snapshots:
  '@types/node@20.11.0': {}
"#,
    )
    .unwrap();

    let graph = parse(&path).unwrap();

    let root = graph.importers.get(".").expect("root importer");
    assert_eq!(root[0].name, "types-alias");
    assert_eq!(root[0].dep_path, "types-alias@20.11.0");

    let pkg = graph
        .packages
        .get("types-alias@20.11.0")
        .expect("synthesized alias-keyed package");
    assert_eq!(pkg.name, "types-alias");
    assert_eq!(pkg.alias_of.as_deref(), Some("@types/node"));
    let real = graph
        .packages
        .get("@types/node@20.11.0")
        .expect("real entry");
    assert_eq!(real.name, "@types/node");
    assert!(real.alias_of.is_none());
}

#[test]
fn parse_synthesizes_npm_alias_whose_target_is_a_file_package() {
    // A `file:` dep consumed under a different in-tree name gets the
    // same implicit alias encoding as a registry alias — the importer
    // key is the alias and `version:` is `<real_name>@file:<path>` —
    // but the target package lives in the lockfile's local-package
    // set, not in `packages:`/`snapshots:` keyed by name@version.
    // The alias synthesis must resolve against local packages too, or
    // the whole lockfile fails to parse (which takes down read-only
    // commands along with install). Shape taken verbatim from
    // vitejs/vite's committed pnpm-lock.yaml (playground/ssr-deps),
    // found by differential corpus testing against vite.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"
lockfileVersion: '9.0'

importers:
  playground/ssr-deps:
    dependencies:
      '@vitejs/test-optimized-cjs-with-nested-external':
        specifier: file:./optimized-with-nested-external
        version: '@vitejs/test-optimized-with-nested-external@file:playground/ssr-deps/optimized-with-nested-external'
      '@vitejs/test-optimized-with-nested-external':
        specifier: file:./optimized-with-nested-external
        version: file:playground/ssr-deps/optimized-with-nested-external

packages:
  '@vitejs/test-optimized-with-nested-external@file:playground/ssr-deps/optimized-with-nested-external':
    resolution: {directory: playground/ssr-deps/optimized-with-nested-external, type: directory}

snapshots:
  '@vitejs/test-optimized-with-nested-external@file:playground/ssr-deps/optimized-with-nested-external': {}
"#,
    )
    .unwrap();

    let graph = parse(&path).unwrap();

    let importer = graph
        .importers
        .get("playground/ssr-deps")
        .expect("playground/ssr-deps importer");
    assert_eq!(importer.len(), 2);

    // Local packages are keyed by the FS-safe hashed form
    // (`LocalSource::dep_path`), never the raw `name@file:<path>`
    // spelling — the graph key doubles as the `.aube/` subdir name,
    // and the linker resolves a dep by looking `<name>@<tail>` up in
    // `graph.packages` directly. The synthesized alias clone must
    // land on the same convention or the linker never finds it.
    let source =
        LocalSource::Directory("playground/ssr-deps/optimized-with-nested-external".into());
    let alias_key = source.dep_path("@vitejs/test-optimized-cjs-with-nested-external");
    let real_key = source.dep_path("@vitejs/test-optimized-with-nested-external");

    for (name, key) in [
        (
            "@vitejs/test-optimized-cjs-with-nested-external",
            &alias_key,
        ),
        ("@vitejs/test-optimized-with-nested-external", &real_key),
    ] {
        let dep = importer
            .iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("importer dep {name} missing"));
        assert_eq!(
            &dep.dep_path, key,
            "importer DirectDep for {name} must point at the hashed local key"
        );
    }

    let alias_pkg = graph
        .packages
        .get(&alias_key)
        .expect("synthesized alias-keyed package for the file: target");
    assert_eq!(
        alias_pkg.name,
        "@vitejs/test-optimized-cjs-with-nested-external"
    );
    assert_eq!(
        alias_pkg.alias_of.as_deref(),
        Some("@vitejs/test-optimized-with-nested-external")
    );
    assert!(
        alias_pkg.local_source.is_some(),
        "alias clone keeps the file: local_source so the linker links the directory, not the registry"
    );

    let real_pkg = graph.packages.get(&real_key).expect("real file: entry");
    assert_eq!(real_pkg.name, "@vitejs/test-optimized-with-nested-external");
    assert!(real_pkg.alias_of.is_none());
}

#[test]
fn parse_synthesizes_npm_alias_for_transitive_deps() {
    // pnpm encodes npm-aliased *transitive* deps as
    // `<alias>: <real>@<resolved>` inside a snapshot's
    // dependencies map (e.g. `@isaacs/cliui@8.0.2` declares
    // `"string-width-cjs": "npm:string-width@^4.2.0"` and
    // pnpm resolves it as `string-width-cjs: string-width@4.2.3`).
    // The reader must rewrite the dep value to the resolved
    // version and synthesize the alias-keyed package entry, or
    // the linker creates a broken symlink to a non-existent
    // `string-width-cjs@string-width@4.2.3` virtual store dir
    // and the resolver's lockfile-reuse path enqueues a
    // transitive task with a malformed `string-width@4.2.3`
    // range that no string-width-cjs version can satisfy.
    // Repro: https://github.com/stevelandeydescript/aube-bug-repros/tree/main/npm-alias-resolution-failure
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      jackspeak:
        specifier: 4.1.1
        version: 4.1.1

packages:
  '@isaacs/cliui@8.0.2':
    resolution: {integrity: sha512-fake}
  jackspeak@4.1.1:
    resolution: {integrity: sha512-fake}
  string-width@4.2.3:
    resolution: {integrity: sha512-fake}
  string-width@5.1.2:
    resolution: {integrity: sha512-fake}

snapshots:
  '@isaacs/cliui@8.0.2':
    dependencies:
      string-width: 5.1.2
      string-width-cjs: string-width@4.2.3
  jackspeak@4.1.1:
    dependencies:
      '@isaacs/cliui': 8.0.2
  string-width@4.2.3: {}
  string-width@5.1.2: {}
"#,
    )
    .unwrap();

    let graph = parse(&path).unwrap();

    let cliui = graph
        .packages
        .get("@isaacs/cliui@8.0.2")
        .expect("cliui entry");
    assert_eq!(
        cliui.dependencies.get("string-width-cjs").unwrap(),
        "4.2.3",
        "transitive alias dep value rewritten from `string-width@4.2.3` to bare `4.2.3`"
    );
    assert_eq!(cliui.dependencies.get("string-width").unwrap(), "5.1.2");

    let alias = graph
        .packages
        .get("string-width-cjs@4.2.3")
        .expect("synthesized alias-keyed package for transitive");
    assert_eq!(alias.name, "string-width-cjs");
    assert_eq!(alias.alias_of.as_deref(), Some("string-width"));
    assert_eq!(alias.dep_path, "string-width-cjs@4.2.3");

    let real = graph
        .packages
        .get("string-width@4.2.3")
        .expect("real entry stays put");
    assert_eq!(real.name, "string-width");
    assert!(real.alias_of.is_none());
}

#[test]
fn parse_handles_npm_alias_for_transitive_deps_with_peer_suffix() {
    // Aliased transitive whose alias target carries a peer
    // suffix: `<alias>: <real>@<resolved>(peer@ver)`. The
    // peer-context tail must follow through to the synthetic
    // alias dep_path so the linker keys the same context.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      parent-pkg:
        specifier: 1.0.0
        version: 1.0.0

packages:
  parent-pkg@1.0.0:
    resolution: {integrity: sha512-fake}
  real-pkg@2.0.0:
    resolution: {integrity: sha512-fake}
  peer-pkg@3.0.0:
    resolution: {integrity: sha512-fake}

snapshots:
  parent-pkg@1.0.0:
    dependencies:
      alias-pkg: real-pkg@2.0.0(peer-pkg@3.0.0)
  real-pkg@2.0.0(peer-pkg@3.0.0):
    dependencies:
      peer-pkg: 3.0.0
  peer-pkg@3.0.0: {}
"#,
    )
    .unwrap();

    let graph = parse(&path).unwrap();
    let parent = graph.packages.get("parent-pkg@1.0.0").expect("parent");
    assert_eq!(
        parent.dependencies.get("alias-pkg").unwrap(),
        "2.0.0(peer-pkg@3.0.0)",
        "peer-context suffix preserved on the rewritten alias dep value"
    );
    let alias = graph
        .packages
        .get("alias-pkg@2.0.0(peer-pkg@3.0.0)")
        .expect("synthesized alias entry with peer suffix");
    assert_eq!(alias.name, "alias-pkg");
    assert_eq!(alias.alias_of.as_deref(), Some("real-pkg"));
}

#[test]
fn parse_synthesizes_npm_alias_for_transitive_deps_of_local_packages() {
    // The local-packages absorption loop runs before the main
    // snapshot loop and pulls a `file:` workspace package's
    // transitive deps directly out of `raw.snapshots`. Those
    // values must go through the same alias rewrite as the main
    // path, or a workspace package depending on
    // `"string-width-cjs": "npm:string-width@^4.2.0"` would still
    // produce the broken `string-width-cjs@string-width@4.2.3`
    // virtual store path on install.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      local-pkg:
        specifier: file:./local-pkg
        version: file:./local-pkg

packages:
  local-pkg@file:./local-pkg:
    resolution: {directory: ./local-pkg, type: directory}
  string-width@4.2.3:
    resolution: {integrity: sha512-fake}

snapshots:
  local-pkg@file:./local-pkg:
    dependencies:
      string-width-cjs: string-width@4.2.3
  string-width@4.2.3: {}
"#,
    )
    .unwrap();

    let graph = parse(&path).unwrap();
    let local = graph
        .packages
        .values()
        .find(|p| p.name == "local-pkg")
        .expect("local-pkg entry");
    assert_eq!(
        local.dependencies.get("string-width-cjs").unwrap(),
        "4.2.3",
        "transitive alias on a local package gets rewritten too"
    );
    let alias = graph
        .packages
        .get("string-width-cjs@4.2.3")
        .expect("synthesized alias entry from local package's transitive");
    assert_eq!(alias.name, "string-width-cjs");
    assert_eq!(alias.alias_of.as_deref(), Some("string-width"));
}

#[test]
fn git_resolution_integrity_roundtrips() {
    let sha = "abcdef0123456789abcdef0123456789abcdef01";
    let integrity = "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";
    let local = LocalSource::Git(GitSource {
        url: "https://github.com/owner/repo.git".to_string(),
        committish: Some(sha.to_string()),
        resolved: sha.to_string(),
        integrity: Some(integrity.to_string()),
        subpath: None,
    });
    let dep_path = local.dep_path("gitdep");
    let mut packages = BTreeMap::new();
    packages.insert(
        dep_path.clone(),
        LockedPackage {
            name: "gitdep".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some(integrity.to_string()),
            dep_path: dep_path.clone(),
            local_source: Some(local),
            ..Default::default()
        },
    );
    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "gitdep".to_string(),
            dep_path: dep_path.clone(),
            dep_type: DepType::Production,
            specifier: Some("github:owner/repo".to_string()),
        }],
    );
    let graph = LockfileGraph {
        importers,
        packages,
        ..LockfileGraph::default()
    };
    let manifest = PackageJson {
        name: Some("root".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: [("gitdep".to_string(), "github:owner/repo".to_string())]
            .into_iter()
            .collect(),
        ..PackageJson::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(yaml.contains("type: git"));
    assert!(yaml.contains(&format!("integrity: {integrity}")));

    let reparsed = parse(&lockfile_path).unwrap();
    let pkg = reparsed.packages.get(&dep_path).unwrap();
    assert_eq!(pkg.integrity.as_deref(), Some(integrity));
    let Some(LocalSource::Git(git)) = pkg.local_source.as_ref() else {
        panic!("expected git local source");
    };
    assert_eq!(git.integrity.as_deref(), Some(integrity));
}

#[test]
fn writer_emits_git_hosted_for_hosted_git_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    let dep_path =
        "demo@git+ssh://git@github.com/acme/demo.git#abcdef0123456789abcdef0123456789abcdef01";
    let graph = LockfileGraph {
        packages: BTreeMap::from([(
            dep_path.to_string(),
            LockedPackage {
                name: "demo".to_string(),
                version: "1.0.0".to_string(),
                integrity: Some("sha512-hosted".to_string()),
                dep_path: dep_path.to_string(),
                local_source: Some(LocalSource::Git(GitSource {
                    url: "git+ssh://git@github.com/acme/demo.git".to_string(),
                    committish: Some("main".to_string()),
                    resolved: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
                    integrity: None,
                    subpath: None,
                })),
                ..Default::default()
            },
        )]),
        importers: BTreeMap::from([(
            ".".to_string(),
            vec![DirectDep {
                name: "demo".to_string(),
                dep_path: dep_path.to_string(),
                dep_type: DepType::Production,
                specifier: Some("github:acme/demo".to_string()),
            }],
        )]),
        ..Default::default()
    };
    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("demo".to_string(), "github:acme/demo".to_string());

    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(yaml.contains("gitHosted: true"), "{yaml}");
    assert!(yaml.contains("integrity: sha512-hosted"), "{yaml}");
}

#[test]
fn parser_preserves_direct_git_resolution_integrity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:
  .:
    dependencies:
      demo:
        specifier: github:acme/demo
        version: git+ssh://git@github.com/acme/demo.git#abcdef0123456789abcdef0123456789abcdef01

packages:
  other@git+ssh://git@github.com/acme/other.git#abcdef0123456789abcdef0123456789abcdef01:
    resolution: {commit: abcdef0, repo: git+ssh://git@github.com/acme/other.git, type: git, integrity: sha512-other, gitHosted: true}
    version: 1.0.0
  demo@git+ssh://git@github.com/acme/demo.git#abcdef0123456789abcdef0123456789abcdef01:
    resolution: {commit: abcdef0, repo: git+ssh://git@github.com/acme/demo.git, type: git, integrity: sha512-hosted, gitHosted: true}
    version: 1.0.0

snapshots:
  other@git+ssh://git@github.com/acme/other.git#abcdef0123456789abcdef0123456789abcdef01: {}
  demo@git+ssh://git@github.com/acme/demo.git#abcdef0123456789abcdef0123456789abcdef01: {}
"#,
    )
    .unwrap();

    let graph = parse(&path).unwrap();
    let pkg = graph
        .packages
        .values()
        .find(|pkg| pkg.name == "demo")
        .expect("demo package");
    assert_eq!(pkg.integrity.as_deref(), Some("sha512-hosted"));
    let Some(LocalSource::Git(git)) = &pkg.local_source else {
        panic!("expected git local source, got {:?}", pkg.local_source);
    };
    assert!(git.url.contains("/acme/demo.git"), "{git:?}");
    assert_eq!(git.resolved, "abcdef0123456789abcdef0123456789abcdef01");

    write(&path, &graph, &PackageJson::default()).unwrap();
    let yaml = std::fs::read_to_string(&path).unwrap();
    assert!(
        yaml.contains("repo: git+ssh://git@github.com/acme/demo.git"),
        "{yaml}"
    );
    assert!(
        !yaml.contains("repo: ssh://git@github.com/acme/demo.git"),
        "{yaml}"
    );
    assert!(yaml.contains("integrity: sha512-hosted"), "{yaml}");
}

#[test]
fn parser_rejects_remote_tarball_resolution_without_integrity() {
    for scheme in ["http", "https"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(
            &path,
            format!(
                r#"
lockfileVersion: '9.0'
importers:
  .:
    dependencies:
      demo:
        specifier: 1.0.0
        version: 1.0.0
packages:
  demo@1.0.0:
    resolution: {{tarball: {scheme}://registry.npmjs.org/demo/-/demo-1.0.0.tgz}}
snapshots:
  demo@1.0.0: {{}}
"#,
            ),
        )
        .unwrap();

        let err = parse(&path).unwrap_err().to_string();
        assert!(
            err.contains("remote tarball resolution without integrity"),
            "{scheme}: {err}"
        );
    }
}

#[test]
fn remote_tarball_integrity_survives_lockfile_reuse_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"
lockfileVersion: '9.0'
importers:
  .:
    dependencies:
      demo:
        specifier: https://registry.example.test/demo/-/demo-1.0.0.tgz
        version: https://registry.example.test/demo/-/demo-1.0.0.tgz
packages:
  demo@https://registry.example.test/demo/-/demo-1.0.0.tgz(react@18.2.0):
    resolution: {integrity: sha512-demo, tarball: https://registry.example.test/demo/-/demo-1.0.0.tgz}
    version: 1.0.0
snapshots:
  demo@https://registry.example.test/demo/-/demo-1.0.0.tgz(react@18.2.0): {}
"#,
    )
    .unwrap();

    let graph = parse(&path).unwrap();
    let pkg = graph
        .packages
        .values()
        .find(|pkg| pkg.name == "demo")
        .expect("demo package");
    assert_eq!(pkg.integrity.as_deref(), Some("sha512-demo"));
    let Some(LocalSource::RemoteTarball(source)) = &pkg.local_source else {
        panic!("expected remote tarball source, got {:?}", pkg.local_source);
    };
    assert_eq!(source.integrity, "sha512-demo");

    write(&path, &graph, &PackageJson::default()).unwrap();
    let yaml = std::fs::read_to_string(&path).unwrap();
    assert!(yaml.contains("integrity: sha512-demo"), "{yaml}");
    assert!(
        yaml.contains("tarball: https://registry.example.test/demo/-/demo-1.0.0.tgz"),
        "{yaml}"
    );
}

#[test]
fn parser_rejects_remote_tarball_with_hosted_git_url_in_query() {
    for tarball in [
        "https://evil.example.com/demo.tgz?ref=://codeload.github.com/acme/demo/tar.gz/abcdef",
        "https://gitlab.com/acme/demo/demo.tgz?redirect=/-/archive/main",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(
            &path,
            format!(
                r#"
lockfileVersion: '9.0'
importers:
  .:
    dependencies:
      demo:
        specifier: 1.0.0
        version: 1.0.0
packages:
  demo@1.0.0:
    resolution: {{tarball: {tarball}}}
snapshots:
  demo@1.0.0: {{}}
"#,
            ),
        )
        .unwrap();

        let err = parse(&path).unwrap_err().to_string();
        assert!(
            err.contains("remote tarball resolution without integrity"),
            "{tarball}: {err}"
        );
    }
}

#[test]
fn parser_allows_git_hosted_tarball_resolution_without_integrity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"
lockfileVersion: '9.0'
importers:
  .:
    dependencies:
      demo:
        specifier: 1.0.0
        version: 1.0.0
packages:
  demo@1.0.0:
    resolution: {tarball: https://codeload.github.com/acme/demo/tar.gz/abcdef, gitHosted: true}
snapshots:
  demo@1.0.0: {}
"#,
    )
    .unwrap();

    parse(&path).unwrap();
}

#[test]
fn parser_expands_transitive_git_resolution_commit_from_dep_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    let full_commit = "abcdef0123456789abcdef0123456789abcdef01";
    std::fs::write(
        &path,
        format!(
            r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:
  .:
    dependencies:
      root:
        specifier: 1.0.0
        version: 1.0.0

packages:
  root@1.0.0:
    resolution: {{integrity: sha512-root}}
  transitive@git+ssh://git@github.com/acme/transitive.git#{full_commit}:
    resolution: {{commit: abcdef0, repo: git+ssh://git@github.com/acme/transitive.git, type: git, integrity: sha512-git, gitHosted: true}}
    version: 1.0.0

snapshots:
  root@1.0.0: {{}}
  transitive@git+ssh://git@github.com/acme/transitive.git#{full_commit}: {{}}
"#
        ),
    )
    .unwrap();

    let graph = parse(&path).unwrap();
    let pkg = graph
        .packages
        .values()
        .find(|pkg| pkg.name == "transitive")
        .expect("transitive package");
    let Some(LocalSource::Git(git)) = &pkg.local_source else {
        panic!("expected git local source, got {:?}", pkg.local_source);
    };
    assert_eq!(git.resolved, full_commit);
}

#[test]
fn writer_preserves_non_derivable_registry_tarball_url_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    let graph = LockfileGraph {
        packages: BTreeMap::from([(
            "@scope/pkg@1.0.0".to_string(),
            LockedPackage {
                name: "@scope/pkg".to_string(),
                version: "1.0.0".to_string(),
                integrity: Some("sha512-private".to_string()),
                dep_path: "@scope/pkg@1.0.0".to_string(),
                tarball_url: Some(
                    "https://npm.pkg.github.com/download/@scope/pkg/1.0.0/deadbeef".to_string(),
                ),
                ..Default::default()
            },
        )]),
        importers: BTreeMap::from([(
            ".".to_string(),
            vec![DirectDep {
                name: "@scope/pkg".to_string(),
                dep_path: "@scope/pkg@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("1.0.0".to_string()),
            }],
        )]),
        ..Default::default()
    };
    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("@scope/pkg".to_string(), "1.0.0".to_string());

    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(
        yaml.contains("tarball: https://npm.pkg.github.com/download/@scope/pkg/1.0.0/deadbeef"),
        "{yaml}"
    );
    assert!(yaml.contains("gitHosted: true"), "{yaml}");
    assert!(!yaml.contains("lockfileIncludeTarballUrl: true"), "{yaml}");
}

#[test]
fn parser_round_trips_registry_git_hosted_tarball_flag() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:
  .:
    dependencies:
      demo:
        specifier: 1.0.0
        version: 1.0.0

packages:
  demo@1.0.0:
    resolution: {integrity: sha512-demo, tarball: https://npm.pkg.github.com/download/demo/1.0.0/deadbeef, gitHosted: true}

snapshots:
  demo@1.0.0: {}
"#,
    )
    .unwrap();

    let graph = parse(&path).unwrap();
    let pkg = graph.packages.get("demo@1.0.0").expect("demo package");
    assert!(pkg.registry_git_hosted);
    assert!(pkg.local_source.is_none());
    assert_eq!(
        pkg.tarball_url.as_deref(),
        Some("https://npm.pkg.github.com/download/demo/1.0.0/deadbeef")
    );

    write(&path, &graph, &PackageJson::default()).unwrap();
    let yaml = std::fs::read_to_string(&path).unwrap();
    assert!(yaml.contains("gitHosted: true"), "{yaml}");
    assert!(
        yaml.contains("tarball: https://npm.pkg.github.com/download/demo/1.0.0/deadbeef"),
        "{yaml}"
    );
}

#[test]
fn writer_preserves_non_derivable_registry_tarball_url_without_integrity() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    let graph = LockfileGraph {
        packages: BTreeMap::from([(
            "@scope/pkg@1.0.0".to_string(),
            LockedPackage {
                name: "@scope/pkg".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "@scope/pkg@1.0.0".to_string(),
                tarball_url: Some(
                    "https://npm.pkg.github.com/download/@scope/pkg/1.0.0/deadbeef".to_string(),
                ),
                ..Default::default()
            },
        )]),
        importers: BTreeMap::from([(
            ".".to_string(),
            vec![DirectDep {
                name: "@scope/pkg".to_string(),
                dep_path: "@scope/pkg@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("1.0.0".to_string()),
            }],
        )]),
        ..Default::default()
    };
    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("@scope/pkg".to_string(), "1.0.0".to_string());

    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(
        yaml.contains("tarball: https://npm.pkg.github.com/download/@scope/pkg/1.0.0/deadbeef"),
        "{yaml}"
    );
    assert!(!yaml.contains("integrity:"), "{yaml}");
}

#[test]
fn writer_omits_derivable_registry_tarball_url_with_query() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    let graph = LockfileGraph {
        packages: BTreeMap::from([(
            "@scope/pkg@1.0.0".to_string(),
            LockedPackage {
                name: "@scope/pkg".to_string(),
                version: "1.0.0".to_string(),
                integrity: Some("sha512-private".to_string()),
                dep_path: "@scope/pkg@1.0.0".to_string(),
                tarball_url: Some(
                    "https://registry.example.test/@scope/pkg/-/pkg-1.0.0.tgz?signature=abc#sha"
                        .to_string(),
                ),
                ..Default::default()
            },
        )]),
        importers: BTreeMap::from([(
            ".".to_string(),
            vec![DirectDep {
                name: "@scope/pkg".to_string(),
                dep_path: "@scope/pkg@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("1.0.0".to_string()),
            }],
        )]),
        ..Default::default()
    };
    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("@scope/pkg".to_string(), "1.0.0".to_string());

    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(!yaml.contains("tarball:"), "{yaml}");
    assert!(yaml.contains("integrity: sha512-private"), "{yaml}");
}

#[test]
fn parser_round_trips_git_hosted_remote_tarball_flag() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:
  .:
    dependencies:
      forge-dep:
        specifier: https://forge.example.test/acme/dep/archive/abcdef.tgz
        version: https://forge.example.test/acme/dep/archive/abcdef.tgz

packages:
  forge-dep@https://forge.example.test/acme/dep/archive/abcdef.tgz:
    resolution: {integrity: sha512-forge, tarball: https://forge.example.test/acme/dep/archive/abcdef.tgz, gitHosted: true}
    version: 1.0.0

snapshots:
  forge-dep@https://forge.example.test/acme/dep/archive/abcdef.tgz: {}
"#,
    )
    .unwrap();

    let graph = parse(&path).unwrap();
    let pkg = graph
        .packages
        .values()
        .find(|pkg| pkg.name == "forge-dep")
        .expect("forge-dep package");
    let Some(LocalSource::RemoteTarball(source)) = &pkg.local_source else {
        panic!("expected remote tarball source, got {:?}", pkg.local_source);
    };
    assert!(source.git_hosted);

    write(&path, &graph, &PackageJson::default()).unwrap();
    let yaml = std::fs::read_to_string(&path).unwrap();
    assert!(yaml.contains("gitHosted: true"), "{yaml}");
}

// ── runtime pins (pnpm 10.14+ devEngines.runtime recording) ─────────

/// A pnpm-11-authored lockfile with a `node@runtime:` pin. The
/// importer's synthetic `node` dep must land in `graph.runtimes` (not
/// `importers`/`packages`) so the install pipeline never tries to
/// fetch `node` from the npm registry — this is both the pin feature
/// and a compat fix for reading pnpm 10.14+ lockfiles.
#[test]
fn runtime_pin_parses_from_pnpm_lockfile() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    let src = r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:

  .:
    devDependencies:
      node:
        specifier: runtime:^24.4.0
        version: runtime:24.4.1

packages:

  node@runtime:24.4.1:
    hasBin: true
    version: 24.4.1
    resolution:
      type: variations
      variants:
        - resolution:
            archive: tarball
            bin: bin/node
            integrity: sha256-aGVsbG8gd29ybGQgdGhpcyBpcyBub3QgcmVhbA==
            type: binary
            url: https://nodejs.org/download/release/v24.4.1/node-v24.4.1-darwin-arm64.tar.gz
          targets:
            - cpu: arm64
              os: darwin
        - resolution:
            archive: zip
            bin:
              node: node.exe
            integrity: sha256-d2luZG93cyBidWlsZCBjaGVja3N1bSBmYWtl
            prefix: node-v24.4.1-win-x64
            type: binary
            url: https://nodejs.org/download/release/v24.4.1/node-v24.4.1-win-x64.zip
          targets:
            - cpu: x64
              os: win32
        - resolution:
            archive: tarball
            bin: bin/node
            integrity: sha256-bXVzbCBidWlsZCBjaGVja3N1bSBmYWtlIQ==
            type: binary
            url: https://unofficial-builds.nodejs.org/download/release/v24.4.1/node-v24.4.1-linux-x64-musl.tar.gz
          targets:
            - cpu: x64
              libc: musl
              os: linux

snapshots:

  node@runtime:24.4.1: {}
"#;
    std::fs::write(&lockfile_path, src).unwrap();
    let graph = parse(&lockfile_path).unwrap();

    // The synthetic dep must not leak into the package graph.
    assert!(graph.importers.get(".").unwrap().is_empty());
    assert!(graph.packages.is_empty());

    let pin = graph.runtimes.get("node").expect("node pin parsed");
    assert_eq!(pin.specifier, "^24.4.0");
    assert_eq!(pin.version, "24.4.1");
    assert!(pin.dev);
    assert!(pin.has_bin);
    assert_eq!(pin.variants.len(), 3);

    let mac = pin.variant_for("darwin", "arm64", None).unwrap();
    assert_eq!(mac.archive, "tarball");
    assert!(mac.bin_is_bare_string);
    assert_eq!(mac.bin.get("node").map(String::as_str), Some("bin/node"));
    assert_eq!(
        mac.url,
        "https://nodejs.org/download/release/v24.4.1/node-v24.4.1-darwin-arm64.tar.gz"
    );

    let win = pin.variant_for("win32", "x64", None).unwrap();
    assert_eq!(win.archive, "zip");
    assert!(!win.bin_is_bare_string);
    assert_eq!(win.bin.get("node").map(String::as_str), Some("node.exe"));
    assert_eq!(win.prefix.as_deref(), Some("node-v24.4.1-win-x64"));

    let musl = pin.variant_for("linux", "x64", Some("musl")).unwrap();
    assert!(musl.url.contains("unofficial-builds"));
    assert!(pin.variant_for("linux", "x64", None).is_none());
}

/// Write → parse round-trip of a runtime pin must preserve the full
/// pnpm shape: importer synthetic dep, `variations` packages entry,
/// and the empty snapshot.
#[test]
fn runtime_pin_round_trips_through_write() {
    use crate::{RuntimePin, RuntimeTarget, RuntimeVariant};

    let dir = tempfile::tempdir().unwrap();
    let mut graph = LockfileGraph::default();
    graph.importers.insert(".".to_string(), Vec::new());
    graph.runtimes.insert(
        "node".to_string(),
        RuntimePin {
            specifier: "^24.4.0".to_string(),
            version: "24.4.1".to_string(),
            dev: true,
            has_bin: true,
            variants: vec![
                RuntimeVariant {
                    targets: vec![RuntimeTarget {
                        os: "darwin".to_string(),
                        cpu: "arm64".to_string(),
                        libc: None,
                    }],
                    archive: "tarball".to_string(),
                    url: "https://nodejs.org/download/release/v24.4.1/node-v24.4.1-darwin-arm64.tar.gz".to_string(),
                    integrity: "sha256-aGVsbG8gd29ybGQgdGhpcyBpcyBub3QgcmVhbA==".to_string(),
                    bin: [("node".to_string(), "bin/node".to_string())].into_iter().collect(),
                    bin_is_bare_string: true,
                    prefix: None,
                },
                RuntimeVariant {
                    targets: vec![RuntimeTarget {
                        os: "win32".to_string(),
                        cpu: "x64".to_string(),
                        libc: None,
                    }],
                    archive: "zip".to_string(),
                    url: "https://nodejs.org/download/release/v24.4.1/node-v24.4.1-win-x64.zip".to_string(),
                    integrity: "sha256-d2luZG93cyBidWlsZCBjaGVja3N1bSBmYWtl".to_string(),
                    bin: [("node".to_string(), "node.exe".to_string())].into_iter().collect(),
                    bin_is_bare_string: false,
                    prefix: Some("node-v24.4.1-win-x64".to_string()),
                },
            ],
        },
    );

    let manifest = PackageJson::default();
    let out_path = dir.path().join("aube-lock.yaml");
    write(&out_path, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&out_path).unwrap();

    assert!(
        written.contains("specifier: runtime:^24.4.0"),
        "importer specifier missing: {written}"
    );
    assert!(
        written.contains("version: runtime:24.4.1"),
        "importer version missing: {written}"
    );
    assert!(
        written.contains("node@runtime:24.4.1:"),
        "packages/snapshots key missing: {written}"
    );
    assert!(
        written.contains("type: variations"),
        "variations resolution missing (or flow-collapsed): {written}"
    );
    // The variations resolution must stay in block form — the
    // flow-collapse pass must not mangle it.
    assert!(
        !written.contains("resolution: {type: variations"),
        "variations resolution was flow-collapsed: {written}"
    );
    assert!(
        written.contains("bin: bin/node"),
        "bare-string bin form not preserved: {written}"
    );

    let reparsed = parse(&out_path).unwrap();
    assert_eq!(
        reparsed.runtimes, graph.runtimes,
        "runtime pin did not round-trip"
    );
    assert!(reparsed.packages.is_empty());
}

/// Branch-lockfile merges of the same pinned version must union the
/// per-platform variants (a pin written on darwin + one written on
/// linux merge into one pin carrying both artifacts) and surface
/// specifier disagreements instead of silently keeping one side.
#[test]
fn runtime_pin_merge_unions_variants() {
    use crate::{RuntimePin, RuntimeTarget, RuntimeVariant};

    fn variant(os: &str) -> RuntimeVariant {
        RuntimeVariant {
            targets: vec![RuntimeTarget {
                os: os.to_string(),
                cpu: "x64".to_string(),
                libc: None,
            }],
            archive: "tarball".to_string(),
            url: format!("https://nodejs.org/download/release/v1.0.0/node-v1.0.0-{os}-x64.tar.gz"),
            integrity: "sha256-AAAA".to_string(),
            bin: [("node".to_string(), "bin/node".to_string())]
                .into_iter()
                .collect(),
            bin_is_bare_string: true,
            prefix: None,
        }
    }
    fn graph_with_pin(os: &str) -> LockfileGraph {
        let mut g = LockfileGraph::default();
        g.importers.insert(".".to_string(), Vec::new());
        g.runtimes.insert(
            "node".to_string(),
            RuntimePin {
                specifier: "^1.0.0".to_string(),
                version: "1.0.0".to_string(),
                dev: true,
                has_bin: true,
                variants: vec![variant(os)],
            },
        );
        g
    }

    let dir = tempfile::tempdir().unwrap();
    let manifest = PackageJson::default();
    write(
        &dir.path().join("aube-lock.yaml"),
        &graph_with_pin("darwin"),
        &manifest,
    )
    .unwrap();
    write(
        &dir.path().join("aube-lock.feature.yaml"),
        &graph_with_pin("linux"),
        &manifest,
    )
    .unwrap();

    let report = crate::merge_branch_lockfiles(dir.path(), &manifest).unwrap();
    assert_eq!(report.merged_files.len(), 1);

    let merged = parse(&dir.path().join("aube-lock.yaml")).unwrap();
    let pin = merged.runtimes.get("node").unwrap();
    let mut oses: Vec<&str> = pin
        .variants
        .iter()
        .flat_map(|v| v.targets.iter().map(|t| t.os.as_str()))
        .collect();
    oses.sort();
    assert_eq!(
        oses,
        vec!["darwin", "linux"],
        "variants must union across branches"
    );
}

/// devEngines drift: a recorded pin whose range no longer matches the
/// manifest (or whose devEngines entry was removed) must read stale;
/// a matching pin stays fresh; and a manifest with devEngines but no
/// recorded pin is NOT drift (foreign formats can't record pins).
#[test]
fn runtime_pin_drift_detection() {
    use crate::{DriftStatus, RuntimePin};

    let mut graph = LockfileGraph::default();
    graph.importers.insert(".".to_string(), Vec::new());
    graph.runtimes.insert(
        "node".to_string(),
        RuntimePin {
            specifier: "^24.4.0".to_string(),
            version: "24.4.1".to_string(),
            dev: true,
            has_bin: true,
            variants: Vec::new(),
        },
    );

    let manifest_with = |range: &str| -> PackageJson {
        let json = format!(
            r#"{{"name": "t", "devEngines": {{"runtime": {{"name": "node", "version": "{range}", "onFail": "download"}}}}}}"#
        );
        serde_json::from_str(&json).unwrap()
    };

    let empty: BTreeMap<String, String> = BTreeMap::new();
    let empty_catalogs: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let fresh = graph.check_drift(&manifest_with("^24.4.0"), &empty, &[], &empty_catalogs);
    assert_eq!(fresh, DriftStatus::Fresh, "matching pin must be fresh");

    let changed = graph.check_drift(&manifest_with("^26.0.0"), &empty, &[], &empty_catalogs);
    assert!(
        matches!(&changed, DriftStatus::Stale { reason } if reason.contains("node")),
        "changed range must be stale: {changed:?}"
    );

    let removed = graph.check_drift(&PackageJson::default(), &empty, &[], &empty_catalogs);
    assert!(
        matches!(removed, DriftStatus::Stale { .. }),
        "removed devEngines must be stale"
    );

    // Entry still names node but drops the version: no concrete
    // range to contradict the pin — must stay fresh (a hard frozen
    // failure here would fire on a field that changes nothing).
    let versionless: PackageJson =
        serde_json::from_str(r#"{"name": "t", "devEngines": {"runtime": {"name": "node"}}}"#)
            .unwrap();
    assert_eq!(
        graph.check_drift(&versionless, &empty, &[], &empty_catalogs),
        DriftStatus::Fresh,
        "version-less devEngines entry must not read as a removed pin"
    );

    // No pin recorded + devEngines present → not drift (the install
    // driver records the pin on formats that support it).
    let no_pin = LockfileGraph {
        importers: graph.importers.clone(),
        ..LockfileGraph::default()
    };
    assert_eq!(
        no_pin.check_drift(&manifest_with("^24.4.0"), &empty, &[], &empty_catalogs),
        DriftStatus::Fresh
    );
}

/// Converting an npm workspace to pnpm must NOT fabricate workspace-member
/// `link:` deps on the empty root importer. npm symlinks every member into
/// the root `node_modules/` and the npm reader surfaces those as root deps
/// (so the linker recreates the symlinks), but pnpm writes `.: {}` for an
/// empty root — members are importers, not deps of the root. Emitting the
/// phantom links made the converted lockfile's root specifiers diverge from
/// the root package.json, so pnpm frozen-rejects with
/// ERR_PNPM_OUTDATED_LOCKFILE. Regression guard for the npm→pnpm leg of
/// `tests/conversion/run.sh` (empty-root-importer).
#[test]
fn npm_to_pnpm_conversion_omits_phantom_member_links_on_empty_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Member package.json files on disk — the pnpm writer reads them to
    // map `name@version` back to the member directory.
    std::fs::create_dir_all(root.join("packages/pkg-a")).unwrap();
    std::fs::create_dir_all(root.join("packages/pkg-b")).unwrap();
    std::fs::write(
        root.join("packages/pkg-a/package.json"),
        r#"{ "name": "@empty/pkg-a", "version": "1.0.0", "dependencies": { "ms": "^2.1.3" } }"#,
    )
    .unwrap();
    std::fs::write(
        root.join("packages/pkg-b/package.json"),
        r#"{ "name": "@empty/pkg-b", "version": "1.0.0", "dependencies": { "kleur": "^4.1.5" } }"#,
    )
    .unwrap();

    // The exact package-lock.json real npm writes for this workspace:
    // the members appear as `node_modules/<name>: { link: true }` root
    // symlinks plus their `packages/<dir>` importer entries.
    let npm_lock = r#"{
  "name": "wsroot",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "packages": {
    "": { "name": "wsroot", "version": "1.0.0", "workspaces": ["packages/*"] },
    "node_modules/@empty/pkg-a": { "resolved": "packages/pkg-a", "link": true },
    "node_modules/@empty/pkg-b": { "resolved": "packages/pkg-b", "link": true },
    "node_modules/kleur": {
      "version": "4.1.5",
      "resolved": "https://registry.npmjs.org/kleur/-/kleur-4.1.5.tgz",
      "integrity": "sha512-o+NO+8WrRiQEE4/7nwRJhN1HWpVmJm511pBHUxPLtp0BUISzlBplORYSmTclCnJvQq2tKu/sgl3xVpkc7ZWuQQ=="
    },
    "node_modules/ms": {
      "version": "2.1.3",
      "resolved": "https://registry.npmjs.org/ms/-/ms-2.1.3.tgz",
      "integrity": "sha512-6FlzubTLZG3J2a/NVCAleEhjzq5oxgHyaCU9yYXvcLsvoVaHJq/s5xXI6/XXP6tz7R9xAOtHnSO/tXtF3WRTlA=="
    },
    "packages/pkg-a": { "name": "@empty/pkg-a", "version": "1.0.0", "dependencies": { "ms": "^2.1.3" } },
    "packages/pkg-b": { "name": "@empty/pkg-b", "version": "1.0.0", "dependencies": { "kleur": "^4.1.5" } }
  }
}"#;
    let npm_path = root.join("package-lock.json");
    std::fs::write(&npm_path, npm_lock).unwrap();

    let graph = crate::npm::parse(&npm_path).unwrap();

    // Root manifest declares NO deps (empty root importer).
    let manifest = PackageJson {
        name: Some("wsroot".to_string()),
        version: Some("1.0.0".to_string()),
        workspaces: Some(aube_manifest::Workspaces::Array(vec!["packages/*".to_string()])),
        ..PackageJson::default()
    };

    let out_path = root.join("pnpm-lock.yaml");
    write(&out_path, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&out_path).unwrap();

    // The root importer must be empty — no phantom member `link:` deps.
    assert!(
        !written.contains("link:packages/pkg-a") && !written.contains("link:packages/pkg-b"),
        "root importer must not carry phantom workspace-member link: deps:\n{written}"
    );
    assert!(
        written.contains("  .: {}"),
        "empty root importer must render as `.: {{}}`:\n{written}"
    );
    // The members are still their own importers with their child deps.
    assert!(
        written.contains("packages/pkg-a:") && written.contains("packages/pkg-b:"),
        "member importers must still be emitted:\n{written}"
    );
}

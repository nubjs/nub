use super::berry::{parse_berry_spec, range_has_protocol, split_berry_header};
use super::classic::{parse_npm_alias_real_name, parse_spec_name};
use super::*;
use crate::{DepType, LocalSource, LockedPackage};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn make_manifest(deps: &[(&str, &str)], dev: &[(&str, &str)]) -> aube_manifest::PackageJson {
    aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: deps
            .iter()
            .map(|(n, r)| (n.to_string(), r.to_string()))
            .collect(),
        dev_dependencies: dev
            .iter()
            .map(|(n, r)| (n.to_string(), r.to_string()))
            .collect(),
        peer_dependencies: Default::default(),
        optional_dependencies: Default::default(),
        update_config: None,
        scripts: Default::default(),
        engines: Default::default(),
        dev_engines: None,
        workspaces: None,
        bundled_dependencies: None,
        extra: Default::default(),
    }
}

#[test]
fn test_parse_spec_name() {
    assert_eq!(parse_spec_name("foo@^1.0.0"), Some("foo".to_string()));
    assert_eq!(parse_spec_name("foo@1.2.3"), Some("foo".to_string()));
    assert_eq!(
        parse_spec_name("@scope/pkg@^1.0.0"),
        Some("@scope/pkg".to_string())
    );
    assert_eq!(parse_spec_name("foo"), None);
}

#[test]
fn test_parse_simple() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

foo@^1.0.0:
  version "1.2.3"
  resolved "https://example.com/foo-1.2.3.tgz"
  integrity sha512-aaa
  dependencies:
    bar "^2.0.0"

bar@^2.0.0:
  version "2.5.0"
  resolved "https://example.com/bar-2.5.0.tgz"
  integrity sha512-bbb
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert_eq!(graph.packages.len(), 2);
    assert!(graph.packages.contains_key("foo@1.2.3"));
    assert!(graph.packages.contains_key("bar@2.5.0"));

    let foo = &graph.packages["foo@1.2.3"];
    assert_eq!(foo.integrity.as_deref(), Some("sha512-aaa"));
    assert_eq!(
        foo.dependencies.get("bar").map(String::as_str),
        Some("2.5.0")
    );

    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "foo");
    assert_eq!(root[0].dep_path, "foo@1.2.3");
}

#[test]
fn test_parse_scoped_and_multi_spec() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

"@scope/pkg@^1.0.0", "@scope/pkg@^1.1.0":
  version "1.1.0"
  integrity sha512-zzz
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("@scope/pkg", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.contains_key("@scope/pkg@1.1.0"));
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root[0].name, "@scope/pkg");
    assert_eq!(root[0].dep_path, "@scope/pkg@1.1.0");
}

/// Yarn classic supports the `npm:` protocol to rename a dep on
/// import — `react-loadable: "npm:@docusaurus/react-loadable@5.5.2"`
/// installs `@docusaurus/react-loadable` under
/// `node_modules/react-loadable/`. The lockfile records the alias
/// in the spec key and the real name only behind the `npm:` value.
/// Without surfacing the real name into `LockedPackage.alias_of`,
/// the install path would fetch the alias-qualified URL and 404
/// (https://github.com/jdx/aube/discussions/681).
#[test]
fn test_parse_npm_protocol_alias_transitive() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

"@docusaurus/core@2.1.0":
  version "2.1.0"
  integrity sha512-aaa
  dependencies:
    react-loadable "npm:@docusaurus/react-loadable@5.5.2"

"react-loadable@npm:@docusaurus/react-loadable@5.5.2":
  version "5.5.2"
  integrity sha512-bbb
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("@docusaurus/core", "2.1.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    let aliased = graph
        .packages
        .get("react-loadable@5.5.2")
        .expect("aliased entry should be keyed by the alias dep_path");
    assert_eq!(aliased.name, "react-loadable");
    assert_eq!(aliased.version, "5.5.2");
    assert_eq!(
        aliased.alias_of.as_deref(),
        Some("@docusaurus/react-loadable")
    );
    assert_eq!(aliased.registry_name(), "@docusaurus/react-loadable");

    // The parent must still resolve the transitive ref to the
    // alias dep_path — symlinks under node_modules/.aube/<parent>/
    // key on the alias, not the real name.
    let core = &graph.packages["@docusaurus/core@2.1.0"];
    assert_eq!(
        core.dependencies.get("react-loadable").map(String::as_str),
        Some("5.5.2")
    );
}

#[test]
fn test_parse_classic_dependency_values_are_dep_path_tails() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

"@rollup/plugin-replace@2.4.1":
  version "2.4.1"
  integrity sha512-aaa
  dependencies:
    "@rollup/pluginutils" "^3.1.0"
    magic-string "^0.25.9"

"@rollup/pluginutils@^3.1.0":
  version "3.1.0"
  integrity sha512-bbb

magic-string@^0.25.9:
  version "0.25.9"
  integrity sha512-ccc
  dependencies:
    sourcemap-codec "^1.4.8"

sourcemap-codec@^1.4.8:
  version "1.4.8"
  integrity sha512-ddd
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("@rollup/plugin-replace", "2.4.1")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    let replace = &graph.packages["@rollup/plugin-replace@2.4.1"];
    assert_eq!(
        replace
            .dependencies
            .get("@rollup/pluginutils")
            .map(String::as_str),
        Some("3.1.0")
    );
    assert_eq!(
        replace.dependencies.get("magic-string").map(String::as_str),
        Some("0.25.9")
    );

    let magic_string = &graph.packages["magic-string@0.25.9"];
    assert_eq!(
        magic_string
            .dependencies
            .get("sourcemap-codec")
            .map(String::as_str),
        Some("1.4.8")
    );
}

/// A yarn-classic `link:` dep is a local on-disk package, not a
/// registry one: yarn records the block keyed by the spec
/// `name@link:<path>` with `version "0.0.0"` and no `resolved` URL.
/// The parser must recognize the protocol and attach a
/// `LocalSource::Link` so the linker symlinks the directory; without
/// it the dep falls through as a registry package, and the installer
/// builds a `<name>/-/<name>-0.0.0.tgz` registry URL that 404s and
/// aborts the whole install.
///
/// Shape taken from facebook/react's committed yarn.lock
/// (`eslint-plugin-react-internal@link:./scripts/eslint-rules`), found
/// by differential corpus testing against react.
#[test]
fn test_parse_classic_link_protocol_is_local_not_registry() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

"eslint-plugin-react-internal@link:./scripts/eslint-rules":
  version "0.0.0"
  uid ""
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(
        &[],
        &[(
            "eslint-plugin-react-internal",
            "link:./scripts/eslint-rules",
        )],
    );
    let graph = parse(tmp.path(), &manifest).unwrap();

    let key = LocalSource::Link(PathBuf::from("./scripts/eslint-rules"))
        .dep_path("eslint-plugin-react-internal");
    let pkg = graph
        .packages
        .get(&key)
        .expect("link: dep must be keyed by its LocalSource::Link dep_path");
    assert!(
        matches!(&pkg.local_source, Some(LocalSource::Link(p)) if p == &PathBuf::from("./scripts/eslint-rules")),
        "link: dep must carry LocalSource::Link so the linker symlinks instead of fetching a 0.0.0 tarball"
    );

    let dep = graph.importers["."]
        .iter()
        .find(|d| d.name == "eslint-plugin-react-internal")
        .expect("link: dep must resolve as a direct dep of the importer");
    assert_eq!(dep.dep_path, key);
}

/// Round-trip safety: our writer emits the canonical
/// `"name@version"` spec first and the npm-alias spec alongside it.
/// On reparse the `[0]` spec carries no `npm:`, so the alias must
/// be detected by scanning every spec in the header — not just the
/// first one.
#[test]
fn test_parse_npm_protocol_alias_canonical_spec_first() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

"react-loadable@5.5.2", "react-loadable@npm:@docusaurus/react-loadable@5.5.2":
  version "5.5.2"
  integrity sha512-bbb
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    let aliased = &graph.packages["react-loadable@5.5.2"];
    assert_eq!(
        aliased.alias_of.as_deref(),
        Some("@docusaurus/react-loadable")
    );
}

#[test]
fn test_parse_npm_alias_real_name_helper() {
    assert_eq!(
        parse_npm_alias_real_name("react-loadable@npm:@docusaurus/react-loadable@5.5.2"),
        Some("@docusaurus/react-loadable".to_string())
    );
    assert_eq!(
        parse_npm_alias_real_name("h3-v2@npm:h3@2.0.1-rc.20"),
        Some("h3".to_string())
    );
    assert_eq!(
        parse_npm_alias_real_name("@my-scope/alias@npm:@upstream/pkg@^1.0.0"),
        Some("@upstream/pkg".to_string())
    );
    // No npm: protocol — the common case.
    assert_eq!(parse_npm_alias_real_name("foo@^1.0.0"), None);
    assert_eq!(parse_npm_alias_real_name("@scope/pkg@^1.0.0"), None);
    // Other protocols pass through as non-aliases (workspace:, file:, …).
    assert_eq!(parse_npm_alias_real_name("foo@workspace:*"), None);
}

#[test]
fn test_detect_berry_vs_classic() {
    // The `__metadata:` marker is what distinguishes berry from
    // classic; `is_berry` is the primary dispatcher signal so we
    // assert it fires on every version berry has emitted
    // (`__metadata.version` 3 through 8 across yarn 2–4).
    assert!(is_berry("__metadata:\n  version: 6\n"));
    assert!(is_berry("# comment\n__metadata:\n  version: 8\n"));
    assert!(!is_berry(
        "# yarn lockfile v1\n\nfoo@^1.0.0:\n  version \"1.0.0\"\n"
    ));
}

/// Parse → write → parse should preserve package set,
/// versions, integrity, and the resolved transitive graph. If
/// the writer emits malformed block headers or forgets to
/// requote, round-trip breaks here.
#[test]
fn test_write_roundtrip() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

foo@^1.0.0:
  version "1.2.3"
  integrity sha512-foo
  dependencies:
    bar "^2.0.0"

bar@^2.0.0:
  version "2.5.0"
  integrity sha512-bar
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    let out = tempfile::NamedTempFile::new().unwrap();
    write_classic(out.path(), &graph, &manifest).unwrap();

    // Re-parse the output. The manifest is the same — direct-dep
    // resolution requires a spec key of `foo@^1.0.0`, but the
    // writer emits `"foo@1.2.3"`. So direct-dep lookup will
    // miss; we only assert the packages/transitives round-trip.
    let reparsed_manifest = make_manifest(&[], &[]);
    let reparsed = parse(out.path(), &reparsed_manifest).unwrap();

    assert!(reparsed.packages.contains_key("foo@1.2.3"));
    assert!(reparsed.packages.contains_key("bar@2.5.0"));
    assert_eq!(
        reparsed.packages["foo@1.2.3"].integrity.as_deref(),
        Some("sha512-foo")
    );
    // foo's transitive dep on bar must still resolve: the writer
    // emits `bar "2.5.0"` under foo's dependencies, and reparse
    // finds the block keyed `"bar@2.5.0"` via spec_to_dep_path.
    assert_eq!(
        reparsed.packages["foo@1.2.3"]
            .dependencies
            .get("bar")
            .map(String::as_str),
        Some("2.5.0")
    );
}

#[test]
fn test_dev_dep_classification() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"foo@^1.0.0:
  version "1.0.0"
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[], &[("foo", "^1.0.0")]);
    let graph = parse(tmp.path(), &manifest).unwrap();
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root[0].dep_type, DepType::Dev);
}

// ---- berry (v2+) ---------------------------------------------------

#[test]
fn test_parse_berry_spec() {
    assert_eq!(
        parse_berry_spec("lodash@npm:^4.17.0"),
        Some(("lodash", "npm", "^4.17.0"))
    );
    assert_eq!(
        parse_berry_spec("@types/node@npm:20.1.0"),
        Some(("@types/node", "npm", "20.1.0"))
    );
    assert_eq!(
        parse_berry_spec("my-pkg@workspace:."),
        Some(("my-pkg", "workspace", "."))
    );
    // Missing protocol colon: malformed.
    assert_eq!(parse_berry_spec("no-protocol"), None);
}

#[test]
fn test_split_berry_header() {
    let specs = split_berry_header("lodash@npm:^4.17.0, lodash@npm:^4.18.0");
    assert_eq!(
        specs,
        vec![
            "lodash@npm:^4.17.0".to_string(),
            "lodash@npm:^4.18.0".to_string()
        ]
    );
    let single = split_berry_header("foo@npm:1.0.0");
    assert_eq!(single, vec!["foo@npm:1.0.0".to_string()]);
}

#[test]
fn test_range_has_protocol() {
    assert!(range_has_protocol("npm:^1.0.0"));
    assert!(range_has_protocol("workspace:*"));
    assert!(range_has_protocol("file:./pkgs/foo"));
    assert!(range_has_protocol("patch:react@^18.0.0#./mypatch.patch"));
    // Compound transports: berry emits these for git-over-ssh /
    // git-over-https, and the writer must not re-prefix them with
    // `npm:` when building header specs from the manifest range.
    assert!(range_has_protocol("git+ssh://git@github.com/u/r.git"));
    assert!(range_has_protocol("git+https://github.com/u/r.git"));
    assert!(range_has_protocol("git+file:./vendored.git"));
    // Bare semver ranges never have a protocol.
    assert!(!range_has_protocol("^1.0.0"));
    assert!(!range_has_protocol("1.2.3"));
    assert!(!range_has_protocol(">=1.0 <2.0"));
}

/// Realistic yarn 4 lockfile with `npm:` deps — the overwhelming
/// majority real-world case. Exercises `__metadata` parsing,
/// multi-spec block headers, nested `dependencies:`, and the
/// direct-dep pass that prepends `npm:` to manifest ranges.
#[test]
fn test_parse_berry_simple() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# This file is generated by running "yarn install" inside your project.
# Manual changes might be lost - proceed with caution!

__metadata:
  version: 8
  cacheKey: 10c0

"foo@npm:^1.0.0":
  version: 1.2.3
  resolution: "foo@npm:1.2.3"
  dependencies:
    bar: "npm:^2.0.0"
  checksum: 10c0/abcdef
  languageName: node
  linkType: hard

"bar@npm:^2.0.0":
  version: 2.5.0
  resolution: "bar@npm:2.5.0"
  checksum: 10c0/123456
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert_eq!(graph.packages.len(), 2);
    let foo = &graph.packages["foo@1.2.3"];
    assert_eq!(foo.version, "1.2.3");
    assert_eq!(foo.yarn_checksum.as_deref(), Some("10c0/abcdef"));
    assert_eq!(
        foo.dependencies.get("bar").map(String::as_str),
        Some("bar@2.5.0")
    );

    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "foo");
    assert_eq!(root[0].dep_path, "foo@1.2.3");
}

#[test]
fn test_parse_berry_patch_protocol() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"is-number@patch:is-number@npm%3A7.0.0#./.yarn/patches/is-number.patch::version=7.0.0&hash=abc123":
  version: 7.0.0
  resolution: "is-number@patch:is-number@npm%3A7.0.0#./.yarn/patches/is-number.patch::version=7.0.0&hash=abc123"
  checksum: 10c0/patched
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(
        &[(
            "is-number",
            "patch:is-number@npm%3A7.0.0#./.yarn/patches/is-number.patch::version=7.0.0&hash=abc123",
        )],
        &[],
    );
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.contains_key("is-number@7.0.0"));
    assert_eq!(
        graph
            .patched_dependencies
            .get("is-number@7.0.0")
            .map(String::as_str),
        Some("./.yarn/patches/is-number.patch")
    );
    assert_eq!(graph.importers["."][0].dep_path, "is-number@7.0.0");
}

/// Berry's builtin-compat patch protocol carries a `<qualifier>!` prefix
/// before `builtin<...>` (e.g. `optional!builtin<compat/resolve>`) — the
/// form yarn emits for resolve/typescript via eslint-plugin-import &c.
/// The `!`-qualified target must be recognized as a builtin (not a real
/// patch file path), so the package installs from its companion npm
/// block instead of failing with "failed to read patch file".
#[test]
fn test_parse_berry_qualified_builtin_patch_resolves_from_npm_block() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"resolve@npm:1.22.8, resolve@npm:^1.22.0":
  version: 1.22.8
  resolution: "resolve@npm:1.22.8"
  checksum: 10c0/realnpm
  languageName: node
  linkType: hard

"resolve@patch:resolve@npm%3A1.22.8#optional!builtin<compat/resolve>, resolve@patch:resolve@npm%3A^1.22.0#optional!builtin<compat/resolve>":
  version: 1.22.8
  resolution: "resolve@patch:resolve@npm%3A1.22.8#optional!builtin<compat/resolve>::version=1.22.8&hash=c3c19d"
  checksum: 10c0/patched
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("resolve", "^1.22.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    // The npm package is present; the builtin-compat patch produced no
    // bogus patched_dependencies entry (which would make the install try
    // to read `optional!builtin<compat/resolve>` as a file).
    assert!(graph.packages.contains_key("resolve@1.22.8"));
    assert!(graph.patched_dependencies.is_empty());
    assert_eq!(graph.importers["."][0].dep_path, "resolve@1.22.8");
}

#[test]
fn test_parse_berry_skips_builtin_patch_protocol() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"glob@patch:glob@npm%3A8.1.0#~builtin<compat/glob>":
  version: 8.1.0
  resolution: "glob@patch:glob@npm%3A8.1.0#~builtin<compat/glob>"
  checksum: 10c0/patched
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("glob", "^8.1.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.is_empty());
    assert!(graph.patched_dependencies.is_empty());
    assert!(graph.importers["."].is_empty());
}

/// Scoped package names (`@types/node`) and the `, `-joined
/// multi-spec header format berry uses when two package.json
/// ranges resolve to the same version.
#[test]
fn test_parse_berry_scoped_and_multi_spec() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"@scope/pkg@npm:^1.0.0, @scope/pkg@npm:^1.1.0":
  version: 1.1.0
  resolution: "@scope/pkg@npm:1.1.0"
  checksum: 10c0/zzz
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("@scope/pkg", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.contains_key("@scope/pkg@1.1.0"));
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root[0].name, "@scope/pkg");
    assert_eq!(root[0].dep_path, "@scope/pkg@1.1.0");
}

/// A root `resolutions` entry rewrites the descriptor yarn writes to
/// the lockfile: with `resolutions: {"@types/node": "18.x"}`, the
/// manifest still declares `^18.14` but `yarn.lock` is keyed only by
/// the resolved descriptor `@types/node@npm:18.x`. The direct-dep pass
/// must apply the resolution before matching, or the importer dep is
/// silently dropped and the satisfaction check refuses a tree that
/// `yarn install --immutable` accepts.
///
/// Shape taken from jestjs/jest's committed package.json + yarn.lock
/// (root `resolutions: {"@types/node": "18.x"}`), found by differential
/// corpus testing against jest.
#[test]
fn test_parse_berry_applies_root_resolution_to_direct_dep() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"@types/node@npm:18.x":
  version: 18.19.130
  resolution: "@types/node@npm:18.19.130"
  checksum: 10c0/abcdef
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();

    let mut manifest = make_manifest(&[], &[("@types/node", "^18.14")]);
    manifest.extra.insert(
        "resolutions".to_string(),
        serde_json::json!({ "@types/node": "18.x" }),
    );

    let graph = parse(tmp.path(), &manifest).unwrap();

    let root = graph.importers.get(".").unwrap();
    let dep = root
        .iter()
        .find(|d| d.name == "@types/node")
        .expect("@types/node direct dep must resolve through the root resolution");
    assert_eq!(dep.dep_path, "@types/node@18.19.130");
}

/// Blocks for the project's own workspace entry shouldn't become
/// `LockedPackage`s — they're the root importer, not a
/// resolved dep. Skipping them keeps the graph shape identical to
/// what parsing the `package.json` alone would produce.
#[test]
fn test_parse_berry_skips_workspace_root() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"my-project@workspace:.":
  version: 0.0.0-use.local
  resolution: "my-project@workspace:."
  dependencies:
    foo: "npm:^1.0.0"
  languageName: unknown
  linkType: soft

"foo@npm:^1.0.0":
  version: 1.0.0
  resolution: "foo@npm:1.0.0"
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    // Workspace block is skipped; only the real resolved dep survives.
    assert_eq!(graph.packages.len(), 1);
    assert!(graph.packages.contains_key("foo@1.0.0"));
    assert!(!graph.packages.contains_key("my-project@0.0.0-use.local"));
}

/// Berry emits `version:` unquoted, so scalar-looking values can
/// parse as numbers instead of strings. Our parser must unfold
/// those back to strings instead of failing with "has no version" —
/// real packages with fewer-than-three-component versions do exist
/// (even if rare).
#[test]
fn test_parse_berry_unquoted_numeric_version() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"int-version@npm:5":
  version: 5
  resolution: "int-version@npm:5"
  languageName: node
  linkType: hard

"two-part@npm:1.0":
  version: 1.0
  resolution: "two-part@npm:1.0"
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.contains_key("int-version@5"));
    assert!(graph.packages.contains_key("two-part@1.0"));
    assert_eq!(graph.packages["int-version@5"].version, "5");
    assert_eq!(graph.packages["two-part@1.0"].version, "1.0");
}

/// Same scalar hazard applies to dependency values:
/// `peerDependencies: { foo: 5 }` writes a YAML number, and
/// boolean-looking tags or ranges can parse as booleans. The parser
/// routes dep values through `yaml_scalar_as_string` so a future
/// regression shows up as a missing peer edge rather than a parse
/// error.
#[test]
fn test_parse_berry_typed_dep_values() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"foo@npm:^1.0.0":
  version: 1.0.0
  resolution: "foo@npm:1.0.0"
  peerDependencies:
    numeric-peer: 5
    bool-peer: true
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();
    let foo = &graph.packages["foo@1.0.0"];
    assert_eq!(
        foo.peer_dependencies
            .get("numeric-peer")
            .map(String::as_str),
        Some("5")
    );
    assert_eq!(
        foo.peer_dependencies.get("bool-peer").map(String::as_str),
        Some("true")
    );
}

/// Berry's `https:` tarball protocol and `git+ssh:` / `git:`
/// transports both survive parsing with a populated
/// `LocalSource`, rather than falling through to the "unknown
/// protocol" skip path.
///
/// The hazard this guards against: `parse_berry_spec` splits
/// `"foo@https://host/path"` into `res_protocol = "https"` /
/// `res_body = "//host/path"` — the body never starts with
/// `https://`, so a URL-body check would always miss. Parsing the
/// file and verifying the package lands in the graph with the
/// right `LocalSource` catches any future regression of the
/// dispatch match arms.
#[test]
fn test_parse_berry_http_and_git_protocols() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"tarball-pkg@https://example.com/pkg-1.0.0.tgz":
  version: 1.0.0
  resolution: "tarball-pkg@https://example.com/pkg-1.0.0.tgz"
  languageName: node
  linkType: hard

"git-pkg@https://github.com/user/repo.git#commit=abcdef0123456789abcdef0123456789abcdef01":
  version: 2.0.0
  resolution: "git-pkg@https://github.com/user/repo.git#commit=abcdef0123456789abcdef0123456789abcdef01"
  languageName: node
  linkType: hard

"ssh-git-pkg@git+ssh://git@github.com/user/other.git#deadbeef":
  version: 3.0.0
  resolution: "ssh-git-pkg@git+ssh://git@github.com/user/other.git#deadbeef"
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    // All three packages should be present — none silently
    // skipped as "unrecognized protocol". The `.values()` scan
    // below asserts the `LocalSource` shape for each.
    assert_eq!(graph.packages.len(), 3);
    let by_name: BTreeMap<&str, &LockedPackage> = graph
        .packages
        .values()
        .map(|p| (p.name.as_str(), p))
        .collect();

    // `.tgz` on https → remote tarball.
    let tar = by_name["tarball-pkg"];
    assert!(matches!(
        &tar.local_source,
        Some(LocalSource::RemoteTarball(_))
    ));

    // `.git` on https → git source, not tarball.
    let git = by_name["git-pkg"];
    let Some(LocalSource::Git(git)) = &git.local_source else {
        panic!("expected git LocalSource");
    };
    assert_eq!(git.url, "https://github.com/user/repo.git");
    assert_eq!(git.resolved, "abcdef0123456789abcdef0123456789abcdef01");

    // `git+ssh:` prefix → git source.
    let ssh = by_name["ssh-git-pkg"];
    assert!(matches!(&ssh.local_source, Some(LocalSource::Git(_))));
}

#[test]
fn test_parse_berry_portal_and_exec_protocols() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"portal-pkg@portal:./packages/portal":
  version: 1.0.0
  resolution: "portal-pkg@portal:./packages/portal"
  dependencies:
    left-pad: "npm:^1.3.0"
  languageName: node
  linkType: soft

"exec-pkg@exec:./scripts/generate-exec.js":
  version: 2.0.0
  resolution: "exec-pkg@exec:./scripts/generate-exec.js"
  languageName: node
  linkType: hard

"left-pad@npm:^1.3.0":
  version: 1.3.0
  resolution: "left-pad@npm:1.3.0"
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(
        &[
            ("portal-pkg", "portal:./packages/portal"),
            ("exec-pkg", "exec:./scripts/generate-exec.js"),
        ],
        &[],
    );
    let graph = parse(tmp.path(), &manifest).unwrap();

    let portal_key = LocalSource::Portal(PathBuf::from("./packages/portal")).dep_path("portal-pkg");
    let portal = &graph.packages[&portal_key];
    assert!(matches!(
        &portal.local_source,
        Some(LocalSource::Portal(p)) if p == &PathBuf::from("./packages/portal")
    ));
    assert_eq!(
        portal.dependencies.get("left-pad").map(String::as_str),
        Some("left-pad@1.3.0")
    );

    let exec_key =
        LocalSource::Exec(PathBuf::from("./scripts/generate-exec.js")).dep_path("exec-pkg");
    let exec = &graph.packages[&exec_key];
    assert!(matches!(
        &exec.local_source,
        Some(LocalSource::Exec(p)) if p == &PathBuf::from("./scripts/generate-exec.js")
    ));
    assert_eq!(graph.importers["."].len(), 2);
}

/// Round-trip: parse berry → write berry → parse berry should
/// preserve packages, versions, checksum (via `yarn_checksum`),
/// and transitive edges. This is the core round-trip contract.
#[test]
fn test_write_berry_roundtrip() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"foo@npm:^1.0.0":
  version: 1.2.3
  resolution: "foo@npm:1.2.3"
  dependencies:
    bar: "npm:^2.0.0"
  checksum: 10c0/foohash
  languageName: node
  linkType: hard

"bar@npm:^2.0.0":
  version: 2.5.0
  resolution: "bar@npm:2.5.0"
  checksum: 10c0/barhash
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();

    // Confirm the output is berry-shaped so dispatcher picks the
    // right parser on reparse.
    let written = std::fs::read_to_string(out.path()).unwrap();
    assert!(is_berry(&written));

    let reparsed_manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let reparsed = parse(out.path(), &reparsed_manifest).unwrap();

    assert!(reparsed.packages.contains_key("foo@1.2.3"));
    assert!(reparsed.packages.contains_key("bar@2.5.0"));
    assert_eq!(
        reparsed.packages["foo@1.2.3"].yarn_checksum.as_deref(),
        Some("10c0/foohash")
    );
    assert_eq!(
        reparsed.packages["foo@1.2.3"]
            .dependencies
            .get("bar")
            .map(String::as_str),
        Some("bar@2.5.0")
    );
    // The manifest spec `foo@^1.0.0` appears verbatim (with `npm:`
    // prepended) in the block header, so direct-dep lookup
    // succeeds on reparse — which it did NOT for classic, so this
    // is a stronger round-trip guarantee.
    let root = reparsed.importers.get(".").unwrap();
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].dep_path, "foo@1.2.3");
}

#[test]
fn test_write_berry_output_matches_yarn4_layout() {
    // The berry writer reproduces yarn 4's exact on-disk layout — proven
    // byte-identical against real `yarn install` output (tests/conformance
    // berry leg). This guards the specific shape choices that make
    // `yarn install --immutable` zero-churn: metadata version 10, the root
    // `<name>@workspace:.` block sorted in among the packages, unquoted
    // `version`/`checksum`/bare-key scalars, quoted `resolution`/`npm:`
    // values, headers carrying only the declared ranges (not the exact
    // `name@npm:version` resolution).
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"foo@npm:^1.0.0":
  version: 1.2.3
  resolution: "foo@npm:1.2.3"
  dependencies:
    bar: "npm:^2.0.0"
  checksum: 10c0/foohash
  languageName: node
  linkType: hard

"bar@npm:^2.0.0":
  version: 2.5.0
  resolution: "bar@npm:2.5.0"
  checksum: 10c0/barhash
  languageName: node
  linkType: hard
"#;
    let src = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(src.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(src.path(), &manifest).unwrap();
    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    // metadata version is yarn 4's (10), not the historical 8.
    assert!(
        written.contains("__metadata:\n  version: 10\n  cacheKey: 10c0\n"),
        "metadata must be yarn-4-shaped, got:\n{written}"
    );
    // root workspace block present (manifest name defaults to "root").
    assert!(
        written.contains("\"test@workspace:.\":\n  version: 0.0.0-use.local\n"),
        "root workspace importer block must be emitted, got:\n{written}"
    );
    // version + checksum unquoted; resolution + npm-range values quoted.
    assert!(
        written.contains("  version: 1.2.3\n"),
        "version must be unquoted"
    );
    assert!(
        written.contains("  checksum: 10c0/foohash\n"),
        "checksum must be unquoted"
    );
    assert!(
        written.contains("  resolution: \"foo@npm:1.2.3\"\n"),
        "resolution must be quoted (carries a `:`)"
    );
    assert!(
        written.contains("    bar: \"npm:^2.0.0\"\n"),
        "dep keys bare, npm-range values quoted"
    );
    // header carries the declared range only, not the exact resolution spec.
    assert!(
        written.contains("\"foo@npm:^1.0.0\":\n"),
        "header lists the declared range, got:\n{written}"
    );
    assert!(
        !written.contains("foo@npm:1.2.3, "),
        "the exact resolution spec must not be folded into the header"
    );
    // blocks sorted by descriptor: bar < foo < root@workspace.
    let bar_at = written.find("\"bar@npm").unwrap();
    let foo_at = written.find("\"foo@npm").unwrap();
    let root_at = written.find("\"test@workspace").unwrap();
    assert!(
        bar_at < foo_at && foo_at < root_at,
        "blocks must be sorted by header"
    );
}

#[test]
fn test_write_berry_roundtrips_patch_protocol() {
    let mut packages = BTreeMap::new();
    packages.insert(
        "is-number@7.0.0".to_string(),
        LockedPackage {
            name: "is-number".to_string(),
            version: "7.0.0".to_string(),
            dep_path: "is-number@7.0.0".to_string(),
            yarn_checksum: Some("10c0/patched".to_string()),
            ..Default::default()
        },
    );
    let graph = LockfileGraph {
        importers: {
            let mut m = BTreeMap::new();
            m.insert(
                ".".to_string(),
                vec![crate::DirectDep {
                    name: "is-number".to_string(),
                    dep_path: "is-number@7.0.0".to_string(),
                    dep_type: DepType::Production,
                    specifier: None,
                }],
            );
            m
        },
        packages,
        patched_dependencies: BTreeMap::from([(
            "is-number@7.0.0".to_string(),
            ".yarn/patches/is-number.patch".to_string(),
        )]),
        ..Default::default()
    };
    let manifest = make_manifest(
        &[(
            "is-number",
            "patch:is-number@npm%3A7.0.0#.yarn/patches/is-number.patch::version=7.0.0&hash=abc123",
        )],
        &[],
    );

    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();
    let full_spec = "is-number@patch:is-number@npm%3A7.0.0#.yarn/patches/is-number.patch::version=7.0.0&hash=abc123";
    assert!(written.contains(full_spec));
    assert!(!written.contains(&format!(
        "is-number@patch:is-number@npm%3A7.0.0#.yarn/patches/is-number.patch, {full_spec}"
    )));

    let reparsed = parse(out.path(), &manifest).unwrap();
    assert_eq!(
        reparsed
            .patched_dependencies
            .get("is-number@7.0.0")
            .map(String::as_str),
        Some(".yarn/patches/is-number.patch")
    );
    assert_eq!(reparsed.importers["."][0].dep_path, "is-number@7.0.0");
}

/// `link:` deps are pure symlinks in berry's model, which means
/// the block must carry `linkType: soft` — writing `hard` makes
/// yarn's own linker try to copy/hardlink the target into the
/// virtual store on the next install. Registry packages (no
/// `local_source`) stay `hard`, the default.
#[test]
fn test_write_berry_link_type_soft_for_link_deps() {
    let mut packages = BTreeMap::new();
    packages.insert(
        "linked-pkg@1.0.0".to_string(),
        LockedPackage {
            name: "linked-pkg".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "linked-pkg@1.0.0".to_string(),
            local_source: Some(LocalSource::Link(PathBuf::from("./vendor/linked-pkg"))),
            ..Default::default()
        },
    );
    packages.insert(
        "regular-pkg@2.0.0".to_string(),
        LockedPackage {
            name: "regular-pkg".to_string(),
            version: "2.0.0".to_string(),
            dep_path: "regular-pkg@2.0.0".to_string(),
            ..Default::default()
        },
    );
    let graph = LockfileGraph {
        importers: {
            let mut m = BTreeMap::new();
            m.insert(".".to_string(), vec![]);
            m
        },
        packages,
        ..Default::default()
    };
    let manifest = make_manifest(&[], &[]);

    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    // The `link:` block gets `soft`; the registry block stays `hard`.
    // Block order is sorted by canonical key, so `linked-pkg`
    // comes before `regular-pkg` and each block's `linkType`
    // appears after its `languageName` line.
    let linked_idx = written.find("linked-pkg@").unwrap();
    let regular_idx = written.find("regular-pkg@").unwrap();
    let linked_block = &written[linked_idx..regular_idx];
    let regular_block = &written[regular_idx..];
    assert!(
        linked_block.contains("linkType: soft"),
        "link: block should be soft-linked:\n{linked_block}"
    );
    assert!(
        regular_block.contains("linkType: hard"),
        "registry block should be hard-linked:\n{regular_block}"
    );
}

#[test]
fn test_write_berry_roundtrips_portal_and_exec_protocols() {
    let portal_source = LocalSource::Portal(PathBuf::from("./packages/portal"));
    let exec_source = LocalSource::Exec(PathBuf::from("./scripts/generate-exec.js"));
    let mut packages = BTreeMap::new();
    packages.insert(
        portal_source.dep_path("portal-pkg"),
        LockedPackage {
            name: "portal-pkg".to_string(),
            version: "1.0.0".to_string(),
            dep_path: portal_source.dep_path("portal-pkg"),
            local_source: Some(portal_source),
            ..Default::default()
        },
    );
    packages.insert(
        exec_source.dep_path("exec-pkg"),
        LockedPackage {
            name: "exec-pkg".to_string(),
            version: "2.0.0".to_string(),
            dep_path: exec_source.dep_path("exec-pkg"),
            local_source: Some(exec_source),
            ..Default::default()
        },
    );
    let graph = LockfileGraph {
        importers: {
            let mut m = BTreeMap::new();
            m.insert(".".to_string(), vec![]);
            m
        },
        packages,
        ..Default::default()
    };
    let manifest = make_manifest(&[], &[]);

    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    assert!(written.contains("portal-pkg@portal:./packages/portal"));
    assert!(written.contains("exec-pkg@exec:./scripts/generate-exec.js"));
    let portal_idx = written.find("portal-pkg@portal:").unwrap();
    let exec_idx = written.find("exec-pkg@exec:").unwrap();
    assert!(
        exec_idx < portal_idx,
        "expected exec block before portal block:\n{written}"
    );
    let portal_block = &written[portal_idx..];
    let exec_block = &written[exec_idx..portal_idx];
    assert!(portal_block.contains("linkType: soft"));
    assert!(exec_block.contains("linkType: hard"));
}

/// Header and `resolution:` both carry spec strings that may
/// contain backslashes (Windows-style `file:` paths) or embedded
/// quotes (patched-package descriptors). The writer must route
/// them through `quote_yaml_scalar` so the emitted YAML is
/// well-formed. We can't easily drive backslashes into the model
/// from a parsed berry file (berry itself doesn't emit them on
/// macOS/Linux), so we construct a package with a `file:` source
/// that contains a backslash directly and assert the output
/// escapes it and round-trips through `yaml_serde::from_str`.
#[test]
fn test_write_berry_escapes_resolution_and_header() {
    let mut packages = BTreeMap::new();
    packages.insert(
        "weird-pkg@1.0.0".to_string(),
        LockedPackage {
            name: "weird-pkg".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "weird-pkg@1.0.0".to_string(),
            // A file: source whose path has a backslash. The
            // header and resolution both become
            // `weird-pkg@file:./a\b/c`; without escaping, the
            // raw backslash in the YAML string would be a
            // malformed escape.
            local_source: Some(LocalSource::Directory(PathBuf::from("./a\\b/c"))),
            ..Default::default()
        },
    );
    let graph = LockfileGraph {
        importers: {
            let mut m = BTreeMap::new();
            m.insert(".".to_string(), vec![]);
            m
        },
        packages,
        ..Default::default()
    };
    let manifest = make_manifest(&[], &[]);

    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    // The emitted file must parse as YAML — any missing escape
    // blows up here instead of corrupting a real install.
    let _doc: yaml_serde::Value = yaml_serde::from_str(&written)
        .unwrap_or_else(|e| panic!("berry writer produced malformed YAML: {e}\n{written}"));
}

/// The berry writer emits the package's `bin:` map (yarn 4 carries it from
/// the manifest into the lockfile). It's modeled on `LockedPackage.bin` but
/// the berry reader never populates it, so a graph built from the registry —
/// where bins are real — must round-trip them through the writer. Empty-key
/// placeholders are skipped so they don't render as `"": …`.
#[test]
fn test_write_berry_emits_bin_map() {
    let mut packages = BTreeMap::new();
    packages.insert(
        "cli-tool@1.0.0".to_string(),
        LockedPackage {
            name: "cli-tool".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "cli-tool@1.0.0".to_string(),
            bin: [
                ("cli-tool".to_string(), "./bin/cli.js".to_string()),
                // Empty-key placeholder (pnpm's hasBin collapse) — must not
                // render as `"": …`.
                (String::new(), "ignored".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        },
    );
    let graph = LockfileGraph {
        importers: {
            let mut m = BTreeMap::new();
            m.insert(".".to_string(), vec![]);
            m
        },
        packages,
        ..Default::default()
    };
    let manifest = make_manifest(&[], &[]);

    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    // `bin:` nested map with the executable, indented like the dep maps.
    assert!(
        written.contains("  bin:\n    cli-tool: ./bin/cli.js\n"),
        "berry writer must emit the bin map, got:\n{written}"
    );
    // Empty-key placeholder is dropped, not rendered as `"": …`.
    assert!(
        !written.contains("\"\":"),
        "empty-key bin placeholder must be skipped, got:\n{written}"
    );
    // Output stays valid YAML.
    let _doc: yaml_serde::Value = yaml_serde::from_str(&written)
        .unwrap_or_else(|e| panic!("berry writer produced malformed YAML: {e}\n{written}"));
}

/// Yarn v1's lockfile parser reads a leading `@` as the start of a
/// scoped-package token and requires the dependency-map key to be a
/// double-quoted string; a bare `@scope/name` key throws `Unknown
/// token … INVALID`. The classic writer must quote scoped keys inside
/// `dependencies:` (matching real yarn, which writes
/// `"@babel/helper-validator-identifier" "^7.x"`) while leaving plain
/// names unquoted. Regression test for the 0.0.34 conformance gate.
#[test]
fn test_write_classic_quotes_scoped_dependency_keys() {
    let mut packages = BTreeMap::new();
    packages.insert(
        "parent@1.0.0".to_string(),
        LockedPackage {
            name: "parent".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "parent@1.0.0".to_string(),
            dependencies: BTreeMap::from([
                (
                    "@babel/helper".to_string(),
                    "@babel/helper@7.0.0".to_string(),
                ),
                ("js-tokens".to_string(), "js-tokens@4.0.0".to_string()),
            ]),
            ..Default::default()
        },
    );
    packages.insert(
        "@babel/helper@7.0.0".to_string(),
        LockedPackage {
            name: "@babel/helper".to_string(),
            version: "7.0.0".to_string(),
            dep_path: "@babel/helper@7.0.0".to_string(),
            ..Default::default()
        },
    );
    packages.insert(
        "js-tokens@4.0.0".to_string(),
        LockedPackage {
            name: "js-tokens".to_string(),
            version: "4.0.0".to_string(),
            dep_path: "js-tokens@4.0.0".to_string(),
            ..Default::default()
        },
    );
    let graph = LockfileGraph {
        importers: BTreeMap::from([(".".to_string(), vec![])]),
        packages,
        ..Default::default()
    };
    let manifest = make_manifest(&[], &[]);

    let out = tempfile::NamedTempFile::new().unwrap();
    write_classic(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    // Scoped key quoted, bare key not — exactly as yarn v1 emits them.
    assert!(
        written.contains("    \"@babel/helper\" \"7.0.0\"\n"),
        "scoped dep key must be quoted:\n{written}"
    );
    assert!(
        written.contains("    js-tokens \"4.0.0\"\n"),
        "bare dep key must stay unquoted:\n{written}"
    );
    // The unquoted `@…` form that yarn v1 rejects must not appear.
    assert!(
        !written.contains("    @babel/helper "),
        "unquoted scoped dep key would break yarn v1:\n{written}"
    );
}

/// A `file:` local-source package is keyed by the protocol descriptor
/// its consumer declared (`local-utils@file:./local-pkg`), carries a
/// `version` but no `resolved`/`integrity`, and the header uses the
/// manifest's literal range (preserving the `./`) so yarn v1's
/// `--frozen-lockfile` reconciliation against package.json matches.
/// Keying it `name@version` or dropping the `./` makes yarn demand a
/// rewrite. Regression test for the 0.0.34 conformance gate.
#[test]
fn test_write_classic_file_dep_header_and_no_resolved() {
    let source = LocalSource::Directory(PathBuf::from("local-pkg"));
    let mut packages = BTreeMap::new();
    packages.insert(
        source.dep_path("local-utils"),
        LockedPackage {
            name: "local-utils".to_string(),
            version: "1.0.0".to_string(),
            // A stray integrity must be suppressed for local sources.
            integrity: Some("sha512-should-not-appear".to_string()),
            dep_path: source.dep_path("local-utils"),
            local_source: Some(source),
            ..Default::default()
        },
    );
    let graph = LockfileGraph {
        importers: BTreeMap::from([(".".to_string(), vec![])]),
        packages,
        ..Default::default()
    };
    // Manifest declares the canonical `file:./local-pkg` range; the
    // header must reproduce it verbatim, leading `./` and all.
    let manifest = make_manifest(&[("local-utils", "file:./local-pkg")], &[]);

    let out = tempfile::NamedTempFile::new().unwrap();
    write_classic(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    assert!(
        written.contains("\"local-utils@file:./local-pkg\":\n"),
        "file: block must be keyed by the declared protocol range:\n{written}"
    );
    assert!(
        written.contains("  version \"1.0.0\"\n"),
        "file: block must carry its version:\n{written}"
    );
    assert!(
        !written.contains("integrity"),
        "file: block must not carry a registry integrity:\n{written}"
    );
    assert!(
        !written.contains("\"local-utils@1.0.0\""),
        "file: block must not be keyed name@version:\n{written}"
    );
}

// A hosted git dep (npm `resolved: git+ssh://…#<40-char-sha>`) must be
// keyed by the ORIGINAL git descriptor the manifest declared
// (`ms@vercel/ms#4ff48cec`), with a codeload `resolved` line — exactly
// what real yarn v1 writes. Keying it by the expanded resolved URL makes
// `yarn install --frozen-lockfile` reject the file (it can't match the
// manifest's range), so the recovered descriptor is load-bearing.
#[test]
fn test_write_classic_git_dep_uses_declared_descriptor_and_codeload_resolved() {
    let git = crate::GitSource {
        url: "ssh://git@github.com/vercel/ms.git".to_string(),
        committish: Some("4ff48cec099f0514c3e9bbca18706c9c21122bfb".to_string()),
        resolved: "4ff48cec099f0514c3e9bbca18706c9c21122bfb".to_string(),
        integrity: None,
        subpath: None,
    };
    let source = LocalSource::Git(git);
    let mut packages = BTreeMap::new();
    packages.insert(
        source.dep_path("ms"),
        LockedPackage {
            name: "ms".to_string(),
            version: "4.0.0".to_string(),
            dep_path: source.dep_path("ms"),
            local_source: Some(source),
            ..Default::default()
        },
    );
    let graph = LockfileGraph {
        importers: BTreeMap::from([(".".to_string(), vec![])]),
        packages,
        ..Default::default()
    };
    // The manifest declares the GitHub shorthand; the block header must
    // reproduce it, not the expanded ssh url.
    let manifest = make_manifest(&[("ms", "vercel/ms#4ff48cec")], &[]);

    let out = tempfile::NamedTempFile::new().unwrap();
    write_classic(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    assert!(
        written.contains("\"ms@vercel/ms#4ff48cec\":\n"),
        "git block must be keyed by the declared descriptor:\n{written}"
    );
    assert!(
        written.contains(
            "  resolved \"https://codeload.github.com/vercel/ms/tar.gz/\
             4ff48cec099f0514c3e9bbca18706c9c21122bfb\"\n"
        ),
        "git block must carry the codeload resolved URL:\n{written}"
    );
    assert!(
        !written.contains("ssh://git@github.com"),
        "the expanded ssh url must not leak into the block header or resolved:\n{written}"
    );
}

// When a git dep's original descriptor can't be recovered from the
// manifest (e.g. a transitive git dep absent from every package.json), the
// writer must REFUSE rather than emit the unmatchable expanded-URL header
// — never silently write a yarn-rejected lockfile.
#[test]
fn test_write_classic_git_dep_without_declared_descriptor_is_refused() {
    let git = crate::GitSource {
        url: "ssh://git@github.com/vercel/ms.git".to_string(),
        committish: Some("4ff48cec099f0514c3e9bbca18706c9c21122bfb".to_string()),
        resolved: "4ff48cec099f0514c3e9bbca18706c9c21122bfb".to_string(),
        integrity: None,
        subpath: None,
    };
    let source = LocalSource::Git(git);
    let mut packages = BTreeMap::new();
    packages.insert(
        source.dep_path("ms"),
        LockedPackage {
            name: "ms".to_string(),
            version: "4.0.0".to_string(),
            dep_path: source.dep_path("ms"),
            local_source: Some(source),
            ..Default::default()
        },
    );
    let graph = LockfileGraph {
        importers: BTreeMap::from([(".".to_string(), vec![])]),
        packages,
        ..Default::default()
    };
    // No `ms` in the manifest (nor any other package's declared deps): the
    // descriptor is unrecoverable.
    let manifest = make_manifest(&[], &[]);

    let out = tempfile::NamedTempFile::new().unwrap();
    let err = write_classic(out.path(), &graph, &manifest).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("ms") && msg.contains("git dependency"),
        "the refusal must name the dep and explain the git-conversion limit, got: {msg}"
    );
}

// A hosted-git dependency the resolver fetched through a codeload archive
// (or that pnpm v9+ recorded the same way) arrives as a
// `RemoteTarball { git_hosted: true }`, NOT a `LocalSource::Git`. The yarn
// classic writer must still key it by the declared git spec with a
// `resolved "<codeload tarball URL>"` line and the real semver `version` —
// keying it by the tarball URL with `version "0.0.0"` (the old local-source
// fallback) makes `yarn install --frozen-lockfile` reject the file. This is
// the shape the `nub pm use yarn` conversion actually feeds the writer (its
// graph comes from the pnpm reader), so it is the real-world path.
#[test]
fn test_write_classic_git_hosted_tarball_echoes_declared_git_url_resolved() {
    let sha = "1c6264b795492e8fdecbc82cb8802fcfbfc08d26";
    let source = LocalSource::RemoteTarball(crate::RemoteTarballSource {
        url: format!("https://codeload.github.com/vercel/ms/tar.gz/{sha}"),
        // pnpm's codeload resolution carries no integrity; even when one is
        // present, yarn v1 keys off `resolved`, not `integrity`.
        integrity: String::new(),
        git_hosted: true,
    });
    let mut packages = BTreeMap::new();
    packages.insert(
        source.dep_path("ms"),
        LockedPackage {
            name: "ms".to_string(),
            version: "2.1.3".to_string(),
            dep_path: source.dep_path("ms"),
            local_source: Some(source),
            ..Default::default()
        },
    );
    let graph = LockfileGraph {
        importers: BTreeMap::from([(".".to_string(), vec![])]),
        packages,
        ..Default::default()
    };
    // The manifest declares the full git+https spec; the block header must
    // reproduce it verbatim (what yarn matches against on a frozen install).
    let declared = format!("git+https://github.com/vercel/ms.git#{sha}");
    let manifest = make_manifest(&[("ms", &declared)], &[]);

    let out = tempfile::NamedTempFile::new().unwrap();
    write_classic(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    assert!(
        written.contains(&format!("\"ms@{declared}\":\n")),
        "git block must be keyed by the declared git spec, not the tarball URL:\n{written}"
    );
    assert!(
        written.contains("  version \"2.1.3\"\n"),
        "git block must carry the real semver, not `0.0.0`:\n{written}"
    );
    // A `git+https://…#<sha>` URL declaration is echoed VERBATIM as
    // `resolved` (yarn's GitFetcher needs the commit on the URL fragment);
    // a codeload tarball would make yarn fail `Commit hash required`.
    assert!(
        written.contains(&format!("  resolved \"{declared}\"\n")),
        "a git+https URL declaration must echo verbatim as resolved:\n{written}"
    );
}

// End-to-end of the actual `nub pm use yarn` conversion path: parse a real
// pnpm v9 lockfile (where a git dep is recorded as a codeload tarball with
// the declared git spec on the importer `specifier`) and write it as a yarn
// v1 lockfile. The two git deps in the fixture cover both spellings real
// pnpm preserves: a `github:owner/repo#tag` shorthand and a
// `git+https://…#<sha>` URL. The output must match what real yarn 1.x
// writes — declared-spec keys, real versions, codeload `resolved` URLs.
#[test]
fn pnpm_v9_codeload_git_deps_convert_to_accepted_yarn_classic() {
    let pnpm_lock = r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:

  .:
    dependencies:
      is-number:
        specifier: github:jonschlinkert/is-number#7.0.0
        version: https://codeload.github.com/jonschlinkert/is-number/tar.gz/98e8ff1da1a89f93d1397a24d7413ed15421c139
      ms:
        specifier: git+https://github.com/vercel/ms.git#1c6264b795492e8fdecbc82cb8802fcfbfc08d26
        version: https://codeload.github.com/vercel/ms/tar.gz/1c6264b795492e8fdecbc82cb8802fcfbfc08d26

packages:

  is-number@https://codeload.github.com/jonschlinkert/is-number/tar.gz/98e8ff1da1a89f93d1397a24d7413ed15421c139:
    resolution: {tarball: https://codeload.github.com/jonschlinkert/is-number/tar.gz/98e8ff1da1a89f93d1397a24d7413ed15421c139}
    version: 7.0.0
    engines: {node: '>=0.12.0'}

  ms@https://codeload.github.com/vercel/ms/tar.gz/1c6264b795492e8fdecbc82cb8802fcfbfc08d26:
    resolution: {tarball: https://codeload.github.com/vercel/ms/tar.gz/1c6264b795492e8fdecbc82cb8802fcfbfc08d26}
    version: 2.1.3

snapshots:

  is-number@https://codeload.github.com/jonschlinkert/is-number/tar.gz/98e8ff1da1a89f93d1397a24d7413ed15421c139: {}

  ms@https://codeload.github.com/vercel/ms/tar.gz/1c6264b795492e8fdecbc82cb8802fcfbfc08d26: {}
"#;
    let pnpm_path = tempfile::Builder::new()
        .suffix("-pnpm-lock.yaml")
        .tempfile()
        .unwrap();
    std::fs::write(pnpm_path.path(), pnpm_lock).unwrap();
    let graph = crate::pnpm::parse(pnpm_path.path()).unwrap();

    let manifest = make_manifest(
        &[
            ("is-number", "github:jonschlinkert/is-number#7.0.0"),
            (
                "ms",
                "git+https://github.com/vercel/ms.git#1c6264b795492e8fdecbc82cb8802fcfbfc08d26",
            ),
        ],
        &[],
    );

    let out = tempfile::NamedTempFile::new().unwrap();
    write_classic(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    // `github:` shorthand → declared-spec key, real version, codeload tarball.
    assert!(
        written.contains("\"is-number@github:jonschlinkert/is-number#7.0.0\":\n"),
        "is-number must be keyed by its declared github: spec:\n{written}"
    );
    assert!(
        written.contains(
            "  resolved \"https://codeload.github.com/jonschlinkert/is-number/tar.gz/\
             98e8ff1da1a89f93d1397a24d7413ed15421c139\"\n"
        ),
        "is-number must carry its codeload resolved URL:\n{written}"
    );
    // `git+https://…#<sha>` URL → declared-spec key, and `resolved` echoed
    // verbatim (NOT a codeload tarball — yarn's GitFetcher needs the commit
    // on the URL fragment, else `Invariant Violation: Commit hash required`).
    assert!(
        written.contains(
            "\"ms@git+https://github.com/vercel/ms.git#\
             1c6264b795492e8fdecbc82cb8802fcfbfc08d26\":\n"
        ),
        "ms must be keyed by its declared git+https spec:\n{written}"
    );
    assert!(
        written.contains(
            "  resolved \"git+https://github.com/vercel/ms.git#\
             1c6264b795492e8fdecbc82cb8802fcfbfc08d26\"\n"
        ),
        "ms (git+https URL form) must echo its declared URL verbatim as resolved:\n{written}"
    );
    assert!(
        written.contains("  version \"7.0.0\"\n") && written.contains("  version \"2.1.3\"\n"),
        "both git deps must carry their real semver, not `0.0.0`:\n{written}"
    );
    assert!(
        !written.contains("@https://codeload"),
        "no git dep may be keyed by the codeload tarball URL:\n{written}"
    );
}

/// A yarn v1 yarn.lock is a flat resolution list with no workspace
/// structure — converting a yarn-SOURCE workspace must reconstruct each
/// member importer from the root manifest's `workspaces` globs plus the
/// on-disk member package.json files, or the target PM frozen-rejects
/// (pnpm ERR_PNPM_OUTDATED_LOCKFILE, npm "Missing" members, bun
/// "lockfile had changes"). Regression guard for the yarn-source
/// conversion legs of `tests/conversion/run.sh` (empty-root-importer).
#[test]
fn classic_reconstructs_workspace_members_from_globs_and_disk() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Root manifest: empty root importer, `packages/*` workspaces — the
    // empty-root-importer fixture shape.
    std::fs::write(
        root.join("package.json"),
        r#"{ "name": "wsroot", "version": "1.0.0", "private": true, "workspaces": ["packages/*"] }"#,
    )
    .unwrap();
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

    // Flat yarn.lock: only the resolved registry deps, exactly what
    // `yarn install` writes for this workspace (no importer mapping).
    let yarn_lock = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

kleur@^4.1.5:
  version "4.1.5"
  resolved "https://registry.yarnpkg.com/kleur/-/kleur-4.1.5.tgz#95106101795f7050c6c650f350c683febddb1780"
  integrity sha512-o+NO+8WrRiQEE4/7nwRJhN1HWpVmJm511pBHUxPLtp0BUISzlBplORYSmTclCnJvQq2tKu/sgl3xVpkc7ZWuQQ==

ms@^2.1.3:
  version "2.1.3"
  resolved "https://registry.yarnpkg.com/ms/-/ms-2.1.3.tgz#574c8138ce1d2b5861f0b44579dbadd60c6615b2"
  integrity sha512-6FlzubTLZG3J2a/NVCAleEhjzq5oxgHyaCU9yYXvcLsvoVaHJq/s5xXI6/XXP6tz7R9xAOtHnSO/tXtF3WRTlA==
"#;
    let lock_path = root.join("yarn.lock");
    std::fs::write(&lock_path, yarn_lock).unwrap();

    let manifest = aube_manifest::PackageJson::from_path(&root.join("package.json")).unwrap();
    let graph = parse(&lock_path, &manifest).unwrap();

    // The empty root importer carries no deps; the two members are
    // reconstructed as their own importers with their child deps.
    assert_eq!(graph.importers["."].len(), 0, "root importer must stay empty");
    let pkg_a = graph
        .importers
        .get("packages/pkg-a")
        .expect("packages/pkg-a importer must be reconstructed");
    assert_eq!(pkg_a.len(), 1);
    assert_eq!(pkg_a[0].name, "ms");
    assert_eq!(pkg_a[0].dep_path, "ms@2.1.3");
    assert_eq!(pkg_a[0].dep_type, DepType::Production);

    let pkg_b = graph
        .importers
        .get("packages/pkg-b")
        .expect("packages/pkg-b importer must be reconstructed");
    assert_eq!(pkg_b.len(), 1);
    assert_eq!(pkg_b[0].name, "kleur");
    assert_eq!(pkg_b[0].dep_path, "kleur@4.1.5");
}

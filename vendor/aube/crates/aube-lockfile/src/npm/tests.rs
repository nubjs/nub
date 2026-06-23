use super::layout::package_name_from_install_path;
use super::source::local_git_source_from_resolved;
use super::*;
use crate::{DepType, DirectDep, Error, GitSource, LocalSource, LockedPackage, LockfileGraph};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[test]
fn test_package_name_from_install_path() {
    assert_eq!(
        package_name_from_install_path("node_modules/foo"),
        Some("foo".to_string())
    );
    assert_eq!(
        package_name_from_install_path("node_modules/@scope/pkg"),
        Some("@scope/pkg".to_string())
    );
    assert_eq!(
        package_name_from_install_path("node_modules/foo/node_modules/bar"),
        Some("bar".to_string())
    );
    assert_eq!(
        package_name_from_install_path("node_modules/foo/node_modules/@scope/pkg"),
        Some("@scope/pkg".to_string())
    );
}

#[test]
fn test_parse_simple() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "foo": "^1.0.0" },
                    "devDependencies": { "bar": "^2.0.0" }
                },
                "node_modules/foo": {
                    "version": "1.2.3",
                    "integrity": "sha512-aaa",
                    "dependencies": { "nested": "^3.0.0" }
                },
                "node_modules/nested": {
                    "version": "3.1.0",
                    "integrity": "sha512-bbb"
                },
                "node_modules/bar": {
                    "version": "2.5.0",
                    "integrity": "sha512-ccc",
                    "dev": true
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();

    assert_eq!(graph.packages.len(), 3);
    assert!(graph.packages.contains_key("foo@1.2.3"));
    assert!(graph.packages.contains_key("nested@3.1.0"));
    assert!(graph.packages.contains_key("bar@2.5.0"));

    let foo = &graph.packages["foo@1.2.3"];
    assert_eq!(foo.integrity.as_deref(), Some("sha512-aaa"));
    // `LockedPackage.dependencies` values are dep_path *tails* (the
    // substring after `<name>@`), not full dep_paths — matches the
    // pnpm parser and the linker's sibling-symlink builder.
    assert_eq!(
        foo.dependencies.get("nested").map(String::as_str),
        Some("3.1.0")
    );

    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 2);
    let foo_dep = root.iter().find(|d| d.name == "foo").unwrap();
    assert_eq!(foo_dep.dep_type, DepType::Production);
    // The declared range from the root entry's `dependencies` value
    // must survive as the importer specifier — the pnpm writer needs it
    // to emit a non-empty `specifiers:` map, or pnpm frozen-install
    // rejects the converted lockfile.
    assert_eq!(foo_dep.specifier.as_deref(), Some("^1.0.0"));
    let bar_dep = root.iter().find(|d| d.name == "bar").unwrap();
    assert_eq!(bar_dep.dep_type, DepType::Dev);
    assert_eq!(bar_dep.specifier.as_deref(), Some("^2.0.0"));
}

/// `npm install --prefix <proj>` invoked from a different cwd writes
/// every `packages` key as a climb back to the project
/// (`../../../abs/proj/node_modules/foo`) instead of the canonical
/// `node_modules/foo`. The reader must normalize those so root direct
/// deps still resolve — otherwise importers come out empty, which
/// produced a pnpm-lock with an empty `specifiers:` map (pnpm frozen
/// install rejects it) and a bun.lock with `"packages": {}` (bun
/// `InvalidPackageInfo`). Regression guard for the cross-format
/// conversion harness (`tests/conversion/run.sh`, npm→pnpm + npm→bun).
#[test]
fn prefix_install_climb_paths_still_populate_importers_and_packages() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "foo": "^1.0.0" },
                    "devDependencies": { "bar": "^2.0.0" }
                },
                "../../../abs/path/proj/node_modules/foo": {
                    "version": "1.2.3",
                    "integrity": "sha512-aaa",
                    "dependencies": { "nested": "^3.0.0" }
                },
                "../../../abs/path/proj/node_modules/nested": {
                    "version": "3.1.0",
                    "integrity": "sha512-bbb"
                },
                "../../../abs/path/proj/node_modules/bar": {
                    "version": "2.5.0",
                    "integrity": "sha512-ccc"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();

    // Package resolution (version + integrity) lands under the
    // canonical name@version key — what the bun writer needs.
    assert_eq!(graph.packages.len(), 3);
    assert_eq!(
        graph.packages["foo@1.2.3"].integrity.as_deref(),
        Some("sha512-aaa")
    );
    // Transitive resolution survives the climb-prefix walk too.
    assert_eq!(
        graph.packages["foo@1.2.3"]
            .dependencies
            .get("nested")
            .map(String::as_str),
        Some("3.1.0")
    );

    // Root importer direct deps resolve, carrying their dep_path and
    // declared specifier — what the pnpm writer needs.
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 2);
    let foo_dep = root.iter().find(|d| d.name == "foo").unwrap();
    assert_eq!(foo_dep.dep_path, "foo@1.2.3");
    assert_eq!(foo_dep.specifier.as_deref(), Some("^1.0.0"));
    let bar_dep = root.iter().find(|d| d.name == "bar").unwrap();
    assert_eq!(bar_dep.dep_type, DepType::Dev);
    assert_eq!(bar_dep.specifier.as_deref(), Some("^2.0.0"));
}

#[test]
fn test_parse_git_resolved_package() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sha = "abcdef1234567890abcdef1234567890abcdef12";
    let content = format!(
        r#"{{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 2,
            "packages": {{
                "": {{
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": {{ "git-only": "github:owner/repo#{sha}" }}
                }},
                "node_modules/git-only": {{
                    "version": "1.2.3",
                    "resolved": "git+ssh://git@github.com/owner/repo.git#{sha}",
                    "integrity": "sha512-aaa"
                }}
            }}
        }}"#
    );
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let root = &graph.importers["."];
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "git-only");
    assert!(!graph.packages.contains_key("git-only@1.2.3"));

    let pkg = &graph.packages[&root[0].dep_path];
    assert_eq!(pkg.name, "git-only");
    assert_eq!(pkg.version, "1.2.3");
    assert_eq!(pkg.integrity.as_deref(), Some("sha512-aaa"));
    assert!(pkg.tarball_url.is_none());

    let Some(LocalSource::Git(git)) = &pkg.local_source else {
        panic!("expected git local source, got {:?}", pkg.local_source);
    };
    assert_eq!(git.url, "ssh://git@github.com/owner/repo.git");
    assert_eq!(git.committish.as_deref(), Some(sha));
    assert_eq!(git.resolved, sha);
}

#[test]
fn test_unpinned_git_resolved_url_is_not_locked_git_source() {
    assert!(local_git_source_from_resolved("git+https://github.com/owner/repo.git").is_none());
}

#[test]
fn test_write_preserves_git_resolved_url() {
    let sha = "abcdef1234567890abcdef1234567890abcdef12";
    let mut graph = LockfileGraph::default();
    let local = LocalSource::Git(GitSource {
        url: "ssh://git@github.com/owner/repo.git".to_string(),
        committish: Some(sha.to_string()),
        resolved: sha.to_string(),
        integrity: None,
        subpath: None,
    });
    let dep_path = local.dep_path("git-only");
    graph.packages.insert(
        dep_path.clone(),
        LockedPackage {
            name: "git-only".to_string(),
            version: "1.2.3".to_string(),
            dep_path: dep_path.clone(),
            local_source: Some(local),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "git-only".to_string(),
            dep_path,
            dep_type: DepType::Production,
            specifier: Some(format!("github:owner/repo#{sha}")),
        }],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("git-only".to_string(), format!("github:owner/repo#{sha}"))]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let body = std::fs::read_to_string(out.path()).unwrap();
    assert!(
        body.contains(&format!(
            "\"resolved\": \"git+ssh://git@github.com/owner/repo.git#{sha}\""
        )),
        "expected git resolved URL emitted; got:\n{body}"
    );

    let reparsed = parse(out.path()).unwrap();
    let pkg = &reparsed.packages[&graph.importers["."][0].dep_path];
    assert!(matches!(pkg.local_source, Some(LocalSource::Git(_))));
}

// npm canonicalizes a hosted git dep's `resolved` to the provider's
// sshurl form no matter what protocol the spec used — `github:owner/
// repo#tag` and `git+https://github.com/owner/repo.git#tag` both land
// as `git+ssh://git@github.com/owner/repo.git#<sha>` (verified against
// npm 11.13.0). The resolver stores the https clone URL for the
// `github:` shorthand, so the writer must re-derive the canonical form
// or `npm install` rewrites the line on its first run.
#[test]
fn test_write_canonicalizes_hosted_git_resolved_to_sshurl() {
    let sha = "1c6264b795492e8fdecbc82cb8802fcfbfc08d26";
    let mut graph = LockfileGraph::default();
    let local = LocalSource::Git(GitSource {
        url: "https://github.com/vercel/ms.git".to_string(),
        committish: Some("2.1.3".to_string()),
        resolved: sha.to_string(),
        integrity: None,
        subpath: None,
    });
    let dep_path = local.dep_path("ms");
    graph.packages.insert(
        dep_path.clone(),
        LockedPackage {
            name: "ms".to_string(),
            version: "2.1.3".to_string(),
            dep_path: dep_path.clone(),
            local_source: Some(local),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "ms".to_string(),
            dep_path,
            dep_type: DepType::Production,
            specifier: Some("github:vercel/ms#2.1.3".to_string()),
        }],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("ms".to_string(), "github:vercel/ms#2.1.3".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let body = std::fs::read_to_string(out.path()).unwrap();
    assert!(
        body.contains(&format!(
            "\"resolved\": \"git+ssh://git@github.com/vercel/ms.git#{sha}\""
        )),
        "hosted git resolved URL must use npm's canonical sshurl form; got:\n{body}"
    );

    // A non-hosted git URL keeps its stored form — only the three
    // hosted providers get the sshurl identity.
    let self_hosted = LocalSource::Git(GitSource {
        url: "https://git.example.com/owner/repo.git".to_string(),
        committish: None,
        resolved: sha.to_string(),
        integrity: None,
        subpath: None,
    });
    let pkg = LockedPackage {
        name: "repo".to_string(),
        version: "1.0.0".to_string(),
        dep_path: self_hosted.dep_path("repo"),
        local_source: Some(self_hosted),
        ..Default::default()
    };
    assert_eq!(
        super::source::npm_resolved_field(&pkg).as_deref(),
        Some(format!("git+https://git.example.com/owner/repo.git#{sha}").as_str())
    );
}

// Upstream #857 reclassified github/gitlab/bitbucket-shorthand git deps
// to resolve via a codeload archive — `LocalSource::RemoteTarball { url:
// "https://codeload.github.com/<o>/<r>/tar.gz/<sha>", git_hosted: true }`
// rather than `LocalSource::Git`. The npm writer must still (a) PLACE the
// dep in the lockfile (the bug: it was dropped, so `npm ci` rejected the
// lock with `Missing: ms from lock file`) and (b) emit npm's canonical
// `git+ssh://…#<sha>` resolved field, NOT the codeload archive URL.
#[test]
fn test_write_places_git_hosted_codeload_dep_with_sshurl_resolved() {
    let sha = "1c6264b795492e8fdecbc82cb8802fcfbfc08d26";
    let codeload = format!("https://codeload.github.com/vercel/ms/tar.gz/{sha}");
    let local = LocalSource::RemoteTarball(crate::RemoteTarballSource {
        url: codeload.clone(),
        integrity: "sha512-deadbeef".to_string(),
        git_hosted: true,
    });
    let dep_path = local.dep_path("ms");
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        dep_path.clone(),
        LockedPackage {
            name: "ms".to_string(),
            version: "2.1.3".to_string(),
            dep_path: dep_path.clone(),
            tarball_url: Some(codeload),
            local_source: Some(local),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "ms".to_string(),
            dep_path,
            dep_type: DepType::Production,
            specifier: Some("github:vercel/ms#2.1.3".to_string()),
        }],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("ms".to_string(), "github:vercel/ms#2.1.3".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let body = std::fs::read_to_string(out.path()).unwrap();

    assert!(
        body.contains("\"node_modules/ms\""),
        "git-hosted codeload dep must be placed in the lockfile; got:\n{body}"
    );
    assert!(
        body.contains(&format!(
            "\"resolved\": \"git+ssh://git@github.com/vercel/ms.git#{sha}\""
        )),
        "git-hosted codeload dep must emit npm's canonical sshurl, not the codeload URL; got:\n{body}"
    );
    assert!(
        !body.contains("codeload.github.com"),
        "the codeload archive URL must NOT leak into the npm resolved field; got:\n{body}"
    );
}

// `npm_resolved_field` inverts each provider's codeload archive form back
// to the canonical sshurl. A plain (non-git) remote tarball, and a
// git-hosted URL whose shape isn't a recognized pinned archive, must NOT
// be turned into a git resolved field — they fall through to the
// `tarball_url`/None paths unchanged.
#[test]
fn test_npm_resolved_field_inverts_codeload_per_host() {
    let sha = "1c6264b795492e8fdecbc82cb8802fcfbfc08d26";
    let field = |url: &str, git_hosted: bool| {
        let local = LocalSource::RemoteTarball(crate::RemoteTarballSource {
            url: url.to_string(),
            integrity: String::new(),
            git_hosted,
        });
        let pkg = LockedPackage {
            name: "pkg".to_string(),
            version: "1.0.0".to_string(),
            dep_path: local.dep_path("pkg"),
            tarball_url: Some(url.to_string()),
            local_source: Some(local),
            ..Default::default()
        };
        super::source::npm_resolved_field(&pkg)
    };

    assert_eq!(
        field(
            &format!("https://codeload.github.com/vercel/ms/tar.gz/{sha}"),
            true
        )
        .as_deref(),
        Some(format!("git+ssh://git@github.com/vercel/ms.git#{sha}").as_str())
    );
    assert_eq!(
        field(
            &format!("https://gitlab.com/o/r/-/archive/{sha}/r-{sha}.tar.gz"),
            true
        )
        .as_deref(),
        Some(format!("git+ssh://git@gitlab.com/o/r.git#{sha}").as_str())
    );
    assert_eq!(
        field(&format!("https://bitbucket.org/o/r/get/{sha}.tar.gz"), true).as_deref(),
        Some(format!("git+ssh://git@bitbucket.org/o/r.git#{sha}").as_str())
    );

    // Non-git remote tarball: keep its tarball URL, never a git form.
    let plain = "https://registry.example.com/pkg/-/pkg-1.0.0.tgz";
    assert_eq!(field(plain, false).as_deref(), Some(plain));
    // git_hosted but unrecognized/unpinned shape: fall through, no git form.
    let weird = "https://codeload.github.com/o/r/zip/main";
    assert_eq!(field(weird, true).as_deref(), Some(weird));
}

/// A `file:` directory dep must serialize as npm's two-entry pair: a
/// `<path>: { name, version }` package record keyed by the on-disk
/// path, plus a `node_modules/<name>: { resolved: "<path>", link: true }`
/// link node. Emitting neither (the prior behavior) made `npm ci` reject
/// nub's lockfile with `Missing: <name>@<version> from lock file`, since
/// the root `dependencies` entry had no matching `packages` record.
#[test]
fn test_write_emits_file_dir_dep_as_link_pair() {
    let local = LocalSource::Directory(PathBuf::from("./local-pkg"));
    let dep_path = local.dep_path("local-utils");
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        dep_path.clone(),
        LockedPackage {
            name: "local-utils".to_string(),
            version: "1.0.0".to_string(),
            dep_path: dep_path.clone(),
            local_source: Some(local),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "local-utils".to_string(),
            dep_path,
            dep_type: DepType::Production,
            specifier: Some("file:./local-pkg".to_string()),
        }],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("local-utils".to_string(), "file:./local-pkg".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let body = std::fs::read_to_string(out.path()).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    let packages = &doc["packages"];

    // npm strips `file:` and the leading `./`, keying the metadata
    // entry by the bare path `local-pkg` with only name + version.
    assert_eq!(packages["local-pkg"]["name"], "local-utils");
    assert_eq!(packages["local-pkg"]["version"], "1.0.0");
    assert!(
        packages["local-pkg"].get("resolved").is_none(),
        "file: metadata entry carries no resolved field, got {}",
        packages["local-pkg"]
    );

    // The link node points back at that path.
    assert_eq!(
        packages["node_modules/local-utils"]["resolved"],
        "local-pkg"
    );
    assert_eq!(packages["node_modules/local-utils"]["link"], true);
}

#[test]
fn test_parse_file_resolved_without_link() {
    // npm writes `resolved: "file:..."` without `link: true` for
    // local tarball deps (`npm install file:../foo-1.0.0.tgz`) and
    // for some directory deps. Both shapes must surface as a
    // LocalSource so the resolver dispatches the local-source
    // branch and doesn't fall through to a registry fetch.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "dependencies": {
                        "tar-dep": "file:../utils/tar-dep-1.0.0.tgz",
                        "dir-dep": "file:../utils"
                    }
                },
                "node_modules/tar-dep": {
                    "version": "1.0.0",
                    "resolved": "file:../utils/tar-dep-1.0.0.tgz",
                    "integrity": "sha512-aaa"
                },
                "node_modules/dir-dep": {
                    "version": "1.0.0",
                    "resolved": "file:../utils"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();

    let tar_pkg = graph
        .packages
        .values()
        .find(|p| p.name == "tar-dep")
        .expect("tar-dep entry");
    assert!(
        matches!(&tar_pkg.local_source, Some(LocalSource::Tarball(p)) if p == Path::new("../utils/tar-dep-1.0.0.tgz")),
        "expected Tarball source, got {:?}",
        tar_pkg.local_source,
    );
    assert!(
        tar_pkg.dep_path.starts_with("tar-dep@file+"),
        "tarball dep_path should be local-source-keyed, got {}",
        tar_pkg.dep_path,
    );

    let dir_pkg = graph
        .packages
        .values()
        .find(|p| p.name == "dir-dep")
        .expect("dir-dep entry");
    assert!(
        matches!(&dir_pkg.local_source, Some(LocalSource::Directory(p)) if p == Path::new("../utils")),
        "expected Directory source, got {:?}",
        dir_pkg.local_source,
    );
    assert!(
        dir_pkg.dep_path.starts_with("dir-dep@file+"),
        "directory dep_path should be local-source-keyed, got {}",
        dir_pkg.dep_path,
    );

    let root = graph.importers.get(".").unwrap();
    let tar_direct = root.iter().find(|d| d.name == "tar-dep").unwrap();
    assert_eq!(tar_direct.dep_path, tar_pkg.dep_path);
    let dir_direct = root.iter().find(|d| d.name == "dir-dep").unwrap();
    assert_eq!(dir_direct.dep_path, dir_pkg.dep_path);
}

#[test]
fn test_parse_scoped_package() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "dependencies": { "@scope/pkg": "^1.0.0" }
                },
                "node_modules/@scope/pkg": {
                    "version": "1.0.0",
                    "integrity": "sha512-zzz"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    assert!(graph.packages.contains_key("@scope/pkg@1.0.0"));
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root[0].name, "@scope/pkg");
    assert_eq!(root[0].dep_path, "@scope/pkg@1.0.0");
}

#[test]
fn test_parse_multi_version_nested() {
    // bar exists at two versions: 2.0.0 hoisted to root, 1.0.0 nested under foo.
    // foo's transitive dep on bar must resolve to 1.0.0, not 2.0.0.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "dependencies": { "foo": "^1.0.0", "bar": "^2.0.0" }
                },
                "node_modules/bar": {
                    "version": "2.0.0",
                    "integrity": "sha512-top-bar"
                },
                "node_modules/foo": {
                    "version": "1.0.0",
                    "integrity": "sha512-foo",
                    "dependencies": { "bar": "^1.0.0" }
                },
                "node_modules/foo/node_modules/bar": {
                    "version": "1.0.0",
                    "integrity": "sha512-nested-bar"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    // Both versions of bar should be present.
    assert!(graph.packages.contains_key("bar@2.0.0"));
    assert!(graph.packages.contains_key("bar@1.0.0"));
    assert!(graph.packages.contains_key("foo@1.0.0"));

    // foo's transitive dep must point to the nested (1.0.0), not the hoisted (2.0.0).
    // Value is the dep_path tail (version) — see the `LockedPackage.dependencies` doc.
    let foo = &graph.packages["foo@1.0.0"];
    assert_eq!(
        foo.dependencies.get("bar").map(String::as_str),
        Some("1.0.0")
    );

    // Root's direct bar dep points to the hoisted 2.0.0.
    let root = graph.importers.get(".").unwrap();
    let root_bar = root.iter().find(|d| d.name == "bar").unwrap();
    assert_eq!(root_bar.dep_path, "bar@2.0.0");
}

/// Regression: a package reachable from both a dev root and
/// an optional root (but *not* from any production root) must
/// be written with `devOptional: true`, not with both `dev: true`
/// and `optional: true`. Emitting both trips `npm install
/// --omit=dev` (and `--omit=optional`) into dropping a package
/// the other chain still needs.
#[test]
fn test_write_dev_and_optional_reachable_uses_dev_optional() {
    let mut graph = LockfileGraph::default();
    let mk = |name: &str| LockedPackage {
        name: name.to_string(),
        version: "1.0.0".to_string(),
        integrity: Some(format!("sha512-{name}")),
        dep_path: format!("{name}@1.0.0"),
        dependencies: [("shared".to_string(), "1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    graph
        .packages
        .insert("dev-root@1.0.0".to_string(), mk("dev-root"));
    graph
        .packages
        .insert("opt-root@1.0.0".to_string(), mk("opt-root"));
    graph.packages.insert(
        "shared@1.0.0".to_string(),
        LockedPackage {
            name: "shared".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-shared".to_string()),
            dep_path: "shared@1.0.0".to_string(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "dev-root".to_string(),
                dep_path: "dev-root@1.0.0".to_string(),
                dep_type: DepType::Dev,
                specifier: None,
            },
            DirectDep {
                name: "opt-root".to_string(),
                dep_path: "opt-root@1.0.0".to_string(),
                dep_type: DepType::Optional,
                specifier: None,
            },
        ],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dev_dependencies: [("dev-root".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        optional_dependencies: [("opt-root".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };

    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();

    let shared = &json["packages"]["node_modules/shared"];
    assert_eq!(shared["devOptional"], true, "expected devOptional flag");
    assert!(
        shared.get("dev").is_none(),
        "must not emit dev: true alongside devOptional",
    );
    assert!(
        shared.get("optional").is_none(),
        "must not emit optional: true alongside devOptional",
    );

    // Roots themselves retain their specific flag.
    assert_eq!(json["packages"]["node_modules/dev-root"]["dev"], true);
    assert_eq!(json["packages"]["node_modules/opt-root"]["optional"], true);
}

/// npm's flags are path-based, and below the root the only typed edge
/// is a package's `optionalDependencies`: a production dep's optional
/// child is `optional: true` (verified against npm 11: chokidar ⇒
/// fsevents carries the flag), and a package reachable only via a dev
/// chain *and* a transitive-optional chain is `devOptional: true`
/// (arborist's calc-dep-flags: no pure-production path, but neither
/// "every path dev" nor "every path optional" holds).
#[test]
fn test_write_transitive_optional_edges_set_optional_and_dev_optional() {
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        "parent@1.0.0".to_string(),
        LockedPackage {
            name: "parent".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-parent".to_string()),
            dep_path: "parent@1.0.0".to_string(),
            dependencies: [("shared".to_string(), "1.0.0".to_string())]
                .into_iter()
                .collect(),
            optional_dependencies: [("shared".to_string(), "^1.0.0".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
    );
    graph.packages.insert(
        "shared@1.0.0".to_string(),
        LockedPackage {
            name: "shared".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-shared".to_string()),
            dep_path: "shared@1.0.0".to_string(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "parent".to_string(),
            dep_path: "parent@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );
    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("parent".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };

    // Production root → optional edge: every path to `shared` crosses an
    // optional edge ⇒ `optional: true` (the parent itself stays unflagged).
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
    assert_eq!(
        json["packages"]["node_modules/shared"]["optional"], true,
        "a production dep's optionalDependencies child must be optional"
    );
    assert!(
        json["packages"]["node_modules/parent"]
            .get("optional")
            .is_none()
    );

    // Add a dev root depending on `shared` directly: now no pure-production
    // path exists, but neither flag holds alone ⇒ `devOptional: true`.
    graph.packages.insert(
        "dev-root@1.0.0".to_string(),
        LockedPackage {
            name: "dev-root".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-dev".to_string()),
            dep_path: "dev-root@1.0.0".to_string(),
            dependencies: [("shared".to_string(), "1.0.0".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
    );
    graph.importers.get_mut(".").unwrap().push(DirectDep {
        name: "dev-root".to_string(),
        dep_path: "dev-root@1.0.0".to_string(),
        dep_type: DepType::Dev,
        specifier: None,
    });
    let manifest = aube_manifest::PackageJson {
        dev_dependencies: [("dev-root".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..manifest
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
    let shared = &json["packages"]["node_modules/shared"];
    assert_eq!(
        shared["devOptional"], true,
        "dev chain + transitive-optional chain must collapse to devOptional"
    );
    assert!(shared.get("dev").is_none() && shared.get("optional").is_none());
}

/// Regression: the npm writer must drop `dependencies` entries
/// whose target isn't in the canonical map. Platform-filtered
/// optionals and `ignoredOptionalDependencies` leave the parent's
/// declared `dependencies` map pointing at packages the resolver
/// already removed; emitting them anyway produces a lockfile
/// where `npm ci` sees a reference with no matching `packages`
/// entry and refuses to install. Must match the bun/yarn
/// writers, which already filter this way.
#[test]
fn test_write_filters_missing_canonical_deps() {
    let mut graph = LockfileGraph::default();
    // Root has one real package, `foo`, which declares a dep on
    // `ghost@1.0.0` — but `ghost` was filtered out of the graph
    // (e.g. a platform-gated optional). The canonical map won't
    // contain it.
    graph.packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-foo".to_string()),
            dep_path: "foo@1.0.0".to_string(),
            dependencies: [("ghost".to_string(), "1.0.0".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "foo".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );

    let manifest = test_manifest();
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    // Parse the raw JSON directly — the aube reparser tolerates
    // dangling references so we assert on the serialized shape.
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
    let foo_entry = &json["packages"]["node_modules/foo"];
    assert!(
        foo_entry
            .get("dependencies")
            .and_then(|d| d.get("ghost"))
            .is_none(),
        "writer emitted a ghost dep that has no packages entry: {foo_entry}",
    );
    // And there should be no node_modules/ghost entry at all.
    assert!(
        json["packages"].get("node_modules/ghost").is_none(),
        "writer hallucinated a ghost entry",
    );
}

/// Regression for the shadow-nesting bug: if an intermediate
/// ancestor carries the *wrong* version of a dep, Node's
/// runtime walk stops there and never reaches a correct entry
/// at root. The writer must nest a fresh entry inside the
/// current parent's own `node_modules` instead of assuming
/// hoisting is fine just because root happens to have the
/// right version.
///
/// Shape:
///   root → foo → baz, baz depends on bar@2.0.0
///   foo already pulled in bar@1.0.0 for a sibling, so bar@1.0.0
///     lives at node_modules/foo/node_modules/bar
///   root has bar@2.0.0 at node_modules/bar
///
///   When we walk baz's deps and get to bar@2.0.0, the nearest
///   ancestor hit is bar@1.0.0 (shadowing), not root. We must
///   place a fresh entry at
///   `node_modules/foo/node_modules/baz/node_modules/bar` so
///   Node resolves the right version.
#[test]
fn test_nested_shadow_forces_nested_placement() {
    // Build a graph by hand to control the dep order deterministically.
    let mut graph = LockfileGraph::default();
    let mk = |name: &str, version: &str, deps: &[(&str, &str)]| LockedPackage {
        name: name.to_string(),
        version: version.to_string(),
        integrity: Some(format!("sha512-{name}-{version}")),
        dep_path: format!("{name}@{version}"),
        dependencies: deps
            .iter()
            .map(|(n, v)| (n.to_string(), (*v).to_string()))
            .collect(),
        ..Default::default()
    };
    graph.packages.insert(
        "foo@1.0.0".to_string(),
        mk(
            "foo",
            "1.0.0",
            &[
                // foo pulls in bar@1.0.0 and baz@1.0.0 as siblings.
                ("bar", "1.0.0"),
                ("baz", "1.0.0"),
            ],
        ),
    );
    graph.packages.insert(
        "baz@1.0.0".to_string(),
        // baz wants bar@2.0.0, which matches the root version.
        mk("baz", "1.0.0", &[("bar", "2.0.0")]),
    );
    graph
        .packages
        .insert("bar@1.0.0".to_string(), mk("bar", "1.0.0", &[]));
    graph
        .packages
        .insert("bar@2.0.0".to_string(), mk("bar", "2.0.0", &[]));
    graph.importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
            DirectDep {
                name: "bar".to_string(),
                dep_path: "bar@2.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
        ],
    );

    let manifest = test_manifest();
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let reparsed = parse(out.path()).unwrap();

    // baz's transitive dep must resolve to bar@2.0.0, not the
    // shadowing bar@1.0.0 under foo. Value is the dep_path tail
    // (version) so the linker can recombine it with the dep name.
    let baz = &reparsed.packages["baz@1.0.0"];
    assert_eq!(
        baz.dependencies.get("bar").map(String::as_str),
        Some("2.0.0"),
        "baz's bar dep was shadowed by foo/bar@1.0.0 — shadow-nest fix regressed",
    );
}

#[test]
fn test_parse_npm_preserves_platform_optional_metadata() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "platform-optional-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "platform-optional-root",
                    "version": "1.0.0",
                    "dependencies": { "host": "file:host" }
                },
                "node_modules/host": {
                    "resolved": "host",
                    "link": true
                },
                "host": {
                    "name": "host",
                    "version": "1.0.0",
                    "optionalDependencies": { "native-win": "1.0.0" }
                },
                "node_modules/native-win": {
                    "version": "1.0.0",
                    "resolved": "https://registry.npmjs.org/native-win/-/native-win-1.0.0.tgz",
                    "integrity": "sha512-native",
                    "optional": true,
                    "os": ["win32"],
                    "cpu": ["x64"],
                    "libc": ["glibc"]
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let host_dep_path = &graph.importers["."][0].dep_path;
    let host = &graph.packages[host_dep_path];
    assert_eq!(
        host.dependencies.get("native-win").map(String::as_str),
        Some("1.0.0")
    );
    assert_eq!(
        host.optional_dependencies
            .get("native-win")
            .map(String::as_str),
        Some("1.0.0")
    );

    let native = &graph.packages["native-win@1.0.0"];
    assert_eq!(
        native.os.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["win32"]
    );
    assert_eq!(
        native.cpu.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["x64"]
    );
    assert_eq!(
        native.libc.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["glibc"]
    );
}

/// npm sometimes emits `os` / `cpu` / `libc` as scalar strings instead
/// of arrays (e.g. `sass-embedded-linux-arm@1.99.0` ships
/// `"libc": "glibc"`). Verbatim-roundtripped into package-lock.json,
/// the field stays scalar — accept both shapes the same way the
/// pnpm + bun parsers already do.
#[test]
fn parse_npm_package_platform_fields_accept_scalar_strings() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "scalar-platform-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "scalar-platform-root",
                    "version": "1.0.0",
                    "dependencies": { "sass-embedded-linux-arm": "1.99.0" }
                },
                "node_modules/sass-embedded-linux-arm": {
                    "version": "1.99.0",
                    "resolved": "https://registry.npmjs.org/sass-embedded-linux-arm/-/sass-embedded-linux-arm-1.99.0.tgz",
                    "integrity": "sha512-native",
                    "cpu": "arm",
                    "os": "linux",
                    "libc": "glibc"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let pkg = &graph.packages["sass-embedded-linux-arm@1.99.0"];
    assert_eq!(
        pkg.os.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["linux"]
    );
    assert_eq!(
        pkg.cpu.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["arm"]
    );
    assert_eq!(
        pkg.libc.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["glibc"]
    );
}

#[test]
fn test_write_npm_preserves_platform_optional_metadata() {
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        "host@1.0.0".to_string(),
        LockedPackage {
            name: "host".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-host".to_string()),
            dep_path: "host@1.0.0".to_string(),
            dependencies: [("native-win".to_string(), "1.0.0".to_string())]
                .into_iter()
                .collect(),
            optional_dependencies: [("native-win".to_string(), "1.0.0".to_string())]
                .into_iter()
                .collect(),
            declared_dependencies: [("native-win".to_string(), "1.0.0".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
    );
    graph.packages.insert(
        "native-win@1.0.0".to_string(),
        LockedPackage {
            name: "native-win".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-native".to_string()),
            dep_path: "native-win@1.0.0".to_string(),
            os: vec!["win32".to_string()].into(),
            cpu: vec!["x64".to_string()].into(),
            libc: vec!["glibc".to_string()].into(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "host".to_string(),
            dep_path: "host@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("1.0.0".to_string()),
        }],
    );
    let manifest = aube_manifest::PackageJson {
        name: Some("platform-optional-root".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("host".to_string(), "1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };

    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();

    let host = &json["packages"]["node_modules/host"];
    assert_eq!(host["optionalDependencies"]["native-win"], "1.0.0");
    assert!(
        host.get("dependencies")
            .and_then(|deps| deps.get("native-win"))
            .is_none(),
        "optional child must not be duplicated as a required dependency: {host}",
    );

    let native = &json["packages"]["node_modules/native-win"];
    assert_eq!(native["os"], serde_json::json!(["win32"]));
    assert_eq!(native["cpu"], serde_json::json!(["x64"]));
    assert_eq!(native["libc"], serde_json::json!(["glibc"]));

    let reparsed = parse(out.path()).unwrap();
    let host = &reparsed.packages["host@1.0.0"];
    assert_eq!(
        host.optional_dependencies
            .get("native-win")
            .map(String::as_str),
        Some("1.0.0")
    );
    assert_eq!(
        host.dependencies.get("native-win").map(String::as_str),
        Some("1.0.0")
    );
    let native = &reparsed.packages["native-win@1.0.0"];
    assert_eq!(
        native.os.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["win32"]
    );
    assert_eq!(
        native.cpu.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["x64"]
    );
    assert_eq!(
        native.libc.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["glibc"]
    );
}

/// Regression: `canonical_key_from_dep_path` must strip the
/// peer-context suffix *before* splitting on `@`. A naive
/// `rfind('@')` lands inside the peer suffix and returns the
/// input unchanged, which silently drops every peer-contextualized
/// root dep from the written lockfile. The capped hash form
/// `(<short-hash>)` shares the same canonical identity and is
/// stripped the same way — it also begins at the first `(`.
#[test]
fn test_canonical_key_strips_peer_suffix() {
    assert_eq!(canonical_key_from_dep_path("foo@1.0.0"), "foo@1.0.0");
    assert_eq!(
        canonical_key_from_dep_path("styled-components@6.1.0(react@18.2.0)"),
        "styled-components@6.1.0"
    );
    assert_eq!(
        canonical_key_from_dep_path("@scope/pkg@2.0.0(peer@1.0.0)"),
        "@scope/pkg@2.0.0"
    );
    // Capped suffix: a single parenthesized short hash (pnpm's
    // `createPeerDepGraphHash`), stripped like any other `(…)` tail.
    let hashed = "expo-router@4.0.22(94c00fd028a1b2c3d4e5f60718293a4b)";
    assert_eq!(canonical_key_from_dep_path(hashed), "expo-router@4.0.22");
    assert_eq!(
        child_canonical_key("expo-router", "4.0.22(94c00fd028a1b2c3d4e5f60718293a4b)"),
        "expo-router@4.0.22"
    );
    assert_eq!(
        dep_value_as_version(
            "expo-router",
            "expo-router@4.0.22(94c00fd028a1b2c3d4e5f60718293a4b)"
        ),
        "4.0.22"
    );
}

fn test_manifest() -> aube_manifest::PackageJson {
    aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [
            ("foo".to_string(), "^1.0.0".to_string()),
            ("bar".to_string(), "^2.0.0".to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    }
}

/// Parse a fixture, write it back, re-parse: the resulting graph
/// must have the same packages, direct deps, and integrity hashes.
/// Catches silent data loss in the hoist/nest walk.
#[test]
fn test_write_roundtrip_multi_version() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "foo": "^1.0.0", "bar": "^2.0.0" }
                },
                "node_modules/bar": {
                    "version": "2.0.0",
                    "integrity": "sha512-top-bar"
                },
                "node_modules/foo": {
                    "version": "1.0.0",
                    "integrity": "sha512-foo",
                    "dependencies": { "bar": "^1.0.0" }
                },
                "node_modules/foo/node_modules/bar": {
                    "version": "1.0.0",
                    "integrity": "sha512-nested-bar"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let manifest = test_manifest();

    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let reparsed = parse(out.path()).unwrap();

    // Both versions of bar survived the round-trip.
    assert!(reparsed.packages.contains_key("bar@1.0.0"));
    assert!(reparsed.packages.contains_key("bar@2.0.0"));
    assert!(reparsed.packages.contains_key("foo@1.0.0"));
    assert_eq!(
        reparsed.packages["bar@2.0.0"].integrity.as_deref(),
        Some("sha512-top-bar")
    );
    assert_eq!(
        reparsed.packages["bar@1.0.0"].integrity.as_deref(),
        Some("sha512-nested-bar")
    );
    // foo's nested bar dep still resolves to 1.0.0, not the
    // hoisted 2.0.0. If the writer failed to nest, reparse would
    // snap this to bar@2.0.0. Value is the dep_path tail.
    assert_eq!(
        reparsed.packages["foo@1.0.0"]
            .dependencies
            .get("bar")
            .map(String::as_str),
        Some("1.0.0")
    );
}

/// Dev-only and optional-only packages get the right flags after
/// round-trip so `npm install --omit=dev` on the written file
/// does the right thing.
#[test]
fn test_write_dev_optional_flags() {
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-foo".to_string()),
            dep_path: "foo@1.0.0".to_string(),
            ..Default::default()
        },
    );
    graph.packages.insert(
        "devdep@1.0.0".to_string(),
        LockedPackage {
            name: "devdep".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-dev".to_string()),
            dep_path: "devdep@1.0.0".to_string(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
            DirectDep {
                name: "devdep".to_string(),
                dep_path: "devdep@1.0.0".to_string(),
                dep_type: DepType::Dev,
                specifier: None,
            },
        ],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("foo".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        dev_dependencies: [("devdep".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };

    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
    let packages = &json["packages"];
    assert_eq!(packages["node_modules/devdep"]["dev"], true);
    // Prod dep should have no dev field (skipped when false).
    assert!(packages["node_modules/foo"].get("dev").is_none());
}

#[test]
fn test_reject_v1() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "lockfileVersion": 1,
            "dependencies": {}
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let err = parse(tmp.path()).unwrap_err();
    assert!(matches!(err, Error::Parse(_, msg) if msg.contains("lockfileVersion 1")));
}

/// Pre-npm-2.x packages (e.g. `ansi-html-community@0.0.8`) ship
/// `"engines": ["node >= 0.8.0"]` as an array; npm preserves that
/// shape verbatim in v2/v3 lockfiles. Without tolerant parsing, a
/// single such entry blows up the whole `aube ci`. Normalize to an
/// empty map (matches what modern npm does for engine-strict on
/// the array shape) so the install proceeds.
#[test]
fn test_parse_legacy_array_engines() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "ansi-html-community": "0.0.8" }
                },
                "node_modules/ansi-html-community": {
                    "version": "0.0.8",
                    "integrity": "sha512-aaa",
                    "engines": ["node >= 0.8.0"]
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let pkg = &graph.packages["ansi-html-community@0.0.8"];
    // Array shape gets normalized to an empty map — same as the
    // manifest parser, and same as what modern npm honors for the
    // engine-strict check on the array form.
    assert!(pkg.engines.is_empty());
}

/// npm writes `"h3-v2": "npm:h3@..."` aliases as a packages entry
/// at `node_modules/h3-v2` with `name: "h3"` and the real registry
/// `resolved:` URL. Aube keys the graph on the *alias* (so
/// `node_modules/h3-v2` ends up at `.aube/h3-v2@.../node_modules/h3-v2`)
/// but remembers the real package name in `alias_of` so fetches
/// and store-index lookups use the URL that actually exists.
#[test]
fn test_parse_npm_alias_dependency() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "h3-v2": "npm:h3@2.0.1-rc.20" }
                },
                "node_modules/h3-v2": {
                    "name": "h3",
                    "version": "2.0.1-rc.20",
                    "resolved": "https://registry.npmjs.org/h3/-/h3-2.0.1-rc.20.tgz",
                    "integrity": "sha512-aliased"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    assert_eq!(graph.packages.len(), 1);
    // Graph key and LockedPackage.name both carry the alias —
    // that's what consumers (and the linker's folder-name logic)
    // refer to when they say "h3-v2".
    let pkg = graph
        .packages
        .get("h3-v2@2.0.1-rc.20")
        .expect("aliased entry should be keyed by the alias dep_path");
    assert_eq!(pkg.name, "h3-v2");
    assert_eq!(pkg.version, "2.0.1-rc.20");
    assert_eq!(pkg.alias_of.as_deref(), Some("h3"));
    assert_eq!(pkg.registry_name(), "h3");
    // `resolved:` round-trips into `tarball_url` so the fetcher
    // skips re-deriving from the alias-qualified name (which
    // would 404 the registry).
    assert_eq!(
        pkg.tarball_url.as_deref(),
        Some("https://registry.npmjs.org/h3/-/h3-2.0.1-rc.20.tgz")
    );

    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "h3-v2");
    assert_eq!(root[0].dep_path, "h3-v2@2.0.1-rc.20");
}

/// Non-aliased entries (the common case) leave `alias_of` unset
/// and `registry_name()` degenerates to `name`. Regression guard
/// against over-aggressive alias detection that would flag every
/// entry carrying an explicit `name:` field (npm sometimes emits
/// one for non-aliased roots too).
#[test]
fn test_parse_non_alias_preserves_empty_alias_of() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "foo": "^1.0.0" }
                },
                "node_modules/foo": {
                    "name": "foo",
                    "version": "1.2.3",
                    "integrity": "sha512-foo"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let pkg = &graph.packages["foo@1.2.3"];
    assert_eq!(pkg.name, "foo");
    assert!(pkg.alias_of.is_none());
    assert_eq!(pkg.registry_name(), "foo");
    assert!(pkg.tarball_url.is_none());
}

/// Round-trip: writer must emit `name:` and `resolved:` for the
/// aliased entry so a subsequent `parse()` still recognizes it as
/// an alias. Without both fields the re-parser would see
/// `node_modules/h3-v2` with no `name:` and treat it as a plain
/// package called `h3-v2` — which doesn't exist on the registry.
#[test]
fn test_write_roundtrip_npm_alias() {
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        "h3-v2@2.0.1-rc.20".to_string(),
        LockedPackage {
            name: "h3-v2".to_string(),
            version: "2.0.1-rc.20".to_string(),
            integrity: Some("sha512-aliased".to_string()),
            dep_path: "h3-v2@2.0.1-rc.20".to_string(),
            alias_of: Some("h3".to_string()),
            tarball_url: Some("https://registry.npmjs.org/h3/-/h3-2.0.1-rc.20.tgz".to_string()),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "h3-v2".to_string(),
            dep_path: "h3-v2@2.0.1-rc.20".to_string(),
            dep_type: DepType::Production,
            specifier: Some("npm:h3@2.0.1-rc.20".to_string()),
        }],
    );

    let manifest = test_manifest();
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let body = std::fs::read_to_string(out.path()).unwrap();
    assert!(
        body.contains("\"name\": \"h3\""),
        "expected `name: h3` emitted for aliased entry; got:\n{body}"
    );
    assert!(
        body.contains("\"resolved\": \"https://registry.npmjs.org/h3/-/h3-2.0.1-rc.20.tgz\""),
        "expected `resolved:` URL emitted for aliased entry; got:\n{body}"
    );

    let reparsed = parse(out.path()).unwrap();
    let pkg = &reparsed.packages["h3-v2@2.0.1-rc.20"];
    assert_eq!(pkg.alias_of.as_deref(), Some("h3"));
    assert_eq!(pkg.registry_name(), "h3");
}

/// npm v7+ writes `peerDependencies` / `peerDependenciesMeta` onto
/// every package entry. The parser must populate the matching
/// `LockedPackage` fields so the resolver's `apply_peer_contexts`
/// pass (run on npm-lockfile installs to wire peer siblings in the
/// isolated virtual store) actually has peer info to work with.
/// Before this parser change, peer-dependent packages like
/// `@tanstack/devtools-vite` would install without a sibling
/// `vite` link and die at runtime.
#[test]
fn test_parse_peer_dependencies() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "peer-test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "peer-test",
                    "version": "1.0.0",
                    "dependencies": { "devtools-vite": "0.6.0", "vite": "8.0.0" }
                },
                "node_modules/devtools-vite": {
                    "version": "0.6.0",
                    "integrity": "sha512-a",
                    "peerDependencies": {
                        "vite": "^6.0.0 || ^7.0.0 || ^8.0.0"
                    },
                    "peerDependenciesMeta": {
                        "vite": { "optional": false }
                    }
                },
                "node_modules/vite": {
                    "version": "8.0.0",
                    "integrity": "sha512-b"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let devtools = &graph.packages["devtools-vite@0.6.0"];
    assert_eq!(
        devtools.peer_dependencies.get("vite").map(String::as_str),
        Some("^6.0.0 || ^7.0.0 || ^8.0.0")
    );
    assert_eq!(
        devtools
            .peer_dependencies_meta
            .get("vite")
            .map(|m| m.optional),
        Some(false)
    );
}

/// Packages without peer fields keep both maps empty — guard
/// against accidental defaulting to `optional: true` or spurious
/// keys showing up in the LockedPackage from serde leak paths.
#[test]
fn test_parse_no_peer_fields_stays_empty() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "no-peers",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "no-peers", "version": "1.0.0", "dependencies": { "foo": "1.0.0" } },
                "node_modules/foo": { "version": "1.0.0", "integrity": "sha512-x" }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let foo = &graph.packages["foo@1.0.0"];
    assert!(foo.peer_dependencies.is_empty());
    assert!(foo.peer_dependencies_meta.is_empty());
}

/// Writer round-trips `peerDependencies` so a second `parse()` on
/// the rewritten lockfile still feeds the peer-context pass. The
/// install path writes out the lockfile after every install; if
/// peers vanished on the first write-back, the *next* install
/// would ship without peer siblings again.
#[test]
fn test_write_roundtrip_peer_dependencies() {
    let mut graph = LockfileGraph::default();
    let mut peer_deps = BTreeMap::new();
    peer_deps.insert("vite".to_string(), "^6.0.0 || ^7.0.0 || ^8.0.0".to_string());
    // Include an `optional: true` entry so the round-trip covers
    // `peerDependenciesMeta` — without it, the writer's meta
    // block isn't exercised and the round-trip would silently
    // re-flag the peer as required on every subsequent install
    // (see `hoist_auto_installed_peers` + `detect_unmet_peers`,
    // which key off `optional`).
    let mut peer_deps_meta = BTreeMap::new();
    peer_deps_meta.insert("vite".to_string(), crate::PeerDepMeta { optional: true });
    graph.packages.insert(
        "devtools-vite@0.6.0".to_string(),
        LockedPackage {
            name: "devtools-vite".to_string(),
            version: "0.6.0".to_string(),
            integrity: Some("sha512-a".to_string()),
            dep_path: "devtools-vite@0.6.0".to_string(),
            peer_dependencies: peer_deps,
            peer_dependencies_meta: peer_deps_meta,
            ..Default::default()
        },
    );
    graph.packages.insert(
        "vite@8.0.0".to_string(),
        LockedPackage {
            name: "vite".to_string(),
            version: "8.0.0".to_string(),
            integrity: Some("sha512-b".to_string()),
            dep_path: "vite@8.0.0".to_string(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "devtools-vite".to_string(),
                dep_path: "devtools-vite@0.6.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
            DirectDep {
                name: "vite".to_string(),
                dep_path: "vite@8.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
        ],
    );

    let manifest = test_manifest();
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let body = std::fs::read_to_string(out.path()).unwrap();
    assert!(
        body.contains("\"peerDependencies\""),
        "expected peerDependencies block to round-trip; got:\n{body}"
    );
    assert!(
        body.contains("\"peerDependenciesMeta\""),
        "expected peerDependenciesMeta block to round-trip; got:\n{body}"
    );

    let reparsed = parse(out.path()).unwrap();
    let devtools = &reparsed.packages["devtools-vite@0.6.0"];
    assert_eq!(
        devtools.peer_dependencies.get("vite").map(String::as_str),
        Some("^6.0.0 || ^7.0.0 || ^8.0.0")
    );
    assert_eq!(
        devtools
            .peer_dependencies_meta
            .get("vite")
            .map(|m| m.optional),
        Some(true),
        "peerDependenciesMeta.optional must survive write → parse round-trip"
    );
}

#[test]
fn test_parse_npm_workspace_importers() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "workspace-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "workspace-root",
                    "version": "1.0.0",
                    "workspaces": ["web"]
                },
                "node_modules/mise-versions-web": {
                    "resolved": "web",
                    "link": true
                },
                "web": {
                    "name": "mise-versions-web",
                    "version": "0.0.1",
                    "dependencies": { "astro": "^6.0.0" },
                    "devDependencies": { "vite": "^7.3.2" }
                },
                "web/node_modules/astro": {
                    "version": "6.2.1",
                    "integrity": "sha512-astro"
                },
                "web/node_modules/vite": {
                    "version": "7.3.2",
                    "integrity": "sha512-vite"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let root = graph.importers.get(".").expect("root importer");
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "mise-versions-web");
    assert!(matches!(
        graph.packages[&root[0].dep_path].local_source,
        Some(LocalSource::Link(_))
    ));

    let web = graph.importers.get("web").expect("web importer");
    assert_eq!(web.len(), 2);
    assert!(web.iter().any(|dep| {
        dep.name == "astro"
            && dep.dep_type == DepType::Production
            && dep.specifier.as_deref() == Some("^6.0.0")
    }));
    assert!(web.iter().any(|dep| {
        dep.name == "vite"
            && dep.dep_type == DepType::Dev
            && dep.specifier.as_deref() == Some("^7.3.2")
    }));
}

#[test]
fn test_parse_npm_workspace_importer_keeps_nested_conflicting_direct_dep() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "workspace-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "workspace-root",
                    "version": "1.0.0",
                    "workspaces": ["packages/*"],
                    "dependencies": { "commander": "^5.0.0" }
                },
                "node_modules/commander": {
                    "version": "5.1.0",
                    "integrity": "sha512-commander5"
                },
                "node_modules/tempo": {
                    "resolved": "packages/cli",
                    "link": true
                },
                "packages/cli": {
                    "name": "tempo",
                    "version": "1.0.0",
                    "dependencies": { "commander": "^12.1.0" }
                },
                "packages/cli/node_modules/commander": {
                    "version": "12.1.0",
                    "integrity": "sha512-commander12"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let root_commander = graph.importers["."]
        .iter()
        .find(|dep| dep.name == "commander")
        .expect("root commander direct dep");
    assert_eq!(root_commander.dep_path, "commander@5.1.0");
    assert_eq!(root_commander.specifier.as_deref(), Some("^5.0.0"));

    let cli_commander = graph.importers["packages/cli"]
        .iter()
        .find(|dep| dep.name == "commander")
        .expect("workspace commander direct dep");
    assert_eq!(cli_commander.dep_path, "commander@12.1.0");
    assert_eq!(cli_commander.specifier.as_deref(), Some("^12.1.0"));
}

#[test]
fn test_write_npm_workspace_importers() {
    let mut graph = LockfileGraph::default();
    let web_link = LocalSource::Link(PathBuf::from("web"));
    let web_dep_path = web_link.dep_path("mise-versions-web");
    graph.packages.insert(
        web_dep_path.clone(),
        LockedPackage {
            name: "mise-versions-web".to_string(),
            version: "0.0.1".to_string(),
            dep_path: web_dep_path.clone(),
            local_source: Some(web_link),
            ..Default::default()
        },
    );
    graph.packages.insert(
        "astro@6.2.1".to_string(),
        LockedPackage {
            name: "astro".to_string(),
            version: "6.2.1".to_string(),
            integrity: Some("sha512-astro".to_string()),
            dep_path: "astro@6.2.1".to_string(),
            ..Default::default()
        },
    );
    graph.packages.insert(
        "vite@7.3.2".to_string(),
        LockedPackage {
            name: "vite".to_string(),
            version: "7.3.2".to_string(),
            integrity: Some("sha512-vite".to_string()),
            dep_path: "vite@7.3.2".to_string(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "mise-versions-web".to_string(),
            dep_path: web_dep_path.clone(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );
    graph.importers.insert(
        "web".to_string(),
        vec![
            DirectDep {
                name: "astro".to_string(),
                dep_path: "astro@6.2.1".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^6.0.0".to_string()),
            },
            DirectDep {
                name: "vite".to_string(),
                dep_path: "vite@7.3.2".to_string(),
                dep_type: DepType::Dev,
                specifier: Some("^7.3.2".to_string()),
            },
        ],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("workspace-root".to_string()),
        version: Some("1.0.0".to_string()),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
    assert_eq!(
        json["packages"]["node_modules/mise-versions-web"]["link"],
        true
    );
    assert_eq!(
        json["packages"]["node_modules/mise-versions-web"]["resolved"],
        "web"
    );
    assert_eq!(json["packages"]["web"]["dependencies"]["astro"], "^6.0.0");
    assert_eq!(json["packages"]["web"]["devDependencies"]["vite"], "^7.3.2");
    assert_eq!(
        json["packages"]["web/node_modules/astro"]["version"],
        "6.2.1"
    );
    assert_eq!(
        json["packages"]["web/node_modules/vite"]["version"],
        "7.3.2"
    );

    let reparsed = parse(out.path()).unwrap();
    assert!(reparsed.importers.contains_key("web"));
}

/// When the root tree already hoists a package to
/// `node_modules/<name>`, the workspace tree must NOT emit a
/// redundant `<workspace>/node_modules/<name>` for the same
/// version — Node's upward `node_modules` walk resolves the root
/// copy. Real `npm install` omits the redundant entry, and
/// emitting it produces a diff on every round-trip.
#[test]
fn test_write_npm_workspace_skips_root_hoisted_dups() {
    let mut graph = LockfileGraph::default();
    let web_link = LocalSource::Link(PathBuf::from("web"));
    let web_dep_path = web_link.dep_path("workspace-web");
    graph.packages.insert(
        web_dep_path.clone(),
        LockedPackage {
            name: "workspace-web".to_string(),
            version: "0.0.1".to_string(),
            dep_path: web_dep_path.clone(),
            local_source: Some(web_link),
            ..Default::default()
        },
    );
    graph.packages.insert(
        "astro@6.2.1".to_string(),
        LockedPackage {
            name: "astro".to_string(),
            version: "6.2.1".to_string(),
            integrity: Some("sha512-astro".to_string()),
            dep_path: "astro@6.2.1".to_string(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "astro".to_string(),
                dep_path: "astro@6.2.1".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^6.0.0".to_string()),
            },
            DirectDep {
                name: "workspace-web".to_string(),
                dep_path: web_dep_path.clone(),
                dep_type: DepType::Production,
                specifier: None,
            },
        ],
    );
    graph.importers.insert(
        "web".to_string(),
        vec![DirectDep {
            name: "astro".to_string(),
            dep_path: "astro@6.2.1".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^6.0.0".to_string()),
        }],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("workspace-root".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("astro".to_string(), "^6.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
    assert_eq!(json["packages"]["node_modules/astro"]["version"], "6.2.1");
    assert!(
        json["packages"].get("web/node_modules/astro").is_none(),
        "redundant workspace-nested astro should not be emitted"
    );
}

/// Byte-parity with a real `npm install`-generated lockfile. The
/// fixture at `tests/fixtures/npm-native.json` was produced by
/// `npm install` (v11) against a `{ chalk, picocolors, semver }`
/// manifest. A parse → write round-trip must reproduce the exact
/// bytes. Covers `resolved:` on every entry, `license:` /
/// `engines:` / `bin:` / `funding:` field preservation, and the
/// sibling declared-range preservation that rides on
/// `declared_dependencies`.
#[test]
fn test_write_byte_identical_to_native_npm() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/npm-native.json");
    // Same LF normalization as the pnpm / bun byte-parity tests —
    // Windows' `core.autocrlf=true` rewrites the checked-out
    // fixture to CRLF even with `.gitattributes eol=lf`.
    let original = std::fs::read_to_string(&fixture)
        .unwrap()
        .replace("\r\n", "\n");
    let graph = parse(&fixture).unwrap();
    let manifest = aube_manifest::PackageJson {
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

    let tmp = tempfile::NamedTempFile::new().unwrap();
    write(tmp.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(tmp.path()).unwrap();

    if written != original {
        panic!(
            "npm writer drifted from native npm output.\n\n--- expected ---\n{original}\n--- got ---\n{written}"
        );
    }
}

#[test]
fn test_parse_workspace_links() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "workspace-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "workspace-root",
                    "version": "1.0.0",
                    "dependencies": { "@scope/app": "file:packages/app" }
                },
                "node_modules/@scope/app": {
                    "resolved": "packages/app",
                    "link": true
                },
                "node_modules/chalk": {
                    "version": "5.4.1",
                    "integrity": "sha512-chalk"
                },
                "packages/app": {
                    "name": "@scope/app",
                    "version": "0.68.1",
                    "dependencies": {
                        "chalk": "^5.4.1"
                    }
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let dep_path = LocalSource::Link(PathBuf::from("packages/app")).dep_path("@scope/app");

    let importer = &graph.importers["."];
    assert_eq!(importer.len(), 1);
    assert_eq!(importer[0].name, "@scope/app");
    assert_eq!(importer[0].dep_path, dep_path);
    assert!(matches!(importer[0].dep_type, DepType::Production));
    // The declared range from the root entry's `dependencies` value is
    // now carried through as the importer specifier (a `file:` workspace
    // link declares `file:packages/app`).
    assert_eq!(importer[0].specifier.as_deref(), Some("file:packages/app"));

    let app = &graph.packages[&importer[0].dep_path];
    assert_eq!(app.version, "0.68.1");
    assert_eq!(
        app.local_source,
        Some(LocalSource::Link(PathBuf::from("packages/app")))
    );
    assert_eq!(
        app.dependencies.get("chalk").map(String::as_str),
        Some("5.4.1")
    );
    assert!(!graph.packages.contains_key("@scope/app@0.68.1"));
}

/// npm workspaces that aren't listed in the root manifest's
/// `dependencies`/`devDependencies` still get a `node_modules/<name>`
/// link entry in the lockfile — npm symlinks every workspace member
/// at the workspace root regardless. The siemens/element repo
/// (https://github.com/siemens/element) hits this: its 11 workspace
/// projects under `projects/*` aren't declared as deps of the root
/// `package.json`, so the linker had nothing to link at the root and
/// `node_modules/@siemens/element-ng` (and friends) silently went
/// missing — breaking Angular CLI builds that resolve workspace
/// libraries from the repo root.
#[test]
fn test_parse_workspace_links_undeclared_in_root_deps() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "workspace-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "workspace-root",
                    "version": "1.0.0",
                    "workspaces": ["projects/element-ng", "projects/charts-ng"],
                    "dependencies": { "chalk": "^5.4.1" }
                },
                "node_modules/@siemens/element-ng": {
                    "resolved": "projects/element-ng",
                    "link": true
                },
                "node_modules/@siemens/charts-ng": {
                    "resolved": "projects/charts-ng",
                    "link": true
                },
                "node_modules/chalk": {
                    "version": "5.4.1",
                    "integrity": "sha512-chalk"
                },
                "projects/element-ng": {
                    "name": "@siemens/element-ng",
                    "version": "21.0.0"
                },
                "projects/charts-ng": {
                    "name": "@siemens/charts-ng",
                    "version": "21.0.0"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let importer = &graph.importers["."];

    let names: Vec<&str> = importer.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"chalk"));
    assert!(
        names.contains(&"@siemens/element-ng"),
        "workspace package `@siemens/element-ng` should be a direct dep of root \
             so the linker creates `node_modules/@siemens/element-ng`, even though \
             the root manifest doesn't list it; got importer deps {names:?}"
    );
    assert!(
        names.contains(&"@siemens/charts-ng"),
        "workspace package `@siemens/charts-ng` should be a direct dep of root; \
             got importer deps {names:?}"
    );

    // Each workspace dep_path round-trips through LocalSource::Link.
    let element_ng = importer
        .iter()
        .find(|d| d.name == "@siemens/element-ng")
        .unwrap();
    assert_eq!(
        graph.packages[&element_ng.dep_path].local_source,
        Some(LocalSource::Link(PathBuf::from("projects/element-ng")))
    );
}

/// npm copies `funding:` verbatim from each package's
/// `package.json`, so all three registry-permitted shapes (bare
/// string, `{url}` object, mixed array of either) appear in real
/// lockfiles. The pre-fix parser only accepted the object form
/// and would hard-fail on any project pulling in `htmlparser2`,
/// `@csstools/*`, etc. Aube only carries one URL per package, so
/// the contract is "first URL wins, no shape rejected".
#[test]
fn test_parse_funding_all_shapes() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": {
                        "string-funding": "1.0.0",
                        "object-funding": "1.0.0",
                        "array-funding": "1.0.0",
                        "mixed-array-funding": "1.0.0",
                        "no-funding": "1.0.0"
                    }
                },
                "node_modules/string-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-aaa",
                    "funding": "https://example.com/sponsor"
                },
                "node_modules/object-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-bbb",
                    "funding": { "type": "github", "url": "https://github.com/sponsors/foo" }
                },
                "node_modules/array-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-ccc",
                    "funding": [
                        { "type": "github", "url": "https://github.com/sponsors/csstools" },
                        { "type": "opencollective", "url": "https://opencollective.com/csstools" }
                    ]
                },
                "node_modules/mixed-array-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-ddd",
                    "funding": [
                        "https://github.com/fb55/htmlparser2?sponsor=1",
                        { "type": "github", "url": "https://github.com/sponsors/fb55" }
                    ]
                },
                "node_modules/no-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-eee"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    assert_eq!(
        graph.packages["string-funding@1.0.0"]
            .funding_url
            .as_deref(),
        Some("https://example.com/sponsor"),
    );
    assert_eq!(
        graph.packages["object-funding@1.0.0"]
            .funding_url
            .as_deref(),
        Some("https://github.com/sponsors/foo"),
    );
    // Array form: aube collapses to the first URL.
    assert_eq!(
        graph.packages["array-funding@1.0.0"].funding_url.as_deref(),
        Some("https://github.com/sponsors/csstools"),
    );
    // Mixed array (bare string + object): first element is a
    // string, so its value is the URL.
    assert_eq!(
        graph.packages["mixed-array-funding@1.0.0"]
            .funding_url
            .as_deref(),
        Some("https://github.com/fb55/htmlparser2?sponsor=1"),
    );
    assert!(graph.packages["no-funding@1.0.0"].funding_url.is_none());
}

/// Real-world `package-lock.json` entries can carry the legacy
/// object / array-of-objects shapes for `license:` (npm copies
/// whatever's in the package's `package.json` verbatim, and older
/// packages like `tv4` still ship the deprecated forms). Regression
/// guard for https://github.com/jdx/aube/discussions/510.
#[test]
fn test_parse_license_all_shapes() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": {
                        "string-license": "1.0.0",
                        "object-license": "1.0.0",
                        "array-license": "1.0.0",
                        "mixed-array-license": "1.0.0",
                        "no-license": "1.0.0"
                    }
                },
                "node_modules/string-license": {
                    "version": "1.0.0",
                    "integrity": "sha512-aaa",
                    "license": "MIT"
                },
                "node_modules/object-license": {
                    "version": "1.0.0",
                    "integrity": "sha512-bbb",
                    "license": { "type": "ISC", "url": "https://example.com/ISC" }
                },
                "node_modules/array-license": {
                    "version": "1.0.0",
                    "integrity": "sha512-ccc",
                    "license": [
                        { "type": "Public Domain", "url": "http://geraintluff.github.io/tv4/LICENSE.txt" },
                        { "type": "MIT", "url": "http://jsonary.com/LICENSE.txt" }
                    ]
                },
                "node_modules/mixed-array-license": {
                    "version": "1.0.0",
                    "integrity": "sha512-ddd",
                    "license": [
                        "MIT",
                        { "type": "Apache-2.0", "url": "https://example.com/apache" }
                    ]
                },
                "node_modules/no-license": {
                    "version": "1.0.0",
                    "integrity": "sha512-eee"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    assert_eq!(
        graph.packages["string-license@1.0.0"].license.as_deref(),
        Some("MIT"),
    );
    assert_eq!(
        graph.packages["object-license@1.0.0"].license.as_deref(),
        Some("ISC"),
    );
    // Array form: aube collapses to the first license type.
    assert_eq!(
        graph.packages["array-license@1.0.0"].license.as_deref(),
        Some("Public Domain"),
    );
    // Mixed array (bare string + object): first element is a
    // string, so its value is the license.
    assert_eq!(
        graph.packages["mixed-array-license@1.0.0"]
            .license
            .as_deref(),
        Some("MIT"),
    );
    assert!(graph.packages["no-license@1.0.0"].license.is_none());
}

/// A multi-member `workspaces` monorepo resolved fresh from package.json
/// (no source lockfile, so the graph carries NO `LocalSource::Link`
/// member packages) must still round-trip through the npm writer with
/// every member importer, its `node_modules/<member>` link record, and
/// its child dependency entries present. The prior writer keyed member
/// identity solely off a reader-synthesized `LocalSource::Link` package,
/// so on a fresh resolve it dropped every member + child dep and
/// `npm ci` rejected the lockfile with `Missing: <member> from lock
/// file`. The writer now recovers member name/version from each member's
/// `package.json` on disk, the same way the pnpm/bun writers do.
#[test]
fn test_write_emits_workspace_members_on_fresh_resolve() {
    let proj = tempfile::tempdir().unwrap();
    // Member manifests on disk — the only place a fresh resolve carries
    // the members' name/version (no LocalSource::Link in the graph).
    for (dir, name, dep) in [
        ("packages/pkg-a", "@dedup/pkg-a", ("lodash", "^3.10.1")),
        ("packages/pkg-b", "@dedup/pkg-b", ("lodash", "^4.17.0")),
    ] {
        let d = proj.path().join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("package.json"),
            format!(
                r#"{{"name":"{name}","version":"1.0.0","dependencies":{{"{}":"{}"}}}}"#,
                dep.0, dep.1
            ),
        )
        .unwrap();
    }

    let mut graph = LockfileGraph::default();
    // Two registry child versions (the dedup case): lodash 3 + 4.
    for (ver, integ) in [("3.10.1", "sha512-three"), ("4.18.1", "sha512-four")] {
        let dep_path = format!("lodash@{ver}");
        graph.packages.insert(
            dep_path.clone(),
            LockedPackage {
                name: "lodash".to_string(),
                version: ver.to_string(),
                dep_path,
                integrity: Some(integ.to_string()),
                tarball_url: Some(format!(
                    "https://registry.npmjs.org/lodash/-/lodash-{ver}.tgz"
                )),
                ..Default::default()
            },
        );
    }
    // Root importer: no deps. Member importers: their lodash dep, each
    // pointing at the matching registry version (dedup).
    graph.importers.insert(".".to_string(), Vec::new());
    graph.importers.insert(
        "packages/pkg-a".to_string(),
        vec![DirectDep {
            name: "lodash".to_string(),
            dep_path: "lodash@3.10.1".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^3.10.1".to_string()),
        }],
    );
    graph.importers.insert(
        "packages/pkg-b".to_string(),
        vec![DirectDep {
            name: "lodash".to_string(),
            dep_path: "lodash@4.18.1".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^4.17.0".to_string()),
        }],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("conform-workspace-dedup".to_string()),
        version: Some("1.0.0".to_string()),
        ..Default::default()
    };
    let out = proj.path().join("package-lock.json");
    write(&out, &graph, &manifest).unwrap();

    let body = std::fs::read_to_string(&out).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    let packages = &doc["packages"];

    // Member importer entries carry name/version + their own deps.
    assert_eq!(packages["packages/pkg-a"]["name"], "@dedup/pkg-a");
    assert_eq!(packages["packages/pkg-a"]["version"], "1.0.0");
    assert_eq!(packages["packages/pkg-a"]["dependencies"]["lodash"], "^3.10.1");
    assert_eq!(packages["packages/pkg-b"]["name"], "@dedup/pkg-b");
    assert_eq!(packages["packages/pkg-b"]["dependencies"]["lodash"], "^4.17.0");

    // Root node_modules symlink record for each member.
    assert_eq!(packages["node_modules/@dedup/pkg-a"]["link"], true);
    assert_eq!(packages["node_modules/@dedup/pkg-a"]["resolved"], "packages/pkg-a");
    assert_eq!(packages["node_modules/@dedup/pkg-b"]["link"], true);
    assert_eq!(packages["node_modules/@dedup/pkg-b"]["resolved"], "packages/pkg-b");

    // Both deduped child versions land as nested package entries under
    // their owning member — npm ci rejects the lockfile if either is
    // absent.
    assert_eq!(
        packages["packages/pkg-a/node_modules/lodash"]["version"],
        "3.10.1"
    );
    assert_eq!(
        packages["packages/pkg-b/node_modules/lodash"]["version"],
        "4.18.1"
    );

    // Re-parsing the writer's output must recover all three importers,
    // proving the write→read round-trip is whole.
    let reparsed = parse(&out).unwrap();
    assert!(reparsed.importers.contains_key("packages/pkg-a"));
    assert!(reparsed.importers.contains_key("packages/pkg-b"));
}

/// `hasInstallScript`, `hasShrinkwrap`, `inBundle`, `deprecated`, and
/// `bundleDependencies` are npm's canonical per-package verbatim keys.
/// npm writes them on every matching entry; before this fix the reader
/// dropped them and the writer never re-emitted them, so each
/// `nub`-mediated rewrite of a real npm lockfile produced a spurious
/// diff on exactly the security-relevant packages (native addons carry
/// `hasInstallScript`). This guards the read → graph → write → re-read
/// round-trip for all five, plus npm's exact key *placement*
/// (`json-stringify-nice`'s type-then-alpha order).
#[test]
fn test_roundtrip_preserves_npm_verbatim_meta_fields() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    // `node_modules/native-addon` carries every field; `inner` is its
    // bundled child (`inBundle: true`). Field values pulled from a real
    // npm 11.x lockfile shape.
    let content = r#"{
        "name": "test",
        "version": "1.0.0",
        "lockfileVersion": 3,
        "requires": true,
        "packages": {
            "": {
                "name": "test",
                "version": "1.0.0",
                "dependencies": { "native-addon": "^1.0.0" }
            },
            "node_modules/native-addon": {
                "version": "1.0.0",
                "resolved": "https://registry.npmjs.org/native-addon/-/native-addon-1.0.0.tgz",
                "integrity": "sha512-aaa",
                "hasInstallScript": true,
                "hasShrinkwrap": true,
                "deprecated": "use native-addon@2 instead",
                "bundleDependencies": ["inner"],
                "dependencies": { "inner": "2.0.0" }
            },
            "node_modules/native-addon/node_modules/inner": {
                "version": "2.0.0",
                "integrity": "sha512-bbb",
                "inBundle": true
            }
        }
    }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let addon = &graph.packages["native-addon@1.0.0"];
    assert!(addon.has_install_script, "hasInstallScript should parse");
    assert!(addon.has_shrinkwrap, "hasShrinkwrap should parse");
    assert_eq!(
        addon.deprecated.as_deref(),
        Some("use native-addon@2 instead"),
    );
    assert_eq!(addon.bundled_dependencies, vec!["inner".to_string()]);
    let inner = &graph.packages["inner@2.0.0"];
    assert!(inner.in_bundle, "inBundle should parse");

    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("native-addon".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let body = std::fs::read_to_string(out.path()).unwrap();

    // Fields re-emitted at all.
    assert!(body.contains("\"hasInstallScript\": true"), "got:\n{body}");
    assert!(body.contains("\"hasShrinkwrap\": true"), "got:\n{body}");
    assert!(body.contains("\"inBundle\": true"), "got:\n{body}");
    assert!(
        body.contains("\"deprecated\": \"use native-addon@2 instead\""),
        "got:\n{body}"
    );
    assert!(body.contains("\"bundleDependencies\""), "got:\n{body}");
    assert!(body.contains("\"inner\""), "got:\n{body}");

    // npm's `json-stringify-nice` ordering: `bundleDependencies` (an
    // array → non-object) sorts at `b` ahead of `integrity`'s scalar
    // group; `deprecated` follows `integrity`; the `hasInstallScript` /
    // `hasShrinkwrap` bools sort after `integrity` and before
    // `dependencies` (the only object key). Assert relative placement so
    // a future reorder can't silently produce churn vs npm.
    let pos = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("missing {needle}\n{body}"));
    assert!(pos("\"bundleDependencies\"") < pos("\"deprecated\""));
    assert!(pos("\"deprecated\"") < pos("\"hasInstallScript\""));
    assert!(pos("\"hasInstallScript\"") < pos("\"hasShrinkwrap\""));
    // Object key `dependencies` comes after all the scalars.
    assert!(pos("\"hasShrinkwrap\"") < body.rfind("\"dependencies\"").unwrap());

    // Re-parse: every field survives a full cycle.
    let reparsed = parse(out.path()).unwrap();
    let addon2 = &reparsed.packages["native-addon@1.0.0"];
    assert!(addon2.has_install_script);
    assert!(addon2.has_shrinkwrap);
    assert_eq!(addon2.deprecated.as_deref(), Some("use native-addon@2 instead"));
    assert_eq!(addon2.bundled_dependencies, vec!["inner".to_string()]);
    assert!(reparsed.packages["inner@2.0.0"].in_bundle);
}

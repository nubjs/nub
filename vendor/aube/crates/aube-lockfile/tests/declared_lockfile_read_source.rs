//! A `packageManager` declaration decides which lockfile a READ resolves
//! against, not only which one a write targets.
//!
//! `package-lock.json` sorts LAST in filename precedence, so a stray `bun.lock`
//! used to win the read while `write_lockfile_preserving_existing` still
//! resolved npm. An install then serialized bun's thinner graph back out as
//! `package-lock.json`, silently dropping the `resolved`, `license` and
//! `engines` that bun's format cannot carry.

use aube_lockfile::{
    LockfileKind, ResolvedLockfileKind, parse_lockfile_with_kind, resolve_project_lockfile_kind,
};
use std::path::Path;

const LODASH_20_INTEGRITY: &str = "sha512-PlhdFcillOINfeV7Ni6oF1TAEayyZBoZ8bcshTHqOYJYlrqzRK5hagpagky5o4HfCzzd1TRkXPMFq6cKk9rGmA==";
const LODASH_21_INTEGRITY: &str = "sha512-v2kDEe57lecTulaDIuNTPy3Ry4gLGJ6Z1O3vE1krgXZNrsQ+LFTGHVxVjcXPs17LhbZVGedAJv8XZ1tvj5FvSg==";

/// The two lockfiles deliberately disagree on lodash's version, so which one
/// the read picked is visible in the returned graph rather than inferred.
fn fixture(dir: &Path, package_manager: &str) -> aube_manifest::PackageJson {
    let manifest_json = format!(
        r#"{{"name":"fx","version":"1.0.0","packageManager":"{package_manager}",
            "dependencies":{{"lodash":"^4.17.0"}}}}"#
    );
    std::fs::write(dir.join("package.json"), &manifest_json).unwrap();

    std::fs::write(
        dir.join("package-lock.json"),
        format!(
            r#"{{"name":"fx","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{{
                "":{{"name":"fx","version":"1.0.0","dependencies":{{"lodash":"^4.17.0"}}}},
                "node_modules/lodash":{{
                    "version":"4.17.20",
                    "resolved":"https://registry.npmjs.org/lodash/-/lodash-4.17.20.tgz",
                    "integrity":"{LODASH_20_INTEGRITY}",
                    "license":"MIT"}}}}}}"#
        ),
    )
    .unwrap();

    std::fs::write(
        dir.join("bun.lock"),
        format!(
            r#"{{"lockfileVersion":1,
                "workspaces":{{"":{{"name":"fx","dependencies":{{"lodash":"^4.17.0"}},}},}},
                "packages":{{"lodash":["lodash@4.17.21","",{{}},"{LODASH_21_INTEGRITY}"],}}}}"#
        ),
    )
    .unwrap();

    serde_json::from_str(&manifest_json).unwrap()
}

#[test]
fn declared_npm_reads_package_lock_over_a_higher_precedence_bun_lockfile() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = fixture(dir.path(), "npm@11.0.0");

    let (graph, kind) = parse_lockfile_with_kind(dir.path(), &manifest).unwrap();
    assert_eq!(
        kind,
        LockfileKind::Npm,
        "declared npm must read package-lock.json, not the bun.lock that outranks it in filename precedence"
    );
    assert_eq!(
        resolve_project_lockfile_kind(dir.path()).unwrap(),
        ResolvedLockfileKind::Existing(LockfileKind::Npm),
        "the write path resolves npm, so a read landing anywhere else rewrites one format's graph into the other's file"
    );

    let lodash = graph
        .packages
        .values()
        .find(|p| p.name == "lodash")
        .expect("lodash missing from the parsed graph");
    assert_eq!(
        lodash.version, "4.17.20",
        "read the npm lockfile's pin; bun.lock pins 4.17.21"
    );
    assert_eq!(
        lodash.license.as_deref(),
        Some("MIT"),
        "npm-only metadata that bun's format cannot carry must survive the read"
    );
    assert_eq!(
        lodash.tarball_url.as_deref(),
        Some("https://registry.npmjs.org/lodash/-/lodash-4.17.20.tgz"),
        "npm-only metadata that bun's format cannot carry must survive the read"
    );
}

#[test]
fn declared_bun_still_reads_bun_lock_over_a_coexisting_package_lock() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = fixture(dir.path(), "bun@1.2.0");

    let (graph, kind) = parse_lockfile_with_kind(dir.path(), &manifest).unwrap();
    assert_eq!(
        kind,
        LockfileKind::Bun,
        "the declaration decides in both directions — this is not an npm-always rule"
    );

    let lodash = graph
        .packages
        .values()
        .find(|p| p.name == "lodash")
        .expect("lodash missing from the parsed graph");
    assert_eq!(
        lodash.version, "4.17.21",
        "read bun.lock's pin; package-lock.json pins 4.17.20"
    );
}

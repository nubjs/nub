use super::{EjectPhantomImporters, grow_to_importers, names, should_seed};
use pnpm_store_dir::ResolvedPackage;
use std::collections::HashSet;

/// Build a resolved set from `(id, dependencies)` pairs, with no store rows:
/// the closure is about the edges, and a package with no row is exactly what
/// the policy must handle without ejecting it.
fn resolved<'a>(edges: &'a [(&'static str, Vec<String>)]) -> Vec<ResolvedPackage<'a>> {
    edges
        .iter()
        .map(|(id, dependencies)| ResolvedPackage {
            id,
            dependencies,
            index_key: None,
            root_direct: false,
        })
        .collect()
}

fn seed(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|id| (*id).to_string()).collect()
}

/// Ejecting a package is only sound if everything that imports it is ejected
/// too, however far up the chain that reaches — a store-resident importer
/// would go on resolving the shared copy.
#[test]
fn the_closure_reaches_every_importer_however_deep() {
    let edges = [
        ("app@1.0.0", vec!["mid@1.0.0".to_string()]),
        ("mid@1.0.0", vec!["leaf@1.0.0".to_string()]),
        ("leaf@1.0.0", Vec::new()),
        ("unrelated@1.0.0", Vec::new()),
    ];
    let kept = grow_to_importers(&resolved(&edges), seed(&["leaf@1.0.0"]));

    assert_eq!(
        kept,
        seed(&["leaf@1.0.0", "mid@1.0.0", "app@1.0.0"]),
        "the closure must climb past the direct importer and stop at the graph's edge",
    );
}

/// A cycle must not spin: the loop adds a package at most once, so it ends.
#[test]
fn a_cycle_between_importers_terminates() {
    let edges = [
        ("a@1.0.0", vec!["b@1.0.0".to_string()]),
        (
            "b@1.0.0",
            vec!["a@1.0.0".to_string(), "seed@1.0.0".to_string()],
        ),
        ("seed@1.0.0", Vec::new()),
    ];
    let kept = grow_to_importers(&resolved(&edges), seed(&["seed@1.0.0"]));

    assert_eq!(kept, seed(&["seed@1.0.0", "b@1.0.0", "a@1.0.0"]));
}

/// Nothing flagged means nothing moves. Without this the policy could eject
/// the whole graph and still look like it was working.
#[test]
fn an_empty_seed_keeps_every_package_shared() {
    let edges = [
        ("app@1.0.0", vec!["leaf@1.0.0".to_string()]),
        ("leaf@1.0.0", Vec::new()),
    ];
    assert!(grow_to_importers(&resolved(&edges), HashSet::new()).is_empty());
}

/// The configured seed is a package NAME, and an identifier carries a
/// version — so the match is on the name alone, and must not fire on a
/// package whose name merely starts with it.
#[test]
fn a_seed_name_matches_that_package_and_not_a_longer_one() {
    assert!(names("vite@5.0.0", "vite"));
    assert!(names("@scope/pkg@1.0.0", "@scope/pkg"));
    assert!(
        !names("vite-plugin-foo@1.0.0", "vite"),
        "a longer name that starts with the seed is a different package",
    );
    assert!(!names("rollup@4.0.0", "vite"));
}

/// A policy with only its seed names filled in: the store handles are what
/// the scan half reads, and the seed half never touches them.
fn policy(seeds: &[&str]) -> EjectPhantomImporters {
    EjectPhantomImporters {
        cache_dir: None,
        store_dir: None,
        seeds: seeds.iter().map(|name| (*name).to_string()).collect(),
    }
}

/// The scan's own record of one undeclared target. Its provenance bits are
/// private to the scanner and the sidecar is the only way one is ever built,
/// so a test builds one the same way — through the sidecar's format.
fn targets(names: &[&str]) -> Vec<nub_phantom_scan::PhantomTarget> {
    names
        .iter()
        .map(|name| {
            serde_json::from_value(serde_json::json!({
                "name": name,
                "from_main": true,
                "from_subpath": false,
            }))
            .expect("build a phantom target from the sidecar's own shape")
        })
        .collect()
}

fn set<'a>(names: &[&'a str]) -> HashSet<&'a str> {
    names.iter().copied().collect()
}

/// From 8.1 vite finds the shared store itself, so the name seed that every
/// install carries must not move it — the eject would take its whole
/// ancestor closure with it and buy nothing.
#[test]
fn the_vite_seed_ejects_only_the_versions_that_need_it() {
    let subject = policy(&["vite"]);

    assert!(subject.seeded("vite@5.4.11"), "below 8.1 the eject is what makes the copy patchable");
    assert!(subject.seeded("vite@8.0.9"));
    assert!(!subject.seeded("vite@8.1.0"), "8.1 reads the store location itself");
    assert!(!subject.seeded("vite@9.0.0"));
    assert!(
        !subject.seeded("vite@8.1.0-beta.1"),
        "a prerelease of 8.1 carries the same reader",
    );
    assert!(
        !policy(&["vite-plugin-foo"]).seeded("vite@9.0.0"),
        "the version rule answers for vite whatever else is seeded",
    );
}

/// The curated project-context names are what keeps a build script that
/// walks up to its consuming project from running detached in the shared
/// store. They reach this policy through the same seed list.
#[test]
fn a_project_context_package_is_seeded_by_the_default_list() {
    let subject = policy(super::super::phantom_closure::NUB_PROJECT_CONTEXT_EJECT);
    assert!(subject.seeded("simple-git-hooks@2.11.1"));
}

/// A flagged package stays shared only when every undeclared target is
/// PROVABLY reachable either way. Anything less keeps the eject, because a
/// wrong skip is a real import failure while a redundant eject costs disk.
#[test]
fn a_flag_is_downgraded_only_when_every_target_is_a_sibling_the_project_lacks() {
    assert!(
        !should_seed(&targets(&["protobufjs"]), &set(&["protobufjs"]), &set(&["other"])),
        "a sibling the project does not name resolves the same either way",
    );
    assert!(
        should_seed(&targets(&["protobufjs"]), &set(&[]), &set(&[])),
        "a target that is not a sibling has nowhere to resolve from once shared",
    );
    assert!(
        should_seed(&targets(&["protobufjs"]), &set(&["protobufjs"]), &set(&["protobufjs"])),
        "a target the project itself depends on resolves differently once ejected",
    );
    assert!(
        should_seed(&targets(&["a", "b"]), &set(&["a"]), &set(&[])),
        "one unprovable target is enough to keep the eject",
    );
    assert!(
        should_seed(&[], &set(&[]), &set(&[])),
        "no recorded targets is no proof of anything",
    );
}

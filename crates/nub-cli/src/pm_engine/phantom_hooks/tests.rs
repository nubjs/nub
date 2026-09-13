use super::{grow_to_importers, names};
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

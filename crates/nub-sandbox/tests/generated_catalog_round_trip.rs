//! A GENERATED catalog must survive parse-and-lookup with its grants intact.
//!
//! ⛔ WHY THIS EXISTS. The corpus harness emitted catalogs that parsed cleanly and granted NOTHING,
//! and no test caught it — every existing check asserts against the COMPILED-IN table
//! (`PACKAGE_NETWORK_ALLOWED`), which is hand-maintained and correct. Nothing asserted that a
//! GENERATED catalog reaches the same lookup the jail uses.
//!
//! The measured consequence: on Windows, packages needing egress failed 67-87% of the time against
//! 0-5% for packages that do not — at EVERY grant including the widest. macOS and Linux hid it for
//! hundreds of packages because they deny egress IN-KERNEL off the v2 capability and never consult
//! the table at all; Windows' userland net gate is its only consumer.
//!
//! ⛔ TWO PARSERS EXIST AND PICKING THE WRONG ONE IS THE TRAP THIS FILE FELL INTO FIRST. The
//! generator emits **v2** (`packages[<name>].default.network`); `catalog::parse` is **v1** and
//! requires `networkHosts`/`packageGrants`. `catalog_override` tries v2 first and falls back to v1.
//! Asserting a v2 document against the v1 parser fails with "`networkHosts` must be an array",
//! which looks like a generator defect and is not.

// UN-GATED 2026-08-14. This file carried `#![cfg(feature = "build-jail-catalog-override")]`, so the
// ONLY test asserting that a GENERATED catalog reaches the lookup the jail uses never ran in a
// default build — precisely the gap that let a catalog "parse cleanly and grant NOTHING" reach
// Windows. It needs nothing but `catalog_v2`, which every build now compiles.

/// Exactly what `collate.mjs` emits for a package measured as needing egress.
const GENERATED_V2: &str = r#"{
  "packages": {
    "@apollo/rover": { "default": { "network": true, "notes": "latest measured 0.29.1" } },
    "some-fs-only-pkg": { "default": { "write": "disk", "notes": "no egress" } },
    "old-net-pkg": {
      "default": { "write": "disk", "notes": "current releases need no egress" },
      "versions": { "<2.0.0": { "network": true, "notes": "older versions downloaded a binary" } }
    }
  },
  "baseline": [],
  "env": []
}"#;

fn egress_names(c: &nub_sandbox::catalog_v2::Catalog) -> Vec<&str> {
    // Resolved on the CURRENT OS, mirroring `catalog_override::package_network_allowed` exactly
    // — a grant's `network` can now differ per OS, so "does this package need egress" has no
    // OS-free answer.
    let here = nub_sandbox::catalog_v2::Platform::current();
    let mut v: Vec<&str> = c
        .packages
        .iter()
        .filter(|(_, e)| {
            e.default.on(here).network || e.versions.iter().any(|b| b.grant.on(here).network)
        })
        .map(|(n, _)| n.as_str())
        .collect();
    v.sort_unstable();
    v
}

#[test]
fn a_generated_v2_catalog_parses_and_its_egress_grants_are_reachable() {
    let c = nub_sandbox::catalog_v2::parse(GENERATED_V2)
        .expect("the generator's own output must parse — if this fails the v2 schema drifted");

    assert_eq!(
        egress_names(&c),
        vec!["@apollo/rover", "old-net-pkg"],
        "the egress set derived from a v2 catalog must name every package that measured a network \
         need at ANY version, and nothing else. This is the exact derivation \
         `catalog_override::package_network_allowed` performs to feed `build_jail_net_allowed`, \
         which is the ONLY thing granting egress on Windows."
    );
}

/// THE REGRESSION. A package needing egress only in an OLD band must still appear: the runtime
/// table is name-scoped, so withholding the name denies the very versions that need it.
#[test]
fn a_version_banded_egress_need_still_names_the_package() {
    let c = nub_sandbox::catalog_v2::parse(GENERATED_V2).expect("parses");
    let old = c.packages.get("old-net-pkg").expect("entry present");

    assert!(
        !old.default
            .on(nub_sandbox::catalog_v2::Platform::current())
            .network,
        "fixture precondition: this package's CURRENT releases must not need egress, or it cannot \
         distinguish band-derived admission from default-derived admission"
    );
    assert!(
        egress_names(&c).contains(&"old-net-pkg"),
        "a package needing egress in any band must be admitted — over-granting is the direction \
         this project accepts; under-granting breaks the package"
    );
}

/// A package that measured no egress anywhere must never be admitted. Silent over-granting is the
/// failure the catalog exists to prevent.
#[test]
fn a_package_with_no_measured_egress_is_not_admitted() {
    let c = nub_sandbox::catalog_v2::parse(GENERATED_V2).expect("parses");
    assert!(
        !egress_names(&c).contains(&"some-fs-only-pkg"),
        "a filesystem-only grant must not carry egress"
    );
}

/// THE COLLATOR AND THE PARSER MUST MOVE TOGETHER, and the fixtures above cannot enforce that:
/// they are hand-written, so they keep passing while `collate.mjs` emits a field the parser has
/// since retired. This binds the real deliverable — the catalog checked in beside the harness —
/// to the parser that has to load it.
///
/// Round-tripped as well as parsed, because `emit` is the half a hand-written fixture is least
/// likely to exercise: a serializer that dropped a per-OS block would still satisfy every
/// assertion above.
#[test]
fn the_checked_in_catalog_parses_and_round_trips() {
    const SHIPPED: &str = include_str!("../../../tests/build-jail-search/catalog-v2.json");
    let c = nub_sandbox::catalog_v2::parse(SHIPPED).unwrap_or_else(|e| {
        panic!("tests/build-jail-search/catalog-v2.json must parse — collator/parser drift: {e}")
    });
    assert!(
        !c.packages.is_empty(),
        "an empty catalog would satisfy every assertion below without testing anything"
    );
    let emitted = nub_sandbox::catalog_v2::emit(&c);
    assert_eq!(
        c,
        nub_sandbox::catalog_v2::parse(&emitted).expect("emitted catalog must parse"),
        "emit -> parse must be lossless for the real catalog"
    );
}

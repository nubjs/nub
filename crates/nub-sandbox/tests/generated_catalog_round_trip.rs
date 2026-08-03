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

#![cfg(feature = "build-jail-catalog-override")]

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
    let mut v: Vec<&str> = c
        .packages
        .iter()
        .filter(|(_, e)| e.default.network || e.versions.iter().any(|b| b.grant.network))
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
        !old.default.network,
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

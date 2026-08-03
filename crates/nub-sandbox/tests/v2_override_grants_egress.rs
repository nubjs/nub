//! A v2 catalog override must grant egress through `build_jail_net_allowed` — the REAL function.
//!
//! ⛔ WHY THIS EXISTS SEPARATELY FROM `generated_catalog_round_trip.rs`. That test reimplements the
//! derivation as a filter expression and asserts its own arithmetic, so it would pass even with the
//! shipped function still broken — it checks the SHAPE of a generated catalog, not the code path.
//! This one drives `build_jail_net_allowed`, which is what the Windows net gate actually consults.
//!
//! THE BUG IT PINS: `catalog_override::package_network_allowed()` consulted only the v1 catalog. A
//! v2 override parses and loads fine, but v2 carries egress as a per-package CAPABILITY
//! (`packages[<name>].default.network`) rather than a v1 `packageNetwork.full` table — so the
//! function returned `None`, `build_jail_net_allowed` fell back to the COMPILED-IN table, and that
//! table cannot name a package an override exists to test. Measured on Windows: packages needing
//! egress failed 83% of the time against 3.5% for those that do not, at EVERY grant including the
//! widest.
//!
//! macOS and Linux deny egress IN-KERNEL off the capability and never reach this code, which is why
//! a cross-platform override bug surfaced on exactly one platform — and why this test is worth
//! having on every platform rather than behind a `#[cfg(windows)]`.

#![cfg(feature = "build-jail-catalog-override")]

/// A package name that cannot collide with the compiled-in table, so admission can only come from
/// the override. Asserted below rather than assumed.
const ONLY_IN_OVERRIDE: &str = "nub-test-egress-package-that-is-not-in-the-catalog";

const V2_OVERRIDE: &str = r#"{
  "packages": {
    "nub-test-egress-package-that-is-not-in-the-catalog": {
      "default": { "network": true, "notes": "measured as needing egress" }
    },
    "nub-test-fs-only-package": {
      "default": { "write": "disk", "notes": "measured as needing no egress" }
    }
  },
  "baseline": [],
  "env": []
}"#;

/// ⛔ ONE TEST, NOT THREE. The override is read once per process behind a `OnceLock`, so several
/// `#[test]` fns in one binary would race on which catalog won. Cargo runs test fns as threads of a
/// single process — separate assertions here, one activation.
#[test]
fn a_v2_override_grants_egress_through_the_real_predicate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("override.json");
    std::fs::write(&path, V2_OVERRIDE).expect("write override");

    // ⛔ SETTING THE VARIABLE DOES NOT LOAD THE OVERRIDE. The loader latches once, from an explicit
    // `init_from_env()` / `decide()` call that nub makes at startup — `active_v2()` only READS that
    // latch. A test that sets the env var and asserts straight away is asserting against an
    // unloaded catalog, and the failure looks exactly like the bug it is meant to pin. Drive the
    // real entry point instead.
    let decision = nub_sandbox::catalog_override::decide(Some(path.to_str().expect("utf8 path")))
        .expect("a readable v2 catalog must not be refused");
    assert!(
        matches!(
            decision,
            nub_sandbox::catalog_override::Decision::Loaded { .. }
        ),
        "the override must LOAD, not fall back — otherwise every assertion below is about the \
         compiled-in catalog: {decision:?}"
    );

    // THE CONTROL. If the compiled-in table happened to carry this name the test would pass for the
    // wrong reason, so prove the name is absent from it first.
    assert!(
        !nub_sandbox::PACKAGE_NETWORK_ALLOWED
            .iter()
            .any(|(n, _)| *n == ONLY_IN_OVERRIDE),
        "fixture precondition: `{ONLY_IN_OVERRIDE}` must NOT be in the compiled-in table, or \
         admission below proves nothing about the override"
    );

    assert!(
        nub_sandbox::build_jail_net_allowed(Some(ONLY_IN_OVERRIDE), Some("1.0.0")),
        "a v2 override granting `network: true` must admit the package through \
         `build_jail_net_allowed` — this is the ONLY thing that grants egress on Windows, and it \
         returned false for every overridden package before e3cdc0e7f9"
    );

    assert!(
        !nub_sandbox::build_jail_net_allowed(Some("nub-test-fs-only-package"), Some("1.0.0")),
        "a package the override gives no egress must stay denied — silent over-granting is the \
         failure the catalog exists to prevent"
    );

    assert!(
        !nub_sandbox::build_jail_net_allowed(
            Some("nub-test-package-absent-entirely"),
            Some("1.0.0")
        ),
        "a name absent from the override must be denied: no entry means no capability, which is \
         the default that IS the mechanism"
    );
}

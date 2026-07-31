//! The dev-only catalog override, FAILURE arm: every way of getting the override wrong falls
//! back to the compiled catalog, loudly, without failing the launch.
//!
//! Its own test binary because the override latches process-globally and this arm must run in
//! a process that never latched a good catalog — see `build_jail_catalog_override`, which owns
//! the valid-load half.
//!
//! WHY FALL BACK RATHER THAN ABORT. `data/README.md` fixes the direction: the compiled copy is
//! the FLOOR and a rejection resolves to it, "never to grant everything". Not aborting is the
//! second half, and it is a corpus-loop decision — a typo in a scratch catalog should cost one
//! banner, not a 344-package run. The banner is what keeps the fallback from being silent,
//! which the same doc requires "because it is indistinguishable from an attack".

#[cfg(feature = "build-jail-catalog-override")]
use nub_sandbox::catalog_override::Decision;
use nub_sandbox::catalog_override::decide;

/// Rejected by the SHARED validator, not by a JSON parser: this parses fine and then fails
/// `require_project_relative`. That is the property worth pinning — the override runs the same
/// build-time checks, so it cannot introduce a shape `cargo build` would have refused.
///
/// Feature-gated with the arm that uses it: a default build has no loader, and an ungated
/// constant would trip `dead_code` and fail the workspace clippy gate.
#[cfg(feature = "build-jail-catalog-override")]
const TRAVERSING_CATALOG: &str = r#"{
  "networkHosts": [],
  "packageGrants": [
    {
      "package": "escaper",
      "projectReads": ["../../../etc"],
      "mechanism": "A fixture whose project read escapes the project root.",
      "evidence": "measured",
      "observed": "Synthesised by the catalog-override fallback test; no real package.",
      "platform": "any"
    }
  ]
}"#;

#[test]
fn every_failure_mode_falls_back_to_the_compiled_catalog_without_aborting() {
    // An empty value is a harness bug in every build, and is refused rather than ignored.
    assert!(decide(Some("")).is_err());

    #[cfg(not(feature = "build-jail-catalog-override"))]
    {
        let refusal = decide(Some("/any/path.json"))
            .expect_err("a default build must refuse the variable, not ignore it");
        assert!(
            refusal.contains("build-jail-catalog-override"),
            "the refusal must name the feature that would honour it: {refusal}"
        );
    }

    #[cfg(feature = "build-jail-catalog-override")]
    {
        // The compiled tables, captured before any load attempt — the fallback is asserted
        // against these rather than against a re-read of the same accessor.
        let compiled_hosts = nub_sandbox::download_hosts();
        let compiled_egress = nub_sandbox::package_network_allowed();
        assert!(
            !compiled_hosts.is_empty(),
            "the shipped catalog must be non-empty or the assertions below are vacuous"
        );

        // 1. A path that does not exist.
        let missing = decide(Some("/nonexistent/build-jail-catalog.json"))
            .expect("an unreadable override falls back, it does not abort");
        assert!(
            matches!(missing, Decision::FellBack { .. }),
            "a missing file must fall back: {missing:?}"
        );
        assert!(
            missing.banner().is_some_and(|b| b.contains("compiled-in")),
            "the fallback must say which catalog is actually in force"
        );

        // 2. A file that is not JSON, and 3. one that parses but fails VALIDATION. Both are
        // checked through `decide` after the latch above already resolved to "compiled", so
        // they assert the latch is not disturbed by a later attempt either.
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, body) in [
            ("garbage.json", "this is not json {{{"),
            ("traversing.json", TRAVERSING_CATALOG),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, body).expect("write fixture");
            let decision = decide(Some(path.to_str().expect("utf-8 path")))
                .expect("a rejected override falls back, it does not abort");
            assert!(
                matches!(decision, Decision::FellBack { .. }),
                "{name} must fall back: {decision:?}"
            );
        }

        // The whole point: after every failure above, the compiled catalog is still what the
        // jail compiles against — not an empty table, and not a partial one.
        assert_eq!(
            nub_sandbox::download_hosts(),
            compiled_hosts,
            "the compiled host list must survive every rejected override"
        );
        assert_eq!(
            nub_sandbox::package_network_allowed(),
            compiled_egress,
            "the compiled egress set must survive every rejected override"
        );
    }
}

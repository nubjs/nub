//! The dev-only catalog override, VALID-load arm: inert and unreachable in a default build,
//! and a real table replacement in a featured one.
//!
//! ONE test in its own binary, deliberately, for the same reason as
//! `build_jail_project_grant_arm`: the override latches into a process-global `OnceLock`, so
//! the unset control and the post-load assertions must be ordered within a single test and no
//! sibling may share the process and observe a latch it did not set. The INVALID-catalog
//! fallback needs a process that never latched a good one, so it lives in its own binary
//! (`build_jail_catalog_override_fallback`).
//!
//! Every assertion is paired with a control. "The override replaced the table" would pass
//! just as well against a table that collapsed to empty, so the override catalog deliberately
//! carries entries the compiled one does NOT (`example.invalid`, `override-only-pkg`) and the
//! compiled-only entries are asserted GONE — replacement, not merge, in both directions.

use nub_sandbox::catalog_override::{Decision, decide};

/// A syntactically and semantically VALID catalog that shares no entry with the shipped one,
/// so every assertion below distinguishes replacement from both "unchanged" and "emptied".
/// Feature-gated: a default build has no loader to hand it to, and an ungated constant would
/// trip `dead_code` and fail the workspace clippy gate.
#[cfg(feature = "build-jail-catalog-override")]
const OVERRIDE_CATALOG: &str = r#"{
  "networkHosts": [
    {
      "host": "example.invalid",
      "artifact": "a fixture host that exists in no shipped catalog",
      "fetchedBy": ["override-only-pkg"],
      "evidence": "measured",
      "observed": "Synthesised by the catalog-override test; never resolved anywhere.",
      "platform": "any"
    }
  ],
  "packageGrants": [
    {
      "package": "override-only-pkg",
      "siblingDirs": [".override-fixture"],
      "mechanism": "A fixture entry asserting the override replaces the compiled grant table.",
      "evidence": "measured",
      "observed": "Synthesised by the catalog-override test; no real package is involved.",
      "platform": "any"
    }
  ]
}"#;

#[test]
fn the_override_is_inert_by_default_and_replaces_every_table_when_loaded() {
    // ── Control, and the shipped behaviour in EVERY build: unset means untouched. ──
    assert_eq!(decide(None), Ok(Decision::Default));
    assert_eq!(
        nub_sandbox::download_hosts(),
        nub_sandbox::DOWNLOAD_HOSTS,
        "unset must leave the compiled host list exactly in force"
    );
    assert_eq!(
        nub_sandbox::package_network_allowed(),
        nub_sandbox::PACKAGE_NETWORK_ALLOWED,
        "unset must leave the compiled egress set exactly in force"
    );
    // The compiled table is non-empty, so the "gone after override" assertions below are
    // meaningful rather than vacuous.
    assert!(
        nub_sandbox::DOWNLOAD_HOSTS.contains(&"nodejs.org"),
        "the shipped catalog is the baseline this test measures against"
    );

    // ── A set variable is never a silent no-op. ──
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
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catalog.json");
        std::fs::write(&path, OVERRIDE_CATALOG).expect("write override catalog");

        let decision = decide(Some(path.to_str().expect("utf-8 path")))
            .expect("a featured build accepts a valid catalog");
        assert!(
            matches!(decision, Decision::Loaded { .. }),
            "a valid catalog must load: {decision:?}"
        );
        assert!(
            decision.banner().is_some_and(|b| b.contains("OVERRIDDEN")),
            "an active override must announce itself so a run can assert which catalog it \
             measured"
        );

        // REPLACE, not merge — asserted in both directions so neither a stale compiled entry
        // nor a missing override entry can pass.
        assert_eq!(
            nub_sandbox::download_hosts(),
            ["example.invalid"],
            "the override's host list must be the whole host list"
        );
        assert!(
            !nub_sandbox::download_hosts().contains(&"nodejs.org"),
            "a compiled-only host must be GONE, or this is a merge and not a replacement"
        );
        assert_eq!(
            nub_sandbox::package_network_allowed(),
            [("override-only-pkg", None)],
            "egress is derived from the override's own networkHosts[].fetchedBy, and a \
             `fetchedBy` observation names no version so it can only be unscoped"
        );

        // The grant table is private, so it is measured through the policy it compiles to:
        // the override's package gets its sibling grant and the compiled catalog's does not.
        assert!(
            granted_siblings("override-only-pkg")
                .iter()
                .any(|g| g.contains(".override-fixture")),
            "the override's grant must reach the compiled policy"
        );
        assert!(
            granted_siblings("@prisma/client").is_empty(),
            "a package present only in the COMPILED catalog must get nothing once the \
             override replaced it"
        );
    }
}

/// The fs globs the curated table grants `package_name` under an isolated store layout.
/// Empty means the package matched no entry.
#[cfg(feature = "build-jail-catalog-override")]
fn granted_siblings(package_name: &str) -> Vec<String> {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    let drive = if cfg!(windows) { "C:" } else { "" };
    let homes = nub_sandbox::Homes {
        home: PathBuf::from(format!("{drive}/home/u")),
        tmp: PathBuf::from(format!("{drive}/tmp")),
        cache: PathBuf::from(format!("{drive}/home/u/.cache")),
        project: PathBuf::from(format!("{drive}/proj")),
    };
    let package_dir = homes.project.join(format!(
        "node_modules/.store/x@1/node_modules/{package_name}"
    ));
    let with_grant = nub_sandbox::compile_build_jail(
        homes.clone(),
        &package_dir,
        Some(package_name),
        Some("1.0.0"),
        Vec::new(),
        Vec::new(),
        BTreeMap::new(),
    )
    .expect("the build jail compiles");
    // Diffed against the SAME package dir compiled with no installer identity, so the result
    // is the curated table's contribution alone rather than every baseline grant.
    let baseline = nub_sandbox::compile_build_jail(
        homes,
        &package_dir,
        None,
        None,
        Vec::new(),
        Vec::new(),
        BTreeMap::new(),
    )
    .expect("the build jail compiles");

    let baseline_globs: Vec<String> = baseline
        .fs
        .rules
        .entries
        .iter()
        .map(|r| r.matcher.as_str().to_string())
        .collect();
    with_grant
        .fs
        .rules
        .entries
        .iter()
        .map(|r| r.matcher.as_str().to_string())
        .filter(|g| !baseline_globs.contains(g))
        .collect()
}

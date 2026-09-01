//! A `default` band that names no capability is a DENY, not a fallback.
//!
//! ⛔ THE MECHANISM, because it reads the opposite way round. `Entry::grant_for` picks the narrowest
//! `<X` bound that covers the version and does NOT merge `default` in as a base those bands extend,
//! so a release above every band lands on `default` alone. And a `default` that carries only a
//! `notes` string is not the same as having no entry: an ABSENT package falls back to
//! `catalog_v2::baseline_caps()` and still promotes the baseline's `write_paths`, while a PRESENT
//! entry resolves to `Some(grant)` with every capability empty and returns before the promotion loop
//! (`pm_engine::build_jail`). A notes-only band is therefore STRICTLY TIGHTER than saying nothing.
//!
//! ⛔ WHY THAT IS A DEFECT HERE AND NOT EVERYWHERE. An empty `default` is CORRECT for a package whose
//! current release genuinely does nothing -- it dropped its lifecycle hook, or it ships prebuilt --
//! and many entries are legitimately that. What is not correct is a band nobody ever measured
//! resolving to a hard deny: 93 entries currently have a `default` granting nothing while a lower
//! band grants something, and the shape alone does not separate the two cases. `electron` is the one
//! where the entry's OWN note settles it -- its `<43.4.0` band records that withdrawing network made
//! a cold install fail `getaddrinfo ENOTFOUND` fetching `electron-v33.4.11-darwin-arm64.zip` from
//! github.com, and the dist-tag has since moved to 44.x, which lands on `default`.
//!
//! Reads the shipped bytes through `include_str!` rather than the runtime lookup, which consults a
//! dev override and an on-disk update tier first; the subject here is the file in this repository.
use nub_sandbox::catalog_v2::{Catalog, Platform, Scope};

fn shipped() -> Catalog {
    nub_sandbox::catalog_v2::parse(include_str!("../data/build-jail-catalog-v2.json"))
        .expect("the shipped catalog parses; build.rs fails the build otherwise")
}

/// `44.0.0` is above every `electron` band, so it resolves to `default`; `33.4.11` is one of the
/// versions the `<43.4.0` band was measured at. The pair is the control: if the band below stopped
/// granting egress too, the accessor is broken rather than the default band.
#[test]
fn electron_still_reaches_github_at_a_version_above_every_measured_band() {
    let catalog = shipped();
    let entry = catalog
        .packages
        .get("electron")
        .expect("electron has a catalog entry");

    let mut denied: Vec<String> = Vec::new();
    for platform in [Platform::Macos, Platform::Linux, Platform::Windows] {
        // CONTROL first: a version INSIDE the measured band must still carry egress. Without it a
        // failure below is equally well explained by the lookup returning nothing at all.
        let measured = entry.grant_for(Some("33.4.11")).on(platform);
        assert!(
            measured.network,
            "control failed on {}: the measured <43.4.0 band lost its egress, so this file is no \
             longer testing what it claims",
            platform.key()
        );

        let latest = entry.grant_for(Some("44.0.0")).on(platform);
        if !latest.network {
            denied.push(format!("{}: network", platform.key()));
        }
        // The postinstall unpacks the download into the package directory.
        if !latest.write.covers(Scope::Deps) {
            denied.push(format!("{}: write.deps", platform.key()));
        }
    }

    assert!(
        denied.is_empty(),
        "electron@44.0.0 resolves to a band that denies {}. Its postinstall downloads a platform \
         zip from github.com, and a notes-only `default` denies more than having no entry at all: \
         {}",
        denied.len(),
        denied.join(", ")
    );
}

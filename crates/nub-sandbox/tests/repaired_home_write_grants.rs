//! The grants that a re-bake against the same measurement records would withdraw again.
//!
//! ⛔ WHY THIS FILE EXISTS. The corpus harness scores a capability by re-running an install with
//! that capability DROPPED and checking the artefact. Its artifact gate walks only the package's
//! OWN directory, so a write to the user's REAL home is invisible to it: for a package whose
//! product IS a home write the drop arm passes with the product silently missing, and the descent
//! narrows the grant on a pass it did not earn. The gate was fixed (corpus epoch 70) and 115
//! archive records were re-recorded from their own logs (epoch 71); 95 of them resolved to a
//! shipped grant that no longer carried what the log attributes, across 25 cells. 20 of those 25
//! remain below -- the three macOS `electron-chromedriver` rows and two Linux rows were withdrawn
//! again on later measurements, for the reasons at the end of this comment.
//!
//! ⛔ `build.rs` PROVES THE CATALOG PARSES AND NOTHING MORE. It cannot know that a per-OS overlay
//! says what a measurement said, so a re-bake from the same inputs would withdraw these again with
//! no signal at all. That is the same failure shape `windows_promotion_paths.rs` was written for.
//!
//! ⛔ THE ASSERTION IS `covers`, NOT EQUALITY, AND THAT IS THE POINT. Every row is a FLOOR: a later
//! measurement may widen a cell, and this file must not turn that into a failure. Only a NARROWING
//! below what the archived log attributes is a regression.
//!
//! ⛔ IT READS THE SHIPPED BYTES DIRECTLY rather than going through the runtime lookup, because that
//! lookup consults a dev override and an on-disk update tier before the baked copy. Those tiers are
//! the right answer for the jail and the wrong one for a test whose subject is the file in this
//! repository.
//!
//! NOT HERE ON PURPOSE: `electron`'s `<43.4.0` macOS `write` withdrawal. Four repaired darwin
//! records attribute 2-3 real-home writes to it, and all of them land under
//! `$cache/nub/pm/tools/electron-cache`, which `compiler::preset` grants read-write to every jailed
//! script unconditionally. The withdrawal is therefore correct as measured, and pinning a
//! capability the package provably does not need would be pinning noise.
//!
//! ⛔ REMOVED 2026-09-01, AND FOR THE SAME REASON ONE STEP FURTHER: the three macOS
//! `electron-chromedriver` rows (`43.2.0` on `default`, `32.3.3` on `<43.2.0`, `12.0.0` on
//! `<32.3.3`). The paragraph above excused `electron` because its recorded paths were LITERALLY
//! inside the tool-cache leaf. `electron-chromedriver`'s were not -- they read
//! `~/Library/Caches/electron`, which is `@electron/get`'s UNJAILED default -- so the path test said
//! "real home" and the rows were restored. THE PATH TEST IS THE WRONG TEST. `pm_engine::build_jail`
//! unconditionally points `electron_config_cache` at `$cache/nub/pm/tools/electron-cache`, so what an
//! unjailed observation writes to the vendor default a JAILED run writes to the free leaf. Measured
//! with a jail-off control on all three bands: jail off leaves the same content-addressed directory
//! in the real home, jail on puts it in the leaf, and an arm granting only `network` reproduces
//! `bin/icudtl.dat` at 10,735,358 bytes while dropping `network` loses it. The macOS cells are now
//! pinned the other way by `macos_home_write_withdrawals.rs`; the WINDOWS rows below are untouched,
//! and that file's control asserts they stay.
//!
//! ⛔ REMOVED 2026-09-01, ON THE SAME REASONING APPLIED TO THE OTHER PLATFORM: the two LINUX rows
//! `@mui/x-telemetry@9.10.0` and `@pdftron/pdfnet-node@7.1.1`. The premise at the top of this file is
//! that the artifact gate cannot see a write to the user's REAL home, so a drop arm passes with the
//! product missing. That is true, and it does not reach these two, because the OBSERVE census whose
//! log attributed the writes runs the script with the JAIL OFF -- where `$HOME` IS the real home.
//! Under the jail the same code resolves `$HOME` to the redirected private home. `@mui/x-telemetry`
//! shows the mechanism directly: `postinstall/storage.js` reads
//! `XDG_CONFIG_HOME || os.homedir() + '/.config'`, the jail's env allowlist drops `XDG_CONFIG_HOME`,
//! and the confined run's file lands at `.cache/nub/jail-home/<hash>/.config/mui-x/config.json`. So
//! epoch 71 measured an unjailed write and inferred a jailed capability requirement from it. Settled
//! by comparing the two arms' ENTIRE jail `$HOME` under the wide and narrowed grants: 16 entries
//! against 16 for `@mui/x-telemetry` and 18 against 18 for `@pdftron/pdfnet-node`, nothing on either
//! side. Both cells are now pinned the other way by `linux_home_write_withdrawals.rs`; their WINDOWS
//! rows are untouched and that file's control asserts they stay.
use nub_sandbox::catalog_v2::{Catalog, Platform, Scope};

fn shipped() -> Catalog {
    nub_sandbox::catalog_v2::parse(include_str!("../data/build-jail-catalog-v2.json"))
        .expect("the shipped catalog parses; build.rs fails the build otherwise")
}

/// One restored cell: package, a version that RESOLVES to the repaired band, the platform whose
/// overlay was widened, the write scopes its archived log attributes, and whether that log also
/// recorded egress. The version is taken from an actual under-granted record, so the band it
/// selects is the one that was repaired rather than one chosen by hand.
#[rustfmt::skip]
const RESTORED: &[(&str, &str, Platform, &[Scope], bool)] = &[
    ("@depot/cli", "0.0.1-cli.2.99.1", Platform::Linux, &[Scope::UserHome], false),   // band default, 1 record(s)
    ("@mui/x-telemetry", "9.10.0", Platform::Windows, &[Scope::Deps, Scope::UserHome], false),   // band default, 1 record(s)
    ("@netlify/esbuild", "0.13.6", Platform::Linux, &[Scope::UserHome], true),   // band <0.14.39, 1 record(s)
    ("@netlify/esbuild", "0.13.6", Platform::Windows, &[Scope::Deps, Scope::UserHome], true),   // band <0.14.39, 1 record(s)
    ("@shopify/ngrok", "4.3.2", Platform::Windows, &[Scope::Deps, Scope::UserHome], true),   // band default, 1 record(s)
    ("electron-chromedriver", "1.8.0", Platform::Windows, &[Scope::Deps, Scope::Project, Scope::UserHome], true),   // band <32.3.3, 28 record(s)
    ("electron-chromedriver", "43.2.0", Platform::Windows, &[Scope::Deps, Scope::Project, Scope::UserHome], true),   // band default, 1 record(s)
    ("electron-prebuilt", "0.25.3", Platform::Windows, &[Scope::Deps, Scope::UserHome], true),   // band default, 14 record(s)
    ("esbuild", "0.11.23", Platform::Linux, &[Scope::UserHome], true),   // band <0.17.19, 1 record(s)
    ("esbuild", "0.11.23", Platform::Windows, &[Scope::Deps, Scope::UserHome], true),   // band <0.17.19, 1 record(s)
    ("exiftool-vendored", "0.1.1", Platform::Windows, &[Scope::Deps, Scope::Project, Scope::UserHome], true),   // band <37.2.0, 1 record(s)
    ("ffmpeg-static", "5.3.0", Platform::Windows, &[Scope::Deps, Scope::UserHome], true),   // band default, 1 record(s)
    ("ibm_db", "2.8.2", Platform::Windows, &[Scope::Deps, Scope::Project, Scope::UserHome], true),   // band default, 3 record(s)
    ("keccak", "1.4.0", Platform::Linux, &[Scope::UserHome], true),   // band <3.0.4, 1 record(s)
    ("mbt", "0.0.9", Platform::Linux, &[Scope::UserHome], true),   // band <1.2.49, 2 record(s)
    ("ngrok", "5.0.0-beta.2", Platform::Windows, &[Scope::Deps, Scope::UserHome], true),   // band default, 1 record(s)
    ("purescript", "0.0.1", Platform::Linux, &[Scope::UserHome], true),   // band <0.9.3, 3 record(s)
    ("react-native-purchases", "0.4.3", Platform::Linux, &[Scope::UserHome], true),   // band <1.5.4, 2 record(s)
    ("saucectl", "0.101.1", Platform::Windows, &[Scope::Deps, Scope::UserHome], true),   // band default, 1 record(s)
    ("ursa-optional", "0.9.9", Platform::Linux, &[Scope::UserHome], true),   // band default, 1 record(s)
];

#[test]
fn a_cell_repaired_from_its_archived_log_still_grants_what_that_log_attributes() {
    let catalog = shipped();
    // COLLECTED, not asserted per row. A re-bake withdraws a WHOLE CLASS of cells at once, and a
    // panic on the first one reports 1 of 25 -- which reads as an isolated typo rather than as the
    // systematic withdrawal it is, and costs a rebuild per cell to enumerate.
    let mut lost: Vec<String> = Vec::new();
    for (pkg, version, platform, scopes, network) in RESTORED {
        let entry = catalog
            .packages
            .get(*pkg)
            .unwrap_or_else(|| panic!("{pkg} has no catalog entry at all"));
        let caps = entry.grant_for(Some(version)).on(*platform);
        for scope in *scopes {
            if !caps.write.covers(*scope) {
                lost.push(format!(
                    "{pkg}@{version} on {}: write.{}",
                    platform.key(),
                    scope.as_str()
                ));
            }
        }
        if *network && !caps.network {
            lost.push(format!("{pkg}@{version} on {}: network", platform.key()));
        }
    }
    assert!(
        lost.is_empty(),
        "{} capability(ies) withdrawn from cells whose archived log attributes them. The drop arm \
         that scored each one could not see a real-home write, so the narrowing was never \
         measured; restore them rather than re-baking over them:\n  {}",
        lost.len(),
        lost.join("\n  ")
    );
}

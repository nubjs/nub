//! The win32 home-write and full-disk grants withdrawn once a post-repair binary re-measured them
//! at the BASE PROFILE.
//!
//! ⛔ WHY THESE CELLS WERE WIDE, AND WHY THE ROWS WERE STALE BY CONSTRUCTION. Every win32 record
//! backing them was measured 2026-07-31/08-01 on nub v0.6.0. Both Windows repairs landed after:
//! `50ec17043a` (2026-08-05) gives the AppContainer profile a traverse chain, so
//! `realpathSync(os.tmpdir())` stops dying `EPERM` inside `temp-dir` -> `tempfile` ->
//! `bin-wrapper`; `46b623e352` (2026-08-31) materializes the `$cache/nub/pm/tools/*` leaves, whose
//! read-write rule `derive_grants` had been dropping as `FsOrigin::Speculative` whenever the path
//! was absent. Before either, a `bin-wrapper` or download-then-move installer could not resolve or
//! stage its temp, every narrow rung failed, and the ladder climbed to the home or to `write:"disk"`
//! — which passes only because that rung declines the LowBox token altogether
//! (`backend/windows.rs`, `policy.build_jail && !confine_fs`). The grant was curing a resolution
//! fault by removing the sandbox.
//!
//! ⛔ WHAT "BASE PROFILE" MEANS HERE, because the measuring harness names it `(nothing)` and it is
//! not nothing. The grant search spells its cheapest rung as NO ENTRY for the package under test,
//! and an absent package takes [`catalog_v2::baseline_caps`] — `network: true`, `write: {deps}`,
//! and `BASELINE_WRITE_PATHS`. So a pass at that rung licenses withdrawing the real-home write and
//! the whole-disk write, and NOTHING MORE. In particular it does not license withdrawing
//! `write.deps` — the search has no rung below the baseline, so a package running without `deps`
//! was never measured at all. Every [`WITHDRAWN`] row therefore lands on `{deps: true}`, never on
//! nothing, and `every_withdrawal_keeps_the_write_deps_the_base_profile_measured` holds that.
//!
//! ⛔ TWO CELLS FROM THE SAME RUN DID NOT PASS AT THAT RUNG, and they live in
//! [`WITHDRAWN_ON_A_LADDER`] rather than here, because the sentence above is not true of them.
//! Their minima are `network` and `read.disk + network`, so the deps floor does not apply to the
//! second one at all. That list's own doc carries the distinction.
//!
//! ⛔ THE ARTIFACT PREDICATE ALONE DID NOT DECIDE ANY ROW. That predicate compares a digest of the
//! created-path list across the project and the store, and it is blind to a product that lands
//! outside both: on the same runner every `@pulumi/*` cell scored a digest PASS while its narrow
//! arm printed `There was an error installing the resource provider plugin` and the control
//! downloaded and unpacked it. Each row below was additionally required to reproduce the CONTROL
//! ARM'S SCRIPT OUTPUT line for line — `playwright`'s 116 MB Chromium fetch, `@shopify/ngrok`'s
//! `bin.equinox.io` download, `saucectl`'s `Installation succeeded`, `optipng pre-build test passed
//! successfully`.
//! Four candidates measured on the same run are NOT here because that comparison caught them:
//! `gif2webp-bin` (`pre-build test failed` under the narrow arm, `passed` under the control),
//! `pngout-bin` (`Error: spawn UNKNOWN`), `netlify-cli` (`gyp info ok` under the control,
//! `gyp ERR! build error` under the narrow arm) and `@eth-optimism/sdk` (`Access is denied.`).
//!
//! ⛔ NETWORK IS RETAINED, NOT RE-MEASURED. The base-profile rung grants egress, so no row below
//! can say anything about whether its package needs it. Each cell keeps the `network` it already
//! had, and the second test asserts that so a re-bake cannot drop both at once.
//!
//! ⛔⛔ AND SEVEN OF THE `false` VALUES HAVE SINCE MOVED TO `true`, ON THE AXIS THIS FILE
//! DISCLAIMS. `@azure-devops/mcp`, `@clerk/shared`, `@depot/cli`, `@mui/x-telemetry`,
//! `dtrace-provider`, `nodemon` and `rc-editor-core` inherited their denial from corpus records
//! that `baseline_egress_withdrawals.rs` no longer accepts as a licence — every one carries
//! `arms-unfalsifiable`, and no arm behind any of them ever dropped egress. The rung re-measured
//! here CARRIED egress, so those cells were shipping a grant narrower than the arm that passed
//! them. Restoring `network` follows from this file rather than reversing it: the column records
//! what each cell carries so a re-bake cannot silently move it, and the value moves when the
//! OTHER axis's evidence does. `@progress/kendo-licensing` stays `false`, which is what keeps the
//! assertion able to fail in both directions.
//!
//! ⛔ THESE ARE HAND EDITS ON A GENERATED FILE, WHICH IS WHY THEY NEED PINNING. `build.rs` proves
//! the catalog parses and nothing more; a re-bake from the archived records would restore all of
//! them, because those records were scored on v0.6.0 before either repair existed. Same shape as
//! `windows_fulldisk_withdrawals.rs`, one epoch on.
//!
//! Reads the shipped bytes through `include_str!` rather than the runtime lookup, which consults a
//! dev override and an on-disk update tier first; the subject here is the file in this repository.
use nub_sandbox::catalog_v2::{Catalog, Platform, Reach, Scope};

fn shipped() -> Catalog {
    nub_sandbox::catalog_v2::parse(include_str!("../data/build-jail-catalog-v2.json"))
        .expect("the shipped catalog parses; build.rs fails the build otherwise")
}

/// One withdrawn cell: package, the version the arms ACTUALLY RAN, the band that version resolves
/// to through `Entry::grant_for`'s narrowest-bound rule, and whether the cell carries egress.
///
/// The version is the measured one rather than one chosen by hand, so the row cannot drift onto a
/// band nothing measured — the trap `<7.0.1` and `<9.0.0` set, where the plain and the banded cell
/// were measured separately and disagree about what they need.
#[rustfmt::skip]
const WITHDRAWN: &[(&str, &str, &str, bool)] = &[
    ("@azure-devops/mcp",           "2.8.0",             "<2.9.0",   true),
    ("@clerk/shared",               "4.14.0",            "<4.29.1",  true),
    ("@depot/cli",                  "0.0.1-cli.2.102.7", "default",  true),
    ("@hyperjump/json-schema",      "0.23.5",            "<1.17.8",  true),
    ("@hyperjump/json-schema-core", "0.28.4",            "<0.28.5",  true),
    ("@mui/x-telemetry",            "9.10.0",            "default",  true),
    ("@prisma/engines",             "7.9.0",             "<7.9.1",   true),
    ("@progress/kendo-licensing",   "1.11.3",            "default",  false),
    ("@sentry/cli",                 "3.6.1",             "<3.6.2",   true),
    ("@shopify/ngrok",              "4.3.2",             "default",  true),
    ("chromedriver",                "152.0.2",           "default",  true),
    ("cwebp-bin",                   "8.0.0",             "default",  true),
    ("docxtemplater",               "3.31.3",            "<3.69.3",  true),
    ("dtrace-provider",             "0.8.8",             "default",  true),
    ("gifsicle",                    "7.0.1",             "default",  true),
    ("gifsicle",                    "7.0.0",             "<7.0.1",   true),
    ("jpeg-recompress-bin",         "7.0.0",             "default",  true),
    ("keccak",                      "3.0.3",             "<3.0.4",   true),
    ("ngrok",                       "5.0.0-beta.2",      "default",  true),
    ("node-jq",                     "6.3.1",             "default",  true),
    ("nodemon",                     "2.0.19",            "<3.1.14",  true),
    ("nx",                          "23.1.0",            "<23.1.1",  true),
    ("optipng-bin",                 "9.0.0",             "default",  true),
    ("optipng-bin",                 "8.1.0",             "<9.0.0",   true),
    ("playwright",                  "1.37.1",            "<1.62.1",  true),
    ("pngquant-bin",                "9.0.0",             "default",  true),
    ("rc-editor-core",              "0.3.13",            "<0.8.10",  true),
    ("react-native-purchases",      "4.6.3",             "<10.7.1",  true),
    ("saucectl",                    "0.213.0",           "default",  true),
    ("truffle",                     "5.11.5",            "default",  true),
    ("unicode",                     "0.6.1",             "<14.0.0",  true),
    ("ursa-optional",               "0.10.2",            "default",  true),
    ("zopflipng-bin",               "7.1.0",             "default",  true),
];

/// The two cells from the same run whose minimum is NOT the base profile, kept in their own list
/// because attributing them to that arm would be a claim they did not earn.
///
/// A cost-0 minimum has exactly ONE arm: the search walks ascending and stops at the first pass, so
/// state 0 passing means no other rung ever ran. These two failed state 0 and climbed, which is why
/// they are the only cells in this file with a red arm at all — 4 of 5 for `electron-prebuilt`, 16
/// of 17 for `appium-uiautomator2-driver`, each losing artefacts the control produced.
///
/// `electron-prebuilt` verified `network` ALONE, yet it keeps `write.deps`: one measured version is
/// not grounds to drop a second capability across a `default` band covering 125 versions, all of
/// which run an install script. Its state-0 failure is a PROMOTION artefact rather than a capability
/// need — `BASELINE_WRITE_PATHS` contains `.electron`, so at that rung the download is promoted out
/// of the throwaway home into the real one and its path stops matching the control's, which is
/// exactly the two paths the record lists as missing.
///
/// `appium-uiautomator2-driver` verified `read.disk + network` and keeps NO write scope, matching
/// what macOS and Linux already carry for this band. The read reach is the point of the row: it is
/// what the passing arm carried, and it is strictly narrower than the `write: "disk"` it replaces,
/// which makes `fs_confines` false so `backend/windows.rs` declines the AppContainer token and
/// compiles no rule list at all.
#[rustfmt::skip]
const WITHDRAWN_ON_A_LADDER: &[(&str, &str, &str, bool)] = &[
    ("appium-uiautomator2-driver", "0.11.0", "<8.4.0",  true),
    ("electron-prebuilt",          "1.4.13", "default", true),
];

/// `write.deps` SURVIVES every withdrawal, because the base profile the withdrawal rests on has it.
///
/// ⛔ THIS IS THE HALF A `userHome`-ONLY GUARD MISSES, and it caught a real defect in this file's
/// own first batch. `baseline_caps()` is `write: Scopes([Deps])` plus egress plus five promotions,
/// and an entry REPLACES the baseline whole (`preset.rs`, `v2_grant_for(...).unwrap_or_else(
/// baseline_caps)`). So `"win": {"write": null}` withdraws `deps` as well as `userHome` — and the
/// grant search has NO RUNG BELOW the baseline, so nothing ever measured a package without `deps`.
/// Seventeen cells went in as `null`. A guard that only checked `userHome` was gone would have
/// passed all of them.
///
/// It also subsumes the floor case that surfaced first: five of those cells carry `network: false`
/// already, so `null` left an entry granting NOTHING — and `catalog_v2::parse`'s own doc is
/// explicit that a present-but-empty entry is strictly TIGHTER than absence, because absence takes
/// the baseline. `deps` cannot be present and the cell still be empty.
#[test]
fn every_withdrawal_keeps_the_write_deps_the_base_profile_measured() {
    let catalog = shipped();
    let mut lost: Vec<String> = Vec::new();

    for (pkg, version, band, _) in WITHDRAWN {
        let caps = catalog
            .packages
            .get(*pkg)
            .unwrap_or_else(|| panic!("{pkg} has no catalog entry at all"))
            .grant_for(Some(version))
            .on(Platform::Windows);
        if !caps.write.covers(Scope::Deps) {
            lost.push(format!("{pkg}@{version} [band {band}]"));
        }
    }

    assert!(
        lost.is_empty(),
        "{} win32 cell(s) lost `write.deps`, which the base-profile arm that licensed the \
         withdrawal HAD. The search has no rung below the baseline, so its absence is unmeasured; \
         spell these `write: {{deps: true}}` rather than removing the write outright:\n  {}",
        lost.len(),
        lost.join("\n  ")
    );
}

/// The win32 real-home write and the whole-disk write are both gone.
#[test]
fn a_withdrawn_cell_grants_no_win32_home_write_and_no_whole_disk() {
    let catalog = shipped();
    // COLLECTED, not asserted per row: a re-bake moves the whole class at once, and a panic on the
    // first row reports 1 of 28 — which reads as an isolated typo rather than the systematic
    // restoration it is, and costs a rebuild per cell to enumerate.
    let mut wrong: Vec<String> = Vec::new();

    for (pkg, version, band, _) in WITHDRAWN.iter().chain(WITHDRAWN_ON_A_LADDER) {
        let caps = catalog
            .packages
            .get(*pkg)
            .unwrap_or_else(|| panic!("{pkg} has no catalog entry at all"))
            .grant_for(Some(version))
            .on(Platform::Windows);
        if matches!(caps.write, Reach::Disk) {
            wrong.push(format!(
                "{pkg}@{version} [band {band}] win: write is back to \"disk\""
            ));
        } else if caps.write.covers(Scope::UserHome) {
            wrong.push(format!(
                "{pkg}@{version} [band {band}] win: write.userHome restored"
            ));
        }
    }

    assert!(
        wrong.is_empty(),
        "{} win32 cell(s) re-widened. Each was re-measured on windows-latest at the base profile \
         against a binary carrying 50ec17043a and 46b623e352, and reproduced the control arm's \
         product; the archived v0.6.0 records a re-bake would restore predate both repairs:\n  {}",
        wrong.len(),
        wrong.join("\n  ")
    );
}

/// Egress survived the withdrawal, and so did the base profile's `write.deps` on every cell that
/// previously reached it through `disk`.
///
/// Asserted alongside the withdrawal because a re-bake that dropped BOTH would satisfy the test
/// above while breaking every downloader in the list. The base-profile rung the withdrawal rests on
/// GRANTS egress, so it measured nothing about whether egress is needed.
#[test]
fn a_withdrawn_cell_keeps_the_egress_it_already_had() {
    let catalog = shipped();
    let mut wrong: Vec<String> = Vec::new();

    for (pkg, version, band, network) in WITHDRAWN.iter().chain(WITHDRAWN_ON_A_LADDER) {
        let caps = catalog
            .packages
            .get(*pkg)
            .unwrap_or_else(|| panic!("{pkg} has no catalog entry at all"))
            .grant_for(Some(version))
            .on(Platform::Windows);
        if caps.network != *network {
            wrong.push(format!(
                "{pkg}@{version} [band {band}] win: network is {}, expected {network}",
                caps.network
            ));
        }
    }

    assert!(
        wrong.is_empty(),
        "{} win32 cell(s) moved on the egress axis, which no row here measured:\n  {}",
        wrong.len(),
        wrong.join("\n  ")
    );
}

/// THE CONTROL, and it is what keeps the two tests above from being vacuous.
///
/// Ten cells measured on the very same run were held back at first, because a standing pin asserted
/// their win32 home write from an earlier measurement. Seven have since moved, each together with
/// its pin. THREE STILL STAND, and they are the three named here, so this control keeps proving the
/// accessor reports `true` when a cell HAS the grant — without that, a `covers` that always returned
/// `false` would pass everything above.
///
/// Each of the three is held for a reason the same run produced, not for want of attention:
///
/// - `ibm_db@4.0.1` — the harness's OWN minimum retains the home write. Its two control runs
///   disagreed on 27 paths, so `search.mjs` escalated the unmeasured scope back into the verdict and
///   emitted `{"write": {"userHome": true}}` rather than `null`. Nothing was withdrawn to withdraw.
/// - `esbuild@0.17.18` and `@netlify/esbuild@0.14.25` — the arm was VACUOUS. Both installs printed
///   no script output at all in either arm and reproduced the control with zero paths missing,
///   because at those versions the platform binary arrives as an `optionalDependency` and
///   `install.js` takes its already-present branch. Both bands are catalogued from a version where
///   the download DOES happen (`0.11.23`, `0.13.6`), and `esbuild <0.17.19`'s own note already
///   records the same call for the same band: `0.16.17` "verified an empty grant but produced no red
///   arm at all, so the band keeps the stricter of the two answers".
#[test]
fn the_cells_held_back_by_a_standing_pin_still_grant_the_win32_home_write() {
    let catalog = shipped();
    for (pkg, version) in [
        ("ibm_db", "4.0.1"),
        ("esbuild", "0.17.18"),
        ("@netlify/esbuild", "0.14.25"),
    ] {
        assert!(
            catalog
                .packages
                .get(pkg)
                .unwrap_or_else(|| panic!("{pkg} has no catalog entry at all"))
                .grant_for(Some(version))
                .on(Platform::Windows)
                .write
                .covers(Scope::UserHome),
            "{pkg}@{version} lost its win32 home write, so the withdrawals above are no longer \
             testing anything. It is held back by a standing pin in another test; move it here \
             only together with that pin."
        );
    }
}

/// `appium-uiautomator2-driver`'s replacement capability, which is the whole reason its narrowing is
/// safe and which no other test in this file would notice going missing.
///
/// The band shipped `write: "disk"` on every platform. macOS and Linux withdrew it outright, and
/// Windows now withdraws it too — but the Windows ladder's minimum was `read.disk + network`, not
/// `network`, so dropping the read reach along with the write would take the cell BELOW the arm that
/// passed. The two reaches are not interchangeable: a write reach of `disk` clears the rule list and
/// flips `default_effect` to Allow, which is what makes `fs_confines` false and costs the
/// AppContainer token; a read reach of `disk` compiles `disk_minus_secrets_read_allows` as ordinary
/// allow entries, so the token is taken normally and `SECRET_READ_RELPATHS` stays out.
#[test]
fn the_ladder_cell_that_needed_a_read_reach_still_has_one() {
    let catalog = shipped();
    let caps = catalog
        .packages
        .get("appium-uiautomator2-driver")
        .expect("appium-uiautomator2-driver has a catalog entry")
        .grant_for(Some("0.11.0"))
        .on(Platform::Windows);

    assert!(
        matches!(caps.read, Reach::Disk),
        "appium-uiautomator2-driver@0.11.0 [band <8.4.0] win: read is {:?}, but the arm that \
         licensed dropping `write: \"disk\"` was `read.disk + network` — 16 of its 17 rungs went \
         red, `network` alone among them. Without the read reach this cell grants less than \
         anything measured.",
        caps.read
    );
    assert!(
        matches!(caps.write, Reach::None),
        "appium-uiautomator2-driver@0.11.0 [band <8.4.0] win: write is {:?}; the passing arm \
         carried no write scope at all, and macOS and Linux already ship that answer for this band.",
        caps.write
    );
}

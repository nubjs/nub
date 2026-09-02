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
//! was never measured at all. Every row below therefore lands on `{deps: true}`, never on nothing,
//! and `every_withdrawal_keeps_the_write_deps_the_base_profile_measured` holds that.
//!
//! ⛔ THE ARTIFACT PREDICATE ALONE DID NOT DECIDE ANY ROW. That predicate compares a digest of the
//! created-path list across the project and the store, and it is blind to a product that lands
//! outside both: on the same runner every `@pulumi/*` cell scored a digest PASS while its narrow
//! arm printed `There was an error installing the resource provider plugin` and the control
//! downloaded and unpacked it. Each row below was additionally required to reproduce the CONTROL
//! ARM'S SCRIPT OUTPUT line for line — `playwright`'s 116 MB Chromium fetch, `ibm_db`'s 27 MB
//! driver, `saucectl`'s `Installation succeeded`, `optipng pre-build test passed successfully`.
//! Four candidates measured on the same run are NOT here because that comparison caught them:
//! `gif2webp-bin` (`pre-build test failed` under the narrow arm, `passed` under the control),
//! `pngout-bin` (`Error: spawn UNKNOWN`), `netlify-cli` (`gyp info ok` under the control,
//! `gyp ERR! build error` under the narrow arm) and `@eth-optimism/sdk` (`Access is denied.`).
//!
//! ⛔ NETWORK IS RETAINED, NOT RE-MEASURED. The base-profile rung grants egress, so no row below
//! can say anything about whether its package needs it. Each cell keeps the `network` it already
//! had, and the second test asserts that so a re-bake cannot drop both at once.
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
    ("@azure-devops/mcp",           "2.8.0",             "<2.9.0",   false),
    ("@clerk/shared",               "4.14.0",            "<4.29.1",  false),
    ("@depot/cli",                  "0.0.1-cli.2.102.7", "default",  false),
    ("@hyperjump/json-schema",      "0.23.5",            "<1.17.8",  true),
    ("@hyperjump/json-schema-core", "0.28.4",            "<0.28.5",  true),
    ("@prisma/engines",             "7.9.0",             "<7.9.1",   true),
    ("@progress/kendo-licensing",   "1.11.3",            "default",  false),
    ("@sentry/cli",                 "3.6.1",             "<3.6.2",   true),
    ("chromedriver",                "152.0.2",           "default",  true),
    ("cwebp-bin",                   "8.0.0",             "default",  true),
    ("docxtemplater",               "3.31.3",            "<3.69.3",  true),
    ("dtrace-provider",             "0.8.8",             "default",  false),
    ("gifsicle",                    "7.0.1",             "default",  true),
    ("gifsicle",                    "7.0.0",             "<7.0.1",   true),
    ("jpeg-recompress-bin",         "7.0.0",             "default",  true),
    ("keccak",                      "3.0.3",             "<3.0.4",   true),
    ("node-jq",                     "6.3.1",             "default",  true),
    ("nodemon",                     "2.0.19",            "<3.1.14",  false),
    ("nx",                          "23.1.0",            "<23.1.1",  true),
    ("optipng-bin",                 "9.0.0",             "default",  true),
    ("optipng-bin",                 "8.1.0",             "<9.0.0",   true),
    ("pngquant-bin",                "9.0.0",             "default",  true),
    ("rc-editor-core",              "0.3.13",            "<0.8.10",  false),
    ("react-native-purchases",      "4.6.3",             "<10.7.1",  true),
    ("truffle",                     "5.11.5",            "default",  true),
    ("unicode",                     "0.6.1",             "<14.0.0",  true),
    ("ursa-optional",               "0.10.2",            "default",  true),
    ("zopflipng-bin",               "7.1.0",             "default",  true),
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

    for (pkg, version, band, _) in WITHDRAWN {
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

    for (pkg, version, band, network) in WITHDRAWN {
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
/// Ten cells measured on the very same run are deliberately NOT withdrawn, because a standing pin
/// asserts their win32 home write from an earlier measurement: `repaired_home_write_grants.rs`
/// holds eight, `macos_home_write_withdrawals.rs` holds `appium-uiautomator2-driver`, and
/// `linux_home_write_withdrawals.rs` holds `playwright`. Naming three of them here proves the
/// accessor reports `true` when a cell HAS the grant — without that, a `covers` that always
/// returned `false` would pass everything above.
#[test]
fn the_cells_held_back_by_a_standing_pin_still_grant_the_win32_home_write() {
    let catalog = shipped();
    for (pkg, version) in [
        ("playwright", "1.37.1"),
        ("appium-uiautomator2-driver", "0.11.0"),
        ("saucectl", "0.213.0"),
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

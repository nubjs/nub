//! The macOS `write.userHome` grants withdrawn once a jail-off control showed the write was free.
//!
//! ⛔ WHY THESE NARROWED, AND WHY A PATH MATCH IS THE WRONG TEST. `compiler::preset` grants
//! `$cache/nub/pm/tools/{npm-prefix,ms-playwright,electron-cache}` READ-WRITE to every jailed script
//! unconditionally and materializes each leaf, and `pm_engine::build_jail` unconditionally points
//! `electron_config_cache`/`ELECTRON_CACHE` and `PLAYWRIGHT_BROWSERS_PATH` at two of them. So a
//! confined download lands in a directory that is already writable for free, and the whole-home grant
//! buys the package nothing. The tempting check -- "is the observed path inside the leaf?" -- answers
//! NO and is misleading: an UNJAILED observation writes to `~/Library/Caches/{electron,ms-playwright}`,
//! which is the vendor default and a different path. Only running the jail answers it.
//!
//! ⛔ THE MEASUREMENT SHAPE, because an arm that goes green proves nothing on its own. Each cell was
//! run on four arms with a JAIL-OFF POSITIVE CONTROL and a RED NEGATIVE CONTROL. Jail off puts the
//! artefact in the real home; jail on puts the same content-addressed directory in the free leaf; the
//! arm granting only `network` reproduces it; and dropping `network` fails, losing a NAMED product.
//! `electron-chromedriver@43.2.0` is the clearest: `bin/icudtl.dat` is 10,735,358 bytes in the jail-off,
//! wide and drop arms and ABSENT in the red one.
//!
//! ⛔ THIS REVERSES THE 2026-09-01 macOS RESTORE FOR `electron-chromedriver`, and the three rows it
//! removed from `repaired_home_write_grants.rs` say so. That restore read the OBSERVE census's
//! `~/Library/Caches/electron` paths as real-home writes the drop arm could not have seen. Under the
//! jail those writes are the free leaf, so the census billed a write nub hands out. The same file had
//! ALREADY applied this reasoning to sibling `electron`; it did not reach `electron-chromedriver`
//! because that package's recorded path is the vendor default rather than the leaf itself. Only the
//! macOS half is reversed -- the win32 restore stands, and this file's control asserts it.
//!
//! ⛔ NOT EVERY CELL HERE IS THE TOOL-CACHE STORY. `appium-uiautomator2-driver` and
//! `azure-streamanalytics-cicd` narrow for a simpler reason: with the jail OFF and a real `$HOME`,
//! each writes ZERO files to the home, so the capability was granted for a write that does not occur.
//! Their artifact gates are not vacuous either, so the gate itself is the discriminator -- azure's red
//! arm scores 6/481 against 481/481 in both the green arm and the jail-off control.
//!
//! ⛔ ADDED 2026-09-01: `@shoelace-style/shoelace`, which IS the tool-cache story. Its `postinstall`,
//! read out of its packed tarball, is literally `npx playwright install`, so it shares its mechanism
//! with the playwright rows above: its `no-network` arm goes red with `ENOTFOUND` and an EMPTY
//! tool-cache leaf, while the drop arm fills that leaf with 7315 files across the same five browser
//! directories as the wide arm. Its `writePaths` is untouched and still carries
//! `Library/Caches/node-gyp`.
//!
//! ⛔ CONSIDERED TWICE AND NOT ADDED: `electron@default`. The second pass tried to narrow it to
//! `{deps, project}` rather than withdraw it, on the argument that none of the 25 versions at or
//! above 43.4.0 declares a `scripts` field. THAT ARGUMENT IS WRONG, and the reason generalises to
//! every band in this catalog: `Entry::grant_for` selects with `version_scope::applies`, which is
//! `semver::VersionReq::matches`, and a PRERELEASE never matches a plain `<X` bound -- pinned in
//! that module as `!applies("<0.13.0", "0.12.0-rc.1")`. So `<43.4.0` covers only the STABLE releases
//! below it, and every one of electron's 711 prereleases falls through to `default` alongside the 7
//! stable releases above the bound. Counted from the packument: `default` covers 718 versions, 665
//! of which declare an install hook -- `postinstall: node install.js`, back to `1.8.2-beta.1`.
//! Narrowing it would strip the grant from those 665. An under-grant is worse than an over-grant, so
//! the cell stays wide, and "the latest release dropped its script" is never on its own a reason to
//! touch a `default` band.
//!
//! ⛔ WHEN RE-CHECKING A no-script CLAIM, MIND THE DOCUMENT FLAVOUR -- do not distrust the field.
//! `hasInstallScript` lives ONLY in the abbreviated packument
//! (`Accept: application/vnd.npm.install-v1+json`) and `scripts` ONLY in the full one, so reading
//! either off the wrong document yields `undefined` for every version and manufactures a convincing
//! fake zero. That is how an earlier note here came to record `hasInstallScript` as "`false` for all
//! 1785 versions". Measured over all 1785 electron versions: absent from the full document every
//! time; present on 1691 in the abbreviated one and OMITTED WHEN FALSE on the other 94, so absent
//! there means false rather than unknown; and ZERO disagreements against the full document's
//! `scripts` under npm's own rule, `scripts` intersected with {preinstall, install, postinstall}.
//! Either field is trustworthy read from the document that carries it, and the abbreviated one is
//! far cheaper than unpacking tarballs. The research note `npm-corpus-data-sources` sets out both
//! forms and the content-negotiation trap that produces the wrong one.
//!
//! ⛔ THESE ARE HAND EDITS ON A GENERATED FILE, WHICH IS WHY THEY NEED PINNING. `build.rs` proves the
//! catalog parses and nothing more; it cannot know that a per-OS overlay says what a measurement said.
//! A re-bake from the archived records would restore all twelve, because the records were scored
//! before the tool-cache leaves were granted. This file is the counterpart to
//! `repaired_home_write_grants.rs` in the opposite direction: that one pins grants a re-bake would
//! WITHDRAW, this one pins withdrawals a re-bake would RESTORE. `linux_home_write_withdrawals.rs` is
//! the same shape for Linux.
//!
//! Reads the shipped bytes through `include_str!` rather than the runtime lookup, which consults a dev
//! override and an on-disk update tier first; the subject here is the file in this repository.
use nub_sandbox::catalog_v2::{Catalog, Platform, Scope};

fn shipped() -> Catalog {
    nub_sandbox::catalog_v2::parse(include_str!("../data/build-jail-catalog-v2.json"))
        .expect("the shipped catalog parses; build.rs fails the build otherwise")
}

/// One withdrawn cell: package, a version that RESOLVES to the band that was measured, that band's
/// label for the failure message, and what WINDOWS grants for `write.userHome` on the same cell. The
/// version is the one the arms actually ran, so the band it selects is the band that was measured
/// rather than one chosen by hand.
///
/// The last field records what WINDOWS grants on the same cell, two-sided ON PURPOSE: pinning it in
/// both directions catches a change applied too broadly AND an accidental widening.
/// `azure-streamanalytics-cicd` is `false` because its `win` overlay already withdrew `write`
/// outright before any of this, and recording that rather than dropping the row is what keeps it
/// under the same guard.
///
/// ⛔ THIS FIELD ONCE MEANT "the withdrawal is macOS-only, so eleven of these twelve must STILL
/// grant on Windows", AND THAT IS NO LONGER TRUE OF THREE ROWS. The `default` bands of
/// `electron-chromedriver`, `@playwright/browser-chromium` and `playwright-chromium` are now `false`
/// on their own Windows measurement. The reasoning at the head of this file — that what an unjailed
/// observation writes to the vendor default a JAILED run writes to the free tool-cache leaf — holds
/// on Windows too, because `redirect_electron_cache` / `redirect_playwright_browsers` are plain
/// env-var writes with no per-OS branch. What made Windows look different was a Windows-only defect:
/// the leaf's read-write grant is `FsOrigin::Speculative`, and `derive_grants`
/// (`backend/windows.rs`) DROPPED it when the path was absent, so the package's own `mkdir` hit the
/// deliberately read-only `tools` parent and the ladder escalated to the whole home. `46b623e352`
/// materializes the leaves during the compile. Re-measured on a `windows-latest` runner that PROVED
/// the leaves absent before every arm: the `{network}` arm reproduces the product inside the leaf —
/// 606 files and a 297,987,584-byte `chrome-win64/chrome.dll` for the playwright rows, the
/// `electron-cache` leaf for `electron-chromedriver` — while the empty arm loses it. The three
/// version-banded siblings below keep `true`: `latest` resolves to `default`, so nothing measured
/// them.
#[rustfmt::skip]
const WITHDRAWN: &[(&str, &str, &str, bool)] = &[
    ("electron-chromedriver",       "43.2.0", "default", false),
    ("electron-chromedriver",       "42.8.0", "<43.2.0", true),
    ("electron-chromedriver",       "31.7.7", "<32.3.3", true),
    ("@playwright/browser-webkit",  "1.62.1", "default", true),
    ("@playwright/browser-firefox", "1.62.1", "default", true),
    ("@playwright/browser-chromium","1.62.1", "default", false),
    ("@playwright/browser-chromium","1.61.1", "<1.62.1", true),
    ("playwright-chromium",         "1.62.1", "default", false),
    ("playwright-webkit",           "1.62.1", "default", true),
    ("appium-uiautomator2-driver",  "0.11.0", "<8.4.0",  true),
    ("azure-streamanalytics-cicd",  "4.0.0",  "default", false),
    ("@shoelace-style/shoelace",    "2.13.1", "default", true),
];

/// The macOS home write is gone and egress is not.
///
/// Egress is asserted alongside it because a re-bake that dropped BOTH would satisfy a
/// withdrawal-only assertion while breaking every one of these packages: each one's red arm was
/// `no-network`, so `network` is the capability these cells were measured to NEED.
#[test]
fn a_withdrawn_cell_grants_no_macos_home_write_and_keeps_its_egress() {
    let catalog = shipped();
    // COLLECTED, not asserted per row: a re-bake moves a whole class at once, and a panic on the
    // first row reports 1 of 9 -- which reads as an isolated typo rather than the systematic
    // restoration it is, and costs a rebuild per cell to enumerate.
    let mut wrong: Vec<String> = Vec::new();

    for (pkg, version, band, _) in WITHDRAWN {
        let entry = catalog
            .packages
            .get(*pkg)
            .unwrap_or_else(|| panic!("{pkg} has no catalog entry at all"));
        let caps = entry.grant_for(Some(version)).on(Platform::Macos);

        if caps.write.covers(Scope::UserHome) {
            wrong.push(format!(
                "{pkg}@{version} [band {band}] macos: write.userHome is back"
            ));
        }
        if !caps.network {
            wrong.push(format!(
                "{pkg}@{version} [band {band}] macos: network was withdrawn"
            ));
        }
    }

    assert!(
        wrong.is_empty(),
        "{} macOS cell(s) no longer match what their arms measured. Each was run with a jail-off \
         positive control and a red `no-network` arm: the home write is free under the jail and the \
         egress is necessary, so neither may move without a new measurement:\n  {}",
        wrong.len(),
        wrong.join("\n  ")
    );
}

/// Guards the ENUMERATION itself. Both tests above iterate `WITHDRAWN`, so emptying or trimming that
/// list makes them pass while asserting nothing -- a failure mode neither can see from the inside.
#[test]
fn every_measured_withdrawal_is_still_enumerated() {
    assert_eq!(
        WITHDRAWN.len(),
        12,
        "the withdrawal list changed size; a row may only leave it alongside a measurement that \
         restores the grant in the catalog"
    );
}

/// The control, and without it the test above passes on a catalog that granted nothing anywhere.
///
/// Two independent halves, because they fail for different reasons. WINDOWS: every withdrawn cell
/// carries exactly the `write.userHome` its OWN Windows measurement settled on -- nine of the twelve
/// still grant it, and a blanket removal would satisfy the assertion above while silently widening
/// the change to bands and packages nothing measured. macOS: `playwright@<1.62.1` is the sibling
/// whose refusal STANDS (its primary CDN is retired, so every arm including jail-off extracts the
/// same 212 KB stub and nothing is measurable), so it must still grant the home write -- proving the
/// macOS accessor still reports one when a cell has it.
#[test]
fn windows_matches_its_own_measurement_and_the_unmeasurable_sibling_keeps_its_grant() {
    let catalog = shipped();
    let mut lost: Vec<String> = Vec::new();

    for (pkg, version, band, win_keeps_home_write) in WITHDRAWN {
        let entry = catalog
            .packages
            .get(*pkg)
            .unwrap_or_else(|| panic!("{pkg} has no catalog entry at all"));
        let on_win = entry
            .grant_for(Some(version))
            .on(Platform::Windows)
            .write
            .covers(Scope::UserHome);
        if on_win != *win_keeps_home_write {
            lost.push(format!(
                "{pkg}@{version} [band {band}] win: write.userHome is {on_win}, expected \
                 {win_keeps_home_write}; each row pins the Windows grant its own measurement \
                 settled on, so Windows must not move until something re-measures THAT cell"
            ));
        }
    }

    // `playwright@1.31.0` resolves to `<1.62.1`, the band whose arms are degenerate in every venue
    // reached so far. An under-grant is worse than an over-grant, so it keeps the home write.
    if !catalog
        .packages
        .get("playwright")
        .expect("playwright has a catalog entry")
        .grant_for(Some("1.31.0"))
        .on(Platform::Macos)
        .write
        .covers(Scope::UserHome)
    {
        lost.push(
            "playwright@1.31.0 [band <1.62.1] macos: write.userHome withdrawn on a cell whose arms \
             produce the same stub at every rung, so nothing measured this"
                .to_string(),
        );
    }

    assert!(
        lost.is_empty(),
        "{} control(s) failed, so the withdrawal test above is no longer testing what it claims:\n  \
         {}",
        lost.len(),
        lost.join("\n  ")
    );
}

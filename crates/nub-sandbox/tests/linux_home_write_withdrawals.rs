//! The Linux `write.userHome` grants withdrawn once a five-arm ladder showed the write was free.
//!
//! ⛔ THE MEASUREMENT SHAPE, because a green arm proves nothing on its own. Each cell was run under
//! Landlock ABI 7 on five arms: a JAIL-OFF positive control, the shipped grant, the shipped grant
//! minus `network`, the shipped grant minus every reach over the user's home, and an empty grant as
//! the RED negative control. A cell is here only when the minus-home arm is GREEN and at least one
//! other arm went RED naming its own cause -- so the descent proved the retained capabilities
//! necessary rather than merely observing one arm pass.
//!
//! ⛔ AND A WIDE-VERSUS-NARROW HOME DIFFERENTIAL, which is the half that answers the objection
//! below. Comparing the arm's ENTIRE jail `$HOME` under the shipped grant against the same install
//! with the home reach dropped shows nothing the wider grant produced: 16 entries against 16 for
//! `@mui/x-telemetry`, 15 against 15 for `@pact-foundation/pact-node`, 18 against 18 for
//! `@pdftron/pdfnet-node`, each with zero files on either side.
//!
//! ⛔ THIS REVERSES THE 2026-09-01 LINUX HALF OF THE corpus-epoch-71 RESTORE, and the two rows it
//! removed from `repaired_home_write_grants.rs` say so. That restore argued the corpus artifact gate
//! walks only the package's own directory, so a write to the user's REAL home is invisible to the
//! drop arm and the narrowing was never earned. The argument is sound and it does not reach these
//! cells: the OBSERVE census that attributed those writes runs the script with the JAIL OFF, where
//! `$HOME` is the real home. Under the jail the same line resolves to the redirected private home.
//! `@mui/x-telemetry` is the clearest -- `postinstall/storage.js` reads
//! `XDG_CONFIG_HOME || os.homedir() + '/.config'`, the jail's env allowlist drops
//! `XDG_CONFIG_HOME`, and the file lands at `.cache/nub/jail-home/<hash>/.config/mui-x/config.json`.
//! So epoch 71 measured an UNJAILED write and inferred a jailed capability requirement from it.
//!
//! ⛔ NOT EVERY CELL HERE IS THAT STORY. The playwright family and `electron-chromedriver` narrow
//! for the tool-cache reason `macos_home_write_withdrawals.rs` sets out at length:
//! `pm_engine::build_jail` points `PLAYWRIGHT_BROWSERS_PATH` and `electron_config_cache` at
//! `$cache/nub/pm/tools/{ms-playwright,electron-cache}`, which `compiler::preset` grants read-write
//! to every jailed script unconditionally. `@shoelace-style/shoelace` shares it because its
//! `postinstall` is literally `npx playwright install`.
//!
//! ⛔ THESE ARE HAND EDITS ON A GENERATED FILE, WHICH IS WHY THEY NEED PINNING. `build.rs` proves
//! the catalog parses and nothing more; it cannot know that a per-OS overlay says what a measurement
//! said, so a re-bake from the archived records would restore all ten with no signal at all. This
//! file is the Linux counterpart to `macos_home_write_withdrawals.rs`.
//!
//! NOT HERE ON PURPOSE: `unrs-resolver`. Its Linux arms read as a clean narrowing, but the same
//! package was REFUTED on macOS -- the fixture never installed the platform optionalDependency, so
//! the install script took its missing-dependency workaround branch and died fetching from the
//! registry, which proves the fallback needs egress rather than that the package needs the home.
//! Whether the Linux fixture has the same flaw is unresolved, an under-grant is worse than an
//! over-grant, and the control below asserts it keeps the grant until that is settled.
//!
//! Reads the shipped bytes through `include_str!` rather than the runtime lookup, which consults a
//! dev override and an on-disk update tier first; the subject here is the file in this repository.
use nub_sandbox::catalog_v2::{Catalog, Platform, Scope};

fn shipped() -> Catalog {
    nub_sandbox::catalog_v2::parse(include_str!("../data/build-jail-catalog-v2.json"))
        .expect("the shipped catalog parses; build.rs fails the build otherwise")
}

/// One withdrawn cell: package, a version that RESOLVES to the band that was measured, that band's
/// label for the failure message, the write scopes the narrowing LEFT IN PLACE, and whether WINDOWS
/// still grants `write.userHome` there. The version is the one the arms actually ran.
///
/// The retained scopes are carried because the risk a narrowing runs is the opposite one: an
/// UNDER-grant, a package that stops building. Eight of these ten keep no write at all, and the two
/// that do had those scopes proved necessary by a red arm -- `@pact-foundation/pact-node`'s empty
/// arm fails `EACCES` opening the project's `.npmrc`, which names `write.project` directly.
///
/// The last field records what WINDOWS grants on the same cell, two-sided ON PURPOSE: pinning the
/// value in both directions is what makes an accidental WIDENING fail as loudly as a narrowing.
/// `@pact-foundation/pact-node` and `@pdftron/pdfnet-node` are `false` because their `win` overlays
/// already granted no write before any of this, and recording that rather than dropping the rows is
/// what keeps them under the same guard.
///
/// ⛔ THIS FIELD ONCE MEANT "the withdrawal is Linux-only, so Windows must not move in EITHER
/// direction", AND THAT IS NO LONGER TRUE OF THREE ROWS. `@playwright/browser-chromium`,
/// `playwright-chromium` and `electron-chromedriver` are `false` on their OWN Windows measurement
/// rather than by inheriting this one, and the Windows home write they used to carry was never a
/// capability need. The `$cache/nub/pm/tools/{ms-playwright,electron-cache}` leaf these packages are
/// redirected into is granted read-write as `FsOrigin::Speculative`, and `derive_grants`
/// (`backend/windows.rs`) DROPS such a rule when its path is absent -- so on any machine that had
/// not already run an unjailed install the leaf carried no grant, the package's own `mkdir` hit the
/// deliberately read-only `tools` parent, and the ladder escalated until the whole home worked.
/// `46b623e352` materializes the leaves during the compile. Re-measured on a `windows-latest` runner
/// that PROVED the three leaves absent before every arm: the `{network}` arm puts 606 files and a
/// 297,987,584-byte `chrome-win64/chrome.dll` into the free leaf with no home grant at all, while
/// the empty arm collapses it to 1 file. The other seven rows keep `true` because nothing has
/// measured them on Windows.
#[rustfmt::skip]
const WITHDRAWN: &[(&str, &str, &str, &[Scope], bool)] = &[
    ("@mui/x-telemetry",            "9.10.0",  "default", &[Scope::Deps, Scope::Project], true),
    ("@pact-foundation/pact-node",  "10.18.0", "default", &[Scope::Deps, Scope::Project], false),
    ("@pdftron/pdfnet-node",        "12.0.0",  "default", &[],                            false),
    ("@playwright/browser-chromium","1.62.1",  "default", &[],                            false),
    ("@playwright/browser-firefox", "1.62.1",  "default", &[],                            true),
    ("@playwright/browser-webkit",  "1.62.1",  "default", &[],                            true),
    ("@shoelace-style/shoelace",    "2.13.1",  "default", &[],                            true),
    ("electron-chromedriver",       "43.2.0",  "default", &[],                            false),
    ("playwright-chromium",         "1.62.1",  "default", &[],                            false),
    ("playwright-webkit",           "1.62.1",  "default", &[],                            true),
];

/// The Linux home write is gone, and neither egress nor the retained write scopes went with it.
///
/// Egress is asserted alongside the withdrawal because a re-bake that dropped BOTH would satisfy a
/// withdrawal-only assertion while breaking every one of these packages: each one's red arm named
/// `network`, so it is the capability these cells were measured to NEED.
#[test]
fn a_withdrawn_cell_grants_no_linux_home_write_and_keeps_what_its_arms_needed() {
    let catalog = shipped();
    // COLLECTED, not asserted per row: a re-bake moves a whole class at once, and a panic on the
    // first row reports 1 of 10 -- which reads as an isolated typo rather than the systematic
    // restoration it is, and costs a rebuild per cell to enumerate.
    let mut wrong: Vec<String> = Vec::new();

    for (pkg, version, band, keeps_write, _) in WITHDRAWN {
        let entry = catalog
            .packages
            .get(*pkg)
            .unwrap_or_else(|| panic!("{pkg} has no catalog entry at all"));
        let caps = entry.grant_for(Some(version)).on(Platform::Linux);

        if caps.write.covers(Scope::UserHome) {
            wrong.push(format!(
                "{pkg}@{version} [band {band}] linux: write.userHome is back"
            ));
        }
        if !caps.network {
            wrong.push(format!(
                "{pkg}@{version} [band {band}] linux: network was withdrawn"
            ));
        }
        for scope in *keeps_write {
            if !caps.write.covers(*scope) {
                wrong.push(format!(
                    "{pkg}@{version} [band {band}] linux: write.{} was withdrawn",
                    scope.as_str()
                ));
            }
        }
    }

    assert!(
        wrong.is_empty(),
        "{} Linux cell(s) no longer match what their arms measured. Each ran a five-arm ladder \
         with a jail-off positive control and an empty-grant red arm: the home write bought \
         nothing observable and everything else was proved necessary, so neither may move without \
         a new measurement:\n  {}",
        wrong.len(),
        wrong.join("\n  ")
    );
}

/// Guards the ENUMERATION itself. The test above iterates `WITHDRAWN`, so emptying or trimming that
/// list makes it pass while asserting nothing -- a failure mode it cannot see from the inside.
#[test]
fn every_measured_linux_withdrawal_is_still_enumerated() {
    assert_eq!(
        WITHDRAWN.len(),
        10,
        "the withdrawal list changed size; a row may only leave it alongside a measurement that \
         restores the grant in the catalog"
    );
}

/// The control, and without it the test above passes on a catalog that granted nothing anywhere.
///
/// Two independent halves, because they fail for different reasons. WINDOWS: every withdrawn cell
/// holds exactly the `write.userHome` it held before -- the withdrawal was Linux-only, and a
/// blanket removal would satisfy the assertion above while silently widening the change. LINUX: two
/// siblings must STILL grant the home there, which is what proves the Linux accessor reports one
/// when a cell has it. `unrs-resolver` is the contested cell held back from this withdrawal, and
/// `windows-build-tools` carries `write:"disk"`, so it also exercises the `Reach::Disk` arm of
/// `covers` rather than only the scope-set arm.
#[test]
fn the_withdrawal_is_linux_only_and_the_held_siblings_keep_their_grants() {
    let catalog = shipped();
    let mut lost: Vec<String> = Vec::new();

    for (pkg, version, band, _, win_keeps_home_write) in WITHDRAWN {
        let on_win = catalog
            .packages
            .get(*pkg)
            .unwrap_or_else(|| panic!("{pkg} has no catalog entry at all"))
            .grant_for(Some(version))
            .on(Platform::Windows)
            .write
            .covers(Scope::UserHome);
        if on_win != *win_keeps_home_write {
            lost.push(format!(
                "{pkg}@{version} [band {band}] win: write.userHome is {on_win}, expected \
                 {win_keeps_home_write}; the withdrawal was Linux-only and Windows must not move \
                 in either direction"
            ));
        }
    }

    for (pkg, version, why) in [
        (
            "unrs-resolver",
            "1.12.2",
            "its Linux arms read as a narrowing but the macOS run refuted the same package, so it \
             is held until that is settled",
        ),
        (
            "windows-build-tools",
            "0.1.8",
            "it carries `write:\"disk\"`, which no measurement has touched",
        ),
    ] {
        if !catalog
            .packages
            .get(pkg)
            .unwrap_or_else(|| panic!("{pkg} has no catalog entry at all"))
            .grant_for(Some(version))
            .on(Platform::Linux)
            .write
            .covers(Scope::UserHome)
        {
            lost.push(format!(
                "{pkg}@{version} linux: write.userHome was withdrawn, but {why}"
            ));
        }
    }

    assert!(
        lost.is_empty(),
        "{} control(s) failed, so the withdrawal test above is no longer testing what it \
         claims:\n  {}",
        lost.len(),
        lost.join("\n  ")
    );
}

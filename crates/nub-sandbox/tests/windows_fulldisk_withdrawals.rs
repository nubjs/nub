//! The win32 `write: "disk"` grants withdrawn once a post-repair binary re-measured them at zero.
//!
//! ⛔ WHY THESE THREE WERE EVER WIDE, AND WHY IT WAS NEVER A WRITE. All three are `bin-wrapper`
//! downloaders, and every win32 record backing their `fullDisk` rung was measured 2026-07-31/08-01
//! on nub v0.6.0 -- `grant-matrix-win32-x64.ndjson`, `-fixedfloor` and `-tier2`, whose own
//! `measured_at` and `nub_version` fields say so. `50ec17043a` landed 2026-08-05, AFTER all of them.
//! It makes the AppContainer profile a leaf of the ancestor chain, so the intermediate
//! `…\AppData\Local\Packages` finally carries a traverse ace for the container. Before it, the child
//! could WRITE its temp and not RESOLVE it: `realpathSync(os.tmpdir())` died
//! `EPERM: lstat '…\AppData\Local\Packages'`, which `temp-dir` calls AT MODULE LOAD beneath
//! `tempfile` -> `download`/`decompress` -> `bin-build`/`bin-wrapper`. Every rung below `fullDisk`
//! failed for them, and `fullDisk` passed only because that rung declines the LowBox token
//! altogether (`backend/windows.rs`, `policy.build_jail && !confine_fs`). So the rows were stale by
//! construction, and the grant was curing a resolution fault by removing the sandbox.
//!
//! ⛔ THE RE-MEASUREMENT, AND ITS NEGATIVE CONTROL. Re-run on `windows-latest` against ONE binary
//! built from a tree containing the repair, with the three pinned node-gyp positive controls
//! (`dtrace-provider@0.8.8`, `bigint-buffer@1.1.5`, `websocket@1.0.31`) green on their `no_grants`
//! cell, so a toolchain-shaped failure on any other row would be that package's property:
//!
//!   jpegtran-bin@7.0.0     NEEDS-FULL-DISK -> NEEDS-NOTHING
//!   mozjpeg@8.0.0          NEEDS-FULL-DISK -> NEEDS-NOTHING
//!   hugo-bin@0.161.0       NEEDS-FULL-DISK -> NEEDS-NOTHING
//!
//! The NEGATIVE control rode the same run and the same binary: `tree-sitter-kotlin@0.3.8` passes
//! its jail-off cell and FAILS `no_grants`, `both` and `full_disk` alike (`MSBuild.exe` exit 1,
//! scored "not a filesystem need"). A rung that can still fail a package is a rung that was
//! enforcing, so the three passes above are not the vacuous kind an unenforced jail produces.
//! That is also why `tree-sitter-kotlin` KEEPS its `write: "disk"` and is the control below rather
//! than a fourth withdrawal -- nothing here measured a filesystem need it does not have.
//!
//! ⛔ `hugo-bin` WAS PREVIOUSLY READ AS AN EXEC FAILURE, and that reading does not survive the
//! evidence. Its recorded line is `The "…\vendor\hugo.exe" binary doesn't seem to work correctly`,
//! which is `bin-wrapper`'s generic catch-all around running the downloaded binary: it wraps ANY
//! error, the `EPERM` included, so it never distinguished an exec denial from the resolution fault
//! its two siblings hit. It re-measures at zero alongside them.
//!
//! ⛔ NETWORK IS RETAINED, NOT MEASURED-NECESSARY. The re-run scores all three NEEDS-NOTHING on
//! win32, but macOS and Linux both measure them NEEDS-EGRESS, and one platform's pass is not grounds
//! to drop a capability the other two measured. An under-grant is worse than an over-grant, so the
//! `network: true` on each `default` band is left exactly as it was.
//!
//! ⛔ THESE ARE HAND EDITS ON A GENERATED FILE, WHICH IS WHY THEY NEED PINNING. `build.rs` proves the
//! catalog parses and nothing more; it cannot know that a per-OS overlay says what a measurement
//! said. A re-bake from the archived records would restore all three, because those records were
//! scored on v0.6.0 before the repair existed. Same shape as
//! `macos_home_write_withdrawals.rs` / `linux_home_write_withdrawals.rs`, on the win32 axis.
//!
//! Reads the shipped bytes through `include_str!` rather than the runtime lookup, which consults a
//! dev override and an on-disk update tier first; the subject here is the file in this repository.
use nub_sandbox::catalog_v2::{Catalog, Platform, Reach, Scope};

fn shipped() -> Catalog {
    nub_sandbox::catalog_v2::parse(include_str!("../data/build-jail-catalog-v2.json"))
        .expect("the shipped catalog parses; build.rs fails the build otherwise")
}

/// One withdrawn cell: package, and the version whose band the arms actually ran, so the band the
/// test selects is the band that was measured rather than one chosen by hand. All three sit on
/// `default`, which is where `latest` resolves.
#[rustfmt::skip]
const WITHDRAWN: &[(&str, &str)] = &[
    ("jpegtran-bin", "7.0.0"),
    ("mozjpeg",      "8.0.0"),
    ("hugo-bin",     "0.161.0"),
];

#[test]
fn a_withdrawn_cell_grants_no_win32_write_and_keeps_its_egress() {
    let catalog = shipped();
    // COLLECTED, not asserted per row: a re-bake moves the whole class at once, and a panic on the
    // first row reports 1 of 3 -- which reads as an isolated typo rather than the systematic
    // restoration it is.
    let mut wrong: Vec<String> = Vec::new();

    for (pkg, version) in WITHDRAWN {
        let entry = catalog
            .packages
            .get(*pkg)
            .unwrap_or_else(|| panic!("{pkg} has no catalog entry at all"));
        let caps = entry.grant_for(Some(version)).on(Platform::Windows);

        if !caps.write.is_none() {
            wrong.push(format!(
                "{pkg}@{version} win32: write is back as {:?}",
                caps.write
            ));
        }
        if !caps.network {
            wrong.push(format!("{pkg}@{version} win32: network was withdrawn"));
        }
    }

    assert!(
        wrong.is_empty(),
        "{} win32 cell(s) no longer match what the re-measurement scored. Each re-measured \
         NEEDS-NOTHING on a binary containing 50ec17043a, on a run whose three node-gyp positive \
         controls passed ungranted and whose tree-sitter-kotlin negative control still failed the \
         same rung. `write: \"disk\"` on win32 declines the LowBox token, so restoring one gives \
         the package back the whole filesystem AND unconfined egress:\n  {}",
        wrong.len(),
        wrong.join("\n  ")
    );
}

#[test]
fn every_measured_withdrawal_is_still_enumerated() {
    assert_eq!(
        WITHDRAWN.len(),
        3,
        "the withdrawal list changed size; a row may only leave it alongside a measurement that \
         restores the grant in the catalog"
    );
}

/// The control, and without it the test above passes on a catalog that granted nothing anywhere.
///
/// Two independent halves, because they fail for different reasons. `tree-sitter-kotlin` is the
/// sibling measured in the SAME run on the SAME binary whose `write: "disk"` STANDS -- it fails
/// every jailed rung including `full_disk`, so nothing measured a filesystem need to withdraw. It
/// must still report `Reach::Disk` on win32, which is what proves the accessor reports a full-disk
/// grant when a cell has one. The withdrawn three must still carry `write: "disk"` on their
/// `default` band underneath: the change is a `win` overlay, and a blanket edit of `default` would
/// satisfy the assertion above while silently moving macOS and Linux too.
#[test]
fn the_withdrawal_is_win32_only_and_the_failing_sibling_keeps_its_grant() {
    let catalog = shipped();

    let kotlin = catalog
        .packages
        .get("tree-sitter-kotlin")
        .expect("tree-sitter-kotlin has no catalog entry at all");
    assert_eq!(
        kotlin.grant_for(Some("0.3.8")).on(Platform::Windows).write,
        Reach::Disk,
        "tree-sitter-kotlin@0.3.8 lost its win32 write: \"disk\". It fails no_grants, both AND \
         full_disk on the same binary that cleared the other three, so no measurement here \
         withdrew it -- and with it gone this file's main assertion could pass vacuously."
    );

    let mut lost: Vec<String> = Vec::new();
    for (pkg, version) in WITHDRAWN {
        let entry = catalog.packages.get(*pkg).unwrap();
        for platform in [Platform::Macos, Platform::Linux] {
            let caps = entry.grant_for(Some(version)).on(platform);
            if caps.write.covers(Scope::Project) {
                lost.push(format!(
                    "{pkg}@{version} {platform:?}: gained a write the win32 edit should not have \
                     touched"
                ));
            }
            if !caps.network {
                lost.push(format!(
                    "{pkg}@{version} {platform:?}: lost its measured egress"
                ));
            }
        }
    }
    assert!(
        lost.is_empty(),
        "the win32 withdrawal reached another platform:\n  {}",
        lost.join("\n  ")
    );
}

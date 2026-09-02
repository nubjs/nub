//! A cell that grants no `write.deps` while ALSO denying egress is below the base profile on both
//! axes at once, with nothing anywhere else to compensate — a grant no arm the search can reach
//! ever produced.
//!
//! ⛔ THE SEAM THIS FILE EXISTS TO CLOSE, and it is the third instance of one recurring hole: a
//! guard whose predicate silently excuses the cell it was written to catch.
//!
//!   * [`baseline_egress_withdrawals`] gates on `caps.write.covers(Scope::Deps)`. A cell whose
//!     write is NOTHING does not cover `Deps`, so the very cells that withdraw the most are the
//!     ones it skips.
//!   * `default_band_grants::no_cell_denies_everything_unless_its_package_runs_no_lifecycle_hook`
//!     gates on `Caps::widens_nothing()`, which requires `write_paths` to be EMPTY. A cell that
//!     promotes even one path widens something, so it is skipped there too.
//!   * `windows_base_profile_withdrawals` asserted `caps.widens_nothing()`, an ALL-AXIS
//!     conjunction, so 12 win32 cells with `network: true` passed a test whose own doc comment
//!     stated the invariant they broke (repaired in 5c53cfb53f).
//!
//! Between the first two, 138 cells fell through: they grant no `write.deps`, deny egress, and
//! widen something (usually a `writePaths` promotion) — so the egress test skipped them for want
//! of a deps write and the empty-cell test skipped them for having a promotion. 120 were repaired
//! and 18 remain, each licensed below. This file selects on the WRITE SCOPE alone, so a promotion
//! cannot hide a cell from it.
//!
//! ⛔ THE PREDICATE IS `!covers(Scope::Deps)`, NOT `write.is_none()`, and the difference is not
//! cosmetic. A cell whose write is `{project}` or `{userHome}` ALONE also hands out no deps write,
//! so it sits under the base profile just as a null one does while looking populated. Three of the
//! repairs — `jest-preview <0.3.3` on macOS and Linux, `netlify-cli default` on win32 — were
//! reachable only through that spelling.
//!
//! ⛔ WHY THIS IS AN UNDER-GRANT AND NOT MERELY A TIGHT ONE. Resolution is
//! `catalog_override::v2_grant_for(..).unwrap_or_else(|| baseline_caps())`
//! (`compiler/preset.rs`), so a package with NO ENTRY takes
//! [`baseline_caps`](nub_sandbox::catalog_v2::baseline_caps) — `write: {deps}`, `network: true`,
//! plus the promotion list. AN ENTRY REPLACES THAT WHOLE, with no merging, so an entry can grant
//! strictly LESS than having no entry at all. The grant search's cheapest rung, state 0, is
//! spelled as exactly that absent entry (`harness/search.mjs`), which makes the base profile the
//! floor: there is no rung beneath it, so nothing the search returns can license going under it.
//!
//! ⛔ THE TRAP THAT PRODUCED THESE CELLS, recorded because it is not visible from the catalog. The
//! 54-state space in `harness/states.mjs` is read x write x network and has NO `writePaths` axis —
//! `grantForState` reads `state.atoms` alone. So a record whose grant is `{"writePaths": [...]}`
//! carries no state atoms at all: it is a STATE-0 pass with a collator-attached promotion, and it
//! licenses the base profile PLUS that promotion. Reading its absent `network` and `write` keys as
//! measured refusals inverts the finding, and that is what produced the macOS and Linux half of
//! this population. The win32 re-measurement campaign read the same shape correctly, and
//! `@mui/x-telemetry`'s own note says why: "write.deps is retained because the base profile IS
//! catalog_v2::baseline_caps(), which carries it, and the search has no rung below it."
//!
//! The win32 half arrived by a different route and needs no misreading to explain: no win32 record
//! carries an `eventLog` AT ALL (0 of 2,270, against 1,912/2,293 on darwin and 2,058/2,324 on
//! linux), and every win32 record in this population verdicts `BROKEN-WITHOUT-JAIL-TOO` or
//! `HARNESS-TIMEOUT` — the package failed to install with the jail OFF. Nothing was measured
//! there, so nothing licenses a withdrawal, and the baseline is what an unmeasured cell gets.
//!
//! ⛔ WHAT IS DELIBERATELY OUT OF SCOPE, and it is larger than what is in it. A cell that grants no
//! `write.deps` but DOES carry egress is also under the base profile on the write axis, and there
//! are 550 of those. They are a separate population needing their own per-cell evidence, so this
//! predicate stops at the cells that are under the baseline on BOTH axes at once — where no
//! compensating capability anywhere can explain the shape. Widening the predicate to cover all 550
//! would turn this into a failing assertion about work nobody has done yet.
//!
//! Reads the shipped bytes through `include_str!`, like its sibling tests: the subject is the file
//! in this repository, not whatever a dev override or update tier resolves to at runtime.
use nub_sandbox::catalog_v2::{Catalog, Platform, Scope};

fn shipped() -> Catalog {
    nub_sandbox::catalog_v2::parse(include_str!("../data/build-jail-catalog-v2.json"))
        .expect("the shipped catalog parses; build.rs fails the build otherwise")
}

/// Cells whose BAND covers no version that executes anything, so the jail never compiles a grant
/// for them and what the cell says is inert.
///
/// ⛔ THE LICENCE IS "EXECUTES NOTHING", VERIFIED PER COVERED VERSION, NOT PER PACKAGE. A cell is a
/// BAND: `Entry::grant_for` picks the NARROWEST matching `<X` bound, and a prerelease never
/// matches a plain `<X` bound at all, so `default` catches every prerelease plus everything above
/// the highest bound. Each row below was enumerated over exactly the versions its own band covers,
/// from the FULL packument — `scripts` lives only there — and the hook predicate is
/// `aube_scripts::has_dep_lifecycle_work`: any of preinstall/install/postinstall, OR a bare
/// `binding.gyp` in the PACKED TARBALL driving an implicit `node-gyp rebuild`.
///
/// ⛔ THE REGISTRY'S `gypfile` FLAG IS NOT THE INSTRUMENT and disagrees with the tarball in both
/// directions — `fsevents@2.3.3` sets `gypfile: true` and ships no `binding.gyp` at all, only a
/// prebuilt `fsevents.node`. So `binding.gyp` was read from the packed tarball, and the reader was
/// controlled against packages that certainly ship one (`better-sqlite3`, `keccak`, `oniguruma`)
/// before any absence here was believed.
const BAND_EXECUTES_NOTHING: &[(&str, &str, &str)] = &[
    // 93 covered releases, 0 declaring preinstall/install/postinstall, 0 shipping `binding.gyp`
    // over a 7-version tarball sample spanning 0.0.3 to 21.0.5. The covered count independently
    // reproduces the figure this band's own note reached from a full tarball read.
    ("angularx-qrcode", "<22.0.1", "macos"),
    ("angularx-qrcode", "<22.0.1", "linux"),
    // 43 covered releases, 0 hooked, 0 with `binding.gyp` over a 7-version sample. The published
    // tarballs hold one or two files — the package is a stub that grew its real payload only at
    // and above 1.18.18, which the `default` band covers.
    ("@opencode-ai/cli", "<1.18.18", "macos"),
];

/// Cells that REPRODUCE the arm that passed: a live corpus record at a version the band covers, on
/// this platform, whose winning state is non-empty, carries no `network`, and carries no
/// `write.deps` either. The search reached that state, ran the real script under it, and passed —
/// so the cell is the measurement rather than a withdrawal from it.
///
/// ⛔ CHECKED AS A UNION OVER THE BAND, NOT AGAINST ONE RECORD. A cell answers for every version
/// its band covers, so the bar is the union of every live in-band record's winning state. Three
/// cells that looked identical to these failed exactly there — `jest-preview <0.3.3` on macOS and
/// Linux and `netlify-cli default` on win32 each carry a `{project}` or `{userHome}` state for one
/// version while ANOTHER covered version's record is a state-0 pass, which demands the whole base
/// profile. They were repaired rather than licensed.
///
/// ⛔ A STATE-0 RECORD CAN NEVER APPEAR HERE. Its grant names no atoms, so it is the base profile
/// and licenses nothing below it; see the module doc on `grantForState`.
const CELL_REPRODUCES_THE_PASSING_ARM: &[(&str, &str, &str)] = &[
    ("@cypress/snapshot", "default", "macos"),
    ("@cypress/snapshot", "default", "linux"),
    ("@cypress/snapshot", "default", "win"),
    ("@depot/cli", "default", "linux"),
    ("@sap/hana-client", "default", "macos"),
    ("@sap/hana-client", "default", "linux"),
    ("angularx-qrcode", "default", "win"),
    ("egg-ci", "default", "macos"),
    ("egg-ci", "default", "linux"),
    ("egg-ci", "default", "win"),
    ("nodemon", "<3.1.14", "linux"),
    ("openclaw", "default", "win"),
    ("rc-editor-core", "<0.8.10", "linux"),
];

/// Cells whose NARROW ARM ran at exactly this grant — no write scope and no egress — and
/// reproduced the control arm. The instrument is the real-home differential pinned in
/// [`linux_home_write_withdrawals`]`::WITHDRAWN_REAL_HOME`: each was run twice on a Landlock ABI 7
/// host, control against narrow, with a pristine real home per arm, and the narrow arm reproduced
/// the control's real-home tree entry for entry, its installer output and its artefact gate. Both
/// were observed at `peers: 0`, which is why that file pins their `network` TWO-SIDED rather than
/// asserting it true.
///
/// ⛔ THIS IS THE ONE LICENCE CATEGORY THAT IS NOT A CORPUS RECORD, and it outranks one. A corpus
/// state-0 record exists in-band for both of these, and read alone it would demand the whole base
/// profile here. A later hand measurement on a real kernel beats an earlier generator bake: these
/// two were repaired on that record and then REVERTED once the differential was found. When the
/// two disagree, the arm that actually ran wins.
const MEASURED_AT_NO_WRITE_NO_EGRESS: &[(&str, &str, &str)] = &[
    ("@clerk/shared", "<4.29.1", "linux"),
    ("netlify-cli", "default", "linux"),
];

fn platform_key(platform: Platform) -> &'static str {
    match platform {
        Platform::Macos => "macos",
        Platform::Linux => "linux",
        Platform::Windows => "win",
    }
}

#[test]
fn no_cell_withdraws_the_base_profile_write_scope_while_also_denying_egress() {
    let catalog = shipped();
    let licensed: std::collections::BTreeSet<(&str, &str, &str)> = BAND_EXECUTES_NOTHING
        .iter()
        .chain(CELL_REPRODUCES_THE_PASSING_ARM)
        .chain(MEASURED_AT_NO_WRITE_NO_EGRESS)
        .copied()
        .collect();

    let mut offending: Vec<String> = Vec::new();
    let mut licence_used: std::collections::BTreeSet<(&str, &str, &str)> =
        std::collections::BTreeSet::new();
    let mut selected = 0usize;

    for (name, entry) in &catalog.packages {
        let bands = std::iter::once(("default", &entry.default))
            .chain(entry.versions.iter().map(|b| (b.range.as_str(), &b.grant)));
        for (range, grant) in bands {
            for platform in [Platform::Macos, Platform::Linux, Platform::Windows] {
                let caps = grant.on(platform);
                // `Reach::Disk` covers every scope, so asking for `Deps` alone also admits the
                // whole-disk spelling and the predicate never has to name the representation. It
                // does NOT require the write to be empty: `{project}` and `{userHome}` fail this
                // too, and three of the repairs were reachable only that way.
                if caps.write.covers(Scope::Deps) || caps.network {
                    continue;
                }
                // A cell that widens NOTHING is the other test's population, licensed there by
                // package name in `default_band_grants::EXECUTES_NOTHING`. Selecting it here too
                // would fork one licence across two files and let them drift.
                if caps.widens_nothing() {
                    continue;
                }
                selected += 1;
                let key = (name.as_str(), range, platform_key(platform));
                match licensed.get(&key) {
                    Some(hit) => {
                        licence_used.insert(*hit);
                    }
                    None => offending.push(format!(
                        "{name} [band {range}] on {}",
                        platform_key(platform)
                    )),
                }
            }
        }
    }

    // CONTROL 1 — the walk selects cells at all. Without it, a predicate that stopped matching (a
    // renamed scope, an emptied `packages`, a `covers` that answers true unconditionally) would
    // pass this test by examining nothing, and would do it silently.
    assert!(
        selected > 0,
        "control failed: not one cell in the shipped catalog withdraws the base profile's write \
         scope while denying egress, so this test cannot distinguish a repaired catalog from a \
         broken traversal"
    );

    // CONTROL 2 — every licence is live. A licensed cell that no longer matches is a DEAD entry,
    // and a dead entry silently pre-approves the next withdrawal that cell acquires.
    let dead: Vec<String> = licensed
        .iter()
        .filter(|k| !licence_used.contains(*k))
        .map(|(n, b, o)| format!("{n} [band {b}] on {o}"))
        .collect();
    assert!(
        dead.is_empty(),
        "{} licensed cell(s) no longer withdraw the base profile's write scope while denying \
         egress, so the licence is dead and would pre-approve a future under-grant. Drop them:\n  \
         {}",
        dead.len(),
        dead.join("\n  ")
    );

    assert!(
        offending.is_empty(),
        "{} cell(s) grant NO write scope and ALSO deny egress, which is strictly less than having \
         no catalog entry at all -- an entry replaces `baseline_caps()` whole, and that baseline \
         carries `write: {{deps}}` and `network: true`. The grant search has no rung below the \
         base profile, so no arm ever passed here: a state-0 record spells its pass as an ABSENT \
         ENTRY, and its missing `write`/`network` keys are the baseline's, not a measured refusal. \
         A native build needs the deps write for its output and the egress for its headers, so \
         this breaks packages rather than merely tightening them. Spell the cell \
         `\"write\": {{\"deps\": true}}` and `\"network\": true` in its per-OS block, or license it \
         with the per-covered-version enumeration that shows its band executes nothing:\n  {}",
        offending.len(),
        offending.join("\n  ")
    );
}

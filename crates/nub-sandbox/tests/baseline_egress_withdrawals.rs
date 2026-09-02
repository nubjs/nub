//! A cell that carries the baseline's write and withdraws the baseline's egress is a grant no arm
//! ever reached — unless a measurement withdrew the egress on purpose.
//!
//! ⛔ THE MECHANISM, one axis over from [`windows_base_profile_withdrawals`] and from
//! `no_cell_denies_everything_unless_its_package_runs_no_lifecycle_hook`. The grant search walks
//! states in ASCENDING COST and stops at the first pass (`harness/states.mjs`). Its cheapest rung,
//! state 0, is spelled as NO ENTRY for the package (`harness/search.mjs`: "State 0 is the BASE
//! PROFILE, and the catalog spells that as NO ENTRY"), and an absent package takes
//! [`baseline_caps`](nub_sandbox::catalog_v2::baseline_caps) — `network: true`, `write: {deps}`,
//! and the promotion list. So a record whose grant is `{}` means "the BASELINE sufficed", and the
//! arm that passed CARRIED EGRESS. There is no rung below the baseline, so such a record licenses
//! withdrawing the real-home write and the whole-disk write, and NOTHING MORE — in particular it
//! does not license withdrawing `network`, exactly as it does not license withdrawing `write.deps`.
//!
//! ⛔ WHY A LICENCE LIST RATHER THAN A BLANKET RULE. Egress denials that a measurement genuinely
//! reached DO exist, and they are the exfiltration axis, so re-widening them has a real cost. What
//! survives as a licence is the COLD-INSTALL SWEEP (`tests/jail-acceptance/cold-network-sweep.sh`),
//! which loads the shipped catalog, checks the jail actually ran, and files an rc=0 install as
//! SUSPECT anyway when the log carries `getaddrinfo`/`ENOTFOUND` or shows the tried-then-compiled-
//! from-source pair — the silent-fallback shape an exit code cannot see.
//!
//! ⛔⛔ THE SECOND LICENCE CLASS IS WITHDRAWN, AND 45 CELLS CAME BACK TO THE BASELINE WITH IT. It
//! read: "a corpus record whose grant is NON-EMPTY and carries no `network`, meaning the observed
//! synthesis was verified with egress denied and passed." That describes what the harness did
//! accurately, and still licenses nothing, for four reasons that compound:
//!
//!   * EVERY record backing those cells carries `arms-unfalsifiable`. `d0179017e5` settled what
//!     that means for a narrowing — "the arm outcomes could not have gone red, so a passing arm is
//!     not evidence that the narrower grant suffices" — and set the standing three-term rule:
//!     verdict MINIMUM, `verifiedBy: "synth"`, and NO `arms-unfalsifiable` note.
//!   * `record.mjs` later refined that: a record flagged `gate-vacuous` ALONE still has its exit
//!     code as a live detector, so a RED sibling descent arm can license the narrowing after all.
//!     Not one of the 93 records carries `falsifiabilityReasons` or `descentRedArm` at all, so none
//!     reaches the refined bar either — not because they fail it, but because the instruments that
//!     would answer it had not been written when they were taken.
//!   * NO arm ever dropped egress. Across all 93, every `overPredictedBy` entry is `no-write-deps`
//!     or `no-write-project`; `network` appears in none. The absence of `network` from these grants
//!     is an absence in the OBSERVE SYNTHESIS — the run saw no socket — and not a capability some
//!     arm removed and then re-verified without.
//!   * A CELL IS A BAND, and these bands reach far past the versions that were measured.
//!     `@prisma/client <7.9.1` covers 199 published versions, 186 of them script-bearing, from
//!     three measurements. The `ttf2woff2`, `tree-sitter-cpp`, `tree-sitter-ruby`,
//!     `dtrace-provider` and `wrtc` bands are node-gyp / `node-pre-gyp` / `prebuild-install`
//!     builds that fetch headers or a prebuilt archive; `rc-editor-core <0.8.10` runs
//!     `typings install`; `@hyperjump/json-pointer <1.1.2` and `@hyperjump/pact <1.4.0` contain
//!     versions whose postinstall is `npx rimraf dist`, which reaches the registry when `rimraf`
//!     is absent; and `vnu-jar`'s postinstall is `node vnu-java-downloader.js`. `default`
//!     additionally catches every PRERELEASE and every version published after the measurement.
//!
//! Two venue facts sharpen the same conclusion per platform. On macOS every corpus record carrying
//! an event log carries `events-lost` with it (1,912 of 1,912), and `lifecyclePids` is 1 on 1,460
//! of them where Linux records 8–13 for the same package at the same version — so a macOS
//! synthesis cannot support an ABSENCE claim about what the script reached for. On Windows no
//! record carries an event log at all (0 of 2,270), so the same check cannot be run there.
//!
//! An over-grant on this axis costs a package the jail would otherwise have confined a little more
//! tightly. An under-grant costs the install. The baseline is what an uncatalogued package already
//! gets, so returning a cell to it is the direction that cannot break anyone.
//!
//! ⛔ THE GATE COVERS `write.project` TOO, and it did not before. Eight cells carried
//! `write: {deps, project}` while denying egress and were invisible to this walk, because the
//! predicate required `!covers(Scope::Project)`. A WIDER write is never a reason the egress
//! question is settled, so excluding those cells hid exactly the shape this file exists to catch.
//! `Scope::UserHome` stays excluded: a cell reaching the real home is a different measurement
//! lineage, pinned by the home-write suites.
//!
//! A cell whose only same-platform evidence is a record with `lifecyclePids == 0` is NOT licensed:
//! no lifecycle script ran, so nothing was measured.
//!
//! Reads the shipped bytes through `include_str!`, like its sibling tests: the subject is the file
//! in this repository, not whatever a dev override or update tier resolves to at runtime.
use nub_sandbox::catalog_v2::{Catalog, Platform, Scope};

fn shipped() -> Catalog {
    nub_sandbox::catalog_v2::parse(include_str!("../data/build-jail-catalog-v2.json"))
        .expect("the shipped catalog parses; build.rs fails the build otherwise")
}

/// Cells licensed by a COLD INSTALL that ran the real package against the shipped grant.
/// `cold-network-sweep-{macos,linux,win}.tsv`, verdict `OK-cold-as-shipped`.
const COLD_SWEPT: &[(&str, &str, &str)] = &[
    ("@bazel/cypress", "default", "macos"),
    ("blake-hash", "default", "linux"),
    ("blake-hash", "default", "win"),
    ("bun", "default", "linux"),
    ("bun", "default", "macos"),
    ("cz-customizable", "<7.5.4", "linux"),
    ("cz-customizable", "<7.5.4", "macos"),
    ("fast-folder-size", "default", "linux"),
    ("fast-folder-size", "default", "macos"),
    ("geckodriver", "<6.1.0", "win"),
    ("handbrake-js", "default", "linux"),
    ("isolated-vm", "<7.0.1", "linux"),
    ("keccak", "default", "linux"),
    ("keccak", "default", "win"),
    ("nx", "<23.1.1", "linux"),
    ("nx", "<23.1.1", "macos"),
    ("pizzip", "default", "linux"),
    ("pizzip", "default", "macos"),
    ("samlify", "<2.13.1", "linux"),
    ("samlify", "<2.13.1", "macos"),
];

/// Cells with no readable in-band record whose PRE-repair grant was already non-empty without
/// `network` — so a measured network-free record contributed to the union that built them. Inferred
/// from the collator's construction rather than read off a record, and kept on the
/// when-in-doubt-do-not-widen rule. Re-audit these first if the corpus is ever re-collated.
///
/// ⛔ THIS LICENCE IS WEAKER THAN THE COLD SWEEP, and weaker than the class withdrawn above, since
/// its members were inferred from records that no longer license anything on their own. It is three
/// cells on one platform, so it is kept and flagged rather than acted on blind; the next cold sweep
/// that reaches these packages on win32 settles them either way.
const INFERRED_FROM_A_NON_EMPTY_UNION: &[(&str, &str, &str)] = &[
    ("@apollo/protobufjs", "default", "win"),
    ("@progress/kendo-licensing", "default", "win"),
    ("subrequests", "default", "win"),
];

fn platform_key(platform: Platform) -> &'static str {
    match platform {
        Platform::Macos => "macos",
        Platform::Linux => "linux",
        Platform::Windows => "win",
    }
}

/// Every cell that grants the baseline write, reads nothing, stays out of the real home, and denies
/// egress. That is a grant strictly NARROWER than the base profile on the one axis, so no rung the
/// search can reach ever produced it.
#[test]
fn no_cell_withdraws_baseline_egress_without_a_measurement_that_withdrew_it() {
    let catalog = shipped();
    let licensed: std::collections::BTreeSet<(&str, &str, &str)> = COLD_SWEPT
        .iter()
        .chain(INFERRED_FROM_A_NON_EMPTY_UNION)
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
                // `Reach::Disk` covers every scope, so ruling out UserHome also rules out Disk —
                // the predicate never has to name the representation. `Scope::Project` is
                // deliberately NOT ruled out; see the module doc.
                let baseline_write =
                    caps.write.covers(Scope::Deps) && !caps.write.covers(Scope::UserHome);
                if !baseline_write || !caps.read.is_none() || caps.network {
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
    // renamed scope, an emptied `packages`, a `covers` that answers false) would pass this test by
    // examining nothing, and would do it silently.
    assert!(
        selected > 0,
        "control failed: not one cell in the shipped catalog carries the baseline write while \
         denying egress, so this test cannot distinguish a repaired catalog from a broken traversal"
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
        "{} licensed cell(s) no longer carry the baseline write while denying egress, so the \
         licence is dead and would pre-approve a future under-grant. Drop them:\n  {}",
        dead.len(),
        dead.join("\n  ")
    );

    assert!(
        offending.is_empty(),
        "{} cell(s) carry the baseline write `{{deps}}` and read nothing, yet WITHDRAW the \
         baseline's egress. The grant search has no rung below the base profile, so a package that \
         passed there was never measured without network -- and node-gyp fetches its headers over \
         it, so this breaks native builds rather than merely tightening them. Spell the cell \
         `\"network\": true` in its per-OS block, or license it with a cold-sweep row:\n  {}",
        offending.len(),
        offending.join("\n  ")
    );
}

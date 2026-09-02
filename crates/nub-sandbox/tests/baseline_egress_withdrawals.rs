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
//! reached DO exist, and they are the exfiltration axis, so re-widening them has a real cost. Two
//! kinds are licensed below, and each names its instrument:
//!
//!   * a cold-install sweep (`tests/jail-acceptance/cold-network-sweep.sh`) that loads the shipped
//!     catalog, checks the jail actually ran, and files an rc=0 install as SUSPECT anyway when the
//!     log carries `getaddrinfo`/`ENOTFOUND` or shows the tried-then-compiled-from-source pair —
//!     the silent-fallback shape that an exit code cannot see; and
//!   * a corpus record whose grant is NON-EMPTY and carries no `network`, meaning the observed
//!     synthesis was verified with egress denied and passed.
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

/// Cells licensed by a corpus record at a version the band covers, on this platform, whose grant is
/// non-empty and carries no `network` — an arm that ran with egress denied and passed.
const MEASURED_WITHOUT_EGRESS: &[(&str, &str, &str)] = &[
    ("@apollo/protobufjs", "<1.2.8", "win"),
    ("@azure-devops/mcp", "<2.9.0", "win"),
    ("@bufbuild/buf", "default", "win"),
    ("@clerk/shared", "<4.29.1", "win"),
    ("@danmarshall/deckgl-typings", "default", "linux"),
    ("@danmarshall/deckgl-typings", "default", "macos"),
    ("@danmarshall/deckgl-typings", "default", "win"),
    ("@depot/cli", "default", "win"),
    ("@firebase/util", "default", "win"),
    ("@heroui/shared-utils", "<2.1.12", "win"),
    ("@heroui/shared-utils", "default", "win"),
    ("@hyperjump/json-pointer", "<1.1.2", "win"),
    ("@hyperjump/json-schema-core", "default", "win"),
    ("@hyperjump/pact", "<1.4.0", "win"),
    ("@mui/x-telemetry", "default", "win"),
    ("@prisma/client", "<7.9.1", "linux"),
    ("@prisma/client", "<7.9.1", "macos"),
    ("@prisma/client", "<7.9.1", "win"),
    ("@substrate/connect", "default", "win"),
    ("@syncfusion/ej2-angular-base", "default", "win"),
    ("@tloncorp/tlon-skill", "default", "linux"),
    ("@tloncorp/tlon-skill", "default", "macos"),
    ("backport", "default", "win"),
    ("compresion", "default", "win"),
    ("dtrace-provider", "default", "win"),
    ("iso-constants", "default", "win"),
    ("nodemon", "<3.1.14", "win"),
    ("rc-editor-core", "<0.8.10", "win"),
    ("storage-engine", "default", "win"),
    ("stream-chat-react-native-core", "<9.7.6", "linux"),
    ("stream-chat-react-native-core", "<9.7.6", "macos"),
    ("stream-chat-react-native-core", "<9.7.6", "win"),
    ("stream-chat-react-native-core", "default", "linux"),
    ("stream-chat-react-native-core", "default", "macos"),
    ("stream-chat-react-native-core", "default", "win"),
    ("subrequests-json-merger", "default", "win"),
    ("tree-sitter-cpp", "default", "win"),
    ("tree-sitter-ruby", "default", "win"),
    ("ttf2woff2", "<2.0.3", "win"),
    ("ttf2woff2", "<7.0.0", "win"),
    ("vue-inbrowser-compiler-demi", "default", "win"),
    ("wordpos", "default", "linux"),
    ("wordpos", "default", "macos"),
    ("wordpos", "default", "win"),
    ("wrtc", "default", "win"),
];

/// Cells with no readable in-band record whose PRE-repair grant was already non-empty without
/// `network` — so a measured network-free record contributed to the union that built them. Inferred
/// from the collator's construction rather than read off a record, and kept on the
/// when-in-doubt-do-not-widen rule. Re-audit these first if the corpus is ever re-collated.
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

/// Every cell that grants the baseline write and nothing wider, reads nothing, and denies egress.
/// That is a grant strictly NARROWER than the base profile on the one axis, with nothing added
/// anywhere else — so no rung the search can reach ever produced it.
#[test]
fn no_cell_withdraws_baseline_egress_without_a_measurement_that_withdrew_it() {
    let catalog = shipped();
    let licensed: std::collections::BTreeSet<(&str, &str, &str)> = COLD_SWEPT
        .iter()
        .chain(MEASURED_WITHOUT_EGRESS)
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
                // `Reach::Disk` covers every scope, so ruling out Project and UserHome also rules
                // out Disk — the predicate never has to name the representation.
                let baseline_write = caps.write.covers(Scope::Deps)
                    && !caps.write.covers(Scope::Project)
                    && !caps.write.covers(Scope::UserHome);
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
         `\"network\": true` in its per-OS block, or license it with the cold-sweep row or the \
         non-empty network-free record that measured the withdrawal:\n  {}",
        offending.len(),
        offending.join("\n  ")
    );
}

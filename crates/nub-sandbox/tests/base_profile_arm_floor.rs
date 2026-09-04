//! A cell narrowed BECAUSE the base-profile arm passed must still grant that arm's `write.deps`.
//!
//! ⛔ WHAT THE ARM IS, because the harness prints it as `(nothing)` and it is not nothing.
//! `tests/build-jail-search/search.mjs` spells its cheapest rung as NO ENTRY for the package under
//! test — "State 0 is the BASE PROFILE, and the catalog spells that as NO ENTRY" — and an absent
//! package takes [`catalog_v2::baseline_caps`]: `write: {deps}`, `network: true`, and
//! `BASELINE_WRITE_PATHS`. There is no rung BELOW it. So a pass at that rung licenses withdrawing
//! the real-home write and the whole-disk write and NOTHING ELSE; the absence of `write.deps` was
//! never measured on any cell that cites it, and cannot have been.
//!
//! ⛔ WHY A SEPARATE FILE FROM `windows_base_profile_withdrawals.rs`, WHICH ALREADY WALKS THESE
//! CELLS. That file's floor test asks `caps.widens_nothing()`, which is
//! `read.is_none() && write.is_none() && !network && write_paths.is_empty()` — a conjunction over
//! every axis. A cell narrowed to `"win": {"write": null}` while its entry still carries egress has
//! `network == true`, so `widens_nothing()` is FALSE and the cell passes a test whose own doc
//! comment states the invariant it fails: "it does NOT license withdrawing `write.deps`". The floor
//! has to be asserted on the write axis ALONE, which is what this file does.
//!
//! ⛔ THE SUBJECT SET IS DERIVED FROM THE NOTES, NOT HAND-LISTED. Every narrowing of this kind
//! records itself in the band's `notes` as "<os> write narrowed …: … the base-profile arm
//! reproduces the control run's script output and product exactly", so a batch landed tomorrow is
//! covered the day it lands rather than the day someone remembers to extend a list. A hand-list is
//! right where the SHAPE is not the defect (`default_band_grants.rs` says so, and it is); here the
//! note is the evidence, and the evidence is exactly what the cell must not undercut.
//!
//! Reads the shipped bytes through `include_str!` rather than the runtime lookup, which consults a
//! dev override and an on-disk update tier first; the subject here is the file in this repository.
//!
//! ⛔ THE NOTES COME FROM THE SIDECAR AND ARE READ FROM DISK, NOT `include_str!`. Prose is ~46% of
//! what the catalog used to weigh and no runtime code reads it, so it moved to
//! `build-jail-catalog-notes.json` and the shipped catalog carries none. Embedding the sidecar here
//! would put those bytes straight back into every binary that links this crate, which is the whole
//! thing the split was for — so this reads the file at test time instead.
use nub_sandbox::catalog_v2::{Catalog, Platform, Scope};
use std::collections::BTreeMap;

fn shipped() -> Catalog {
    nub_sandbox::catalog_v2::parse(include_str!("../data/build-jail-catalog-v2.json"))
        .expect("the shipped catalog parses; build.rs fails the build otherwise")
}

/// `(package, band, platform-key-or-empty) -> note`, where the empty key is the cell's own note.
type Notes = BTreeMap<(String, String, String), String>;

fn sidecar_notes() -> Notes {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/data/build-jail-catalog-notes.json"
    );
    let raw = std::fs::read_to_string(path).expect("the notes sidecar is readable");
    let doc: serde_json::Value = serde_json::from_str(&raw).expect("the notes sidecar parses");
    let mut out = Notes::new();
    let pkgs = doc["packages"].as_object().expect("`packages` object");
    for (name, entry) in pkgs {
        let mut bands: Vec<(String, &serde_json::Value)> = Vec::new();
        for (key, val) in entry.as_object().expect("entry object") {
            if key == "versions" {
                for (range, cell) in val.as_object().expect("versions object") {
                    bands.push((range.clone(), cell));
                }
            } else {
                bands.push((key.clone(), val));
            }
        }
        for (band, cell) in bands {
            for (scope, note) in cell.as_object().expect("cell object") {
                if let Some(t) = note.as_str() {
                    out.insert((name.clone(), band.clone(), scope.clone()), t.to_string());
                }
            }
        }
    }
    out
}

/// The note that applies to one cell on one platform, mirroring `Grant::on`'s own rule for the
/// field: the platform's own note when it has one, otherwise the cell's.
fn note_for(notes: &Notes, name: &str, band: &str, platform: Platform) -> String {
    let key = match platform {
        Platform::Macos => "macos",
        Platform::Linux => "linux",
        Platform::Windows => "win",
    };
    notes
        .get(&(name.to_string(), band.to_string(), key.to_string()))
        .or_else(|| notes.get(&(name.to_string(), band.to_string(), String::new())))
        .cloned()
        .unwrap_or_default()
}

/// How a note names each OS when it records one of these narrowings. `win32` rather than `win`:
/// the JSON KEY is `win`, but the prose the measuring lanes write is `win32`, and matching the key
/// here would select nothing.
const OS_IN_PROSE: [(&str, Platform); 3] = [
    ("win32 write narrowed", Platform::Windows),
    ("linux write narrowed", Platform::Linux),
    ("macOS write narrowed", Platform::Macos),
];

/// One cell: package, the band's range as written (`default` for the entry's default grant), and
/// the OS whose narrowing the note records.
type Cell = (String, String, Platform);

/// Every cell whose own note attributes a write narrowing to the base-profile arm.
///
/// Both halves are required. "base-profile arm" alone appears in prose about OTHER cells — a
/// `default` note routinely describes what was done to the bands below it — and matching on it
/// alone attributes a band's measurement to the default that merely mentions it.
fn cells_narrowed_at_the_base_profile(catalog: &Catalog, notes: &Notes) -> Vec<Cell> {
    let mut found = Vec::new();
    for (name, entry) in &catalog.packages {
        let bands = std::iter::once((String::from("default"), &entry.default)).chain(
            entry
                .versions
                .iter()
                .map(|band| (band.range.clone(), &band.grant)),
        );
        for (range, grant) in bands {
            for (prose, platform) in OS_IN_PROSE {
                let _ = grant;
                let note = note_for(notes, name, &range, platform);
                if note.contains(prose) && note.contains("base-profile arm") {
                    found.push((name.clone(), range.clone(), platform));
                }
            }
        }
    }
    found.sort();
    found
}

/// THE FLOOR, over EVERY cell the walk selects — there is deliberately no excuse list.
///
/// ⛔ THE HISTORY, because it is what the assertion is shaped against. The batch in `e2f4ea419d`
/// spelled one verdict two ways: sixteen cells took `{"deps": true}` and twelve took
/// `"write": null`. `ae518a1d97` — "lift the five floored cells off zero" — caught five of the
/// twelve, and `5c53cfb53f` caught the last seven, putting all 28 on `{"deps": true}`. Every one of
/// them kept its egress throughout, which is precisely why the sibling guard never fired on any of
/// the twelve.
///
/// Asserted on the write axis ALONE. That is the whole point: the sibling's conjunction over every
/// axis cannot fire while the cell keeps its egress.
#[test]
fn a_cell_narrowed_at_the_base_profile_still_grants_that_arm_s_write_deps() {
    let catalog = shipped();
    // COLLECTED, not asserted per row: these land in batches, and a panic on the first row reports
    // 1 of N — which reads as an isolated typo rather than the systematic narrowing it is.
    let mut floored: Vec<String> = Vec::new();

    for (name, range, platform) in cells_narrowed_at_the_base_profile(&catalog, &sidecar_notes()) {
        let entry = &catalog.packages[&name];
        let grant = if range == "default" {
            &entry.default
        } else {
            &entry
                .versions
                .iter()
                .find(|b| b.range == range)
                .expect("the range came from this entry's own bands")
                .grant
        };
        if !grant.on(platform).write.covers(Scope::Deps) {
            floored.push(format!("{name} [band {range}] on {}", platform.key()));
        }
    }

    assert!(
        floored.is_empty(),
        "{} cell(s) were narrowed on the strength of the base-profile arm but no longer grant that \
         arm's `write.deps`. The arm IS `baseline_caps()` — the search harness has no rung below \
         it — so a package writing into its own declared dependency directory now breaks, and \
         nothing ever measured that. Write `{{\"write\": {{\"deps\": true}}}}` rather than \
         `{{\"write\": null}}`:\n  {}",
        floored.len(),
        floored.join("\n  ")
    );
}

/// THE CONTROL, and it is what stops the floor above from passing by selecting nothing.
///
/// The subject set is derived by matching prose, so a lane that rewords the note — or a `notes`
/// field that stops resolving through `Grant::on` — would empty the walk and turn the assertion
/// into a tautology. That failure is invisible from inside the test above, which is why the count
/// is pinned from below here rather than left implicit.
#[test]
fn the_derived_walk_still_finds_the_cells_it_is_meant_to_guard() {
    let catalog = shipped();
    let cells = cells_narrowed_at_the_base_profile(&catalog, &sidecar_notes());

    assert!(
        cells.len() >= 24,
        "the note-derived walk found only {} cell(s), so the floor test is asserting almost \
         nothing. Either the narrowing note was reworded — match the new wording in OS_IN_PROSE — \
         or the note is no longer in `data/build-jail-catalog-notes.json`, which is where prose \
         lives now that the shipped catalog carries none.",
        cells.len()
    );
}

/// THE POSITIVE CONTROL ON THE ACCESSOR. Without it a `covers` that always returned `true` would
/// make the floor test report nothing — the direction that ships a defect — and the walk above
/// could not tell a held floor from a broken accessor.
///
/// Deliberately NOT paired with a negative control naming one of the pending twelve. That would
/// pass only while the cell is still broken, so the repair this file is waiting for would land
/// RED on a lane that did exactly the right thing. The negative direction is proved by breaking
/// the catalog and watching the floor test name the cell, which is a stronger check than an
/// assertion that rots the moment it succeeds.
#[test]
fn the_write_accessor_reports_deps_when_the_cell_actually_grants_it() {
    let catalog = shipped();

    // Narrowed in the SAME batch as the pending twelve and landed correctly, as `{"deps": true}`.
    for (pkg, version) in [("dtrace-provider", "0.8.8"), ("nx", "23.1.0")] {
        assert!(
            catalog.packages[pkg]
                .grant_for(Some(version))
                .on(Platform::Windows)
                .write
                .covers(Scope::Deps),
            "{pkg}@{version} lost the `write.deps` its base-profile narrowing landed with, so the \
             floor test above can no longer tell a held floor from a broken accessor."
        );
    }
}

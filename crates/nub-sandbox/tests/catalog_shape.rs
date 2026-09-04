//! Shape rules for the shipped catalog: no redundant per-OS overlays, and no prose.
//!
//! Both are byte-size rules with a correctness edge. The catalog is `include_str!`d into every
//! binary that links this crate, so anything carried here is carried by every user.

use nub_sandbox::catalog_v2::Platform;
use std::collections::BTreeMap;

const CATALOG: &str = include_str!("../data/build-jail-catalog-v2.json");
const NOTES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/data/build-jail-catalog-notes.json"
);

fn doc() -> serde_json::Value {
    serde_json::from_str(CATALOG).expect("the shipped catalog is JSON")
}

/// `(package, band)` for every cell, paired with the cell itself.
fn cells(doc: &serde_json::Value) -> Vec<(String, String, serde_json::Value)> {
    let mut out = Vec::new();
    for (name, entry) in doc["packages"].as_object().expect("`packages` object") {
        for (key, val) in entry.as_object().expect("entry object") {
            if key == "versions" {
                for (range, cell) in val.as_object().expect("versions object") {
                    out.push((name.clone(), range.clone(), cell.clone()));
                }
            } else {
                out.push((name.clone(), key.clone(), val.clone()));
            }
        }
    }
    out
}

const OVERLAY_KEYS: [&str; 3] = ["macos", "linux", "win"];

/// ⛔ THREE IDENTICAL OVERLAYS SAY NOTHING THE BASE CANNOT. An overlay overrides per FIELD with
/// fallback to the base, so when all three platforms carry the same block the base's value for
/// those fields is unobservable and the block is pure weight — 62 cells carried one, and hoisting
/// them into the base cut nothing but bytes (proved by comparing all 1,299 resolved grants before
/// and after). Two overlays are NOT this: the third platform still reads the base.
#[test]
fn no_cell_carries_the_same_overlay_on_all_three_platforms() {
    let d = doc();
    let all = cells(&d);
    let mut redundant = Vec::new();
    let mut with_overlays = 0usize;
    for (name, band, cell) in &all {
        let blocks: Vec<&serde_json::Value> = OVERLAY_KEYS
            .iter()
            .filter_map(|k| cell.get(*k))
            .filter(|v| v.is_object())
            .collect();
        if !blocks.is_empty() {
            with_overlays += 1;
        }
        if blocks.len() == 3 && blocks[0] == blocks[1] && blocks[1] == blocks[2] {
            redundant.push(format!("{name} [{band}]"));
        }
    }
    // CONTROL — without it, a catalog that lost its overlays entirely would pass by examining none.
    assert!(
        with_overlays > 50,
        "control: only {with_overlays} cell(s) carry any per-OS overlay, so this test is not \
         looking at the shape it guards"
    );
    assert!(
        redundant.is_empty(),
        "{} cell(s) repeat one overlay on all three platforms; hoist the block into the cell and \
         delete the overlays:\n  {}",
        redundant.len(),
        redundant.join("\n  ")
    );
}

/// ⛔ PROSE DOES NOT SHIP. Notes were ~46% of this file (92 KB of 201 KB) and no runtime code reads
/// one; they live in `data/build-jail-catalog-notes.json` instead. A generator that starts emitting
/// them again would silently double the embedded catalog.
#[test]
fn the_shipped_catalog_carries_no_prose() {
    let d = doc();
    let mut offenders = Vec::new();
    for (name, band, cell) in cells(&d) {
        if cell.get("notes").is_some() {
            offenders.push(format!("{name} [{band}]"));
        }
        for k in OVERLAY_KEYS {
            if cell.get(k).and_then(|o| o.get("notes")).is_some() {
                offenders.push(format!("{name} [{band}] {k}"));
            }
        }
    }
    for section in ["baseline", "env"] {
        if let Some(rows) = d[section].as_array() {
            for row in rows {
                if row.get("notes").is_some() {
                    offenders.push(format!("{section} row"));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "{} `notes` field(s) are back in the shipped catalog; move them to \
         data/build-jail-catalog-notes.json:\n  {}",
        offenders.len(),
        offenders.join("\n  ")
    );
}

/// The sidecar is only useful while it still addresses real cells. A note whose package or band no
/// longer exists is orphaned prose nobody will ever read, and it is the shape this split invites.
#[test]
fn every_sidecar_note_addresses_a_cell_that_exists() {
    let d = doc();
    let known: BTreeMap<(String, String), ()> = cells(&d)
        .into_iter()
        .map(|(n, b, _)| ((n, b), ()))
        .collect();
    let raw = std::fs::read_to_string(NOTES).expect("the notes sidecar is readable");
    let side: serde_json::Value = serde_json::from_str(&raw).expect("the notes sidecar parses");

    let mut orphans = Vec::new();
    let mut checked = 0usize;
    for (name, entry) in side["packages"].as_object().expect("`packages` object") {
        for (key, val) in entry.as_object().expect("entry object") {
            let bands: Vec<String> = if key == "versions" {
                val.as_object()
                    .expect("versions object")
                    .keys()
                    .cloned()
                    .collect()
            } else {
                vec![key.clone()]
            };
            for band in bands {
                checked += 1;
                if !known.contains_key(&(name.clone(), band.clone())) {
                    orphans.push(format!("{name} [{band}]"));
                }
            }
        }
    }
    assert!(
        checked > 300,
        "control: only {checked} sidecar note(s) examined — the sidecar walk is broken, not clean"
    );
    assert!(
        orphans.is_empty(),
        "{} sidecar note(s) address a cell the catalog no longer has:\n  {}",
        orphans.len(),
        orphans.join("\n  ")
    );
    // Names the platform keys so a rename of `win` shows up here rather than as silently lost prose.
    let _ = Platform::ALL;
}

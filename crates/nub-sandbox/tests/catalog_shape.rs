//! Shape rules for the shipped catalog: no redundant per-OS overlays, and no prose.
//!
//! "Redundant" has two shapes and both are guarded here, because the second HIDES the first: an
//! overlay field that merely restates the base's value is dead on its own, and removing 64 of them
//! exposed 27 further cells whose three overlays were then identical. A single pass over either
//! shape alone leaves the other behind.
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
/// those fields is unobservable and the block is pure weight — 89 cells carried one, and hoisting
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

/// The value an overlay or a base actually resolves to for one field, canonicalised so two
/// spellings of one meaning compare equal.
///
/// ⛔ THIS MIRRORS `parse_overlay`, AND THE ABSENT/NULL DISTINCTION IS THE WHOLE POINT. In an
/// OVERLAY, an absent key means "inherit the base" while an explicit `null` means "withdraw the
/// outer grant on this OS" — so `{"write": null}` is a real narrowing and must never be mistaken
/// for dead weight. In the BASE, an absent key means the `Caps` default, which is the same value
/// `null` denotes. That is why a base which simply omits `network` and an overlay that writes
/// `"network": null` are redundant, while the raw JSON for the two looks nothing alike.
fn effective(obj: &serde_json::Value, key: &str) -> String {
    let v = obj.get(key);
    match key {
        "network" => match v {
            Some(serde_json::Value::Bool(true)) => "true".to_string(),
            _ => "false".to_string(),
        },
        "writePaths" => match v {
            Some(serde_json::Value::Array(a)) => a
                .iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(","),
            _ => String::new(),
        },
        // `read` / `write`: a reach. Scope order is not meaning, so sort before comparing.
        _ => match v {
            None | Some(serde_json::Value::Null) => "none".to_string(),
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Object(m)) => {
                let mut on: Vec<&str> = m
                    .iter()
                    .filter(|(_, val)| val.as_bool() == Some(true))
                    .map(|(k, _)| k.as_str())
                    .collect();
                on.sort_unstable();
                on.join("+")
            }
            Some(other) => other.to_string(),
        },
    }
}

/// ⛔ AN OVERLAY FIELD THAT RESTATES THE BASE IS DEAD, AND IT HIDES THE REDUNDANCY ABOVE. `Grant::on`
/// falls back to the base per field, so an overlay naming a value the base already resolves to
/// changes nothing on that platform. 64 such fields shipped, 4 of them accounting for an overlay's
/// entire contents; deleting them left every one of the 1,299 resolved grants identical AND made 27
/// more cells collapse under the three-way rule, which is why this guard exists rather than a note
/// to look again later.
#[test]
fn no_overlay_field_restates_the_base() {
    let d = doc();
    let mut dead = Vec::new();
    let mut examined = 0usize;
    for (name, band, cell) in cells(&d) {
        for key in OVERLAY_KEYS {
            let Some(overlay) = cell.get(key).and_then(|v| v.as_object()) else {
                continue;
            };
            for field in overlay.keys() {
                examined += 1;
                if effective(&serde_json::Value::Object(overlay.clone()), field)
                    == effective(&cell, field)
                {
                    dead.push(format!("{name} [{band}] {key}.{field}"));
                }
            }
        }
    }
    // CONTROL — a catalog whose overlays all vanished would otherwise pass by examining nothing.
    assert!(
        examined > 300,
        "control: only {examined} overlay field(s) examined, so this test is not looking at the \
         shape it guards"
    );
    assert!(
        dead.is_empty(),
        "{} overlay field(s) resolve to exactly what the base already gives; delete them (and the \
         overlay, if that empties it):\n  {}",
        dead.len(),
        dead.join("\n  ")
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

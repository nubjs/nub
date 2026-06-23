//! Emit `docs/error-codes.data.json` from `aube_codes::errors::ALL`
//! and `warnings::ALL`. Run via
//! `cargo run -p aube-codes --bin generate-error-codes-docs`
//! (wired into `mise run render`).
//!
//! VitePress imports the JSON via `docs/error-codes.data.ts`
//! (build-time data loader) and renders it through the
//! `<ErrorCodesTable>` Vue component on the `/error-codes` page.
//! Same shape as the benchmarks pipeline (`benchmarks/results.json`
//! → `docs/benchmarks.data.ts` → `<BenchChart>`).
//!
//! The JSON is the data contract — every code's identifier,
//! category, description, and (optional) bespoke exit code lives
//! next to its `pub const` declaration in
//! `crates/aube-codes/src/{errors,warnings}.rs`. Hand-edits to the
//! JSON or the markdown will be clobbered on the next
//! `mise run render`. Update the registry instead.

use aube_codes::{CodeMeta, errors, warnings};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Top-level shape of `docs/error-codes.data.json`.
///
/// Mirrors `docs/error-codes.data.ts::ErrorCodesData`. `categories`
/// lists each kind's labels in first-seen order so the
/// `<ErrorCodesTable>` Vue component can render filter chips
/// deterministically without re-deriving the order from individual
/// entries.
#[derive(Serialize)]
struct ErrorCodesData<'a> {
    errors: &'a [CodeMeta],
    warnings: &'a [CodeMeta],
    categories: Categories,
}

#[derive(Serialize)]
struct Categories {
    errors: Vec<&'static str>,
    warnings: Vec<&'static str>,
}

fn main() {
    let root = workspace_root();
    let out_path = root.join("docs/error-codes.data.json");
    let data = ErrorCodesData {
        errors: errors::ALL,
        warnings: warnings::ALL,
        categories: Categories {
            errors: ordered_categories(errors::ALL),
            warnings: ordered_categories(warnings::ALL),
        },
    };
    let mut json = serde_json::to_string_pretty(&data)
        .unwrap_or_else(|e| panic!("failed to serialize error-codes data: {e}"));
    // `to_string_pretty` omits the trailing newline; adding it keeps
    // diffs clean and matches the editor convention.
    json.push('\n');
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)
            .unwrap_or_else(|e| panic!("failed to create {}: {e}", parent.display()));
    }
    fs::write(&out_path, json)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", out_path.display()));
    println!(
        "generated {}",
        out_path.strip_prefix(&root).unwrap().display()
    );
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Walk `all` once and emit each distinct category in first-seen
/// order. `BTreeSet` would alphabetize, which would re-order the
/// filter chips in the docs and decouple them from the source
/// layout — keeping declaration order makes the page match the
/// registry top-to-bottom.
fn ordered_categories(all: &[CodeMeta]) -> Vec<&'static str> {
    let mut seen: BTreeSet<&'static str> = BTreeSet::new();
    let mut ordered: Vec<&'static str> = Vec::new();
    for meta in all {
        if seen.insert(meta.category) {
            ordered.push(meta.category);
        }
    }
    ordered
}

//! Workspace-level audit for generated typed setting accessors.
//!
//! Background: `values::resolved` in this crate lives in a `pub mod`
//! of a library crate. `pub fn`s in a `pub mod` of a lib crate are
//! treated as reachable from the crate's public API, so rustc's
//! `dead_code` lint never fires on unused accessors. Dropping the
//! module-level `#[allow(dead_code)]` — an otherwise tempting
//! cleanup — does nothing.
//!
//! This test recovers the intent (an unused setting should surface
//! loudly) at `cargo test` time instead of compile time: for every
//! `SettingMeta` with a supported scalar type, it verifies that
//! *some* workspace `.rs` file outside `aube-settings` references
//! the generated `resolved::<name>` accessor. Settings honored
//! through other pathways (direct env reads, `NpmConfig` string
//! lookups, accepted-for-parity no-ops) opt out with
//! `typedAccessorUnused = true` in `settings.toml`.
//!
//! Diagnostic on failure names the offending settings and suggests
//! the two legal fixes: wire a `resolved::<name>(...)` caller, or
//! set `typedAccessorUnused = true` with a comment explaining how
//! the setting *is* honored. That turns "I forgot to plumb the new
//! setting through" into a CI failure with a pointer to the call
//! site that's missing.

use aube_settings::meta::{SettingMeta, all};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Locate the workspace root by walking up from this crate's manifest.
/// `CARGO_MANIFEST_DIR` points at `crates/aube-settings`, so two
/// parents gets us to the workspace root without needing
/// `cargo_metadata` as a dev-dep.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("aube-settings must live two levels below the workspace root")
        .to_path_buf()
}

/// Collect every `.rs` file under `crates/` except the ones in
/// `aube-settings/src/` and this audit file itself. `aube-settings/src/`
/// is where the accessors are *defined*, and this file is where the
/// auditor lives — neither counts as "a caller exists". We still walk
/// other files under `aube-settings/tests/` since test-only callers
/// are valid proof of wiring.
///
/// Skipping this file also prevents docstring comments that reference
/// accessor names (e.g. explaining the word-boundary probe with a
/// literal `resolved::<name>` example) from registering as false
/// matches in either direction of the audit.
///
/// `target/` is skipped via name match so we don't pull in generated
/// files that would hide real gaps behind matches in
/// `settings_resolved.rs` itself.
fn collect_rs_files(dir: &Path, skip_src: &Path, skip_file: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if path.is_dir() {
            if name == "target" || path == skip_src {
                continue;
            }
            collect_rs_files(&path, skip_src, skip_file, out);
        } else if path.extension().is_some_and(|e| e == "rs") && path != skip_file {
            out.push(path);
        }
    }
}

/// Mirror `build.rs::snake_case` exactly. Keeping this duplicated
/// (rather than re-exported from the build script) is deliberate: if
/// someone changes the generator's conversion rule, this test should
/// keep the *old* rule and fail loudly, forcing the author to think
/// about the mapping before silently renaming every accessor.
fn snake_case(name: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if c == '-' || c == '_' || c == '.' {
            if !out.ends_with('_') {
                out.push('_');
            }
            prev_lower = false;
        } else if c.is_ascii_uppercase() {
            if prev_lower {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
            prev_lower = false;
        } else {
            out.push(c);
            prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        }
    }
    out
}

/// A setting has a generated typed accessor iff its `type` is one of
/// the scalar kinds the build script supports. Kept in lockstep with
/// the match in `build.rs::generate_resolved_accessors` — types not
/// listed here (`object`) produce no accessor, so there's
/// nothing for the audit to check.
fn has_typed_accessor(type_: &str) -> bool {
    matches!(
        type_,
        "bool" | "string" | "path" | "url" | "int" | "list<string>"
    ) || type_.starts_with('"')
}

/// Search `corpus` for a `resolved::<accessor>` call, treating the
/// accessor name as a whole identifier rather than a substring. A
/// plain `corpus.contains("resolved::ci")` would match
/// `resolved::ci_timeout` and hide a real missing-caller regression
/// (or falsely flag `typedAccessorUnused` as stale for the shorter
/// name). We require the character immediately following the
/// accessor name to be something that can't continue a Rust
/// identifier — end-of-input, whitespace, `(`, `,`, `;`, `)`, `:`
/// (for path separators in rarer forms like `resolved::foo::bar`),
/// etc. Everything non-alphanumeric-and-not-`_` counts as a boundary.
fn corpus_has_accessor_call(corpus: &str, accessor: &str) -> bool {
    let probe = format!("resolved::{accessor}");
    corpus.match_indices(&probe).any(
        |(idx, _)| match corpus[idx + probe.len()..].chars().next() {
            None => true,
            Some(c) => !c.is_ascii_alphanumeric() && c != '_',
        },
    )
}

#[test]
fn every_setting_has_a_typed_accessor_caller() {
    let root = workspace_root();
    let crates_dir = root.join("crates");
    let self_src = root.join("crates").join("aube-settings").join("src");
    let self_file = root.join(file!());

    let mut files = Vec::new();
    collect_rs_files(&crates_dir, &self_src, &self_file, &mut files);
    assert!(
        !files.is_empty(),
        "walked {} and found no .rs files — workspace layout assumption is wrong",
        crates_dir.display()
    );

    // One big corpus + one pass per accessor is O(settings * corpus_bytes),
    // which is fine at current workspace size (~100 settings, a few MB of
    // Rust). If this ever gets slow, switch to a single regex pass over
    // the corpus.
    let mut corpus = String::new();
    for f in &files {
        if let Ok(s) = fs::read_to_string(f) {
            corpus.push_str(&s);
            corpus.push('\n');
        }
    }

    let mut missing: Vec<String> = Vec::new();
    // BTreeSet to stably dedupe + sort the report; accessor names are
    // unique today but de-duping keeps the output readable if two
    // settings ever happen to snake_case-collapse.
    let mut reported: BTreeSet<String> = BTreeSet::new();

    for s in all() {
        if s.typed_accessor_unused {
            continue;
        }
        if !has_typed_accessor(s.type_) {
            continue;
        }
        let accessor = snake_case(s.name);
        // Match the qualified call form the codebase uses everywhere:
        // `aube_settings::resolved::<name>(...)`, `resolved::<name>(...)`
        // (after a `use aube_settings::resolved`), or the
        // `super::resolved::<name>` bounce. All three end in
        // `resolved::<name>`, so that probe is the least-brittle
        // marker. A bare `<name>(` would catch more but collide with
        // unrelated local helpers. `corpus_has_accessor_call` enforces
        // a word boundary after `accessor` so `resolved::ci` doesn't
        // get a spurious hit from a future `resolved::ci_timeout`.
        if !corpus_has_accessor_call(&corpus, &accessor) && reported.insert(accessor.clone()) {
            missing.push(format!(
                "  - {name:<36} → resolved::{accessor}()",
                name = s.name
            ));
        }
    }

    if !missing.is_empty() {
        panic!(
            "\n\
             Settings in settings.toml with no call to their generated \
             `resolved::<name>` accessor anywhere in the workspace ({n} total):\n\
             {list}\n\n\
             Fix one of two ways:\n  \
             1. Wire a real caller (typical path for a newly-added setting).\n  \
             2. Set `typedAccessorUnused = true` in settings.toml with a comment \
             explaining how the setting *is* honored — e.g. read directly through \
             env at startup, looked up by string key in `NpmConfig`, or accepted \
             for pnpm parity with no behavior.\n\n\
             See `crates/aube-settings/tests/accessor_audit.rs` for the full check \
             logic, and `SettingMeta::typed_accessor_unused` for the schema.\n",
            n = missing.len(),
            list = missing.join("\n")
        );
    }
}

/// Belt-and-braces: also verify that the opt-out flag is only set on
/// settings that genuinely have no typed-accessor call site. If
/// someone flips `typedAccessorUnused = true` and *then* adds a
/// caller, the flag is now lying — flag that too so the two stay in
/// sync. Mirrors the main test's grep but inverted.
#[test]
fn typed_accessor_unused_flag_is_accurate() {
    let root = workspace_root();
    let crates_dir = root.join("crates");
    let self_src = root.join("crates").join("aube-settings").join("src");
    let self_file = root.join(file!());

    let mut files = Vec::new();
    collect_rs_files(&crates_dir, &self_src, &self_file, &mut files);

    let mut corpus = String::new();
    for f in &files {
        if let Ok(s) = fs::read_to_string(f) {
            corpus.push_str(&s);
            corpus.push('\n');
        }
    }

    let mut stale: Vec<&SettingMeta> = Vec::new();
    for s in all() {
        if !s.typed_accessor_unused {
            continue;
        }
        if !has_typed_accessor(s.type_) {
            // Nothing to be stale about — setting has no accessor
            // regardless. Flag is harmless here; ignore.
            continue;
        }
        let accessor = snake_case(s.name);
        if corpus_has_accessor_call(&corpus, &accessor) {
            stale.push(s);
        }
    }

    assert!(
        stale.is_empty(),
        "\nSettings marked `typedAccessorUnused = true` that now *do* have a \
         `resolved::<name>` caller ({} stale):\n{}\n\nDrop the flag — the opt-out \
         is no longer accurate.\n",
        stale.len(),
        stale
            .iter()
            .map(|s| format!("  - {} → resolved::{}()", s.name, snake_case(s.name)))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Lock in the word-boundary behavior of `corpus_has_accessor_call`
/// with the specific prefix-collision pairs that exist in
/// `settings.toml` today. A plain `corpus.contains("resolved::hoist")`
/// would be satisfied by `resolved::hoist_pattern(…)` alone — if the
/// `hoist` setting lost its real caller, the audit would then
/// silently pass. These tests fail loudly if the probe ever regresses
/// back to unanchored substring matching.
///
/// Cases mirror the real accessor-name overlaps in the workspace as
/// of this commit:
///   - `hoist` vs `hoist_pattern` / `hoist_workspace_packages`
///   - `lockfile` vs `lockfile_include_tarball_url`
///   - `side_effects_cache` vs `side_effects_cache_readonly`
///   - `git_branch_lockfile` vs `merge_git_branch_lockfiles_branch_pattern`
///
/// The last pair is actually disjoint under `contains` too (the
/// prefix doesn't start at `resolved::`), but including it here keeps
/// the check aligned with the review comment that called them out.
#[cfg(test)]
mod probe_unit_tests {
    use super::corpus_has_accessor_call;

    #[test]
    fn shorter_accessor_not_satisfied_by_longer_prefix_caller() {
        for (shorter, longer) in [
            ("hoist", "resolved::hoist_pattern(ctx)"),
            ("hoist", "resolved::hoist_workspace_packages(ctx)"),
            ("lockfile", "resolved::lockfile_include_tarball_url(ctx)"),
            (
                "side_effects_cache",
                "resolved::side_effects_cache_readonly(ctx)",
            ),
            (
                "git_branch_lockfile",
                "resolved::merge_git_branch_lockfiles_branch_pattern(ctx)",
            ),
        ] {
            assert!(
                !corpus_has_accessor_call(longer, shorter),
                "probe `resolved::{shorter}` was falsely satisfied by `{longer}` — \
                 the word-boundary check in corpus_has_accessor_call regressed to \
                 a substring match."
            );
        }
    }

    #[test]
    fn real_bare_call_is_satisfied() {
        // Sanity: a real call site with each of the delimiters we
        // expect in the wild (`(`, `;`, `,`, whitespace, `:` for a
        // `resolved::foo::BAR` nested reference, end-of-input) all
        // qualify as a valid match. Any of these failing would mean
        // the audit misses legitimate callers.
        for caller in [
            "resolved::hoist(ctx)",
            "resolved::hoist (ctx)",
            "resolved::hoist;",
            "let f = resolved::hoist,",
            "use aube_settings::resolved::hoist\n",
            "resolved::hoist::MAX_FOO",
            "resolved::hoist",
        ] {
            assert!(
                corpus_has_accessor_call(caller, "hoist"),
                "probe `resolved::hoist` should have matched `{caller}` but \
                 corpus_has_accessor_call returned false"
            );
        }
    }

    #[test]
    fn underscore_suffix_blocks_match() {
        // `_` is specifically excluded from boundary chars because it
        // continues a Rust identifier. This test pins that down so a
        // "simpler" refactor to `!c.is_ascii_alphanumeric()` alone
        // (dropping the `c != '_'` clause) fails loudly.
        assert!(!corpus_has_accessor_call("resolved::ci_timeout", "ci"));
        assert!(!corpus_has_accessor_call(
            "resolved::lockfile_v2",
            "lockfile"
        ));
    }
}

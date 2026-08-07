//! Grant-building code must not use `std::fs::canonicalize`.
//!
//! ⛔ WHY THIS IS A RULE AND NOT A STYLE PREFERENCE. On Windows `std::fs::canonicalize` is
//! `GetFinalPathNameByHandleW(VOLUME_NAME_DOS)`, which always returns the `\\?\` VERBATIM form. The
//! compiler slash-normalizes grant paths, and in that spelling the `?` re-reads as a glob
//! metacharacter — so the grant is silently **DROPPED**. A dropped grant is an under-grant, the one
//! direction this project forbids, and it fails without an error anywhere.
//!
//! `canonicalize_including_nonexistent` strips the prefix and additionally resolves a path whose
//! tail does not exist yet, instead of handing the input back untouched.
//!
//! ⛔ THIS GUARD EXISTS BECAUSE THE AUDIT THAT FOUND THE RULE CANNOT BE RE-RUN BY HAND. Two Windows
//! security checks had already been found inert for this reason. The audit (2026-08-07) then read
//! every call site and found the grant-building files CLEAN — `preset.rs`, `curated.rs`,
//! `defaults.rs` and `build_jail.rs` use only the safe canonicalizers. That is a fact about today's
//! source, and the only way it stays true is a check that runs.
//!
//! ⛔ WHAT IS DELIBERATELY NOT COVERED. `matcher/path.rs` IS the canonicalizer, so it must call the
//! std one. `backend/windows.rs` resolves the child's working directory once and uses the SAME
//! absolute path for the DACL preflight and the launch — that one is correct and fails LOUDLY
//! (`Degradation`) rather than falling back, which is the property that matters there.

use std::path::Path;

/// Source files that BUILD grants or the env handed to a lifecycle script.
const GRANT_BUILDERS: &[&str] = &[
    "src/compiler/preset.rs",
    "src/compiler/curated.rs",
    "src/compiler/defaults.rs",
];

/// Strip `//`-comment tails and whole-line comments. Crude on purpose: the doc comment in
/// `build_jail.rs` that says "NOT `std::fs::canonicalize`" is exactly the shape that made a raw
/// grep report a call site that does not exist, and reading that miscount as a finding is the
/// failure this function prevents.
fn code_only(src: &str) -> String {
    src.lines()
        .map(|l| l.trim_start())
        .filter(|l| !l.starts_with("//"))
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Everything before the file's `#[cfg(test)]` module.
///
/// ⛔ TEST FIXTURES LEGITIMATELY USE THE STD CANONICALIZER, and excluding them is NOT a weakening.
/// A fixture calls it to learn a `tempdir()`'s real path — `std::fs::canonicalize(tmp.path())` —
/// which never becomes a grant and never reaches the Windows compiler. MEASURED when this guard was
/// written: it flagged 10 sites, every one of them a `.expect("canonical home")` / `.expect("fixture")`
/// inside a test module, and ZERO in production code. Keeping them would make the guard permanently
/// red, which is a check nobody can act on.
///
/// ⛔⛔ "CUT AT THE FIRST `#[cfg(test)]`" IS WRONG, AND THE CONTROL BELOW CAUGHT IT. `curated.rs`
/// carries a `#[cfg(test)] impl CuratedGrant` helper at line 262 and then continues with REAL
/// production code — `grant_curated_package`, `curated_table`, `grant_from_table`. Truncating there
/// discarded 85% of the file INCLUDING the grant builders, so the guard scanned almost nothing and
/// passed. That is the vacuous-green shape this repo keeps rediscovering, and it was one measurement
/// away from shipping as a "passing" audit.
///
/// So skip each `#[cfg(test)]` ITEM by brace depth and keep everything else. Comments are stripped
/// first, because a `{` inside one would desynchronise the depth count.
fn production_only(src: &str) -> String {
    let mut out = Vec::new();
    let mut pending = false; // saw the attribute, waiting for the item's opening brace
    let mut depth: i32 = 0;
    let mut skip_from: Option<i32> = None;
    let stripped = code_only(src);
    for line in stripped.lines() {
        if skip_from.is_none() && line.trim_start().starts_with("#[cfg(test)]") {
            pending = true;
            continue;
        }
        let opens = line.matches('{').count() as i32;
        let closes = line.matches('}').count() as i32;
        if pending {
            if opens > 0 {
                skip_from = Some(depth);
                pending = false;
            } else if line.trim_end().ends_with(';') {
                // ⛔ A BRACELESS `#[cfg(test)]` ITEM — `#[cfg(test)] use tempfile::TempDir;` or
                // `mod tests;`. MEASURED: without this arm `pending` stayed set, the NEXT production
                // `fn … {` was swallowed as if it were the test item, and `preset.rs` retained 9% of
                // its bytes. That is the same vacuous-green failure as the truncate version, reached
                // by a different route — which is why the retention control is asserted, not assumed.
                pending = false;
                continue;
            }
        }
        let before = depth;
        depth += opens - closes;
        match skip_from {
            // The item closes when depth returns to what it was before its brace opened.
            Some(base) if depth <= base && before > base => skip_from = None,
            Some(_) => {}
            None => out.push(line),
        }
    }
    out.join("\n")
}

#[test]
fn grant_builders_never_use_std_fs_canonicalize() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();
    for rel in GRANT_BUILDERS {
        let src = std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| {
            panic!("{rel} is unreadable ({e}) — the guard would pass vacuously")
        });
        for (n, line) in production_only(&src).lines().enumerate() {
            if line.contains("fs::canonicalize") {
                offenders.push(format!("{rel}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "grant-building code must use `canonicalize_including_nonexistent`, never \
         `std::fs::canonicalize` — on Windows the latter's `\\\\?\\` prefix makes the compiler \
         read `?` as a glob metachar and DROP the grant:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_guard_reads_real_files_and_is_not_vacuous() {
    // Without this, a renamed file would make the case above pass by scanning nothing.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for rel in GRANT_BUILDERS {
        let src = std::fs::read_to_string(root.join(rel)).expect("grant builder must exist");
        assert!(
            src.len() > 500,
            "{rel} is implausibly small — is this the right file?"
        );
        // ⛔ THE CONTROL ON THE TEST-MODULE EXCLUSION. If the exclusion ever swallowed a file, the
        // guard above would pass by scanning an empty string — permanently green, undetectably.
        //
        // ⛔ COMPARE LIKE WITH LIKE. `production_only` returns COMMENT-STRIPPED, DE-INDENTED text, so
        // measuring it against the RAW file length is a broken instrument, not a finding: it read
        // 9% and looked exactly like a real over-skip, which cost two wrong fixes before the ratio
        // was probed. The denominator must be `code_only`.
        //
        // MEASURED 2026-08-07 — production share of stripped code: preset 32%, curated 37%,
        // defaults 49%. These files really are half test code. 20% leaves margin without letting a
        // swallowed file through.
        let kept = production_only(&src);
        let code = code_only(&src);
        assert!(
            kept.len() * 5 > code.len(),
            "{rel}: the #[cfg(test)] exclusion kept only {} of {} code bytes — the guard would be \
             scanning almost nothing.",
            kept.len(),
            code.len()
        );
    }
}

#[test]
fn the_exclusion_keeps_the_actual_grant_builders() {
    // ⛔ THE CASE THAT CAUGHT THE FIRST IMPLEMENTATION. `curated.rs` has a `#[cfg(test)] impl` at
    // line 262 followed by real production code; a naive truncate-at-first-marker dropped
    // `grant_curated_package` and friends, so the guard scanned 15% of the file and passed.
    // Naming the symbols makes that unrepeatable.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (rel, symbol) in [
        ("src/compiler/curated.rs", "fn grant_curated_package"),
        ("src/compiler/curated.rs", "fn curated_table"),
        ("src/compiler/preset.rs", "fn "),
    ] {
        let src = std::fs::read_to_string(root.join(rel)).expect("readable");
        assert!(
            production_only(&src).contains(symbol),
            "{rel}: `{symbol}` was excluded as test code — the guard is not scanning the grant builder"
        );
    }
}

#[test]
fn the_test_module_exclusion_keeps_production_code_and_drops_fixtures() {
    // Both polarities again: a fixture call after the marker is dropped, a production call before
    // it survives. An exclusion that dropped everything would make the whole guard vacuous.
    let src = "fn build() { std::fs::canonicalize(p) }\n#[cfg(test)]\nmod tests { fn f() { std::fs::canonicalize(t) } }";
    let prod = production_only(src);
    assert!(
        prod.contains("fs::canonicalize"),
        "a production call must survive the exclusion"
    );
    assert!(
        !prod.contains("mod tests"),
        "the test module must be excluded"
    );
}

#[test]
fn the_comment_stripper_does_not_hide_a_real_call() {
    // ⛔ BOTH POLARITIES. The stripper must drop the doc-comment mention that made a raw grep
    // miscount, and must NOT drop a genuine call — a stripper that ate everything would make the
    // guard permanently green, which is the vacuous shape this repo keeps rediscovering.
    let commented = "/// The matcher's canonicalizer, NOT `std::fs::canonicalize`.\n// std::fs::canonicalize(p)";
    assert!(
        !code_only(commented).contains("fs::canonicalize"),
        "comment mentions must be stripped"
    );

    let real = "    let root = std::fs::canonicalize(p)?;";
    assert!(
        code_only(real).contains("fs::canonicalize"),
        "a genuine call must survive stripping"
    );

    let trailing = "    let x = 1; // see std::fs::canonicalize";
    assert!(
        !code_only(trailing).contains("fs::canonicalize"),
        "a trailing comment must be stripped"
    );
}

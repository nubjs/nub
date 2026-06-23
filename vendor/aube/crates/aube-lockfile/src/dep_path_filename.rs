//! Bounded, filename-safe encoding of pnpm dep paths.
//!
//! pnpm dep paths embed the full peer-dependency graph in parentheses
//! (e.g. `@fig/eslint-config-autocomplete@2.0.0(@typescript-eslint+parser@7.18.0(eslint@8.57.1))...`),
//! which blows through Linux's 255-byte filename limit on
//! peer-heavy graphs. Port pnpm's `depPathToFilename` from
//! `/tmp/pnpm/deps/path/src/index.ts`: escape forbidden characters,
//! flatten parentheses into `_`, and — if the result is still too
//! long — truncate and append a short sha256-based hash so collisions
//! are vanishingly unlikely.
//!
//! Keep the truncation math bit-for-bit compatible with pnpm
//! (`max_length - 33` prefix + `_` + 32 hex chars) so the fingerprint
//! matches what pnpm itself produces at the same `maxLength`.

/// Default `virtual-store-dir-max-length`, matching pnpm's Linux/macOS
/// default. pnpm uses 60 on Windows — we don't run on Windows yet, so
/// we only expose the POSIX default for now.
pub const DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH: usize = 120;

/// Encode a pnpm dep path as a filesystem-safe directory name no
/// longer than `max_length` bytes. Port of pnpm's `depPathToFilename`.
pub fn dep_path_to_filename(dep_path: &str, max_length: usize) -> String {
    let mut filename = escape_unescaped(dep_path);

    if filename.contains('(') {
        if filename.ends_with(')') {
            filename.pop();
        }
        let mut out = String::with_capacity(filename.len());
        let mut chars = filename.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                ')' if chars.peek() == Some(&'(') => {
                    chars.next();
                    out.push('_');
                }
                '(' | ')' => out.push('_'),
                other => out.push(other),
            }
        }
        filename = out;
    }

    let has_upper = filename.bytes().any(|b| b.is_ascii_uppercase());
    let needs_hash = filename.len() > max_length || (has_upper && !filename.starts_with("file+"));

    if needs_hash {
        // The hash tail is `_` plus 32 hex chars = 33 bytes, so any
        // `max_length` below 34 can't fit even a bare hash suffix.
        // pnpm's minimum default is 60 (Windows), so this only bites
        // if a caller overrides the cap with something tiny — catch it
        // loudly in debug builds rather than silently returning a
        // 33-char output for a nominally-smaller cap.
        debug_assert!(
            max_length > 33,
            "virtual-store-dir-max-length ({max_length}) must be > 33 to fit the hash suffix"
        );
        let prefix_len = max_length.saturating_sub(33);
        let short = short_hash(&filename);
        let mut out = String::with_capacity(max_length);
        // Truncate on a byte boundary that's also a char boundary.
        let prefix_end = floor_char_boundary(&filename, prefix_len);
        out.push_str(&filename[..prefix_end]);
        out.push('_');
        out.push_str(&short);
        return out;
    }

    filename
}

fn escape_unescaped(dep_path: &str) -> String {
    // pnpm's `depPathToFilenameUnescaped` + the top-level
    // `replace(/[\\/:*?"<>|#]/g, '+')`. We fold them into one pass.
    let mut out = String::with_capacity(dep_path.len());

    // Handle the `file:` / leading-slash / scope-package prefixes
    // exactly as pnpm does before the global character replacement.
    let rest: &str = if let Some(r) = dep_path.strip_prefix("file:") {
        out.push_str("file+");
        r
    } else {
        dep_path.strip_prefix('/').unwrap_or(dep_path)
    };

    for ch in rest.chars() {
        match ch {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '#' => out.push('+'),
            c => out.push(c),
        }
    }

    out
}

fn short_hash(input: &str) -> String {
    // First 32 hex chars of BLAKE3(input). pnpm uses sha256 here; aube
    // doesn't share the `.aube/<encoded>/` layout with pnpm so no
    // interop breaks. BLAKE3 is the project default for non-crypto
    // hashes (3-5x faster than SHA-256 for short inputs).
    let digest = blake3::hash(input.as_bytes());
    digest.to_hex()[..32].to_string()
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: usize = DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH;

    #[test]
    fn short_path_passes_through() {
        assert_eq!(dep_path_to_filename("foo@1.0.0", MAX), "foo@1.0.0");
    }

    #[test]
    fn scope_slash_becomes_plus() {
        assert_eq!(
            dep_path_to_filename("@scope/foo@1.0.0", MAX),
            "@scope+foo@1.0.0"
        );
    }

    #[test]
    fn parens_flatten_to_underscore() {
        // `foo@1.0.0(peer@2.0.0)` — trailing `)` dropped, `(` → `_`.
        assert_eq!(
            dep_path_to_filename("foo@1.0.0(peer@2.0.0)", MAX),
            "foo@1.0.0_peer@2.0.0"
        );
    }

    #[test]
    fn nested_parens_flatten() {
        // Matches pnpm's output: strip one trailing `)`, then turn
        // every remaining `)(`/`(`/`)` into `_`. The inner closing
        // paren becomes a trailing `_`, which is how pnpm writes it.
        assert_eq!(dep_path_to_filename("a@1(b@2(c@3))", MAX), "a@1_b@2_c@3_");
    }

    #[test]
    fn long_path_is_truncated_and_hashed() {
        let long = format!("foo@1.0.0({})", "a@1.0.0".repeat(60));
        let got = dep_path_to_filename(&long, MAX);
        assert_eq!(got.len(), MAX);
        // Hash segment is 32 hex chars preceded by `_`.
        assert!(got.as_bytes()[MAX - 33] == b'_');
    }

    #[test]
    fn long_path_is_deterministic() {
        let long = format!("foo@1.0.0({})", "a@1.0.0".repeat(60));
        assert_eq!(
            dep_path_to_filename(&long, MAX),
            dep_path_to_filename(&long, MAX)
        );
    }

    #[test]
    fn different_long_paths_produce_different_hashes() {
        let a = format!("foo@1.0.0({})", "a@1.0.0".repeat(60));
        let b = format!("foo@1.0.0({})", "b@1.0.0".repeat(60));
        assert_ne!(dep_path_to_filename(&a, MAX), dep_path_to_filename(&b, MAX));
    }

    #[test]
    fn uppercase_forces_hash_unless_file_prefix() {
        // Mixed-case names fall into the hashed branch so two packages
        // that differ only in case end up at distinct directory names
        // on case-insensitive filesystems.
        let got = dep_path_to_filename("Foo@1.0.0", MAX);
        assert!(got.contains('_'));
        assert!(got.len() > "Foo@1.0.0".len());

        // `file:` deps skip the case rule — they're local paths where
        // we preserve what the user typed.
        let got = dep_path_to_filename("file:../Foo", MAX);
        assert_eq!(got, "file+..+Foo");
    }

    #[test]
    fn fig_eslint_config_autocomplete_fits_in_255_bytes() {
        // Regression for the mise install failure: this is the exact
        // dep_path that overflowed ext4's 255-byte NAME_MAX.
        let dep_path = "@fig/eslint-config-autocomplete@2.0.0(@typescript-eslint+eslint-plugin@7.18.0(@typescript-eslint+parser@7.18.0(eslint@8.57.1))(eslint@8.57.1))(@typescript-eslint+parser@7.18.0(eslint@8.57.1))(@withfig+eslint-plugin-fig-linter@1.4.1)(eslint@8.57.1)(eslint-plugin-compat@4.2.0(eslint@8.57.1))(typescript@5.9.3)";
        let got = dep_path_to_filename(dep_path, MAX);
        assert!(got.len() <= MAX, "got {} bytes: {got}", got.len());
    }

    #[test]
    fn multi_byte_chars_do_not_split() {
        // Construct a path that would land the truncation point
        // mid-codepoint. We just assert that the output is valid UTF-8
        // (guaranteed by `String`) and not longer than max_length.
        let s: String = "π".repeat(200);
        let got = dep_path_to_filename(&s, MAX);
        assert!(got.len() <= MAX);
    }
}

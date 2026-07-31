//! Nesting bound for externally-authored JSON-family text, applied to the raw
//! source before a recursive-descent parser is handed it.
//!
//! The parsers nub embeds descend recursively, and the `serde_json::Value` they
//! produce drops recursively too, so a deeply-nested document exhausts the stack.
//! That is not a catchable failure — it aborts the process (`SIGSEGV`, or
//! `STATUS_STACK_OVERFLOW` on Windows) before any validation can reject the file.
//! Windows sees it first: its main-thread stack budget is ~1 MiB against ~8 MiB
//! on Linux and macOS.
//!
//! Measured on the `fast` profile, feeding `[[[…1…]]]` through the addon's
//! `parseJson5` (json5 0.4 imposes no bound of its own): abort between 2500 and
//! 3000 levels on a stock 8 MiB macOS stack, and between 300 and 400 on a 1 MiB
//! one. `jsonc_parser` 0.32.4 happens to stop at 512 internally — above what a
//! 1 MiB stack survives, and an upstream implementation detail no test of ours
//! pins and a version bump can remove — so JSONC is bounded here too rather than
//! left to it.
//!
//! The bound is applied to the TEXT because that is the only point at which the
//! recursion can still be stopped: none of these parsers exposes a depth option,
//! and by the time one returns, the stack has already been spent.

/// Byte-scan for the deepest `{`/`[` nesting, ignoring delimiters inside strings
/// and comments. Bytes are safe to scan directly: every delimiter is ASCII and
/// UTF-8 continuation bytes are all >= 0x80, so no multi-byte character can be
/// mistaken for one.
///
/// `max_depth` is the caller's, not a constant here, because the two callers
/// bound different things — nub's own config schema versus arbitrary user data
/// reaching the runtime's data-format loaders.
pub fn check_nesting_depth(text: &str, max_depth: usize) -> Result<(), String> {
    #[derive(Clone, Copy)]
    enum State {
        Code,
        /// Inside a string opened by this quote byte. JSONC and JSON5 both accept
        /// single-quoted strings, so the closing quote must match the opener.
        Str(u8),
        LineComment,
        BlockComment,
    }

    let bytes = text.as_bytes();
    let mut state = State::Code;
    let mut depth: usize = 0;
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        match state {
            State::Code => match b {
                b'"' | b'\'' => state = State::Str(b),
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    state = State::LineComment;
                    i += 1;
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    state = State::BlockComment;
                    i += 1;
                }
                b'{' | b'[' => {
                    depth += 1;
                    if depth > max_depth {
                        return Err(format!(
                            "nesting is deeper than the {max_depth}-level limit"
                        ));
                    }
                }
                b'}' | b']' => depth = depth.saturating_sub(1),
                _ => {}
            },
            State::Str(quote) => match b {
                // Covers JSON5's escaped-newline line continuation as well as an
                // escaped quote: either way the next byte is not a delimiter.
                b'\\' => i += 1,
                _ if b == quote => state = State::Code,
                _ => {}
            },
            State::LineComment => {
                if b == b'\n' {
                    state = State::Code;
                }
            }
            State::BlockComment => {
                if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    state = State::Code;
                    i += 1;
                }
            }
        }
        i += 1;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nest(depth: usize) -> String {
        format!("{}1{}", "[".repeat(depth), "]".repeat(depth))
    }

    #[test]
    fn accepts_up_to_the_limit_and_rejects_past_it() {
        assert!(check_nesting_depth(&nest(64), 64).is_ok());
        let over = check_nesting_depth(&nest(65), 64).unwrap_err();
        assert!(over.contains("deeper than the 64-level"), "{over}");

        // The depths that abort the process unguarded, at both stack budgets.
        assert!(check_nesting_depth(&nest(400), 128).is_err());
        assert!(check_nesting_depth(&nest(3000), 128).is_err());
    }

    #[test]
    fn depth_counts_only_real_delimiters() {
        // Brackets inside strings, escapes, and both comment forms are not nesting.
        let ok = |t: &str| check_nesting_depth(t, 64).is_ok();
        assert!(ok(&format!(r#"{{ "a": "{}" }}"#, "[".repeat(500))));
        assert!(ok(&format!(r#"{{ "a": '{}' }}"#, "{".repeat(500))));
        assert!(ok(r#"{ "a": "he said \"[[[\"" }"#));
        assert!(ok(&format!("// {}\n{{}}", "[".repeat(500))));
        assert!(ok(&format!("/* {} */ {{}}", "[".repeat(500))));

        // Sibling blocks nest independently — depth is a maximum, not a total.
        assert!(ok(&format!("[{}]", "[1],".repeat(500))));
    }

    /// A file truncated mid-string or mid-comment leaves the scanner in a
    /// non-Code state at EOF. That must simply end the scan and report no
    /// violation, so the parser — not this guard — decides the outcome.
    #[test]
    fn truncated_input_ends_the_scan_rather_than_failing_it() {
        assert!(check_nesting_depth("{ \"a\": \"unterminated", 64).is_ok());
        assert!(check_nesting_depth("/* unterminated", 64).is_ok());
        assert!(check_nesting_depth("// unterminated", 64).is_ok());
    }
}

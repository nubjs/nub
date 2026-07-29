//! The guarded JSONC reader every externally-authored file goes through.
//!
//! `jsonc_parser`'s descent is unbounded, as is the recursive `Drop` of the
//! `serde_json::Value` it produces, so a deeply-nested document exhausts the
//! stack. That is not a catchable failure — it aborts the process
//! (`STATUS_STACK_OVERFLOW` on Windows, `SIGSEGV` elsewhere), and it happens
//! before any validation can reject the file. Windows is where it bites first:
//! its default thread stack is ~1 MiB against ~8 MiB on Linux and macOS.
//!
//! Depth is therefore capped on the RAW TEXT, before the parser is handed it —
//! the only point at which the recursion can still be bounded, since the parser
//! exposes no depth option.
//!
//! [`read_guarded`] is the other half of the same job: these files arrive by a
//! path the user chose, so obtaining the bytes is itself unbounded work unless
//! the read is bounded too.

use std::io::Read;
use std::path::Path;

use jsonc_parser::ParseOptions;
use serde_json::Value;

/// Deepest `{`/`[` nesting accepted. Nub's own schema bottoms out around three
/// levels (`install.linker.hoist[]`), so this leaves an order of magnitude of
/// headroom for hand-authored files while staying far below the ~380 levels a
/// debug build survives on a 1 MiB Windows stack.
pub(crate) const MAX_NESTING_DEPTH: usize = 64;

/// Parse JSONC after bounding its nesting. `Ok(None)` is an empty or
/// comment-only document; the error is a human-readable message for the caller
/// to wrap in its own error type.
pub(crate) fn parse_to_value(text: &str) -> Result<Option<Value>, String> {
    check_nesting_depth(text)?;
    jsonc_parser::parse_to_serde_value(text, &ParseOptions::default()).map_err(|e| e.to_string())
}

/// Largest externally-authored file read into memory. A fully-populated
/// `nub.jsonc` is a couple of kilobytes and the tsconfig one points at is a
/// hand-written sibling, so a megabyte is three orders of magnitude of headroom
/// — matching [`crate::phantom_scan`]'s bound on a hand-written source file.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Read an externally-authored file under both a type and a size bound.
///
/// The type check comes first, and is what turns a character device or a
/// writer-less FIFO into an error instead of a hang: `stat` answers where
/// `open` would block forever, and it follows symlinks, so a link to
/// `/dev/zero` is judged by its target.
///
/// The size bound is then enforced by [`Read::take`] rather than by
/// `metadata.len()`, because a regular file may under-report its length
/// (`/proc` entries declare 0) — the declared length is a hint, the bounded
/// read is the ceiling.
pub(crate) fn read_guarded(path: &Path) -> std::io::Result<String> {
    if !std::fs::metadata(path)?.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }

    // Read bytes, not a String: `take` can cut a multi-byte character in half,
    // which would report an over-cap file as invalid UTF-8.
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!(
                "larger than the {} MiB limit",
                MAX_FILE_BYTES / (1024 * 1024)
            ),
        ));
    }
    String::from_utf8(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.utf8_error()))
}

/// Byte-scan for the deepest `{`/`[` nesting, ignoring delimiters inside strings
/// and comments. Bytes are safe to scan directly: every delimiter is ASCII and
/// UTF-8 continuation bytes are all >= 0x80, so no multi-byte character can be
/// mistaken for one.
fn check_nesting_depth(text: &str) -> Result<(), String> {
    #[derive(Clone, Copy)]
    enum State {
        Code,
        /// Inside a string opened by this quote byte. `jsonc_parser` accepts
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
                    if depth > MAX_NESTING_DEPTH {
                        return Err(format!(
                            "nesting is deeper than the {MAX_NESTING_DEPTH}-level limit"
                        ));
                    }
                }
                b'}' | b']' => depth = depth.saturating_sub(1),
                _ => {}
            },
            State::Str(quote) => match b {
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

    fn depth_ok(text: &str) -> bool {
        check_nesting_depth(text).is_ok()
    }

    fn nest(depth: usize) -> String {
        format!("{}1{}", "[".repeat(depth), "]".repeat(depth))
    }

    #[test]
    fn rejects_nesting_past_the_limit_before_the_parser_recurses() {
        // The depth that aborted the Windows test binary must now be a clean Err,
        // and the limit itself must still parse.
        assert!(depth_ok(&nest(MAX_NESTING_DEPTH)));
        assert!(parse_to_value(&nest(MAX_NESTING_DEPTH)).is_ok());

        let over = parse_to_value(&nest(MAX_NESTING_DEPTH + 1)).unwrap_err();
        assert!(over.contains("deeper than"), "{over}");
        assert!(
            parse_to_value(&nest(2000)).is_err(),
            "the CI-crashing input"
        );
    }

    #[test]
    fn depth_counts_only_real_delimiters() {
        // Brackets inside strings, escapes, and both comment forms are not nesting.
        assert!(depth_ok(&format!(r#"{{ "a": "{}" }}"#, "[".repeat(500))));
        assert!(depth_ok(&format!(r#"{{ "a": '{}' }}"#, "{".repeat(500))));
        assert!(depth_ok(r#"{ "a": "he said \"[[[\"" }"#));
        assert!(depth_ok(&format!("// {}\n{{}}", "[".repeat(500))));
        assert!(depth_ok(&format!("/* {} */ {{}}", "[".repeat(500))));

        // Sibling blocks nest independently — depth is a maximum, not a total.
        let siblings = format!("[{}]", "[1],".repeat(500));
        assert!(depth_ok(&siblings));
        assert!(parse_to_value(&siblings).is_ok());
    }

    /// The guard is only worth as much as its coverage: a single direct call to
    /// `jsonc_parser` re-opens the abort on a different door, and that door would
    /// be invisible to a test asserting the guarded path. Bypasses are therefore
    /// caught here rather than by reviewer vigilance.
    #[test]
    fn no_module_parses_jsonc_outside_this_one() {
        fn rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("readable source dir") {
                let path = entry.expect("readable dir entry").path();
                if path.is_dir() {
                    rs_files(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }

        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rs_files(&src, &mut files);
        assert!(files.len() > 1, "source walk found nothing at {src:?}");

        let offenders: Vec<_> = files
            .iter()
            .filter(|path| path.file_name().is_some_and(|n| n != "jsonc.rs"))
            .filter(|path| {
                std::fs::read_to_string(path)
                    .expect("readable source file")
                    .contains("parse_to_serde_value")
            })
            .collect();

        assert!(
            offenders.is_empty(),
            "these call jsonc_parser directly and so skip MAX_NESTING_DEPTH; \
             route them through jsonc::parse_to_value: {offenders:#?}"
        );
    }

    #[test]
    fn reads_stop_at_the_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let at_limit = dir.path().join("at-limit");
        let over_limit = dir.path().join("over-limit");
        std::fs::write(&at_limit, vec![b'/'; MAX_FILE_BYTES as usize]).unwrap();
        std::fs::write(&over_limit, vec![b'/'; MAX_FILE_BYTES as usize + 1]).unwrap();

        assert_eq!(
            read_guarded(&at_limit).unwrap().len() as u64,
            MAX_FILE_BYTES
        );
        let over = read_guarded(&over_limit).unwrap_err();
        assert!(over.to_string().contains("1 MiB limit"), "{over}");
    }

    #[test]
    fn truncated_input_passes_through_to_the_parser_verdict() {
        // A file truncated mid-string or mid-comment leaves the scanner in a
        // non-Code state at EOF. That must simply end the scan, so the parser --
        // not the depth guard -- decides the outcome.
        assert!(parse_to_value("{ \"a\": \"unterminated").is_err());
        assert!(parse_to_value("/* unterminated").is_err());
        assert!(parse_to_value("// unterminated").is_ok_and(|v| v.is_none()));
    }
}

//! The guarded JSONC reader every externally-authored file goes through.
//!
//! `jsonc_parser` descends recursively, as does the `Drop` of the
//! `serde_json::Value` it produces, so a deeply-nested document exhausts the
//! stack. That is not a catchable failure — it aborts the process
//! (`STATUS_STACK_OVERFLOW` on Windows, `SIGSEGV` elsewhere), and it happens
//! before any validation can reject the file. Windows is where it bites first:
//! its default thread stack is ~1 MiB against ~8 MiB on Linux and macOS.
//!
//! Depth is therefore capped on the RAW TEXT, before the parser is handed it —
//! the only point at which the recursion can still be bounded, since the parser
//! exposes no depth option. `jsonc_parser` 0.32.4 does stop at 512 levels of its
//! own accord, but that is an upstream detail a version bump can remove, and it
//! sits above what a 1 MiB stack survives, so nub does not rely on it.
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
    let parsed: Option<Value> = jsonc_parser::parse_to_serde_value(text, &ParseOptions::default())
        .map_err(|e| e.to_string())?;
    if parsed.is_some() {
        return Ok(parsed);
    }
    // Deserializing into `Option<T>` maps a literal `null` document to `None`,
    // which is indistinguishable from the empty one — and the callers treat an
    // empty document as a valid EMPTY CONFIG, so collapsing the two would turn a
    // file nub must reject into one it silently accepts, dropping every setting
    // in it (`dlx.consent` included). The AST reports presence, not value.
    let present = jsonc_parser::parse_to_value(text, &ParseOptions::default())
        .map_err(|e| e.to_string())?
        .is_some();
    Ok(present.then_some(Value::Null))
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
    // Windows editors and PowerShell's `Out-File` write a UTF-8 BOM by default,
    // and the JSONC parser reports one as a syntax error at line 1 column 1 —
    // pointing at a file that looks perfectly correct in the editor that wrote
    // it. Stripping it here covers every reader and the CST writer alike, so a
    // BOM'd file round-trips instead of being rejected wholesale. Only a LEADING
    // one is a marker; anywhere else U+FEFF is data and must survive verbatim.
    let bytes = bytes
        .strip_prefix(UTF8_BOM)
        .map_or(bytes.as_slice(), |rest| rest)
        .to_vec();
    String::from_utf8(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.utf8_error()))
}

/// The UTF-8 encoding of U+FEFF. Named so the reader that strips it and the
/// writer that puts it back cannot drift.
pub(crate) const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Whether the file at `path` opens with a UTF-8 BOM — what [`read_guarded`]
/// stripped, so a rewriter can restore it. An absent or unreadable file is
/// `false`: there is no prior marker to carry, and inventing one would be its
/// own surprise.
pub(crate) fn starts_with_bom(path: &Path) -> bool {
    let mut head = [0u8; 3];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok_and(|()| head == *UTF8_BOM)
}

/// Bound this text's nesting at [`MAX_NESTING_DEPTH`].
///
/// Callable on its own because the CST writer in [`crate::config`] hands the same
/// externally-authored text to `jsonc_parser::cst`, which descends recursively in
/// exactly the same way — the guard belongs to the text, not to one parser entry
/// point.
///
/// The scan itself lives in `nub-json-guard` so the addon, which parses the same
/// JSON-family formats in its own workspace, shares this implementation and its
/// tests rather than carrying a copy that could drift.
pub(crate) fn check_nesting_depth(text: &str) -> Result<(), String> {
    nub_json_guard::check_nesting_depth(text, MAX_NESTING_DEPTH)
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

    /// `Ok(None)` means "the document held no value", never "the value was
    /// null" — the callers read the first as a valid EMPTY config, so merging
    /// the two would make a `null` file silently drop every setting instead of
    /// being rejected. `parse_to_serde_value`'s `Option<T>` target collapses
    /// them, which is why this is asserted rather than assumed.
    #[test]
    fn a_null_document_is_a_value_and_an_empty_one_is_not() {
        assert_eq!(parse_to_value("null").unwrap(), Some(Value::Null));
        for empty in ["", "   \n\t ", "// just a comment\n", "/* block */"] {
            assert_eq!(parse_to_value(empty).unwrap(), None, "{empty:?}");
        }
        assert_eq!(
            parse_to_value(r#"{ "a": null }"#).unwrap(),
            Some(serde_json::json!({ "a": null }))
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

    /// The guard is only worth as much as its coverage: a single direct call to a
    /// recursive-descent JSON-family parser re-opens the abort on a different
    /// door, and that door would be invisible to a test asserting the guarded
    /// path. Bypasses are therefore caught here rather than by reviewer vigilance.
    ///
    /// The walk spans every crate, not just this one, because the addon reaches
    /// these parsers from its OWN workspace — `nub-native` carries `test = false`
    /// (a cdylib cannot link a test harness against napi symbols), so nothing
    /// inside it can assert this and a scan scoped to `nub-cli/src` reported the
    /// gap as closed while three unguarded calls sat one directory over.
    #[test]
    fn no_module_parses_json_without_bounding_its_nesting() {
        fn rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("readable source dir") {
                let path = entry.expect("readable dir entry").path();
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                if path.is_dir() {
                    rs_files(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }

        // The entry points that recurse on nesting with no bound of their own, or
        // with one too loose to survive a 1 MiB stack.
        const UNGUARDED_PARSERS: [&str; 2] = ["parse_to_serde_value", "json5::from_str"];

        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates dir");
        let mut files = Vec::new();
        rs_files(crates, &mut files);
        assert!(files.len() > 1, "source walk found nothing at {crates:?}");

        // A file may reach one of those parsers only if it also names the bound.
        // Co-occurrence rather than an allowlist of blessed paths: an allowlist
        // says nothing once a file is on it, so deleting the guard from a listed
        // file would still pass. This fails on both moves — a new unguarded call
        // site, and the bound being dropped from an existing one.
        let offenders: Vec<_> = files
            .iter()
            .filter(|path| {
                let text = std::fs::read_to_string(path).expect("readable source file");
                UNGUARDED_PARSERS.iter().any(|p| text.contains(p))
                    && !text.contains("check_nesting_depth")
            })
            .collect();

        assert!(
            offenders.is_empty(),
            "these parse JSON-family text without bounding its nesting, so a deep \
             document aborts the process; call check_nesting_depth on the source \
             first: {offenders:#?}"
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

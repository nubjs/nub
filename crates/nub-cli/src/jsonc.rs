//! The guarded JSONC reader every externally-authored file goes through.
//!
//! Two bounds, each applied before the unbounded work it stands in front of.
//! Nesting depth is capped on the RAW TEXT by [`nub_json_guard`], whose module
//! doc carries the measured stack-overflow rationale: the parser exposes no
//! depth option, so the text is the last point at which its recursion can still
//! be stopped. [`read_guarded`] is the other half — these files arrive by a path
//! the user chose, so obtaining the bytes is itself unbounded work unless the
//! read is bounded too.

use std::io::Read;
use std::path::Path;

use jsonc_parser::ParseOptions;
use serde_json::Value;

/// Deepest `{`/`[` nesting accepted. Nub's own schema bottoms out around three
/// levels (`install.linker.hoist[]`), so this leaves an order of magnitude of
/// headroom for hand-authored files while staying far below the ~380 levels a
/// debug build survives on a 1 MiB Windows stack.
pub(crate) const MAX_NESTING_DEPTH: usize = 64;

/// Pin acceptance to the JSONC dialect nub documented before `jsonc-parser`
/// 0.32 added more permissive defaults. Keep this as the one CLI definition:
/// project/global readers and the comment-preserving CST writer must accept the
/// same documents, while the separately-built native addon mirrors these fields
/// in its standalone workspace.
pub(crate) fn parse_options() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_loose_object_property_names: true,
        allow_trailing_commas: true,
        allow_missing_commas: false,
        allow_single_quoted_strings: true,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

/// Parse JSONC after bounding its nesting. `Ok(None)` is an empty or
/// comment-only document; the error is a human-readable message for the caller
/// to wrap in its own error type.
pub(crate) fn parse_to_value(text: &str) -> Result<Option<Value>, String> {
    check_nesting_depth(text)?;
    let parsed: Option<Value> =
        jsonc_parser::parse_to_serde_value(text, &parse_options()).map_err(|e| e.to_string())?;
    if parsed.is_some() {
        return Ok(parsed);
    }
    // Deserializing into `Option<T>` maps a literal `null` document to `None`,
    // which is indistinguishable from the empty one — and the callers treat an
    // empty document as a valid EMPTY CONFIG, so collapsing the two would turn a
    // file nub must reject into one it silently accepts, dropping every setting
    // in it (`dlx.consent` included). The AST reports presence, not value.
    let present = jsonc_parser::parse_to_value(text, &parse_options())
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
    if bytes.starts_with(UTF8_BOM) {
        bytes.drain(..UTF8_BOM.len());
    }
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

    fn rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("readable source dir") {
            let path = entry.expect("readable dir entry").path();
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            if path.is_dir() {
                rs_files(&path, out);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                out.push(path);
            }
        }
    }

    fn nest(depth: usize) -> String {
        format!("{}1{}", "[".repeat(depth), "]".repeat(depth))
    }

    /// What the scan itself counts as nesting is pinned in `nub-json-guard`;
    /// this is the boundary as `parse_to_value` applies it.
    #[test]
    fn rejects_nesting_past_the_limit_before_the_parser_recurses() {
        // The depth that aborted the Windows test binary must now be a clean Err,
        // and the limit itself must still parse.
        assert!(check_nesting_depth(&nest(MAX_NESTING_DEPTH)).is_ok());
        assert!(parse_to_value(&nest(MAX_NESTING_DEPTH)).is_ok());

        let over = parse_to_value(&nest(MAX_NESTING_DEPTH + 1)).unwrap_err();
        assert!(over.contains("deeper than"), "{over}");
        assert!(
            parse_to_value(&nest(2000)).is_err(),
            "the CI-crashing input"
        );

        // Breadth is not depth: siblings nest independently, so an arbitrarily
        // wide document is still within the bound and still parses.
        assert!(parse_to_value(&format!("[{}]", "[1],".repeat(500))).is_ok());
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
        // The entry points that recurse on nesting with no bound of their own, or
        // with one too loose to survive a 1 MiB stack. Each CALL must have its
        // preflight immediately above it: file-wide co-occurrence would let one
        // guarded parser bless a second unguarded parser in the same module.
        const GUARDED_PARSERS: [(&str, &[&str]); 3] = [
            (
                concat!("jsonc_parser::", "parse_to_serde_value"),
                &["check_nesting_depth", "check_depth"],
            ),
            (concat!("json5::", "from_str("), &["check_depth"]),
            (
                concat!("YamlLoader::", "load_from_str("),
                &["check_yaml_depth"],
            ),
        ];

        fn preceding_lines(text: &str, call_at: usize, count: usize) -> &str {
            let mut start = call_at;
            for _ in 0..count {
                let Some(previous) = text[..start].rfind('\n') else {
                    start = 0;
                    break;
                };
                start = previous;
            }
            &text[start..call_at]
        }

        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates dir");
        let mut files = Vec::new();
        rs_files(crates, &mut files);
        assert!(files.len() > 1, "source walk found nothing at {crates:?}");

        // A parser call may appear only when one of its accepted guards is in
        // the preceding twelve lines. The local window catches a new unguarded
        // call or a reordered/removed guard without attempting to parse Rust
        // braces inside strings and comments.
        let mut offenders = Vec::new();
        for path in &files {
            let source = std::fs::read_to_string(path).expect("readable source file");
            for (parser, guards) in GUARDED_PARSERS {
                for (call_at, _) in source.match_indices(parser) {
                    let preflight = preceding_lines(&source, call_at, 12);
                    if !guards.iter().any(|guard| preflight.contains(guard)) {
                        offenders.push(format!(
                            "{}: `{parser}` with none of {guards:?} above it",
                            path.display()
                        ));
                    }
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "these parser call sites lack their required preflight, so a deep \
             document can abort the process; add the guard before parsing: {offenders:#?}"
        );
    }

    /// The native addon is the only library target in this tree with
    /// `test = false`: its N-API symbols cannot link into a Rust test harness.
    /// Any local `#[cfg(test)]` block there is therefore dead code that reads as
    /// coverage while Cargo and CI silently skip it. Keep those tests in a
    /// testable helper crate or exercise them through the built addon instead.
    #[test]
    fn native_addon_contains_no_dead_cfg_test_blocks() {
        let native = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates dir")
            .join("nub-native/src");
        let mut files = Vec::new();
        rs_files(&native, &mut files);
        assert!(!files.is_empty(), "source walk found nothing at {native:?}");

        let offenders = files
            .iter()
            .filter_map(|path| {
                let source = std::fs::read_to_string(path).expect("readable source file");
                source
                    .lines()
                    .enumerate()
                    .find(|(_, line)| {
                        line.chars()
                            .filter(|ch| !ch.is_whitespace())
                            .collect::<String>()
                            .starts_with("#[cfg(test)]")
                    })
                    .map(|(line, _)| format!("{}:{}", path.display(), line + 1))
            })
            .collect::<Vec<_>>();
        assert!(
            offenders.is_empty(),
            "nub-native has `test = false`, so these test blocks never compile; move their coverage to a testable crate or an addon integration fixture: {offenders:#?}"
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

    /// A file truncated mid-string or mid-comment leaves the depth scanner in a
    /// non-Code state at EOF, which `nub-json-guard` pins as ending the scan
    /// rather than failing it. Here that shows up as the parser — not the guard
    /// — deciding the outcome.
    #[test]
    fn truncated_input_passes_through_to_the_parser_verdict() {
        assert!(parse_to_value("{ \"a\": \"unterminated").is_err());
        assert!(parse_to_value("/* unterminated").is_err());
        assert!(parse_to_value("// unterminated").is_ok_and(|v| v.is_none()));
    }
}

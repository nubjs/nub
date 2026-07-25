//! Purpose-built byte-cursor SUBSET parser for `pnpm-lock.yaml`.
//!
//! The overall approach — walk the raw `&[u8]` with a cursor, scan for
//! structural bytes, and decline to a full parser the moment the input
//! leaves the recognized subset — was inspired by jzon-rs:
//! <https://github.com/Rajaniraiyn/jzon-rs>. We borrow that *shape* (a
//! byte-cursor with a hard decline-on-doubt boundary), not its specific
//! kernels: jzon-rs is a SIMD/SWAR JSON parser whose speed comes from
//! word-at-a-time and hand-vectorized structural scans, none of which is
//! used here. This parser is a plain byte-cursor line scanner — line
//! boundaries are found with `memchr` (whose own SIMD does the bulk
//! scanning), and the single structural `:` separator is matched with a
//! short scalar pass. The constrained pnpm dialect (short keys, tiny
//! values) does not justify bringing jzon-rs's vectorized kernels across.
//!
//! pnpm emits a tightly constrained dialect of YAML: 2-space block
//! indentation, no anchors/aliases, no multi-line scalars, no flow
//! style except small inline maps (`resolution: {...}`, `engines:
//! {...}`) and inline seqs (`os: [linux, darwin]`). That regularity
//! lets us skip a general YAML state machine and walk the bytes
//! directly, using `memchr` to find line boundaries and the single
//! structural `:` separator. This is dramatically cheaper than the
//! event-stream + serde path for large lockfiles, where the bulk of
//! the work is thousands of trivial `key: value` snapshot/dependency
//! lines.
//!
//! ## Default-preserving by construction
//!
//! The parser is a *subset* parser: it recognizes the exact shape pnpm
//! writes and produces the SAME [`RawPnpmLockfile`] the serde path
//! produces. The instant it meets anything outside the recognized
//! subset — an unexpected indent, a flow construct it doesn't model, a
//! multi-document stream, a quoting style it can't normalize — it
//! returns `None` and the caller transparently falls back to the
//! `yaml_serde` parser. So the engine's observable behavior is
//! unchanged: the fast path only ever fires when it can produce a
//! byte-identical result, and everything else degrades to the original
//! parser.
//!
//! Inline flow values (`{...}` / `[...]`) are not re-implemented; the
//! small fragment is handed to `yaml_serde` so the fiddly resolution /
//! variants / `string_or_seq` shapes stay in lockstep with serde.

use super::raw::{
    RawCatalogEntry, RawDepSpec, RawImporter, RawPackageInfo, RawPatchedDependency,
    RawPnpmLockfile, RawSettings, RawSnapshot,
};
use serde::Deserialize;
use std::collections::BTreeMap;

/// Try to parse `content` with the subset parser. Returns `None`
/// whenever the input strays outside the recognized pnpm subset, in
/// which case the caller must fall back to the general YAML parser.
pub(super) fn try_parse(content: &str) -> Option<RawPnpmLockfile> {
    // Multi-document streams (pnpm v11 bootstrap + project doc) are
    // left to the scoring fallback — detecting which document to keep
    // is exactly the heuristic the serde path owns.
    if has_document_separator(content) {
        return None;
    }
    let mut p = Parser::new(content.as_bytes());
    p.parse()
}

/// `---` on its own line (a YAML document separator) signals a
/// multi-document stream we don't handle here. A bare `---`, or `---`
/// followed by whitespace (space/tab/CR) or a `#` comment, all open a
/// document. This is a fast-path HINT only: even if a separator slipped
/// past it, the actual guarantee is structural — a `---`-prefixed line
/// has no `key: ` separator, so `split_key` returns `None` and the whole
/// parse declines to serde. Tightened here only so it isn't misleading.
fn has_document_separator(content: &str) -> bool {
    content.as_bytes().split(|&b| b == b'\n').any(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        match line.strip_prefix(b"---") {
            None => false,
            Some(rest) => rest.is_empty() || matches!(rest[0], b' ' | b'\t' | b'#'),
        }
    })
}

struct Parser<'a> {
    bytes: &'a [u8],
    /// Cursor at the start of the current unconsumed line.
    pos: usize,
}

/// A logical line, split into its leading indent (count of spaces) and
/// the trimmed remainder. Blank/comment lines are skipped before this
/// is ever produced.
struct Line<'a> {
    indent: usize,
    /// Content after the indent, with no trailing `\r`/`\n`. Trailing
    /// spaces are NOT trimmed (pnpm never emits them; if present we
    /// bail to be safe).
    body: &'a [u8],
    /// Byte offset of the start of this line (for rewind).
    start: usize,
}

impl<'a> Parser<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Parser { bytes, pos: 0 }
    }

    /// Peek the next non-blank, non-comment logical line without
    /// consuming it. Returns `None` at EOF.
    fn peek_line(&self) -> Option<Line<'a>> {
        let mut pos = self.pos;
        while pos < self.bytes.len() {
            let nl = memchr::memchr(b'\n', &self.bytes[pos..])
                .map(|i| pos + i)
                .unwrap_or(self.bytes.len());
            let mut raw = &self.bytes[pos..nl];
            if raw.last() == Some(&b'\r') {
                raw = &raw[..raw.len() - 1];
            }
            let indent = raw.iter().take_while(|&&b| b == b' ').count();
            let body = &raw[indent..];
            // Skip blank lines and full-line comments.
            if body.is_empty() || body[0] == b'#' {
                pos = nl + 1;
                continue;
            }
            return Some(Line {
                indent,
                body,
                start: pos,
            });
        }
        None
    }

    /// Consume up to and including the line that `peek_line` returned.
    fn consume_line(&mut self, line: &Line<'a>) {
        let nl = memchr::memchr(b'\n', &self.bytes[line.start..])
            .map(|i| line.start + i + 1)
            .unwrap_or(self.bytes.len());
        self.pos = nl;
    }

    fn parse(&mut self) -> Option<RawPnpmLockfile> {
        let mut lockfile_version: Option<yaml_serde::Value> = None;
        let mut settings = None;
        let mut overrides = None;
        let mut package_extensions_checksum = None;
        let mut pnpmfile_checksum = None;
        let mut catalogs = None;
        let mut patched_dependencies = None;
        let mut ignored_optional_dependencies = None;
        let mut importers = BTreeMap::new();
        let mut packages = BTreeMap::new();
        let mut snapshots = BTreeMap::new();
        let mut time = None;

        while let Some(line) = self.peek_line() {
            // Top-level keys live at indent 0.
            if line.indent != 0 {
                return None;
            }
            let (key, inline) = split_key(line.body)?;
            self.consume_line(&line);
            match key {
                b"lockfileVersion" => {
                    lockfile_version = Some(parse_scalar_value(inline?)?);
                }
                b"packageExtensionsChecksum" => {
                    package_extensions_checksum = Some(scalar_string(inline?)?);
                }
                b"pnpmfileChecksum" => {
                    pnpmfile_checksum = Some(scalar_string(inline?)?);
                }
                b"settings" => {
                    // An inline `settings: {...}` flow map is outside the
                    // block subset these helpers model — they would scan
                    // an empty block and drop the inline entries. Decline
                    // so serde parses it. (Same for every block-bodied key
                    // below.)
                    if inline.is_some() {
                        return None;
                    }
                    settings = Some(self.parse_via_serde::<RawSettings>(2)?);
                }
                b"overrides" => {
                    overrides = Some(self.parse_string_map(inline, 2)?);
                }
                b"catalogs" => {
                    if inline.is_some() {
                        return None;
                    }
                    catalogs = Some(self.parse_catalogs()?);
                }
                b"patchedDependencies" => {
                    if inline.is_some() {
                        return None;
                    }
                    patched_dependencies =
                        Some(self.parse_via_serde::<BTreeMap<String, RawPatchedDependency>>(2)?);
                }
                b"ignoredOptionalDependencies" => {
                    // Inline `[a, b]` or a block seq — delegate.
                    ignored_optional_dependencies = Some(self.parse_seq_value(inline, 2)?);
                }
                b"importers" => {
                    if inline.is_some() {
                        return None;
                    }
                    importers = self.parse_importers()?;
                }
                b"packages" => {
                    if inline.is_some() {
                        return None;
                    }
                    packages = self.parse_packages()?;
                }
                b"snapshots" => {
                    if inline.is_some() {
                        return None;
                    }
                    snapshots = self.parse_snapshots()?;
                }
                b"time" => {
                    time = Some(self.parse_string_map(inline, 2)?);
                }
                // Any top-level key we don't recognize: bail to serde so
                // we never silently drop a field a future pnpm adds.
                _ => {
                    core::hint::cold_path();
                    return None;
                }
            }
        }

        Some(RawPnpmLockfile {
            lockfile_version: lockfile_version?,
            settings,
            overrides,
            package_extensions_checksum,
            pnpmfile_checksum,
            catalogs,
            patched_dependencies,
            ignored_optional_dependencies,
            importers,
            packages,
            snapshots,
            time,
        })
    }

    /// Collect the raw text of a block nested at `>= min_indent` plus
    /// the parent header line, and hand the whole fragment to
    /// `yaml_serde`. Used for sub-trees whose shape is fiddly enough
    /// that re-implementing it risks divergence (settings, catalogs
    /// entries, patchedDependencies). `header` is the already-consumed
    /// parent key (without trailing colon), reconstructed at indent 0
    /// for the fragment.
    fn parse_via_serde<T: for<'de> Deserialize<'de>>(&mut self, min_indent: usize) -> Option<T> {
        let block = self.take_block(min_indent)?;
        // Re-indent: the block lines are at >= min_indent; serde wants
        // them as a top-level mapping, so strip exactly min_indent.
        let dedented = dedent(&block, min_indent)?;
        yaml_serde::from_str::<T>(&dedented).ok()
    }

    /// Gather the raw source text of every line indented at
    /// `>= min_indent`, stopping at the first line with smaller indent
    /// (or EOF). Consumes those lines. Returns the raw bytes as a
    /// UTF-8 string slice range copied out.
    fn take_block(&mut self, min_indent: usize) -> Option<String> {
        let start = self.pos;
        let mut end = self.pos;
        while let Some(line) = self.peek_line() {
            if line.indent < min_indent {
                break;
            }
            self.consume_line(&line);
            end = self.pos;
        }
        std::str::from_utf8(&self.bytes[start..end])
            .ok()
            .map(|s| s.to_string())
    }

    /// Parse a simple `key: scalar` block (string→string) at
    /// `min_indent`. `inline` is the value on the header line, if any.
    ///
    /// Declines (returns `None`, falling back to serde) in two cases that
    /// would otherwise be a silent wrong parse:
    ///   - the value is an INLINE flow map (`{foo: bar}`) — the block
    ///     scanner would ignore it and produce an empty map, while serde
    ///     parses the entries;
    ///   - the block has NO entries — serde reads a missing/null value as
    ///     `None`, not `Some({})`, so an empty result here is ambiguous
    ///     and we let serde decide. (pnpm never writes an empty block
    ///     map; it writes `{}` inline or omits the key.)
    fn parse_string_map(
        &mut self,
        inline: Option<&[u8]>,
        min_indent: usize,
    ) -> Option<BTreeMap<String, String>> {
        if inline.is_some() {
            return None;
        }
        let mut map = BTreeMap::new();
        while let Some(line) = self.peek_line() {
            if line.indent < min_indent {
                break;
            }
            if line.indent != min_indent {
                core::hint::cold_path();
                return None;
            }
            let (k, v) = split_key(line.body)?;
            let v = v?; // must be inline
            self.consume_line(&line);
            map.insert(scalar_key_string(k)?, scalar_string(v)?);
        }
        if map.is_empty() {
            return None;
        }
        Some(map)
    }

    /// Parse a seq value that is either inline (`[a, b]`) on the header
    /// line or a block seq nested below. Delegates to serde via a
    /// reconstructed fragment to keep edge cases exact.
    fn parse_seq_value(&mut self, inline: Option<&[u8]>, min_indent: usize) -> Option<Vec<String>> {
        if let Some(v) = inline {
            return serde_value_from_fragment(v);
        }
        // Block seq: gather indented `- item` lines.
        let block = self.take_block(min_indent)?;
        let dedented = dedent(&block, min_indent)?;
        yaml_serde::from_str::<Vec<String>>(&dedented).ok()
    }

    fn parse_catalogs(&mut self) -> Option<BTreeMap<String, BTreeMap<String, RawCatalogEntry>>> {
        // catalogs:
        //   <catalogName>:
        //     <pkg>:
        //       specifier: ...
        //       version: ...
        // Delegate the whole sub-tree to serde — catalogs are tiny and
        // rare, not worth a bespoke path.
        let block = self.take_block(2)?;
        let dedented = dedent(&block, 2)?;
        yaml_serde::from_str(&dedented).ok()
    }

    fn parse_importers(&mut self) -> Option<BTreeMap<String, RawImporter>> {
        let mut importers = BTreeMap::new();
        while let Some(line) = self.peek_line() {
            if line.indent < 2 {
                break;
            }
            if line.indent != 2 {
                core::hint::cold_path();
                return None;
            }
            // `<importerPath>:` header. pnpm writes an empty importer
            // (a workspace package with no dependencies) as an inline
            // `{}`; everything else is a block body.
            let (name, rest) = split_key(line.body)?;
            let name = scalar_key_string(name)?;
            self.consume_line(&line);
            let imp = if let Some(inline) = rest {
                if inline == b"{}" {
                    default_importer()
                } else {
                    return None;
                }
            } else {
                self.parse_importer_body()?
            };
            importers.insert(name, imp);
        }
        Some(importers)
    }

    fn parse_importer_body(&mut self) -> Option<RawImporter> {
        let mut dependencies = None;
        let mut dev_dependencies = None;
        let mut optional_dependencies = None;
        let mut skipped_optional_dependencies = None;
        while let Some(line) = self.peek_line() {
            if line.indent < 4 {
                break;
            }
            if line.indent != 4 {
                return None;
            }
            let (key, rest) = split_key(line.body)?;
            if rest.is_some() {
                return None;
            }
            self.consume_line(&line);
            let specs = self.parse_dep_specs(6)?;
            match key {
                b"dependencies" => dependencies = Some(specs),
                b"devDependencies" => dev_dependencies = Some(specs),
                b"optionalDependencies" => optional_dependencies = Some(specs),
                b"skippedOptionalDependencies" => skipped_optional_dependencies = Some(specs),
                _ => {
                    core::hint::cold_path();
                    return None;
                }
            }
        }
        Some(RawImporter {
            dependencies,
            dev_dependencies,
            optional_dependencies,
            skipped_optional_dependencies,
        })
    }

    /// Parse a block of `<pkg>:` entries each with `specifier:` /
    /// `version:` children at `entry_indent`.
    fn parse_dep_specs(&mut self, entry_indent: usize) -> Option<BTreeMap<String, RawDepSpec>> {
        let mut map = BTreeMap::new();
        while let Some(line) = self.peek_line() {
            if line.indent < entry_indent {
                break;
            }
            if line.indent != entry_indent {
                core::hint::cold_path();
                return None;
            }
            let (name, rest) = split_key(line.body)?;
            if rest.is_some() {
                return None;
            }
            let name = scalar_key_string(name)?;
            self.consume_line(&line);
            // children: specifier / version at entry_indent + 2
            let mut specifier = None;
            let mut version = None;
            let child_indent = entry_indent + 2;
            while let Some(c) = self.peek_line() {
                if c.indent < child_indent {
                    break;
                }
                if c.indent != child_indent {
                    core::hint::cold_path();
                    return None;
                }
                let (ck, cv) = split_key(c.body)?;
                let cv = cv?;
                self.consume_line(&c);
                match ck {
                    b"specifier" => specifier = Some(scalar_string(cv)?),
                    b"version" => version = Some(scalar_string(cv)?),
                    _ => {
                        core::hint::cold_path();
                        return None;
                    }
                }
            }
            map.insert(
                name,
                RawDepSpec {
                    specifier: specifier?,
                    version: version?,
                },
            );
        }
        // A bare `dependencies:` (no children) yields an empty map here,
        // but serde reads the `Option<BTreeMap<…>>` field as `None`, not
        // `Some({})`. Decline so serde decides — mirrors the same guard in
        // `parse_string_map`.
        if map.is_empty() {
            return None;
        }
        Some(map)
    }

    fn parse_packages(&mut self) -> Option<BTreeMap<String, RawPackageInfo>> {
        let mut map = BTreeMap::new();
        while let Some(line) = self.peek_line() {
            if line.indent < 2 {
                break;
            }
            if line.indent != 2 {
                core::hint::cold_path();
                return None;
            }
            let (key, rest) = split_key(line.body)?;
            // `<depPath>: {}` (empty inline) or block body.
            let name = scalar_key_string(key)?;
            self.consume_line(&line);
            let info = if let Some(inline) = rest {
                // Inline body for a package entry is unexpected (pnpm
                // writes block bodies); bail unless it's an empty map.
                if inline == b"{}" {
                    default_package_info()
                } else {
                    return None;
                }
            } else {
                self.parse_package_body(4)?
            };
            map.insert(name, info);
        }
        Some(map)
    }

    fn parse_package_body(&mut self, indent: usize) -> Option<RawPackageInfo> {
        // Collect the whole package sub-block and hand to serde. Package
        // bodies carry the fiddly `resolution`/`variants`/`string_or_seq`
        // shapes; re-implementing them risks divergence and they are a
        // minority of total lines compared to snapshots+importers.
        let block = self.take_block(indent)?;
        if block.is_empty() {
            return Some(default_package_info());
        }
        let dedented = dedent(&block, indent)?;
        yaml_serde::from_str::<RawPackageInfo>(&dedented).ok()
    }

    fn parse_snapshots(&mut self) -> Option<BTreeMap<String, RawSnapshot>> {
        let mut map = BTreeMap::new();
        while let Some(line) = self.peek_line() {
            if line.indent < 2 {
                break;
            }
            if line.indent != 2 {
                core::hint::cold_path();
                return None;
            }
            let (key, rest) = split_key(line.body)?;
            let name = scalar_key_string(key)?;
            self.consume_line(&line);
            let snap = if let Some(inline) = rest {
                if inline == b"{}" {
                    default_snapshot()
                } else {
                    return None;
                }
            } else {
                self.parse_snapshot_body(4)?
            };
            map.insert(name, snap);
        }
        Some(map)
    }

    fn parse_snapshot_body(&mut self, indent: usize) -> Option<RawSnapshot> {
        let mut dependencies = None;
        let mut optional_dependencies = None;
        let mut bundled_dependencies = None;
        let mut optional = None;
        let mut transitive_peer_dependencies = None;
        while let Some(line) = self.peek_line() {
            if line.indent < indent {
                break;
            }
            if line.indent != indent {
                core::hint::cold_path();
                return None;
            }
            let (key, rest) = split_key(line.body)?;
            self.consume_line(&line);
            match key {
                b"dependencies" => {
                    dependencies = Some(self.parse_string_map(rest, indent + 2)?);
                }
                b"optionalDependencies" => {
                    optional_dependencies = Some(self.parse_string_map(rest, indent + 2)?);
                }
                b"optional" => {
                    optional = Some(parse_bool(rest?)?);
                }
                b"bundledDependencies" => {
                    bundled_dependencies = Some(self.parse_seq_value(rest, indent + 2)?);
                }
                b"transitivePeerDependencies" => {
                    transitive_peer_dependencies = Some(self.parse_seq_value(rest, indent + 2)?);
                }
                _ => {
                    core::hint::cold_path();
                    return None;
                }
            }
        }
        Some(RawSnapshot {
            dependencies,
            optional_dependencies,
            bundled_dependencies,
            optional,
            transitive_peer_dependencies,
        })
    }
}

fn default_package_info() -> RawPackageInfo {
    // An empty package entry (`<depPath>: {}`). `RawPackageInfo`'s
    // `#[derive(Default)]` produces the exact same value serde reads from
    // `{}` — every field is `Option`/`Vec`/`bool`/`BTreeMap` defaulting to
    // none/empty/false — so we build it infallibly here instead of routing
    // a constant through serde with an `.expect` on each call. A
    // differential test pins this equivalence to serde's `{}` parse.
    RawPackageInfo::default()
}

fn default_importer() -> RawImporter {
    RawImporter {
        dependencies: None,
        dev_dependencies: None,
        optional_dependencies: None,
        skipped_optional_dependencies: None,
    }
}

fn default_snapshot() -> RawSnapshot {
    RawSnapshot {
        dependencies: None,
        optional_dependencies: None,
        bundled_dependencies: None,
        optional: None,
        transitive_peer_dependencies: None,
    }
}

/// Split `key: value` on the FIRST structural `:` (a `:` followed by a
/// space or end-of-line). Returns the raw key bytes and the inline
/// value (`None` if the line is just `key:`). Bails (None) on a line
/// with no structural colon.
fn split_key(body: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
    // pnpm package/snapshot keys contain `@` and version `:`-free
    // dep-paths; the structural separator is the LAST `: ` / trailing
    // `:` is not reliable for keys like `foo@1.0.0(bar@2.0.0)`. pnpm
    // never puts a bare `: ` inside a top-level/entry key except inside
    // quotes. We scan for the first `:` that is followed by a space or
    // is at end-of-line, skipping over quoted regions.
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < body.len() {
        let b = body[i];
        match b {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b':' if !in_single && !in_double => {
                let next = body.get(i + 1);
                if next.is_none() {
                    return Some((&body[..i], None));
                }
                if next == Some(&b' ') {
                    let val = &body[i + 2..];
                    // strip trailing spaces (pnpm shouldn't emit any)
                    let val = trim_trailing_ws(val);
                    return Some((&body[..i], Some(val)));
                }
            }
            _ => {}
        }
        i += 1;
    }
    core::hint::cold_path();
    None
}

fn trim_trailing_ws(b: &[u8]) -> &[u8] {
    let mut end = b.len();
    while end > 0 && (b[end - 1] == b' ' || b[end - 1] == b'\t') {
        end -= 1;
    }
    &b[..end]
}

/// A YAML key that may be quoted. Unquote single/double quotes;
/// otherwise take verbatim. Bail on anything needing escape processing
/// inside double quotes (rare in pnpm keys — those are plain).
fn scalar_key_string(raw: &[u8]) -> Option<String> {
    scalar_string(raw)
}

/// Decode a scalar value: bare, single-quoted (`'...'` with `''`
/// escape), or double-quoted (only when it contains no backslash
/// escapes — otherwise bail to serde). Plain scalars are taken
/// verbatim.
fn scalar_string(raw: &[u8]) -> Option<String> {
    if raw.is_empty() {
        return Some(String::new());
    }
    if raw[0] == b'\'' {
        if raw.len() < 2 || raw[raw.len() - 1] != b'\'' {
            return None;
        }
        let inner = &raw[1..raw.len() - 1];
        // YAML single-quote escape is `''` → `'`.
        let s = std::str::from_utf8(inner).ok()?;
        return Some(s.replace("''", "'"));
    }
    if raw[0] == b'"' {
        if raw.len() < 2 || raw[raw.len() - 1] != b'"' {
            return None;
        }
        let inner = &raw[1..raw.len() - 1];
        // Bail if any backslash escape is present — let serde handle it.
        if inner.contains(&b'\\') {
            return None;
        }
        return std::str::from_utf8(inner).ok().map(|s| s.to_string());
    }
    // Bare scalar. Must not be a flow construct.
    if raw[0] == b'{' || raw[0] == b'[' || raw[0] == b'&' || raw[0] == b'*' {
        return None;
    }
    // Strip a YAML inline comment. On a PLAIN scalar a `#` starts a comment
    // when it is at the very start of the value (the structural `: ` already
    // separates it) or preceded by whitespace (` #` / `\t#`); a `#` with no
    // leading space inside the value is part of it (`bar#baz`). Quoted scalars
    // never reach here (handled above), so a literal `#` inside quotes is
    // preserved. This matches yaml_serde, so hand-edited lockfiles with
    // trailing comments don't silently misparse instead of deferring to serde.
    let end = strip_inline_comment(raw);
    // A value that is ENTIRELY a comment is YAML null. The subset only models
    // string scalars, so decline and let serde apply its null coercion.
    if end == 0 {
        return None;
    }
    std::str::from_utf8(&raw[..end]).ok().map(|s| s.to_string())
}

/// Return the byte offset where a plain-scalar value ends, accounting for a
/// trailing YAML inline comment. A comment begins at the first `#` that is
/// either at offset 0 (the structural `: ` already precedes the value) or
/// preceded by an ASCII space or tab; trailing whitespace before that `#` is
/// dropped. With no such `#`, returns `raw.len()`. Returns 0 when the value
/// is entirely a comment.
fn strip_inline_comment(raw: &[u8]) -> usize {
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'#' && (i == 0 || raw[i - 1] == b' ' || raw[i - 1] == b'\t') {
            // Back up over the whitespace separating value from comment.
            let mut end = i;
            while end > 0 && (raw[end - 1] == b' ' || raw[end - 1] == b'\t') {
                end -= 1;
            }
            return end;
        }
        i += 1;
    }
    raw.len()
}

/// Parse `lockfileVersion`-style scalar into a `yaml_serde::Value`,
/// preserving the original string/number distinction by going through
/// serde for that one tiny value.
fn parse_scalar_value(raw: &[u8]) -> Option<yaml_serde::Value> {
    let s = std::str::from_utf8(raw).ok()?;
    yaml_serde::from_str::<yaml_serde::Value>(s).ok()
}

fn parse_bool(raw: &[u8]) -> Option<bool> {
    match raw {
        b"true" => Some(true),
        b"false" => Some(false),
        _ => None,
    }
}

/// Deserialize a tiny inline flow fragment (`[a, b]`, etc.) via serde.
fn serde_value_from_fragment(raw: &[u8]) -> Option<Vec<String>> {
    let s = std::str::from_utf8(raw).ok()?;
    yaml_serde::from_str::<Vec<String>>(s).ok()
}

/// Strip exactly `n` leading spaces from every non-blank line so a
/// nested block parses as a top-level serde mapping. Returns `None` if
/// a non-blank line has fewer than `n` leading spaces.
fn dedent(block: &str, n: usize) -> Option<String> {
    let mut out = String::with_capacity(block.len());
    for line in block.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            out.push('\n');
            continue;
        }
        let spaces = trimmed.bytes().take_while(|&b| b == b' ').count();
        if spaces < n {
            return None;
        }
        out.push_str(&line[n..]);
    }
    Some(out)
}

#[cfg(test)]
mod subset_tests {
    use super::super::raw::{SubsetDiff, diff_subset_vs_serde};
    use super::*;

    // -- Committed fixtures, run through the differential harness. --

    const NATIVE: &str = include_str!("../../tests/fixtures/pnpm-native.yaml");
    const KITCHEN_SINK: &str = include_str!("../../tests/fixtures/pnpm-kitchen-sink.yaml");
    const V6: &str = include_str!("../../tests/fixtures/pnpm-v6.yaml");
    const GIT_RESOLUTION: &str = include_str!("../../tests/fixtures/pnpm-git-resolution.yaml");

    // Real cross-version lockfiles harvested from pinned pnpm releases in
    // containers (pnpm 6 → lv5.3, pnpm 7 → lv5.4, pnpm 8 → lv6.0, pnpm
    // 9/10 → lv9.0). They pin the version-by-version behavior so CI
    // covers it without Docker: the pre-v9 formats must DECLINE cleanly
    // (never misparse), and the v9 shapes — including git/tarball/npm-
    // alias keys and a workspace+catalog — must FIRE and match serde.
    const V5_3_PLAIN: &str = include_str!("../../tests/fixtures/pnpm-v5_3-plain.yaml");
    const V5_4_PLAIN: &str = include_str!("../../tests/fixtures/pnpm-v5_4-plain.yaml");
    const V6_TOPLEVEL: &str = include_str!("../../tests/fixtures/pnpm-v6-toplevel-deps.yaml");
    const V6_WORKSPACE: &str = include_str!("../../tests/fixtures/pnpm-v6-workspace.yaml");
    const V9_EXOTIC: &str = include_str!("../../tests/fixtures/pnpm-v9-exotic.yaml");
    const V9_WS_CATALOG: &str = include_str!("../../tests/fixtures/pnpm-v9-workspace-catalog.yaml");

    /// The load-bearing invariant: for any input, the subset parser
    /// either produces a structurally-identical `RawPnpmLockfile` to the
    /// serde path, or declines (returns `None`). A silent wrong parse —
    /// the subset path accepting and producing a DIFFERENT result — is
    /// the only real bug, and `diff_subset_vs_serde` is what catches it.
    /// Equality is compared via the `{:#?}` rendering (the raw types are
    /// `Deserialize`-only).
    fn assert_match_or_declines(name: &str, content: &str) {
        match diff_subset_vs_serde(content) {
            SubsetDiff::Match | SubsetDiff::Declined => {}
            SubsetDiff::Divergence { subset, serde } => {
                panic!(
                    "SILENT WRONG PARSE in `{name}`: subset fast path diverged from serde.\n\
                     --- subset ---\n{subset}\n--- serde ---\n{serde}"
                );
            }
        }
    }

    /// Like `assert_match_or_declines`, but additionally requires the
    /// fast path to FIRE (not decline) — used where we want to prove the
    /// parser models a shape, not merely fall back on it.
    fn assert_fires_and_matches(name: &str, content: &str) {
        match diff_subset_vs_serde(content) {
            SubsetDiff::Match => {}
            SubsetDiff::Declined => {
                panic!("`{name}`: expected the subset fast path to fire, but it declined")
            }
            SubsetDiff::Divergence { subset, serde } => {
                panic!(
                    "SILENT WRONG PARSE in `{name}`: subset diverged from serde.\n\
                     --- subset ---\n{subset}\n--- serde ---\n{serde}"
                );
            }
        }
    }

    #[test]
    fn differential_committed_fixtures() {
        // Every fixture must agree with serde or cleanly decline. These
        // four cover, between them: the simple single-importer case;
        // catalog:/catalogs:, overrides, patchedDependencies, npm-alias
        // deps, scoped packages, peerDependencies(+Meta), os/cpu/libc,
        // deprecated, bundledDependencies, remote-tarball resolutions,
        // empty importer `{}`, optional snapshots, transitivePeer; the
        // v6 (`/name@version` keys, no snapshots:) layout; and git/ssh
        // resolutions with peer-paren and subpath keys.
        assert_fires_and_matches("pnpm-native", NATIVE);
        assert_fires_and_matches("pnpm-kitchen-sink", KITCHEN_SINK);
        assert_match_or_declines("pnpm-v6", V6);
        assert_fires_and_matches("pnpm-git-resolution", GIT_RESOLUTION);
    }

    #[test]
    fn cross_version_corpus_matches_or_declines() {
        // The load-bearing cross-version invariant. None of these may
        // diverge from serde.
        assert_match_or_declines("pnpm-v5_3-plain", V5_3_PLAIN);
        assert_match_or_declines("pnpm-v5_4-plain", V5_4_PLAIN);
        assert_match_or_declines("pnpm-v6-toplevel-deps", V6_TOPLEVEL);
        assert_match_or_declines("pnpm-v6-workspace", V6_WORKSPACE);
        assert_match_or_declines("pnpm-v9-exotic", V9_EXOTIC);
        assert_match_or_declines("pnpm-v9-workspace-catalog", V9_WS_CATALOG);
    }

    #[test]
    fn pre_v9_top_level_dep_formats_decline_never_misparse() {
        // pnpm <= 8 single-package lockfiles carry `dependencies:` /
        // `devDependencies:` at the top level (no `importers:`), and v5
        // omits `snapshots:` entirely. The parser does not model those
        // top-level keys, so it MUST decline — never silently drop them.
        assert!(try_parse(V5_3_PLAIN).is_none(), "lv5.3 must decline");
        assert!(try_parse(V5_4_PLAIN).is_none(), "lv5.4 must decline");
        assert!(
            try_parse(V6_TOPLEVEL).is_none(),
            "lv6 top-level deps must decline"
        );
    }

    #[test]
    fn v9_git_tarball_alias_keys_fire_and_match() {
        // The highest-value v9 case: package/snapshot keys that embed a
        // `#` fragment and an `@https://…` tarball URL (git/github +
        // remote-tarball + npm-alias deps). These exercise `split_key`
        // and `scalar_key_string` on keys full of `:` `/` `#` `@`, and
        // must produce a byte-identical result to serde.
        assert_fires_and_matches("pnpm-v9-exotic", V9_EXOTIC);
        let raw = try_parse(V9_EXOTIC).unwrap();
        assert!(
            raw.packages
                .keys()
                .any(|k| k.contains("codeload.github.com"))
        );
    }

    #[test]
    fn fast_path_fires_on_native_fixture() {
        let raw = try_parse(NATIVE).expect("subset parser should accept native pnpm fixture");
        assert_eq!(raw.packages.len(), 8);
        assert_eq!(raw.snapshots.len(), 8);
        assert_eq!(raw.importers.len(), 1);
    }

    #[test]
    fn kitchen_sink_fires_and_models_every_section() {
        let raw = try_parse(KITCHEN_SINK).expect("kitchen-sink should be accepted");
        // Three importers, including one empty (`packages/empty: {}`).
        assert_eq!(raw.importers.len(), 3);
        assert!(raw.catalogs.is_some());
        assert!(raw.overrides.is_some());
        assert!(raw.patched_dependencies.is_some());
        assert_eq!(raw.packages.len(), 9);
        assert_eq!(raw.snapshots.len(), 9);
    }

    // -- Empty-importer `{}` fast-path coverage. --

    #[test]
    fn empty_importer_inline_map_fires_and_matches() {
        // pnpm writes a workspace package with no dependencies as an
        // inline `{}`. The fast path must handle this directly rather than
        // declining and falling the whole (often large, multi-package)
        // workspace lockfile back to serde. The result must be
        // byte-identical to serde's.
        let lock = "lockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies:\n      a:\n        specifier: ^1\n        version: 1.0.0\n  packages/empty: {}\n  packages/also-empty: {}\n";
        assert_fires_and_matches("empty-importer", lock);
        let raw = try_parse(lock).unwrap();
        assert_eq!(raw.importers.len(), 3);
        let empty = &raw.importers["packages/empty"];
        assert!(empty.dependencies.is_none());
        assert!(empty.dev_dependencies.is_none());
    }

    // -- Inline-flow-map / empty-block handling. A map-typed field whose
    //    value is an inline `{...}` flow map, or whose block is empty
    //    (serde reads it as a missing/null `None`, not an empty map), must
    //    DECLINE — never silently parse as `Some({})`. --

    #[test]
    fn inline_flow_map_fields_decline_not_misparse() {
        // overrides / time as inline `{...}`: the block scanner would
        // ignore the inline entries and produce `Some({})`; serde parses
        // the entries. Must decline.
        for content in [
            "lockfileVersion: '9.0'\noverrides: {foo: bar}\n",
            "lockfileVersion: '9.0'\ntime: {foo@1.0.0: '2020-01-01'}\n",
            "lockfileVersion: '9.0'\nsettings: {autoInstallPeers: true}\n",
            "lockfileVersion: '9.0'\npatchedDependencies: {foo@1.0.0: {path: a, hash: b}}\n",
            "lockfileVersion: '9.0'\ncatalogs: {default: {react: {specifier: ^18, version: 18.0.0}}}\n",
            "lockfileVersion: '9.0'\nimporters: {.: {dependencies: {a: {specifier: ^1, version: 1.0.0}}}}\n",
        ] {
            assert!(
                try_parse(content).is_none(),
                "inline flow map must decline, not misparse: {content:?}"
            );
            assert_match_or_declines("inline-flow-map", content);
        }
    }

    #[test]
    fn empty_block_map_declines_to_distinguish_null_from_empty() {
        // `overrides:` with a null/empty body deserializes to `None` under
        // serde, not `Some({})`. The subset path must not assert an empty
        // map; it declines so serde decides.
        let null_overrides = "lockfileVersion: '9.0'\noverrides:\n";
        assert!(try_parse(null_overrides).is_none());
        assert_match_or_declines("null-overrides", null_overrides);
        // Same for an empty snapshot `dependencies:` block.
        let snap_empty_deps = "lockfileVersion: '9.0'\nsnapshots:\n  foo@1.0.0:\n    dependencies:\n    optional: true\n";
        assert!(try_parse(snap_empty_deps).is_none());
        assert_match_or_declines("snap-empty-deps", snap_empty_deps);
    }

    #[test]
    fn empty_importer_dependencies_block_declines_not_misparse() {
        // An importer with a bare `dependencies:` (no children) and a
        // populated `devDependencies:`: serde reads the empty
        // `Option<BTreeMap<…>>` field as `None`, but the subset path would
        // build an empty map and store `Some({})` — a silent divergence.
        // The empty-map guard in `parse_dep_specs` makes it decline so
        // serde decides, exactly as `parse_string_map` does for overrides.
        let lock = "lockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies:\n    devDependencies:\n      a:\n        specifier: ^1\n        version: 1.0.0\n";
        assert!(try_parse(lock).is_none());
        assert_match_or_declines("empty-importer-dependencies", lock);
    }

    #[test]
    fn default_package_info_matches_serde_empty_map() {
        // The infallible `RawPackageInfo::default()` must be byte-identical
        // to what serde reads from `{}` (the value pnpm's empty `pkg: {}`
        // entry produces), so the `<depPath>: {}` fast path stays exact.
        let by_default = format!("{:#?}", default_package_info());
        let by_serde = format!(
            "{:#?}",
            yaml_serde::from_str::<RawPackageInfo>("{}").unwrap()
        );
        assert_eq!(by_default, by_serde);
    }

    #[test]
    fn block_string_map_still_fires() {
        // The normal block form must still take the fast path and match.
        let block = "lockfileVersion: '9.0'\noverrides:\n  foo: bar\n  baz: '^1.0.0'\n";
        assert_fires_and_matches("block-overrides", block);
    }

    // -- Fast-path hit-rate guard. The decline logic (`inline.is_some() ->
    //    decline` on the eight map-typed fields, plus an empty-map decline
    //    inside `parse_string_map`) must not over-trigger: a block form
    //    NEVER carries an inline value, so it must still FIRE. These pin
    //    each field's legitimate, populated BLOCK form to the fast path —
    //    evidence the inline/empty declines do not eat the hit rate for the
    //    common real-world case. (settings / catalogs /
    //    patchedDependencies / snapshot.optionalDependencies block forms
    //    are also exercised by the kitchen-sink fixture's
    //    `assert_fires_and_matches`; these isolate each one.) --

    #[test]
    fn populated_block_forms_still_fire_per_field() {
        let cases: &[(&str, &str)] = &[
            (
                "overrides-block",
                "lockfileVersion: '9.0'\noverrides:\n  foo: '1'\n",
            ),
            (
                "time-block",
                "lockfileVersion: '9.0'\ntime:\n  foo@1.0.0: '2020-01-01T00:00:00.000Z'\n",
            ),
            (
                "settings-block",
                "lockfileVersion: '9.0'\nsettings:\n  autoInstallPeers: true\n  excludeLinksFromLockfile: false\n",
            ),
            (
                "patchedDependencies-block",
                "lockfileVersion: '9.0'\npatchedDependencies:\n  foo@1.0.0:\n    path: patches/foo.patch\n    hash: abc123\n",
            ),
            (
                "catalogs-block",
                "lockfileVersion: '9.0'\ncatalogs:\n  default:\n    react:\n      specifier: ^18\n      version: 18.0.0\n",
            ),
            (
                "importers-block",
                "lockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies:\n      a:\n        specifier: ^1\n        version: 1.0.0\n",
            ),
            (
                "snapshot-optionalDeps-block",
                "lockfileVersion: '9.0'\nsnapshots:\n  foo@1.0.0:\n    optionalDependencies:\n      bar: 2.0.0\n    optional: true\n",
            ),
        ];
        for (name, content) in cases {
            assert_fires_and_matches(name, content);
        }
    }

    #[test]
    fn map_field_null_empty_populated_track_serde() {
        // For each null/empty/populated rendering of a map-typed field, the
        // subset decision (fire+Match vs Declined) must agree with serde's
        // None-vs-empty-vs-populated reading. serde reads a null body
        // (`overrides:` with nothing) and an inline `{}` both as a value the
        // subset path declines on, while a populated block fires. The
        // harness asserts no divergence in every case; we additionally pin
        // the populated case to FIRE and the empty cases to DECLINE so the
        // intended fast-path boundary is explicit.
        let populated = "lockfileVersion: '9.0'\noverrides:\n  foo: bar\n";
        assert_fires_and_matches("overrides-populated", populated);

        for empty in [
            // null body — serde reads `None`
            "lockfileVersion: '9.0'\noverrides:\n",
            // inline empty map — serde reads `Some({})`; subset can't model
            // the inline form so it declines (no divergence either way)
            "lockfileVersion: '9.0'\noverrides: {}\n",
        ] {
            assert!(
                try_parse(empty).is_none(),
                "empty/inline overrides must decline: {empty:?}"
            );
            assert_match_or_declines("overrides-empty", empty);
        }
    }

    // -- Adversarial structural cases (self-review). Each must MATCH or
    //    DECLINE; a divergence is a silent wrong parse. --

    #[test]
    fn snapshot_dep_value_with_peer_parens_keeps_full_value() {
        // The common real-world snapshot dependency form: the version
        // value carries a peer-resolution suffix `2.0.0(baz@3.0.0)`. The
        // `(…)` contains `@` and could trip a naive split; the value must
        // be kept verbatim and agree with serde.
        let lock = "lockfileVersion: '9.0'\nsnapshots:\n  foo@1.0.0:\n    dependencies:\n      bar: 2.0.0(baz@3.0.0)\n";
        assert_fires_and_matches("dep-value-peer-parens", lock);
    }

    #[test]
    fn scoped_peer_key_fires_and_matches() {
        // A quoted dep-path key with a scoped peer paren
        // `@foo/bar@1.0.0(@types/node@20.0.0)` — `@`, `/`, parens, no
        // structural colon inside.
        let lock = "lockfileVersion: '9.0'\nsnapshots:\n  '@foo/bar@1.0.0(@types/node@20.0.0)':\n    dependencies:\n      '@types/node': 20.0.0\n";
        assert_fires_and_matches("scoped-peer-key", lock);
    }

    #[test]
    fn numeric_and_bool_looking_string_values_match_serde() {
        // String-typed fields (specifier/version, overrides, snapshot
        // deps) receiving scalars that LOOK like numbers/bools/null. The
        // subset path takes the bytes verbatim; serde coerces the same
        // token into the String target. They must agree.
        let lock = "lockfileVersion: '9.0'\noverrides:\n  a: 1.5\n  b: 10\n  c: true\n  d: null\n";
        assert_fires_and_matches("numeric-string-values", lock);
    }

    #[test]
    fn quoted_bool_for_optional_declines_not_misparses() {
        // `optional` is a typed bool. A quoted `'true'` is a STRING in
        // YAML, not the bool — `parse_bool` only accepts the bare tokens,
        // so the parser must decline rather than coerce a string to true.
        let lock = "lockfileVersion: '9.0'\nsnapshots:\n  foo@1.0.0:\n    optional: 'true'\n";
        assert!(try_parse(lock).is_none());
        assert_match_or_declines("quoted-optional", lock);
    }

    #[test]
    fn multiline_block_scalar_declines() {
        // The scanner is single-line per logical value; a `|` literal or
        // `>` folded scalar is outside the subset and must decline.
        let lock = "lockfileVersion: '9.0'\noverrides:\n  foo: |\n    multi\n    line\n";
        assert!(try_parse(lock).is_none());
        assert_match_or_declines("literal-scalar", lock);
    }

    // -- Fallback (decline) cases — never a wrong parse. --

    #[test]
    fn declines_multi_document_stream() {
        // pnpm v11 two-document layout: the scoring fallback owns this.
        let two_docs = "lockfileVersion: '9.0'\npackages: {}\n---\nlockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies: {}\n";
        assert!(try_parse(two_docs).is_none());
    }

    #[test]
    fn declines_unknown_top_level_key() {
        // An unmodeled top-level field must fall back so it is never
        // silently dropped.
        let with_unknown = "lockfileVersion: '9.0'\nfutureField: 1\n";
        assert!(try_parse(with_unknown).is_none());
    }

    #[test]
    fn declines_tab_indentation_rather_than_misparsing() {
        // Tab indentation is not pnpm's 2-space dialect; the indent
        // counter only counts spaces, so a tab-indented child reads as
        // indent 0 and the parser must decline — never silently treat a
        // tab-nested value as a sibling top-level key.
        let tabbed = "lockfileVersion: '9.0'\nimporters:\n\t.:\n\t\tdependencies: {}\n";
        assert!(try_parse(tabbed).is_none());
        // And whatever it does, it must not diverge from serde.
        assert_match_or_declines("tab-indent", tabbed);
    }

    #[test]
    fn declines_empty_input() {
        // No `lockfileVersion` → the final `lockfile_version?` is None.
        assert!(try_parse("").is_none());
        assert!(try_parse("\n\n").is_none());
        assert!(try_parse("# just a comment\n").is_none());
    }

    #[test]
    fn importer_only_no_packages_fires() {
        let lock = "lockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies:\n      a:\n        specifier: ^1\n        version: 1.0.0\n";
        assert_fires_and_matches("importer-only", lock);
        let raw = try_parse(lock).unwrap();
        assert!(raw.packages.is_empty());
        assert!(raw.snapshots.is_empty());
    }

    // -- Byte-scanner edge cases. --

    #[test]
    fn crlf_line_endings_parse_identically() {
        // The scanner strips a trailing `\r`; a CRLF file must produce
        // the same result as its LF twin (and agree with serde).
        let lf = "lockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies:\n      a:\n        specifier: ^1\n        version: 1.0.0\n";
        let crlf = lf.replace('\n', "\r\n");
        assert_fires_and_matches("crlf", &crlf);
        let a = format!("{:#?}", try_parse(lf).unwrap());
        let b = format!("{:#?}", try_parse(&crlf).unwrap());
        assert_eq!(a, b, "CRLF and LF must parse to the same structure");
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let lock = "lockfileVersion: '9.0'\n\n# a comment\nimporters:\n\n  # importer comment\n  .:\n    dependencies:\n      a:\n        specifier: ^1\n        version: 1.0.0\n";
        assert_fires_and_matches("comments", lock);
    }

    #[test]
    fn values_containing_colons_keep_the_full_value() {
        // URLs and git specs contain `:` after the structural separator;
        // `split_key` must split on the FIRST `: ` only and keep the
        // rest verbatim. Compared against serde to be sure.
        let lock = "lockfileVersion: '9.0'\noverrides:\n  pkg: 'git+ssh://git@github.com/foo/bar.git#v1'\n  url: 'https://example.com/path:8080/x'\n";
        assert_fires_and_matches("colon-values", lock);
        let raw = try_parse(lock).unwrap();
        let ov = raw.overrides.unwrap();
        assert_eq!(ov["pkg"], "git+ssh://git@github.com/foo/bar.git#v1");
        assert_eq!(ov["url"], "https://example.com/path:8080/x");
    }

    #[test]
    fn split_key_respects_quoted_colons() {
        let (k, v) = split_key(b"'a:b': value").unwrap();
        assert_eq!(k, b"'a:b'");
        assert_eq!(v, Some(&b"value"[..]));
    }

    #[test]
    fn split_key_trailing_colon_has_no_value() {
        let (k, v) = split_key(b"importers:").unwrap();
        assert_eq!(k, b"importers");
        assert_eq!(v, None);
    }

    #[test]
    fn split_key_bails_without_structural_colon() {
        // A line with a `:` not followed by space/EOL (e.g. `a:b`) has no
        // structural separator and must not be mistaken for a mapping.
        assert!(split_key(b"justtext").is_none());
        assert!(split_key(b"a:b").is_none());
    }

    #[test]
    fn scalar_string_handles_quote_styles() {
        assert_eq!(scalar_string(b"plain").unwrap(), "plain");
        assert_eq!(scalar_string(b"'quoted'").unwrap(), "quoted");
        assert_eq!(scalar_string(b"\"double\"").unwrap(), "double");
        // YAML single-quote escape `''` -> `'`.
        assert_eq!(scalar_string(b"'it''s'").unwrap(), "it's");
        // Empty value is a valid empty string.
        assert_eq!(scalar_string(b"").unwrap(), "");
    }

    #[test]
    fn scalar_string_bails_on_backslash_escapes_and_flow() {
        // Double-quoted with a backslash escape: defer to serde.
        assert!(scalar_string(b"\"a\\tb\"").is_none());
        // Unterminated quotes.
        assert!(scalar_string(b"'open").is_none());
        assert!(scalar_string(b"\"open").is_none());
        // Flow constructs / anchors / aliases as bare values.
        assert!(scalar_string(b"{inline: map}").is_none());
        assert!(scalar_string(b"[a, b]").is_none());
        assert!(scalar_string(b"&anchor").is_none());
        assert!(scalar_string(b"*alias").is_none());
    }

    #[test]
    fn plain_scalar_strips_inline_comment_quoted_keeps_hash() {
        // Plain scalar: ` #` starts a YAML inline comment — stripped, value
        // trimmed (matches yaml_serde, see the differential below).
        assert_eq!(scalar_string(b"bar # keep this").unwrap(), "bar");
        assert_eq!(scalar_string(b"bar\t# c").unwrap(), "bar");
        assert_eq!(scalar_string(b"bar baz # c").unwrap(), "bar baz");
        // `#` not preceded by whitespace is part of the value, not a comment.
        assert_eq!(scalar_string(b"bar#baz").unwrap(), "bar#baz");
        // A value that is ENTIRELY a comment is YAML null — decline to serde.
        assert!(scalar_string(b"# x").is_none());
        assert!(scalar_string(b" # x").is_none());
        // Quoted scalars preserve a literal `#` (no comment processing).
        assert_eq!(
            scalar_string(b"\"bar # keep this\"").unwrap(),
            "bar # keep this"
        );
        assert_eq!(
            scalar_string(b"'bar # keep this'").unwrap(),
            "bar # keep this"
        );

        // End-to-end: a hand-edited lockfile with an inline comment on a
        // plain scalar value must parse identically to serde, not misparse
        // (the bug: the value was returned verbatim as "1.0.0 # pin").
        let lock = "lockfileVersion: '9.0'\n\nimporters:\n\n  .:\n    dependencies:\n      a:\n        specifier: ^1 # range\n        version: 1.0.0 # pin\n";
        assert!(matches!(
            diff_subset_vs_serde(lock),
            SubsetDiff::Match | SubsetDiff::Declined
        ));
        // It must actually FIRE (not decline) and carry the stripped value.
        let parsed = try_parse(lock).expect("subset should accept");
        let importer = parsed.importers.get(".").expect("root importer");
        let deps = importer.dependencies.as_ref().expect("dependencies block");
        let dep = deps.get("a").expect("dep a");
        assert_eq!(dep.version, "1.0.0");
        assert_eq!(dep.specifier, "^1");
    }

    #[test]
    fn trailing_whitespace_on_a_value_is_trimmed_consistently() {
        // pnpm never emits trailing spaces, but if present they must not
        // change the parsed value vs serde.
        let lock = "lockfileVersion: '9.0'\noverrides:\n  pkg: ^1.0.0  \n";
        assert_match_or_declines("trailing-ws", lock);
    }

    // -- Inline-vs-block fuzz proptest. Generates a logical pnpm-lock
    //    structure over the eight serde-delegated / map-typed fields and
    //    renders it two ways — pnpm's normal 2-space BLOCK style, and an
    //    INLINE flow-map style — then asserts the core invariant for BOTH
    //    renderings: `diff_subset_vs_serde` returns Match or Declined,
    //    never Divergence. This fuzzes the inline-vs-block boundary across
    //    the whole field class, so any regression that re-introduces a
    //    silent inline misparse is caught. The seed is fixed below for
    //    deterministic CI. --

    use proptest::prelude::*;

    /// One generated map-valued field. `Null`/`Empty` exercise the
    /// missing/empty-body boundary; `Populated` carries real entries that
    /// must round-trip through whichever rendering is chosen.
    #[derive(Debug, Clone)]
    enum FieldVal {
        Null,
        Empty,
        Populated(Vec<(String, String)>),
    }

    /// Plain pnpm-style keys/values: lowercase alnum + the punctuation pnpm
    /// actually emits in dep-path / specifier positions. Kept inside the
    /// recognized subset (no quotes/colons-needing-escapes) so the BLOCK
    /// rendering is a legitimate fast-path candidate; the INLINE rendering
    /// of the same data must still never diverge.
    fn plain_token() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9]{0,7}"
    }

    fn field_val() -> impl Strategy<Value = FieldVal> {
        prop_oneof![
            1 => Just(FieldVal::Null),
            1 => Just(FieldVal::Empty),
            3 => prop::collection::vec((plain_token(), plain_token()), 1..4)
                .prop_map(FieldVal::Populated),
        ]
    }

    /// A logical lockfile: the two string→string map fields whose inline
    /// vs block boundary `parse_string_map` governs directly
    /// (`overrides`, `time`), each independently Null / Empty / Populated.
    fn logical_lock() -> impl Strategy<Value = (FieldVal, FieldVal)> {
        (field_val(), field_val())
    }

    /// Render a string→string map field in BLOCK style (or omit it for
    /// `Null`/`Empty`, since pnpm never writes an empty block map — it
    /// omits the key or writes `{}` inline; we cover the inline `{}` in the
    /// inline renderer).
    fn render_block(key: &str, val: &FieldVal, out: &mut String) {
        match val {
            FieldVal::Null | FieldVal::Empty => {}
            FieldVal::Populated(entries) => {
                out.push_str(key);
                out.push_str(":\n");
                for (k, v) in entries {
                    // Dedup-safe: serde + subset both keep last on dup keys
                    // via BTreeMap; the generator may repeat a key, which is
                    // a valid thing to fuzz.
                    out.push_str("  ");
                    out.push_str(k);
                    out.push_str(": ");
                    out.push_str(v);
                    out.push('\n');
                }
            }
        }
    }

    /// Render the same field in INLINE flow-map style. `Null` → `key:`
    /// (empty body), `Empty` → `key: {}`, `Populated` → `key: {a: b, ...}`.
    /// Every inline form here is one the subset path must DECLINE on (it
    /// models only the block form) — so the invariant under test is "no
    /// divergence", with decline being the expected outcome.
    fn render_inline(key: &str, val: &FieldVal, out: &mut String) {
        match val {
            FieldVal::Null => {
                out.push_str(key);
                out.push_str(":\n");
            }
            FieldVal::Empty => {
                out.push_str(key);
                out.push_str(": {}\n");
            }
            FieldVal::Populated(entries) => {
                out.push_str(key);
                out.push_str(": {");
                let parts: Vec<String> = entries.iter().map(|(k, v)| format!("{k}: {v}")).collect();
                out.push_str(&parts.join(", "));
                out.push_str("}\n");
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 256,
            // Fixed seed: deterministic CI, reproducible failures.
            rng_algorithm: proptest::test_runner::RngAlgorithm::ChaCha,
            ..ProptestConfig::default()
        })]

        /// The core invariant across the inline-vs-block boundary: for a
        /// generated logical structure rendered EITHER way, the subset
        /// parser matches serde or cleanly declines — never a silent wrong
        /// parse. The block rendering of populated data should fire+match;
        /// the inline rendering should decline; both must avoid divergence,
        /// which is all `assert_match_or_declines` asserts.
        #[test]
        fn inline_vs_block_never_diverges((overrides, time) in logical_lock()) {
            let mut block = String::from("lockfileVersion: '9.0'\n");
            render_block("overrides", &overrides, &mut block);
            render_block("time", &time, &mut block);
            assert_match_or_declines("fuzz-block", &block);

            let mut inline = String::from("lockfileVersion: '9.0'\n");
            render_inline("overrides", &overrides, &mut inline);
            render_inline("time", &time, &mut inline);
            assert_match_or_declines("fuzz-inline", &inline);
        }
    }

    // -- Opt-in differential sweep over an external real-world corpus. --
    //
    // Point `AUBE_PNPM_CORPUS` at a directory of real `pnpm-lock.yaml`
    // files and run with `--ignored --nocapture`. Reports match/decline
    // counts and FAILS on any divergence. Not run by default (the corpus
    // is local-only); the committed fixtures above are the CI coverage.
    #[test]
    #[ignore]
    fn differential_real_corpus() {
        let dir = match std::env::var("AUBE_PNPM_CORPUS") {
            Ok(d) if !d.is_empty() => d,
            _ => {
                eprintln!("AUBE_PNPM_CORPUS not set; skipping");
                return;
            }
        };
        let (mut total, mut matched, mut declined) = (0usize, 0usize, 0usize);
        let mut diverged = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("corpus dir readable") {
            let p = entry.unwrap().path();
            if !p.is_file() {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&p) else {
                continue;
            };
            total += 1;
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            match diff_subset_vs_serde(&content) {
                SubsetDiff::Match => {
                    matched += 1;
                    eprintln!("MATCH    {name}");
                }
                SubsetDiff::Declined => {
                    declined += 1;
                    eprintln!("DECLINE  {name}");
                }
                SubsetDiff::Divergence { subset, serde } => {
                    eprintln!("DIVERGE  {name}");
                    diverged.push((p.clone(), subset, serde))
                }
            }
        }
        eprintln!(
            "corpus={total} match={matched} declined={declined} diverge={}",
            diverged.len()
        );
        for (p, s, d) in &diverged {
            eprintln!("=== DIVERGENCE {} ===", p.display());
            for (i, (a, b)) in s.lines().zip(d.lines()).enumerate() {
                if a != b {
                    eprintln!("  first diff at line {i}:\n    subset: {a}\n    serde : {b}");
                    break;
                }
            }
        }
        assert!(
            diverged.is_empty(),
            "{} divergence(s) found",
            diverged.len()
        );
    }
}

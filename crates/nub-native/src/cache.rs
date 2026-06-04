//! The transpile cache, collapsed into one native call: `cacheGet` + (on miss)
//! `transform` + post-processing + `cacheSet`. Byte-identical on-disk format to
//! the old JS cache, so warm caches survive the JS→Rust move (no global miss).
//!
//! Cache key preimage (no trailing separator), matching the old JS exactly:
//!   `NUB_VERSION \0 CACHE_SCHEMA \0 source \0 ext \0 tsconfig_hash \0 (pkg_type||"")`
//!   → sha256 → 64-hex lowercase → cache FILENAME.
//! On-disk entry: `[16-hex integrity = sha256(body)[..16]][body]`, where
//!   `body = format_byte('c'|'m') + post_processed_code`.
//! Atomic write via a `*.tmp` sibling + rename (the `*.tmp` suffix is what
//! `runtime/cache-evict.mjs` recognizes as an in-flight temp).
//!
//! The post-processing that the old JS did in `loadTranspile` after `transform`
//! moves in here so the cached bytes are the FINAL bytes: drop oxc's empty
//! `export {};` marker for CJS, append the inline base64 sourceMap, append the
//! `//# sourceURL=<absolute path>` magic comment.

use std::sync::atomic::{AtomicU64, Ordering};

use napi::Result;
use napi_derive::napi;
use oxc_napi::OxcError;
use sha2::{Digest, Sha256};

use crate::transform::{TransformOptions, transform};

/// On-disk entry format version. Bump ONLY if the byte layout diverges from the
/// old JS (it does not), so the two regimes never read each other's files.
const CACHE_SCHEMA: &str = "3";
const INTEGRITY_LEN: usize = 16;
/// Lockstep with `runtime/version.mjs` via `make version`; the sole version
/// component of the key (a new nub release ships any emit change + a rebuilt addon).
const NUB_VERSION: &str = env!("CARGO_PKG_VERSION");

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[napi(object)]
pub struct CachedTransformResult {
    /// "commonjs" | "module".
    pub format: String,
    /// The final, post-processed transpiled code (what the loader hands to Node).
    pub code: String,
    /// Non-empty ⇒ the JS side throws, same as today.
    pub errors: Vec<OxcError>,
}

/// `cacheGet` + transform-on-miss + post-process + `cacheSet`, in one call.
///
/// `format_byte` ('c'|'m') is computed in JS (`moduleFormatFor`) and folded into
/// the cached body; `cache_dir` is the JS enable/disable signal (`None` ⇒ skip all
/// I/O, just transform). The remaining key components are JS-supplied so the key
/// stays byte-identical to the old pipeline.
#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
#[napi]
pub fn transform_cached(
    filename: String,
    source: String,
    options: Option<TransformOptions>,
    ext: String,
    tsconfig_hash: String,
    pkg_type: String,
    format_byte: String,
    cache_dir: Option<String>,
) -> Result<CachedTransformResult> {
    let fmt = format_byte.as_str();
    let format = if fmt == "c" { "commonjs" } else { "module" };

    let key = cache_key(&source, &ext, &tsconfig_hash, &pkg_type);

    // Cache hit path.
    if let Some(dir) = cache_dir.as_deref() {
        if let Some(body) = cache_get(dir, &key) {
            // body[0] is the stored format byte; the rest is the final code.
            let stored_fmt = body.as_bytes().first().copied();
            let stored_format = if stored_fmt == Some(b'c') {
                "commonjs"
            } else {
                "module"
            };
            return Ok(CachedTransformResult {
                format: stored_format.to_string(),
                code: body[1..].to_string(),
                errors: Vec::new(),
            });
        }
    }

    // Miss: transform.
    let result = transform(filename.clone(), source.clone(), options);
    if !result.errors.is_empty() {
        return Ok(CachedTransformResult {
            format: format.to_string(),
            code: String::new(),
            errors: result.errors,
        });
    }

    // Post-processing (moved from JS `loadTranspile`).
    let mut code = result.code;
    if format == "commonjs" {
        code = strip_empty_export_marker(&code);
    }
    if let Some(map) = result.map {
        // Re-embed with sourcesContent = [source], matching the JS path. The JS
        // side received `result.map` as the napi SourceMap object, set
        // `sourcesContent = [source]`, then `JSON.stringify`-ed it — so the
        // serialized shape must match napi's object encoding (camelCase keys, the
        // explicit `x_google_ignoreList` name, `undefined`/`None` fields omitted,
        // and `version` numeric). Build that exact JSON here.
        let serialized = serialize_source_map(&map, &source);
        let b64 = base64_encode(serialized.as_bytes());
        code.push_str(&format!(
            "\n//# sourceMappingURL=data:application/json;base64,{b64}\n"
        ));
    }
    // sourceURL magic comment — absolute file path, as Node's strip-types does.
    code.push_str(&format!("\n//# sourceURL={filename}\n"));

    // Store: body = format_byte + code.
    if let Some(dir) = cache_dir.as_deref() {
        let body = format!("{format_byte}{code}");
        cache_set(dir, &key, &body);
    }

    Ok(CachedTransformResult {
        format: format.to_string(),
        code,
        errors: Vec::new(),
    })
}

/// sha256(NUB_VERSION \0 SCHEMA \0 source \0 ext \0 tsconfig_hash \0 pkg_type)
/// → 64-hex lowercase.
fn cache_key(source: &str, ext: &str, tsconfig_hash: &str, pkg_type: &str) -> String {
    let mut h = Sha256::new();
    h.update(NUB_VERSION.as_bytes());
    h.update(b"\0");
    h.update(CACHE_SCHEMA.as_bytes());
    h.update(b"\0");
    h.update(source.as_bytes());
    h.update(b"\0");
    h.update(ext.as_bytes());
    h.update(b"\0");
    h.update(tsconfig_hash.as_bytes());
    h.update(b"\0");
    h.update(pkg_type.as_bytes());
    to_hex(&h.finalize())
}

fn integrity(body: &str) -> String {
    let mut h = Sha256::new();
    h.update(body.as_bytes());
    let full = to_hex(&h.finalize());
    full[..INTEGRITY_LEN].to_string()
}

fn cache_get(dir: &str, key: &str) -> Option<String> {
    let path = std::path::Path::new(dir).join(key);
    let raw = std::fs::read_to_string(&path).ok()?;
    if raw.len() < INTEGRITY_LEN {
        return None;
    }
    let body = &raw[INTEGRITY_LEN..];
    if raw[..INTEGRITY_LEN] != integrity(body) {
        // Self-heal: any mismatch (truncation, corruption, edits) ⇒ miss.
        return None;
    }
    Some(body.to_string())
}

fn cache_set(dir: &str, key: &str, body: &str) {
    let final_path = std::path::Path::new(dir).join(key);
    let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let tmp_path = std::path::Path::new(dir).join(format!("{key}.{pid}.{counter}.tmp"));
    let contents = format!("{}{}", integrity(body), body);
    if std::fs::write(&tmp_path, contents).is_ok() {
        if std::fs::rename(&tmp_path, &final_path).is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
    } else {
        let _ = std::fs::remove_file(&tmp_path);
    }
}

/// Drop a trailing bare `export {};` (oxc injects it to preserve module-ness after
/// stripping a file's only module syntax). Mirror of the JS `EMPTY_EXPORT_MARKER`
/// regex: `(?:^|\n)[ \t]*export[ \t]*\{[ \t]*\}[ \t]*;?\s*$`.
fn strip_empty_export_marker(code: &str) -> String {
    // Find the last `export` that begins a trailing empty-export marker.
    // The regex anchors at end-of-string (after optional trailing whitespace) and
    // allows the marker to start at string-start or after a newline.
    let trimmed_end = code.trim_end();
    // Walk backwards to locate a candidate `export {};` tail.
    if let Some(stripped) = match_empty_export_tail(trimmed_end) {
        return stripped;
    }
    code.to_string()
}

/// Returns the code with a trailing `export {};` marker removed, or `None` if the
/// tail does not match. Faithful to the JS regex semantics.
fn match_empty_export_tail(s: &str) -> Option<String> {
    // After trimming trailing whitespace, the tail must be `export <ws> { <ws> }
    // <ws> ;?` preceded by start-of-string or a newline.
    let bytes = s.as_bytes();
    let mut i = bytes.len();

    // optional trailing `;`
    let mut end = i;
    // skip trailing spaces/tabs (already trimmed, but be defensive)
    while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
        end -= 1;
    }
    if end > 0 && bytes[end - 1] == b';' {
        end -= 1;
    }
    while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
        end -= 1;
    }
    // expect `}`
    if end == 0 || bytes[end - 1] != b'}' {
        return None;
    }
    end -= 1;
    while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
        end -= 1;
    }
    // expect `{`
    if end == 0 || bytes[end - 1] != b'{' {
        return None;
    }
    end -= 1;
    while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
        end -= 1;
    }
    // expect `export`
    const KW: &[u8] = b"export";
    if end < KW.len() || &bytes[end - KW.len()..end] != KW {
        return None;
    }
    let marker_start = end - KW.len();
    // preceding char must be start-of-string or a newline.
    if marker_start > 0 && bytes[marker_start - 1] != b'\n' {
        return None;
    }
    i = marker_start;
    // Trim a single preceding newline + the marker, leaving the rest intact.
    let mut head_end = i;
    if head_end > 0 && bytes[head_end - 1] == b'\n' {
        head_end -= 1;
    }
    Some(s[..head_end].to_string())
}

/// Serialize the source map to the exact JSON `JSON.stringify` produced over the
/// napi `SourceMap` object the old JS path saw. Key order matches napi's struct
/// field declaration order (the order napi `Object::set`s them), with `None`
/// fields omitted (JS `JSON.stringify` skips `undefined`) and `sourcesContent`
/// overridden to `[source]`. Built by hand because serde_json (no `preserve_order`
/// feature) would reorder keys alphabetically, and the napi `SourceMap` type
/// derives no `Serialize`.
fn serialize_source_map(map: &oxc_sourcemap::napi::SourceMap, source: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let js_str = |s: &str| serde_json::Value::String(s.to_string()).to_string();
    let js_str_arr = |arr: &[String]| {
        let items: Vec<String> = arr.iter().map(|s| js_str(s)).collect();
        format!("[{}]", items.join(","))
    };

    if let Some(file) = &map.file {
        parts.push(format!("\"file\":{}", js_str(file)));
    }
    parts.push(format!("\"mappings\":{}", js_str(&map.mappings)));
    parts.push(format!("\"names\":{}", js_str_arr(&map.names)));
    if let Some(root) = &map.source_root {
        parts.push(format!("\"sourceRoot\":{}", js_str(root)));
    }
    parts.push(format!("\"sources\":{}", js_str_arr(&map.sources)));
    let content_entry = format!("\"sourcesContent\":[{}]", js_str(source));
    // The JS path did `map.sourcesContent = [source]`. If napi already exposed
    // `sourcesContent` (Some), that assignment overwrites the key IN PLACE
    // (between `sources` and `version`); if it was absent (None), the assignment
    // ADDS a new key at the END. Replicate both orderings for byte-parity.
    let content_in_place = map.sources_content.is_some();
    if content_in_place {
        parts.push(content_entry.clone());
    }
    parts.push(format!("\"version\":{}", map.version));
    if let Some(list) = &map.x_google_ignorelist {
        let items: Vec<String> = list.iter().map(u32::to_string).collect();
        parts.push(format!("\"x_google_ignoreList\":[{}]", items.join(",")));
    }
    if !content_in_place {
        parts.push(content_entry);
    }
    format!("{{{}}}", parts.join(","))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

/// Standard base64 (RFC 4648, with padding) — matches JS `Buffer.from(x).toString("base64")`.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

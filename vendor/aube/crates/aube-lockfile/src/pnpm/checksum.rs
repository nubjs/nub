//! pnpm-compatible lockfile checksums.
//!
//! `pnpm-lock.yaml` records two integrity fields that let pnpm notice
//! when resolution-affecting config changed without re-reading every
//! input: `packageExtensionsChecksum` (a hash of the effective
//! `packageExtensions` object) and `pnpmfileChecksum` (a hash of the
//! local pnpmfile contents). aube emits both so a lockfile it writes is
//! a drop-in match for pnpm's.
//!
//! Correctness matters more here than for most round-tripped fields: a
//! *wrong* checksum is worse than an absent one, because pnpm aborts a
//! `--frozen-lockfile` install with `ERR_PNPM_LOCKFILE_CONFIG_MISMATCH`
//! when a recorded checksum disagrees with what pnpm itself would
//! compute. Both algorithms below are therefore reproduced byte-for-byte
//! from pnpm and pinned to ground-truth vectors in the unit tests.
//!
//! * `packageExtensionsChecksum` — `@pnpm/crypto.object-hasher`'s
//!   `hashObjectNullableWithPrefix`, i.e. the `object-hash` npm package
//!   run with `{ respectType: false, algorithm: 'sha256', encoding:
//!   'base64', unorderedArrays: true, unorderedObjects: true,
//!   unorderedSets: true }`, prefixed `sha256-`. Empty/absent
//!   extensions hash to nothing, so the field is omitted.
//! * `pnpmfileChecksum` — `@pnpm/crypto.hash`'s
//!   `createHashFromMultipleFiles`: each file is read as UTF-8 with
//!   `\r\n` normalized to `\n` and hashed to `sha256-<base64>`. A single
//!   file returns that hash directly; multiple files hash the
//!   comma-joined per-file hashes (over their sorted paths).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `sha256-<base64>` of `bytes`, matching pnpm's `createHash(...).
/// digest('base64')` and `object-hash`'s `encoding: 'base64'` with the
/// `sha256-` prefix pnpm prepends.
fn sha256_base64_prefixed(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256-{}", STANDARD.encode(digest))
}

/// pnpm's `packageExtensionsChecksum`.
///
/// Returns `None` for an empty map — pnpm's `hashObjectNullableWithPrefix`
/// yields `undefined` for an empty/absent object and the lockfile field
/// is omitted. Otherwise serializes the object exactly as `object-hash`
/// does under pnpm's options and hashes the result.
pub fn package_extensions_checksum(extensions: &BTreeMap<String, Value>) -> Option<String> {
    if extensions.is_empty() {
        return None;
    }
    let mut buf = Vec::new();
    write_object_entries(extensions.iter().map(|(k, v)| (k.as_str(), v)), &mut buf);
    Some(sha256_base64_prefixed(&buf))
}

/// pnpm's `pnpmfileChecksum` (`createHashFromMultipleFiles`).
///
/// `paths` are the *local* pnpmfiles that participate in the checksum;
/// pnpm excludes the global pnpmfile. Returns `None` when no paths are
/// given (caller omits the field). Paths are sorted before hashing to
/// match pnpm, which sorts the file list first.
pub fn pnpmfile_checksum(paths: &[PathBuf]) -> std::io::Result<Option<String>> {
    if paths.is_empty() {
        return Ok(None);
    }
    let mut sorted: Vec<&PathBuf> = paths.iter().collect();
    sorted.sort();
    if sorted.len() == 1 {
        return Ok(Some(hash_pnpmfile(sorted[0])?));
    }
    let hashes = sorted
        .iter()
        .map(|p| hash_pnpmfile(p))
        .collect::<std::io::Result<Vec<String>>>()?;
    Ok(Some(sha256_base64_prefixed(hashes.join(",").as_bytes())))
}

/// `@pnpm/crypto.hash`'s `createHashFromFile`: read the file as UTF-8,
/// normalize CRLF → LF, then hash. Normalization makes the checksum
/// stable across the line-ending a pnpmfile happens to be checked out
/// with (Windows vs. POSIX).
fn hash_pnpmfile(path: &Path) -> std::io::Result<String> {
    let content = std::fs::read_to_string(path)?;
    let normalized = content.replace("\r\n", "\n");
    Ok(sha256_base64_prefixed(normalized.as_bytes()))
}

/// `object-hash`'s `dispatch` for a JSON value under pnpm's options
/// (`respectType: false`, `unorderedObjects: true`, `unorderedArrays:
/// true`). Appends the same byte stream `object-hash` feeds to its
/// digest, so hashing `out` reproduces the JS result.
fn object_hash_dispatch(value: &Value, out: &mut Vec<u8>) {
    match value {
        // `_null` writes the literal `Null`.
        Value::Null => out.extend_from_slice(b"Null"),
        // `_boolean` writes `bool:true` / `bool:false`.
        Value::Bool(b) => {
            out.extend_from_slice(b"bool:");
            out.extend_from_slice(if *b { b"true" } else { b"false" });
        }
        // `_number` writes `number:` + the JS `Number.toString()`.
        Value::Number(n) => {
            out.extend_from_slice(b"number:");
            out.extend_from_slice(js_number_string(n).as_bytes());
        }
        Value::String(s) => write_hashed_string(s, out),
        Value::Array(items) => write_array(items, out),
        Value::Object(map) => {
            write_object_entries(map.iter().map(|(k, v)| (k.as_str(), v)), out);
        }
    }
}

/// `object-hash`'s `_string`: `string:<len>:<value>`, where `<len>` is
/// the JS string length (UTF-16 code units, not bytes).
fn write_hashed_string(s: &str, out: &mut Vec<u8>) {
    let len = s.encode_utf16().count();
    out.extend_from_slice(format!("string:{len}:").as_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// `object-hash`'s `_object` with `respectType: false` (no synthetic
/// `prototype`/`__proto__`/`constructor` keys) and `unorderedObjects:
/// true` (keys sorted): `object:<n>:`, then per sorted key the key as a
/// hashed string, a `:`, the dispatched value, and a trailing `,`.
fn write_object_entries<'a, I>(entries: I, out: &mut Vec<u8>)
where
    I: Iterator<Item = (&'a str, &'a Value)>,
{
    let mut pairs: Vec<(&str, &Value)> = entries.collect();
    // JS `Array.prototype.sort()` orders by UTF-16 code units; Rust's
    // `str` ordering is by Unicode scalar value. These agree for every
    // BMP character, which covers all package-selector keys (ASCII).
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    out.extend_from_slice(format!("object:{}:", pairs.len()).as_bytes());
    for (key, value) in pairs {
        write_hashed_string(key, out);
        out.push(b':');
        object_hash_dispatch(value, out);
        out.push(b',');
    }
}

/// `object-hash`'s `_array` with `unorderedArrays: true`. Writes
/// `array:<n>:`, then — when the array has more than one element —
/// serializes each element independently, sorts those serializations,
/// and re-emits them through the ordered-array path (a second
/// `array:<n>:` followed by each serialization dispatched as a string).
/// Zero/one-element arrays emit their elements in place, matching
/// object-hash's `if (typeof options.unorderedArrays !== 'undefined'
/// && arr.length > 1)` guard.
fn write_array(items: &[Value], out: &mut Vec<u8>) {
    out.extend_from_slice(format!("array:{}:", items.len()).as_bytes());
    if items.len() <= 1 {
        for item in items {
            object_hash_dispatch(item, out);
        }
        return;
    }
    let mut serialized: Vec<Vec<u8>> = items
        .iter()
        .map(|item| {
            let mut b = Vec::new();
            object_hash_dispatch(item, &mut b);
            b
        })
        .collect();
    serialized.sort();
    out.extend_from_slice(format!("array:{}:", serialized.len()).as_bytes());
    for entry in &serialized {
        // object-hash's recursive `_array(contents, false)` dispatches
        // each already-serialized element as a JS string. The bytes are
        // UTF-8 by construction (everything we emit is), so re-reading
        // them as `&str` is sound.
        let entry_str =
            std::str::from_utf8(entry).expect("object-hash element serialization is valid UTF-8");
        write_hashed_string(entry_str, out);
    }
}

/// JS `Number.prototype.toString()` (radix 10) for a serde_json number.
///
/// `packageExtensions` values are almost always strings (versions are
/// quoted), so numbers are rare here, but `object-hash` still routes
/// them through `(n).toString()`. Integers match Rust's formatting
/// across the i64/u64 range. Floats reuse Rust's shortest round-trip
/// digits — the same digit sequence V8 picks — but re-apply V8's
/// notation rules (see [`js_f64_to_string`]), because Rust's
/// `f64::to_string` renders large/small magnitudes in plain decimal
/// (`1000000000000000000000`, `0.0000001`) where V8 switches to
/// exponential (`1e+21`, `1e-7`). serde_json never produces
/// NaN/Infinity; JS prints negative zero as `0`.
fn js_number_string(n: &serde_json::Number) -> String {
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    match n.as_f64() {
        Some(f) => js_f64_to_string(f),
        None => "0".to_string(),
    }
}

/// ECMA-262 `Number::toString` (radix 10) for a finite `f64`, matching
/// V8 byte-for-byte.
///
/// Rust and V8 agree on the *shortest round-trip digit sequence*; they
/// disagree only on notation. We grab those digits from `{:e}` (which
/// always normalizes to one leading digit) and re-emit them with the
/// spec's fixed-vs-exponential rules: fixed-point for `-6 < n <= 21`,
/// exponential otherwise, where `n` is the position of the decimal
/// point relative to the first significant digit.
fn js_f64_to_string(f: f64) -> String {
    // Covers both +0.0 and -0.0; JS renders each as "0".
    if f == 0.0 {
        return "0".to_string();
    }

    // `{:e}` yields the shortest mantissa as `d[.ddd]e<exp>` with a
    // single leading digit (1 <= |mantissa| < 10) — the exact
    // significant digits V8 uses, in a notation we can decompose.
    let exp_form = format!("{:e}", f.abs());
    // INVARIANT: Rust's LowerExp formatting always emits an `e`, and the
    // exponent after it is always a valid base-10 i64 — neither can fail.
    let (mantissa, exp) = exp_form
        .split_once('e')
        .expect("{:e} always emits an exponent separator");
    let exp: i64 = exp.parse().expect("{:e} exponent is a base-10 integer");

    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i64; // count of significant digits
    // ECMA-262 `n`: value == digits × 10^(n - k); the decimal point sits
    // after the first `n` digits. With one leading digit, n = exp + 1.
    let n = exp + 1;

    let mut out = String::new();
    if f < 0.0 {
        out.push('-');
    }
    if k <= n && n <= 21 {
        // Integer: every digit, then (n - k) trailing zeros.
        out.push_str(&digits);
        out.push_str(&"0".repeat((n - k) as usize));
    } else if 0 < n && n <= 21 {
        // Decimal point falls inside the digit run.
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        // Leading "0.", then (-n) zeros, then every digit.
        out.push_str("0.");
        out.push_str(&"0".repeat((-n) as usize));
        out.push_str(&digits);
    } else {
        // Exponential: d[.ddd]e{+|-}{exp}, with exp == n - 1.
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let e10 = n - 1;
        out.push('e');
        out.push(if e10 < 0 { '-' } else { '+' });
        out.push_str(&e10.abs().to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `packageExtensions`-shaped map from a JSON object string.
    fn pe(json: &str) -> BTreeMap<String, Value> {
        match serde_json::from_str::<Value>(json).expect("valid JSON") {
            Value::Object(m) => m.into_iter().collect(),
            other => panic!("expected JSON object, got {other:?}"),
        }
    }

    #[test]
    fn package_extensions_empty_is_none() {
        assert_eq!(package_extensions_checksum(&BTreeMap::new()), None);
    }

    // Each vector below was produced by the real `object-hash` npm
    // package under pnpm's exact options
    // (`{respectType:false,algorithm:'sha256',encoding:'base64',
    // unorderedArrays:true,unorderedObjects:true,unorderedSets:true}`)
    // with the `sha256-` prefix pnpm adds. They lock the port to pnpm's
    // byte stream: nested objects + key sorting, booleans, numbers,
    // and the unordered-array sort path.

    #[test]
    fn package_extensions_simple_matches_pnpm() {
        let got = package_extensions_checksum(&pe(r#"{"a":{"dependencies":{"b":"1.0.0"}}}"#));
        assert_eq!(
            got.as_deref(),
            Some("sha256-9yDK//Ix13a8CrWmJGIeVC0z1tCnQxNHOLTw47oh10s=")
        );
    }

    #[test]
    fn package_extensions_bool_and_number_match_pnpm() {
        let got = package_extensions_checksum(&pe(r#"{"k":{"optional":true,"count":3}}"#));
        assert_eq!(
            got.as_deref(),
            Some("sha256-EOT4Rq2KGdwdUwAI9FuL2HmoawSWgN2C+QLiGsRhY20=")
        );
    }

    /// `js_f64_to_string` must match V8's `Number.prototype.toString()`
    /// byte-for-byte — especially the exponential notation V8 uses for
    /// magnitudes >= 1e21 and < 1e-6, where Rust's `f64::to_string`
    /// emits plain decimal and would silently diverge the checksum from
    /// pnpm (triggering a frozen-lockfile config mismatch).
    #[test]
    fn js_f64_to_string_matches_v8_notation() {
        let cases = [
            (0.0_f64, "0"),
            (-0.0_f64, "0"),
            (3.5, "3.5"),
            (0.5, "0.5"),
            (0.0001, "0.0001"),
            (1e-6, "0.000001"), // last fixed-point magnitude
            (1e-7, "1e-7"),     // first exponential (small)
            (1e-8, "1e-8"),
            (-1.5e-7, "-1.5e-7"),
            (123.456, "123.456"),
            (1e20, "100000000000000000000"), // last fixed-point integer
            (1e21, "1e+21"),                 // first exponential (large)
            (1.5e21, "1.5e+21"),
            (-1e21, "-1e+21"),
        ];
        for (input, want) in cases {
            assert_eq!(js_f64_to_string(input), want, "f64 {input:?}");
        }
    }

    /// Integers route through Rust's integer formatting; float-backed
    /// numbers route through the V8-parity path.
    #[test]
    fn js_number_string_routes_ints_and_floats() {
        let int = match serde_json::from_str::<Value>("42").expect("valid JSON") {
            Value::Number(n) => n,
            other => panic!("expected number, got {other:?}"),
        };
        let big = match serde_json::from_str::<Value>("1e21").expect("valid JSON") {
            Value::Number(n) => n,
            other => panic!("expected number, got {other:?}"),
        };
        assert_eq!(js_number_string(&int), "42");
        assert_eq!(js_number_string(&big), "1e+21");
    }

    #[test]
    fn package_extensions_unordered_array_matches_pnpm() {
        // `bundledDependencies` is intentionally out of order to
        // exercise object-hash's array-sort branch.
        let got = package_extensions_checksum(&pe(
            r#"{"pkg":{"bundledDependencies":["z","a","m"],"dependencies":{"x":"1"}}}"#,
        ));
        assert_eq!(
            got.as_deref(),
            Some("sha256-9nkLQlH+XcJg38ygPgoq2a+Lz8cfE7PtUaAbUzni6oA=")
        );
    }

    #[test]
    fn package_extensions_key_order_is_irrelevant() {
        // Same logical object, keys written in a different order: the
        // `unorderedObjects` sort must make both hash identically.
        let a = package_extensions_checksum(&pe(r#"{"a":{"x":"1","y":"2"}}"#));
        let b = package_extensions_checksum(&pe(r#"{"a":{"y":"2","x":"1"}}"#));
        assert!(a.is_some());
        assert_eq!(a, b);
    }

    #[test]
    fn package_extensions_realistic_object_is_order_independent() {
        // A realistic multi-selector `packageExtensions` block (public
        // packages only) exercising nested dependency maps, peer deps, a
        // `github:` spec, and an out-of-order `bundledDependencies` array.
        // pnpm hashes objects and arrays unordered, so the same logical
        // config written in any key/element order must hash identically.
        // The exact byte stream is pinned to real object-hash output by
        // the focused vectors above.
        let canonical = package_extensions_checksum(&pe(r#"{
            "express@*": {"dependencies": {"body-parser": "1.20.2", "compression": "1.7.4"}},
            "react-dom@*": {
                "dependencies": {"scheduler": "0.23.0"},
                "peerDependencies": {"react": "^18.0.0"}
            },
            "request@*": {
                "dependencies": {
                    "tough-cookie": "github:salesforce/tough-cookie#v4.1.3",
                    "form-data": "4.0.0"
                },
                "bundledDependencies": ["zlib", "abbrev", "minimist"]
            },
            "zod@*": {"dependencies": {"@types/node": "20.11.0"}},
            "lodash@*": {"dependencies": {"just-extend": "6.2.0"}}
        }"#));
        assert!(canonical.is_some());

        // The same config with top-level keys, nested keys, and the
        // `bundledDependencies` array all reordered must hash identically.
        let reordered = package_extensions_checksum(&pe(r#"{
            "lodash@*": {"dependencies": {"just-extend": "6.2.0"}},
            "zod@*": {"dependencies": {"@types/node": "20.11.0"}},
            "request@*": {
                "bundledDependencies": ["minimist", "zlib", "abbrev"],
                "dependencies": {
                    "form-data": "4.0.0",
                    "tough-cookie": "github:salesforce/tough-cookie#v4.1.3"
                }
            },
            "react-dom@*": {
                "peerDependencies": {"react": "^18.0.0"},
                "dependencies": {"scheduler": "0.23.0"}
            },
            "express@*": {"dependencies": {"compression": "1.7.4", "body-parser": "1.20.2"}}
        }"#));

        assert_eq!(canonical, reordered);
    }

    #[test]
    fn pnpmfile_checksum_empty_is_none() {
        assert_eq!(pnpmfile_checksum(&[]).unwrap(), None);
    }

    #[test]
    fn pnpmfile_checksum_single_file_matches_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".pnpmfile.cjs");
        std::fs::write(&path, "module.exports = { hooks: {} };\n").unwrap();
        // sha256-<base64> of the exact bytes above.
        let expected = sha256_base64_prefixed(b"module.exports = { hooks: {} };\n");
        assert_eq!(
            pnpmfile_checksum(std::slice::from_ref(&path)).unwrap(),
            Some(expected)
        );
    }

    #[test]
    fn pnpmfile_checksum_normalizes_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let lf = dir.path().join("lf.cjs");
        let crlf = dir.path().join("crlf.cjs");
        std::fs::write(&lf, "a\nb\nc\n").unwrap();
        std::fs::write(&crlf, "a\r\nb\r\nc\r\n").unwrap();
        // Same logical content → same hash after CRLF normalization.
        assert_eq!(
            pnpmfile_checksum(std::slice::from_ref(&lf)).unwrap(),
            pnpmfile_checksum(std::slice::from_ref(&crlf)).unwrap(),
        );
    }

    #[test]
    fn pnpmfile_checksum_multiple_files_hashes_joined_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.cjs");
        let b = dir.path().join("b.cjs");
        std::fs::write(&a, "first\n").unwrap();
        std::fs::write(&b, "second\n").unwrap();
        // Expected = sha256-<base64> of the comma-joined per-file hashes
        // in sorted-path order (`a.cjs` < `b.cjs`).
        let ha = sha256_base64_prefixed(b"first\n");
        let hb = sha256_base64_prefixed(b"second\n");
        let expected = sha256_base64_prefixed(format!("{ha},{hb}").as_bytes());
        assert_eq!(
            pnpmfile_checksum(&[b.clone(), a.clone()]).unwrap(),
            Some(expected)
        );
    }
}

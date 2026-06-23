use crate::{Error, PackageIndex};
use sha1::Sha1;
use sha2::{Digest as _, Sha256, Sha384, Sha512};
use std::cell::RefCell;

thread_local! {
    static SHA512_HASHER: RefCell<Sha512> = RefCell::new(Sha512::new());
}

pub const SHA512_INTEGRITY_PREFIX: &str = "sha512-";

/// Subresource Integrity (SRI) algorithm prefixes aube accepts in
/// `dist.integrity`. sha512 is what modern registries emit; sha1 is
/// kept for legacy packages (e.g. `co@4.6.0`) that were published
/// before npm's 2017 SRI rollout and never had their metadata rewritten.
const SRI_PREFIXES: &[(&str, IntegrityAlgo)] = &[
    ("sha512-", IntegrityAlgo::Sha512),
    ("sha384-", IntegrityAlgo::Sha384),
    ("sha256-", IntegrityAlgo::Sha256),
    ("sha1-", IntegrityAlgo::Sha1),
];

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum IntegrityAlgo {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl IntegrityAlgo {
    fn prefix(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1-",
            Self::Sha256 => "sha256-",
            Self::Sha384 => "sha384-",
            Self::Sha512 => "sha512-",
        }
    }
}

fn parse_sri(expected: &str) -> Option<(IntegrityAlgo, &str)> {
    SRI_PREFIXES
        .iter()
        .find_map(|(prefix, algo)| expected.strip_prefix(prefix).map(|rest| (*algo, rest)))
}

/// Validate a package name and return the `safe_name` form used as a
/// cache filename stem (`/` collapsed to `__` so scoped names survive
/// a single path component). Refuses anything outside the npm name
/// grammar so a hostile packument cannot turn a cache write into an
/// arbitrary-file-write primitive. Public so callers in
/// `aube-registry` and `aube` (which own separate cache layouts under
/// the same cache root) can share one validator.
///
/// A malicious packument can set `name` to `../../etc/passwd` (or, on
/// Windows, to something with a drive prefix or backslash). The old
/// `name.replace('/', "__")` only stripped forward slashes, so
/// `index_dir().join(format!("{name}@{version}.json"))` would silently
/// resolve outside the cache directory on the first resolve of the
/// hostile package.
///
/// Accepted grammar is `[A-Za-z0-9_.-]` per component, with a single
/// optional `@scope/` prefix. Uppercase and leading `.` / `_` are
/// allowed on purpose: npm's registry bans them for *new* publishes
/// but thousands of pre-rule packages (`JSONStream`, `Base64`, etc.)
/// still resolve fine under pnpm and bun, and mirroring the registry's
/// publish grammar here would block their cache path and break
/// install. The only rejects are empty components, `.` / `..`, the
/// 214-char length ceiling, and any byte outside the grammar.
pub fn validate_and_encode_name(name: &str) -> Option<String> {
    if name.is_empty() || name.len() > 214 {
        return None;
    }
    let (scope, bare) = match name.strip_prefix('@') {
        Some(rest) => {
            let (s, b) = rest.split_once('/')?;
            (Some(s), b)
        }
        None => (None, name),
    };
    let ok_component = |s: &str| -> bool {
        // npm's registry bars new packages from leading `.` / `_` but
        // historical packages that predate the rule still resolve
        // fine, and scoped private registries allow them. Only bar
        // empty and `.`/`..` since those collide with path components
        // after the `/` → `__` folding.
        if s.is_empty() || s == "." || s == ".." {
            return false;
        }
        s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    if let Some(s) = scope
        && !ok_component(s)
    {
        return None;
    }
    if !ok_component(bare) {
        return None;
    }
    Some(name.replace('/', "__"))
}

/// Check a version string for use as a cache filename component.
/// The lockfile already constrains versions to semver-ish shapes, but
/// the cache path is independent of the lockfile on the write side so
/// a crafted packument version would still land here. Returns `true`
/// for anything the cache path builder is willing to accept.
pub fn validate_version(version: &str) -> bool {
    if version.is_empty() || version.len() > 256 {
        return false;
    }
    // pnpm and bun sometimes route non-semver specs (git URLs, file
    // specs, aliased registries) through the `version` slot, so the
    // guard only needs to block what actually breaks the cache path
    // builder: path separators on any platform, `\0`, control chars,
    // and the two "this is a directory name" aliases.
    if version
        .bytes()
        .any(|b| b.is_ascii_control() || matches!(b, b'/' | b'\\' | b'\0'))
    {
        return false;
    }
    if version == "." || version == ".." {
        return false;
    }
    true
}

/// Verify that data matches an SRI integrity hash. Accepts any of
/// `sha512-` / `sha384-` / `sha256-` / `sha1-` prefixed base64 digests
/// — the set npm and pnpm accept in `dist.integrity`. Returns `Ok(())`
/// on match, `Err(Error::Integrity)` on mismatch or unknown algorithm.
pub fn verify_integrity(data: &[u8], expected: &str) -> Result<(), Error> {
    let Some((algo, expected_b64)) = parse_sri(expected) else {
        return Err(Error::Integrity(format!(
            "unsupported integrity format (expected sha1/sha256/sha384/sha512-...): {expected}"
        )));
    };

    // Stack-buffer the actual digest (max sha512 = 64 bytes) so the
    // hot path stays allocation-free. sha512 reuses the thread-local
    // hasher because it's the common case by 3+ orders of magnitude;
    // the legacy algorithms one-shot a fresh hasher.
    let mut actual_buf = [0u8; 64];
    let actual_len = match algo {
        IntegrityAlgo::Sha1 => {
            let d = Sha1::digest(data);
            actual_buf[..d.len()].copy_from_slice(&d);
            d.len()
        }
        IntegrityAlgo::Sha256 => {
            let d = Sha256::digest(data);
            actual_buf[..d.len()].copy_from_slice(&d);
            d.len()
        }
        IntegrityAlgo::Sha384 => {
            let d = Sha384::digest(data);
            actual_buf[..d.len()].copy_from_slice(&d);
            d.len()
        }
        IntegrityAlgo::Sha512 => SHA512_HASHER.with(|cell| {
            let mut hasher = cell.borrow_mut();
            hasher.reset();
            hasher.update(data);
            let d = hasher.finalize_reset();
            actual_buf[..d.len()].copy_from_slice(&d);
            d.len()
        }),
    };
    let actual = &actual_buf[..actual_len];

    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    let mut expected_digest = [0u8; 64];
    let matched = engine
        .decode_slice(expected_b64, &mut expected_digest)
        .map(|n| n == actual_len && expected_digest[..n] == actual[..])
        .unwrap_or(false);
    if matched {
        Ok(())
    } else {
        let actual_b64 = engine.encode(actual);
        Err(Error::Integrity(format!(
            "integrity mismatch: expected {expected}, got {prefix}{actual_b64}",
            prefix = algo.prefix(),
        )))
    }
}

/// Return the npm Subresource Integrity string for `data` using SHA-512.
pub fn sha512_integrity(data: &[u8]) -> String {
    let digest = SHA512_HASHER.with(|cell| {
        let mut hasher = cell.borrow_mut();
        hasher.reset();
        hasher.update(data);
        hasher.finalize_reset()
    });
    use base64::Engine;
    format!(
        "{SHA512_INTEGRITY_PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

/// Verify a precomputed SHA-512 digest against an SRI integrity
/// string. Used by the streaming-tarball fetch path: SHA-512 is
/// computed during the chunk read loop, then handed here so the
/// owned `Bytes` are not re-hashed on the import side. Saves one
/// pass over the buffer (~7 ms / 5 MB tarball).
///
/// Returns `Ok(true)` when the SRI uses SHA-512 and the digest
/// matches. Returns `Ok(false)` when the SRI uses a non-SHA-512
/// algo (legacy SHA-1 / SHA-256 / SHA-384) so the caller can
/// fall through to the buffered `verify_integrity` path that
/// re-hashes with the right algo. Returns `Err` on parse failure
/// or SHA-512 mismatch.
pub fn verify_precomputed_sha512(actual: &[u8; 64], expected: &str) -> Result<bool, Error> {
    let Some((algo, expected_b64)) = parse_sri(expected) else {
        return Err(Error::Integrity(format!(
            "unsupported integrity format (expected sha1/sha256/sha384/sha512-...): {expected}"
        )));
    };
    if !matches!(algo, IntegrityAlgo::Sha512) {
        return Ok(false);
    }
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    let mut expected_digest = [0u8; 64];
    let decoded_len = match engine.decode_slice(expected_b64, &mut expected_digest) {
        Ok(n) => n,
        Err(e) => {
            return Err(Error::Integrity(format!(
                "integrity field has malformed base64: {expected} ({e})"
            )));
        }
    };
    if decoded_len != 64 {
        return Err(Error::Integrity(format!(
            "integrity field decoded to {decoded_len} bytes, expected 64 for sha512: {expected}"
        )));
    }
    if expected_digest[..decoded_len] == actual[..] {
        Ok(true)
    } else {
        let actual_b64 = engine.encode(actual);
        Err(Error::Integrity(format!(
            "integrity mismatch: expected {expected}, got sha512-{actual_b64}",
        )))
    }
}

/// Cross-check that an extracted tarball's `package.json` reports the
/// same `name` and `version` the registry told us to fetch. This is the
/// implementation behind the `strictStorePkgContentCheck` setting and
/// guards against registry-substitution attacks where a tarball is
/// served under one (name, version) but actually contains a different
/// package on disk.
///
/// `index` must be the result of a freshly-completed `import_tarball`
/// (or `import_directory`) — the helper reads `package.json` straight
/// from the on-disk store path recorded in the index, so the bytes
/// being validated are exactly the bytes that just landed in the CAS.
///
/// Returns `Ok(())` when both fields match, `Err(Error::PkgContentMismatch)`
/// when they don't, and `Err(Error::Tar)` if the manifest is missing
/// or unparseable. We deliberately treat a missing/broken manifest as
/// a check failure rather than silently passing — a registry tarball
/// without a usable `package.json` is itself a corruption signal.
pub fn validate_pkg_content(
    index: &PackageIndex,
    expected_name: &str,
    expected_version: &str,
) -> Result<(), Error> {
    // The two error paths below intentionally omit the
    // `{expected_name}@{expected_version}` coordinate. Every caller
    // wraps with `miette!("{name}@{version}: {e}")` (mirroring the
    // Error::Integrity path), so embedding it here would print the
    // same coordinate twice — same rationale as the
    // Error::PkgContentMismatch return below.
    let stored = index
        .get("package.json")
        .ok_or_else(|| Error::Tar("package.json missing from tarball".to_string()))?;
    let bytes =
        std::fs::read(&stored.store_path).map_err(|e| Error::Io(stored.store_path.clone(), e))?;
    let v: serde_json::Value = sonic_rs::from_slice(&bytes)
        .or_else(|_| serde_json::from_slice(&bytes))
        .map_err(|e| Error::Tar(format!("invalid package.json: {e}")))?;
    let actual_name = v.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let actual_version = v.get("version").and_then(|v| v.as_str()).unwrap_or("");
    // Tolerate a leading `v` on the tarball's version (e.g. "v2.0.8").
    // Some publishers ship this shape; npm and bun normalize it on
    // install, so aube does too rather than rejecting a package the
    // other managers accept. The registry-side coordinate is the
    // source of truth, so we only normalize the tarball side.
    let actual_version_normalized = actual_version
        .strip_prefix('v')
        .filter(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or(actual_version);
    // pnpm v9 lockfiles key git-hosted deps by the codeload tarball URL
    // (or a `git+<url>#<commit>` form) in the `version` slot of the
    // dep_path — that URL is what the resolver hands us as
    // `expected_version`, and it can't meaningfully be compared to the
    // tarball's real semver. pnpm scopes its equivalent check to
    // registry sources; do the same by dropping the version comparison
    // (but still checking the name) whenever `expected_version` isn't
    // semver-shaped.
    let expected_is_url_or_ref = expected_version.contains("://")
        || expected_version.starts_with("git+")
        || expected_version.starts_with("file:");
    let version_matches = expected_is_url_or_ref || actual_version_normalized == expected_version;
    if actual_name != expected_name || !version_matches {
        // Only carry the *actual* coordinate the tarball declared.
        // Every caller wraps the error with the expected
        // `{name}@{version}: ` prefix (mirroring the Error::Integrity
        // path), so embedding `expected` here would print the same
        // coordinate twice in the rendered diagnostic.
        return Err(Error::PkgContentMismatch {
            actual: format!("{actual_name}@{actual_version}"),
        });
    }
    Ok(())
}

/// Decode a pnpm-style SRI integrity string (`sha512-` / `sha384-` /
/// `sha256-` / `sha1-` + base64) into its raw hex digest. Used by
/// introspection commands that accept the registry integrity format
/// as an ergonomic input, and by `index_path` to shard the cache
/// directory by integrity prefix. Returns `None` if the input isn't a
/// well-formed SRI integrity string.
pub fn integrity_to_hex(integrity: &str) -> Option<String> {
    let (_, b64) = parse_sri(integrity)?;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    Some(hex::encode(bytes))
}

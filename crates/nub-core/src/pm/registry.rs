//! Registry metadata lookup for PM provisioning: dynamic version resolution
//! (exact / dist-tag / range), the tarball URL, and the dist integrity that gates
//! extraction. Mirrors `version_management::node_index`'s split — a PURE resolver
//! over already-fetched metadata ([`resolve_dist`]) plus a thin networked wrapper
//! ([`resolve_version`]) — so the resolution logic is unit-tested offline.
//!
//! Trust model: HTTPS authenticates that the packument came from the registry;
//! the per-version `dist.integrity` (sha512) authenticates the tarball before it
//! is extracted. No signatures / Sigstore / TUF in scope. sha512 is preferred;
//! `dist.shasum` (sha1) is the fail-closed fallback for ancient publishes that
//! predate `integrity`.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha1::Sha1;
use sha2::{Digest, Sha512};

use crate::version_management::download;
use crate::workspace::scripts::npmrc_value;

/// A single resolved version's dist: where to fetch it, how to verify it, and the
/// path within the extracted `package/` dir to the runnable bin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionDist {
    pub version: String,
    pub tarball: String,
    pub integrity: Integrity,
    /// The bin entry's path relative to the package root (`bin/pnpm.cjs`). For a
    /// PM the resolver picks the entry whose name matches the package.
    pub bin_subpath: PathBuf,
}

/// The dist checksum that gates extraction. sha512 (the modern `dist.integrity`
/// SRI hash, base64) is preferred; sha1 (`dist.shasum`, hex) is the fallback for
/// publishes too old to carry `integrity`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Integrity {
    /// SRI sha512 payload, base64 (the part after `sha512-`).
    Sha512(String),
    /// `dist.shasum`, lowercase hex.
    Sha1(String),
}

/// The registry base URL, in precedence order:
///   1. `npm_config_registry` (npm/pnpm/yarn all export this when they shell out).
///   2. `.npmrc`'s `registry` key (project, then `~/.npmrc`).
///   3. the public registry.
///
/// Deliberately single-override per source (no `COREPACK_NPM_REGISTRY` layer —
/// matching `resolve_mirror_base`'s single-env shape). The trailing slash is
/// normalized off so callers concatenate `/<pkg>` without doubling it.
pub fn registry_base(root: &std::path::Path) -> String {
    let raw = std::env::var("npm_config_registry")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| npmrc_value(root, "registry"))
        .unwrap_or_else(|| "https://registry.npmjs.org/".to_string());
    raw.trim_end_matches('/').to_string()
}

/// Resolve `spec` against an already-fetched packument. PURE — no network, no env
/// — so the resolution rules are unit-tested offline. Handles:
///   - an exact `X.Y.Z` (verbatim key lookup; a typo'd version errors here rather
///     than attempting a doomed download),
///   - a dist-tag (`latest`, `next`, …) via the `dist-tags` map,
///   - a semver range (`^9`, `10`, `>=9 <11`) via the highest satisfying key.
///
/// Range parsing goes through Cargo's `semver` crate, which requires comparators
/// be comma-separated; npm/node-semver (what `packageManager` / `devEngines` users
/// write) separates them by space. [`normalize_range`] bridges the two so a
/// `>=9 <11` pin resolves rather than erroring. The `||` OR operator is NOT
/// supported (Cargo's `semver` has no OR) — vanishingly rare in a PM pin.
pub fn resolve_dist(packument: &Value, spec: &str) -> Result<VersionDist> {
    let spec = spec.trim();
    let versions = packument
        .get("versions")
        .and_then(Value::as_object)
        .context("packument has no \"versions\" map")?;

    // 1. A dist-tag short-circuits to its concrete version.
    if let Some(tagged) = packument
        .get("dist-tags")
        .and_then(|t| t.get(spec))
        .and_then(Value::as_str)
    {
        let meta = versions.get(tagged).with_context(|| {
            format!("dist-tag \"{spec}\" points at {tagged}, absent from \"versions\"")
        })?;
        return dist_from_meta(tagged, meta);
    }

    // 2. An exact version is a verbatim key lookup.
    if let Some(meta) = versions.get(spec) {
        return dist_from_meta(spec, meta);
    }

    // 3. Otherwise treat the spec as a semver range and pick the highest match.
    let req = semver::VersionReq::parse(&normalize_range(spec)).with_context(|| {
        format!("\"{spec}\" is not an exact version, dist-tag, or semver range")
    })?;
    let best = versions
        .keys()
        .filter_map(|v| semver::Version::parse(v).ok().map(|parsed| (parsed, v)))
        .filter(|(parsed, _)| req.matches(parsed))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, key)| key)
        .with_context(|| format!("no published version satisfies \"{spec}\""))?;
    dist_from_meta(best, &versions[best])
}

/// Translate a node-semver range into the form Cargo's `semver` crate parses:
/// space-separated comparators (`>=9 <11`) become comma-separated (`>=9, <11`).
/// A spec that already uses commas, or that is a single comparator (`^9`, `10`,
/// `>=9`), is returned unchanged. This is a syntactic bridge only — it does not
/// translate the `||` OR operator (unsupported by Cargo's `semver`).
fn normalize_range(spec: &str) -> String {
    let spec = spec.trim();
    // Single token, or the user already comma-separated → nothing to do.
    if spec.contains(',') || !spec.contains(char::is_whitespace) {
        return spec.to_string();
    }
    spec.split_whitespace().collect::<Vec<_>>().join(", ")
}

/// Build a [`VersionDist`] from one `versions[X.Y.Z]` entry. `version` is the
/// resolved key (so callers print the concrete version, never the spec).
fn dist_from_meta(version: &str, meta: &Value) -> Result<VersionDist> {
    let dist = meta
        .get("dist")
        .with_context(|| format!("version {version} has no \"dist\" object"))?;
    let tarball = dist
        .get("tarball")
        .and_then(Value::as_str)
        .with_context(|| format!("version {version} has no dist.tarball"))?
        .to_string();
    let integrity = parse_integrity(dist)
        .with_context(|| format!("version {version} has no usable dist integrity"))?;
    let bin_subpath = bin_subpath(meta)
        .with_context(|| format!("version {version} has no resolvable bin entry to run"))?;
    Ok(VersionDist {
        version: version.to_string(),
        tarball,
        integrity,
        bin_subpath,
    })
}

/// Prefer sha512 from the SRI `dist.integrity` (it may list several algorithms
/// space-separated — pick the sha512 entry), then fall back to the hex
/// `dist.shasum` (sha1).
fn parse_integrity(dist: &Value) -> Option<Integrity> {
    if let Some(sri) = dist.get("integrity").and_then(Value::as_str) {
        if let Some(sha512) = sri
            .split_whitespace()
            .find_map(|tok| tok.strip_prefix("sha512-"))
        {
            return Some(Integrity::Sha512(sha512.to_string()));
        }
    }
    dist.get("shasum")
        .and_then(Value::as_str)
        .map(|hex| Integrity::Sha1(hex.to_string()))
}

/// The bin path to run, relative to the package root. npm's `bin` is either a
/// string (single bin == the package name) or a map of `name -> path`; for a PM
/// the entry whose key matches the package `name` is the launcher, with the
/// sole-entry and single-string forms as fallbacks.
fn bin_subpath(meta: &Value) -> Option<PathBuf> {
    let bin = meta.get("bin")?;
    if let Some(path) = bin.as_str() {
        return Some(PathBuf::from(path));
    }
    let map = bin.as_object()?;
    let name = meta.get("name").and_then(Value::as_str);
    let chosen = name
        .and_then(|n| map.get(n))
        .or_else(|| (map.len() == 1).then(|| map.values().next()).flatten())?;
    chosen.as_str().map(PathBuf::from)
}

/// Networked wrapper: fetch the packument from `base` and resolve `spec` against
/// it. `pkg` is the package name (`pnpm`, `npm`, `yarn`).
pub fn resolve_version(base: &str, pkg: &str, spec: &str) -> Result<VersionDist> {
    let url = format!("{}/{pkg}", base.trim_end_matches('/'));
    let body = download::fetch_text(&url).with_context(|| format!("fetching packument {url}"))?;
    let packument: Value =
        serde_json::from_str(&body).with_context(|| format!("parsing packument {url}"))?;
    resolve_dist(&packument, spec).with_context(|| format!("resolving {pkg}@{spec}"))
}

/// Verify a downloaded tarball against its dist integrity. Fail-closed: a mismatch
/// (or an unreadable file) is an error, and the caller verifies BEFORE extracting.
/// sha512 is checked when present; sha1 only for publishes that lack `integrity`.
///
/// The expected sha512 is the registry's base64 SRI payload — decoded to raw
/// bytes and compared against the raw digest, so there's no base64-vs-base64
/// canonicalization risk (and no base64 *encoder* dependency).
pub fn verify_integrity(file: &std::path::Path, want: &Integrity) -> Result<()> {
    let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    match want {
        Integrity::Sha512(expected_b64) => {
            let expected = base64_decode(expected_b64)
                .with_context(|| format!("decoding sha512 SRI for {}", file.display()))?;
            let got = Sha512::digest(&bytes);
            if got.as_slice() != expected.as_slice() {
                bail!(
                    "sha512 integrity mismatch for {}: expected sha512-{expected_b64}, got sha512-{}",
                    file.display(),
                    base64_encode(&got)
                );
            }
        }
        Integrity::Sha1(expected_hex) => {
            let got = hex_lower(&Sha1::digest(&bytes));
            if !got.eq_ignore_ascii_case(expected_hex) {
                bail!(
                    "sha1 integrity mismatch for {}: expected {expected_hex}, got {got}",
                    file.display()
                );
            }
        }
    }
    Ok(())
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard-alphabet base64 decode (the SRI form npm publishes — padded, `+/`).
/// Small and self-contained so verification needs no base64 *crate*; only the
/// alphabet npm uses is accepted (`-_` URL-safe or stray chars are rejected).
fn base64_decode(s: &str) -> Result<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for ch in s.bytes() {
        let val = B64
            .iter()
            .position(|&c| c == ch)
            .with_context(|| format!("invalid base64 character {:?}", ch as char))?
            as u32;
        acc = (acc << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

/// Standard-alphabet base64 encode — only used to render the *actual* digest in a
/// mismatch message (the happy path never encodes).
fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[((n >> (18 - i * 6)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small packument in the real registry shape: a `dist-tags` map and a
    /// `versions` object whose entries carry `dist.{tarball,integrity,shasum}` and
    /// `bin`. Mixed integrity coverage: 10.0.0 has sha512, 8.0.0 is sha1-only.
    const PACKUMENT: &str = r#"{
        "name": "pnpm",
        "dist-tags": { "latest": "10.0.0", "next": "11.0.0-rc.1" },
        "versions": {
            "8.0.0": {
                "name": "pnpm",
                "bin": { "pnpm": "bin/pnpm.cjs", "pnpx": "bin/pnpx.cjs" },
                "dist": {
                    "tarball": "https://registry.npmjs.org/pnpm/-/pnpm-8.0.0.tgz",
                    "shasum": "0123456789abcdef0123456789abcdef01234567"
                }
            },
            "9.5.0": {
                "name": "pnpm",
                "bin": { "pnpm": "bin/pnpm.cjs" },
                "dist": {
                    "tarball": "https://registry.npmjs.org/pnpm/-/pnpm-9.5.0.tgz",
                    "integrity": "sha512-AAAA",
                    "shasum": "aaaa"
                }
            },
            "10.0.0": {
                "name": "pnpm",
                "bin": { "pnpm": "bin/pnpm.cjs" },
                "dist": {
                    "tarball": "https://registry.npmjs.org/pnpm/-/pnpm-10.0.0.tgz",
                    "integrity": "sha512-BBBB",
                    "shasum": "bbbb"
                }
            },
            "11.0.0-rc.1": {
                "name": "pnpm",
                "bin": { "pnpm": "bin/pnpm.cjs" },
                "dist": {
                    "tarball": "https://registry.npmjs.org/pnpm/-/pnpm-11.0.0-rc.1.tgz",
                    "integrity": "sha512-CCCC"
                }
            }
        }
    }"#;

    fn packument() -> Value {
        serde_json::from_str(PACKUMENT).unwrap()
    }

    #[test]
    fn resolves_exact_dist_tag_and_range_to_the_right_version() {
        let p = packument();

        // Exact: verbatim key, sha512 chosen over the sibling shasum.
        let exact = resolve_dist(&p, "9.5.0").unwrap();
        assert_eq!(exact.version, "9.5.0");
        assert_eq!(exact.integrity, Integrity::Sha512("AAAA".into()));
        assert_eq!(exact.bin_subpath, PathBuf::from("bin/pnpm.cjs"));

        // dist-tag: `latest` resolves to its mapped concrete version.
        assert_eq!(resolve_dist(&p, "latest").unwrap().version, "10.0.0");

        // Range: highest satisfying STABLE key (the rc is not in range for ^9).
        assert_eq!(resolve_dist(&p, "^9").unwrap().version, "9.5.0");
        // Bare-major range picks the newest 10.x, not the 11 rc.
        assert_eq!(resolve_dist(&p, "10").unwrap().version, "10.0.0");
        // node-semver space-separated comparators (`>=9 <10`) — npm/devEngines
        // write these; Cargo's semver needs commas, so the normalizer bridges it.
        assert_eq!(resolve_dist(&p, ">=9 <10").unwrap().version, "9.5.0");
    }

    #[test]
    fn normalize_range_bridges_space_separated_comparators_only() {
        // Space-separated comparators → comma form (the only translation).
        assert_eq!(normalize_range(">=9 <11"), ">=9, <11");
        assert_eq!(normalize_range(">=9   <11"), ">=9, <11"); // runs of space collapse
        // Single comparators and bare versions pass through untouched.
        assert_eq!(normalize_range("^9"), "^9");
        assert_eq!(normalize_range("10"), "10");
        // An already-comma'd spec is left alone (no double-comma).
        assert_eq!(normalize_range(">=9, <11"), ">=9, <11");
    }

    #[test]
    fn nonexistent_exact_and_unsatisfiable_range_error() {
        let p = packument();
        // An exact version that isn't published isn't a valid range either, so it
        // surfaces the range parse/resolution error rather than a doomed fetch.
        assert!(resolve_dist(&p, "9.9.9").is_err());
        // A range with no matching key errors naming the spec.
        let err = resolve_dist(&p, ">=20").unwrap_err().to_string();
        assert!(
            err.contains(">=20"),
            "error names the unsatisfiable spec: {err}"
        );
    }

    #[test]
    fn sha1_only_publish_falls_back_to_shasum() {
        // 8.0.0 has no `integrity` — the resolver must fall back to dist.shasum.
        let dist = resolve_dist(&packument(), "8.0.0").unwrap();
        assert_eq!(
            dist.integrity,
            Integrity::Sha1("0123456789abcdef0123456789abcdef01234567".into())
        );
    }

    #[test]
    fn bin_subpath_picks_the_entry_named_for_the_package() {
        // 8.0.0's bin map has two entries; the one keyed by the package name wins.
        let dist = resolve_dist(&packument(), "8.0.0").unwrap();
        assert_eq!(dist.bin_subpath, PathBuf::from("bin/pnpm.cjs"));
    }

    #[test]
    fn registry_base_reads_npmrc_and_normalizes_the_trailing_slash() {
        // The `npm_config_registry` branch is process-global env (flaky to mutate
        // under the parallel harness) — covered by the documented single-override
        // shape, not asserted here. When it's set in the ambient env, skip the
        // lower-precedence assertions it would shadow.
        let env_set = std::env::var("npm_config_registry").is_ok_and(|v| !v.trim().is_empty());

        let dir = std::env::temp_dir().join(format!("nub-pm-reg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // A project `.npmrc registry` is read and its trailing slash trimmed.
        std::fs::write(dir.join(".npmrc"), "registry=https://r.example.test/\n").unwrap();
        if !env_set {
            assert_eq!(registry_base(&dir), "https://r.example.test");
        }

        // No project key and no env → the public registry, slash trimmed.
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        if !env_set && npmrc_value(&empty, "registry").is_none() {
            assert_eq!(registry_base(&empty), "https://registry.npmjs.org");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_integrity_is_fail_closed_and_prefers_sha512() {
        let dir = std::env::temp_dir().join(format!("nub-pm-int-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("blob");
        std::fs::write(&f, b"abc").unwrap();

        // Precomputed digests of "abc":
        //   sha512(base64) = the canonical "abc" SHA-512, base64-encoded
        //   sha1(hex)      = a9993e364706816aba3e25717850c26c9cd0d89d
        let sha512_abc = "3a81oZNherrMQXNJriBBMRLm+k6JqX6iCp7u5ktV05ohkpkqJ0/BqDa6PCOj/uu9RU1EI2Q86A4qmslPpUyknw==";
        let sha1_abc = "a9993e364706816aba3e25717850c26c9cd0d89d";

        assert!(
            verify_integrity(&f, &Integrity::Sha512(sha512_abc.into())).is_ok(),
            "matching sha512 verifies"
        );
        assert!(
            verify_integrity(&f, &Integrity::Sha1(sha1_abc.into())).is_ok(),
            "matching sha1 verifies (uppercase-tolerant)"
        );

        // A wrong sha512 must fail, and the message must carry both digests so a CI
        // failure is self-debugging.
        let err = verify_integrity(&f, &Integrity::Sha512("WRONG".into()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("expected sha512-WRONG"),
            "names the expected: {err}"
        );
        assert!(err.contains(sha512_abc), "names the actual digest: {err}");

        assert!(
            verify_integrity(&f, &Integrity::Sha1("dead".into())).is_err(),
            "a wrong sha1 fails closed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

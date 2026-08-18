//! Fetch a published build-jail catalog from the npm registry and hand it to the installer.
//!
//! ⛔⛔ WHY THIS EXISTS AND WHY IT IS THE LAST HALF. The catalog is `include_str!`-compiled, so a wrong
//! grant on a popular package breaks installs until the next nub release. `nub_sandbox::catalog_update`
//! ships the READER, and `install_from_file` ships the validated atomic WRITER — but nothing fetched, so a
//! user could only benefit by obtaining the file themselves. With the jail default-ON that gap is the
//! difference between a grant fix taking minutes and taking a release cycle.
//!
//! ⛔ THE CHANNEL IS AN npm PACKAGE, DELIBERATELY. `@nubjs/build-jail-catalog` shares nub's own trust root
//! — the CLI is published with npm OIDC provenance — so no new signing story is invented here. It also
//! means the artifact is immutable and versioned, and that a user behind a registry mirror or proxy reaches
//! it through the same infrastructure their dependencies already come through.
//!
//! ⛔ THIS MODULE DECIDES NOTHING ABOUT POLICY. It obtains bytes, proves they are the bytes the registry
//! published, and calls `install_from_file`. Every acceptance rule — schema validity, the compiled-catalog
//! floor, the downgrade refusal, the atomic replace — lives there and is exercised by the reader's own
//! `decide`. A fetcher that validated on its own would be a second opinion that can drift from the reader.

use anyhow::{Context as _, Result, bail};
use std::path::Path;

/// The published catalog package. `@nubjs` is the project's own org — the bare `@nub` scope is the one the
/// brand boundary forbids, and this is install-time plumbing rather than anything a user imports.
const PACKAGE: &str = "@nubjs/build-jail-catalog";

/// Only these hosts, and a redirect off them is refused rather than followed. The registry may legitimately
/// redirect a tarball to its CDN, so both are named; anything else is a hop this fetch will not take.
fn host_allowed(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    matches!(
        parsed.host_str(),
        Some("registry.npmjs.org") | Some("registry.npmmirror.com")
    )
}

/// A tarball big enough to be a catalog and small enough not to be a resource exhaustion. The shipped
/// catalog is ~70 KB of JSON, so 32 MiB is orders of magnitude of headroom while still bounding a
/// registry that returns something unexpected.
const MAX_TARBALL: u64 = 32 * 1024 * 1024;

fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("nub/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("too many redirects");
            }
            if host_allowed(attempt.url().as_str()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .context("building the HTTP client")
}

/// The tarball URL and the integrity string the registry published for the newest version.
///
/// The INTEGRITY comes from the packument, not from the tarball — that is the whole point. Hashing a blob
/// and comparing it against a hash carried by the same blob proves nothing; the packument is a separate
/// fetch, so a substituted tarball fails the comparison.
fn resolve_latest(client: &reqwest::blocking::Client) -> Result<(String, String, String)> {
    // A scoped name's `/` must be percent-encoded for the registry's single-package endpoint.
    let url = format!("https://registry.npmjs.org/{}", PACKAGE.replace('/', "%2F"));
    let resp = client
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .with_context(|| format!("fetching {url}"))?;
    if !resp.status().is_success() {
        bail!("{url} returned HTTP {}", resp.status().as_u16());
    }
    // `text()` then parse, because reqwest is built here without its `json` feature — and parsing
    // explicitly is what lets the error name the packument rather than surfacing a decode error.
    let body = resp.text().context("reading the packument body")?;
    let doc: serde_json::Value =
        serde_json::from_str(&body).context("parsing the packument as JSON")?;
    let latest = doc
        .pointer("/dist-tags/latest")
        .and_then(|v| v.as_str())
        .context("the packument names no dist-tags.latest")?
        .to_string();
    let dist = doc
        .pointer(&format!("/versions/{latest}/dist"))
        .with_context(|| format!("the packument has no versions.{latest}.dist"))?;
    let tarball = dist
        .get("tarball")
        .and_then(|v| v.as_str())
        .context("that version carries no dist.tarball")?
        .to_string();
    let integrity = dist
        .get("integrity")
        .and_then(|v| v.as_str())
        .context(
            "that version carries no dist.integrity — refusing to install an unverifiable catalog",
        )?
        .to_string();
    if !host_allowed(&tarball) {
        bail!("the published tarball URL is not on an allowed host: {tarball}");
    }
    Ok((latest, tarball, integrity))
}

/// Does `bytes` match `integrity`?
///
/// Only `sha512-` is accepted. npm's `integrity` may in principle carry several algorithms or a weaker
/// one; taking whatever it offers would let a downgrade to a broken algorithm pass, so an unrecognised
/// prefix is a refusal rather than a fallback.
pub(crate) fn integrity_matches(bytes: &[u8], integrity: &str) -> Result<()> {
    use base64::Engine as _;
    use sha2::Digest as _;
    let Some(expected_b64) = integrity.strip_prefix("sha512-") else {
        bail!("unsupported integrity algorithm in {integrity:?} — only sha512 is accepted");
    };
    let expected = base64::engine::general_purpose::STANDARD
        .decode(expected_b64.trim())
        .context("the integrity string is not valid base64")?;
    let actual = sha2::Sha512::digest(bytes);
    if actual.as_slice() != expected.as_slice() {
        bail!("the downloaded catalog does not match the integrity the registry published");
    }
    Ok(())
}

/// The catalog JSON inside an npm tarball.
///
/// npm tarballs put everything under a single `package/` prefix, and the file is matched by NAME rather
/// than by position: a tarball may carry a README and a licence beside it, and picking "the first entry"
/// would be a silent dependence on pack order.
pub(crate) fn extract_catalog(tar_gz: &[u8]) -> Result<String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tar_gz));
    for entry in archive.entries().context("reading the tarball")? {
        let mut entry = entry.context("reading a tarball entry")?;
        let path = entry
            .path()
            .context("a tarball entry has no path")?
            .to_path_buf();
        // Only a regular file directly named as the catalog. A tarball entry path is attacker-controlled
        // in the general case, so nothing here is joined onto a filesystem path — the bytes are read into
        // memory and handed to the validator.
        if path.file_name().and_then(|n| n.to_str()) == Some("build-jail-catalog-v2.json") {
            let mut text = String::new();
            use std::io::Read as _;
            entry
                .read_to_string(&mut text)
                .context("reading the catalog out of the tarball")?;
            return Ok(text);
        }
    }
    bail!("the published package contains no build-jail-catalog-v2.json")
}

/// Fetch, verify, and install. Returns the message to print on success.
pub(crate) fn fetch_and_install(data_dir: &Path) -> Result<String> {
    let client = client()?;
    let (version, tarball, integrity) = resolve_latest(&client)?;
    let mut resp = client
        .get(&tarball)
        // The body must BE the published artifact: a transparently re-encoded response would hash
        // differently than the integrity the registry published for it.
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .with_context(|| format!("fetching {tarball}"))?;
    if !resp.status().is_success() {
        bail!("{tarball} returned HTTP {}", resp.status().as_u16());
    }
    let mut bytes = Vec::new();
    {
        use std::io::Read as _;
        // CAPPED read. An unbounded copy from a remote body is how a fetch becomes a memory-exhaustion
        // vector, and the cap is checked by reading one byte MORE than allowed so exceeding it is
        // detectable rather than silently truncated into a hash mismatch.
        let mut limited = resp.by_ref().take(MAX_TARBALL + 1);
        limited
            .read_to_end(&mut bytes)
            .context("reading the tarball body")?;
    }
    if bytes.len() as u64 > MAX_TARBALL {
        bail!("the published catalog tarball is larger than {MAX_TARBALL} bytes");
    }
    integrity_matches(&bytes, &integrity)?;
    let text = extract_catalog(&bytes)?;

    // Staged beside the destination so `install_from_file` renames within one directory, and so a
    // rejected candidate never sits at the live path even briefly.
    let staged = data_dir.join("catalog").join("incoming.json");
    if let Some(parent) = staged.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&staged, text.as_bytes())
        .with_context(|| format!("writing {}", staged.display()))?;
    let baked = nub_sandbox::catalog_update::baked_generated_at();
    let outcome =
        nub_sandbox::catalog_update::install_from_file(&staged, data_dir, baked.as_deref());
    let _ = std::fs::remove_file(&staged);
    match outcome {
        nub_sandbox::catalog_update::Installed::Placed {
            path,
            generated_at,
            packages,
        } => Ok(format!(
            "build-jail catalog updated to {PACKAGE}@{version}: {path} \
             ({packages} packages, generated {generated_at})"
        )),
        nub_sandbox::catalog_update::Installed::Refused { reason } => {
            bail!("{PACKAGE}@{version} was not installed ({reason}) — nothing was changed.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ⛔ THE HOST ALLOWLIST IS THE ONLY THING STOPPING A PACKUMENT REDIRECTING THIS FETCH ANYWHERE, so it
    /// is tested by its refusals rather than its acceptance. `http` is refused even on an allowed host: a
    /// downgrade to cleartext would let a network attacker supply both the packument AND the tarball, which
    /// defeats the integrity check by substituting the expected hash too.
    #[test]
    fn only_https_registry_hosts_are_followed() {
        assert!(host_allowed(
            "https://registry.npmjs.org/@nubjs%2Fbuild-jail-catalog"
        ));
        assert!(host_allowed(
            "https://registry.npmmirror.com/x/-/x-1.0.0.tgz"
        ));
        for refused in [
            "http://registry.npmjs.org/x",
            "https://evil.test/x",
            "https://registry.npmjs.org.evil.test/x",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(!host_allowed(refused), "{refused} must not be followed");
        }
    }

    /// A hash comparison that cannot FAIL is not a verification. This flips one byte and requires the
    /// refusal, then requires the matching case to pass — the second half alone would be satisfied by a
    /// function that returns Ok unconditionally.
    #[test]
    fn integrity_is_checked_and_a_single_flipped_byte_is_refused() {
        use base64::Engine as _;
        use sha2::Digest as _;
        let body = b"the published bytes".to_vec();
        let good = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(sha2::Sha512::digest(&body))
        );
        integrity_matches(&body, &good).expect("the real bytes must verify");

        let mut tampered = body.clone();
        tampered[0] ^= 0x01;
        assert!(
            integrity_matches(&tampered, &good).is_err(),
            "a flipped byte must be refused"
        );
    }

    /// ⛔ AN UNRECOGNISED ALGORITHM IS A REFUSAL, NOT A FALLBACK. npm integrity strings can name other
    /// algorithms, and accepting whatever is offered would let a downgrade to a broken one through — the
    /// check would still "pass" while proving much less.
    #[test]
    fn a_non_sha512_integrity_is_refused_rather_than_accepted() {
        for weak in ["sha1-abc", "md5-abc", "abc", ""] {
            assert!(
                integrity_matches(b"x", weak).is_err(),
                "{weak:?} must be refused"
            );
        }
    }

    /// The catalog is found BY NAME inside the tarball, not by position. This packs a README FIRST so a
    /// "take the first entry" implementation fails, which is exactly the bug the name match prevents.
    #[test]
    fn the_catalog_is_found_by_name_even_behind_another_file() {
        use std::io::Write as _;
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            for (name, body) in [
                ("package/README.md", "not the catalog"),
                ("package/build-jail-catalog-v2.json", r#"{"packages":{}}"#),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, name, body.as_bytes())
                    .unwrap();
            }
            builder.finish().unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar_bytes).unwrap();
        let packed = gz.finish().unwrap();

        assert_eq!(extract_catalog(&packed).unwrap(), r#"{"packages":{}}"#);
    }

    /// ⛔ THE WHOLE PIPELINE BELOW THE HTTP HOP, on the REAL shipped catalog: verify integrity, extract by
    /// name, install through the validator, then require the READER to load what was installed. Without
    /// this the unit tests above each prove one link and nothing proves they compose — and the composition
    /// is where a mistake would be invisible, because every link would still pass alone.
    ///
    /// The network hop is not faked. It is covered by `only_https_registry_hosts_are_followed` and the
    /// integrity tests; standing up a fake registry would test a fake registry.
    #[test]
    fn the_pipeline_from_tarball_to_a_catalog_the_reader_loads() {
        use std::io::Write as _;
        // The catalog nub actually ships, stamped forward so it beats the compiled floor.
        let shipped = include_str!("../../../nub-sandbox/data/build-jail-catalog-v2.json");
        let mut doc: serde_json::Value = serde_json::from_str(shipped).unwrap();
        doc["provenance"]["generatedAt"] = serde_json::Value::from("2099-01-01T00:00:00Z");
        let text = serde_json::to_string(&doc).unwrap();

        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            // A sibling file FIRST, so a position-based extractor fails here.
            for (name, body) in [
                (
                    "package/package.json",
                    r#"{"name":"@nubjs/build-jail-catalog"}"#,
                ),
                ("package/build-jail-catalog-v2.json", text.as_str()),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, name, body.as_bytes())
                    .unwrap();
            }
            builder.finish().unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar_bytes).unwrap();
        let packed = gz.finish().unwrap();

        // Integrity, computed the way the registry would publish it.
        use base64::Engine as _;
        use sha2::Digest as _;
        let integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(sha2::Sha512::digest(&packed))
        );
        integrity_matches(&packed, &integrity).expect("our own bytes must verify");

        let extracted = extract_catalog(&packed).expect("the catalog must be found by name");
        assert_eq!(extracted, text, "extraction must be byte-exact");

        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let staged = data_dir.join("catalog").join("incoming.json");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, extracted.as_bytes()).unwrap();

        let placed = nub_sandbox::catalog_update::install_from_file(&staged, &data_dir, None);
        let live = match placed {
            nub_sandbox::catalog_update::Installed::Placed { path, packages, .. } => {
                assert!(
                    packages > 100,
                    "the shipped catalog has many packages, got {packages}"
                );
                path
            }
            other => panic!("expected Placed, got {other:?}"),
        };
        let (decision, _) =
            nub_sandbox::catalog_update::decide(std::path::Path::new(&live), None, |p| {
                std::fs::read_to_string(p)
            });
        assert!(
            matches!(
                decision,
                nub_sandbox::catalog_update::Decision::Loaded { .. }
            ),
            "the reader must LOAD what the pipeline installed, got {decision:?}"
        );
    }

    /// A tarball with no catalog is an error, not an empty string that would then be handed to the
    /// validator and rejected with a confusing schema message.
    #[test]
    fn a_tarball_without_a_catalog_is_an_error() {
        use std::io::Write as _;
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let body = b"nope";
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "package/README.md", &body[..])
                .unwrap();
            builder.finish().unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar_bytes).unwrap();
        let packed = gz.finish().unwrap();
        assert!(extract_catalog(&packed).is_err());
    }
}

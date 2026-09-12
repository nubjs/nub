//! Fetch a package's latest published tarball from the npm registry and extract
//! it to a scratch directory. Only the published tarball is analyzed — the same
//! bytes a consumer installs — never a git repo (which carries un-published
//! test/dev files).

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use reqwest::blocking::Client;

const REGISTRY: &str = "https://registry.npmjs.org";
/// npm's abbreviated-packument media type — the one `npm install` itself sends.
const ABBREVIATED_PACKUMENT: &str = "application/vnd.npm.install-v1+json";
/// Skip a tarball larger than this — a phantom scan does not need to page in a
/// half-gigabyte package, and it bounds a scan's disk/time.
const MAX_TARBALL_BYTES: u64 = 64 * 1024 * 1024;
/// Decompressed-size cap — the compressed cap can be evaded by a chunked response
/// or a high-ratio gzip bomb, so the unpacked stream is bounded independently.
const MAX_DECOMPRESSED_BYTES: u64 = 512 * 1024 * 1024;

/// A package extracted to a temp dir; the dir is removed on drop.
pub struct Extracted {
    /// The package root (the dir holding `package.json`).
    pub root: PathBuf,
    pub version: String,
    _tmp: TempDir,
}

/// GET with retry/backoff. The npm registry rate-limits (HTTP 429) under a wide
/// scan and returns transient 5xx; both are retried with exponential backoff that
/// honors a `Retry-After` header, so a scan does not silently drop half its
/// corpus. A 4xx other than 429 (e.g. an unpublished/renamed package's 404) is
/// terminal and returned immediately.
fn get_with_retry(
    client: &Client,
    url: &str,
    accept: &str,
) -> Result<reqwest::blocking::Response, String> {
    const MAX_ATTEMPTS: u32 = 9;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match client.get(url).header("accept", accept).send() {
            Ok(resp) => {
                let status = resp.status();
                let retryable = status.as_u16() == 429 || status.is_server_error();
                if !retryable {
                    return resp.error_for_status().map_err(|e| e.to_string());
                }
                if attempt >= MAX_ATTEMPTS {
                    return Err(format!("status {status} after {attempt} attempts"));
                }
                let wait = retry_after(&resp).unwrap_or_else(|| backoff(attempt));
                std::thread::sleep(wait);
            }
            Err(e) => {
                if attempt >= MAX_ATTEMPTS {
                    return Err(e.to_string());
                }
                std::thread::sleep(backoff(attempt));
            }
        }
    }
}

/// Exponential backoff with a cap: ~0.5s, 1s, 2s, 4s, 8s, 16s, then 30s.
fn backoff(attempt: u32) -> std::time::Duration {
    let secs = 2u64.saturating_pow(attempt.saturating_sub(1));
    std::time::Duration::from_millis((secs * 500).min(30_000))
}

/// Parse a `Retry-After` header (delta-seconds form, which the npm registry uses).
fn retry_after(resp: &reqwest::blocking::Response) -> Option<std::time::Duration> {
    let secs: u64 = resp
        .headers()
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(std::time::Duration::from_secs(secs.min(30)))
}

pub fn client() -> Client {
    Client::builder()
        .user_agent("nub-phantom/0.1 (+https://github.com/nubjs/nub)")
        .timeout(std::time::Duration::from_secs(45))
        .build()
        .expect("reqwest client build")
}

/// Fetch `name`'s latest version and extract its tarball. Errors are returned as
/// strings (a scan logs and skips rather than aborting the batch).
pub fn fetch(client: &Client, name: &str) -> Result<Extracted, String> {
    // The ABBREVIATED packument, not `/{name}/latest`. Measured 2026-09-05: under
    // a wide scan `registry.npmjs.org/<pkg>/latest` rate-limits to a hard 429 that
    // no backoff clears — five consecutive probes returned 429 for `/latest` while
    // the abbreviated document returned 200 for the same package on the same
    // connection. The abbreviated form is the endpoint npm's own installer uses,
    // so it is the best-cached path on the registry CDN and is not throttled the
    // same way. It carries every field this fetcher needs (`dist-tags.latest` and
    // that version's `dist.tarball`); the manifest itself is read out of the
    // tarball, never from metadata, so the abbreviated document's dropped fields
    // (`exports`, `scripts`, …) cost nothing here.
    let meta_url = format!("{REGISTRY}/{name}");
    let meta: serde_json::Value = get_with_retry(client, &meta_url, ABBREVIATED_PACKUMENT)
        .map_err(|e| format!("metadata: {e}"))?
        .json()
        .map_err(|e| format!("metadata parse: {e}"))?;

    let version = meta
        .get("dist-tags")
        .and_then(|t| t.get("latest"))
        .and_then(|v| v.as_str())
        .ok_or("no version in metadata")?
        .to_string();
    let tarball = meta
        .get("versions")
        .and_then(|vs| vs.get(&version))
        .and_then(|v| v.get("dist"))
        .and_then(|d| d.get("tarball"))
        .and_then(|t| t.as_str())
        .ok_or("no tarball url in metadata")?;

    let resp = get_with_retry(client, tarball, "application/octet-stream")
        .map_err(|e| format!("tarball: {e}"))?;
    if let Some(len) = resp.content_length()
        && len > MAX_TARBALL_BYTES
    {
        return Err(format!("tarball too large ({len} bytes)"));
    }
    let gz = resp.bytes().map_err(|e| format!("tarball read: {e}"))?;

    let tmp = TempDir::new(name);
    fs::create_dir_all(&tmp.0).map_err(|e| format!("mkdir tmp: {e}"))?;

    // Decode gzip then untar. The tarball body is gzip (a `.tgz`), independent of
    // any HTTP transfer encoding. The DECOMPRESSED size is capped (a chunked
    // response can skip the content-length check above; and a gzip bomb would
    // otherwise OOM) via a bounded reader.
    let mut decoder = GzDecoder::new(&gz[..]).take(MAX_DECOMPRESSED_BYTES + 1);
    let mut raw = Vec::new();
    decoder
        .read_to_end(&mut raw)
        .map_err(|e| format!("gunzip: {e}"))?;
    if raw.len() as u64 > MAX_DECOMPRESSED_BYTES {
        return Err("decompressed tarball too large".to_string());
    }
    let mut archive = tar::Archive::new(&raw[..]);
    // `tar` 0.4's `unpack` sanitizes `..`/absolute members, so extraction cannot
    // escape `tmp` — we rely on that built-in traversal protection.
    archive.unpack(&tmp.0).map_err(|e| format!("untar: {e}"))?;

    let root = locate_package_root(&tmp.0);
    if !root.join("package.json").is_file() {
        return Err("no package.json in tarball".to_string());
    }
    Ok(Extracted {
        root,
        version,
        _tmp: tmp,
    })
}

/// npm tarballs conventionally nest under `package/`, but some legacy tarballs use
/// a different single top directory. Pick the dir containing `package.json`.
fn locate_package_root(tmp: &Path) -> PathBuf {
    let conventional = tmp.join("package");
    if conventional.join("package.json").is_file() {
        return conventional;
    }
    if let Ok(entries) = fs::read_dir(tmp) {
        let dirs: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        if let [only] = dirs.as_slice()
            && only.join("package.json").is_file()
        {
            return only.clone();
        }
    }
    tmp.to_path_buf()
}

/// A scratch directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        let slug: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        TempDir(std::env::temp_dir().join(format!(
            "nub-phantom-{slug}-{}-{unique}",
            std::process::id()
        )))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Fetch a download-ranked package-name list from the `npm-high-impact` dataset
/// (Sindre Sorhus'). The most-downloaded ranking lives in `lib/top-download.js`
/// as `export const topDownload = ['semver', 'minimatch', …]` (a formatted JS
/// array, not JSON). Returns the first `n` names. The default corpus for
/// `scan --top N`.
pub fn top_packages(client: &Client, n: usize) -> Result<Vec<String>, String> {
    let extracted = fetch(client, "npm-high-impact")?;
    let src = fs::read_to_string(extracted.root.join("lib/top-download.js"))
        .map_err(|e| format!("read npm-high-impact top-download.js: {e}"))?;
    let names = extract_quoted(&src);
    if names.is_empty() {
        return Err("no package names parsed from npm-high-impact".to_string());
    }
    Ok(names.into_iter().take(n).collect())
}

/// Pull every single-quoted string literal from the source. The file's only
/// single-quoted tokens are the array's package names (the surrounding `export
/// const … = [ … ]` carries none), so this recovers the ranked list without a JS
/// parser and without depending on the prettier line formatting.
fn extract_quoted(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = src.chars();
    while let Some(c) = chars.next() {
        if c == '\'' {
            let mut name = String::new();
            for c2 in chars.by_ref() {
                if c2 == '\'' {
                    break;
                }
                name.push(c2);
            }
            if !name.is_empty() {
                out.push(name);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn extract_quoted_recovers_names() {
        let src = "export const topDownload = [\n  'semver',\n  '@babel/core',\n  'ms',\n]";
        assert_eq!(
            super::extract_quoted(src),
            vec!["semver", "@babel/core", "ms"]
        );
    }
}

//! Supply-chain checks for `aube add`.
//!
//! Two probes against public APIs run before any new spec lands in
//! `package.json`:
//!
//! - [`fetch_malicious_advisories`] batch-queries `api.osv.dev` for
//!   `MAL-*` advisories. A hit is a confirmed-malicious package — the
//!   caller refuses the add with `ERR_AUBE_MALICIOUS_PACKAGE`.
//! - [`fetch_weekly_downloads`] looks up a package's `last-week`
//!   download count via `api.npmjs.org`. Typosquats and impersonations
//!   have near-zero downloads on day one regardless of how cleverly
//!   they're named, so a download floor catches the long tail of
//!   reported-after-the-fact malicious names.
//!
//! Both probes target public hosts and use their own reqwest client
//! rather than [`crate::client::RegistryClient`] — they don't need
//! the registry's auth/scoped-route logic, and isolating them keeps
//! the OSV failure mode (fail-open with a warning) from interacting
//! with packument fetch retries.

use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

/// HTTP timeout applied to every supply-chain probe. Keep tight: these
/// are non-critical gates on the human-intent path of `aube add`, and
/// a slow OSV mirror shouldn't add minutes of perceived latency to an
/// otherwise local operation.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// Public host for OSV's batch-query endpoint.
const OSV_ENDPOINT: &str = "https://api.osv.dev/v1/querybatch";

/// Public host for OSV's vulnerability detail endpoint.
const OSV_VULN_BASE: &str = "https://api.osv.dev/v1/vulns";

/// Max queries per OSV `/querybatch` request. OSV's documented
/// limit is 1000; staying well under that lets transitive-graph
/// checks (hundreds to low thousands of distinct names on a
/// large monorepo) chunk without truncating. Sequential chunks
/// share one client so the connection pool stays warm.
const OSV_BATCH_LIMIT: usize = 500;

/// Public host for npm's downloads API. The `point/last-week/{pkg}`
/// route returns one integer per request — cheap and rate-limit
/// friendly compared to the `range` endpoint.
const NPM_DOWNLOADS_BASE: &str = "https://api.npmjs.org/downloads/point/last-week";

/// One malicious-package advisory hit. We surface the OSV id and the
/// candidate package name; the caller composes a link of the form
/// `https://osv.dev/vulnerability/{id}`.
///
/// `version` is populated by the post-resolve probes
/// ([`fetch_malicious_advisories_versioned`] and
/// [`crate::osv_mirror::OsvMirror::lookup_advisories_versioned`])
/// where each name is paired with a resolved version. The pre-resolve
/// `aube add` name-gate leaves it `None` because no resolver has run
/// yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaliciousAdvisory {
    pub package: String,
    pub advisory_id: String,
    pub version: Option<String>,
}

/// Errors raised by the supply-chain probes. Distinct from
/// [`crate::Error`] so callers can react differently to fail-open vs
/// fail-closed paths without parsing the inner reqwest error chain.
#[derive(Debug, thiserror::Error)]
pub enum SupplyChainError {
    #[error("supply-chain probe HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("supply-chain probe JSON decode failed: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("supply-chain probe returned non-success status: {0}")]
    Status(reqwest::StatusCode),
    /// OSV's batch endpoint contract guarantees one `results[i]` per
    /// `queries[i]`. A short response means a trailing subset of
    /// candidate names was never actually checked — silently
    /// treating that as "no advisories" would let a known-malicious
    /// package slip through on a truncated reply. The caller
    /// surfaces this as a probe failure so the configured
    /// fail-open/fail-closed policy applies.
    #[error("OSV returned {got} results for {expected} queries — truncated response")]
    TruncatedOsvResponse { expected: usize, got: usize },
}

#[derive(Debug, serde::Serialize)]
struct OsvQuery<'a> {
    package: OsvPackage<'a>,
    /// Resolved version. Omitted (`None`) by the pre-resolve
    /// `aube add` name-gate; populated by the post-resolve
    /// transitive gate so OSV can filter advisories down to those
    /// that actually affect the version the resolver picked.
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
}

#[derive(Debug, serde::Serialize)]
struct OsvPackage<'a> {
    name: &'a str,
    ecosystem: &'a str,
}

#[derive(Debug, serde::Serialize)]
struct OsvBatchRequest<'a> {
    queries: Vec<OsvQuery<'a>>,
}

#[derive(Debug, Deserialize, Default)]
struct OsvBatchResponse {
    #[serde(default)]
    results: Vec<OsvResult>,
}

#[derive(Debug, Deserialize, Default)]
struct OsvResult {
    #[serde(default)]
    vulns: Vec<OsvVuln>,
}

#[derive(Debug, Deserialize)]
struct OsvVuln {
    id: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct OsvVulnDetails {
    #[serde(default)]
    affected: Vec<OsvAffected>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct OsvAffected {
    #[serde(default)]
    package: OsvAffectedPackage,
    #[serde(default)]
    versions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct OsvAffectedPackage {
    #[serde(default)]
    name: String,
    #[serde(default)]
    ecosystem: String,
}

#[derive(Debug, Deserialize)]
struct NpmDownloadsResponse {
    /// `point/last-week/<pkg>` returns this field on success; the
    /// `error` branch (scoped packages, unknown names) omits it.
    #[serde(default)]
    downloads: Option<u64>,
    /// Present when the registry returns a soft error rather than a
    /// non-2xx — typically `"package @scope/name not found"` for
    /// scoped packages, which the downloads API doesn't support.
    #[serde(default)]
    error: Option<String>,
}

/// Build the shared probe `reqwest::Client`. Centralized so the OSV
/// and downloads probes use identical timeout / TLS settings and so
/// `aube add a b c` can reuse a single client + connection pool
/// across all per-package downloads requests.
pub fn build_probe_client() -> Result<reqwest::Client, SupplyChainError> {
    Ok(
        aube_util::http::with_webpki_root_fallback(reqwest::Client::builder())
            .timeout(PROBE_TIMEOUT)
            .build()?,
    )
}

/// Probe OSV for `MAL-*` advisories on every candidate against a
/// caller-supplied shared client. Versions are intentionally
/// omitted from the query: typosquats and impersonation packages
/// are usually malicious in every published version, and we
/// haven't run the resolver yet when this fires.
///
/// Use [`fetch_malicious_advisories_versioned`] for post-resolve
/// transitive checks where each name has a resolved version —
/// otherwise a version-specific compromise (e.g. the Sep 2025
/// shai-hulud worm that affected `ansi-regex@6.2.1` only) would
/// collapse into a name-level block of every published release.
///
/// Returns the subset of input names that hit a `MAL-*` advisory.
/// An `Err` is a fetch / decode / truncated-response failure — the
/// caller decides whether to surface it (`advisoryCheck=required`)
/// or warn-and-continue (`advisoryCheck=on`).
///
/// Mirrors [`fetch_weekly_downloads_with`]: the gate caller builds
/// one [`build_probe_client`] up front and threads it through both
/// probes so the OSV → downloads sequence reuses the same connection
/// pool across all per-package requests.
pub async fn fetch_malicious_advisories(
    client: &reqwest::Client,
    names: &[String],
) -> Result<Vec<MaliciousAdvisory>, SupplyChainError> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let mut hits = Vec::new();
    for chunk in names.chunks(OSV_BATCH_LIMIT) {
        hits.extend(fetch_malicious_advisories_chunk(client, chunk).await?);
    }
    Ok(hits)
}

/// Version-aware sibling of [`fetch_malicious_advisories`] for the
/// post-resolve transitive gate. Each pair becomes an OSV query that
/// includes both `package.name` and `version`, so a `MAL-*` advisory
/// only surfaces when it actually covers the resolved version.
///
/// Why this matters: the Sep 2025 shai-hulud worm compromised
/// specific versions of widely-used names (`ansi-regex@6.2.1`,
/// `strip-ansi@7.1.1`, etc.). The name-only query treats every
/// release of those packages as malicious, which transitively
/// breaks every install that pulls in an older, untouched version
/// of the same name. Including the version flips OSV's filter on
/// so only the actually-compromised pair fires.
pub async fn fetch_malicious_advisories_versioned(
    client: &reqwest::Client,
    pairs: &[(String, String)],
) -> Result<Vec<MaliciousAdvisory>, SupplyChainError> {
    if pairs.is_empty() {
        return Ok(Vec::new());
    }
    let mut hits = Vec::new();
    for chunk in pairs.chunks(OSV_BATCH_LIMIT) {
        hits.extend(fetch_malicious_advisories_versioned_chunk(client, chunk).await?);
    }
    filter_malicious_versioned_hits(client, hits).await
}

async fn fetch_malicious_advisories_chunk(
    client: &reqwest::Client,
    names: &[String],
) -> Result<Vec<MaliciousAdvisory>, SupplyChainError> {
    let body = OsvBatchRequest {
        queries: names
            .iter()
            .map(|n| OsvQuery {
                package: OsvPackage {
                    name: n.as_str(),
                    ecosystem: "npm",
                },
                version: None,
            })
            .collect(),
    };
    let parsed = post_osv_batch(client, &body, names.len()).await?;
    Ok(extract_malicious(names, &parsed))
}

async fn fetch_malicious_advisories_versioned_chunk(
    client: &reqwest::Client,
    pairs: &[(String, String)],
) -> Result<Vec<MaliciousAdvisory>, SupplyChainError> {
    let body = OsvBatchRequest {
        queries: pairs
            .iter()
            .map(|(name, version)| OsvQuery {
                package: OsvPackage {
                    name: name.as_str(),
                    ecosystem: "npm",
                },
                version: Some(version.as_str()),
            })
            .collect(),
    };
    let parsed = post_osv_batch(client, &body, pairs.len()).await?;
    Ok(extract_malicious_versioned(pairs, &parsed))
}

async fn post_osv_batch(
    client: &reqwest::Client,
    body: &OsvBatchRequest<'_>,
    expected: usize,
) -> Result<OsvBatchResponse, SupplyChainError> {
    let resp = client.post(OSV_ENDPOINT).json(body).send().await?;
    if !resp.status().is_success() {
        return Err(SupplyChainError::Status(resp.status()));
    }
    let bytes = resp.bytes().await?;
    let parsed: OsvBatchResponse = serde_json::from_slice(&bytes)?;
    // Enforce the OSV `results[i] ↔ queries[i]` parity contract.
    // A short response is treated as a probe failure (not "no
    // advisories") so the trailing entries aren't silently skipped —
    // the `advisoryCheck` policy then decides whether to warn-and-
    // continue or fail closed.
    if parsed.results.len() != expected {
        return Err(SupplyChainError::TruncatedOsvResponse {
            expected,
            got: parsed.results.len(),
        });
    }
    Ok(parsed)
}

fn extract_malicious(names: &[String], resp: &OsvBatchResponse) -> Vec<MaliciousAdvisory> {
    // Caller (`fetch_malicious_advisories`) has already validated
    // `names.len() == resp.results.len()` and bailed otherwise, so
    // the zip below is safe — every input name has a corresponding
    // result slot. Tests call this helper directly with hand-built
    // responses; those happen to pass matched-length slices, so no
    // runtime check is needed here.
    let mut hits = Vec::new();
    for (name, result) in names.iter().zip(resp.results.iter()) {
        for vuln in &result.vulns {
            if vuln.id.starts_with("MAL-") {
                hits.push(MaliciousAdvisory {
                    package: name.clone(),
                    advisory_id: vuln.id.clone(),
                    version: None,
                });
            }
        }
    }
    hits
}

fn extract_malicious_versioned(
    pairs: &[(String, String)],
    resp: &OsvBatchResponse,
) -> Vec<MaliciousAdvisory> {
    let mut hits = Vec::new();
    for ((name, version), result) in pairs.iter().zip(resp.results.iter()) {
        for vuln in &result.vulns {
            if vuln.id.starts_with("MAL-") {
                hits.push(MaliciousAdvisory {
                    package: name.clone(),
                    advisory_id: vuln.id.clone(),
                    version: Some(version.clone()),
                });
            }
        }
    }
    hits
}

async fn filter_malicious_versioned_hits(
    client: &reqwest::Client,
    hits: Vec<MaliciousAdvisory>,
) -> Result<Vec<MaliciousAdvisory>, SupplyChainError> {
    let mut details = HashMap::new();
    let mut tasks = tokio::task::JoinSet::new();
    for hit in &hits {
        if hit.version.is_none() || details.contains_key(&hit.advisory_id) {
            continue;
        }
        details.insert(hit.advisory_id.clone(), None);
        let client = client.clone();
        let advisory_id = hit.advisory_id.clone();
        tasks.spawn(async move {
            let details = fetch_osv_vuln_details(&client, &advisory_id).await.ok();
            (advisory_id, details)
        });
    }
    while let Some(joined) = tasks.join_next().await {
        if let Ok((advisory_id, fetched)) = joined {
            details.insert(advisory_id, fetched);
        }
    }

    let mut filtered = Vec::new();
    for hit in hits {
        let affects = details
            .get(&hit.advisory_id)
            .and_then(Option::as_ref)
            .is_none_or(|d| {
                let Some(version) = hit.version.as_deref() else {
                    return true;
                };
                versioned_hit_affects_resolved_version(d, &hit.package, version)
            });
        if affects {
            filtered.push(hit);
        }
    }
    Ok(filtered)
}

async fn fetch_osv_vuln_details(
    client: &reqwest::Client,
    advisory_id: &str,
) -> Result<OsvVulnDetails, SupplyChainError> {
    let url = format!("{OSV_VULN_BASE}/{advisory_id}");
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        return Err(SupplyChainError::Status(resp.status()));
    }
    let bytes = resp.bytes().await?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn versioned_hit_affects_resolved_version(
    details: &OsvVulnDetails,
    package: &str,
    version: &str,
) -> bool {
    let mut matched_package = false;
    for affected in &details.affected {
        if !affected.package.ecosystem.eq_ignore_ascii_case("npm")
            || affected.package.name != package
        {
            continue;
        }
        matched_package = true;
        if affected.versions.is_empty() || affected.versions.iter().any(|v| v == version) {
            return true;
        }
    }
    !matched_package
}

/// Lookup result for a single package on npm's downloads API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadCount {
    /// Weekly downloads reported by the API.
    Known(u64),
    /// The API doesn't have data for this name. Common cases: scoped
    /// packages (`@scope/name`), brand-new packages with no published
    /// version, registry mirrors that don't proxy `api.npmjs.org`.
    /// Callers should treat this as "no signal" — skip the gate
    /// rather than fail closed, since absence of data is not
    /// evidence of typosquat.
    Unknown,
}

/// Look up `name`'s weekly download count using a caller-supplied
/// shared client. The caller is expected to reuse one
/// [`build_probe_client`] across every probe in an invocation so
/// the reqwest connection pool stays warm — see
/// `crates/aube/src/commands/add_supply_chain.rs::downloads_gate`.
pub async fn fetch_weekly_downloads_with(
    client: &reqwest::Client,
    name: &str,
) -> Result<DownloadCount, SupplyChainError> {
    // Scoped names contain `/` which must be percent-encoded for the
    // path segment. We still fire the request — npm returns a 404
    // with a JSON `error` body that the parse step recognizes.
    let encoded = name.replace('/', "%2F");
    let url = format!("{NPM_DOWNLOADS_BASE}/{encoded}");
    let resp = client.get(&url).send().await?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(DownloadCount::Unknown);
    }
    if !status.is_success() {
        return Err(SupplyChainError::Status(status));
    }
    let bytes = resp.bytes().await?;
    let parsed: NpmDownloadsResponse = serde_json::from_slice(&bytes)?;
    Ok(parse_downloads(&parsed))
}

fn parse_downloads(resp: &NpmDownloadsResponse) -> DownloadCount {
    if resp.error.is_some() {
        return DownloadCount::Unknown;
    }
    match resp.downloads {
        Some(n) => DownloadCount::Known(n),
        None => DownloadCount::Unknown,
    }
}

/// `https://osv.dev/vulnerability/<id>` — the user-facing URL for an
/// advisory id surfaced by [`fetch_malicious_advisories`]. Centralized
/// so the format stays consistent across the warn and error sites.
pub fn advisory_url(advisory_id: &str) -> String {
    format!("https://osv.dev/vulnerability/{advisory_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_malicious_filters_non_mal_ids() {
        // OSV returns GHSA-*/CVE-* alongside MAL-*; only MAL-* should
        // make it through this filter. Audit-class advisories belong
        // to `aube audit`, not the add-time block.
        let names = vec!["evil-pkg".to_string(), "fine-pkg".to_string()];
        let resp = OsvBatchResponse {
            results: vec![
                OsvResult {
                    vulns: vec![
                        OsvVuln {
                            id: "MAL-2026-3652".to_string(),
                        },
                        OsvVuln {
                            id: "GHSA-xxxx".to_string(),
                        },
                    ],
                },
                OsvResult {
                    vulns: vec![OsvVuln {
                        id: "CVE-2024-9999".to_string(),
                    }],
                },
            ],
        };
        let hits = extract_malicious(&names, &resp);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].package, "evil-pkg");
        assert_eq!(hits[0].advisory_id, "MAL-2026-3652");
        assert_eq!(hits[0].version, None);
    }

    #[test]
    fn extract_malicious_versioned_carries_resolved_version() {
        // The versioned path pairs each query with the resolved
        // version, so callers can show `name@version` in the refusal
        // message and reason about per-version compromise (e.g. the
        // ansi-regex@6.2.1-only shai-hulud advisory).
        let pairs = vec![("ansi-regex".to_string(), "6.2.1".to_string())];
        let resp = OsvBatchResponse {
            results: vec![OsvResult {
                vulns: vec![OsvVuln {
                    id: "MAL-2025-46966".to_string(),
                }],
            }],
        };
        let hits = extract_malicious_versioned(&pairs, &resp);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].package, "ansi-regex");
        assert_eq!(hits[0].version.as_deref(), Some("6.2.1"));
        assert_eq!(hits[0].advisory_id, "MAL-2025-46966");
    }

    #[test]
    fn versioned_hit_prefers_explicit_affected_versions_over_broad_range() {
        let details = OsvVulnDetails {
            affected: vec![OsvAffected {
                package: OsvAffectedPackage {
                    name: "@mistralai/mistralai".to_string(),
                    ecosystem: "npm".to_string(),
                },
                versions: vec![
                    "2.2.4".to_string(),
                    "2.2.3".to_string(),
                    "2.2.2".to_string(),
                ],
            }],
        };

        assert!(!versioned_hit_affects_resolved_version(
            &details,
            "@mistralai/mistralai",
            "2.2.1",
        ));
        assert!(versioned_hit_affects_resolved_version(
            &details,
            "@mistralai/mistralai",
            "2.2.2",
        ));
    }

    #[test]
    fn versioned_hit_without_explicit_versions_still_blocks() {
        let details = OsvVulnDetails {
            affected: vec![OsvAffected {
                package: OsvAffectedPackage {
                    name: "evil-pkg".to_string(),
                    ecosystem: "npm".to_string(),
                },
                versions: Vec::new(),
            }],
        };

        assert!(versioned_hit_affects_resolved_version(
            &details, "evil-pkg", "1.0.0",
        ));
    }

    #[test]
    fn versioned_hit_without_matching_package_still_blocks() {
        let details = OsvVulnDetails {
            affected: vec![OsvAffected {
                package: OsvAffectedPackage {
                    name: "evil-pkg".to_string(),
                    ecosystem: "PyPI".to_string(),
                },
                versions: vec!["1.0.0".to_string()],
            }],
        };

        assert!(versioned_hit_affects_resolved_version(
            &details, "evil-pkg", "1.0.0",
        ));
    }

    #[test]
    fn truncated_osv_response_carries_lengths_in_error() {
        // `fetch_malicious_advisories` rejects a short response
        // rather than silently zipping the prefix — a missing
        // `results[i]` would otherwise let the corresponding query's
        // package skip the malicious-advisory gate entirely. The
        // error carries both expected and actual lengths so the
        // operator-facing log message names the gap.
        let err = SupplyChainError::TruncatedOsvResponse {
            expected: 3,
            got: 1,
        };
        let rendered = err.to_string();
        assert!(rendered.contains("3"), "expected count missing: {rendered}");
        assert!(rendered.contains("1"), "got count missing: {rendered}");
        assert!(
            rendered.contains("truncated"),
            "category word missing: {rendered}"
        );
    }

    #[test]
    fn parse_downloads_treats_error_body_as_unknown() {
        // Scoped packages return 200 with `{"error": "package
        // @scope/name not found"}`. We need that to fold into
        // `Unknown` so callers don't trip the low-download gate
        // on every scoped install.
        let resp = NpmDownloadsResponse {
            downloads: None,
            error: Some("package @scope/name not found".to_string()),
        };
        assert_eq!(parse_downloads(&resp), DownloadCount::Unknown);
    }

    #[test]
    fn parse_downloads_reads_known_count() {
        let resp = NpmDownloadsResponse {
            downloads: Some(42_000_000),
            error: None,
        };
        assert_eq!(parse_downloads(&resp), DownloadCount::Known(42_000_000));
    }

    #[test]
    fn advisory_url_uses_osv_domain() {
        assert_eq!(
            advisory_url("MAL-2026-3652"),
            "https://osv.dev/vulnerability/MAL-2026-3652"
        );
    }
}

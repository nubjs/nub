//! Speculative parallel GET: issue N concurrent requests for the same
//! resource and return the first successful response, aborting the
//! rest. Trades extra bandwidth for tail-latency reduction on flaky
//! networks where one of `N` mirror URLs lands fast.
//!
//! Useful when a packument or tarball is mirrored across multiple
//! CDN edges (e.g. Cloudflare anycast IP variants for
//! `registry.npmjs.org`) and the slowest path dominates a sequential
//! fallback. A 2-3 way race against the same Cloudflare zone is
//! near-free because the request hits the same edge cache.
//!
//! `AUBE_DISABLE_REQUEST_RACING=1` collapses to a single-URL fallback
//! (the first URL in the list) so a debugging session can isolate
//! per-mirror behaviour without changing call sites.

use std::time::Duration;

const DEFAULT_RACE_TIMEOUT: Duration = Duration::from_secs(10);

/// Returns true when speculative request racing is disabled.
#[inline]
pub fn is_disabled() -> bool {
    crate::env::embedder_env("DISABLE_REQUEST_RACING").is_some()
}

/// Race the given `(client, url)` candidates in parallel. The first
/// 2xx response wins; the rest abort. Returns the winning response.
///
/// `Err(RaceError::AllFailed)` collects every candidate's failure for
/// the diagnostic chain — a single upstream outage that fails all
/// mirrors should still surface a useful error rather than just the
/// last one polled.
pub async fn race_get<I>(targets: I) -> Result<reqwest::Response, RaceError>
where
    I: IntoIterator<Item = (reqwest::Client, String)>,
{
    let candidates: Vec<(reqwest::Client, String)> = targets.into_iter().collect();
    if candidates.is_empty() {
        return Err(RaceError::Empty);
    }
    if is_disabled() || candidates.len() == 1 {
        // Disabled or a one-url race is just a normal GET; skip the
        // join-set scaffolding so the killswitch path stays cheap.
        let (client, url) = candidates.into_iter().next().expect("len >= 1");
        return client
            .get(&url)
            .timeout(DEFAULT_RACE_TIMEOUT)
            .send()
            .await
            .map_err(|e| RaceError::single(url, e));
    }

    let mut joinset: tokio::task::JoinSet<Result<reqwest::Response, (String, reqwest::Error)>> =
        tokio::task::JoinSet::new();
    for (client, url) in candidates {
        let url_for_err = url.clone();
        joinset.spawn(async move {
            client
                .get(&url)
                .timeout(DEFAULT_RACE_TIMEOUT)
                .send()
                .await
                .map_err(|e| (url_for_err, e))
        });
    }

    // Both transport errors and intermediate non-2xx responses fold
    // into the same failure list. Dropping a non-2xx (e.g. 401 from
    // a misconfigured-token candidate) when sibling tasks fail with
    // transport errors would hide the actual root cause from the
    // caller's diagnostic chain.
    let mut failures: Vec<CandidateFailure> = Vec::new();
    while let Some(joined) = joinset.join_next().await {
        match joined {
            Ok(Ok(resp)) if resp.status().is_success() => {
                joinset.abort_all();
                return Ok(resp);
            }
            Ok(Ok(resp)) => {
                let status = resp.status();
                let url = resp.url().to_string();
                tracing::debug!(status = %status, url = %url, "race candidate non-2xx");
                failures.push(CandidateFailure::NonSuccess { url, status });
            }
            Ok(Err((url, source))) => failures.push(CandidateFailure::Transport { url, source }),
            Err(join_err) => {
                tracing::debug!(error = %join_err, "race candidate task panicked");
            }
        }
    }
    Err(RaceError::AllFailed(failures))
}

/// One candidate's failure shape — transport error or non-2xx HTTP
/// status. `race_get` collects every candidate's failure under
/// `RaceError::AllFailed` so the caller sees all of them, not just
/// the last one polled.
#[derive(Debug, thiserror::Error)]
pub enum CandidateFailure {
    #[error("{url} transport failure: {source}")]
    Transport {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("{url} returned {status}")]
    NonSuccess {
        url: String,
        status: reqwest::StatusCode,
    },
}

/// Errors that can surface from `race_get`.
#[derive(Debug, thiserror::Error)]
pub enum RaceError {
    #[error("no candidates supplied to race_get")]
    Empty,
    #[error("{url} failed: {source}")]
    Single {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("all {} candidates failed (first: {})", .0.len(), .0.first().map(|f| f.to_string()).unwrap_or_default())]
    AllFailed(Vec<CandidateFailure>),
}

impl RaceError {
    fn single(url: String, source: reqwest::Error) -> Self {
        Self::Single { url, source }
    }
}

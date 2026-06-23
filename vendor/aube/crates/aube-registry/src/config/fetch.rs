/// Resolved values for the five `fetch*` settings declared in
/// `settings.toml`. Kept separate from [`NpmConfig`] because these are
/// generic pnpm settings (sourced by the settings resolver, not the
/// registry-client-specific `.npmrc` parser in [`NpmConfig::apply`]) and
/// because wiring them through a single struct keeps the retry helper
/// on [`crate::client::RegistryClient`] from growing five parameters.
///
/// All durations are stored in milliseconds to match pnpm / npm's
/// `.npmrc` conventions; callers convert to [`std::time::Duration`] at
/// the reqwest boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchPolicy {
    /// `fetchTimeout` — per-request HTTP timeout. Applied via
    /// `reqwest::ClientBuilder::timeout` so it covers the whole
    /// response (headers + body).
    pub timeout_ms: u64,
    /// `fetchRetries` — number of *additional* attempts on transient
    /// failure. `retries = 2` means up to 3 total attempts, matching
    /// pnpm / `make-fetch-happen`.
    pub retries: u32,
    /// `fetchRetryFactor` — exponential backoff factor. Attempt `n`
    /// waits `min(mintimeout * factor^n, maxtimeout)` ms before retry.
    pub retry_factor: u32,
    /// `fetchRetryMintimeout` — lower bound on the computed backoff.
    pub retry_min_timeout_ms: u64,
    /// `fetchRetryMaxtimeout` — upper bound on the computed backoff.
    pub retry_max_timeout_ms: u64,
    /// `fetchWarnTimeoutMs` — observability threshold: emit a warning
    /// when a *metadata* request (packument, dist-tags) takes longer
    /// than this to receive a response. Does not fail the request; the
    /// hard cut-off is still [`Self::timeout_ms`]. `0` disables the
    /// warning, matching pnpm's convention for "unset observability".
    pub warn_timeout_ms: u64,
    /// `fetchMinSpeedKiBps` — observability threshold: emit a warning
    /// when a tarball finishes downloading with an average speed below
    /// this value (KiB/s). `0` disables the warning. As with
    /// `warn_timeout_ms`, we only warn — we never abort the transfer.
    pub min_speed_kibps: u64,
    /// `packumentMaxBytes` — hard cap on a packument response body.
    /// Primarily a hardening knob against hostile or misconfigured
    /// registries. `0` disables the cap entirely (not recommended for
    /// untrusted registries).
    pub packument_max_bytes: u64,
    /// `tarballMaxBytes` — hard cap on a tarball response body
    /// (on-wire, still compressed). Same hardening role as
    /// `packument_max_bytes`; `0` disables.
    pub tarball_max_bytes: u64,
}

impl Default for FetchPolicy {
    /// Matches the declared defaults in `settings.toml` (and npm / pnpm
    /// defaults). Callers that skip [`FetchPolicy::from_ctx`] still get
    /// sensible retry + timeout behavior.
    fn default() -> Self {
        Self {
            timeout_ms: 300_000,
            retries: 2,
            retry_factor: 10,
            retry_min_timeout_ms: 10_000,
            retry_max_timeout_ms: 60_000,
            warn_timeout_ms: 10_000,
            min_speed_kibps: 50,
            // Defaults match `settings.toml`.
            packument_max_bytes: 200 << 20,
            tarball_max_bytes: 1 << 30,
        }
    }
}

impl FetchPolicy {
    /// Resolve every field from a settings [`ResolveCtx`]. Walks the
    /// full cli > env > {project,user} aubeConfig/npmrc > workspaceYaml
    /// precedence chain via the generated accessors, so env-var
    /// overrides like `NPM_CONFIG_FETCH_TIMEOUT` Just Work without
    /// bespoke parsing.
    pub fn from_ctx(ctx: &aube_settings::ResolveCtx<'_>) -> Self {
        Self {
            timeout_ms: aube_settings::resolved::fetch_timeout(ctx),
            retries: clamp_u32(aube_settings::resolved::fetch_retries(ctx)),
            retry_factor: clamp_u32(aube_settings::resolved::fetch_retry_factor(ctx)),
            retry_min_timeout_ms: aube_settings::resolved::fetch_retry_mintimeout(ctx),
            retry_max_timeout_ms: aube_settings::resolved::fetch_retry_maxtimeout(ctx),
            warn_timeout_ms: aube_settings::resolved::fetch_warn_timeout_ms(ctx),
            min_speed_kibps: aube_settings::resolved::fetch_min_speed_ki_bps(ctx),
            packument_max_bytes: aube_settings::resolved::packument_max_bytes(ctx),
            tarball_max_bytes: aube_settings::resolved::tarball_max_bytes(ctx),
        }
    }

    /// Compute the sleep duration before the given retry attempt
    /// (1-indexed: `attempt=1` is the wait before the *second* HTTP
    /// request, i.e. the first retry). Clamped into
    /// `[retry_min_timeout_ms, retry_max_timeout_ms]`.
    ///
    /// Algorithm mirrors `make-fetch-happen`'s exponential backoff:
    /// `min(mintimeout * factor^(attempt-1), maxtimeout)`. Arithmetic
    /// uses saturating math so huge `factor` values don't panic on
    /// overflow — they just get clamped to the max.
    pub fn backoff_for_attempt(&self, attempt: u32) -> std::time::Duration {
        let attempt = attempt.max(1);
        let factor = u64::from(self.retry_factor.max(1));
        let exp = attempt.saturating_sub(1);
        let mut wait = self.retry_min_timeout_ms;
        for _ in 0..exp {
            wait = wait.saturating_mul(factor);
            if wait >= self.retry_max_timeout_ms {
                wait = self.retry_max_timeout_ms;
                break;
            }
        }
        let clamped = wait
            .max(self.retry_min_timeout_ms)
            .min(self.retry_max_timeout_ms);
        std::time::Duration::from_millis(clamped)
    }
}

/// The generated accessors expose these counts as `u64` (the common
/// int wire type), but reqwest / our retry loop want `u32`. Values
/// that big are meaningless for "retry attempts" / "backoff factor" so
/// clamp instead of erroring — a user writing `fetchRetries=99999999`
/// gets `u32::MAX` attempts, which is effectively "retry forever".
fn clamp_u32(v: u64) -> u32 {
    v.min(u64::from(u32::MAX)) as u32
}

//! One retry policy for every registry request.
//!
//! The packument fetchers, the revalidation path and the tarball
//! fetchers all handle a *successful* response differently — 304
//! revalidation, cache writes, streaming SHA-512, typed vs. untyped
//! parse — so their loop bodies legitimately differ. What must *not*
//! differ is the retry decision: how long to back off, what to log,
//! and how much of the attempt budget a timeout is allowed to spend.
//!
//! That decision did drift. [`TIMEOUT_RETRY_CAP`] was introduced to stop
//! a stalled transfer costing the full attempt budget, and it reached
//! the two tarball helpers in `request.rs` but never reached the four
//! copied loops in `packument.rs`. The result was that one stalled
//! packument cost `3 x fetchTimeout` (measured: 970 s with the default
//! 5-minute timeout) while the equivalent tarball stall cost two
//! attempts. The resolver blocks on packument fetches, so the path that
//! kept the unbounded policy was the one where a stall is most visible
//! to the user — it presents as an install that hangs at 0% CPU.
//!
//! Centralising the decision here is what stops it drifting again: a
//! new caller gets the cap, the backoff and the user-facing wording by
//! construction rather than by remembering to copy them.

use super::body::TIMEOUT_RETRY_CAP;
use crate::Error;
use crate::config::FetchPolicy;
use std::time::Duration;

/// Why a request is being retried. Selects the warning code and the
/// wording, so the four call sites cannot describe the same event
/// differently.
#[derive(Clone, Copy)]
pub(super) enum RetryCause {
    /// `send()` itself failed: connection refused, TLS failure, DNS,
    /// or the request exceeded `fetchTimeout` / `fetchStallTimeout`.
    Transport,
    /// The response arrived but its body could not be read or decoded.
    BodyDecode,
    /// The registry answered with a retriable status (5xx, 429). Carries
    /// the code so [`RetryPolicy::warn_retry`] can keep emitting the
    /// structured `status` field the tarball paths in `request.rs` and
    /// `tarball.rs` emit. Dropping it here would have reintroduced,
    /// between packument and tarball retries, exactly the per-call-site
    /// divergence this module exists to prevent.
    Status(u16),
}

impl RetryCause {
    fn code(self) -> &'static str {
        match self {
            Self::Transport => aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSPORT,
            Self::BodyDecode => aube_codes::warnings::WARN_AUBE_HTTP_RETRY_BODY_DECODE,
            Self::Status(_) => aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSIENT,
        }
    }

    fn what_happened(self) -> &'static str {
        match self {
            Self::Transport => "connection failed or timed out",
            Self::BodyDecode => "response body could not be read",
            Self::Status(_) => "registry returned a temporary error",
        }
    }
}

/// Shared retry driver. Construct one per logical request, then ask it
/// what to do after each failed attempt.
pub(super) struct RetryPolicy<'a> {
    policy: &'a FetchPolicy,
    label: &'a str,
    /// Timeout-shaped failures consumed so far. Counted separately from
    /// the attempt index so a 503 on attempt 1 never eats the timeout
    /// budget — the two failure modes cost very different wall-clock.
    timeout_retries: u32,
}

impl<'a> RetryPolicy<'a> {
    pub(super) fn new(policy: &'a FetchPolicy, label: &'a str) -> Self {
        Self {
            policy,
            label,
            timeout_retries: 0,
        }
    }

    pub(super) fn max_attempts(&self) -> u32 {
        self.policy.retries.saturating_add(1)
    }

    /// Decide what to do after attempt `attempt` (0-indexed) failed.
    ///
    /// `Some(wait)` means sleep that long and try again; the warning has
    /// already been emitted. `None` means give up and surface the error
    /// to the caller — either the budget is spent or the failure is
    /// timeout-shaped and has already cost more wall-clock than another
    /// attempt is worth.
    pub(super) fn next_backoff(&mut self, is_timeout: bool, attempt: u32) -> Option<Duration> {
        if attempt + 1 >= self.max_attempts() {
            return None;
        }
        // A timeout has already burned a full `fetchTimeout`. Retrying
        // it repeatedly multiplies a hang the user is already staring
        // at, so it gets a tighter budget than the retry count implies.
        if is_timeout {
            if self.timeout_retries >= TIMEOUT_RETRY_CAP {
                return None;
            }
            self.timeout_retries += 1;
        }
        Some(self.policy.backoff_for_attempt(attempt + 1))
    }

    /// Emit the user-facing retry warning. Kept separate from the
    /// decision so a caller can drop a single-flight guard between the
    /// two without holding it across the log call.
    ///
    /// The message names the package, says what went wrong in plain
    /// words, and says what happens next. A user watching an install
    /// stall needs all three: without the package name they cannot tell
    /// which dependency is at fault, and without the wait they cannot
    /// tell whether the tool is still working or wedged.
    pub(super) fn warn_retry(
        &self,
        cause: RetryCause,
        err: &dyn std::fmt::Display,
        attempt: u32,
        wait: Duration,
    ) {
        // Two arms rather than an `Option` field: the status paths in
        // `request.rs` / `tarball.rs` emit a bare `status = <u16>`, and
        // matching that exactly is the whole point of centralising here.
        match cause {
            RetryCause::Status(status) => tracing::warn!(
                attempt = attempt + 1,
                max_attempts = self.max_attempts(),
                backoff_ms = wait.as_millis() as u64,
                status,
                error = %err,
                label = self.label,
                code = cause.code(),
                "{}: {} — retrying in {}s (attempt {} of {})",
                self.label,
                cause.what_happened(),
                wait.as_secs().max(1),
                attempt + 2,
                self.max_attempts(),
            ),
            _ => tracing::warn!(
                attempt = attempt + 1,
                max_attempts = self.max_attempts(),
                backoff_ms = wait.as_millis() as u64,
                error = %err,
                label = self.label,
                code = cause.code(),
                "{}: {} — retrying in {}s (attempt {} of {})",
                self.label,
                cause.what_happened(),
                wait.as_secs().max(1),
                attempt + 2,
                self.max_attempts(),
            ),
        }
    }

    /// Emit the warning for a stop that the retry *count* does not
    /// explain. Every call site guards on `!is_last`, so reaching here
    /// means [`Self::next_backoff`] refused for the other reason: the
    /// failure was timeout-shaped and the timeout budget is spent.
    ///
    /// Worth its own line because the arithmetic is otherwise baffling
    /// from outside — the user set `fetchRetries=5`, saw two attempts,
    /// and is owed the reason. The error itself still propagates and is
    /// rendered by the caller, so this says *why we stopped*, not what
    /// broke.
    ///
    /// The `elapsed` argument is the request's own measured wall-clock,
    /// and it has to be measured rather than derived from `fetchTimeout`.
    /// A timeout error can now come from either bound, and reqwest's
    /// `is_timeout` cannot tell them apart, so reporting the
    /// `fetchTimeout` value would claim 300s for a request that actually
    /// spent 60s on the stall bound. This is the line the reporter of
    /// the originating issue sees, so its one number has to be true.
    pub(super) fn warn_giving_up(
        &self,
        cause: RetryCause,
        err: &dyn std::fmt::Display,
        elapsed: Duration,
    ) {
        tracing::warn!(
            error = %err,
            label = self.label,
            elapsed_ms = elapsed.as_millis() as u64,
            code = cause.code(),
            "{}: {} — not retrying, this request has already spent {}s; raise fetchStallTimeout \
             or fetchTimeout if the registry is simply slow",
            self.label,
            cause.what_happened(),
            elapsed.as_secs().max(1),
        );
    }
}

/// Whether a `reqwest` error is timeout-shaped. Both `fetchTimeout` and
/// `fetchStallTimeout` surface through this, and reqwest reports them as
/// a request-kind error whose *source* is its `TimedOut` marker, so the
/// check has to walk the source chain — which `is_timeout` does.
pub(super) fn is_timeout_error(err: &reqwest::Error) -> bool {
    err.is_timeout()
}

/// Same question for an already-wrapped [`Error`].
pub(super) fn is_timeout_body_error(err: &Error) -> bool {
    matches!(err, Error::Http(e) if e.is_timeout())
}

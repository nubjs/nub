//! Pure-function tests for [`warn_slow_tarball`]. The helper emits
//! a `tracing::warn` so we can't directly assert on output here;
//! instead we cover the branching (threshold=0 → no-op, sub-second
//! transfer → no-op, empty body → no-op, slow real download → warn)
//! by asserting the helper doesn't panic. The BATS smoke test
//! exercises the log line end-to-end.
use super::body::warn_slow_tarball;
use std::time::Duration;

#[test]
fn zero_threshold_disables_warning() {
    // threshold=0 short-circuits before any math — safe with any
    // inputs, including a genuinely slow transfer.
    warn_slow_tarball(
        0,
        "https://example.com/pkg.tgz",
        1024,
        Duration::from_secs(10),
    );
}

#[test]
fn sub_second_transfer_skipped_to_avoid_handshake_noise() {
    // Matches pnpm's `elapsedSec > 1` gate. A 2 KiB tarball
    // completing in 500ms computes to 4 KiB/s — well below the
    // 50 KiB/s threshold — but the "average" is dominated by TCP/
    // TLS handshake + TTFB, not real throughput. Must not warn.
    warn_slow_tarball(
        50,
        "https://example.com/quick.tgz",
        2048,
        Duration::from_millis(500),
    );
}

#[test]
fn exactly_one_second_skipped() {
    // Boundary: pnpm uses `elapsedSec > 1` (strictly greater), so
    // a transfer that took exactly one second must not warn even
    // though its computed average is below threshold.
    warn_slow_tarball(
        50,
        "https://example.com/boundary.tgz",
        10_240,
        Duration::from_secs(1),
    );
}

#[test]
fn zero_elapsed_skipped_to_avoid_division_by_zero() {
    // `resp.bytes()` can plausibly complete in under a millisecond
    // for cached/in-memory responses (wiremock is in-process). The
    // sub-second gate covers this too, but we keep the test to pin
    // the branch.
    warn_slow_tarball(50, "https://example.com/fast.tgz", 10_240, Duration::ZERO);
}

#[test]
fn fast_download_does_not_warn() {
    // 10 MiB in 2 seconds ≈ 5_120 KiB/s, far above the 50 KiB/s
    // default threshold. Elapsed clears the one-second gate so
    // the math runs — and must not warn.
    warn_slow_tarball(
        50,
        "https://example.com/pkg.tgz",
        10 * 1024 * 1024,
        Duration::from_secs(2),
    );
}

#[test]
fn slow_download_triggers_warning_path() {
    // 10 KiB in 2 seconds = 5 KiB/s, well below the 50 KiB/s
    // threshold and past the one-second gate. The helper should
    // take the warn branch; we rely on the BATS smoke test to
    // observe the log line itself, but this call must at least
    // not panic on arithmetic.
    warn_slow_tarball(
        50,
        "https://example.com/slow.tgz",
        10_240,
        Duration::from_secs(2),
    );
}

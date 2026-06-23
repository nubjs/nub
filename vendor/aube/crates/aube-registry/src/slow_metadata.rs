//! Debounced, grouped warnings for slow registry metadata fetches.
//!
//! `fetchWarnTimeoutMs` is an observability knob: when a packument
//! takes longer than the threshold, aube wants to surface that slowness
//! so operators can spot registry latency without enabling debug
//! tracing. Emitting one warning *per* slow packument floods the
//! install output — a single throttled run can produce dozens of
//! near-identical lines. Waiting until end-of-resolve to summarize
//! goes too far the other way: the user sees nothing for tens of
//! seconds while a slow registry stalls the install.
//!
//! This module sits in the middle: a tumbling window opens on the
//! first slow event and stays open for `FLUSH_WINDOW`. Every event in
//! that window is accumulated into one group, and the window's expiry
//! flushes the group as a single `tracing::warn!`. If more events
//! arrive after the flush, a fresh window opens. The install pipeline
//! and the process-exit hook in `aube/src/main.rs` both call
//! [`flush_summary`] to drain any trailing group whose window hasn't
//! expired — install-time flush gives the user feedback before the
//! fetch phase starts; the process-exit flush catches slow fetches
//! from non-install commands (`aube add`, `aube audit`, `aube
//! deprecate`, `aube deprecations`) that don't run a resolver.
//!
//! The result: groups roughly the size of "events that arrived close
//! together in time," refreshed at the cadence of the window. The
//! `code = WARN_AUBE_SLOW_METADATA` field on each emission is
//! unchanged — CI scripts and ndjson reporters that branch on the
//! code stay working.
//!
//! Per-event detail (label + elapsed) is *not* logged at any level by
//! default. `--loglevel debug` floods unrelated DEBUG sites and isn't
//! a usable escape hatch; if structured per-package telemetry is ever
//! needed, ndjson is the right vehicle.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Window during which slow-fetch events are coalesced into one group.
/// Long enough that bursts (~18 events within a few seconds) emit a
/// single warning; short enough that streaming slowness surfaces in
/// near-real-time rather than waiting for end-of-resolve. The
/// threshold itself is typically 10s, so a sub-threshold window keeps
/// groups distinguishable: at most one group per window, multiple
/// groups per install when latency persists.
const FLUSH_WINDOW: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
struct Record {
    label: String,
    elapsed_ms: u64,
}

#[derive(Default)]
struct State {
    records: Vec<Record>,
    /// Whether a background timer is currently armed to flush the
    /// current window. Only set after the spawn succeeds — the no-
    /// runtime path leaves it `false` so the next [`record`] call can
    /// retry once a runtime is available (matters when `record` is
    /// invoked outside a runtime, e.g. early test-time paths).
    timer_armed: bool,
    /// Monotonic generation counter incremented every time a window
    /// closes (timer-driven or explicit [`flush_summary`]). The timer
    /// task captures the generation at spawn time; when it wakes, it
    /// re-acquires the mutex and only drains if the generation still
    /// matches. A stale timer left over from an explicit flush sees a
    /// mismatched generation and exits without stealing records from
    /// the next window.
    generation: u64,
    /// Most recent `fetchWarnTimeoutMs` seen at a [`record`] call.
    /// Carried into the timer-driven summary so the grouped warning
    /// can name the threshold without the timer task re-reading
    /// settings. Invariant across a single install run.
    threshold_ms: u64,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

/// Record that `label` took `elapsed_ms` and exceeded `threshold_ms`
/// (`fetchWarnTimeoutMs`). Called by the registry client's metadata
/// fetch path in place of a per-event `tracing::warn!`.
///
/// The first event in a window arms a background tokio timer that
/// drains the group after [`FLUSH_WINDOW`]. Subsequent events inside
/// that window simply accumulate. If no tokio runtime is current
/// (early-process paths, unit tests outside `#[tokio::test]`), no
/// timer is armed and the group only drains via [`flush_summary`].
pub fn record(label: &str, elapsed_ms: u64, threshold_ms: u64) {
    let needs_arm = {
        let Ok(mut g) = state().lock() else {
            return;
        };
        g.records.push(Record {
            label: label.to_string(),
            elapsed_ms,
        });
        g.threshold_ms = threshold_ms;
        !g.timer_armed
    };
    if needs_arm {
        try_arm_flush_timer();
    }
}

/// Best-effort: spawn a tokio task that drains the current window
/// after [`FLUSH_WINDOW`]. Returns immediately on a runtime miss
/// (early-process and unit-test paths) leaving `timer_armed = false`
/// so the next `record` call retries the spawn. Production call
/// sites all run inside a tokio runtime (install drives the registry
/// client via `tokio::spawn`/`JoinSet`), so this fallback is purely
/// defensive.
///
/// The timer task captures `generation` at spawn time and only drains
/// when the generation still matches at wake-up. An intervening
/// explicit [`flush_summary`] bumps the generation so the stale timer
/// no-ops instead of stealing records that belong to the next window.
fn try_arm_flush_timer() {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let captured_generation = {
        let Ok(mut g) = state().lock() else {
            return;
        };
        g.timer_armed = true;
        g.generation
    };
    handle.spawn(async move {
        tokio::time::sleep(FLUSH_WINDOW).await;
        drain_window_if_current(captured_generation);
    });
}

/// Drain the current window if `generation` still matches the one the
/// caller captured at spawn time. Stale timer wake-ups (left over
/// from an explicit [`flush_summary`] that bumped the generation)
/// silently exit so they don't emit records that belong to a fresh
/// window.
fn drain_window_if_current(captured_generation: u64) {
    let (records, threshold_ms) = {
        let Ok(mut g) = state().lock() else {
            return;
        };
        if g.generation != captured_generation {
            return;
        }
        let records = std::mem::take(&mut g.records);
        g.timer_armed = false;
        g.generation = g.generation.wrapping_add(1);
        (records, g.threshold_ms)
    };
    emit(records, threshold_ms);
}

fn emit(records: Vec<Record>, threshold_ms: u64) {
    let count = records.len();
    if count == 0 {
        return;
    }
    let slowest = records
        .iter()
        .max_by_key(|r| r.elapsed_ms)
        .expect("count > 0 implies a slowest record");
    // Rendered message carries count, threshold, and which package was
    // slowest. Drop redundant structured fields — they trail the line
    // in CLI output and just print the same numbers twice. Also drop
    // the slowest fetch's elapsed_ms: it's selection-biased to land
    // just over the threshold so it carries no actionable signal.
    // `code` stays so ndjson reporters and CI scripts can branch on it.
    tracing::warn!(
        code = aube_codes::warnings::WARN_AUBE_SLOW_METADATA,
        "registry slow: {count} metadata fetches over {threshold_ms}ms (slowest: {})",
        slowest.label,
    );
}

/// Drain any trailing group whose window hasn't fired yet. Called
/// from two places: end-of-resolve in the install pipeline (so the
/// user sees the tail before the fetch phase takes over), and the
/// process-exit hook in `aube/src/main.rs` (so non-install commands
/// like `aube add` / `aube audit` / `aube deprecate` /
/// `aube deprecations` still surface slow-fetch warnings even though
/// they don't run a resolver).
///
/// Bumps the generation counter so any in-flight timer task that was
/// armed for this window wakes up, sees the bumped generation, and
/// exits without re-emitting the drained records.
pub fn flush_summary() {
    let (records, threshold_ms) = {
        let Ok(mut g) = state().lock() else {
            return;
        };
        let records = std::mem::take(&mut g.records);
        g.timer_armed = false;
        g.generation = g.generation.wrapping_add(1);
        (records, g.threshold_ms)
    };
    emit(records, threshold_ms);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Process-global state plus parallel test execution means two
    /// tests touching the accumulator can race. Serialize the whole
    /// module under one test entry point so the cases run
    /// deterministically.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn debounced_grouping_lifecycle() {
        // Reset state in case another test ran first.
        {
            let mut g = state().lock().unwrap();
            g.records.clear();
            g.timer_armed = false;
            g.generation = 0;
            g.threshold_ms = 0;
        }

        // Empty case: flush with nothing accumulated emits nothing
        // and leaves the buffer empty.
        flush_summary();
        assert!(state().lock().unwrap().records.is_empty());

        // First event arms the timer. Second event in the same window
        // joins the group without arming again.
        record("packument a", 11_000, 10_000);
        record("packument b", 13_500, 10_000);
        {
            let g = state().lock().unwrap();
            assert_eq!(g.records.len(), 2, "both events buffered into the group");
            assert!(g.timer_armed, "first event armed the flush timer");
        }

        // Advance virtual time past the window; the timer task drains
        // the group and clears the flag. Multiple yields cover the
        // case where the spawned task's wake takes more than one poll
        // cycle to flush on the current-thread runtime.
        tokio::time::sleep(FLUSH_WINDOW + Duration::from_millis(50)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        {
            let g = state().lock().unwrap();
            assert!(
                g.records.is_empty(),
                "timer must drain the window after FLUSH_WINDOW",
            );
            assert!(!g.timer_armed, "timer must clear the armed flag on drain");
        }

        // New event after the drain opens a fresh window.
        record("packument c", 14_000, 10_000);
        assert!(state().lock().unwrap().timer_armed);

        // Explicit flush_summary drains the trailing group even though
        // the window hasn't expired, and bumps the generation so the
        // stale timer left over from this window no-ops on wake-up.
        let generation_before_flush = state().lock().unwrap().generation;
        flush_summary();
        {
            let g = state().lock().unwrap();
            assert!(g.records.is_empty(), "flush_summary drains the tail");
            assert!(!g.timer_armed, "flush_summary clears the armed flag");
            assert_ne!(
                g.generation, generation_before_flush,
                "flush_summary must bump the generation to invalidate the pending timer",
            );
        }

        // Stale-timer-protection check: record a fresh event (opens a
        // new window with a fresh generation + new timer), then let
        // the *first* window's timer wake up. It must see the bumped
        // generation and exit without stealing the new window's
        // records.
        record("packument d", 15_000, 10_000);
        let stolen_window_records = state().lock().unwrap().records.len();
        // The previous window's timer was armed at generation N; the
        // current window is at generation N+1 or N+2. Advance just
        // enough wall-clock for the stale timer to wake.
        tokio::time::sleep(FLUSH_WINDOW + Duration::from_millis(50)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        // After the stale timer wakes and the new timer also fires,
        // the new window's records must have been drained by the
        // *new* timer (matching generation), not stolen by the stale
        // timer (which would have left the buffer empty earlier and
        // the new window with nothing to drain).
        assert_eq!(
            stolen_window_records, 1,
            "fresh window started with one record",
        );
        assert!(
            state().lock().unwrap().records.is_empty(),
            "new window's timer must drain its own records",
        );
    }
}

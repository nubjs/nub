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

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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
    /// Metadata requests that are in flight *right now*, keyed by a
    /// monotonic id, holding the label and the instant the request
    /// started.
    ///
    /// This is what makes a stall visible. Everything else in this
    /// module is post-hoc: [`record`] runs only after a request has
    /// returned, so a request that never returns produced no output at
    /// all. A stalled packument therefore showed the user nothing —
    /// frozen progress bar, 0% CPU, silence — until `fetchTimeout`
    /// expired minutes later. The pending set lets the ticker below
    /// report the wait *while it is happening*.
    pending: HashMap<u64, (String, Instant)>,
    next_id: u64,
    /// Whether the pending-request ticker is running. It stops itself
    /// once `pending` drains, so an install with no slow fetches pays
    /// nothing after its first metadata request completes.
    ticker_armed: bool,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

/// How often the pending-request ticker wakes. Also the spacing between
/// "still waiting" lines, so a 60s stall produces about five of them —
/// enough to show the elapsed time climbing, few enough not to bury the
/// rest of the install output.
const PENDING_TICK: Duration = Duration::from_secs(10);

/// Guard marking one metadata request as in flight. Drop it when the
/// request finishes, succeeds or fails; [`begin`] returns it and the
/// pending set is what the ticker reports from.
///
/// `id` is `None` when `fetchWarnTimeoutMs` is `0`: the guard is inert
/// and nothing was registered, so the disabled setting costs nothing.
#[must_use = "the request stays marked in-flight until this guard drops"]
pub struct InFlight {
    id: Option<u64>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let Some(id) = self.id else {
            return;
        };
        if let Ok(mut g) = state().lock() {
            g.pending.remove(&id);
        }
    }
}

/// Mark `label` as in flight and make sure the ticker is running.
///
/// Pair this with the existing [`record`] call, which reports the same
/// request once it has *finished*. The two answer different questions:
/// `record` says "that was slow", `begin` is what lets nub say "this is
/// still going" while the user is waiting on it.
///
/// `threshold_ms == 0` is `fetchWarnTimeoutMs`'s documented "disable the
/// warning entirely", and it disables this line too. Gating at the
/// boundary rather than at the emit site is what makes that airtight: a
/// ticker can then never be running without having seen a real
/// threshold, so there is no default to fall back to and no way for the
/// disabled setting to produce a line every 10s.
pub fn begin(label: &str, threshold_ms: u64) -> InFlight {
    if threshold_ms == 0 {
        return InFlight { id: None };
    }
    let id = {
        let Ok(mut g) = state().lock() else {
            return InFlight { id: None };
        };
        let id = g.next_id;
        g.next_id = g.next_id.wrapping_add(1);
        g.pending.insert(id, (label.to_string(), Instant::now()));
        g.threshold_ms = threshold_ms;
        id
    };
    arm_pending_ticker();
    InFlight { id: Some(id) }
}

/// Start the ticker if it is not already running. Mirrors
/// [`try_arm_flush_timer`]: a missing tokio runtime leaves the flag
/// clear so the next [`begin`] retries the spawn.
fn arm_pending_ticker() {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    {
        let Ok(mut g) = state().lock() else {
            return;
        };
        if g.ticker_armed {
            return;
        }
        g.ticker_armed = true;
    }
    handle.spawn(async move {
        // The stop decision and the `ticker_armed` clear happen together
        // inside `report_pending_once`, under one lock. Splitting them
        // would let a `begin` land in between, see the flag still set,
        // skip the spawn, and then be left pending with no ticker — a
        // stall reported by nobody, which is the exact failure this
        // module exists to remove.
        while report_pending_once() {
            tokio::time::sleep(PENDING_TICK).await;
        }
    });
}

/// What the ticker found on one pass. `None` from [`pending_report`]
/// means nothing is in flight at all, which is the ticker's stop
/// condition; `over_threshold` being `None` means requests are in
/// flight but none has waited long enough to be worth a line yet.
struct PendingReport {
    over_threshold: Option<(usize, String, Duration)>,
}

/// Pure half of [`report_pending_once`], split out so the selection
/// logic — which requests count, and which one is named — is testable
/// without a tokio runtime or a tracing subscriber.
///
/// Returning `None` also CLEARS `ticker_armed`, under the same lock that
/// observed the empty set. That atomicity is load-bearing: with the two
/// split, a `begin` could insert between them, see the flag still set,
/// decline to spawn, and be left with no ticker watching it. Same shape
/// as [`drain_window_if_current`], which already pairs its decision and
/// its flag write this way.
fn pending_report(now: Instant) -> Option<PendingReport> {
    let mut g = state().lock().ok()?;
    if g.pending.is_empty() {
        g.ticker_armed = false;
        return None;
    }
    // `begin` refuses to register anything when the threshold is 0, so a
    // non-empty pending set implies a real threshold was recorded.
    let threshold = Duration::from_millis(g.threshold_ms);
    let mut over = 0usize;
    let mut longest_label = String::new();
    let mut longest_wait = Duration::ZERO;
    for (label, started) in g.pending.values() {
        let waited = now.saturating_duration_since(*started);
        if waited >= threshold {
            over += 1;
        }
        if waited > longest_wait {
            longest_wait = waited;
            longest_label = label.clone();
        }
    }
    Some(PendingReport {
        over_threshold: (over > 0).then_some((over, longest_label, longest_wait)),
    })
}

/// Emit one "still waiting" line if any in-flight request has been
/// waiting longer than the threshold. Returns `false` once nothing is
/// pending, having cleared `ticker_armed` in the same critical section.
fn report_pending_once() -> bool {
    let Some(outcome) = pending_report(Instant::now()) else {
        return false;
    };
    let PendingReport { over_threshold } = outcome;
    let Some((count, label, waited)) = over_threshold else {
        // Nothing over the threshold yet, but requests are still in
        // flight — keep ticking.
        return true;
    };
    // Names the package and the elapsed wait so a stalled install is
    // legible while it is stalled, instead of only in hindsight.
    tracing::warn!(
        code = aube_codes::warnings::WARN_AUBE_SLOW_METADATA,
        "still waiting on {count} registry metadata request{} (longest: {label}, {}s)",
        if count == 1 { "" } else { "s" },
        waited.as_secs(),
    );
    true
}

/// Record that `label` took `elapsed_ms` and exceeded `threshold_ms`
/// (`fetchWarnTimeoutMs`). Called by the registry client's metadata
/// fetch path in place of a per-event `tracing::warn!`.
///
/// The first event in a window arms a background tokio timer that
/// drains the group after `FLUSH_WINDOW`. Subsequent events inside
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
/// after `FLUSH_WINDOW`. Returns immediately on a runtime miss
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
    /// tests touching the accumulator can race. Every test in this
    /// module holds this lock for its duration, so the cases run
    /// deterministically no matter how cargo schedules them.
    ///
    /// (Previously there was a single test and the comment said to keep
    /// it that way. A lock is the cheaper constraint: it lets the
    /// pending-request cases live in their own test without re-opening
    /// the race.)
    ///
    /// Deliberately tokio's mutex, not `std`'s: these tests await, and a
    /// `std` guard held across an await point is what
    /// `clippy::await_holding_lock` exists to reject.
    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Clear every field so a test starts from a known state
    /// regardless of what ran before it.
    fn reset_state() {
        let mut g = state().lock().unwrap();
        g.records.clear();
        g.timer_armed = false;
        g.generation = 0;
        g.threshold_ms = 0;
        g.pending.clear();
        g.next_id = 0;
        g.ticker_armed = false;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn debounced_grouping_lifecycle() {
        let _serial = TEST_LOCK.lock().await;
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

    /// The in-flight side: what the user sees *while* a fetch is stuck.
    /// Before this existed, a request that never returned produced no
    /// output at all, because everything else here reports on requests
    /// that have already finished.
    #[tokio::test(flavor = "current_thread")]
    async fn pending_report_names_the_longest_waiting_request() {
        let _serial = TEST_LOCK.lock().await;
        reset_state();

        // Nothing in flight: the ticker's stop condition.
        assert!(
            pending_report(Instant::now()).is_none(),
            "an empty pending set must stop the ticker",
        );

        let quick = begin("packument fast-pkg", 10_000);
        let stuck = begin("packument stuck-pkg", 10_000);
        let now = Instant::now();

        // Both are young, so there is nothing worth telling the user
        // yet — but the ticker must keep running.
        let report = pending_report(now).expect("requests are in flight");
        assert!(
            report.over_threshold.is_none(),
            "a request under the threshold must not produce a line",
        );

        // Age only the stuck one past the threshold.
        {
            let mut g = state().lock().unwrap();
            let started = g
                .pending
                .values_mut()
                .find(|(label, _)| label == "packument stuck-pkg")
                .map(|(_, started)| started)
                .expect("stuck request is registered");
            *started = now - Duration::from_secs(45);
        }

        let report = pending_report(now).expect("requests are in flight");
        let (count, label, waited) = report
            .over_threshold
            .expect("a request past the threshold must produce a line");
        assert_eq!(count, 1, "only the aged request is over the threshold");
        assert_eq!(
            label, "packument stuck-pkg",
            "the line must name the package the user is actually blocked on",
        );
        assert_eq!(waited.as_secs(), 45, "the line must report the real wait");

        // Dropping the guards clears the set, which is what stops the
        // ticker once an install recovers.
        drop(quick);
        drop(stuck);
        assert!(
            pending_report(Instant::now()).is_none(),
            "dropping every guard must stop the ticker",
        );
    }

    /// `fetchWarnTimeoutMs = 0` is documented as disabling the warning
    /// entirely. It has to disable the in-flight line too — otherwise
    /// turning the warning off would turn on a line every 10s, which is
    /// the opposite of what the setting says.
    #[tokio::test(flavor = "current_thread")]
    async fn zero_threshold_registers_nothing() {
        let _serial = TEST_LOCK.lock().await;
        reset_state();

        let disabled = begin("packument whatever", 0);
        assert!(
            state().lock().unwrap().pending.is_empty(),
            "a disabled threshold must not register an in-flight request",
        );
        assert!(
            !state().lock().unwrap().ticker_armed,
            "a disabled threshold must not arm the ticker",
        );
        assert!(
            pending_report(Instant::now()).is_none(),
            "a disabled threshold must produce no report",
        );
        drop(disabled);
    }

    /// The ticker's stop condition and its `ticker_armed` clear must
    /// happen together. Split across two lock acquisitions, a `begin`
    /// landing between them would see the flag still set, skip the
    /// spawn, and be left with nothing watching it.
    #[tokio::test(flavor = "current_thread")]
    async fn emptying_the_pending_set_clears_the_armed_flag() {
        let _serial = TEST_LOCK.lock().await;
        reset_state();
        state().lock().unwrap().ticker_armed = true;

        let guard = begin("packument solo", 10_000);
        assert!(state().lock().unwrap().ticker_armed);
        drop(guard);

        assert!(
            pending_report(Instant::now()).is_none(),
            "an empty pending set stops the ticker",
        );
        assert!(
            !state().lock().unwrap().ticker_armed,
            "the same critical section that saw the empty set must clear the flag",
        );
    }
}

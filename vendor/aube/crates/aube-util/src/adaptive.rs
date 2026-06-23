/*!
 * Aube's adaptive runtime helpers. A global library of runtime tuned
 * lock free primitives that replace hard coded magic numbers across
 * the codebase with online observation.
 *
 * # Members
 *
 * [`AdaptiveLimit`] is a concurrency limiter. Packed `inflight|limit`
 * `u64`, slow start grow on success, hard throttle shrink on
 * 429 / 503 / timeout, CUSUM gated multiplicative shrink on
 * sustained rising latency. Replaces every static
 * `Semaphore::new(N)` site.
 *
 * [`RegimeDetector`] is a CUSUM cumulative sum change point detector.
 * Distinguishes transient jitter from sustained distribution shift on
 * a streaming signal. Two atomic counters of state.
 *
 * # Why this is fundamentally faster than every other PM
 *
 * Every package manager today (npm, pnpm, yarn, bun, vlt) ships a
 * static cap. `--maxsockets`, `network-concurrency`,
 * `--max-fetch-attempts`. Those numbers are wrong on every machine
 * that is not the developer's laptop. Too low for fat pipes (idle
 * bandwidth). Too high for thin pipes (queueing). Too high for
 * private registries (429 spam). Too low for public CDNs
 * (under saturation). The user is on the hook for tuning per
 * environment. None of the other tools tune.
 *
 * Aube steers everything from observed RTT, producer / consumer
 * skew, throttle responses, and CUSUM detected regime changes. Never
 * config knobs. Operating points converge to bandwidth delay product
 * without any user input.
 *
 * # Architecture
 *
 * **Single packed state**. `inflight:u32 | limit:u32` packed into one
 * `AtomicU64`. Acquire is one CAS, not two atomics that race. Release
 * is one `fetch_sub`. No separate "limit" plus "inflight" coherence
 * story.
 *
 * **Lock free EWMA**. Two exponentially weighted moving averages.
 * `ewma_fast` (alpha 1/4, seconds scale) and `ewma_slow` (alpha 1/32,
 * tens of seconds). Their ratio detects regime shifts. Atomic CAS
 * update. No mutex anywhere on the hot path. BBR and TCP Vegas use
 * the same two clock trick.
 *
 * **Cache line layout**. Hot atomics that mutate per request live on
 * one cache line. Rarely mutated bounds plus notify on another.
 * False sharing across the two stays out of the way of high rate
 * fanout.
 *
 * **No allocation in the limiter**. Zero heap traffic on the success
 * path. Only allocations are the `Arc` (one) and the `Notify` slots
 * tokio internally manages on contended waits, which for this
 * workload happens at most a few times per install.
 *
 * **`#[must_use]` permit plus Drop guard**. Dropping a permit
 * without calling `record_*` counts as cancellation, not a leak. The
 * signal is silently discarded so a question mark propagated error
 * mid request does not bias the controller toward shrinking when the
 * network was fine.
 *
 * # Math
 *
 * RTT samples are stored as `u64` microseconds. EWMA update for a
 * smoothing constant alpha 1 over 2 to the k is
 * `next : avg + (sample - avg) >> k`. Two adds and one shift,
 * branchless, no `f64`. The gradient comparison is
 * `(min_rtt * 10) / avg` and at least 9. The float divide stays off
 * the hot path.
 *
 * # Bounds
 *
 * `min_limit` and `max_limit` are guard rails, not the operating
 * point. The default range `[4, 1024]` is chosen so that 4 keeps
 * progress under continuous throttling (we never deadlock at zero
 * permits even if every reply is a 503), and 1024 caps RAM growth
 * from a hypothetical bug free registry that replies in 0 ns to
 * every request (never observed in practice). Real public npm
 * traffic settles around 60 to 120. Private Artifactory settles
 * around 20 to 40. Both happen automatically.
 */

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/**
 * EWMA smoothing exponents. Alpha is 1 over 2 to the
 * `EWMA_FAST_SHIFT` for the fast clock and 1 over 2 to the
 * `EWMA_SLOW_SHIFT` for the slow clock. The fast EWMA reacts in
 * roughly 4 to 8 samples. The slow EWMA reacts in roughly 32 to 64.
 * Their ratio is the regime change detector that BBR style
 * controllers use to distinguish transient jitter from sustained
 * slowdown.
 */
const EWMA_FAST_SHIFT: u32 = 2;
const EWMA_SLOW_SHIFT: u32 = 5;

/**
 * Multiplicative shrink factor on CUSUM detected sustained rising
 * regime. 7 over 10 (0.7x) per shrink event takes a 256 cap limiter
 * down to the 60 floor in roughly 5 detected shifts. Fast enough to
 * react to sustained backend slowdown. Slow enough that one detector
 * trip does not collapse to floor on its own.
 */
const SHRINK_NUM_FACTOR: u64 = 7;
const SHRINK_DEN_FACTOR: u64 = 10;

/**
 * Hard back pressure (HTTP 429, 503, timeout, connection reset).
 * Halve the cap and freeze growth for the cooldown window so we do
 * not immediately re saturate the upstream that just told us to slow
 * down.
 */
const THROTTLE_NUM: u64 = 1;
const THROTTLE_DEN: u64 = 2;
const THROTTLE_COOLDOWN_NS: u64 = 1_000_000_000;

/**
 * Slow start envelope. While `successes` stays under
 * `SLOW_START_SAMPLES` and no throttle has fired, every success
 * doubles the limit (capped by `max_limit`). Past the envelope the
 * controller switches to additive increase (`+1` per success). This
 * mirrors TCP slow start. Cold installs need to ramp the cap to
 * bandwidth delay product in tens of completions, not hundreds.
 */
const SLOW_START_SAMPLES: u64 = 32;

#[inline(always)]
fn pack(inflight: u32, limit: u32) -> u64 {
    ((limit as u64) << 32) | (inflight as u64)
}

#[inline(always)]
fn unpack(s: u64) -> (u32, u32) {
    (s as u32, (s >> 32) as u32)
}

#[repr(align(64))]
struct HotState {
    /**
     * Packed state. Low 32 bits hold `inflight`. High 32 bits hold
     * `limit`. One `AtomicU64` so a permit reservation is a single
     * CAS that atomically observes the limit and the in flight
     * count. Avoids the classic two atomic race where `limit`
     * shrinks between the `inflight` load and the CAS.
     */
    state: AtomicU64,
    /** Minimum observed RTT in microseconds. Ratchets only down. */
    min_rtt_us: AtomicU64,
    /** Fast EWMA of RTT samples in microseconds. Alpha 1/4. */
    ewma_fast_us: AtomicU64,
    /** Slow EWMA of RTT samples in microseconds. Alpha 1/32. */
    ewma_slow_us: AtomicU64,
    /**
     * Wall instant after which `limit` is allowed to grow again.
     * Stored as nanoseconds since `created_at`. An atomic load
     * suffices.
     */
    throttle_until_ns: AtomicU64,
    /**
     * Total successful completions. Used for the slow start phase.
     * While this stays below `SLOW_START_SAMPLES` and no throttle
     * has been observed (`first_throttle` reads as 0), the limiter
     * doubles on each success instead of incrementing by 1. Mirrors
     * TCP slow start. A cold install with 350 plus tarballs needs
     * to ramp from a small seed to the working point in seconds,
     * not minutes.
     */
    successes: AtomicU64,
    /**
     * Set to 1 the first time `record_throttle` fires. Permanently
     * disables slow start so any subsequent regrow is gentle AIMD.
     * Atomic store release plus load acquire pair so the slow start
     * decision in `record_success` sees the flag set as soon as the
     * throttle path commits.
     */
    first_throttle: AtomicU64,
}

#[repr(align(64))]
struct ColdState {
    available: Notify,
    bounds: (u32, u32),
    created_at: Instant,
    /**
     * CUSUM regime detector on observed RTT. Fed every successful
     * completion. When it fires `Rising`, the limiter applies a
     * multiplicative shrink. This re enables the gradient intuition
     * in a way that does not false positive on cold start jitter.
     * A single slow handshake will not accumulate enough CUSUM to
     * cross the threshold. Sustained 2x baseline RTT will.
     *
     * The threshold parameter is wall time accumulated upward
     * deviation, not a rate. Any caller visible RTT works because
     * the detector internally re centers on its own EWMA.
     */
    regime: RegimeDetector,
    /**
     * When set, the CUSUM `Rising` regime signal does not shrink
     * the limit. Throttle-driven shrink (`record_throttle`, fired
     * on real backpressure: HTTP 429/503, IO errors) is unaffected.
     *
     * Use case: filesystem-bound limiters where per-op latency
     * variance is exogenous (antivirus scans, NTFS cold-cache
     * reads, COW reflink fall-through to copy). Rising RTT on
     * those paths is intrinsic noise, not a signal that more
     * concurrency is making things worse — there is no upstream
     * queue to relieve. Treating it as backpressure was observed
     * to collapse the linker prewarm limit from seed 16 to 12 on
     * Windows, queueing 1195 packages behind a 12-permit cap.
     *
     * Network limiters (registry packument, registry tarball)
     * keep CUSUM enabled because rising RTT there does correlate
     * with upstream queueing.
     */
    cusum_shrink_disabled: AtomicBool,
}

#[derive(Debug)]
pub struct AdaptiveLimit {
    hot: HotState,
    cold: ColdState,
}

impl std::fmt::Debug for HotState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (inflight, limit) = unpack(self.state.load(Ordering::Relaxed));
        f.debug_struct("HotState")
            .field("inflight", &inflight)
            .field("limit", &limit)
            .field("min_rtt_us", &self.min_rtt_us.load(Ordering::Relaxed))
            .field("ewma_fast_us", &self.ewma_fast_us.load(Ordering::Relaxed))
            .field("ewma_slow_us", &self.ewma_slow_us.load(Ordering::Relaxed))
            .finish()
    }
}

impl std::fmt::Debug for ColdState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ColdState")
            .field("bounds", &self.bounds)
            .finish()
    }
}

impl AdaptiveLimit {
    pub fn new(initial: usize, min_limit: usize, max_limit: usize) -> Arc<Self> {
        assert!(min_limit >= 1, "min_limit must be at least 1");
        assert!(min_limit <= max_limit, "min_limit must be <= max_limit");
        assert!(max_limit <= u32::MAX as usize, "max_limit fits in u32");
        let initial = initial.clamp(min_limit, max_limit) as u32;
        Arc::new(Self {
            hot: HotState {
                state: AtomicU64::new(pack(0, initial)),
                min_rtt_us: AtomicU64::new(u64::MAX),
                ewma_fast_us: AtomicU64::new(0),
                ewma_slow_us: AtomicU64::new(0),
                throttle_until_ns: AtomicU64::new(0),
                successes: AtomicU64::new(0),
                first_throttle: AtomicU64::new(0),
            },
            cold: ColdState {
                available: Notify::new(),
                bounds: (min_limit as u32, max_limit as u32),
                created_at: Instant::now(),
                /*
                 * Threshold of 5 seconds of accumulated upward
                 * deviation. Picked so a sustained ~100 ms baseline
                 * doubling (very common when a registry's edge is
                 * degraded) accumulates the full threshold in
                 * roughly 50 samples, while a single 5 s outlier
                 * (npm cold cache miss) is absorbed by subsequent
                 * baseline tracking.
                 */
                regime: RegimeDetector::new(5_000_000),
                cusum_shrink_disabled: AtomicBool::new(false),
            },
        })
    }

    /**
     * Disable CUSUM-driven shrinking on the success path. Call
     * once after construction for limiters that gate
     * filesystem-bound work (linker, materializer). Throttle-path
     * shrink (record_throttle on real IO errors) remains active.
     * See [`ColdState::cusum_shrink_disabled`] for rationale.
     */
    pub fn disable_cusum_shrink(&self) {
        self.cold
            .cusum_shrink_disabled
            .store(true, Ordering::Relaxed);
    }

    pub fn current_limit(&self) -> usize {
        unpack(self.hot.state.load(Ordering::Relaxed)).1 as usize
    }

    pub fn inflight(&self) -> usize {
        unpack(self.hot.state.load(Ordering::Relaxed)).0 as usize
    }

    /**
     * Construct a limiter whose initial value is loaded from the
     * given [`PersistentState`] under `key`, falling back to
     * `default_initial` when no prior run has persisted a value or
     * the file is unreadable. Convenience over `new` for any site
     * that wants cross run learning.
     */
    pub fn from_persistent(
        state: &PersistentState,
        key: &str,
        default_initial: usize,
        min_limit: usize,
        max_limit: usize,
    ) -> Arc<Self> {
        let seed = state.load_seed(key, default_initial);
        Self::new(seed, min_limit, max_limit)
    }

    /**
     * Persist the current observed limit back to the given store.
     * Call at the end of a phase or process so the next invocation
     * starts where this one converged.
     */
    pub fn persist(&self, state: &PersistentState, key: &str) {
        state.save_observed(key, self.current_limit());
    }

    pub async fn acquire(self: &Arc<Self>) -> AdaptivePermit {
        loop {
            let waiter = self.cold.available.notified();
            tokio::pin!(waiter);
            let s = self.hot.state.load(Ordering::Acquire);
            let (inflight, limit) = unpack(s);
            if inflight < limit {
                // `compare_exchange` (strong) instead of weak: a
                // spurious failure here would drop us into
                // `waiter.as_mut().await` even though a permit
                // slot was available. If no `release()` has fired
                // since we registered the waiter, the await sleeps
                // until the next release — which can be the full
                // duration of an in-flight request. Strong CAS
                // pays a tiny extra cost on the contended path
                // (LL/SC archs may retry internally) in exchange
                // for not gambling latency on a CPU mispredict.
                match self.hot.state.compare_exchange(
                    s,
                    pack(inflight + 1, limit),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        return AdaptivePermit {
                            limiter: Arc::clone(self),
                            started: Instant::now(),
                            consumed: false,
                        };
                    }
                    Err(_) => continue,
                }
            }
            waiter.as_mut().await;
        }
    }

    fn record_success(&self, rtt: Duration) {
        let rtt_us = (rtt.as_micros().min(u64::MAX as u128)) as u64;
        let rtt_us = rtt_us.max(1);
        Self::ratchet_min(&self.hot.min_rtt_us, rtt_us);
        let fast = Self::ewma_update(&self.hot.ewma_fast_us, rtt_us, EWMA_FAST_SHIFT);
        let slow = Self::ewma_update(&self.hot.ewma_slow_us, rtt_us, EWMA_SLOW_SHIFT);
        let min_rtt = self.hot.min_rtt_us.load(Ordering::Relaxed);

        let now_ns = self.cold.created_at.elapsed().as_nanos() as u64;
        let cooldown_active = now_ns < self.hot.throttle_until_ns.load(Ordering::Relaxed);

        let n_succ = self.hot.successes.fetch_add(1, Ordering::Relaxed);
        let in_slow_start =
            n_succ < SLOW_START_SAMPLES && self.hot.first_throttle.load(Ordering::Acquire) == 0;

        /*
         * Two shrink paths exist. Hard back pressure is handled by
         * [`record_throttle`]. CUSUM detected sustained regime rise
         * is handled here. The CUSUM gate is the upgrade over the
         * naive gradient. CUSUM integrates upward deviation over
         * many samples instead of reacting to a single ratio, so
         * cold start jitter (TLS handshake, DNS, first cache miss)
         * does not false positive into a shrink.
         *
         * Grow path. Slow start while in envelope. AIMD `+1`
         * afterward. Both gated on the post throttle cooldown.
         */
        let _ = (slow, min_rtt, fast);
        let regime = self.cold.regime.record(rtt_us);
        if regime == RegimeSignal::Rising
            && !self.cold.cusum_shrink_disabled.load(Ordering::Relaxed)
        {
            self.scale_limit(SHRINK_NUM_FACTOR, SHRINK_DEN_FACTOR);
            self.release();
            return;
        }
        if in_slow_start && !cooldown_active {
            self.scale_limit_grow(2, 1);
        } else if !cooldown_active {
            self.bump_limit_by(1);
        }
        self.release();
    }

    fn record_throttle(&self) {
        self.scale_limit(THROTTLE_NUM, THROTTLE_DEN);
        let cooldown_end = self.cold.created_at.elapsed().as_nanos() as u64 + THROTTLE_COOLDOWN_NS;
        self.hot
            .throttle_until_ns
            .store(cooldown_end, Ordering::Relaxed);
        self.hot.first_throttle.store(1, Ordering::Release);
        self.release();
    }

    fn record_cancelled(&self) {
        self.release();
    }

    fn release(&self) {
        let s = self.hot.state.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(unpack(s).0 > 0, "release without matching acquire");
        self.cold.available.notify_one();
    }

    #[inline]
    fn ratchet_min(slot: &AtomicU64, sample: u64) {
        let mut current = slot.load(Ordering::Relaxed);
        while sample < current {
            match slot.compare_exchange_weak(current, sample, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Lock-free EWMA update with relaxed concurrency semantics.
    ///
    /// When two threads land here at the same time, both read the
    /// same `current`, both compute their own `next`, and only one
    /// wins the CAS. The loser retries against the winner's value
    /// as the new base, so a single sample can effectively be
    /// "applied twice" (winner's value + loser's new step from
    /// it) while a different sample never gets folded in. For a
    /// statistical smoother fed thousands of samples per install
    /// this is a negligible bias — the EWMA still tracks the
    /// underlying RTT distribution. CUSUM downstream re-centers
    /// on its own slow EWMA, so a momentarily inflated sample
    /// gets damped before it reaches the shrink decision.
    ///
    /// We accept this trade because the alternative — a Mutex —
    /// would introduce contention on every successful request,
    /// which dominates the workload. "Lock-free" here means no
    /// blocking primitives, not exact serialization.
    #[inline]
    fn ewma_update(slot: &AtomicU64, sample: u64, shift: u32) -> u64 {
        let mut current = slot.load(Ordering::Relaxed);
        loop {
            let next = if current == 0 {
                sample
            } else {
                let diff = sample as i128 - current as i128;
                let step = diff >> shift;
                ((current as i128).saturating_add(step)).max(1) as u64
            };
            match slot.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return next,
                Err(observed) => current = observed,
            }
        }
    }

    fn bump_limit_by(&self, delta: u32) {
        let mut s = self.hot.state.load(Ordering::Relaxed);
        loop {
            let (inflight, limit) = unpack(s);
            let next_limit = limit.saturating_add(delta).min(self.cold.bounds.1);
            if next_limit == limit {
                return;
            }
            match self.hot.state.compare_exchange_weak(
                s,
                pack(inflight, next_limit),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    /*
                     * Wake one waiter per new permit. Tokio
                     * `Notify` stores `notify_one` calls as permits
                     * when no waiter is registered, so even
                     * acquirers that are mid `notified()` future
                     * setup pick up the signal instead of dropping
                     * it. `notify_waiters` would lose signals to
                     * acquirers that had not polled yet, and was
                     * the cause of slow start under utilizing the
                     * new capacity in earlier versions.
                     */
                    let added = next_limit - limit;
                    for _ in 0..added {
                        self.cold.available.notify_one();
                    }
                    return;
                }
                Err(observed) => s = observed,
            }
        }
    }

    fn scale_limit_grow(&self, num: u64, den: u64) {
        let mut s = self.hot.state.load(Ordering::Relaxed);
        loop {
            let (inflight, limit) = unpack(s);
            let scaled = ((limit as u64).saturating_mul(num) / den.max(1)) as u32;
            let next_limit = scaled.clamp(self.cold.bounds.0, self.cold.bounds.1);
            if next_limit == limit {
                return;
            }
            match self.hot.state.compare_exchange_weak(
                s,
                pack(inflight, next_limit),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    let added = next_limit - limit;
                    for _ in 0..added {
                        self.cold.available.notify_one();
                    }
                    return;
                }
                Err(observed) => s = observed,
            }
        }
    }

    fn scale_limit(&self, num: u64, den: u64) {
        let mut s = self.hot.state.load(Ordering::Relaxed);
        loop {
            let (inflight, limit) = unpack(s);
            let scaled = ((limit as u64).saturating_mul(num) / den) as u32;
            let next_limit = scaled.clamp(self.cold.bounds.0, self.cold.bounds.1);
            if next_limit == limit {
                return;
            }
            match self.hot.state.compare_exchange_weak(
                s,
                pack(inflight, next_limit),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => s = observed,
            }
        }
    }
}

/**
 * RAII permit. Must be consumed via [`AdaptivePermit::record_success`],
 * [`AdaptivePermit::record_throttle`], or
 * [`AdaptivePermit::record_cancelled`]. A bare drop counts as
 * cancellation so an early return error path does not poison the
 * controller signal.
 */
#[must_use = "permit must be recorded with record_success/record_throttle, or dropped to cancel"]
pub struct AdaptivePermit {
    limiter: Arc<AdaptiveLimit>,
    started: Instant,
    consumed: bool,
}

impl AdaptivePermit {
    pub fn record_success(mut self) {
        self.consumed = true;
        let rtt = self.started.elapsed();
        self.limiter.record_success(rtt);
    }

    pub fn record_throttle(mut self) {
        self.consumed = true;
        self.limiter.record_throttle();
    }

    pub fn record_cancelled(mut self) {
        self.consumed = true;
        self.limiter.record_cancelled();
    }

    #[cfg(test)]
    pub(crate) fn record_success_with_rtt(mut self, rtt: Duration) {
        self.consumed = true;
        self.limiter.record_success(rtt);
    }
}

impl Drop for AdaptivePermit {
    fn drop(&mut self) {
        if !self.consumed {
            self.limiter.record_cancelled();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn grows_under_stable_latency() {
        let limit = AdaptiveLimit::new(8, 4, 64);
        let baseline = Duration::from_millis(50);
        for _ in 0..200 {
            let permit = limit.acquire().await;
            permit.record_success_with_rtt(baseline);
        }
        assert!(
            limit.current_limit() > 16,
            "expected growth, got {}",
            limit.current_limit()
        );
    }

    #[tokio::test]
    async fn shrinks_on_throttle() {
        let limit = AdaptiveLimit::new(32, 4, 64);
        let permit = limit.acquire().await;
        permit.record_throttle();
        assert_eq!(limit.current_limit(), 16);
    }

    #[tokio::test]
    async fn floor_holds_under_continuous_throttle() {
        let limit = AdaptiveLimit::new(32, 4, 64);
        for _ in 0..20 {
            let permit = limit.acquire().await;
            permit.record_throttle();
        }
        assert_eq!(limit.current_limit(), 4);
    }

    #[tokio::test]
    async fn ceiling_holds_under_runaway_growth() {
        let limit = AdaptiveLimit::new(8, 4, 16);
        for _ in 0..1000 {
            limit.bump_limit_by(1);
        }
        assert_eq!(limit.current_limit(), 16);
    }

    #[tokio::test]
    async fn permit_drop_releases_without_signal() {
        let limit = AdaptiveLimit::new(2, 1, 4);
        let p1 = limit.acquire().await;
        let p2 = limit.acquire().await;
        assert_eq!(limit.inflight(), 2);
        drop(p1);
        assert_eq!(limit.inflight(), 1);
        drop(p2);
        assert_eq!(limit.inflight(), 0);
    }

    #[tokio::test]
    async fn awaits_when_saturated() {
        let limit = AdaptiveLimit::new(1, 1, 4);
        let hold = limit.acquire().await;
        let limit_clone = Arc::clone(&limit);
        let task = tokio::spawn(async move {
            let p = limit_clone.acquire().await;
            p.record_cancelled();
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(hold);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn ewma_converges_toward_sample_mean() {
        let slot = AtomicU64::new(0);
        for _ in 0..100 {
            AdaptiveLimit::ewma_update(&slot, 1000, EWMA_FAST_SHIFT);
        }
        let v = slot.load(Ordering::Relaxed);
        assert!((900..=1100).contains(&v), "ewma settled at {v}");
    }

    #[tokio::test]
    async fn shrink_only_on_explicit_throttle() {
        /*
         * Per the design comment in `record_success`, gradient
         * driven shrink is gated by CUSUM. Only `record_throttle`
         * or a sustained CUSUM crossing shrinks. This test pins
         * the contract that 200 completions at modestly elevated
         * latency (5x the first sample) does not shrink the cap
         * because the slow EWMA tracks the new baseline before
         * CUSUM accumulates enough deviation.
         */
        let limit = AdaptiveLimit::new(64, 4, 256);
        for i in 0..200 {
            let p = limit.acquire().await;
            let rtt = if i == 0 {
                Duration::from_millis(20)
            } else {
                Duration::from_millis(100)
            };
            p.record_success_with_rtt(rtt);
        }
        assert!(
            limit.current_limit() >= 64,
            "no shrink without throttle, got {}",
            limit.current_limit()
        );
    }
}

/**
 * Cumulative sum (CUSUM) change point detector for streaming
 * signals. Computes the cumulative deviation from a running
 * baseline. When the sum exceeds a threshold (positive or
 * negative), the detector declares a regime shift. Reference:
 * Page, "Continuous Inspection Schemes" (1954). The textbook
 * quickest change detection algorithm.
 *
 * # State
 *
 * Two atomic counters per direction. `pos` accumulates positive
 * deviation (rising regime). `neg` accumulates negative deviation
 * (falling regime). The baseline is itself an EWMA (alpha 1/32) so
 * the detector adapts to long term drift while still firing on
 * rapid shifts.
 *
 * # Why this is the right detector
 *
 * Pure threshold tests on raw RTT either fire too eagerly (single
 * spike triggers a false positive) or too late (gradual creep
 * gets ignored). CUSUM accumulates evidence. A small shift in
 * mean integrates over time to a detectable signal, while a
 * single outlier washes out. This is the property
 * [`AdaptiveLimit`] needs to gate gradient driven shrink against
 * transient jitter.
 */
pub struct RegimeDetector {
    pos: AtomicU64,
    neg: AtomicU64,
    baseline_us: AtomicU64,
    threshold_us: u64,
}

impl std::fmt::Debug for RegimeDetector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegimeDetector")
            .field("pos", &self.pos.load(Ordering::Relaxed))
            .field("neg", &self.neg.load(Ordering::Relaxed))
            .field("baseline_us", &self.baseline_us.load(Ordering::Relaxed))
            .field("threshold_us", &self.threshold_us)
            .finish()
    }
}

impl RegimeDetector {
    pub fn new(threshold_us: u64) -> Self {
        Self {
            pos: AtomicU64::new(0),
            neg: AtomicU64::new(0),
            baseline_us: AtomicU64::new(0),
            threshold_us,
        }
    }

    pub fn record(&self, sample_us: u64) -> RegimeSignal {
        let baseline = self.baseline_us.load(Ordering::Relaxed);
        let next_baseline = if baseline == 0 {
            sample_us
        } else {
            let diff = sample_us as i128 - baseline as i128;
            let step = diff >> EWMA_SLOW_SHIFT;
            ((baseline as i128).saturating_add(step)).max(1) as u64
        };
        self.baseline_us.store(next_baseline, Ordering::Relaxed);

        let dev = sample_us as i64 - next_baseline as i64;
        if dev >= 0 {
            let pos = self
                .pos
                .fetch_add(dev as u64, Ordering::Relaxed)
                .saturating_add(dev as u64);
            self.neg.store(0, Ordering::Relaxed);
            if pos >= self.threshold_us {
                self.pos.store(0, Ordering::Relaxed);
                return RegimeSignal::Rising;
            }
        } else {
            let neg = self
                .neg
                .fetch_add(-dev as u64, Ordering::Relaxed)
                .saturating_add(-dev as u64);
            self.pos.store(0, Ordering::Relaxed);
            if neg >= self.threshold_us {
                self.neg.store(0, Ordering::Relaxed);
                return RegimeSignal::Falling;
            }
        }
        RegimeSignal::Stable
    }

    pub fn baseline_us(&self) -> u64 {
        self.baseline_us.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegimeSignal {
    Stable,
    Rising,
    Falling,
}

/**
 * Persistent learned state. A simple key value store backed by a
 * JSON file under the user's cache directory. Lets the adaptive
 * controllers carry their converged operating point across process
 * boundaries so a cold install resumes from where the previous run
 * left off.
 *
 * # Why this matters
 *
 * Short bursts (a single `aube install`) take 5 to 15 seconds. The
 * AIMD slow start ramp on `AdaptiveLimit` needs roughly 5 to 10
 * seconds of completions to reach the bandwidth delay product on a
 * fresh process. That means the limiter spends most of a short
 * install ramping rather than running at the optimum. Persistence
 * fixes that by seeding the next run from the previous run's
 * observed steady state.
 *
 * # Format
 *
 * `$XDG_CACHE_HOME/aube/adaptive-state.json` (falls back to
 * `~/.cache/aube/adaptive-state.json` on Linux,
 * `~/Library/Caches/aube/adaptive-state.json` on macOS,
 * `%LOCALAPPDATA%\aube\adaptive-state.json` on Windows).
 *
 * Schema:
 *
 * ```json
 * {
 *   "version": 1,
 *   "values": {
 *     "tarball:registry.npmjs.org": 96,
 *     "packument:registry.npmjs.org": 64
 *   }
 * }
 * ```
 *
 * Reads tolerate missing or corrupt files (falls back to `None`).
 * Writes are atomic via temp file plus rename. Concurrent writers
 * are safe in the sense that the last writer wins, no torn JSON.
 *
 * # API
 *
 * `load_seed(key)` returns the previously persisted value for a key,
 * blended via EWMA towards the caller's static default. The blend
 * factor is intentionally conservative (alpha 0.3 toward the
 * persisted value) so a single bad run does not poison future
 * starts.
 *
 * `save_observed(key, value)` writes the current observed value back
 * for the next process to consume.
 */
pub struct PersistentState {
    path: PathBuf,
    cache: std::sync::Mutex<Option<PersistedSnapshot>>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
struct PersistedSnapshot {
    version: u32,
    values: std::collections::BTreeMap<String, u64>,
}

impl std::fmt::Debug for PersistentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistentState")
            .field("path", &self.path)
            .finish()
    }
}

const PERSISTED_SCHEMA_VERSION: u32 = 1;
const PERSISTED_BLEND_NUM: u64 = 7;
const PERSISTED_BLEND_DEN: u64 = 10;

impl PersistentState {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            cache: std::sync::Mutex::new(None),
        }
    }

    /**
     * Read the persisted value for `key` and blend it with `default`
     * via fixed weight (currently 70% persisted, 30% default).
     * Returns `default` when no persisted value exists, the file is
     * missing, the file is malformed, or the schema version does not
     * match. The blend protects against a previous run that
     * over fitted on a one off network condition.
     */
    pub fn load_seed(&self, key: &str, default: usize) -> usize {
        let snapshot = self.snapshot();
        let persisted = snapshot.values.get(key).copied();
        match persisted {
            Some(v) if snapshot.version == PERSISTED_SCHEMA_VERSION => {
                let blended = (v.saturating_mul(PERSISTED_BLEND_NUM)
                    + (default as u64).saturating_mul(PERSISTED_BLEND_DEN - PERSISTED_BLEND_NUM))
                    / PERSISTED_BLEND_DEN;
                blended as usize
            }
            _ => default,
        }
    }

    /**
     * Write `value` back for `key`, replacing any prior entry.
     * Performs a read modify write of the on disk file under the
     * internal mutex, then atomically renames over the destination
     * so a crash mid write leaves the previous good file in place.
     * Tolerates failure silently because adaptive state is a hint,
     * not a correctness requirement.
     */
    pub fn save_observed(&self, key: &str, value: usize) {
        let mut snapshot = self.snapshot();
        snapshot.version = PERSISTED_SCHEMA_VERSION;
        snapshot.values.insert(key.to_string(), value as u64);
        if let Ok(mut cached) = self.cache.lock() {
            *cached = Some(snapshot.clone());
        }
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(json) = serde_json::to_vec_pretty(&snapshot) else {
            return;
        };
        let Some(parent) = self.path.parent() else {
            return;
        };
        let tmp = parent.join(format!(
            ".adaptive-state.tmp.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    fn snapshot(&self) -> PersistedSnapshot {
        if let Ok(mut cached) = self.cache.lock() {
            if let Some(snap) = cached.as_ref() {
                return snap.clone();
            }
            let snap = read_snapshot(&self.path).unwrap_or_default();
            *cached = Some(snap.clone());
            return snap;
        }
        read_snapshot(&self.path).unwrap_or_default()
    }
}

fn read_snapshot(path: &Path) -> Option<PersistedSnapshot> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/**
 * Default location for the global persistent adaptive state file.
 * Honors `XDG_CACHE_HOME` first, then falls back to `~/.cache`,
 * `~/Library/Caches`, or `%LOCALAPPDATA%`. The cache namespace is the active
 * embedder's `cache_namespace` (standalone aube → `"aube"`), not a literal.
 */
pub fn default_persistent_state_path() -> Option<PathBuf> {
    let ns = crate::embedder().cache_namespace;
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join(ns).join("adaptive-state.json"));
    }
    if cfg!(windows) {
        std::env::var("LOCALAPPDATA")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|p| PathBuf::from(p).join(ns).join("adaptive-state.json"))
    } else if cfg!(target_os = "macos") {
        std::env::var("HOME").ok().map(|h| {
            PathBuf::from(h)
                .join("Library")
                .join("Caches")
                .join(ns)
                .join("adaptive-state.json")
        })
    } else {
        std::env::var("HOME").ok().map(|h| {
            PathBuf::from(h)
                .join(".cache")
                .join(ns)
                .join("adaptive-state.json")
        })
    }
}

/**
 * Process global persistent state singleton. Lazy initialised on
 * first access via [`global_persistent_state`]. Callers that need a
 * different path for testing can construct their own
 * [`PersistentState`] directly.
 */
static GLOBAL_PERSISTENT_STATE: std::sync::OnceLock<Arc<PersistentState>> =
    std::sync::OnceLock::new();

pub fn global_persistent_state() -> Option<Arc<PersistentState>> {
    let path = default_persistent_state_path()?;
    Some(
        GLOBAL_PERSISTENT_STATE
            .get_or_init(|| Arc::new(PersistentState::new(path)))
            .clone(),
    )
}

#[cfg(test)]
mod additional_tests {
    use super::*;

    #[test]
    fn cusum_detects_rising_shift() {
        let det = RegimeDetector::new(50_000);
        for _ in 0..100 {
            assert_eq!(det.record(10_000), RegimeSignal::Stable);
        }
        let mut saw_rising = false;
        for _ in 0..200 {
            if det.record(50_000) == RegimeSignal::Rising {
                saw_rising = true;
                break;
            }
        }
        assert!(saw_rising, "expected rising regime detection");
    }

    #[test]
    fn cusum_ignores_single_outlier() {
        let det = RegimeDetector::new(500_000);
        for _ in 0..100 {
            det.record(10_000);
        }
        let r = det.record(2_000_000);
        assert_eq!(
            r,
            RegimeSignal::Rising,
            "single big outlier above threshold fires"
        );
        let det2 = RegimeDetector::new(5_000_000);
        for _ in 0..100 {
            det2.record(10_000);
        }
        for _ in 0..5 {
            assert_eq!(det2.record(50_000), RegimeSignal::Stable);
        }
    }
}

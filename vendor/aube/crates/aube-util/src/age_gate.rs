//! Process-global sinks for the two ways `minimumReleaseAge` moves a version
//! pick off the one the range would otherwise have produced.
//!
//! **Immature fallback picks.** With `minimumReleaseAge` set in loose mode (the
//! default), the resolver falls back to the lowest satisfying version when every
//! candidate is younger than the cutoff. pnpm does the same, then auto-persists
//! that package to `minimumReleaseAgeExclude` so a later verify-lockfile pass
//! accepts the pin. The resolver here does the fallback but assigns it no
//! policy; an embedder that wants pnpm's auto-persist ARMS that sink before a
//! resolve, DRAINS it after, and writes the exclude entry in whatever config
//! surface its identity model dictates.
//!
//! **Gate downgrades.** The opposite direction: a `latest` request the cutoff
//! blocked, answered with the newest release that clears it instead (#681).
//! Nothing to persist here — the pick is mature and correct — but the caller ran
//! an older tool than it asked for and deserves to be told, which matters most
//! for `dlx`, whose scratch project is deleted before it could be inspected.
//!
//! Both are disarmed by default: standalone aube arms nothing, records nothing,
//! and its behavior is unchanged. Each record check is a single atomic load,
//! reached only on an actual off-range pick.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static ARMED: AtomicBool = AtomicBool::new(false);
static PICKS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

static DOWNGRADES_ARMED: AtomicBool = AtomicBool::new(false);
static DOWNGRADES: Mutex<Vec<AgeGateDowngrade>> = Mutex::new(Vec::new());

/// A `latest` pick the release-age gate steered downward: `picked` is the
/// newest release clearing the cutoff, `blocked` the `dist-tags.latest` that
/// did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgeGateDowngrade {
    pub name: String,
    pub picked: String,
    pub blocked: String,
}

/// Arm gate-downgrade collection for the next resolve, clearing any prior
/// entries.
pub fn arm_age_gate_downgrade_collection() {
    if let Ok(mut d) = DOWNGRADES.lock() {
        d.clear();
    }
    DOWNGRADES_ARMED.store(true, Ordering::Release);
}

/// Record a `latest` request the cutoff steered to an older mature release.
/// No-op unless armed.
pub fn record_age_gate_downgrade(name: &str, picked: &str, blocked: &str) {
    if !DOWNGRADES_ARMED.load(Ordering::Acquire) {
        return;
    }
    if let Ok(mut d) = DOWNGRADES.lock() {
        d.push(AgeGateDowngrade {
            name: name.to_string(),
            picked: picked.to_string(),
            blocked: blocked.to_string(),
        });
    }
}

/// Disarm and drain every recorded downgrade, in record order.
pub fn take_age_gate_downgrades() -> Vec<AgeGateDowngrade> {
    DOWNGRADES_ARMED.store(false, Ordering::Release);
    match DOWNGRADES.lock() {
        Ok(mut d) => std::mem::take(&mut *d),
        Err(_) => Vec::new(),
    }
}

/// Arm collection for the next resolve, clearing any prior picks. The embedder
/// calls this before an install/add/update resolve it wants to auto-persist.
pub fn arm_age_gated_fallback_pick_collection() {
    if let Ok(mut picks) = PICKS.lock() {
        picks.clear();
    }
    ARMED.store(true, Ordering::Release);
}

/// Record an immature fallback pick as `(registry_name, version)`. No-op unless
/// armed — the standalone-aube path returns after one relaxed atomic load.
pub fn record_age_gated_fallback_pick(name: &str, version: &str) {
    if !ARMED.load(Ordering::Acquire) {
        return;
    }
    if let Ok(mut picks) = PICKS.lock() {
        picks.push((name.to_string(), version.to_string()));
    }
}

/// Disarm and drain every recorded pick, in record order (the caller dedups).
/// The embedder calls this once, after the resolve completes.
pub fn take_age_gated_fallback_picks() -> Vec<(String, String)> {
    ARMED.store(false, Ordering::Release);
    match PICKS.lock() {
        Ok(mut picks) => std::mem::take(&mut *picks),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Single test: the sink is process-global, so exercising the whole
    // disarmed → armed → drained lifecycle in one test avoids cross-test races.
    #[test]
    fn lifecycle_disarmed_is_noop_armed_collects_take_drains_and_disarms() {
        // Disarmed by default: records are dropped.
        record_age_gated_fallback_pick("a", "1.0.0");
        assert!(take_age_gated_fallback_picks().is_empty());

        // Armed: records land, in order; take drains them.
        arm_age_gated_fallback_pick_collection();
        record_age_gated_fallback_pick("caniuse-lite", "1.0.30001700");
        record_age_gated_fallback_pick("vite", "6.0.0");
        assert_eq!(
            take_age_gated_fallback_picks(),
            vec![
                ("caniuse-lite".to_string(), "1.0.30001700".to_string()),
                ("vite".to_string(), "6.0.0".to_string()),
            ]
        );

        // take disarmed the sink: subsequent records are dropped again.
        record_age_gated_fallback_pick("b", "2.0.0");
        assert!(take_age_gated_fallback_picks().is_empty());
    }
}

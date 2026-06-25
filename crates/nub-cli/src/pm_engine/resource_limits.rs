//! Detect the effective process/thread ceiling the install runs under, so the
//! engine's concurrency (tokio worker + blocking pool, parallel build-script
//! count) can be bounded BELOW it on a constrained box.
//!
//! WHY this exists: `nub ci` intermittently aborted with exit 101 on
//! resource-constrained CI (Vercel). Root cause — at the install tail the tokio
//! runtime must grow an OS thread (`spawn_blocking` for CAS save/restore, fanned
//! out concurrently with the parallel native postinstalls), `clone(2)` returns
//! `EAGAIN` under peak PID/thread pressure, and tokio's INTERNAL thread growth
//! PANICS on that failure. Under v0.2's `panic = "abort"` that panic aborts the
//! whole install. We cannot guard inside tokio, and `catch_unwind` cannot save a
//! panic=abort process — so the only in-process fix is to PREVENT the
//! exhaustion: keep the peak thread+process count safely under the box's ceiling.
//!
//! DESIGN — tighten ONLY under a DETECTED constraint. On an unconstrained box
//! (no cgroup PID cap, generous `RLIMIT_NPROC`) every detector returns `None`
//! and the caller keeps its full-speed defaults — so normal-box install
//! performance is untouched. The cap engages exactly when the environment is the
//! hostile one that triggers the abort.

/// The effective ceiling on the number of processes/threads this install may
/// create, derived from the most restrictive of: cgroup v2 `pids.max`,
/// `RLIMIT_NPROC` (soft), and the current thread/process headroom. `None` means
/// "no meaningful constraint detected — use full-speed defaults."
///
/// The returned value is a HEADROOM budget: roughly how many additional OS
/// threads/processes we can create before hitting the ceiling, already
/// discounted by a safety margin and an estimate of threads/processes already
/// live. It is intentionally conservative — under-counting headroom degrades to
/// "a bit slower," over-counting risks the abort we are preventing.
#[cfg(target_os = "linux")]
pub(crate) fn spawn_headroom() -> Option<usize> {
    let pids_max = cgroup_pids_max();
    let rlimit = rlimit_nproc_soft();
    let in_use = current_thread_count().unwrap_or(64) as u64;
    headroom_from(pids_max, rlimit, in_use)
}

// `UNCONSTRAINED_FLOOR`, `SAFETY_MARGIN`, `headroom_from`, `parse_pids_max` are
// exercised on Linux (the only platform that detects a ceiling) and by the
// cross-platform unit tests; on other targets the detector short-circuits to
// `None`, so they're dead outside tests there — hence the conditional allow.

/// A very high ceiling is effectively unconstrained — don't tighten. 4096 is
/// comfortably above what a normal install peaks at (a few hundred), so above it
/// we keep full-speed defaults.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const UNCONSTRAINED_FLOOR: u64 = 4096;
/// Slack reserved below the ceiling for threads/processes we can't precount
/// (tokio bookkeeping, the linker rayon pool, transient grandchildren).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const SAFETY_MARGIN: u64 = 64;

/// Pure budget computation, split out so the clamp logic is unit-testable with
/// synthetic ceilings (the real detectors read `/proc` + `getrlimit`). Returns
/// the spawn HEADROOM: room left below the most-restrictive ceiling, discounted
/// by what's already live and a safety margin. `None` = no meaningful constraint.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn headroom_from(pids_max: Option<u64>, rlimit: Option<u64>, in_use: u64) -> Option<usize> {
    // The hard ceiling is the smaller of the two limits (whichever the kernel
    // enforces first). If neither is set, there is no constraint.
    let ceiling = match (pids_max, rlimit) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    if ceiling >= UNCONSTRAINED_FLOOR {
        return None;
    }
    let budget = ceiling.saturating_sub(in_use).saturating_sub(SAFETY_MARGIN);
    // Never report zero/one — that would serialize everything. A budget under a
    // small floor still means "constrained, go minimal."
    Some(budget.max(2) as usize)
}

/// One spawn-headroom budget divided across the FOUR concurrent OS-thread/process
/// consumers of an install — they all draw from the SAME PID budget, so the
/// shares must SUM within it, not each be `min(budget, …)` (which would let the
/// sum blow past the ceiling). Returns
/// `(tokio_workers, tokio_blocking, rayon_global, build_script_concurrency)`.
/// Pure + testable.
///
/// The two big thread pools (tokio blocking — tarball decode + CAS writes; rayon
/// global — the same CAS/delta/fetch fan-out) get the bulk; tokio workers are few
/// (the install is IO-bound); the build-script fan-out spawns native
/// grandchildren, budgeted conservatively. The three THREAD-pool shares
/// (workers + blocking + rayon) are what must fit the PID headroom.
pub(crate) fn split_budget(headroom: usize) -> (usize, usize, usize, usize) {
    // Proportional shares, each clamped to a working floor. For a tiny budget the
    // floors can sum above it — acceptable: a sub-10-PID-headroom box is already
    // past saving, and the per-spawn EAGAIN guards + retry are the backstop there
    // (NOT this sum-fitting). For any realistically-constrained box (headroom in
    // the dozens–hundreds) the thread-pool shares sum to ≤ headroom.
    let workers = (headroom / 8).clamp(2, 8);
    let blocking = (headroom / 3).max(4);
    let rayon = (headroom / 4).clamp(2, 64);
    let build = (headroom / 8).clamp(1, 5);
    (workers, blocking, rayon, build)
}

/// Non-Linux platforms (macOS, Windows) have no cgroup PID controller, and the
/// abort was only ever observed on Linux CI. `RLIMIT_NPROC` exists on macOS but
/// is generous by default; we treat non-Linux as unconstrained to avoid
/// regressing normal-box behavior on platforms that never exhibited the bug.
#[cfg(not(target_os = "linux"))]
pub(crate) fn spawn_headroom() -> Option<usize> {
    None
}

/// The cgroup `pids.max` limit for the current process, trying cgroup v2
/// (unified) first and falling back to cgroup v1 (the `pids` controller). Many
/// CI/container hosts — including the ones that triggered this bug — are still
/// v1 or hybrid, so v1 coverage is load-bearing, not optional. `None` = no
/// cgroup pids limit found (or set to `max`).
#[cfg(target_os = "linux")]
fn cgroup_pids_max() -> Option<u64> {
    cgroup_v2_pids_max().or_else(cgroup_v1_pids_max)
}

/// cgroup v2 (unified): the current cgroup is named in `/proc/self/cgroup` as
/// `0::<relpath>`; `pids.max` lives at `/sys/fs/cgroup/<relpath>/pids.max`.
/// Resolve the relpath so a NESTED cgroup (the common CI case) reads its OWN
/// limit rather than the (usually-`max`) root.
#[cfg(target_os = "linux")]
fn cgroup_v2_pids_max() -> Option<u64> {
    let rel = std::fs::read_to_string("/proc/self/cgroup")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("0::").map(str::to_string))?;
    let rel = rel.trim_start_matches('/');
    let path = format!("/sys/fs/cgroup/{rel}/pids.max");
    match std::fs::read_to_string(&path) {
        Ok(raw) => parse_pids_max(&raw),
        Err(_) => {
            // Don't silently fall back to the root cgroup's pids.max — the root is
            // almost always `max`, so that read would masquerade "constrained" as
            // "unconstrained" (the unsafe direction). Returning `None` here lets
            // the v1 probe and `RLIMIT_NPROC` still contribute.
            tracing::debug!(%path, "cgroup v2 pids.max unreadable at the nested path");
            None
        }
    }
}

/// cgroup v1: the `pids` controller is named in `/proc/self/cgroup` as a line
/// `<id>:pids:<relpath>`; the limit lives at
/// `/sys/fs/cgroup/pids/<relpath>/pids.max`.
///
/// Safe no-op on a pure cgroup v2 host: there the only line is `0::<path>`, whose
/// middle (controller) field is EMPTY, so the `c == "pids"` match never fires and
/// this returns `None` — the v2 probe already handled that host.
#[cfg(target_os = "linux")]
fn cgroup_v1_pids_max() -> Option<u64> {
    let rel = std::fs::read_to_string("/proc/self/cgroup")
        .ok()?
        .lines()
        .find_map(|l| {
            // Format: `hierarchy-id:controller-list:cgroup-path`. Match the
            // controller field containing `pids`.
            let mut parts = l.splitn(3, ':');
            let _id = parts.next()?;
            let controllers = parts.next()?;
            let path = parts.next()?;
            controllers
                .split(',')
                .any(|c| c == "pids")
                .then(|| path.to_string())
        })?;
    let rel = rel.trim_start_matches('/');
    let raw = std::fs::read_to_string(format!("/sys/fs/cgroup/pids/{rel}/pids.max")).ok()?;
    parse_pids_max(&raw)
}

/// Parse a `pids.max` value: a decimal count, or the literal `max` (= no limit
/// → `None`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_pids_max(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw == "max" {
        return None;
    }
    raw.parse::<u64>().ok()
}

/// Soft `RLIMIT_NPROC` (max user processes). `RLIM_INFINITY` → `None`.
#[cfg(target_os = "linux")]
fn rlimit_nproc_soft() -> Option<u64> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes into the provided `rlimit` out-param; the
    // pointer is valid for the duration of the call.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_NPROC, &mut lim) };
    if rc != 0 {
        return None;
    }
    if lim.rlim_cur == libc::RLIM_INFINITY {
        return None;
    }
    // `rlim_t` is `u64` on every Rust Linux target (gnu + musl), so no cast.
    Some(lim.rlim_cur)
}

/// Best-effort count of threads currently live in this process, from
/// `/proc/self/status`'s `Threads:` field. Used to discount the ceiling by
/// what's already in flight.
#[cfg(target_os = "linux")]
fn current_thread_count() -> Option<usize> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("Threads:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headroom_is_none_or_positive() {
        // The detector must never return `Some(0)` — a zero budget would
        // serialize the whole install. On the dev/CI host it's typically `None`
        // (unconstrained); under a tight cgroup it's a small positive number.
        match spawn_headroom() {
            None => {}
            Some(n) => assert!(n >= 2, "budget must be at least 2, got {n}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rlimit_nproc_is_readable_or_infinite() {
        // Either a finite soft limit or `None` (RLIM_INFINITY) — never a panic.
        let _ = rlimit_nproc_soft();
    }

    #[test]
    fn headroom_none_when_unconstrained() {
        // No limits at all, or a ceiling at/above the unconstrained floor → None.
        assert_eq!(headroom_from(None, None, 50), None);
        assert_eq!(headroom_from(Some(UNCONSTRAINED_FLOOR), None, 50), None);
        assert_eq!(headroom_from(Some(100_000), Some(200_000), 50), None);
    }

    #[test]
    fn headroom_picks_the_most_restrictive_ceiling() {
        // The smaller of pids.max and RLIMIT_NPROC wins; budget discounts in_use
        // and the safety margin.
        let h = headroom_from(Some(512), Some(1024), 64).unwrap();
        assert_eq!(h, 512 - 64 - SAFETY_MARGIN as usize);
        // RLIMIT smaller than pids.max.
        let h2 = headroom_from(Some(1024), Some(300), 64).unwrap();
        assert_eq!(h2, 300 - 64 - SAFETY_MARGIN as usize);
    }

    #[test]
    fn headroom_floors_at_two_under_extreme_pressure() {
        // A ceiling barely above in_use + margin must not return 0/1.
        assert_eq!(headroom_from(Some(70), None, 64), Some(2));
        assert_eq!(headroom_from(Some(10), None, 64), Some(2));
    }

    #[test]
    fn split_budget_thread_pools_sum_within_budget_for_real_constraints() {
        // For any realistically-constrained box (headroom ≥ ~24) the THREE
        // thread-pool shares (tokio workers + tokio blocking + rayon global) must
        // sum within the PID headroom — they all draw from the same budget.
        for headroom in [24usize, 40, 64, 128, 256, 512, 1000] {
            let (w, b, rayon, build) = split_budget(headroom);
            assert!(
                w >= 2 && b >= 4 && rayon >= 2 && build >= 1,
                "floors hold @ {headroom}"
            );
            assert!(
                w + b + rayon <= headroom,
                "thread pools {w}+{b}+{rayon} exceed budget {headroom}"
            );
            assert!(build <= 5, "build concurrency never above the default of 5");
        }
    }

    #[test]
    fn split_budget_sub_floor_band_holds_floors_without_panic() {
        // Honesty test for the documented sub-floor band: at a tiny headroom the
        // fixed floors (workers≥2, blocking≥4, rayon≥2) intentionally sum ABOVE
        // the budget. That box is already past sum-fitting — the per-spawn EAGAIN
        // guards + retry are the backstop there, NOT this function. We only assert
        // the floors hold and nothing panics/underflows.
        for headroom in [2usize, 4, 6, 8] {
            let (w, b, rayon, build) = split_budget(headroom);
            assert!(w >= 2 && b >= 4 && rayon >= 2 && build >= 1);
        }
    }

    #[test]
    fn parse_pids_max_handles_max_and_numbers() {
        assert_eq!(parse_pids_max("max"), None);
        assert_eq!(parse_pids_max("max\n"), None);
        assert_eq!(parse_pids_max("1024"), Some(1024));
        assert_eq!(parse_pids_max("  256\n"), Some(256));
        assert_eq!(parse_pids_max("garbage"), None);
    }
}

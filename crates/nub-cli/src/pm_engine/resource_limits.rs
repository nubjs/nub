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
    let pids_max = cgroup_v2_pids_max();
    let rlimit = rlimit_nproc_soft();

    // The hard ceiling is the smaller of the two limits (whichever the kernel
    // enforces first). If neither is set, there is no constraint.
    let ceiling = match (pids_max, rlimit) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };

    // A very high ceiling is effectively unconstrained — don't tighten. 4096 is
    // comfortably above what a normal install peaks at (a few hundred), so above
    // it we keep full-speed defaults.
    const UNCONSTRAINED_FLOOR: u64 = 4096;
    if ceiling >= UNCONSTRAINED_FLOOR {
        return None;
    }

    // Discount the ceiling by what's already live plus a safety margin, so the
    // budget is the room we actually have left, not the absolute cap.
    let in_use = current_thread_count().unwrap_or(64) as u64;
    const SAFETY_MARGIN: u64 = 64;
    let budget = ceiling.saturating_sub(in_use).saturating_sub(SAFETY_MARGIN);

    // Never report zero/one — that would serialize everything; the caller clamps
    // to its own floor, but a budget under a small floor still means "constrained,
    // go minimal."
    Some(budget.max(2) as usize)
}

/// Non-Linux platforms (macOS, Windows) have no cgroup PID controller, and the
/// abort was only ever observed on Linux CI. `RLIMIT_NPROC` exists on macOS but
/// is generous by default; we treat non-Linux as unconstrained to avoid
/// regressing normal-box behavior on platforms that never exhibited the bug.
#[cfg(not(target_os = "linux"))]
pub(crate) fn spawn_headroom() -> Option<usize> {
    None
}

/// Read cgroup v2 `pids.max` for the current process. Returns `None` when the
/// file is absent (cgroup v1, no cgroup, non-Linux) or set to `max` (no limit).
#[cfg(target_os = "linux")]
fn cgroup_v2_pids_max() -> Option<u64> {
    // The unified hierarchy mounts the current cgroup's controllers under a path
    // named in /proc/self/cgroup as `0::<relpath>`. The pids controller exposes
    // `pids.max` there. We resolve the relpath rather than assuming the root, so
    // a nested cgroup (the common CI case) reads its OWN limit.
    let rel = std::fs::read_to_string("/proc/self/cgroup")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("0::").map(str::to_string))?;
    let rel = rel.trim_start_matches('/');
    let path = format!("/sys/fs/cgroup/{rel}/pids.max");
    let raw = std::fs::read_to_string(&path)
        // Fall back to the cgroup root, covering a host that doesn't expose the
        // nested path or reports an unhelpful relpath.
        .or_else(|_| std::fs::read_to_string("/sys/fs/cgroup/pids.max"))
        .ok()?;
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
    Some(lim.rlim_cur as u64)
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
}

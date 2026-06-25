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

/// Raise the soft `RLIMIT_NOFILE` (max open file descriptors) to the hard limit,
/// best-effort. WHY: a large install fans tarball fetches, CAS imports, and
/// symlink passes out concurrently, each holding open descriptors. macOS ships a
/// stingy default soft limit (commonly 256) while the hard limit is far higher,
/// so a big dependency tree (e.g. the AWS SDK / aws-cdk-lib) exhausts the soft
/// limit and the install dies with `Too many open files (os error 24)`. The aube
/// engine raises this in its own `inner_main` startup, but nub dispatches the
/// command impls directly and never runs that path — so we mirror the raise here,
/// on the one startup all PM verbs flow through.
///
/// DESIGN: only ever RAISES the soft limit toward the hard ceiling, never lowers
/// it, and silently keeps the existing limit on any failure (an unprivileged
/// process can always raise its soft limit up to the hard limit). When the hard
/// limit is `RLIM_INFINITY` (macOS reports this, but the kernel still enforces
/// `kern.maxfilesperproc`), a direct raise to infinity is rejected, so we fall
/// back to a generous finite target. No-op on platforms without the syscall.
#[cfg(unix)]
pub(crate) fn raise_nofile_limit() {
    // SAFETY: get/setrlimit are sync syscalls reading/writing this process's own
    // resource table. The out-param pointer is valid for the call; failure is a
    // non-zero return, handled below.
    //
    // NOTE: aube's original (startup.rs) emits a `tracing::trace!` on each branch;
    // those are deliberately dropped here — nub has no `tracing` pipeline wired at
    // this site. Re-add them when syncing from aube only if nub gains one.
    unsafe {
        let mut rlim = std::mem::zeroed::<libc::rlimit>();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) != 0 {
            return;
        }
        let before = rlim.rlim_cur;
        if before >= rlim.rlim_max {
            return;
        }
        rlim.rlim_cur = rlim.rlim_max;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) == 0 {
            return;
        }
        // The hard limit was `RLIM_INFINITY` (or otherwise un-grantable as-is);
        // retry with a finite target the kernel will accept.
        rlim.rlim_cur = before.max(10240).min(rlim.rlim_max);
        let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &rlim);
    }
}

#[cfg(not(unix))]
pub(crate) fn raise_nofile_limit() {}

// ───────────────────────── CPU budget (cgroup CFS quota) ─────────────────────────
//
// The CPU analog of `spawn_headroom`. The PID axis (above) is REACTIVE — it sizes
// pools below the kernel thread ceiling to dodge the `clone(2)` EAGAIN abort. This
// axis is PROACTIVE: on a box with a cgroup CPU bandwidth limit (a 0.5-CPU
// Vercel/Lambda/K8s pod) the host still reports many cores, so the pools over-
// subscribe the quota → CFS throttles the process (latency cliffs) and the attendant
// thread growth feeds the SAME PID exhaustion. Sizing CPU-bound pools to the real
// quota prevents both. Rust's `available_parallelism()` already reads cgroup-v2
// `cpu.max` + affinity (since 1.61), but NOT cgroup-v1 quota, and silently drops the
// v2 quota when `sched_getaffinity` is unreadable (sandbox) — `cpu_budget` closes
// both gaps by reading the cgroup files directly, exactly as the PID detector does.
// Default-preserving: an unconstrained box returns `None` and pools keep full cores.

/// The dedicated user override for the effective CPU budget — a hard cap on the
/// auto-detected CPU-count the pools size against. DISTINCT from the existing
/// `NUB_CONCURRENCY` knob, which is aube's tarball-FETCH concurrency (an IO knob,
/// clamped `[8, 256]`); this caps the CPU/thread pools (workers, rayon), so it
/// needs its own name and its own range `[1, cores]`.
const CPU_BUDGET_ENV: &str = "NUB_CPU_BUDGET";

/// The effective CPU count the engine's CPU-bound pools (tokio workers, rayon
/// global) should size against: `min(available_parallelism, cgroup-CFS-quota-cores)`
/// with `max(1, ceil)` rounding. `None` means "no CPU constraint detected — use
/// full-speed defaults," matching today's `available_parallelism()`.
///
/// Auto-detection is `num_cpus::get()`, which reads the cgroup CFS quota for BOTH
/// cgroup v1 (`cpu.cfs_quota_us`/`cfs_period_us`) and v2 (`cpu.max`) — locating the
/// controller via `/proc/self/mountinfo` — and returns `min(ceil(quota/period),
/// logical_cores)`. That is exactly the automaxprocs algorithm and the maintainer-
/// chosen `max(1, ceil)` rounding (any positive quota ceils to ≥1; the `quota == 0`
/// degenerate is guarded). It closes the two gaps in std's `available_parallelism()`
/// that nub would otherwise inherit (no cgroup-v1 quota; the v2 quota is dropped when
/// `sched_getaffinity` is unreadable under a sandbox).
///
/// An explicit `NUB_CPU_BUDGET` always wins over auto-detection (the automaxprocs/Go
/// `GOMAXPROCS`-env precedence model), clamped to `[1, cores]`.
///
/// Returns a budget ONLY when it constrains BELOW the logical core count — at/above
/// the cores it tightens nothing, so we return `None` and the caller keeps its
/// full-speed default (no needless pool rebuild). Linux-gated: `num_cpus`'s quota
/// read is a Linux-cgroup concept, and like `spawn_headroom` the over-report trap
/// was only ever a Linux-container problem; on macOS/Windows only the explicit
/// override can produce a budget.
pub(crate) fn cpu_budget() -> Option<usize> {
    // The override's ceiling is the quota-IGNORING host logical-CPU count, so an
    // EXPLICIT value can raise concurrency ABOVE a detected quota (explicit wins) —
    // not the quota-aware `available_parallelism()`, which would silently clamp the
    // user's request back down to the quota.
    if let Some(n) = cpu_budget_override(host_logical_cpus()) {
        return Some(n);
    }
    #[cfg(target_os = "linux")]
    {
        // `num_cpus::get()` returns min(ceil(quota), logical) reading the cgroup
        // files directly (v1 AND v2, even when affinity is unreadable). Gate on
        // "actually below available_parallelism()" so a box std already sized
        // correctly (v2 + readable affinity) stays `None` — we only ADD the v1 /
        // sandbox coverage std misses.
        cpu_budget_from(available_cores(), num_cpus::get())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Quota-aware logical-core count — `available_parallelism()` reads affinity AND
/// the cgroup-v2 quota (since Rust 1.61), so on a pure-v2 host it already returns
/// the effective budget. The detected-budget gate clamps to this, and
/// `build_runtime` uses it as `raw_cpu`, so both agree on the same `unwrap_or(1)`
/// fallback. NOTE: if `available_parallelism()` ERRORS (a rare seccomp sandbox
/// blocking the syscall), this collapses to 1, which makes the gate
/// `cpu_budget_from(1, _)` return `None` — i.e. CPU-budget DETECTION quietly
/// no-ops there. Harmless: the caller then uses `raw_cpu == 1` and sizes pools
/// minimally anyway; an explicit `NUB_CPU_BUDGET` still works (it's read before
/// this gate).
pub(crate) fn available_cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// The host's online logical-CPU count, IGNORING any cgroup quota — the ceiling an
/// explicit `NUB_CPU_BUDGET` may request up to. On Linux this is
/// `sysconf(_SC_NPROCESSORS_ONLN)` (quota-blind by design); elsewhere there's no
/// quota, so `available_parallelism()` is already the host count.
fn host_logical_cpus() -> usize {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `sysconf` with a valid name has no preconditions and no
        // out-params; it returns the count or -1 on error.
        let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        if n > 0 {
            return n as usize;
        }
    }
    available_cores()
}

/// Read + clamp the `NUB_CPU_BUDGET` override. `Some(n)` (clamped to `[1, ceiling]`)
/// when set to a positive integer; `None` when unset/invalid (auto-detect). A value
/// above the ceiling clamps down rather than erroring — asking for more than the
/// host has just gets the host.
///
/// Note the asymmetry between lowering and raising: the override's primary use is
/// to LOWER concurrency (pin to 1 on a box where auto-detection missed a v1 quota),
/// which is fully honored everywhere. Raising ABOVE the auto-detected v2 quota is
/// honored for the tokio worker count nub sets directly from `cpu_budget()`, but
/// any pool whose ceiling is `available_parallelism()` — rayon/tokio's own internal
/// defaults AND aube's linker pool via `effective_cpu_cap()` — is already
/// quota-clamped by Rust std, so a request above the v2 quota can't lift those past
/// the quota. A deliberately minor edge: you can't conjure CPU the cgroup won't
/// grant, and the common case (lowering) is unaffected.
fn cpu_budget_override(ceiling: usize) -> Option<usize> {
    let raw = std::env::var(CPU_BUDGET_ENV).ok()?;
    parse_override(&raw, ceiling)
}

/// Pure parse+clamp of a `NUB_CPU_BUDGET` value (split out so the clamp/precedence
/// is unit-testable without mutating the process env). `Some(n)` clamped to
/// `[1, ceiling]` for a positive integer; `None` (with a warning) otherwise.
fn parse_override(raw: &str, ceiling: usize) -> Option<usize> {
    match raw.trim().parse::<usize>() {
        Ok(n) if n >= 1 => Some(n.min(ceiling.max(1))),
        _ => {
            tracing::warn!(
                value = %raw,
                "{CPU_BUDGET_ENV} ignored: must be a positive integer; using the auto-detected CPU budget"
            );
            None
        }
    }
}

/// Pure budget gate (testable with synthetic inputs): given the logical core count
/// and `num_cpus::get()`'s already-quota-clamped count, return `Some(detected)` ONLY
/// when it constrains below the cores; otherwise `None` (default-preserving). The
/// `max(1)` floor mirrors `num_cpus`'s own (it never returns 0 for a live process).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn cpu_budget_from(cores: usize, detected: usize) -> Option<usize> {
    let cores = cores.max(1);
    let effective = detected.max(1).min(cores);
    (effective < cores).then_some(effective)
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

    #[cfg(unix)]
    #[test]
    fn raise_nofile_never_lowers_the_soft_limit() {
        let read_soft = || unsafe {
            let mut lim = std::mem::zeroed::<libc::rlimit>();
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim), 0);
            lim.rlim_cur
        };
        let before = read_soft();
        raise_nofile_limit();
        assert!(
            read_soft() >= before,
            "raise_nofile_limit must never lower the soft limit"
        );
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

    // ─────────────────────── CPU budget ───────────────────────

    #[test]
    fn cpu_budget_is_none_or_in_range() {
        // Detector contract: either `None` (unconstrained / undetectable) or a
        // value in `[1, cores]`. Never 0, never above the host cores.
        if let Some(n) = cpu_budget() {
            let cores = available_cores();
            assert!(n >= 1 && n <= cores, "budget {n} out of [1, {cores}]");
        }
    }

    #[test]
    fn cpu_budget_from_reports_only_when_constraining() {
        // `num_cpus::get()` already returns min(ceil(quota), logical); this gate
        // only reports a budget when it's strictly below the logical cores.
        // Quota of 2 on an 8-core box → 2 (constrains).
        assert_eq!(cpu_budget_from(8, 2), Some(2));
        // 0.5-CPU quota ceils to 1 inside num_cpus → 1 here (constrains, never 0).
        assert_eq!(cpu_budget_from(8, 1), Some(1));
    }

    #[test]
    fn cpu_budget_from_unconstrained_is_none() {
        // Detected == cores → no real constraint → None (default-preserving).
        assert_eq!(cpu_budget_from(8, 8), None);
        // Detected above cores (shouldn't happen, but be safe) → clamps to cores,
        // which equals cores → None.
        assert_eq!(cpu_budget_from(4, 16), None);
        // A 1-core box can never tighten below itself.
        assert_eq!(cpu_budget_from(1, 1), None);
    }

    #[test]
    fn cpu_budget_from_floors_at_one_and_clamps_to_cores() {
        // A degenerate 0 from the detector floors to 1 (never serialize to 0).
        assert_eq!(cpu_budget_from(8, 0), Some(1));
        // cores=0 (impossible in practice) is treated as 1 → nothing to constrain.
        assert_eq!(cpu_budget_from(0, 4), None);
    }

    #[test]
    fn parse_override_clamps_and_validates() {
        // Positive integer, within the host ceiling → honored verbatim.
        assert_eq!(parse_override("4", 10), Some(4));
        // Above the ceiling clamps DOWN to the host (can't conjure CPU).
        assert_eq!(parse_override("16", 10), Some(10));
        // Explicit-wins ABOVE a detected v2 quota: ceiling is the quota-ignoring
        // host count, so 3 is honored on a 2-quota box (ceiling 10 here).
        assert_eq!(parse_override("3", 10), Some(3));
        // Whitespace tolerated.
        assert_eq!(parse_override("  2\n", 10), Some(2));
        // 0 / negative / non-numeric / empty → None (auto-detect).
        assert_eq!(parse_override("0", 10), None);
        assert_eq!(parse_override("-1", 10), None);
        assert_eq!(parse_override("garbage", 10), None);
        assert_eq!(parse_override("", 10), None);
        // A degenerate ceiling of 0 still floors the honored value at 1.
        assert_eq!(parse_override("5", 0), Some(1));
    }
}

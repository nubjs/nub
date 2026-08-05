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
// quota prevents both — but note that std's `available_parallelism()` ALREADY does
// almost all of this, so `cpu_budget` is close to inert in practice; see the gate
// note on `cpu_budget` below before assuming it contributes. Default-preserving: an
// unconstrained box returns `None` and pools keep full cores.
//
// AUTO-DETECT ONLY — no public user override. No neutral existing env var fits
// (UV_THREADPOOL_SIZE is libuv-pool-specific; RAYON_NUM_THREADS is rayon-only + a
// brand leak), and per the brand boundary a knob ships only on demonstrated demand.
// If a manual cap is ever needed it lands as a neutral npmrc field, never an `NUB_*`
// env var. A `#[cfg(test)]` injection seam (`test_cpu_budget_override`) forces a
// budget deterministically for the unit tests without any public knob.

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
/// degenerate is guarded).
///
/// MEASURED 2026-07-30 — this gate is very nearly unfireable, and the two "gaps in
/// std" it was written to close DO NOT EXIST. std reads the CFS quota for v1 too
/// (`sys/thread/unix.rs` dispatches `Cgroup::V1 => quota_v1`), and it re-applies the
/// quota when `sched_getaffinity` fails rather than dropping it (same file: the
/// quota is computed BEFORE the affinity call, and the `sysconf` fallback path ends
/// `count.min(quota)`). Both were confirmed against a shipped binary: on cgroup v1,
/// `--cpus 0.5 --cpuset-cpus 0,1,2,3` gave `available_parallelism() == 1` with
/// affinity left wide; on v2, blocking `sched_getaffinity` with a seccomp
/// `SCMP_ACT_ERRNO` profile still applied the quota. Since std FLOORS the quota and
/// `num_cpus` CEILS it, `detected >= cores` always holds wherever std sees the quota
/// at all, so `effective < cores` below is false and this returns `None` — it did so
/// in every constrained cell of a ~44-cell v2 sweep and all 9 v1 quota cells.
/// The one case that can still fire is std's own documented uncovered case, cgroup
/// v2 in a NON-STANDARD MOUNTPOINT, which `num_cpus` does resolve via
/// `/proc/self/mountinfo`. Keep/reshape/remove is a live design question; the
/// `Embedder::cpu_budget` hook means it is not a local decision.
///
/// Returns a budget ONLY when it constrains BELOW the logical core count — at/above
/// the cores it tightens nothing, so we return `None` and the caller keeps its
/// full-speed default (no needless pool rebuild). Linux-gated: `num_cpus`'s quota
/// read is a Linux-cgroup concept, and like `spawn_headroom` the over-report trap
/// was only ever a Linux-container problem; on macOS/Windows nothing constrains, so
/// the detector returns `None`.
pub(crate) fn cpu_budget() -> Option<usize> {
    // A `#[cfg(test)]` injection seam forces a deterministic budget for the unit
    // tests; in a release build this is a no-op (compiles out entirely).
    if let Some(n) = test_cpu_budget_override() {
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
/// minimally anyway.
pub(crate) fn available_cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// `#[cfg(test)]`-only injection seam: force `cpu_budget()` to a deterministic value
/// in unit tests via the INTERNAL `__NUB_TEST_CPU_BUDGET` env var. Double-underscore
/// = internal test plumbing (brand-EXEMPT per the brand boundary), never a public
/// knob — it does NOT exist in a release build (this fn compiles to a constant
/// `None`). Production CPU sizing is auto-detect-only.
#[cfg(test)]
fn test_cpu_budget_override() -> Option<usize> {
    std::env::var("__NUB_TEST_CPU_BUDGET")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|&n| n >= 1)
}

#[cfg(not(test))]
fn test_cpu_budget_override() -> Option<usize> {
    None
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
/// `<id>:pids:<cgroup-path>`; [`cgroup_v1_pids_candidates`] resolves where that
/// cgroup's `pids.max` actually lives on this mount.
///
/// Safe no-op on a pure cgroup v2 host: there the only line is `0::<path>`, whose
/// middle (controller) field is EMPTY, so the `c == "pids"` match never fires and
/// this returns `None` — the v2 probe already handled that host.
#[cfg(target_os = "linux")]
fn cgroup_v1_pids_max() -> Option<u64> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    // An unreadable mountinfo degrades to the historical assumption (root `/`
    // at `/sys/fs/cgroup/pids`) rather than losing detection entirely.
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    // Fall through ONLY on a failed READ. A successful read of the literal `max` is a
    // real answer — "this cgroup has no limit" — and consulting a further candidate
    // there would report some OTHER cgroup's limit, which is worse than saying so.
    for path in cgroup_v1_pids_candidates(&cgroup, &mountinfo) {
        if let Ok(raw) = std::fs::read_to_string(path) {
            return parse_pids_max(&raw);
        }
    }
    None
}

/// Resolve the `pids.max` path for this process's cgroup-v1 `pids` controller
/// from `/proc/self/cgroup` + `/proc/self/mountinfo`. Pure, so the container
/// topologies below are unit-testable without a real cgroupfs.
///
/// The naive join — mount point + the path from `/proc/self/cgroup` — is WRONG
/// inside a container, and fails in the dangerous direction. Docker's default
/// `--cgroupns` is `host` on cgroup v1 (it is `private` only on v2), so
/// `/proc/self/cgroup` reports the HOST-absolute `/docker/<id>` while the
/// container's `/sys/fs/cgroup/pids` is a bind mount ALREADY rooted at that same
/// cgroup. Joining the two yields `/sys/fs/cgroup/pids/docker/<id>/pids.max`,
/// which does not exist, so the read fails open to "unconstrained" and nub sizes
/// a 128-thread blocking pool inside a 64-PID container — the exact
/// `clone(2)`-EAGAIN exhaustion this module exists to prevent.
///
/// The fix is the standard containerized-cgroup resolution: strip the mount ROOT
/// (mountinfo field 4) off the cgroup path, then join the remainder onto the
/// MOUNT POINT (field 5). `num_cpus` already does this for the cpu controller,
/// which is why the CPU axis never exhibited this bug.
///
/// PRIOR ART — this is the OCI reference behaviour, not an invention here.
/// `opencontainers/cgroups/v1_utils.go` (vendored by runc) does the same thing,
/// `filepath.Rel(root, cgroup)` then `filepath.Join(mnt, rel)`, under the comment
/// *"This is needed for nested containers, because in /proc/self/cgroup we see paths
/// from host, which don't exist in container."* Two deliberate differences, both
/// because nub reads this from INSIDE the container while runc manages containers
/// from the host: runc takes the FIRST matching mount, where we take the
/// longest-matching root and retry the others; and `filepath.Rel` happily emits a
/// `../`-traversing path when the root is not a prefix, where the component-wise
/// `starts_with` filter below rejects that candidate outright.
///
/// Returns CANDIDATES rather than one path, most-specific first, with the pre-fix
/// location last. A single "best" answer looks right and is not: committing to the
/// longest-matching mount loses detection outright whenever that mount happens to be
/// unreadable — shadowed by a later overmount, or mode `0700` under an unprivileged
/// process — even though a shorter-matching mount would have resolved the same cgroup.
/// Measured on both Ubuntu 20.04 and Rocky 8: the pre-fix code read the limit and the
/// single-answer version reported UNCONSTRAINED, which is the dangerous direction and
/// exactly what this module exists to prevent. Trying candidates in order keeps the
/// change no worse than its predecessor on every topology.
///
/// Deliberately NOT a fallback to a *bare* `<mountpoint>/pids.max`: every candidate here
/// still resolves THIS process's own cgroup, so the root-cgroup masquerade that
/// [`cgroup_v2_pids_max`] refuses for v2 cannot occur.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn cgroup_v1_pids_candidates(cgroup: &str, mountinfo: &str) -> Vec<std::path::PathBuf> {
    let Some(base) = cgroup.lines().find_map(|l| {
        // Format: `hierarchy-id:controller-list:cgroup-path`.
        let mut parts = l.splitn(3, ':');
        let _id = parts.next()?;
        let controllers = parts.next()?;
        let path = parts.next()?;
        controllers.split(',').any(|c| c == "pids").then_some(path)
    }) else {
        return Vec::new();
    };

    // mountinfo: `<id> <parent> <maj:min> <root> <mountpoint> <opts> [tags...] - <fstype> <src> <superopts>`.
    // The optional tag list is variable-length and terminated by ` - `, hence the
    // split rather than a fixed field index.
    //
    // Order by LONGEST matching root: the mount whose root is the most specific prefix of
    // our cgroup path is the one that governs us. `Path::starts_with` is component-wise, so
    // a mount rooted at `/tst/ab` is correctly NOT treated as a prefix of `/tst/abc` — a
    // string-prefix comparison here would select it and resolve the wrong cgroup.
    let mut mounts: Vec<(&str, &str)> = mountinfo
        .lines()
        .filter_map(|l| {
            let (pre, post) = l.split_once(" - ")?;
            let mut post = post.split_whitespace();
            (post.next()? == "cgroup").then_some(())?;
            let _source = post.next()?;
            post.next()?.split(',').any(|o| o == "pids").then_some(())?;
            let mut pre = pre.split_whitespace().skip(3);
            Some((pre.next()?, pre.next()?))
        })
        .filter(|(root, _)| std::path::Path::new(base).starts_with(root))
        .collect();
    mounts.sort_by_key(|(root, _)| std::cmp::Reverse(root.len()));

    let mut out: Vec<std::path::PathBuf> = mounts
        .iter()
        .filter_map(|(root, mount_point)| {
            let rel = std::path::Path::new(base).strip_prefix(root).ok()?;
            Some(std::path::Path::new(mount_point).join(rel).join("pids.max"))
        })
        .collect();

    // The pre-fix location, last: on a host with no `pids` mount in mountinfo (or one we
    // could not resolve) this is exactly what the old code read, so we never end up blind
    // where it saw a limit. Deduplicated because on an ordinary host it IS the first entry.
    let legacy = std::path::Path::new("/sys/fs/cgroup/pids")
        .join(base.trim_start_matches('/'))
        .join("pids.max");
    if !out.contains(&legacy) {
        out.push(legacy);
    }
    out
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
    use std::sync::Mutex;

    // Serializes the tests that drive `cpu_budget()` against the process-global
    // `__NUB_TEST_CPU_BUDGET` seam: the suite runs `#[test]`s on multiple threads,
    // so a test reading `cpu_budget()` with no seam must not interleave with the
    // seam test mid-mutation (on a 1-core host an observed `=2` would fail the
    // range assertion). Both tests below take this lock.
    static CPU_SEAM_LOCK: Mutex<()> = Mutex::new(());

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

    // Real `/proc/self/mountinfo` shapes. The `pids` controller is identified by
    // fstype `cgroup` + a `pids` super-option; the optional-tag field before ` - `
    // is present in the host form and absent in the container form, which is why
    // the parser splits on ` - ` instead of indexing a fixed field.
    const MOUNTINFO_HOST: &str = "36 35 0:31 / /sys/fs/cgroup/pids rw,nosuid,nodev,noexec,relatime shared:16 - cgroup cgroup rw,pids\n\
         30 25 0:26 / /sys/fs/cgroup/memory rw,nosuid,nodev,noexec,relatime - cgroup cgroup rw,memory\n";
    const MOUNTINFO_DOCKER: &str = "666 665 0:31 /docker/abc123 /sys/fs/cgroup/pids ro,nosuid,nodev,noexec,relatime master:16 - cgroup cgroup rw,pids\n";

    fn pb(s: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(s)
    }

    #[test]
    fn cgroup_v1_pids_strips_the_bind_mount_root_inside_a_container() {
        // THE BUG. Docker defaults to `--cgroupns=host` on cgroup v1, so
        // /proc/self/cgroup reports the HOST-absolute path while the pids hierarchy
        // is bind-mounted at that very cgroup. Joining them naively produced
        // `/sys/fs/cgroup/pids/docker/abc123/pids.max` — a path that does not exist,
        // so detection failed open and a 64-PID container looked unconstrained.
        let cgroup = "9:pids:/docker/abc123\n4:cpu,cpuacct:/docker/abc123\n";
        let got = cgroup_v1_pids_candidates(cgroup, MOUNTINFO_DOCKER);
        assert_eq!(got.first(), Some(&pb("/sys/fs/cgroup/pids/pids.max")));
        // The pre-fix location is retained as a last resort, never as the answer.
        assert_eq!(
            got.last(),
            Some(&pb("/sys/fs/cgroup/pids/docker/abc123/pids.max"))
        );
    }

    #[test]
    fn cgroup_v1_pids_keeps_the_nested_path_on_a_host() {
        // Mount root `/` — nothing to strip, so a nested systemd scope still resolves
        // to its own cgroup rather than the (usually `max`) root. Here the resolved
        // path and the legacy path coincide, so there is exactly ONE candidate.
        let cgroup = "9:pids:/user.slice/user-1001.slice/session-3.scope\n";
        assert_eq!(
            cgroup_v1_pids_candidates(cgroup, MOUNTINFO_HOST),
            vec![pb(
                "/sys/fs/cgroup/pids/user.slice/user-1001.slice/session-3.scope/pids.max"
            )]
        );
    }

    #[test]
    fn cgroup_v1_pids_falls_back_when_mountinfo_is_unusable() {
        // An empty/unreadable mountinfo still yields the historical location.
        let cgroup = "9:pids:/user.slice\n";
        assert_eq!(
            cgroup_v1_pids_candidates(cgroup, ""),
            vec![pb("/sys/fs/cgroup/pids/user.slice/pids.max")]
        );
    }

    #[test]
    fn cgroup_v1_pids_orders_longest_matching_mount_root_first() {
        // Two `pids` mounts visible, the OUTER one listed first. Selecting positionally
        // would resolve a different cgroup's limit; longest-root-first puts the mount
        // that actually governs this process at the head.
        let mountinfo = "20 19 0:31 / /host/sys/fs/cgroup/pids rw,relatime shared:9 - cgroup cgroup rw,pids\n\
             99 20 0:31 /docker/abc123 /sys/fs/cgroup/pids ro,relatime master:22 - cgroup cgroup rw,pids\n";
        let cgroup = "9:pids:/docker/abc123/nested\n";
        let got = cgroup_v1_pids_candidates(cgroup, mountinfo);
        assert_eq!(
            got.first(),
            Some(&pb("/sys/fs/cgroup/pids/nested/pids.max"))
        );
        // …and the shorter-rooted mount is RETAINED as a retry rather than discarded.
        assert!(
            got.contains(&pb(
                "/host/sys/fs/cgroup/pids/docker/abc123/nested/pids.max"
            )),
            "shorter-rooted mount must remain a candidate: {got:?}"
        );
    }

    #[test]
    fn cgroup_v1_pids_retries_past_an_unreadable_longest_match() {
        // The regression this ordering exists to prevent, measured on Ubuntu 20.04 and
        // Rocky 8: when the longest-matching mount is unreadable (shadowed by a later
        // overmount, or mode 0700 under an unprivileged process), committing to it alone
        // reported UNCONSTRAINED on a constrained box, while the pre-fix code read the
        // limit through the shorter mount. Every alternative must therefore survive as a
        // candidate, with the pre-fix location last.
        let mountinfo = "20 19 0:31 / /sys/fs/cgroup/pids rw,relatime shared:9 - cgroup cgroup rw,pids\n\
             99 20 0:31 /tst /mnt/gov rw,relatime master:22 - cgroup cgroup rw,pids\n";
        let cgroup = "9:pids:/tst/abc\n";
        assert_eq!(
            cgroup_v1_pids_candidates(cgroup, mountinfo),
            vec![
                pb("/mnt/gov/abc/pids.max"),
                pb("/sys/fs/cgroup/pids/tst/abc/pids.max"),
            ]
        );
    }

    #[test]
    fn cgroup_v1_pids_requires_a_component_wise_prefix() {
        // `/tst/ab` is a longer STRING prefix of `/tst/abc` than `/tst` is, so a
        // string-prefix implementation would select the decoy and resolve the wrong
        // cgroup. `Path::starts_with` is component-wise, so the decoy is excluded.
        let mountinfo = "20 19 0:31 / /sys/fs/cgroup/pids rw,relatime shared:9 - cgroup cgroup rw,pids\n\
             98 20 0:31 /tst /mnt/gov rw,relatime master:21 - cgroup cgroup rw,pids\n\
             99 20 0:31 /tst/ab /mnt/dec rw,relatime master:22 - cgroup cgroup rw,pids\n";
        let cgroup = "9:pids:/tst/abc\n";
        let got = cgroup_v1_pids_candidates(cgroup, mountinfo);
        assert_eq!(got.first(), Some(&pb("/mnt/gov/abc/pids.max")));
        assert!(
            !got.iter().any(|p| p.starts_with("/mnt/dec")),
            "the /tst/ab decoy is not a component-wise prefix of /tst/abc: {got:?}"
        );
    }

    #[test]
    fn cgroup_v1_pids_ignores_a_pure_v2_host() {
        // A v2-only `/proc/self/cgroup` is a single `0::<path>` line whose controller
        // field is empty, so the v1 probe must find nothing and let the v2 probe answer.
        assert!(cgroup_v1_pids_candidates("0::/user.slice/x.scope\n", MOUNTINFO_HOST).is_empty());
    }

    // ─────────────────────── CPU budget ───────────────────────

    #[test]
    fn cpu_budget_is_none_or_in_range() {
        // Detector contract: either `None` (unconstrained / undetectable) or a
        // value in `[1, cores]`. Never 0, never above the host cores. Hold the seam
        // lock so the seam test isn't mid-mutation when we read `cpu_budget()`.
        let _guard = CPU_SEAM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    fn test_seam_forces_a_deterministic_budget() {
        // The `#[cfg(test)]` injection seam (NOT a public knob — `__NUB_TEST_*`
        // internal plumbing) lets a test pin `cpu_budget()` regardless of the host.
        // The seam env var is process-global and the suite is multi-threaded, so
        // hold the shared lock to serialize against other `cpu_budget()` readers.
        let _guard = CPU_SEAM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: `set_var`/`remove_var` mutate the process env; the lock above
        // serializes against the only other test that reads `cpu_budget()`, so no
        // concurrent env reader races us here.
        unsafe { std::env::set_var("__NUB_TEST_CPU_BUDGET", "2") };
        assert_eq!(cpu_budget(), Some(2));
        unsafe { std::env::set_var("__NUB_TEST_CPU_BUDGET", "garbage") };
        // Invalid → seam yields None; falls through to real auto-detection, whose
        // contract is None-or-in-range (asserted by `cpu_budget_is_none_or_in_range`).
        let auto = cpu_budget();
        assert!(auto.is_none_or(|n| n >= 1 && n <= available_cores()));
        unsafe { std::env::remove_var("__NUB_TEST_CPU_BUDGET") };
    }
}

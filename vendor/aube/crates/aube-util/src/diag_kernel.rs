/*!
 * Kernel and OS signal capture for the diagnostics layer.
 *
 * Activated by setting `AUBE_DIAG_KERNEL=1` (or by passing `--diag full`
 * with the env var also set, since the kernel sampler is opt-in even
 * when diag itself is on). Emits `cat=kernel,name=<phase>` events with
 * deltas for user CPU, system CPU, peak resident set size, and page
 * faults around bracketed scopes.
 *
 * Linux and macOS use `libc::getrusage` directly; the macOS variant
 * reports `ru_maxrss` in bytes while Linux reports it in kibibytes, so
 * the snapshot normalizes both to bytes. Other platforms (notably
 * Windows) currently return `None` from [`snapshot`] — wiring
 * `windows-sys` for `GetProcessTimes` + `GetProcessMemoryInfo` is a
 * straightforward follow-up but adds a workspace dependency, so it is
 * deferred.
 */

use std::time::Duration;

/**
 * One-shot snapshot of kernel-level resource counters for the calling
 * process.
 *
 * Field reference:
 *   `user_cpu_ms`     user-mode CPU time consumed since process start
 *   `sys_cpu_ms`      kernel-mode CPU time consumed since process start
 *   `max_rss_bytes`   peak resident set size since process start
 *   `minor_faults`    soft page faults (no disk IO required)
 *   `major_faults`    hard page faults (required disk IO)
 *
 * On platforms without a kernel implementation the snapshot is `None`.
 */
#[derive(Default, Clone, Copy, Debug)]
pub struct KernelSnapshot {
    pub user_cpu_ms: u64,
    pub sys_cpu_ms: u64,
    pub max_rss_bytes: u64,
    pub minor_faults: u64,
    pub major_faults: u64,
}

#[cfg(unix)]
pub fn snapshot() -> Option<KernelSnapshot> {
    use std::mem::MaybeUninit;
    // Safety: `getrusage(RUSAGE_SELF, &mut buf)` is the documented FFI
    // entry point. The buffer is initialized in place by the kernel.
    unsafe {
        let mut ru = MaybeUninit::<libc::rusage>::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr()) != 0 {
            return None;
        }
        let ru = ru.assume_init();
        let user_us = (ru.ru_utime.tv_sec as u64) * 1_000_000 + (ru.ru_utime.tv_usec as u64);
        let sys_us = (ru.ru_stime.tv_sec as u64) * 1_000_000 + (ru.ru_stime.tv_usec as u64);
        let max_rss = if cfg!(target_os = "macos") {
            // macOS reports bytes.
            ru.ru_maxrss as u64
        } else {
            // Linux reports kibibytes.
            (ru.ru_maxrss as u64) * 1024
        };
        Some(KernelSnapshot {
            user_cpu_ms: user_us / 1000,
            sys_cpu_ms: sys_us / 1000,
            max_rss_bytes: max_rss,
            minor_faults: ru.ru_minflt as u64,
            major_faults: ru.ru_majflt as u64,
        })
    }
}

#[cfg(not(unix))]
pub fn snapshot() -> Option<KernelSnapshot> {
    None
}

/**
 * Reports whether kernel signal capture is requested AND available.
 *
 * Returns `true` only when `AUBE_DIAG_KERNEL=1` is set in the
 * environment AND [`snapshot`] succeeds on the host platform. Cheap
 * to call: one env var check plus one syscall on Unix, env var only on
 * other platforms.
 */
pub fn enabled() -> bool {
    crate::env::embedder_env("DIAG_KERNEL").is_some() && snapshot().is_some()
}

/**
 * Emit a `cat=kernel,name=<phase>` event with the deltas between
 * `before` and `after`.
 *
 * `dur` of the emitted event is the sum of the user-CPU and system-CPU
 * deltas, giving a single comparable number against the wall time of
 * neighboring spans. Per-component deltas land in the meta object so
 * the analyzer can compute user-vs-sys ratios.
 */
pub fn emit_phase_delta(phase: &'static str, before: KernelSnapshot, after: KernelSnapshot) {
    let user_d = after.user_cpu_ms.saturating_sub(before.user_cpu_ms);
    let sys_d = after.sys_cpu_ms.saturating_sub(before.sys_cpu_ms);
    let rss_after = after.max_rss_bytes;
    let minor_d = after.minor_faults.saturating_sub(before.minor_faults);
    let major_d = after.major_faults.saturating_sub(before.major_faults);
    crate::diag::event_lazy(
        crate::diag::Category::Kernel,
        phase,
        Duration::from_millis(user_d + sys_d),
        || {
            format!(
                r#"{{"user_ms":{user_d},"sys_ms":{sys_d},"rss_peak":{rss_after},"minor_faults":{minor_d},"major_faults":{major_d}}}"#
            )
        },
    );
}

/**
 * Bracket an async block with a kernel snapshot pair.
 *
 * Returns the value produced by the future. When kernel sampling is
 * disabled the future runs directly with no overhead. When enabled,
 * snapshots are taken before and after and a `kernel.<phase>` event
 * is emitted with the delta.
 */
pub async fn track_phase_async<F, T>(phase: &'static str, f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    if !enabled() {
        return f.await;
    }
    let before = snapshot().unwrap_or_default();
    let result = f.await;
    let after = snapshot().unwrap_or_default();
    emit_phase_delta(phase, before, after);
    result
}

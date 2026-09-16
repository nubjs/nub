//! Per-process descriptor and thread counts for the two leak tests, sampled at QUIESCENCE.
//!
//! A JOINED THREAD IS NOT IMMEDIATELY GONE FROM `/proc/self/task`. `pthread_join` returns once the
//! kernel clears the tid futex, which `mm_release()` does partway through `do_exit()` — before
//! `release_task()` unlinks the task from its thread group and drops its procfs entry. So a count
//! taken the instant a sandbox command returns can still include a thread that is already joined
//! and already fully accounted for, and the sample is then a mid-teardown number.
//!
//! That is how this failed on CI, and the shape of the failure is what identifies it: the
//! descriptor halves matched EXACTLY and the thread half came out one LOWER than the baseline. A
//! leak cannot subtract. The baseline had been taken while a warm-up command's supervisor thread
//! was still winding down, and the final count was taken after everything had settled.
//!
//! Waiting for the pair to stop moving does not weaken either assertion, which is still an exact
//! equality against the baseline: a steady leak settles on a DIFFERENT number and still goes red,
//! and an ongoing one never settles at all and fails on the deadline.

use std::time::{Duration, Instant};

/// This process's open descriptors and live threads, read once two consecutive reads agree.
pub fn settled() -> (usize, usize) {
    let read = || {
        (
            std::fs::read_dir("/proc/self/fd").unwrap().count(),
            std::fs::read_dir("/proc/self/task").unwrap().count(),
        )
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = read();
    loop {
        std::thread::sleep(Duration::from_millis(25));
        let now = read();
        if now == last {
            return now;
        }
        assert!(
            Instant::now() < deadline,
            "per-process resource counts never settled: {last:?} then {now:?}",
        );
        last = now;
    }
}

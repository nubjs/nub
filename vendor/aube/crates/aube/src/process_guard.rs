//! Ties a spawned child's lifetime to the aube process so that stopping
//! aube stops the tool it launched, instead of orphaning it under `init`
//! (`ppid=1`). See <https://github.com/jdx/aube/discussions/1059>.
//!
//! `aube dlx <tool>` / `aube exec <bin>` used to `spawn` a child and simply
//! await it, with no link between the child and aube's own lifetime. A
//! supervisor that stops the tool by signalling the `aube` process it
//! launched (a long-running host running a stdio server via `aube dlx`,
//! say) would strand the tool on every launch — `npx`, `pnpm dlx`, and
//! `bunx` all tear it down.
//!
//! Coverage:
//!   - Any Unix: catchable termination signals (SIGTERM/SIGINT/SIGHUP/
//!     SIGQUIT) delivered to aube are forwarded to the child, so
//!     `pkill -x aube` tears the tool down.
//!   - Linux additionally sets `PR_SET_PDEATHSIG`, so even an uncatchable
//!     `SIGKILL` of aube reaps the child via the kernel — nothing runs in
//!     aube on `SIGKILL`, so this has to come from the OS.
//!   - macOS `SIGKILL`-of-aube is the one remaining gap: there is no
//!     `PDEATHSIG` equivalent and no aube code runs on `SIGKILL`, so the
//!     teardown has to live in a process that outlives aube. That
//!     `kqueue`/`NOTE_EXIT` watcher is a tracked follow-up; see the spawn
//!     site below.

use std::process::ExitStatus;

/// Configure `cmd` so the child is torn down when aube dies, then spawn it
/// and wait — forwarding catchable termination signals to the child while
/// it runs. Drop-in replacement for `cmd.status().await`.
pub(crate) async fn spawn_and_wait(
    mut cmd: tokio::process::Command,
) -> std::io::Result<ExitStatus> {
    set_parent_death_signal(&mut cmd);

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        // Register the signal listeners *before* spawning. Registration is
        // process-global and independent of the child, and a `signal()`
        // failure here must not leave a running child behind —
        // `tokio::process::Child` does not kill on drop, so spawning first
        // and then erroring out of `?` would orphan exactly what this module
        // exists to prevent.
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sighup = signal(SignalKind::hangup())?;
        let mut sigquit = signal(SignalKind::quit())?;

        let mut child = cmd.spawn()?;

        // macOS SIGKILL-of-aube teardown (follow-up, discussion #1059):
        // spawn a `kqueue`/`NOTE_EXIT` watchdog here on `child.id()`. It has
        // to be a separate process because a `SIGKILL`ed aube runs no code;
        // the watchdog survives aube (reparented to init) and kills the
        // child when aube's pid fires `NOTE_EXIT`. Signal forwarding below
        // already covers the catchable cases on macOS.

        // Cache the pid up front: after `child.wait()` resolves the pid is
        // gone, and we only ever forward while the child is still running.
        let pid = child.id();
        loop {
            tokio::select! {
                status = child.wait() => return status,
                _ = sigterm.recv() => forward_signal(pid, libc::SIGTERM),
                _ = sigint.recv() => forward_signal(pid, libc::SIGINT),
                _ = sighup.recv() => forward_signal(pid, libc::SIGHUP),
                _ = sigquit.recv() => forward_signal(pid, libc::SIGQUIT),
            }
            // Keep waiting after forwarding: let the child decide how to
            // react and propagate its real exit status, matching `npx` /
            // `pnpm dlx`. If the child ignores the signal, aube stays bound
            // to it rather than exiting out from under it.
        }
    }
    #[cfg(not(unix))]
    {
        let mut child = cmd.spawn()?;
        child.wait().await
    }
}

/// Apply the OS-level "die with the parent" hint before spawn. Linux-only
/// (`PR_SET_PDEATHSIG`); a no-op elsewhere.
fn set_parent_death_signal(cmd: &mut tokio::process::Command) {
    #[cfg(target_os = "linux")]
    {
        // aube's own pid, captured before the fork. The child compares its
        // parent against this to detect an aube that already exited — a plain
        // `getppid() == 1` check would miss it under `PR_SET_CHILD_SUBREAPER`
        // (containers with tini, `systemd --user` sessions), where orphans
        // reparent to a subreaper with a pid > 1 rather than to init.
        let parent_pid = std::process::id() as libc::pid_t;
        // SAFETY: this closure runs in the forked child between fork and
        // exec, so it may only touch async-signal-safe libc calls. `prctl`,
        // `getppid`, and `raise` are all async-signal-safe; there is no
        // allocation and no lock acquisition on this path.
        unsafe {
            cmd.pre_exec(move || {
                // Deliver SIGTERM to this child when the thread that forked
                // it goes away — covers an uncatchable SIGKILL of aube.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Race guard: aube may have exited between fork and the prctl
                // above, in which case the death signal was already (not)
                // delivered and we would run detached. If our parent is no
                // longer aube, we have already been reparented — self-
                // terminate rather than leak.
                if libc::getppid() != parent_pid {
                    libc::raise(libc::SIGTERM);
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cmd;
    }
}

#[cfg(unix)]
fn forward_signal(pid: Option<u32>, sig: libc::c_int) {
    let Some(pid) = pid else {
        return;
    };
    // SAFETY: `kill(2)` with a concrete pid and a valid signal number. A
    // child that has already exited yields `ESRCH`, which we ignore — the
    // `child.wait()` arm of the select will resolve on the next poll.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

//! POSIX descendant reaping for lifecycle scripts — the Unix half of what
//! [`crate::windows_job`] does with a kill-on-job-close job object.
//!
//! A lifecycle shell's grandchildren (`node-gyp` → `make` → `cc`) are not
//! reachable from the shell's pid: `kill_on_drop(true)` SIGKILLs `sh` alone, the
//! build tools reparent to init, and they keep writing into `node_modules` long
//! after aube returned. Measured on a 50-package build corpus: 2,105 lines of
//! script output *after* the command reported its result, and one shard that
//! leaked 11,401. The kernel already tracks exactly the set we need — a process
//! GROUP, which every `fork` inherits — so the fix is to give each shell its own
//! group and signal `-pgid`.
//!
//! This generalises what nub's Landlock build jail already does on the confined
//! path (`setsid` in `pre_exec`, then `kill(-pid, SIGKILL)` once the child is
//! waited) to the path aube uses when nothing confines the script, which is
//! every standalone-aube install and every unconfined nub install.
//!
//! Rejected alternatives: a process-TREE walk (`/proc` on Linux, `libproc` on
//! macOS) is racy by construction — a grandchild forked between the walk and the
//! kill is reparented to init and becomes invisible, which is the very leak
//! being fixed — and needs two OS-specific implementations. `PR_SET_CHILD_SUBREAPER`
//! is Linux-only and only re-parents orphans; it still leaves you enumerating
//! them. `PR_SET_PDEATHSIG` reaches the shell but not its descendants.

use std::sync::Once;
use std::sync::atomic::{AtomicI32, Ordering};

/// Live lifecycle process groups, readable from a signal handler.
///
/// A `Mutex`/`Vec` cannot be touched from a signal handler (neither is
/// async-signal-safe), so the registry is a fixed array of atomics: a slot holds
/// a pgid or 0. Capacity is far above any realistic `child-concurrency`; if it
/// ever fills, the surplus script simply isn't reachable from the handler — its
/// [`ProcessGroupReaper`] guard still reaps it on the ordinary paths.
const REGISTRY_SLOTS: usize = 256;
static REGISTRY: [AtomicI32; REGISTRY_SLOTS] = [const { AtomicI32::new(0) }; REGISTRY_SLOTS];
/// `Once` rather than a CAS flag: a second caller must BLOCK until the handler
/// is actually installed, not return early on seeing "installation in progress"
/// and publish a pgid the not-yet-installed handler cannot reap.
static HANDLER: Once = Once::new();

/// Signals whose default action terminates aube and that a user or supervisor
/// actually sends: a terminal Ctrl-C, `docker stop` / CI cancellation, a
/// hangup. Each one, unhandled, would leave every live lifecycle group running.
const REAPED_SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// Put the lifecycle shell in its own process group so `kill(-pgid, …)` reaches
/// everything it forks.
///
/// Both sides call `setpgid` — the child from `pre_exec` and the parent right
/// after `spawn` (see [`ProcessGroupReaper::arm`]) — which is the textbook
/// race-free idiom: whichever runs first wins, and the loser's call fails
/// harmlessly (`EACCES` once the child has `execve`d). A parent-only call would
/// race the exec; a child-only call would leave the parent guessing whether the
/// group exists yet, and the guess is not safe to get wrong (see `arm`).
///
/// `setpgid` rather than `setsid`: the group is all the reaping needs, and
/// keeping the shell in aube's session preserves the controlling terminal for
/// scripts that write to it. The job-control stops are the one cost of leaving
/// the foreground group — a background read on the terminal would otherwise
/// raise `SIGTTIN` and STOP the whole build tree — so both are ignored here.
/// `SIG_IGN` turns that stop into an `EIO` the reading tool reports and moves
/// past, which is what npm and pnpm produce anyway by never handing install
/// scripts the terminal's stdin in the first place.
pub(crate) fn group_on_spawn(cmd: &mut tokio::process::Command) {
    // SAFETY: `setpgid` and `signal` are async-signal-safe, touch no parent
    // state, and allocate nothing — the only things permitted between `fork`
    // and `execve` in a multithreaded parent.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            libc::signal(libc::SIGTTIN, libc::SIG_IGN);
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            Ok(())
        });
    }
}

/// Kills the lifecycle shell's whole process group when it drops — on normal
/// return, on error, and on task abort.
///
/// Reaping on the NORMAL return too is deliberate and matches both sibling
/// implementations: the Windows job object fires on every exit from
/// `run_command_reaping_descendants`, and nub's Landlock path signals the group
/// after a successful `status()`. A script that leaves a build daemon running
/// past its own exit is the same orphan, whether or not the script failed.
pub(crate) struct ProcessGroupReaper {
    pgid: libc::pid_t,
    slot: Option<usize>,
}

impl ProcessGroupReaper {
    /// Arm the reaper for a just-spawned child, or return `None` if the child is
    /// not its own group leader.
    ///
    /// TAKES THE `Child`, NOT A PID, AND THAT IS LOAD-BEARING. `arm` completes
    /// the race-free `setpgid` pair — so it MOVES its argument into a new group
    /// before checking, and `Drop` then SIGKILLs that group. Pointed at anything
    /// other than a child spawned through [`group_on_spawn`], it would relocate
    /// an unrelated process and kill it. Owning a `&Child` is what makes that
    /// unrepresentable; the pid is only read back out here.
    ///
    /// The `getpgid == pid` check that follows is then a genuine confirmation:
    /// it is false exactly when BOTH `setpgid` calls failed, which leaves the
    /// child in aube's own group, where `kill(-pid, …)` would name a group aube
    /// belongs to and kill the installer along with every other running script.
    /// That case degrades to `kill_on_drop`-only — the same fail-open posture
    /// the Windows job-object path takes when the OS refuses it.
    pub(crate) fn arm(child: &tokio::process::Child, script_name: &str) -> Option<Self> {
        let pid = child.id()? as libc::pid_t;
        // SAFETY: `setpgid`/`getpgid` on a live child of this process. The
        // `setpgid` is the parent half of the pair in `group_on_spawn`; its
        // failure is expected and ignored (`EACCES` once the child has exec'd
        // and already done it itself, `ESRCH` if it exited), because the
        // `getpgid` is what decides.
        let is_leader = unsafe {
            libc::setpgid(pid, pid);
            libc::getpgid(pid) == pid
        };
        if !is_leader {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_PROCESS_GROUP_UNAVAILABLE,
                "unix: `{script_name}` did not become its own process-group leader; \
                 running without descendant reaping — build tools it spawns may be \
                 orphaned and keep writing after this install returns"
            );
            return None;
        }
        Some(Self {
            pgid: pid,
            slot: register(pid),
        })
    }
}

impl Drop for ProcessGroupReaper {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            REGISTRY[slot].store(0, Ordering::SeqCst);
        }
        // SIGKILL rather than a TERM-then-KILL escalation: `Drop` cannot await,
        // and blocking a runtime worker on a grace period is worse than the
        // hard kill `kill_on_drop` and the Windows job object already apply to
        // the shell. ESRCH (the group is already empty) is the normal case.
        //
        // A reaped leader leaves the pgid free to be recycled, so this races pid
        // wraparound in the microseconds after `wait` — the same residual window
        // nub's Landlock path accepts. While any descendant survives, the group
        // is non-empty and the pgid cannot be reused, which is exactly the case
        // this exists for.
        // SAFETY: `kill` on a group id this guard owns.
        unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
    }
}

fn register(pgid: libc::pid_t) -> Option<usize> {
    install_signal_handler();
    REGISTRY.iter().position(|slot| {
        slot.compare_exchange(0, pgid, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    })
}

/// Reap every live lifecycle group when aube is asked to terminate.
///
/// Without this, moving scripts out of aube's process group would REGRESS the
/// common case: a terminal Ctrl-C reaches the foreground group, so today it
/// happens to reach the scripts too, and a script in its own group would no
/// longer see it. With it the coverage is strictly wider than before — a
/// `SIGTERM` from a supervisor, a CI cancellation, or a `kill -INT` from a
/// non-terminal parent reaches only aube today and orphans the entire tree.
///
/// Installed over `SIG_DFL` ONLY. A non-default disposition means an embedder
/// (nub's own forwarder) or the user already owns the signal and has its own
/// teardown; replacing it would silently break that. `SIG_IGN` is likewise left
/// alone — it is a deliberate choice by whoever set it.
fn install_signal_handler() {
    HANDLER.call_once(|| {
        for signo in REAPED_SIGNALS {
            // SAFETY: `sigaction` with a zeroed `sigaction` struct read back into
            // local storage, then a handler that is itself async-signal-safe.
            unsafe {
                let mut current: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(signo, std::ptr::null(), &mut current) != 0 {
                    continue;
                }
                if current.sa_sigaction != libc::SIG_DFL {
                    continue;
                }
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = reap_and_resignal as *const () as usize;
                // SA_RESETHAND restores SIG_DFL before the handler runs, so the
                // `raise` below takes the default action once the handler returns
                // and unblocks the signal — aube still dies with the exit status the
                // user expects, having reaped first.
                action.sa_flags = libc::SA_RESETHAND;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signo, &action, std::ptr::null_mut());
            }
        }
    });
}

/// Signal handler: `kill`, `raise` and relaxed atomic loads are the only things
/// it does, all async-signal-safe.
extern "C" fn reap_and_resignal(signo: libc::c_int) {
    for slot in REGISTRY.iter() {
        let pgid = slot.load(Ordering::SeqCst);
        if pgid != 0 {
            // SAFETY: `kill` on a pgid a live `ProcessGroupReaper` published.
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
    }
    // SAFETY: re-raise so the default action (already restored by SA_RESETHAND)
    // produces the exit status the caller would have seen without this handler.
    unsafe { libc::raise(signo) };
}

// No unit test asserts the "refuses a non-leader pid" branch. Reaching it
// requires handing `arm` a pid that is not a child spawned through
// `group_on_spawn`, and `arm` starts by MOVING its argument into its own group —
// so a test that passes any live pid manufactures the leader condition and then
// SIGKILLs that process's group. Writing the test is the hazard, which is why
// `arm` takes `&Child` instead of a bare pid. The reaping behaviour it guards is
// covered end-to-end by `unix_process_group_tests` in `lib.rs`.

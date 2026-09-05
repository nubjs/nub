//! POSIX descendant reaping for lifecycle scripts — the Unix half of what
//! [`crate::windows_job`] does with a kill-on-job-close job object.
//!
//! A lifecycle shell's grandchildren (`node-gyp` → `make` → `cc`) are not
//! reachable from the shell's pid: `kill_on_drop(true)` SIGKILLs `sh` alone, the
//! build tools reparent to init, and they keep writing into `node_modules` long
//! after aube returned. Measured on a 50-package build corpus: 2,105 lines of
//! script output *after* the command reported its result, and one shard that
//! leaked 11,407. The kernel already tracks exactly the set we need — a process
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
/// Ignoring `SIGTTIN` turns that stop into an `EIO` the reading tool reports and
/// moves past; a hung install is the worse of the two. Ignoring `SIGTTOU` is
/// behaviour-preserving — it just lets a terminal-mode change succeed as it
/// does today. NOTE this reaches ROOT scripts too (`prepare`, `prepack`,
/// `prepublishOnly`), which are likelier than a dependency's to want the
/// terminal; an interactive read there now fails rather than blocking.
pub(crate) fn group_on_spawn(cmd: &mut tokio::process::Command) {
    // BEFORE the spawn, not at registration time: the child leaves the
    // foreground process group the instant `pre_exec` runs, and from then until
    // the handler exists a terminal Ctrl-C reaches neither the script nor
    // anything that would reap it — the exact window the handler is for.
    install_signal_handler();
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
/// `run_command_killing_descendants`, and nub's Landlock path signals the group
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
    /// The `getpgid == pid` check that follows is then a genuine confirmation.
    /// Its main failure mode is both `setpgid` calls failing, which leaves the
    /// child in aube's own group, where `kill(-pid, …)` would name a group aube
    /// belongs to and kill the installer along with every other running script;
    /// it also declines if `getpgid` errors or the shell relocated itself.
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
                code = aube_codes::warnings::WARN_AUBE_UNIX_PROCESS_GROUP_UNAVAILABLE,
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
        // Cleared AFTER the kill, never before: a signal landing in between
        // would otherwise find the slot empty, skip this group, and the process
        // would die at handler return with the group leaked. Killing twice is
        // ESRCH; killing zero times is the leak.
        if let Some(slot) = self.slot {
            REGISTRY[slot].store(0, Ordering::SeqCst);
        }
    }
}

/// Arm the terminate-signal reaper ahead of a spawn an EMBEDDER owns.
///
/// aube arms it inside [`group_on_spawn`], which the embedder-confined path never reaches
/// — that spawn belongs to the host sandbox. Called BEFORE the spawn for the same reason
/// `group_on_spawn` calls it there: the child leaves the foreground process group the
/// instant it starts, and until the handler exists a terminal Ctrl-C reaches neither the
/// script nor anything that would reap it.
///
/// The `SIG_DFL`-only probe behind it is a ONE-SHOT (`Once`), so an embedder that arms a
/// chaining registrar such as `signal-hook` before its first confined spawn forfeits
/// enrolment for the whole process rather than for one script. Nothing on nub's install
/// path does today; a future one would have to arm this first.
pub fn arm_group_reaper() {
    install_signal_handler();
}

/// Enrol a process group an EMBEDDER created in the same registry the handler sweeps, so a
/// `SIGINT`/`SIGTERM`/`SIGHUP` that kills aube reaps the group instead of orphaning it.
/// `None` if the group is not enrollable (see below) or the registry is full — both
/// degrade to the embedder's own teardown, never to a wrong kill.
///
/// REGISTRATION ONLY: the returned guard clears the slot and does NOT kill. The embedder
/// owns the ordinary exits through its own launch handle; this covers the one path no RAII
/// guard can, where the signal's default action kills the process before any `Drop` runs.
///
/// `pgid` MUST be a group the caller confirmed its child leads. The two checks here are a
/// backstop for that contract, not a substitute for it — the handler issues `kill(-pgid)`,
/// so a pgid naming aube's OWN group would SIGKILL the installer and every sibling script.
pub fn register_embedder_group(pgid: libc::pid_t) -> Option<EmbedderGroupRegistration> {
    // SAFETY: both are plain reads of process state, valid to call at any time.
    let own_group = unsafe { libc::getpgrp() };
    if pgid <= 0 || pgid == own_group || unsafe { libc::getpgid(pgid) } != pgid {
        return None;
    }
    // Idempotent (`Once`), and a no-op when `arm_group_reaper` already ran. Kept so a
    // caller that only registers is still covered rather than silently unreaped.
    install_signal_handler();
    register(pgid).map(|slot| EmbedderGroupRegistration { slot })
}

/// The registry slot held by [`register_embedder_group`], released on drop.
pub struct EmbedderGroupRegistration {
    slot: usize,
}

impl Drop for EmbedderGroupRegistration {
    fn drop(&mut self) {
        REGISTRY[self.slot].store(0, Ordering::SeqCst);
    }
}

fn register(pgid: libc::pid_t) -> Option<usize> {
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

/// Signal handler. Everything it touches — atomic loads, `kill`, `sigaction`,
/// `raise` — is on POSIX's async-signal-safe list.
extern "C" fn reap_and_resignal(signo: libc::c_int) {
    // `SA_RESETHAND` is NOT enough to guarantee the `raise` below terminates.
    // A chaining registrar layered over this disposition — which is what
    // `signal-hook`, and therefore `tokio::signal`, installs — calls us from
    // inside ITS handler, so the kernel never applied our reset: the `raise`
    // re-enters through the chain instead of dying. Measured with the real
    // `signal-hook-registry`: unchained the process exits 130, chained it
    // re-entered >100,000 times and never terminated. So force the default
    // disposition here rather than trusting the flag, and refuse re-entry.
    static REENTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if REENTERED.swap(true, Ordering::SeqCst) {
        return;
    }
    for slot in REGISTRY.iter() {
        let pgid = slot.load(Ordering::SeqCst);
        if pgid != 0 {
            // SAFETY: `kill` on a pgid a live `ProcessGroupReaper` published.
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
    }
    // SAFETY: install SIG_DFL for this signal, then re-raise, so the process
    // dies with the status it would have had without this handler.
    unsafe {
        let mut dfl: libc::sigaction = std::mem::zeroed();
        dfl.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut dfl.sa_mask);
        libc::sigaction(signo, &dfl, std::ptr::null_mut());
        libc::raise(signo);
    }
}

// No unit test asserts the "refuses a non-leader pid" branch. Reaching it
// requires handing `arm` a pid that is not a child spawned through
// `group_on_spawn`, and `arm` starts by MOVING its argument into its own group —
// so a test that passes any live pid manufactures the leader condition and then
// SIGKILLs that process's group. Writing the test is the hazard, which is why
// `arm` takes `&Child` instead of a bare pid. The reaping behaviour it guards is
// covered end-to-end by `unix_process_group_tests` in `lib.rs`.
//
// `register_embedder_group`'s rejection branch IS testable, because registration
// only READS process state — it neither relocates a process nor signals one, so a
// misuse test is inert rather than lethal.
#[cfg(test)]
mod embedder_group_tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    /// The guard that keeps a `kill(-pgid)` from naming the installer's own group and
    /// killing aube along with every sibling script.
    #[test]
    fn refuses_a_group_the_installer_belongs_to() {
        // SAFETY: a read of this process's own group id.
        let own = unsafe { libc::getpgrp() };
        assert!(register_embedder_group(own).is_none());
        // Whichever of the two guards catches it, a pid that does not lead its own
        // group is never enrollable either.
        assert!(register_embedder_group(std::process::id() as libc::pid_t).is_none());
        assert!(register_embedder_group(0).is_none());
        assert!(register_embedder_group(-1).is_none());
    }

    #[test]
    fn enrols_a_confirmed_leader_and_releases_its_slot_on_drop() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "sleep 5"]);
        // SAFETY: `setpgid` is async-signal-safe and touches no parent state.
        unsafe {
            command.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }
        let mut child = command.spawn().expect("spawn group leader");
        let pgid = child.id() as libc::pid_t;

        let registration = register_embedder_group(pgid).expect("a confirmed leader enrols");
        let slot = registration.slot;
        assert_eq!(REGISTRY[slot].load(Ordering::SeqCst), pgid);
        drop(registration);
        assert_eq!(
            REGISTRY[slot].load(Ordering::SeqCst),
            0,
            "the slot must be free again, or a later script cannot enrol"
        );

        // SAFETY: the group this test created, which it owns.
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
        let _ = child.wait();
    }
}

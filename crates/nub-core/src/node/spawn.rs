//! Node process spawning with augmentation: flag injection, PATH shim,
//! preload injection, env loading. The central pipeline that composes
//! all of Nub's runtime augmentation into a single child-process spawn.

use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{
    self as unix_fs, DirBuilderExt, MetadataExt as UnixMetadataExt, PermissionsExt,
};
#[cfg(windows)]
use std::os::windows::fs::MetadataExt as WindowsMetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Mutex;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
#[cfg(unix)]
use same_file::Handle as FileHandle;

use super::discovery::ResolvedNode;
use super::flags;

#[cfg(windows)]
#[derive(Debug)]
struct FileHandle {
    file: fs::File,
    identity: WindowsFileIdentity,
}

#[cfg(windows)]
#[derive(Debug, Eq, PartialEq)]
struct WindowsFileIdentity {
    volume_serial: u64,
    file_id: [u8; 16],
}

#[cfg(windows)]
fn windows_file_identity(
    volume_serial: u64,
    file_id: [u8; 16],
) -> std::io::Result<WindowsFileIdentity> {
    if file_id == [0; 16] || file_id == [u8::MAX; 16] {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "filesystem did not provide a stable 128-bit file identity",
        ));
    }
    Ok(WindowsFileIdentity {
        volume_serial,
        file_id,
    })
}

#[cfg(windows)]
impl FileHandle {
    fn from_path(path: impl AsRef<Path>) -> std::io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
        };

        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let mut info = FILE_ID_INFO::default();
        // SAFETY: `file` owns a valid handle; `info` is writable for exactly the
        // size reported, and both remain live for the call and returned record.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                std::ptr::addr_of_mut!(info).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            file,
            identity: windows_file_identity(info.VolumeSerialNumber, info.FileId.Identifier)?,
        })
    }

    fn as_file(&self) -> &fs::File {
        &self.file
    }
}

#[cfg(windows)]
impl PartialEq for FileHandle {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
    }
}

#[cfg(windows)]
impl Eq for FileHandle {}

/// Spawn a child, retrying briefly on a TRANSIENT `EAGAIN`/`ENOMEM` from the
/// kernel's `fork`/`clone` under peak thread/PID pressure. On a resource-
/// constrained box (the `nub ci` exit-101 family) the OS can momentarily refuse
/// a `fork` while siblings are mid-spawn; a short bounded backoff lets the
/// transient spike pass instead of surfacing a spurious spawn failure. Non-
/// transient errors (ENOENT, EACCES, …) propagate immediately — we never mask a
/// real failure, and the retry count is small so a persistent shortage still
/// fails fast rather than hanging.
pub fn spawn_with_eagain_retry(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    use std::io::ErrorKind;
    const MAX_RETRIES: u32 = 5;
    let mut attempt = 0u32;
    loop {
        match cmd.spawn() {
            Ok(child) => return Ok(child),
            Err(e) => {
                let transient = matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::OutOfMemory)
                    || e.raw_os_error() == Some(libc_eagain())
                    || e.raw_os_error() == Some(libc_enomem());
                if !transient || attempt >= MAX_RETRIES {
                    return Err(e);
                }
                // Exponential-ish backoff: 5ms, 10ms, 20ms, 40ms, 80ms — total
                // ~155ms worst case, long enough for a spawn spike to drain,
                // short enough to fail fast on a real shortage.
                let backoff_ms = 5u64 << attempt;
                std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                attempt += 1;
            }
        }
    }
}

#[cfg(unix)]
fn libc_eagain() -> i32 {
    libc::EAGAIN
}
#[cfg(unix)]
fn libc_enomem() -> i32 {
    libc::ENOMEM
}
#[cfg(not(unix))]
fn libc_eagain() -> i32 {
    -1
}
#[cfg(not(unix))]
fn libc_enomem() -> i32 {
    -1
}

/// Terminating-signal forwarding to the current child, registered once per
/// process. Nub catches SIGINT (Ctrl-C), SIGTERM (docker stop / systemd / CI
/// cancel) and SIGHUP (terminal hangup) and re-sends the SAME signal to the Node
/// child, so the child runs its own handler and exits with the matching code —
/// instead of being reparented to PID 1 and running forever, which is what
/// happened when only SIGINT was handled. A single background thread reads the
/// current child's pid from a global atomic that each spawn updates, so
/// sequential / re-entrant spawns forward to the right child, and a stray signal
/// after a child exits (pid cleared to 0) is a no-op rather than a kill of a
/// reused pid.
///
/// The diagnostic signals SIGUSR1, SIGUSR2 and SIGQUIT are forwarded too, for a
/// different reason than the terminating ones. Node assigns them meaning at the
/// child: SIGUSR1 activates the inspector / debugger, SIGUSR2 is the conventional
/// `--report-signal` trigger (and what tools like nodemon send), and SIGQUIT
/// reaches V8. Their DEFAULT disposition would terminate (SIGUSR1/USR2) or
/// terminate-and-core (SIGQUIT) the resident Rust PARENT — killing nub before the
/// child ever sees them. Registering a `signal-hook` handler for each overrides
/// that default disposition (the parent no longer dies), and the forwarder relays
/// the same signo to the child. Crucially, nub does NOT exit on these: unlike the
/// terminating set, nub keeps running and waits for the child after relaying, so
/// e.g. `kill -USR2 <nub>` writes a diagnostic report in the child and both
/// processes stay alive — exactly as if `node` had received the signal directly.
#[cfg(unix)]
mod ctrl_c {
    use std::sync::Once;
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

    // The forward TARGET, as the argument to `kill(2)`: a POSITIVE pid signals one
    // process (the file-run path's `node`, which IS the leaf); a NEGATIVE value
    // signals the whole PROCESS GROUP `-value` (the script path's `sh -c` child,
    // made a group leader via `setpgid`, so the signal reaches `sh` AND the `node`
    // it forks — a non-interactive `sh -c` does NOT relay signals to a forked
    // child, so single-pid delivery left the workload orphaned under dash). 0 = no
    // child tracked.
    static CURRENT_TARGET: AtomicI32 = AtomicI32::new(0);
    static REGISTERED: Once = Once::new();

    // When the controlling terminal's FOREGROUND process group has been handed to
    // the child (the interactive TTY path — see `foreground_child` in spawn.rs),
    // the kernel delivers a terminal Ctrl-C straight to the child. nub is no longer
    // in the foreground group, so it does NOT receive the TTY SIGINT — but a
    // `kill -INT <nub>` (non-TTY: a parent process, some CI cancels) still reaches
    // nub directly, and re-forwarding THAT to the foreground child would deliver
    // SIGINT twice (issue #26). So while the child owns the terminal we SUPPRESS
    // nub's SIGINT *forward* only: the TTY path is already exactly-once via the
    // kernel, and a direct `kill -INT <nub>` is intentionally swallowed (the child
    // is the foreground job; a shell running a foreground job behaves the same —
    // its own SIGINT isn't relayed to the job). SIGTERM/SIGHUP and the diagnostic
    // signals are unaffected and always forward. Reset on child exit.
    static SUPPRESS_SIGINT_FORWARD: AtomicBool = AtomicBool::new(false);

    /// Suppress (or re-enable) forwarding of SIGINT specifically — used while the
    /// child owns the terminal foreground, where the TTY already delivers Ctrl-C to
    /// the child directly and a forward would double it (issue #26). All other
    /// forwarded signals are unaffected.
    pub(super) fn set_suppress_sigint_forward(suppress: bool) {
        SUPPRESS_SIGINT_FORWARD.store(suppress, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(super) fn sigint_forward_suppressed() -> bool {
        SUPPRESS_SIGINT_FORWARD.load(Ordering::SeqCst)
    }

    /// Record the `kill(2)` target (see [`CURRENT_TARGET`]), registering the signal
    /// handler on the first call. Later calls just update the target.
    pub(super) fn track(target: i32) {
        CURRENT_TARGET.store(target, Ordering::SeqCst);
        REGISTERED.call_once(|| {
            use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2};
            use signal_hook::iterator::Signals;
            // signal-hook delivers the signo on a normal thread (via a self-pipe),
            // so `kill` here is not in an async-signal context. Forward the EXACT
            // signal Nub received to the child — TERM→TERM, HUP→HUP, INT→INT, and
            // the diagnostic USR1/USR2/QUIT→same — so the child runs its own handler
            // (terminating set exits 128+signo, byte-for-byte with plain Node; the
            // diagnostic set does whatever Node does with it). Merely listing a signal
            // in `Signals::new` installs a signal-hook handler for it, which overrides
            // the kernel's default disposition: that is what stops USR1/USR2 (default:
            // terminate) and QUIT (default: terminate+core) from killing the resident
            // parent before they can be relayed. If registration fails we simply don't
            // forward (the pre-existing no-handler behavior), never crash.
            if let Ok(mut signals) =
                Signals::new([SIGINT, SIGTERM, SIGHUP, SIGUSR1, SIGUSR2, SIGQUIT])
            {
                // `Builder::spawn` (returns `io::Result`) over `thread::spawn`
                // (which PANICS on OS thread-create failure): under thread/PID
                // exhaustion an EAGAIN here would otherwise crash the parent — and
                // under `panic = "abort"` abort it. On failure we simply don't
                // install the forwarder, identical to the `Signals::new` error path
                // (the pre-existing no-handler behavior), never crash.
                let _ = std::thread::Builder::new()
                    .name("nub-signal-forward".into())
                    .spawn(move || {
                        for signo in signals.forever() {
                            // While the child owns the terminal foreground, the kernel
                            // already delivered a TTY Ctrl-C to it directly — so a
                            // SIGINT forward here would double it (issue #26). Suppress
                            // SIGINT only; every other signal still forwards.
                            if signo == SIGINT && SUPPRESS_SIGINT_FORWARD.load(Ordering::SeqCst) {
                                continue;
                            }
                            let target = CURRENT_TARGET.load(Ordering::SeqCst);
                            if target != 0 {
                                // SAFETY: kill(2) with a stored-live target + the received
                                // signal. A positive target signals one process; a negative
                                // one signals process group `-target`. Benign if the
                                // child/group already exited (ESRCH); cleared to 0 on exit.
                                unsafe {
                                    libc::kill(target, signo);
                                }
                            }
                        }
                    });
            }
        });
    }

    /// Clear the current target after the child exits.
    pub(super) fn untrack() {
        CURRENT_TARGET.store(0, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(super) fn current() -> i32 {
        CURRENT_TARGET.load(Ordering::SeqCst)
    }
}

/// Track a child's process GROUP as the signal-forward target — for the `nub run`
/// script path, whose child is `sh -c <script>`. The script child is made a group
/// leader by [`group_on_spawn`], so signaling group `-pid` reaches `sh` AND the
/// `node` it forks. This is what `spawn_node`'s single-pid tracking can't do for
/// scripts: a non-interactive `sh -c` does not relay a forwarded signal to a
/// forked child, so `docker stop` on a `nub run` entrypoint orphaned the workload
/// (the Nub leader and `sh` exited; the `node` subtree ran on). No-op off Unix.
pub fn track_child_group(pid: u32) {
    #[cfg(unix)]
    ctrl_c::track(-(pid as i32));
    #[cfg(not(unix))]
    let _ = pid;
}

/// Clear the tracked child/group after it exits — pair with [`track_child_group`].
pub fn untrack_child() {
    #[cfg(unix)]
    ctrl_c::untrack();
}

/// Hand the controlling terminal's FOREGROUND process group to a just-spawned
/// child so an interactive full-screen TUI (Nx, turbo, `vitest --ui`, …) can read
/// the terminal and receive Ctrl-C directly — restoring nub's own group as
/// foreground when the returned guard drops (child exit).
///
/// Why this exists: #26 (f41f9a3) put the script/file child in its OWN process
/// group via [`group_on_spawn`] to make a terminal Ctrl-C deliver SIGINT exactly
/// once. But an own-group child is a BACKGROUND group w.r.t. the controlling TTY,
/// and a program that reads the terminal in raw mode then gets SIGTTIN and STOPS —
/// the unkillable "hang" of issue #27. The fix is the missing half of how a shell
/// runs a foreground job: `tcsetpgrp` the child's group to the terminal's
/// foreground. Now the TUI can read the terminal, and the kernel delivers Ctrl-C
/// straight to it — so we also suppress nub's now-redundant SIGINT forward (see
/// [`ctrl_c::set_suppress_sigint_forward`]) to keep #26's exactly-once.
///
/// Only meaningful when stdin is a real TTY and stdio is inherited — callers gate
/// on `foreground_handoff_applicable`. Returns `None` (no-op) off a TTY, off Unix,
/// or if the handoff syscalls fail, in which case behavior is exactly the prior
/// own-group-without-tcsetpgrp path (correct for non-interactive children).
#[cfg(unix)]
#[must_use]
fn foreground_child(child_pid: u32) -> Option<ForegroundGuard> {
    // SAFETY: pure FFI reads; STDIN_FILENO is always valid in this process.
    let stdin_is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    if !stdin_is_tty {
        return None;
    }
    // nub's own (current) foreground group, to restore on the child's exit. If we
    // can't read it, don't attempt the handoff — we'd have nothing to restore to.
    // SAFETY: tcgetpgrp on a TTY fd; returns -1 on error, which we treat as opt-out.
    let nub_pgrp = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
    if nub_pgrp < 0 {
        return None;
    }

    // `tcsetpgrp` from a process whose group is NOT the terminal's foreground
    // raises SIGTTOU (whose default disposition would STOP us). nub IS currently
    // the foreground group here, so it wouldn't fire — but ignore SIGTTOU around
    // the calls defensively (and because the restore on drop runs after we've
    // backgrounded ourselves, where it genuinely would). Save/restore the prior
    // disposition so we don't perturb anything else.
    // SAFETY: signal(2)/tcsetpgrp(3) FFI with valid args; child_pid is the live
    // group leader (it ran setpgid(0,0) at exec, so its pgid == its pid).
    unsafe {
        let prev = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        let rc = libc::tcsetpgrp(libc::STDIN_FILENO, child_pid as libc::pid_t);
        libc::signal(libc::SIGTTOU, prev);
        if rc != 0 {
            return None;
        }
    }

    // The child now owns the terminal: the kernel delivers TTY Ctrl-C to it
    // directly, so suppress nub's redundant SIGINT forward (#26 exactly-once).
    ctrl_c::set_suppress_sigint_forward(true);

    Some(ForegroundGuard { nub_pgrp })
}

/// Restores nub's own process group as the terminal foreground on drop, and
/// re-enables SIGINT forwarding. Pair with [`foreground_child`]; must outlive the
/// child's `wait()`.
#[cfg(unix)]
pub struct ForegroundGuard {
    nub_pgrp: libc::pid_t,
}

#[cfg(unix)]
impl Drop for ForegroundGuard {
    fn drop(&mut self) {
        // We are a BACKGROUND group at this point (the child held the foreground),
        // so `tcsetpgrp` here WOULD raise SIGTTOU — ignore it across the call.
        // SAFETY: FFI with a saved-valid pgrp and the controlling-TTY fd.
        unsafe {
            let prev = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            libc::tcsetpgrp(libc::STDIN_FILENO, self.nub_pgrp);
            libc::signal(libc::SIGTTOU, prev);
        }
        ctrl_c::set_suppress_sigint_forward(false);
    }
}

/// Put the spawned child in its own process group (`setpgid(0, 0)` at exec) so
/// [`track_child_group`] can signal the whole subtree. On Linux, additionally
/// arm `PR_SET_PDEATHSIG(SIGTERM)` as the kernel-side backstop for signals the
/// forwarder structurally cannot relay: SIGKILL on the Nub leader (Playwright's
/// default `webServer` teardown, `docker kill`, CI cancellation, the OOM killer)
/// is never delivered to userspace, so the forward thread never runs and the
/// child subtree would run on orphaned (#463). This guards BOTH callers: the
/// `nub run` script child (`sh -c` subtree) and the `nub <file>` file-run
/// `node` leaf — the orphan exposure is identical. With the pdeathsig armed
/// the kernel itself TERMs the child when Nub's spawning thread dies, no
/// matter how.
/// No-op off Unix; macOS has no pdeathsig equivalent, so it keeps the
/// forwarding-only behavior (the group forward covers every catchable signal).
pub fn group_on_spawn(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Captured BEFORE fork: the child's TOCTOU re-check below must compare
        // getppid() against the REAL parent, not a post-fork read that would
        // already name the reaper if the parent died between fork and prctl.
        #[cfg(target_os = "linux")]
        let parent = unsafe { libc::getpid() };
        // SAFETY: setpgid(0, 0) / prctl / getppid / raise between fork and exec
        // are all async-signal-safe and touch no parent state.
        unsafe {
            cmd.pre_exec(move || {
                libc::setpgid(0, 0);
                #[cfg(target_os = "linux")]
                {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                    // The pdeathsig only fires for deaths AFTER registration; if
                    // the parent died in the fork→prctl window, deliver the same
                    // signal ourselves instead of running on orphaned.
                    if libc::getppid() != parent {
                        libc::raise(libc::SIGTERM);
                    }
                }
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// The disarm byte [`GroupReaper::drop`] writes on the NORMAL teardown path, so
/// the watcher can tell "Nub reaped the workload and is shutting down cleanly"
/// (leave any deliberate background survivors alone, matching what plain
/// node/sh leave behind) from "Nub died" (bare EOF — tear the group down).
#[cfg(unix)]
const PDEATH_DISARM: u8 = b'D';

/// macOS backstop for the one signal the forwarder structurally cannot relay
/// and that Linux's pdeathsig (see [`group_on_spawn`]) covers kernel-side:
/// SIGKILL of the Nub leader — or of Nub's whole process group — must not
/// orphan the workload (#480). macOS has no `PR_SET_PDEATHSIG`, so Nub instead
/// plants a tiny WATCHER process inside the CHILD's process group:
///
/// - The watcher holds the read end of a pipe whose only write end lives in
///   Nub (both ends CLOEXEC; the read end is re-armed by `dup2` for the
///   watcher alone, so the workload never sees either fd). It blocks in
///   `read(2)`.
/// - Nub dying by ANY means — including SIGKILL, where the kernel closes its
///   fds — delivers EOF, and the watcher SIGTERMs the child's group (the same
///   signal pdeathsig delivers on Linux) and exits. EOF is level-triggered, so
///   unlike pdeathsig there is no died-before-registration TOCTOU window to
///   re-check.
/// - Living in the CHILD's group is what makes it SIGKILL-proof: a supervisor's
///   group-kill aimed at Nub's group cannot touch it. Membership also PINS the
///   pgid — a process group cannot be recycled while a member lives — so the
///   group kill can never land on a reused pgid.
///
/// On the normal path the guard writes [`PDEATH_DISARM`] before closing, and
/// the watcher exits without signaling anyone. The guard then reaps the
/// watcher, so nub processes that spawn many sequential children (workspace
/// `-r` runs) don't accumulate zombies. (`nub watch` is NOT covered: its
/// spawn path predates `group_on_spawn` entirely, so it has neither pdeathsig
/// nor this reaper — a pre-existing gap on both platforms.)
///
/// Returns `None` off macOS (Linux is already covered kernel-side; a second
/// mechanism would be redundant) and on any setup failure — the backstop
/// degrades to the status quo rather than failing the run. Known residual
/// windows, both milliseconds wide and of the same class as pdeathsig's
/// fork→prctl gap:
/// - a group-kill landing between the workload spawn and the watcher's own
///   `setpgid` still orphans (the watcher dies in Nub's group);
/// - macOS has no `pipe2(O_CLOEXEC)`, so a CONCURRENT `Command::spawn` on
///   another thread (workspace `--parallel`) forking between `pipe()` and the
///   `fcntl` below can duplicate the write end into an unrelated workload,
///   deferring the watcher's EOF until that workload also dies. The disarm
///   path is unaffected (the byte travels regardless of extra writers).
///
/// Divergence note: on Nub death this TERMs the whole GROUP (matching what
/// the forwarder does for catchable signals), while Linux's pdeathsig TERMs
/// only the direct child — a deliberate background survivor outlives a
/// force-killed nub on Linux but not on macOS. Group scope is what #480 asks
/// for; the asymmetry is accepted.
#[cfg(unix)]
pub fn spawn_group_reaper(child_pid: u32) -> Option<GroupReaper> {
    use std::os::unix::process::CommandExt;
    // cfg!(test) guard: nub-core's own in-process unit tests call the spawn
    // paths directly, where current_exe() is the libtest harness — re-invoking
    // THAT with watcher argv would re-run any test whose name substring-matches
    // the argv. The integration suite (pdeath_watch.rs) spawns the real `nub`
    // binary and exercises the watcher fully.
    if !cfg!(target_os = "macos") || cfg!(test) {
        return None;
    }
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: pipe(2) into a local array; F_SETFD on the two fresh fds.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return None;
        }
        for fd in fds {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    let close_both = || {
        // SAFETY: closing the two fds this function just opened.
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
    };
    // `current_exe()` is whatever NAME nub is running under — for any workload
    // spawned through nub's own PATH shim that is `node`, not `nub`. The verb
    // below is therefore dispatched in `cli::run()` ABOVE argv0 detection; when
    // it hung off the `nub`-only argv0 arm, a shim-named re-invocation ran
    // `__pdeath-watch` as a SCRIPT and spawned another watcher per level (regression from #504).
    let Ok(exe) = std::env::current_exe() else {
        close_both();
        return None;
    };
    let mut cmd = Command::new(exe);
    cmd.arg("__pdeath-watch")
        .arg(child_pid.to_string())
        .arg("3")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let pgid = child_pid as libc::pid_t;
    // SAFETY: setpgid / signal / dup2 / fcntl between fork and exec are
    // async-signal-safe and touch no parent state.
    unsafe {
        cmd.pre_exec(move || {
            // Join the WORKLOAD's group. It exists while its leader lives; if
            // the workload already exited there is nothing left to protect and
            // the watcher's membership self-check exits it immediately.
            libc::setpgid(0, pgid);
            // Ignore, HERE rather than after exec, everything the watcher can
            // be hit with as a group member: the forwarder's terminating set
            // (a docker-stop relay landing before the watcher's main ran would
            // otherwise kill it and silently drop the backstop — SIG_IGN
            // dispositions survive execve, unlike handlers) and the job-control
            // stops (a TTY Ctrl-Z would otherwise STOP it, and the guard's
            // reaping wait() would then hang on a stopped, never-dead child).
            for sig in [
                libc::SIGINT,
                libc::SIGTERM,
                libc::SIGHUP,
                libc::SIGUSR1,
                libc::SIGUSR2,
                libc::SIGQUIT,
                libc::SIGTSTP,
                libc::SIGTTIN,
                libc::SIGTTOU,
            ] {
                libc::signal(sig, libc::SIG_IGN);
            }
            // Re-arm ONLY the read end across the exec, at a fixed fd. dup2
            // clears CLOEXEC on the destination — except when src == dst, where
            // it is a no-op and the flag must be cleared explicitly.
            if read_fd == 3 {
                if libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(read_fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    match spawn_with_eagain_retry(&mut cmd) {
        Ok(watcher) => {
            // SAFETY: Nub's copy of the read end; the watcher holds its own.
            unsafe { libc::close(read_fd) };
            Some(GroupReaper { write_fd, watcher })
        }
        Err(_) => {
            close_both();
            None
        }
    }
}

/// Keeps the parent-death watcher's pipe write end open for the workload's
/// lifetime — hold it across `wait()`, drop it after. See [`spawn_group_reaper`].
#[cfg(unix)]
pub struct GroupReaper {
    write_fd: libc::c_int,
    watcher: std::process::Child,
}

#[cfg(unix)]
impl Drop for GroupReaper {
    fn drop(&mut self) {
        // Normal teardown: disarm (so deliberate background survivors in the
        // workload's group are left running, exactly as plain node/sh leave
        // them), close Nub's sole write end, and reap the watcher — it exits
        // within microseconds of the EOF, and reaping here keeps a long-lived
        // `nub watch` loop from accumulating zombies.
        // SAFETY: write/close on the fd this guard owns.
        unsafe {
            let byte = [PDEATH_DISARM];
            // EINTR-retry: a lost disarm byte would read as bare EOF — Nub
            // "died" — and TERM deliberate background survivors on a NORMAL
            // exit, the exact additivity break the byte exists to prevent.
            while libc::write(self.write_fd, byte.as_ptr().cast(), 1) < 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
            {}
            libc::close(self.write_fd);
        }
        let _ = self.watcher.wait();
    }
}

/// The `__pdeath-watch` hidden-verb entry: `<child-pgid> <read-fd>` — the
/// watcher half of [`spawn_group_reaper`]. Returns the process exit code.
#[cfg(unix)]
pub fn run_pdeath_watch(args: &[String]) -> i32 {
    let (Some(pgid), Some(fd)) = (
        args.first().and_then(|s| s.parse::<libc::pid_t>().ok()),
        args.get(1).and_then(|s| s.parse::<libc::c_int>().ok()),
    ) else {
        return 2;
    };
    // SAFETY: getpgrp/read/kill FFI on values this process owns.
    unsafe {
        // Signal dispositions were already set in the spawner's pre_exec (they
        // survive execve): the forwarded terminating set and the job-control
        // stops are all SIG_IGN, so the watcher acts on exactly one stimulus —
        // the pipe — and exits on its own. (SIGKILL still takes it down with
        // the group, as it should.)
        //
        // Membership self-check: pre_exec's setpgid can only have failed if the
        // workload group was already gone — nothing to protect. Being a member
        // is load-bearing (SIGKILL immunity + pgid pinning, see
        // `spawn_group_reaper`), so never watch from outside the group.
        if libc::getpgrp() != pgid {
            return 0;
        }
        let mut buf = [0u8; 1];
        loop {
            let n = libc::read(fd, buf.as_mut_ptr().cast(), 1);
            if n == 1 && buf[0] == PDEATH_DISARM {
                // Nub reaped the workload and is exiting cleanly: stand down
                // without signaling, leaving any deliberate background
                // survivors exactly as plain node/sh would.
                return 0;
            }
            if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            // EOF (Nub died) — or an unreadable pipe, where killing beats
            // orphaning.
            break;
        }
        libc::kill(-pgid, libc::SIGTERM);
    }
    0
}

/// Spawn `cmd` in its own process group and wait, forwarding terminating signals
/// to the whole group while it runs — the signal-faithful, subtree-reaching
/// equivalent of `cmd.status()` for a `sh -c <script>` child. Use for the `nub
/// run` script path so `docker stop` / Ctrl-C reach the script and everything it
/// spawns, not just Nub's leader.
pub fn status_forwarding_signals(cmd: &mut Command) -> std::io::Result<ExitStatus> {
    group_on_spawn(cmd);
    let mut child = spawn_with_eagain_retry(cmd)?;
    track_child_group(child.id());
    // SIGKILL-on-the-leader backstop (#480) — macOS-only inside; see
    // `spawn_group_reaper`. Held across the wait, dropped (disarmed) after.
    #[cfg(unix)]
    let _reaper = spawn_group_reaper(child.id());
    // Interactive path (stdin is a TTY + inherited stdio): hand the terminal
    // foreground to the child so a full-screen TUI can read it / receive Ctrl-C
    // (issue #27); the guard restores nub's foreground group on drop. No-op off a
    // TTY — the `kill -INT` forward then stays the sole, correct SIGINT path.
    #[cfg(unix)]
    let _fg = foreground_child(child.id());
    let status = child.wait();
    untrack_child();
    status
}

/// Configuration for spawning an augmented Node process.
pub struct SpawnConfig<'a> {
    /// The resolved Node binary.
    pub node: &'a ResolvedNode,
    /// User's original argv to pass to Node.
    pub user_args: &'a [String],
    /// Whether to skip all runtime augmentation (--node compat mode).
    pub compat_mode: bool,
    /// Nub's --show-warnings flag.
    pub show_warnings: bool,
    /// Path to the Nub binary itself (for the PATH shim).
    pub nub_binary: &'a Path,
    /// Parsed .env vars to inject into the child environment.
    pub env_vars: &'a std::collections::HashMap<String, String>,
    /// Yarn PnP `.pnp.cjs` path (from `nub_core::pnp::detect`), injected via
    /// `--require` ahead of nub's own preload so PnP's resolver patches install
    /// first. `None` when not in a PnP tree.
    pub pnp: Option<&'a std::path::Path>,
    /// Working directory for the spawned Node child. For `nub <file>` this is the
    /// process cwd (a no-op); the workspace-bin path threads each member's dir so
    /// a node bin run via `nub exec -r` executes IN the member, seeing its own
    /// `.env` / Node pin / `.bin` chain rather than the workspace root's.
    pub cwd: &'a Path,
}

/// The result of spawning a Node process.
pub struct SpawnResult {
    status: ExitStatus,
}

/// Spawn Node with Nub's augmentation pipeline.
///
/// In compat mode, spawns Node with only the user's args — no flag
/// injection, no preloads, no PATH shim.
pub fn spawn_node(config: &SpawnConfig<'_>) -> Result<SpawnResult> {
    let mut cmd = Command::new(config.node.path.as_str());
    // Process-identity fidelity: set argv0 to "node" so the spawned process
    // reports `process.title` and `process.argv0` as "node" — matching what
    // plain `node` reports when invoked by PATH name — instead of the full
    // resolved binary path that Rust passes by default. `process.execPath`
    // is NOT affected: it is populated by Node from the resolved binary path
    // (via `/proc/self/exe` on Linux, `_NSGetExecutablePath` on macOS) and
    // ignores argv0 entirely.
    //
    // Unix-only — and this is a hard platform boundary, not just a missing API:
    //   * Rust's `CommandExt::arg0` exists only on Unix; Windows passes a single
    //     command-line string whose token[0] is, by universal launcher
    //     convention, the executable path — there is no separate argv0 channel
    //     to override.
    //   * Even if there were, Node's `process.title` on Windows is NOT
    //     argv0-derived: libuv's `uv_get_process_title` reads
    //     `GetModuleFileNameW(NULL)` (the OS image path), so it is always the
    //     absolute `node.exe` path regardless of how the child was launched.
    // Crucially, plain Windows `node` reports that same full path for both
    // `process.title` and `process.argv0`, so nub does NOT diverge from Node on
    // Windows — there is nothing to fix there, and nothing the spawner could do
    // to force "node". (See crates/nub-cli/tests/process_identity.rs, which
    // asserts the Unix "node" invariant and the Windows path-passthrough one.)
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.arg0("node");
    }
    // Run the child in the configured cwd. For `nub <file>` this equals the
    // process cwd (a no-op); the workspace-bin path threads a member dir so a
    // node bin executes IN that member rather than inheriting the parent's cwd.
    cmd.current_dir(config.cwd);

    // Permission model detection and auto-grant.
    let has_permission = config.user_args.iter().any(|a| is_permission_flag(a));
    let has_allow_addons = config.user_args.iter().any(|a| a == "--allow-addons");

    if has_permission && !has_allow_addons && !config.compat_mode {
        anyhow::bail!(
            "nub: --permission requires --allow-addons\n\
             \x20\x20Nub's transpiler uses a native addon (oxc-transform).\n\
             \x20\x20Add --allow-addons to your Node permission flags,\n\
             \x20\x20or use --node to run without Nub's augmentation."
        );
    }

    if has_permission && !config.compat_mode {
        // Auto-grant read access to Nub's install directory.
        let install_dir = config
            .nub_binary
            .parent()
            .and_then(|p| p.parent())
            .unwrap_or(config.nub_binary);
        cmd.arg(format!("--allow-fs-read={}", install_dir.display()));
    }

    // Removes the compile-cache sentinel (R8) on drop, after the child exits.
    let mut _ccache_guard: Option<CompileCacheSentinelGuard> = None;

    // Our preload path is both the re-entrancy key and the thing we inject, so
    // resolve it once up front. Detect a re-entrant invocation (a parent nub
    // already augmented this process tree via the PATH shim) by checking
    // NODE_OPTIONS for OUR specific preload path — not a generic "preload.mjs"
    // substring, which would false-positive on a user's own `--import` of an
    // unrelated file named preload.mjs and silently disable augmentation (A26).
    let preload = find_preload(config.nub_binary);
    // The injected form is tier-specific: `--require <path>` (fast tier, CJS
    // preload.cjs) or `--import <url>` (compat tier, ESM preload.mjs). Re-entrancy
    // is detected by finding that exact `--flag=value` token in the child's
    // inherited NODE_OPTIONS — so key the check on the token nub actually injects,
    // not a bare path/URL. (On Windows the URL has forward slashes and a stripped
    // prefix; token-keying keeps the parent/child match consistent across both
    // tiers and platforms, and still can't false-positive on a user's unrelated
    // preload.mjs — A26.)
    let injection = preload
        .as_deref()
        .map(|p| preload_injection(p, &config.node.version));
    let reentrancy_key = injection.as_ref().map(|i| i.node_options_token());
    // Read the inherited NODE_OPTIONS once and reuse it for both the re-entrancy
    // check and the flag-injection block below — the env value is constant across
    // a single spawn.
    let node_options = env::var("NODE_OPTIONS").ok();
    let is_reentrant = is_reentrant_in(node_options.as_deref(), reentrancy_key.as_deref());

    // Augment only when we can locate our own preload. If `find_preload` fails —
    // a broken install, or (Windows, A-WIN2) the PATH-shim `node.exe` running
    // from a temp dir where the relative walk to `runtime/` can't reach (a
    // hardlink/copy, unlike a unix symlink, doesn't canonicalize back to the
    // install dir) — there is nothing to inject. Pass through instead: the child
    // inherits the parent's NODE_OPTIONS (absolute preload path) + PATH shim,
    // which already carry the augmentation, so re-augmenting here would only add
    // a half-setup (flags + a nested shim, no preload). See
    // wiki/runtime/hijack-by-default.md.
    if !config.compat_mode && !is_reentrant && preload.is_some() {
        // Flag injection — intersected with the binary's actual accepted-flag set
        // (probed + cached) so an open-ended `Unflag` band never injects a flag a
        // future Node has removed (which would abort startup with "bad option").
        let accepted = super::discovery::accepted_env_flags(config.node.path.as_std_path());
        let inject = flags::compute_inject_flags(
            config.node.version.clone(),
            config.user_args,
            node_options.as_deref(),
            config.show_warnings,
            accepted.as_ref(),
        );
        for flag in &inject {
            cmd.arg(flag);
        }

        // Web Storage: injected here, NOT through `compute_inject_flags`, so it sits
        // OUTSIDE the Stage-4 accepted-flag intersection above. Safe: its band is
        // CLOSED (`22.4–<25`) and the flag stabilized (not removed) at 25 — no
        // open-ended-removal hazard, so it needs no probe guard. (Any FUTURE
        // open-ended flag should go through `compute_inject_flags` to inherit the
        // guard, not this direct-injection path.)
        //
        // nub ALWAYS injects `--experimental-webstorage` on the band
        // where that flag is the enabling mechanism (Node 22.4 through <25, i.e.
        // `webstorage_flag_needed`), regardless of whether the user opted into
        // localStorage persistence (the maintainer, 2026-06-15: "a flag that we inject no
        // matter what"). On that band `sessionStorage` needs ONLY the flag (no file)
        // — gating it behind a `--localstorage-file` opt-in wrongly broke out-of-the-
        // box sessionStorage. So inject the flag unconditionally in-band; this makes
        // sessionStorage work everywhere on 22.4–24 and installs the `localStorage`
        // getter (which still throws `ERR_INVALID_ARG_VALUE` on ACCESS until the user
        // supplies a `--localstorage-file`). Empirically the flag alone does NOT throw
        // at startup on 22.4–24, so always-injecting is safe.
        //
        // nub NEVER synthesizes `--localstorage-file` — localStorage persistence
        // stays the user's explicit opt-in (forwarded verbatim if they pass it).
        //
        // Scope is exactly the `webstorage_flag_needed` band: below 22.4 the flag is
        // an unrecognized "bad option" (would crash startup), and on 25+ Web Storage
        // is native so the flag is unnecessary. Skip the inject when the user already
        // supplied `--experimental-webstorage` / `--no-experimental-webstorage` (no
        // double-add; respect an explicit disable — nub never re-enables over a user
        // negation).
        if should_inject_webstorage_flag(
            &config.node.version,
            config.user_args,
            node_options.as_deref(),
        ) {
            cmd.arg("--experimental-webstorage");
        }

        // Web Storage localStorage neutralization: on the band where nub injects
        // `--experimental-webstorage` AND the user did NOT supply their own
        // `--localstorage-file`, the injected flag installs a `localStorage` getter
        // that throws `ERR_INVALID_ARG_VALUE` on access (even `typeof localStorage`
        // throws). Signal nub's startup preload to replace that throwing getter with
        // a plain `undefined` value — matching Node 25+'s clean shape so
        // `typeof localStorage === "undefined"` feature-detection is safe — while
        // `sessionStorage` (which needs only the flag) keeps working out of the box.
        // When the user passes `--localstorage-file`, this is skipped and
        // `localStorage` works normally. The signal is an internal `__NUB_*` env var
        // (brand-boundary-permitted plumbing); the preload deletes it after reading.
        if should_neutralize_localstorage(
            &config.node.version,
            config.user_args,
            node_options.as_deref(),
        ) {
            cmd.env(NEUTRALIZE_LOCALSTORAGE_ENV, "1");
        }

        // PATH shim: prepend a temp dir with a `node` symlink → nub.
        if let Ok(shim_dir) = setup_path_shim(config.nub_binary) {
            let mut new_path = std::ffi::OsString::from(shim_dir.as_str());
            if let Some(existing) = env::var_os("PATH") {
                new_path.push(crate::PATH_LIST_SEPARATOR);
                new_path.push(existing);
            }
            cmd.env("PATH", new_path);
        }

        // `process.versions.nub` source: hand the running binary's version to the
        // preload so it can publish the self-identification marker. Coupled to the
        // preload injection below — both live in this augment block, so `--node`
        // and re-entrant child shells skip it for free.
        cmd.env(VERSION_ENV, env!("CARGO_PKG_VERSION"));

        // Force the async loader-worker tier when this child hosts a foreign async
        // loader (tsx/ts-node/--import) on a Node whose sync/async hook composition
        // is broken — the sync fast tier would otherwise crash with
        // ERR_METHOD_NOT_IMPLEMENTED (see force_async_tier_env / node_hook_compose_broken).
        if let Some((k, val)) = force_async_tier_env(
            &config.node.version,
            config.user_args.iter().map(String::as_str),
        ) {
            cmd.env(k, val);
        }

        // Value-bearing preload/PnP `--require`/`--import` flags are NOT passed via
        // argv here — ONLY via NODE_OPTIONS (assembled below). The direct child
        // inherits that NODE_OPTIONS, so it is still fully augmented; keeping the
        // flags off argv avoids a fork-reconstruction hazard: a child that rebuilds
        // its Node flags by MERGING process.execArgv + NODE_OPTIONS (Next's
        // getParsedNodeOptions→formatNodeOptions, jest-worker) would collect the
        // SAME preload/PnP path from both channels and space-join the duplicate into
        // one broken `--require "a b"`, dying with `Cannot find module 'a b'`. Boolean
        // flags above stay argv-safe (idempotent when merged twice); only these
        // value-bearing, path-carrying flags double destructively, so only they are
        // routed single-channel. NODE_OPTIONS `--require` preserves the R1 sync-entry
        // semantics identically to argv (R1 is the `--require`-vs-`--import` tier
        // choice — see PreloadInjection — not the argv-vs-NODE_OPTIONS channel), and
        // this matches the already-single-channel `compute_augmentation_env` script
        // path. PnP's install-before-preload ordering is preserved by the
        // NODE_OPTIONS token order (PnP token pushed before the preload token below).

        // Coverage-exclude nub's own runtime (R9). When the user runs the test
        // runner under `--experimental-test-coverage`, Node instruments every
        // module it loads — including nub's preloaded runtime/*.mjs — and folds
        // them into the user's coverage report, tanking the aggregate (a 100% TS
        // fixture drops to ~55%) and adding phantom rows. Node accepts MULTIPLE
        // `--test-coverage-exclude=<glob>` flags, so we add one more keyed to the
        // ABSOLUTE nub runtime dir (the directory holding the preload injected via
        // NODE_OPTIONS below) — never a broad `**/runtime/**`, which would also
        // exclude a user's own `runtime/` source. This flag is safe to pass on argv
        // even though it also rides NODE_OPTIONS: it is repeatable, so a merged
        // duplicate is two independent exclude tokens (a harmless re-exclude), not a
        // space-joined single value like the preload/PnP `--require` above.
        if flags::test_coverage_exclude_supported(&config.node.version) {
            if let Some(glob) = coverage_exclude_glob(
                config.user_args,
                node_options.as_deref(),
                preload.as_deref(),
            ) {
                cmd.arg(glob);
            }
        }

        // Compile-cache pollution fix (R8). When the user sets NODE_COMPILE_CACHE
        // (or NODE_OPTIONS carries --use-compile-cache), Node enables the V8 code
        // cache AT BOOTSTRAP — *before* the user entry — so every module nub's
        // `--require` preload chain pulls in (preload.cjs, transform-core.mjs,
        // preload-common.cjs, polyfills.cjs, …) gets compiled-and-cached into the
        // USER's cache dir. A program that does `fs.readdirSync(NODE_COMPILE_CACHE)`
        // then sees ~9 nub entries instead of its own 1 (program-observable).
        //
        // Fix: STRIP NODE_COMPILE_CACHE from the child env so bootstrap caches
        // NOTHING, then hand the original dir to the preload through a non-env
        // sentinel file (brand rule: no NUB_* env var, and we must not leave the
        // var visible in the child's `printenv`). The preload, AFTER all nub setup
        // and right before user code, calls `module.enableCompileCache(dir)` so the
        // user's OWN modules still cache into their dir — the feature keeps working,
        // only the preload chain is excluded. The sentinel path is keyed on nub's
        // PID; the child reads it from `process.ppid` (nub is its direct parent).
        // Unlike the randomized PATH shim, this private parent/child handoff stays
        // PID-only intentionally.
        //
        // COVERAGE GATE (compile-cache vs V8 coverage). A WARM compile cache makes
        // V8's coverage imprecise: cached bytecode collapses/omits per-branch ranges,
        // so under `--experimental-test-coverage` / NODE_V8_COVERAGE the line/branch
        // percentages inflate and ranges collapse vs plain node — silently. So when
        // THIS nub invocation is itself collecting coverage (flag in argv/NODE_OPTIONS,
        // or NODE_V8_COVERAGE in env — same signal coverage_exclude_glob keys on, plus
        // the env var), set up NO compile cache at all: no default dir, and don't honor
        // a user-set one for this run either (coverage precision wins over their cache;
        // it's a single coverage run). The complementary case — a coverage child that
        // nub's OWN spawn path never sees because the user's test code spawns it
        // directly — is handled in the preload (reenableUserCompileCache sets
        // NODE_DISABLE_COMPILE_CACHE=1 so descendants boot cache-off).
        let node_v8_coverage = env::var("NODE_V8_COVERAGE").ok();
        let coverage = coverage_active_for_cache(
            config.user_args,
            node_options.as_deref(),
            node_v8_coverage.as_deref(),
        );
        if let Some(dir) = env::var("NODE_COMPILE_CACHE")
            .ok()
            .filter(|s| !s.is_empty())
        {
            // A user-set NODE_COMPILE_CACHE is honored ALWAYS — including under
            // coverage (the maintainer, 2026-06-11: an explicit user flag clobbers any
            // default nub sets; their coverage numbers may be cache-affected, the
            // same tradeoff they'd have on plain node). Normal R8 strip+sentinel.
            cmd.env_remove("NODE_COMPILE_CACHE");
            if write_compile_cache_sentinel(&dir).is_ok() {
                _ccache_guard = Some(CompileCacheSentinelGuard);
            }
        } else if coverage {
            // No user cache + coverage active: suppress nub's DEFAULT compile
            // cache (a warm V8 cache collapses/omits per-branch coverage ranges,
            // silently inflating `--experimental-test-coverage` / NODE_V8_COVERAGE
            // numbers vs plain node). Drop any empty-string env and write no
            // sentinel, so the preload's restore finds nothing. Mirrored in the JS
            // half (preload-common.cjs reenableUserCompileCache) for coverage
            // children nub's spawn path never sees.
            cmd.env_remove("NODE_COMPILE_CACHE");
        } else if let Some(dir) = default_compile_cache_dir() {
            // Default-on compile cache (decided 2026-06-10, measured): when the
            // user hasn't set NODE_COMPILE_CACHE, point it at a nub-owned dir.
            // Big single-file bundles gain tens of ms per invocation (pnpm −70ms,
            // typescript.js −67ms — verified working through nub's full hook chain
            // via NODE_DEBUG_NATIVE=COMPILE_CACHE: blobs accepted on read, persist
            // skipped when unchanged); small graphs measure at noise, and a stale/
            // incompatible blob is validated-and-rejected by V8, never trusted.
            //
            // Route it through the SAME strip+sentinel dance as the user-set branch
            // (not a bare `cmd.env(NODE_COMPILE_CACHE, dir)`). Leaving the dir in the
            // child env meant EVERY descendant — including a coverage child the user's
            // test code spawns directly (`spawnSync(execPath, [fixtureWithCoverage])`),
            // which nub's own spawn path never sees — inherited it and enabled the
            // cache AT BOOTSTRAP, before any preload could gate it, collapsing that
            // child's V8 coverage ranges (the test-runner coverage-width snapshot
            // tests). With the sentinel, NODE_COMPILE_CACHE is absent from the child
            // env, so nothing boots cache-warm; each nub-preloaded process re-enables
            // the cache post-bootstrap via reenableUserCompileCache, which SKIPS the
            // re-enable (and sets NODE_DISABLE_COMPILE_CACHE=1 for its own descendants)
            // when that process is collecting coverage. Cost: the preload chain itself
            // is no longer bootstrap-cached on the default path — but that chain was
            // never the perf target (big user bundles are), and not caching nub's own
            // modules is strictly better for the R8 pollution invariant too.
            // Escape hatches unchanged: NODE_COMPILE_CACHE yourself, or
            // NODE_DISABLE_COMPILE_CACHE=1 (honored by Node).
            cmd.env_remove("NODE_COMPILE_CACHE");
            if let Some(dir) = dir.to_str()
                && write_compile_cache_sentinel(dir).is_ok()
            {
                _ccache_guard = Some(CompileCacheSentinelGuard);
            }
        }

        // Dual-channel injection: set NODE_OPTIONS so hardcoded-path `node`
        // invocations inherit the preload + flags. We only reach here when NOT
        // re-entrant — i.e. NODE_OPTIONS does not already carry our preload — so
        // always (re)build it, appending any pre-existing NODE_OPTIONS. (The old
        // `already_injected` guard checked the same full path and is subsumed by
        // `is_reentrant` above.) Reuses the NODE_OPTIONS read at the top of the
        // function rather than re-reading the (constant) env value.
        let existing_opts = node_options
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let mut node_opts_parts: Vec<String> = Vec::new();
        for flag in &inject {
            node_opts_parts.push(flag.to_string());
        }
        // Yarn PnP token BEFORE nub's preload token, mirroring the argv order
        // above so hardcoded-path `node` invocations inherit PnP-first ordering.
        // Quoted so a `.pnp.cjs` under a spacey path survives the tokenizer.
        if let Some(pnp) = config.pnp {
            node_opts_parts.push(format!(
                "--require={}",
                node_options_quote(&pnp.display().to_string())
            ));
        }
        if let Some(ref inj) = injection {
            node_opts_parts.push(inj.node_options_token());
        }
        // Coverage-exclude nub's own runtime (R9) — via NODE_OPTIONS, not just argv.
        // The CLI-arg form at the `cmd.arg(glob)` site above only reaches the DIRECT
        // child nub spawns. But the test-runner coverage fixtures spawn the actual
        // coverage child via `process.execPath` (their own `spawnSync`), which nub
        // never sees: that grandchild inherits nub's preload ONLY through NODE_OPTIONS
        // (carrying `--require=runtime/preload.cjs`), so without the exclude flag ALSO
        // in NODE_OPTIONS, nub's runtime modules get instrumented into the user's
        // coverage report — phantom rows + a skewed `all files` aggregate.
        //
        // It is NOT gated on `coverage_active`: the parent nub can't observe that the
        // grandchild will enable coverage (the flag lives in the fixture's own argv),
        // so we inject the exclude whenever a preload is present AND the target Node
        // actually has the flag. Node >= 22.5 accepts `--test-coverage-exclude` in
        // NODE_OPTIONS and treats it as a harmless no-op when coverage is off; below
        // 22.5 the flag does not exist and is REJECTED in NODE_OPTIONS ("not allowed
        // in NODE_OPTIONS"), aborting every nub invocation before it runs a line — so
        // it must be version-gated exactly like --disable-warning / webstorage.
        if flags::test_coverage_exclude_supported(&config.node.version)
            && let Some(ref p) = preload
            && let Some(runtime_dir) = Path::new(p).parent()
        {
            // Quote the glob value: the runtime dir can sit under a spacey
            // install path (Windows `Program Files`, macOS `Application
            // Support`); an unquoted space would split the flag and either
            // abort ("not allowed in NODE_OPTIONS" on the fragment) or
            // silently drop the exclude.
            node_opts_parts.push(format!(
                "--test-coverage-exclude={}",
                node_options_quote(&format!("{}/**", runtime_dir.display()))
            ));
        }
        // Web Storage (mirrors the argv site above): always inject
        // `--experimental-webstorage` into NODE_OPTIONS on the flag-needed band
        // (22.4–24.x), regardless of any `--localstorage-file` opt-in, so a child
        // `node` re-invocation inherits the flag and `sessionStorage` works out of
        // the box. nub never synthesizes `--localstorage-file`. Same guard: only
        // in-band, and not if the user already supplied/disabled the flag.
        if should_inject_webstorage_flag(
            &config.node.version,
            config.user_args,
            node_options.as_deref(),
        ) {
            node_opts_parts.push("--experimental-webstorage".to_string());
        }
        if let Some(existing) = existing_opts {
            // An INHERITED NODE_OPTIONS (ancestor nub or user-set) is appended
            // verbatim EXCEPT we first snip any version-gated flag whose floor
            // exceeds the child's Node version — otherwise a gated flag the child
            // can't parse (e.g. --experimental-webstorage on Node <22.4) aborts it
            // with exit 9 ("not allowed in NODE_OPTIONS"). See
            // flags::strip_unsupported_node_options.
            let stripped = flags::strip_unsupported_node_options(&existing, &config.node.version);
            if !stripped.is_empty() {
                node_opts_parts.push(stripped);
            }
        }
        if !node_opts_parts.is_empty() {
            cmd.env("NODE_OPTIONS", node_opts_parts.join(" "));
        }

        // NODE_PATH so the transpile's bare helper requires (e.g.
        // `@oxc-project/runtime/helpers/decorate` for decorators) resolve to
        // nub's vendored runtime deps. The ESM-import form is handled by the
        // resolve hook (VENDORED_PACKAGES), but a CJS `require()` bypasses the
        // hook and uses Node's native resolver, which only finds them via
        // NODE_PATH. No-op in dev (runtime/ has no node_modules → walk-up to the
        // repo's), active for an installed package (A30).
        if let Some(node_path) = vendored_node_path(preload.as_deref()) {
            cmd.env("NODE_PATH", node_path);
        }
    }

    // .env vars injected by the CLI layer.
    for (k, v) in config.env_vars {
        cmd.env(k, v);
    }

    // User args always pass through.
    cmd.args(config.user_args);

    // Inherit stdio.
    cmd.stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    // Put the Node child in its OWN process group (setpgid at exec) so an
    // interactive Ctrl-C is delivered EXACTLY ONCE. Without this the child shares
    // nub's process group on the controlling TTY, so a terminal Ctrl-C makes the
    // kernel deliver SIGINT to the WHOLE foreground group — the child receives it
    // directly — AND nub's forwarder (below) re-sends it, so the child's
    // `process.on('SIGINT')` fired TWICE per Ctrl-C (issue #26; plain `node` fires
    // once). With the child in its own group, the TTY signals only nub's group;
    // nub then forwards once via the group target below — a single delivery that
    // matches plain Node, while a non-TTY `kill -INT <nub>` still reaches the child
    // through that same forward (the TTY is not the only path, so own-group is
    // required for both cases to be correct). Forwarding to the group `-pid` (not a
    // bare pid) also reaches anything the Node child itself spawns (dev-server
    // subprocesses). No-op off Unix; Windows has no process-group SIGINT semantics
    // and keeps its existing behavior.
    group_on_spawn(&mut cmd);

    let mut child = spawn_with_eagain_retry(&mut cmd)
        .with_context(|| format!("failed to spawn {}", config.node.path))?;

    // Forward terminating/diagnostic signals to the child's process GROUP.
    // Registered once; the current target lives in a global atomic (see `ctrl_c`).
    // The child is its own group leader (see `group_on_spawn` above), so the
    // negative target signals the child and its descendants exactly once.
    // (No-op off Unix.)
    track_child_group(child.id());

    // SIGKILL-on-the-leader backstop (#480) — macOS-only inside; see
    // `spawn_group_reaper`. Held across the wait, dropped (disarmed) after.
    #[cfg(unix)]
    let _reaper = spawn_group_reaper(child.id());

    // Interactive path: hand the terminal foreground to the child (issue #27) so a
    // `nub <file>` that draws a full-screen TUI can read the terminal and receive
    // Ctrl-C directly. Gated on stdin being a TTY inside `foreground_child`; the
    // guard restores nub's foreground group + re-enables SIGINT forward on drop.
    #[cfg(unix)]
    let _fg = foreground_child(child.id());

    let status = child.wait().with_context(|| "waiting for Node child")?;

    // Stop forwarding to this (now-exited) group before returning. (No-op off Unix.)
    untrack_child();

    Ok(SpawnResult { status })
}

/// Path of the compile-cache sentinel file (R8) for a given nub PID. spawn.rs
/// writes the user's original `NODE_COMPILE_CACHE` dir here keyed on nub's own
/// PID; the child preload reads it from a path derived from `process.ppid` (nub
/// is the child's direct parent).
fn compile_cache_sentinel_path(nub_pid: u32) -> PathBuf {
    compile_cache_tmpdir().join(format!("nub-ccache-{nub_pid}"))
}

/// The temp dir for the compile-cache sentinel, resolved to MATCH the JS side's
/// `tmpdirNoOs()` (preload-common.cjs) so both ends agree on the path. Both must
/// resolve identically or the child can't find the sentinel nub wrote — which
/// silently disables the compile cache (the symptom: the default cache never
/// populates when TMPDIR is unset). We deliberately do NOT use `env::temp_dir()`:
/// on macOS it returns the per-user Darwin confstr dir (`/var/folders/.../T`) even
/// when TMPDIR is unset, whereas Node's `os.tmpdir()` falls back to `/tmp` — so the
/// two disagree in a clean (`env -i`) environment, exactly the case the corpus
/// harness spawns under (it forwards only PATH + HOME, not TMPDIR). Mirror Node's
/// libuv resolution: POSIX TMPDIR→TMP→TEMP→/tmp, Win32 TEMP→TMP→SystemRoot\temp,
/// trailing-separator-stripped — identical to tmpdirNoOs(). nub forwards its own
/// env to the child, so resolving from nub's env vars yields the child's view.
fn compile_cache_tmpdir() -> PathBuf {
    // Read the live process env; the resolution logic is pure over a lookup so it can
    // be table-tested without mutating process env (see compile_cache_tmpdir_from).
    compile_cache_tmpdir_from(|k| env::var(k).ok().filter(|s| !s.is_empty()))
}

/// Pure resolver behind [`compile_cache_tmpdir`]: given an env lookup that returns
/// `None` for unset/empty, reproduce Node's libuv `os.tmpdir()` env resolution
/// (POSIX: TMPDIR→TMP→TEMP→/tmp; Win32: TEMP→TMP→SystemRoot/windir+\temp),
/// trailing-separator-stripped. Kept byte-parity with the JS `tmpdirNoOs()`
/// (preload-common.cjs) so both ends agree on the sentinel path. Injectable so the
/// table test never touches process env (the suite runs tests in parallel).
fn compile_cache_tmpdir_from(lookup: impl Fn(&str) -> Option<String>) -> PathBuf {
    let strip_trailing = |mut s: String, sep: char| -> String {
        if s.len() > 1 && s.ends_with(sep) && !s.ends_with(&format!(":{sep}")) {
            s.pop();
        }
        s
    };
    if cfg!(windows) {
        let dir = lookup("TEMP").or_else(|| lookup("TMP")).unwrap_or_else(|| {
            let root = lookup("SystemRoot")
                .or_else(|| lookup("windir"))
                .unwrap_or_default();
            format!("{root}\\temp")
        });
        return PathBuf::from(strip_trailing(dir, '\\'));
    }
    let dir = lookup("TMPDIR")
        .or_else(|| lookup("TMP"))
        .or_else(|| lookup("TEMP"))
        .unwrap_or_else(|| "/tmp".to_string());
    PathBuf::from(strip_trailing(dir, '/'))
}

/// Write the user's original compile-cache dir to this nub process's sentinel
/// file. The preload reads + deletes it, then calls
/// `module.enableCompileCache(dir)` so the user's own modules cache into their
/// dir while nub's stripped-out preload chain never does (R8). Best-effort: a
/// write failure just means the child won't re-enable compile cache (no
/// pollution either way, since we've already stripped the env var).
fn write_compile_cache_sentinel(dir: &str) -> std::io::Result<()> {
    fs::write(compile_cache_sentinel_path(std::process::id()), dir)
}

/// Removes this process's compile-cache sentinel on drop (R8). The preload
/// deletes it on read in the common case; this guard reclaims it if the child
/// exited before reading (early crash, bad flag) so we never leak the file.
struct CompileCacheSentinelGuard;

impl Drop for CompileCacheSentinelGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(compile_cache_sentinel_path(std::process::id()));
    }
}

const PATH_SHIM_PREFIX: &str = "nub-node-shim-";
const PATH_SHIM_CREATE_RETRIES: usize = 16;

static PATH_SHIM_MANAGER: PathShimManager = PathShimManager::new();

struct PathShimManager {
    state: Mutex<Option<ShimRecord>>,
}

struct ShimRecord {
    dir: PathBuf,
    dir_identity: FileHandle,
    node_identity: FileHandle,
}

#[derive(Default)]
struct ShimSetupOptions {
    #[cfg(windows)]
    force_copy: bool,
    #[cfg(test)]
    fail_before_commit: bool,
}

struct PendingShimDir {
    path: PathBuf,
    pid: u32,
    identity: Option<FileHandle>,
    armed: bool,
}

impl PendingShimDir {
    fn new(path: PathBuf, pid: u32) -> Self {
        Self {
            path,
            pid,
            identity: None,
            armed: true,
        }
    }

    fn set_identity(&mut self, identity: FileHandle) {
        self.identity = Some(identity);
    }

    fn identity(&self) -> &FileHandle {
        self.identity
            .as_ref()
            .expect("created PATH shim directory has an identity")
    }

    fn disarm(mut self) -> FileHandle {
        let identity = self
            .identity
            .take()
            .expect("published PATH shim directory has an identity");
        self.armed = false;
        identity
    }
}

impl Drop for PendingShimDir {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(created) = self.identity.take() else {
            let _ = fs::remove_dir(&self.path);
            return;
        };
        let unchanged =
            validate_shim_dir(&self.path, false).is_ok_and(|current| current.eq(&created));
        if unchanged {
            remove_shim_entry(&node_shim_path(&self.path));
            remove_shim_entry(&staging_shim_path(&self.path, self.pid));
            let _ = fs::remove_dir(&self.path);
        }
    }
}

impl PathShimManager {
    const fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }

    fn setup(&self, nub_binary: &Path) -> Result<Utf8PathBuf> {
        self.setup_in(
            nub_binary,
            &env::temp_dir(),
            std::process::id(),
            secure_shim_nonce,
            ShimSetupOptions::default(),
        )
    }

    fn setup_in(
        &self,
        nub_binary: &Path,
        temp_dir: &Path,
        pid: u32,
        mut nonce: impl FnMut() -> Result<[u8; 16]>,
        options: ShimSetupOptions,
    ) -> Result<Utf8PathBuf> {
        // Declared before every source/node handle so an error unwinds those
        // handles before the pending directory guard tries to remove entries.
        // This matters on Windows, where a delete-pending hardlink keeps its
        // directory entry until the last file handle closes.
        let mut pending = None;
        let mut cached_validation_error = None;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(record) = state.as_ref() {
            match validate_shim_record(record) {
                Ok(()) => return shim_dir_utf8(record.dir.clone()),
                Err(error) => {
                    cached_validation_error = Some(format!("{error:#}"));
                    // Keep the manager lock held while retiring the invalid
                    // record and publishing its replacement. Concurrent callers
                    // can then observe only the old record before validation or
                    // the fully validated replacement, never an empty window.
                    // Do not clean the invalid path here: validation failed, so
                    // pathname-based cleanup can no longer prove its identity.
                    drop(state.take().expect("cached PATH shim record is present"));
                }
            }
        }

        let nub_binary = fs::canonicalize(nub_binary)
            .with_context(|| format!("resolving Nub binary: {}", nub_binary.display()))?;
        let nub_identity = validate_nub_binary(&nub_binary)?;

        for _ in 0..PATH_SHIM_CREATE_RETRIES {
            let suffix = nonce_hex(nonce()?);
            let path = temp_dir.join(format!("{PATH_SHIM_PREFIX}{pid}-{suffix}"));
            match create_shim_dir(&path) {
                Ok(()) => {
                    pending = Some(PendingShimDir::new(path, pid));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("creating PATH shim dir: {}", path.display()));
                }
            }
        }
        let pending_dir = pending.as_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "could not create a collision-free PATH shim directory after {PATH_SHIM_CREATE_RETRIES} attempts"
            )
        })?;

        set_private_shim_permissions(&pending_dir.path)?;
        let dir_identity = validate_shim_dir(&pending_dir.path, true)?;
        pending_dir.set_identity(dir_identity);
        let published =
            publish_node_shim(&nub_binary, &nub_identity, &pending_dir.path, pid, &options)?;
        let utf8_dir = shim_dir_utf8(pending_dir.path.clone())?;
        validate_shim_candidate(&pending_dir.path, pending_dir.identity(), &published)?;
        #[cfg(test)]
        if options.fail_before_commit {
            anyhow::bail!("injected PATH shim commit failure");
        }
        let dir_identity = pending
            .take()
            .expect("validated PATH shim directory is pending")
            .disarm();

        let record = ShimRecord {
            dir: PathBuf::from(utf8_dir.as_str()),
            dir_identity,
            node_identity: published,
        };
        *state = Some(record);
        drop(state);
        if let Some(error) = cached_validation_error {
            tracing::warn!(
                error = %error,
                "cached Node PATH shim failed validation; created a replacement"
            );
        }
        Ok(utf8_dir)
    }

    fn cleanup(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(record) = state.take() else { return };
        let ShimRecord {
            dir,
            dir_identity,
            node_identity,
        } = record;
        let valid_dir =
            validate_shim_dir(&dir, true).is_ok_and(|current| current.eq(&dir_identity));
        let node = node_shim_path(&dir);
        let valid_node = open_node_shim(&node).is_ok_and(|current| current.eq(&node_identity));
        if valid_dir && valid_node {
            // Windows retains a delete-pending file's directory entry until
            // its last handle closes. Keep only the directory handle live
            // across the bounded file + empty-directory removal.
            drop(node_identity);
            remove_shim_entry(&node);
            let _ = fs::remove_dir(&dir);
        }
        drop(dir_identity);
    }
}

fn setup_path_shim(nub_binary: &Path) -> Result<Utf8PathBuf> {
    PATH_SHIM_MANAGER.setup(nub_binary)
}

fn secure_shim_nonce() -> Result<[u8; 16]> {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|error| anyhow::anyhow!("generating PATH shim nonce: {error}"))?;
    Ok(nonce)
}

fn nonce_hex(nonce: [u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(32);
    for byte in nonce {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0xf) as usize] as char);
    }
    encoded
}

fn create_shim_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).create(path)
    }
    #[cfg(windows)]
    {
        // The exclusive random leaf inherits the established per-user TEMP
        // DACL; validation separately rejects directory reparse points.
        fs::create_dir(path)
    }
}

fn set_private_shim_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting PATH shim permissions: {}", path.display()))?;
    }
    #[cfg(windows)]
    let _ = path;
    Ok(())
}

fn validate_nub_binary(path: &Path) -> Result<FileHandle> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading Nub binary metadata: {}", path.display()))?;
    if !metadata.file_type().is_file() || is_reparse_point(&metadata) {
        anyhow::bail!("Nub binary is not a regular file: {}", path.display());
    }
    FileHandle::from_path(path)
        .with_context(|| format!("opening Nub binary identity: {}", path.display()))
}

fn validate_shim_dir(path: &Path, require_private: bool) -> Result<FileHandle> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading PATH shim dir metadata: {}", path.display()))?;
    if !metadata.file_type().is_dir() || is_reparse_point(&metadata) {
        anyhow::bail!("PATH shim path is not a real directory: {}", path.display());
    }
    #[cfg(unix)]
    {
        // SAFETY: `geteuid` has no preconditions.
        let euid = unsafe { libc::geteuid() };
        if metadata.uid() != euid {
            anyhow::bail!("PATH shim directory is not owned by the current user");
        }
        if require_private && metadata.mode() & 0o7777 != 0o700 {
            anyhow::bail!("PATH shim directory permissions are not 0700");
        }
    }
    #[cfg(windows)]
    let _ = require_private;
    FileHandle::from_path(path)
        .with_context(|| format!("opening PATH shim dir identity: {}", path.display()))
}

#[cfg(unix)]
fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn node_shim_path(dir: &Path) -> PathBuf {
    #[cfg(unix)]
    let name = "node";
    #[cfg(windows)]
    let name = "node.exe";
    dir.join(name)
}

fn staging_shim_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!(".node-staging-{pid}"))
}

/// Remove one known immediate shim entry, never a directory tree. A concurrent
/// replacement can make this fail or delete only that immediate file/symlink;
/// it can never redirect cleanup into unrelated contents.
fn remove_shim_entry(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if !metadata.file_type().is_dir() {
        let _ = fs::remove_file(path);
    }
}

fn publish_node_shim(
    nub_binary: &Path,
    nub_identity: &FileHandle,
    shim_dir: &Path,
    pid: u32,
    options: &ShimSetupOptions,
) -> Result<FileHandle> {
    let node_shim = node_shim_path(shim_dir);
    let staging = staging_shim_path(shim_dir, pid);

    #[cfg(unix)]
    {
        let _ = options;
        unix_fs::symlink(nub_binary, &staging)
            .with_context(|| format!("creating node shim symlink in {}", shim_dir.display()))?;
        let identity = validate_node_shim(&staging, nub_identity)?;
        fs::rename(&staging, &node_shim)
            .with_context(|| format!("publishing node shim into {}", shim_dir.display()))?;
        Ok(identity)
    }

    #[cfg(windows)]
    {
        // TEMP may be on a different volume from the running binary, so a
        // failed hardlink falls back to copying the captured source into staging.
        let identity = if !options.force_copy && fs::hard_link(nub_binary, &staging).is_ok() {
            validate_node_shim(&staging, nub_identity)?
        } else {
            copy_file_from_handle(nub_identity, &staging)
                .with_context(|| format!("copying node shim into {}", shim_dir.display()))?
        };
        fs::rename(&staging, &node_shim)
            .with_context(|| format!("publishing node shim into {}", shim_dir.display()))?;
        Ok(identity)
    }
}

#[cfg(unix)]
fn open_node_shim(path: &Path) -> Result<FileHandle> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading node shim metadata: {}", path.display()))?;
    if !metadata.file_type().is_symlink() {
        anyhow::bail!("node shim is not a symlink: {}", path.display());
    }
    FileHandle::from_path(path)
        .with_context(|| format!("opening node shim target: {}", path.display()))
}

#[cfg(unix)]
fn validate_node_shim(path: &Path, nub_identity: &FileHandle) -> Result<FileHandle> {
    let identity = open_node_shim(path)?;
    if !identity.eq(nub_identity) {
        anyhow::bail!("node shim does not target the current Nub binary");
    }
    Ok(identity)
}

#[cfg(windows)]
fn open_node_shim(path: &Path) -> Result<FileHandle> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading node shim metadata: {}", path.display()))?;
    if !metadata.file_type().is_file() || is_reparse_point(&metadata) {
        anyhow::bail!("node shim is not a regular non-reparse file");
    }
    FileHandle::from_path(path)
        .with_context(|| format!("opening node shim identity: {}", path.display()))
}

#[cfg(windows)]
fn validate_node_shim(path: &Path, nub_identity: &FileHandle) -> Result<FileHandle> {
    let identity = open_node_shim(path)?;
    if !identity.eq(nub_identity) {
        anyhow::bail!("node shim hardlink does not target the current Nub binary");
    }
    Ok(identity)
}

fn validate_shim_record(record: &ShimRecord) -> Result<()> {
    let dir_identity = validate_shim_dir(&record.dir, true)?;
    if !dir_identity.eq(&record.dir_identity) {
        anyhow::bail!("PATH shim directory identity changed");
    }
    let node_identity = open_node_shim(&node_shim_path(&record.dir))?;
    if !node_identity.eq(&record.node_identity) {
        anyhow::bail!("node shim identity changed");
    }
    Ok(())
}

fn validate_shim_candidate(
    dir: &Path,
    expected_dir: &FileHandle,
    published: &FileHandle,
) -> Result<()> {
    let dir_identity = validate_shim_dir(dir, true)?;
    if !dir_identity.eq(expected_dir) {
        anyhow::bail!("PATH shim directory identity changed during creation");
    }
    let node_identity = open_node_shim(&node_shim_path(dir))?;
    if !node_identity.eq(published) {
        anyhow::bail!("node shim identity changed during creation");
    }
    Ok(())
}

#[cfg(windows)]
fn copy_file_from_handle(source: &FileHandle, destination: &Path) -> Result<FileHandle> {
    use std::io::Write;
    use std::os::windows::fs::FileExt;

    let mut destination_file = fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(destination)?;
    let mut offset = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = source.as_file().seek_read(&mut buffer, offset)?;
        if read == 0 {
            break;
        }
        destination_file.write_all(&buffer[..read])?;
        offset += read as u64;
    }
    destination_file.flush()?;
    drop(destination_file);
    FileHandle::from_path(destination).map_err(Into::into)
}

fn shim_dir_utf8(path: PathBuf) -> Result<Utf8PathBuf> {
    Utf8PathBuf::try_from(path).map_err(|error| anyhow::anyhow!("shim dir path not UTF-8: {error}"))
}

/// Compute the augmentation environment variables (NODE_OPTIONS + PATH)
/// that script runners need to set on child shells so that `node` invocations
/// inside scripts get nub's transpilation, polyfills, and flag injection — the
/// same augmentation `nub <file>` applies via direct args. (Web Storage is
/// opt-in and never injected here; see `spawn_node`.)
///
/// Returns `None` if already re-entrant (parent nub already set up augmentation)
/// or if compat mode is active.
///
/// The active PATH shim temp dir is process-wide and reclaimed exactly once on
/// process exit via [`cleanup_shim`]. A record that fails validation is retired
/// without pathname-based cleanup and replaced; a later dead-process reaper
/// handles its old directory. The active shim is
/// deliberately NOT returned as a per-call RAII guard, because concurrent
/// workspace scripts share the one dir and a per-call drop would `rm -rf` it
/// out from under sibling scripts still running.
pub fn compute_augmentation_env(
    nub_binary: &Path,
    node_path: &Path,
    node_version: super::version::NodeVersion,
    compat_mode: bool,
    pnp: Option<&Path>,
) -> Option<AugmentationEnv> {
    if compat_mode {
        return None;
    }

    // Bail if a parent nub already augmented this process tree, detected by OUR
    // specific preload path in NODE_OPTIONS (not a "preload.mjs" substring, which
    // would false-positive on a user's unrelated preload.mjs — A26).
    let preload = find_preload(nub_binary);
    // Key re-entrancy on the tier-specific injection token nub actually emits
    // (`--require=<cjs>` fast / `--import=<url>` compat), not a bare path (see
    // spawn_node + preload_injection).
    let injection = preload
        .as_deref()
        .map(|p| preload_injection(p, &node_version));
    let reentrancy_key = injection.as_ref().map(|i| i.node_options_token());
    // Read the inherited NODE_OPTIONS once and reuse it for both the re-entrancy
    // check and the NODE_OPTIONS rebuild below — the env value is constant here.
    let node_options = env::var("NODE_OPTIONS").ok();
    if is_reentrant_in(node_options.as_deref(), reentrancy_key.as_deref()) {
        return None;
    }
    // Nothing to inject if we can't locate our preload (broken install, or a
    // Windows temp PATH-shim that can't walk back to runtime/ — A-WIN2): pass
    // through so the child inherits the parent's already-augmented env.
    let preload = preload?;
    let injection = injection.expect("injection is Some when preload is Some");

    let existing_node_options = node_options.filter(|s| !s.is_empty());

    // Build NODE_OPTIONS. Unlike the direct-spawn path (which passes flags as
    // argv to `node`), scripts run under a shell, so EVERY flag must travel via
    // NODE_OPTIONS — injected experimental flags, the preload, and webstorage.
    // Dedupe injected flags against any existing NODE_OPTIONS so we don't emit a
    // flag the user already set.
    // Intersected with the binary's actual accepted-flag set (probed + cached),
    // same self-correcting guard as the direct-spawn path: a flag a future Node has
    // removed is dropped instead of aborting the script-runner child at startup.
    let accepted = super::discovery::accepted_env_flags(node_path);
    let inject = flags::compute_inject_flags(
        node_version.clone(),
        &[],
        existing_node_options.as_deref(),
        false,
        accepted.as_ref(),
    );
    let mut node_opts_parts: Vec<String> = inject.iter().map(|f| f.to_string()).collect();
    // Yarn PnP `--require <.pnp.cjs>` BEFORE nub's preload token so PnP's
    // resolver installs first in script-runner child shells too. Quoted: a
    // `.pnp.cjs` under a spacey project path would otherwise fragment.
    if let Some(pnp) = pnp {
        node_opts_parts.push(format!(
            "--require={}",
            node_options_quote(&pnp.display().to_string())
        ));
    }
    node_opts_parts.push(injection.node_options_token());
    // Web Storage (mirrors `spawn_node`): always inject
    // `--experimental-webstorage` on the flag-needed band (22.4–24.x) so a
    // script-run child shell's `node` has `sessionStorage` out of the box, with no
    // `--localstorage-file` opt-in required. nub never synthesizes
    // `--localstorage-file`. (Scripts have no argv here — the only user channel is
    // NODE_OPTIONS.) Guarded against double-add / a user
    // `--no-experimental-webstorage` disable.
    if should_inject_webstorage_flag(&node_version, &[], existing_node_options.as_deref()) {
        node_opts_parts.push("--experimental-webstorage".to_string());
    }
    // localStorage-neutralize decision: compute BEFORE `existing_node_options` is
    // consumed below. Scripts have no argv here — the only user channel is
    // NODE_OPTIONS. Neutralize when nub injects the flag (flag-needed band, no user
    // `--no-experimental-webstorage`) AND the user hasn't opted into persistence via
    // `--localstorage-file`.
    let neutralize_localstorage =
        should_neutralize_localstorage(&node_version, &[], existing_node_options.as_deref());
    if let Some(existing) = existing_node_options {
        // Snip below-floor version-gated flags out of the inherited NODE_OPTIONS
        // before appending (mirror of the direct-spawn site above) — a gated flag
        // the child Node can't parse otherwise aborts it with exit 9. See
        // flags::strip_unsupported_node_options.
        let stripped = flags::strip_unsupported_node_options(&existing, &node_version);
        if !stripped.is_empty() {
            node_opts_parts.push(stripped);
        }
    }

    let node_options = if node_opts_parts.is_empty() {
        None
    } else {
        Some(node_opts_parts.join(" "))
    };

    // The bare PATH-shim dir. Callers compose `shim_dir : node_modules/.bin :
    // existing PATH` — the shim first so child `node` hits nub-as-node, then the
    // walked-up `.bin` dirs BEFORE the system PATH so a locally-installed tool
    // shadows a global one (npm/pnpm parity; bundling `existing` into the shim
    // here used to push `.bin` after the system PATH, the A9-adjacent shadowing
    // bug). `existing` appears exactly once, supplied by `bin_path`.
    let shim_dir = setup_path_shim(nub_binary)
        .ok()
        .map(|d| d.as_str().to_string());

    Some(AugmentationEnv {
        node_options,
        shim_dir,
        node_path: vendored_node_path(Some(&preload)),
        neutralize_localstorage,
    })
}

/// Augmentation environment for script runners.
pub struct AugmentationEnv {
    pub node_options: Option<String>,
    /// The bare PATH-shim dir (NOT bundled with the system PATH). Callers prepend
    /// it ahead of `node_modules/.bin` + the system PATH.
    pub shim_dir: Option<String>,
    /// NODE_PATH so CJS `require()` of the transpile's vendored helper deps
    /// resolves from an installed package (A30). `None` in dev / when absent.
    pub node_path: Option<std::ffi::OsString>,
    /// Whether to set the internal `__NUB_NEUTRALIZE_LOCALSTORAGE` env var on the
    /// child so nub's preload replaces the throwing `localStorage` getter with
    /// `undefined` (the flag-needed band, no user `--localstorage-file`). Consumers
    /// apply it via [`AugmentationEnv::apply_localstorage_env`]. See
    /// `should_neutralize_localstorage`.
    pub neutralize_localstorage: bool,
}

impl AugmentationEnv {
    /// The PATH-shim's `node` entry (a symlink/hardlink → nub), suitable as the
    /// `$NODE` value that npm/pnpm set so userland `$NODE child.js` (and
    /// `spawn(process.env.NODE, …)`) invoke "the same Node this script runs under."
    /// Pointing `$NODE` here — rather than the raw binary — makes an absolute-path
    /// `$NODE` re-enter nub and stay augmented, identical to a bare `node` (which
    /// reaches the shim via PATH). The shim is a faithful node front-end
    /// (`$NODE --version` prints Node's version; `process.execPath` still reports the
    /// real binary), so introspection is preserved. `None` when no shim was set up
    /// (then callers fall back to the real binary for plain npm/pnpm parity).
    /// Apply the localStorage-neutralize signal to a child command's environment
    /// when this augmentation calls for it (sets the internal
    /// `__NUB_NEUTRALIZE_LOCALSTORAGE` env var the preload reads, then deletes). A
    /// no-op when `neutralize_localstorage` is false, so consumers can call it
    /// unconditionally. Factored here so the internal var name lives in exactly one
    /// place. Generic over `std::process::Command` / `tokio::process::Command` via
    /// the minimal `env`-setting shape they share.
    pub fn apply_localstorage_env(&self, set_env: impl FnOnce(&str, &str)) {
        if self.neutralize_localstorage {
            set_env(NEUTRALIZE_LOCALSTORAGE_ENV, "1");
        }
    }

    pub fn node_shim_exe(&self) -> Option<std::ffi::OsString> {
        self.shim_dir.as_deref().map(|dir| {
            #[cfg(windows)]
            let name = "node.exe";
            #[cfg(not(windows))]
            let name = "node";
            Path::new(dir).join(name).into_os_string()
        })
    }
}

/// Node's permission-model flags — the *exact, closed* set that, when present,
/// engages Node's `--permission` sandbox (and therefore needs `--allow-addons`
/// for nub's native oxc-transform addon to dlopen). This MUST be an exact
/// allowlist, not a `starts_with("--allow-")` prefix match: V8 exposes flags that
/// share the `--allow-` prefix but are NOT permission flags — most notably
/// `--allow-natives-syntax` (enables `%`-prefixed V8 natives like
/// `%OptimizeFunctionOnNextCall`). The old prefix match misclassified it as a
/// permission flag and aborted `nub --allow-natives-syntax x.js` with
/// "--permission requires --allow-addons", where stock node runs it (exit 0).
/// `--allow-ffi` is a real Node permission flag on the versions that carry it
/// (it is gone on node 25, where node itself rejects it as a bad option) and is
/// deliberately kept here so it classifies correctly wherever it exists. Match
/// the token up to any `=`, since the value-taking flags appear as
/// `--allow-fs-read=/path`, `--allow-net=host`, etc.
fn is_permission_flag(arg: &str) -> bool {
    const PERMISSION_FLAGS: &[&str] = &[
        "--permission",
        "--allow-addons",
        "--allow-child-process",
        "--allow-ffi",
        "--allow-fs-read",
        "--allow-fs-write",
        "--allow-inspector",
        "--allow-net",
        "--allow-wasi",
        "--allow-worker",
    ];
    let token = arg.split('=').next().unwrap_or(arg);
    PERMISSION_FLAGS.contains(&token)
}

/// Whether the user already supplied the `--experimental-webstorage` flag in
/// either polarity (`--experimental-webstorage` or `--no-experimental-webstorage`)
/// via argv or NODE_OPTIONS. When true, nub must NOT add its own
/// `--experimental-webstorage`: a duplicate positive is redundant, and overriding a
/// user's explicit `--no-experimental-webstorage` would defeat their disable
/// (and nub never re-enables over a user negation). Pure over its inputs.
fn user_has_webstorage_flag(user_args: &[String], node_options: Option<&str>) -> bool {
    let is_ws = |t: &str| t == "--experimental-webstorage" || t == "--no-experimental-webstorage";
    let in_argv = user_args.iter().any(|a| is_ws(a));
    let in_opts = node_options
        .map(|o| o.split_whitespace().any(is_ws))
        .unwrap_or(false);
    in_argv || in_opts
}

/// Whether nub should inject `--experimental-webstorage` for this invocation
/// (the maintainer, 2026-06-15: "a flag that we inject no matter what"). True iff the Node
/// version is on the flag-needed band (22.4 through <25, where the flag both EXISTS
/// and is still REQUIRED) AND the user hasn't already supplied the flag in either
/// polarity. The inject is UNCONDITIONAL on the band — it does not depend on any
/// `--localstorage-file` opt-in — so `sessionStorage` works out of the box; it
/// installs the `localStorage` getter too (which throws on access until the user
/// supplies their own `--localstorage-file`; nub never synthesizes one). Below 22.4
/// the flag is a "bad option" startup crash; on 25+ Web Storage is native so the
/// flag is unnecessary. Pure over its inputs for testability.
fn should_inject_webstorage_flag(
    node_version: &super::version::NodeVersion,
    user_args: &[String],
    node_options: Option<&str>,
) -> bool {
    flags::webstorage_flag_needed(node_version)
        && !user_has_webstorage_flag(user_args, node_options)
}

/// Whether the user supplied a `--localstorage-file[=<path>]` (in either argv or
/// NODE_OPTIONS). When true, the user has explicitly opted into persistent
/// `localStorage`, so nub must NOT neutralize the global — it forwards the file
/// verbatim and `localStorage` works normally. Matches both the `=`-joined form
/// (`--localstorage-file=/p`) and the space-separated form (`--localstorage-file /p`),
/// which appears as a bare `--localstorage-file` token. Pure over its inputs.
fn user_has_localstorage_file(user_args: &[String], node_options: Option<&str>) -> bool {
    let is_lsf = |t: &str| t == "--localstorage-file" || t.starts_with("--localstorage-file=");
    let in_argv = user_args.iter().any(|a| is_lsf(a));
    let in_opts = node_options
        .map(|o| o.split_whitespace().any(is_lsf))
        .unwrap_or(false);
    in_argv || in_opts
}

/// Whether nub should NEUTRALIZE the `localStorage` global to read `undefined`
/// (matching Node 25+'s clean shape) for this invocation (the maintainer, 2026-06-15). True
/// iff nub is injecting `--experimental-webstorage` on the flag-needed band AND the
/// user did NOT supply their own `--localstorage-file`. On that band the injected
/// flag installs a `localStorage` getter that THROWS `ERR_INVALID_ARG_VALUE` on
/// access (even `typeof localStorage` throws) until a `--localstorage-file` is
/// supplied — so when the user hasn't opted into persistence, nub replaces that
/// throwing getter with a plain `undefined` value in its startup preload, leaving
/// `sessionStorage` (which needs only the flag) fully working and making
/// `typeof localStorage === "undefined"` feature-detection safe. When the user DOES
/// pass `--localstorage-file`, this is false — `localStorage` works normally. The
/// neutralization is signaled to the preload via the internal
/// `__NUB_NEUTRALIZE_LOCALSTORAGE` env var. Pure over its inputs for testability.
fn should_neutralize_localstorage(
    node_version: &super::version::NodeVersion,
    user_args: &[String],
    node_options: Option<&str>,
) -> bool {
    should_inject_webstorage_flag(node_version, user_args, node_options)
        && !user_has_localstorage_file(user_args, node_options)
}

/// Internal env var that tells nub's startup preload to neutralize the
/// `localStorage` global (replace the throwing getter with `undefined`). An
/// internal `__NUB_*` plumbing var, NOT a user knob — explicitly permitted by the
/// brand boundary. The preload deletes it after reading so it does not leak to
/// grandchild processes.
const NEUTRALIZE_LOCALSTORAGE_ENV: &str = "__NUB_NEUTRALIZE_LOCALSTORAGE";

/// Carries the running binary's version (`env!("CARGO_PKG_VERSION")`) to the
/// preload, which publishes it as `process.versions.nub` — the universal
/// `process.versions.<runtime>` self-identification marker (cf. `.bun`,
/// `.electron`) that lets tooling detect "running under nub". Set only inside the
/// augment block, coupled to preload injection: under `--node`/`NODE_COMPAT` no
/// preload runs and the var is unset, so the marker is correctly absent (plain
/// Node). An internal `__NUB_*` plumbing var, NOT a user knob — explicitly
/// permitted by the brand boundary. Unlike the localStorage signal it is NOT
/// deleted by the preload, so it inherits into augmented descendants (which run
/// the same preload via NODE_OPTIONS) and they advertise the marker too.
const VERSION_ENV: &str = "__NUB_VERSION";

/// Tells nub's fast-tier preload to register its module hooks via the ASYNC
/// loader-worker path (`module.register`) instead of the sync
/// `module.registerHooks`, even on a Node that supports the sync fast tier. Set
/// only when nub is about to host a FOREIGN async `module.register` loader
/// (tsx/ts-node) on a Node whose sync/async hook composition is broken
/// ([`node_hook_compose_broken`]). Inherited by the whole augmented subtree
/// (like [`VERSION_ENV`]) so every child in a tsx run composes async. An internal
/// `__NUB_*` plumbing var, NOT a user knob — explicitly permitted by the brand
/// boundary.
const FORCE_ASYNC_TIER_ENV: &str = "__NUB_FORCE_ASYNC_TIER";

/// Node versions where the async `module.register` loader's `resolveSync`/
/// `loadSync` are unimplemented stubs that throw `ERR_METHOD_NOT_IMPLEMENTED`.
/// nub's sync `module.registerHooks` fast tier forces resolution synchronous, so
/// composing it with a foreign async loader (tsx/ts-node) reaches that throwing
/// stub and crashes the run. Node implemented these methods in 24.11.1
/// (block-on-loader-worker), so the broken window is 22.15.0 ..= 24.11.0
/// inclusive; 24.11.1+/25.2+/26 are fine. Refs nodejs/node#59666. (A later 22.x
/// that backported the fix would be over-covered here — harmless, since the
/// async tier composes correctly on every version.)
fn node_hook_compose_broken(v: &super::version::NodeVersion) -> bool {
    use super::version::NodeVersion;
    *v >= NodeVersion::new(22, 15, 0) && *v <= NodeVersion::new(24, 11, 0)
}

/// Whether the child nub is about to launch hosts a FOREIGN async ESM loader — a
/// tsx/ts-node executable, or an explicit `--import`/`--loader`/
/// `--experimental-loader` that registers one. Scans ALL tokens (not just
/// argv[0]) so an env-prefixed/compound script (`NODE_ENV=prod tsx x`) is still
/// caught; a rare false positive (a literal `echo tsx`) only makes that one
/// process use nub's async tier, which is always correct — never a crash.
fn child_hosts_async_loader<'a>(tokens: impl IntoIterator<Item = &'a str>) -> bool {
    tokens.into_iter().any(|tok| {
        let flag = tok.split_once('=').map_or(tok, |(f, _)| f);
        if matches!(flag, "--import" | "--loader" | "--experimental-loader") {
            return true;
        }
        let base = tok.rsplit(['/', '\\']).next().unwrap_or(tok);
        // Strip the Windows launcher-shim suffixes npm generates (`tsx.cmd`,
        // `tsx.ps1`, a native `.exe`) so the basename compares as the tool name.
        let base = base
            .strip_suffix(".cmd")
            .or_else(|| base.strip_suffix(".exe"))
            .or_else(|| base.strip_suffix(".ps1"))
            .unwrap_or(base);
        matches!(base, "tsx" | "ts-node" | "ts-node-esm")
    })
}

/// The `(key, "1")` env pair that forces nub's async tier for a child that will
/// host a foreign async loader on a broken-compose Node — or `None` when the
/// fast tier is safe. Callers set it on the child only when nub is establishing
/// augmentation (not compat/`--node`, not re-entrant); a re-entrant descendant
/// inherits the already-set var through the environment.
pub fn force_async_tier_env<'a>(
    node_version: &super::version::NodeVersion,
    child_tokens: impl IntoIterator<Item = &'a str>,
) -> Option<(&'static str, &'static str)> {
    (node_hook_compose_broken(node_version) && child_hosts_async_loader(child_tokens))
        .then_some((FORCE_ASYNC_TIER_ENV, "1"))
}

/// Whether Node's test-runner coverage is active for this invocation — i.e. the
/// user passed `--experimental-test-coverage` directly in argv or via NODE_OPTIONS.
/// (`nub` has no separate coverage verb; coverage is engaged solely by that flag,
/// so detecting it in either channel is the complete trigger.)
fn coverage_active(user_args: &[String], node_options: Option<&str>) -> bool {
    let in_argv = user_args
        .iter()
        .any(|a| a == "--experimental-test-coverage");
    let in_opts = node_options
        .map(|o| {
            o.split_whitespace()
                .any(|t| t == "--experimental-test-coverage")
        })
        .unwrap_or(false);
    in_argv || in_opts
}

/// Whether V8 coverage is active for the compile-cache gate — the same
/// `--experimental-test-coverage` signal `coverage_active` keys on (argv +
/// NODE_OPTIONS), PLUS a non-empty `NODE_V8_COVERAGE` env. The extra env check is
/// what `coverage_active` (used only for the R9 exclude-glob, which is itself
/// keyed to the coverage *flag*) doesn't need but the cache gate does: a user can
/// engage coverage purely through `NODE_V8_COVERAGE=<dir>` with no flag, and a warm
/// compile cache corrupts that path's ranges just the same. A user-set
/// NODE_COMPILE_CACHE is intentionally NOT consulted here — see the call site for
/// why a coverage run overrides even an explicit cache dir.
fn coverage_active_for_cache(
    user_args: &[String],
    node_options: Option<&str>,
    node_v8_coverage: Option<&str>,
) -> bool {
    coverage_active(user_args, node_options) || node_v8_coverage.is_some_and(|v| !v.is_empty())
}

/// The `--test-coverage-exclude=<glob>` flag nub injects to keep its own preloaded
/// runtime modules out of the user's coverage report (R9), or `None` when coverage
/// isn't active or the runtime dir can't be resolved. The glob is keyed to the
/// ABSOLUTE directory holding the injected preload — the same dir `find_preload`
/// returns the preload from — so it can never accidentally match a user's own
/// `runtime/` directory the way a relative `**/runtime/**` would.
///
/// HONESTY CAVEAT: passing ANY `--test-coverage-exclude` perturbs Node's branch
/// baseline slightly — Node computes the total branch count over the set of files
/// it decides to report, so excluding files shifts the `all files` branch %
/// denominator a hair. This is a stock-Node quirk of `--test-coverage-exclude`,
/// NOT something nub introduces; a future reader comparing nub's aggregate to a
/// hand-computed one should not be surprised by a fractional branch-% difference.
fn coverage_exclude_glob(
    user_args: &[String],
    node_options: Option<&str>,
    preload: Option<&str>,
) -> Option<String> {
    if !coverage_active(user_args, node_options) {
        return None;
    }
    let runtime_dir = Path::new(preload?).parent()?;
    Some(format!(
        "--test-coverage-exclude={}/**",
        runtime_dir.display()
    ))
}

/// True when `node_options` already carries OUR specific preload path — i.e. a
/// parent nub set up augmentation for this process tree (a re-entrant invocation
/// reached through the PATH shim, whose `node` resolves back to nub). Matching
/// the full preload path, rather than a generic `"preload.mjs"` substring, means
/// a user's own `--import` of an unrelated file that happens to be named
/// `preload.mjs` is never mistaken for ours and cannot silently disable
/// augmentation (A26). Pure over its inputs so it is testable without touching
/// the process environment.
fn is_reentrant_in(node_options: Option<&str>, preload: Option<&str>) -> bool {
    match (node_options, preload) {
        (Some(opts), Some(preload)) => opts.contains(preload),
        _ => false,
    }
}

/// Strip Windows' verbatim / extended-length path prefixes (`\\?\` and
/// `\\?\UNC\`) that `fs::canonicalize` emits. Node's module loader and NODE_PATH
/// reject them. Returns a native Windows path (backslashes preserved — valid for
/// NODE_PATH and fs ops). Pure over `windows` so both branches test on any host.
fn strip_verbatim(path: &str, windows: bool) -> String {
    if windows {
        if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{rest}");
        }
        if let Some(rest) = path.strip_prefix(r"\\?\") {
            return rest.to_string();
        }
    }
    path.to_string()
}

/// Convert a filesystem path to a `file://` URL Node's loader accepts on every
/// platform. On Windows a path is `C:\a\b` (or a canonicalized `\\?\C:\a\b`); a
/// naive `format!("file://{path}")` yields `file://C:\a\b`, which Node's
/// `fileURLToPath` rejects (ERR_INVALID_FILE_URL_PATH — the drive is parsed as the
/// URL authority and backslashes are invalid). Emit `file:///C:/a/b` for drive
/// paths and `file://server/share/...` for UNC. On Unix the path is already an
/// absolute forward-slash path, so `file://` + path gives the correct
/// `file:///abs/...`. Pure over `windows` so both branches test on any host.
fn to_file_url(path: &str, windows: bool) -> String {
    if !windows {
        return format!("file://{path}");
    }
    let forward = strip_verbatim(path, true).replace('\\', "/");
    if forward.starts_with("//") {
        // UNC: //server/share/... -> file://server/share/...
        format!("file:{forward}")
    } else {
        // Drive: C:/a/b -> file:///C:/a/b
        format!("file:///{forward}")
    }
}

/// How nub injects its preload, chosen BY TIER. The fast tier (Node 22.15+) loads a
/// CommonJS preload via `--require`; the compat tier (18.19–22.14) loads the ESM
/// preload via `--import`. The channel choice is load-bearing: an `--import` ESM
/// preload forces eager ESM-loader init, which routes even a CJS entry through the
/// async ESM module-job and breaks Node's synchronous `Module.runMain` semantics
/// (top-level `executionAsyncId`, sync exception origin, `require.main.id`,
/// `module.parent`, missing-entry error code) — the R1 regression cluster. A
/// `--require` CJS preload keeps the sync entry path; on 22.15+ it can still
/// `module.registerHooks` + transpile TS. The compat tier has no reliable sync
/// surface (no `module.registerHooks`, `require(esm)` unreliable), so it keeps the
/// async `--import` path.
pub struct PreloadInjection {
    /// The flag introducing the preload: `--require` (fast) or `--import` (compat).
    pub flag: &'static str,
    /// The injected value: a raw path for `--require`, a `file://` URL for `--import`.
    pub value: String,
}

impl PreloadInjection {
    /// The single token form for NODE_OPTIONS (`--require=<v>` / `--import=<v>`),
    /// which doubles as the re-entrancy key: a child detects a parent-injected
    /// preload by finding this exact token in its inherited NODE_OPTIONS.
    ///
    /// The VALUE half is quoted with [`node_options_quote`] so a preload path
    /// containing a space (e.g. a cache or temp dir under `C:\Users\John Doe\…`,
    /// or a macOS `~/Library/Application Support/…`) survives Node's NODE_OPTIONS
    /// tokenizer, which splits on unquoted spaces. The re-entrancy detector
    /// ([`is_reentrant_in`]) compares against this same quoted form, so the key
    /// still round-trips.
    pub fn node_options_token(&self) -> String {
        format!("{}={}", self.flag, node_options_quote(&self.value))
    }
}

/// Quote a value for safe embedding in NODE_OPTIONS. Node's NODE_OPTIONS
/// tokenizer (`ParseNodeOptionsEnvVar`, .repos/node/src/node_options.cc:2214)
/// splits on spaces UNLESS the run is inside a double-quoted string, and treats
/// backslash as an escape ONLY inside such a string. So a value with a space
/// must be wrapped in `"…"`, and inside those quotes every `\` and `"` must be
/// backslash-escaped or the path corrupts (the load-bearing Windows case:
/// `C:\Users\John Doe\.cache` → without escaping, `\U`, `\J`, `\.` get eaten).
/// Single quotes do NOT work — Node has no single-quote handling, so they'd
/// become literal characters in the path (`ERR_INVALID_STATE` on the store).
///
/// Values WITHOUT a space are returned unchanged: they tokenize fine bare, and
/// not quoting them keeps NODE_OPTIONS readable and matches plain-Node argv.
/// Use this for EVERY value-bearing flag nub writes into NODE_OPTIONS
/// (`--test-coverage-exclude=`, the preload `--require=`/`--import=` token,
/// PnP `--require=`).
fn node_options_quote(value: &str) -> String {
    if value.contains(' ') {
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        value.to_string()
    }
}

/// Pick the preload injection for a Node version, given the located ESM preload
/// path (`runtime/preload.mjs`). On the fast tier the sibling `runtime/preload.cjs`
/// is injected via `--require` (raw path — `require` takes a path, not a URL); on
/// the compat tier the `.mjs` is injected via `--import` (file:// URL). Pure over
/// `windows` for testability.
fn preload_injection_for(
    preload_mjs: &str,
    version: &super::version::NodeVersion,
    windows: bool,
) -> PreloadInjection {
    if version.supports_augmentation() {
        // Sibling .cjs in the same runtime dir. `--require` resolves a plain path
        // (it does NOT accept a file:// URL), so inject the raw path; verbatim
        // prefixes were already stripped by find_preload.
        let cjs = preload_mjs
            .strip_suffix(".mjs")
            .map(|stem| format!("{stem}.cjs"))
            .unwrap_or_else(|| preload_mjs.to_string());
        PreloadInjection {
            flag: "--require",
            value: cjs,
        }
    } else {
        PreloadInjection {
            flag: "--import",
            value: to_file_url(preload_mjs, windows),
        }
    }
}

/// Public wrapper over [`preload_injection_for`] for the current platform.
pub fn preload_injection(
    preload_mjs: &str,
    version: &super::version::NodeVersion,
) -> PreloadInjection {
    preload_injection_for(preload_mjs, version, cfg!(windows))
}

/// NODE_PATH value that makes nub's vendored runtime deps resolvable to a
/// CommonJS `require()` from transpiled output (A30). The transpile emits bare
/// helper imports (e.g. `@oxc-project/runtime/helpers/decorate` for decorators);
/// the ESM-import form resolves via the resolve hook (VENDORED_PACKAGES), but a
/// CJS `require()` bypasses the hook and uses Node's native resolver, which only
/// finds them through NODE_PATH. Returns `<preload-dir>/node_modules` prepended
/// to any existing NODE_PATH — but only when that dir exists (an installed
/// package). In dev `runtime/` has no `node_modules`, so this is None and the
/// requires resolve by walking up to the repo's `node_modules`, unchanged.
fn vendored_node_path(preload: Option<&str>) -> Option<std::ffi::OsString> {
    let vendored = Path::new(preload?).parent()?.join("node_modules");
    if !vendored.is_dir() {
        return None;
    }
    let mut value = vendored.into_os_string();
    if let Some(existing) = env::var_os("NODE_PATH").filter(|s| !s.is_empty()) {
        value.push(crate::PATH_LIST_SEPARATOR);
        value.push(existing);
    }
    Some(value)
}

/// Find the preload entry script relative to the Nub binary.
///
/// In development: `<repo>/runtime/preload.mjs`
/// In distribution: `<nub-install-dir>/runtime/preload.mjs`
pub fn find_public_preload(nub_binary: &Path) -> Option<String> {
    find_preload(nub_binary)
}

fn find_preload(nub_binary: &Path) -> Option<String> {
    // Single-binary mode: the runtime is embedded in the binary. Extract it once
    // (memoized) to a versioned cache dir and point the preload there. This runs
    // synchronously, BEFORE preload injection / NODE_OPTIONS assembly / the child
    // spawn (find_preload is the first thing spawn touches), so extraction is
    // complete before any path is read. The returned `.mjs` path is the
    // byte-identical sibling of today's sidecar `preload.mjs`; only its directory
    // moved (sidecar → cache), so `preload_injection_for`'s `--require <stem>.cjs`,
    // `vendored_node_path`'s `<dir>/node_modules`, and the addon's
    // `./addons/nub-native.node` all resolve unchanged. A `None` here (no writable
    // cache dir anywhere) leaves nub un-augmented, exactly as a not-found sidecar
    // would — never falls through to the (sidecar-less) walk below.
    #[cfg(feature = "embed-runtime")]
    {
        let _ = nub_binary; // resolution is from the embedded blob, not the binary's dir
        super::runtime_cache::ensure_runtime().and_then(|dir| {
            let preload = dir.join("preload.mjs");
            // Defense-in-depth: never trust a dir that isn't actually backed by the
            // preload on disk (a self-heal that lost a race could hand back a dead dir);
            // a missing file degrades to un-augmented rather than bricking the child.
            preload
                .is_file()
                .then(|| preload.to_str().map(|s| strip_verbatim(s, cfg!(windows))))
                .flatten()
        })
    }

    // Dev / feature-off: locate runtime/preload.mjs (the in-repo, live-editable
    // sidecar) by two independent routes, so a dev binary augments identically to
    // CI no matter which target dir the build landed in.
    //
    // Route 1 — walk up from the binary's directory. Hits the sidecar when the
    // target dir sits under the repo (a plain `cargo build` into `<repo>/target`,
    // which is what CI does).
    //
    // Route 2 — the source tree that COMPILED this binary (`CARGO_MANIFEST_DIR` →
    // `<repo>/runtime`). A build routed to a target dir with no `runtime/` ancestor
    // — the shared cross-worktree dir (`~/.cache/nub/shared-target`) that
    // `scripts/rust-build.sh` uses on the fast path — has no sidecar to walk to, so
    // route 1 fails and the binary would otherwise run WHOLLY un-augmented. That
    // silent local/CI behavior split is exactly the gap that let a
    // lifecycle-augmentation hang survive the whole suite (#528); route 2 is
    // layout-agnostic and closes it.
    //
    // Both routes compile ONLY here: the shipped binary is built with
    // `embed-runtime` and resolves its preload from the embedded blob above, never
    // reaching this branch — so the compile-time source path can never leak into a
    // released nub.
    #[cfg(not(feature = "embed-runtime"))]
    {
        if let Some(mut dir) = nub_binary.parent().map(Path::to_path_buf) {
            for _ in 0..5 {
                let candidate = dir.join("runtime").join("preload.mjs");
                if candidate.is_file() {
                    // Strip the `\\?\` verbatim prefix `fs::canonicalize` adds on Windows so
                    // the path is usable in NODE_PATH and convertible to a valid file:// URL.
                    return candidate.to_str().map(|s| strip_verbatim(s, cfg!(windows)));
                }
                if !dir.pop() {
                    break;
                }
            }
        }
        // `canonicalize` doubles as the existence check and resolves the `../..`
        // (and any symlink) so NODE_OPTIONS / the file:// URL carry a clean absolute
        // path, matching route 1's output shape.
        let source_preload =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/preload.mjs");
        if let Ok(canon) = fs::canonicalize(&source_preload) {
            return canon.to_str().map(|s| strip_verbatim(s, cfg!(windows)));
        }
        tracing::warn!("preload not found relative to nub binary or source root");
        None
    }
}

/// Resolve the path to the currently running Nub binary (follows symlinks).
pub fn current_nub_binary() -> Result<PathBuf> {
    let exe = env::current_exe().context("could not determine path to nub binary")?;
    fs::canonicalize(&exe).or(Ok(exe))
}

/// Map a child's [`ExitStatus`] to a Unix-faithful process exit code: the normal
/// exit code when the child exited normally, or `128 + signal` when it was killed
/// by a signal (SIGTERM → 143, SIGINT → 130, SIGSEGV → 139) — matching what a
/// shell and plain `node` report. The previous `code().unwrap_or(1)` collapsed
/// every signal death to 1, discarding the signal. Non-Unix falls back to the
/// code or 1.
pub fn exit_code_from_status(status: &ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    1
}

/// Convert a Node [`SpawnResult`] to a process exit code (see
/// [`exit_code_from_status`]).
pub fn exit_code(result: &SpawnResult) -> i32 {
    exit_code_from_status(&result.status)
}

/// Clean up this process's active validated PATH shim directory.
pub fn cleanup_shim() {
    PATH_SHIM_MANAGER.cleanup();
}

/// Cap on directories examined in a single reaper sweep. A sweep is best-effort
/// and bounded so it can never spin on a pathologically large `TMPDIR`; any
/// leftover stale dirs are simply collected on a later run.
const REAP_SCAN_CAP: usize = 4096;
// A legacy directory can contain several abandoned concurrent-publication
// staging files. Bound that inner scan too; later invocations resume cleanup.
const REAP_LEGACY_ENTRY_CAP: usize = 256;

/// Reap randomized PATH shim directories leaked by dead processes, plus the
/// legacy PID-only directories created by older Nub versions.
///
/// HOT PATH: this is NOT called on the run/spawn/teardown critical path. It does
/// a directory scan + per-entry `stat`, which is exactly the synchronous cost the
/// latency-sensitive run path must not pay. Drive it ONLY off the thread via
/// [`spawn_stale_shim_reaper`], which detaches it so the run never waits on it.
fn reap_stale_shims() {
    reap_stale_shims_in(&env::temp_dir(), std::process::id(), pid_is_alive);
}

/// Core of [`reap_stale_shims`], parameterized over the temp dir, this process's
/// pid, and a pid-liveness probe so it is unit-testable without touching the
/// shared global temp dir or real process state.
fn reap_stale_shims_in(temp: &Path, self_pid: u32, is_alive: impl Fn(u32) -> bool) {
    let Ok(entries) = fs::read_dir(temp) else {
        return;
    };

    for entry in entries.take(REAP_SCAN_CAP).flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some((pid, kind)) = parse_shim_dir_name(name) else {
            continue;
        };
        if pid == self_pid || is_alive(pid) {
            continue;
        }
        let require_private = matches!(kind, ShimDirNameKind::Randomized);
        let Ok(dir_identity) = validate_shim_dir(&entry.path(), require_private) else {
            continue;
        };
        remove_shim_entry(&node_shim_path(&entry.path()));
        remove_shim_entry(&staging_shim_path(&entry.path(), pid));
        if matches!(kind, ShimDirNameKind::Legacy) {
            remove_legacy_staging_entries(&entry.path(), pid);
        }
        let _ = fs::remove_dir(entry.path());
        drop(dir_identity);
    }
}

fn remove_legacy_staging_entries(dir: &Path, pid: u32) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.take(REAP_LEGACY_ENTRY_CAP).flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if is_legacy_staging_name(name, pid) {
            remove_shim_entry(&entry.path());
        }
    }
}

fn is_legacy_staging_name(name: &str, pid: u32) -> bool {
    let prefix = format!(".node-staging-{pid}-");
    let Some(counter) = name.strip_prefix(&prefix) else {
        return false;
    };
    if counter.is_empty()
        || (counter.len() > 1 && counter.starts_with('0'))
        || !counter.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    counter.parse::<u64>().is_ok()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShimDirNameKind {
    Legacy,
    Randomized,
}

fn parse_shim_dir_name(name: &str) -> Option<(u32, ShimDirNameKind)> {
    let tail = name.strip_prefix(PATH_SHIM_PREFIX)?;
    let (pid, kind) = match tail.split_once('-') {
        Some((pid, nonce)) => {
            if nonce.len() != 32
                || !nonce
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return None;
            }
            (pid, ShimDirNameKind::Randomized)
        }
        None => (tail, ShimDirNameKind::Legacy),
    };
    if pid.is_empty() || pid.starts_with('0') || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let pid = pid.parse::<u32>().ok()?;
    (pid != 0).then_some((pid, kind))
}

/// Spawn [`reap_stale_shims`] on a DETACHED background thread so the sweep's
/// directory scan never adds latency to the run/spawn/teardown path. Fire and
/// forget: if the process exits before the sweep finishes, any not-yet-reaped
/// stale dirs are collected by a later run. Call once, early.
pub fn spawn_stale_shim_reaper() {
    let _ = std::thread::Builder::new()
        .name("nub-shim-reaper".into())
        .spawn(reap_stale_shims);
}

/// Is `pid` a currently-live process? Used by the shim reaper to avoid reaping a
/// concurrent run's live dir. Conservative on error: a probe that can't decide
/// reports ALIVE, so an ambiguous case is never reaped (leak-over-data-loss).
#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    // kill(pid, 0) performs the permission/existence check WITHOUT sending a
    // signal: 0 → alive; ESRCH → no such process (reapable); EPERM → process
    // exists but is owned by another user (alive — do not reap).
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        // Negative pid_t values address process groups or every permitted
        // process. Never reinterpret an out-of-range directory name that way.
        return true;
    };
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    !matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    )
}

#[cfg(windows)]
fn pid_is_alive(pid: u32) -> bool {
    // OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) succeeds for a live process;
    // a dead pid yields a null handle (reapable). Anything else (e.g. access
    // denied on a live process) is treated as alive — conservative, never reap.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut core::ffi::c_void;
        fn CloseHandle(h: *mut core::ffi::c_void) -> i32;
        fn GetLastError() -> u32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const ERROR_INVALID_PARAMETER: u32 = 87;
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if !h.is_null() {
            CloseHandle(h);
            return true;
        }
        // A dead/never-existed pid fails with ERROR_INVALID_PARAMETER → reapable.
        // Any other failure (e.g. access denied) → treat as alive, don't reap.
        GetLastError() != ERROR_INVALID_PARAMETER
    }
}

/// The nub-owned default compile-cache dir (`<cache>/nub/v8-compile-cache`),
/// created best-effort. `None` when the cache root can't be resolved (no HOME) —
/// the spawn simply proceeds uncached, never errors.
pub fn default_compile_cache_dir() -> Option<std::ffi::OsString> {
    let dir = crate::node::discovery::cache_dir()?.join("v8-compile-cache");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.into_os_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::version::NodeVersion;

    #[test]
    fn node_options_quote_only_wraps_spacey_values() {
        // No space → returned bare (tokenizes fine, stays readable / argv-like).
        assert_eq!(node_options_quote("/tmp/store.sqlite"), "/tmp/store.sqlite");
        // Space → wrapped in double quotes (single quotes are literal to Node's
        // tokenizer and would corrupt the path → ERR_INVALID_STATE).
        assert_eq!(
            node_options_quote("/tmp/nub cache/store.sqlite"),
            "\"/tmp/nub cache/store.sqlite\""
        );
        // Windows: backslashes inside the quotes are escape chars to Node, so each
        // must be doubled or `\U`/`\J`/`\.` get eaten. Only quoted when spacey.
        assert_eq!(
            node_options_quote(r"C:\Users\John Doe\.cache\store.sqlite"),
            r#""C:\\Users\\John Doe\\.cache\\store.sqlite""#
        );
        // A backslash path WITHOUT a space stays bare — Node only treats `\` as an
        // escape INSIDE a quoted string, so an unquoted backslash is literal.
        assert_eq!(
            node_options_quote(r"C:\Users\John\.cache\store.sqlite"),
            r"C:\Users\John\.cache\store.sqlite"
        );
        // An embedded double-quote in a spacey value is backslash-escaped.
        assert_eq!(node_options_quote(r#"/tmp/a "b" c"#), r#""/tmp/a \"b\" c""#);
    }

    #[test]
    fn webstorage_flag_always_injected_on_band_without_localstorage_file() {
        // the maintainer, 2026-06-15: nub injects --experimental-webstorage "no matter what"
        // on the flag-needed band (22.4–24), with NO --localstorage-file present —
        // so sessionStorage works out of the box. (a) in-band with no file → inject.
        for ver in [
            NodeVersion::new(22, 4, 0),
            NodeVersion::new(22, 15, 0),
            NodeVersion::new(24, 0, 0),
            NodeVersion::new(24, 99, 0),
        ] {
            assert!(
                should_inject_webstorage_flag(&ver, &[], None),
                "must inject --experimental-webstorage on {ver:?} with no --localstorage-file"
            );
        }
    }

    #[test]
    fn webstorage_flag_not_injected_below_floor_or_when_native() {
        // (b) below 22.4 the flag is an unrecognized "bad option" → never inject.
        for ver in [NodeVersion::new(18, 19, 0), NodeVersion::new(22, 3, 0)] {
            assert!(
                !should_inject_webstorage_flag(&ver, &[], None),
                "must NOT inject below the 22.4 floor ({ver:?}) — would crash startup"
            );
        }
        // (c) on 25+ Web Storage is native → the flag is unnecessary, don't inject.
        for ver in [NodeVersion::new(25, 0, 0), NodeVersion::new(26, 2, 0)] {
            assert!(
                !should_inject_webstorage_flag(&ver, &[], None),
                "must NOT inject on {ver:?} — Web Storage is native there"
            );
        }
    }

    #[test]
    fn webstorage_flag_not_double_injected_when_user_supplied() {
        // (e) user already passed the flag (either polarity, either channel) → nub
        // must not double-inject / must respect an explicit disable.
        let s = |v: &str| v.to_string();
        let v = NodeVersion::new(22, 15, 0);
        assert!(!should_inject_webstorage_flag(
            &v,
            &[s("--experimental-webstorage")],
            None
        ));
        assert!(!should_inject_webstorage_flag(
            &v,
            &[],
            Some("--experimental-webstorage")
        ));
        assert!(!should_inject_webstorage_flag(
            &v,
            &[s("--no-experimental-webstorage")],
            None
        ));
        assert!(!should_inject_webstorage_flag(
            &v,
            &[],
            Some("--no-experimental-webstorage")
        ));
        // A --localstorage-file opt-in does NOT change the in-band decision — the
        // flag injects either way; (d) nub never synthesizes --localstorage-file, so
        // its presence/absence is irrelevant to whether the flag is injected.
        assert!(should_inject_webstorage_flag(
            &v,
            &[s("--localstorage-file=/tmp/x.sqlite")],
            None
        ));
    }

    #[test]
    fn existing_user_webstorage_flag_suppresses_injection() {
        let s = |v: &str| v.to_string();
        // Neither polarity present → nub may inject.
        assert!(!user_has_webstorage_flag(&[s("app.js")], None));
        // User already passed the positive → don't double-add.
        assert!(user_has_webstorage_flag(
            &[s("--experimental-webstorage")],
            None
        ));
        assert!(user_has_webstorage_flag(
            &[],
            Some("--experimental-webstorage")
        ));
        // User explicitly disabled → respect it, never re-enable.
        assert!(user_has_webstorage_flag(
            &[s("--no-experimental-webstorage")],
            None
        ));
        assert!(user_has_webstorage_flag(
            &[],
            Some("--no-experimental-webstorage --localstorage-file=/tmp/x")
        ));
    }

    #[test]
    fn user_localstorage_file_detected_in_either_channel() {
        let s = |v: &str| v.to_string();
        // Absent → not detected.
        assert!(!user_has_localstorage_file(&[s("app.js")], None));
        // `=`-joined form, argv.
        assert!(user_has_localstorage_file(
            &[s("--localstorage-file=/tmp/x.sqlite")],
            None
        ));
        // Space-separated form (bare token), argv.
        assert!(user_has_localstorage_file(
            &[s("--localstorage-file"), s("/tmp/x.sqlite")],
            None
        ));
        // Via NODE_OPTIONS.
        assert!(user_has_localstorage_file(
            &[],
            Some("--experimental-webstorage --localstorage-file=/tmp/x.sqlite")
        ));
        // A look-alike that is NOT the flag must not match.
        assert!(!user_has_localstorage_file(
            &[s("--localstorage-file-extra")],
            None
        ));
    }

    #[test]
    fn neutralize_localstorage_gate_set_iff_flag_injected_and_no_user_file() {
        let s = |v: &str| v.to_string();
        // (a) On the flag-needed band with NO user --localstorage-file → neutralize:
        // nub injects the flag, the user didn't opt into persistence, so the throwing
        // getter must be replaced with `undefined`.
        for ver in [
            NodeVersion::new(22, 4, 0),
            NodeVersion::new(22, 15, 0),
            NodeVersion::new(24, 99, 0),
        ] {
            assert!(
                should_neutralize_localstorage(&ver, &[], None),
                "must neutralize on {ver:?} with no --localstorage-file"
            );
        }

        // (b) User passed --localstorage-file (either channel/form) → do NOT
        // neutralize; localStorage works normally.
        let v = NodeVersion::new(22, 15, 0);
        assert!(!should_neutralize_localstorage(
            &v,
            &[s("--localstorage-file=/tmp/x.sqlite")],
            None
        ));
        assert!(!should_neutralize_localstorage(
            &v,
            &[s("--localstorage-file"), s("/tmp/x.sqlite")],
            None
        ));
        assert!(!should_neutralize_localstorage(
            &v,
            &[],
            Some("--localstorage-file=/tmp/x.sqlite")
        ));

        // (c) Off the flag-needed band (pre-22.4 / 25+ native) → no flag injected, so
        // never neutralize regardless of file.
        for ver in [
            NodeVersion::new(18, 19, 0),
            NodeVersion::new(22, 3, 0),
            NodeVersion::new(25, 0, 0),
            NodeVersion::new(26, 2, 0),
        ] {
            assert!(
                !should_neutralize_localstorage(&ver, &[], None),
                "must NOT neutralize off the flag-needed band ({ver:?})"
            );
        }

        // User-supplied/disabled --experimental-webstorage suppresses the inject, so
        // there is no nub-installed throwing getter to neutralize.
        assert!(!should_neutralize_localstorage(
            &v,
            &[s("--experimental-webstorage")],
            None
        ));
        assert!(!should_neutralize_localstorage(
            &v,
            &[s("--no-experimental-webstorage")],
            None
        ));
    }

    #[test]
    fn hook_compose_broken_band_is_22_15_through_24_11_0_inclusive() {
        // Broken (async-loader resolveSync stub throws): the whole 22.15.0–24.11.0 window.
        for v in [
            NodeVersion::new(22, 15, 0),
            NodeVersion::new(22, 16, 0),
            NodeVersion::new(23, 11, 0),
            NodeVersion::new(24, 11, 0),
        ] {
            assert!(
                node_hook_compose_broken(&v),
                "{v:?} must be in the broken band"
            );
        }
        // Fixed at 24.11.1 (Node implemented resolveSync/loadSync), and below the
        // fast-tier floor there is no sync registerHooks to force the crash.
        for v in [
            NodeVersion::new(22, 14, 99),
            NodeVersion::new(24, 11, 1),
            NodeVersion::new(24, 12, 0),
            NodeVersion::new(25, 2, 0),
            NodeVersion::new(26, 2, 0),
        ] {
            assert!(
                !node_hook_compose_broken(&v),
                "{v:?} must be outside the broken band"
            );
        }
    }

    #[test]
    fn child_hosts_async_loader_detects_tsx_ts_node_and_loader_flags() {
        let t = |toks: &[&str]| child_hosts_async_loader(toks.iter().copied());
        // Bare + path-prefixed + platform-suffixed tsx/ts-node launchers.
        assert!(t(&["tsx", "--conditions", "@zod/source"]));
        assert!(t(&["node_modules/.bin/tsx", "x.ts"]));
        assert!(t(&["ts-node", "x.ts"]));
        assert!(t(&["C:\\proj\\node_modules\\.bin\\tsx.cmd", "x.ts"]));
        assert!(t(&["C:\\proj\\node_modules\\.bin\\tsx.ps1", "x.ts"]));
        // Loader flags in either form register an async loader.
        assert!(t(&["node", "--import", "tsx/esm", "app.mjs"]));
        assert!(t(&["node", "--loader=ts-node/esm", "app.mjs"]));
        assert!(t(&["node", "--experimental-loader", "x.mjs"]));
        // No foreign loader → fast tier stays. A stray `tsx` SUBSTRING must not match.
        assert!(!t(&["webpack", "--mode", "production"]));
        assert!(!t(&["node", "server.js"]));
        assert!(!t(&["mytsx", "x"]));
        assert!(!t(&["tsx.ts"]));
    }

    #[test]
    fn force_async_tier_env_gated_on_band_and_loader() {
        let broken = NodeVersion::new(22, 16, 0);
        let fixed = NodeVersion::new(24, 12, 0);
        // Broken band + foreign loader → the signal.
        assert_eq!(
            force_async_tier_env(&broken, ["tsx", "x.ts"]),
            Some((FORCE_ASYNC_TIER_ENV, "1"))
        );
        // Broken band, no foreign loader → fast tier (None).
        assert_eq!(force_async_tier_env(&broken, ["node", "x.js"]), None);
        // Foreign loader but a fixed Node → no need to force (None).
        assert_eq!(force_async_tier_env(&fixed, ["tsx", "x.ts"]), None);
    }

    // `ctrl_c::CURRENT_CHILD` is a process-global AtomicU32. The two tests that
    // exercise it (`ctrl_c_forwards_*` and `diagnostic_signal_*`) therefore race
    // when cargo runs them on parallel threads — one test's `track(<real pid>)`
    // flips the global out from under the other's `current()` assertion (an
    // intermittent CI failure). Serialize them behind this guard. Poison-tolerant
    // so a panic in one doesn't cascade into a spurious failure in the other.
    static CTRL_C_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(unix)]
    #[test]
    fn exit_code_maps_signal_death_to_128_plus_signo() {
        // A child killed by a signal exits BY the signal (`.code()` is None,
        // `.signal()` is the signo), so `exit_code_from_status` must report
        // 128 + signo — SIGTERM => 143 — not collapse it to a generic 1.
        let killed = Command::new("sh")
            .arg("-c")
            .arg("kill -TERM $$")
            .status()
            .unwrap();
        assert_eq!(exit_code_from_status(&killed), 143);
        // A normal exit code passes through untouched.
        let normal = Command::new("sh").arg("-c").arg("exit 7").status().unwrap();
        assert_eq!(exit_code_from_status(&normal), 7);
    }

    #[cfg(unix)]
    #[test]
    fn ctrl_c_forwards_to_the_latest_child_not_the_first() {
        let _serial = CTRL_C_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // The bug A20 fixes: a second spawn's set_handler no-op'd, so the single
        // handler kept the first (dead) pid. Now the global pid updates per spawn,
        // so the handler always targets the current child; untrack clears it so a
        // stray SIGINT after exit is a no-op rather than a kill of a reused pid.
        ctrl_c::untrack(); // reset the shared global before asserting on it
        ctrl_c::track(111);
        assert_eq!(ctrl_c::current(), 111);
        ctrl_c::track(222);
        assert_eq!(
            ctrl_c::current(),
            222,
            "a later spawn must become the forwarded target"
        );
        ctrl_c::untrack();
        assert_eq!(
            ctrl_c::current(),
            0,
            "untrack clears the pid after the child exits"
        );
    }

    #[cfg(unix)]
    #[test]
    fn status_forwarding_signals_runs_then_clears_the_tracked_pid() {
        // The `nub run` script path routes through this instead of a raw
        // `command.status()` so docker stop / Ctrl-C reach the script child. It
        // must return the child's real status AND leave the global untracked, so a
        // stray signal after the script exits can't kill a reused pid.
        let _serial = CTRL_C_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        ctrl_c::untrack();
        let status = status_forwarding_signals(Command::new("sh").arg("-c").arg("exit 7"))
            .expect("spawn sh");
        assert_eq!(
            exit_code_from_status(&status),
            7,
            "the child's code passes through"
        );
        assert_eq!(
            ctrl_c::current(),
            0,
            "the tracked pid is cleared once the child exits"
        );
    }

    #[cfg(unix)]
    #[test]
    fn group_targeting_stores_a_negative_pid() {
        // track_child_group must store `-pid` so the forwarder's kill(2) hits the
        // process GROUP (sh + the node it forks), not just sh — the orphan the
        // single-pid path left under a dash that forks its `sh -c` child.
        let _serial = CTRL_C_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        ctrl_c::untrack();
        track_child_group(4321);
        assert_eq!(ctrl_c::current(), -4321, "group target is the negated pid");
        untrack_child();
        assert_eq!(ctrl_c::current(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn sigint_reaches_an_own_group_child_exactly_once() {
        // Regression for issue #26: the file-run child fired `process.on('SIGINT')`
        // TWICE per interactive Ctrl-C. Root cause: the child shared nub's process
        // group on the controlling TTY, so a terminal Ctrl-C delivered SIGINT to the
        // whole foreground group (the child got it directly) AND nub's forwarder
        // re-sent it. The fix puts the child in its OWN process group (`setpgid` via
        // `group_on_spawn`), so the TTY signals only nub's group and the forwarder's
        // single group-targeted relay is the lone delivery — exactly like plain Node.
        //
        // This test reproduces that topology in-process: a child placed in its own
        // group (so the test runner's own group-SIGINT, if any, can't leak in), a
        // SIGINT trap that COUNTS deliveries, and a single forwarded SIGINT to the
        // child's group. The handler must run exactly once and the child must exit
        // 130 (128 + SIGINT) — the byte-for-byte-with-Node contract. (The true
        // interactive TTY Ctrl-C is not reproducible in CI without a pty; this pins
        // the own-group + single-forward invariant that makes it correct.)
        let _serial = CTRL_C_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        ctrl_c::untrack();

        let marker = env::temp_dir().join(format!(
            "nub-sigint-count-{}-{}.marker",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_file(&marker);

        // On SIGINT: append a line to the marker (so a double delivery would write
        // two), then exit 130. Until then, block on a backgrounded sleep so the trap
        // fires promptly. The child is its own group leader (`group_on_spawn`).
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!(
            "trap 'echo x >>{m}; exit 130' INT; sleep 5 & wait",
            m = marker.display()
        ));
        group_on_spawn(&mut cmd);
        let mut child = cmd.spawn().expect("spawn sigint-count child");

        // Forward to the child's GROUP — the single, sole delivery path now that the
        // child is in its own group (nothing else signals it).
        track_child_group(child.id());

        // Let the trap install before delivering.
        std::thread::sleep(std::time::Duration::from_millis(200));

        // One SIGINT to the child's process group — the forwarder relays exactly one.
        // SAFETY: kill(2) on the child's own group; benign if it already exited.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGINT);
        }

        let status = loop_wait(&mut child, std::time::Duration::from_secs(5));
        untrack_child();

        let deliveries = fs::read_to_string(&marker)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        let _ = fs::remove_file(&marker);

        assert_eq!(
            deliveries, 1,
            "SIGINT handler must fire EXACTLY once (issue #26 double-emit regression)"
        );
        assert_eq!(
            status.and_then(|s| s.code()),
            Some(130),
            "child must exit 130 (128 + SIGINT) via its own trap"
        );
    }

    #[cfg(unix)]
    #[test]
    fn diagnostic_signal_reaches_child_and_parent_survives() {
        let _serial = CTRL_C_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // SIGUSR2 is the diagnostic-signal exemplar (Node's --report-signal default,
        // and what nodemon sends). Default disposition is TERMINATE the receiver, so
        // without nub installing a handler this signal would kill the resident parent
        // before the child ever saw it. This proves the two-part contract in one shot:
        //   (1) the child RECEIVES a relayed SIGUSR2 (it writes a marker file), and
        //   (2) the parent (this test process) SURVIVES — it keeps running past the
        //       signal to observe the marker, rather than being terminated by USR2.
        // A representative diagnostic signal covers the relay+survival contract; we
        // don't repeat per-signal (USR1/QUIT register through the identical path).
        let marker = env::temp_dir().join(format!(
            "nub-usr2-relay-{}-{}.marker",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_file(&marker);

        // A child that, on SIGUSR2, writes the marker and exits 0; otherwise sleeps.
        // It blocks (`wait`) on a background sleep so the trap can fire promptly.
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "trap 'echo got >{m}; exit 0' USR2; sleep 5 & wait",
                m = marker.display()
            ))
            .spawn()
            .expect("spawn signal-trap child");

        // Register nub's forwarder for this child (installs the SIGUSR2 handler that
        // overrides the parent's terminate-on-USR2 default and relays to the child).
        ctrl_c::track(child.id() as i32);

        // Give the child's `trap` a moment to install before we deliver the signal.
        std::thread::sleep(std::time::Duration::from_millis(150));

        // Send SIGUSR2 to OURSELVES. If nub hadn't installed a handler, this line
        // would terminate the test binary (USR2's default action) and the test would
        // be recorded as a signal death — never reaching the assertions below.
        unsafe {
            libc::kill(std::process::id() as i32, libc::SIGUSR2);
        }

        // The relay is async (signal-hook self-pipe → forwarder thread → kill child),
        // so poll for the marker / child exit rather than racing it.
        let status = loop_wait(&mut child, std::time::Duration::from_secs(5));
        ctrl_c::untrack();

        let marker_written = marker.exists();
        let _ = fs::remove_file(&marker);

        assert!(
            marker_written,
            "child must have received the relayed SIGUSR2 and written its marker"
        );
        // The child exits 0 from its own trap — proving it ran ITS handler, not that
        // it was hard-killed.
        assert_eq!(
            status.and_then(|s| s.code()),
            Some(0),
            "child must exit 0 via its own SIGUSR2 trap"
        );
        // Reaching here at all is the parent-survival half: a process killed by USR2
        // never runs these assertions.
    }

    // ---- issue #27: terminal-foreground hand-off to an interactive child ----

    /// The PTY-scenario worker, run as its OWN fresh process (NOT a fork of the
    /// multithreaded test harness — that hazards a malloc-lock deadlock). The test
    /// re-execs the test binary with `NUB_PTY_SCENARIO=0|1` set; that re-exec lands
    /// in `interactive_tui_*`, which calls this immediately and exits with the
    /// verdict as its process exit code. Here we play the "nub" role on a brand-new
    /// PTY of our own: `setsid()` for a new session with no controlling terminal,
    /// open the PTY slave so it becomes this session's controlling terminal, then
    /// `dup2` the slave onto stdin so `STDIN_FILENO` is that terminal.
    ///
    /// Then spawn a `sh` grandchild in its OWN process group (exactly what
    /// `group_on_spawn` does) that BLOCKS reading the terminal — the TUI's raw-mode
    /// read. An own-group child is a BACKGROUND group, so that read raises SIGTTIN
    /// and the child STOPS (the unkillable #27 hang). When `apply_fix` is set we call
    /// [`foreground_child`] first, handing the terminal to the grandchild's group so
    /// the read succeeds. Returns the verdict exit code: 0 = child read OK (fix
    /// works), 1 = child SIGTTIN-stopped / hung (no fix), 2 = setup error.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn pty_scenario_worker(apply_fix: bool) -> i32 {
        use std::os::unix::io::RawFd;

        // Becoming a session leader with the PTY as controlling terminal means a
        // hangup on that terminal would SIGHUP us; ignore it so the worker always
        // runs to its verdict. Do NOT touch SIGTTIN/SIGTTOU here: SIG_IGN is
        // inherited across exec, and the grandchild MUST keep the default SIGTTIN
        // disposition for the background-read STOP to reproduce (`foreground_child`
        // already brackets its own tcsetpgrp with a local SIGTTOU ignore).
        // SAFETY: signal(2) with a constant disposition.
        unsafe {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
        }

        // SAFETY: openpty(3) gives a connected master/slave fd pair.
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return 2;
        }

        // New session → no controlling terminal, then claim the slave as ours.
        // SAFETY: setsid()/ioctl(TIOCSCTTY)/dup2 on the fresh session leader (us).
        unsafe {
            if libc::setsid() < 0 {
                return 2;
            }
            libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
            libc::dup2(slave, libc::STDIN_FILENO);
            if slave > 2 {
                libc::close(slave);
            }
        }

        // Spawn the "TUI" grandchild in its OWN process group (like group_on_spawn):
        // it BLOCKS reading the controlling terminal (stdin). As a background group
        // that read raises SIGTTIN → STOP, unless we hand it the foreground first.
        // stdout/stderr go to /dev/null so the worker's own captured output stays
        // clean; stdin is inherited (our pty slave) — that's the terminal under test.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("read x; exit 0");
        cmd.stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        group_on_spawn(&mut cmd);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => return 2,
        };
        let child_pid = child.id();

        // Apply the fix under test (or not).
        let _fg = if apply_fix {
            foreground_child(child_pid)
        } else {
            None
        };

        // Let the grandchild reach its blocking terminal read; if it's going to
        // SIGTTIN-stop (no fix), it will have stopped by now.
        std::thread::sleep(std::time::Duration::from_millis(300));

        // The "keystroke": a newline to the PTY master. With the fix the child is
        // foreground and reads it → exits 0; without it the child is stopped.
        // SAFETY: write to the live master fd.
        unsafe {
            let nl = b"\n";
            libc::write(master, nl.as_ptr() as *const libc::c_void, 1);
        }

        // Observe the child with WUNTRACED so a SIGTTIN STOP is reported, polling ~2s.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let verdict = loop {
            let mut status: libc::c_int = 0;
            // SAFETY: waitpid on our child, non-blocking + report-stops.
            let r = unsafe {
                libc::waitpid(
                    child_pid as libc::pid_t,
                    &mut status,
                    libc::WNOHANG | libc::WUNTRACED,
                )
            };
            if r == child_pid as libc::pid_t {
                if libc::WIFSTOPPED(status) {
                    break 1; // stopped (SIGTTIN) → the hang
                }
                if libc::WIFEXITED(status) {
                    break 0; // read the keystroke and exited cleanly
                }
            }
            if std::time::Instant::now() >= deadline {
                break 1; // never exited → stuck
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };

        // Clean up the (possibly stopped) grandchild so we never leak it.
        // SAFETY: SIGKILL + reap; benign if already gone.
        unsafe {
            libc::kill(child_pid as libc::pid_t, libc::SIGKILL);
            libc::close(master);
        }
        let _ = child.wait();
        verdict
    }

    /// PTY worker proving issue #26's single-SIGINT guarantee STILL HOLDS once the
    /// #27 foreground hand-off is active — the case `sigint_reaches_an_own_group_child_exactly_once`
    /// cannot reach (that one forwards a `kill` directly, never exercising the real
    /// TTY-Ctrl-C routing + the handoff's SIGINT-forward suppression together).
    ///
    /// Reproduces nub's FULL interactive topology on a real controlling terminal:
    /// own-group child (`group_on_spawn`) running a SIGINT trap that COUNTS
    /// deliveries; nub's signal forwarder registered (`track_child_group` — the path
    /// that double-delivered in #26); AND `foreground_child` applied (tcsetpgrp +
    /// SIGINT-forward suppress). Then a literal `^C` (0x03) is written to the PTY
    /// master: the line discipline (ISIG on by default) turns it into a SIGINT for
    /// the terminal's FOREGROUND group — which the handoff made the child's group.
    ///
    /// The child must see SIGINT EXACTLY ONCE: the kernel delivers it to the
    /// foreground child directly, nub is now a background group (so the TTY does not
    /// signal nub), and even if nub's forwarder did fire, the suppress drops the
    /// redundant forward. A double here would mean the handoff regressed #26.
    /// Returns: 0 = exactly-once (correct), 3 = zero deliveries (lost SIGINT),
    /// 4 = two-or-more (the #26 double under the handoff), 2 = setup error.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn pty_sigint_count_worker() -> i32 {
        use std::os::unix::io::RawFd;

        // SAFETY: ignore SIGHUP so a terminal hangup never kills the worker mid-verdict.
        unsafe {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
        }

        // SAFETY: openpty(3) gives a connected master/slave fd pair.
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return 2;
        }

        // New session, claim the slave as our controlling terminal, put it on stdin.
        // SAFETY: setsid()/ioctl(TIOCSCTTY)/dup2 on the fresh session leader (us).
        unsafe {
            if libc::setsid() < 0 {
                return 2;
            }
            libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
            libc::dup2(slave, libc::STDIN_FILENO);
            if slave > 2 {
                libc::close(slave);
            }
        }

        // The child counts EVERY SIGINT delivery into a marker file. Crucially the
        // trap does NOT exit on the first one — it keeps reading — so a SECOND
        // (doubled) delivery is still counted instead of being masked by an
        // exit-on-first. A small grace sleep after the first hit absorbs any second
        // delivery, then the child exits 130 (the SIGINT exit code). Own group via
        // `group_on_spawn`, exactly like the real path.
        let marker = std::env::temp_dir().join(format!(
            "nub-pty-sigint-count-{}.marker",
            std::process::id()
        ));
        // Readiness handshake: the child writes `ready` AFTER installing the INT trap
        // and BEFORE its `read`. The driver waits for it (below) before sending the
        // ETX, so the SIGINT can never land before the trap is in place — closing the
        // verdict-3 race where a fixed grace sleep was too short on a loaded runner and
        // the default disposition killed the child before it could record the delivery.
        let ready = std::env::temp_dir().join(format!(
            "nub-pty-sigint-ready-{}.marker",
            std::process::id()
        ));
        let _ = fs::remove_file(&marker);
        let _ = fs::remove_file(&ready);
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!(
            "trap 'echo x >>{m}' INT; echo r >{r}; read ignored; sleep 0.3; exit 130",
            m = marker.display(),
            r = ready.display()
        ));
        cmd.stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        group_on_spawn(&mut cmd);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => return 2,
        };
        let child_pid = child.id();

        // The full nub topology: register the forwarder (the #26 double-delivery
        // path) AND hand the terminal foreground to the child (tcsetpgrp + suppress).
        ctrl_c::untrack();
        track_child_group(child_pid);
        let _fg = foreground_child(child_pid);

        // Wait for the child to signal its INT trap is installed (it writes `ready`
        // AFTER `trap ... INT`). Bounded poll — no fixed-duration timing assumption, so
        // a loaded runner can't deliver the ETX before the trap is in place (the original
        // flake: a too-short fixed grace let the SIGINT land pre-trap and the default
        // disposition killed the child, recording nothing). Once the trap is installed
        // the SIGINT is caught whether or not the child has yet reached its `read` (ISIG
        // raises it for the foreground group regardless).
        let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ready.exists() {
            if std::time::Instant::now() >= ready_deadline {
                // SAFETY: reap the child we spawned so the bail-out never leaks it.
                unsafe {
                    libc::kill(child_pid as libc::pid_t, libc::SIGKILL);
                    libc::close(master);
                }
                let _ = child.wait();
                let _ = fs::remove_file(&marker);
                let _ = fs::remove_file(&ready);
                return 2; // setup error: child never signalled trap-installed
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // The "Ctrl-C": ETX on the master. With ISIG (default) the line discipline raises
        // SIGINT for the terminal's foreground group = the child's group.
        // SAFETY: write to the live master fd.
        unsafe {
            let etx = b"\x03";
            libc::write(master, etx.as_ptr() as *const libc::c_void, 1);
        }

        // Gate the verdict on the child's ACTUAL recorded receipt (the line its INT trap
        // appends to `marker`), polled with a generous deadline that tolerates scheduling
        // latency on a loaded runner — NOT a fixed-duration window (the verdict-3 flake
        // fix). The child does not exit on its own: after the trap fires its `read`
        // restarts (a trapped signal does not make `read` return on dash/sh) and it
        // blocks until the SIGKILL below, so we cannot wait on its exit. A delivered SIGINT
        // is recorded in milliseconds; the full window only elapses when the synthetic ETX
        // failed to raise a signal at all (see the verdict-3 note below).
        let marker_lines = |p: &Path| {
            fs::read_to_string(p)
                .map(|s| s.lines().count())
                .unwrap_or(0)
        };
        let recv_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while marker_lines(&marker) == 0 && std::time::Instant::now() < recv_deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Once the first receipt lands, a short bounded settle surfaces a #26 DOUBLE: the
        // kernel's TTY delivery and nub's redundant forward fire off the SAME ETX,
        // microseconds apart, so 500ms is ample to catch a second receipt before we read
        // the authoritative count. No per-run cost when there is no double to see.
        if marker_lines(&marker) >= 1 {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }

        // Clean up before reading the verdict so we never leak the child.
        // SAFETY: SIGKILL + reap; benign if already gone.
        unsafe {
            libc::kill(child_pid as libc::pid_t, libc::SIGKILL);
            libc::close(master);
        }
        let _ = child.wait();
        drop(_fg);

        let deliveries = marker_lines(&marker);
        let _ = fs::remove_file(&marker);
        let _ = fs::remove_file(&ready);

        match deliveries {
            1 => 0, // exactly once — #26 preserved under the handoff
            // Empty marker: the synthetic ETX did not raise a SIGINT at all. In THIS
            // topology that can ONLY be a pty-stimulus miss, never a nub fault: the
            // kernel delivers a terminal Ctrl-C to the foreground child independently of
            // nub (nub's suppression gates only its own *forward*, it cannot eat the TTY
            // delivery), and the diagnostics confirm the child stays the live foreground
            // group across the whole window. The miss is persistent for a given pty
            // instance (resending the ETX does not help), so the DRIVER re-runs a FRESH
            // worker on this verdict — see the retry loop there.
            0 => 3,
            _ => 4, // double (or more) — the #26 regression
        }
    }

    /// Re-exec the test binary to run [`pty_scenario_worker`] as a fresh process and
    /// return its verdict exit code (0 read-OK / 1 hung / 2 error / other = crash).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn run_pty_scenario(apply_fix: bool) -> i32 {
        let exe = std::env::current_exe().expect("current_exe");
        let status = Command::new(exe)
            .args([
                "--exact",
                "node::spawn::tests::interactive_tui_child_can_read_terminal_only_with_foreground_handoff",
            ])
            .env("NUB_PTY_SCENARIO", if apply_fix { "1" } else { "0" })
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("re-exec test binary for PTY scenario");
        status.code().unwrap_or(-1)
    }

    /// Issue #27 PTY repro. Normally (no `NUB_PTY_SCENARIO`) this is the DRIVER: it
    /// re-execs itself twice — once without the fix (the child must SIGTTIN-hang) and
    /// once with it (the child must read the terminal and exit). When the re-exec
    /// sets `NUB_PTY_SCENARIO`, this same `#[test]` instead BECOMES the worker and
    /// exits with the scenario verdict as its process code (so the worker runs as a
    /// fresh, non-forked process — avoiding fork-in-a-multithreaded-harness).
    ///
    /// The worker re-exec uses `--exact <this test>` so only this test runs in the
    /// child process; the driver runs in the normal suite.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn interactive_tui_child_can_read_terminal_only_with_foreground_handoff() {
        // Worker mode: we were re-exec'd to BE the scenario. Run it and exit with the
        // verdict code so the driver can read it (no normal test-harness teardown).
        if let Ok(v) = std::env::var("NUB_PTY_SCENARIO") {
            let code = pty_scenario_worker(v == "1");
            std::process::exit(code);
        }

        // Driver mode: spawn the worker twice as fresh processes.
        let _serial = CTRL_C_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());

        let without = run_pty_scenario(false);
        assert_eq!(
            without, 1,
            "without the foreground hand-off the own-group TUI child must SIGTTIN-stop \
             (the #27 hang); worker verdict code {without} (0=read-ok 1=hung 2=setup-err)"
        );

        let with = run_pty_scenario(true);
        assert_eq!(
            with, 0,
            "with the tcsetpgrp foreground hand-off the TUI child must read the \
             terminal and exit cleanly (no #27 hang); worker verdict code {with} \
             (0=read-ok 1=hung 2=setup-err)"
        );
    }

    /// Issue #26 single-SIGINT, proven UNDER the #27 foreground hand-off (the gap the
    /// other #26 test can't cover — it forwards a `kill` directly rather than going
    /// through the real TTY-Ctrl-C routing + the handoff's forward-suppression).
    /// Same driver/worker re-exec shape as the #27 pty test: on `NUB_PTY_SIGINT_COUNT`
    /// this becomes the worker and exits with the verdict; otherwise it drives.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn ctrl_c_under_foreground_handoff_delivers_sigint_to_child_exactly_once() {
        // Worker mode: run the scenario and exit with its verdict as the process code.
        if std::env::var("NUB_PTY_SIGINT_COUNT").is_ok() {
            std::process::exit(pty_sigint_count_worker());
        }

        // Driver mode: re-exec ourselves as a fresh (non-forked) worker process.
        let _serial = CTRL_C_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let exe = std::env::current_exe().expect("current_exe");
        let run_worker = || -> i32 {
            Command::new(&exe)
                .args([
                    "--exact",
                    "node::spawn::tests::ctrl_c_under_foreground_handoff_delivers_sigint_to_child_exactly_once",
                ])
                .env("NUB_PTY_SIGINT_COUNT", "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("re-exec test binary for PTY sigint-count scenario")
                .code()
                .unwrap_or(-1)
        };

        // Verdict 3 = the synthetic ETX did not raise a SIGINT for that pty instance at
        // all (a pty-stimulus miss, NOT a nub fault — see `pty_sigint_count_worker`). The
        // miss is persistent for a given pty, so re-run a FRESH worker (fresh openpty) to
        // get a conclusive verdict. We retry ONLY on 3; a real #26 regression surfaces as
        // 4 (double) and must fail immediately, never be retried away.
        let mut code = run_worker();
        let mut tries = 1;
        while code == 3 && tries < 8 {
            code = run_worker();
            tries += 1;
        }

        assert_eq!(
            code, 0,
            "under the #27 foreground hand-off a terminal Ctrl-C must deliver SIGINT to \
             the child EXACTLY ONCE (issue #26); final worker verdict {code} after {tries} \
             attempt(s) (0=exactly-once 3=stimulus-miss 4=double 2=setup-err)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn foreground_child_is_noop_off_a_tty() {
        // The non-TTY / CI / piped path must be unchanged: with stdin NOT a TTY,
        // `foreground_child` returns None (no tcsetpgrp attempted) and does NOT
        // suppress SIGINT forwarding — so a `kill -INT <nub>` still relays to the
        // child exactly as before (issue #26's non-TTY contract).
        let _serial = CTRL_C_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        ctrl_c::set_suppress_sigint_forward(false);

        // The cargo test harness runs with stdin redirected (not a TTY), so a direct
        // call here exercises the off-TTY branch. If a developer somehow runs the
        // suite attached to a TTY, skip rather than perturb their terminal.
        // SAFETY: isatty on STDIN_FILENO.
        if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
            return;
        }
        let guard = foreground_child(std::process::id());
        assert!(
            guard.is_none(),
            "foreground_child must be a no-op when stdin is not a TTY"
        );
        assert!(
            !ctrl_c::sigint_forward_suppressed(),
            "off a TTY, SIGINT forwarding must NOT be suppressed (non-TTY kill -INT \
             must still reach the child — issue #26)"
        );
    }

    /// Wait up to `timeout` for `child` to exit, polling so an async relay has time
    /// to land. Returns the exit status, or None on timeout (then kills the child so
    /// the test never leaks a process).
    #[cfg(unix)]
    fn loop_wait(
        child: &mut std::process::Child,
        timeout: std::time::Duration,
    ) -> Option<ExitStatus> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Some(status);
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    #[test]
    fn compile_cache_sentinel_round_trips_and_cleans_up() {
        // R8: spawn.rs hands the user's NODE_COMPILE_CACHE dir to the child preload
        // through a PID-keyed sentinel file (never a NUB_* env var). Prove the
        // write lands at the path the child derives from process.ppid, carries the
        // exact dir bytes, and that the guard reclaims it on drop (the early-exit
        // fallback for when the preload didn't consume it).
        let dir = "/tmp/some/user compile cache";
        write_compile_cache_sentinel(dir).unwrap();
        let path = compile_cache_sentinel_path(std::process::id());
        assert_eq!(fs::read_to_string(&path).unwrap(), dir);

        drop(CompileCacheSentinelGuard);
        assert!(
            !path.exists(),
            "the guard must remove the sentinel so it never leaks"
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn compile_cache_tmpdir_mirrors_node_os_tmpdir_on_posix() {
        // The sentinel-dir resolver must stay byte-parity with the JS `tmpdirNoOs()`
        // (preload-common.cjs): if the two ends disagree, the child can't find the
        // sentinel nub wrote and the compile cache silently never enables. POSIX order
        // is TMPDIR→TMP→TEMP→/tmp, trailing-slash stripped. Driven through the
        // injectable resolver so the test never mutates (parallel-safe) process env.
        let resolve = |pairs: &[(&str, &str)]| -> PathBuf {
            let map: std::collections::HashMap<String, String> = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            // Mirror the live resolver: empty values are treated as unset.
            compile_cache_tmpdir_from(|k| map.get(k).cloned().filter(|s| !s.is_empty()))
        };

        // TMPDIR set → used verbatim.
        assert_eq!(
            resolve(&[("TMPDIR", "/custom/tmp")]),
            PathBuf::from("/custom/tmp")
        );
        // All unset → /tmp fallback (the case the clean `env -i` corpus harness hits).
        assert_eq!(resolve(&[]), PathBuf::from("/tmp"));
        // Trailing slash stripped (so the sentinel path doesn't double the separator).
        assert_eq!(
            resolve(&[("TMPDIR", "/custom/tmp/")]),
            PathBuf::from("/custom/tmp")
        );
        // TMP fallback when TMPDIR is unset.
        assert_eq!(resolve(&[("TMP", "/from/tmp")]), PathBuf::from("/from/tmp"));
        // TEMP fallback when TMPDIR and TMP are both unset (lowest POSIX priority).
        assert_eq!(
            resolve(&[("TEMP", "/from/temp")]),
            PathBuf::from("/from/temp")
        );
        // Priority: TMPDIR wins over TMP/TEMP when several are set.
        assert_eq!(
            resolve(&[("TMPDIR", "/win"), ("TMP", "/lose"), ("TEMP", "/lose")]),
            PathBuf::from("/win"),
        );
        // An empty TMPDIR is treated as unset → falls through to TMP.
        assert_eq!(
            resolve(&[("TMPDIR", ""), ("TMP", "/from/tmp")]),
            PathBuf::from("/from/tmp")
        );
    }

    #[test]
    #[cfg(windows)]
    fn compile_cache_tmpdir_mirrors_node_os_tmpdir_on_windows() {
        // Win32 order is TEMP→TMP→(SystemRoot|windir)\temp, trailing-backslash stripped
        // except after a drive root (`C:\`). Byte-parity with the JS `tmpdirNoOs()`
        // Win32 branch. Injectable resolver → no process-env mutation.
        let resolve = |pairs: &[(&str, &str)]| -> PathBuf {
            let map: std::collections::HashMap<String, String> = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            compile_cache_tmpdir_from(|k| map.get(k).cloned().filter(|s| !s.is_empty()))
        };

        // TEMP wins (highest Win32 priority).
        assert_eq!(
            resolve(&[
                ("TEMP", "C:\\Users\\me\\AppData\\Local\\Temp"),
                ("TMP", "C:\\lose")
            ]),
            PathBuf::from("C:\\Users\\me\\AppData\\Local\\Temp"),
        );
        // TMP fallback when TEMP is unset.
        assert_eq!(
            resolve(&[("TMP", "C:\\from\\tmp")]),
            PathBuf::from("C:\\from\\tmp")
        );
        // Neither TEMP nor TMP → SystemRoot\temp.
        assert_eq!(
            resolve(&[("SystemRoot", "C:\\Windows")]),
            PathBuf::from("C:\\Windows\\temp"),
        );
        // windir is the SystemRoot fallback.
        assert_eq!(
            resolve(&[("windir", "D:\\WinDir")]),
            PathBuf::from("D:\\WinDir\\temp"),
        );
        // Trailing backslash stripped, but a bare drive root `C:\` is preserved.
        assert_eq!(
            resolve(&[("TEMP", "C:\\Temp\\")]),
            PathBuf::from("C:\\Temp")
        );
        assert_eq!(resolve(&[("TEMP", "C:\\")]), PathBuf::from("C:\\"));
    }

    fn shim_test_root(label: &str) -> PathBuf {
        for _ in 0..PATH_SHIM_CREATE_RETRIES {
            let root = env::temp_dir().join(format!(
                "nub-path-shim-test-{}-{label}-{}",
                std::process::id(),
                nonce_hex(secure_shim_nonce().unwrap())
            ));
            match fs::create_dir(&root) {
                Ok(()) => return root,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("creating isolated PATH shim test root: {error}"),
            }
        }
        panic!("could not create an isolated PATH shim test root")
    }

    fn shim_test_binary(root: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn path_shim_is_private_randomized_and_shared_by_concurrent_setups() {
        let root = shim_test_root("concurrent");
        let nub_binary = shim_test_binary(&root, "nub-bin", b"current nub");
        let manager = PathShimManager::new();
        let pid = 4242;
        let dirs: Vec<_> = std::thread::scope(|scope| {
            (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        manager
                            .setup_in(
                                &nub_binary,
                                &root,
                                pid,
                                || Ok([0xab; 16]),
                                ShimSetupOptions::default(),
                            )
                            .unwrap()
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect()
        });
        assert!(
            dirs.windows(2).all(|pair| pair[0] == pair[1]),
            "concurrent callers published different PATH shim dirs: {dirs:?}"
        );

        let dir = PathBuf::from(dirs[0].as_str());
        assert_eq!(
            parse_shim_dir_name(dir.file_name().unwrap().to_str().unwrap()),
            Some((pid, ShimDirNameKind::Randomized))
        );
        assert!(
            !fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().contains("staging"))
        );
        #[cfg(unix)]
        {
            let metadata = fs::symlink_metadata(&dir).unwrap();
            assert_eq!(metadata.mode() & 0o7777, 0o700);
            // SAFETY: `geteuid` has no preconditions.
            assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
            assert_eq!(
                fs::read_link(node_shim_path(&dir)).unwrap(),
                fs::canonicalize(&nub_binary).unwrap()
            );
        }

        manager.cleanup();
        assert!(!dir.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_same_pid_collision_is_never_adopted() {
        let root = shim_test_root("collision");
        let nub_binary = shim_test_binary(&root, "nub-bin", b"current nub");
        let pid = 5151;
        let stale_nonce = [0x11; 16];
        let fresh_nonce = [0x22; 16];
        let stale = root.join(format!(
            "{PATH_SHIM_PREFIX}{pid}-{}",
            nonce_hex(stale_nonce)
        ));
        create_shim_dir(&stale).unwrap();
        set_private_shim_permissions(&stale).unwrap();
        fs::write(node_shim_path(&stale), b"obsolete shim").unwrap();

        let mut nonces = [stale_nonce, fresh_nonce].into_iter();
        let manager = PathShimManager::new();
        let created = manager
            .setup_in(
                &nub_binary,
                &root,
                pid,
                || Ok(nonces.next().unwrap()),
                ShimSetupOptions::default(),
            )
            .unwrap();
        let expected = root.join(format!(
            "{PATH_SHIM_PREFIX}{pid}-{}",
            nonce_hex(fresh_nonce)
        ));
        assert_eq!(Path::new(created.as_str()), expected);
        assert_eq!(fs::read(node_shim_path(&stale)).unwrap(), b"obsolete shim");

        manager.cleanup();
        assert!(stale.exists(), "cleanup removed a stale same-PID collision");
        assert!(!expected.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unpublished_path_shim_rolls_back_before_retry() {
        let root = shim_test_root("rollback");
        let nub_binary = shim_test_binary(&root, "nub-bin", b"current nub");
        let pid = 6161;
        let nonce = [0x33; 16];
        let dir = root.join(format!("{PATH_SHIM_PREFIX}{pid}-{}", nonce_hex(nonce)));
        let manager = PathShimManager::new();

        let error = manager
            .setup_in(
                &nub_binary,
                &root,
                pid,
                || Ok(nonce),
                ShimSetupOptions {
                    #[cfg(windows)]
                    force_copy: false,
                    fail_before_commit: true,
                },
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("injected PATH shim commit failure")
        );
        assert!(!dir.exists(), "an unpublished PATH shim directory leaked");
        assert!(manager.state.lock().unwrap().is_none());

        manager
            .setup_in(
                &nub_binary,
                &root,
                pid,
                || Ok(nonce),
                ShimSetupOptions::default(),
            )
            .unwrap();
        manager.cleanup();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_cached_shim_is_replaced_once_for_concurrent_callers() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let root = shim_test_root("cached-invalid");
        let nub_binary = shim_test_binary(&root, "nub-bin", b"current nub");
        let replacement = shim_test_binary(&root, "replacement-bin", b"not nub");
        let manager = PathShimManager::new();
        let dir = manager
            .setup_in(
                &nub_binary,
                &root,
                7171,
                || Ok([0x44; 16]),
                ShimSetupOptions::default(),
            )
            .unwrap();
        let node = node_shim_path(Path::new(dir.as_str()));
        fs::rename(&node, node.with_extension("old")).unwrap();
        #[cfg(unix)]
        unix_fs::symlink(&replacement, &node).unwrap();
        #[cfg(windows)]
        fs::copy(&replacement, &node).unwrap();

        let nonce_calls = AtomicUsize::new(0);
        let replacements: Vec<_> = std::thread::scope(|scope| {
            (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        manager
                            .setup_in(
                                &nub_binary,
                                &root,
                                7171,
                                || {
                                    nonce_calls.fetch_add(1, Ordering::Relaxed);
                                    Ok([0x55; 16])
                                },
                                ShimSetupOptions::default(),
                            )
                            .unwrap()
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect()
        });
        assert_eq!(nonce_calls.load(Ordering::Relaxed), 1);
        assert!(
            replacements.windows(2).all(|pair| pair[0] == pair[1]),
            "concurrent callers published different replacements: {replacements:?}"
        );
        assert_ne!(replacements[0], dir);
        let replacement_dir = PathBuf::from(replacements[0].as_str());
        assert!(replacement_dir.exists());

        manager.cleanup();
        assert!(!replacement_dir.exists());
        assert!(
            Path::new(dir.as_str()).exists(),
            "cleanup removed a directory whose node entry changed identity"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_cached_shim_replacement_can_retry_later() {
        let root = shim_test_root("cached-retry");
        let nub_binary = shim_test_binary(&root, "nub-bin", b"current nub");
        let replacement = shim_test_binary(&root, "replacement-bin", b"not nub");
        let manager = PathShimManager::new();
        let original = manager
            .setup_in(
                &nub_binary,
                &root,
                7272,
                || Ok([0x66; 16]),
                ShimSetupOptions::default(),
            )
            .unwrap();
        let node = node_shim_path(Path::new(original.as_str()));
        fs::rename(&node, node.with_extension("old")).unwrap();
        #[cfg(unix)]
        unix_fs::symlink(&replacement, &node).unwrap();
        #[cfg(windows)]
        fs::copy(&replacement, &node).unwrap();

        let failed_nonce = [0x77; 16];
        let failed_dir = root.join(format!(
            "{PATH_SHIM_PREFIX}{}-{}",
            7272,
            nonce_hex(failed_nonce)
        ));
        let error = manager
            .setup_in(
                &nub_binary,
                &root,
                7272,
                || Ok(failed_nonce),
                ShimSetupOptions {
                    #[cfg(windows)]
                    force_copy: false,
                    fail_before_commit: true,
                },
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("injected PATH shim commit failure")
        );
        assert!(manager.state.lock().unwrap().is_none());
        assert!(!failed_dir.exists());

        let retry_nonce = [0x88; 16];
        let retried = manager
            .setup_in(
                &nub_binary,
                &root,
                7272,
                || Ok(retry_nonce),
                ShimSetupOptions::default(),
            )
            .unwrap();
        assert_eq!(
            Path::new(retried.as_str()),
            root.join(format!(
                "{PATH_SHIM_PREFIX}{}-{}",
                7272,
                nonce_hex(retry_nonce)
            ))
        );

        manager.cleanup();
        assert!(!Path::new(retried.as_str()).exists());
        assert!(Path::new(original.as_str()).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn randomized_shim_name_parser_is_canonical() {
        let nonce = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            parse_shim_dir_name("nub-node-shim-1"),
            Some((1, ShimDirNameKind::Legacy))
        );
        assert_eq!(
            parse_shim_dir_name(&format!("nub-node-shim-4294967295-{nonce}")),
            Some((u32::MAX, ShimDirNameKind::Randomized))
        );
        assert!(is_legacy_staging_name(".node-staging-1-0", 1));
        assert!(is_legacy_staging_name(
            ".node-staging-1-18446744073709551615",
            1
        ));

        for invalid in [
            "nub-node-shim-01",
            "nub-node-shim-4294967296",
            "nub-node-shim-1-0123456789abcdef0123456789abcde",
            "nub-node-shim-1-0123456789abcdef0123456789abcdeF",
            "nub-node-shim-1-0123456789abcdef0123456789abcdef-extra",
        ] {
            assert_eq!(parse_shim_dir_name(invalid), None, "accepted {invalid}");
        }
        for invalid in [
            ".node-staging-1-01",
            ".node-staging-1-18446744073709551616",
            ".node-staging-1-0-extra",
        ] {
            assert!(!is_legacy_staging_name(invalid, 1), "accepted {invalid}");
        }
    }

    #[test]
    fn reaper_handles_randomized_and_legacy_names_without_pid_reuse_damage() {
        let root = shim_test_root("reaper");
        let make = |name: &str| {
            let dir = root.join(name);
            create_shim_dir(&dir).unwrap();
            set_private_shim_permissions(&dir).unwrap();
            fs::write(node_shim_path(&dir), b"shim").unwrap();
            dir
        };
        let own = make("nub-node-shim-1000-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let live = make("nub-node-shim-2000-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let dead = make("nub-node-shim-3000-dddddddddddddddddddddddddddddddd");
        let legacy_dead = make("nub-node-shim-3001");
        fs::write(legacy_dead.join(".node-staging-3001-0"), b"staged").unwrap();
        let dead_with_foreign = make("nub-node-shim-3005-55555555555555555555555555555555");
        fs::write(dead_with_foreign.join("foreign-entry"), b"keep").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&legacy_dead, fs::Permissions::from_mode(0o755)).unwrap();
        let malformed = make("nub-node-shim-3002-EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE");
        #[cfg(unix)]
        let non_private = {
            let dir = make("nub-node-shim-3003-eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
            dir
        };
        #[cfg(unix)]
        let redirected = {
            let target = root.join("redirect-target");
            fs::create_dir(&target).unwrap();
            let path = root.join("nub-node-shim-3004-ffffffffffffffffffffffffffffffff");
            unix_fs::symlink(target, &path).unwrap();
            path
        };

        reap_stale_shims_in(&root, 1000, |pid| pid == 2000);

        assert!(own.exists());
        assert!(live.exists());
        assert!(!dead.exists());
        assert!(!legacy_dead.exists());
        assert!(
            dead_with_foreign.join("foreign-entry").exists(),
            "reaper recursively removed foreign directory contents"
        );
        assert!(malformed.exists());
        #[cfg(unix)]
        assert!(
            non_private.exists(),
            "a non-private randomized dir was reaped"
        );
        #[cfg(unix)]
        assert!(
            redirected.symlink_metadata().is_ok(),
            "a redirected randomized path was reaped"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_staging_reaping_is_bounded_per_directory() {
        let root = shim_test_root("legacy-reap-cap");
        let dir = root.join("nub-node-shim-7777");
        fs::create_dir(&dir).unwrap();
        for counter in 0..=REAP_LEGACY_ENTRY_CAP {
            fs::write(dir.join(format!(".node-staging-7777-{counter}")), b"staged").unwrap();
        }

        remove_legacy_staging_entries(&dir, 7777);
        assert_eq!(
            fs::read_dir(&dir).unwrap().flatten().count(),
            1,
            "one sweep exceeded the per-directory reaper cap"
        );
        remove_legacy_staging_entries(&dir, 7777);
        assert_eq!(fs::read_dir(&dir).unwrap().flatten().count(), 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(windows)]
    fn windows_identity_compares_the_full_128_bit_file_id() {
        let lower_64_bits = [0x5a; 8];
        let mut first_id = [0u8; 16];
        first_id[..8].copy_from_slice(&lower_64_bits);
        let mut second_id = first_id;
        second_id[15] = 1;

        assert_ne!(
            WindowsFileIdentity {
                volume_serial: 7,
                file_id: first_id,
            },
            WindowsFileIdentity {
                volume_serial: 7,
                file_id: second_id,
            },
            "ReFS identities that differ above 64 bits must remain distinct"
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_identity_rejects_unsupported_sentinel_values() {
        assert!(windows_file_identity(7, [0; 16]).is_err());
        assert!(windows_file_identity(7, [u8::MAX; 16]).is_err());
    }

    #[test]
    #[cfg(windows)]
    fn windows_hardlink_and_forced_copy_keep_distinct_validated_identities() {
        let root = shim_test_root("windows-publication");
        let source = shim_test_binary(&root, "nub-bin", b"current nub bytes");

        let hardlink_probe = root.join("hardlink-probe");
        if fs::hard_link(&source, &hardlink_probe).is_ok() {
            fs::remove_file(hardlink_probe).unwrap();
            let hardlink_manager = PathShimManager::new();
            let hardlink_dir = hardlink_manager
                .setup_in(
                    &source,
                    &root,
                    9191,
                    || Ok([0x88; 16]),
                    ShimSetupOptions::default(),
                )
                .unwrap();
            {
                let state = hardlink_manager.state.lock().unwrap();
                let record = state.as_ref().unwrap();
                let source_identity = FileHandle::from_path(&source).unwrap();
                assert_eq!(source_identity, record.node_identity);
                assert_eq!(
                    FileHandle::from_path(&record.dir).unwrap(),
                    record.dir_identity
                );
            }
            hardlink_manager.cleanup();
            assert!(
                !Path::new(hardlink_dir.as_str()).exists(),
                "Windows hardlink cleanup leaked its shim directory"
            );
        } else {
            eprintln!("skipping hardlink publication: test TEMP does not support hardlinks");
        }

        let copy_manager = PathShimManager::new();
        let dir = copy_manager
            .setup_in(
                &source,
                &root,
                9292,
                || Ok([0x99; 16]),
                ShimSetupOptions {
                    force_copy: true,
                    ..ShimSetupOptions::default()
                },
            )
            .unwrap();
        {
            let state = copy_manager.state.lock().unwrap();
            let record = state.as_ref().unwrap();
            let source_identity = FileHandle::from_path(&source).unwrap();
            assert_ne!(source_identity, record.node_identity);
            assert_eq!(
                fs::read(&source).unwrap(),
                fs::read(node_shim_path(&record.dir)).unwrap(),
                "forced-copy bytes differ from the source"
            );
        }

        copy_manager.cleanup();
        assert!(
            !Path::new(dir.as_str()).exists(),
            "Windows copy cleanup leaked its shim directory"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(windows)]
    fn windows_cached_shim_rejects_directory_and_node_reparse_points() {
        use std::os::windows::fs::{symlink_dir, symlink_file};

        let root = shim_test_root("windows-reparse");
        let source = shim_test_binary(&root, "nub-bin", b"current nub bytes");

        let dir_manager = PathShimManager::new();
        let dir = PathBuf::from(
            dir_manager
                .setup_in(
                    &source,
                    &root,
                    9393,
                    || Ok([0xab; 16]),
                    ShimSetupOptions::default(),
                )
                .unwrap()
                .as_str(),
        );
        let displaced = dir.with_extension("old");
        match fs::rename(&dir, &displaced) {
            Ok(()) => {
                if let Err(error) = symlink_dir(&displaced, &dir) {
                    if error.raw_os_error() == Some(1314) {
                        eprintln!(
                            "skipping Windows directory reparse rejection: symlink privilege is unavailable"
                        );
                        dir_manager.cleanup();
                    } else {
                        panic!("creating directory reparse point: {error}");
                    }
                } else {
                    let replacement = dir_manager
                        .setup_in(
                            &source,
                            &root,
                            9393,
                            || Ok([0xac; 16]),
                            ShimSetupOptions::default(),
                        )
                        .unwrap();
                    assert_ne!(Path::new(replacement.as_str()), dir);
                    assert!(Path::new(replacement.as_str()).exists());
                    dir_manager.cleanup();
                    assert!(!Path::new(replacement.as_str()).exists());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                // Windows can block renaming a directory while the cached
                // directory/node handles are live. That is already fail-closed;
                // exercise the reparse validator directly so this branch still
                // covers the fallback boundary.
                dir_manager.cleanup();
                let target = root.join("directory-reparse-target");
                let reparse = root.join("directory-reparse");
                fs::create_dir(&target).unwrap();
                if let Err(error) = symlink_dir(&target, &reparse) {
                    if error.raw_os_error() == Some(1314) {
                        eprintln!(
                            "skipping direct Windows directory reparse rejection: symlink privilege is unavailable"
                        );
                    } else {
                        panic!("creating direct directory reparse point: {error}");
                    }
                } else {
                    let error = validate_shim_dir(&reparse, true).unwrap_err();
                    assert!(
                        error.to_string().contains("not a real directory"),
                        "direct directory reparse point was not rejected: {error:#}"
                    );
                }
            }
            Err(error) => panic!("displacing cached shim directory: {error}"),
        }

        let node_manager = PathShimManager::new();
        let dir = node_manager
            .setup_in(
                &source,
                &root,
                9494,
                || Ok([0xad; 16]),
                ShimSetupOptions {
                    force_copy: true,
                    ..ShimSetupOptions::default()
                },
            )
            .unwrap();
        let node = node_shim_path(Path::new(dir.as_str()));
        fs::rename(&node, node.with_extension("old")).unwrap();
        if let Err(error) = symlink_file(&source, &node) {
            if error.raw_os_error() == Some(1314) {
                eprintln!(
                    "skipping Windows node reparse rejection: symlink privilege is unavailable"
                );
                node_manager.cleanup();
                fs::remove_dir_all(root).unwrap();
                return;
            }
            panic!("creating file reparse point: {error}");
        }
        let replacement = node_manager
            .setup_in(
                &source,
                &root,
                9494,
                || Ok([0xae; 16]),
                ShimSetupOptions::default(),
            )
            .unwrap();
        assert_ne!(Path::new(replacement.as_str()), Path::new(dir.as_str()));
        assert!(Path::new(replacement.as_str()).exists());
        node_manager.cleanup();
        assert!(!Path::new(replacement.as_str()).exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn permission_flag_classifier_is_exact_not_a_prefix_match() {
        // Real permission flags engage Node's sandbox (and need --allow-addons).
        assert!(is_permission_flag("--permission"));
        assert!(is_permission_flag("--allow-addons"));
        // Value-taking permission flags appear as `--flag=value`; match up to `=`.
        assert!(is_permission_flag("--allow-fs-read=/tmp"));
        assert!(is_permission_flag("--allow-net=localhost"));
        // --allow-ffi is a real permission flag on the Node versions that have it.
        assert!(is_permission_flag("--allow-ffi"));

        // The bug R5 fixes: a V8 flag that shares the --allow- prefix but is NOT a
        // permission flag. Stock node runs `--allow-natives-syntax x.js`; the old
        // prefix match aborted it as "--permission requires --allow-addons".
        assert!(!is_permission_flag("--allow-natives-syntax"));
        // Plain user args and other --allow-*-looking-but-unknown tokens don't trip it.
        assert!(!is_permission_flag("--enable-source-maps"));
        assert!(!is_permission_flag("script.js"));
    }

    #[test]
    fn coverage_exclude_targets_absolute_runtime_dir_only_when_coverage_active() {
        let preload = "/opt/nub/runtime/preload.mjs";

        // No coverage flag anywhere → no exclude injected.
        assert!(coverage_exclude_glob(&[], None, Some(preload)).is_none());

        // Coverage via argv → exclude keyed to the ABSOLUTE runtime dir (the
        // preload's parent), with a trailing `/**` — not a broad `**/runtime/**`.
        let argv = vec![
            "--test".to_string(),
            "--experimental-test-coverage".to_string(),
        ];
        assert_eq!(
            coverage_exclude_glob(&argv, None, Some(preload)).as_deref(),
            Some("--test-coverage-exclude=/opt/nub/runtime/**"),
        );

        // Coverage via NODE_OPTIONS is detected the same way.
        assert_eq!(
            coverage_exclude_glob(&[], Some("--experimental-test-coverage"), Some(preload))
                .as_deref(),
            Some("--test-coverage-exclude=/opt/nub/runtime/**"),
        );

        // Coverage active but no resolvable preload → nothing to exclude.
        assert!(coverage_exclude_glob(&argv, None, None).is_none());
    }

    #[test]
    fn compile_cache_coverage_gate_fires_on_every_coverage_channel() {
        // The compile-cache/coverage gate (Fix 3): nub must set up NO compile cache
        // when this run is collecting V8 coverage, because a warm cache collapses
        // V8's per-branch ranges. Coverage engages through three channels — gate on
        // all of them.
        let cov_argv = vec![
            "--test".to_string(),
            "--experimental-test-coverage".to_string(),
        ];
        let plain_argv = vec!["app.js".to_string()];

        // (1) Coverage via argv.
        assert!(coverage_active_for_cache(&cov_argv, None, None));
        // (2) Coverage via NODE_OPTIONS.
        assert!(coverage_active_for_cache(
            &plain_argv,
            Some("--experimental-test-coverage"),
            None
        ));
        // (3) Coverage via NODE_V8_COVERAGE env (no flag anywhere) — the channel
        //     coverage_active (R9 exclude-glob) does NOT cover, but the cache gate
        //     must, since `NODE_V8_COVERAGE=<dir> node app.js` collects coverage
        //     with no flag.
        assert!(coverage_active_for_cache(
            &plain_argv,
            None,
            Some("/tmp/cov")
        ));

        // No coverage signal on any channel → gate stays OFF (cache enabled). An
        // EMPTY NODE_V8_COVERAGE is not coverage (Node treats empty as disabled),
        // and a user-set NODE_COMPILE_CACHE is intentionally not consulted here —
        // its preservation is the caller's concern, not this gate's.
        assert!(!coverage_active_for_cache(&plain_argv, None, None));
        assert!(!coverage_active_for_cache(
            &plain_argv,
            Some("--enable-source-maps"),
            None
        ));
        assert!(!coverage_active_for_cache(&plain_argv, None, Some("")));
    }

    #[test]
    fn reentrancy_matches_full_preload_path_not_filename_substring() {
        let ours = "/opt/nub/runtime/preload.mjs";

        // The A26 bug: a user's OWN --import of a file merely named preload.mjs
        // must NOT register as ours (the old substring check did, and wrongly
        // disabled augmentation).
        assert!(
            !is_reentrant_in(Some("--import=file:///home/me/app/preload.mjs"), Some(ours),),
            "a user's unrelated preload.mjs must not be mistaken for nub's"
        );

        // NODE_OPTIONS carrying our actual preload path IS re-entrant (a parent
        // nub injected it), even alongside other flags and a user import.
        assert!(
            is_reentrant_in(
                Some(&format!(
                    "--experimental-vm-modules --import=file://{ours} --import=file:///u/preload.mjs"
                )),
                Some(ours),
            ),
            "our own preload path in NODE_OPTIONS means a parent nub already augmented"
        );

        // Degenerate inputs are never re-entrant.
        assert!(!is_reentrant_in(None, Some(ours)), "no NODE_OPTIONS set");
        assert!(
            !is_reentrant_in(Some("--import=file:///x/preload.mjs"), None),
            "no preload resolved"
        );
        assert!(!is_reentrant_in(Some(""), Some(ours)), "empty NODE_OPTIONS");
    }

    #[test]
    fn preload_injection_is_require_cjs_on_fast_tier_import_mjs_on_compat() {
        let mjs = "/opt/nub/runtime/preload.mjs";

        // Fast tier (>= 22.15): `--require` the sibling CJS preload by raw PATH
        // (require does not accept a file:// URL). This is the channel that keeps
        // Node's synchronous CJS entry path (the R1 fix).
        let fast = preload_injection_for(mjs, &NodeVersion::new(22, 15, 0), false);
        assert_eq!(fast.flag, "--require");
        assert_eq!(fast.value, "/opt/nub/runtime/preload.cjs");
        assert_eq!(
            fast.node_options_token(),
            "--require=/opt/nub/runtime/preload.cjs"
        );

        // A clearly-fast version too (24.x).
        let fast24 = preload_injection_for(mjs, &NodeVersion::new(24, 0, 0), false);
        assert_eq!(fast24.flag, "--require");
        assert_eq!(fast24.value, "/opt/nub/runtime/preload.cjs");

        // Compat tier (< 22.15): `--import` the ESM preload by file:// URL — the
        // async path stays unchanged.
        let compat = preload_injection_for(mjs, &NodeVersion::new(20, 11, 0), false);
        assert_eq!(compat.flag, "--import");
        assert_eq!(compat.value, "file:///opt/nub/runtime/preload.mjs");
        assert_eq!(
            compat.node_options_token(),
            "--import=file:///opt/nub/runtime/preload.mjs"
        );

        // The 22.14.x boundary stays on the compat (import) channel.
        let boundary = preload_injection_for(mjs, &NodeVersion::new(22, 14, 99), false);
        assert_eq!(boundary.flag, "--import");
    }

    #[test]
    fn file_url_unix_is_file_plus_path() {
        assert_eq!(
            to_file_url("/opt/nub/runtime/preload.mjs", false),
            "file:///opt/nub/runtime/preload.mjs"
        );
    }

    #[test]
    fn file_url_windows_drive_and_verbatim() {
        // Plain drive path.
        assert_eq!(
            to_file_url(r"C:\npm\prefix\runtime\preload.mjs", true),
            "file:///C:/npm/prefix/runtime/preload.mjs"
        );
        // The exact path from the 0.0.9 windows test-install failure: a canonicalized
        // `\\?\` verbatim path. A naive `file://` + path produced the malformed
        // `file:////?\C:\...` that Node rejected (ERR_INVALID_FILE_URL_PATH).
        assert_eq!(
            to_file_url(
                r"\\?\C:\npm\prefix\node_modules\@nubjs\nub\node_modules\@nubjs\nub-win32-x64\runtime\preload.mjs",
                true
            ),
            "file:///C:/npm/prefix/node_modules/@nubjs/nub/node_modules/@nubjs/nub-win32-x64/runtime/preload.mjs"
        );
        // UNC verbatim path -> file://server/share/...
        assert_eq!(
            to_file_url(r"\\?\UNC\server\share\runtime\preload.mjs", true),
            "file://server/share/runtime/preload.mjs"
        );
    }

    #[test]
    fn strip_verbatim_removes_windows_prefixes_only() {
        assert_eq!(strip_verbatim(r"\\?\C:\a\b", true), r"C:\a\b");
        assert_eq!(strip_verbatim(r"\\?\UNC\srv\sh", true), r"\\srv\sh");
        assert_eq!(strip_verbatim(r"C:\a\b", true), r"C:\a\b"); // no prefix: unchanged
        // Non-Windows host never strips (a unix path could legitimately start oddly).
        assert_eq!(strip_verbatim(r"\\?\C:\a", false), r"\\?\C:\a");
    }

    #[test]
    fn reentrancy_holds_through_url_keying_on_windows() {
        // The fix's invariant: the parent injects `--import=<url>` into NODE_OPTIONS,
        // and the child detects re-entrancy by finding that URL — for the SAME
        // canonicalized preload path on both sides. Proven for the Windows verbatim
        // path on any host via the `windows` param.
        let raw = r"\\?\C:\app\runtime\preload.mjs";
        let url = to_file_url(raw, true);
        let injected = format!("--experimental-vm-modules --import={url}");
        assert!(
            is_reentrant_in(Some(&injected), Some(&url)),
            "child must detect the parent-injected url in NODE_OPTIONS"
        );
        // And an unrelated user preload.mjs still must not false-positive.
        assert!(
            !is_reentrant_in(Some("--import=file:///C:/me/app/preload.mjs"), Some(&url)),
            "a different preload path must not register as ours"
        );
    }

    #[test]
    fn vendored_node_path_present_only_for_installed_package() {
        let tmp = env::temp_dir().join(format!("nub-a30-{}", std::process::id()));
        let runtime = tmp.join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        let preload = runtime.join("preload.mjs");
        fs::write(&preload, "").unwrap();
        let preload_str = preload.to_str().unwrap();

        // Dev: runtime/ has no node_modules → None (CJS requires resolve by
        // walking up to the repo's node_modules; no NODE_PATH needed).
        assert!(
            vendored_node_path(Some(preload_str)).is_none(),
            "no node_modules → None"
        );

        // Installed package: runtime/node_modules exists → NODE_PATH leads with it.
        let vendored = runtime.join("node_modules");
        fs::create_dir_all(&vendored).unwrap();
        let np = vendored_node_path(Some(preload_str)).expect("node_modules present → Some");
        assert!(
            np.to_string_lossy().starts_with(vendored.to_str().unwrap()),
            "NODE_PATH leads with the vendored node_modules, got {np:?}"
        );

        assert!(vendored_node_path(None).is_none(), "no preload → None");
        let _ = fs::remove_dir_all(&tmp);
    }
}

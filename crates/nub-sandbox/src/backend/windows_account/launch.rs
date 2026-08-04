//! The unelevated per-run launch: ACL the policy's paths, then start the child AS the
//! sandbox account through the Secondary Logon service.
//!
//! WHY `CreateProcessWithLogonW` AND NOT `LogonUser` + `CreateProcessAsUserW`: the latter
//! makes the caller hold a FOREIGN primary token, which requires
//! `SE_ASSIGNPRIMARYTOKEN_NAME` + `SE_INCREASE_QUOTA_NAME` — i.e. administrator, on every
//! run. `CreateProcessWithLogonW` hands the credential to the seclogon service, which does
//! the token work out of process, so an ordinary unelevated token suffices. That single API
//! choice is what makes the "one elevated setup, then never again" promise hold. (SRT and
//! Codex independently converged on it.)
//!
//! CONSEQUENCES OF THAT API, all load-bearing:
//!   - It has NO `bInheritHandles` parameter and no `STARTUPINFOEX` overload. seclogon
//!     duplicates exactly the `STARTF_USESTDHANDLES` handles into the new logon and nothing
//!     else — so stdio must ride those three fields, and there is no handle-list attribute to
//!     scope inheritance with (nor any need for one: nothing else crosses).
//!   - `lpDesktop` is left NULL deliberately — but NULL does NOT mean there is no
//!     window-station work to do. seclogon's auto-grant covers `WinSta0` only, so a launch
//!     from a NON-INTERACTIVE caller (an SSH session, a service, a CI agent), which runs on a
//!     per-logon `Service-0x0-…$` station, needs an explicit station + desktop ace or the
//!     child dies in loader init with `0xC0000142` (VM-diagnosed; see
//!     [`WindowAceGuard`]). Setting `lpDesktop` was tried and does not substitute for it. The
//!     cost of NULL is no desktop isolation, which is a hardening follow-up, not a
//!     confinement hole.
//!   - `AssignProcessToJobObject` on the resulting child commonly fails `ERROR_NOT_SUPPORTED`:
//!     seclogon already placed it in its own job, and current Windows refuses that nesting
//!     cross-session. The assignment is attempted and its failure reported, never silently
//!     swallowed — whole-tree reap is genuinely weaker here than on the AppContainer path.

use super::{AccountLaunch, SANDBOX_ACCOUNT, account, acl, state};
use crate::backend::windows::launch::{build_command_line, build_env_block, to_wide};
use std::io;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    DESKTOP_CREATEMENU, DESKTOP_CREATEWINDOW, DESKTOP_ENUMERATE, DESKTOP_HOOKCONTROL,
    DESKTOP_JOURNALPLAYBACK, DESKTOP_JOURNALRECORD, DESKTOP_READ_CONTROL, DESKTOP_READOBJECTS,
    DESKTOP_SWITCHDESKTOP, DESKTOP_WRITEOBJECTS, GetProcessWindowStation, GetThreadDesktop,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessWithLogonW, GetCurrentThreadId,
    GetExitCodeProcess, INFINITE, LOGON_WITH_PROFILE, PROCESS_INFORMATION, ResumeThread,
    STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};

const ERROR_NOT_SUPPORTED: i32 = 50;
const ERROR_LOGON_FAILURE: i32 = 1326;
const ERROR_SERVICE_DISABLED: i32 = 1058;

/// `STATUS_DLL_INIT_FAILED`, which arrives as the CHILD'S EXIT CODE rather than a
/// `CreateProcessWithLogonW` failure — so nothing in [`map_spawn_error`] ever sees it and an
/// unmapped run surfaces the bare `-1073741502`.
const STATUS_DLL_INIT_FAILED: u32 = 0xC000_0142;

/// `WINSTA_ALL_ACCESS` (0x37F) — the union of the nine `WINSTA_*` rights. Spelled here because
/// `windows-sys` exports it only from `Win32_UI_WindowsAndMessaging`, a feature this crate does
/// not otherwise need. `DESKTOP_READ_CONTROL` is the same `READ_CONTROL` bit (0x0002_0000),
/// which is LOAD-BEARING on the station: without it the child HANGS in loader init rather than
/// failing, so it is folded in here and never optional.
const WINSTA_GRANT: u32 = 0x0000_037F | DESKTOP_READ_CONTROL;

/// The documented `DESKTOP_*` rights union (0x1FF), plus `READ_CONTROL` for the same reason.
const DESKTOP_GRANT: u32 = DESKTOP_READOBJECTS
    | DESKTOP_CREATEWINDOW
    | DESKTOP_CREATEMENU
    | DESKTOP_HOOKCONTROL
    | DESKTOP_JOURNALRECORD
    | DESKTOP_JOURNALPLAYBACK
    | DESKTOP_ENUMERATE
    | DESKTOP_WRITEOBJECTS
    | DESKTOP_SWITCHDESKTOP
    | DESKTOP_READ_CONTROL;

/// Strips every ace this run applied, on drop. Ordering matters: declared before the child is
/// spawned but dropped after the wait returns, so a granted path is never revoked out from
/// under a live child. Best-effort — a failed strip leaves an over-permissive ace for a
/// confined account, which the ledger sweep (`nub setup-sandbox --clean`) collects later.
struct AceGuard {
    paths: Vec<std::path::PathBuf>,
    sid: String,
}

impl Drop for AceGuard {
    fn drop(&mut self) {
        for p in &self.paths {
            match acl::strip(p, &self.sid) {
                Ok(()) => {}
                // A recorded path that no longer exists carries no ace to strip: it was either
                // skipped as absent at apply time, or deleted by the child inside a grant.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::debug!(path = %p.display(), error = %e, "sandbox: ace strip failed — left for the ledger sweep");
                }
            }
        }
    }
}

/// An ace target that does not exist is SKIPPED, not fatal: the flagship agent policy denies
/// `~/.ssh`, `~/.aws`, `<proj>/.env` and most are absent on any real machine, so failing would
/// kill the backend on its own headline shape. Denying an absent path denies nothing — the
/// created-later-inside-a-grant residual is in LIMITATIONS.md. Every other error stays fatal.
fn tolerate_absent(r: io::Result<()>, path: &Path, what: &str) -> io::Result<()> {
    match r {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            tracing::debug!(path = %path.display(), "sandbox: {what} target does not exist — skipped");
            Ok(())
        }
        other => other,
    }
}

impl AccountLaunch {
    pub(crate) fn run(self) -> io::Result<ExitStatus> {
        let marker = state::read_marker()?.ok_or_else(not_provisioned)?;

        // The account can be deleted out from under a stale marker; catching that here turns
        // an inscrutable `ERROR_LOGON_FAILURE` into an actionable message.
        match account::lookup_sid()? {
            Some(live) if live == marker.sid => {}
            Some(_) => {
                return Err(io::Error::other(
                    "the nub sandbox account exists but its SID no longer matches the recorded \
                     setup — re-run `nub setup-sandbox` from an elevated prompt",
                ));
            }
            None => return Err(not_provisioned()),
        }

        // Ledger BEFORE apply: a crash between the two leaves a recorded path whose ace was
        // never written, and stripping an absent ace is a no-op. The reverse order would
        // leave an ace nothing knows about.
        let mut guard = AceGuard {
            paths: Vec::new(),
            sid: marker.sid.clone(),
        };
        for (path, access) in self
            .read_grants
            .iter()
            .map(|p| (p, acl::Access::Read))
            .chain(
                self.write_grants
                    .iter()
                    .map(|p| (p, acl::Access::ReadWrite)),
            )
        {
            state::record_acl_path(path)?;
            guard.paths.push(path.clone());
            tolerate_absent(acl::grant(path, &marker.sid, access), path, "grant")?;
        }
        // Denies go on AFTER the grants so the deny ace is inserted into a DACL that already
        // carries the grant it must outrank — the canonical-order insert has to see both.
        for path in &self.denies {
            let targets = match acl::deny_targets(path) {
                Ok(t) => t,
                // Same skip-if-absent rule as the grants above.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    tracing::debug!(path = %path.display(), "sandbox: deny target does not exist — skipped");
                    continue;
                }
                Err(e) => return Err(e),
            };
            for t in targets {
                state::record_acl_path(&t)?;
                guard.paths.push(t);
            }
            acl::deny(path, &marker.sid)?;
        }

        let password = account::load_credential()?;
        let status = self.spawn_and_wait(&marker.sid, password.as_str());
        drop(guard);
        status
    }

    fn spawn_and_wait(&self, sid: &str, password: &str) -> io::Result<ExitStatus> {
        // Declared before the spawn and dropped after the wait, like `AceGuard`: revoking the
        // station access out from under a live child is the same hazard as revoking a path.
        let _window = WindowAceGuard::grant(sid);

        // The COMPILE-TIME const, never a name read from the marker file: the SID checked in
        // `run` resolves THIS name, so a name field on disk would have been an attacker-chosen
        // logon target that still passed that check — the marker lives in a directory a
        // standard user can pre-create and then own.
        let user_w = to_wide(SANDBOX_ACCOUNT);
        // "." targets the LOCAL SAM regardless of whether the machine is domain-joined.
        let domain_w = to_wide(".");
        let mut password_w: Vec<u16> = password.encode_utf16().chain(std::iter::once(0)).collect();
        let mut cmdline = build_command_line(&self.program, &self.args);
        let app_w = to_wide(&self.program.to_string_lossy());
        let cwd_w = self.cwd.as_ref().map(|c| to_wide(&c.to_string_lossy()));
        let env_block = self.env.as_ref().map(build_env_block);

        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        // seclogon duplicates ONLY these three handles, and only when the flag is set.
        let std_handles = inheritable_std_handles();
        if let Some((i, o, e)) = std_handles {
            si.dwFlags |= STARTF_USESTDHANDLES;
            si.hStdInput = i;
            si.hStdOutput = o;
            si.hStdError = e;
        }

        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let flags = CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED;
        let env_ptr: *const std::ffi::c_void = match &env_block {
            Some(b) => b.as_ptr().cast(),
            // NULL + LOGON_WITH_PROFILE makes seclogon build the SANDBOX ACCOUNT's own
            // profile environment — isolated USERPROFILE/TEMP/LOCALAPPDATA, machine PATH.
            None => std::ptr::null(),
        };

        // SAFETY: every buffer referenced (user/domain/password/app/cmdline/cwd/env/si)
        // outlives this call; `lpCommandLine` is a writable UTF-16 buffer as required.
        let ok = unsafe {
            CreateProcessWithLogonW(
                user_w.as_ptr(),
                domain_w.as_ptr(),
                password_w.as_ptr(),
                LOGON_WITH_PROFILE,
                app_w.as_ptr(),
                cmdline.as_mut_ptr(),
                flags,
                env_ptr,
                cwd_w.as_ref().map_or(std::ptr::null(), |w| w.as_ptr()),
                &si,
                &mut pi,
            )
        };
        account::scrub_u16(&mut password_w);
        if ok == 0 {
            return Err(map_spawn_error(io::Error::last_os_error()));
        }

        // Best-effort containment. seclogon has already placed the child in its own job and
        // current Windows refuses cross-session nesting, so this commonly returns
        // ERROR_NOT_SUPPORTED — reported, never presented as success.
        let job = match create_confinement_job() {
            Ok(j) => Some(j),
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "sandbox: could not create the reaping Job Object — whole-tree reap is \
                     best-effort"
                );
                None
            }
        };
        if let Some(j) = job {
            // SAFETY: both handles are live; the child is still suspended.
            if unsafe { AssignProcessToJobObject(j, pi.hProcess) } == 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(ERROR_NOT_SUPPORTED) {
                    tracing::debug!(
                        "sandbox: the sandbox child could not be placed in a nub Job Object \
                         (seclogon owns it) — whole-tree reap is best-effort"
                    );
                } else {
                    // A non-nesting failure is a real fault: terminate the still-suspended
                    // child rather than run it uncontained.
                    unsafe {
                        TerminateProcess(pi.hProcess, 1);
                        CloseHandle(pi.hThread);
                        CloseHandle(pi.hProcess);
                        CloseHandle(j);
                    }
                    return Err(io::Error::other(format!(
                        "sandbox: could not contain the child in a Job Object: {e}"
                    )));
                }
            }
        }

        // A failed resume must NOT fall through to the wait below: the child would stay
        // suspended forever and `WaitForSingleObject(…, INFINITE)` would never return.
        // SAFETY: `pi.hThread` is the suspended primary thread of the process just created.
        if unsafe { ResumeThread(pi.hThread) } == u32::MAX {
            let e = io::Error::last_os_error();
            // SAFETY: both handles are live and each is closed exactly once.
            unsafe {
                TerminateProcess(pi.hProcess, 1);
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
            }
            close_job(job);
            return Err(io::Error::other(format!(
                "sandbox: the sandboxed child could not be resumed: {e}"
            )));
        }

        // SAFETY: `pi.hProcess` is live until closed below.
        if unsafe { WaitForSingleObject(pi.hProcess, INFINITE) } != WAIT_OBJECT_0 {
            let e = io::Error::last_os_error();
            // SAFETY: as above.
            unsafe {
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
            }
            close_job(job);
            return Err(e);
        }

        let mut code: u32 = u32::MAX;
        // SAFETY: `pi.hProcess` is a live, signalled process handle.
        let queried = unsafe { GetExitCodeProcess(pi.hProcess, &mut code) };
        let query_err = (queried == 0).then(io::Error::last_os_error);
        // SAFETY: both handles came from CreateProcessWithLogonW and are closed exactly once.
        unsafe {
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
        }
        // Closed LAST so KILL_ON_JOB_CLOSE reaps anything still in the tree.
        close_job(job);

        if let Some(e) = query_err {
            return Err(io::Error::other(format!(
                "sandbox: the sandboxed child's exit code could not be read: {e}"
            )));
        }
        if code == STATUS_DLL_INIT_FAILED {
            return Err(io::Error::other(
                "sandbox: the sandboxed child died in loader init with STATUS_DLL_INIT_FAILED \
                 (0xC0000142) — it could not attach to this session's window station. nub grants \
                 the sandbox account access to the caller's station and desktop before \
                 launching; a station nub cannot re-ACL (an unusual service or remoting host) \
                 can still refuse it. Running from an ordinary interactive session avoids it.",
            ));
        }
        Ok(ExitStatus::from_raw(code))
    }
}

fn close_job(job: Option<HANDLE>) {
    if let Some(j) = job {
        // SAFETY: the handle came from CreateJobObjectW and is closed exactly once.
        unsafe { CloseHandle(j) };
    }
}

/// The parent's three std handles, marked inheritable so seclogon can duplicate them. `None`
/// when any is absent (a detached parent) — the child then gets no stdio rather than a
/// half-wired set that would make it block on a dead handle.
fn inheritable_std_handles() -> Option<(HANDLE, HANDLE, HANDLE)> {
    let i = std::io::stdin().as_raw_handle() as HANDLE;
    let o = std::io::stdout().as_raw_handle() as HANDLE;
    let e = std::io::stderr().as_raw_handle() as HANDLE;
    for h in [i, o, e] {
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return None;
        }
        // SAFETY: each handle is owned by this process for its whole lifetime.
        unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
    }
    Some((i, o, e))
}

/// The window-station and desktop aces the child needs, restored on drop.
///
/// WHY THIS EXISTS (VM-diagnosed 2026-07-24, reproduced with a bare P/Invoke on a throwaway
/// account and no nub code): a NON-INTERACTIVE caller — SSH, a service, a CI agent — runs on a
/// per-logon `Service-0x0-…$` window station, NOT `WinSta0`, and the Secondary Logon service's
/// station auto-grant covers only `WinSta0`. Without these two aces the child dies in loader
/// init with `0xC0000142 STATUS_DLL_INIT_FAILED`. Setting `lpDesktop` explicitly was tried and
/// does NOT substitute, which is why the launch still passes NULL.
///
/// Neither handle is closed: `GetProcessWindowStation` and `GetThreadDesktop` both return
/// handles the caller does not own.
/// ⛔ USED BY BOTH WINDOWS BACKENDS. The dedicated-account route calls it for its account SID;
/// the AppContainer/build-jail route (`backend/windows.rs`) calls it for the container SID,
/// because a LowBox is subject to exactly the same station gate. Measured 2026-08-04: over SSH
/// (a non-interactive station) every USER32-importing child — `node.exe`, `git.exe`, `nub.exe` —
/// died `STATUS_DLL_INIT_FAILED`, while a std-only crt-static probe and System32's
/// `hostname.exe` ran fine. Keep this ONE implementation: the restore-on-drop and the
/// fail-forward behaviour below are security-relevant and must not diverge between backends.
pub(crate) struct WindowAceGuard {
    /// `(handle, the DACL that was there before this run)`. `None` restores a NULL DACL.
    restore: Vec<(HANDLE, Option<Vec<u32>>)>,
}

impl WindowAceGuard {
    pub(crate) fn grant(sid: &str) -> Self {
        // SAFETY: neither call takes a parameter that can be invalid, and both return a handle
        // owned by the system for this process/thread's lifetime.
        let (station, desktop) = unsafe {
            (
                GetProcessWindowStation().cast::<std::ffi::c_void>(),
                GetThreadDesktop(GetCurrentThreadId()).cast::<std::ffi::c_void>(),
            )
        };
        let mut guard = WindowAceGuard {
            restore: Vec::with_capacity(2),
        };
        for (handle, mask) in [(station, WINSTA_GRANT), (desktop, DESKTOP_GRANT)] {
            if handle.is_null() {
                continue;
            }
            // FAIL FORWARD, never abort. On an interactive `WinSta0` these aces are redundant
            // — seclogon's auto-grant already covers it — so a station whose DACL nub cannot
            // rewrite (a locked-down remoting host, some CI agents) must still launch rather
            // than lose a run that worked before this ace existed. The one case where the ace
            // IS load-bearing surfaces instead as the mapped `STATUS_DLL_INIT_FAILED` exit.
            match acl::grant_window_object(handle, sid, mask) {
                Ok(saved) => guard.restore.push((handle, saved)),
                Err(e) => tracing::debug!(
                    error = %e,
                    "sandbox: could not grant the sandbox account window-object access — a \
                     child on a non-interactive station may fail loader init"
                ),
            }
        }
        guard
    }
}

impl Drop for WindowAceGuard {
    fn drop(&mut self) {
        for (handle, dacl) in &self.restore {
            if let Err(e) = acl::restore_window_object(*handle, dacl.as_deref()) {
                tracing::debug!(
                    error = %e,
                    "sandbox: could not restore the window-station DACL — the sandbox account \
                     keeps station access until this session ends"
                );
            }
        }
    }
}

/// The confinement Job: whole-tree reap on handle close, plus the active-process
/// ceiling (see [`crate::backend::windows::active_process_cap`]). On this backend the
/// assignment is best-effort — seclogon may already own the child — so the cap holds
/// only where the assignment succeeded.
fn create_confinement_job() -> io::Result<HANDLE> {
    // SAFETY: unnamed job with default security.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    info.BasicLimitInformation.ActiveProcessLimit = crate::backend::windows::active_process_cap();
    // SAFETY: `info` is a correctly-sized JOBOBJECT_EXTENDED_LIMIT_INFORMATION.
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::from_mut(&mut info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        let e = io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        return Err(e);
    }
    Ok(job)
}

fn not_provisioned() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "this policy needs nub's dedicated Windows sandbox account, which has not been set up \
         on this machine. Run `nub setup-sandbox` once from an elevated (Run as \
         administrator) prompt.",
    )
}

/// Turn the two spawn failures a user can actually act on into instructions.
fn map_spawn_error(e: io::Error) -> io::Error {
    match e.raw_os_error() {
        Some(ERROR_LOGON_FAILURE) => io::Error::other(format!(
            "sandbox: the stored credential for `{SANDBOX_ACCOUNT}` was rejected — the \
             account's password was changed or the account was disabled. Re-run \
             `nub setup-sandbox` from an elevated prompt to reprovision it."
        )),
        Some(ERROR_SERVICE_DISABLED) => io::Error::other(
            "sandbox: the Windows Secondary Logon service is disabled, so nub cannot start the \
             sandboxed child under its dedicated account. Enable the `seclogon` service (it \
             may be disabled by group policy).",
        ),
        _ => e,
    }
}

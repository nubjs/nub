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

/// The window-station and desktop aces the child needs, removed again on drop.
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
    /// The trustee this guard granted: the key into [`WINDOW_ACES`], and what the teardown
    /// strip matches on.
    sid: String,
    /// The station and desktop handles to clean up — captured here rather than re-derived in
    /// [`Drop`] so teardown cannot address a different desktop than the grant did. Recorded
    /// whether or not the grant itself succeeded: a mixed outcome across two guards sharing
    /// one SID must still leave the last of them able to remove whichever ace did land.
    handles: Vec<HANDLE>,
}

/// Every window-object grant this process holds, keyed by SID, with the number of live
/// [`WindowAceGuard`]s behind each. The Mutex serializes the DACL read-modify-write itself,
/// not just the bookkeeping.
///
/// ⛔ THE STATION AND THE DESKTOP ARE PROCESS-GLOBAL, SO A PER-RUN DACL SNAPSHOT IS WRONG BY
/// CONSTRUCTION. `child_concurrency` defaults to 5, so five lifecycle scripts launch at once,
/// each guarding these same two handles. The design this replaced saved the DACL each guard
/// found and restored that snapshot on drop, which loses an ace two independent ways:
///
///   1. Two grants interleave their read-modify-write, and the later write drops the earlier
///      ace — the ordinary lost update, which a lock alone does fix.
///   2. A guard restores a snapshot taken BEFORE a sibling granted, deleting that sibling's
///      ace while its child is still in loader init. A LOCK CANNOT FIX THIS ONE: the snapshot
///      is already stale by the time the restore is correctly serialized.
///
/// The child then dies `0xC0000142 STATUS_DLL_INIT_FAILED` before `main`, which reads as the
/// jail breaking the package rather than as a sandbox fault. Measured 2026-09-04 over SSH (a
/// non-interactive `Service-0x0-…$` station, where these aces are load-bearing): 6 of 7
/// concurrent installs found the object carrying NO ace for their SID with three guards still
/// live, and each lost a lifecycle script to that exit code.
///
/// So nothing is snapshotted. A grant is an additive read-modify-write under this lock, and
/// teardown removes exactly the aces naming this guard's SID — also under this lock, against
/// the DACL as it stands at that moment. The refcount is what makes the shared-SID case safe:
/// the AppContainer backend mints a fresh container SID per run, but the dedicated-account
/// backend grants ONE account SID for every run, so a strip on first drop would revoke an ace
/// a sibling's child still needs. Only the last live guard for a SID strips it.
static WINDOW_ACES: std::sync::Mutex<std::collections::BTreeMap<String, usize>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

impl WindowAceGuard {
    /// The station and desktop ace state for this guard's SID, read from the DACLs AS THEY
    /// STAND RIGHT NOW, plus how many guards are live across every SID.
    ///
    /// Callers print this beside a finished child's exit code under `NUB_JAIL_DUMP_POLICY`.
    /// `station_ace=false` next to `code=3221225794` names the fault outright, where the exit
    /// code alone is indistinguishable from a package that no longer builds — which is exactly
    /// how one lost ace was scored as thirteen broken packages instead of one sandbox bug.
    pub(crate) fn probe(&self) -> String {
        let total: usize = {
            let live = WINDOW_ACES.lock().unwrap_or_else(|e| e.into_inner());
            live.values().sum()
        };
        let mut out = format!("live={total}");
        for (i, handle) in self.handles.iter().enumerate() {
            let label = if i == 0 { "station" } else { "desktop" };
            match acl::window_object_has_sid(*handle, &self.sid) {
                Ok(v) => out.push_str(&format!(" {label}_ace={v}")),
                Err(e) => out.push_str(&format!(" {label}_ace=err({e})")),
            }
        }
        out
    }

    pub(crate) fn grant(sid: &str) -> Self {
        // SAFETY: neither call takes a parameter that can be invalid, and both return a handle
        // owned by the system for this process/thread's lifetime.
        let (station, desktop) = unsafe {
            (
                GetProcessWindowStation().cast::<std::ffi::c_void>(),
                GetThreadDesktop(GetCurrentThreadId()).cast::<std::ffi::c_void>(),
            )
        };
        // Poison-tolerant: a panicking holder leaves the map itself consistent, and refusing
        // to grant here would cost a run for nothing.
        let mut live = WINDOW_ACES.lock().unwrap_or_else(|e| e.into_inner());
        let mut handles = Vec::with_capacity(2);
        for (handle, mask) in [(station, WINSTA_GRANT), (desktop, DESKTOP_GRANT)] {
            if handle.is_null() {
                continue;
            }
            handles.push(handle);
            // FAIL FORWARD, never abort. On an interactive `WinSta0` these aces are redundant
            // — seclogon's auto-grant already covers it — so a station whose DACL nub cannot
            // rewrite (a locked-down remoting host, some CI agents) must still launch rather
            // than lose a run that worked before this ace existed. The one case where the ace
            // IS load-bearing surfaces instead as the mapped `STATUS_DLL_INIT_FAILED` exit.
            if let Err(e) = acl::grant_window_object(handle, sid, mask) {
                tracing::debug!(
                    error = %e,
                    "sandbox: could not grant the sandbox account window-object access — a \
                     child on a non-interactive station may fail loader init"
                );
            }
        }
        // Counted even when no grant landed, so the count stays balanced against `Drop`.
        *live.entry(sid.to_string()).or_insert(0) += 1;
        WindowAceGuard {
            sid: sid.to_string(),
            handles,
        }
    }
}

impl Drop for WindowAceGuard {
    fn drop(&mut self) {
        let mut live = WINDOW_ACES.lock().unwrap_or_else(|e| e.into_inner());
        match live.get_mut(&self.sid) {
            // A sibling run still has a child alive under this same SID. Stripping now is the
            // exact bug this guard exists to avoid, so leave the ace and let the last one out
            // remove it.
            Some(n) if *n > 1 => {
                *n -= 1;
                return;
            }
            _ => {
                live.remove(&self.sid);
            }
        }
        for handle in &self.handles {
            if let Err(e) = acl::strip_window_object(*handle, &self.sid) {
                tracing::debug!(
                    error = %e,
                    "sandbox: could not remove the sandbox account's window-object ace — it \
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

#[cfg(test)]
mod tests {
    use super::{GetProcessWindowStation, WindowAceGuard, acl};
    use windows_sys::Win32::Foundation::HANDLE;

    /// Three synthetic account SIDs. `ConvertStringSidToSidW` parses the string form without
    /// resolving it, so an ALLOW ace naming one of these grants nobody anything — the test can
    /// write them onto the caller's real window station and take them off again.
    const SID_A: &str = "S-1-5-21-1111111111-2222222222-3333333333-4001";
    const SID_B: &str = "S-1-5-21-1111111111-2222222222-3333333333-4002";
    const SID_SHARED: &str = "S-1-5-21-1111111111-2222222222-3333333333-4003";

    fn station() -> HANDLE {
        // SAFETY: takes no parameter that can be invalid and returns a handle owned by the
        // system for this process's lifetime.
        unsafe { GetProcessWindowStation().cast::<std::ffi::c_void>() }
    }

    fn has(sid: &str) -> bool {
        acl::window_object_has_sid(station(), sid).unwrap()
    }

    /// Both lost-update shapes concurrent lifecycle scripts hit on the process-global window
    /// station, in one deterministic sequence — no threads and no sleeps, because the defect
    /// is in the ORDER of grant and teardown rather than in their timing.
    ///
    /// Written as a single test on purpose: `cargo test` runs test functions in parallel and
    /// these all write the same object's DACL, so splitting them would put the suite's own
    /// concurrency in the way of reading the result.
    #[test]
    fn a_guard_teardown_leaves_a_concurrent_guards_ace_in_place() {
        assert!(!station().is_null(), "no window station for this process");

        let a = WindowAceGuard::grant(SID_A);
        if !has(SID_A) {
            // This host will not let nub re-acl its own station at all. The guard fails
            // forward by design, so there is no invariant left to check — asserting anything
            // here would be a green with nothing behind it.
            return;
        }
        let b = WindowAceGuard::grant(SID_B);
        assert!(has(SID_B), "the second guard's grant did not land");

        // THE REGRESSION. Teardown used to restore the DACL `a` snapshotted, which predates
        // `b`'s grant — so `b` lost its ace while its child was still in loader init and died
        // `0xC0000142`.
        drop(a);
        assert!(
            has(SID_B),
            "tearing down one guard deleted a concurrent guard's window-station ace"
        );
        assert!(!has(SID_A), "the torn-down guard left its own ace behind");

        drop(b);
        assert!(!has(SID_B), "the last guard left its own ace behind");

        // THE SHARED-SID CASE the dedicated-account backend is in: every run grants the SAME
        // account SID, so a strip on the first drop would revoke an ace a sibling still needs.
        let first = WindowAceGuard::grant(SID_SHARED);
        let second = WindowAceGuard::grant(SID_SHARED);
        drop(first);
        assert!(
            has(SID_SHARED),
            "the first of two guards sharing a SID stripped the ace the second still needs"
        );
        drop(second);
        assert!(!has(SID_SHARED), "the shared ace outlived its last guard");
    }
}

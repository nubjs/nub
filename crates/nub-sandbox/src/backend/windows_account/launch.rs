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
//!   - `lpDesktop` is left NULL deliberately. A non-NULL desktop makes seclogon SKIP its
//!     window-station auto-grant, at which point the child needs an explicit `WinSta0` ace
//!     (including `READ_CONTROL`, without which it HANGS in loader init) plus a session
//!     `BaseNamedObjects` ace. NULL avoids all of it; the cost is no desktop isolation, which
//!     is a hardening follow-up, not a confinement hole.
//!   - `AssignProcessToJobObject` on the resulting child commonly fails `ERROR_NOT_SUPPORTED`:
//!     seclogon already placed it in its own job, and current Windows refuses that nesting
//!     cross-session. The assignment is attempted and its failure reported, never silently
//!     swallowed — whole-tree reap is genuinely weaker here than on the AppContainer path.

use super::{AccountLaunch, AccountNet, acl, account, state};
use crate::backend::windows::launch::{build_command_line, build_env_block, to_wide};
use std::io;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::ExitStatusExt;
use std::process::ExitStatus;
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessWithLogonW, GetExitCodeProcess,
    INFINITE, LOGON_WITH_PROFILE, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};

const ERROR_NOT_SUPPORTED: i32 = 50;
const ERROR_LOGON_FAILURE: i32 = 1326;
const ERROR_SERVICE_DISABLED: i32 = 1058;

/// Strips every ace this run applied, on drop. Ordering matters: declared before the child is
/// spawned but dropped after the wait returns, so a granted path is never revoked out from
/// under a live child. Best-effort — a failed strip leaves an over-permissive ace for a
/// confined account, which the ledger sweep (`nub run --sandbox-clean`) collects later.
struct AceGuard {
    paths: Vec<std::path::PathBuf>,
    sid: String,
}

impl Drop for AceGuard {
    fn drop(&mut self) {
        for p in &self.paths {
            if let Err(e) = acl::strip(p, &self.sid) {
                tracing::debug!(path = %p.display(), error = %e, "sandbox: ace strip failed — left for the ledger sweep");
            }
        }
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
                     setup — re-run the elevated sandbox setup",
                ));
            }
            None => return Err(not_provisioned()),
        }

        // Ledger BEFORE apply: a crash between the two leaves a recorded path whose ace was
        // never written, and stripping an absent ace is a no-op. The reverse order would
        // leave an ace nothing knows about.
        let mut applied = Vec::new();
        let mut guard = AceGuard {
            paths: Vec::new(),
            sid: marker.sid.clone(),
        };
        for (path, access) in self
            .read_grants
            .iter()
            .map(|p| (p, acl::Access::Read))
            .chain(self.write_grants.iter().map(|p| (p, acl::Access::ReadWrite)))
        {
            state::record_acl_path(path)?;
            guard.paths.push(path.clone());
            acl::grant(path, &marker.sid, access)?;
            applied.push(path.clone());
        }
        // Denies go on AFTER the grants so the deny ace is inserted into a DACL that already
        // carries the grant it must outrank — the canonical-order insert has to see both.
        for path in &self.denies {
            state::record_acl_path(path)?;
            guard.paths.push(path.clone());
            acl::deny(path, &marker.sid)?;
        }

        let password = account::load_credential()?;
        let status = self.spawn_and_wait(&marker.account, &password);
        drop(guard);
        status
    }

    fn spawn_and_wait(&self, account_name: &str, password: &str) -> io::Result<ExitStatus> {
        let user_w = to_wide(account_name);
        // "." targets the LOCAL SAM regardless of whether the machine is domain-joined.
        let domain_w = to_wide(".");
        let mut password_w: Vec<u16> = password
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
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
        let mut flags = CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED;
        let env_ptr: *const std::ffi::c_void = match &env_block {
            Some(b) => b.as_ptr().cast(),
            // NULL + LOGON_WITH_PROFILE makes seclogon build the SANDBOX ACCOUNT's own
            // profile environment — isolated USERPROFILE/TEMP/LOCALAPPDATA, machine PATH.
            None => std::ptr::null(),
        };
        let _ = &mut flags;

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
        password_w.fill(0);
        if ok == 0 {
            return Err(map_spawn_error(io::Error::last_os_error(), account_name));
        }

        // Best-effort containment. seclogon has already placed the child in its own job and
        // current Windows refuses cross-session nesting, so this commonly returns
        // ERROR_NOT_SUPPORTED — reported, never presented as success.
        let job = create_kill_on_close_job().ok();
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

        // SAFETY: `pi` handles came from a successful CreateProcessWithLogonW.
        let code = unsafe {
            ResumeThread(pi.hThread);
            if WaitForSingleObject(pi.hProcess, INFINITE) != WAIT_OBJECT_0 {
                let e = io::Error::last_os_error();
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
                if let Some(j) = job {
                    CloseHandle(j);
                }
                return Err(e);
            }
            let mut code: u32 = 0;
            GetExitCodeProcess(pi.hProcess, &mut code);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            code
        };
        if let Some(j) = job {
            // Closing last triggers KILL_ON_JOB_CLOSE for anything still in the tree.
            unsafe { CloseHandle(j) };
        }
        Ok(ExitStatus::from_raw(code))
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

fn create_kill_on_close_job() -> io::Result<HANDLE> {
    // SAFETY: unnamed job with default security.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
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
         on this machine. Run `nub run --sandbox-setup` once from an elevated (Run as \
         administrator) prompt.",
    )
}

/// Turn the two spawn failures a user can actually act on into instructions.
fn map_spawn_error(e: io::Error, account: &str) -> io::Error {
    match e.raw_os_error() {
        Some(ERROR_LOGON_FAILURE) => io::Error::other(format!(
            "sandbox: the stored credential for `{account}` was rejected — the account's \
             password was changed or the account was disabled. Re-run `nub run \
             --sandbox-setup` from an elevated prompt to reprovision it."
        )),
        Some(ERROR_SERVICE_DISABLED) => io::Error::other(
            "sandbox: the Windows Secondary Logon service is disabled, so nub cannot start the \
             sandboxed child under its dedicated account. Enable the `seclogon` service (it \
             may be disabled by group policy).",
        ),
        _ => e,
    }
}

/// Net posture is enforced entirely by the persistent WFP filters installed at setup, keyed
/// on the account SID — there is nothing per-run to do. This exists so the caller's match on
/// [`AccountNet`] is exhaustive at the launch site and a future posture cannot be added
/// without visiting here.
pub(crate) fn net_is_enforced_by_setup(net: AccountNet) -> bool {
    match net {
        AccountNet::ProxyOnly | AccountNet::DenyAll => true,
        AccountNet::UnconfinedButFenced => false,
    }
}

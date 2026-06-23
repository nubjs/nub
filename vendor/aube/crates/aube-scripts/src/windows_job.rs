//! Windows Job Object wrapper for reaping a script's full process tree.
//!
//! Lifecycle scripts launched via cmd.exe routinely shell out to
//! grandchildren (`node-gyp` → `MSBuild` → `node`, `prebuild-install` →
//! `node`, etc.). When the parent `JoinSet` task is aborted on first
//! failure, tokio's `kill_on_drop` calls `TerminateProcess` on the
//! direct shell — but Windows leaves any grandchildren orphaned because
//! they aren't part of the shell's job by default. The user then sees
//! aube exit with an error while node-gyp keeps logging to the console
//! (Discussion #654).
//!
//! Creating a job object with
//! [`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`] and assigning the spawned
//! shell to it makes the kernel terminate every process in the job —
//! parent *and* every descendant — the moment our last handle to the
//! job is closed. Dropping [`JobObject`] is therefore the kill signal,
//! which makes the cleanup safe under task abort, panic, and normal
//! exit alike.

use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::io::RawHandle;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};

/// Owns a Windows Job Object configured with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Drop = kill the whole tree.
pub(crate) struct JobObject {
    handle: HANDLE,
}

// SAFETY: `HANDLE` is `*mut c_void` and not `Send`/`Sync` by default,
// but a job-object handle is a kernel-owned reference — duplicating
// across threads is supported and our use only ever moves the handle
// between a tokio task and its drop. The kernel serializes the FFI
// calls below.
unsafe impl Send for JobObject {}
unsafe impl Sync for JobObject {}

impl JobObject {
    /// Create a private, unnamed job object with kill-on-close set.
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: both args null → kernel returns a fresh private job
        // object handle, or NULL on failure (documented entry point).
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `info` is a stack-local struct zeroed before use;
        // we hand the kernel its exact size so it knows which
        // extension fields are populated.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            let err = io::Error::last_os_error();
            // SAFETY: handle is the one we just created and haven't
            // shared anywhere yet.
            unsafe { CloseHandle(handle) };
            return Err(err);
        }
        Ok(Self { handle })
    }

    /// Attach a freshly-spawned process to the job. The caller must
    /// pass the `raw_handle()` of a live `tokio::process::Child`. Any
    /// processes the child spawns *after* assignment are inherited
    /// into the job automatically — so the spawn-then-assign race
    /// window only matters for the direct shell, and tokio's
    /// `kill_on_drop` already covers that path.
    pub(crate) fn assign(&self, process: RawHandle) -> io::Result<()> {
        // SAFETY: caller upholds that `process` is a live process
        // handle owned by a `Child` we just spawned. The job handle
        // is alive while `self` exists.
        let ok = unsafe { AssignProcessToJobObject(self.handle, process as HANDLE) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        // SAFETY: handle came from `CreateJobObjectW` and is closed
        // exactly once here. With `KILL_ON_JOB_CLOSE` set, the kernel
        // terminates every assigned process before returning.
        unsafe { CloseHandle(self.handle) };
    }
}

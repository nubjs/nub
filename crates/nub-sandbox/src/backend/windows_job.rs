//! Process-tree ownership for Windows launches that do not take a LowBox token.

use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use std::ptr;
use windows_sys::Win32::Foundation::{
    ERROR_NO_MORE_FILES, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};

pub(super) struct Job(OwnedHandle);

impl Job {
    fn new() -> io::Result<Self> {
        // SAFETY: null security/name creates a private, non-inheritable job handle.
        let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful call transferred ownership of this handle.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(raw) });
        // SAFETY: zero is valid for the unused fields; the API receives the exact size.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                raw,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    fn assign(&self, child: &Child) -> io::Result<()> {
        // SAFETY: both handles are owned and live for this call. The child is suspended.
        if unsafe {
            AssignProcessToJobObject(
                self.0.as_raw_handle() as HANDLE,
                child.as_raw_handle() as HANDLE,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

pub(super) fn spawn(command: &mut Command) -> io::Result<(Child, Job)> {
    let job = Job::new()?;
    // Rust's stable Command API cannot supply PROC_THREAD_ATTRIBUTE_JOB_LIST.
    // Keep its argument/stdio handling, but do not let child code run before assignment.
    // These internally constructed plain commands carry no other creation flags.
    command.creation_flags(CREATE_SUSPENDED);
    let mut child = command.spawn()?;
    if let Err(error) = job
        .assign(&child)
        .and_then(|()| resume_initial_thread(&child))
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok((child, job))
}

fn resume_initial_thread(child: &Child) -> io::Result<()> {
    // Command closes the initial thread handle. Before that suspended thread has
    // run, the owned child must have exactly one thread; reject ambiguity rather
    // than resuming an arbitrary thread. Holding Child prevents process-ID reuse.
    // SAFETY: this creates an owned snapshot handle, or INVALID_HANDLE_VALUE.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a successful snapshot handle, now closed by OwnedHandle.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
    // SAFETY: THREADENTRY32 contains integers; dwSize is set as required by ToolHelp.
    let mut entry: THREADENTRY32 = unsafe { zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    let mut thread_id = None;
    // SAFETY: snapshot and the initialized entry buffer remain live throughout iteration.
    let mut ok = unsafe { Thread32First(snapshot.as_raw_handle() as HANDLE, &mut entry) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    while ok != 0 {
        if entry.dwSize as usize
            >= std::mem::offset_of!(THREADENTRY32, th32OwnerProcessID) + size_of::<u32>()
            && entry.th32OwnerProcessID == child.id()
        {
            if thread_id.replace(entry.th32ThreadID).is_some() {
                return Err(io::Error::other("suspended child has more than one thread"));
            }
        }
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        // SAFETY: same live snapshot and valid buffer as Thread32First.
        ok = unsafe { Thread32Next(snapshot.as_raw_handle() as HANDLE, &mut entry) };
    }
    // SAFETY: read the error from the immediately preceding failed Thread32Next.
    let error = unsafe { GetLastError() };
    if error != ERROR_NO_MORE_FILES {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let thread_id =
        thread_id.ok_or_else(|| io::Error::other("suspended child thread not found"))?;
    // SAFETY: the ID came from the still-owned suspended child. Request only resume access.
    let raw = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenThread returned an owned handle.
    let thread = unsafe { OwnedHandle::from_raw_handle(raw) };
    // SAFETY: the job already owns the child; this releases its initial suspend.
    match unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) } {
        1 => Ok(()),
        u32::MAX => Err(io::Error::last_os_error()),
        count => Err(io::Error::other(format!(
            "unexpected initial thread suspend count: {count}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::JobObjects::IsProcessInJob;

    #[test]
    fn plain_child_starts_in_the_owned_job() {
        let (mut child, job) =
            spawn(Command::new("cmd.exe").args(["/d", "/c", "exit", "7"])).unwrap();
        let mut member = 0;
        // SAFETY: both handles and the output pointer are live.
        assert_ne!(
            unsafe {
                IsProcessInJob(
                    child.as_raw_handle() as HANDLE,
                    job.0.as_raw_handle() as HANDLE,
                    &mut member,
                )
            },
            0
        );
        assert_ne!(member, 0);
        assert_eq!(child.wait().unwrap().code(), Some(7));
    }
}

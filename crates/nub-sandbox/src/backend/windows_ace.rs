//! The window-station / desktop ACE machinery the AppContainer backend needs, and nothing else.
//!
//! A LowBox (AppContainer) token reaches a window station and desktop only where their DACLs
//! grant its container SID; `USER32`'s `DllMain` attaches the process to both, so a
//! USER32-importing child (node, git, …) that cannot reach them dies `STATUS_DLL_INIT_FAILED`
//! (0xC0000142) before `main` — an exit code with nothing in it to suggest a sandbox. On an
//! interactive `WinSta0` seclogon auto-grants this, so the failure only appears on a
//! non-interactive station (an SSH/service session, some CI agents); the grant is cheap and
//! unconditional so the jail behaves the same in both.
//!
//! Resurrected verbatim (epic 1.6/3.2) from the dropped `windows_account` module — the
//! privileged dedicated-account tier that was removed with the curated import (epic 0.3), which
//! is where this machinery happened to live. Only the window-object subgraph is kept: the
//! AppContainer path journals [`grant_persistent`] mutations, and the DACL
//! read-modify-write is a SID-keyed strip (never a snapshot restore) so concurrent runs on the
//! process-global station cannot delete each other's still-live aces.

#![cfg(target_os = "windows")]

use std::io;
use std::path::Path;
use std::ptr::null_mut;
#[cfg(test)]
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicBool, Ordering},
};
use std::sync::{Mutex, MutexGuard};
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ACCESS_MODE, ConvertSidToStringSidW, ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
    GetSecurityInfo, NO_MULTIPLE_TRUSTEE, SE_WINDOW_OBJECT, SetEntriesInAclW, SetSecurityInfo,
    TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, AddAce,
    CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
    InitializeAcl, OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, PSID,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    DESKTOP_CREATEMENU, DESKTOP_CREATEWINDOW, DESKTOP_ENUMERATE, DESKTOP_HOOKCONTROL,
    DESKTOP_JOURNALPLAYBACK, DESKTOP_JOURNALRECORD, DESKTOP_READ_CONTROL, DESKTOP_READOBJECTS,
    DESKTOP_SWITCHDESKTOP, DESKTOP_WRITEOBJECTS, GetProcessWindowStation, GetThreadDesktop,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0x00;
const ACCESS_DENIED_ACE_TYPE: u8 = 0x01;
const INHERITED_ACE_FLAG: u8 = 0x10;
const WINDOW_OBJECT: &str = "<window-object>";

// A process has exactly one current window station.  The journal may temporarily borrow it to
// open a desktop in an old station, so every observation and child creation that depends on that
// process-global value must share this in-process lock.  `OperationLock` remains necessary for
// cross-process DACL read-modify-write; it cannot serialize threads in this process.
static WINDOW_STATION_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
#[derive(Clone)]
struct StationSwitchHook {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

#[cfg(test)]
static TEST_STATION_SWITCH_HOOK: Mutex<Option<StationSwitchHook>> = Mutex::new(None);

#[cfg(test)]
static TEST_FORCE_FOREIGN_SESSION_LIVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn station_guard() -> MutexGuard<'static, ()> {
    WINDOW_STATION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
fn after_station_switch() {
    let hook = TEST_STATION_SWITCH_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Some(hook) = hook {
        hook.entered.wait();
        hook.release.wait();
    }
}

fn recorded_session_exists(session_id: u32) -> io::Result<bool> {
    #[cfg(test)]
    if TEST_FORCE_FOREIGN_SESSION_LIVE.load(Ordering::Relaxed) {
        return Ok(true);
    }
    use windows_sys::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTSEnumerateSessionsW, WTSFreeMemory,
    };
    let mut sessions = null_mut();
    let mut count = 0;
    if unsafe { WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut sessions, &mut count) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    let exists = if count == 0 {
        false
    } else {
        unsafe { std::slice::from_raw_parts(sessions, count as usize) }
            .iter()
            .any(|session| session.SessionId == session_id)
    };
    unsafe { WTSFreeMemory(sessions.cast()) };
    Ok(exists)
}

/// `WINSTA_ALL_ACCESS` (0x37F) — the union of the nine `WINSTA_*` rights, spelled here because
/// `windows-sys` exports it only from a feature this crate does not otherwise need.
/// `DESKTOP_READ_CONTROL` is the `READ_CONTROL` bit, LOAD-BEARING on the station: without it the
/// child HANGS in loader init rather than failing, so it is folded in and never optional.
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

/// The string (S-1-…) form of a container SID, needed to key its window-object ace. Shared with
/// the AppContainer backend, which holds the SID as a raw `PSID`.
///
/// # Safety
/// `sid` must point at a valid self-relative SID for the duration of the call.
pub(crate) unsafe fn sid_to_string(sid: PSID) -> io::Result<String> {
    let mut out: *mut u16 = null_mut();
    // SAFETY: caller guarantees `sid`; `out` is a valid slot.
    let ok = unsafe { ConvertSidToStringSidW(sid, &mut out) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut len = 0usize;
    // SAFETY: on success the buffer is NUL-terminated UTF-16 allocated by `LocalAlloc`.
    while unsafe { *out.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` units precede the terminator.
    let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(out, len) });
    // SAFETY: `ConvertSidToStringSidW` documents `LocalFree` as the release.
    unsafe { LocalFree(out.cast()) };
    Ok(s)
}

struct OwnedSid(PSID);

impl OwnedSid {
    fn parse(s: &str) -> io::Result<Self> {
        let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sid: PSID = null_mut();
        // SAFETY: `wide` is NUL-terminated and outlives the call; `sid` is a valid out-slot.
        if unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a valid SID string: {s}"),
            ));
        }
        Ok(OwnedSid(sid))
    }
}

impl Drop for OwnedSid {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from ConvertStringSidToSidW, which documents LocalFree.
        unsafe { LocalFree(self.0) };
    }
}

struct LocalFreeGuard(*mut std::ffi::c_void);

impl Drop for LocalFreeGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: every pointer wrapped here came from an API documenting LocalFree.
            unsafe { LocalFree(self.0) };
        }
    }
}

fn explicit_access(sid: PSID, mask: u32, mode: ACCESS_MODE, inherit: bool) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: mode,
        grfInheritance: if inherit {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            0
        },
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.cast(),
        },
    }
}

/// The trustee SID of an allow/deny ace. Caller must have checked the type — the inline
/// `SidStart` field is only at this offset for the types sharing `ACCESS_ALLOWED_ACE`'s layout.
fn sid_of(ace: *mut std::ffi::c_void) -> PSID {
    // SAFETY: `addr_of!` takes the field address without forming a reference to the
    // variable-length SID that follows it.
    unsafe { std::ptr::addr_of!((*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart) }
        .cast_mut()
        .cast()
}

/// Canonical-order bucket: explicit DENY, explicit ALLOW, explicit other, then inherited.
fn canonical_bucket(ace_type: u8, inherited: bool) -> u8 {
    if inherited {
        return 3;
    }
    match ace_type {
        ACCESS_DENIED_ACE_TYPE => 0,
        ACCESS_ALLOWED_ACE_TYPE => 1,
        _ => 2,
    }
}

/// Walk every ace in `acl` in DACL order. A NULL `acl` means "no DACL, everything allowed".
fn walk_aces(
    acl: *mut ACL,
    path: &Path,
    mut f: impl FnMut(u32, ACE_HEADER, *mut std::ffi::c_void),
) -> io::Result<()> {
    if acl.is_null() {
        return Ok(());
    }
    let mut info = ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    // SAFETY: `acl` is a live ACL; `info` is a correctly sized out-slot for the class.
    let ok = unsafe {
        GetAclInformation(
            acl,
            std::ptr::from_mut(&mut info).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if ok == 0 {
        return Err(win32_last_err("GetAclInformation", path));
    }
    for i in 0..info.AceCount {
        let mut ace: *mut std::ffi::c_void = null_mut();
        // SAFETY: `i` is below the reported AceCount of a live ACL.
        if unsafe { GetAce(acl, i, &mut ace) } == 0 {
            return Err(win32_last_err("GetAce", path));
        }
        // SAFETY: every ace GetAce yields begins with an ACE_HEADER.
        f(i, unsafe { *ace.cast::<ACE_HEADER>() }, ace);
    }
    Ok(())
}

/// Rebuild `existing`'s ace list without any explicit ace for `sid`, in canonical order.
/// `Ok(None)` means nothing matched and the caller must not write anything back.
fn rebuild_without_sid(
    path: &Path,
    existing: *mut ACL,
    sid: &OwnedSid,
) -> io::Result<Option<Vec<u32>>> {
    let mut kept: Vec<(u8, u32, *const std::ffi::c_void, u32)> = Vec::new();
    let mut kept_bytes: u32 = 0;
    let mut dropped = 0usize;

    walk_aces(existing, path, |i, header, ace| {
        let inherited = header.AceFlags & INHERITED_ACE_FLAG != 0;
        let is_ours = !inherited
            && matches!(
                header.AceType,
                ACCESS_ALLOWED_ACE_TYPE | ACCESS_DENIED_ACE_TYPE
            )
            // SAFETY: layout checked above; `SidStart` is the first DWORD of the inline SID.
            && unsafe { EqualSid(sid_of(ace), sid.0) } != 0;

        if is_ours {
            dropped += 1;
            return;
        }
        kept.push((
            canonical_bucket(header.AceType, inherited),
            i,
            ace.cast_const(),
            u32::from(header.AceSize),
        ));
        kept_bytes += u32::from(header.AceSize);
    })?;

    if dropped == 0 {
        return Ok(None);
    }
    if kept.is_empty() {
        tracing::debug!(
            "sandbox: window-object ace strip skipped — our aces are the only ones, and an \
             empty DACL would deny everyone"
        );
        return Ok(None);
    }
    kept.sort_by_key(|&(bucket, index, _, _)| (bucket, index));

    let acl_bytes = (std::mem::size_of::<ACL>() as u32 + kept_bytes).next_multiple_of(4);
    let mut buf: Vec<u32> = vec![0; (acl_bytes as usize).div_ceil(4)];
    let acl = buf.as_mut_ptr().cast::<ACL>();

    // SAFETY: `existing` is live; `acl` points at `acl_bytes` of zeroed, DWORD-aligned space.
    let revision = u32::from(unsafe { (*existing).AclRevision });
    // SAFETY: as above.
    if unsafe { InitializeAcl(acl, acl_bytes, revision) } == 0 {
        return Err(win32_last_err("InitializeAcl", path));
    }
    for &(_, _, ace, size) in &kept {
        // SAFETY: `acl` was sized to hold exactly these aces; each `ace` is a live `size`-byte
        // ace inside the descriptor kept alive across this call.
        if unsafe { AddAce(acl, revision, u32::MAX, ace, size) } == 0 {
            return Err(win32_last_err("AddAce", path));
        }
    }
    Ok(Some(buf))
}

/// A window object's DACL plus the descriptor owning its storage. The descriptor MUST outlive
/// every read of `acl`, which points INTO it.
struct ReadWindowDacl {
    acl: *mut ACL,
    _sd: LocalFreeGuard,
}

impl ReadWindowDacl {
    fn open(handle: HANDLE) -> io::Result<Self> {
        let mut acl: *mut ACL = null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `handle` is a live window-station/desktop handle; every out-param is a valid
        // slot and the unwanted ones are NULL.
        let rc = unsafe {
            GetSecurityInfo(
                handle,
                SE_WINDOW_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &mut acl,
                null_mut(),
                &mut sd,
            )
        };
        if rc != 0 {
            return Err(win32_obj_err("GetSecurityInfo", rc));
        }
        Ok(ReadWindowDacl {
            acl,
            _sd: LocalFreeGuard(sd),
        })
    }
}

fn set_window_dacl(handle: HANDLE, dacl: *const ACL) -> io::Result<()> {
    // SAFETY: `handle` is a live window-station/desktop handle; `dacl` is NULL or a live ACL
    // outliving the call. DACL only — never PROTECTED, so whatever the object inherited stays.
    let rc = unsafe {
        SetSecurityInfo(
            handle,
            SE_WINDOW_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(win32_obj_err("SetSecurityInfo", rc));
    }
    Ok(())
}

/// Add an ALLOW ace for `sid` on a window-station or desktop HANDLE. Not inheritable.
fn grant_window_object(handle: HANDLE, sid: &str, mask: u32) -> io::Result<()> {
    let sid = OwnedSid::parse(sid)?;
    let existing = ReadWindowDacl::open(handle)?;

    // A NULL DACL is UNRESTRICTED access, not an empty allow-set. Merging into it would produce a
    // DACL holding ONLY our ace, which the teardown could not undo; and there is nothing to grant
    // since a NULL DACL already admits the sandbox principal.
    if existing.acl.is_null() {
        tracing::debug!(
            "sandbox: window-object grant skipped — the object has a NULL DACL, so access is \
             already unrestricted"
        );
        return Ok(());
    }

    let ea = explicit_access(sid.0, mask, GRANT_ACCESS, false);
    let mut new_dacl: *mut ACL = null_mut();
    // SAFETY: `ea` and the SID it points at outlive the call; `existing.acl` is non-NULL here.
    let rc = unsafe { SetEntriesInAclW(1, &ea, existing.acl, &mut new_dacl) };
    if rc != 0 {
        return Err(win32_obj_err("SetEntriesInAclW", rc));
    }
    let _guard = LocalFreeGuard(new_dacl.cast());
    set_window_dacl(handle, new_dacl)
}

/// Remove every explicit ace naming `sid` from a window station or desktop, leaving every other
/// ace where it was. A SID-keyed strip (never a snapshot restore): the station is process-global
/// and a concurrent run's ace must survive this teardown.
fn strip_window_object(handle: HANDLE, sid: &str) -> io::Result<()> {
    let sid = OwnedSid::parse(sid)?;
    let read = ReadWindowDacl::open(handle)?;
    let Some(rebuilt) = rebuild_without_sid(Path::new(WINDOW_OBJECT), read.acl, &sid)? else {
        return Ok(());
    };
    set_window_dacl(handle, rebuilt.as_ptr().cast::<ACL>())
}

/// Does this window object's DACL currently carry an ALLOW ace for `sid`? The direct diagnostic
/// question behind `NUB_JAIL_DUMP_POLICY`'s `station_ace=` field.
fn window_object_has_sid(handle: HANDLE, sid: &str) -> io::Result<bool> {
    let sid = OwnedSid::parse(sid)?;
    let read = ReadWindowDacl::open(handle)?;
    let mut found = false;
    walk_aces(read.acl, Path::new(WINDOW_OBJECT), |_i, header, ace| {
        if header.AceType == ACCESS_ALLOWED_ACE_TYPE
            // SAFETY: type checked, so `SidStart` sits at the ACCESS_ALLOWED_ACE offset.
            && unsafe { EqualSid(sid_of(ace), sid.0) } != 0
        {
            found = true;
        }
    })?;
    Ok(found)
}

fn win32_obj_err(op: &str, rc: u32) -> io::Error {
    if rc == ERROR_ACCESS_DENIED {
        return io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{op} on this session's window object: access denied"),
        );
    }
    io::Error::other(format!(
        "{op} on this session's window object failed (Win32 error {rc})"
    ))
}

fn win32_last_err(op: &str, path: &Path) -> io::Error {
    let rc = io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32;
    io::Error::other(format!(
        "{op} on {} failed (Win32 error {rc})",
        path.display()
    ))
}

use super::windows::windows_registry::{OperationLock, WindowObject};

fn object_name(handle: HANDLE) -> io::Result<String> {
    use windows_sys::Win32::System::StationsAndDesktops::{GetUserObjectInformationW, UOI_NAME};
    let mut bytes = 0;
    unsafe {
        GetUserObjectInformationW(handle, UOI_NAME, null_mut(), 0, &mut bytes);
    }
    if bytes == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut name = vec![0u16; (bytes as usize).div_ceil(2)];
    if unsafe {
        GetUserObjectInformationW(
            handle,
            UOI_NAME,
            name.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let len = name
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(name.len());
    Ok(String::from_utf16_lossy(&name[..len]))
}

fn current_objects_unlocked() -> io::Result<Vec<WindowObject>> {
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    let mut session = 0;
    if unsafe { ProcessIdToSessionId(std::process::id(), &mut session) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let station = object_name(unsafe { GetProcessWindowStation() })?;
    let desktop = object_name(unsafe { GetThreadDesktop(GetCurrentThreadId()) })?;
    Ok(vec![
        WindowObject {
            session,
            station: station.clone(),
            desktop: None,
        },
        WindowObject {
            session,
            station,
            desktop: Some(desktop),
        },
    ])
}

pub(crate) fn current_objects() -> io::Result<Vec<WindowObject>> {
    let _station = station_guard();
    current_objects_unlocked()
}

struct WindowHandle {
    raw: HANDLE,
    desktop: bool,
}
impl Drop for WindowHandle {
    fn drop(&mut self) {
        use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, CloseWindowStation};
        unsafe {
            if self.desktop {
                CloseDesktop(self.raw);
            } else {
                CloseWindowStation(self.raw);
            }
        }
    }
}

fn open_recorded(object: &WindowObject) -> io::Result<Option<WindowHandle>> {
    use windows_sys::Win32::System::StationsAndDesktops::{
        OpenDesktopW, OpenWindowStationW, SetProcessWindowStation,
    };
    let _station = station_guard();
    let current = current_objects_unlocked()?;
    if current[0].session != object.session {
        let exists = recorded_session_exists(object.session)?;
        // A logged-off session no longer owns any station or desktop to revoke.
        if !exists {
            return Ok(None);
        }
        // `OpenWindowStationW` resolves names in this process's session.  A live foreign session
        // could contain the same name, so leave its record journaled rather than touching a
        // current-session lookalike.
        return Err(io::Error::other(
            "sandbox window-object cleanup requires its recorded logon session and window station",
        ));
    }
    const READ_CONTROL_WRITE_DAC: u32 = 0x0006_0000;
    let station_name: Vec<u16> = object
        .station
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // A window station has its own namespace for desktops.  A later SSH logon can be in the
    // same terminal-services session but attached to a different Service-0x0-* station, so
    // `OpenDesktopW` must not be pointed at the caller's current station.  Open the exact
    // recorded station first; this cannot resolve a same-named station in another session.
    let station = unsafe { OpenWindowStationW(station_name.as_ptr(), 0, READ_CONTROL_WRITE_DAC) };
    if station.is_null() {
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(2 | 3)) {
            return Ok(None);
        }
        return Err(error);
    }
    let station_guard = WindowHandle {
        raw: station,
        desktop: false,
    };

    if object.desktop.is_none() {
        return Ok(Some(station_guard));
    }

    // `OpenDesktopW` is documented to accept desktops only from the process's current station.
    // Keep the station swap tightly scoped and restore the borrowed current-station handle before
    // returning.  `WINDOW_STATION_LOCK` also covers confined CreateProcessW, so no Nub child can
    // observe this temporary station. Only the explicitly opened recorded station is touched.
    let previous = unsafe { GetProcessWindowStation() };
    if previous.is_null() {
        return Err(io::Error::last_os_error());
    }
    if unsafe { SetProcessWindowStation(station_guard.raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(test)]
    after_station_switch();
    struct RestoreStation {
        previous: HANDLE,
        restored: bool,
    }
    impl RestoreStation {
        fn restore(&mut self) -> io::Result<()> {
            if self.restored {
                return Ok(());
            }
            if unsafe { SetProcessWindowStation(self.previous) } == 0 {
                return Err(io::Error::last_os_error());
            }
            self.restored = true;
            Ok(())
        }
    }
    impl Drop for RestoreStation {
        fn drop(&mut self) {
            // Try to repair an early-error path. The normal return path calls `restore` explicitly
            // and propagates its error, so a failed restore cannot retire the journal as success.
            if !self.restored && self.restore().is_err() {
                tracing::error!(error = ?io::Error::last_os_error(), "sandbox failed to restore its window station after cleanup");
            }
        }
    }
    let mut restore = RestoreStation {
        previous,
        restored: false,
    };
    let desktop_name: Vec<u16> = object
        .desktop
        .as_deref()
        .expect("desktop case checked above")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // Microsoft documents that standard security access on a desktop also requires both object
    // access bits.  Request them even though cleanup only reads/writes the descriptor.
    let raw = unsafe {
        OpenDesktopW(
            desktop_name.as_ptr(),
            0,
            0,
            READ_CONTROL_WRITE_DAC | DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS,
        )
    };
    if raw.is_null() {
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(2 | 3)) {
            return Ok(None);
        }
        return Err(error);
    }
    // Restoration is a cleanup precondition, not diagnostics. Do this before handing the desktop
    // back to the DACL caller so `RecoveryNeeded` retains the journal on failure.
    restore.restore()?;
    Ok(Some(WindowHandle {
        raw,
        desktop: object.desktop.is_some(),
    }))
}

/// The registry journals the object before this mutation and owns the ACE until
/// eviction. A command finishing must not revoke another process's shared SID.
pub(crate) fn grant_persistent(object: &WindowObject, sid: PSID) -> io::Result<()> {
    let _lock = OperationLock::acquire("acl")?;
    let handle = open_recorded(object)?
        .ok_or_else(|| io::Error::other("sandbox window object disappeared"))?;
    let sid = unsafe { sid_to_string(sid) }?;
    if window_object_has_sid(handle.raw, &sid)? {
        return Ok(());
    }
    grant_window_object(
        handle.raw,
        &sid,
        if handle.desktop {
            DESKTOP_GRANT
        } else {
            WINSTA_GRANT
        },
    )
}

pub(crate) fn revoke_persistent(object: &WindowObject, sid: PSID) -> io::Result<()> {
    let _lock = OperationLock::acquire("acl")?;
    let Some(handle) = open_recorded(object)? else {
        return Ok(());
    };
    let sid = unsafe { sid_to_string(sid) }?;
    strip_window_object(handle.raw, &sid)
}

/// Exercise the recovery path against a real, non-current station and desktop.  It runs only in
/// the isolated lifecycle fixture: changing a process's current station is necessarily global to
/// that process, even though the production recovery swap is immediately restored.
#[cfg(test)]
pub(crate) fn test_revoke_from_noncurrent_station() -> io::Result<()> {
    use windows_sys::Win32::System::StationsAndDesktops::{
        CreateDesktopW, CreateWindowStationW, SetProcessWindowStation,
    };

    const WINSTA_ALL_ACCESS: u32 = 0x000F_037F;
    const DESKTOP_ALL_ACCESS: u32 = 0x000F_01FF;

    let previous = unsafe { GetProcessWindowStation() };
    if previous.is_null() {
        return Err(io::Error::last_os_error());
    }
    let station = unsafe { CreateWindowStationW(null_mut(), 0, WINSTA_ALL_ACCESS, null_mut()) };
    if station.is_null() {
        return Err(io::Error::last_os_error());
    }
    let station_guard = WindowHandle {
        raw: station,
        desktop: false,
    };
    let station_name = object_name(station_guard.raw)?;
    if unsafe { SetProcessWindowStation(station_guard.raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let desktop_name = format!("nub-recovery-{}", std::process::id());
    let desktop_wide: Vec<u16> = desktop_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let desktop = unsafe {
        CreateDesktopW(
            desktop_wide.as_ptr(),
            null_mut(),
            null_mut(),
            0,
            DESKTOP_ALL_ACCESS,
            null_mut(),
        )
    };
    // `CreateWindowStationW` connects the process to the new station.  Restore before calling the
    // recovery API so this is the exact successive-logon shape being guarded.
    if unsafe { SetProcessWindowStation(previous) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if desktop.is_null() {
        return Err(io::Error::last_os_error());
    }
    let desktop_guard = WindowHandle {
        raw: desktop,
        desktop: true,
    };
    let sid = OwnedSid::parse("S-1-15-2-1")?;
    let station_object = WindowObject {
        session: current_objects()?[0].session,
        station: station_name,
        desktop: None,
    };
    let desktop_object = WindowObject {
        desktop: Some(desktop_name),
        ..station_object.clone()
    };

    grant_persistent(&station_object, sid.0)?;
    grant_persistent(&desktop_object, sid.0)?;
    // A journal entry owns only its AppContainer SID.  The recovery must retain this independent
    // principal even though it shares both old objects with the journaled grant.
    grant_window_object(station_guard.raw, "S-1-15-2-2", WINSTA_GRANT)?;
    grant_window_object(desktop_guard.raw, "S-1-15-2-2", DESKTOP_GRANT)?;

    // A live foreign terminal-services session cannot be named from this process.  Force that
    // classification around a colliding current-session name and prove cleanup returns an error
    // before opening or stripping either local object.
    let foreign_session_object = WindowObject {
        session: if station_object.session == 0 { 1 } else { 0 },
        ..station_object.clone()
    };
    TEST_FORCE_FOREIGN_SESSION_LIVE.store(true, Ordering::Relaxed);
    let foreign_result = revoke_persistent(&foreign_session_object, sid.0);
    TEST_FORCE_FOREIGN_SESSION_LIVE.store(false, Ordering::Relaxed);
    if foreign_result.is_ok()
        || !window_object_has_sid(station_guard.raw, "S-1-15-2-1")?
        || !window_object_has_sid(desktop_guard.raw, "S-1-15-2-1")?
    {
        return Err(io::Error::other(
            "foreign-session cleanup reached a current-session name collision",
        ));
    }

    // Hold recovery immediately after the process-wide switch. A competing station observation
    // cannot finish until the desktop opener restores the original station and releases the same
    // in-process mutex that confined CreateProcessW uses.
    let before_station = current_objects()?[0].station.clone();
    let hook = StationSwitchHook {
        entered: Arc::new(Barrier::new(2)),
        release: Arc::new(Barrier::new(2)),
    };
    *TEST_STATION_SWITCH_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(hook.clone());
    let (recovered_tx, recovered_rx) = std::sync::mpsc::channel();
    let desktop_for_recovery = desktop_object.clone();
    let recovery = std::thread::spawn(move || {
        let sid = OwnedSid::parse("S-1-15-2-1");
        let result = sid.and_then(|sid| revoke_persistent(&desktop_for_recovery, sid.0));
        let _ = recovered_tx.send(result);
    });
    hook.entered.wait();
    let (observer_started_tx, observer_started_rx) = std::sync::mpsc::channel();
    let (observed_tx, observed_rx) = std::sync::mpsc::channel();
    let observer = std::thread::spawn(move || {
        let _ = observer_started_tx.send(());
        let _ = observed_tx.send(current_objects());
    });
    observer_started_rx
        .recv()
        .map_err(|_| io::Error::other("station observer did not start"))?;
    let observation_blocked = observed_rx
        .recv_timeout(std::time::Duration::from_millis(100))
        .is_err();
    hook.release.wait();
    let recovered = recovered_rx
        .recv()
        .map_err(|_| io::Error::other("station recovery worker exited without a result"))?;
    recovery
        .join()
        .map_err(|_| io::Error::other("station recovery worker panicked"))?;
    *TEST_STATION_SWITCH_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    let observed = observed_rx
        .recv()
        .map_err(|_| io::Error::other("station observer exited without a result"))??;
    observer
        .join()
        .map_err(|_| io::Error::other("station observer panicked"))?;
    if !observation_blocked || recovered.is_err() || observed[0].station != before_station {
        return Err(io::Error::other(
            "station recovery did not block observation and restore the caller station",
        ));
    }
    revoke_persistent(&station_object, sid.0)?;

    // Both lookups force the recovery machinery to target the old station, not the fixture's
    // current one.  The original handles remain open solely to keep the test objects alive.
    let station = open_recorded(&station_object)?.expect("test station disappeared");
    let desktop = open_recorded(&desktop_object)?.expect("test desktop disappeared");
    let station_removed = !window_object_has_sid(station.raw, "S-1-15-2-1")?;
    let desktop_removed = !window_object_has_sid(desktop.raw, "S-1-15-2-1")?;
    let foreign_station_retained = window_object_has_sid(station.raw, "S-1-15-2-2")?;
    let foreign_desktop_retained = window_object_has_sid(desktop.raw, "S-1-15-2-2")?;
    drop(desktop);
    drop(station);
    drop(desktop_guard);
    drop(station_guard);
    if station_removed && desktop_removed && foreign_station_retained && foreign_desktop_retained {
        Ok(())
    } else {
        Err(io::Error::other(
            "cleanup did not strip only its ACE from non-current window objects",
        ))
    }
}

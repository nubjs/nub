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
//! AppContainer path calls exactly [`WindowAceGuard::grant`] + [`sid_to_string`], and the DACL
//! read-modify-write is a SID-keyed strip (never a snapshot restore) so concurrent runs on the
//! process-global station cannot delete each other's still-live aces.

#![cfg(target_os = "windows")]

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::ptr::null_mut;
use std::sync::Mutex;
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
    io::Error::other(format!("{op} on {} failed (Win32 error {rc})", path.display()))
}

/// Every window-object grant this process holds, keyed by SID, with the number of live guards
/// behind each. The Mutex serializes the DACL read-modify-write itself, not just the bookkeeping,
/// so two runs granting the process-global station cannot lost-update each other. The refcount
/// orders the shared-SID case; the AppContainer backend mints a fresh container SID per run, so
/// only the last live guard for a SID strips it.
static WINDOW_ACES: Mutex<BTreeMap<String, usize>> = Mutex::new(BTreeMap::new());

/// Grants this run's container SID access to the process window station and thread desktop, and
/// strips exactly its own aces on drop. See the module doc for why this is load-bearing on a
/// non-interactive station. Fails FORWARD: a station whose DACL nub cannot rewrite still launches.
pub(crate) struct WindowAceGuard {
    sid: String,
    handles: Vec<HANDLE>,
}

impl WindowAceGuard {
    /// The station/desktop ace state for this guard's SID, read from the DACLs as they stand,
    /// plus how many guards are live across every SID. Printed beside a child's exit code under
    /// `NUB_JAIL_DUMP_POLICY` — `station_ace=false` next to `code=3221225794` names the fault.
    pub(crate) fn probe(&self) -> String {
        let total: usize = {
            let live = WINDOW_ACES.lock().unwrap_or_else(|e| e.into_inner());
            live.values().sum()
        };
        let mut out = format!("live={total}");
        for (i, handle) in self.handles.iter().enumerate() {
            let label = if i == 0 { "station" } else { "desktop" };
            match window_object_has_sid(*handle, &self.sid) {
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
        // Poison-tolerant: a panicking holder leaves the map consistent, and refusing to grant
        // here would cost a run for nothing.
        let mut live = WINDOW_ACES.lock().unwrap_or_else(|e| e.into_inner());
        let mut handles = Vec::with_capacity(2);
        for (handle, mask) in [(station, WINSTA_GRANT), (desktop, DESKTOP_GRANT)] {
            if handle.is_null() {
                continue;
            }
            handles.push(handle);
            if let Err(e) = grant_window_object(handle, sid, mask) {
                tracing::debug!(
                    error = %e,
                    "sandbox: could not grant the container SID window-object access — a child \
                     on a non-interactive station may fail loader init"
                );
            }
        }
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
            // exact bug this guard exists to avoid, so leave the ace for the last one out.
            Some(n) if *n > 1 => {
                *n -= 1;
                return;
            }
            _ => {
                live.remove(&self.sid);
            }
        }
        for handle in &self.handles {
            if let Err(e) = strip_window_object(*handle, &self.sid) {
                tracing::debug!(
                    error = %e,
                    "sandbox: could not remove the container SID's window-object ace — it keeps \
                     station access until this session ends"
                );
            }
        }
    }
}

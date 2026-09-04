//! Every DACL write the account backend makes: explicit ACEs keyed on the sandbox SID for the
//! policy's paths, plus the two non-filesystem objects a launch touches (the caller's window
//! station and desktop) and the lock on nub's own credential store.
//!
//! ON USER PATHS THE MODEL IS PURELY ADDITIVE. The sandbox account is a DIFFERENT local
//! principal, so every path it was never granted is already unreachable — the invoking user's
//! profile needs no ACE authored at all. [`grant`]/[`deny`]/[`strip`] therefore only ever ADD
//! ACEs for the sandbox SID and later remove exactly those. They never rewrite, protect, or
//! snapshot a user path's descriptor, which is why there is no crash journal here and nothing
//! to restore after a hard kill beyond [`strip`]. (The abandoned deny-strip design —
//! `SE_DACL_PROTECTED` plus a DACL restore journal — is why that distinction is worth stating;
//! see [`super`].) [`lock_to_admins`] is the ONE protected write, and it lands on nub's own
//! state directory, never a user path.
//!
//! CANONICAL DACL ORDER IS THE WHOLE MECHANISM. Windows resolves an access check first-match
//! over the DACL and orders ACEs explicit-DENY → explicit-ALLOW → explicit-other → inherited
//! (any type). Because *explicit* always precedes *inherited*, a DENY written directly onto
//! `<project>/.env` outranks the ALLOW that `<project>`'s `(OI)(CI)` grant PROPAGATED onto it
//! as an inherited ACE. That is exactly the deny-inside-allow the AppContainer backend cannot
//! express. The hazard: a non-canonical order is ACCEPTED by Windows and still resolves
//! first-match, so a misplaced DENY silently resolves as ALLOW — a security failure with no
//! error anywhere. `SetEntriesInAclW` canonicalizes on insert and [`strip`] canonicalizes by
//! hand, and `deny_inside_a_grant_lands_before_the_inherited_allow` pins the result rather
//! than trusting either.
//!
//! ON A USER PATH, INHERITANCE IS DELIBERATELY LEFT UNPROTECTED. Every write there passes
//! `DACL_SECURITY_INFORMATION` alone, never `PROTECTED_DACL_SECURITY_INFORMATION`: the
//! invoking user's own inherited access must survive so they can still read what the sandbox
//! child creates inside a granted tree.

#![cfg(target_os = "windows")]

use std::io;
use std::path::{Component, Path, PathBuf};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ACCESS_MODE, ConvertStringSidToSidW, DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS,
    GetNamedSecurityInfoW, GetSecurityInfo, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SE_WINDOW_OBJECT,
    SetEntriesInAclW, SetNamedSecurityInfoW, SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, AddAce,
    CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
    GetTokenInformation, InitializeAcl, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
// `FILE_TRAVERSE` is the SAME bit as `FILE_EXECUTE` — the kernel reads it as traverse on a
// directory and as execute on a file, and no primitive separates them. It is imported under
// the traverse spelling because that is the property the mask assertions below are about.
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ALL_ACCESS, FILE_DELETE_CHILD, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_TRAVERSE, WRITE_DAC, WRITE_OWNER,
};

/// Read + write + execute + delete. What a policy's write grant stamps.
const RW: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;

/// Read + execute. What a policy's read grant stamps.
const RO: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;

// What these masks LEAVE OUT is the security property, and an omission is invisible on
// inspection — so each exclusion is asserted at compile time, against windows-sys' own values.
const _: () = {
    // `FILE_DELETE_CHILD` on a granted PARENT is checked INSTEAD OF `DELETE` on the child, so
    // including it would let the account delete a file carrying a full deny ACE. That single
    // bit voids every deny-inside-allow rule the policy can express.
    assert!(RW & FILE_DELETE_CHILD == 0);
    // Either bit lets the confined account rewrite its own confinement.
    assert!(RW & (WRITE_DAC | WRITE_OWNER) == 0);
    assert!(RO & (WRITE_DAC | WRITE_OWNER) == 0);
    // The ladder must nest, or a read grant could reach something a write grant cannot.
    assert!(RW & RO == RO);
    // Without traverse, `SetCurrentDirectoryW` into a granted directory fails
    // ERROR_ACCESS_DENIED — which is why `FILE_GENERIC_EXECUTE` is in BOTH masks.
    assert!(RO & FILE_TRAVERSE != 0);
};

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0x00;
const ACCESS_DENIED_ACE_TYPE: u8 = 0x01;
const INHERITED_ACE_FLAG: u8 = 0x10;

/// `NT AUTHORITY\SYSTEM` and `BUILTIN\Administrators`, by well-known SID because the NAMES are
/// localized ("Administratoren", "Administrateurs") and the SIDs are not.
const SID_LOCAL_SYSTEM: &str = "S-1-5-18";
const SID_BUILTIN_ADMINISTRATORS: &str = "S-1-5-32-544";

/// What a grant hands the sandbox account on a subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Read,
    ReadWrite,
}

impl Access {
    fn mask(self) -> u32 {
        match self {
            Access::Read => RO,
            Access::ReadWrite => RW,
        }
    }
}

/// Add an inheritable ALLOW ace for `sid` on `path`.
///
/// Inheritance is applied only on a directory: `(OI)(CI)` on a leaf file is meaningless and
/// Windows would reject or silently strip it.
pub(crate) fn grant(path: &Path, sid: &str, access: Access) -> io::Result<()> {
    let (target, is_dir) = resolve(path)?;
    let sid = OwnedSid::parse(sid)?;
    add_ace(&target, &sid, access.mask(), GRANT_ACCESS, is_dir)
}

/// Add an inheritable DENY ace for `sid` on `path`, plus the parent-side carve that stops the
/// account deleting or renaming `path` through its parent directory.
///
/// Stamps exactly the paths [`deny_targets`] reports, which the caller must have recorded in
/// the ledger first — the parent carve is a SECOND ace on a SECOND object, and one that is
/// never stripped outlives every run as a permanent explicit DENY for the sandbox SID.
pub(crate) fn deny(path: &Path, sid: &str) -> io::Result<()> {
    let (target, is_dir) = resolve(path)?;
    let sid = OwnedSid::parse(sid)?;
    add_ace(&target, &sid, FILE_ALL_ACCESS, DENY_ACCESS, is_dir)?;

    // The counterpart to excluding `FILE_DELETE_CHILD` from the grant masks: that exclusion
    // only covers rights WE stamp, while the account may hold the bit on the parent through
    // an inherited `BUILTIN\Users` ACE it picks up as a local user. Denying it explicitly on
    // the parent closes `del`/`ren` of the denied target. NOT inheritable — the check that
    // matters is against the parent directory object itself, so propagating it down would
    // confine sibling subtrees for no gain. (SRT applies the same carve with `(OI)(CI)`.)
    let Some(parent) = carve_parent(&target) else {
        return Ok(());
    };
    add_ace(&parent, &sid, FILE_DELETE_CHILD, DENY_ACCESS, false)
}

/// Every path a [`deny`] on `path` will stamp: the resolved target, and the parent carrying
/// its `FILE_DELETE_CHILD` carve when there is one.
///
/// Exposed rather than merely returned by [`deny`] so the caller can honor the ledger's
/// record-BEFORE-apply rule for both aces — recording after the fact would leave the parent
/// carve unrecorded across the window a crash can land in.
pub(crate) fn deny_targets(path: &Path) -> io::Result<Vec<PathBuf>> {
    let (target, _) = resolve(path)?;
    let mut out = Vec::with_capacity(2);
    out.extend(carve_parent(&target));
    out.push(target);
    Ok(out)
}

/// The directory `target`'s delete-through carve belongs on, or `None` when there is none.
///
/// `Path::parent` on the extended-length spellings [`resolve`] produces returns the VOLUME
/// ROOT for a top-level target (`\\?\C:\secrets.env` → `\\?\C:\`), which must not be ACE'd:
/// unelevated that fails the whole run with a misleading error, and where the caller does hold
/// `WRITE_DAC` it writes a permanent explicit DENY onto the drive itself. The accepted cost is
/// that a deny on a volume-root child gets no delete-through carve — `BUILTIN\Users` holds no
/// `FILE_DELETE_CHILD` on a default `C:\`, so the account has nothing to carve away.
fn carve_parent(target: &Path) -> Option<PathBuf> {
    target
        .parent()
        .filter(|p| !is_volume_root(p))
        .map(Path::to_path_buf)
}

/// Whether `p` names a volume root — a prefix and its root separator with no named component
/// under it. Covers both forms [`resolve`] emits: `\\?\C:\` (`Prefix` + `RootDir`) and
/// `\\?\UNC\server\share\`, where the share itself is part of the prefix.
fn is_volume_root(p: &Path) -> bool {
    p.components()
        .all(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
}

/// Take ownership of `dir` and replace its DACL with a PROTECTED one naming ONLY SYSTEM,
/// `BUILTIN\Administrators` and the calling user — the lock on nub's own sandbox state
/// directory.
///
/// PROTECTED IS CORRECT HERE AND NOWHERE ELSE IN THIS MODULE. `%PROGRAMDATA%\nub\sandbox`
/// inherits ProgramData's `BUILTIN\Users:(RX)`, and it holds the DPAPI credential — whose
/// machine scope is explicitly NOT a boundary, so any local user who can READ the ciphertext
/// can decrypt it and then hold the sandbox account's password. Blocking that inheritance is
/// the entire boundary; an additive deny cannot express it. This is nub's own state directory,
/// so the module doc's unprotected rule (which exists to preserve a USER's inherited access on
/// THEIR files) does not apply.
///
/// THE OWNER IS RESET TOO, AND THAT HALF IS NOT OPTIONAL. An object's owner holds implicit
/// `READ_CONTROL | WRITE_DAC` whatever the DACL says, and `%PROGRAMDATA%` grants
/// `BUILTIN\Users:(CI)(AD)` — so a standard user can pre-create this directory before setup
/// has ever run, stay its owner through the lock, then rewrite the DACL back and read the
/// credential. Handing ownership to `BUILTIN\Administrators` is what closes that; a protected
/// DACL on its own does not. (Same pre-create-and-own primitive the marker's identity check
/// defends against — see [`super::state::Marker`].)
///
/// The calling user is named so the provisioning administrator's later UNELEVATED runs can
/// still read the marker and append the ledger. Consequence, deliberate: when setup is
/// elevated with a DIFFERENT admin account (over-the-shoulder UAC), the ordinary user is not
/// named and their runs fail closed as "not provisioned".
pub(crate) fn lock_to_admins(dir: &Path) -> io::Result<()> {
    let (target, _) = resolve(dir)?;
    let system = OwnedSid::parse(SID_LOCAL_SYSTEM)?;
    let admins = OwnedSid::parse(SID_BUILTIN_ADMINISTRATORS)?;
    let token_user = current_token_user()?;
    // SAFETY: the block holds one `TOKEN_USER` whose `User.Sid` points inside it, and it
    // outlives every use of the pointer below.
    let me = unsafe { (*token_user.as_ptr().cast::<TOKEN_USER>()).User.Sid };

    let entries: Vec<EXPLICIT_ACCESS_W> = [system.0, admins.0, me]
        .into_iter()
        .map(|sid| explicit_access(sid, FILE_ALL_ACCESS, GRANT_ACCESS, true))
        .collect();

    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // A NULL "old ACL" is the point: the result is built from these three entries ALONE, so
    // nothing pre-existing and nothing inherited survives into it.
    // SAFETY: every entry and the SIDs they point at outlive the call; `new_dacl` is a valid
    // out-slot.
    let rc = unsafe {
        SetEntriesInAclW(
            entries.len() as u32,
            entries.as_ptr(),
            std::ptr::null_mut(),
            &mut new_dacl,
        )
    };
    if rc != 0 {
        return Err(win32_err("SetEntriesInAclW", &target, rc));
    }
    let _guard = LocalFreeGuard(new_dacl.cast());

    let wpath = wide(&target);
    // SAFETY: `new_dacl` is a live ACL from SetEntriesInAclW, `admins` outlives the call, and
    // `wpath` is NUL-terminated UTF-16. An elevated token carries `BUILTIN\Administrators` as
    // a valid owner, so the OWNER write needs no extra privilege.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION,
            admins.0,
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(win32_err("SetNamedSecurityInfoW", &target, rc));
    }
    Ok(())
}

/// The calling process's `TOKEN_USER` block. Returned as its raw backing buffer because the
/// SID inside it is only valid while that buffer lives. `u64`-backed because `TOKEN_USER`'s
/// first field is a `PSID` POINTER — 8-aligned on x64, which a `Vec<u32>` does not promise.
fn current_token_user() -> io::Result<Vec<u64>> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: query-only handle onto this process's own token.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let out = read_token_user(token);
    // SAFETY: `token` came from a successful OpenProcessToken and is closed exactly once.
    unsafe { CloseHandle(token) };
    out
}

fn read_token_user(token: HANDLE) -> io::Result<Vec<u64>> {
    let mut len: u32 = 0;
    // SAFETY: the documented sizing form — NULL buffer, zero length; it fails and sets `len`.
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len) };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf: Vec<u64> = vec![0; (len as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `len` bytes and outlives the call.
    let ok =
        unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buf)
}

// ── window station + desktop: the launch's other securable objects ──────────────

/// Neither the window station nor the desktop has a named-path form, so the pseudo-path the
/// shared ace machinery reports errors against names the object class instead.
const WINDOW_OBJECT: &str = "<window-object>";

/// Add an ALLOW ace for `sid` on a window-station or desktop HANDLE.
///
/// These are `SE_WINDOW_OBJECT`s — handle-addressed, with no named-path form — so this is the
/// `Get`/`SetSecurityInfo` twin of [`add_ace`] rather than a variant of it. The ace is NOT
/// inheritable: the caller aces the desktop directly rather than relying on propagation from
/// the station.
///
/// NOTHING IS SNAPSHOTTED HERE, DELIBERATELY. The station and desktop are process-global and
/// concurrent runs ace them at the same time, so a per-run snapshot is stale the moment a
/// sibling grants (see [`super::launch::WindowAceGuard`]). Teardown is
/// [`strip_window_object`], which re-reads the DACL as it stands and removes only this SID.
pub(crate) fn grant_window_object(handle: HANDLE, sid: &str, mask: u32) -> io::Result<()> {
    let sid = OwnedSid::parse(sid)?;
    let existing = ReadWindowDacl::open(handle)?;

    // A NULL DACL is UNRESTRICTED access, not an empty allow-set — the same trap [`add_ace`]
    // guards on a file path. Merging into it would produce a DACL holding ONLY our ace, and
    // the teardown could not undo that: [`rebuild_without_sid`] refuses to write an empty
    // DACL, so the station would keep a lockout nothing removes. There is also nothing to
    // grant, since a NULL DACL already admits the sandbox principal.
    if existing.acl.is_null() {
        tracing::debug!(
            "sandbox: window-object grant skipped — the object has a NULL DACL, so access is \
             already unrestricted"
        );
        return Ok(());
    }

    let ea = explicit_access(sid.0, mask, GRANT_ACCESS, false);
    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `ea` and the SID it points at outlive the call; `existing.acl` came from
    // GetSecurityInfo and is non-NULL by the check above.
    let rc = unsafe { SetEntriesInAclW(1, &ea, existing.acl, &mut new_dacl) };
    if rc != 0 {
        return Err(win32_obj_err("SetEntriesInAclW", rc));
    }
    let _guard = LocalFreeGuard(new_dacl.cast());
    set_window_dacl(handle, new_dacl)
}

/// Remove every explicit ace naming `sid` from a window station or desktop, leaving every
/// other ace — including a concurrent run's — exactly where it was. The `SE_WINDOW_OBJECT`
/// twin of [`strip`], sharing its hand-rolled rebuild for the same reason: on Windows 11
/// `SetEntriesInAclW(REVOKE_ACCESS)` cannot be trusted to remove an ace.
///
/// This REPLACED a byte-exact restore of the DACL each run found. The restore was correct in
/// isolation and wrong under `child_concurrency`: the DACL a run snapshots predates every
/// sibling that granted after it, so putting it back deletes a live sibling's ace and its
/// child dies in loader init (see [`super::launch::WindowAceGuard`]). A SID-keyed strip has
/// no snapshot to go stale. Its own hazard — the dedicated-account backend grants ONE shared
/// account SID for every run, so the first teardown would strip an ace the others still need
/// — is handled by the refcount in that guard, not here.
///
/// ⛔ ACCEPTED DELTA: THIS REMOVES AN EXPLICIT DENY NUB DID NOT AUTHOR. [`rebuild_without_sid`]
/// matches `ACCESS_ALLOWED_ACE_TYPE | ACCESS_DENIED_ACE_TYPE`, so an administrator's explicit
/// DENY naming the sandbox SID on the caller's own station goes with the strip, where the old
/// snapshot would have put it back. Two things bound it, and neither is the project's usual
/// prefer-over-grant rule — that rule is about not breaking packages, and this is about
/// overriding machine-wide policy on a shared object, which needs its own argument:
///
///   - The BUILD JAIL CANNOT REACH IT. AppContainer mints a fresh container SID per run, so no
///     admin can have authored a DENY for a trustee that did not exist until the run started.
///     The delta exists only for the dedicated-account backend, whose account SID is stable.
///   - Narrowing it here is the wrong trade. [`rebuild_without_sid`] is shared with the
///     filesystem [`strip`] path, so an allow-only variant would change behaviour well outside
///     this defect, for a case the build jail cannot hit. Documented rather than fixed.
///
/// Stated plainly for whoever reads this next: on the dedicated-account backend nub removes an
/// explicit DENY it did not write, so an admin who set one to keep the sandbox account off a
/// window station would find it gone.
pub(crate) fn strip_window_object(handle: HANDLE, sid: &str) -> io::Result<()> {
    let sid = OwnedSid::parse(sid)?;
    let read = ReadWindowDacl::open(handle)?;
    let Some(rebuilt) = rebuild_without_sid(Path::new(WINDOW_OBJECT), read.acl, &sid)? else {
        // Nothing named this SID, or our aces were the only ones on the object. Writing
        // anything back would be a no-op at best and an empty deny-everyone DACL at worst.
        return Ok(());
    };
    set_window_dacl(handle, rebuilt.as_ptr().cast::<ACL>())
}

/// Does this window object's DACL currently carry an ALLOW ace for `sid`? The direct question
/// the guard's regression test asks — "is a concurrent run's ace still there?" — rather than
/// inferring it from a child's exit code.
///
/// Compiled outside `cfg(test)` too, because the same question is the one worth asking of a
/// REAL confined child: [`super::launch::WindowAceGuard::probe`] answers it at the moment the
/// child exits, and `NUB_JAIL_DUMP_POLICY` prints it beside the exit code. A `false` next to
/// `0xC0000142` is the whole diagnosis; without it the exit code alone reads as a broken
/// package, which is how this defect stayed invisible across three platform sweeps.
pub(crate) fn window_object_has_sid(handle: HANDLE, sid: &str) -> io::Result<bool> {
    let sid = OwnedSid::parse(sid)?;
    let read = ReadWindowDacl::open(handle)?;
    let mut found = false;
    walk_aces(read.acl, Path::new(WINDOW_OBJECT), |_i, header, ace| {
        if header.AceType == ACCESS_ALLOWED_ACE_TYPE
            // SAFETY: type checked above, so `SidStart` sits at the ACCESS_ALLOWED_ACE offset.
            && unsafe { EqualSid(sid_of(ace), sid.0) } != 0
        {
            found = true;
        }
    })?;
    Ok(found)
}

fn set_window_dacl(handle: HANDLE, dacl: *const ACL) -> io::Result<()> {
    // SAFETY: `handle` is a live window-station/desktop handle owned by this process; `dacl`
    // is NULL or a live ACL outliving the call. DACL only — never PROTECTED, so whatever the
    // object inherited stays.
    let rc = unsafe {
        SetSecurityInfo(
            handle,
            SE_WINDOW_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(win32_obj_err("SetSecurityInfo", rc));
    }
    Ok(())
}

/// A window object's DACL plus the descriptor owning its storage — the `SE_WINDOW_OBJECT` twin
/// of [`ReadDacl`]. The descriptor MUST outlive every read of `acl`, which points INTO it.
struct ReadWindowDacl {
    acl: *mut ACL,
    _sd: LocalFreeGuard,
}

impl ReadWindowDacl {
    fn open(handle: HANDLE) -> io::Result<Self> {
        let mut acl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `handle` is a live window-station/desktop handle; every out-param is a valid
        // slot and the unwanted ones are NULL.
        let rc = unsafe {
            GetSecurityInfo(
                handle,
                SE_WINDOW_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut acl,
                std::ptr::null_mut(),
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

/// The handle-addressed counterpart to [`win32_err`]: a window object has no path to name, and
/// access-denied here means the caller cannot re-ACL its own station rather than anything
/// about a file.
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

/// Remove EVERY explicit ace whose trustee is `sid`, preserving all other explicit aces
/// verbatim and leaving inherited aces alone. Idempotent; safe on a path with no such ace.
///
/// This deliberately does NOT use `SetEntriesInAclW(REVOKE_ACCESS)`: on Windows 11 25H2 that
/// fails to remove explicit `ACCESS_DENIED` aces (field-observed, and regression-tested as
/// MXC's `deny_round_trip_leaves_no_residue`). The documented behavior and the observed
/// behavior disagree, so the DACL is rebuilt by hand instead.
pub(crate) fn strip(path: &Path, sid: &str) -> io::Result<()> {
    let (target, _) = resolve(path)?;
    let sid = OwnedSid::parse(sid)?;

    let dacl = ReadDacl::open(&target)?;
    let Some(rebuilt) = rebuild_without_sid(&target, dacl.acl, &sid)? else {
        // Nothing matched. Returning early is not just an optimization: writing a rebuilt
        // DACL onto a path whose DACL is NULL would replace "no DACL, everyone allowed" with
        // an EMPTY DACL, which denies everyone — a catastrophic silent lockout on a path we
        // were only asked to clean up.
        return Ok(());
    };

    let wpath = wide(&target);
    // SAFETY: `rebuilt` is a live, DWORD-aligned, InitializeAcl'd buffer that outlives the
    // call; `wpath` is NUL-terminated UTF-16 and likewise outlives it. DACL only —
    // inheritance stays unprotected so the invoking user keeps their inherited access.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            rebuilt.as_ptr().cast::<ACL>(),
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(win32_err("SetNamedSecurityInfoW", &target, rc));
    }
    Ok(())
}

/// One `SetEntriesInAclW` read-modify-write. `SetEntriesInAclW` merges the new entry into the
/// path's existing DACL and canonicalizes on insert, so this is additive: no pre-existing ace,
/// explicit or inherited, is disturbed.
fn add_ace(
    path: &Path,
    sid: &OwnedSid,
    mask: u32,
    mode: ACCESS_MODE,
    inherit: bool,
) -> io::Result<()> {
    let wpath = wide(path);
    let dacl = ReadDacl::open(path)?;

    // A NULL DACL means "no DACL at all", which Windows reads as UNRESTRICTED access — not as
    // an empty allow-set. `SetEntriesInAclW` merging into NULL yields a DACL containing ONLY
    // our ace, and writing that converts unrestricted into "the sandbox account and nobody
    // else", permanently locking out the object's own owner. That is destructive and violates
    // this module's additive-on-user-paths contract, so neither polarity may write here.
    // (VM-observed on a `C:\Windows\Temp` child, 2026-07-25: a grant reduced a 7-ace DACL to
    // one `nub-sandbox` ace and the owner lost traverse. `strip` already guards the mirror
    // case; `add_ace` did not.)
    if dacl.acl.is_null() {
        return match mode {
            // Nothing to grant: a NULL DACL already admits the sandbox account.
            GRANT_ACCESS => {
                tracing::debug!(
                    path = %path.display(),
                    "sandbox: grant skipped — path has a NULL DACL, so access is already unrestricted"
                );
                Ok(())
            }
            // FAIL CLOSED. The deny cannot be expressed without replacing the object's
            // permissive state wholesale, and silently skipping it would leave a hole while
            // reporting full enforcement.
            _ => Err(io::Error::other(format!(
                "sandbox: cannot deny {} to the sandbox account — the path has a NULL DACL \
                 (unrestricted access), and adding a deny there would replace that with a DACL \
                 that locks out its own owner. Give the path an explicit DACL, or drop it from \
                 the policy's deny list.",
                path.display()
            ))),
        };
    }

    let ea = explicit_access(sid.0, mask, mode, inherit);

    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `ea` and the SID it points at outlive the call; `dacl.acl` came from
    // GetNamedSecurityInfoW and may legitimately be NULL.
    let rc = unsafe { SetEntriesInAclW(1, &ea, dacl.acl, &mut new_dacl) };
    if rc != 0 {
        return Err(win32_err("SetEntriesInAclW", path, rc));
    }
    let _new_guard = LocalFreeGuard(new_dacl.cast());

    // SAFETY: `new_dacl` is a live ACL from SetEntriesInAclW; the path buffer is
    // NUL-terminated. `DACL_SECURITY_INFORMATION` only — never PROTECTED (see module doc).
    let rc = unsafe {
        SetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(win32_err("SetNamedSecurityInfoW", path, rc));
    }
    Ok(())
}

/// One ace description for `SetEntriesInAclW`. The returned struct BORROWS `sid`, which must
/// outlive the call it is passed to.
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
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            // UNKNOWN, not `TRUSTEE_IS_USER`: this helper also stamps the well-known
            // `BUILTIN\Administrators` and SYSTEM SIDs, and the field is advisory anyway.
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.cast(),
        },
    }
}

/// Rebuild `existing`'s ace list without any explicit ace for `sid`, in canonical order.
/// `Ok(None)` means nothing matched and the caller must not write anything back.
///
/// The rebuilt buffer is a `Vec<u32>` because `InitializeAcl` requires DWORD alignment.
fn rebuild_without_sid(
    path: &Path,
    existing: *mut ACL,
    sid: &OwnedSid,
) -> io::Result<Option<Vec<u32>>> {
    // (bucket, original index, ace pointer, ace size). The stable sort on (bucket, index)
    // preserves each bucket's original ordering so unrelated aces never shuffle.
    let mut kept: Vec<(u8, u32, *const std::ffi::c_void, u32)> = Vec::new();
    let mut kept_bytes: u32 = 0;
    let mut dropped = 0usize;

    walk_aces(existing, path, |i, header, ace| {
        let inherited = header.AceFlags & INHERITED_ACE_FLAG != 0;
        // Only the allow/deny types share the `{ header, mask, SidStart }` layout the SID is
        // read out of, and they are the only types this module ever writes. An ace of any
        // other type — object, callback, audit — is copied through untouched rather than
        // guessed at: keeping a foreign ace is safe, dropping one corrupts someone else's ACL.
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
    // Leaving our own ace in place is the deliberate lesser evil: a residual ace for a confined
    // account is over-permission, while an empty DACL locks the OWNER out of their own file
    // and is not recoverable without taking ownership.
    if kept.is_empty() {
        tracing::debug!(
            path = %path.display(),
            "sandbox: ace strip skipped — our aces are the only ones on this path, and an \
             empty DACL would deny everyone"
        );
        return Ok(None);
    }
    kept.sort_by_key(|&(bucket, index, _, _)| (bucket, index));

    let acl_bytes = (std::mem::size_of::<ACL>() as u32 + kept_bytes).next_multiple_of(4);
    let mut buf: Vec<u32> = vec![0; (acl_bytes as usize).div_ceil(4)];
    let acl = buf.as_mut_ptr().cast::<ACL>();

    // Preserve the source revision rather than assuming ACL_REVISION: an ACL carrying object
    // aces is revision 4, and AddAce rejects a revision-4 ace into a revision-2 ACL.
    // SAFETY: `existing` is live; `acl` points at `acl_bytes` of zeroed, DWORD-aligned space.
    let revision = u32::from(unsafe { (*existing).AclRevision });
    // SAFETY: as above.
    if unsafe { InitializeAcl(acl, acl_bytes, revision) } == 0 {
        return Err(win32_last_err("InitializeAcl", path));
    }

    // `AddAce` and the `AddAccess{Allowed,Denied}AceEx` family both APPEND at the tail and do
    // NOT canonicalize despite the latter's name, so the bucket order above is what actually
    // produces a canonical DACL. Emitting out of order would still be accepted by Windows and
    // would silently resolve a deny as an allow.
    for &(_, _, ace, size) in &kept {
        // SAFETY: `acl` was sized to hold exactly these aces; each `ace` is a live
        // `size`-byte ace inside the descriptor `ReadDacl` keeps alive across this call.
        if unsafe { AddAce(acl, revision, u32::MAX, ace, size) } == 0 {
            return Err(win32_last_err("AddAce", path));
        }
    }
    Ok(Some(buf))
}

/// Walk every ace in `acl` in DACL order. A NULL `acl` means "no DACL, everything allowed"
/// and yields nothing.
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
    // SAFETY: `acl` is a live ACL from GetNamedSecurityInfoW; `info` is a correctly sized
    // out-slot for the AclSizeInformation class.
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
        let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `i` is below the reported AceCount of a live ACL.
        if unsafe { GetAce(acl, i, &mut ace) } == 0 {
            return Err(win32_last_err("GetAce", path));
        }
        // SAFETY: every ace GetAce yields begins with an ACE_HEADER.
        f(i, unsafe { *ace.cast::<ACE_HEADER>() }, ace);
    }
    Ok(())
}

/// The trustee SID of an allow/deny ace. Caller must have checked the type — the inline
/// `SidStart` field is only at this offset for the types sharing `ACCESS_ALLOWED_ACE`'s
/// layout.
fn sid_of(ace: *mut std::ffi::c_void) -> PSID {
    // SAFETY: `addr_of!` takes the field address without forming a reference to the
    // variable-length SID that follows it.
    unsafe { std::ptr::addr_of!((*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart) }
        .cast_mut()
        .cast()
}

/// Canonical-order bucket: explicit DENY, explicit ALLOW, explicit other, then inherited.
/// Smaller sorts earlier.
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

/// A path's DACL plus the security descriptor that owns its storage. The descriptor MUST
/// outlive every read of `acl` — the ACL points INTO it.
struct ReadDacl {
    acl: *mut ACL,
    _sd: LocalFreeGuard,
}

impl ReadDacl {
    fn open(path: &Path) -> io::Result<Self> {
        let wpath = wide(path);
        let mut acl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `wpath` is NUL-terminated UTF-16 and outlives the call; every out-param is
        // a valid slot and the unwanted ones are NULL.
        let rc = unsafe {
            GetNamedSecurityInfoW(
                wpath.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut acl,
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        if rc != 0 {
            return Err(win32_err("GetNamedSecurityInfoW", path, rc));
        }
        Ok(ReadDacl {
            acl,
            _sd: LocalFreeGuard(sd),
        })
    }
}

/// A SID parsed from its string form, `LocalFree`d on drop.
struct OwnedSid(PSID);

impl OwnedSid {
    fn parse(s: &str) -> io::Result<Self> {
        let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sid: PSID = std::ptr::null_mut();
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

/// Canonicalize for the Win32 security APIs, returning the path and whether it is a directory.
///
/// `fs::canonicalize` emits `\\?\C:\…` and `\\?\UNC\server\share\…` — the extended-length
/// spellings both `Get`/`SetNamedSecurityInfoW` accept, and the only forms that are correct
/// for BOTH drive and UNC inputs (a naive `\\?\` prefix produces a malformed UNC path). It
/// also supplies the not-found check for free. Deliberate consequence: a symlink or junction
/// resolves to its TARGET, so the ACE lands on the object an open actually reaches rather than
/// on a reparse point, which does not gate content access.
fn resolve(path: &Path) -> io::Result<(PathBuf, bool)> {
    let canonical = std::fs::canonicalize(path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("sandbox ACL target does not exist: {}", path.display()),
            )
        } else {
            io::Error::other(format!(
                "resolving sandbox ACL target {}: {e}",
                path.display()
            ))
        }
    })?;
    let is_dir = std::fs::metadata(&canonical)?.is_dir();
    Ok((canonical, is_dir))
}

fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn win32_err(op: &str, path: &Path, rc: u32) -> io::Error {
    match rc {
        ERROR_ACCESS_DENIED => io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{op} on {}: access denied — nub holds no WRITE_DAC on this path, so the \
                 sandbox account's access cannot be set",
                path.display()
            ),
        ),
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => io::Error::new(
            io::ErrorKind::NotFound,
            format!("{op} on {}: path no longer exists", path.display()),
        ),
        _ => io::Error::other(format!(
            "{op} on {} failed (Win32 error {rc})",
            path.display()
        )),
    }
}

/// For the ACL-surgery calls, which report failure through `GetLastError` rather than a
/// returned status.
fn win32_last_err(op: &str, path: &Path) -> io::Error {
    let rc = io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32;
    win32_err(op, path, rc)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One ace, flattened enough to assert ordering and byte-level preservation without
    /// holding a pointer into a freed descriptor.
    struct Ace {
        ace_type: u8,
        inherited: bool,
        bytes: Vec<u8>,
    }

    /// Every ace on `path`'s DACL, in DACL order. Shares [`walk_aces`] with the production
    /// rebuild so the assertion can never drift from what `strip` actually sees.
    fn read_aces(path: &Path) -> Vec<Ace> {
        let (canonical, _) = resolve(path).expect("resolve");
        let dacl = ReadDacl::open(&canonical).expect("GetNamedSecurityInfoW");
        let mut out = Vec::new();
        walk_aces(dacl.acl, &canonical, |_, header, ace| {
            // SAFETY: `AceSize` is the ace's own length, inside the descriptor `dacl` holds.
            let bytes =
                unsafe { std::slice::from_raw_parts(ace.cast::<u8>(), header.AceSize as usize) };
            out.push(Ace {
                ace_type: header.AceType,
                inherited: header.AceFlags & INHERITED_ACE_FLAG != 0,
                bytes: bytes.to_vec(),
            });
        })
        .expect("walk DACL");
        out
    }

    /// The raw SID bytes a `S-1-…` string parses to, for matching aces by trustee.
    fn sid_bytes(sid: &str) -> Vec<u8> {
        let parsed = OwnedSid::parse(sid).expect("parse SID");
        // SubAuthorityCount is byte 1; a SID is 8 + 4 * count bytes.
        let head = unsafe { std::slice::from_raw_parts(parsed.0.cast::<u8>(), 2) };
        let len = 8 + 4 * head[1] as usize;
        unsafe { std::slice::from_raw_parts(parsed.0.cast::<u8>(), len) }.to_vec()
    }

    /// Explicit allow/deny aces on `aces` whose trustee is `sid` — the SID sits at offset 8,
    /// immediately after the 4-byte header and the 4-byte mask.
    fn explicit_for(aces: &[Ace], sid: &[u8]) -> Vec<Vec<u8>> {
        aces.iter()
            .filter(|a| {
                !a.inherited
                    && matches!(a.ace_type, ACCESS_ALLOWED_ACE_TYPE | ACCESS_DENIED_ACE_TYPE)
                    && a.bytes.len() >= 8 + sid.len()
                    && &a.bytes[8..8 + sid.len()] == sid
            })
            .map(|a| a.bytes.clone())
            .collect()
    }

    /// No explicit ace after any inherited one; no explicit DENY after any explicit ALLOW.
    fn assert_canonical(aces: &[Ace], label: &str) {
        let order: Vec<(u8, bool)> = aces.iter().map(|a| (a.ace_type, a.inherited)).collect();
        let mut saw_inherited = false;
        let mut saw_allow = false;
        for (i, a) in aces.iter().enumerate() {
            if a.inherited {
                saw_inherited = true;
                continue;
            }
            assert!(
                !saw_inherited,
                "{label}: explicit ace at {i} follows an inherited ace; order={order:?}"
            );
            match a.ace_type {
                ACCESS_DENIED_ACE_TYPE => assert!(
                    !saw_allow,
                    "{label}: explicit DENY at {i} follows an explicit ALLOW — Windows accepts \
                     this and resolves it first-match, so the deny would silently read as \
                     allow; order={order:?}"
                ),
                ACCESS_ALLOWED_ACE_TYPE => saw_allow = true,
                _ => {}
            }
        }
    }

    /// BUILTIN\Users. Always resolves, harmless to ace on a temp dir the test owns, and does
    /// not appear as an *explicit* ace under a user-profile `%TEMP%`.
    const SID: &str = "S-1-5-32-545";
    /// Everyone — a second, distinct trustee for the preservation test.
    const OTHER_SID: &str = "S-1-1-0";

    /// A top-level deny target's `Path::parent` IS the volume root, so without this check
    /// `deny: ["C:/creds.json"]` writes a permanent explicit DENY ace onto the whole drive
    /// (or, unelevated, aborts the run with an error naming a path the policy never mentioned).
    #[test]
    fn a_volume_root_is_recognized_and_carries_no_carve() {
        for root in [r"\\?\C:\", r"\\?\UNC\server\share\"] {
            assert!(is_volume_root(Path::new(root)), "{root}");
        }
        for under in [r"\\?\C:\creds.json", r"\\?\UNC\server\share\creds.json"] {
            assert!(!is_volume_root(Path::new(under)), "{under}");
        }
        assert_eq!(carve_parent(Path::new(r"\\?\C:\creds.json")), None);
        assert_eq!(
            carve_parent(Path::new(r"\\?\C:\proj\.env")),
            Some(PathBuf::from(r"\\?\C:\proj"))
        );
    }

    /// The property the whole agent-sandbox fs axis rests on: a deny written onto a file
    /// inside a granted tree must land AHEAD of the allow the tree's `(OI)(CI)` grant
    /// propagated onto that file. Windows accepts the wrong order silently, so nothing but an
    /// assertion catches a regression here.
    #[test]
    fn deny_inside_a_grant_lands_before_the_inherited_allow() {
        let td = tempfile::tempdir().unwrap();
        grant(td.path(), SID, Access::ReadWrite).expect("grant");

        // Created AFTER the grant, so the inheritable allow propagates onto it as an
        // INHERITED ace — the exact shape the deny has to outrank.
        let secret = td.path().join(".env");
        std::fs::write(&secret, b"TOKEN=x").unwrap();
        deny(&secret, SID).expect("deny");

        let aces = read_aces(&secret);
        assert_canonical(&aces, "deny inside grant");
        assert!(
            aces.iter().any(|a| a.inherited),
            "the grant should have propagated an inherited ace onto the file"
        );
        assert!(
            aces.iter()
                .any(|a| !a.inherited && a.ace_type == ACCESS_DENIED_ACE_TYPE),
            "the deny should be present as an EXPLICIT ace"
        );
    }

    /// The regression `SetEntriesInAclW(REVOKE_ACCESS)` fails: on Windows 11 25H2 an explicit
    /// ACCESS_DENIED ace survives a REVOKE. Baseline-relative so a pre-existing ace for the
    /// same SID cannot make it pass or fail spuriously.
    #[test]
    fn strip_removes_a_deny_ace() {
        let td = tempfile::tempdir().unwrap();
        let sid = sid_bytes(SID);
        let baseline = explicit_for(&read_aces(td.path()), &sid).len();

        grant(td.path(), SID, Access::ReadWrite).expect("grant");
        deny(td.path(), SID).expect("deny");
        assert!(
            explicit_for(&read_aces(td.path()), &sid).len() > baseline,
            "setup did not add any explicit ace to strip"
        );

        strip(td.path(), SID).expect("strip");
        assert_eq!(
            explicit_for(&read_aces(td.path()), &sid).len(),
            baseline,
            "strip left residue — the REVOKE_ACCESS deny-survival defect is back"
        );
        assert_canonical(&read_aces(td.path()), "after strip");
    }

    /// Teardown must not disturb aces nub did not author — including ones written by other
    /// tools. Byte-identical, not merely present: a rebuild that re-encoded a kept ace would
    /// silently rewrite a third party's rights.
    #[test]
    fn strip_preserves_foreign_aces() {
        let td = tempfile::tempdir().unwrap();
        grant(td.path(), OTHER_SID, Access::Read).expect("seed foreign ace");
        grant(td.path(), SID, Access::ReadWrite).expect("grant");

        let foreign = sid_bytes(OTHER_SID);
        let before = explicit_for(&read_aces(td.path()), &foreign);
        assert!(!before.is_empty(), "foreign ace was not seeded");

        strip(td.path(), SID).expect("strip");
        assert_eq!(
            explicit_for(&read_aces(td.path()), &foreign),
            before,
            "stripping our SID altered another trustee's aces"
        );
    }
}

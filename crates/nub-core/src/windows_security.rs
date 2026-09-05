//! Windows cache-directory security primitives shared by the runtime cache and
//! compiled-artifact launcher. The protected DACL is installed atomically, so no
//! inherited/shared access window exists while a cache directory is created.

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

type Handle = *mut c_void;
type Sid = *mut c_void;

#[repr(C)]
struct SecurityAttributes {
    length: u32,
    descriptor: *mut c_void,
    inherit_handle: i32,
}

#[repr(C)]
struct Acl {
    _revision: u8,
    _reserved1: u8,
    _size: u16,
    ace_count: u16,
    _reserved2: u16,
}

#[repr(C)]
struct AceHeader {
    ace_type: u8,
    ace_flags: u8,
    ace_size: u16,
}

#[repr(C)]
struct SidAndAttributes {
    sid: Sid,
    _attributes: u32,
}

#[repr(C)]
struct TokenUser {
    user: SidAndAttributes,
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string: *const u16,
        revision: u32,
        descriptor: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
    fn GetNamedSecurityInfoW(
        name: *const u16,
        object_type: u32,
        security_info: u32,
        owner: *mut Sid,
        group: *mut Sid,
        dacl: *mut *mut Acl,
        sacl: *mut *mut Acl,
        descriptor: *mut *mut c_void,
    ) -> u32;
    #[cfg(feature = "embed-runtime")]
    fn SetNamedSecurityInfoW(
        name: *mut u16,
        object_type: u32,
        security_info: u32,
        owner: Sid,
        group: Sid,
        dacl: *mut Acl,
        sacl: *mut Acl,
    ) -> u32;
    #[cfg(feature = "embed-runtime")]
    fn GetSecurityDescriptorDacl(
        descriptor: *mut c_void,
        present: *mut i32,
        dacl: *mut *mut Acl,
        defaulted: *mut i32,
    ) -> i32;
    fn GetAce(acl: *const Acl, index: u32, ace: *mut *mut c_void) -> i32;
    fn EqualSid(first: Sid, second: Sid) -> i32;
    fn IsWellKnownSid(sid: Sid, kind: i32) -> i32;
    #[cfg(test)]
    fn CreateWellKnownSid(kind: i32, domain: Sid, sid: *mut c_void, size: *mut u32) -> i32;
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    fn GetTokenInformation(
        token: Handle,
        class: i32,
        information: *mut c_void,
        information_len: u32,
        return_len: *mut u32,
    ) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateDirectoryW(path: *const u16, attributes: *mut SecurityAttributes) -> i32;
    fn GetCurrentProcess() -> Handle;
    fn CloseHandle(handle: Handle) -> i32;
    fn GetLastError() -> u32;
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
}

const SDDL_REVISION_1: u32 = 1;
const SE_FILE_OBJECT: u32 = 1;
const OWNER_SECURITY_INFORMATION: u32 = 0x1;
const DACL_SECURITY_INFORMATION: u32 = 0x4;
#[cfg(feature = "embed-runtime")]
const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;
const TOKEN_QUERY: u32 = 0x8;
const TOKEN_USER_CLASS: i32 = 1;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_ALLOWED_COMPOUND_ACE_TYPE: u8 = 4;
const ACCESS_ALLOWED_OBJECT_ACE_TYPE: u8 = 5;
const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 9;
const ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE: u8 = 11;
const ACE_OBJECT_TYPE_PRESENT: u32 = 0x1;
const ACE_INHERITED_OBJECT_TYPE_PRESENT: u32 = 0x2;
const INHERIT_ONLY_ACE: u8 = 0x08;

const WIN_CREATOR_OWNER_SID: i32 = 3;
const WIN_LOCAL_SYSTEM_SID: i32 = 22;
const WIN_BUILTIN_ADMINISTRATORS_SID: i32 = 26;
const WIN_CREATOR_OWNER_RIGHTS_SID: i32 = 71;

const DELETE: u32 = 0x0001_0000;
const WRITE_DAC: u32 = 0x0004_0000;
const WRITE_OWNER: u32 = 0x0008_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const FILE_ADD_FILE: u32 = 0x0000_0002;
const FILE_ADD_SUBDIRECTORY: u32 = 0x0000_0004;
const FILE_WRITE_EA: u32 = 0x0000_0010;
const FILE_DELETE_CHILD: u32 = 0x0000_0040;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;

struct LocalAllocation(*mut c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
}

struct OwnedHandle(Handle);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

struct CurrentUserSid {
    _storage: Vec<usize>,
    sid: Sid,
}

/// Protected DACL: object/container-inheritable full access for the object owner,
/// LocalSystem, and Administrators only. `P` blocks inheritance from the parent,
/// so a newly created cache does not inherit broader access.
const PRIVATE_DACL: &str = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

pub fn create_private_directory(path: &Path) -> io::Result<()> {
    create_directory_with_sddl(path, PRIVATE_DACL)
}

/// Re-apply [`PRIVATE_DACL`] to a directory that already exists.
///
/// nub's cache root is created by whichever subsystem reaches it first, and the
/// ordinary `fs::create_dir_all` path inherits the parent's ACEs instead of
/// installing a protected DACL. A container-inheritable `BUILTIN\Users:(AD)/(WD)`
/// on the parent — the DEFAULT on a secondary volume, and what GitHub's
/// `D:\a\_temp` carries — therefore leaves the leaf writable by every local user,
/// and the runtime cache correctly refuses to load code from it. Without this the
/// refusal is silent and the cache relocates to `$TMPDIR`, ignoring an explicitly
/// configured `XDG_CACHE_HOME`.
///
/// Strictly narrowing: it drops the inherited grants and leaves owner/SYSTEM/
/// Administrators. Refuses unless the OWNER is already trusted — hardening a
/// directory somebody else created would adopt a tree an attacker may have
/// planted and hide it behind a now-clean DACL, so a foreign owner stays declined.
#[cfg(feature = "embed-runtime")]
pub(crate) fn harden_private_directory(path: &Path) -> io::Result<()> {
    let current = current_user_sid()?;
    let wide = wide_path(path);

    let mut owner = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let _descriptor = LocalAllocation(descriptor);
    if owner.is_null() || !trusted_sid(owner, current.sid) {
        return Err(io::Error::other(
            "refusing to harden a cache directory owned by another principal",
        ));
    }

    let sddl: Vec<u16> = PRIVATE_DACL
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut template = ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut template,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let _template = LocalAllocation(template);

    let mut dacl = ptr::null_mut();
    let mut present = 0i32;
    let mut defaulted = 0i32;
    if unsafe { GetSecurityDescriptorDacl(template, &mut present, &mut dacl, &mut defaulted) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if present == 0 || dacl.is_null() {
        return Err(io::Error::other("private DACL template carries no ACL"));
    }

    let mut wide = wide;
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null_mut(),
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn create_shared_directory(path: &Path) -> io::Result<()> {
    create_directory_with_sddl(path, "D:P(A;OICI;FA;;;WD)")
}

fn create_directory_with_sddl(path: &Path, sddl: &str) -> io::Result<()> {
    let sddl: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut descriptor = ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let _descriptor = LocalAllocation(descriptor);
    let mut attributes = SecurityAttributes {
        length: std::mem::size_of::<SecurityAttributes>() as u32,
        descriptor,
        inherit_handle: 0,
    };
    let path = wide_path(path);
    if unsafe { CreateDirectoryW(path.as_ptr(), &mut attributes) } == 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { GetLastError() } as i32
        ));
    }
    Ok(())
}

pub fn directory_is_stable(path: &Path, leaf: bool, volume_root: bool) -> io::Result<bool> {
    let current = current_user_sid()?;
    let path = wide_path(path);
    let mut owner = ptr::null_mut();
    let mut dacl = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let _descriptor = LocalAllocation(descriptor);
    // The volume root cannot itself be renamed. Its DACL still must prevent
    // an untrusted principal from deleting/replacing the next component, but
    // Windows installations may assign the root to TrustedInstaller rather
    // than SYSTEM/Administrators.
    if owner.is_null() || (!volume_root && !trusted_sid(owner, current.sid)) || dacl.is_null() {
        return Ok(false);
    }

    let acl = unsafe { &*dacl };
    for index in 0..u32::from(acl.ace_count) {
        let mut raw_ace = ptr::null_mut();
        if unsafe { GetAce(dacl, index, &mut raw_ace) } == 0 || raw_ace.is_null() {
            return Err(io::Error::last_os_error());
        }
        let header = unsafe { &*(raw_ace.cast::<AceHeader>()) };
        let standard_allow = matches!(
            header.ace_type,
            ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE
        );
        let object_allow = matches!(
            header.ace_type,
            ACCESS_ALLOWED_OBJECT_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE
        );
        if !standard_allow && !object_allow && header.ace_type != ACCESS_ALLOWED_COMPOUND_ACE_TYPE {
            continue;
        }
        // An INHERIT_ONLY ACE is a template, never consulted when checking access
        // to the object carrying it — only when composing a child's DACL. On an
        // ANCESTOR that makes it irrelevant here: the walk visits every component,
        // so wherever the ACE actually propagates it is examined as an effective
        // ACE there, and a component that blocks inheritance stops it outright.
        // Counting it is what refused `C:\` on stock Windows, whose volume root
        // always carries `Authenticated Users:(OI)(CI)(IO)(M)` — that rejected the
        // first component of every chain, so no runtime-cache or compiled-artifact
        // base was usable on any Windows machine. The LEAF still counts them: its
        // children are the cache CONTENTS, which no later component validates, so
        // an inherit-only grant there does reach the files we are protecting.
        if !leaf && header.ace_flags & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        if usize::from(header.ace_size) < 8 {
            return Ok(false);
        }
        let bytes = raw_ace.cast::<u8>();
        let mask = unsafe { ptr::read_unaligned(bytes.add(4).cast::<u32>()) };
        let mut dangerous =
            DELETE | WRITE_DAC | WRITE_OWNER | FILE_DELETE_CHILD | GENERIC_WRITE | GENERIC_ALL;
        if leaf {
            dangerous |=
                FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY | FILE_WRITE_EA | FILE_WRITE_ATTRIBUTES;
        }
        if mask & dangerous == 0 {
            continue;
        }
        // Compound allow ACEs have a different SID layout and are obsolete.
        // A mutating one is not a cache-compatible ACL.
        if header.ace_type == ACCESS_ALLOWED_COMPOUND_ACE_TYPE {
            return Ok(false);
        }

        let sid_offset = if standard_allow {
            8
        } else {
            if usize::from(header.ace_size) < 12 {
                return Ok(false);
            }
            let flags = unsafe { ptr::read_unaligned(bytes.add(8).cast::<u32>()) };
            12 + usize::from(flags & ACE_OBJECT_TYPE_PRESENT != 0) * 16
                + usize::from(flags & ACE_INHERITED_OBJECT_TYPE_PRESENT != 0) * 16
        };
        if sid_offset >= usize::from(header.ace_size) {
            return Ok(false);
        }
        let sid = unsafe { bytes.add(sid_offset).cast::<c_void>() };
        if !trusted_sid(sid, current.sid) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn current_user_sid() -> io::Result<CurrentUserSid> {
    let mut token = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);
    let mut bytes = 0;
    unsafe {
        GetTokenInformation(token.0, TOKEN_USER_CLASS, ptr::null_mut(), 0, &mut bytes);
    }
    if unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER || bytes == 0 {
        return Err(io::Error::last_os_error());
    }
    let words = (bytes as usize).div_ceil(std::mem::size_of::<usize>());
    let mut storage = vec![0usize; words];
    if unsafe {
        GetTokenInformation(
            token.0,
            TOKEN_USER_CLASS,
            storage.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let token_user = unsafe { &*(storage.as_ptr().cast::<TokenUser>()) };
    Ok(CurrentUserSid {
        sid: token_user.user.sid,
        _storage: storage,
    })
}

fn trusted_sid(sid: Sid, current: Sid) -> bool {
    unsafe {
        EqualSid(sid, current) != 0
            || IsWellKnownSid(sid, WIN_CREATOR_OWNER_SID) != 0
            || IsWellKnownSid(sid, WIN_LOCAL_SYSTEM_SID) != 0
            || IsWellKnownSid(sid, WIN_BUILTIN_ADMINISTRATORS_SID) != 0
            || IsWellKnownSid(sid, WIN_CREATOR_OWNER_RIGHTS_SID) != 0
    }
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Owner-only, except one INHERIT_ONLY grant of full control to Everyone —
    /// the shape stock Windows ships on `C:\` (`Authenticated Users:(OI)(CI)(IO)(M)`).
    fn create_inherit_only_shared_directory(path: &Path) -> io::Result<()> {
        create_directory_with_sddl(
            path,
            "D:P(A;OICIIO;FA;;;WD)(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
        )
    }

    /// Pins the ancestor/leaf asymmetry. Reading an inherit-only ACE as if it
    /// granted access on its own object rejected every chain at `C:\`; ignoring it
    /// at the LEAF too would let the cache contents inherit a foreign grant.
    /// `WinWorldSid`, i.e. Everyone. Not in the trusted set, and the only
    /// well-known SID that makes the refusal branch reachable without a second
    /// account or elevation.
    const WIN_WORLD_SID: i32 = 1;

    /// The refusal half of `harden_private_directory`.
    ///
    /// Hardening a directory we do NOT own would adopt an attacker-planted tree and
    /// paper over it — the DACL would end up correct while the CONTENTS stayed
    /// suspect. That branch had no coverage, because nothing in the tree creates a
    /// directory owned by another principal, so the decision is tested where it is
    /// actually made: the predicate, not the filesystem.
    #[test]
    fn a_directory_owned_by_another_principal_is_not_trusted() {
        let current = current_user_sid().expect("this process has a token");

        // Positive control: our own SID must pass, or a `false` below would prove
        // nothing about foreign owners.
        assert!(
            trusted_sid(current.sid, current.sid),
            "our own SID must be trusted, else this test cannot discriminate"
        );

        let mut everyone = vec![0u8; 68];
        let mut size = everyone.len() as u32;
        let made = unsafe {
            CreateWellKnownSid(
                WIN_WORLD_SID,
                ptr::null_mut(),
                everyone.as_mut_ptr().cast(),
                &mut size,
            )
        };
        assert!(made != 0, "CreateWellKnownSid(WinWorldSid) failed");

        let everyone: Sid = everyone.as_mut_ptr().cast();
        assert!(
            !trusted_sid(everyone, current.sid),
            "Everyone must not be a trusted owner — hardening such a directory would adopt it"
        );
    }

    #[test]
    fn an_inherit_only_grant_is_an_ancestor_template_but_a_leaf_hazard() {
        let dir = std::env::temp_dir().join(format!("nub-ws-io-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        create_inherit_only_shared_directory(&dir).unwrap();

        assert!(
            directory_is_stable(&dir, false, false).unwrap(),
            "an inherit-only ACE grants nothing on the ancestor carrying it"
        );
        assert!(
            !directory_is_stable(&dir, true, false).unwrap(),
            "the leaf's children are the cache contents, which do receive it"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

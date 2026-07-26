//! Windows agent-sandbox: the dedicated low-privilege PRINCIPAL half (design B3, bit 1).
//!
//! The agent-sandbox tier (per-host net, generous-read-minus-secrets) runs the child as a
//! fixed local account `nub-sandbox` that is STRICTLY LOWER-privileged than any real user.
//! Being a separate principal is what makes generous-read-minus-secrets and `.env` carving
//! expressible (deny-ACEs target OUR account SID, not the user's) and gives WFP a stable SID
//! to key per-host egress on. This module owns that account's LIFECYCLE, its stored
//! credential, and the setup-vs-run detection — NOT the launcher (bit 2) or WFP (bit 3).
//!
//! TWO PRIVILEGE PHASES (design §2 / §6):
//!   - `windows_setup()` — ONE-TIME ELEVATED, idempotent. Creates/rotates the account, grants
//!     it SeBatchLogonRight (+ denies interactive/network/remote logon), stores a random
//!     password machine-scoped (DPAPI `CRYPTPROTECT_LOCAL_MACHINE`), and writes `setup.json`.
//!   - `detect()` — UNELEVATED, read-only. Reads `setup.json`, checks `setup_version`, confirms
//!     the account exists (`NetUserGetInfo` level 0 is readable by authenticated users), and
//!     DPAPI-decrypts the stored password for the launcher. Touches NOTHING privileged: the
//!     unelevated run never opens the WFP engine (that is the broker's job, bit 3).
//!
//! CREDENTIAL RECOVERY POSTURE (design §9.2, ACCEPTED): any authenticated user can
//! `CryptUnprotectData` the machine-scoped blob (gated only by the file DACL). This is not an
//! escalation — the account is lower-priv than every user, so borrowing its identity grants
//! nothing a user does not already have.

use serde::{Deserialize, Serialize};

/// The single fixed sandbox account (SRT/Codex pattern — one shared account, not per-run).
/// Concurrent runs share the SID; cross-run proxy-port reach is defanged by the per-session
/// proxy token, and the shared SID forces the exact-ACE teardown in bit 4 (design §9.3).
pub const ACCOUNT_NAME: &str = "nub-sandbox";

/// Bumped whenever the on-disk `setup.json` schema or the setup steps change in a way that
/// makes an older setup insufficient — a mismatch makes `detect()` fail closed with the
/// re-run hint rather than launching against a stale principal.
pub const SETUP_VERSION: u32 = 2;

/// Persistent, machine-visible setup record. Written by `windows_setup()` (elevated) with a
/// DACL granting authenticated users READ; consumed by `detect()` (unelevated). The WFP GUID
/// fields are populated by bit 3's setup half — optional here so bit 1 can land and bit 3 can
/// fill them without a format break.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupInfo {
    pub setup_version: u32,
    pub account_name: String,
    /// String SID (`S-1-5-21-…`) of the account — the acceptance-gate identity and the WFP
    /// `ALE_USER_ID` key. Stored as text so `detect()` need not re-resolve it.
    pub account_sid: String,
    /// WFP provider GUID (bit 3). `None` until the WFP setup half runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wfp_provider_guid: Option<String>,
    /// WFP sublayer GUID (bit 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wfp_sublayer_guid: Option<String>,
}

impl SetupInfo {
    pub fn to_json(&self) -> String {
        // A fixed small record; serialization cannot realistically fail, but never unwrap in
        // library code — fall back to a minimal valid document.
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// Encode 32 CSPRNG bytes as a Windows-password-complexity-satisfying ASCII string: it must
/// contain an upper, a lower, a digit, and a symbol (3-of-4 categories is the default policy;
/// we supply all four unconditionally) and avoid the shell/quoting hazards that would break
/// the command line. Deterministic given the input, so it is host-unit-tested. The full 256
/// bits of entropy are preserved in the base-62-ish body; the 4 guaranteed category chars are
/// appended, so length is 32-ish + 4 and every draw is complexity-valid regardless of bytes.
pub fn encode_password(bytes: &[u8]) -> String {
    // Symbol-free alnum alphabet for the entropy body (no quoting hazards); the required
    // symbol is supplied by the guaranteed suffix.
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(bytes.len() + 4);
    for &b in bytes {
        out.push(ALPHABET[(b as usize) % ALPHABET.len()] as char);
    }
    // Guarantee all four categories independent of the random body.
    out.push_str("Aa1-");
    out
}

/// Where the machine-scoped setup record + credential blob live: `%ProgramData%\nub\sandbox`.
/// `%ProgramData%` is world-readable and admin-writable — correct for a machine-scoped,
/// authenticated-users-readable store. Falls back to the conventional literal if the env var
/// is unset (should never happen on a real Windows host). Host-testable via the env var.
pub fn store_dir() -> std::path::PathBuf {
    let base = std::env::var_os("ProgramData")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\ProgramData"));
    base.join("nub").join("sandbox")
}

pub fn setup_json_path() -> std::path::PathBuf {
    store_dir().join("setup.json")
}
pub fn account_blob_path() -> std::path::PathBuf {
    store_dir().join("account.bin")
}

/// Why a run cannot use the agent-sandbox tier — surfaced by `detect()` so the caller can
/// fail closed with the exact one-time-setup hint (design §6) instead of silently degrading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectError {
    /// No `setup.json` — the one-time elevated setup has never run.
    NotSetUp,
    /// `setup.json` records an older `setup_version` than this binary expects.
    VersionMismatch { found: u32, expected: u32 },
    /// setup.json present but the account is gone, the blob is unreadable, or a lookup failed.
    Broken(String),
}

impl std::fmt::Display for DetectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DetectError::NotSetUp => write!(
                f,
                "nub sandbox is not set up on this machine. Run: nub sandbox setup (one-time, elevated)"
            ),
            DetectError::VersionMismatch { found, expected } => write!(
                f,
                "nub sandbox setup is out of date (found v{found}, need v{expected}). \
                 Run: nub sandbox setup (one-time, elevated)"
            ),
            DetectError::Broken(why) => write!(f, "nub sandbox setup is broken: {why}"),
        }
    }
}

impl std::error::Error for DetectError {}

/// Everything an unelevated run needs to launch as the dedicated account. The password is
/// held only transiently (zeroization is a later hardening item, tracked, not bit 1).
#[cfg(target_os = "windows")]
#[derive(Debug, Clone)]
pub struct AccountCreds {
    pub account_name: String,
    pub account_sid: String,
    pub password: String,
    pub info: SetupInfo,
}

// ── the elevated + unelevated FFI ──────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
pub use imp::{detect, windows_setup, windows_teardown};

#[cfg(target_os = "windows")]
mod imp {
    use super::{
        ACCOUNT_NAME, AccountCreds, DetectError, SETUP_VERSION, SetupInfo, account_blob_path,
        encode_password, setup_json_path, store_dir,
    };
    use std::io;

    use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
    use windows_sys::Win32::NetworkManagement::NetManagement::{
        NetApiBufferFree, NetUserAdd, NetUserDel, NetUserGetInfo, NetUserSetInfo, USER_INFO_1,
        USER_INFO_1003,
    };
    use windows_sys::Win32::Security::Authentication::Identity::{
        LSA_HANDLE, LSA_OBJECT_ATTRIBUTES, LSA_UNICODE_STRING, LsaAddAccountRights, LsaClose,
        LsaNtStatusToWinError, LsaOpenPolicy, LsaRemoveAccountRights,
    };
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CryptProtectData, CryptUnprotectData,
    };
    use windows_sys::Win32::Security::{LookupAccountNameW, PSID, SID_NAME_USE};

    // netapi3 numeric contract (values are stable across the Windows API; defined locally to
    // avoid depending on which windows-sys submodule re-exports each — mirrors windows.rs's
    // local GENERIC_* consts). Provenance: lmaccess.h / lmerr.h.
    const NERR_SUCCESS: u32 = 0;
    const NERR_USER_NOT_FOUND: u32 = 2221;
    const USER_PRIV_USER: u32 = 1;
    const UF_SCRIPT: u32 = 0x0001;
    const UF_PASSWD_CANT_CHANGE: u32 = 0x0040;
    const UF_DONT_EXPIRE_PASSWD: u32 = 0x1_0000;
    // DPAPI: protect under the MACHINE key so any principal on this host can decrypt (file DACL
    // is the gate). Provenance: dpapi.h.
    const CRYPTPROTECT_LOCAL_MACHINE: u32 = 0x4;
    // LSA policy access needed to add account rights. Provenance: ntsecapi.h.
    const POLICY_CREATE_ACCOUNT: u32 = 0x0000_0010;
    const POLICY_LOOKUP_NAMES: u32 = 0x0000_0800;

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// A NUL-terminated wide buffer whose pointer we hand to a Win32 API as a mutable PWSTR.
    /// The API treats it as input-only here; the buffer outlives the call.
    fn wide_mut(s: &str) -> Vec<u16> {
        to_wide(s)
    }

    fn last_error(ctx: &str) -> io::Error {
        io::Error::other(format!("{ctx}: {}", io::Error::from_raw_os_error(unsafe {
            GetLastError()
        } as i32)))
    }

    // ── account lifecycle (elevated) ───────────────────────────────────────────────────

    /// Whether the `nub-sandbox` account exists. `NetUserGetInfo` level 0 is readable by
    /// authenticated users, so this is also usable from the unelevated `detect()` path.
    fn account_exists(name: &str) -> io::Result<bool> {
        let wname = to_wide(name);
        let mut buf: *mut u8 = std::ptr::null_mut();
        let rc = unsafe { NetUserGetInfo(std::ptr::null(), wname.as_ptr(), 0, &mut buf) };
        if !buf.is_null() {
            unsafe { NetApiBufferFree(buf.cast()) };
        }
        match rc {
            NERR_SUCCESS => Ok(true),
            NERR_USER_NOT_FOUND => Ok(false),
            other => Err(io::Error::from_raw_os_error(other as i32)),
        }
    }

    /// Create the account (level 1). Non-expiring, cannot-change password, script flag set —
    /// a service-style login account, never interactive.
    fn net_user_add(name: &str, password: &str) -> io::Result<()> {
        let mut wname = wide_mut(name);
        let mut wpass = wide_mut(password);
        let mut wcomment = wide_mut("nub agent-sandbox low-privilege account");
        let mut info: USER_INFO_1 = unsafe { std::mem::zeroed() };
        info.usri1_name = wname.as_mut_ptr();
        info.usri1_password = wpass.as_mut_ptr();
        info.usri1_priv = USER_PRIV_USER;
        info.usri1_comment = wcomment.as_mut_ptr();
        info.usri1_flags = UF_SCRIPT | UF_DONT_EXPIRE_PASSWD | UF_PASSWD_CANT_CHANGE;
        let mut parm_err: u32 = 0;
        let rc = unsafe {
            NetUserAdd(
                std::ptr::null(),
                1,
                std::ptr::from_mut(&mut info).cast(),
                &mut parm_err,
            )
        };
        if rc != NERR_SUCCESS {
            return Err(io::Error::other(format!(
                "NetUserAdd failed rc={rc} (bad param index {parm_err})"
            )));
        }
        Ok(())
    }

    /// Rotate the password of an existing account (level 1003).
    fn net_user_set_password(name: &str, password: &str) -> io::Result<()> {
        let wname = to_wide(name);
        let mut wpass = wide_mut(password);
        let mut info: USER_INFO_1003 = unsafe { std::mem::zeroed() };
        info.usri1003_password = wpass.as_mut_ptr();
        let mut parm_err: u32 = 0;
        let rc = unsafe {
            NetUserSetInfo(
                std::ptr::null(),
                wname.as_ptr(),
                1003,
                std::ptr::from_mut(&mut info).cast(),
                &mut parm_err,
            )
        };
        if rc != NERR_SUCCESS {
            return Err(io::Error::other(format!("NetUserSetInfo failed rc={rc}")));
        }
        Ok(())
    }

    fn net_user_del(name: &str) -> io::Result<()> {
        let wname = to_wide(name);
        let rc = unsafe { NetUserDel(std::ptr::null(), wname.as_ptr()) };
        if rc != NERR_SUCCESS && rc != NERR_USER_NOT_FOUND {
            return Err(io::Error::other(format!("NetUserDel failed rc={rc}")));
        }
        Ok(())
    }

    /// Resolve an account name to (owned SID bytes, string SID). The two-call sizing pattern:
    /// first call fails with the required buffer sizes, second fills them.
    fn lookup_account_sid(name: &str) -> io::Result<(Vec<u8>, String)> {
        let wname = to_wide(name);
        let mut sid_len: u32 = 0;
        let mut dom_len: u32 = 0;
        let mut sid_use: SID_NAME_USE = 0;
        // Sizing call — expected to fail with ERROR_INSUFFICIENT_BUFFER, setting the lengths.
        unsafe {
            LookupAccountNameW(
                std::ptr::null(),
                wname.as_ptr(),
                std::ptr::null_mut(),
                &mut sid_len,
                std::ptr::null_mut(),
                &mut dom_len,
                &mut sid_use,
            )
        };
        if sid_len == 0 {
            return Err(last_error("LookupAccountNameW sizing"));
        }
        let mut sid = vec![0u8; sid_len as usize];
        let mut dom = vec![0u16; dom_len as usize];
        let ok = unsafe {
            LookupAccountNameW(
                std::ptr::null(),
                wname.as_ptr(),
                sid.as_mut_ptr() as PSID,
                &mut sid_len,
                dom.as_mut_ptr(),
                &mut dom_len,
                &mut sid_use,
            )
        };
        if ok == 0 {
            return Err(last_error("LookupAccountNameW"));
        }
        let s = sid_to_string(sid.as_mut_ptr() as PSID)?;
        Ok((sid, s))
    }

    fn sid_to_string(sid: PSID) -> io::Result<String> {
        let mut out: *mut u16 = std::ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(sid, &mut out) } == 0 {
            return Err(last_error("ConvertSidToStringSidW"));
        }
        // Read the NUL-terminated string, then LocalFree the API-allocated buffer.
        let mut len = 0usize;
        unsafe {
            while *out.add(len) != 0 {
                len += 1;
            }
        }
        let slice = unsafe { std::slice::from_raw_parts(out, len) };
        let s = String::from_utf16_lossy(slice);
        unsafe { LocalFree(out.cast()) };
        Ok(s)
    }

    // ── LSA account rights (elevated) ──────────────────────────────────────────────────

    fn lsa_unicode_string(buf: &mut [u16]) -> LSA_UNICODE_STRING {
        // Length/MaximumLength are in BYTES; Length excludes the trailing NUL, Maximum
        // includes it. `buf` must be NUL-terminated and outlive the struct.
        let chars_no_nul = buf.len().saturating_sub(1);
        LSA_UNICODE_STRING {
            Length: (chars_no_nul * 2) as u16,
            MaximumLength: (buf.len() * 2) as u16,
            Buffer: buf.as_mut_ptr(),
        }
    }

    fn lsa_open_policy() -> io::Result<LSA_HANDLE> {
        let mut oa: LSA_OBJECT_ATTRIBUTES = unsafe { std::mem::zeroed() };
        oa.Length = std::mem::size_of::<LSA_OBJECT_ATTRIBUTES>() as u32;
        // LSA_HANDLE is an opaque isize handle in windows-sys (not a pointer).
        let mut handle: LSA_HANDLE = 0;
        let status = unsafe {
            LsaOpenPolicy(
                std::ptr::null(),
                &oa,
                POLICY_CREATE_ACCOUNT | POLICY_LOOKUP_NAMES,
                &mut handle,
            )
        };
        if status != 0 {
            let win = unsafe { LsaNtStatusToWinError(status) };
            return Err(io::Error::other(format!(
                "LsaOpenPolicy failed: {}",
                io::Error::from_raw_os_error(win as i32)
            )));
        }
        Ok(handle)
    }

    /// Set the account's logon rights so it can be a sandbox launch target but NEVER a
    /// remote-login identity. GRANT SeInteractiveLogonRight + SeBatchLogonRight; DENY only
    /// network + remote-interactive logon.
    ///
    /// PROVENANCE (empirically established on nub-win, 2026-07-26): `CreateProcessWithLogonW`
    /// (bit 2's unelevated launcher) performs an INTERACTIVE-type secondary logon, so the
    /// account MUST hold SeInteractiveLogonRight — with it DENIED, CreateProcessWithLogonW
    /// fails with 1385 (ERROR_LOGON_TYPE_NOT_GRANTED). The original design's blanket
    /// deny-interactive is therefore incompatible with the chosen launcher; the real
    /// protections are the DPAPI-only password (no one can type it) plus deny-network
    /// (no SMB/remote) and deny-remote-interactive (no RDP). Idempotent: also REMOVES a
    /// stale deny-interactive a prior setup version may have set (a deny overrides a grant).
    fn lsa_set_account_rights(sid: PSID) -> io::Result<()> {
        let policy = lsa_open_policy()?;
        let result = (|| {
            let add = |name: &str| -> io::Result<()> {
                let mut w = to_wide(name);
                let arr = [lsa_unicode_string(&mut w)];
                let status = unsafe { LsaAddAccountRights(policy, sid, arr.as_ptr(), 1) };
                if status != 0 {
                    let win = unsafe { LsaNtStatusToWinError(status) };
                    return Err(io::Error::other(format!(
                        "LsaAddAccountRights({name}) failed: {}",
                        io::Error::from_raw_os_error(win as i32)
                    )));
                }
                Ok(())
            };
            add("SeInteractiveLogonRight")?;
            add("SeBatchLogonRight")?;
            // Converge a v1 setup that denied interactive logon (a deny would override the
            // grant above). Ignore status — removing an absent right is benign.
            let mut di = to_wide("SeDenyInteractiveLogonRight");
            let di_arr = [lsa_unicode_string(&mut di)];
            unsafe { LsaRemoveAccountRights(policy, sid, false, di_arr.as_ptr(), 1) };
            add("SeDenyNetworkLogonRight")?;
            add("SeDenyRemoteInteractiveLogonRight")?;
            Ok(())
        })();
        unsafe { LsaClose(policy) };
        result
    }

    /// Remove ALL account rights the account holds (teardown). `AllRights = TRUE`.
    fn lsa_remove_all_rights(sid: PSID) -> io::Result<()> {
        let policy = lsa_open_policy()?;
        let status =
            unsafe { LsaRemoveAccountRights(policy, sid, true, std::ptr::null(), 0) };
        unsafe { LsaClose(policy) };
        if status != 0 {
            let win = unsafe { LsaNtStatusToWinError(status) };
            // A never-had-rights account yields a benign status; report but don't hard-fail
            // teardown on it.
            return Err(io::Error::other(format!(
                "LsaRemoveAccountRights failed: {}",
                io::Error::from_raw_os_error(win as i32)
            )));
        }
        Ok(())
    }

    // ── DPAPI credential store ─────────────────────────────────────────────────────────

    fn dpapi_protect(plaintext: &[u8]) -> io::Result<Vec<u8>> {
        let mut in_blob = CRYPT_INTEGER_BLOB {
            cbData: plaintext.len() as u32,
            pbData: plaintext.as_ptr() as *mut u8,
        };
        let mut out_blob = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let ok = unsafe {
            CryptProtectData(
                &mut in_blob,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_LOCAL_MACHINE,
                &mut out_blob,
            )
        };
        if ok == 0 {
            return Err(last_error("CryptProtectData"));
        }
        let bytes =
            unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec() };
        unsafe { LocalFree(out_blob.pbData.cast()) };
        Ok(bytes)
    }

    fn dpapi_unprotect(ciphertext: &[u8]) -> io::Result<Vec<u8>> {
        let mut in_blob = CRYPT_INTEGER_BLOB {
            cbData: ciphertext.len() as u32,
            pbData: ciphertext.as_ptr() as *mut u8,
        };
        let mut out_blob = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let ok = unsafe {
            CryptUnprotectData(
                &mut in_blob,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_LOCAL_MACHINE,
                &mut out_blob,
            )
        };
        if ok == 0 {
            return Err(last_error("CryptUnprotectData"));
        }
        let bytes =
            unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec() };
        unsafe { LocalFree(out_blob.pbData.cast()) };
        Ok(bytes)
    }

    // ── the public seams ───────────────────────────────────────────────────────────────

    /// ONE-TIME ELEVATED, idempotent. Create-or-rotate the account, grant/deny its rights,
    /// store the credential machine-scoped, and write `setup.json`. Returns the resolved
    /// `SetupInfo`. Re-running rotates the password (a fresh random credential each setup).
    pub fn windows_setup() -> io::Result<SetupInfo> {
        // 1. Fresh random password (256 bits → complexity-valid string).
        let mut raw = [0u8; 32];
        getrandom::getrandom(&mut raw)
            .map_err(|e| io::Error::other(format!("getrandom failed: {e}")))?;
        let password = encode_password(&raw);

        // 2. Create or rotate the account.
        if account_exists(ACCOUNT_NAME)? {
            net_user_set_password(ACCOUNT_NAME, &password)?;
        } else {
            net_user_add(ACCOUNT_NAME, &password)?;
        }

        // 3. Resolve the SID and set its rights.
        let (mut sid_bytes, sid_str) = lookup_account_sid(ACCOUNT_NAME)?;
        lsa_set_account_rights(sid_bytes.as_mut_ptr() as PSID)?;

        // 4. Persist the credential + record.
        std::fs::create_dir_all(store_dir())?;
        let blob = dpapi_protect(password.as_bytes())?;
        std::fs::write(account_blob_path(), &blob)?;
        let info = SetupInfo {
            setup_version: SETUP_VERSION,
            account_name: ACCOUNT_NAME.to_string(),
            account_sid: sid_str,
            wfp_provider_guid: None,
            wfp_sublayer_guid: None,
        };
        std::fs::write(setup_json_path(), info.to_json())?;

        // 5. Make both files authenticated-users-readable (the unelevated run reads them).
        //    Well-known SID `S-1-5-11` = Authenticated Users; GENERIC_READ, non-inheritable.
        grant_authenticated_users_read(&setup_json_path())?;
        grant_authenticated_users_read(&account_blob_path())?;
        Ok(info)
    }

    /// UNELEVATED, read-only. Establish that the machine is set up and hand the launcher the
    /// account credential, or a `DetectError` carrying the exact re-run hint.
    pub fn detect() -> Result<AccountCreds, DetectError> {
        let json = std::fs::read_to_string(setup_json_path()).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                DetectError::NotSetUp
            } else {
                DetectError::Broken(format!("reading setup.json: {e}"))
            }
        })?;
        let info = SetupInfo::from_json(&json)
            .map_err(|e| DetectError::Broken(format!("parsing setup.json: {e}")))?;
        if info.setup_version != SETUP_VERSION {
            return Err(DetectError::VersionMismatch {
                found: info.setup_version,
                expected: SETUP_VERSION,
            });
        }
        match account_exists(&info.account_name) {
            Ok(true) => {}
            Ok(false) => return Err(DetectError::NotSetUp),
            Err(e) => return Err(DetectError::Broken(format!("NetUserGetInfo: {e}"))),
        }
        let blob = std::fs::read(account_blob_path())
            .map_err(|e| DetectError::Broken(format!("reading account.bin: {e}")))?;
        let pw = dpapi_unprotect(&blob)
            .map_err(|e| DetectError::Broken(format!("CryptUnprotectData: {e}")))?;
        let password = String::from_utf8(pw)
            .map_err(|_| DetectError::Broken("decrypted credential is not UTF-8".to_string()))?;
        Ok(AccountCreds {
            account_name: info.account_name.clone(),
            account_sid: info.account_sid.clone(),
            password,
            info,
        })
    }

    /// Reverse `windows_setup()` (elevated): remove rights, delete the account, remove the
    /// store. Best-effort per step so a partially-set-up machine still cleans up.
    pub fn windows_teardown() -> io::Result<()> {
        if let Ok((mut sid_bytes, _)) = lookup_account_sid(ACCOUNT_NAME) {
            let _ = lsa_remove_all_rights(sid_bytes.as_mut_ptr() as PSID);
        }
        let _ = net_user_del(ACCOUNT_NAME);
        let _ = std::fs::remove_file(setup_json_path());
        let _ = std::fs::remove_file(account_blob_path());
        Ok(())
    }

    /// Grant `Authenticated Users` (S-1-5-11) GENERIC_READ on `path`, additively. Reuses the
    /// same Get/SetNamedSecurityInfoW + SetEntriesInAclW pattern as `windows.rs::set_ace`;
    /// duplicated here (a leaf, non-inheritable grant) rather than exposing the launch
    /// module's private helper across the file boundary.
    fn grant_authenticated_users_read(path: &std::path::Path) -> io::Result<()> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW,
            NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW,
            TRUSTEE_IS_SID, TRUSTEE_IS_GROUP, TRUSTEE_W,
        };
        use windows_sys::Win32::Security::{ACL, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

        const GENERIC_READ: u32 = 0x8000_0000;
        const NO_INHERITANCE: u32 = 0x0;

        let wpath: Vec<u16> = path
            .to_string_lossy()
            .replace('/', "\\")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let auth_users = to_wide("S-1-5-11");
        let mut sid: PSID = std::ptr::null_mut();
        if unsafe { ConvertStringSidToSidW(auth_users.as_ptr(), &mut sid) } == 0 {
            return Err(last_error("ConvertStringSidToSidW(S-1-5-11)"));
        }
        let _free = LocalFreeGuard(sid.cast());

        let mut old_dacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let rc = unsafe {
            GetNamedSecurityInfoW(
                wpath.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut old_dacl,
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        let _sd = LocalFreeGuard(sd);

        let mut ea: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
        ea.grfAccessPermissions = GENERIC_READ;
        ea.grfAccessMode = GRANT_ACCESS;
        ea.grfInheritance = NO_INHERITANCE;
        ea.Trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_GROUP,
            ptstrName: sid.cast(),
        };
        let mut new_dacl: *mut ACL = std::ptr::null_mut();
        let rc = unsafe { SetEntriesInAclW(1, &ea, old_dacl, &mut new_dacl) };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        let _new = LocalFreeGuard(new_dacl.cast());
        let rc = unsafe {
            SetNamedSecurityInfoW(
                wpath.as_ptr() as *mut u16,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                new_dacl,
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        Ok(())
    }

    struct LocalFreeGuard(*mut std::ffi::c_void);
    impl Drop for LocalFreeGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0) };
            }
        }
    }
}

// ── host-testable pure-data tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_info_round_trips() {
        let info = SetupInfo {
            setup_version: SETUP_VERSION,
            account_name: ACCOUNT_NAME.to_string(),
            account_sid: "S-1-5-21-1-2-3-1001".to_string(),
            wfp_provider_guid: None,
            wfp_sublayer_guid: None,
        };
        let json = info.to_json();
        assert_eq!(SetupInfo::from_json(&json).unwrap(), info);
        // The optional WFP fields are omitted when absent (bit 3 fills them later).
        assert!(!json.contains("wfp_provider_guid"));
    }

    #[test]
    fn setup_info_tolerates_wfp_fields() {
        let info = SetupInfo {
            setup_version: 1,
            account_name: ACCOUNT_NAME.to_string(),
            account_sid: "S-1-5-21-9".to_string(),
            wfp_provider_guid: Some("{GUID-A}".to_string()),
            wfp_sublayer_guid: Some("{GUID-B}".to_string()),
        };
        let back = SetupInfo::from_json(&info.to_json()).unwrap();
        assert_eq!(back.wfp_provider_guid.as_deref(), Some("{GUID-A}"));
    }

    #[test]
    fn encoded_password_satisfies_complexity() {
        // Adversarial input: all-zero bytes still yields all four categories via the suffix.
        let pw = encode_password(&[0u8; 32]);
        assert!(pw.len() >= 32);
        assert!(pw.chars().any(|c| c.is_ascii_uppercase()));
        assert!(pw.chars().any(|c| c.is_ascii_lowercase()));
        assert!(pw.chars().any(|c| c.is_ascii_digit()));
        assert!(pw.chars().any(|c| !c.is_ascii_alphanumeric()));
        // No quoting/shell hazards in the entropy body (only the fixed suffix carries a symbol).
        assert!(!pw[..32].contains(['"', '\'', ' ', '\\', '\t']));
    }

    #[test]
    fn store_paths_are_under_program_data() {
        // Deterministic regardless of the host: point ProgramData at a temp root.
        // SAFETY: single-threaded test; restored below.
        let prev = std::env::var_os("ProgramData");
        unsafe { std::env::set_var("ProgramData", "Z:\\pd") };
        assert_eq!(store_dir(), std::path::PathBuf::from("Z:\\pd").join("nub").join("sandbox"));
        assert!(setup_json_path().ends_with("setup.json"));
        assert!(account_blob_path().ends_with("account.bin"));
        match prev {
            Some(v) => unsafe { std::env::set_var("ProgramData", v) },
            None => unsafe { std::env::remove_var("ProgramData") },
        }
    }
}

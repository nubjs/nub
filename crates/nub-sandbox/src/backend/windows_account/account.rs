//! The sandbox account's lifecycle and its credential store.
//!
//! THE PRIVILEGE SPLIT LIVES HERE. [`provision`] and [`deprovision`] are the ELEVATED
//! one-time halves — SAM writes (`NetUserAdd`, `NetLocalGroupAddMembers`) and an `HKLM` value
//! all demand administrator. [`lookup_sid`] and [`load_credential`] are the UNELEVATED
//! per-run halves the broker calls on every launch. Keeping that split visible in this file's
//! signatures is what stops a per-run path from quietly acquiring an elevation requirement
//! (see [`super`]'s module doc: every run after setup is unelevated).
//!
//! WHY THE CREDENTIAL IS DPAPI **MACHINE** SCOPE. The elevated setup writes the blob and the
//! unelevated broker reads it. Those are the same *user* but DIFFERENT LOGON SESSIONS, and a
//! self-elevated child may not have the user's master key loaded at all — user-scope DPAPI
//! does not round-trip across that split, machine scope does. The honest consequence is that
//! machine scope is **not a security boundary**: any local principal that can READ the
//! ciphertext can decrypt it, the sandbox account included. The directory's DACL is the only
//! gate, and this module never writes a DACL — [`credential_dir`] exists so the setup path can
//! hand the directory to the acl module (which owns every DACL write) and have it locked down
//! BEFORE [`provision`] writes a credential into it.
//!
//! Mirrors SRT's `vendor/srt-win-src/src/{user,sam,dpapi}.rs` (read 2026-07-24); Codex's
//! `windows-sandbox-rs/src/bin/setup_main/win/sandbox_users.rs` is the second reference.

#![cfg(target_os = "windows")]

use super::{SANDBOX_ACCOUNT, SANDBOX_GROUP};
use std::io;
use std::path::PathBuf;
use std::ptr::{from_mut, from_ref, null, null_mut};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER,
    ERROR_MEMBER_IN_ALIAS, ERROR_NONE_MAPPED, ERROR_SUCCESS, GetLastError, HANDLE, LocalFree,
};
use windows_sys::Win32::NetworkManagement::NetManagement::{
    LOCALGROUP_INFO_1, LOCALGROUP_MEMBERS_INFO_0, NERR_GroupExists, NERR_GroupNotFound,
    NERR_PasswordTooShort, NERR_UserExists, NERR_UserNotFound, NetApiBufferFree, NetLocalGroupAdd,
    NetLocalGroupAddMembers, NetLocalGroupDel, NetUserAdd, NetUserDel, NetUserGetInfo,
    NetUserModalsGet, NetUserSetInfo, UF_DONT_EXPIRE_PASSWD, UF_SCRIPT, USER_INFO_1,
    USER_INFO_1003, USER_INFO_1008, USER_MODALS_INFO_0, USER_PRIV_USER,
};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom, CRYPT_INTEGER_BLOB,
    CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, LookupAccountNameW, LookupAccountSidW, PSID, SID_NAME_USE,
    TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE, RegCloseKey,
    RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::DeleteProfileW;

/// `BUILTIN\Users`. The well-known SID is stable across locales; the *name* is not
/// ("Benutzer" on de-DE, "Utilisateurs" on fr-FR), which is why membership goes through a
/// reverse lookup rather than a literal.
const SID_BUILTIN_USERS: &str = "S-1-5-32-545";

const WINLOGON_USERLIST: &str =
    r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\SpecialAccounts\UserList";

/// Benign-idempotency codes `windows-sys` has no constant for.
const ERROR_ALIAS_EXISTS: u32 = 1379;
const ERROR_NO_SUCH_ALIAS: u32 = 1376;

const MAX_PW_ATTEMPTS: usize = 5;
const PW_LEN: usize = 32;

// ───────────────────────────── public surface ─────────────────────────────

/// ELEVATED. Create-or-repair the sandbox account and its group, rotate the credential, and
/// store it. Idempotent — re-running repairs a half-provisioned machine (account present but
/// out of `BUILTIN\Users`, flags cleared by a GPO, credential file deleted). Returns the
/// account's SID, the identity every downstream object keys on: WFP's `ALE_USER_ID`
/// descriptor and every granted or denied ACE.
pub(crate) fn provision() -> io::Result<String> {
    ensure_group()?;
    let password = ensure_user()?;

    let sid = lookup_sid()?.ok_or_else(|| {
        io::Error::other(format!(
            "the {SANDBOX_ACCOUNT} account was created but does not resolve to a SID"
        ))
    })?;
    let psid = LocalSid::parse(&sid)?;

    // `BUILTIN\Users` is REQUIRED, not cosmetic: it carries the interactive-logon right that
    // `CreateProcessWithLogonW` depends on, and without it every launch fails at logon.
    let builtin_users = lookup_account_name(SID_BUILTIN_USERS)?;
    add_member(&builtin_users, &psid)?;
    add_member(SANDBOX_GROUP, &psid)?;

    // Deliberately NOT granting `SeDenyInteractiveLogonRight`: `CreateProcessWithLogonW` goes
    // through an INTERACTIVE-type logon, so a deny-interactive right plausibly breaks the
    // launch outright. Neither SRT nor Codex sets one. Revisit only against a real Windows box
    // that can prove the launch survives it.
    set_hidden(true)?;

    store_credential(password.as_str())
        .map(|()| sid)
        .map_err(|e| io::Error::new(e.kind(), format!("storing the sandbox credential: {e}")))
}

/// ELEVATED. Remove the account, its profile, the group, the Winlogon hide entry, and the
/// credential file. Every step tolerates already-absent state and runs even after an earlier
/// step failed, so a partially-provisioned or crash-interrupted machine still gets cleaned as
/// far as it can; the FIRST failure is what surfaces.
pub(crate) fn deprovision() -> io::Result<()> {
    // Resolve BEFORE `NetUserDel` destroys the SAM mapping — `DeleteProfileW` takes the SID
    // *string* and there is no route back to it once the account is gone.
    let sid = lookup_sid().ok().flatten();

    let mut failure: Option<io::Error> = None;
    let mut record = |r: io::Result<()>| {
        if let Err(e) = r {
            let _ = failure.get_or_insert(e);
        }
    };

    if let Some(sid) = &sid {
        let sid_w = to_wide(sid);
        // Best-effort by design: Windows only materializes the profile on first logon, and a
        // stuck child can hold it open. Neither may block the account delete below.
        // SAFETY: NUL-terminated SID string; NULL profile path and NULL computer name select
        // the local machine's default profile, which is the documented form.
        unsafe { DeleteProfileW(sid_w.as_ptr(), null(), null()) };
    }

    let name_w = to_wide(SANDBOX_ACCOUNT);
    // SAFETY: NUL-terminated account name; NULL server means the local SAM.
    let rc = unsafe { NetUserDel(null(), name_w.as_ptr()) };
    if rc != 0 && rc != NERR_UserNotFound {
        record(Err(net_err("NetUserDel", rc)));
    }

    let group_w = to_wide(SANDBOX_GROUP);
    // SAFETY: as above.
    let rc = unsafe { NetLocalGroupDel(null(), group_w.as_ptr()) };
    if rc != 0 && rc != NERR_GroupNotFound && rc != ERROR_NO_SUCH_ALIAS {
        record(Err(net_err("NetLocalGroupDel", rc)));
    }

    record(set_hidden(false));
    record(
        credential_path().and_then(|p| match std::fs::remove_file(&p) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }),
    );

    match failure {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// UNELEVATED. The sandbox account's SID, or `None` when it does not exist. An access-denied
/// or transient LSA failure PROPAGATES — reporting "absent" for those would make an
/// already-provisioned machine look unprovisioned and trigger a spurious elevation prompt.
pub(crate) fn lookup_sid() -> io::Result<Option<String>> {
    lookup_account_sid(SANDBOX_ACCOUNT)
}

/// UNELEVATED. Decrypt the stored credential into a [`Secret`], which zeroes itself on drop.
///
/// Every buffer the plaintext passes through is zeroed: DPAPI's own output block (in
/// [`take_blob`], before it is freed), the decoded copy here, the `Secret` on drop, and the
/// UTF-16 relay the launch builds for `CreateProcessWithLogonW`. That bounds how long the
/// password sits readable in nub's heap; it is not a defence against a process-memory attacker.
pub(crate) fn load_credential() -> io::Result<Secret> {
    let path = credential_path()?;
    let ciphertext = std::fs::read(&path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "cannot read the sandbox credential at {} ({e}) — the one-time elevated sandbox \
                 setup has not run on this machine",
                path.display()
            ),
        )
    })?;
    let mut plaintext = dpapi_unprotect(&ciphertext)?;
    let out = std::str::from_utf8(&plaintext)
        .map(|s| Secret(s.to_owned()))
        .map_err(|_| io::Error::other("the stored sandbox credential is corrupt (not UTF-8)"));
    scrub_u8(&mut plaintext);
    out
}

/// The credential store's directory — the SAME directory as the marker and the ledger, which is
/// why it delegates rather than recomputing the path: setup locks whatever THIS returns, so a
/// second definition that drifted would protect a directory the credential is not in.
///
/// Machine-scope DPAPI protects nothing on its own, so that directory's owner + DACL are the
/// whole boundary: the setup path hands it to [`super::acl::lock_to_admins`] BEFORE any
/// credential is written into it. This module never writes a DACL.
pub(crate) fn credential_dir() -> io::Result<PathBuf> {
    super::state::state_dir()
}

pub(crate) fn credential_path() -> io::Result<PathBuf> {
    Ok(credential_dir()?.join("credential.bin"))
}

/// Whether this process holds an ELEVATED (full-admin) token — the exact condition under
/// which the SAM and `HKLM` writes in [`provision`] succeed. A standard user and an admin's
/// filtered Medium-IL token both report `false`, and both would get `ERROR_ACCESS_DENIED`.
pub(crate) fn is_elevated() -> bool {
    let mut token: HANDLE = null_mut();
    // SAFETY: query-only handle into our own process token.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut ret_len: u32 = 0;
    // SAFETY: `elevation` is a correctly-sized TOKEN_ELEVATION out-buffer.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            from_mut(&mut elevation).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
    };
    // SAFETY: `token` came from a successful OpenProcessToken and is closed once.
    unsafe { CloseHandle(token) };
    ok != 0 && elevation.TokenIsElevated != 0
}

// ───────────────────────────── errors, scratch, RAII ─────────────────────────────

/// Access-denied on any of these operations means "you are not elevated", not a fault — the
/// caller turns `PermissionDenied` into an actionable re-run-elevated message rather than
/// surfacing a bare numeric code.
fn net_err(op: &str, rc: u32) -> io::Error {
    if rc == ERROR_ACCESS_DENIED {
        return io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{op} failed: administrator rights are required to manage the \
                 {SANDBOX_ACCOUNT} account"
            ),
        );
    }
    io::Error::other(format!("{op} failed (status {rc})"))
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn scrub_u8(buf: &mut [u8]) {
    for b in buf {
        // SAFETY: `b` is a live unique reference. Volatile so the store survives a compiler
        // that would otherwise treat writing to a dying buffer as dead code.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
}

/// Also the launch's own UTF-16 relay into `CreateProcessWithLogonW`, which is why this is
/// `pub(crate)`: a plain `fill(0)` on a buffer about to be dropped is a dead store the
/// optimizer may delete, and the password would survive in the heap.
pub(crate) fn scrub_u16(buf: &mut [u16]) {
    for b in buf {
        // SAFETY: as above.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
}

/// A plaintext credential zeroed on drop. NOT a defence against a process-memory attacker
/// (the value was copied at least once on the way in) — it bounds how long the password sits
/// readable in nub's heap, which is the part this module controls.
pub(crate) struct Secret(String);

impl Secret {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // SAFETY: zeroing every byte keeps the buffer valid UTF-8 (NUL is ASCII), so the
        // `String` invariant holds.
        scrub_u8(unsafe { self.0.as_bytes_mut() });
    }
}

/// The UTF-16 form the `USER_INFO_*` structs point at, zeroed on drop for the same reason.
struct WideSecret(Vec<u16>);

impl Drop for WideSecret {
    fn drop(&mut self) {
        scrub_u16(&mut self.0);
    }
}

/// A `PSID` minted by `ConvertStringSidToSidW`, released with `LocalFree` — never `FreeSid`,
/// which is only valid for SIDs built by `AllocateAndInitializeSid`.
struct LocalSid(PSID);

impl LocalSid {
    fn parse(sid: &str) -> io::Result<Self> {
        let w = to_wide(sid);
        let mut psid: PSID = null_mut();
        // SAFETY: `w` is NUL-terminated UTF-16; `psid` is a valid out-slot.
        let ok = unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut psid) };
        if ok == 0 {
            // SAFETY: read immediately after the failed call on this thread.
            let rc = unsafe { GetLastError() };
            return Err(net_err(&format!("ConvertStringSidToSidW({sid})"), rc));
        }
        Ok(LocalSid(psid))
    }
}

impl Drop for LocalSid {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `ConvertStringSidToSidW` documents `LocalFree` as the release.
            unsafe { LocalFree(self.0) };
        }
    }
}

// ───────────────────────────── SID lookups ─────────────────────────────

/// Two-call `LookupAccountNameW`. `ERROR_NONE_MAPPED` is the "no such account" answer and
/// becomes `Ok(None)`; everything else is a real failure.
fn lookup_account_sid(name: &str) -> io::Result<Option<String>> {
    let name_w = to_wide(name);
    let mut cb_sid: u32 = 0;
    let mut cch_dom: u32 = 0;
    let mut kind: SID_NAME_USE = 0;

    // SAFETY: the documented sizing form — NULL buffers with zeroed lengths.
    let ok = unsafe {
        LookupAccountNameW(
            null(),
            name_w.as_ptr(),
            null_mut(),
            &mut cb_sid,
            null_mut(),
            &mut cch_dom,
            &mut kind,
        )
    };
    if ok == 0 {
        // SAFETY: read immediately after the failed call on this thread.
        let rc = unsafe { GetLastError() };
        if rc == ERROR_NONE_MAPPED {
            return Ok(None);
        }
        if rc != ERROR_INSUFFICIENT_BUFFER {
            return Err(net_err(&format!("LookupAccountNameW({name})"), rc));
        }
    }
    if cb_sid == 0 {
        return Ok(None);
    }

    // `u32` backing, not `u8`: a SID's `SubAuthority` array is `DWORD`-typed, so the buffer
    // must be DWORD-aligned — which `Vec<u8>` does not promise.
    let mut sid = vec![0u32; cb_sid.div_ceil(4) as usize];
    let mut dom = vec![0u16; cch_dom.max(1) as usize];
    // SAFETY: both buffers are sized by the call above and outlive this one.
    let ok = unsafe {
        LookupAccountNameW(
            null(),
            name_w.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut cb_sid,
            dom.as_mut_ptr(),
            &mut cch_dom,
            &mut kind,
        )
    };
    if ok == 0 {
        // SAFETY: as above.
        let rc = unsafe { GetLastError() };
        if rc == ERROR_NONE_MAPPED {
            return Ok(None);
        }
        return Err(net_err(&format!("LookupAccountNameW({name})"), rc));
    }
    // SAFETY: the buffer now holds a valid self-relative SID.
    Ok(Some(unsafe { sid_to_string(sid.as_mut_ptr().cast()) }?))
}

/// Reverse lookup, used to resolve a well-known SID to its LOCALIZED account name so the
/// name-taking SAM calls work on non-English Windows.
fn lookup_account_name(sid_str: &str) -> io::Result<String> {
    let sid = LocalSid::parse(sid_str)?;
    let mut cch_name: u32 = 0;
    let mut cch_dom: u32 = 0;
    let mut kind: SID_NAME_USE = 0;

    // SAFETY: sizing call against a live PSID; both out-lengths are valid slots.
    unsafe {
        LookupAccountSidW(
            null(),
            sid.0,
            null_mut(),
            &mut cch_name,
            null_mut(),
            &mut cch_dom,
            &mut kind,
        )
    };
    if cch_name == 0 {
        return Err(io::Error::other(format!(
            "LookupAccountSidW({sid_str}) sizing returned 0"
        )));
    }
    let mut name = vec![0u16; cch_name as usize];
    let mut dom = vec![0u16; cch_dom.max(1) as usize];
    // SAFETY: both buffers are sized by the call above and outlive this one.
    let ok = unsafe {
        LookupAccountSidW(
            null(),
            sid.0,
            name.as_mut_ptr(),
            &mut cch_name,
            dom.as_mut_ptr(),
            &mut cch_dom,
            &mut kind,
        )
    };
    if ok == 0 {
        // SAFETY: read immediately after the failed call on this thread.
        let rc = unsafe { GetLastError() };
        return Err(net_err(&format!("LookupAccountSidW({sid_str})"), rc));
    }
    Ok(String::from_utf16_lossy(&name[..cch_name as usize]))
}

/// Shared with the AppContainer backend, which holds its container SID as a raw `PSID` but
/// needs the string form to ace the window station — see `WindowStationAceGuard` in
/// `backend/windows.rs`.
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

// ───────────────────────────── password ─────────────────────────────

/// The 85-symbol alphabet. It EXCLUDES `"`, `\`, backtick, whitespace and the shell-special
/// `& | < > ^` set, so the credential survives any cmd / PowerShell / argv relay between here
/// and `CreateProcessWithLogonW`. 32 chars ≈ 205 bits; Windows caps a local account at 127.
const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ\
                       abcdefghijklmnopqrstuvwxyz\
                       0123456789!#$%()*+,-./:;=?@[]_{}~";

const CLASSES: [&[u8]; 4] = [
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZ",
    b"abcdefghijklmnopqrstuvwxyz",
    b"0123456789",
    b"!#$%()*+,-./:;=?@[]_{}~",
];

fn fill_random(buf: &mut [u8]) -> io::Result<()> {
    // SAFETY: `buf` is a live writable slice of exactly `len()` bytes; a NULL algorithm handle
    // is what BCRYPT_USE_SYSTEM_PREFERRED_RNG requires.
    let st = unsafe {
        BCryptGenRandom(
            null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if st != 0 {
        return Err(io::Error::other(format!(
            "BCryptGenRandom failed (NTSTATUS {st:#010x})"
        )));
    }
    Ok(())
}

/// 32 chars rejection-sampled from [`ALPHA`] via the system CSPRNG (never a userspace PRNG).
/// Rejection sampling is what makes each pick UNIFORM — 85 does not divide 256, so a bare
/// `% 85` would bias the first 86 symbols.
fn gen_password() -> io::Result<Secret> {
    let mut raw = [0u8; PW_LEN];
    fill_random(&mut raw)?;
    let bound = (u8::MAX - (u8::MAX % ALPHA.len() as u8)) as usize;
    let mut out = Vec::with_capacity(PW_LEN);
    let mut i = 0usize;
    while out.len() < PW_LEN {
        if i == raw.len() {
            fill_random(&mut raw)?;
            i = 0;
        }
        let b = raw[i] as usize;
        i += 1;
        if b < bound {
            out.push(ALPHA[b % ALPHA.len()]);
        }
    }
    scrub_u8(&mut raw);

    let mut extra = [0u8; 5];
    fill_random(&mut extra)?;
    apply_class_floor(&mut out, &extra);
    scrub_u8(&mut extra);
    Ok(Secret(String::from_utf8(out).expect("ALPHA is ASCII")))
}

/// Force one character from each complexity class when the uniform draw missed one. The 2245
/// retry loop in [`ensure_user`] is the primary defence against a tightened local policy;
/// this makes the FIRST attempt pass almost always.
fn apply_class_floor(out: &mut [u8], extra: &[u8; 5]) {
    if CLASSES.iter().all(|c| out.iter().any(|b| c.contains(b))) {
        return;
    }
    let base = extra[0] as usize;
    for (k, class) in CLASSES.iter().enumerate() {
        let slot = (base + k) % out.len();
        out[slot] = class[extra[1 + k] as usize % class.len()];
    }
}

/// The rejected draw's CLASS COMPOSITION — never the password itself. This is what makes a
/// 2245 on a user's machine diagnosable from logs alone.
fn class_summary(password: &str) -> String {
    let (mut u, mut l, mut d, mut s) = (0u32, 0u32, 0u32, 0u32);
    for b in password.bytes() {
        match b {
            b'A'..=b'Z' => u += 1,
            b'a'..=b'z' => l += 1,
            b'0'..=b'9' => d += 1,
            _ => s += 1,
        }
    }
    format!("len={} U={u} L={l} D={d} S={s}", password.len())
}

/// `(min_password_len, password_history_len)` from the local SAM, `(-1, -1)` when unreadable.
fn local_password_policy() -> (i64, i64) {
    let mut buf: *mut u8 = null_mut();
    // SAFETY: NULL server = local SAM; `buf` is a valid out-slot.
    if unsafe { NetUserModalsGet(null(), 0, &mut buf) } != 0 || buf.is_null() {
        return (-1, -1);
    }
    // SAFETY: on success netapi returns one USER_MODALS_INFO_0 in its own buffer.
    let m = unsafe { *buf.cast::<USER_MODALS_INFO_0>() };
    // SAFETY: the buffer came from NetUserModalsGet and is freed exactly once.
    unsafe { NetApiBufferFree(buf.cast()) };
    (
        i64::from(m.usrmod0_min_passwd_len),
        i64::from(m.usrmod0_password_hist_len),
    )
}

fn warn_password_rejected(op: &str, attempt: usize, password: &str) {
    let (min_len, hist) = local_password_policy();
    tracing::warn!(
        "nub sandbox: {op} rejected a generated password (NERR_PasswordTooShort, attempt {}/{MAX_PW_ATTEMPTS}); \
         local policy min_len={min_len} history={hist}; rejected draw {}; retrying",
        attempt + 1,
        class_summary(password)
    );
}

// ───────────────────────────── account + group ─────────────────────────────

/// Create the account, or rotate the credential of the one already present.
///
/// Retries on `NERR_PasswordTooShort` (2245) because SAM returns that code for ANY local
/// password-policy rejection — length, history, or a third-party password-filter DLL — not
/// literally "too short". A policy tightened since the last provision will bounce an
/// otherwise-valid 32-char draw, and a fresh draw usually clears it.
fn ensure_user() -> io::Result<Secret> {
    let mut name_w = to_wide(SANDBOX_ACCOUNT);
    let mut comment_w = to_wide("nub: dedicated account for OS-enforced sandboxing");

    for attempt in 0..MAX_PW_ATTEMPTS {
        let password = gen_password()?;
        let mut pw = WideSecret(to_wide(password.as_str()));

        let info = USER_INFO_1 {
            usri1_name: name_w.as_mut_ptr(),
            usri1_password: pw.0.as_mut_ptr(),
            usri1_password_age: 0,
            usri1_priv: USER_PRIV_USER,
            usri1_home_dir: null_mut(),
            usri1_comment: comment_w.as_mut_ptr(),
            // UF_SCRIPT is MANDATORY on workstation SKUs — a vestigial LAN-Manager flag SAM
            // still insists on. Omit it and NetUserAdd fails with NERR_BadUsername /
            // ERROR_INVALID_PARAMETER, neither of which hints at the real cause.
            usri1_flags: UF_SCRIPT | UF_DONT_EXPIRE_PASSWD,
            usri1_script_path: null_mut(),
        };
        // SAFETY: every PWSTR field points into a buffer that outlives this call.
        let rc = unsafe { NetUserAdd(null(), 1, from_ref(&info).cast(), null_mut()) };
        if rc == 0 {
            return Ok(password);
        }
        if rc == NERR_PasswordTooShort && attempt + 1 < MAX_PW_ATTEMPTS {
            warn_password_rejected("NetUserAdd", attempt, password.as_str());
            continue;
        }
        if rc != NERR_UserExists {
            return Err(net_err("NetUserAdd", rc));
        }

        // Already present — rotate, so the credential file about to be rewritten matches the
        // live account. Levels 1003 (password) and 1008 (flags) rather than another level-1
        // SetInfo, which would clobber priv / home_dir / comment.
        let info = USER_INFO_1003 {
            usri1003_password: pw.0.as_mut_ptr(),
        };
        // SAFETY: `name_w` and the password buffer outlive this call.
        let rc = unsafe {
            NetUserSetInfo(
                null(),
                name_w.as_ptr(),
                1003,
                from_ref(&info).cast(),
                null_mut(),
            )
        };
        if rc == NERR_PasswordTooShort && attempt + 1 < MAX_PW_ATTEMPTS {
            warn_password_rejected("NetUserSetInfo(1003)", attempt, password.as_str());
            continue;
        }
        if rc != 0 {
            return Err(net_err("NetUserSetInfo(1003, password rotate)", rc));
        }
        reassert_flags(&name_w)?;
        return Ok(password);
    }
    Err(io::Error::other(format!(
        "the local password policy rejected {MAX_PW_ATTEMPTS} generated passwords \
         (NERR_PasswordTooShort)"
    )))
}

/// Re-OR `UF_DONT_EXPIRE_PASSWD` into an existing account's flags. An older nub or a GPO may
/// have cleared it since the last provision, and an expiring password silently breaks every
/// future launch with an opaque logon failure.
fn reassert_flags(name_w: &[u16]) -> io::Result<()> {
    let mut buf: *mut u8 = null_mut();
    // SAFETY: NUL-terminated name; `buf` is a valid out-slot.
    let rc = unsafe { NetUserGetInfo(null(), name_w.as_ptr(), 1, &mut buf) };
    if rc != 0 || buf.is_null() {
        return Err(net_err("NetUserGetInfo(1)", rc));
    }
    // SAFETY: on success netapi returns one USER_INFO_1 in its own buffer.
    let flags = unsafe { (*buf.cast::<USER_INFO_1>()).usri1_flags };
    // SAFETY: the buffer came from NetUserGetInfo and is freed exactly once.
    unsafe { NetApiBufferFree(buf.cast()) };

    let info = USER_INFO_1008 {
        usri1008_flags: flags | UF_DONT_EXPIRE_PASSWD,
    };
    // SAFETY: `info` and `name_w` both outlive the call.
    let rc = unsafe {
        NetUserSetInfo(
            null(),
            name_w.as_ptr(),
            1008,
            from_ref(&info).cast(),
            null_mut(),
        )
    };
    if rc != 0 {
        return Err(net_err("NetUserSetInfo(1008, flags)", rc));
    }
    Ok(())
}

fn ensure_group() -> io::Result<()> {
    let mut name_w = to_wide(SANDBOX_GROUP);
    let mut comment_w = to_wide("nub: holds the dedicated account used for OS-enforced sandboxing");
    let info = LOCALGROUP_INFO_1 {
        lgrpi1_name: name_w.as_mut_ptr(),
        lgrpi1_comment: comment_w.as_mut_ptr(),
    };
    // SAFETY: both PWSTR fields point into buffers that outlive this call.
    let rc = unsafe { NetLocalGroupAdd(null(), 1, from_ref(&info).cast(), null_mut()) };
    if rc != 0 && rc != NERR_GroupExists && rc != ERROR_ALIAS_EXISTS {
        return Err(net_err("NetLocalGroupAdd", rc));
    }
    Ok(())
}

/// Add `member` to the local group NAMED `group`, by PSID at level 0. Level 3 takes a name
/// and wants the literal `<ComputerName>\<name>` form — it rejects `.\name` with
/// `ERROR_NO_SUCH_MEMBER`, so the SID form is the only portable one.
fn add_member(group: &str, member: &LocalSid) -> io::Result<()> {
    let group_w = to_wide(group);
    let info = LOCALGROUP_MEMBERS_INFO_0 {
        lgrmi0_sid: member.0,
    };
    // SAFETY: `group_w` and the member's PSID both outlive this call.
    let rc =
        unsafe { NetLocalGroupAddMembers(null(), group_w.as_ptr(), 0, from_ref(&info).cast(), 1) };
    if rc != 0 && rc != ERROR_MEMBER_IN_ALIAS {
        return Err(net_err(&format!("NetLocalGroupAddMembers({group})"), rc));
    }
    Ok(())
}

/// Add or remove the Winlogon `SpecialAccounts\UserList` entry. COSMETIC ONLY — it keeps the
/// account off the sign-in picker and changes nothing about its rights; the account stays
/// fully usable through `CreateProcessWithLogonW`.
fn set_hidden(hide: bool) -> io::Result<()> {
    let sub_w = to_wide(WINLOGON_USERLIST);
    let val_w = to_wide(SANDBOX_ACCOUNT);
    let mut key: HKEY = null_mut();

    if hide {
        // Create, not open: the SpecialAccounts subtree does not exist on a stock install.
        // SAFETY: NUL-terminated subkey; `key` is a valid out-slot.
        let rc = unsafe {
            RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                sub_w.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                null(),
                &mut key,
                null_mut(),
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(net_err("RegCreateKeyExW(Winlogon SpecialAccounts)", rc));
        }
        let data = 0u32.to_ne_bytes();
        // SAFETY: `key` is open for KEY_SET_VALUE; `data` is the 4 bytes REG_DWORD requires.
        let rc = unsafe { RegSetValueExW(key, val_w.as_ptr(), 0, REG_DWORD, data.as_ptr(), 4) };
        // SAFETY: `key` came from a successful create and is closed once.
        unsafe { RegCloseKey(key) };
        if rc != ERROR_SUCCESS {
            return Err(net_err("RegSetValueExW(SpecialAccounts UserList)", rc));
        }
    } else {
        // Open, not create: if the key was never written there is nothing to remove.
        // SAFETY: NUL-terminated subkey; `key` is a valid out-slot.
        let rc = unsafe {
            RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                sub_w.as_ptr(),
                0,
                KEY_SET_VALUE,
                &mut key,
            )
        };
        if rc != ERROR_SUCCESS {
            return Ok(());
        }
        // SAFETY: `key` is open for KEY_SET_VALUE.
        let rc = unsafe { RegDeleteValueW(key, val_w.as_ptr()) };
        // SAFETY: `key` came from a successful open and is closed once.
        unsafe { RegCloseKey(key) };
        if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
            return Err(net_err("RegDeleteValueW(SpecialAccounts UserList)", rc));
        }
    }
    Ok(())
}

// ───────────────────────────── credential store ─────────────────────────────

fn store_credential(password: &str) -> io::Result<()> {
    let dir = credential_dir()?;
    std::fs::create_dir_all(&dir)?;
    let ciphertext = dpapi_protect(password.as_bytes())?;
    std::fs::write(credential_path()?, &ciphertext)
}

fn dpapi_protect(plaintext: &[u8]) -> io::Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: plaintext.len() as u32,
        pbData: plaintext.as_ptr().cast_mut(),
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `input` borrows a live slice for the call and `out` is a valid out-slot; every
    // optional parameter is NULL, which the API documents as "unused".
    let ok = unsafe {
        CryptProtectData(
            &input,
            null(),
            null(),
            null(),
            null(),
            CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success `out` describes a LocalAlloc'd buffer this call now owns.
    Ok(unsafe { take_blob(out) })
}

fn dpapi_unprotect(ciphertext: &[u8]) -> io::Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: ciphertext.len() as u32,
        pbData: ciphertext.as_ptr().cast_mut(),
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as above. No scope flag: DPAPI reads it back out of the blob header.
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            null_mut(),
            null(),
            null(),
            null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    Ok(unsafe { take_blob(out) })
}

/// # Safety
/// `out` must be a blob DPAPI filled in on a successful call; ownership transfers here.
///
/// The block is SCRUBBED before it is freed: on the unprotect path it holds the decrypted
/// password, and `LocalFree` only returns the pages to the heap.
unsafe fn take_blob(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    if out.pbData.is_null() {
        return Vec::new();
    }
    // SAFETY: DPAPI guarantees `cbData` readable+writable bytes at `pbData`, which this call
    // now owns exclusively.
    let block = unsafe { std::slice::from_raw_parts_mut(out.pbData, out.cbData as usize) };
    let v = block.to_vec();
    scrub_u8(block);
    // SAFETY: freed exactly once, and freed even when `cbData` is 0 — which the API can
    // return for an empty plaintext.
    unsafe { LocalFree(out.pbData.cast()) };
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The alphabet is the whole reason the credential survives a cmd / PowerShell / argv
    /// relay into `CreateProcessWithLogonW`. One stray metacharacter here becomes an
    /// intermittent, machine-specific logon failure.
    #[test]
    fn alphabet_excludes_every_shell_hostile_character() {
        assert_eq!(ALPHA.len(), 85, "the rejection bound assumes this size");
        for c in b"\"\\`' &|<>^" {
            assert!(!ALPHA.contains(c), "ALPHA contains {}", *c as char);
        }
        assert!(ALPHA.iter().all(u8::is_ascii_graphic));
        // Every class must be drawable from ALPHA, or the floor below writes a character the
        // uniform draw could never produce.
        for class in CLASSES {
            assert!(class.iter().all(|b| ALPHA.contains(b)));
        }
    }

    /// A tightened local complexity policy rejects a draw that happens to miss a class, and
    /// the retry loop then burns attempts on identically-shaped draws. The floor is what
    /// makes the first attempt pass.
    #[test]
    fn class_floor_repairs_a_draw_missing_every_class() {
        let mut out = [b'a'; PW_LEN];
        apply_class_floor(&mut out, &[7, 0, 0, 0, 0]);
        assert!(out.iter().any(u8::is_ascii_uppercase));
        assert!(out.iter().any(u8::is_ascii_lowercase));
        assert!(out.iter().any(u8::is_ascii_digit));
        assert!(out.iter().any(|b| !b.is_ascii_alphanumeric()));
    }

    /// A complete draw must be left ALONE — rewriting four slots with a pick derived from
    /// five bytes would shed entropy on every generated password.
    #[test]
    fn class_floor_leaves_a_complete_draw_untouched() {
        let mut out = *b"Aa1!Aa1!Aa1!Aa1!Aa1!Aa1!Aa1!Aa1!";
        let before = out;
        apply_class_floor(&mut out, &[3, 9, 9, 9, 9]);
        assert_eq!(out, before);
    }

    /// The 2245 diagnostic is logged on a user's machine; leaking the rejected password into
    /// it would be a credential disclosure in plain logs.
    #[test]
    fn class_summary_reports_composition_without_the_password() {
        let pw = "Aa1!Aa1!";
        let s = class_summary(pw);
        assert_eq!(s, "len=8 U=2 L=2 D=2 S=2");
        assert!(!s.contains(pw) && !s.contains("Aa1"));
    }

    /// End-to-end shape of the generator against the real system CSPRNG. Needs Windows but no
    /// elevation, so it runs on the ordinary CI leg.
    #[test]
    fn generated_password_is_ascii_complex_and_unique() {
        let p = gen_password().expect("gen_password");
        assert_eq!(p.as_str().len(), PW_LEN);
        assert!(p.as_str().bytes().all(|b| ALPHA.contains(&b)));
        for class in CLASSES {
            assert!(
                p.as_str().bytes().any(|b| class.contains(&b)),
                "missing a complexity class: {}",
                class_summary(p.as_str())
            );
        }
        assert_ne!(p.as_str(), gen_password().unwrap().as_str());
    }
}

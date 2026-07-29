//! Would a RESTRICTED TOKEN at low integrity read what an AppContainer cannot?
//!
//! THE QUESTION, AND WHY IT IS THE ARCHITECTURAL ONE. The build jail's AppContainer fails on every
//! operation that opens an ancestor directory as a target — `realpathSync`, `process.chdir`,
//! `find-up`-shaped upward walks, `_nodeModulePaths` probing. The cause is DISCRETIONARY: a per-run
//! AppContainer profile gets a brand-new SID that appears in no existing DACL, so nothing is
//! readable until nub writes an ACE, and above `%USERPROFILE%` it cannot — `C:\` is owned by
//! TrustedInstaller and `C:\Users` by SYSTEM, and neither grants a standard group WRITE_DAC
//! (measured read-only across three images).
//!
//! A restricted token is a different mechanism with a different failure surface: it keeps the
//! user's OWN sid, so the DACLs that already grant that user read still apply, and INTEGRITY —
//! a mandatory mechanism, orthogonal to the DACL — is what confines it. Windows' default object
//! mandatory policy is `NO_WRITE_UP` alone (the three flags are separate; see
//! `sandbox/win/src/acl.h` in the vendored Chromium sandbox), so a low-integrity process is
//! expected to read up and not write up. If that holds, reads need no ACE anywhere and the
//! ancestor problem is not a problem.
//!
//! WHY `AccessCheck` RATHER THAN A LAUNCH. `AccessCheck` is the OS's own evaluator, applied to a
//! real token against a real security descriptor, and it answers the DACL-plus-integrity question
//! without a process. That sidesteps three things that would otherwise have to work first — the
//! `CreateProcessAsUser` privilege question, a window-station/desktop ACE, and the loader-init
//! failure a mislabelled station produces — none of which bear on whether the ACCESS would be
//! granted. It is a model of the check rather than the check in situ, which is exactly why the
//! baseline arm exists: an unrestricted token must come back GRANTED on the same paths, or the
//! harness is measuring its own mistake.
//!
//! WHAT THIS DOES NOT ANSWER, stated so no one reads more into a green run than is there: whether
//! such a child can be LAUNCHED unprivileged, whether it can be confined to a Job, and what blocks
//! egress once the AppContainer's withheld `internetClient` capability is no longer the mechanism.
//! Those are separate and are not measured here.
//!
//! Every token is derived from THIS process's own token, and integrity is only ever LOWERED —
//! `CreateRestrictedToken` leaves the level untouched, and lowering never needs a privilege (only
//! raising does). So nothing here requires elevation by construction; the runner's own elevation is
//! reported as a fact so an elevated baseline is never mistaken for the shipping case.
//!
//! Branch-scoped via `.github/workflows/win-restricted-token-probe.yml`, no pull request.

#[cfg(not(target_os = "windows"))]
fn main() {
    // Non-Windows host: nothing to measure. (`harness = false` needs a `main`.)
}

#[cfg(target_os = "windows")]
fn main() {
    std::process::exit(win::probe_main());
}

#[cfg(target_os = "windows")]
mod win {
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSidToSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        AccessCheck, CreateRestrictedToken, DACL_SECURITY_INFORMATION, DISABLE_MAX_PRIVILEGE,
        DuplicateTokenEx, GENERIC_MAPPING, GROUP_SECURITY_INFORMATION, GetLengthSid,
        OWNER_SECURITY_INFORMATION, PRIVILEGE_SET, PSECURITY_DESCRIPTOR, PSID, SID_AND_ATTRIBUTES,
        SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS, TOKEN_DUPLICATE,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenImpersonation, TokenIntegrityLevel,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // The two access shapes that decide the question. READ is what every failing operation needs
    // (`lstat` wants FILE_READ_ATTRIBUTES; a directory walk wants FILE_TRAVERSE); WRITE is what the
    // jail must still refuse outside the project.
    const FILE_READ_DATA: u32 = 0x0001;
    const FILE_WRITE_DATA: u32 = 0x0002;
    const FILE_ADD_FILE: u32 = 0x0002;
    const FILE_TRAVERSE: u32 = 0x0020;
    const FILE_READ_ATTRIBUTES: u32 = 0x0080;
    const READ_SET: u32 = FILE_READ_DATA | FILE_TRAVERSE | FILE_READ_ATTRIBUTES;
    const WRITE_SET: u32 = FILE_WRITE_DATA | FILE_ADD_FILE;
    const SE_GROUP_INTEGRITY: u32 = 0x20;

    struct TokenGuard(HANDLE);
    impl Drop for TokenGuard {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }
    struct SidGuard(PSID);
    impl Drop for SidGuard {
        fn drop(&mut self) {
            unsafe { LocalFree(self.0.cast()) };
        }
    }
    struct SdGuard(PSECURITY_DESCRIPTOR);
    impl Drop for SdGuard {
        fn drop(&mut self) {
            unsafe { LocalFree(self.0.cast()) };
        }
    }

    fn sid_from(text: &str) -> std::io::Result<SidGuard> {
        let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
        let mut sid: PSID = std::ptr::null_mut();
        // SAFETY: a well-formed SDDL sid string; the buffer is LocalFree'd by the guard.
        if unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(SidGuard(sid))
    }

    /// An impersonation-level duplicate of our own token, unmodified. The baseline arm.
    fn own_token() -> std::io::Result<TokenGuard> {
        let mut me: HANDLE = std::ptr::null_mut();
        // SAFETY: opens our own process token with exactly the rights used below.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut me) }
            == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let _me = TokenGuard(me);
        duplicate_for_check(me)
    }

    /// `CreateRestrictedToken` with Administrators reduced to deny-only and every privilege but
    /// `SeChangeNotifyPrivilege` stripped, then integrity lowered to `level`.
    ///
    /// Both steps act on a token derived from our OWN, which is what makes them unprivileged:
    /// a caller may always produce a more restricted version of its own token, and integrity is
    /// only ever lowered here.
    fn restricted_token(level: &str) -> std::io::Result<TokenGuard> {
        let mut me: HANDLE = std::ptr::null_mut();
        // SAFETY: as above.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut me) }
            == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let _me = TokenGuard(me);

        let admins = sid_from("S-1-5-32-544")?;
        let mut deny = [SID_AND_ATTRIBUTES {
            Sid: admins.0,
            Attributes: 0,
        }];
        let mut restricted: HANDLE = std::ptr::null_mut();
        // SAFETY: `deny` outlives the call; all other list pointers are null with zero counts.
        let ok = unsafe {
            CreateRestrictedToken(
                me,
                DISABLE_MAX_PRIVILEGE,
                deny.len() as u32,
                deny.as_mut_ptr(),
                0,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut restricted,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let restricted = TokenGuard(restricted);
        set_integrity(restricted.0, level)?;
        duplicate_for_check(restricted.0)
    }

    fn set_integrity(token: HANDLE, level: &str) -> std::io::Result<()> {
        let sid = sid_from(level)?;
        let mut label = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: sid.0,
                Attributes: SE_GROUP_INTEGRITY,
            },
        };
        // SAFETY: `label` and the sid it points at outlive the call.
        let ok = unsafe {
            SetTokenInformation(
                token,
                TokenIntegrityLevel,
                std::ptr::from_mut(&mut label).cast(),
                std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + GetLengthSid(sid.0),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// `AccessCheck` requires an IMPERSONATION-level token, not a primary one.
    fn duplicate_for_check(token: HANDLE) -> std::io::Result<TokenGuard> {
        let mut dup: HANDLE = std::ptr::null_mut();
        // SAFETY: duplicates an open token handle we own to impersonation level.
        let ok = unsafe {
            DuplicateTokenEx(
                token,
                TOKEN_ALL_ACCESS,
                std::ptr::null(),
                SecurityImpersonation,
                TokenImpersonation,
                &mut dup,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(TokenGuard(dup))
    }

    /// Whether `token` would be granted `desired` on `path`, decided by the OS.
    ///
    /// OWNER and GROUP are fetched alongside the DACL deliberately: `AccessCheck` rejects a
    /// descriptor missing either with `ERROR_INVALID_SECURITY_DESCR`, which would read as a denial
    /// if the error were folded into the result.
    fn access_check(token: HANDLE, path: &Path, desired: u32) -> Result<bool, String> {
        let wide: Vec<u16> = path
            .to_string_lossy()
            .encode_utf16()
            .chain([0])
            .collect::<Vec<u16>>();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: out-params are initialised; the descriptor is LocalFree'd by the guard.
        let rc = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        if rc != 0 {
            return Err(format!("descriptor-unreadable:{rc}"));
        }
        let _sd = SdGuard(sd);

        let mapping = GENERIC_MAPPING {
            GenericRead: READ_SET,
            GenericWrite: WRITE_SET,
            GenericExecute: FILE_TRAVERSE,
            GenericAll: READ_SET | WRITE_SET,
        };
        let mut privs: PRIVILEGE_SET = unsafe { std::mem::zeroed() };
        let mut priv_len = std::mem::size_of::<PRIVILEGE_SET>() as u32;
        let mut granted = 0u32;
        let mut status = 0i32;
        // SAFETY: every out-param is initialised; `mapping`/`privs` outlive the call.
        let ok = unsafe {
            AccessCheck(
                sd,
                token,
                desired,
                &mapping,
                &mut privs,
                &mut priv_len,
                &mut granted,
                &mut status,
            )
        };
        if ok == 0 {
            return Err(format!("check-failed:{}", std::io::Error::last_os_error()));
        }
        Ok(status != 0 && (granted & desired) == desired)
    }

    fn report(fails: &mut u32, prop: &str, ok: bool, detail: &str) {
        println!(
            "  prop:{prop}={}  {detail}",
            if ok { "PASS" } else { "FAIL" }
        );
        if !ok {
            *fails += 1;
        }
    }

    fn admin_authority() -> bool {
        use windows_sys::Win32::System::Services::{
            CloseServiceHandle, OpenSCManagerW, SC_MANAGER_CREATE_SERVICE,
        };
        // SAFETY: opens the local SCM for a right never exercised; mutates nothing.
        unsafe {
            let h = OpenSCManagerW(
                std::ptr::null(),
                std::ptr::null(),
                SC_MANAGER_CREATE_SERVICE,
            );
            if h.is_null() {
                return false;
            }
            CloseServiceHandle(h);
            true
        }
    }

    pub fn probe_main() -> i32 {
        let mut fails = 0u32;
        println!("PROBE windows restricted token vs AppContainer, by AccessCheck");
        println!("  fact:runner-admin-authority={}", admin_authority());

        let tmp = std::env::temp_dir().join(format!("nub-rt-{}", std::process::id()));
        let leaf = tmp.join("leaf.txt");
        if std::fs::create_dir_all(&tmp).is_err() || std::fs::write(&leaf, "x").is_err() {
            eprintln!("could not build the fixture");
            return 1;
        }

        let profile =
            PathBuf::from(std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users".to_string()));
        let paths: Vec<(&str, PathBuf)> = vec![
            ("c-root", PathBuf::from("C:\\")),
            ("c-users", PathBuf::from("C:\\Users")),
            ("user-profile", profile),
            ("fixture-dir", tmp.clone()),
            ("fixture-leaf", leaf.clone()),
        ];

        // BASELINE first, and it is not decoration: `AccessCheck` models the check rather than
        // performing it in situ, so an unrestricted token must come back GRANTED on these very
        // paths or the harness is reporting its own error as a denial.
        let arms: Vec<(&str, std::io::Result<TokenGuard>)> = vec![
            ("baseline-own-token", own_token()),
            ("restricted-medium-il", restricted_token("S-1-16-8192")),
            ("restricted-low-il", restricted_token("S-1-16-4096")),
        ];

        for (arm, token) in &arms {
            let token = match token {
                Ok(t) => t,
                Err(e) => {
                    report(
                        &mut fails,
                        &format!("token-built-{arm}"),
                        false,
                        &e.to_string(),
                    );
                    continue;
                }
            };
            report(&mut fails, &format!("token-built-{arm}"), true, "");
            for (label, path) in &paths {
                for (kind, desired) in [("read", READ_SET), ("write", WRITE_SET)] {
                    let verdict = match access_check(token.0, path, desired) {
                        Ok(true) => "GRANTED".to_string(),
                        Ok(false) => "DENIED".to_string(),
                        Err(e) => format!("ERROR:{e}"),
                    };
                    println!("  fact:{arm} {kind} {label} = {verdict}");
                }
            }
        }

        // The controls. A baseline that cannot read `C:\` means the harness is broken, and the
        // whole table below it is unattributable.
        for (label, path) in &paths {
            if let Ok(base) = &arms[0].1 {
                let ok = access_check(base.0, path, READ_SET).unwrap_or(false);
                report(
                    &mut fails,
                    &format!("baseline-reads-{label}"),
                    ok,
                    "an unrestricted token must read this, or AccessCheck is being misused here",
                );
            }
        }

        let _ = std::fs::remove_dir_all(&tmp);
        println!("PROBE end fails={fails}");
        i32::from(fails > 0)
    }
}

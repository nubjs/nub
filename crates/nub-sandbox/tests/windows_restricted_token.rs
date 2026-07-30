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
        SecurityImpersonation, SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ALL_ACCESS,
        TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
        TokenImpersonation, TokenIntegrityLevel,
    };
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
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

    /// The rights the derived tokens need, and the reason the first revision of this probe
    /// measured nothing: `CreateRestrictedToken` gives the new token THE SAME access rights as the
    /// handle it was derived from, and `SetTokenInformation(TokenIntegrityLevel)` requires
    /// `TOKEN_ADJUST_DEFAULT`. Opening with only `TOKEN_DUPLICATE | TOKEN_QUERY` therefore built a
    /// restricted token that could not be relabelled, and both treatment arms failed with
    /// ACCESS_DENIED before a single access check ran — which reads exactly like the mechanism
    /// being unavailable rather than the harness being wrong.
    const TOKEN_RIGHTS: u32 =
        TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT | TOKEN_ASSIGN_PRIMARY;

    /// An impersonation-level duplicate of our own token, unmodified. The baseline arm.
    fn own_token() -> std::io::Result<TokenGuard> {
        let mut me: HANDLE = std::ptr::null_mut();
        // SAFETY: opens our own process token with exactly the rights used below.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_RIGHTS, &mut me) } == 0 {
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
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_RIGHTS, &mut me) } == 0 {
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
            return Err(std::io::Error::other(format!(
                "CreateRestrictedToken: {}",
                std::io::Error::last_os_error()
            )));
        }
        let restricted = TokenGuard(restricted);
        // Named per step: "Access is denied" alone cannot be told from the mechanism being
        // unavailable, which is how the first revision's failure was nearly read as a finding.
        set_integrity(restricted.0, level)
            .map_err(|e| std::io::Error::other(format!("SetTokenInformation(integrity): {e}")))?;
        duplicate_for_check(restricted.0)
            .map_err(|e| std::io::Error::other(format!("DuplicateTokenEx: {e}")))
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

    /// `OBJECT_ATTRIBUTES`, spelled locally rather than pulling a Wdk feature in for one struct.
    #[repr(C)]
    struct ObjectAttributes {
        length: u32,
        root_directory: HANDLE,
        object_name: *mut std::ffi::c_void,
        attributes: u32,
        security_descriptor: *mut std::ffi::c_void,
        security_quality_of_service: *mut std::ffi::c_void,
    }

    type NtCreateLowBoxTokenFn = unsafe extern "system" fn(
        *mut HANDLE,
        HANDLE,
        u32,
        *mut ObjectAttributes,
        PSID,
        u32,
        *mut SID_AND_ATTRIBUTES,
        u32,
        *mut HANDLE,
    ) -> i32;

    /// Turn `base` into a LowBox (AppContainer) token carrying `package` and NO capabilities.
    ///
    /// `NtCreateLowBoxToken` is undocumented but it is the syscall `CreateProcessW` itself reaches
    /// through when handed `SECURITY_CAPABILITIES`, and it is what makes the composition testable:
    /// it takes the BASE token as a parameter, so a restricted token can be the base. Chromium does
    /// exactly this (`app_container_base.cc` `BuildPrimaryToken`/`BuildImpersonationToken`, and
    /// `app_container_unittest.cc` asserts the result keeps the base's User sid while still
    /// reporting `IsAppContainer`).
    ///
    /// ZERO capabilities is deliberate and is half the design under test: no `internetClient` means
    /// no egress. The other half is whether reads survive, which is what the caller measures.
    fn lowbox_token(base: HANDLE, package: PSID) -> std::io::Result<TokenGuard> {
        let ntdll: Vec<u16> = "ntdll.dll".encode_utf16().chain([0]).collect();
        // SAFETY: resolves an exported symbol from an already-loaded module; the returned pointer
        // is transmuted to the documented-by-reverse-engineering signature above.
        let func: NtCreateLowBoxTokenFn = unsafe {
            let module = GetModuleHandleW(ntdll.as_ptr());
            if module.is_null() {
                return Err(std::io::Error::other("ntdll not loaded"));
            }
            let sym = GetProcAddress(module, c"NtCreateLowBoxToken".as_ptr().cast());
            match sym {
                Some(p) => std::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    NtCreateLowBoxTokenFn,
                >(p),
                None => return Err(std::io::Error::other("NtCreateLowBoxToken not exported")),
            }
        };
        let mut attrs = ObjectAttributes {
            length: std::mem::size_of::<ObjectAttributes>() as u32,
            root_directory: std::ptr::null_mut(),
            object_name: std::ptr::null_mut(),
            attributes: 0,
            security_descriptor: std::ptr::null_mut(),
            security_quality_of_service: std::ptr::null_mut(),
        };
        let mut out: HANDLE = std::ptr::null_mut();
        const TOKEN_ALL: u32 = 0xF01FF;
        // SAFETY: `attrs` outlives the call; capability and handle lists are empty with zero counts.
        let status = unsafe {
            func(
                &mut out,
                base,
                TOKEN_ALL,
                &mut attrs,
                package,
                0,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(std::io::Error::other(format!(
                "NtCreateLowBoxToken: NTSTATUS 0x{status:08x}"
            )));
        }
        Ok(TokenGuard(out))
    }

    /// A package sid without creating any on-disk profile — the sid is a pure hash of the name, so
    /// this leaves nothing to clean up and needs no privilege.
    fn package_sid(name: &str) -> std::io::Result<PSID> {
        use windows_sys::Win32::Security::Isolation::DeriveAppContainerSidFromAppContainerName;
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let mut sid: PSID = std::ptr::null_mut();
        // SAFETY: derives a sid from a well-formed name; freed with FreeSid by the caller.
        let hr = unsafe { DeriveAppContainerSidFromAppContainerName(wide.as_ptr(), &mut sid) };
        if hr != 0 {
            return Err(std::io::Error::other(format!(
                "DeriveAppContainerSid: hr 0x{hr:08x}"
            )));
        }
        Ok(sid)
    }

    /// `TokenIsAppContainer` and the capability count, so an arm's claim to BE an AppContainer with
    /// no capabilities is read off the token rather than assumed from how it was built.
    fn token_shape(token: HANDLE) -> String {
        use windows_sys::Win32::Security::{
            GetTokenInformation, TokenCapabilities, TokenIsAppContainer,
        };
        let mut is_ac = 0u32;
        let mut len = 0u32;
        // SAFETY: fixed-size out-param sized exactly.
        unsafe {
            GetTokenInformation(
                token,
                TokenIsAppContainer,
                std::ptr::from_mut(&mut is_ac).cast(),
                4,
                &mut len,
            )
        };
        let mut needed = 0u32;
        // SAFETY: size query with a null buffer, as documented.
        unsafe {
            GetTokenInformation(
                token,
                TokenCapabilities,
                std::ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        let mut buf = vec![0u8; needed.max(4) as usize];
        // SAFETY: buffer sized by the query above.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenCapabilities,
                buf.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        };
        let caps = if ok == 0 {
            "?".to_string()
        } else {
            // TOKEN_GROUPS: a leading u32 count.
            u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]).to_string()
        };
        format!("is-appcontainer={} capabilities={caps}", is_ac != 0)
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

        // FOUR ROWS, and the two known ones are what make the new one interpretable.
        //
        // `lowbox-on-own-base` is the CRITICAL CONTROL for this experiment specifically: a LowBox
        // token built on an unrestricted base must come back DENIED on `C:\`, matching what the
        // real confined launch measured. If it came back GRANTED, `AccessCheck` would not be
        // modelling the AppContainer gate at all and the composed rows would mean nothing — the
        // same class of false result as the `TOKEN_ADJUST_DEFAULT` mask bug, in the opposite
        // direction.
        //
        // Both construction orders are measured rather than inferred. Chromium's helper takes the
        // base token as a parameter, which suggests restrict-then-lowbox, but a suggestion is not
        // a measurement.
        let package = match package_sid("nub-probe-composed") {
            Ok(p) => p,
            Err(e) => {
                report(&mut fails, "package-sid-derived", false, &e.to_string());
                return 1;
            }
        };
        report(&mut fails, "package-sid-derived", true, "");

        let composed_restrict_then_lowbox = restricted_token("S-1-16-4096")
            .and_then(|base| lowbox_token(base.0, package))
            .and_then(|t| duplicate_for_check(t.0));
        let composed_lowbox_then_restrict = own_token()
            .and_then(|base| lowbox_token(base.0, package))
            .and_then(|lb| {
                // Restricting AFTER: the same deny-only Administrators reduction, applied to a
                // token that is already an AppContainer.
                let admins = sid_from("S-1-5-32-544")?;
                let mut deny = [SID_AND_ATTRIBUTES {
                    Sid: admins.0,
                    Attributes: 0,
                }];
                let mut out: HANDLE = std::ptr::null_mut();
                // SAFETY: `deny` outlives the call; other lists are empty.
                let ok = unsafe {
                    CreateRestrictedToken(
                        lb.0,
                        DISABLE_MAX_PRIVILEGE,
                        deny.len() as u32,
                        deny.as_mut_ptr(),
                        0,
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null_mut(),
                        &mut out,
                    )
                };
                if ok == 0 {
                    return Err(std::io::Error::other(format!(
                        "CreateRestrictedToken(on lowbox): {}",
                        std::io::Error::last_os_error()
                    )));
                }
                let out = TokenGuard(out);
                set_integrity(out.0, "S-1-16-4096")?;
                duplicate_for_check(out.0)
            });

        let arms: Vec<(&str, std::io::Result<TokenGuard>)> = vec![
            ("baseline-own-token", own_token()),
            ("restricted-medium-il", restricted_token("S-1-16-8192")),
            ("restricted-low-il", restricted_token("S-1-16-4096")),
            (
                "lowbox-on-own-base",
                own_token()
                    .and_then(|b| lowbox_token(b.0, package))
                    .and_then(|t| duplicate_for_check(t.0)),
            ),
            (
                "lowbox-on-restricted-base-low-il",
                composed_restrict_then_lowbox,
            ),
            (
                "restricted-after-lowbox-low-il",
                composed_lowbox_then_restrict,
            ),
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
            println!("  fact:{arm} shape = {}", token_shape(token.0));
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

        // THE CONTROL THAT MAKES THIS EXPERIMENT INTERPRETABLE. A LowBox token on an unrestricted
        // base must be DENIED on `C:\`, reproducing what the real confined launch measured. If it
        // were GRANTED, `AccessCheck` would not be applying the AppContainer gate and every
        // composed row above would be meaningless.
        if let Some(Ok(lowbox)) = arms
            .iter()
            .find(|(a, _)| *a == "lowbox-on-own-base")
            .map(|(_, t)| t.as_ref())
        {
            let denied = !access_check(lowbox.0, Path::new("C:\\"), READ_SET).unwrap_or(true);
            report(
                &mut fails,
                "lowbox-gate-is-modelled",
                denied,
                "a LowBox token on an unrestricted base must be DENIED on C:\\, or AccessCheck \
                 is not applying the AppContainer gate and the composed rows mean nothing",
            );
        } else {
            report(
                &mut fails,
                "lowbox-gate-is-modelled",
                false,
                "the control arm did not build",
            );
        }

        drop(arms);
        // SAFETY: the sid came from DeriveAppContainerSidFromAppContainerName; last use.
        unsafe { windows_sys::Win32::Security::FreeSid(package) };
        let _ = std::fs::remove_dir_all(&tmp);
        println!("PROBE end fails={fails}");
        i32::from(fails > 0)
    }
}

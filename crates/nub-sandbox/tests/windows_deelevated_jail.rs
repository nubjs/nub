//! Windows build jail WITHOUT ELEVATION — the paired elevated/de-elevated differential.
//!
//! CANON requires the build jail to hold at ZERO privilege on every OS. On Windows the
//! design supports it (coarse egress deny is the ABSENCE of the `internetClient`
//! capability; `plan_net` consults elevation only on the WFP/per-host path, which belongs
//! to `nub sandbox`, not the jail) — but every Windows measurement to date was taken from
//! an ELEVATED CI token, so the property was asserted and never observed. This binary
//! observes it.
//!
//! THE DIFFERENTIAL: the parent runs the SAME arm — byte-identical argv, byte-identical
//! code — twice, and the ONLY variable is the primary token the arm process is created
//! with. Arm A is spawned with the ambient (elevated, on CI) token; arm B with a
//! de-elevated one. A property that silently required elevation fails in B and passes in
//! A, which is the whole point of pairing them.
//!
//! WHY THE LINKED TOKEN: `runas /trustlevel:0x20000` was measured (run 30422926046) and is
//! DISQUALIFIED — it detaches, relays no child output, and exits 0 regardless, so an arm
//! built on it reports success having measured nothing. `OpenProcessToken` →
//! `GetTokenInformation(TokenLinkedToken)` → `DuplicateTokenEx(TokenPrimary)` →
//! `CreateProcessAsUserW` yields an OBSERVABLE child exit code and keeps the SAME session
//! and window station — load-bearing, because AppContainer cannot be launched from session
//! 0 at all (every SSH-launched attempt returns 0xC0000142 STATUS_DLL_INIT_FAILED, which is
//! why CI is the venue).
//!
//! …AND WHY THE FALLBACK IS THE ONE THAT ACTUALLY RUNS ON CI: the `windows-latest`
//! runner has `EnableLUA=1` but its `runneradmin` token is `TokenElevationTypeDefault`, i.e.
//! NOT a split token, so `TokenLinkedToken` fails with ERROR_NO_SUCH_LOGON_SESSION (1312)
//! and there is no standard-user half to borrow (measured, run 30424255514). The fallback
//! therefore synthesizes an equivalent principal: `CreateRestrictedToken` with
//! `BUILTIN\Administrators` reduced to deny-only and `DISABLE_MAX_PRIVILEGE` (keeping only
//! `SeChangeNotifyPrivilege`, the traverse-bypass the backend's leaf-only grants already
//! rely on), then dropped to MEDIUM integrity. On every axis that governs an access check
//! that is a standard user or stricter. [`deelevated_primary_token`] names the route it
//! used, and logs why the preferred one was unavailable.
//!
//! THE ANTI-HOLLOW CONTRACT: an arm that never ran must never read as a pass. So the arm
//! reports its OWN token state to a marker file as its first act, and the parent fails
//! unless (a) both markers exist, (b) the de-elevated arm could NOT pass an access check
//! only an administrator passes while the elevated arm COULD, and (c) every property
//! reported a verdict in BOTH arms. The `acl-grant-allow` property is the CONTROL — it must
//! pass in both arms, which is what distinguishes it from a second copy of the treatment.
//!
//! The gate is an access check, not `TokenIsElevated`, because that flag is not a sound
//! oracle here: `CreateRestrictedToken` COPIES it rather than recomputing it, so the
//! fallback route produces a token with no administrative authority that still reports
//! `TokenIsElevated=1` (measured, run 30423750288). Both values are reported; only the
//! access-checked one gates. See [`admin_authority`].

#[cfg(not(target_os = "windows"))]
fn main() {
    // Non-Windows host: nothing to measure. (`harness = false` needs a `main`.)
}

#[cfg(target_os = "windows")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("__sbxchild__") => std::process::exit(win::child_main(&args[2..])),
        Some("__jailarm__") => std::process::exit(win::arm_main(&args[2])),
        _ => std::process::exit(win::differential_main()),
    }
}

#[cfg(target_os = "windows")]
mod win {
    use nub_sandbox::policy::{
        CanonGlob, Effect, EnvPolicy, FsAccess, FsOrigin, FsPolicy, FsRule, FsRuleSet, NetPolicy,
        PidPolicy, SandboxPolicy, TmpMode,
    };
    use nub_sandbox::{CommandSpec, apply};
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::net::{SocketAddr, TcpStream};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    // ── the jailed probe child ────────────────────────────────────────────────────
    // Same exit-code contract as `windows_enforcement`: 0 ok, 4 env-absent, 5
    // access-denied, 6 timeout, 9 other-error, 10 not-in-an-AppContainer.

    pub fn child_main(a: &[String]) -> i32 {
        match a.first().map(String::as_str) {
            Some("read") => match std::fs::read(&a[1]) {
                Ok(_) => 0,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => 5,
                Err(_) => 9,
            },
            Some("write") => match std::fs::write(&a[1], b"x") {
                Ok(_) => 0,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => 5,
                Err(_) => 9,
            },
            Some("connect") => connect(&a[1], a[2].parse().unwrap_or(0)),
            Some("token") => token_check(),
            Some("spawnchild") => spawn_grandchild(&a[1]),
            Some("sleep") => {
                std::thread::sleep(Duration::from_secs(120));
                0
            }
            Some("sleepms") => {
                std::thread::sleep(Duration::from_millis(a[1].parse().unwrap_or(0)));
                0
            }
            _ => 2,
        }
    }

    fn connect(host: &str, port: u16) -> i32 {
        let Ok(addr) = format!("{host}:{port}").parse::<SocketAddr>() else {
            return 9;
        };
        match TcpStream::connect_timeout(&addr, Duration::from_secs(8)) {
            Ok(_) => 0,
            // 10013 == WSAEACCES — the AppContainer egress block.
            Err(e) if e.raw_os_error() == Some(10013) => 5,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => 6,
            Err(_) => 9,
        }
    }

    fn spawn_grandchild(marker: &str) -> i32 {
        let Ok(exe) = std::env::current_exe() else {
            return 9;
        };
        match std::process::Command::new(exe)
            .args(["__sbxchild__", "sleep"])
            .spawn()
        {
            Ok(child) => {
                let _ = std::fs::write(marker, child.id().to_string());
                0
            }
            Err(_) => 9,
        }
    }

    /// The jailed child reports its own confinement AND its own elevation, so a "denied"
    /// can never be a launch failure and the arm's token state is visible end-to-end.
    fn token_check() -> i32 {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{
            GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation, TokenIsAppContainer,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        // SAFETY: standard token-query sequence; each buffer is exactly sized for the
        // DWORD / TOKEN_ELEVATION its class returns.
        unsafe {
            let mut tok = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
                return 9;
            }
            let mut is_ac: u32 = 0;
            let mut ret = 0u32;
            let ok_ac = GetTokenInformation(
                tok,
                TokenIsAppContainer,
                std::ptr::from_mut(&mut is_ac).cast(),
                4,
                &mut ret,
            );
            let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let ok_el = GetTokenInformation(
                tok,
                TokenElevation,
                std::ptr::from_mut(&mut elev).cast(),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut ret,
            );
            CloseHandle(tok);
            if ok_ac == 0 || ok_el == 0 {
                return 9;
            }
            println!(
                "    JAILED CHILD IsAppContainer={is_ac} IsElevated={}",
                elev.TokenIsElevated
            );
            if is_ac == 1 { 0 } else { 10 }
        }
    }

    // ── token inspection + de-elevation ───────────────────────────────────────────

    pub fn is_elevated() -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{
            GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        // SAFETY: query-only handle on our own token; the buffer is a TOKEN_ELEVATION.
        unsafe {
            let mut tok = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
                return false;
            }
            let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut ret = 0u32;
            let ok = GetTokenInformation(
                tok,
                TokenElevation,
                std::ptr::from_mut(&mut elev).cast(),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut ret,
            );
            CloseHandle(tok);
            ok != 0 && elev.TokenIsElevated != 0
        }
    }

    /// `TokenElevationType`: 1 Default (no split token on this host/logon), 2 Full
    /// (the elevated half of a split), 3 Limited (the standard-user half). Reported because
    /// it is what says whether a LINKED token should exist at all.
    fn elevation_type() -> u32 {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TokenElevationType};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        // SAFETY: query-only handle on our own token; the buffer is the DWORD this class
        // returns.
        unsafe {
            let mut tok = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
                return 0;
            }
            let mut ty: u32 = 0;
            let mut ret = 0u32;
            let ok = GetTokenInformation(
                tok,
                TokenElevationType,
                std::ptr::from_mut(&mut ty).cast(),
                4,
                &mut ret,
            );
            CloseHandle(tok);
            if ok != 0 { ty } else { 0 }
        }
    }

    /// This process's mandatory integrity level as its well-known RID: 4096 Low, 8192
    /// Medium (what a standard user runs at), 12288 High (what an elevated admin runs at).
    /// Reported so "the arm is standard-user-equivalent" is a measurement rather than a
    /// claim — removing admin authority alone would leave the arm at High integrity, which a
    /// standard user never is.
    fn integrity_level() -> u32 {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{
            GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
            TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        // SAFETY: two-call pattern — size the variable-length TOKEN_MANDATORY_LABEL, then
        // read the label SID's last subauthority, which is the IL RID.
        unsafe {
            let mut tok = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
                return 0;
            }
            let mut need = 0u32;
            GetTokenInformation(tok, TokenIntegrityLevel, std::ptr::null_mut(), 0, &mut need);
            let mut buf = vec![0u8; need.max(64) as usize];
            let ok = GetTokenInformation(
                tok,
                TokenIntegrityLevel,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut need,
            );
            CloseHandle(tok);
            if ok == 0 {
                return 0;
            }
            let label = buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
            let sid = (*label).Label.Sid;
            let count = *GetSidSubAuthorityCount(sid);
            if count == 0 {
                return 0;
            }
            *GetSidSubAuthority(sid, u32::from(count) - 1)
        }
    }

    /// Drop `token` to MEDIUM integrity — the level a standard user's token carries.
    /// `CreateRestrictedToken` removes group authority and privileges but leaves the
    /// integrity level UNTOUCHED, so without this the de-elevated arm would still run at
    /// High integrity and would be weaker than a real standard user on exactly one axis.
    /// Lowering an IL never needs a privilege (only raising one does).
    fn set_medium_integrity(token: windows_sys::Win32::Foundation::HANDLE) -> std::io::Result<()> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
        use windows_sys::Win32::Security::{
            PSID, SID_AND_ATTRIBUTES, SetTokenInformation, TOKEN_MANDATORY_LABEL,
            TokenIntegrityLevel,
        };
        // windows-sys files this under System_SystemServices; spelled out rather than
        // pulling a whole feature in for one integer.
        const SE_GROUP_INTEGRITY: u32 = 0x20;
        let text: Vec<u16> = "S-1-16-8192".encode_utf16().chain([0]).collect();
        let mut sid: PSID = std::ptr::null_mut();
        // SAFETY: converts a well-formed SDDL SID string; freed below.
        if unsafe { ConvertStringSidToSidW(text.as_ptr(), &mut sid) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut label = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: sid,
                Attributes: SE_GROUP_INTEGRITY,
            },
        };
        // SAFETY: `label` (and the SID it points at) outlives the call.
        let ok = unsafe {
            SetTokenInformation(
                token,
                TokenIntegrityLevel,
                std::ptr::from_mut(&mut label).cast(),
                std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32
                    + windows_sys::Win32::Security::GetLengthSid(sid),
            )
        };
        let err = std::io::Error::last_os_error();
        unsafe { LocalFree(sid.cast()) };
        if ok == 0 { Err(err) } else { Ok(()) }
    }

    /// Whether this process actually WIELDS administrative authority, decided by an access
    /// check rather than by a token flag.
    ///
    /// This is the load-bearing oracle, and `TokenIsElevated` is NOT a substitute for it:
    /// `CreateRestrictedToken` COPIES the elevation flag from its parent instead of
    /// recomputing it, so a token whose `BUILTIN\Administrators` SID has been reduced to
    /// deny-only and whose privileges are all stripped still reports `TokenIsElevated=1`
    /// while being unable to do anything an administrator can (measured on windows-latest,
    /// run 30423750288). `SC_MANAGER_CREATE_SERVICE` is the canonical probe — the SCM grants
    /// it to Administrators only, a deny-only SID does not match, and the call mutates
    /// nothing.
    fn admin_authority() -> bool {
        use windows_sys::Win32::System::Services::{
            CloseServiceHandle, OpenSCManagerW, SC_MANAGER_CREATE_SERVICE,
        };
        // SAFETY: opens the local SCM for a right we never exercise; NULL machine/database.
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

    /// A PRIMARY token for the SAME user with administrative authority removed, usable as
    /// `CreateProcessAsUserW`'s token. Returns the route taken so the report never has to
    /// guess which mechanism produced the measurement.
    ///
    /// The linked token is preferred: on a UAC-filtered admin it IS the standard-user token
    /// Windows already minted for this logon, so the arm runs as a genuine standard user.
    /// Every step is logged, because a silent fall-through to route 2 would leave the report
    /// unable to say WHY the ideal token was unavailable.
    ///
    /// The restricted-token fallback is STRICTLY MORE confined than a standard user — the
    /// Administrators SID becomes deny-only rather than merely absent — so a jail that holds
    /// under it holds for a standard user, while a failure under it needs the deny-only SID
    /// ruled out before being read as an elevation requirement. `DISABLE_MAX_PRIVILEGE`
    /// keeps only `SeChangeNotifyPrivilege`, which is exactly the traverse-bypass the
    /// backend's leaf-only grants already depend on.
    ///
    /// Neither route is accepted on the `TokenIsElevated` flag — see [`admin_authority`] for
    /// why that flag lies under route 2. The caller gates on the arm's own access-checked
    /// verdict instead.
    fn deelevated_primary_token()
    -> std::io::Result<(windows_sys::Win32::Foundation::HANDLE, &'static str)> {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
        use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
        use windows_sys::Win32::Security::{
            CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, DuplicateTokenEx, GetTokenInformation,
            PSID, SID_AND_ATTRIBUTES, SecurityImpersonation, TOKEN_ALL_ACCESS,
            TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_LINKED_TOKEN, TOKEN_QUERY,
            TokenLinkedToken, TokenPrimary,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        let mut me: HANDLE = std::ptr::null_mut();
        // SAFETY: opens our own token with exactly the rights the two routes below need.
        if unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY,
                &mut me,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }

        // Route 1 — the linked (standard-user) token.
        let mut linked = TOKEN_LINKED_TOKEN {
            LinkedToken: std::ptr::null_mut(),
        };
        let mut ret = 0u32;
        // SAFETY: out-buffer is exactly a TOKEN_LINKED_TOKEN; a host with no linked token
        // simply fails the call.
        let got = unsafe {
            GetTokenInformation(
                me,
                TokenLinkedToken,
                std::ptr::from_mut(&mut linked).cast(),
                std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
                &mut ret,
            )
        };
        if got == 0 {
            println!(
                "    [linked-token] GetTokenInformation(TokenLinkedToken) failed: {}",
                std::io::Error::last_os_error()
            );
        } else if linked.LinkedToken.is_null() {
            println!("    [linked-token] no linked token on this logon");
        } else {
            let mut primary: HANDLE = std::ptr::null_mut();
            // `TokenLinkedToken` hands back an IMPERSONATION token; CreateProcessAsUserW
            // needs a PRIMARY one. Without `SeTcbPrivilege` the handle can come back at
            // IDENTIFICATION level with less than TOKEN_ALL_ACCESS, so the wide request is
            // retried at the minimum CreateProcessAsUserW actually needs before giving up.
            let wanted = [
                ("TOKEN_ALL_ACCESS", TOKEN_ALL_ACCESS),
                (
                    "assign-primary|duplicate|query",
                    TOKEN_ASSIGN_PRIMARY | TOKEN_DUPLICATE | TOKEN_QUERY,
                ),
            ];
            for (label, access) in wanted {
                // SAFETY: duplicating a token handle we own into a primary token.
                let dup = unsafe {
                    DuplicateTokenEx(
                        linked.LinkedToken,
                        access,
                        std::ptr::null(),
                        SecurityImpersonation,
                        TokenPrimary,
                        &mut primary,
                    )
                };
                if dup != 0 {
                    unsafe { CloseHandle(linked.LinkedToken) };
                    unsafe { CloseHandle(me) };
                    println!("    [linked-token] duplicated as primary ({label})");
                    return Ok((primary, "linked-token"));
                }
                println!(
                    "    [linked-token] DuplicateTokenEx({label}) failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            unsafe { CloseHandle(linked.LinkedToken) };
        }

        // Route 2 — a restricted token with BUILTIN\Administrators deny-only and every
        // privilege dropped.
        let admins_text: Vec<u16> = "S-1-5-32-544".encode_utf16().chain([0]).collect();
        let mut admins: PSID = std::ptr::null_mut();
        // SAFETY: converts a well-formed SDDL SID string; freed below.
        if unsafe { ConvertStringSidToSidW(admins_text.as_ptr(), &mut admins) } == 0 {
            let e = std::io::Error::last_os_error();
            unsafe { CloseHandle(me) };
            return Err(e);
        }
        let disable = [SID_AND_ATTRIBUTES {
            Sid: admins,
            Attributes: 0,
        }];
        let mut restricted: HANDLE = std::ptr::null_mut();
        // SAFETY: `disable` outlives the call; `me` is a primary token, so the derived
        // token is primary too.
        let ok = unsafe {
            CreateRestrictedToken(
                me,
                DISABLE_MAX_PRIVILEGE,
                1,
                disable.as_ptr(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                &mut restricted,
            )
        };
        let err = std::io::Error::last_os_error();
        unsafe {
            LocalFree(admins.cast());
            CloseHandle(me);
        }
        if ok == 0 {
            return Err(err);
        }
        // `CreateRestrictedToken` hands back a handle carrying only the source handle's
        // rights, and `SetTokenInformation(TokenIntegrityLevel)` needs TOKEN_ADJUST_DEFAULT
        // (ACCESS_DENIED without it, run 30424678530). Re-open the token we already own at
        // full access rather than widening the process-token handle above.
        let mut full: HANDLE = std::ptr::null_mut();
        // SAFETY: duplicating a token handle we own into a full-access primary token.
        let dup = unsafe {
            DuplicateTokenEx(
                restricted,
                TOKEN_ALL_ACCESS,
                std::ptr::null(),
                SecurityImpersonation,
                TokenPrimary,
                &mut full,
            )
        };
        let dup_err = std::io::Error::last_os_error();
        unsafe { CloseHandle(restricted) };
        if dup == 0 {
            return Err(dup_err);
        }
        // Fail rather than measure at High integrity: an arm that kept the elevated token's
        // IL would be weaker than a standard user, which is the one direction this
        // substitution must never go.
        set_medium_integrity(full).map_err(|e| {
            unsafe { CloseHandle(full) };
            std::io::Error::other(format!(
                "could not drop the restricted token to medium IL: {e}"
            ))
        })?;
        Ok((full, "restricted-token+medium-il"))
    }

    /// Spawn `argv` under `token` in THIS session/window station and return its exit code.
    /// stdio is inherited so the arm's output lands in the CI log live.
    fn spawn_as_token(
        token: windows_sys::Win32::Foundation::HANDLE,
        program: &Path,
        args: &[&str],
    ) -> std::io::Result<i32> {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{
            CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
        };
        use windows_sys::Win32::System::Threading::{
            CreateProcessAsUserW, GetExitCodeProcess, INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
            WaitForSingleObject,
        };

        let mut cl: Vec<u16> = Vec::new();
        cl.push(u16::from(b'"'));
        cl.extend(program.as_os_str().encode_wide());
        cl.push(u16::from(b'"'));
        for a in args {
            cl.push(u16::from(b' '));
            cl.push(u16::from(b'"'));
            cl.extend(a.encode_utf16());
            cl.push(u16::from(b'"'));
        }
        cl.push(0);

        let stdin: HANDLE = std::io::stdin().as_raw_handle().cast();
        let stdout: HANDLE = std::io::stdout().as_raw_handle().cast();
        let stderr: HANDLE = std::io::stderr().as_raw_handle().cast();
        for h in [stdin, stdout, stderr] {
            // SAFETY: marking our own std handles inheritable, as `std`'s inherited-stdio
            // spawn does.
            unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
        }
        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
        si.hStdInput = stdin;
        si.hStdOutput = stdout;
        si.hStdError = stderr;
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        // NULL lpEnvironment ⇒ the child inherits OUR environment. Deliberate: the arms
        // must differ ONLY in token, and the de-elevated token is the same user, so the
        // ambient env (TEMP, LOCALAPPDATA, PATH) is already correct for it.
        // SAFETY: `cl` is a writable NUL-terminated UTF-16 buffer that outlives the call.
        let ok = unsafe {
            CreateProcessAsUserW(
                token,
                std::ptr::null(),
                cl.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                0,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::from_mut(&mut si).cast(),
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: wait for exit, read the code, close both handles.
        let code = unsafe {
            WaitForSingleObject(pi.hProcess, INFINITE);
            let mut code = 0u32;
            GetExitCodeProcess(pi.hProcess, &mut code);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            code as i32
        };
        Ok(code)
    }

    // ── the fixture ───────────────────────────────────────────────────────────────

    struct Fixture {
        root: PathBuf,
        child: PathBuf,
        work: PathBuf,
        allowed: PathBuf,
        secret: PathBuf,
        project: PathBuf,
        package: PathBuf,
        home: PathBuf,
    }

    /// PROTECTED DACL on the fixture root: inherited ACEs stripped, only the current user
    /// granted. Without it an inherited `ALL APPLICATION PACKAGES` grant would satisfy the
    /// LowBox check before default-deny is reached, and every "denied" assertion below
    /// would be measuring `%TEMP%`'s ACL rather than the backend's.
    fn secure_root(root: &Path) {
        let user = std::env::var("USERNAME").expect("USERNAME set on Windows");
        let status = std::process::Command::new("icacls")
            .arg(root)
            .args(["/inheritance:r", "/grant:r"])
            .arg(format!("{user}:(OI)(CI)F"))
            .status()
            .expect("run icacls");
        assert!(status.success(), "icacls failed to secure the fixture root");
    }

    impl Fixture {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!("nub-deelev-{nonce:x}"));
            std::fs::create_dir_all(&root).unwrap();
            secure_root(&root);
            let bin = root.join("bin");
            let work = root.join("work");
            let vault = root.join("vault");
            let project = root.join("proj");
            let package = project.join("node_modules/pkg");
            let home = root.join("home");
            let cache = home.join("cache");
            // The build jail's SPECULATIVE roots (the project manifest, the PM store/tools
            // cache) are materialized here so this differential measures ELEVATION and
            // nothing else. `derive_grants` now skips a missing speculative source, so their
            // absence would no longer fail the launch — but it would silently shrink the
            // read set both arms are compared on. `windows_speculative_grants` owns the
            // absent case.
            for d in [
                &bin,
                &work,
                &vault,
                &package,
                &cache.join("nub/pm/store"),
                &cache.join("nub/pm/tools"),
            ] {
                std::fs::create_dir_all(d).unwrap();
            }
            std::fs::write(project.join("package.json"), b"{}").unwrap();
            let child = bin.join("child.exe");
            std::fs::copy(std::env::current_exe().unwrap(), &child).unwrap();
            let allowed = work.join("allowed.txt");
            std::fs::write(&allowed, b"this-is-fine").unwrap();
            let secret = vault.join("secret.env");
            std::fs::write(&secret, b"TOPSECRET_TOKEN=do-not-leak").unwrap();
            Fixture {
                root,
                child,
                work,
                allowed,
                secret,
                project,
                package,
                home,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn canon(p: &Path) -> String {
        p.to_string_lossy().replace('\\', "/")
    }

    fn rule(p: &Path, access: FsAccess) -> FsRule {
        FsRule {
            matcher: CanonGlob(canon(p)),
            effect: Effect::Allow,
            access,
            origin: FsOrigin::Authored,
        }
    }

    /// A build-jail-SHAPED policy: pure default-deny read allowlist, own-dir write, coarse
    /// egress deny. `build_jail: true` because this is the jail's posture, not `nub
    /// sandbox`'s — and on Windows the jail's net axis is exactly `deny-all` (see
    /// `preset::build_jail_net`), which is the posture whose unprivileged-ness is under test.
    fn jail_shaped(f: &Fixture, deny_egress: bool) -> SandboxPolicy {
        SandboxPolicy {
            fs: FsPolicy {
                rules: FsRuleSet {
                    entries: vec![rule(&f.work, FsAccess::ReadWrite)],
                    default_effect: Effect::Deny,
                },
                tmp: TmpMode::Private,
            },
            net: if deny_egress {
                NetPolicy {
                    enforce: true,
                    rules: Vec::new(),
                    default_effect: Effect::Deny,
                    ..Default::default()
                }
            } else {
                NetPolicy::default()
            },
            env: EnvPolicy::resolved(os_essential_env()),
            pid: PidPolicy::default(),
            build_jail: true,
        }
    }

    /// The Windows-essential env a LowBox child needs to START (CreateProcessW resolves the
    /// per-container storage from the passed block, so a too-minimal env fails with
    /// ERROR_ENVVAR_NOT_FOUND). Mirrors `windows_enforcement::base_env`.
    fn os_essential_env() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        for k in [
            "SystemRoot",
            "SystemDrive",
            "windir",
            "ComSpec",
            "PATHEXT",
            "Path",
            "TEMP",
            "TMP",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
            "APPDATA",
            "LOCALAPPDATA",
            "ProgramData",
            "ALLUSERSPROFILE",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "CommonProgramFiles",
            "PUBLIC",
            "USERNAME",
            "USERDOMAIN",
            "COMPUTERNAME",
            "NUMBER_OF_PROCESSORS",
            "PROCESSOR_ARCHITECTURE",
            "OS",
            "DriverData",
        ] {
            if let Ok(v) = std::env::var(k) {
                m.insert(k.to_string(), v);
            }
        }
        m
    }

    /// Run a jailed child through the REAL backend path and return its exit code.
    /// `-100` = apply() refused the policy, `-101` = the launch itself failed. Both are
    /// distinguishable from every child exit code, so a setup failure can never read as an
    /// enforcement result.
    fn code(policy: &SandboxPolicy, f: &Fixture, cwd: &Path, args: &[&str]) -> i32 {
        let spec = CommandSpec::new(f.child.as_os_str())
            .args(args.iter().copied())
            .cwd(cwd);
        let prepared = match apply(policy, spec) {
            Ok(p) => p,
            Err(d) => {
                println!("    [apply Err] {d:?}");
                return -100;
            }
        };
        match prepared.status() {
            Ok(s) => s.code().unwrap_or(-1),
            Err(e) => {
                println!("    [status Err] {e} os={:?}", e.raw_os_error());
                -101
            }
        }
    }

    // ── observing the per-run OS state the jail creates and destroys ───────────────

    /// How many AppContainer profile dirs THIS process created still exist.
    /// `unique_profile_name` keys the profile on pid, so a sibling process's profiles are
    /// invisible here and the count is exact rather than merely indicative.
    fn own_profile_dirs() -> usize {
        let Ok(local) = std::env::var("LOCALAPPDATA") else {
            return 0;
        };
        let prefix = format!("nub_sbx_{}_", std::process::id());
        let Ok(rd) = std::fs::read_dir(PathBuf::from(local).join("Packages")) else {
            return 0;
        };
        rd.flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .count()
    }

    /// The number of ACEs on `path`'s DACL. The per-run grant ADDS one and teardown must
    /// remove it, so this is the observable that makes "the ACEs were revoked" a
    /// measurement rather than an assertion.
    fn ace_count(path: &Path) -> Option<u32> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::ACL;
        use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
        use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
        let w: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: standard DACL query; both out-pointers are owned by `sd`, freed below.
        let rc = unsafe {
            GetNamedSecurityInfoW(
                w.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        if rc != 0 || dacl.is_null() {
            if !sd.is_null() {
                unsafe { LocalFree(sd) };
            }
            return None;
        }
        // SAFETY: `dacl` points into the live security descriptor.
        let n = unsafe { (*dacl).AceCount } as u32;
        unsafe { LocalFree(sd) };
        Some(n)
    }

    // ── the arm ───────────────────────────────────────────────────────────────────

    /// One verdict line. `name` is stable so the parent can require the SAME property set
    /// from both arms — a property that silently vanished from one arm is a missing key,
    /// not a pass.
    struct Report {
        marker: std::fs::File,
        failures: u32,
    }
    impl Report {
        fn record(&mut self, name: &str, ok: bool, detail: &str) {
            let verdict = if ok { "PASS" } else { "FAIL" };
            if !ok {
                self.failures += 1;
            }
            let line = format!("prop:{name}={verdict} {detail}");
            println!("  {line}");
            let _ = writeln!(self.marker, "{line}");
            let _ = self.marker.flush();
        }
    }

    /// The arm. Byte-identical in both halves of the differential; only the primary token
    /// it was created with differs.
    pub fn arm_main(marker_path: &str) -> i32 {
        let elevated = is_elevated();
        let admin = admin_authority();
        // FIRST ACT, before anything can fail: the marker's existence proves the arm ran,
        // and the two token lines prove WHICH arm it was. `admin=` is the gate the parent
        // actually trusts — `elevated=` is the token flag, which route 2 leaves stale.
        let mut marker = match std::fs::File::create(marker_path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("ARM could not create marker {marker_path}: {e}");
                return 2;
            }
        };
        let _ = writeln!(marker, "elevated={}", u8::from(elevated));
        let _ = writeln!(marker, "admin={}", u8::from(admin));
        let _ = writeln!(marker, "il={}", integrity_level());
        let _ = marker.flush();
        println!(
            "ARM elevated={} admin={} il={} elevation-type={} pid={}",
            u8::from(elevated),
            u8::from(admin),
            integrity_level(),
            elevation_type(),
            std::process::id()
        );

        let f = Fixture::new();
        let mut r = Report {
            marker,
            failures: 0,
        };

        // (1)+(2) profile created, child launches, and it is genuinely in a LowBox.
        let confine = jail_shaped(&f, false);
        let rc = code(&confine, &f, &f.root, &["__sbxchild__", "token"]);
        r.record(
            "profile-create-and-launch",
            rc == 0,
            &format!("(child exit {rc}; 0 = launched, IsAppContainer=1)"),
        );

        // (3a) THE CONTROL — a legitimate build's reads and writes succeed. This passes in
        // BOTH arms, which is what makes it a control rather than a second treatment: it
        // fails only if the jail became unusable, never merely because enforcement worked.
        let read_ok = code(
            &confine,
            &f,
            &f.root,
            &["__sbxchild__", "read", &f.allowed.to_string_lossy()],
        );
        let write_target = f.work.join("built.txt");
        let write_ok = code(
            &confine,
            &f,
            &f.root,
            &["__sbxchild__", "write", &write_target.to_string_lossy()],
        );
        r.record(
            "acl-grant-allow",
            read_ok == 0 && write_ok == 0 && write_target.exists(),
            &format!(
                "(read {read_ok}, write {write_ok}, file written {})",
                write_target.exists()
            ),
        );

        // (3b) …and the ungranted secret stays unreachable.
        let denied = code(
            &confine,
            &f,
            &f.root,
            &["__sbxchild__", "read", &f.secret.to_string_lossy()],
        );
        r.record(
            "acl-grant-deny",
            denied == 5 || denied == 9,
            &format!("(child exit {denied}; 5/9 = denied)"),
        );

        // (4) teardown, observed rather than assumed: watch the profile dir and the grant
        // ACE APPEAR while a launch is in flight, then require both gone afterwards. A
        // post-hoc zero alone would also hold if nothing had ever been created.
        let base_aces = ace_count(&f.work);
        let base_profiles = own_profile_dirs();
        let (during_profile, during_ace, after_profile, after_ace) =
            observe_teardown(&f, base_aces);
        let created = during_profile > base_profiles && during_ace > base_aces;
        let torn_down = after_profile == base_profiles && after_ace == base_aces;
        r.record(
            "teardown",
            created && torn_down,
            &format!(
                "(profiles {base_profiles}→{during_profile}→{after_profile}, \
                 aces {base_aces:?}→{during_ace:?}→{after_ace:?})"
            ),
        );

        // (4b) the Job Object closes and reaps the tree.
        let pid_marker = f.work.join("gc.pid");
        let spawn_rc = code(
            &confine,
            &f,
            &f.root,
            &["__sbxchild__", "spawnchild", &pid_marker.to_string_lossy()],
        );
        std::thread::sleep(Duration::from_millis(500));
        let gc: u32 = std::fs::read_to_string(&pid_marker)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        r.record(
            "job-reap",
            spawn_rc == 0 && gc != 0 && !is_alive(gc),
            &format!(
                "(spawn {spawn_rc}, grandchild pid {gc}, alive {})",
                is_alive(gc)
            ),
        );

        // (5) coarse egress deny — the property that is pure capability WITHHOLDING, so it
        // is the one that must not need a privilege. Paired with the NC that the same
        // AppContainer WITH internetClient does reach the network, isolating the capability
        // as the cause rather than a broken runner network.
        let net_deny = jail_shaped(&f, true);
        let blocked = code(
            &net_deny,
            &f,
            &f.root,
            &["__sbxchild__", "connect", "1.1.1.1", "443"],
        );
        let allowed_egress = code(
            &confine,
            &f,
            &f.root,
            &["__sbxchild__", "connect", "1.1.1.1", "443"],
        );
        r.record(
            "egress-deny",
            (blocked == 5 || blocked == 6) && allowed_egress == 0,
            &format!("(deny arm {blocked} [5/6 expected], NC internetClient arm {allowed_egress} [0 expected])"),
        );

        // (6) the PRODUCTION policy, not just its shape: `compile_build_jail` is the entry
        // aube's lifecycle hook drives, so applying and launching it de-elevated is what
        // actually clears the jail for default-on.
        production_jail(&mut r, &f);

        // (7) …and with the REAL interpreter rather than the probe binary, which is the one
        // variable (6) cannot vary and the one that carried the showstopper.
        interpreter_group(&mut r, &f);

        println!(
            "ARM done elevated={} failures={}",
            u8::from(elevated),
            r.failures
        );
        i32::from(r.failures > 0)
    }

    /// Launch a long-running jailed child on a worker thread and sample the OS state while
    /// it is alive, so profile creation and the grant ACE are seen to EXIST before teardown
    /// is asked to remove them.
    fn observe_teardown(
        f: &Fixture,
        base_aces: Option<u32>,
    ) -> (usize, Option<u32>, usize, Option<u32>) {
        let policy = jail_shaped(f, false);
        let child = f.child.clone();
        let root = f.root.clone();
        let work = f.work.clone();
        let handle = std::thread::spawn(move || {
            let spec = CommandSpec::new(child.as_os_str())
                .args(["__sbxchild__", "sleepms", "6000"])
                .cwd(&root);
            match apply(&policy, spec) {
                Ok(p) => {
                    let _ = p.status();
                }
                Err(d) => println!("    [teardown apply Err] {d:?}"),
            }
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let (mut during_profile, mut during_ace) = (0usize, base_aces);
        while Instant::now() < deadline {
            let p = own_profile_dirs();
            let a = ace_count(&work);
            if p > 0 && a > base_aces {
                during_profile = p;
                during_ace = a;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = handle.join();
        (
            during_profile,
            during_ace,
            own_profile_dirs(),
            ace_count(&work),
        )
    }

    fn is_alive(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        if pid == 0 {
            return false;
        }
        // SAFETY: open by pid for query only; STILL_ACTIVE (259) ⇒ alive.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code);
            CloseHandle(h);
            ok != 0 && code == 259
        }
    }

    /// The real `compile_build_jail` policy, applied and launched. Both halves matter: the
    /// child must START (so the profile + ACL + Job path all worked) and the jail's deny-all
    /// egress must hold.
    fn production_jail(r: &mut Report, f: &Fixture) {
        let homes = nub_sandbox::Homes {
            home: f.home.clone(),
            tmp: std::env::temp_dir(),
            cache: f.home.join("cache"),
            project: f.project.clone(),
        };
        let ambient: BTreeMap<String, String> = std::env::vars().collect();
        let policy = match nub_sandbox::compile_build_jail(
            homes,
            &f.package,
            vec![f.child.clone()],
            Vec::new(),
            ambient,
        ) {
            Ok(p) => p,
            Err(e) => {
                r.record(
                    "production-jail-launch",
                    false,
                    &format!("(compile err {e:?})"),
                );
                r.record("production-jail-egress", false, "(not reached)");
                return;
            }
        };
        let started = code(&policy, f, &f.package, &["__sbxchild__", "token"]);
        r.record(
            "production-jail-launch",
            started == 0,
            &format!("(child exit {started}; 0 = compile_build_jail policy launched)"),
        );
        let egress = code(
            &policy,
            f,
            &f.package,
            &["__sbxchild__", "connect", "1.1.1.1", "443"],
        );
        r.record(
            "production-jail-egress",
            egress == 5 || egress == 6,
            &format!("(child exit {egress}; 5/6 = denied)"),
        );
    }

    // ── the interpreter group: can a lifecycle script run at all? ─────────────────

    /// THE SHOWSTOPPER THIS GROUP EXISTS FOR. `production_jail` above grants the PROBE BINARY as
    /// the interpreter — a file in the fixture, which nub owns — so it has always passed and could
    /// never have seen the real defect. A real install grants the user's own Node, and for a
    /// standard user with an all-users install that is a path nub cannot write an ACE on
    /// (`%ProgramFiles%\nodejs`, `C:\hostedtoolcache`) AND, independently, an image a confined
    /// caller cannot open. Either one makes the jail unable to start a single lifecycle script.
    ///
    /// So this runs the SAME lifecycle script twice under the SAME real `compile_build_jail`
    /// policy, with ONE variable: whether the interpreter is the ambient install or a nub-owned
    /// copy of it. The ambient arm is the CONTROL — without an arm where the defect still
    /// reproduces, a green staged arm is measuring nothing.
    ///
    /// It calls the same engine entry the product does
    /// (`windows_jail_bin::stage_appcontainer_readable_copy`) rather than re-implementing the
    /// staging, because a probe that re-implements the mechanism measures the probe.
    fn interpreter_group(r: &mut Report, f: &Fixture) {
        let Some(source_exe) = ambient_node() else {
            for prop in INTERPRETER_PROPS {
                r.record(
                    prop,
                    false,
                    "(no ambient node.exe found — nothing measured)",
                );
            }
            return;
        };
        let source_root = source_exe
            .parent()
            .expect("node.exe has a parent")
            .to_path_buf();
        println!("  fact:interp-source={}", source_exe.display());
        // Whether the SOURCE install already publishes read to AppContainers is the fact that
        // decides whether this machine can exhibit the defect at all: on one that does (43 of the
        // 44 `C:\Program Files` children do — `nodejs` is the outlier), the ambient arm passes for
        // a reason that has nothing to do with nub, and the differential is VOID rather than green.
        let source_publishes = nub_sandbox::windows_leaf_grant_redundant(&source_root);
        println!("  fact:interp-source-publishes-aap={source_publishes}");

        let (ambient_rc, ambient_log) = lifecycle_arm(f, "ambient", &source_exe, &source_root);

        // Staged under the fixture's own cache home, which is where `compile_build_jail`'s `$cache`
        // anchor points — the same relationship the product's `<cache>/jail-bin/<key>` has, without
        // touching the real user cache.
        let staged_dir = f.home.join("cache/nub/pm/jail-bin/probe");
        let began = Instant::now();
        let staged = nub_sandbox::backend::windows_jail_bin::stage_appcontainer_readable_copy(
            &source_root,
            &staged_dir,
        );
        let staged_exe = staged_dir.join("node.exe");
        println!(
            "  fact:interp-stage-ms={} result={staged:?}",
            began.elapsed().as_millis()
        );
        // THE ZERO-PRIVILEGE CLAIM, and it is only a claim in the de-elevated arm — which is
        // exactly why it lives in this differential rather than in a Windows unit test.
        r.record(
            "interp-staging-succeeds",
            staged.is_ok() && staged_exe.is_file(),
            &format!("({staged:?}; node.exe present {})", staged_exe.is_file()),
        );

        let (staged_rc, staged_log) = lifecycle_arm(f, "staged", &staged_exe, &staged_dir);

        // The control. `!= 0` rather than a specific code because the ambient arm can fail at
        // either of two independent points — the ACE write, or the confined image open — and
        // pinning one would make the property FAIL when the OTHER cause fired.
        r.record(
            "interp-ambient-install-cannot-run-a-lifecycle-script",
            ambient_rc != 0 && !ambient_log.contains("LIFECYCLE-OK"),
            &format!("(rc {ambient_rc}; log {})", one_line(&ambient_log)),
        );
        r.record(
            "interp-staged-copy-runs-a-lifecycle-script-to-completion",
            staged_rc == 0 && staged_log.contains("LIFECYCLE-OK"),
            &format!("(rc {staged_rc}; log {})", one_line(&staged_log)),
        );
        // ATTRIBUTION, and it is what stops the staged arm passing for the wrong reason. A refused
        // read grant is now SKIPPED rather than fatal, so a launch can succeed while the child
        // quietly ran the ambient interpreter off some other PATH entry — which would look
        // identical here. The child reports its own `process.execPath`, so it cannot.
        r.record(
            "interp-staged-child-execpath-is-the-nub-owned-copy",
            staged_log
                .lines()
                .any(|l| l.trim().eq_ignore_ascii_case(&staged_exe.to_string_lossy())),
            &format!(
                "(expected {}; log {})",
                staged_exe.display(),
                one_line(&staged_log)
            ),
        );
        // THE ABI CONSTRAINT, measured rather than asserted: `prebuild-install` and
        // `node-gyp-build` both default the prebuild they fetch to `process.versions.modules` of
        // the RUNNING Node, so a staged interpreter reporting a different one would silently make
        // every package in that family download an unloadable binary. Both arms report it, so this
        // compares the copy against its own source rather than against a tabulated expectation.
        let (source_abi, copy_abi) = (abi_of(&ambient_log), abi_of(&staged_log));
        r.record(
            "interp-staged-abi-matches-the-source",
            copy_abi.is_some() && copy_abi == source_abi,
            &format!("(source {source_abi:?}, staged {copy_abi:?})"),
        );
        // The cost claim: a published tree is what the backend's own leaf grant SKIPS on, so a
        // lifecycle spawn writes no DACL across the ~2,400-entry tree. Asserted on a DEEP entry
        // too, which is the half that proves the ace was written BEFORE the copy — an entry
        // several levels down carries it only by inheritance at creation.
        r.record(
            "interp-staged-dir-publishes-read-to-appcontainers",
            nub_sandbox::windows_leaf_grant_redundant(&staged_dir),
            "(an inheritable ALL APPLICATION PACKAGES ace ⇒ the per-spawn leaf grant skips)",
        );
        let deep = staged_dir.join("node_modules/npm/bin");
        r.record(
            "interp-staged-deep-entry-inherited-the-ace-at-creation",
            deep.is_dir() && nub_sandbox::windows_leaf_grant_redundant(&deep),
            &format!("({} exists {})", deep.display(), deep.is_dir()),
        );
        // Confinement survives it: the jail is not merely startable, it still withholds what it
        // never granted.
        let policy = build_jail_for(f, &staged_exe, &staged_dir);
        let denied = code(
            &policy,
            f,
            &f.package,
            &["__sbxchild__", "read", &f.secret.to_string_lossy()],
        );
        r.record(
            "interp-staged-ungranted-secret-still-refused",
            denied == 5 || denied == 9,
            &format!("(child exit {denied}; 5/9 = denied)"),
        );
    }

    /// Every property [`interpreter_group`] reports, so a machine with no Node still emits a
    /// verdict for each rather than leaving the table looking complete with rows missing.
    const INTERPRETER_PROPS: &[&str] = &[
        "interp-staging-succeeds",
        "interp-ambient-install-cannot-run-a-lifecycle-script",
        "interp-staged-copy-runs-a-lifecycle-script-to-completion",
        "interp-staged-child-execpath-is-the-nub-owned-copy",
        "interp-staged-abi-matches-the-source",
        "interp-staged-dir-publishes-read-to-appcontainers",
        "interp-staged-deep-entry-inherited-the-ace-at-creation",
        "interp-staged-ungranted-secret-still-refused",
    ];

    /// The all-users MSI install first, because it is the configuration that exhibits the defect
    /// and the one a real user has; the runner's unzipped `hostedtoolcache` Node otherwise, which
    /// carries no AppContainer ace either. Whichever it found is reported, so a run that measured
    /// an already-readable Node is legible rather than silently reassuring.
    fn ambient_node() -> Option<PathBuf> {
        let msi = PathBuf::from(std::env::var("ProgramFiles").unwrap_or_default())
            .join("nodejs/node.exe");
        if msi.is_file() {
            return Some(msi);
        }
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .map(|d| d.join("node.exe"))
            .find(|c| c.is_file())
    }

    /// One arm: the real production policy for `exe`, running a real `.cmd` lifecycle script
    /// through `cmd.exe` — which is how aube spawns one, and the shape that makes the child's own
    /// `node` a NESTED spawn from inside the AppContainer rather than one nub performs itself.
    ///
    /// The script deliberately contains NO PIPE. A piped `child_process` spawn under an
    /// AppContainer hangs in libuv's named-pipe retry, which is a SEPARATE defect with a separate
    /// repair (the `NODE_OPTIONS` stdio shim) — including one here would make a hang
    /// indistinguishable from an interpreter that could not be read.
    fn lifecycle_arm(f: &Fixture, tag: &str, exe: &Path, root: &Path) -> (i32, String) {
        let marker = f.package.join(format!("lc-{tag}.log"));
        let script = f.package.join(format!("lc-{tag}.cmd"));
        let entry = f.package.join(format!("lc-{tag}.js"));
        // Reaching the distribution's BUNDLED npm tree is the exact cell an un-ACE'd install fails
        // on, and requiring a file out of it is the cheapest honest way to ask for it.
        std::fs::write(
            &entry,
            format!(
                "console.log(process.execPath);\n\
                 console.log('abi=' + process.versions.modules);\n\
                 console.log('npm=' + require({}).version);\n",
                js_string(&root.join("node_modules/npm/package.json"))
            ),
        )
        .expect("write the arm's entry file");
        // `node` BARE, not by absolute path: a PATH search is itself a directory probe, and a
        // confined child is refused on an un-ACE'd install tree — which surfaces as `'node' is not
        // recognized`, a different and earlier failure than a refused image open. Both are the
        // defect; naming the interpreter absolutely would have measured only the second.
        std::fs::write(
            &script,
            format!(
                "@echo off\r\n\
                 node -p \"process.version\" > \"{m}\" 2>&1\r\n\
                 if errorlevel 1 exit /b 11\r\n\
                 node \"{e}\" >> \"{m}\" 2>&1\r\n\
                 if errorlevel 1 exit /b 12\r\n\
                 echo LIFECYCLE-OK>> \"{m}\"\r\n",
                m = marker.display(),
                e = entry.display()
            ),
        )
        .expect("write the arm's script");

        let policy = build_jail_for(f, exe, root);
        let comspec = std::env::var("ComSpec").unwrap_or_else(|_| {
            format!(
                "{}\\System32\\cmd.exe",
                std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into())
            )
        });
        let spec = CommandSpec::new(&comspec)
            .arg("/c")
            .arg(&script)
            .cwd(&f.package);
        let rc = match apply(&policy, spec) {
            Ok(p) => match p.status() {
                Ok(s) => s.code().unwrap_or(-1),
                Err(e) => {
                    println!("    [{tag} status Err] {e} os={:?}", e.raw_os_error());
                    -101
                }
            },
            Err(d) => {
                println!("    [{tag} apply Err] {d:?}");
                -100
            }
        };
        let log = std::fs::read_to_string(&marker).unwrap_or_default();
        println!("    arm:{tag} rc={rc}");
        for line in log.lines() {
            println!("      {tag}| {line}");
        }
        (rc, log)
    }

    /// The REAL production policy — `compile_build_jail`, the entry aube's lifecycle hook drives —
    /// with the interpreter and its distribution's global package tree as the only variables. The
    /// env mirrors what the embedder constructs: both interpreter spellings named, and `PATH`
    /// carrying the distribution plus the OS floor.
    fn build_jail_for(f: &Fixture, exe: &Path, root: &Path) -> SandboxPolicy {
        let system32 = format!(
            "{}\\System32",
            std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into())
        );
        let mut ambient: BTreeMap<String, String> = std::env::vars().collect();
        ambient.insert("PATH".to_string(), format!("{};{system32}", root.display()));
        ambient.insert(
            "npm_node_execpath".to_string(),
            exe.to_string_lossy().into_owned(),
        );
        ambient.insert("NODE".to_string(), exe.to_string_lossy().into_owned());
        let homes = nub_sandbox::Homes {
            home: f.home.clone(),
            tmp: std::env::temp_dir(),
            cache: f.home.join("cache"),
            project: f.project.clone(),
        };
        nub_sandbox::compile_build_jail(
            homes,
            &f.package,
            vec![exe.to_path_buf()],
            vec![root.join("node_modules")],
            ambient,
        )
        .expect("compile_build_jail")
    }

    /// A Windows path as a JS string literal — backslashes doubled, so the emitted `require()`
    /// argument is the path rather than a run of escape sequences.
    fn js_string(p: &Path) -> String {
        format!("\"{}\"", p.to_string_lossy().replace('\\', "\\\\"))
    }

    fn abi_of(log: &str) -> Option<&str> {
        log.lines().find_map(|l| l.trim().strip_prefix("abi="))
    }

    /// A child log flattened onto the single `prop:` line, so a verdict carries its own evidence
    /// without the reader having to correlate it against the interleaved arm output.
    fn one_line(log: &str) -> String {
        let flat: Vec<&str> = log
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let joined = flat.join(" / ");
        match joined.char_indices().nth(240) {
            Some((cut, _)) => format!("{}…", &joined[..cut]),
            None => joined,
        }
    }

    // ── the differential driver ───────────────────────────────────────────────────

    struct Arm {
        /// The `TokenIsElevated` flag. Informative only — `CreateRestrictedToken` copies it.
        elevated: bool,
        /// Access-checked administrative authority. THE gate.
        admin: bool,
        /// Mandatory integrity level RID (8192 Medium = standard-user, 12288 High).
        il: u32,
        props: Vec<(String, bool)>,
    }

    /// Parse an arm's marker. `None` ⇒ the arm never wrote one, which is the "it did not
    /// run" case that must never read as a pass.
    fn read_marker(path: &Path) -> Option<Arm> {
        let raw = std::fs::read_to_string(path).ok()?;
        let (mut elevated, mut admin, mut il) = (None, None, None);
        let mut props = Vec::new();
        for line in raw.lines() {
            if let Some(v) = line.strip_prefix("elevated=") {
                elevated = Some(v.trim() == "1");
            } else if let Some(v) = line.strip_prefix("admin=") {
                admin = Some(v.trim() == "1");
            } else if let Some(v) = line.strip_prefix("il=") {
                il = v.trim().parse().ok();
            } else if let Some(rest) = line.strip_prefix("prop:") {
                let (name, tail) = rest.split_once('=')?;
                props.push((name.to_string(), tail.starts_with("PASS")));
            }
        }
        Some(Arm {
            elevated: elevated?,
            admin: admin?,
            il: il?,
            props,
        })
    }

    /// Medium integrity — the level a standard user's token carries.
    const IL_MEDIUM: u32 = 8192;

    pub fn differential_main() -> i32 {
        let parent_elevated = is_elevated();
        println!(
            "BASELINE parent user={} IsElevated={}",
            std::env::var("USERNAME").unwrap_or_default(),
            u8::from(parent_elevated)
        );
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("FATAL: current_exe: {e}");
                return 2;
            }
        };
        let dir = std::env::temp_dir().join(format!("nub-deelev-markers-{}", std::process::id()));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("FATAL: marker dir: {e}");
            return 2;
        }
        let elev_marker = dir.join("elevated.txt");
        let deelev_marker = dir.join("deelevated.txt");

        // Arm A — the ambient token (elevated on CI). Skipped when the parent is already
        // unelevated: there is no elevated arm to pair with, and a fabricated one would be
        // the same measurement twice.
        let mut elev_rc = None;
        if parent_elevated {
            println!("── ARM A: ambient (elevated) token ─────────────────────────");
            elev_rc = Some(
                std::process::Command::new(&exe)
                    .args(["__jailarm__", &elev_marker.to_string_lossy()])
                    .status()
                    .map(|s| s.code().unwrap_or(-1))
                    .unwrap_or(-1),
            );
        }

        // Arm B — the de-elevated token.
        println!("── ARM B: de-elevated token ────────────────────────────────");
        let (deelev_rc, route) = if parent_elevated {
            match deelevated_primary_token() {
                Ok((token, route)) => {
                    println!("DEELEV route={route}");
                    let rc = spawn_as_token(
                        token,
                        &exe,
                        &["__jailarm__", &deelev_marker.to_string_lossy()],
                    );
                    // SAFETY: the token is ours and the child has already exited.
                    unsafe { windows_sys::Win32::Foundation::CloseHandle(token) };
                    match rc {
                        Ok(c) => (c, route),
                        Err(e) => {
                            eprintln!("FATAL: CreateProcessAsUserW failed: {e}");
                            return 2;
                        }
                    }
                }
                Err(e) => {
                    eprintln!("FATAL: could not obtain a de-elevated primary token: {e}");
                    return 2;
                }
            }
        } else {
            // Already unelevated: THIS process is the de-elevated arm's token, so run it
            // directly. Honest single-sided result, not a fabricated pair.
            println!("DEELEV route=already-unelevated");
            (
                std::process::Command::new(&exe)
                    .args(["__jailarm__", &deelev_marker.to_string_lossy()])
                    .status()
                    .map(|s| s.code().unwrap_or(-1))
                    .unwrap_or(-1),
                "already-unelevated",
            )
        };

        // ── the anti-hollow gate ────────────────────────────────────────────────
        let mut fails: Vec<String> = Vec::new();
        let Some(deelev) = read_marker(&deelev_marker) else {
            eprintln!("FATAL: the de-elevated arm wrote NO marker — it did not run");
            return 2;
        };
        // THE gate: the arm must have failed an access check only an administrator passes.
        // A token flag is not enough — route 2 leaves `TokenIsElevated` stale, so trusting
        // it would either reject a genuinely de-elevated arm or, worse on some other host,
        // accept an elevated one.
        if deelev.admin {
            eprintln!(
                "FATAL: the de-elevated arm still holds administrative authority via route \
                 {route} (opened the SCM for CREATE_SERVICE) — de-elevation did not happen, \
                 so nothing was measured"
            );
            return 2;
        }
        // Where the flag IS meaningful (the linked token is a real standard-user token), it
        // must agree.
        if route == "linked-token" && deelev.elevated {
            eprintln!("FATAL: the linked token reports IsElevated=1");
            return 2;
        }
        // Standard-user-equivalent on the integrity axis too, so the substitution is never
        // WEAKER than the principal it stands in for.
        if deelev.il > IL_MEDIUM {
            eprintln!(
                "FATAL: the de-elevated arm ran at integrity level {} (> medium {IL_MEDIUM}) — \
                 it is not standard-user-equivalent",
                deelev.il
            );
            return 2;
        }
        if deelev.props.is_empty() {
            eprintln!("FATAL: the de-elevated arm recorded zero properties");
            return 2;
        }
        if deelev_rc != 0 {
            fails.push(format!("de-elevated arm exited {deelev_rc}"));
        }

        let elev = elev_rc.map(|rc| (rc, read_marker(&elev_marker)));
        if let Some((rc, marker)) = &elev {
            match marker {
                None => fails.push("the elevated arm wrote no marker".to_string()),
                Some(arm) => {
                    // The differential is only a differential if arm A really did hold the
                    // authority arm B lacks.
                    if !arm.admin {
                        fails.push(
                            "the elevated arm did NOT hold administrative authority — the \
                             two arms did not differ"
                                .to_string(),
                        );
                    }
                    if *rc != 0 {
                        fails.push(format!("elevated arm exited {rc}"));
                    }
                    // A property present in one arm and absent in the other is a silently
                    // skipped measurement, which must not read as agreement.
                    let a: Vec<&String> = arm.props.iter().map(|(n, _)| n).collect();
                    let b: Vec<&String> = deelev.props.iter().map(|(n, _)| n).collect();
                    if a != b {
                        fails.push(format!(
                            "arms measured different properties: {a:?} vs {b:?}"
                        ));
                    }
                }
            }
        }

        let elev_arm = elev.as_ref().and_then(|(_, m)| m.as_ref());
        println!("\n── DIFFERENTIAL ────────────────────────────────────────────");
        println!("de-elevation route: {route}");
        println!(
            "{:<58} {:>10} {:>14}",
            "token state", "elevated", "de-elevated"
        );
        println!(
            "{:<58} {:>10} {:>14}",
            "admin authority (SCM)",
            elev_arm.map_or("n/a", |a| if a.admin { "YES" } else { "NO" }),
            if deelev.admin { "YES" } else { "NO" }
        );
        println!(
            "{:<58} {:>10} {:>14}",
            "integrity level",
            elev_arm.map_or("n/a".to_string(), |a| a.il.to_string()),
            deelev.il
        );
        println!(
            "{:<58} {:>10} {:>14}",
            "TokenIsElevated flag (stale)",
            elev_arm.map_or("n/a", |a| if a.elevated { "1" } else { "0" }),
            if deelev.elevated { "1" } else { "0" }
        );
        println!(
            "{:<58} {:>10} {:>14}",
            "property", "elevated", "de-elevated"
        );
        for (name, ok) in &deelev.props {
            let e = elev_arm
                .and_then(|a| a.props.iter().find(|(n, _)| n == name))
                .map(|(_, v)| if *v { "PASS" } else { "FAIL" })
                .unwrap_or("n/a");
            println!(
                "{name:<58} {e:>10} {:>14}",
                if *ok { "PASS" } else { "FAIL" }
            );
            if !ok {
                fails.push(format!("{name} FAILED de-elevated"));
            }
        }

        let _ = std::fs::remove_dir_all(&dir);
        if fails.is_empty() {
            println!("\nWINDOWS BUILD JAIL HOLDS WITH NO ELEVATION (route={route})");
            0
        } else {
            eprintln!("\nDE-ELEVATION DIFFERENTIAL FAILED:");
            for f in &fails {
                eprintln!("  - {f}");
            }
            1
        }
    }
}

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
//! THE PER-PACKAGE EGRESS GROUP rides this binary for the same reason as the two above — it
//! needs both principals — and asks the half nothing here had ever measured: does a CATALOGUED
//! package actually REACH the network on Windows. Every Windows egress cell before it compiled
//! with `package_name = None`, i.e. the denied side; the granted side was asserted from the
//! generation chain and never observed, so a break anywhere along it would have left every
//! shipped grant silently inert on a third of the platforms. See [`win::net_grant`].
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
    use std::os::windows::process::CommandExt;
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
            Some("resolve") => resolve_connect(&a[1], a[2].parse().unwrap_or(0)),
            // exec <marker> <deadline_secs> <sink> <program> [args…] — a NESTED spawn from
            // inside the AppContainer, which is the real lifecycle shape, with a deadline the
            // jail cannot outlive. The bound lives HERE rather than in the parent because a
            // parent-side `status()` is a blocking wait with nothing to interrupt it: a cell
            // that hangs would burn the whole job and CI discards a cancelled job's log.
            Some("exec") => run_bounded(&a[1], &a[2], &a[3], &a[4], &a[5..]),
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

    /// Resolve `host` and connect. Kept SEPARATE from [`connect`] because the two fail by
    /// different mechanisms under an AppContainer: name resolution is an RPC to the DNS Client
    /// service rather than a socket, so it can outlive a withheld `internetClient`. A grant that
    /// opens sockets but leaves `getaddrinfo` dead is inert for every real package, all of which
    /// fetch by hostname — so a literal-IP connect alone would answer the wrong question.
    /// 7 = resolution failed; the remaining codes match `connect`.
    fn resolve_connect(host: &str, port: u16) -> i32 {
        use std::net::ToSocketAddrs;
        let addr = match (host, port).to_socket_addrs() {
            Ok(mut it) => match it.next() {
                Some(a) => a,
                None => {
                    println!("    [resolve] getaddrinfo({host}) returned no addresses");
                    return 7;
                }
            },
            Err(e) => {
                println!(
                    "    [resolve] getaddrinfo({host}) failed: {e} raw={:?}",
                    e.raw_os_error()
                );
                return 7;
            }
        };
        println!("    [resolve] {host} -> {addr}");
        match TcpStream::connect_timeout(&addr, Duration::from_secs(8)) {
            Ok(_) => 0,
            Err(e) if e.raw_os_error() == Some(10013) => 5,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => 6,
            Err(e) => {
                println!(
                    "    [resolve] connect failed: {e} raw={:?}",
                    e.raw_os_error()
                );
                9
            }
        }
    }

    /// Spawn `program` with FILE-backed stdio (a pipe under the AppContainer hangs in libuv's
    /// named-pipe retry — a separate defect with a separate repair, and including one here
    /// would make a hang indistinguishable from a refused image open), wait to a deadline, and
    /// record the outcome as the marker's first line. `spawn-refused` is kept distinct from
    /// every exit code: a program the jail will not let the child open is a DIFFERENT finding
    /// from one that ran and failed.
    fn run_bounded(marker: &str, secs: &str, sink: &str, program: &str, args: &[String]) -> i32 {
        let marker = Path::new(marker);
        let Ok(out) = std::fs::File::create(sink) else {
            let _ = std::fs::write(marker, "sink-create-failed");
            return 9;
        };
        let Ok(err) = out.try_clone() else {
            let _ = std::fs::write(marker, "sink-clone-failed");
            return 9;
        };
        let spawned = std::process::Command::new(program)
            .args(args)
            .stdout(std::process::Stdio::from(out))
            .stderr(std::process::Stdio::from(err))
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::write(
                    marker,
                    format!(
                        "spawn-refused kind={:?} raw={:?}",
                        e.kind(),
                        e.raw_os_error()
                    ),
                );
                return 5;
            }
        };
        let start = Instant::now();
        let deadline = Duration::from_secs(secs.parse().unwrap_or(30));
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let _ = std::fs::write(marker, format!("exit={:?}", status.code()));
                    return 0;
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = std::fs::write(marker, format!("waiterr {e:?}"));
                    return 9;
                }
            }
            if start.elapsed() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::write(marker, "HUNG");
                return 7;
            }
            std::thread::sleep(Duration::from_millis(100));
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

        // (6b) the granted half of the same production path — the catalogued identity actually
        // reaching the network, which (6) can never show because it compiles with `None`.
        net_grant(&mut r, &f);

        // (7) …and with the REAL interpreter rather than the probe binary, which is the one
        // variable (6) cannot vary and the one that carried the showstopper.
        let staged = interpreter_group(&mut r, &f);

        // (8) IS THE ANCESTOR REPAIR NECESSARY AT ALL — the deletion question. Runs on the
        //     STAGED interpreter because that is the only one usable de-elevated, so it cannot
        //     run without (7) having produced one.
        match &staged {
            Some((exe, root)) => ancestor_necessity(&mut r, &f, exe, root),
            None => {
                for prop in NECESSITY_PROPS {
                    r.record(prop, false, "(no staged interpreter — nothing measured)");
                }
            }
        }

        // (9) DOES ES-MODULE RESOLUTION SURVIVE THE JAIL — the repair measured against the real
        //     AppContainer rather than the macOS simulation, which cannot model a native realpath
        //     refusal. Same dependence on (7) as (8), and for the same reason.
        match &staged {
            Some((exe, root)) => esm_resolution(&mut r, &f, exe, root),
            None => {
                for prop in ESM_PROPS {
                    r.record(prop, false, "(no staged interpreter — nothing measured)");
                }
            }
        }

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
            None,
            None,
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

    // ── the per-package egress group: does a CATALOGUED grant actually work here? ──

    /// A literal address, so the primitive is measured without DNS in the way. `1.1.1.1:443`
    /// is the same target `egress-deny` uses, which is what makes the two comparable.
    const EGRESS_IP: &str = "1.1.1.1";
    const EGRESS_PORT: &str = "443";
    /// The host every `packageNetwork.full` entry in the catalog actually fetches, so the DNS
    /// cell measures the name a granted package really resolves rather than a stand-in.
    const EGRESS_HOST: &str = "github.com";

    /// Property names in the order recorded. The parent's anti-hollow contract requires the SAME
    /// property set from both arms, so every path out of [`net_grant`] must record all of them —
    /// this array is what a bail-out fills in.
    const NET_GRANT_PROPS: [&str; 10] = [
        "netgrant-gate-catalog-nonempty",
        "netgrant-gate-unconfined-egress-reachable",
        "netgrant-gate-catalogued-child-starts",
        "netgrant-gate-uncatalogued-child-starts",
        "netgrant-plan-catalogued-net-unenforced",
        "netgrant-plan-uncatalogued-net-enforced",
        "netgrant-catalogued-connects",
        "netgrant-catalogued-resolves-dns",
        "netgrant-every-catalogued-name-connects",
        "netgrant-uncatalogued-refused",
    ];

    /// A name no catalog will ever hold, used as the paired denied identity. It differs from
    /// [`production_jail`]'s `None` on purpose: `None` is aube withholding an identity, while
    /// this is an identity the table does not admit — the case a real unvetted dependency is in.
    const UNCATALOGUED: &str = "nub-probe-definitely-not-in-the-catalog";

    /// DOES A CATALOGUED PACKAGE REACH THE NETWORK ON WINDOWS — the question the rest of this
    /// binary never asked. [`production_jail`] compiles with `package_name = None`, so every
    /// Windows egress measurement to date has been of the DENIED half; the granted half was
    /// asserted from the generation path (`preset::build_jail_net` → `net: true` → `enforce =
    /// false` → the backend's `allow_internet: !policy.net.enforce` → the `internetClient`
    /// capability SID) and never observed. If that chain were broken anywhere, every catalogued
    /// grant would be silently inert on a third of the supported platforms and the corpus would
    /// score those packages broken for a reason with nothing to do with the catalog.
    ///
    /// A green positive arm alone cannot answer it, which is why this is a differential on ONE
    /// variable — the identity handed to `compile_build_jail` — with two controls that fail in
    /// opposite directions: an UNJAILED connect (a refused positive arm is otherwise
    /// indistinguishable from a runner with no egress) and a `token` launch per policy (a jail
    /// that cannot start a child at all would otherwise read as a network block).
    fn net_grant(r: &mut Report, f: &Fixture) {
        let allowed = nub_sandbox::package_network_allowed();
        println!(
            "  fact: PACKAGE_NETWORK_ALLOWED ({} entries) = {allowed:?}",
            allowed.len()
        );
        r.record(
            "netgrant-gate-catalog-nonempty",
            !allowed.is_empty(),
            &format!("({} entries in the compiled egress set)", allowed.len()),
        );

        let bare = std::process::Command::new(&f.child)
            .args(["__sbxchild__", "connect", EGRESS_IP, EGRESS_PORT])
            .status()
            .map_or(-1, |s| s.code().unwrap_or(-1));
        r.record(
            "netgrant-gate-unconfined-egress-reachable",
            bare == 0,
            &format!("(UNJAILED connect {EGRESS_IP}:{EGRESS_PORT} exit {bare}; 0 expected)"),
        );

        let Some(&granted) = allowed.first() else {
            for p in &NET_GRANT_PROPS[2..] {
                r.record(p, false, "(the egress set is empty — nothing to measure)");
            }
            return;
        };
        println!("  fact: binding cells use the catalogued name `{granted}`");

        let compile = |name: Option<&str>| {
            nub_sandbox::compile_build_jail(
                nub_sandbox::Homes {
                    home: f.home.clone(),
                    tmp: std::env::temp_dir(),
                    cache: f.home.join("cache"),
                    project: f.project.clone(),
                },
                &f.package,
                name,
                Some("1.0.0"),
                vec![f.child.clone()],
                Vec::new(),
                std::env::vars().collect(),
            )
        };
        let (grant_policy, deny_policy) =
            match (compile(Some(granted)), compile(Some(UNCATALOGUED))) {
                (Ok(g), Ok(d)) => (g, d),
                (g, d) => {
                    let detail = format!(
                        "(compile err granted={:?} uncatalogued={:?})",
                        g.err(),
                        d.err()
                    );
                    for p in &NET_GRANT_PROPS[2..] {
                        r.record(p, false, &detail);
                    }
                    return;
                }
            };

        let grant_start = code(&grant_policy, f, &f.package, &["__sbxchild__", "token"]);
        r.record(
            "netgrant-gate-catalogued-child-starts",
            grant_start == 0,
            &format!(
                "(`{granted}` policy, child exit {grant_start}; 0 = launched in an AppContainer)"
            ),
        );
        let deny_start = code(&deny_policy, f, &f.package, &["__sbxchild__", "token"]);
        r.record(
            "netgrant-gate-uncatalogued-child-starts",
            deny_start == 0,
            &format!("(`{UNCATALOGUED}` policy, child exit {deny_start}; 0 = launched)"),
        );

        // The compiled shape, recorded next to the behaviour it is supposed to produce. On its
        // own it proves nothing — it is the pure-Rust half that already has host unit tests —
        // but paired with the connect cells it localises a failure to the compiler or to the
        // backend instead of leaving "egress does not work" undecomposed.
        r.record(
            "netgrant-plan-catalogued-net-unenforced",
            !grant_policy.net.enforce,
            &format!(
                "(`{granted}` compiled net.enforce={}; false ⇒ the backend grants internetClient)",
                grant_policy.net.enforce
            ),
        );
        r.record(
            "netgrant-plan-uncatalogued-net-enforced",
            deny_policy.net.enforce,
            &format!(
                "(`{UNCATALOGUED}` compiled net.enforce={}; true ⇒ capability withheld)",
                deny_policy.net.enforce
            ),
        );

        let grant_connect = code(
            &grant_policy,
            f,
            &f.package,
            &["__sbxchild__", "connect", EGRESS_IP, EGRESS_PORT],
        );
        r.record(
            "netgrant-catalogued-connects",
            grant_connect == 0,
            &format!("(`{granted}` → {EGRESS_IP}:{EGRESS_PORT} exit {grant_connect}; 0 expected)"),
        );

        let grant_dns = code(
            &grant_policy,
            f,
            &f.package,
            &["__sbxchild__", "resolve", EGRESS_HOST, EGRESS_PORT],
        );
        r.record(
            "netgrant-catalogued-resolves-dns",
            grant_dns == 0,
            &format!("(`{granted}` → {EGRESS_HOST}:{EGRESS_PORT} exit {grant_dns}; 0 expected, 7 = getaddrinfo failed)"),
        );

        // EVERY admitted name, not just the first. The binding question is whether the grants
        // WE SHIP work, and a per-name curated entry could compile to something that fails to
        // start on Windows while its neighbour is fine — which one arbitrary name cannot see.
        let mut broken: Vec<String> = Vec::new();
        for &name in allowed {
            let outcome = match compile(Some(name)) {
                Err(e) => format!("compile-err {e:?}"),
                Ok(p) => {
                    let c = code(
                        &p,
                        f,
                        &f.package,
                        &["__sbxchild__", "connect", EGRESS_IP, EGRESS_PORT],
                    );
                    if c == 0 {
                        "connect=0".to_string()
                    } else {
                        format!("connect={c}")
                    }
                }
            };
            println!("  fact: catalogued `{name}` {outcome}");
            if outcome != "connect=0" {
                broken.push(format!("{name} ({outcome})"));
            }
        }
        r.record(
            "netgrant-every-catalogued-name-connects",
            broken.is_empty(),
            &format!("({} of {} failed: {broken:?})", broken.len(), allowed.len()),
        );

        let deny_connect = code(
            &deny_policy,
            f,
            &f.package,
            &["__sbxchild__", "connect", EGRESS_IP, EGRESS_PORT],
        );
        r.record(
            "netgrant-uncatalogued-refused",
            deny_connect == 5 || deny_connect == 6,
            &format!(
                "(`{UNCATALOGUED}` → {EGRESS_IP}:{EGRESS_PORT} exit {deny_connect}; 5/6 = denied)"
            ),
        );
        // Recorded but NOT gated: DNS is an RPC to the DNS Client service, so it surviving a
        // withheld capability is expected rather than a leak. It is here because a denied arm
        // that ALSO cannot resolve would mean the positive DNS cell above is measuring the
        // capability rather than the resolver, and the pair is the only way to tell.
        let deny_dns = code(
            &deny_policy,
            f,
            &f.package,
            &["__sbxchild__", "resolve", EGRESS_HOST, EGRESS_PORT],
        );
        println!(
            "  fact: uncatalogued resolve {EGRESS_HOST} exit {deny_dns} (5/6 denied, 7 = getaddrinfo failed)"
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
    fn interpreter_group(r: &mut Report, f: &Fixture) -> Option<(PathBuf, PathBuf)> {
        let Some(source_exe) = ambient_node() else {
            for prop in INTERPRETER_PROPS {
                r.record(
                    prop,
                    false,
                    "(no ambient node.exe found — nothing measured)",
                );
            }
            return None;
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

        // THE PROXIMATE CAUSE, asked directly and WITHOUT MUTATING ANYTHING. `set_ace` needs
        // `WRITE_DAC` on its target, so whether nub can grant the ambient interpreter at all is a
        // property of this token against that DACL — and opening the file for `WRITE_DAC` asks
        // exactly that. Writing a real ace to find out would leave a lasting widening on the
        // user's own Node install, which is not a probe's business.
        let write_dac = can_write_dacl(&source_exe);
        println!("  fact:interp-source-write-dac={write_dac}");

        let (ambient_rc, ambient_log) = lifecycle_arm(f, "ambient", &source_exe, &source_root);

        // Staged under the fixture's own cache home, which is where `compile_build_jail`'s `$cache`
        // anchor points — the same relationship the product's `<cache>/jail-bin/<key>` has, without
        // touching the real user cache.
        let staged_dir = f.home.join("cache").join("nub").join("pm").join("jail-bin");
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
            "interp-staging-needs-no-write-dac-anywhere-privileged",
            staged.is_ok() && staged_exe.is_file(),
            &format!("({staged:?}; node.exe present {})", staged_exe.is_file()),
        );

        let (staged_rc, staged_log) = lifecycle_arm(f, "staged", &staged_exe, &staged_dir);
        let ambient_ok = ambient_rc == 0 && ambient_log.contains("LIFECYCLE-OK");
        let staged_ok = staged_rc == 0 && staged_log.contains("LIFECYCLE-OK");

        // THE LAW, stated so it holds in BOTH arms rather than being an expectation that
        // contradicts itself across them. An earlier draft demanded the ambient arm FAIL in both,
        // which is wrong and would have been a self-inflicted red: an ELEVATED nub holds
        // `WRITE_DAC` on `%ProgramFiles%`, writes the ace, and the ambient interpreter works.
        // What is invariant is the DEPENDENCE — the ambient install is usable exactly when nub
        // could grant it — and stating it that way makes the elevated arm carry information
        // instead of being the same measurement twice.
        // `source_publishes` is the second disjunct and it is not hypothetical: run 30544198501
        // read this FALSE in the elevated arm and TRUE in the de-elevated one, on the same machine,
        // for reasons not yet established (the elevated arm's own grant on that path is the
        // suspect, but its teardown revokes a DIFFERENT sid than the one this matches, so the
        // mechanism is UNEXPLAINED and is not asserted here). Whatever the cause, an install the OS
        // already publishes is readable without nub granting anything, so the law has to admit it —
        // and both inputs are read BEFORE the arm runs.
        let readable_without_a_grant = write_dac || source_publishes;
        r.record(
            "interp-ambient-install-is-usable-only-where-nub-can-write-its-dacl",
            ambient_ok == readable_without_a_grant,
            &format!(
                "(write_dac {write_dac}, already-published {source_publishes}, lifecycle ok \
                 {ambient_ok}; rc {ambient_rc}; log {})",
                one_line(&ambient_log)
            ),
        );
        // Re-read at the END, so a run where an arm CHANGED the ambient install's
        // AppContainer-readability is visible rather than inferred. This is the reading the next
        // probe of that question starts from.
        println!(
            "  fact:interp-source-publishes-aap-after={}",
            nub_sandbox::windows_leaf_grant_redundant(&source_root)
        );
        // …and the fix is that the staged copy does NOT depend on it. This is the property the
        // whole change exists for, so it is gated in both arms and the de-elevated one is the
        // meaningful half.
        r.record(
            "interp-staged-copy-runs-a-lifecycle-script-without-write-dac",
            staged_ok,
            &format!("(rc {staged_rc}; log {})", one_line(&staged_log)),
        );
        // ATTRIBUTION, and it is what stops the staged arm passing for the wrong reason. A refused
        // read grant is now SKIPPED rather than fatal, so a launch can succeed while the child
        // quietly ran the ambient interpreter off some other PATH entry — which would look
        // identical here. The child reports its own `process.execPath`, so it cannot.
        //
        // Compared as PATHS, not strings. The first draft compared spellings and failed on a run
        // where the fix worked, because a `/`-containing `join` literal and Windows's own `\`
        // spelling name the same file — a control that varied more than the thing under test.
        r.record(
            "interp-staged-child-execpath-is-the-nub-owned-copy",
            staged_log.lines().any(|l| same_file(l.trim(), &staged_exe)),
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
        // compares the copy against its own source rather than against a tabulated expectation —
        // and it can only be answered where BOTH arms produced a reading.
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
        let deep = staged_dir.join("node_modules").join("npm").join("bin");
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

        // ── the fail-closed differential ──────────────────────────────────────────────
        //
        // A sibling lane measured `cmd.exe` de-elevated as 19-of-19 cells ABSENT — exit 1 at
        // 100 ms, empty transcript, `Access is denied.` — and read it as cmd being unusable under
        // confinement, which would make a different lifecycle shell necessary rather than merely
        // better. This arm settles it, because the leaf-read-grant loop is the ONE thing that
        // changed and it is exactly the suspect: `resolve_program` auto-grants the program FILE, so
        // launching `%SystemRoot%\System32\cmd.exe` attempts an ACE on it, and de-elevated a
        // standard user holds no `WRITE_DAC` there. Under `?` that aborted the launch — which
        // produces precisely that shape and is indistinguishable from cmd misbehaving.
        //
        // ONE VARIABLE: the same fixture, the same policy, the same script, the same token; only
        // the seam differs. Reported as a fact in the elevated arm (where the grant SUCCEEDS, so
        // there is nothing to abort and the arms cannot differ) and GATED de-elevated.
        // `can_write_dacl` is a FACT here and deliberately NOT what the property keys on. It is an
        // UNSOUND proxy for whether `set_ace` succeeds, measured: it returned false in the ELEVATED
        // arm on `System32\cmd.exe` while the real grant there plainly succeeded — that arm produced
        // cells under fail-closed and never aborted. An elevated token's DACL-write authority does
        // not come from the file's DACL, so an access-checked open cannot see it. Kept because it is
        // still right de-elevated and it is the reading a future reader reaches for; labelled so
        // nobody keys a verdict on it again.
        println!(
            "  fact:interp-cmd-program-grant-write-dac-open={} (UNSOUND above medium IL)",
            can_write_dacl(Path::new(&comspec_path()))
        );
        let (fc_rc, fc_log) = with_fail_closed_read_grants(|| {
            lifecycle_arm(f, "staged-failclosed", &staged_exe, &staged_dir)
        });
        let fc_cells = CMD_CELLS.iter().filter(|c| fc_log.contains(**c)).count();
        let soft_cells = CMD_CELLS
            .iter()
            .filter(|c| staged_log.contains(**c))
            .count();
        // THE PROPERTY, keyed on the MECHANISM rather than a proxy for it: fail-closed loses the
        // launch exactly when the leaf grant it aborts on is the one that was refused, which the
        // launcher names in its OWN error. Elevated that grant succeeds and the launch survives;
        // de-elevated it is refused and the launch dies with
        // `installing read grant ACE on C:\Windows\system32\cmd.exe failed` — verbatim the string a
        // sibling lane read as cmd being unusable under confinement. One law, both arms.
        let aborted = fc_rc == -101 && fc_cells == 0;
        let grant_refused = !program_grant_landed(f, &staged_exe, &staged_dir);
        r.record(
            "interp-cmd-under-fail-closed-aborts-iff-its-own-program-grant-is-refused",
            aborted == grant_refused,
            &format!(
                "(program grant refused {grant_refused}; fail-closed cells {fc_cells}/{}, rc \
                 {fc_rc}; fail-soft cells {soft_cells}/{})",
                CMD_CELLS.len(),
                CMD_CELLS.len()
            ),
        );
        // The consequence the sibling verdict turns on, and deliberately NOT "the whole battery":
        // de-elevated `dir /b` returns `Access is denied.` in the package dir even though it works
        // elevated, which is a real residual belonging to the ancestor/traverse work rather than to
        // the interpreter. What this asserts is the part the sibling read as ZERO — cmd starts, opens
        // its script, resolves `node` off the sanitized PATH, and reaches its verdict line. The full
        // tally stays a fact so a regression in the rest is still visible.
        for cell in [
            "STEP-CMD-OPENED-THE-SCRIPT",
            "STEP-BARE-NODE-RESOLVED-AND-RAN",
            "LIFECYCLE-OK",
        ] {
            r.record(
                &format!(
                    "interp-cmd-under-fail-soft-reaches-{}",
                    cell.to_ascii_lowercase()
                ),
                staged_log.contains(cell),
                &format!("({soft_cells}/{} cells total)", CMD_CELLS.len()),
            );
        }
        if staged_exe.is_file() {
            Some((staged_exe, staged_dir))
        } else {
            None
        }
    }

    // ── is the ancestor repair NECESSARY? ─────────────────────────────────────────────
    //
    // THE QUESTION, and it is a DELETION question. The backend repairs the ancestor chain in
    // two halves, and only one of them has ever been in doubt:
    //
    //  - The CAPABILITY half harvests the `S-1-15-3-65536-…` AppSilo SIDs off `C:\` and
    //    `C:\Users` and requests them. The kernel refuses that RID class outright
    //    (`0xc000000d` from `NtCreateLowBoxToken`, `ERROR_INVALID_PARAMETER` at launch), so the
    //    launch always falls back — `capability-fallbacks=1` in BOTH principals. It is measured
    //    dead and is not what this group is about.
    //  - The ACE half writes a non-inherited traverse ace where nub holds `WRITE_DAC`, which
    //    de-elevated is `%USERPROFILE%` and below only. Whether THAT is load-bearing is the
    //    open question, and this group is the arm that answers it.
    //
    // WHY IT IS PLAUSIBLY DEAD TOO. The repair exists for exactly one defect: Node's
    // `realpathSync` opens every path prefix as a TARGET starting at the volume root, and
    // bypass-traverse does not make an ancestor openable as a target. But `C:\` is owned by
    // TrustedInstaller and `C:\Users` by SYSTEM, and neither grants a standard group
    // `WRITE_DAC` — so de-elevated the walk still dies on its FIRST component whatever the
    // repair did further down. Meanwhile `realpath_shim_node_options` ships a userland walk that
    // TOLERATES a refused component when it is a strict ancestor of a granted root, which covers
    // the same defect and covers `C:\` too.
    //
    // THE ARM NOBODY HAD RUN. Every previous matrix varied the repair with NO preload, or
    // stamped the preload only in the repaired arm. The cell that decides deletion is
    // repair-OFF **with** the preload, next to repair-ON with the same preload, on one fixture
    // in one run. That is this group.
    //
    // THE GATE, and no arm counts as evidence without it. Two rounds of Windows conclusions
    // were retracted because they were measured on launches reproducing none of nub's repairs.
    // So every preload arm asserts the shim's own `globalThis` sentinel FROM INSIDE the confined
    // child, and every no-preload arm asserts its ABSENCE — a pair of cells that fails in either
    // direction invalidates the group rather than quietly passing it.
    //
    // THE NON-NODE RESIDUAL, which is the one argument that could save the ace half: the shim
    // is a Node shim and cannot help a tool that walks paths itself. The executable set is
    // closed and small — by PATH-resolved head across the corpus, `node`, `node-gyp` (nub's
    // own), `npm` and `npx`, all of which ARE Node — leaving `python`, MSVC/`MSBuild`, `git` and
    // the shells as the second-order set. Those are measured here as a repair-ON vs repair-OFF
    // differential with no preload involved, because for them the preload is irrelevant either
    // way.

    /// Every property this group reports, so the parent can require the SAME set from both arms
    /// and a silently-vanished cell reads as missing rather than as a pass.
    const NECESSITY_PROPS: &[&str] = &[
        "anc-gate-preload-installs-the-realpath-shim",
        "anc-gate-no-preload-leaves-the-shim-absent",
        "anc-production-stamp-reaches-node",
        "anc-control-absolute-require-fails-unpreloaded-unrepaired",
        "anc-node-absolute-require-works-preloaded-with-repair-off",
        "anc-node-bare-through-junction-works-preloaded-with-repair-off",
        "anc-node-npm-cli-entry-works-preloaded-with-repair-off",
        "anc-repair-changes-no-node-cell-under-the-preload",
        "anc-control-single-spelling-roots-still-lose-a-cell",
        "anc-nonnode-tools-identical-with-repair-off",
    ];

    /// The realpath preload term, with the SAME roots `build_jail.rs` passes in production: the
    /// project root, the package dir, and the interpreter. Scoping the tolerance to the jail's
    /// own anchors is what keeps it from being `--preserve-symlinks` — a component at or below a
    /// root is still `lstat`'d for real, so a store-cell link still decides which version
    /// resolves.
    fn realpath_term(f: &Fixture, exe: &Path) -> String {
        nub_sandbox::realpath_shim_node_options(&[
            f.project.clone(),
            f.package.clone(),
            exe.to_path_buf(),
        ])
    }

    /// The same term with every root spelled the way the FILESYSTEM spells it — which, now that
    /// `realpath_shim_node_options` stamps BOTH spellings, makes this the arm that carries only
    /// ONE. It is therefore the CONTROL for that fix rather than a candidate treatment: a
    /// canonical root canonicalizes to itself, so the expansion adds nothing here and the walk
    /// still meets the 8.3 spelling it cannot match.
    ///
    /// WHY THIS ARM EXISTS. The shim's tolerance rule is a string prefix test over lowercased
    /// `path.resolve` output, so it only fires when the refused component and the root share a
    /// spelling. `std::env::temp_dir()` hands back the 8.3 SHORT form (`C:\Users\RUNNER~1\…`)
    /// while the confined child walks the LONG form (`C:\Users\runneradmin\…`) — measured, both
    /// in this fixture's own paths and in the child's `cd`. So a root derived from `%TEMP%`
    /// matches nothing, and `lstat 'C:\Users\runneradmin'` is refused rather than tolerated.
    ///
    /// That makes this the ONE variable separating "the ancestor repair is load-bearing" from
    /// "the ancestor repair is covering for a root-spelling defect", and the two call for
    /// opposite next moves. `canonicalize` returns the verbatim `\\?\` form, which the shim
    /// strips for comparison but which is not what a root should carry, so it is removed here.
    fn realpath_term_longform(f: &Fixture, exe: &Path) -> String {
        let long = |p: &Path| -> PathBuf {
            std::fs::canonicalize(p)
                .map(|c| match c.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
                    Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
                    _ => c.clone(),
                })
                .unwrap_or_else(|_| p.to_path_buf())
        };
        let roots = [long(&f.project), long(&f.package), long(exe)];
        for r in &roots {
            println!("  fact:anc-longform-root={}", r.display());
        }
        nub_sandbox::realpath_shim_node_options(&roots)
    }

    /// Run npm's own entry point under the realpath preload PLUS [`refusal_tracer_term`], with the
    /// ancestor repair left ON, and print whatever the child managed to record. The elevated arm's
    /// trace is the reference the de-elevated one is read against.
    fn npm_cli_refusal_trace(
        f: &Fixture,
        exe: &Path,
        root: &Path,
        dir: &Path,
        npm_cli: &Path,
        realpath: &str,
    ) {
        let trace = dir.join("npmcli.trace");
        let _ = std::fs::remove_file(&trace);
        let policy = build_jail_with_env(
            f,
            exe,
            root,
            &[(
                "NODE_OPTIONS",
                format!("{realpath} {}", refusal_tracer_term(&trace)),
            )],
        );
        confined_exec(
            f,
            &policy,
            "trace-npmcli",
            60,
            exe,
            &[
                npm_cli.to_string_lossy().into_owned(),
                "--version".to_string(),
            ],
        );
        match std::fs::read_to_string(&trace) {
            Ok(text) => {
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    println!("  fact:anc-npmcli-trace| {line}");
                }
            }
            Err(e) => println!("  fact:anc-npmcli-trace-unreadable={e}"),
        }

        // THE BISECT, and it is what the tracer alone cannot give. npm's own CLI swallows the
        // error: `Npm.load` routes every failure through `#handleError`, which sets the exit code
        // from `err.errno` and re-throws into an exit handler whose only reporting channel is a
        // log stream `Display.load` had not yet resumed. Driving `Npm.load()` DIRECTLY from `-e`
        // runs the identical code with the rejection caught by the probe instead, so the cause
        // arrives on stderr with a stack.
        let plain = build_jail_with_env(f, exe, root, &[("NODE_OPTIONS", realpath.to_string())]);
        let npm_root = npm_cli
            .parent()
            .and_then(Path::parent)
            .expect("npm-cli.js sits at <npm>/bin/npm-cli.js");
        for (tag, body) in [
            (
                "bisect-require",
                format!(
                    "require({p});console.log('REQUIRE-OK')",
                    p = js_literal(&npm_root.join("lib/npm.js"))
                ),
            ),
            (
                "bisect-load-version",
                bisect_load(&npm_root.join("lib/npm.js"), "'npm','--version'"),
            ),
            // The same load WITHOUT `--version`, which is the arm that reaches everything the
            // early return skips — the cache mkdir, the logs dir, the log file. That is the shape
            // a real lifecycle `npm run build` takes, so a refusal there matters even though the
            // measured cell does not reach it.
            (
                "bisect-load-full",
                bisect_load(&npm_root.join("lib/npm.js"), "'npm','ls'"),
            ),
        ] {
            let (_, _, sink) = confined_exec(f, &plain, tag, 60, exe, &["-e".to_string(), body]);
            // Printed WHOLE rather than through `one_line`, which truncates at 240 characters —
            // the cause of this failure lives in the stack, which is longer than that.
            for line in sink.lines().filter(|l| !l.trim().is_empty()) {
                println!("  fact:anc-{tag}| {}", line.trim());
            }
        }
    }

    /// The `-e` body that drives `Npm.load()` directly and REPORTS the rejection.
    ///
    /// `argv` is spelled as a JS list because `@npmcli/config` reads `process.argv.slice(2)` —
    /// under `-e` `process.argv` is `[execPath]` alone, so a bare `'--version'` lands at index 1
    /// and is silently dropped. The leading `'npm'` is the placeholder that puts the real flag
    /// where the parser looks.
    fn bisect_load(npm_js: &Path, argv: &str) -> String {
        format!(
            "const N=require({p});const n=new N({{argv:[{argv}]}});\
             n.load().then(r=>console.log('LOAD-OK '+JSON.stringify(r)),\
             e=>console.log('LOAD-ERR code='+e.code+' errno='+e.errno+' syscall='+e.syscall\
             +' path='+e.path+' :: '+(e.stack||e)))",
            p = js_literal(npm_js)
        )
    }

    /// A second `--import` term that makes the confined child NAME its own refusals.
    ///
    /// WHY IT IS NEEDED AT ALL. npm's entry point dies `-4048` with an EMPTY transcript, which is
    /// the one failure shape that carries no information: npm buffers every log and output event
    /// until `Display.load` calls `log.resume()`, so anything that throws before that line exits
    /// with `err.errno` as the status (`getExitCodeFromError`, `lib/utils/error-message.js`) and
    /// prints nothing at all. The stack has to be taken from inside the child.
    ///
    /// Stamped AFTER the realpath term, so the `fs.realpathSync` it wraps is already the shim's
    /// walk and a refusal the shim declined to tolerate is recorded with the shim's own frames.
    /// Delivered as a `data:` URL for the same reason the shim is — it needs no grant.
    fn refusal_tracer_term(trace: &Path) -> String {
        use base64::Engine as _;
        let js = REFUSAL_TRACER.replace("__NUB_TRACE_OUT__", &js_literal(trace));
        format!(
            "--import data:text/javascript;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(js)
        )
    }

    const REFUSAL_TRACER: &str = r#"
import fs from "node:fs";
import fsp from "node:fs/promises";
const OUT = __NUB_TRACE_OUT__;
const lines = [];
const push = (s) => { if (lines.length < 300) lines.push(String(s).replace(/[\r\n]+/g, " ")); };
const rawWrite = fs.writeFileSync;
const flush = () => { try { rawWrite(OUT, lines.join("\n") + "\n"); } catch {} };
const frames = (e) => String((e && e.stack) || "")
  .split("\n").slice(1, 7)
  .map((l) => l.trim().replace(/data:text\/javascript;base64,[A-Za-z0-9+/=]+/g, "<preload>"))
  .join(" << ");
// DEDUPED, and the dedupe is what makes the trace readable rather than merely bounded: the
// realpath shim itself throws once per TOLERATED ancestor, so an undeduped trace is hundreds of
// copies of the same three components and the one refusal that was not tolerated is buried.
const seen = new Set();
const note = (ns, name, args, e) => {
  const a = args && args[0];
  const spelled = typeof a === "string" ? a : a && a.href ? a.href : String(a);
  const target = e.path ?? spelled;
  const key = `${ns}.${name}|${e.code}|${target}`;
  if (seen.has(key)) return;
  seen.add(key);
  push(`REFUSED ${ns}.${name} code=${e.code} errno=${e.errno} syscall=${e.syscall} path=${target} | ${frames(e)}`);
  flush();
};
const refused = (e) => e && (e.code === "EPERM" || e.code === "EACCES");
const wrapSync = (ns, tag, keys) => {
  for (const k of keys) {
    const orig = ns[k];
    if (typeof orig !== "function") continue;
    const wrapped = function (...a) {
      try { return orig.apply(this, a); }
      catch (e) { if (refused(e)) note(tag, k, a, e); throw e; }
    };
    if (orig.native) wrapped.native = orig.native;
    ns[k] = wrapped;
  }
};
const wrapAsync = (ns, tag, keys) => {
  for (const k of keys) {
    const orig = ns[k];
    if (typeof orig !== "function") continue;
    ns[k] = function (...a) {
      return Promise.resolve(orig.apply(this, a)).catch((e) => {
        if (refused(e)) note(tag, k, a, e);
        throw e;
      });
    };
  }
};
wrapSync(fs, "fs", ["statSync","lstatSync","readFileSync","readdirSync","realpathSync","openSync","mkdirSync","accessSync","readlinkSync","opendirSync","writeFileSync"]);
wrapAsync(fsp, "fsp", ["stat","lstat","readFile","readdir","realpath","open","mkdir","access","readlink","opendir","writeFile"]);
const realExit = process.exit.bind(process);
process.exit = (code) => { push(`EXIT code=${code} | ${frames(new Error("exit"))}`); flush(); return realExit(code); };
process.on("exit", (c) => { push(`ONEXIT code=${c} exitCode=${process.exitCode}`); flush(); });
process.on("uncaughtException", (e) => { push(`UNCAUGHT ${e && e.stack}`); flush(); });
process.on("unhandledRejection", (e) => { push(`UNHANDLED ${e && (e.stack || e)}`); flush(); });
for (const s of ["stdout", "stderr"]) {
  const stream = process[s];
  const orig = stream.write.bind(stream);
  stream.write = (chunk, ...rest) => { push(`${s.toUpperCase()} ${String(chunk).slice(0, 300)}`); return orig(chunk, ...rest); };
}
let cwd = "<uv_cwd refused>";
try { cwd = process.cwd(); } catch (e) { cwd = `<${e.code}>`; }
push(`START execPath=${process.execPath} cwd=${cwd} argv=${process.argv.slice(1).join(" ")}`);
flush();
"#;

    /// One confined NESTED launch: the probe child (already inside the AppContainer) spawns
    /// `program` on a deadline and writes both the launch outcome and the program's stdio where
    /// the parent can read them. Returns `(child_rc, outcome, sink)`.
    fn confined_exec(
        f: &Fixture,
        policy: &SandboxPolicy,
        tag: &str,
        secs: u32,
        program: &Path,
        args: &[String],
    ) -> (i32, String, String) {
        let marker = f.package.join(format!("nx-{tag}.out"));
        let sink = f.package.join(format!("nx-{tag}.sink"));
        let _ = std::fs::remove_file(&marker);
        let _ = std::fs::remove_file(&sink);
        let mut argv: Vec<String> = vec![
            "__sbxchild__".into(),
            "exec".into(),
            marker.to_string_lossy().into_owned(),
            secs.to_string(),
            sink.to_string_lossy().into_owned(),
            program.to_string_lossy().into_owned(),
        ];
        argv.extend(args.iter().cloned());
        let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
        let rc = code(policy, f, &f.package, &borrowed);
        let outcome = std::fs::read_to_string(&marker).unwrap_or_else(|_| "<no marker>".into());
        let sink_text = std::fs::read_to_string(&sink).unwrap_or_default();
        // The `data:` frame is DROPPED before truncating. A preload delivered as a data URL puts
        // ~15 kB of base64 at the head of any stack thrown inside it, so the first 240 characters
        // of a shim-thrown error are pure payload and the message — the whole point of reading a
        // sink — never appears. This is what turned "the shim threw" into a readable cause.
        let salient: String = sink_text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("data:"))
            .collect::<Vec<_>>()
            .join(" / ");
        println!(
            "    anc:{tag} rc={rc} outcome={outcome} sink={}",
            one_line(&salient)
        );
        (rc, outcome, sink_text)
    }

    /// A Windows path as a DOUBLE-quoted JS literal. Distinct from [`js_string`], which exists
    /// for text that has to survive cmd's own quoting; nothing here goes through a shell.
    fn js_literal(p: &Path) -> String {
        format!("\"{}\"", p.to_string_lossy().replace('\\', "\\\\"))
    }

    /// The four Node cells, run under one policy. Each writes its own marker from inside the
    /// confined child, so a verdict is something the child DID rather than something the harness
    /// inferred from an exit code.
    struct NodeCells {
        /// `globalThis` sentinels the child observed: `(realpath_shim, stdio_shim)`.
        sentinels: (bool, bool),
        /// `node <absolute entry>` whose body `require()`s a second file by absolute path.
        absolute_require: bool,
        /// A bare specifier resolved through a junction — nub's own `Isolated` layout.
        bare_through_junction: bool,
        /// The distribution's own `npm-cli.js` as an absolute entry point, which is the
        /// realpath-heaviest thing a real lifecycle script reaches for.
        npm_cli: bool,
    }

    impl NodeCells {
        fn detail(&self) -> String {
            format!(
                "(shim {} stdio {} | abs-require {} bare-junction {} npm-cli {})",
                self.sentinels.0,
                self.sentinels.1,
                self.absolute_require,
                self.bare_through_junction,
                self.npm_cli
            )
        }
        /// The three OUTCOME cells, without the sentinels — this is what a repair-ON vs
        /// repair-OFF comparison is over.
        fn outcomes(&self) -> (bool, bool, bool) {
            (
                self.absolute_require,
                self.bare_through_junction,
                self.npm_cli,
            )
        }
    }

    /// Which preloads actually reached the confined Node — `(realpath, stdio, netgate)`, read off
    /// `globalThis` FROM INSIDE the jail. This is the gate the whole group rests on: two earlier
    /// rounds of Windows conclusions were retracted because they were measured on launches that
    /// reproduced none of nub's repairs, and a stamped string is not evidence that Node saw it.
    ///
    /// `-e` deliberately, because it has NO main module and therefore never touches
    /// `resolveMainPath` — the only shape that can read the shim state in an arm where the entry
    /// point itself would be refused. `--import` preloads are awaited before the eval body, so a
    /// FALSE here means the preload genuinely did not install.
    fn sentinel_probe(
        f: &Fixture,
        policy: &SandboxPolicy,
        tag: &str,
        exe: &Path,
        dir: &Path,
    ) -> (bool, bool, bool) {
        let marker = dir.join(format!("{tag}.sentinel"));
        let _ = std::fs::remove_file(&marker);
        confined_exec(
            f,
            policy,
            &format!("{tag}-sentinel"),
            30,
            exe,
            &[
                "-e".into(),
                format!(
                    "require('fs').writeFileSync({m},['__nubJailRealpathShim',\
                     '__nubJailStdioShim','__nubJailNetGate'].map(k=>String(!!globalThis[k]))\
                     .join(' '))",
                    m = js_literal(&marker)
                ),
            ],
        );
        let text = std::fs::read_to_string(&marker).unwrap_or_default();
        println!("  fact:anc-{tag}-sentinels={text:?}");
        let flags: Vec<bool> = text.split_whitespace().map(|w| w == "true").collect();
        let at = |i: usize| flags.get(i).copied().unwrap_or(false);
        (at(0), at(1), at(2))
    }

    fn node_cells(
        f: &Fixture,
        policy: &SandboxPolicy,
        tag: &str,
        exe: &Path,
        dir: &Path,
        npm_cli: &Path,
    ) -> NodeCells {
        let markers = ["mainran", "deploaded", "bare"].map(|n| dir.join(format!("{tag}.{n}")));
        for m in &markers {
            let _ = std::fs::remove_file(m);
        }
        let [mainran, deploaded, bare] = &markers;

        let sentinels3 = sentinel_probe(f, policy, tag, exe, dir);
        let sentinels = (sentinels3.0, sentinels3.1);
        confined_exec(
            f,
            policy,
            &format!("{tag}-absreq"),
            30,
            exe,
            &[dir.join("main.js").to_string_lossy().into_owned()],
        );
        confined_exec(
            f,
            policy,
            &format!("{tag}-bare"),
            30,
            exe,
            &[
                "-e".into(),
                format!(
                    "require('dep');require('fs').writeFileSync({m},'bare')",
                    m = js_literal(bare)
                ),
            ],
        );
        confined_exec(
            f,
            policy,
            &format!("{tag}-npmcli"),
            45,
            exe,
            &[
                npm_cli.to_string_lossy().into_owned(),
                "--version".into(),
                // `npm --version` writes to stdout, which lands in the sink; the marker is what
                // the parent gates on, so the cell writes one itself via `--userconfig`-free
                // plumbing is not available — instead the sink is checked for a version line.
            ],
        );
        let npm_sink = std::fs::read_to_string(f.package.join(format!("nx-{tag}-npmcli.sink")))
            .unwrap_or_default();
        NodeCells {
            sentinels,
            absolute_require: mainran.is_file() && deploaded.is_file(),
            bare_through_junction: bare.is_file(),
            // A version line, not an exit code: npm exits 0 on some failures and the sink is the
            // only place its own answer appears.
            npm_cli: npm_sink.lines().any(|l| {
                let t = l.trim();
                !t.is_empty() && t.chars().next().is_some_and(|c| c.is_ascii_digit())
            }),
        }
    }

    /// The non-Node second-order executable set, run through one policy and reported as ONE
    /// transcript so two arms can be diffed rather than eyeballed cell by cell.
    fn nonnode_transcript(f: &Fixture, policy: &SandboxPolicy, tag: &str) -> String {
        let mut out = String::new();
        let system32 =
            PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()))
                .join("System32");
        let mut cells: Vec<(&str, PathBuf, Vec<String>)> = vec![
            (
                "cmd-dir",
                system32.join("cmd.exe"),
                vec!["/c".into(), "dir /b".into()],
            ),
            (
                "cmd-cd",
                system32.join("cmd.exe"),
                vec!["/c".into(), "cd".into()],
            ),
            (
                "where-node",
                system32.join("where.exe"),
                vec!["node".into()],
            ),
            (
                "powershell",
                system32.join("WindowsPowerShell/v1.0/powershell.exe"),
                vec![
                    "-NoProfile".into(),
                    "-Command".into(),
                    "Write-Output ps-ok".into(),
                ],
            ),
        ];
        // `git` and `python` are located rather than assumed: a machine without them yields an
        // ABSENT cell in BOTH arms, which is honest, where a hardcoded path would yield a
        // spawn-refused pair that looks like a measurement.
        for (label, exe_name, args) in [
            ("git-version", "git.exe", vec!["--version".to_string()]),
            (
                "git-toplevel",
                "git.exe",
                vec!["rev-parse".into(), "--show-toplevel".into()],
            ),
            ("python", "python.exe", vec!["-c".into(), "print(1)".into()]),
        ] {
            if let Some(p) = which(exe_name) {
                cells.push((label, p, args));
            }
        }
        for (label, program, args) in &cells {
            let (rc, outcome, sink) =
                confined_exec(f, policy, &format!("{tag}-{label}"), 20, program, args);
            out.push_str(&format!(
                "{label} rc={rc} outcome={outcome} sink={}\n",
                one_line(&sink)
            ));
        }
        out
    }

    fn which(exe_name: &str) -> Option<PathBuf> {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .map(|d| d.join(exe_name))
            .find(|c| c.is_file())
    }

    /// THE GROUP. One fixture, one run, the preload stamped in every arm that claims to measure
    /// the shipping configuration, and `NUB_SANDBOX_WIN_NO_ANCESTOR_REPAIR` as the only variable
    /// between the two arms being compared.
    fn ancestor_necessity(r: &mut Report, f: &Fixture, exe: &Path, root: &Path) {
        // ── the fixture: an absolute entry that requires an absolute dep, plus a bare
        //    specifier reached through a JUNCTION, which is nub's default `Isolated` shape and
        //    the one that forces a realpath even where a plain directory might not. `mklink /J`
        //    needs no privilege, which keeps the fixture representative of a real install.
        let dir = f.package.join("anc");
        let store = dir.join("store").join("dep@1.0.0");
        std::fs::create_dir_all(&store).expect("fixture store");
        std::fs::write(
            dir.join("package.json"),
            b"{\"name\":\"anc-fixture\",\"version\":\"0.0.0\"}\n",
        )
        .expect("fixture manifest");
        std::fs::write(
            store.join("package.json"),
            b"{\"name\":\"dep\",\"version\":\"1.0.0\",\"main\":\"index.js\"}\n",
        )
        .expect("dep manifest");
        let junction = f.package.join("node_modules").join("dep");
        std::fs::create_dir_all(f.package.join("node_modules")).expect("package node_modules");
        // `raw_arg`, and the mklink output is PRINTED. A first revision built the command line
        // through `Command::arg` — Rust's own quoting, applied on the way to a shell that does
        // not parse it that way — and discarded the output, so the junction was silently never
        // created and the bare-specifier cell failed in all six arm-instances INCLUDING the
        // control. Failing in the control is the tell that separated a harness defect from a
        // finding; without the output there was nothing to read it off.
        let mklink = std::process::Command::new(comspec_path())
            .raw_arg(format!(
                "/c mklink /J \"{}\" \"{}\"",
                junction.display(),
                store.display()
            ))
            .output();
        match &mklink {
            Ok(o) => println!(
                "  fact:anc-mklink status={:?} out={} err={}",
                o.status.code(),
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .replace('\n', " | "),
                String::from_utf8_lossy(&o.stderr)
                    .trim()
                    .replace('\n', " | ")
            ),
            Err(e) => println!("  fact:anc-mklink spawn-failed={e}"),
        }
        println!("  fact:anc-junction-created={}", junction.is_dir());

        // ── the arm-local paths. Every marker is baked into the JS as an absolute LITERAL: the
        //    jail resolves the child's whole environment, so a path arriving through an env var
        //    would make a control vacuous.
        let npm_cli = root.join("node_modules/npm/bin/npm-cli.js");
        println!("  fact:anc-npm-cli-present={}", npm_cli.is_file());

        // ── coverage: which ancestors can this principal repair AT ALL. The ace half needs
        //    `WRITE_DAC`, so this is the ceiling on everything it could ever buy — reported as a
        //    fact, because the verdict below is a differential and must not rest on a proxy.
        let mut chain: Vec<PathBuf> = Vec::new();
        let anchors = [f.package.clone(), root.to_path_buf(), std::env::temp_dir()];
        for leafp in &anchors {
            for a in leafp.ancestors().skip(1) {
                let a = a.to_path_buf();
                if !chain.contains(&a) {
                    chain.push(a);
                }
            }
        }
        let writable = chain.iter().filter(|d| can_write_dacl(d)).count();
        println!(
            "  fact:anc-repairable-ancestors={writable}/{} [{}]",
            chain.len(),
            chain
                .iter()
                .map(|d| format!("{}={}", d.display(), u8::from(can_write_dacl(d))))
                .collect::<Vec<_>>()
                .join(" ")
        );

        std::fs::write(store.join("index.js"), b"module.exports=1;\n").expect("store index");

        // ── the NODE_OPTIONS values. THE ARMS CARRY THE REALPATH TERM ALONE, deliberately:
        //    the question is whether the ancestor repair earns its place GIVEN the realpath
        //    preload, and the stdio shim and the net gate are orthogonal to it. Stamping all
        //    three would put ~34k characters into an environment block and make every arm a
        //    measurement of the block ceiling instead of of the repair — one variable, not three.
        let realpath = realpath_term(f, exe);
        // …and the PRODUCTION stamp is still built and launched once, as its own fact, because
        // its SIZE is the thing that forced the arms to be scoped and a reader will otherwise
        // assume the arms ran what ships.
        let production = format!(
            "{} {realpath}",
            // `None` version: `anc-fixture` is a synthetic name with no published version, and
            // the second argument only selects a version-scoped egress band. Inventing a version
            // here would silently measure a band no real package is in.
            nub_sandbox::windows_build_jail_node_options(Some("anc-fixture"), None)
        )
        .trim_end()
        .to_string();
        println!(
            "  fact:anc-node-options-chars realpath_only={} production={} (a CreateProcessW \
             environment block holds 32767 characters IN TOTAL)",
            realpath.len(),
            production.len()
        );

        let preloaded = build_jail_with_env(f, exe, root, &[("NODE_OPTIONS", realpath.clone())]);
        let unpreloaded = build_jail_with_env(
            f,
            exe,
            root,
            &[("NODE_OPTIONS", "--title=nub-anc-control".to_string())],
        );
        let prod_policy =
            build_jail_with_env(f, exe, root, &[("NODE_OPTIONS", production.clone())]);
        for (label, p) in [
            ("realpath_only", &preloaded),
            ("none", &unpreloaded),
            ("production", &prod_policy),
        ] {
            println!(
                "  fact:anc-env-block-chars-{label}={}",
                p.env
                    .constructed
                    .iter()
                    .map(|(k, v)| k.chars().count() + v.chars().count() + 2)
                    .sum::<usize>()
                    + 1
            );
        }

        // THE PRODUCTION-STAMP GATE, asked of the launch rather than of the code that built the
        // string: the confined child reports all three `globalThis` sentinels itself, so a stamp
        // that never reached Node cannot read as one that did.
        let prod_sentinels = sentinel_probe(f, &prod_policy, "prod", exe, &dir);
        println!("  fact:anc-production-stamp-sentinels={prod_sentinels:?}");
        r.record(
            "anc-production-stamp-reaches-node",
            prod_sentinels.1,
            &format!(
                "(the PRODUCTION stamp's stdio shim, observed from inside the confined child: \
                 {prod_sentinels:?}; production stamp {} chars)",
                production.len()
            ),
        );

        // ── ARM 1: repair ON, preload ON. The shipping configuration for the realpath axis.
        println!("── anc arm: repair ON + preload ──");
        write_entry(&dir, "on");
        let on = node_cells(f, &preloaded, "on", exe, &dir, &npm_cli);
        println!("  fact:anc-on={}", on.detail());

        // ── ARM 2: repair OFF, preload ON. THE DECISIVE ARM, and the one nobody had run.
        println!("── anc arm: repair OFF + preload (THE DECISIVE ARM) ──");
        write_entry(&dir, "off");
        let off = without_ancestor_repair(|| node_cells(f, &preloaded, "off", exe, &dir, &npm_cli));
        println!("  fact:anc-off={}", off.detail());

        // ── ARM 3: repair OFF, NO preload. The control: without an arm where the defect still
        //    reproduces, a green decisive arm is measuring nothing.
        println!("── anc arm: repair OFF, no preload (THE CONTROL) ──");
        write_entry(&dir, "ctl");
        let control =
            without_ancestor_repair(|| node_cells(f, &unpreloaded, "ctl", exe, &dir, &npm_cli));
        println!("  fact:anc-control={}", control.detail());

        // ── ARM 4: repair OFF, preload ON, roots carrying a SINGLE spelling. THE CONTROL for
        //    the both-spellings fix: without an arm where the defect still reproduces, ARM 2
        //    coming back green would be measuring nothing. Canonical roots canonicalize to
        //    themselves, so this arm is genuinely un-expanded while ARM 2 is expanded.
        println!("── anc arm: repair OFF + preload, SINGLE-SPELLING roots (control) ──");
        let long_policy = build_jail_with_env(
            f,
            exe,
            root,
            &[("NODE_OPTIONS", realpath_term_longform(f, exe))],
        );
        write_entry(&dir, "lng");
        let long_roots =
            without_ancestor_repair(|| node_cells(f, &long_policy, "lng", exe, &dir, &npm_cli));
        println!("  fact:anc-longform={}", long_roots.detail());

        // ── DIAGNOSTIC: what npm's own entry point cannot reach. Reported as facts, never gated —
        //    it names a cause, it does not decide anything. Runs in BOTH principals, which is the
        //    whole point: the elevated arm succeeds and the de-elevated one does not, so the two
        //    traces DIFF down to the operation the ancestor repair is standing in for.
        println!("── anc diagnostic: npm entry under the refusal tracer ──");
        npm_cli_refusal_trace(f, exe, root, &dir, &npm_cli, &realpath);

        // ── the gates. No arm counts as evidence unless these pass.
        r.record(
            "anc-gate-preload-installs-the-realpath-shim",
            on.sentinels.0 && off.sentinels.0,
            &format!(
                "(repair-on {} repair-off {}; both preload arms must observe the shim FROM \
                 INSIDE the confined child)",
                on.sentinels.0, off.sentinels.0
            ),
        );
        r.record(
            "anc-gate-no-preload-leaves-the-shim-absent",
            !control.sentinels.0,
            &format!(
                "(control sentinel {}; if the shim were present here the differential would be \
                 vacuous)",
                control.sentinels.0
            ),
        );
        r.record(
            "anc-control-absolute-require-fails-unpreloaded-unrepaired",
            !control.absolute_require,
            &format!(
                "(THE CONTROL: {}; without the defect reproducing, the decisive arm proves \
                 nothing)",
                control.detail()
            ),
        );

        // ── the verdict cells.
        r.record(
            "anc-node-absolute-require-works-preloaded-with-repair-off",
            off.absolute_require,
            &off.detail(),
        );
        r.record(
            "anc-node-bare-through-junction-works-preloaded-with-repair-off",
            off.bare_through_junction,
            &off.detail(),
        );
        r.record(
            "anc-node-npm-cli-entry-works-preloaded-with-repair-off",
            off.npm_cli,
            &off.detail(),
        );
        // THE DELETION VERDICT for the Node half, stated as an EQUALITY rather than as three
        // greens: what settles whether the repair earns its place is whether removing it CHANGES
        // anything, not whether the jail happens to work with it on.
        r.record(
            "anc-repair-changes-no-node-cell-under-the-preload",
            on.outcomes() == off.outcomes(),
            &format!(
                "(repair-on {:?} vs repair-off {:?})",
                on.outcomes(),
                off.outcomes()
            ),
        );

        // THE CONTROL FOR THE FIX. A root set carrying one spelling must STILL lose cells; if it
        // did not, the both-spellings expansion would not be what changed ARM 2's outcome and the
        // verdict above would rest on nothing.
        r.record(
            "anc-control-single-spelling-roots-still-lose-a-cell",
            long_roots.outcomes() != on.outcomes(),
            &format!(
                "(repair-ON {:?} vs repair-OFF both-spellings {:?} vs repair-OFF single-spelling {:?})",
                on.outcomes(),
                off.outcomes(),
                long_roots.outcomes()
            ),
        );

        // ── the non-Node residual, which the preload cannot help by construction.
        println!("── anc non-Node tools: repair ON ──");
        let tools_on = nonnode_transcript(f, &preloaded, "ton");
        println!("── anc non-Node tools: repair OFF ──");
        let tools_off = without_ancestor_repair(|| nonnode_transcript(f, &preloaded, "tof"));
        // Compared with the arm TAG stripped, which is the only thing that legitimately differs
        // between the two transcripts — every other byte is a real outcome.
        let normalize = |s: &str, tag: &str| s.replace(tag, "T");
        let identical = normalize(&tools_on, "ton") == normalize(&tools_off, "tof");
        println!("  fact:anc-tools-on=\n{}", indent_lines(&tools_on));
        println!("  fact:anc-tools-off=\n{}", indent_lines(&tools_off));
        r.record(
            "anc-nonnode-tools-identical-with-repair-off",
            identical,
            &format!(
                "(a difference here is the ONE argument that keeps the ace half; identical \
                 {identical})"
            ),
        );
    }

    // ── does ES-MODULE resolution survive the jail? ───────────────────────────────────
    //
    // THE DEFECT. `internal/modules/esm/resolve.js` DESTRUCTURES `realpathSync` at module-load
    // time on Node 18.19-24.16, 25.x and 26.0 — before any `--import` preload exists — so the
    // realpath shim `realpath_shim_node_options` stamps never reaches it. CommonJS picks the shim
    // up (its `toRealPath` reads the property at call time); ESM does not, and every relative or
    // bare `import` in a lifecycle script dies EPERM at rc=1 with no output. 31 of 354 corpus
    // packages have an ESM lifecycle entry, so this is not a corner. Upstream's restoration
    // (nodejs/node#62835) is in 24.17+ and 26.1+ and was NOT backported to v22, the fast-tier
    // floor, so waiting does not cover us.
    //
    // WHY THIS GROUP EXISTS AND WHY IT IS HERE. Every measurement of the repair so far was taken
    // on macOS against a SIMULATED jail that patches `fs.lstatSync`/`fs.realpathSync` at the JS
    // layer. That simulation cannot model the AppContainer's NATIVE realpath refusal — the
    // kernel refusing an ancestor open is a different failure from a JS function throwing — and
    // it cannot reach a `module.register` loader thread at all. This binary already builds the
    // two principals, launches a real AppContainer, and stamps the production realpath term, so
    // it is the only place the repair can be measured rather than modelled.
    //
    // THE FIVE THINGS THE GROUP HAS TO PRODUCE, and none of them is "the treatment passed":
    //
    //  1. A POSITIVE CONTROL. In a jail with NOTHING repairing the realpath, the ESM entry must
    //     DIE. If it runs, this runner's Node is outside the affected band and every other cell
    //     here is inadmissible — a green treatment against a defect that is not reproducing
    //     measures nothing. That reading is a LOUD failure, not good news, which is why it is a
    //     gated property rather than a fact.
    //
    //     THE CONTROL IS `--preserve-symlinks-main` ALONE, and the spelling is load-bearing in
    //     both directions. With no flag at all the ENTRY POINT's own `resolveMainPath` dies first
    //     — that is the ancestor-reachability defect the sibling group owns, and it would confound
    //     this one by killing the process before the ESM resolver is reached. The main flag alone
    //     skips exactly that walk and nothing else, so what remains is the ESM resolver's own
    //     realpath and the arm isolates it. It is also the one control that CANNOT DRIFT: it names
    //     no nub function, so it keeps measuring the unrepaired defect no matter what lands inside
    //     the shim.
    //  2. The TREATMENT running the same entry to completion. The treatment is whatever
    //     `realpath_shim_node_options` is on the branch under test — the repair replaces its
    //     internals and keeps its name, so this group does not encode which seam is inside and a
    //     run's provenance (branch + SHA) is what says which implementation it measured.
    //  3. A WRONG-VERSION DISCRIMINATOR. Whatever disarms the realpath also moves dependency
    //     resolution off the store cell, and the resulting answer is a DIFFERENT version of a
    //     real package with no error at all. A silent wrong answer is worse than the loud
    //     failure it replaces, so the treatment must report `bar@2.0.0` — the private dep — and
    //     `bar@1.0.0` means the repair has become the flag and must not ship. Asked in three
    //     forms because they are three different functions: CJS `require`, ESM `import`, and
    //     `require.resolve` from inside the cell (which reaches `Module._resolveFilename`, NOT
    //     the `resolveForCJSWithHooks` a resolve hook sees).
    //  4. A REJECTED-CANDIDATE CONTROL: `--preserve-symlinks`, which clears the EPERM by stopping
    //     the resolver calling realpath at all, must answer `bar@1.0.0` — the WRONG version, and
    //     specifically the unrelated top-level one, not merely a different string. This arm is why
    //     the flag was refused, kept in the record so the refusal stays evidenced rather than
    //     remembered; it doubles as the proof that the discriminator CAN produce a wrong answer,
    //     and a fixture that cannot produce one cannot detect one.
    //  5. AN ANTI-HOLLOW GATE. Two rounds of Windows conclusions on this project were retracted
    //     because they were measured on launches reproducing none of nub's repairs. A stamped
    //     `NODE_OPTIONS` is not evidence that Node saw it, so the confined child reports its own
    //     state — see [`EsmCells::probe`] for what it reports and why an observable rather than a
    //     name is what the gate keys on.
    //
    // AND TWO SURFACES A NAIVE REPAIR REGRESSES SILENTLY. `fs.realpathSync.native` and
    // `fs.promises.realpath` do not reach the same binding entry point the resolver's own walk
    // does, so a wrapper installed on one leaves the others refused — and a lifecycle script that
    // calls either gets an EPERM the resolver cells would never have shown. They are measured
    // separately for that reason.
    //
    // NOT COVERED, deliberately: the `module.register` loader thread. It is a real worker with its
    // own realm and no `--import` preload reaches it on 18.19 through 26.5, so a repair delivered
    // that way cannot serve it. That is an accepted residual rather than a gap in this probe —
    // 0 of 354 corpus packages register an ESM loader during a lifecycle script.

    /// Every property this group reports, so the parent can require the SAME set from both
    /// principals and a silently-vanished cell reads as missing rather than as a pass.
    const ESM_PROPS: &[&str] = &[
        "esm-gate-shim-installed-in-the-shipping-arm",
        "esm-gate-unrepaired-control-leaves-the-shim-absent",
        "esm-control-unrepaired-jail-leaves-the-esm-entry-dead",
        "esm-control-unrepaired-jail-leaves-the-cjs-require-dead",
        "esm-shipping-esm-entry-completes-relative-and-bare-imports",
        "esm-shipping-cjs-require-binds-the-private-version",
        "esm-shipping-esm-import-binds-the-private-version",
        "esm-shipping-require-resolve-returns-the-store-cell-path",
        "esm-shipping-fs-realpathsync-native-survives-the-jail",
        "esm-shipping-fs-promises-realpath-survives-the-jail",
        "esm-control-preserve-symlinks-binds-the-wrong-version",
    ];

    /// The interpreter that will actually run the cells, read by RUNNING it rather than by
    /// trusting the workflow's `setup-node` pin — the arms launch the STAGED copy, and the band
    /// the defect lives in is a version fact, so a probe reporting a version it did not measure is
    /// how an inadmissible run gets read as a green one.
    /// Whether `version` is one where `esm/resolve.js` still captures `realpathSync` at module
    /// load, i.e. where the unrepaired positive control below can actually die. nodejs/node#62835
    /// restored the late property read on 24.17+ and 26.1+ and was NOT backported to v22 or v20,
    /// so the band is everything below 24.17 plus 25.x plus 26.0. Reported rather than enforced:
    /// a runner image that moves outside it makes the group inadmissible, and the point is that
    /// the log should say WHY rather than leave a reader to infer it from a control that no
    /// longer fires.
    fn esm_defect_band(version: &str) -> bool {
        let parts: Vec<u32> = version
            .trim_start_matches('v')
            .split('.')
            .filter_map(|p| p.parse().ok())
            .collect();
        match parts.as_slice() {
            [major, minor, ..] => match (*major, *minor) {
                (0..=23, _) => true,
                (24, m) => m < 17,
                (25, _) => true,
                (26, m) => m < 1,
                _ => false,
            },
            _ => false,
        }
    }

    fn interpreter_version(exe: &Path) -> String {
        std::process::Command::new(exe)
            .arg("--version")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    }

    /// One arm's readings. The verdict cells carry the CHILD'S OWN LINE rather than a boolean,
    /// because for the discriminator the interesting thing is not whether it answered but WHICH
    /// version answered — a boolean would collapse `bar@1.0.0` and a crash into the same cell.
    struct EsmCells {
        probe: EsmProbe,
        /// The ESM lifecycle entry: a relative import AND a bare import, from the store cell.
        /// Empty ⇒ it never reached its own write, which is the defect's shape.
        esm_entry: String,
        /// `require('bar').v` plus `require.resolve('bar')`, from inside the store cell.
        cjs_entry: String,
        /// The wrong-version discriminator, reached THROUGH the top-level link — CJS then ESM.
        disc_cjs: String,
        disc_esm: String,
    }

    /// What the confined child reported ABOUT ITSELF, which is the whole anti-hollow contract:
    /// every field is something the child observed from inside the jail, never something the
    /// harness inferred from an exit code or from having stamped a string.
    struct EsmProbe {
        /// The shim's own `globalThis` sentinel. Necessary but NOT sufficient — it says the
        /// preload ran, not that it repaired anything.
        shim: bool,
        /// `require.resolve` of the LINK spelling of a store-cell file: did the resolver hand
        /// back the REAL path?
        ///
        /// THIS IS THE GATE, and it is deliberately an OBSERVABLE rather than a name. Whether a
        /// repair publishes a sentinel of its own is a property of the repair, so keying the
        /// group on one would make it silently unfalsifiable the moment the internals are
        /// replaced — which is exactly what happened to this design mid-flight. Asking the
        /// resolver what it returns works against any implementation.
        resolver_real: bool,
        /// `fs.realpathSync.native` and `fs.promises.realpath`, verbatim: a path, or
        /// `threw:<code>`. Reported as strings because "which error" is the finding when they
        /// fail, and separately from each other because they are separate binding entry points.
        realpath_native: String,
        realpath_promise: String,
        version: String,
    }

    impl EsmCells {
        fn detail(&self) -> String {
            let p = &self.probe;
            format!(
                "(node {} | shim {} resolver-real {} native {:?} promise {:?} | esm {:?} cjs {:?} \
                 disc-cjs {:?} disc-esm {:?})",
                p.version,
                p.shim,
                p.resolver_real,
                p.realpath_native,
                p.realpath_promise,
                self.esm_entry,
                self.cjs_entry,
                self.disc_cjs,
                self.disc_esm
            )
        }
    }

    /// `foo`'s PRIVATE dependency is `bar@2.0.0`, wired as a junction inside its own store cell;
    /// an unrelated `bar@1.0.0` sits at the fixture's top level, which is where a link-path walk
    /// lands. That is nub's default `Isolated` layout (`aube-linker/src/lib.rs`) and the shape
    /// `preserve_symlinks_isolated_layout` measures off-Windows — reproduced here because the
    /// question this group asks is whether the answer survives the REAL jail, and that file's
    /// fixture needs none.
    ///
    /// Junctions rather than symlinks: `mklink /J` needs no privilege, so the fixture is buildable
    /// by the de-elevated arm — which is the only arm whose reading means anything.
    fn build_esm_fixture(dir: &Path) -> PathBuf {
        let store = dir.join("node_modules").join(".aube");
        for (cell, version) in [("bar@2.0.0", "2.0.0"), ("bar@1.0.0", "1.0.0")] {
            let bar = store.join(cell).join("node_modules").join("bar");
            std::fs::create_dir_all(&bar).expect("bar cell");
            std::fs::write(
                bar.join("package.json"),
                format!("{{\"name\":\"bar\",\"version\":\"{version}\",\"main\":\"index.js\"}}\n"),
            )
            .expect("bar manifest");
            std::fs::write(
                bar.join("index.js"),
                format!("module.exports={{v:'{version}'}};\n"),
            )
            .expect("bar index");
        }
        let foo_dir = store
            .join("foo@1.0.0")
            .join("node_modules")
            .join("foo")
            .to_path_buf();
        std::fs::create_dir_all(&foo_dir).expect("foo cell");
        std::fs::write(
            foo_dir.join("package.json"),
            b"{\"name\":\"foo\",\"version\":\"1.0.0\",\"main\":\"index.js\"}\n",
        )
        .expect("foo manifest");
        // No `"type"`, so `.js` is CJS and `.mjs` is ESM in the same package — which is what lets
        // one fixture answer the CJS and the ESM form of every question.
        std::fs::write(
            foo_dir.join("index.js"),
            b"module.exports={barVersion:require('bar').v};\n",
        )
        .expect("foo cjs");
        std::fs::write(
            foo_dir.join("index.mjs"),
            b"import bar from 'bar';\nexport const barVersion = bar.v;\n",
        )
        .expect("foo esm");
        std::fs::write(foo_dir.join("rel.mjs"), b"export const REL = 'ok';\n").expect("foo rel");
        std::fs::write(dir.join("package.json"), b"{\"name\":\"esm-fixture\"}\n")
            .expect("fixture manifest");

        for (link, target) in [
            (
                store.join("foo@1.0.0").join("node_modules").join("bar"),
                store.join("bar@2.0.0").join("node_modules").join("bar"),
            ),
            (dir.join("node_modules").join("foo"), foo_dir.clone()),
            (
                dir.join("node_modules").join("bar"),
                store.join("bar@1.0.0").join("node_modules").join("bar"),
            ),
        ] {
            // `raw_arg` and the output PRINTED, for the reason the sibling group records: building
            // the command line through `Command::arg` applies Rust's quoting on the way to a shell
            // that does not parse it that way, and a silently-uncreated junction fails every cell
            // INCLUDING the controls — which is the tell that separates a harness defect from a
            // finding, and is unreadable without the output.
            let out = std::process::Command::new(comspec_path())
                .raw_arg(format!(
                    "/c mklink /J \"{}\" \"{}\"",
                    link.display(),
                    target.display()
                ))
                .output();
            match &out {
                Ok(o) => println!(
                    "  fact:esm-mklink {} status={:?} err={}",
                    link.display(),
                    o.status.code(),
                    String::from_utf8_lossy(&o.stderr)
                        .trim()
                        .replace('\n', " | ")
                ),
                Err(e) => println!("  fact:esm-mklink {} spawn-failed={e}", link.display()),
            }
            println!(
                "  fact:esm-link-created {}={}",
                link.display(),
                link.is_dir()
            );
        }
        foo_dir
    }

    /// The per-arm entry points. Rewritten per arm rather than shared, because every marker path
    /// is baked in as an absolute LITERAL: the jail resolves the child's whole environment, so a
    /// marker arriving through an env var would make a control vacuous.
    fn write_esm_entries(dir: &Path, cell: &Path, tag: &str) {
        let m = |name: &str| js_literal(&dir.join(format!("{tag}.{name}")));
        // The ESM lifecycle entry, and both import forms are load-bearing: the RELATIVE one is
        // `finalizeResolution`'s realpath and the BARE one is `packageResolve`'s, which are
        // different call sites in the resolver. A static import that throws writes nothing, so an
        // empty marker is the defect's own signature and the reason the sink is read for the cause.
        std::fs::write(
            cell.join("esm.mjs"),
            format!(
                "import {{ REL }} from './rel.mjs';\n\
                 import bar from 'bar';\n\
                 import fs from 'node:fs';\n\
                 fs.writeFileSync({m}, `rel=${{REL}} bar@${{bar.v}}`);\n",
                m = m("esm")
            ),
        )
        .expect("esm entry");
        // `require.resolve` is asked HERE rather than folded into the discriminator because it
        // reaches `Module._resolveFilename` — a different function from the one a resolve hook
        // sees, and the seam a hook-only repair was measured missing.
        std::fs::write(
            cell.join("cjs.js"),
            format!(
                "const fs = require('fs');\n\
                 fs.writeFileSync({m}, 'bar@' + require('bar').v + ' resolve=' + \
                 require.resolve('bar'));\n",
                m = m("cjs")
            ),
        )
        .expect("cjs entry");
        // The discriminator entries sit at the fixture ROOT and reach `foo` through the top-level
        // junction, which is what puts a link in the walk the resolver performs. A store-cell entry
        // would find the private `bar` by the shortest path even with the realpath disarmed, so it
        // cannot produce a wrong answer — and a fixture that cannot produce one cannot detect one.
        std::fs::write(
            dir.join("disc.js"),
            format!(
                "require('fs').writeFileSync({m}, 'bar@' + require('foo').barVersion);\n",
                m = m("disc-cjs")
            ),
        )
        .expect("disc cjs");
        std::fs::write(
            dir.join("disc.mjs"),
            format!(
                "import {{ barVersion }} from 'foo/index.mjs';\n\
                 import fs from 'node:fs';\n\
                 fs.writeFileSync({m}, 'bar@' + barVersion);\n",
                m = m("disc-esm")
            ),
        )
        .expect("disc esm");
    }

    /// Whether a path the CHILD reported passes through `segment` — which is how this group asks
    /// "was the realpath applied", because the store cell a resolution lands in names the answer:
    /// `bar@2.0.0` is the private dependency, `foo@1.0.0\node_modules\bar` is the link spelling.
    ///
    /// A SUBSTRING rather than [`same_file`], and that is not laziness. `std::env::temp_dir()`
    /// hands back the 8.3 SHORT form (`C:\Users\RUNNER~1\…`) while a realpath returns the LONG
    /// one (`C:\Users\runneradmin\…`) — measured on this runner, and the reason
    /// `realpath_term_longform` exists next door. Every fixture path here descends from
    /// `temp_dir`, so a whole-path equality would compare two spellings of the same file and fail
    /// on a run where the repair worked. The store-cell segment carries no such prefix.
    fn went_through(reported: &str, segment: &str) -> bool {
        let norm = |s: &str| s.replace('/', "\\").to_ascii_lowercase();
        !reported.trim().is_empty() && norm(reported).contains(&norm(segment))
    }

    /// One line of a marker, or `""` when the cell never reached its own write.
    fn marker_line(dir: &Path, tag: &str, name: &str) -> String {
        std::fs::read_to_string(dir.join(format!("{tag}.{name}")))
            .unwrap_or_default()
            .replace(['\r', '\n'], " ")
            .trim()
            .to_string()
    }

    /// Run every cell of one arm under one policy.
    fn esm_cells(
        f: &Fixture,
        policy: &SandboxPolicy,
        tag: &str,
        exe: &Path,
        dir: &Path,
        cell: &Path,
    ) -> EsmCells {
        for name in ["esm", "cjs", "disc-cjs", "disc-esm", "probe"] {
            let _ = std::fs::remove_file(dir.join(format!("{tag}.{name}")));
        }
        write_esm_entries(dir, cell, tag);

        // `-e` for the probe cell, deliberately: it has no main module and therefore never
        // touches `resolveMainPath`, which is the only shape that can read the child's state in an
        // arm where an entry file itself would be refused.
        //
        // The write is inside the promise continuation so all five readings land in ONE file: a
        // partial marker would leave the parser assigning `threw:` strings to the wrong field,
        // which is a wrong answer rather than a missing one.
        let probe_marker = dir.join(format!("{tag}.probe"));
        let link_entry = dir.join("node_modules").join("foo").join("index.js");
        let link_dir = dir.join("node_modules").join("foo");
        confined_exec(
            f,
            policy,
            &format!("esm-{tag}-probe"),
            30,
            exe,
            &[
                "-e".into(),
                format!(
                    "const fs=require('fs');\
                     const t=(g)=>{{try{{return String(g())}}\
                     catch(e){{return 'threw:'+(e.code||e.message)}}}};\
                     const out=[String(!!globalThis.__nubJailRealpathShim),\
                     t(()=>require.resolve({entry})),t(()=>fs.realpathSync.native({d}))];\
                     fs.promises.realpath({d}).then(p=>String(p),\
                     e=>'threw:'+(e.code||e.message)).then(p=>{{out.push(p);\
                     out.push(process.version);fs.writeFileSync({m},out.join('\\n'))}})",
                    entry = js_literal(&link_entry),
                    d = js_literal(&link_dir),
                    m = js_literal(&probe_marker),
                ),
            ],
        );
        let raw = std::fs::read_to_string(&probe_marker).unwrap_or_default();
        let field = |i: usize| raw.lines().nth(i).unwrap_or_default().trim().to_string();
        let probe = EsmProbe {
            shim: field(0) == "true",
            resolver_real: went_through(&field(1), "foo@1.0.0"),
            realpath_native: field(2),
            realpath_promise: field(3),
            version: field(4),
        };
        println!("  fact:esm-{tag}-probe={raw:?}");

        for (name, entry) in [
            ("esm", cell.join("esm.mjs")),
            ("cjs", cell.join("cjs.js")),
            ("disc-cjs", dir.join("disc.js")),
            ("disc-esm", dir.join("disc.mjs")),
        ] {
            confined_exec(
                f,
                policy,
                &format!("esm-{tag}-{name}"),
                30,
                exe,
                &[entry.to_string_lossy().into_owned()],
            );
        }

        EsmCells {
            probe,
            esm_entry: marker_line(dir, tag, "esm"),
            cjs_entry: marker_line(dir, tag, "cjs"),
            disc_cjs: marker_line(dir, tag, "disc-cjs"),
            disc_esm: marker_line(dir, tag, "disc-esm"),
        }
    }

    /// THE GROUP. Three arms over one fixture, differing ONLY in `NODE_OPTIONS`.
    ///
    /// The SHIPPING arm and the CONTROL arm are not two spellings of one repair: the control
    /// names no nub function at all, so it keeps measuring the unrepaired defect however the shim
    /// is reimplemented, while the shipping arm measures whatever is on the branch. That is what
    /// makes this group survive a redesign — it already survived one.
    fn esm_resolution(r: &mut Report, f: &Fixture, exe: &Path, root: &Path) {
        let dir = f.package.join("esm");
        std::fs::create_dir_all(&dir).expect("esm fixture dir");
        let cell = build_esm_fixture(&dir);

        // THE INTERPRETER IS THE STAGED COPY AND MUST STAY THAT WAY. Substituting another one here
        // — even a correctly-versioned `setup-node` install — makes the de-elevated arm measure
        // NOTHING: `interpreter_group` stages a copy precisely because de-elevated nub can only
        // publish read to an AppContainer where it holds `WRITE_DAC`, which is `%USERPROFILE%` and
        // below, and `C:\hostedtoolcache` is not. Tried in run 30598144544: every cell in arm B
        // came back empty and the shim sentinel read false, while every sibling group passed.
        //
        // The version still has to be watched, because this group's positive control is a VERSION
        // fact — nodejs/node#62835 restored the resolver's late property read on 24.17+ and 26.1+,
        // and on such an image the unrepaired control cannot die. So the band is REPORTED rather
        // than forced, and the control gates on it honestly.
        let version = interpreter_version(exe);
        println!("  fact:esm-interpreter={version} path={}", exe.display());
        println!(
            "  fact:esm-interpreter-in-affected-band={}",
            u8::from(esm_defect_band(&version))
        );

        let roots = [f.project.clone(), f.package.clone(), exe.to_path_buf()];
        let shim = nub_sandbox::realpath_shim_node_options(&roots);
        println!(
            "  fact:esm-node-options-chars shipping={} rejected-candidate={} (a CreateProcessW \
             environment block holds 32767 characters IN TOTAL)",
            shim.len(),
            shim.len() + 21
        );

        // `--title` in the control arm rather than an empty value: the jail's env allowlist admits
        // `NODE_OPTIONS` on Windows, and a key present with an empty value is not the same launch
        // shape as one carrying a flag. Mirrors the sibling group's own control.
        let arms = [
            (
                "ctl",
                "--preserve-symlinks-main --title=nub-esm-control".to_string(),
            ),
            ("shp", shim.clone()),
            ("flg", format!("{shim} --preserve-symlinks")),
        ];
        let mut cells = Vec::new();
        for (tag, options) in &arms {
            println!(
                "── esm arm: {tag} ({options_len} chars) ──",
                options_len = options.len()
            );
            let policy = build_jail_with_env(f, exe, root, &[("NODE_OPTIONS", options.clone())]);
            let c = esm_cells(f, &policy, tag, exe, &dir, &cell);
            println!("  fact:esm-{tag}={}", c.detail());
            cells.push(c);
        }
        let [control, ship, flag] = match <[EsmCells; 3]>::try_from(cells) {
            Ok(a) => a,
            Err(_) => unreachable!("three arms in, three arms out"),
        };

        // ── the gates. No arm counts as evidence unless these pass.
        r.record(
            "esm-gate-shim-installed-in-the-shipping-arm",
            ship.probe.shim && flag.probe.shim,
            &format!(
                "(shipping {} rejected-candidate {}, observed from inside the confined child; a \
                 FALSE means the stamp never reached Node and neither arm is comparable)",
                ship.probe.shim, flag.probe.shim
            ),
        );
        // The other direction, without which the sentinel above is not a gate: an arm carrying no
        // preload must NOT observe the shim. A reading that can only ever come back true measures
        // the harness rather than the launch.
        r.record(
            "esm-gate-unrepaired-control-leaves-the-shim-absent",
            !control.probe.shim,
            &format!(
                "(control sentinel {}; if the shim were present here the differential would be \
                 vacuous)",
                control.probe.shim
            ),
        );

        // ── THE POSITIVE CONTROL. A pass here is the group's own inadmissibility, not good news:
        //    it says this runner's Node is outside the affected band, and a treatment measured
        //    against a defect that is not reproducing has measured nothing.
        let control_dead = control.esm_entry.is_empty();
        if !control_dead {
            println!(
                "  fact:esm-POSITIVE-CONTROL-DID-NOT-FIRE the ESM entry RAN in an UNREPAIRED jail \
                 on {version} ({:?}) — every shipping cell below is INADMISSIBLE, not green",
                control.esm_entry
            );
        }
        r.record(
            "esm-control-unrepaired-jail-leaves-the-esm-entry-dead",
            control_dead,
            &format!(
                "(THE POSITIVE CONTROL on {version}: {}; a PASS means the defect reproduces here \
                 and the arms below are comparable)",
                control.detail()
            ),
        );

        // ── THE CJS CONTROL, and it settles something no simulated arm can. A jail simulated by a
        //    `--require` preload installs its refusal only AFTER its own module has been resolved,
        //    by which point Node's CJS `realpathCache` already holds every ancestor — so a CJS
        //    require succeeds there with no repair whatsoever, and every simulated CJS cell is
        //    unfalsifiable. The real jail refuses from process start and has no such warm cache,
        //    so this is the one venue where "the fs-layer shim is what repairs CJS" can be
        //    evidence rather than an artifact of the harness.
        r.record(
            "esm-control-unrepaired-jail-leaves-the-cjs-require-dead",
            !control.cjs_entry.starts_with("bar@"),
            &format!(
                "(THE CJS CONTROL on {version}: {:?}; a FAIL means CJS resolved with no repair at \
                 all, which makes the shipping CJS cell below unfalsifiable rather than green)",
                control.cjs_entry
            ),
        );

        // ── the verdict cells.
        r.record(
            "esm-shipping-esm-entry-completes-relative-and-bare-imports",
            ship.esm_entry.starts_with("rel=ok"),
            &format!("({:?}; expected `rel=ok bar@2.0.0`)", ship.esm_entry),
        );
        r.record(
            "esm-shipping-cjs-require-binds-the-private-version",
            ship.cjs_entry.starts_with("bar@2.0.0"),
            &format!(
                "({:?}; `bar@1.0.0` is the SILENT wrong answer this fixture exists to catch)",
                ship.cjs_entry
            ),
        );
        r.record(
            "esm-shipping-esm-import-binds-the-private-version",
            ship.disc_esm == "bar@2.0.0" && ship.esm_entry.ends_with("bar@2.0.0"),
            &format!(
                "(through-the-link {:?}, store-cell {:?})",
                ship.disc_esm, ship.esm_entry
            ),
        );
        r.record(
            "esm-shipping-require-resolve-returns-the-store-cell-path",
            ship.cjs_entry
                .split("resolve=")
                .nth(1)
                .is_some_and(|p| went_through(p, "bar@2.0.0")),
            &format!(
                "({:?}; the LINK spelling passes through foo@1.0.0\\node_modules\\bar instead, \
                 and `Module._resolveFilename` is a different function from the one a resolve \
                 hook sees)",
                ship.cjs_entry
            ),
        );
        // The two `binding.realpath` surfaces, reported against the control so a FAIL reads as a
        // regression rather than as a jail that was never going to serve them. `went_through` and
        // not merely "did not throw": a tolerant walk that fell back to its INPUT returns the link
        // path without erroring, which is the silent-wrong-answer shape in a different costume.
        for (prop, ship_value, control_value) in [
            (
                "esm-shipping-fs-realpathsync-native-survives-the-jail",
                &ship.probe.realpath_native,
                &control.probe.realpath_native,
            ),
            (
                "esm-shipping-fs-promises-realpath-survives-the-jail",
                &ship.probe.realpath_promise,
                &control.probe.realpath_promise,
            ),
        ] {
            r.record(
                prop,
                went_through(ship_value, "foo@1.0.0"),
                &format!(
                    "(shipping {ship_value:?} vs unrepaired control {control_value:?}; these \
                     reach binding.realpath, NOT the entry point the resolver's own walk uses, so \
                     a wrapper on one leaves these refused)"
                ),
            );
        }
        // The CJS and ESM discriminators are folded into one property rather than gated
        // separately: they reach the same walk through different resolvers, so they answer the
        // same question and splitting them would inflate one finding into two.
        r.record(
            "esm-control-preserve-symlinks-binds-the-wrong-version",
            flag.disc_cjs == "bar@1.0.0",
            &format!(
                "(THE REJECTED CANDIDATE: --preserve-symlinks cjs {:?} esm {:?} vs shipping cjs \
                 {:?} esm {:?}. `bar@1.0.0` is the unrelated TOP-LEVEL copy — the specific wrong \
                 answer, not merely a different one. Without it here the discriminator could not \
                 detect one)",
                flag.disc_cjs, flag.disc_esm, ship.disc_cjs, ship.disc_esm
            ),
        );
    }

    /// The absolute entry point for one arm: it writes its own marker and then `require()`s a
    /// second file by ABSOLUTE path, so a green cell means the whole resolution walk survived
    /// rather than that the interpreter merely started.
    fn write_entry(dir: &Path, tag: &str) {
        std::fs::write(
            dir.join("dep.js"),
            format!(
                "require('fs').writeFileSync({m},'dep');module.exports=1;\n",
                m = js_literal(&dir.join(format!("{tag}.deploaded")))
            ),
        )
        .expect("dep.js");
        std::fs::write(
            dir.join("main.js"),
            format!(
                "require('fs').writeFileSync({m},'main');\nrequire({d});\n",
                m = js_literal(&dir.join(format!("{tag}.mainran"))),
                d = js_literal(&dir.join("dep.js"))
            ),
        )
        .expect("main.js");
    }

    fn indent_lines(s: &str) -> String {
        s.lines()
            .map(|l| format!("      {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Whether the launcher could install the leaf grant on the PROGRAM file — asked by launching a
    /// trivial `exit /b 0` under fail-closed and reading the launcher's OWN error, which names the
    /// path it refused. That is the mechanism itself; the cheaper stand-in for it was wrong above the
    /// medium integrity level (see the call site), which is why this pays for a launch.
    fn program_grant_landed(f: &Fixture, exe: &Path, root: &Path) -> bool {
        let policy = build_jail_for(f, exe, root);
        let spec = CommandSpec::new(&comspec_path())
            .arg("/c")
            .arg("exit /b 0")
            .cwd(&f.package);
        with_fail_closed_read_grants(|| match apply(&policy, spec) {
            Ok(prepared) => match prepared.status() {
                Ok(_) => true,
                Err(e) => {
                    println!("    [program-grant probe] {e}");
                    !e.to_string().contains("installing read grant ACE")
                }
            },
            Err(d) => {
                println!("    [program-grant probe apply Err] {d:?}");
                false
            }
        })
    }

    /// The command interpreter the arms launch, resolved the way [`lifecycle_arm`] resolves it so
    /// the grant fact is about the same file.
    fn comspec_path() -> String {
        std::env::var("ComSpec").unwrap_or_else(|_| {
            format!(
                "{}\\System32\\cmd.exe",
                std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into())
            )
        })
    }

    /// Whether THIS token may rewrite `path`'s DACL — the one permission [`set_ace`] needs, asked
    /// by opening for it rather than by writing an ace and observing the result. Non-mutating on
    /// purpose: the elevated arm WOULD succeed, and it would leave a lasting AppContainer widening
    /// on the user's own Node install.
    fn can_write_dacl(path: &Path) -> bool {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        const WRITE_DAC: u32 = 0x0004_0000;
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: an OPEN_EXISTING open for one access right on a NUL-terminated wide path; the
        // handle is closed on the success path and nothing is written through it.
        unsafe {
            let h = CreateFileW(
                wide.as_ptr(),
                WRITE_DAC,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            );
            if h == INVALID_HANDLE_VALUE || h.is_null() {
                return false;
            }
            CloseHandle(h);
            true
        }
    }

    /// Whether a path the CHILD printed names the same file as one nub built. Windows accepts both
    /// separators, so two spellings of one path are equal as paths and unequal as strings — and a
    /// string compare here silently inverted a control (see the call site).
    fn same_file(reported: &str, expected: &Path) -> bool {
        let norm = |p: &str| p.replace('/', "\\").to_ascii_lowercase();
        !reported.is_empty() && norm(reported) == norm(&expected.to_string_lossy())
    }

    /// Every property [`interpreter_group`] reports, so a machine with no Node still emits a
    /// verdict for each rather than leaving the table looking complete with rows missing.
    const INTERPRETER_PROPS: &[&str] = &[
        "interp-staging-needs-no-write-dac-anywhere-privileged",
        "interp-ambient-install-is-usable-only-where-nub-can-write-its-dacl",
        "interp-staged-copy-runs-a-lifecycle-script-without-write-dac",
        "interp-staged-child-execpath-is-the-nub-owned-copy",
        "interp-staged-abi-matches-the-source",
        "interp-staged-dir-publishes-read-to-appcontainers",
        "interp-staged-deep-entry-inherited-the-ace-at-creation",
        "interp-staged-ungranted-secret-still-refused",
        "interp-cmd-under-fail-closed-aborts-iff-its-own-program-grant-is-refused",
        "interp-cmd-under-fail-soft-reaches-step-cmd-opened-the-script",
        "interp-cmd-under-fail-soft-reaches-step-bare-node-resolved-and-ran",
        "interp-cmd-under-fail-soft-reaches-lifecycle-ok",
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
        // `node -e`, NOT `node <file>`. An ENTRY FILE goes through `resolveMainPath`, which
        // realpath's every prefix of the path starting at the volume root, and de-elevated that
        // dies `EPERM: operation not permitted, lstat 'C:\'` — the still-open
        // ancestor-reachability repair, which is orthogonal to the interpreter and was masking it
        // in BOTH arms identically (run 30544198501, rc 12 with the stack frame to prove it).
        // `-e` reaches the same reads with no main-path resolution: the run before showed
        // `node -p` unaffected while `node <file>` died, which is the documented shape.
        //
        // Reaching the distribution's BUNDLED npm tree is the exact cell an un-ACE'd install fails
        // on, and requiring a file out of it is the cheapest honest way to ask for it. Only single
        // quotes appear inside, because cmd gives the whole `-e` program to `"`.
        let identity = "console.log(process.execPath);console.log('abi='+process.versions.modules)";
        // READING THE BUNDLED npm TREE IS RECORDED, NOT GATED, and that is a scoping decision with
        // a measurement behind it. It is the cell §5j found failing, so it belongs here — but a CJS
        // `require` of an ABSOLUTE path realpath's every prefix from the volume root
        // (`Module._findPath` → `toRealPath`), which de-elevated dies `EPERM … lstat 'C:\'` in BOTH
        // interpreter arms identically (run 30544794220). That is the ancestor-reachability repair,
        // not the interpreter, and gating on it would make this group red for someone else's open
        // work while reporting nothing about its own subject. So it runs LAST, after the verdict
        // line, and its outcome is a fact.
        let npm_read = format!(
            "console.log('npm='+require({}).version)",
            js_string(&root.join("node_modules/npm/package.json"))
        );
        // Every path INSIDE the script is CWD-RELATIVE, and that is not tidiness. An absolute open
        // from inside the jail walks the whole ancestor chain as targets, and making that chain
        // reachable without elevation is a SEPARATE, still-open repair — an absolute script path
        // made the de-elevated arm fail with cmd's `Access is denied.` before either interpreter
        // was reached, i.e. it measured the ancestor blocker instead of the interpreter. A relative
        // open resolves against the inherited cwd handle and does not.
        //
        // `node` is BARE, not absolute: a PATH search is itself a directory probe, and a confined
        // child is refused on an un-ACE'd install tree — which surfaces as `'node' is not
        // recognized`, a different and earlier failure than a refused image open. Both are the
        // defect; naming the interpreter absolutely would have measured only the second.
        std::fs::write(
            &script,
            format!(
                // BREADCRUMB PER STAGE, so a failure is attributable to one of them instead of
                // arriving as a bare exit code. An earlier run produced `rc 1` with an EMPTY log
                // and nothing distinguished "cmd could not open the script" from "`node` was
                // unresolvable" from "the require was refused" — three causes with three different
                // fixes.
                "@echo off\r\n\
                 echo STEP-CMD-OPENED-THE-SCRIPT> \"{m}\"\r\n\
                 node -p \"process.version\" >> \"{m}\" 2>&1\r\n\
                 if errorlevel 1 exit /b 11\r\n\
                 echo STEP-BARE-NODE-RESOLVED-AND-RAN>> \"{m}\"\r\n\
                 node -e \"{i}\" >> \"{m}\" 2>&1\r\n\
                 if errorlevel 1 exit /b 12\r\n\
                 echo LIFECYCLE-OK>> \"{m}\"\r\n\
                 if exist \"{m}\" echo CELL-IF-EXIST>> \"{m}\"\r\n\
                 cd>> \"{m}\" 2>&1\r\n\
                 if not errorlevel 1 echo CELL-CD>> \"{m}\"\r\n\
                 dir /b>> \"{m}\" 2>&1\r\n\
                 if not errorlevel 1 echo CELL-DIR>> \"{m}\"\r\n\
                 for %%V in (1) do echo CELL-FOR>> \"{m}\"\r\n\
                 set NUBCELL=1\r\n\
                 if \"%NUBCELL%\"==\"1\" echo CELL-SET-AND-EXPAND>> \"{m}\"\r\n\
                 where.exe node>> \"{m}\" 2>&1\r\n\
                 if not errorlevel 1 echo CELL-WHERE>> \"{m}\"\r\n\
                 node -e \"{n}\" >> \"{m}\" 2>&1\r\n\
                 if errorlevel 1 echo NPM-TREE-READ-FAILED>> \"{m}\"\r\n",
                m = leaf(&marker),
                i = identity,
                n = npm_read
            ),
        )
        .expect("write the arm's script");

        let policy = build_jail_for(f, exe, root);
        let comspec = comspec_path();
        let spec = CommandSpec::new(&comspec)
            .arg("/c")
            .arg(leaf(&script))
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
        let cells: Vec<&str> = CMD_CELLS
            .iter()
            .copied()
            .filter(|c| log.contains(c))
            .collect();
        println!(
            "  fact:interp-{tag}-cmd-cells={}/{} [{}]",
            cells.len(),
            CMD_CELLS.len(),
            cells.join(" ")
        );
        println!(
            "  fact:interp-{tag}-npm-tree-read={}",
            if log.contains("npm=") { "ok" } else { "failed" }
        );
        println!("    arm:{tag} rc={rc}");
        if log.is_empty() {
            // Nothing ran, so the grant list is the only remaining evidence about why.
            for rule in &policy.fs.rules.entries {
                println!("      {tag}# grant {:?} {:?}", rule.access, rule.matcher);
            }
        }
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
        build_jail_with_env(f, exe, root, &[])
    }

    /// As [`build_jail_for`], plus `extra_env` folded into the ambient map BEFORE compilation —
    /// which is how the production `NODE_OPTIONS` stamp arrives. `build_jail.rs` writes its own
    /// value into `ambient` and the lifecycle env allowlist admits the key on Windows for
    /// exactly that reason, so stamping it here reproduces the shipping delivery rather than
    /// modelling it.
    fn build_jail_with_env(
        f: &Fixture,
        exe: &Path,
        root: &Path,
        extra_env: &[(&str, String)],
    ) -> SandboxPolicy {
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
        for (k, v) in extra_env {
            ambient.insert((*k).to_string(), v.clone());
        }
        let homes = nub_sandbox::Homes {
            home: f.home.clone(),
            tmp: std::env::temp_dir(),
            cache: f.home.join("cache"),
            project: f.project.clone(),
        };
        nub_sandbox::compile_build_jail(
            homes,
            &f.package,
            // `None` mirrors the sibling `production_jail` call: this fixture is not a curated
            // package, so it must draw no per-package exception.
            None,
            vec![exe.to_path_buf()],
            vec![root.join("node_modules")],
            ambient,
        )
        .expect("compile_build_jail")
    }

    /// A path's file name, for the cwd-relative spellings the script uses. Panics on a path with
    /// no final component, which is a probe bug rather than a measurement.
    fn leaf(p: &Path) -> String {
        p.file_name()
            .expect("an arm file has a name")
            .to_string_lossy()
            .into_owned()
    }

    /// The cmd-interpreter behaviours the script body exercises beyond "a script ran": the
    /// builtins and the PATH search a real postinstall depends on. Named so a partial result is a
    /// list rather than a count — which is the difference between "cmd is broken confined" and "one
    /// builtin is".
    const CMD_CELLS: &[&str] = &[
        "STEP-CMD-OPENED-THE-SCRIPT",
        "STEP-BARE-NODE-RESOLVED-AND-RAN",
        "LIFECYCLE-OK",
        "CELL-IF-EXIST",
        "CELL-CD",
        "CELL-DIR",
        "CELL-FOR",
        "CELL-SET-AND-EXPAND",
        "CELL-WHERE",
    ];

    /// Run `body` with the leaf-read-grant loop restored to its FAIL-CLOSED behaviour, so one arm
    /// of the same run carries the code a sibling lane measured `cmd.exe` under. Mirrors
    /// `windows_jail_repairs`'s `without_ancestor_repair`.
    /// Run `body` with BOTH halves of the backend's ancestor repair disabled. The seam can only
    /// ever REMOVE grants, so it is not a lever anything can be widened with — it exists so one
    /// run measures both directions on one fixture.
    fn without_ancestor_repair<T>(body: impl FnOnce() -> T) -> T {
        const KEY: &str = "NUB_SANDBOX_WIN_NO_ANCESTOR_REPAIR";
        // SAFETY: every launch in this group is sequential on the arm's own thread, and the
        // variable is removed before returning.
        unsafe { std::env::set_var(KEY, "1") };
        let out = body();
        unsafe { std::env::remove_var(KEY) };
        out
    }

    fn with_fail_closed_read_grants<T>(body: impl FnOnce() -> T) -> T {
        const KEY: &str = "NUB_SANDBOX_WIN_FAIL_CLOSED_READ_GRANTS";
        // SAFETY: the arm is single-threaded at this point (every launch in this group is
        // sequential), and the variable is removed before returning on both paths.
        unsafe { std::env::set_var(KEY, "1") };
        let out = body();
        unsafe { std::env::remove_var(KEY) };
        out
    }

    /// A Windows path as a SINGLE-QUOTED JS string literal, backslashes doubled.
    ///
    /// Single quotes are not a style choice: the whole `-e` program is one cmd argument delimited by
    /// `"`, so a double-quoted JS literal inside it would terminate that argument and hand cmd a
    /// path as a second token. Doubling the separators is what keeps `C:\Program Files` a path
    /// rather than a run of escape sequences.
    fn js_string(p: &Path) -> String {
        format!("'{}'", p.to_string_lossy().replace('\\', "\\\\"))
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

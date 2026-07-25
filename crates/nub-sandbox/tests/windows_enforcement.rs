//! Windows AppContainer backend — REAL enforcement probe (windows-latest CI only).
//!
//! Drives the REAL backend (`apply` → `Prepared::status` → `WindowsLaunch::run`): each
//! case compiles a policy, launches a child into the AppContainer, and asserts the
//! LowBox token allowed or denied the action. Every confinement assertion is paired
//! with a NEGATIVE CONTROL (the axis lifted → the same action succeeds) so a pass can't
//! be hollow. Mirrors the CI-validated standalone probe (`tests/sandbox-win-probes/`,
//! run 28276213658) but exercises nub's own code, not a PowerShell harness.
//!
//! `harness = false`: this binary is BOTH the test runner AND the probe child — a
//! `__sbxchild__ <role>` invocation acts as the child (read/write/connect/getenv/token/
//! spawnchild/sleep → an exit-code contract), any other invocation runs the cases. The
//! self-reexec avoids a separate compiled child, and args survive env-scrub (env does
//! not), so the child learns its role even under a scrubbed environment.
//!
//! TRAVERSAL: a LowBox token retains SeChangeNotifyPrivilege (Bypass Traverse Checking)
//! and standard NTFS volumes carry FILE_DEVICE_ALLOW_APPCONTAINER_TRAVERSAL on the volume
//! device, so intermediate-directory ACLs are NOT checked on C: — a leaf-only AC-SID grant
//! is reachable in an ORDINARY `%TEMP%` tree with NO ancestor grants (CI-proven, run
//! 29033024137). The fixture lives under `%TEMP%` with a PROTECTED DACL (inherited ACEs
//! stripped) so only the backend's explicit AC-SID grants gate access.

#[cfg(not(target_os = "windows"))]
fn main() {
    // Non-Windows host: nothing to enforce. (`harness = false` needs a `main`.)
}

#[cfg(target_os = "windows")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("__sbxchild__") {
        std::process::exit(win::child_main(&args[2..]));
    }
    match win::run_enforcement() {
        Ok(()) => println!("ALL WINDOWS ENFORCEMENT PROBES PASSED"),
        Err(n) => {
            eprintln!("{n} WINDOWS ENFORCEMENT PROBE(S) FAILED");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "windows")]
mod win {
    use nub_sandbox::policy::{
        CanonGlob, Effect, EnvPolicy, FsAccess, FsPolicy, FsRule, FsRuleSet, NetPolicy, PidPolicy,
        SandboxPolicy, TmpMode,
    };
    use nub_sandbox::{CommandSpec, apply};
    use std::collections::BTreeMap;
    use std::net::{SocketAddr, TcpStream};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    // ── the probe child ─────────────────────────────────────────────────────────

    /// The child's exit-code contract (read by the parent). Distinct codes so a denial
    /// is never confused with a crash: 0 ok, 4 env-absent, 5 access-denied, 6 timeout,
    /// 9 other-error, 10/11 token-not-as-expected.
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
            Some("getenv") => match std::env::var(&a[1]) {
                Ok(_) => 0,
                Err(_) => 4,
            },
            Some("token") => token_check(),
            Some("checkhandle") => check_handle(&a[1]),
            Some("spawnchild") => spawn_grandchild(&a[1]),
            Some("sleep") => {
                std::thread::sleep(Duration::from_secs(90));
                0
            }
            _ => 2,
        }
    }

    /// Whether the numeric handle value passed by the parent names OUR event in THIS
    /// process — i.e. was inherited. Two checks so a recycled handle-value COLLISION
    /// (Windows reuses small handle values, and the fresh child allocates its own) can't
    /// masquerade as inheritance: the value must be a live handle here
    /// (`GetHandleInformation`) AND `WaitForSingleObject(_, 0)` must return signaled —
    /// the parent creates the event manual-reset + signaled, so only our event (or a
    /// vanishingly-rare signaled-waitable collision) passes both. Exit 0 = inherited,
    /// 7 = not. NOTE: under the AppContainer's strict handle checking the invalid-handle
    /// case does not even reach the `7` return — `GetHandleInformation` on the
    /// not-inherited value RAISES STATUS_INVALID_HANDLE and the OS terminates the child;
    /// the caller treats that termination as equivalent to `7` (both = not inherited).
    fn check_handle(hex: &str) -> i32 {
        use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        let Ok(val) = usize::from_str_radix(hex.trim_start_matches("0x"), 16) else {
            return 9;
        };
        let h = val as HANDLE;
        let mut flags = 0u32;
        // SAFETY: query-only; an un-inherited value simply fails the call (no deref).
        if unsafe { GetHandleInformation(h, &mut flags) } == 0 {
            return 7;
        }
        // SAFETY: non-blocking wait; on an invalid/non-waitable value it fails, not blocks.
        if unsafe { WaitForSingleObject(h, 0) } == WAIT_OBJECT_0 {
            0
        } else {
            7
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

    /// Spawn a detached grandchild (`sleep`), record its pid, exit — so the parent can
    /// prove the Job reaped the grandchild after the direct child was gone.
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

    /// Prove the child is genuinely in a LowBox AppContainer (`TokenIsAppContainer==1`)
    /// — the anti-vacuity guard: a "denied" result is confinement, not a plain process.
    /// Exit 0 if in an AppContainer; 10 if not. Elevation is PRINTED but not gated: the
    /// GitHub windows-latest job token is elevated, so the LowBox child inherits
    /// elevation here; the "not elevated / unprivileged" sub-claim was proven by the
    /// standalone probe (run 28276213658) which de-elevated the parent. Confinement in
    /// THIS test is proven by the actual axis denials, which hold regardless of elevation.
    fn token_check() -> i32 {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{
            GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation, TokenIsAppContainer,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        // SAFETY: standard token-query sequence; buffers are exactly sized for the DWORD
        // / TOKEN_ELEVATION each class returns.
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
                "CHILD token IsAppContainer={is_ac} IsElevated={}",
                elev.TokenIsElevated
            );
            if is_ac == 1 { 0 } else { 10 }
        }
    }

    fn is_alive(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: open by pid for query only; STILL_ACTIVE (259) ⇒ alive (the sleeper
        // never exits with 259 on its own).
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

    // ── the fixture ───────────────────────────────────────────────────────────────

    struct Fixture {
        root: PathBuf,
        child: PathBuf,
        work: PathBuf,
        vault: PathBuf,
        allowed: PathBuf,
        secret: PathBuf,
    }

    /// Give the fixture root a PROTECTED DACL: strip inherited ACEs and grant only the
    /// current user full control, so a not-granted file carries no AC-SID/AAP grant and the
    /// LowBox access check denies it — the clean-DACL confined root the launcher provides in
    /// production. `icacls` is the one-shot reliable path for test setup.
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
            // An ORDINARY %TEMP% tree (NOT a special C:\ store): a LowBox token bypasses
            // traverse checking on a standard NTFS volume, so the backend's leaf-only AC-SID
            // grants are reachable here with no ancestor grants. `secure_root` gives it a
            // PROTECTED DACL so a not-granted file is denied (the LowBox check finds no
            // AC-SID/AAP grant) rather than reachable via some inherited ACE.
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!("nub-sbx-{nonce:x}"));
            // Secure the root's DACL BEFORE creating children, so each child INHERITS the
            // clean protected ACL (no stray AAP) rather than %TEMP%'s inherited ACEs.
            std::fs::create_dir_all(&root).unwrap();
            secure_root(&root);
            let bin = root.join("bin");
            let work = root.join("work");
            let vault = root.join("vault");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::create_dir_all(&work).unwrap();
            std::fs::create_dir_all(&vault).unwrap();
            // The child is a copy of THIS binary under bin/ (reachable via traverse-bypass;
            // kept out of the CI checkout dir under D:\a\… which the LowBox token can't read).
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
                vault,
                allowed,
                secret,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// The canonical-IR spelling of a real path (forward slashes — what the compiler
    /// emits and the backend re-nativizes).
    fn canon(p: &Path) -> String {
        p.to_string_lossy().replace('\\', "/")
    }

    // ── policy builders (direct IR — full control over each axis) ─────────────────

    fn read_confine(read: &[&Path], write: &[&Path]) -> SandboxPolicy {
        let mut entries = Vec::new();
        for r in read {
            entries.push(rule(r, Effect::Allow, FsAccess::Read));
        }
        for w in write {
            entries.push(rule(w, Effect::Allow, FsAccess::ReadWrite));
        }
        SandboxPolicy {
            fs: FsPolicy {
                rules: FsRuleSet {
                    entries,
                    default_effect: Effect::Deny,
                },
                tmp: TmpMode::Private,
            },
            net: NetPolicy::default(),
            env: EnvPolicy::default(),
            pid: PidPolicy::default(),
        }
    }

    fn rule(p: &Path, effect: Effect, access: FsAccess) -> FsRule {
        FsRule {
            matcher: CanonGlob(canon(p)),
            effect,
            access,
        }
    }

    fn relaxed_fs() -> FsPolicy {
        FsPolicy {
            rules: FsRuleSet {
                entries: Vec::new(),
                default_effect: Effect::Allow,
            },
            tmp: TmpMode::Shared,
        }
    }

    /// A minimal env map the AppContainer child needs to start + the caller's marker.
    fn base_env(extra: &[(&str, &str)]) -> BTreeMap<String, String> {
        // An AppContainer's CreateProcessW resolves its per-container storage from the
        // passed environment, so a TOO-minimal block fails with ERROR_ENVVAR_NOT_FOUND
        // (203): the scrubbed child needs the Windows-essential baseline (SystemRoot +
        // the USERPROFILE/LOCALAPPDATA family, …), not just PATH. (Real finding — the
        // compiler's env-scrub baseline must carry these on Windows; see the fray
        // thread.) The secret under test is deliberately absent — that is the scrub.
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
        for (k, v) in extra {
            m.insert(k.to_string(), v.to_string());
        }
        m
    }

    // ── the run helpers ───────────────────────────────────────────────────────────

    fn code(policy: &SandboxPolicy, program: &Path, args: &[&str]) -> i32 {
        let spec = CommandSpec::new(program.as_os_str()).args(args.iter().copied());
        let prepared = match apply(policy, spec) {
            Ok(p) => p,
            Err(d) => {
                eprintln!("  [apply Err] {d:?}");
                return -100;
            }
        };
        // Diagnostic (not panic): a spawn failure prints the OS error so a CI run
        // surfaces the cause and still runs the remaining cases.
        match prepared.status() {
            Ok(s) => s.code().unwrap_or(-1),
            Err(e) => {
                eprintln!("  [status Err] {e} os={:?}", e.raw_os_error());
                -101
            }
        }
    }

    /// Assert a child exit code; on mismatch, record a failure line.
    fn expect(fails: &mut u32, label: &str, got: i32, want: i32) {
        if got == want {
            println!("PASS {label} (exit {got})");
        } else {
            *fails += 1;
            eprintln!("FAIL {label}: exit {got}, expected {want}");
        }
    }
    fn expect_in(fails: &mut u32, label: &str, got: i32, want: &[i32]) {
        if want.contains(&got) {
            println!("PASS {label} (exit {got})");
        } else {
            *fails += 1;
            eprintln!("FAIL {label}: exit {got}, expected one of {want:?}");
        }
    }

    // ── handle-inheritance + env-baseline helpers ──────────────────────────────────

    /// Create an unnamed, inheritable, initially-signaled event so the parent holds a
    /// handle that WOULD inherit under a bInheritHandles=TRUE spawn absent a handle-list.
    fn create_inheritable_event() -> windows_sys::Win32::Foundation::HANDLE {
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        use windows_sys::Win32::System::Threading::CreateEventW;
        let mut sa: SECURITY_ATTRIBUTES = unsafe { std::mem::zeroed() };
        sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
        sa.bInheritHandle = 1;
        // SAFETY: unnamed manual-reset event with an inheritable SA; NULL name.
        unsafe { CreateEventW(&sa, 1, 1, std::ptr::null()) }
    }

    /// The NC for the handle-list scoping: a raw CreateProcessW with bInheritHandles=TRUE
    /// and NO attribute list — the pre-hardening "inherit ALL inheritable handles"
    /// behavior. Proves the test event IS genuinely inheritable (the child sees it), so
    /// the sandbox leg's non-inheritance is the HANDLE_LIST, not a dead handle. Args are
    /// space-free here, so a naive quoted command line suffices.
    fn spawn_inherit_all(program: &Path, args: &[&str]) -> i32 {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            CreateProcessW, GetExitCodeProcess, INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
            WaitForSingleObject,
        };
        let mut cl: Vec<u16> = Vec::new();
        cl.push(u16::from(b'"'));
        cl.extend(program.as_os_str().encode_wide());
        cl.push(u16::from(b'"'));
        for a in args {
            cl.push(u16::from(b' '));
            cl.extend(a.encode_utf16());
        }
        cl.push(0);
        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: inherit-all spawn (bInheritHandles TRUE, no attr list); cl outlives it.
        let ok = unsafe {
            CreateProcessW(
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
            return -101;
        }
        // SAFETY: wait for exit, read the code, close both handles.
        unsafe {
            WaitForSingleObject(pi.hProcess, INFINITE);
            let mut code = 0u32;
            GetExitCodeProcess(pi.hProcess, &mut code);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            code as i32
        }
    }

    /// Compile a REAL `sandbox: true` policy over the CI runner's actual ambient env, so
    /// the env axis is the compiler's own curated baseline (with the Windows-essential
    /// vars) — not the hand-rolled `base_env` list. This is the A1 fix under test.
    fn compile_surface(f: &Fixture, surface: &serde_json::Value) -> SandboxPolicy {
        let ambient: BTreeMap<String, String> = std::env::vars().collect();
        let home = std::env::var("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| f.root.clone());
        let homes = nub_sandbox::Homes {
            home: home.clone(),
            tmp: std::env::temp_dir(),
            cache: home.join("AppData").join("Local"),
            project: f.work.clone(),
        };
        let ctx = nub_sandbox::CompileCtx {
            homes,
            cwd: f.work.clone(),
            trusted: true,
            ambient_env: ambient,
            runner: Box::new(nub_sandbox::compiler::ShellRunner),
        };
        nub_sandbox::compile(surface, &ctx).expect("surface compiles")
    }

    fn compile_sandbox_true(f: &Fixture) -> SandboxPolicy {
        compile_surface(f, &serde_json::Value::Bool(true))
    }

    // ── the cases ─────────────────────────────────────────────────────────────────

    pub fn run_enforcement() -> Result<(), u32> {
        let f = Fixture::new();
        let mut fails = 0u32;
        let child = f.child.clone();
        let a = |s: &str| s.to_string();

        // In-AppContainer proof — the child reports its own token. Load-bearing: without
        // this a "denied" could be a launch failure, not confinement.
        let confine = read_confine(&[&f.work], &[]);
        expect(
            &mut fails,
            "child is genuinely in a LowBox AppContainer (TokenIsAppContainer=1)",
            code(&confine, &child, &["__sbxchild__", "token"]),
            0,
        );

        // ── fs read-confine ──────────────────────────────────────────────────────
        expect(
            &mut fails,
            "read allowed file (NC-B: child can read where granted)",
            code(
                &confine,
                &child,
                &["__sbxchild__", "read", &a(&canon_native(&f.allowed))],
            ),
            0,
        );
        expect_in(
            &mut fails,
            "read secret DENIED (KEY: default-deny vault unreachable)",
            code(
                &confine,
                &child,
                &["__sbxchild__", "read", &a(&canon_native(&f.secret))],
            ),
            &[5, 9],
        );
        // NC: relaxed fs (not sandboxing → plain spawn) reads the secret fine.
        let relaxed = SandboxPolicy {
            fs: relaxed_fs(),
            ..Default::default()
        };
        expect(
            &mut fails,
            "NC read secret under relaxed fs (readable absent confinement)",
            code(
                &relaxed,
                &child,
                &["__sbxchild__", "read", &a(&canon_native(&f.secret))],
            ),
            0,
        );

        // ── fs write-confine ─────────────────────────────────────────────────────
        let wc = read_confine(&[&f.work], &[&f.work]);
        let inside = f.work.join("w.txt");
        let outside = f.vault.join("w.txt");
        expect(
            &mut fails,
            "write inside granted dir (NC-B)",
            code(
                &wc,
                &child,
                &["__sbxchild__", "write", &a(&canon_native(&inside))],
            ),
            0,
        );
        expect(
            &mut fails,
            "write outside DENIED",
            code(
                &wc,
                &child,
                &["__sbxchild__", "write", &a(&canon_native(&outside))],
            ),
            5,
        );

        // ── coarse egress ────────────────────────────────────────────────────────
        // Both legs run UNDER the AppContainer (fs read-confine engages it); only the
        // internetClient capability differs, isolating it as the cause.
        let mut net_deny = read_confine(&[&f.work], &[]);
        net_deny.net = NetPolicy {
            enforce: true,
            rules: Vec::new(),
            default_effect: Effect::Deny,
            ..Default::default()
        };
        expect_in(
            &mut fails,
            "egress DENIED without internetClient (WSAEACCES/timeout)",
            code(
                &net_deny,
                &child,
                &["__sbxchild__", "connect", "1.1.1.1", "443"],
            ),
            &[5, 6],
        );
        expect(
            &mut fails,
            "NC egress ALLOWED with internetClient (net unconfined)",
            code(
                &confine,
                &child,
                &["__sbxchild__", "connect", "1.1.1.1", "443"],
            ),
            0,
        );

        // ── loopback egress denied (local-exfil closed; per-host needs an exemption) ─
        // A LOOPBACK service (docker.sock-class local exfil, or a would-be egress
        // proxy) is unreachable from inside the AppContainer by default: Windows blocks
        // AppContainer loopback absent a registered exemption (`NetworkIsolation…`). This
        // is the narrowed-endpoint property on Windows — local-IPC exfil is closed — AND
        // it is precisely WHY per-host via the loopback proxy is honestly DEGRADED here
        // (the confined child cannot reach the proxy without the exemption, not wired in
        // this phase). We prove the block empirically; the NC is a fully-relaxed (NON-
        // AppContainer) spawn that reaches the same loopback service.
        let loopback = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let lport = loopback.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in loopback.incoming() {
                drop(c);
            }
        });
        expect_in(
            &mut fails,
            "AppContainer child DENIED loopback (local-exfil closed)",
            code(
                &net_deny,
                &child,
                &["__sbxchild__", "connect", "127.0.0.1", &lport.to_string()],
            ),
            &[5, 6, 9],
        );
        let relaxed = SandboxPolicy {
            fs: relaxed_fs(),
            ..Default::default()
        };
        expect(
            &mut fails,
            "NC fully-relaxed (no AppContainer) reaches the loopback service",
            code(
                &relaxed,
                &child,
                &["__sbxchild__", "connect", "127.0.0.1", &lport.to_string()],
            ),
            0,
        );

        // ── env-scrub ────────────────────────────────────────────────────────────
        // SAFETY: single-threaded test main; set the ambient secret the child would
        // inherit absent enforcement.
        unsafe { std::env::set_var("NUB_SBX_SECRET", "sk-leak") };
        let mut scrub = read_confine(&[&f.work], &[]);
        scrub.env = EnvPolicy {
            enforce: true,
            constructed: base_env(&[("NUB_SBX_ALLOWED", "yes")]),
            schema: Vec::new(),
            withheld: vec!["NUB_SBX_SECRET".to_string()],
        };
        // env-read-ascendant is OS-CLOSED (the AppContainer denies the parent
        // OpenProcess(PROCESS_VM_READ), run 29043151805) — apply() must NOT report it LOST
        // for an env-withholding policy, else a frontend would think Windows is degraded when
        // it isn't. Lock in the corrected state so it can't silently regress.
        {
            let spec_args: &[&str] = &["__sbxchild__", "token"];
            let spec = CommandSpec::new(child.as_os_str()).args(spec_args.iter().copied());
            let prepared = apply(&scrub, spec).expect("apply env-withholding scrub policy");
            if prepared
                .degradation
                .lost
                .iter()
                .any(|s| s.as_str() == "env-read-ascendant")
            {
                fails += 1;
                eprintln!(
                    "FAIL env-read-ascendant: apply() still reports it LOST (OS-closed, must not degrade)"
                );
            } else {
                println!("PASS env-read-ascendant NOT reported by apply() (OS-closed)");
            }
        }
        expect(
            &mut fails,
            "scrubbed child does NOT see the secret (absent)",
            code(
                &scrub,
                &child,
                &["__sbxchild__", "getenv", "NUB_SBX_SECRET"],
            ),
            4,
        );
        expect(
            &mut fails,
            "scrubbed child DOES see the allowlisted var (present)",
            code(
                &scrub,
                &child,
                &["__sbxchild__", "getenv", "NUB_SBX_ALLOWED"],
            ),
            0,
        );
        // NC: env not enforced → child inherits the parent env → sees the secret.
        expect(
            &mut fails,
            "NC env not enforced → secret visible (inherited)",
            code(
                &confine,
                &child,
                &["__sbxchild__", "getenv", "NUB_SBX_SECRET"],
            ),
            0,
        );

        // ── env baseline (A1): the compiler's `sandbox: true` curated baseline must
        //    carry the Windows-essential vars so CreateProcessW succeeds (no
        //    ERROR_ENVVAR_NOT_FOUND) and a normal exe runs. Compiled over the runner's
        //    REAL ambient env — replaces the hand-rolled base_env workaround. ─────────
        // SAFETY: single-threaded test main; seed an ambient secret the scrub must drop.
        unsafe { std::env::set_var("NUB_SBX_A1_SECRET", "sk-leak") };
        let true_policy = compile_sandbox_true(&f);
        expect(
            &mut fails,
            "sandbox:true child STARTS under compiler baseline (CreateProcessW ok)",
            code(&true_policy, &child, &["__sbxchild__", "token"]),
            0,
        );
        expect(
            &mut fails,
            "sandbox:true baseline carries SystemRoot (essential var present)",
            code(
                &true_policy,
                &child,
                &["__sbxchild__", "getenv", "SystemRoot"],
            ),
            0,
        );
        expect(
            &mut fails,
            "sandbox:true baseline carries USERPROFILE (essential var present)",
            code(
                &true_policy,
                &child,
                &["__sbxchild__", "getenv", "USERPROFILE"],
            ),
            0,
        );
        expect(
            &mut fails,
            "sandbox:true scrubs the ambient secret",
            code(
                &true_policy,
                &child,
                &["__sbxchild__", "getenv", "NUB_SBX_A1_SECRET"],
            ),
            4,
        );

        // ── env strip-all FLOOR (Q3): a complete-statement `{ fs: [...] }` FLOORS the
        //    unlisted env axis to strip-all. The floor must still inject the minimal
        //    OS-startup essentials so the child STARTS reliably (not just where the OS
        //    tolerates an empty block), while every ambient user var / secret is
        //    withheld. Compiled over the runner's REAL ambient env. ───────────────────
        // SAFETY: single-threaded test main; seed an ambient var the floor must withhold.
        unsafe { std::env::set_var("NUB_SBX_Q3_USERVAR", "must-not-leak") };
        let fs_only = serde_json::json!({ "fs": [f.work.to_string_lossy()] });
        let floored = compile_surface(&f, &fs_only);
        assert!(
            floored.env.enforce && !floored.env.constructed.contains_key("NUB_SBX_Q3_USERVAR"),
            "env axis floored to strip-all, user var not constructed"
        );
        expect(
            &mut fails,
            "floored-env child STARTS (essentials injected, CreateProcessW ok)",
            code(&floored, &child, &["__sbxchild__", "token"]),
            0,
        );
        expect(
            &mut fails,
            "floored-env child sees the OS-startup essential SystemRoot",
            code(&floored, &child, &["__sbxchild__", "getenv", "SystemRoot"]),
            0,
        );
        expect(
            &mut fails,
            "floored-env child sees the AppContainer essential LOCALAPPDATA",
            code(
                &floored,
                &child,
                &["__sbxchild__", "getenv", "LOCALAPPDATA"],
            ),
            0,
        );
        expect(
            &mut fails,
            "floored-env child does NOT see an ambient user var (withheld)",
            code(
                &floored,
                &child,
                &["__sbxchild__", "getenv", "NUB_SBX_Q3_USERVAR"],
            ),
            4,
        );

        // ── inherited-handle scoping (B1): the sandboxed child inherits ONLY its stdio
        //    (PROC_THREAD_ATTRIBUTE_HANDLE_LIST), NOT an arbitrary inheritable handle nub
        //    holds. NC: a raw inherit-ALL spawn DOES pass the same handle, proving it's
        //    genuinely inheritable — so the sandbox leg's non-inheritance is the scoping. ─
        // A not-inherited handle manifests two ways, BOTH proving non-inheritance: the
        // child returns 7 (GetHandleInformation reports the value invalid) OR — observed
        // on windows-latest — the AppContainer's strict handle checking TERMINATES the
        // child with STATUS_INVALID_HANDLE for touching an invalid handle. The NC
        // (inherited) leg reaches the handle and returns 0.
        const STATUS_INVALID_HANDLE: i32 = 0xC000_0008u32 as i32;
        let event = create_inheritable_event();
        if event.is_null() {
            fails += 1;
            eprintln!("FAIL handle-scoping setup: CreateEventW failed");
        } else {
            let harg = format!("0x{:x}", event as usize);
            expect_in(
                &mut fails,
                "sandboxed child does NOT inherit nub's extra handle (scoped to stdio)",
                code(&confine, &child, &["__sbxchild__", "checkhandle", &harg]),
                &[7, STATUS_INVALID_HANDLE],
            );
            expect(
                &mut fails,
                "NC inherit-all spawn DOES pass the handle (proves it's inheritable)",
                spawn_inherit_all(&child, &["__sbxchild__", "checkhandle", &harg]),
                0,
            );
            // SAFETY: close the test event.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(event) };
        }

        // ── process-reap (Job Object KILL_ON_JOB_CLOSE) ──────────────────────────
        job_reap(&mut fails, &f);

        if fails == 0 { Ok(()) } else { Err(fails) }
    }

    /// The sandboxed run reaps the grandchild when `status()` closes the Job handle; the
    /// plain (no-Job) spawn leaves it alive — the difference IS the reap.
    fn job_reap(fails: &mut u32, f: &Fixture) {
        let marker = f.work.join("gc.pid");
        let wc = read_confine(&[&f.work], &[&f.work]);
        let rc = code(
            &wc,
            &f.child,
            &["__sbxchild__", "spawnchild", &canon_native(&marker)],
        );
        if rc != 0 {
            *fails += 1;
            eprintln!("FAIL job-reap setup: spawnchild exit {rc}");
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
        let gc_pid: u32 = std::fs::read_to_string(&marker)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        if gc_pid == 0 {
            *fails += 1;
            eprintln!("FAIL job-reap: grandchild pid not recorded");
            return;
        }
        if is_alive(gc_pid) {
            *fails += 1;
            eprintln!("FAIL job-reap: grandchild {gc_pid} still alive after Job close");
        } else {
            println!("PASS job-reap: grandchild reaped on Job close");
        }

        // NC: a plain (unsandboxed, no Job) spawn of the same scenario leaves the
        // grandchild alive after the direct child exits — proving the reap is the Job's.
        let marker2 = f.work.join("gc2.pid");
        let out = std::process::Command::new(&f.child)
            .args(["__sbxchild__", "spawnchild", &canon_native(&marker2)])
            .status();
        if out.map(|s| s.success()).unwrap_or(false) {
            std::thread::sleep(Duration::from_millis(500));
            if let Ok(pid) = std::fs::read_to_string(&marker2).and_then(|s| {
                s.trim()
                    .parse::<u32>()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            }) {
                if is_alive(pid) {
                    println!("PASS job-reap NC: unsandboxed grandchild outlives parent");
                    kill(pid);
                } else {
                    *fails += 1;
                    eprintln!("FAIL job-reap NC: grandchild not alive (control broken)");
                }
            }
        }
    }

    fn kill(pid: u32) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_TERMINATE, TerminateProcess,
        };
        // SAFETY: best-effort cleanup of the leftover NC grandchild.
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !h.is_null() {
                TerminateProcess(h, 1);
                CloseHandle(h);
            }
        }
    }

    /// A real (backslash) path as a string for passing to the child as an arg.
    fn canon_native(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }
}

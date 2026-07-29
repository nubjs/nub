//! Windows: why `require()` of an absolute path dies in the jail, and which route out of it
//! survives the zero-privilege constraint.
//!
//! THE DEFECT. With the interpreter reachable, a confined `node` starts and dies in the CJS
//! loader:
//!
//! ```text
//! Error: EPERM: operation not permitted, lstat 'C:\'
//!     at Object.realpathSync (node:fs:2749:25)
//!     at toRealPath (node:internal/modules/helpers:61:13)
//!     at Function._findPath (node:internal/modules/cjs/loader:747:24)
//! ```
//!
//! Node's JS `realpathSync` walks a path component by component, and on Windows it `lstat`s
//! the VOLUME ROOT first. The backend grants leaf-only and leans on traverse-bypass
//! (`SeChangeNotifyPrivilege` + `FILE_DEVICE_ALLOW_APPCONTAINER_TRAVERSAL`), which exempts
//! INTERMEDIATE components of one open — it does not make an ancestor openable as a TARGET.
//! `C:\` as a target is access-checked, the LowBox check finds no AppContainer/capability ACE
//! on it, and the open is refused.
//!
//! THE QUESTION THAT DECIDES THE FIX. The obvious repair is an ancestor ACE. That is only
//! shippable if a standard, non-elevated user can write it, because a jail that needs admin
//! cannot be default-on. `writedac_ability` measures exactly that, de-elevated, against the
//! real roots — and pairs it with a positive control on a directory the same de-elevated
//! context OWNS, so a refusal on `C:\` is evidence about `C:\` rather than about a broken
//! impersonation.
//!
//! THE ROUTE OUT. Node reaches its JS `realpathSync` through two gated seams:
//!
//! | seam | gate |
//! | --- | --- |
//! | `resolveMainPath` (`run_main.js`) | `--preserve-symlinks-main` |
//! | `_findPath` / `finalizeResolution` (CJS + ESM) | `--preserve-symlinks` |
//!
//! The first candidate was to keep resolution semantics intact by redirecting
//! `fs.realpathSync` at its NATIVE twin through a `data:` preload — Node keeps that seam
//! monkey-patchable on purpose (`internal/modules/helpers.js`: "Import all of `fs` so that it
//! can be monkey-patched"). It is REFUTED: `fs.realpathSync.native` is refused under this jail
//! too, `EPERM ... realpath` on a file the jail GRANTED and Node reads successfully in the
//! same run. `GetFinalPathNameByHandleW` needs more than the leaf handle the jail allows, so
//! both realpath implementations are unavailable and only NOT CALLING one is left — which is
//! `--preserve-symlinks`. That is measured here and it WORKS, but it is NOT shipped: under
//! nub's default `Isolated` linker it silently binds the wrong package version rather than
//! failing (`preserve_symlinks_isolated_layout`), which is worse than the loud failure it
//! replaces. So the `lifecycle` arm stamps NODE_OPTIONS itself, as a hypothetical the product
//! does not carry.
//!
//! That refuted arm is KEPT as a standing differential rather than deleted: `node-shim-executed`
//! proves the preload evaluated, so `realpath-native-refused-in-jail` is the OS refusing, not
//! the preload failing to arrive. If a future Windows grants that call under an AppContainer,
//! the arm flips and the semantics-preserving route reopens.
//!
//! `node_matrix` measures each piece on its own, so the report can say what each flag buys
//! rather than that some combination worked.
//!
//! EVERY VERDICT IS A MARKER THE CHILD WROTE. Nothing is read off a status the harness
//! reported about itself, every path the child touches is baked in as an absolute LITERAL
//! (the jail strips the environment, and a `readFileSync(undefined)` throws in a way that
//! reads as a refusal), and `psymlinks-ungranted-read-refused` asserts the SPECIFIC error — a
//! canary that came back `ENOENT` would mean the control never tested confinement at all.
//!
//! BLOCKER 2 IS DELIBERATELY NOT MEASURED HERE. A piped `child_process` spawn under this jail
//! does not fail — it BLOCKS INDEFINITELY, and Node's own `timeout` option cannot break it
//! because the block is in libuv's named-pipe setup, before the timer arms. Reproduced twice
//! (runs 30460192608 and 30461823852): both markers end at exactly that call and the harness
//! had to kill the process, taking every later arm with it. That IS the finding, and it is a
//! worse failure mode for a postinstall than a clean refusal would be.
//!
//! CI IS THE ONLY VENUE. AppContainer cannot be launched over SSH (session 0 has no window
//! station; every launch returns 0xC0000142). Runs branch-scoped via
//! `.github/workflows/win-realpath-ancestors-probe.yml`, no pull request.

#[cfg(not(target_os = "windows"))]
fn main() {
    // Non-Windows host: nothing to measure. (`harness = false` needs a `main`.)
}

#[cfg(target_os = "windows")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("__sbxchild__") => std::process::exit(win::child_main(&args[2..])),
        _ => std::process::exit(win::probe_main()),
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
    use std::path::{Path, PathBuf};

    // ─────────────────────────── child modes (run INSIDE the jail) ───────────────────────

    pub fn child_main(a: &[String]) -> i32 {
        match a.first().map(String::as_str) {
            // lstatchain <marker> <path…> — one `lstat` per path, each recorded whatever it
            // does. This is the mechanism measured WITHOUT Node in the way: it answers which
            // ancestors a LowBox token can stat at all, and it still reports when `node`
            // itself cannot start.
            Some("lstatchain") => {
                let marker = Path::new(&a[1]);
                let mut out = String::new();
                for p in &a[2..] {
                    match std::fs::symlink_metadata(p) {
                        Ok(m) => out.push_str(&format!("{p} = ok dir={}\n", m.is_dir())),
                        Err(e) => out.push_str(&format!(
                            "{p} = err {:?} raw={:?}\n",
                            e.kind(),
                            e.raw_os_error()
                        )),
                    }
                }
                let _ = std::fs::write(marker, out);
                0
            }
            // noderun <log> <node.exe> <script> [flags…] — spawn the interpreter on an
            // absolute script. Flags precede the script in node's argv.
            //
            // STDIO IS FILES, deliberately: this jail refuses BOTH the `NUL` device and any
            // NPFS named pipe, so `Stdio::null()` and `Stdio::piped()` would each fail for a
            // reason that has nothing to do with what is being measured. stdin stays
            // inherited for the same reason.
            Some("noderun") => {
                let log = Path::new(&a[1]);
                let node = &a[2];
                let script = &a[3];
                let flags = &a[4..];
                let sink = match std::fs::File::create(log) {
                    Ok(file) => file,
                    Err(e) => {
                        eprintln!("noderun: cannot create log: {e}");
                        return 9;
                    }
                };
                let errsink = match sink.try_clone() {
                    Ok(file) => file,
                    Err(e) => {
                        eprintln!("noderun: cannot clone log: {e}");
                        return 9;
                    }
                };
                let status = std::process::Command::new(node)
                    .args(flags)
                    .arg(script)
                    .stdout(std::process::Stdio::from(sink))
                    .stderr(std::process::Stdio::from(errsink))
                    .status();
                match status {
                    Ok(s) => s.code().unwrap_or(-1),
                    Err(e) => {
                        let _ = std::fs::write(
                            log,
                            format!("spawnerr {e:?} raw={:?}", e.raw_os_error()),
                        );
                        5
                    }
                }
            }
            _ => 2,
        }
    }

    // ─────────────────────────── fixture ────────────────────────────────────────────────

    struct Fixture {
        root: PathBuf,
        child: PathBuf,
        work: PathBuf,
        /// A real file OUTSIDE every grant, used as the confinement canary. It EXISTS, so a
        /// refusal to read it is a permission verdict rather than an absence.
        ungranted: PathBuf,
    }

    /// PROTECTED DACL on the fixture root: inherited ACEs stripped, only the current user
    /// granted. `C:\Users` carries an inheritable `ALL APPLICATION PACKAGES` grant, so without
    /// this every arm under `%TEMP%` would be measuring that inherited ACE rather than the
    /// backend's own default-deny.
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
        /// Keyed by PID as well as a clock nonce: a store that served a previously-built
        /// artifact, or a stale directory from an earlier run, must never be able to satisfy
        /// an arm whose script never ran.
        fn new(tag: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("nub-rpa-{tag}-{}-{nonce:x}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            secure_root(&root);
            let bin = root.join("bin");
            let work = root.join("work");
            let outside = root.join("outside");
            for d in [&bin, &work, &outside] {
                std::fs::create_dir_all(d).unwrap();
            }
            let ungranted = outside.join("secret.txt");
            std::fs::write(&ungranted, "canary-must-not-be-readable").unwrap();
            let child = bin.join("child.exe");
            std::fs::copy(std::env::current_exe().unwrap(), &child).unwrap();
            Fixture {
                root,
                child,
                work,
                ungranted,
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

    fn read_rule(p: &Path) -> FsRule {
        FsRule {
            matcher: CanonGlob(canon(p)),
            effect: Effect::Allow,
            access: FsAccess::Read,
            origin: FsOrigin::Authored,
        }
    }

    fn os_essential_env() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        for k in [
            "SystemRoot",
            "SystemDrive",
            "windir",
            "TEMP",
            "TMP",
            "LOCALAPPDATA",
            "USERPROFILE",
            "PATH",
            "PATHEXT",
            "NUMBER_OF_PROCESSORS",
            "PROCESSOR_ARCHITECTURE",
            "COMPUTERNAME",
            "USERNAME",
            "ALLUSERSPROFILE",
            "ProgramData",
            "ProgramFiles",
            "CommonProgramFiles",
        ] {
            if let Ok(v) = std::env::var(k) {
                m.insert(k.to_string(), v);
            }
        }
        m
    }

    /// A build-jail-SHAPED policy: pure default-deny read allowlist, own-dir write, coarse
    /// egress deny. `extra_env` is how the production `NODE_OPTIONS` delivery is measured —
    /// the jail resolves the child's whole environment, so a knob nub would set in production
    /// arrives through exactly this map.
    fn jail_shaped(f: &Fixture, extra: Vec<FsRule>, extra_env: &[(&str, String)]) -> SandboxPolicy {
        let mut entries = vec![
            FsRule {
                matcher: CanonGlob(canon(&f.work)),
                effect: Effect::Allow,
                access: FsAccess::ReadWrite,
                origin: FsOrigin::Authored,
            },
            read_rule(&f.child),
        ];
        entries.extend(extra);
        let mut env = os_essential_env();
        for (k, v) in extra_env {
            env.insert((*k).to_string(), v.clone());
        }
        SandboxPolicy {
            fs: FsPolicy {
                rules: FsRuleSet {
                    entries,
                    default_effect: Effect::Deny,
                },
                tmp: TmpMode::Private,
            },
            net: NetPolicy {
                enforce: true,
                rules: Vec::new(),
                default_effect: Effect::Deny,
                ..Default::default()
            },
            // `enforce` MUST be set, not merely `resolved`: the Windows backend hands the
            // child the constructed map only when the env axis enforces, and otherwise lets it
            // inherit the parent's environment. With `EnvPolicy::resolved` alone the
            // NODE_OPTIONS arm silently measured the PARENT's env and died at resolveMainPath.
            env: EnvPolicy {
                resolved: true,
                enforce: true,
                constructed: env,
                ..Default::default()
            },
            pid: PidPolicy::default(),
            build_jail: true,
        }
    }

    fn spec(f: &Fixture, cwd: &Path, args: &[String]) -> CommandSpec {
        let mut a = vec!["__sbxchild__".to_string()];
        a.extend(args.iter().cloned());
        CommandSpec::new(f.child.as_os_str()).args(a).cwd(cwd)
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

    fn path_node() -> Option<PathBuf> {
        if let Ok(explicit) = std::env::var("PROBE_NODE_EXE") {
            let p = PathBuf::from(explicit);
            return p.is_file().then_some(p);
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|d| d.join("node.exe"))
            .find(|c| c.is_file())
    }

    fn unverbatim(p: &Path) -> PathBuf {
        match p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
            Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
            _ => p.to_path_buf(),
        }
    }

    fn run_child_in(
        f: &Fixture,
        policy: &SandboxPolicy,
        cwd: &Path,
        mode: &str,
        argv: &[String],
    ) -> i32 {
        let mut args = vec![mode.to_string()];
        args.extend(argv.iter().cloned());
        let outcome = apply(policy, spec(f, cwd, &args)).map(|p| p.status());
        match outcome {
            Ok(Ok(status)) => status.code().unwrap_or(-1),
            Ok(Err(error)) => {
                println!("    launch failed: {error}");
                -2
            }
            Err(degradation) => {
                println!("    policy rejected: {degradation:?}");
                -3
            }
        }
    }

    // ─────────────────────────── de-elevation (parent side) ─────────────────────────────

    /// Whether this context actually WIELDS administrative authority, decided by an ACCESS
    /// CHECK rather than by a token flag. `TokenIsElevated` is not a sound oracle here:
    /// `CreateRestrictedToken` COPIES it instead of recomputing it, so a token with
    /// `BUILTIN\Administrators` reduced to deny-only and every privilege stripped still
    /// reports elevated (measured by the sibling de-elevated harness,
    /// `windows_deelevated_jail.rs`, run 30423750288). `SC_MANAGER_CREATE_SERVICE` is granted
    /// to Administrators only, a deny-only SID does not match it, and the call mutates
    /// nothing.
    fn admin_authority() -> bool {
        use windows_sys::Win32::System::Services::{
            CloseServiceHandle, OpenSCManagerW, SC_MANAGER_CREATE_SERVICE,
        };
        // SAFETY: opens the local SCM for a right never exercised; NULL machine/database.
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

    /// Whether `token` carries `BUILTIN\Administrators` as an ENABLED group, evaluated against
    /// the token handle directly.
    ///
    /// This is the de-elevated arm's oracle, and it is deliberately NOT the SCM probe above:
    /// `OpenSCManagerW` is an RPC, and issuing one while impersonating a restricted token is
    /// the likeliest way for this probe to stall indefinitely — which is a bad trade when a
    /// purely local check answers the same question. It is also not the `TokenIsElevated` flag
    /// that `CreateRestrictedToken` copies rather than recomputes: `CheckTokenMembership`
    /// evaluates the ACTUAL group state and, by contract, reports a DENY-ONLY SID as NOT a
    /// member — which is exactly the reduction the restricted token applies.
    fn has_admin_group(token: windows_sys::Win32::Foundation::HANDLE) -> Option<bool> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
        use windows_sys::Win32::Security::{CheckTokenMembership, PSID};
        let text: Vec<u16> = "S-1-5-32-544".encode_utf16().chain([0]).collect();
        let mut sid: PSID = std::ptr::null_mut();
        // SAFETY: converts a well-formed SDDL SID string; freed below.
        if unsafe { ConvertStringSidToSidW(text.as_ptr(), &mut sid) } == 0 {
            return None;
        }
        let mut is_member: i32 = 0;
        // SAFETY: `token` is an impersonation token owned by the caller; `sid` outlives it.
        let ok = unsafe { CheckTokenMembership(token, sid, &mut is_member) };
        // SAFETY: `sid` came from ConvertStringSidToSidW.
        unsafe { LocalFree(sid.cast()) };
        (ok != 0).then_some(is_member != 0)
    }

    /// An IMPERSONATION token for the same user with administrative authority removed and the
    /// integrity level dropped to medium.
    ///
    /// Impersonation rather than `CreateProcessAsUserW` (which the sibling harness needs, for
    /// an observable child exit code) because every question here is a single access check on
    /// a file object, and a file open uses the calling THREAD's token. Both halves matter:
    /// the deny-only Administrators SID removes DACL authority, and medium integrity is what
    /// makes `C:\`'s `High Mandatory Level:(NW)` label bite — a high-IL standard-user token
    /// would be a strictly weaker measurement than a real standard user.
    ///
    /// `DISABLE_MAX_PRIVILEGE` keeps only `SeChangeNotifyPrivilege`, which is exactly the
    /// traverse-bypass the backend's leaf-only grants already depend on.
    fn deelevated_impersonation_token() -> std::io::Result<windows_sys::Win32::Foundation::HANDLE> {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
        use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
        use windows_sys::Win32::Security::{
            CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, DuplicateTokenEx, PSID,
            SID_AND_ATTRIBUTES, SecurityImpersonation, TOKEN_ALL_ACCESS, TOKEN_DUPLICATE,
            TOKEN_QUERY, TokenImpersonation,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        let mut me: HANDLE = std::ptr::null_mut();
        // SAFETY: opens our own process token with exactly the rights used below.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut me) }
            == 0
        {
            return Err(std::io::Error::last_os_error());
        }

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
        // SAFETY: `disable` outlives the call.
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

        // The restricted handle carries only the source handle's rights, and lowering the
        // integrity level needs TOKEN_ADJUST_DEFAULT — so duplicate to an impersonation token
        // at full access rather than widening the process-token handle above.
        let mut imp: HANDLE = std::ptr::null_mut();
        // SAFETY: duplicating a token handle we own into an impersonation token.
        let dup = unsafe {
            DuplicateTokenEx(
                restricted,
                TOKEN_ALL_ACCESS,
                std::ptr::null(),
                SecurityImpersonation,
                TokenImpersonation,
                &mut imp,
            )
        };
        let err = std::io::Error::last_os_error();
        unsafe { CloseHandle(restricted) };
        if dup == 0 {
            return Err(err);
        }
        set_medium_integrity(imp)?;
        Ok(imp)
    }

    /// `CreateRestrictedToken` leaves the integrity level UNTOUCHED. Lowering one never needs
    /// a privilege (only raising one does).
    fn set_medium_integrity(token: windows_sys::Win32::Foundation::HANDLE) -> std::io::Result<()> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
        use windows_sys::Win32::Security::{
            GetLengthSid, PSID, SID_AND_ATTRIBUTES, SetTokenInformation, TOKEN_MANDATORY_LABEL,
            TokenIntegrityLevel,
        };
        // windows-sys files this under System_SystemServices; spelled out rather than pulling
        // a whole feature in for one integer.
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
                std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + GetLengthSid(sid),
            )
        };
        let err = std::io::Error::last_os_error();
        unsafe { LocalFree(sid.cast()) };
        if ok == 0 { Err(err) } else { Ok(()) }
    }

    /// Run `body` while impersonating `token`, reverting unconditionally afterwards.
    fn impersonating<T>(
        token: windows_sys::Win32::Foundation::HANDLE,
        body: impl FnOnce() -> T,
    ) -> std::io::Result<T> {
        use windows_sys::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
        // SAFETY: `token` is a valid impersonation token owned by the caller.
        if unsafe { ImpersonateLoggedOnUser(token) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let out = body();
        // SAFETY: paired with the successful impersonation above.
        unsafe { RevertToSelf() };
        Ok(out)
    }

    /// Can this context rewrite `path`'s DACL? Asked by OPENING for `WRITE_DAC` rather than by
    /// writing one: the answer is the same access check `SetNamedSecurityInfoW` performs, and
    /// it leaves the DACL of a system root untouched whichever way it comes out.
    fn can_write_dacl(path: &Path) -> Result<(), u32> {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        const WRITE_DAC: u32 = 0x0004_0000;
        let wide: Vec<u16> = {
            use std::os::windows::ffi::OsStrExt;
            path.as_os_str().encode_wide().chain([0]).collect()
        };
        // SAFETY: `wide` is NUL-terminated and outlives the call; the handle is closed below.
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                WRITE_DAC,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE || h.is_null() {
            return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1) as u32);
        }
        // SAFETY: `h` is a handle this function just opened.
        unsafe { CloseHandle(h) };
        Ok(())
    }

    // ─────────────────────────── arms ───────────────────────────────────────────────────

    /// THE QUESTION THE WHOLE FIX HANGS ON. If a standard user cannot write these DACLs, the
    /// ancestor-ACE repair is disqualified regardless of whether it would work, because the
    /// jail has to be default-on with zero privilege.
    ///
    /// Every root is measured twice — as the runner's ambient token and de-elevated — so the
    /// verdict is a DIFFERENTIAL rather than a bare failure. `deelevated-can-write-own-dacl`
    /// is the control that passes in BOTH arms: it proves the impersonated context can still
    /// write a DACL where it owns the object, so a refusal on `C:\` is about `C:\`.
    fn writedac_ability(fails: &mut u32, f: &Fixture) {
        println!("-- writedac_ability");
        let profile = std::env::var("USERPROFILE").unwrap_or_else(|_| r"C:\Users".to_string());
        let targets: Vec<(&str, PathBuf)> = vec![
            ("c-root", PathBuf::from(r"C:\")),
            ("c-users", PathBuf::from(r"C:\Users")),
            ("user-profile", PathBuf::from(&profile)),
            ("program-files", PathBuf::from(r"C:\Program Files")),
            ("hostedtoolcache", PathBuf::from(r"C:\hostedtoolcache")),
            ("temp", std::env::temp_dir()),
            ("own-fixture-root", f.root.clone()),
        ];

        let ambient_admin = admin_authority();
        println!("    ambient admin_authority={ambient_admin}");
        for (name, p) in &targets {
            if !p.exists() {
                println!("    ambient {name} ({}) — absent, skipped", p.display());
                continue;
            }
            match can_write_dacl(p) {
                Ok(()) => println!("    ambient {name} = WRITE_DAC granted"),
                Err(e) => println!("    ambient {name} = refused (win32 {e})"),
            }
        }

        let token = match deelevated_impersonation_token() {
            Ok(t) => t,
            Err(e) => {
                report(
                    fails,
                    "deelevated-token-available",
                    false,
                    &format!("could not build a de-elevated token: {e}"),
                );
                return;
            }
        };
        report(fails, "deelevated-token-available", true, "");

        // The admin verdict is taken against the token handle OUTSIDE the impersonation block,
        // so no RPC is ever issued under a restricted token.
        let deelev_admin_group = has_admin_group(token);
        let measured = impersonating(token, || {
            let rows: Vec<(String, Result<(), u32>)> = targets
                .iter()
                .filter(|(_, p)| p.exists())
                .map(|(n, p)| ((*n).to_string(), can_write_dacl(p)))
                .collect();
            rows
        });
        // SAFETY: the token handle is ours; closing it after the impersonation block.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(token) };

        let rows = match measured {
            Ok(v) => v,
            Err(e) => {
                report(
                    fails,
                    "deelevated-context-is-nonadmin",
                    false,
                    &format!("impersonation failed: {e}"),
                );
                return;
            }
        };

        // Without this the whole arm is vacuous: a context that is still an administrator
        // proves nothing about what a standard user can do.
        report(
            fails,
            "deelevated-context-is-nonadmin",
            deelev_admin_group == Some(false),
            &format!(
                "CheckTokenMembership(BUILTIN\\Administrators) on the de-elevated token = \
                 {deelev_admin_group:?} (ambient SCM admin_authority = {ambient_admin})"
            ),
        );

        for (name, outcome) in &rows {
            match outcome {
                Ok(()) => println!("    deelevated {name} = WRITE_DAC granted"),
                Err(e) => println!("    deelevated {name} = refused (win32 {e})"),
            }
        }
        let get = |name: &str| rows.iter().find(|(n, _)| n == name).map(|(_, r)| r);

        // The positive control. It must PASS in both arms — otherwise a refusal below is
        // indistinguishable from an impersonation that cannot write any DACL at all.
        report(
            fails,
            "deelevated-can-write-own-dacl",
            matches!(get("own-fixture-root"), Some(Ok(()))),
            "a de-elevated context must still control a directory it owns",
        );

        // The headline. These are the ancestors every absolute path's realpath walk ends at.
        for key in ["c-root", "c-users"] {
            let refused = matches!(get(key), Some(Err(_)));
            report(
                fails,
                &format!("deelevated-cannot-write-dacl-{key}"),
                refused,
                "an ancestor ACE is only shippable if a standard user can write it",
            );
        }
    }

    /// Which ancestors a LowBox token can `lstat` at all — the mechanism behind the defect,
    /// measured without Node in the way.
    fn ancestor_lstat_chain(fails: &mut u32, f: &Fixture) {
        println!("-- ancestor_lstat_chain");
        let policy = jail_shaped(f, Vec::new(), &[]);
        let marker = f.work.join("lstat.txt");
        let profile = std::env::var("USERPROFILE").unwrap_or_else(|_| r"C:\Users".to_string());
        let paths: Vec<String> = vec![
            r"C:\".to_string(),
            r"C:\Users".to_string(),
            profile,
            r"C:\Program Files".to_string(),
            f.work.to_string_lossy().into_owned(),
            f.ungranted.to_string_lossy().into_owned(),
        ];
        let mut argv = vec![marker.to_string_lossy().into_owned()];
        argv.extend(paths);
        let code = run_child_in(f, &policy, &f.work, "lstatchain", &argv);
        let text = std::fs::read_to_string(&marker).unwrap_or_else(|_| "<no marker>".into());
        println!("    exit={code}\n{}", indent(&text));

        let line = |needle: &str| {
            text.lines()
                .find(|l| {
                    l.trim_start()
                        .to_ascii_lowercase()
                        .starts_with(&needle.to_ascii_lowercase())
                })
                .unwrap_or("<absent>")
                .to_string()
        };
        let root = line(r"C:\ =");
        report(
            fails,
            "jail-lstat-c-root-refused",
            root.contains("err"),
            &root,
        );
        let work = line(&format!("{} =", f.work.display()));
        report(
            fails,
            "jail-lstat-granted-work-permitted",
            work.contains("= ok"),
            &work,
        );
        let outside = line(&format!("{} =", f.ungranted.display()));
        report(
            fails,
            "jail-lstat-ungranted-refused",
            outside.contains("err"),
            &outside,
        );
    }

    fn indent(s: &str) -> String {
        s.lines()
            .map(|l| format!("      {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The EXACT `NODE_OPTIONS` production stamps, taken from the shipping function rather
    /// than restated — a probe that measured its own copy of the string would go green on a
    /// shim the product does not actually set.
    fn shim_node_options() -> String {
        nub_sandbox::windows_realpath_node_options()
    }

    /// The REFUTED native-realpath shim as a bare `--import` argument, for the differential arm
    /// that passes flags on argv. Derived from the shipped-alongside function rather than
    /// restated, so the arm keeps measuring the exact string that was rejected.
    fn shim_import_arg() -> String {
        nub_sandbox::windows_native_realpath_shim_node_options()
            .split(" --import ")
            .nth(1)
            .expect("the native-realpath shim carries an --import")
            .to_string()
    }

    /// The measurement script. Every path is a baked-in LITERAL and every step appends its own
    /// line the moment it happens, so a crash midway still leaves the steps that did run — and
    /// nothing here reads the environment for a path.
    fn write_script(work: &Path, granted: &Path, ungranted: &Path, marker: &Path) -> PathBuf {
        let script = work.join("probe.js");
        let esc = |p: &Path| p.to_string_lossy().replace('\\', "\\\\");
        std::fs::write(work.join("dep.js"), "module.exports={tag:'dep-loaded'};\n").unwrap();
        // A real package directory has a manifest, and without one Node's nearest-parent
        // package.json walk climbs into ancestors the jail refuses — which would make the
        // arms that DON'T short-circuit that walk fail for a reason unrelated to realpath.
        std::fs::write(
            work.join("package.json"),
            "{\"name\":\"probe-fixture\",\"version\":\"0.0.0\"}\n",
        )
        .unwrap();
        std::fs::write(
            &script,
            format!(
                r#"const fs = require('fs');
const MARKER = "{marker}";
const GRANTED = "{granted}";
const UNGRANTED = "{ungranted}";
function put(line) {{ try {{ fs.appendFileSync(MARKER, line + "\n"); }} catch (e) {{}} }}
function rec(name, fn) {{
  try {{ put(name + "=ok:" + String(fn()).slice(0, 160)); }}
  catch (e) {{ put(name + "=err:" + (e.code || "?") + ":" + String(e.message).slice(0, 160)); }}
}}
put("main-ran=ok");
put("cwd=" + process.cwd());
put("node=" + process.versions.node);
// The shim points fs.realpathSync at its own native twin and re-exposes `.native` on it,
// so this identity holds only once the preload has actually evaluated.
put("shim=" + (fs.realpathSync === fs.realpathSync.native ? "1" : "0"));
put("nodeopts=" + (process.env.NODE_OPTIONS || "<unset>"));
rec("require-dep", () => require("./dep.js").tag);
rec("realpath-native", () => fs.realpathSync.native(GRANTED));
rec("realpath-js", () => fs.realpathSync(GRANTED));
rec("lstat-c-root", () => fs.lstatSync("C:\\").isDirectory());
rec("read-granted", () => fs.readFileSync(GRANTED, "utf8").trim());
rec("read-ungranted", () => fs.readFileSync(UNGRANTED, "utf8").trim());
// Blocker 2, on libuv's OWN path rather than by inference. A direct CreateNamedPipeW was
// measured refused while Rust's Stdio::piped() — also a named pipe — was permitted, so what
// `child_process` actually does is an open question, and it decides whether node-gyp (whose
// Python discovery pipes on every configure) can run under the jail at all.
// BLOCKER 2 IS NOT MEASURED HERE ANY MORE, because measuring it destroys the run. A piped
// `child_process` spawn under this jail does not fail — it BLOCKS INDEFINITELY, and Node's own
// `timeout` option cannot break it (the block is in libuv's named-pipe setup, before the timer
// arms). Reproduced twice, runs 30460192608 and 30461823852: both markers end exactly at this
// point and the harness had to kill the process. That is the finding; re-measuring it costs
// every arm after it.
put("done=ok");
"#,
                marker = esc(marker),
                granted = esc(granted),
                ungranted = esc(ungranted),
            ),
        )
        .unwrap();
        script
    }

    struct NodeArm {
        marker: String,
        log: String,
        code: i32,
    }

    /// Run the measurement script once under one flag/env combination.
    fn node_arm(
        f: &Fixture,
        node: &Path,
        tag: &str,
        flags: &[&str],
        env: &[(&str, String)],
    ) -> NodeArm {
        let marker = f.work.join(format!("{tag}.marker"));
        let log = f.work.join(format!("{tag}.log"));
        let granted = f.work.join("granted.txt");
        std::fs::write(&granted, "granted-content").unwrap();
        let script = write_script(&f.work, &granted, &f.ungranted, &marker);
        let policy = jail_shaped(f, vec![read_rule(node)], env);
        let mut argv = vec![
            log.to_string_lossy().into_owned(),
            node.to_string_lossy().into_owned(),
            script.to_string_lossy().into_owned(),
        ];
        argv.extend(flags.iter().map(|s| (*s).to_string()));
        let code = run_child_in(f, &policy, &f.work, "noderun", &argv);
        let marker_text = std::fs::read_to_string(&marker).unwrap_or_else(|_| "<no marker>".into());
        let log_text = std::fs::read_to_string(&log).unwrap_or_else(|_| "<no log>".into());
        println!("    [{tag}] exit={code}");
        println!("      -- marker --\n{}", indent(&marker_text));
        println!("      -- node stdio --\n{}", indent(&log_text));
        NodeArm {
            marker: marker_text,
            log: log_text,
            code,
        }
    }

    /// What each piece actually buys, isolated. Read top to bottom the arms say: the defect is
    /// real; `--preserve-symlinks-main` alone gets the ENTRY point in but leaves every
    /// dependency `require` broken; adding the `data:` shim fixes the rest without changing
    /// resolution semantics; and the whole thing survives delivery through `NODE_OPTIONS`,
    /// which is the only channel nub has over a lifecycle script's own `node` invocation.
    fn node_matrix(fails: &mut u32, f: &Fixture, node: &Path) {
        println!("-- node_matrix");
        let shim = shim_import_arg();

        let plain = node_arm(f, node, "plain", &[], &[]);
        report(
            fails,
            "node-plain-fails",
            !plain.marker.contains("main-ran=ok"),
            "the defect must reproduce, or every arm below is measuring nothing",
        );
        report(
            fails,
            "node-plain-names-realpath",
            plain.log.contains("realpath") || plain.log.contains("EPERM"),
            "the plain failure should still be the realpath one",
        );

        let main_only = node_arm(f, node, "psmain", &["--preserve-symlinks-main"], &[]);
        report(
            fails,
            "node-preserve-main-lets-entry-run",
            main_only.marker.contains("main-ran=ok"),
            "--preserve-symlinks-main clears the one realpath that precedes any preload",
        );
        report(
            fails,
            "node-preserve-main-alone-leaves-dep-broken",
            main_only.marker.contains("require-dep=err"),
            "a non-main require still walks the JS realpath",
        );

        // THE ONLY REMAINING CANDIDATE, and it is measured BEFORE the shim arm because the
        // shim's premise is already refuted: `fs.realpathSync.native` is refused in the jail
        // too (`realpath-native=err:EPERM` on a file the jail GRANTED and Node can read), so
        // redirecting realpath at the native twin cannot help. Only avoiding realpath outright
        // can, and `--preserve-symlinks` is the one lever that does it — at the cost of
        // dependency resolution no longer resolving symlinks, which an isolated node_modules
        // depends on. That cost is why this needs a measurement and a decision, not a default.
        let preserve_both = node_arm(
            f,
            node,
            "psboth",
            &["--preserve-symlinks-main", "--preserve-symlinks"],
            &[],
        );
        report(
            fails,
            "node-preserve-symlinks-resolves-dep",
            preserve_both.marker.contains("require-dep=ok"),
            "the only lever that avoids the JS realpath entirely",
        );
        report(
            fails,
            "node-preserve-symlinks-completes",
            preserve_both.marker.contains("done=ok"),
            "the script body must run to completion, not merely start",
        );

        let shimmed = node_arm(
            f,
            node,
            "shim",
            &["--preserve-symlinks-main", "--import", shim.as_str()],
            &[],
        );
        // The refuted candidate, kept as a standing differential. `node-shim-executed` proves
        // the data: preload really did evaluate, so the failure below is the OS refusing the
        // native realpath rather than the preload never arriving — without it the two are
        // indistinguishable. If a future Windows grants GetFinalPathNameByHandleW under an
        // AppContainer, this arm flips and the semantics-preserving route becomes available.
        report(
            fails,
            "node-shim-executed",
            shimmed.marker.contains("shim=1"),
            "the data: preload must actually have evaluated",
        );
        report(
            fails,
            "realpath-native-refused-in-jail",
            shimmed.marker.contains("realpath-native=err"),
            "the measured OS fact that disqualifies redirecting realpath at its native twin",
        );

        // The anti-vacuous control, asserting the SPECIFIC error. An `ENOENT` here would mean
        // the canary never tested confinement — that exact false green has been paid for once
        // already, when a canary path arrived through an env var the jail strips.
        let ungranted_line = preserve_both
            .marker
            .lines()
            .find(|l| l.starts_with("read-ungranted="))
            .unwrap_or("<absent>");
        let denied = ungranted_line.contains("EPERM")
            || ungranted_line.contains("EACCES")
            || ungranted_line.contains("EBUSY");
        report(
            fails,
            "psymlinks-ungranted-read-refused",
            denied && !ungranted_line.contains("ENOENT"),
            ungranted_line,
        );
        report(
            fails,
            "psymlinks-granted-read-permitted",
            preserve_both.marker.contains("read-granted=ok"),
            "the control that must pass in the same arm as the refusal above",
        );

        // Production delivery: nub cannot rewrite a lifecycle script's `node` argv, only its
        // environment.
        let via_env = node_arm(
            f,
            node,
            "nodeopts",
            &[],
            &[("NODE_OPTIONS", shim_node_options())],
        );
        report(
            fails,
            "node-shim-via-node-options",
            via_env.marker.contains("require-dep=ok"),
            "NODE_OPTIONS is the only channel nub has over a script's own node invocation",
        );

        let _ = shimmed.code;
    }

    /// THE SUCCESS CRITERION. Not rc=0 and not "no denial logged": a lifecycle-shaped script
    /// body, run under the REAL `compile_build_jail` policy in a package directory, writing
    /// its own marker from inside the jail.
    ///
    /// It also settles `strip_verbatim_prefix`, which until now was only symptom-confirmed:
    /// the script records `process.cwd()`, so the child's working directory being its own
    /// package dir (rather than the Windows directory cmd.exe silently falls back to when
    /// handed a `\\?\` path) is measured rather than inferred.
    fn production_lifecycle(fails: &mut u32, f: &Fixture, node: &Path) {
        println!("-- production_lifecycle");
        let project = f.root.join("project");
        let pkg = project.join("node_modules").join("demo-pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        let marker = pkg.join("postinstall.marker");
        let granted = pkg.join("granted.txt");
        std::fs::write(&granted, "granted-content").unwrap();
        let script = write_script(&pkg, &granted, &f.ungranted, &marker);

        let homes = nub_sandbox::Homes {
            home: f.root.join("home"),
            tmp: std::env::temp_dir(),
            cache: f.root.join("cache"),
            project: project.clone(),
        };
        let env = os_essential_env();
        let policy = match nub_sandbox::compile_build_jail(
            homes,
            &pkg,
            vec![node.to_path_buf()],
            Vec::new(),
            env,
        ) {
            Ok(p) => p,
            Err(e) => {
                report(
                    fails,
                    "lifecycle-policy-compiles",
                    false,
                    &format!("compile_build_jail failed: {e}"),
                );
                return;
            }
        };
        report(fails, "lifecycle-policy-compiles", true, "");

        let log = pkg.join("postinstall.log");
        let argv = vec![
            log.to_string_lossy().into_owned(),
            node.to_string_lossy().into_owned(),
            script.to_string_lossy().into_owned(),
        ];
        // The child image lives outside the package dir, so the production policy has to be
        // asked for it the way the engine does — as an extra read grant on the interpreter
        // plus the harness's own binary.
        let mut policy = policy;
        policy.fs.rules.entries.push(read_rule(&f.child));
        // HYPOTHETICAL, and stamped here rather than in the product: `build_jail.rs` no longer
        // sets NODE_OPTIONS (the repair is disqualified — it silently resolves the wrong
        // package under the default isolated layout), and the lifecycle env allowlist no longer
        // admits the key. Injecting it directly into the compiled policy keeps the measurement
        // — "IF the jail stamped this, does a real script body run?" — without shipping it.
        policy
            .env
            .constructed
            .insert("NODE_OPTIONS".to_string(), shim_node_options());
        let code = run_child_in(f, &policy, &pkg, "noderun", &argv);
        let marker_text = std::fs::read_to_string(&marker).unwrap_or_else(|_| "<no marker>".into());
        let log_text = std::fs::read_to_string(&log).unwrap_or_else(|_| "<no log>".into());
        println!("    exit={code}");
        println!("      -- marker --\n{}", indent(&marker_text));
        println!("      -- node stdio --\n{}", indent(&log_text));

        report(
            fails,
            "lifecycle-script-body-completed",
            marker_text.contains("done=ok"),
            "a real script body must run to completion and write its OWN marker",
        );
        let cwd_line = marker_text
            .lines()
            .find(|l| l.starts_with("cwd="))
            .unwrap_or("<absent>");
        // Compared by TAIL, not by full path: `%TEMP%` reaches the probe as an 8.3 short name
        // (`RUNNER~1`) while the child reports the long form (`runneradmin`), so a whole-path
        // match fails on two spellings of the same directory.
        let cwd_ok = cwd_line
            .to_ascii_lowercase()
            .ends_with("\\project\\node_modules\\demo-pkg");
        report(
            fails,
            "lifecycle-cwd-is-package-dir",
            cwd_ok,
            &format!("{cwd_line} (expected under {})", pkg.display()),
        );
    }

    pub fn probe_main() -> i32 {
        let mut fails = 0u32;
        println!("PROBE windows realpath ancestors under AppContainer");

        let Some(node) = path_node() else {
            eprintln!("no node.exe on PATH — the probe cannot run");
            return 1;
        };
        let node = std::fs::canonicalize(&node)
            .map(|p| unverbatim(&p))
            .unwrap_or(node);
        println!("interpreter: {}", node.display());
        println!("refuted-shim: {}", shim_import_arg());
        println!("stamped NODE_OPTIONS: {}", shim_node_options());

        // Each arm announces itself, flushed, BEFORE it runs. CI logs are unavailable until a
        // job finishes, so a stalled arm is otherwise invisible — the job just sits there and
        // a timeout discards the whole log. With these, the last line printed names the arm.
        let step = |name: &str| {
            use std::io::Write;
            println!("STEP {name}");
            let _ = std::io::stdout().flush();
        };

        step("writedac_ability");
        {
            let f = Fixture::new("dacl");
            writedac_ability(&mut fails, &f);
        }
        step("ancestor_lstat_chain");
        {
            let f = Fixture::new("lstat");
            ancestor_lstat_chain(&mut fails, &f);
        }
        step("node_matrix");
        {
            let f = Fixture::new("node");
            node_matrix(&mut fails, &f, node.as_path());
        }
        step("production_lifecycle");
        {
            let f = Fixture::new("life");
            production_lifecycle(&mut fails, &f, node.as_path());
        }
        step("done");

        if fails == 0 {
            println!("WINDOWS JAIL RESOLVES ABSOLUTE REQUIRES");
            0
        } else {
            eprintln!("{fails} propert(y/ies) failed");
            1
        }
    }
}

//! Windows AppContainer — documented-residual verification + constraint-only red-team.
//!
//! Empirically confirms each Windows residual is REAL and BOUNDED, and probes novel
//! breakout vectors the enforcement suite does not cover:
//!   1. program-dir neighbor read leak (documented): a secret next to the child binary
//!      is readable because the backend auto-grants the program's PARENT DIR.
//!   2. reparse (junction) escape from a write-granted dir: a junction inside the
//!      write-granted tree pointing at an ungranted target — does the AC-SID grant
//!      follow the reparse to the target (escape) or does the OS check the target ACL?
//!   3. AAP-inheritance trap (documented): a secret under a dir carrying an inherited
//!      `ALL APPLICATION PACKAGES` allow-ACE is readable regardless of the allow-set —
//!      i.e. WHY confined work dirs must live under a nub-cleaned DACL root.
//!   4. deny-ACE vs inherited-AAP, ON A READ (the open question the backend's allowlist
//!      rationale rests on): does an EXPLICIT deny-ACE for the AppContainer SID beat an
//!      INHERITED `ALL APPLICATION PACKAGES` allow inside a granted subtree? Plus the
//!      companion escalation question: can the same-user LowBox child WRITE_DAC its way
//!      around such a deny, and does an `OWNER_RIGHTS` ACE close that?
//!
//! `harness = false`: `__resid__ <role> <args>` runs as the AppContainer child; any
//! other invocation runs the cases. Child exit: 0 ok, 5 access-denied, 9 other.

#[cfg(not(target_os = "windows"))]
fn main() {}

#[cfg(target_os = "windows")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("__resid__") {
        std::process::exit(win::child_main(&args[2..]));
    }
    match win::run() {
        Ok(()) => println!("ALL WINDOWS RESIDUAL PROBES PASSED"),
        Err(n) => {
            eprintln!("{n} WINDOWS RESIDUAL PROBE(S) FAILED");
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
    use std::path::{Path, PathBuf};

    pub fn child_main(a: &[String]) -> i32 {
        match a.first().map(String::as_str) {
            Some("read") => match std::fs::read(&a[1]) {
                Ok(_) => 0,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => 5,
                Err(_) => 9,
            },
            Some("write") => match std::fs::write(&a[1], b"pwned") {
                Ok(_) => 0,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => 5,
                Err(_) => 9,
            },
            // Connect to a parent-created named pipe and try to read the secret it serves
            // (local-IPC exfil, a namespace distinct from the loopback-TCP path). 0 if the
            // secret is recovered (EXFIL), 5 if the open is denied, 9 otherwise.
            Some("pipe") => match std::fs::read(&a[1]) {
                Ok(b) if b.windows(4).any(|w| w == b"leak") => 0,
                Ok(_) => 8,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => 5,
                Err(_) => 9,
            },
            // Attempt to spawn a descendant that BREAKS AWAY from the Job. On a Job
            // without JOB_OBJECT_LIMIT_BREAKAWAY_OK, CreateProcess fails ACCESS_DENIED
            // (contained → 5); if it succeeds the breakaway was honored (escape → 0).
            Some("breakaway") => {
                use std::os::windows::process::CommandExt;
                const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
                let Ok(exe) = std::env::current_exe() else {
                    return 9;
                };
                match std::process::Command::new(exe)
                    .args(["__resid__", "sleep"])
                    .creation_flags(CREATE_BREAKAWAY_FROM_JOB)
                    .spawn()
                {
                    Ok(mut c) => {
                        let _ = c.kill();
                        let _ = c.wait();
                        0
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => 5,
                    Err(_) => 9,
                }
            }
            Some("sleep") => {
                std::thread::sleep(std::time::Duration::from_secs(30));
                0
            }
            // Same-user DACL-rewrite escalation: take WRITE_DAC on the target and
            // neutralize its DACL (a NULL DACL grants everyone), then read. A LowBox token
            // still carries the user's identity, so if the kernel's implicit owner grant
            // survives AppContainer confinement this defeats ANY deny-ACE denylist
            // regardless of ACE ordering. 0 = ESCALATED, 5 = the open or the rewrite was
            // refused, 6 = DACL rewritten but the read still failed, 9 = other.
            Some("dacl") => dacl_escalate(&a[1]),
            // The reader a surrogate mechanism spawns: read the policy-denied secret and
            // drop a verdict marker the (unconfined) runner polls. If the surrogate escaped
            // confinement this runs under a full token and LEAKS; if it landed confined the
            // read is denied. Exit code is unused — the marker is the signal.
            Some("surrogate-read") => surrogate::read_role(&a[1], &a[2]),
            // Run one surrogate mechanism from INSIDE the AppContainer, asking a service (or
            // a re-parent) to spawn `surrogate-read`. Exit maps the SPAWN outcome only; the
            // READ outcome is the marker the runner reads. args: <vector> <reader> <secret>
            // <marker> <batch> <anchor_pid>.
            Some("surrogate") => surrogate::attempt(
                &a[1],
                Path::new(&a[2]),
                Path::new(&a[3]),
                Path::new(&a[4]),
                Path::new(&a[5]),
                a[6].parse().unwrap_or(0),
            )
            .to_exit_code(),
            _ => 2,
        }
    }

    /// The `dacl` child role's body (see [`child_main`]).
    fn dacl_escalate(path: &str) -> i32 {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo};
        use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        const WRITE_DAC: u32 = 0x0004_0000;

        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: NUL-terminated wide path, NULL security attributes, no template handle.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                WRITE_DAC,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return 5;
        }
        // A NULL DACL with DACL_SECURITY_INFORMATION means "no DACL" — unrestricted access.
        let rc = unsafe {
            SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        unsafe { CloseHandle(handle) };
        if rc != 0 {
            return 5;
        }
        match std::fs::read(path) {
            Ok(_) => 0,
            Err(_) => 6,
        }
    }

    // ── helpers ───────────────────────────────────────────────────────────────────

    fn secure_root(root: &Path) {
        let user = std::env::var("USERNAME").expect("USERNAME set on Windows");
        let status = std::process::Command::new("icacls")
            .arg(root)
            .args(["/inheritance:r", "/grant:r"])
            .arg(format!("{user}:(OI)(CI)F"))
            .status()
            .expect("run icacls");
        assert!(status.success(), "icacls secure_root failed");
    }

    /// Create an NTFS junction (directory reparse point) `link` → `target` via cmd's
    /// `mklink /J` (junctions need no privilege, unlike symlinks). Returns success.
    fn make_junction(link: &Path, target: &Path) -> bool {
        std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn canon(p: &Path) -> String {
        p.to_string_lossy().replace('\\', "/")
    }

    fn read_confine(read: &[&Path], write: &[&Path]) -> SandboxPolicy {
        let mut entries = Vec::new();
        for r in read {
            entries.push(rule(r, FsAccess::Read));
        }
        for w in write {
            entries.push(rule(w, FsAccess::ReadWrite));
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
            env: EnvPolicy::resolved(std::env::vars().collect()),
            pid: PidPolicy::default(),
        }
    }
    fn rule(p: &Path, access: FsAccess) -> FsRule {
        FsRule {
            matcher: CanonGlob(canon(p)),
            effect: Effect::Allow,
            access,
        }
    }

    fn code(policy: &SandboxPolicy, program: &Path, args: &[&str]) -> i32 {
        let spec = CommandSpec::new(program.as_os_str())
            .args(args.iter().copied())
            .cwd(program.parent().expect("fixture child has a parent"));
        let prepared = match apply(policy, spec) {
            Ok(p) => p,
            Err(d) => {
                eprintln!("  [apply Err] {d:?}");
                return -100;
            }
        };
        match prepared.status() {
            Ok(s) => s.code().unwrap_or(-1),
            Err(e) => {
                eprintln!("  [status Err] {e} os={:?}", e.raw_os_error());
                -101
            }
        }
    }

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

    fn native(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    /// Start a named-pipe server (DEFAULT security) on a background thread that serves the
    /// secret `PIPE_SECRET=leak` to the first client, then closes. Returns the pipe path a
    /// client opens (`\\.\pipe\<name>`). Default security is the realistic case: nub does
    /// not grant the AppContainer SID on an IPC object it holds.
    fn start_pipe_server(name: &str) -> String {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_OUTBOUND;
        use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_WAIT};
        // windows-sys gates these two behind feature combos that fight the pipe imports;
        // link them raw from kernel32 (overlapped arg passed NULL for a blocking pipe).
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn ConnectNamedPipe(h: HANDLE, overlapped: *mut std::ffi::c_void) -> i32;
            fn WriteFile(
                h: HANDLE,
                buf: *const u8,
                len: u32,
                wrote: *mut u32,
                overlapped: *mut std::ffi::c_void,
            ) -> i32;
        }
        let full = format!(r"\\.\pipe\{name}");
        let wide: Vec<u16> = full.encode_utf16().chain(std::iter::once(0)).collect();
        std::thread::spawn(move || {
            // SAFETY: create a byte-mode outbound pipe with DEFAULT security (NULL SA).
            let h = unsafe {
                CreateNamedPipeW(
                    wide.as_ptr(),
                    PIPE_ACCESS_OUTBOUND,
                    PIPE_TYPE_BYTE | PIPE_WAIT,
                    1,
                    512,
                    512,
                    0,
                    std::ptr::null(),
                )
            };
            if h as isize == -1 {
                return;
            }
            // Block for a client, write the secret, disconnect. Timeout-free: the test
            // always connects exactly one client; the fixture teardown ends the process.
            unsafe {
                let _ = ConnectNamedPipe(h, std::ptr::null_mut());
                let msg = b"PIPE_SECRET=leak";
                let mut wrote = 0u32;
                WriteFile(
                    h,
                    msg.as_ptr(),
                    msg.len() as u32,
                    &mut wrote,
                    std::ptr::null_mut(),
                );
                CloseHandle(h);
            }
        });
        full
    }

    // ── the cases ─────────────────────────────────────────────────────────────────

    pub fn run() -> Result<(), u32> {
        let mut fails = 0u32;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("nub-resid-{nonce:x}"));
        std::fs::create_dir_all(&root).unwrap();
        secure_root(&root);

        let bin = root.join("bin");
        let work = root.join("work");
        let vault = root.join("vault");
        for d in [&bin, &work, &vault] {
            std::fs::create_dir_all(d).unwrap();
        }
        let child = bin.join("child.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &child).unwrap();

        // A secret sitting NEXT TO the binary (bin/), and one in an unrelated vault.
        let neighbor = bin.join("neighbor.env");
        std::fs::write(&neighbor, b"NEIGHBOR_SECRET=leak").unwrap();
        let vault_secret = vault.join("secret.env");
        std::fs::write(&vault_secret, b"VAULT_SECRET=leak").unwrap();

        // Policy: read-confine granting ONLY work/. bin/ and vault/ are NOT in the
        // allow-set. The backend auto-grants bin/ (the program's parent dir).
        let confine = read_confine(&[&work], &[]);

        // ── residual 1: program-dir neighbor read leak — CLOSED by the file-only grant.
        // The backend now grants the program FILE, not its parent dir, so a `.env` next
        // to the child binary is NO LONGER swept into the allow-set. Regression guard for
        // the fix: the neighbor is DENIED. (Pre-fix this returned 0 — the leak.)
        expect_in(
            &mut fails,
            "R1 program-dir neighbor .env DENIED (file-only program grant — leak CLOSED)",
            code(&confine, &child, &["__resid__", "read", &native(&neighbor)]),
            &[5, 9],
        );
        // A secret NOT next to the program and NOT granted stays denied too.
        expect_in(
            &mut fails,
            "R1-bound vault secret (not program-dir, not granted) DENIED",
            code(
                &confine,
                &child,
                &["__resid__", "read", &native(&vault_secret)],
            ),
            &[5, 9],
        );

        // ── red-team A: reparse (junction) escape from a write-granted dir ──────────
        // Grant read+write on work/. Place a junction work/esc → vault (ungranted). If
        // the AC-SID modify grant follows the reparse to vault, a write through it
        // escapes the write-confine; a read through it escapes the read-confine.
        let wc = read_confine(&[&work], &[&work]);
        let esc = work.join("esc");
        if make_junction(&esc, &vault) {
            let esc_secret = esc.join("secret.env"); // resolves to vault/secret.env
            expect_in(
                &mut fails,
                "RT-A read vault secret THROUGH work/ junction DENIED (reparse-safe)",
                code(&wc, &child, &["__resid__", "read", &native(&esc_secret)]),
                &[5, 9],
            );
            let esc_write = esc.join("pwned.txt"); // resolves to vault/pwned.txt
            expect_in(
                &mut fails,
                "RT-A write into vault THROUGH work/ junction DENIED (reparse-safe)",
                code(&wc, &child, &["__resid__", "write", &native(&esc_write)]),
                &[5, 9],
            );
            // NC: a write to a genuine path inside work/ succeeds (grant is real).
            expect(
                &mut fails,
                "RT-A NC write to a real path inside work/ (grant works)",
                code(
                    &wc,
                    &child,
                    &["__resid__", "write", &native(&work.join("real.txt"))],
                ),
                0,
            );
        } else {
            eprintln!("SKIP RT-A: mklink /J failed (no junction support)");
        }

        // ── red-team C: named-pipe IPC exfil ───────────────────────────────────────
        // The parent serves a secret over a DEFAULT-security named pipe (distinct object
        // namespace from the loopback-TCP path already tested). The AppContainer child
        // tries to open + read it. Expect DENIED — a LowBox token is not on a normal
        // pipe's DACL. A successful read would be a local-IPC exfil breakout.
        let pipe_name = format!("nub-resid-{nonce:x}");
        let pipe_path = start_pipe_server(&pipe_name);
        std::thread::sleep(std::time::Duration::from_millis(300));
        expect_in(
            &mut fails,
            "RT-C named-pipe read DENIED (LowBox not on the pipe DACL — no IPC exfil)",
            code(&confine, &child, &["__resid__", "pipe", &pipe_path]),
            &[5, 9],
        );

        // ── red-team D: Job-object breakaway ───────────────────────────────────────
        // The child tries to spawn a descendant with CREATE_BREAKAWAY_FROM_JOB. The Job
        // is created without JOB_OBJECT_LIMIT_BREAKAWAY_OK, so the breakaway must fail
        // (contained). A success would let a descendant outlive the Job's reap.
        expect_in(
            &mut fails,
            "RT-D Job breakaway DENIED (no JOB_OBJECT_LIMIT_BREAKAWAY_OK — contained)",
            code(&confine, &child, &["__resid__", "breakaway"]),
            &[5, 9],
        );

        // ── residual 3 / red-team B: AAP-inheritance trap ──────────────────────────
        // A SEPARATE root created WITHOUT secure_root, so it inherits %TEMP%'s ACEs
        // (which on many systems include an inheritable ALL APPLICATION PACKAGES read).
        // A secret there, NOT in the allow-set, tests whether an inherited AAP grant
        // defeats default-deny — the reason a nub-cleaned DACL root is required.
        let dirty = std::env::temp_dir().join(format!("nub-dirty-{nonce:x}"));
        std::fs::create_dir_all(&dirty).unwrap();
        let dirty_secret = dirty.join("aap.env");
        std::fs::write(&dirty_secret, b"AAP_SECRET=leak").unwrap();
        let r = code(
            &confine,
            &child,
            &["__resid__", "read", &native(&dirty_secret)],
        );
        // Report BOTH outcomes explicitly — this is diagnostic, not a hard assert: if
        // %TEMP% carries AAP the read SUCCEEDS (trap real → clean-root contract needed);
        // if it does not, the read is denied (this host's %TEMP% has no AAP).
        match r {
            0 => println!(
                "INFO RT-B AAP-TRAP CONFIRMED: secret under a non-cleaned %TEMP% dir READABLE \
                 (inherited ALL APPLICATION PACKAGES defeats default-deny — clean-DACL root required)"
            ),
            5 | 9 => println!(
                "INFO RT-B: secret under a non-cleaned %TEMP% dir DENIED (this host's %TEMP% \
                 carries no inherited AAP grant)"
            ),
            other => {
                fails += 1;
                eprintln!("FAIL RT-B unexpected exit {other}");
            }
        }
        let _ = std::fs::remove_dir_all(&dirty);

        // ── residual 4: explicit deny-ACE vs inherited AAP, on a READ ──────────────
        // Owns its own LowBox launches (see the `denyace` module doc) and its own roots,
        // because the decisive cell needs an AAP-carrying root that `apply` would reject.
        let deny_base = std::env::temp_dir().join(format!("nub-denyace-{nonce:x}"));
        std::fs::create_dir_all(&deny_base).unwrap();
        denyace::matrix(&mut fails, &child, &deny_base);
        let _ = std::fs::remove_dir_all(&deny_base);

        // ── surrogate-spawn escape class (subordinate to residual 4) ───────────────
        // Does the per-run AppContainer already close the surrogate-spawn class SRT's
        // dedicated-account design exists to close? Each vector asks a service / a re-parent
        // to read the denied secret; only a spawned process that ACTUALLY READS it is a
        // breach — a spawn that lands still-confined is a PASS. The child dir is granted so
        // a not-granted secret under the same clean root stays denied.
        let surro_base = std::env::temp_dir().join(format!("nub-surrogate-{nonce:x}"));
        std::fs::create_dir_all(&surro_base).unwrap();
        surrogate::matrix(&mut fails, &child, &surro_base);
        let _ = std::fs::remove_dir_all(&surro_base);

        let _ = std::fs::remove_dir_all(&root);
        if fails == 0 { Ok(()) } else { Err(fails) }
    }

    #[allow(dead_code)]
    fn _unused(_: PathBuf) {}

    /// Residual 4 — does an EXPLICIT deny-ACE for the AppContainer SID beat an INHERITED
    /// `ALL APPLICATION PACKAGES` allow, for a READ, inside a granted subtree?
    ///
    /// The backend's allowlist rationale asserts it does not. The probe that produced that
    /// assertion is RT-B above, which measured DEFAULT-DENY vs inherited AAP — it contains
    /// no explicit deny-ACE at all, so the ordering question was never asked. Canonical
    /// DACL ordering puts explicit denies ahead of inherited allows, so the assertion may
    /// be backwards; this matrix settles it empirically. If the deny holds, a per-file
    /// denylist becomes expressible on Windows with no new dependency.
    ///
    /// Why this module owns its LowBox launches instead of reusing [`code`]: the deny must
    /// name the per-run AppContainer SID and be in place BEFORE launch (`apply` mints that
    /// SID internally and never exposes it), `apply` REJECTS an AAP-carrying root outright
    /// (`verify_clean_root`), and LPAC is not a posture the backend can express at all.
    mod denyace {
        use std::io;
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::AsRawHandle;
        use std::path::{Path, PathBuf};
        use windows_sys::Win32::Foundation::{
            CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree,
            SetHandleInformation, WAIT_OBJECT_0,
        };
        use windows_sys::Win32::Security::Authorization::{
            ConvertSidToStringSidW, ConvertStringSidToSidW, DENY_ACCESS, EXPLICIT_ACCESS_W,
            GRANT_ACCESS, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS,
            SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID,
            TRUSTEE_IS_USER, TRUSTEE_W,
        };
        use windows_sys::Win32::Security::Isolation::{
            CreateAppContainerProfile, DeleteAppContainerProfile,
        };
        use windows_sys::Win32::Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION,
            FreeSid, GetAce, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
            PSID, SECURITY_CAPABILITIES,
        };
        use windows_sys::Win32::System::Threading::{
            CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
            GetExitCodeProcess, InitializeProcThreadAttributeList,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
            PROCESS_INFORMATION, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
            WaitForSingleObject,
        };

        // The file-specific rights the cells grant and deny. Spelled literally (rather than
        // via GENERIC_*) so the mask under test is exactly the one reported in the DACL dump
        // — a generic mask is rewritten by the generic mapping on the way in, which would
        // make "did my ACE land as I intended?" unanswerable from the dump.
        const FILE_GENERIC_READ: u32 = 0x0012_0089;
        const FILE_GENERIC_EXECUTE: u32 = 0x0012_00A0;
        const READ_CONTROL: u32 = 0x0002_0000;
        const NO_INHERITANCE: u32 = 0x0;
        const ALL_APPLICATION_PACKAGES_SID: &str = "S-1-15-2-1";
        const OWNER_RIGHTS_SID: &str = "S-1-3-4";
        const ACE_TYPE_ALLOWED: u8 = 0x00;
        const ACE_TYPE_DENIED: u8 = 0x01;
        const INHERITED_ACE: u8 = 0x10;
        // ProcThreadAttributeValue(ProcThreadAttributeAllApplicationPackagesPolicy = 15,
        // thread = 0, input = 1, additive = 0). Not exported by windows-sys 0.61.
        const ATTR_ALL_APPLICATION_PACKAGES_POLICY: usize = 0x0002_000F;
        const ALL_APPLICATION_PACKAGES_OPT_OUT: u32 = 0x01;
        const CHILD_WAIT_MS: u32 = 60_000;

        /// One matrix cell's outcome. `deny_held` is `None` when the child never produced a
        /// usable verdict (launch/setup failure) — distinct from "the deny was defeated".
        struct Outcome {
            deny_held: Option<bool>,
            control_ok: bool,
        }

        /// A per-cell AppContainer identity, with RAII teardown of the profile and of every
        /// ACE placed for its SID. The SID is unique per cell, so a REVOKE_ACCESS sweep
        /// removes exactly our ACEs (allow AND deny) wherever we put them.
        struct Container {
            name: Vec<u16>,
            sid: PSID,
            sid_text: String,
            touched: Vec<PathBuf>,
        }

        impl Container {
            fn new(tag: &str) -> io::Result<Self> {
                // AppContainer profile names are <= 64 chars, alphanumeric + underscore.
                let name = format!(
                    "nub_deny_{}_{}_{tag}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0)
                );
                let wname = to_wide(&name);
                let mut sid: PSID = std::ptr::null_mut();
                // SAFETY: NUL-terminated name reused for display name + description.
                let hr = unsafe {
                    CreateAppContainerProfile(
                        wname.as_ptr(),
                        wname.as_ptr(),
                        wname.as_ptr(),
                        std::ptr::null(),
                        0,
                        &mut sid,
                    )
                };
                if hr != 0 {
                    return Err(io::Error::other(format!(
                        "CreateAppContainerProfile failed hr=0x{hr:08x}"
                    )));
                }
                let sid_text = sid_to_string(sid);
                Ok(Self {
                    name: wname,
                    sid,
                    sid_text,
                    touched: Vec::new(),
                })
            }

            fn grant_read_exec(&mut self, path: &Path, inherit: bool) -> io::Result<()> {
                self.touched.push(path.to_path_buf());
                set_ace(
                    path,
                    self.sid,
                    FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
                    GRANT_ACCESS,
                    inherit,
                )
            }

            /// The thing under test: an explicit, this-object-only DENY for the container's
            /// own SID, on a file that already carries an inheritable allow for that SID
            /// (and, in the AAP cells, an inherited AAP allow as well).
            fn deny_read(&mut self, path: &Path) -> io::Result<()> {
                self.touched.push(path.to_path_buf());
                set_ace(
                    path,
                    self.sid,
                    FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
                    DENY_ACCESS,
                    false,
                )
            }

            fn launch(&self, program: &Path, args: &[&str], lpac: bool) -> i32 {
                let mut caps = SECURITY_CAPABILITIES {
                    AppContainerSid: self.sid,
                    Capabilities: std::ptr::null_mut(),
                    CapabilityCount: 0,
                    Reserved: 0,
                };
                let mut opt_out = ALL_APPLICATION_PACKAGES_OPT_OUT;
                let handles = inheritable_std_handles();
                let count = 1 + u32::from(!handles.is_empty()) + u32::from(lpac);
                let Ok(mut attrs) = AttrList::new(count) else {
                    return -110;
                };
                if attrs
                    .update(
                        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                        std::ptr::from_mut(&mut caps).cast(),
                        std::mem::size_of::<SECURITY_CAPABILITIES>(),
                    )
                    .is_err()
                {
                    return -111;
                }
                if !handles.is_empty()
                    && attrs
                        .update(
                            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                            handles.as_ptr().cast_mut().cast(),
                            std::mem::size_of::<HANDLE>() * handles.len(),
                        )
                        .is_err()
                {
                    return -112;
                }
                if lpac
                    && attrs
                        .update(
                            ATTR_ALL_APPLICATION_PACKAGES_POLICY,
                            std::ptr::from_mut(&mut opt_out).cast(),
                            std::mem::size_of::<u32>(),
                        )
                        .is_err()
                {
                    return -113;
                }

                let mut cmdline = command_line(program, args);
                let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
                si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
                si.lpAttributeList = attrs.as_ptr();
                let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

                // lpEnvironment NULL ⇒ inherit the parent block. An AppContainer resolves
                // its per-container storage from the passed environment, and a hand-rolled
                // minimal block fails the spawn with ERROR_ENVVAR_NOT_FOUND (203) — this
                // probe is not testing the env axis, so it takes the complete block.
                //
                // SAFETY: cmdline/attrs/caps/opt_out/handles all outlive the call;
                // lpCommandLine is a writable UTF-16 buffer as CreateProcessW requires.
                let ok = unsafe {
                    CreateProcessW(
                        std::ptr::null(),
                        cmdline.as_mut_ptr(),
                        std::ptr::null(),
                        std::ptr::null(),
                        i32::from(!handles.is_empty()),
                        EXTENDED_STARTUPINFO_PRESENT,
                        std::ptr::null(),
                        std::ptr::null(),
                        std::ptr::from_mut(&mut si).cast(),
                        &mut pi,
                    )
                };
                if ok == 0 {
                    let e = io::Error::last_os_error();
                    eprintln!("  [CreateProcessW] {e} os={:?}", e.raw_os_error());
                    return -114;
                }
                // SAFETY: wait bounded, read the code, close both handles exactly once.
                unsafe {
                    if WaitForSingleObject(pi.hProcess, CHILD_WAIT_MS) != WAIT_OBJECT_0 {
                        TerminateProcess(pi.hProcess, 1);
                        CloseHandle(pi.hThread);
                        CloseHandle(pi.hProcess);
                        return -115;
                    }
                    let mut code = 0u32;
                    GetExitCodeProcess(pi.hProcess, &mut code);
                    CloseHandle(pi.hThread);
                    CloseHandle(pi.hProcess);
                    code as i32
                }
            }
        }

        impl Drop for Container {
            fn drop(&mut self) {
                for path in &self.touched {
                    let _ = set_ace(path, self.sid, 0, REVOKE_ACCESS, false);
                }
                // DeleteAppContainerProfile drops the profile state but not the SID buffer;
                // per MSDN a CreateAppContainerProfile SID is released with FreeSid.
                unsafe {
                    DeleteAppContainerProfile(self.name.as_ptr());
                    FreeSid(self.sid);
                }
            }
        }

        // ── the matrix ────────────────────────────────────────────────────────────

        /// Run all cells. `child` is the fixture copy of this binary (the `__resid__` role
        /// dispatcher); `base` is a scratch dir the cells create their roots under.
        pub(super) fn matrix(fails: &mut u32, child: &Path, base: &Path) {
            for (tag, aap_root, lpac) in [
                ("clean-noLPAC", false, false),
                ("aap-noLPAC", true, false),
                ("clean-LPAC", false, true),
                ("aap-LPAC", true, true),
            ] {
                let root = base.join(tag);
                match read_cell(child, &root, tag, aap_root, lpac) {
                    Ok(outcome) => report(fails, tag, &outcome, lpac),
                    Err(e) => {
                        *fails += 1;
                        eprintln!("FAIL PROBE {tag}: setup failed: {e}");
                    }
                }
                let _ = std::fs::remove_dir_all(&root);
            }
            owner_rights_cells(fails, child, base);
        }

        /// One read cell: a granted subtree carrying an inheritable AC-SID read allow, a
        /// `secret.env` inside it carrying an explicit AC-SID DENY, and a sibling
        /// `control.txt` with no deny. The control is what separates "the deny won" from
        /// "the grant never worked" — without it a universal denial proves nothing.
        fn read_cell(
            child: &Path,
            root: &Path,
            tag: &str,
            aap_root: bool,
            lpac: bool,
        ) -> io::Result<Outcome> {
            let granted = prepare_root(root, aap_root)?;
            let secret = granted.join("secret.env");
            let control = granted.join("control.txt");

            // Grant the inheritable AC-SID allow on the directory BEFORE creating the files,
            // so each inherits it at creation — not relying on SetNamedSecurityInfoW to
            // re-propagate onto already-existing children. The files then carry the inherited
            // AC-SID allow (and, in the AAP cells, the inherited AAP allow from the root);
            // the explicit AC-SID deny is added to secret.env alone. This is the exact
            // deny-inside-a-granted-subtree shape under test.
            let mut container = Container::new(tag)?;
            container.grant_read_exec(child, false)?;
            container.grant_read_exec(&granted, true)?;
            std::fs::write(&secret, b"DENY_PROBE_SECRET=leak")?;
            std::fs::write(&control, b"control-readable")?;
            container.deny_read(&secret)?;

            println!("  [{tag}] AC SID {}", container.sid_text);
            println!("  [{tag}] secret.env {}", dump_dacl(&secret));
            println!("  [{tag}] control.txt {}", dump_dacl(&control));

            let secret_code =
                container.launch(child, &["__resid__", "read", &native(&secret)], lpac);
            let control_code =
                container.launch(child, &["__resid__", "read", &native(&control)], lpac);
            println!("  [{tag}] exit: secret={secret_code} control={control_code}");
            Ok(Outcome {
                deny_held: match secret_code {
                    0 => Some(false),
                    5 | 9 => Some(true),
                    _ => None,
                },
                control_ok: control_code == 0,
            })
        }

        /// The companion escalation question, which bears on the same decision: a LowBox
        /// child runs as the same USER that owns the fixture, and the kernel grants an
        /// object's owner an implicit `READ_CONTROL | WRITE_DAC`. If that survives
        /// AppContainer confinement the child can rewrite the DACL and erase any deny —
        /// defeating a denylist regardless of how the ACEs are ordered. An `OWNER_RIGHTS`
        /// (S-1-3-4) ACE with a non-zero mask is documented to REPLACE that implicit grant,
        /// so the second cell measures whether it closes the hole.
        fn owner_rights_cells(fails: &mut u32, child: &Path, base: &Path) {
            for (tag, owner_rights) in [("escalate-bare", false), ("escalate-ownerrights", true)] {
                let root = base.join(tag);
                match escalation_cell(child, &root, tag, owner_rights) {
                    Ok(code) => {
                        let verdict = match code {
                            0 => "ESCALATED (DACL rewritten, secret read)",
                            5 => "blocked (WRITE_DAC open or rewrite refused)",
                            6 => "partial (DACL rewritten, read still failed)",
                            other => {
                                *fails += 1;
                                eprintln!("FAIL PROBE {tag}: unexpected exit {other}");
                                continue;
                            }
                        };
                        println!("PROBE {tag} owner_rights={owner_rights} escalation={verdict}");
                    }
                    Err(e) => {
                        *fails += 1;
                        eprintln!("FAIL PROBE {tag}: setup failed: {e}");
                    }
                }
                let _ = std::fs::remove_dir_all(&root);
            }
        }

        fn escalation_cell(
            child: &Path,
            root: &Path,
            tag: &str,
            owner_rights: bool,
        ) -> io::Result<i32> {
            let granted = prepare_root(root, false)?;
            let secret = granted.join("secret.env");

            let mut container = Container::new(tag)?;
            container.grant_read_exec(child, false)?;
            container.grant_read_exec(&granted, true)?;
            std::fs::write(&secret, b"DENY_PROBE_SECRET=leak")?;
            container.deny_read(&secret)?;
            if owner_rights {
                // Non-zero mask is load-bearing: SetNamedSecurityInfoW silently DROPS a
                // zero-mask ACE, which would leave the implicit owner grant intact and read
                // as a false "OWNER_RIGHTS does not help".
                let sid = SidString::new(OWNER_RIGHTS_SID)?;
                set_ace(&secret, sid.0, READ_CONTROL, GRANT_ACCESS, false)?;
            }
            println!("  [{tag}] secret.env {}", dump_dacl(&secret));
            Ok(container.launch(child, &["__resid__", "dacl", &native(&secret)], false))
        }

        /// Create `root` with a PROTECTED DACL (so nothing leaks in from %TEMP%), optionally
        /// add an INHERITABLE `ALL APPLICATION PACKAGES` read allow, then create the granted
        /// subtree beneath it so the subtree and its files inherit whatever the root carries.
        /// The AAP ACE is added EXPLICITLY rather than relying on %TEMP% happening to carry
        /// one — that host-dependence is what made RT-B's result unreproducible.
        fn prepare_root(root: &Path, aap: bool) -> io::Result<PathBuf> {
            std::fs::create_dir_all(root)?;
            let user = std::env::var("USERNAME").map_err(io::Error::other)?;
            icacls(&[
                &root.to_string_lossy(),
                "/inheritance:r",
                "/grant:r",
                &format!("{user}:(OI)(CI)F"),
            ])?;
            if aap {
                icacls(&[
                    &root.to_string_lossy(),
                    "/grant",
                    &format!("*{ALL_APPLICATION_PACKAGES_SID}:(OI)(CI)RX"),
                ])?;
            }
            let granted = root.join("granted");
            std::fs::create_dir_all(&granted)?;
            Ok(granted)
        }

        fn report(fails: &mut u32, tag: &str, outcome: &Outcome, lpac: bool) {
            let deny = match outcome.deny_held {
                Some(true) => "held",
                Some(false) => "defeated",
                None => "inconclusive",
            };
            let control = if outcome.control_ok { "ok" } else { "broken" };
            println!("PROBE {tag} deny={deny} control={control}");
            if outcome.control_ok && outcome.deny_held.is_some() {
                return;
            }
            // An LPAC child can legitimately fail to start (it loses ALL APPLICATION
            // PACKAGES, so it depends on system binaries granting ALL RESTRICTED
            // APPLICATION PACKAGES); report it, don't fail the suite. A non-LPAC cell that
            // cannot even read its control measured nothing — that IS a probe defect.
            if lpac {
                println!("INFO {tag}: no verdict from this cell (LPAC child did not run cleanly)");
            } else {
                *fails += 1;
                eprintln!("FAIL {tag}: control read failed — the cell measured nothing");
            }
        }

        // ── ACL plumbing ──────────────────────────────────────────────────────────

        /// Add/remove one ACE for `sid` on `path`, merging into the existing DACL.
        /// `SetEntriesInAclW` canonicalizes, so the resulting order (explicit deny, explicit
        /// allow, inherited deny, inherited allow) is Windows's own — never hand-built here.
        fn set_ace(
            path: &Path,
            sid: PSID,
            access: u32,
            mode: i32,
            inherit: bool,
        ) -> io::Result<()> {
            let wpath = to_wide_path(path);
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
            let sd_guard = LocalFreeGuard(sd);

            let mut ea: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
            ea.grfAccessPermissions = access;
            ea.grfAccessMode = mode;
            ea.grfInheritance = if inherit {
                CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE
            } else {
                NO_INHERITANCE
            };
            ea.Trustee = TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: sid.cast(),
            };

            let mut new_dacl: *mut ACL = std::ptr::null_mut();
            let rc = unsafe { SetEntriesInAclW(1, &ea, old_dacl, &mut new_dacl) };
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc as i32));
            }
            let new_guard = LocalFreeGuard(new_dacl.cast());
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
            drop(new_guard);
            drop(sd_guard);
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc as i32));
            }
            Ok(())
        }

        /// The owner plus every ACE in DACL order, as one log line. This is the evidence
        /// that the ACEs actually landed and in which order — a silently-dropped or
        /// out-of-order ACE would otherwise produce a confident wrong answer.
        fn dump_dacl(path: &Path) -> String {
            let wpath = to_wide_path(path);
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut owner: PSID = std::ptr::null_mut();
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let rc = unsafe {
                GetNamedSecurityInfoW(
                    wpath.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION,
                    &mut owner,
                    std::ptr::null_mut(),
                    &mut dacl,
                    std::ptr::null_mut(),
                    &mut sd,
                )
            };
            if rc != 0 {
                return format!("<GetNamedSecurityInfoW failed rc={rc}>");
            }
            let _sd = LocalFreeGuard(sd);
            let mut out = format!("owner={}", sid_to_string(owner));
            if dacl.is_null() {
                out.push_str(" DACL=<null — grants everyone>");
                return out;
            }
            let count = unsafe { (*dacl).AceCount };
            out.push_str(&format!(" DACL[{count}]:"));
            for i in 0..u32::from(count) {
                let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
                if unsafe { GetAce(dacl, i, &mut ace) } == 0 {
                    out.push_str(&format!(" [{i}]<GetAce failed>"));
                    continue;
                }
                let (ty, flags) = unsafe {
                    (
                        (*ace.cast::<ACE_HEADER>()).AceType,
                        (*ace.cast::<ACE_HEADER>()).AceFlags,
                    )
                };
                let kind = match ty {
                    ACE_TYPE_ALLOWED => "ALLOW",
                    ACE_TYPE_DENIED => "DENY",
                    other => {
                        out.push_str(&format!(" [{i}]type={other}"));
                        continue;
                    }
                };
                // ACCESS_ALLOWED_ACE and ACCESS_DENIED_ACE share this layout (header, Mask,
                // SidStart), so one cast reads either.
                let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
                let mask = unsafe { (*ace).Mask };
                let sid = unsafe { std::ptr::addr_of!((*ace).SidStart) } as PSID;
                let origin = if flags & INHERITED_ACE != 0 {
                    "inherited"
                } else {
                    "explicit"
                };
                out.push_str(&format!(
                    " [{i}]{kind}/{origin} mask=0x{mask:08x} sid={}",
                    sid_to_string(sid)
                ));
            }
            out
        }

        fn sid_to_string(sid: PSID) -> String {
            if sid.is_null() {
                return "<null>".into();
            }
            let mut raw: *mut u16 = std::ptr::null_mut();
            if unsafe { ConvertSidToStringSidW(sid, &mut raw) } == 0 || raw.is_null() {
                return "<unprintable>".into();
            }
            let mut len = 0usize;
            while unsafe { *raw.add(len) } != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(raw, len) });
            unsafe { LocalFree(raw.cast()) };
            text
        }

        /// A SID string converted to a PSID, LocalFree'd on drop.
        struct SidString(PSID);
        impl SidString {
            fn new(text: &str) -> io::Result<Self> {
                let wide = to_wide(text);
                let mut sid: PSID = std::ptr::null_mut();
                if unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(SidString(sid))
            }
        }
        impl Drop for SidString {
            fn drop(&mut self) {
                unsafe { LocalFree(self.0.cast()) };
            }
        }

        struct LocalFreeGuard(*mut std::ffi::c_void);
        impl Drop for LocalFreeGuard {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    unsafe { LocalFree(self.0) };
                }
            }
        }

        // ── launch plumbing ───────────────────────────────────────────────────────

        struct AttrList {
            buf: Vec<usize>,
        }
        impl AttrList {
            fn new(count: u32) -> io::Result<Self> {
                let mut size = 0usize;
                unsafe {
                    InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut size)
                };
                let mut buf = vec![0usize; size.div_ceil(std::mem::size_of::<usize>()).max(1)];
                let ok = unsafe {
                    InitializeProcThreadAttributeList(buf.as_mut_ptr().cast(), count, 0, &mut size)
                };
                if ok == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Self { buf })
            }
            fn update(
                &mut self,
                attr: usize,
                value: *mut std::ffi::c_void,
                size: usize,
            ) -> io::Result<()> {
                let ok = unsafe {
                    UpdateProcThreadAttribute(
                        self.buf.as_mut_ptr().cast(),
                        0,
                        attr,
                        value,
                        size,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
            fn as_ptr(&mut self) -> *mut std::ffi::c_void {
                self.buf.as_mut_ptr().cast()
            }
        }
        impl Drop for AttrList {
            fn drop(&mut self) {
                unsafe { DeleteProcThreadAttributeList(self.buf.as_mut_ptr().cast()) };
            }
        }

        /// Std handles to hand the child, each marked inheritable (a HANDLE_LIST member that
        /// is not inheritable fails the whole spawn). Without this the child can start with
        /// broken stdio and die during runtime init, which would read as a false denial.
        fn inheritable_std_handles() -> Vec<HANDLE> {
            let mut out: Vec<HANDLE> = Vec::new();
            for raw in [
                std::io::stdin().as_raw_handle(),
                std::io::stdout().as_raw_handle(),
                std::io::stderr().as_raw_handle(),
            ] {
                let h: HANDLE = raw.cast();
                if h.is_null() || h == INVALID_HANDLE_VALUE {
                    continue;
                }
                let marked =
                    unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
                if marked != 0 && !out.contains(&h) {
                    out.push(h);
                }
            }
            out
        }

        /// Fixture paths carry no quotes and never end in a backslash, so plain `"…"`
        /// wrapping round-trips through CommandLineToArgvW.
        fn command_line(program: &Path, args: &[&str]) -> Vec<u16> {
            let mut line: Vec<u16> = Vec::new();
            line.push(u16::from(b'"'));
            line.extend(program.as_os_str().encode_wide());
            line.push(u16::from(b'"'));
            for a in args {
                line.push(u16::from(b' '));
                line.push(u16::from(b'"'));
                line.extend(a.encode_utf16());
                line.push(u16::from(b'"'));
            }
            line.push(0);
            line
        }

        fn icacls(args: &[&str]) -> io::Result<()> {
            let status = std::process::Command::new("icacls").args(args).status()?;
            if status.success() {
                Ok(())
            } else {
                Err(io::Error::other(format!(
                    "icacls {args:?} failed: {status}"
                )))
            }
        }

        fn native(p: &Path) -> String {
            p.to_string_lossy().into_owned()
        }

        fn to_wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        }

        fn to_wide_path(p: &Path) -> Vec<u16> {
            to_wide(&p.to_string_lossy().replace('/', "\\"))
        }
    }

    /// Surrogate-spawn escape class — does the per-run AppContainer already close what a
    /// dedicated local-user account would? The one thing an account is documented to buy on
    /// top of the DACL work is closing SURROGATE SPAWN: a confined process asking a service
    /// (Task Scheduler, BITS, WMI) or a re-parent to spawn a reader under an UNCONFINED
    /// token. nub mints an unelevated per-run AppContainer SID, so if AppContainer blocks
    /// these, the account's remaining justification evaporates.
    ///
    /// THE ASSERTION THAT MATTERS is NOT whether the spawn call succeeds — it is whether the
    /// spawned process READS the policy-denied secret. A spawn that succeeds but lands
    /// still-confined (AppContainer token inherited) is a PASS. `spawn` and `read` are
    /// reported distinctly so those two are never conflated. Each vector carries a POSITIVE
    /// CONTROL: the same mechanism from an UNCONFINED process must leak, or the vector is an
    /// environment limit (schtasks/BITS/WMI are often restricted or absent on a CI runner),
    /// reported as `control=unavailable`, not a false "blocked".
    mod surrogate {
        use std::path::Path;
        use std::time::{Duration, Instant};
        use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Threading::{
            CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
            GetExitCodeProcess, InitializeProcThreadAttributeList, OpenProcess,
            PROCESS_INFORMATION, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
        };

        // PROCESS_CREATE_PROCESS: the right a re-parent needs on the target — the exact right
        // an AppContainer token is not expected to obtain on a non-AppContainer process.
        const PROCESS_CREATE_PROCESS: u32 = 0x0080;
        // ProcThreadAttributeValue(ProcThreadAttributeParentProcess = 0, thread = 0,
        // input = 1, additive = 0). Not exported by windows-sys 0.61.
        const ATTR_PARENT_PROCESS: usize = 0x0002_0000;
        // The four vectors × two runs each can each wait out a poll; keep the per-poll bound
        // tight so a fully-unavailable image finishes the group in well under two minutes.
        const POLL: Duration = Duration::from_secs(10);

        #[derive(Clone, Copy, PartialEq)]
        pub(super) enum Spawn {
            Ok,
            Denied,
            Unavailable,
        }
        impl Spawn {
            pub(super) fn to_exit_code(self) -> i32 {
                match self {
                    Spawn::Ok => 0,
                    Spawn::Denied => 5,
                    Spawn::Unavailable => 7,
                }
            }
            fn from_exit_code(code: i32) -> Self {
                match code {
                    0 => Spawn::Ok,
                    7 => Spawn::Unavailable,
                    _ => Spawn::Denied,
                }
            }
        }

        /// The reader body: read `secret`, write `LEAKED`/`DENIED` to `marker`. Writing only
        /// on a NON-EMPTY match avoids a false leak from an empty/other file.
        pub(super) fn read_role(secret: &str, marker: &str) -> i32 {
            let verdict = match std::fs::read(secret) {
                Ok(b) if !b.is_empty() => "LEAKED",
                Ok(_) => "DENIED",
                Err(_) => "DENIED",
            };
            let _ = std::fs::write(marker, verdict);
            i32::from(verdict == "LEAKED")
        }

        /// Trigger one vector to spawn the reader. Returns only the SPAWN outcome; the read
        /// outcome is the marker the runner polls. `batch` is a pre-written launcher the tool
        /// vectors point at (uniform quoting); `reader`/`secret`/`marker` drive the re-parent
        /// vector's direct CreateProcess. `anchor` is the unconfined process the re-parent
        /// targets (0 ⇒ absent).
        pub(super) fn attempt(
            vector: &str,
            reader: &Path,
            secret: &Path,
            marker: &Path,
            batch: &Path,
            anchor: u32,
        ) -> Spawn {
            match vector {
                "schtasks" => schtasks(batch),
                "reparent" => reparent(reader, secret, marker, anchor),
                "bits" => bits(batch),
                "wmi" => wmi(batch),
                _ => Spawn::Unavailable,
            }
        }

        // ── the matrix ──────────────────────────────────────────────────────────────

        pub(super) fn matrix(fails: &mut u32, child: &Path, base: &Path) {
            // Give the secret dir a PROTECTED DACL (strip %TEMP%'s inherited ACEs, grant only
            // the user) so the per-vector secret is genuinely policy-denied to a CONFINED
            // reader — otherwise an inherited AAP grant would let a still-confined reader read
            // it directly and register as a FALSE breach. Unconfined readers (control / a real
            // escape) run as the user and read it, which is exactly the leak under test.
            super::secure_root(base);
            for vector in ["schtasks", "reparent", "bits", "wmi"] {
                run_vector(fails, vector, child, base);
            }
        }

        fn run_vector(fails: &mut u32, vector: &str, child: &Path, base: &Path) {
            // Re-parent needs a live unconfined process to target; the runner spawns one and
            // keeps it alive for both the control and the confined attempt.
            let mut anchor_proc = (vector == "reparent")
                .then(|| {
                    std::process::Command::new(child)
                        .args(["__resid__", "sleep"])
                        .spawn()
                        .ok()
                })
                .flatten();
            let anchor = anchor_proc.as_ref().map_or(0, |c| c.id());

            // POSITIVE CONTROL: same mechanism, UNCONFINED. Must leak, or the vector is an
            // environment limit on this image (reported, not counted as a block).
            let (cbatch, cmarker) = stage(base, vector, "control", child);
            let control_spawn = attempt(
                vector,
                child,
                &secret_of(base, vector),
                &cmarker,
                &cbatch,
                anchor,
            );
            let control_leaked =
                control_spawn != Spawn::Unavailable && poll_marker(&cmarker, POLL) == Read::Leaked;

            // CONFINED ATTEMPT: run the vector from inside the AppContainer.
            let (fbatch, fmarker) = stage(base, vector, "confined", child);
            let secret = secret_of(base, vector);
            let confine = super::read_confine(&[child], &[]);
            let spawn_code = super::code(
                &confine,
                child,
                &[
                    "__resid__",
                    "surrogate",
                    vector,
                    &super::native(child),
                    &super::native(&secret),
                    &super::native(&fmarker),
                    &super::native(&fbatch),
                    &anchor.to_string(),
                ],
            );
            let spawn = Spawn::from_exit_code(spawn_code);
            let read = poll_marker(&fmarker, POLL);

            if let Some(mut c) = anchor_proc.take() {
                let _ = c.kill();
                let _ = c.wait();
            }

            let spawn_s = match spawn {
                Spawn::Ok => "ok",
                Spawn::Denied | Spawn::Unavailable => "denied",
            };
            let read_s = if read == Read::Leaked {
                "leaked"
            } else {
                "denied"
            };
            let control_s = if control_leaked { "ok" } else { "unavailable" };
            println!("PROBE surrogate-{vector} spawn={spawn_s} read={read_s} control={control_s}");

            // A CONFIRMED breach — the mechanism works (control leaked) AND the confined
            // attempt also read the secret — is the finding that would make the account
            // load-bearing; fail loudly. control=unavailable means the vector proved nothing
            // on this image (environment limit), so it is never counted against nub.
            if control_leaked && read == Read::Leaked {
                *fails += 1;
                eprintln!(
                    "FAIL surrogate-{vector}: confined process READ the denied secret via \
                     surrogate spawn (AppContainer did not close this vector)"
                );
            } else if !control_leaked {
                println!(
                    "INFO surrogate-{vector}: mechanism unavailable unconfined on this image \
                     — no verdict for nub (environment limit, not a block)"
                );
            }
        }

        /// Pre-write the launcher batch + return (batch, marker) for one (vector, phase). The
        /// runner is unconfined, so it authors these freely; the reader they invoke is what
        /// may or may not be confined.
        fn stage(
            base: &Path,
            vector: &str,
            phase: &str,
            reader: &Path,
        ) -> (std::path::PathBuf, std::path::PathBuf) {
            let marker = base.join(format!("{vector}-{phase}.marker"));
            let batch = base.join(format!("{vector}-{phase}.cmd"));
            let secret = secret_of(base, vector);
            let _ = std::fs::write(&secret, b"SURROGATE_SECRET=leak");
            let body = format!(
                "@echo off\r\n\"{}\" surrogate-read \"{}\" \"{}\"\r\n",
                reader.display(),
                secret.display(),
                marker.display()
            );
            let _ = std::fs::write(&batch, body);
            (batch, marker)
        }

        fn secret_of(base: &Path, vector: &str) -> std::path::PathBuf {
            base.join(format!("{vector}.secret"))
        }

        #[derive(PartialEq)]
        enum Read {
            Leaked,
            Denied,
        }

        /// Poll the marker until it says LEAKED, or the timeout elapses. Absent-or-DENIED ⇒
        /// no leak observed (a PASS); only an explicit LEAKED marker is a breach.
        fn poll_marker(marker: &Path, timeout: Duration) -> Read {
            let deadline = Instant::now() + timeout;
            loop {
                if let Ok(s) = std::fs::read_to_string(marker) {
                    if s.starts_with("LEAKED") {
                        return Read::Leaked;
                    }
                    if s.starts_with("DENIED") {
                        return Read::Denied;
                    }
                }
                if Instant::now() >= deadline {
                    return Read::Denied;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }

        // ── the vectors ───────────────────────────────────────────────────────────

        /// Vector 1: Task Scheduler. Register a one-shot task whose action is the launcher,
        /// then run it on demand. No `/ru`+password (would prompt/hang) — the task runs as
        /// the registering principal; whether the SERVICE is reachable from the AppContainer
        /// is the test. Availability = whether create+run return success.
        fn schtasks(batch: &Path) -> Spawn {
            let name = format!("nub_surro_{}", std::process::id());
            let created = run_status(
                "schtasks",
                &[
                    "/create",
                    "/tn",
                    &name,
                    "/sc",
                    "once",
                    "/st",
                    "23:59",
                    "/tr",
                    &batch.to_string_lossy(),
                    "/f",
                ],
            );
            match created {
                None => return Spawn::Unavailable,
                Some(false) => return Spawn::Denied,
                Some(true) => {}
            }
            let ran = run_status("schtasks", &["/run", "/tn", &name]);
            let _ = run_status("schtasks", &["/delete", "/tn", &name, "/f"]);
            match ran {
                Some(true) => Spawn::Ok,
                Some(false) => Spawn::Denied,
                None => Spawn::Unavailable,
            }
        }

        /// Vector 3: BITS. A transfer job with a notify command line that fires the launcher
        /// on completion. bitsadmin is deprecated and frequently absent — a missing binary is
        /// `Unavailable`, not a block.
        fn bits(batch: &Path) -> Spawn {
            let job = format!("nub_surro_{}", std::process::id());
            if run_status("bitsadmin", &["/create", &job]).is_none() {
                return Spawn::Unavailable;
            }
            // A trivial local transfer so the job can COMPLETE and fire the notify command.
            let src = batch.with_extension("src");
            let dst = batch.with_extension("dst");
            let _ = std::fs::write(&src, b"x");
            let _ = std::fs::remove_file(&dst);
            let notify = format!("cmd /c \"{}\"", batch.display());
            run_status(
                "bitsadmin",
                &[
                    "/addfile",
                    &job,
                    &src.to_string_lossy(),
                    &dst.to_string_lossy(),
                ],
            );
            run_status(
                "bitsadmin",
                &["/setnotifycmdline", &job, "cmd.exe", &notify],
            );
            let resumed = run_status("bitsadmin", &["/resume", &job]);
            // Give BITS a moment; the notify fires on completion. Complete explicitly so the
            // command runs promptly rather than on the service's own schedule.
            std::thread::sleep(Duration::from_millis(500));
            let _ = run_status("bitsadmin", &["/complete", &job]);
            match resumed {
                Some(true) => Spawn::Ok,
                Some(false) => Spawn::Denied,
                None => Spawn::Unavailable,
            }
        }

        /// Vector 4: WMI Win32_Process.Create via PowerShell's CIM cmdlet (wmic is removed on
        /// current images). ReturnValue 0 ⇒ the provider spawned the process. Whether winmgmt
        /// is reachable from the AppContainer is the test; no PowerShell ⇒ Unavailable.
        fn wmi(batch: &Path) -> Spawn {
            let cmdline = format!("cmd /c \"{}\"", batch.display());
            let script = format!(
                "$r=Invoke-CimMethod -ClassName Win32_Process -MethodName Create \
                 -Arguments @{{CommandLine='{}'}}; exit $r.ReturnValue",
                cmdline.replace('\'', "''")
            );
            match run_status(
                "powershell",
                &["-NoProfile", "-NonInteractive", "-Command", &script],
            ) {
                Some(true) => Spawn::Ok,
                Some(false) => Spawn::Denied,
                None => Spawn::Unavailable,
            }
        }

        /// Vector 2: PROC_THREAD_ATTRIBUTE_PARENT_PROCESS re-parent onto an unconfined
        /// process. The new process inherits the PARENT's token — so success means the reader
        /// runs unconfined. The gate is `OpenProcess(PROCESS_CREATE_PROCESS)` on the target: an
        /// AppContainer token is not expected to obtain it on a non-AppContainer process, so a
        /// null handle is the block. Unconfined (the control) it succeeds and leaks.
        fn reparent(reader: &Path, secret: &Path, marker: &Path, anchor: u32) -> Spawn {
            if anchor == 0 {
                return Spawn::Unavailable;
            }
            // SAFETY: OpenProcess with a bounded access mask; a null return is the denial.
            let target = unsafe { OpenProcess(PROCESS_CREATE_PROCESS, FALSE, anchor) };
            if target.is_null() {
                return Spawn::Denied;
            }
            let _target = HandleGuard(target);

            let mut size = 0usize;
            unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
            let mut buf = vec![0usize; size.div_ceil(std::mem::size_of::<usize>()).max(1)];
            if unsafe {
                InitializeProcThreadAttributeList(buf.as_mut_ptr().cast(), 1, 0, &mut size)
            } == 0
            {
                return Spawn::Denied;
            }
            let _attr = AttrGuard(buf.as_mut_ptr().cast());
            let mut parent = target;
            if unsafe {
                UpdateProcThreadAttribute(
                    buf.as_mut_ptr().cast(),
                    0,
                    ATTR_PARENT_PROCESS,
                    std::ptr::from_mut(&mut parent).cast(),
                    std::mem::size_of::<HANDLE>(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Spawn::Denied;
            }

            let mut cmdline = reader_command_line(reader, secret, marker);
            let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
            si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            si.lpAttributeList = buf.as_mut_ptr().cast();
            let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: cmdline/buf/parent all outlive the call; writable UTF-16 command line.
            let ok = unsafe {
                CreateProcessW(
                    std::ptr::null(),
                    cmdline.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    FALSE,
                    EXTENDED_STARTUPINFO_PRESENT,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::from_mut(&mut si).cast(),
                    &mut pi,
                )
            };
            if ok == 0 {
                return Spawn::Denied;
            }
            // Let the reader finish so the marker is written before the runner polls.
            unsafe {
                WaitForSingleObject(pi.hProcess, POLL.as_millis() as u32);
                let mut code = 0u32;
                GetExitCodeProcess(pi.hProcess, &mut code);
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
            }
            Spawn::Ok
        }

        fn reader_command_line(reader: &Path, secret: &Path, marker: &Path) -> Vec<u16> {
            use std::os::windows::ffi::OsStrExt;
            let mut line: Vec<u16> = Vec::new();
            let quoted = |s: &std::ffi::OsStr, line: &mut Vec<u16>| {
                line.push(u16::from(b'"'));
                line.extend(s.encode_wide());
                line.push(u16::from(b'"'));
            };
            quoted(reader.as_os_str(), &mut line);
            line.push(u16::from(b' '));
            line.extend("surrogate-read".encode_utf16());
            line.push(u16::from(b' '));
            quoted(secret.as_os_str(), &mut line);
            line.push(u16::from(b' '));
            quoted(marker.as_os_str(), &mut line);
            line.push(0);
            line
        }

        /// Run a tool, mapping to Some(success) / Some(failure) / None(not launchable). A
        /// spawn error (missing binary) is None ⇒ Unavailable, never a false Denied.
        fn run_status(program: &str, args: &[&str]) -> Option<bool> {
            std::process::Command::new(program)
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .ok()
                .map(|s| s.success())
        }

        struct HandleGuard(HANDLE);
        impl Drop for HandleGuard {
            fn drop(&mut self) {
                if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                    unsafe { CloseHandle(self.0) };
                }
            }
        }
        struct AttrGuard(*mut std::ffi::c_void);
        impl Drop for AttrGuard {
            fn drop(&mut self) {
                unsafe { DeleteProcThreadAttributeList(self.0) };
            }
        }
    }
}

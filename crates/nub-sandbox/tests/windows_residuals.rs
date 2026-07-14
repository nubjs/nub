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
            _ => 2,
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
        let spec = CommandSpec::new(program.as_os_str()).args(args.iter().copied());
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

        let _ = std::fs::remove_dir_all(&root);
        if fails == 0 { Ok(()) } else { Err(fails) }
    }

    #[allow(dead_code)]
    fn _unused(_: PathBuf) {}
}

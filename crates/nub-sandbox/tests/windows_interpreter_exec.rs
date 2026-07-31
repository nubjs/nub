//! Windows: what does an AppContainer child actually refuse?
//!
//! THE DEFECT THIS RESOLVED. With the verbatim command line landed, a confined `cmd.exe`
//! reached `node` and nub's shim then died spawning the real binary:
//!
//! ```text
//! failed to detect Node version:
//!   "C:\hostedtoolcache\windows\node\22.23.1\x64\node.exe": Access is denied. (os error 5)
//! ```
//!
//! It reads as an execute denial on the interpreter, and it is not. `ERROR_ACCESS_DENIED`
//! surfaces at the `Command` API for every object a spawn touches, and a captured-output
//! spawn touches several before `CreateProcessW` runs at all. So each object is measured
//! here on its own rather than inferred from a spawn that used all of them at once:
//!
//! | measured | verdict |
//! | --- | --- |
//! | read the `ALL APPLICATION PACKAGES`-granted System32 image | permitted |
//! | read the interpreter nub granted by ACE | permitted |
//! | `CreateProcessW`, zeroed STARTUPINFOW, no inherited handles | permitted |
//! | `Command::status()` with stdio inherited | permitted |
//! | `CreatePipe`, and a spawn capturing stdout through one | permitted |
//! | open the `NUL` device, for read or for write | **REFUSED** |
//! | `CreateNamedPipeW` under `\\.\pipe\` | **REFUSED** |
//!
//! `Command::output()` redirects stdin to `Stdio::null()`, which opens `NUL` — so nub's own
//! Node-version detection failed before the interpreter was ever opened, and reported it
//! against the interpreter's path. The grant was correct the whole time, which
//! `production_grant_shape` prints from the real `compile_build_jail` policy.
//!
//! Every property is read off a marker file the CHILD wrote, never off a status the harness
//! reported about itself, and every path the child touches is baked in as an absolute
//! literal argument. `interpreter-ungranted-refused` is the both-directions control: the
//! same fixture and the same inherited-stdio spawn, with the interpreter grant removed, must
//! be REFUSED — so the probe cannot go green by being permissive.
//!
//! CI IS THE ONLY VENUE. AppContainer cannot be launched over SSH (session 0 has no window
//! station; every launch returns 0xC0000142). Runs branch-scoped via
//! `.github/workflows/win-interp-exec-probe.yml`, no pull request.

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

    /// Classify a spawn failure into the probe's exit contract: 5 access-denied, 9 other.
    fn spawn_err(marker: &Path, e: &std::io::Error) -> i32 {
        let _ = std::fs::write(marker, format!("spawnerr {e:?} raw={:?}", e.raw_os_error()));
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            5
        } else {
            9
        }
    }

    /// Child modes. `exec` and `execstatus` run the SAME spawn against the SAME image and
    /// differ only in whether stdio is captured — which on Windows decides whether Rust
    /// allocates an anonymous pipe. The marker is written on BOTH outcomes, so a refusal is
    /// evidence rather than an absence.
    pub fn child_main(a: &[String]) -> i32 {
        match a.first().map(String::as_str) {
            // exec <marker> <exe> [args…] — captured stdio (`Command::output`), the shape
            // nub's own Node-version detection uses.
            Some("exec") => {
                let marker = Path::new(&a[1]);
                let exe = &a[2];
                let out = std::process::Command::new(exe).args(&a[3..]).output();
                match out {
                    Ok(o) if o.status.success() => {
                        let text = String::from_utf8_lossy(&o.stdout);
                        let _ = std::fs::write(marker, format!("ok {}", text.trim()));
                        0
                    }
                    Ok(o) => {
                        let _ = std::fs::write(marker, format!("ranfail status={:?}", o.status));
                        9
                    }
                    Err(e) => spawn_err(marker, &e),
                }
            }
            // execstatus <marker> <exe> [args…] — identical spawn with stdio pointed at NUL,
            // so no pipe is created. The ONE variable between this and `exec`.
            Some("execstatus") => {
                let marker = Path::new(&a[1]);
                let exe = &a[2];
                let out = std::process::Command::new(exe)
                    .args(&a[3..])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                match out {
                    Ok(s) if s.success() => {
                        let _ = std::fs::write(marker, "ok status0");
                        0
                    }
                    Ok(s) => {
                        let _ = std::fs::write(marker, format!("ranfail status={s:?}"));
                        9
                    }
                    Err(e) => spawn_err(marker, &e),
                }
            }
            // execinherit <marker> <exe> [args…] — plain `status()`, stdio INHERITED. Creates
            // no new kernel object before `CreateProcessW`, so it is the spawn on its own:
            // `execstatus` still opens `NUL` three times, which is itself a securable object.
            Some("execinherit") => {
                let marker = Path::new(&a[1]);
                let exe = &a[2];
                match std::process::Command::new(exe).args(&a[3..]).status() {
                    Ok(s) if s.success() => {
                        let _ = std::fs::write(marker, "ok status0");
                        0
                    }
                    Ok(s) => {
                        let _ = std::fs::write(marker, format!("ranfail status={s:?}"));
                        9
                    }
                    Err(e) => spawn_err(marker, &e),
                }
            }
            // rawspawn <marker> <command-line> — `CreateProcessW` with a zeroed STARTUPINFOW
            // and no handle inheritance, so Rust's spawn machinery is out of the picture
            // entirely and the recorded error is the OS's own.
            Some("rawspawn") => {
                use windows_sys::Win32::System::Threading::{
                    CreateProcessW, GetExitCodeProcess, PROCESS_INFORMATION, STARTUPINFOW,
                    WaitForSingleObject,
                };
                let marker = Path::new(&a[1]);
                let mut cmdline: Vec<u16> = a[2].encode_utf16().chain(std::iter::once(0)).collect();
                let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
                si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
                let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
                let ok = unsafe {
                    CreateProcessW(
                        std::ptr::null(),
                        cmdline.as_mut_ptr(),
                        std::ptr::null(),
                        std::ptr::null(),
                        0,
                        0,
                        std::ptr::null(),
                        std::ptr::null(),
                        &si,
                        &mut pi,
                    )
                };
                if ok == 0 {
                    let e = std::io::Error::last_os_error();
                    let _ = std::fs::write(
                        marker,
                        format!("rawspawnerr {e:?} raw={:?}", e.raw_os_error()),
                    );
                    return 5;
                }
                let mut code: u32 = 0;
                unsafe {
                    WaitForSingleObject(pi.hProcess, 0xFFFF_FFFF);
                    GetExitCodeProcess(pi.hProcess, &mut code);
                    windows_sys::Win32::Foundation::CloseHandle(pi.hThread);
                    windows_sys::Win32::Foundation::CloseHandle(pi.hProcess);
                }
                let _ = std::fs::write(marker, format!("ok rawspawn code={code}"));
                0
            }
            // readfile <marker> <path> — can the child READ the image at all? Separates "the
            // token never gets the rights the DACL grants" from "the rights are there and
            // process creation is refused for another reason".
            Some("readfile") => {
                let marker = Path::new(&a[1]);
                match std::fs::File::open(&a[2]) {
                    Ok(mut file) => {
                        use std::io::Read;
                        let mut buf = [0u8; 2];
                        match file.read(&mut buf) {
                            Ok(n) => {
                                let _ = std::fs::write(marker, format!("ok read {n} {buf:?}"));
                                0
                            }
                            Err(e) => {
                                let _ = std::fs::write(marker, format!("readerr {e:?}"));
                                9
                            }
                        }
                    }
                    Err(e) => {
                        let _ = std::fs::write(
                            marker,
                            format!("openerr {e:?} raw={:?}", e.raw_os_error()),
                        );
                        if e.kind() == std::io::ErrorKind::PermissionDenied {
                            5
                        } else {
                            9
                        }
                    }
                }
            }
            // opennul <marker> — `Stdio::null()` opens the `NUL` device. It is a securable
            // object like any other, so a spawn that redirects to it can fail for a reason
            // that has nothing to do with the image.
            Some("opennul") => {
                let marker = Path::new(&a[1]);
                let r = std::fs::File::open("NUL");
                let w = std::fs::OpenOptions::new().write(true).open("NUL");
                let text = format!(
                    "read={} write={}",
                    match &r {
                        Ok(_) => "ok".to_string(),
                        Err(e) => format!("{e:?}"),
                    },
                    match &w {
                        Ok(_) => "ok".to_string(),
                        Err(e) => format!("{e:?}"),
                    }
                );
                let _ = std::fs::write(marker, &text);
                i32::from(r.is_err() || w.is_err()) * 5
            }
            // anonpipe <marker> — `CreatePipe`, the classic anonymous pipe. It still lives in
            // NPFS (`\Device\NamedPipe\Win32Pipes.…`), so this says whether the whole device
            // is refused or only an explicitly named instance under `\\.\pipe\`.
            Some("anonpipe") => {
                use windows_sys::Win32::System::Pipes::CreatePipe;
                let marker = Path::new(&a[1]);
                let mut r: windows_sys::Win32::Foundation::HANDLE = std::ptr::null_mut();
                let mut w: windows_sys::Win32::Foundation::HANDLE = std::ptr::null_mut();
                let ok = unsafe { CreatePipe(&mut r, &mut w, std::ptr::null(), 0) };
                if ok == 0 {
                    let e = std::io::Error::last_os_error();
                    let _ = std::fs::write(
                        marker,
                        format!("anonpipeerr {e:?} raw={:?}", e.raw_os_error()),
                    );
                    5
                } else {
                    unsafe {
                        windows_sys::Win32::Foundation::CloseHandle(r);
                        windows_sys::Win32::Foundation::CloseHandle(w);
                    }
                    let _ = std::fs::write(marker, "ok anonpipe");
                    0
                }
            }
            // pipedstdout <marker> <exe> [args…] — capture only stdout, leaving stdin and
            // stderr inherited, so the pipe is the sole variable against `execinherit`.
            Some("pipedstdout") => {
                let marker = Path::new(&a[1]);
                let exe = &a[2];
                let spawned = std::process::Command::new(exe)
                    .args(&a[3..])
                    .stdout(std::process::Stdio::piped())
                    .spawn();
                match spawned {
                    Ok(child) => match child.wait_with_output() {
                        Ok(o) => {
                            let _ = std::fs::write(
                                marker,
                                format!("ok piped {}", String::from_utf8_lossy(&o.stdout).trim()),
                            );
                            0
                        }
                        Err(e) => {
                            let _ = std::fs::write(marker, format!("waiterr {e:?}"));
                            9
                        }
                    },
                    Err(e) => spawn_err(marker, &e),
                }
            }
            // namedpipe <marker> — an NPFS creation on its own, with no process creation in
            // the way at all.
            //
            // It was added on the premise that `Command::output`'s pipes are these pipes, so
            // that a refusal here would explain a refused captured-stdio spawn. That premise
            // is REFUTED: the sibling shape matrix ran a cell replicating std's exact flags
            // and name alongside a real `Stdio::piped()` spawn, in one arm, and the cell was
            // refused while the spawn succeeded and returned its child's bytes (run
            // 30473523088, `rust-exact-jail-net-deny` vs `rust-stdio-piped-jail-net-deny`).
            // Whatever std reaches for on this Windows, it is not the object measured below.
            // The cell is KEPT because the question it actually answers — can a confined
            // child create a global-namespace named pipe — is the one that decides Node's
            // piped `child_process` spawn, and the answer there is no.
            Some("namedpipe") => {
                use windows_sys::Win32::System::Pipes::CreateNamedPipeW;
                let name: Vec<u16> = format!(r"\\.\pipe\nub-interp-probe-{}", std::process::id())
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                // PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE, byte mode, one instance.
                let h = unsafe {
                    CreateNamedPipeW(
                        name.as_ptr(),
                        0x0000_0001 | 0x0008_0000,
                        0,
                        1,
                        4096,
                        4096,
                        0,
                        std::ptr::null(),
                    )
                };
                let marker = Path::new(&a[1]);
                if h.is_null() || h == (-1isize as *mut std::ffi::c_void) {
                    let e = std::io::Error::last_os_error();
                    let _ =
                        std::fs::write(marker, format!("pipeerr {e:?} raw={:?}", e.raw_os_error()));
                    5
                } else {
                    unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
                    let _ = std::fs::write(marker, "ok pipe-created");
                    0
                }
            }
            // acl <marker> <path> — the DACL as it stands WHILE the grant is live. The ACEs
            // are revoked when the launch drops, so the parent can never see this.
            Some("acl") => {
                let marker = Path::new(&a[1]);
                let icacls = PathBuf::from(std::env::var("SystemRoot").unwrap_or_default())
                    .join("System32")
                    .join("icacls.exe");
                // Redirected to a FILE, not a pipe: capturing stdio is the very thing under
                // test here, so using it would make this diagnostic fail for the reason it
                // exists to report on.
                let sink = match std::fs::File::create(marker) {
                    Ok(file) => file,
                    Err(e) => {
                        eprintln!("icacls sink: {e}");
                        return 9;
                    }
                };
                match std::process::Command::new(icacls)
                    .arg(&a[2])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::from(sink))
                    .stderr(std::process::Stdio::null())
                    .status()
                {
                    Ok(_) => 0,
                    Err(e) => {
                        let _ = std::fs::write(marker, format!("icacls spawnerr {e:?}"));
                        9
                    }
                }
            }
            _ => 2,
        }
    }

    struct Fixture {
        root: PathBuf,
        child: PathBuf,
        work: PathBuf,
    }

    /// PROTECTED DACL on the fixture root: inherited ACEs stripped, only the current user
    /// granted — otherwise an inherited `ALL APPLICATION PACKAGES` grant from `C:\Users`
    /// would satisfy the LowBox check before default-deny is reached and every arm would be
    /// measuring `%TEMP%`'s ACL rather than the backend's.
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
        fn new(tag: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!("nub-interp-{tag}-{nonce:x}"));
            std::fs::create_dir_all(&root).unwrap();
            secure_root(&root);
            let bin = root.join("bin");
            let work = root.join("work");
            for d in [&bin, &work] {
                std::fs::create_dir_all(d).unwrap();
            }
            let child = bin.join("child.exe");
            std::fs::copy(std::env::current_exe().unwrap(), &child).unwrap();
            Fixture { root, child, work }
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
    /// egress deny — the posture under test.
    fn jail_shaped(f: &Fixture, extra: Vec<FsRule>) -> SandboxPolicy {
        let mut entries = vec![
            FsRule {
                matcher: CanonGlob(canon(&f.work)),
                effect: Effect::Allow,
                access: FsAccess::ReadWrite,
                origin: FsOrigin::Authored,
            },
            // The child image itself: `apply` auto-grants the program FILE, but the probe
            // names it too so an arm's grant set is fully explicit.
            read_rule(&f.child),
        ];
        entries.extend(extra);
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
            env: EnvPolicy::resolved(os_essential_env()),
            pid: PidPolicy::default(),
            build_jail: true,
        }
    }

    /// `cwd` is a PARAMETER, not a constant, because it is itself a variable under test:
    /// `CreateProcessW` opens the CALLER's working directory to hand the child a handle to
    /// it, so a confined process whose own cwd is ungranted cannot spawn anything at all —
    /// and the failure is `ERROR_ACCESS_DENIED` on the SPAWN, indistinguishable at the
    /// `Command` API from the image being refused.
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

    /// The interpreter this probe is about: the `node.exe` the runner's PATH resolves, which
    /// on a GitHub-hosted Windows runner is the `C:\hostedtoolcache\…` image the production
    /// failure names. Overridable so the probe can be pointed at another install.
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

    /// `canonicalize` returns the extended-length `\\?\C:\…` form; the security APIs and the
    /// policy IR both want the ordinary one.
    fn unverbatim(p: &Path) -> PathBuf {
        match p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
            Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
            _ => p.to_path_buf(),
        }
    }

    /// Print the DACL of a path and of every ancestor. This is the "what does the ACL
    /// actually say" half of the diagnosis — in particular whether any ancestor grants
    /// `ALL APPLICATION PACKAGES`, and whether nub's own user can rewrite the leaf's DACL
    /// at all.
    fn dump_acl_chain(label: &str, leaf: &Path) {
        println!("---- icacls chain ({label}) ----");
        let mut chain: Vec<&Path> = leaf.ancestors().collect();
        chain.reverse();
        for p in chain {
            if p.as_os_str().is_empty() {
                continue;
            }
            let out = std::process::Command::new("icacls").arg(p).output();
            match out {
                Ok(o) => {
                    print!("{}", String::from_utf8_lossy(&o.stdout));
                    let err = String::from_utf8_lossy(&o.stderr);
                    if !err.trim().is_empty() {
                        print!("  (stderr) {err}");
                    }
                }
                Err(e) => println!("{} -> icacls failed: {e}", p.display()),
            }
        }
        println!("---- end icacls chain ({label}) ----");
    }

    /// Run one child arm in the GRANTED working directory and return
    /// `(exit_code, marker_text)`. `mode` selects the child entry point; `argv` is whatever
    /// that mode takes after the marker path.
    fn run_child(
        f: &Fixture,
        policy: &SandboxPolicy,
        mode: &str,
        marker: &Path,
        argv: &[String],
    ) -> (i32, String) {
        run_child_in(f, policy, &f.work.clone(), mode, marker, argv)
    }

    fn run_child_in(
        f: &Fixture,
        policy: &SandboxPolicy,
        cwd: &Path,
        mode: &str,
        marker: &Path,
        argv: &[String],
    ) -> (i32, String) {
        let mut args = vec![mode.to_string(), marker.to_string_lossy().into_owned()];
        args.extend(argv.iter().cloned());
        let outcome = apply(policy, spec(f, cwd, &args)).map(|p| p.status());
        let code = match outcome {
            Ok(Ok(status)) => status.code().unwrap_or(-1),
            Ok(Err(error)) => {
                println!("    launch failed: {error}");
                -2
            }
            Err(degradation) => {
                println!("    policy rejected: {degradation:?}");
                -3
            }
        };
        let text = std::fs::read_to_string(marker).unwrap_or_else(|_| "<no marker>".to_string());
        println!("    exit={code} marker={text}");
        (code, text)
    }

    pub fn probe_main() -> i32 {
        let mut fails = 0u32;
        println!("PROBE windows interpreter exec under AppContainer");

        let Some(node) = path_node() else {
            eprintln!("no node.exe on PATH — the probe cannot run");
            return 1;
        };
        // The canonical spelling is what the backend grants (`canonicalize_glob_prefix`
        // resolves 8.3 names and links), so the ACL dump and the grant must name the same
        // file or the comparison is meaningless.
        let node = std::fs::canonicalize(&node)
            .map(|p| unverbatim(&p))
            .unwrap_or(node);
        println!("interpreter: {}", node.display());
        dump_acl_chain("before any grant", &node);

        named_pipe_refused(&mut fails);
        stdio_shapes(&mut fails);
        read_before_exec(&mut fails, node.as_path());
        spawn_shapes(&mut fails);
        spawn_cwd_differential(&mut fails);
        system_exec_baseline(&mut fails);
        interpreter_in_place(&mut fails, node.as_path());
        interpreter_ungranted_refused(&mut fails, node.as_path());
        acl_under_jail(&node);
        production_grant_shape(&node);

        dump_acl_chain("after every launch dropped", &node);

        if fails == 0 {
            println!("WINDOWS APPCONTAINER EXECUTES THE GRANTED INTERPRETER");
            0
        } else {
            eprintln!("{fails} propert(y/ies) failed");
            1
        }
    }

    /// THE STDIO SHAPES, each on its own. A spawn that redirects stdio touches TWO securable
    /// objects before `CreateProcessW` ever runs — the `NUL` device and an NPFS pipe — and a
    /// refusal of either is reported at the `Command` API as the same `os error 5` an
    /// unreadable image gives. These arms measure each object directly, so the spawn arms
    /// below can be read as being about the spawn.
    fn stdio_shapes(fails: &mut u32) {
        let f = Fixture::new("stdio");
        let policy = jail_shaped(&f, Vec::new());
        let cmd = PathBuf::from(std::env::var("SystemRoot").unwrap_or_default())
            .join("System32")
            .join("cmd.exe");

        println!("  arm opens-the-nul-device:");
        let m = f.work.join("nul.txt");
        let (code, text) = run_child(&f, &policy, "opennul", &m, &[]);
        report(
            fails,
            "opens-the-nul-device-refused",
            code == 5,
            &format!("exit={code} marker={text} (5 ⇒ a LowBox child cannot open NUL)"),
        );

        println!("  arm creates-an-anonymous-pipe:");
        let m = f.work.join("anonpipe.txt");
        let (code, text) = run_child(&f, &policy, "anonpipe", &m, &[]);
        report(
            fails,
            "creates-an-anonymous-pipe",
            code == 0,
            &format!("exit={code} marker={text}"),
        );

        println!("  arm spawn-with-piped-stdout:");
        let m = f.work.join("pipedout.txt");
        let (code, text) = run_child(
            &f,
            &policy,
            "pipedstdout",
            &m,
            &[
                cmd.to_string_lossy().into_owned(),
                "/c".to_string(),
                "ver".to_string(),
            ],
        );
        report(
            fails,
            "spawn-with-piped-stdout",
            code == 0,
            &format!("exit={code} marker={text}"),
        );
    }

    /// READ before EXEC. Every spawn arm has come back `ERROR_ACCESS_DENIED`, which says
    /// nothing about WHICH object was refused. Reading the two images directly answers the
    /// prior question: whether the LowBox token receives the rights the DACL grants it at
    /// all — the System32 image through `ALL APPLICATION PACKAGES`, the interpreter through
    /// nub's own ACE. If both reads succeed, the rights are there and process creation is
    /// being refused for a reason that is not the image.
    fn read_before_exec(fails: &mut u32, node: &Path) {
        let f = Fixture::new("read");
        let mut extra = vec![read_rule(node)];
        if let Some(dir) = node.parent() {
            extra.push(read_rule(dir));
        }
        let policy = jail_shaped(&f, extra);
        let cmd = PathBuf::from(std::env::var("SystemRoot").unwrap_or_default())
            .join("System32")
            .join("cmd.exe");

        println!("  arm reads-the-aap-granted-system-image:");
        let m = f.work.join("read-sys.txt");
        let (code, text) = run_child(
            &f,
            &policy,
            "readfile",
            &m,
            &[cmd.to_string_lossy().into_owned()],
        );
        report(
            fails,
            "reads-the-aap-granted-system-image",
            code == 0,
            &format!("exit={code} marker={text}"),
        );

        println!("  arm reads-the-nub-granted-interpreter:");
        let m = f.work.join("read-node.txt");
        let (code, text) = run_child(
            &f,
            &policy,
            "readfile",
            &m,
            &[node.to_string_lossy().into_owned()],
        );
        report(
            fails,
            "reads-the-nub-granted-interpreter",
            code == 0,
            &format!("exit={code} marker={text}"),
        );
    }

    /// THE SPAWN SHAPE, isolated. Same image, same policy, same cwd; only how the spawn is
    /// issued changes. `execinherit` adds no kernel object before `CreateProcessW`;
    /// `rawspawn` removes Rust's spawn machinery altogether. If all three shapes are
    /// refused, `CreateProcessW` itself is what an AppContainer child cannot do here.
    fn spawn_shapes(fails: &mut u32) {
        let f = Fixture::new("shapes");
        let policy = jail_shaped(&f, Vec::new());
        let cmd = PathBuf::from(std::env::var("SystemRoot").unwrap_or_default())
            .join("System32")
            .join("cmd.exe");

        println!("  arm spawn-inherited-stdio:");
        let m = f.work.join("shape-inherit.txt");
        let (code, text) = run_child(
            &f,
            &policy,
            "execinherit",
            &m,
            &[
                cmd.to_string_lossy().into_owned(),
                "/c".to_string(),
                "ver".to_string(),
            ],
        );
        report(
            fails,
            "spawn-inherited-stdio",
            code == 0,
            &format!("exit={code} marker={text}"),
        );

        println!("  arm spawn-raw-createprocess:");
        let m = f.work.join("shape-raw.txt");
        let (code, text) = run_child(
            &f,
            &policy,
            "rawspawn",
            &m,
            &[format!("\"{}\" /c ver", cmd.display())],
        );
        report(
            fails,
            "spawn-raw-createprocess",
            code == 0,
            &format!("exit={code} marker={text}"),
        );
    }

    /// THE WORKING-DIRECTORY VARIABLE, isolated — and it turns out NOT to matter. The
    /// caller's own directory was a live suspect while every spawn was failing, because
    /// `CreateProcessW` hands the child a handle to it. Measured both ways with stdio
    /// inherited, a confined process spawns fine from a directory the policy never granted,
    /// so the cwd is not access-checked on the spawn path. Kept because "not a variable" is
    /// worth pinning: it was assumed twice.
    fn spawn_cwd_differential(fails: &mut u32) {
        let f = Fixture::new("cwd");
        let policy = jail_shaped(&f, Vec::new());
        let cmd = PathBuf::from(std::env::var("SystemRoot").unwrap_or_default())
            .join("System32")
            .join("cmd.exe");
        let argv = [
            cmd.to_string_lossy().into_owned(),
            "/c".to_string(),
            "ver".to_string(),
        ];

        println!("  arm spawn-from-granted-cwd:");
        let granted = f.work.join("cwd-granted.txt");
        let (code, text) =
            run_child_in(&f, &policy, &f.work.clone(), "execinherit", &granted, &argv);
        report(
            fails,
            "spawn-from-granted-cwd",
            code == 0,
            &format!("exit={code} marker={text}"),
        );

        println!("  arm spawn-from-ungranted-cwd:");
        let ungranted = f.work.join("cwd-ungranted.txt");
        let (code, text) = run_child_in(
            &f,
            &policy,
            &f.root.clone(),
            "execinherit",
            &ungranted,
            &argv,
        );
        report(
            fails,
            "spawn-from-ungranted-cwd",
            code == 0,
            &format!("exit={code} marker={text} (the cwd is not checked on the spawn path)"),
        );
    }

    /// THE PRIMITIVE, with no process creation in the way. Rust's Windows "anonymous" pipe
    /// is a NAMED pipe, so `Command::output()` opens NPFS before it ever reaches
    /// `CreateProcessW`. Measuring the pipe alone is what separates "the image is denied"
    /// from "the capture is denied" — the two produce an identical `os error 5` at the
    /// `Command` API.
    fn named_pipe_refused(fails: &mut u32) {
        let f = Fixture::new("pipe");
        let marker = f.work.join("pipe.txt");
        let policy = jail_shaped(&f, Vec::new());
        println!("  arm appcontainer-named-pipe:");
        let (code, text) = run_child(&f, &policy, "namedpipe", &marker, &[]);
        // Not a pass/fail of nub's: it records what the OS does. Reported as a property so
        // the verdict step can require it to have been measured.
        report(
            fails,
            "appcontainer-named-pipe-refused",
            code == 5 && text.starts_with("pipeerr"),
            &format!("exit={code} marker={text} (5/pipeerr ⇒ NPFS is denied to a LowBox child)"),
        );
    }

    /// TREATMENT. The real interpreter, at its own location, granted exactly as the build
    /// jail grants it (the file and its bin dir — `grant_build_jail_interpreter`), spawned
    /// WITHOUT captured stdio so no pipe confounds the exec verdict.
    fn interpreter_in_place(fails: &mut u32, node: &Path) {
        let f = Fixture::new("inplace");
        let mut extra = vec![read_rule(node)];
        if let Some(dir) = node.parent() {
            extra.push(read_rule(dir));
        }
        let policy = jail_shaped(&f, extra);
        let argv = [node.to_string_lossy().into_owned(), "--version".to_string()];

        println!("  arm interpreter-in-place-status-execs (no pipe):");
        let status_marker = f.work.join("inplace-status.txt");
        let (code, text) = run_child(&f, &policy, "execinherit", &status_marker, &argv);
        report(
            fails,
            "interpreter-in-place-status-execs",
            code == 0 && text == "ok status0",
            &format!("exit={code} marker={text}"),
        );

        // The production shape, for the record: same policy, same image, captured stdio.
        // Not gated — it is the symptom, and gating on it would re-measure the pipe.
        println!("  arm interpreter-in-place-output-execs (captured stdio):");
        let out_marker = f.work.join("inplace-output.txt");
        let (code, text) = run_child(&f, &policy, "exec", &out_marker, &argv);
        println!("  diag:interpreter-in-place-output-execs exit={code} marker={text}");
    }

    /// CONTROL — the both-directions half, and the one that makes the treatment mean
    /// something. Byte-identical to the treatment except that the interpreter grant is
    /// absent, so nothing but the grant can explain a difference.
    fn interpreter_ungranted_refused(fails: &mut u32, node: &Path) {
        let f = Fixture::new("ungranted");
        let marker = f.work.join("ungranted.txt");
        let policy = jail_shaped(&f, Vec::new());
        println!("  arm interpreter-ungranted-refused (no pipe):");
        let (code, text) = run_child(
            &f,
            &policy,
            "execinherit",
            &marker,
            &[node.to_string_lossy().into_owned(), "--version".to_string()],
        );
        report(
            fails,
            "interpreter-ungranted-refused",
            code == 5 && text.starts_with("spawnerr"),
            &format!("exit={code} (want 5) marker={text}"),
        );
    }

    /// DIAGNOSTIC, not gated: the interpreter's DACL as it stands WHILE the grant is live,
    /// read from inside the jail because the parent's teardown revokes the ACE before it
    /// could look.
    fn acl_under_jail(node: &Path) {
        let f = Fixture::new("acl");
        let marker = f.work.join("acl.txt");
        let mut extra = vec![read_rule(node)];
        if let Some(dir) = node.parent() {
            extra.push(read_rule(dir));
        }
        let policy = jail_shaped(&f, extra);
        let args = vec![
            "acl".to_string(),
            marker.to_string_lossy().into_owned(),
            node.to_string_lossy().into_owned(),
        ];
        let outcome = apply(&policy, spec(&f, &f.work, &args)).map(|p| p.status());
        println!("---- icacls of the interpreter WITH the grant live (from inside the jail) ----");
        println!("  launch outcome: {outcome:?}");
        match std::fs::read_to_string(&marker) {
            Ok(text) => println!("{text}"),
            Err(e) => println!("  (no marker: {e})"),
        }
        println!("---- end live icacls ----");
    }

    /// BASELINE, both stdio shapes. `C:\Windows\System32` grants `ALL APPLICATION PACKAGES`
    /// read+execute by default, so a system image must run with no grant of ours at all. If
    /// the no-pipe leg fails, the child cannot execute ANYTHING and every verdict above is
    /// about process creation rather than about the interpreter; if only the captured leg
    /// fails, the capture is the defect and the image never mattered.
    fn system_exec_baseline(fails: &mut u32) {
        let f = Fixture::new("sysexec");
        let policy = jail_shaped(&f, Vec::new());
        let cmd = PathBuf::from(std::env::var("SystemRoot").unwrap_or_default())
            .join("System32")
            .join("cmd.exe");
        let argv = [
            cmd.to_string_lossy().into_owned(),
            "/c".to_string(),
            "ver".to_string(),
        ];

        println!("  arm system-exec-baseline (no pipe):");
        let status_marker = f.work.join("sysexec-status.txt");
        let (code, text) = run_child(&f, &policy, "execinherit", &status_marker, &argv);
        report(
            fails,
            "system-exec-baseline",
            code == 0,
            &format!("exit={code} marker={text}"),
        );

        println!("  arm system-exec-captured (captured stdio):");
        let out_marker = f.work.join("sysexec-output.txt");
        let (code, text) = run_child(&f, &policy, "exec", &out_marker, &argv);
        println!("  diag:system-exec-captured exit={code} marker={text}");
    }

    /// DIAGNOSTIC. What the PRODUCTION compile actually derives for an interpreter on this
    /// host — the matcher strings the Windows ACE planner reads. A grant that never appears
    /// here can never become an ACE, whatever the mechanism does.
    fn production_grant_shape(node: &Path) {
        let f = Fixture::new("shape");
        let homes = nub_sandbox::Homes {
            home: f.root.join("home"),
            tmp: std::env::temp_dir(),
            cache: f.root.join("cache"),
            project: f.root.join("project"),
        };
        let policy = nub_sandbox::compile_build_jail(
            homes,
            &f.work,
            None,
            None,
            vec![node.to_path_buf()],
            Vec::new(),
            os_essential_env(),
        );
        println!("---- production compile_build_jail rules mentioning the interpreter ----");
        match policy {
            Ok(p) => {
                for r in &p.fs.rules.entries {
                    let m = r.matcher.as_str();
                    if m.to_ascii_lowercase().contains("node") {
                        println!("  {:?} {:?} {:?} {m}", r.effect, r.access, r.origin);
                    }
                }
            }
            Err(e) => println!("  compile failed: {e}"),
        }
        println!("---- end production rules ----");
    }
}

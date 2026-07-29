//! Windows: can an AppContainer child EXECUTE the granted interpreter?
//!
//! THE DEFECT THIS ISOLATES. With the verbatim command line landed, a confined `cmd.exe`
//! reaches `node` and nub's shim then dies spawning the real binary:
//!
//! ```text
//! failed to detect Node version:
//!   "C:\hostedtoolcache\windows\node\22.23.1\x64\node.exe": Access is denied. (os error 5)
//! ```
//!
//! Access-denied on a CreateProcess of a path the build jail believes it granted. Two
//! candidate causes, and they need different fixes: the ACE never reached that file (a
//! derivation or application problem), or it reached it and an AppContainer still cannot
//! execute the image from there (a mechanism problem, e.g. the `C:\`-rooted ancestor chain
//! granting `ALL APPLICATION PACKAGES` nothing). Every arm below changes exactly one of
//! those variables, so the pair of verdicts names the cause instead of ranking guesses.
//!
//! `in-place` is the real PATH interpreter on the runner, at its own location. `copied` is a
//! byte copy of the same image inside the fixture's protected root. They differ only in WHERE
//! the file lives, so a pass on one and a fail on the other is a location verdict; both
//! failing is a mechanism verdict; both passing means the production grant set is what is
//! wrong and the ACL dump says how.
//!
//! Every property is read off a marker file the CHILD wrote, never off a status the harness
//! reported about itself, and every path the child touches is baked in as an absolute
//! literal argument. `interpreter-ungranted-refused` is the both-directions control: the
//! identical fixture with the interpreter grant removed must be REFUSED, so the probe cannot
//! go green by being permissive.
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
        let _ = std::fs::write(
            marker,
            format!("spawnerr {e:?} raw={:?}", e.raw_os_error()),
        );
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
            // namedpipe <marker> — the primitive under `Command::output`, with no process
            // creation in the way at all. Rust's Windows anonymous pipe is a NAMED pipe
            // (`\\.\pipe\__rust_anonymous_pipe1__.<pid>.<n>`, `sys::pal::windows::pipe`), so
            // if an AppContainer refuses NPFS then every captured-stdio spawn fails for a
            // reason that has nothing to do with the image being executed.
            Some("namedpipe") => {
                use windows_sys::Win32::System::Pipes::CreateNamedPipeW;
                let name: Vec<u16> = format!(r"\\.\pipe\nub-interp-probe-{}", std::process::id())
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                // PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE, byte mode, one instance.
                let h = unsafe { CreateNamedPipeW(name.as_ptr(), 0x0000_0001 | 0x0008_0000, 0, 1, 4096, 4096, 0, std::ptr::null()) };
                let marker = Path::new(&a[1]);
                if h.is_null() || h == (-1isize as *mut std::ffi::c_void) {
                    let e = std::io::Error::last_os_error();
                    let _ = std::fs::write(marker, format!("pipeerr {e:?} raw={:?}", e.raw_os_error()));
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

    fn spec(f: &Fixture, args: &[String]) -> CommandSpec {
        let mut a = vec!["__sbxchild__".to_string()];
        a.extend(args.iter().cloned());
        CommandSpec::new(f.child.as_os_str()).args(a).cwd(&f.root)
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

    /// Run one child arm and return `(exit_code, marker_text)`. `mode` selects the child
    /// entry point; `argv` is whatever that mode takes after the marker path.
    fn run_child(
        f: &Fixture,
        policy: &SandboxPolicy,
        mode: &str,
        marker: &Path,
        argv: &[String],
    ) -> (i32, String) {
        let mut args = vec![mode.to_string(), marker.to_string_lossy().into_owned()];
        args.extend(argv.iter().cloned());
        let outcome = apply(policy, spec(f, &args)).map(|p| p.status());
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
        let (code, text) = run_child(&f, &policy, "execstatus", &status_marker, &argv);
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
            "execstatus",
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
        let outcome = apply(&policy, spec(&f, &args)).map(|p| p.status());
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
        let (code, text) = run_child(&f, &policy, "execstatus", &status_marker, &argv);
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

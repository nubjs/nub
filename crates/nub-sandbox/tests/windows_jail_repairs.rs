//! Windows: do the jail's two repairs work end to end, and does confinement SURVIVE them?
//!
//! WHAT IS BEING MEASURED. Two independent blockers kept a confined `node` from running a
//! lifecycle script at all, and both now have a repair in `crates/nub-sandbox`. This probe is
//! the acceptance measurement for the pair, in ONE run, with the unrepaired arm alongside the
//! repaired one — a green repaired arm proves nothing without an arm where the defect still
//! reproduces.
//!
//! REPAIR 1 — ANCESTOR REACHABILITY. Node's JS `realpathSync` opens every prefix of a path as
//! a TARGET, starting at the volume root, so every absolute `require()` died on
//! `EPERM: lstat 'C:\'`. Traverse-bypass exempts intermediate components of ONE open; it does
//! not make an ancestor openable on its own. `backend/windows.rs` now writes a NON-INHERITED
//! traverse + read-attributes ACE on every ancestor of every granted path where the
//! unprivileged user can, and HARVESTS the capability SIDs (`S-1-15-3-…`) already sitting on
//! those ancestors' DACLs so the launch can request them where it cannot write. Setting
//! `NUB_SANDBOX_WIN_NO_ANCESTOR_REPAIR` in the PARENT disables both halves, which is the
//! control arm here: one variable, one fixture, both directions.
//!
//! Non-inherited is the load-bearing word. An ancestor ACE that inherited would silently turn
//! every ancestor into a readable subtree — a repair that opens the jail is a regression, not a
//! fix — so the confinement group re-asserts the canary AND adds a sibling directory under the
//! fixture root that is granted NOTHING. If the ancestor grant had become a subtree grant, that
//! sibling would be readable.
//!
//! REPAIR 2 — PIPED SPAWN. libuv creates a named pipe per piped stdio stream under the GLOBAL
//! NPFS namespace, which is closed to a LowBox token, and `uv__pipe_server` treats the refusal
//! as a name collision and retries forever inside `uv_spawn` — so a confined piped spawn does
//! not fail, it SPINS (measured: cpu_ms 14906 of 15059 wall). `windows_build_jail_node_options()`
//! returns a `NODE_OPTIONS` carrying an `--import data:` preload that rewrites every `'pipe'`
//! slot into a scratch FILE and hands the bytes back through stream objects. The five shapes a
//! real lifecycle script uses are measured through it, plus `fork()`, which has no file
//! analogue and must fail FAST rather than become an unkillable spin.
//!
//! `shim-spawn-stream-returns` is the one that can find a NEW bug rather than confirm an old
//! fix: a raw `spawn()` gets its bytes at child exit, which means the shim has to hold `close`
//! back until the synthesised streams have drained (`_closesNeeded`). If `close` fires first the
//! marker comes back empty, and that is a real defect, reported as measured.
//!
//! EVERY VERDICT IS A MARKER THE CHILD WROTE. Nothing is read off a status the harness reports
//! about itself, and every path a child touches is baked into the JS as an absolute LITERAL —
//! the jail resolves the child's whole environment, so a path arriving through an env var makes
//! a control vacuous. `repair-on-ungranted-read-refused` asserts the SPECIFIC error for the same
//! reason: a canary that came back `NotFound` would mean the control never tested confinement.
//!
//! EVERY NODE LAUNCH IS BOUNDED EXTERNALLY. Node's own `timeout` cannot break libuv's retry
//! loop, so the bound lives in the process that can still act. `unshimmed-piped-hangs` is the
//! deliberate 15-second one; it is the control that says the shim is what changed the outcome.
//!
//! CI IS THE ONLY VENUE. AppContainer cannot be launched over SSH (session 0 has no window
//! station; every launch returns 0xC0000142). Runs branch-scoped via
//! `.github/workflows/win-jail-repairs-probe.yml`, no pull request.

#[cfg(not(target_os = "windows"))]
fn main() {
    // Non-Windows host: nothing to measure. (`harness = false` needs a `main`.)
}

#[cfg(target_os = "windows")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("__sbxchild__") => std::process::exit(win::child_main(&args[2..])),
        // The SHIPPING arm: this same binary, re-executed under a token with no administrative
        // authority, running the shell battery. See `deelev_shell_arm`.
        Some("__deelevshell__") => std::process::exit(win::deelev_shell_main()),
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

    /// The env var `backend/windows.rs` reads to skip BOTH halves of the ancestor repair. It
    /// can only ever REMOVE grants, so it is not a lever anything can be widened with.
    const NO_REPAIR: &str = "NUB_SANDBOX_WIN_NO_ANCESTOR_REPAIR";

    // ── child (runs INSIDE the jail) ─────────────────────────────────────────────────

    pub fn child_main(a: &[String]) -> i32 {
        match a.first().map(String::as_str) {
            // fsprobe <marker> <op…> where op is `label|verb|path`. One line per op, keyed by
            // label, so the parent reads a specific verdict instead of pattern-matching a path
            // that reaches it under two spellings (`%TEMP%` arrives 8.3-short, `RUNNER~1`).
            Some("fsprobe") => {
                let marker = Path::new(&a[1]);
                let mut out = String::new();
                for op in &a[2..] {
                    let mut parts = op.splitn(3, '|');
                    let label = parts.next().unwrap_or("");
                    let verb = parts.next().unwrap_or("");
                    let path = parts.next().unwrap_or("");
                    let outcome: std::io::Result<String> =
                        match verb {
                            "lstat" => std::fs::symlink_metadata(path)
                                .map(|m| format!("dir={}", m.is_dir())),
                            "read" => std::fs::read_to_string(path)
                                .map(|s| format!("bytes={}", s.trim().len())),
                            // The whole iterator is drained: opening a directory handle can succeed
                            // where enumerating it is refused, and enumeration is the right this
                            // arm is about.
                            "list" => std::fs::read_dir(path).and_then(|entries| {
                                let mut n = 0usize;
                                for entry in entries {
                                    entry?;
                                    n += 1;
                                }
                                Ok(format!("entries={n}"))
                            }),
                            "write" => std::fs::write(path, "child-wrote-this")
                                .map(|()| "written".to_string()),
                            _ => Err(std::io::Error::other("unknown verb")),
                        };
                    match outcome {
                        Ok(detail) => out.push_str(&format!("{label}=ok:{detail}\n")),
                        Err(e) => out.push_str(&format!(
                            "{label}=err:{:?}:raw={:?}\n",
                            e.kind(),
                            e.raw_os_error()
                        )),
                    }
                }
                let _ = std::fs::write(marker, out);
                0
            }
            // node <marker> <deadline_secs> <node.exe> <script.js> <sink>
            Some("node") => run_node_bounded(&a[1], &a[2], &a[3], &a[4], &a[5]),
            // exec <marker> <deadline_secs> <sink> <exe> [argv…] — the same bounded,
            // file-backed spawn as `node`, for an arbitrary interpreter.
            Some("exec") => run_exe_bounded(&a[1], &a[2], &a[3], &a[4], &a[5..]),
            Some("privs") => report_privileges(Path::new(&a[1])),
            _ => 2,
        }
    }

    /// Whether the LowBox token holds SeChangeNotifyPrivilege — "bypass traverse checking".
    ///
    /// This is what decides how much of the ancestor problem is real. Windows does not
    /// access-check intermediate path components when the caller holds it, so an open of a
    /// GRANTED leaf succeeds with no ace anywhere on its ancestors — which is exactly what the
    /// jail is measured to do. `lstat C:\` fails for a different reason: realpath opens `C:\`
    /// as the TARGET object, and a target is always checked. If the privilege is present, the
    /// ancestor-ACE repair was never about traversal at all; it was about realpath specifically.
    fn report_privileges(marker: &Path) -> i32 {
        use windows_sys::Win32::Foundation::LUID;
        use windows_sys::Win32::Security::{
            GetTokenInformation, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
            SE_PRIVILEGE_ENABLED_BY_DEFAULT, TOKEN_PRIVILEGES, TOKEN_QUERY, TokenPrivileges,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        let name: Vec<u16> = "SeChangeNotifyPrivilege"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: every out-param is initialised before use; the token handle is closed below.
        let text = unsafe {
            let mut wanted = LUID {
                LowPart: 0,
                HighPart: 0,
            };
            if LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut wanted) == 0 {
                return write_marker(marker, "lookup-failed");
            }
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return write_marker(marker, "open-token-failed");
            }
            let mut needed = 0u32;
            GetTokenInformation(token, TokenPrivileges, std::ptr::null_mut(), 0, &mut needed);
            let mut buf = vec![0u8; needed.max(4) as usize];
            let ok = GetTokenInformation(
                token,
                TokenPrivileges,
                buf.as_mut_ptr().cast(),
                needed,
                &mut needed,
            );
            windows_sys::Win32::Foundation::CloseHandle(token);
            if ok == 0 {
                return write_marker(marker, "query-failed");
            }
            let header = buf.as_ptr().cast::<TOKEN_PRIVILEGES>();
            let count = (*header).PrivilegeCount as usize;
            let entries = std::ptr::addr_of!((*header).Privileges)
                .cast::<windows_sys::Win32::Security::LUID_AND_ATTRIBUTES>();
            let mut found = None;
            for i in 0..count {
                let e = &*entries.add(i);
                if e.Luid.LowPart == wanted.LowPart && e.Luid.HighPart == wanted.HighPart {
                    let enabled = e.Attributes
                        & (SE_PRIVILEGE_ENABLED | SE_PRIVILEGE_ENABLED_BY_DEFAULT)
                        != 0;
                    found = Some(enabled);
                }
            }
            match found {
                Some(true) => format!("present-enabled total={count}"),
                Some(false) => format!("present-disabled total={count}"),
                None => format!("absent total={count}"),
            }
        };
        write_marker(marker, &text)
    }

    fn write_marker(marker: &Path, text: &str) -> i32 {
        match std::fs::write(marker, text) {
            Ok(()) => 0,
            Err(_) => 9,
        }
    }

    /// Spawn Node on a script with FILE-backed stdio (the jail refuses `NUL`, and a pipe is one
    /// of the things under test), wait with a deadline, and report CPU-vs-wall before killing.
    /// A spin reads as CPU ≈ wall; a blocking wait reads as CPU ≈ 0.
    fn run_node_bounded(marker: &str, secs: &str, node: &str, script: &str, sink: &str) -> i32 {
        // `-e:<code>` runs Node with NO MAIN FILE, which is the only way to get past
        // `resolveMainPath` without a flag and therefore the only way to ask what a REQUIRE
        // costs separately from what the entry point costs.
        match script.strip_prefix("-e:") {
            Some(code) => run_exe_bounded(
                marker,
                secs,
                sink,
                node,
                &["-e".to_string(), code.to_string()],
            ),
            None => run_exe_bounded(marker, secs, sink, node, &[script.to_string()]),
        }
    }

    /// Spawn `exe` from INSIDE the jail with FILE-backed stdio, wait with a deadline, and
    /// report CPU-vs-wall before killing. A spin reads as CPU ≈ wall; a blocking wait reads as
    /// CPU ≈ 0. File-backed rather than piped deliberately: a pipe is one of the things under
    /// test elsewhere, and an unbounded launch cannot be measured at all.
    fn run_exe_bounded(marker: &str, secs: &str, sink: &str, exe: &str, args: &[String]) -> i32 {
        use std::os::windows::io::AsRawHandle;
        let marker = Path::new(marker);
        let out = match std::fs::File::create(sink) {
            Ok(f) => f,
            Err(e) => {
                let _ = std::fs::write(marker, format!("sink-create-failed {e:?}"));
                return 9;
            }
        };
        let err = match out.try_clone() {
            Ok(f) => f,
            Err(e) => {
                let _ = std::fs::write(marker, format!("sink-clone-failed {e:?}"));
                return 9;
            }
        };
        let mut command = std::process::Command::new(exe);
        command.args(args);
        let spawned = command
            .stdout(std::process::Stdio::from(out))
            .stderr(std::process::Stdio::from(err))
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::write(marker, format!("spawn-refused raw={:?}", e.raw_os_error()));
                return 5;
            }
        };
        let handle = child.as_raw_handle();
        let start = std::time::Instant::now();
        let deadline = std::time::Duration::from_secs(secs.parse().unwrap_or(30));
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let _ = std::fs::write(
                        marker,
                        format!(
                            "EXITED code={:?} after_ms={}",
                            status.code(),
                            start.elapsed().as_millis()
                        ),
                    );
                    return 0;
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = std::fs::write(marker, format!("waiterr {e:?}"));
                    return 9;
                }
            }
            if start.elapsed() >= deadline {
                let cpu = process_cpu_ms(handle);
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::write(
                    marker,
                    format!(
                        "HUNG killed_after_ms={} cpu_ms={cpu} (cpu≈wall ⇒ a spin, cpu≈0 ⇒ a block)",
                        start.elapsed().as_millis()
                    ),
                );
                return 7;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// User + kernel CPU consumed by a live process, in milliseconds.
    fn process_cpu_ms(handle: std::os::windows::io::RawHandle) -> u64 {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::GetProcessTimes;
        let mut c = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut e = c;
        let mut k = c;
        let mut u = c;
        let ok = unsafe { GetProcessTimes(handle.cast(), &mut c, &mut e, &mut k, &mut u) };
        if ok == 0 {
            return u64::MAX;
        }
        let as_ns100 = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
        (as_ns100(k) + as_ns100(u)) / 10_000
    }

    // ── fixture + policy (mirrors the sibling probes) ────────────────────────────────

    struct Fixture {
        root: PathBuf,
        child: PathBuf,
        work: PathBuf,
        /// A real file OUTSIDE every grant. It EXISTS, so a refusal to read it is a permission
        /// verdict rather than an absence.
        ungranted: PathBuf,
        /// A directory that is a SIBLING of the granted work dir under the fixture root — so
        /// its parent IS on the ancestor chain while it is granted nothing itself. This is what
        /// proves the non-inherited ancestor ACE did not become a subtree grant.
        sibling: PathBuf,
        sibling_file: PathBuf,
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
        /// Keyed by PID as well as a clock nonce: a stale directory from an earlier run must
        /// never be able to satisfy an arm whose child never ran.
        fn new(tag: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root =
                std::env::temp_dir().join(format!("nub-jr-{tag}-{}-{nonce:x}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            secure_root(&root);
            let bin = root.join("bin");
            let work = root.join("work");
            let outside = root.join("outside");
            let sibling = root.join("sibling");
            for d in [&bin, &work, &outside, &sibling] {
                std::fs::create_dir_all(d).unwrap();
            }
            let ungranted = outside.join("secret.txt");
            std::fs::write(&ungranted, "canary-must-not-be-readable").unwrap();
            let sibling_file = sibling.join("sibling-secret.txt");
            std::fs::write(&sibling_file, "sibling-must-not-be-readable").unwrap();
            let child = bin.join("child.exe");
            std::fs::copy(std::env::current_exe().unwrap(), &child).unwrap();
            Fixture {
                root,
                child,
                work,
                ungranted,
                sibling,
                sibling_file,
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

    /// A build-jail-SHAPED policy: default-deny read allowlist, own-dir write, egress denied.
    /// `extra_env` is how the production `NODE_OPTIONS` delivery is measured — the jail resolves
    /// the child's whole environment, so a knob nub stamps in production arrives through exactly
    /// this map.
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
            // `enforce` MUST be set, not merely `resolved`: the Windows backend hands the child
            // the constructed map only when the env axis enforces, and otherwise lets it inherit
            // the parent's. With `resolved` alone the NODE_OPTIONS arms would silently measure
            // the PARENT's environment.
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

    /// CI shows no output until a job ends, so a stalled launch is invisible unless the line
    /// before it has already left the buffer. Called before anything that could hang.
    fn flush() {
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    /// Where the probe currently is, for the watchdog to name if it stops moving.
    static BREADCRUMB: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

    /// A stall inside `apply` cannot be bounded from outside the call — it is synchronous Rust
    /// in this process, so the job timeout kills the run and discards the log, which is how two
    /// runs reported a stall with no location. This names the location instead: every launch
    /// leaves a breadcrumb, and a watchdog thread prints the last one and exits if the probe
    /// stops moving. Exit 97 rather than a panic so the harness reports the code rather than
    /// unwinding through FFI teardown.
    fn arm_watchdog() {
        const STALL: std::time::Duration = std::time::Duration::from_secs(120);
        std::thread::spawn(|| {
            let mut last = None;
            loop {
                std::thread::sleep(STALL);
                let now = BREADCRUMB.lock().ok().and_then(|b| b.clone());
                if now.is_some() && now == last {
                    println!(
                        "  fact:watchdog-stalled-at={}",
                        now.unwrap_or_else(|| "<none>".to_string())
                    );
                    flush();
                    std::process::exit(97);
                }
                last = now;
            }
        });
    }

    fn breadcrumb(where_: &str) {
        if let Ok(mut slot) = BREADCRUMB.lock() {
            *slot = Some(where_.to_string());
        }
    }

    /// The total character count of the environment block a policy would hand `CreateProcessW`,
    /// counted the way the OS does: `KEY=VALUE\0` per entry, plus the block's own terminator.
    /// The documented ceiling is 32767.
    fn env_block_chars(policy: &SandboxPolicy) -> usize {
        policy
            .env
            .constructed
            .iter()
            .map(|(k, v)| k.chars().count() + v.chars().count() + 2)
            .sum::<usize>()
            + 1
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

    /// Run one child arm CONFINED and return `(exit_code, marker_text)`.
    fn run_jailed(
        f: &Fixture,
        policy: &SandboxPolicy,
        cwd: &Path,
        mode: &str,
        marker: &Path,
        argv: &[String],
    ) -> (i32, String) {
        let mut args = vec![mode.to_string(), marker.to_string_lossy().into_owned()];
        args.extend(argv.iter().cloned());
        let _ = std::fs::remove_file(marker);
        breadcrumb(&format!("run_jailed mode={mode} cwd={}", cwd.display()));
        flush();
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
        (code, text)
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

    /// A Windows path as a JS string literal. Backslashes are escaped rather than using a raw
    /// template, so the script text stays valid whatever the path contains.
    fn js_literal(p: &Path) -> String {
        format!("\"{}\"", p.to_string_lossy().replace('\\', "\\\\"))
    }

    fn indent(s: &str) -> String {
        s.lines()
            .map(|l| format!("      {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ── shared arm machinery ─────────────────────────────────────────────────────────

    /// One `fsprobe` launch. Returns the label→result map the child wrote; a label the child
    /// never reached is simply absent, which every caller treats as a failure.
    fn fsprobe(
        f: &Fixture,
        policy: &SandboxPolicy,
        tag: &str,
        ops: &[(String, &str, String)],
    ) -> BTreeMap<String, String> {
        let marker = f.work.join(format!("{tag}.fsprobe"));
        let argv: Vec<String> = ops
            .iter()
            .map(|(label, verb, path)| format!("{label}|{verb}|{path}"))
            .collect();
        let (code, text) = run_jailed(f, policy, &f.work, "fsprobe", &marker, &argv);
        println!("    [{tag}] fsprobe exit={code}\n{}", indent(&text));
        text.lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn line(map: &BTreeMap<String, String>, label: &str) -> String {
        map.get(label).cloned().unwrap_or_else(|| "<absent>".into())
    }

    /// A refusal that is specifically a PERMISSION verdict. `NotFound` here would mean the arm
    /// tested nothing, which is the exact false green a sibling lane already paid for.
    fn denied(value: &str) -> bool {
        value.starts_with("err:") && value.contains("PermissionDenied")
    }

    struct NodeArm {
        outer: String,
        sink: String,
        code: i32,
    }

    /// Write `body` to `<dir>/<tag>.js` and run it under `policy` with an EXTERNAL bound. The
    /// caller reads its own inner marker; this returns only what the harness's own child wrote
    /// about the launch plus Node's stdio.
    fn node_arm(
        f: &Fixture,
        policy: &SandboxPolicy,
        tag: &str,
        node: &Path,
        dir: &Path,
        secs: u32,
        body: &str,
    ) -> NodeArm {
        let sink = dir.join(format!("{tag}.sink"));
        let outer_marker = dir.join(format!("{tag}.outer"));
        // A `-e:` body is passed STRAIGHT THROUGH, never written to a file. Writing it out was a
        // real bug in the first revision: all five require-shape cells silently became
        // entry-point cells running a .js file whose text happened to start with `-e:`, so they
        // measured `resolveMainPath` five times and the eval isolation the group exists for
        // never ran. `-e` is the ONLY way to reach a require without a main module.
        let script = match body.starts_with("-e:") {
            true => body.to_string(),
            false => {
                let path = dir.join(format!("{tag}.js"));
                std::fs::write(&path, body).unwrap();
                path.to_string_lossy().into_owned()
            }
        };
        let (code, outer) = run_jailed(
            f,
            policy,
            dir,
            "node",
            &outer_marker,
            &[
                secs.to_string(),
                node.to_string_lossy().into_owned(),
                script.clone(),
                sink.to_string_lossy().into_owned(),
            ],
        );
        let sink_text = std::fs::read_to_string(&sink).unwrap_or_default();
        println!("  fact:{tag}-outer={outer}");
        if !sink_text.trim().is_empty() {
            println!(
                "  diag:{tag}-sink={}",
                sink_text.trim().replace('\n', " | ")
            );
        }
        NodeArm {
            outer,
            sink: sink_text,
            code,
        }
    }

    /// A real package directory needs a manifest: without one Node's nearest-parent
    /// `package.json` walk climbs into ancestors, which would make an arm fail for a reason
    /// unrelated to what it measures.
    fn seed_package(dir: &Path) {
        std::fs::write(
            dir.join("package.json"),
            "{\"name\":\"probe-fixture\",\"version\":\"0.0.0\"}\n",
        )
        .unwrap();
    }

    /// Run `body` with the ancestor repair DISABLED in this process. The backend reads the var
    /// at launch time, so the control arm's launches have to happen inside.
    fn without_ancestor_repair<T>(body: impl FnOnce() -> T) -> T {
        // SAFETY: the probe is single-threaded; nothing else reads the environment concurrently.
        unsafe { std::env::set_var(NO_REPAIR, "1") };
        let out = body();
        // SAFETY: same.
        unsafe { std::env::remove_var(NO_REPAIR) };
        out
    }

    // ── group 0: what the ancestor ace COSTS ─────────────────────────────────────────
    //
    // The ancestor repair is what makes the Windows jail work unprivileged, and its cost is
    // per lifecycle SPAWN, so a slow writer is not a wart — it is what wedged a 20-minute CI
    // step and stalled three runs of this very probe. `backend/windows.rs` now writes the
    // traverse ace with `SetKernelObjectSecurity`, which goes straight to
    // `NtSetSecurityObject` and has no user-mode inheritance-propagation pass. This group is
    // the measurement of that claim, and it is a TIMING claim, so it needs a differential.
    //
    // THE CONTROL IS LOCAL, DELIBERATELY. `named_propagating_ace` below is a copy of the
    // writer the backend used to call. It lives here rather than in the product because a
    // second writer reachable from `apply` would be dead weight the moment this lands — and
    // because the comparison it enables is only worth anything run on the SAME path with the
    // SAME trustee in the SAME run. One variable: which primitive writes the descriptor.
    //
    // THE TREE IS SYNTHETIC, ALSO DELIBERATELY. `%TEMP%` on a hosted runner is enormous but
    // its size is unknown and varies run to run, so it cannot be the empty-directory control's
    // counterpart. Two sibling directories under one fixture root — identical DACL, identical
    // volume, differing only in descendant count — can. `%TEMP%` is still measured, with the
    // kernel writer only: it is the path that actually stalled, and the propagating writer on
    // it is unbounded by construction.
    //
    // WHAT THIS GROUP DOES NOT MEASURE, because a sibling group already does: whether the ace
    // reaches the confined child, and whether it stops at the directory object. Both now run
    // through this writer, so `repair-on-lstat-chain-permitted` is the effect proof and
    // `repair-on-ancestor-sibling-read-refused` is the scope proof. What is asserted HERE is
    // the DACL-level shape (present, non-inherited, exact mask, absent on a child) and that
    // the descriptor's control bits survive a grant/revoke round trip — the hand-built
    // descriptor starts with a zero control word, so carrying those bits is a property of the
    // new code with nothing else watching it.

    /// Descendants under the populated arm. Sized against python's `Lib\` at 6,412 entries —
    /// the tree that measured ~1000 ms — so the propagating writer has something real to walk.
    const POPULATED_ENTRIES: usize = 4000;

    mod ace {
        use std::io;
        use std::path::Path;
        use windows_sys::Win32::Foundation::{
            CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
        };
        use windows_sys::Win32::Security::Authorization::{
            ConvertSidToStringSidW, ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
            GetNamedSecurityInfoW, GetSecurityInfo, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS,
            SE_FILE_OBJECT, SetEntriesInAclW, SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
            TRUSTEE_W,
        };
        use windows_sys::Win32::Security::Isolation::{
            CreateAppContainerProfile, DeleteAppContainerProfile,
        };
        use windows_sys::Win32::Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION,
            EqualSid, GetAce, GetSecurityDescriptorControl, OBJECT_INHERIT_ACE,
            PSECURITY_DESCRIPTOR, PSID,
        };
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, OPEN_EXISTING,
        };

        /// Byte-identical to the backend's `TRAVERSE_MASK`, so the two writers are asked for
        /// the same thing and a cost difference cannot be a mask difference.
        pub const TRAVERSE_MASK: u32 = 0x0010_00a1;
        pub const INHERIT_FLAGS: u32 = CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE;
        const NO_INHERITANCE: u32 = 0x0;
        const READ_CONTROL: u32 = 0x0002_0000;
        const WRITE_DAC: u32 = 0x0004_0000;

        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        }
        fn wide_path(p: &Path) -> Vec<u16> {
            wide(&p.to_string_lossy())
        }

        /// An ephemeral AppContainer identity, used purely as a trustee. Unique per run and
        /// deleted on drop, so an ace this probe fails to remove names a SID that no longer
        /// resolves rather than a real principal.
        pub struct Trustee {
            name: Vec<u16>,
            pub sddl: String,
        }
        impl Trustee {
            pub fn new() -> io::Result<Self> {
                let nonce = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let name = wide(&format!("nub_acecost_{}_{nonce:x}", std::process::id()));
                let mut sid: PSID = std::ptr::null_mut();
                let hr = unsafe {
                    CreateAppContainerProfile(
                        name.as_ptr(),
                        name.as_ptr(),
                        name.as_ptr(),
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
                let mut out: *mut u16 = std::ptr::null_mut();
                let ok = unsafe { ConvertSidToStringSidW(sid, std::ptr::from_mut(&mut out)) };
                unsafe { LocalFree(sid.cast()) };
                if ok == 0 {
                    let e = io::Error::last_os_error();
                    unsafe { DeleteAppContainerProfile(name.as_ptr()) };
                    return Err(e);
                }
                let mut len = 0usize;
                while unsafe { *out.add(len) } != 0 {
                    len += 1;
                }
                let sddl =
                    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(out, len) });
                unsafe { LocalFree(out.cast()) };
                Ok(Trustee { name, sddl })
            }
        }
        impl Drop for Trustee {
            fn drop(&mut self) {
                unsafe { DeleteAppContainerProfile(self.name.as_ptr()) };
            }
        }

        struct Sid(PSID);
        impl Sid {
            fn new(sddl: &str) -> io::Result<Self> {
                let w = wide(sddl);
                let mut sid: PSID = std::ptr::null_mut();
                if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut sid) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Sid(sid))
            }
        }
        impl Drop for Sid {
            fn drop(&mut self) {
                unsafe { LocalFree(self.0.cast()) };
            }
        }

        struct Handle(HANDLE);
        impl Drop for Handle {
            fn drop(&mut self) {
                unsafe { CloseHandle(self.0) };
            }
        }

        /// THE CONTROL WRITER: what `backend/windows.rs` called before this change. Identical
        /// handle, identical merged DACL, identical non-inherited traverse ace — the only
        /// difference is that `SetSecurityInfo` runs advapi32's inheritance propagation over
        /// the directory's existing children before it returns.
        pub fn named_propagating_ace(dir: &Path, sddl: &str, grant: bool) -> io::Result<()> {
            let sid = Sid::new(sddl)?;
            let wpath = wide_path(dir);
            let handle = unsafe {
                CreateFileW(
                    wpath.as_ptr(),
                    READ_CONTROL | WRITE_DAC,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let _h = Handle(handle);

            let mut old: *mut ACL = std::ptr::null_mut();
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let rc = unsafe {
                GetSecurityInfo(
                    handle,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut old,
                    std::ptr::null_mut(),
                    &mut sd,
                )
            };
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc as i32));
            }
            let mut ea: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
            ea.grfAccessPermissions = TRAVERSE_MASK;
            ea.grfAccessMode = if grant { GRANT_ACCESS } else { REVOKE_ACCESS };
            ea.grfInheritance = NO_INHERITANCE;
            ea.Trustee = TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: sid.0.cast(),
            };
            let mut new: *mut ACL = std::ptr::null_mut();
            let rc = unsafe { SetEntriesInAclW(1, &ea, old, &mut new) };
            unsafe { LocalFree(sd.cast()) };
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc as i32));
            }
            let rc = unsafe {
                SetSecurityInfo(
                    handle,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    new,
                    std::ptr::null_mut(),
                )
            };
            unsafe { LocalFree(new.cast()) };
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc as i32));
            }
            Ok(())
        }

        /// `(access_mask, ace_flags)` of every ace on `dir` naming `sddl`. Empty ⇒ no ace.
        pub fn aces_for(dir: &Path, sddl: &str) -> io::Result<Vec<(u32, u32)>> {
            let sid = Sid::new(sddl)?;
            let (dacl, sd) = read_dacl(dir)?;
            let mut out = Vec::new();
            if !dacl.is_null() {
                unsafe {
                    for i in 0..(*dacl).AceCount as u32 {
                        let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
                        if GetAce(dacl, i, &mut ace) == 0 {
                            continue;
                        }
                        let allow = ace.cast::<ACCESS_ALLOWED_ACE>();
                        let ace_sid: PSID = std::ptr::addr_of!((*allow).SidStart).cast_mut().cast();
                        if EqualSid(ace_sid, sid.0) != 0 {
                            out.push((
                                (*allow).Mask,
                                u32::from((*ace.cast::<ACE_HEADER>()).AceFlags),
                            ));
                        }
                    }
                }
            }
            unsafe { LocalFree(sd.cast()) };
            Ok(out)
        }

        /// The DACL's control word — `SE_DACL_AUTO_INHERITED` / `SE_DACL_PROTECTED` live here,
        /// and a hand-built descriptor that dropped them would show up as a change across a
        /// grant/revoke round trip.
        pub fn control(dir: &Path) -> io::Result<u16> {
            let (_dacl, sd) = read_dacl(dir)?;
            let mut control = 0u16;
            let mut revision = 0u32;
            let ok = unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) };
            unsafe { LocalFree(sd.cast()) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(control)
        }

        fn read_dacl(dir: &Path) -> io::Result<(*mut ACL, PSECURITY_DESCRIPTOR)> {
            let wpath = wide_path(dir);
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let rc = unsafe {
                GetNamedSecurityInfoW(
                    wpath.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut dacl,
                    std::ptr::null_mut(),
                    &mut sd,
                )
            };
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc as i32));
            }
            Ok((dacl, sd))
        }
    }

    /// Grant then revoke with the PRODUCT's writer, returning both durations in microseconds.
    /// Only the grant is compared: the grant is what a lifecycle spawn pays before the child
    /// starts, and it is where the stall was.
    fn time_kernel(dir: &Path, sddl: &str) -> (u128, u128) {
        let t0 = std::time::Instant::now();
        nub_sandbox::windows_object_traverse_ace(dir, sddl, true).expect("kernel grant");
        let grant = t0.elapsed().as_micros();
        let t1 = std::time::Instant::now();
        nub_sandbox::windows_object_traverse_ace(dir, sddl, false).expect("kernel revoke");
        (grant, t1.elapsed().as_micros())
    }

    fn time_named(dir: &Path, sddl: &str) -> (u128, u128) {
        let t0 = std::time::Instant::now();
        ace::named_propagating_ace(dir, sddl, true).expect("named grant");
        let grant = t0.elapsed().as_micros();
        let t1 = std::time::Instant::now();
        ace::named_propagating_ace(dir, sddl, false).expect("named revoke");
        (grant, t1.elapsed().as_micros())
    }

    fn ace_cost(fails: &mut u32) {
        let f = Fixture::new("cost");
        let trustee = match ace::Trustee::new() {
            Ok(t) => t,
            Err(e) => {
                report(fails, "ace-cost-trustee-minted", false, &format!("{e}"));
                return;
            }
        };
        println!("  fact:ace-cost-trustee={}", trustee.sddl);

        let empty = f.root.join("tree-empty");
        let full = f.root.join("tree-full");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&full).unwrap();
        // Spread across subdirectories: the propagation pass walks the whole subtree, and a
        // flat directory understates a real toolchain layout.
        for i in 0..POPULATED_ENTRIES {
            if i % 200 == 0 {
                std::fs::create_dir_all(full.join(format!("d{}", i / 200))).unwrap();
            }
            std::fs::write(full.join(format!("d{}/f{i}.bin", i / 200)), b"x").unwrap();
        }
        println!("  fact:ace-cost-populated-entries={POPULATED_ENTRIES}");

        let control_before = ace::control(&full).expect("read control");
        let (k_empty, k_empty_rev) = time_kernel(&empty, &trustee.sddl);
        let (k_full, k_full_rev) = time_kernel(&full, &trustee.sddl);
        let (n_empty, n_empty_rev) = time_named(&empty, &trustee.sddl);
        let (n_full, n_full_rev) = time_named(&full, &trustee.sddl);
        let control_after = ace::control(&full).expect("read control");
        println!(
            "  fact:ace-cost-us kernel-empty={k_empty} kernel-full={k_full} \
             named-empty={n_empty} named-full={n_full}"
        );
        println!(
            "  fact:ace-revoke-us kernel-empty={k_empty_rev} kernel-full={k_full_rev} \
             named-empty={n_empty_rev} named-full={n_full_rev}"
        );

        // THE CONTROL. If the propagating writer did NOT get materially slower on the
        // populated tree, the tree walk never reproduced on this machine and every comparison
        // below is measuring noise — the one outcome that must not read as a pass.
        report(
            fails,
            "ace-cost-named-writer-scales-with-tree",
            n_full > n_empty.saturating_mul(3).max(n_empty + 5_000),
            &format!(
                "named: {n_empty}us empty -> {n_full}us at {POPULATED_ENTRIES} entries \
                 (THE CONTROL: without this the kernel numbers prove nothing)"
            ),
        );
        // The claim: the kernel writer's cost is a descriptor write, so tree size does not
        // enter into it. Floored generously — building 4000 entries leaves cache state, and the
        // runner is shared.
        report(
            fails,
            "ace-cost-kernel-writer-flat-in-tree-size",
            k_full < k_empty.saturating_mul(3).max(k_empty + 20_000),
            &format!("kernel: {k_empty}us empty -> {k_full}us at {POPULATED_ENTRIES} entries"),
        );
        report(
            fails,
            "ace-cost-kernel-beats-named-on-populated-tree",
            k_full < n_full,
            &format!("kernel={k_full}us named={n_full}us on the same directory"),
        );

        // A re-grant with the ace already present. A tree walk costs the same either way (the
        // signature measured on python's tree); a descriptor write is the same tiny cost.
        nub_sandbox::windows_object_traverse_ace(&full, &trustee.sddl, true).expect("pre-grant");
        let t = std::time::Instant::now();
        nub_sandbox::windows_object_traverse_ace(&full, &trustee.sddl, true).expect("re-grant");
        let regrant = t.elapsed().as_micros();
        let landed = ace::aces_for(&full, &trustee.sddl).expect("read aces");
        let child_aces = ace::aces_for(&full.join("d0"), &trustee.sddl).expect("read child aces");
        nub_sandbox::windows_object_traverse_ace(&full, &trustee.sddl, false).expect("revoke");
        let after_revoke = ace::aces_for(&full, &trustee.sddl).expect("read aces");
        println!("  fact:ace-regrant-us={regrant}");

        // EFFECT, not just a call that returned: the ace is on the DACL carrying exactly the
        // traverse mask. `repair-on-lstat-chain-permitted` in the next group is the same fact
        // seen from inside the jail.
        report(
            fails,
            "ace-lands-with-exact-traverse-mask",
            landed.len() == 1 && landed[0].0 == ace::TRAVERSE_MASK,
            &format!(
                "aces={landed:?} expected one at 0x{:08x}",
                ace::TRAVERSE_MASK
            ),
        );
        // SCOPE, and it is a security property: a non-inherited ace grants the directory
        // OBJECT. An inheritable one would silently turn every ancestor into a readable
        // subtree.
        report(
            fails,
            "ace-carries-no-inheritance-flags",
            landed.len() == 1 && landed[0].1 & ace::INHERIT_FLAGS == 0,
            &format!("flags=0x{:02x}", landed.first().map_or(0, |a| a.1)),
        );
        report(
            fails,
            "ace-absent-on-child-directory",
            child_aces.is_empty(),
            &format!("child aces={child_aces:?} (the grant must stop at the object)"),
        );
        report(
            fails,
            "ace-revoke-removes-it",
            after_revoke.is_empty(),
            &format!("aces after revoke={after_revoke:?}"),
        );
        // The hand-built descriptor starts with a zero control word. Clearing
        // SE_DACL_AUTO_INHERITED or SE_DACL_PROTECTED on a directory nub does not own would be
        // a lasting change to the user's machine, so the bits are carried across explicitly.
        report(
            fails,
            "ace-preserves-dacl-control-bits",
            control_before == control_after,
            &format!("control 0x{control_before:04x} -> 0x{control_after:04x}"),
        );

        // %TEMP% is the path that actually stalled, and it is on every fixture's ancestor
        // chain. Kernel writer only: the propagating writer here is unbounded by construction,
        // which is exactly why three runs of this probe lost their answer.
        let tmp = std::env::temp_dir();
        let (t_grant, t_revoke) = time_kernel(&tmp, &trustee.sddl);
        println!("  fact:ace-cost-real-temp-us grant={t_grant} revoke={t_revoke}");
        report(
            fails,
            "ace-cost-real-temp-under-a-second",
            t_grant < 1_000_000,
            &format!("{t_grant}us on {} (was minutes)", tmp.display()),
        );

        // The AAP skip. Which paths already publish read+execute to every AppContainer is a
        // property of the MACHINE's default ACLs, so the %ProgramFiles% cells are FACTS; the
        // fixture cell is the assertion, because a protected user-only root must never look
        // already-granted or the skip would drop a grant the jail needs.
        let pf = std::env::var("ProgramFiles").ok();
        for (label, dir) in [
            ("programfiles", pf.clone()),
            ("programfiles-nodejs", pf.map(|p| format!("{p}\\nodejs"))),
        ] {
            match dir {
                Some(d) if Path::new(&d).exists() => println!(
                    "  fact:aap-readable-{label}={}",
                    nub_sandbox::windows_leaf_grant_redundant(Path::new(&d))
                ),
                _ => println!("  fact:aap-readable-{label}=absent"),
            }
        }
        report(
            fails,
            "aap-skip-declines-a-protected-fixture-root",
            !nub_sandbox::windows_leaf_grant_redundant(&full),
            "a user-only protected tree must still get its grant",
        );
    }

    /// The teardown half of the AAP skip: a pre-existing inheritable
    /// `ALL APPLICATION PACKAGES` ace must SURVIVE a launch that granted the path, because the
    /// launch skipped writing one and therefore has nothing to revoke. Getting this wrong would
    /// strip an ace off the user's own `%ProgramFiles%` tree, so it is asserted end to end
    /// through `apply` rather than reasoned about from the skip's return value.
    fn aap_skip_teardown(fails: &mut u32, node: &Path) {
        let f = Fixture::new("aap");
        let shared = f.root.join("shared-tool");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("tool.txt"), b"shared").unwrap();
        let seeded = std::process::Command::new("icacls")
            .arg(&shared)
            .args(["/grant", "*S-1-15-2-1:(OI)(CI)(RX)"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !seeded {
            report(
                fails,
                "aap-skip-fixture-seeded",
                false,
                "icacls could not place the ALL APPLICATION PACKAGES ace",
            );
            return;
        }
        report(
            fails,
            "aap-skip-sees-the-seeded-grant",
            nub_sandbox::windows_leaf_grant_redundant(&shared),
            "the seeded inheritable AAP ace must make the grant redundant",
        );

        let policy = jail_shaped(&f, vec![read_rule(node), read_rule(&shared)], &[]);
        let run = fsprobe(
            &f,
            &policy,
            "aap",
            &[(
                "toolread".to_string(),
                "read",
                shared.join("tool.txt").to_string_lossy().into_owned(),
            )],
        );
        report(
            fails,
            "aap-skip-child-still-reads-the-tool",
            line(&run, "toolread").starts_with("ok:"),
            &line(&run, "toolread"),
        );
        let survivors = ace::aces_for(&shared, "S-1-15-2-1").expect("read aces");
        report(
            fails,
            "aap-skip-leaves-the-preexisting-ace-alone",
            survivors
                .iter()
                .any(|(_, flags)| flags & ace::INHERIT_FLAGS != 0),
            &format!(
                "AAP aces after teardown={survivors:?} \
                 (the jail must not strip one it did not create)"
            ),
        );
    }

    // ── group 1: ancestor reachability, both directions ──────────────────────────────

    /// ONE fixture, ONE variable. The unrepaired arm runs first and must reproduce the defect;
    /// the repaired arm then runs the identical ops on the identical paths, so a difference
    /// between them is the repair and nothing else.
    fn ancestor_repair(fails: &mut u32, node: &Path) {
        let f = Fixture::new("anc");
        println!("  fact:fixture-root={}", f.root.display());

        let chain: Vec<PathBuf> = {
            let mut v: Vec<PathBuf> = f.work.ancestors().map(Path::to_path_buf).collect();
            v.reverse();
            v
        };
        let mut base: Vec<(String, &str, String)> =
            vec![("croot".to_string(), "lstat", r"C:\".to_string())];
        for (i, dir) in chain.iter().enumerate() {
            base.push((
                format!("anc{i}"),
                "lstat",
                dir.to_string_lossy().into_owned(),
            ));
        }
        base.push((
            "workwrite".to_string(),
            "write",
            f.work.join("liveness.txt").to_string_lossy().into_owned(),
        ));

        let policy = jail_shaped(&f, vec![read_rule(node)], &[]);

        // -- unrepaired control --------------------------------------------------------
        println!("  ---- arm repair-off ----");
        let off = without_ancestor_repair(|| fsprobe(&f, &policy, "off", &base));
        report(
            fails,
            "repair-off-lstat-c-root-refused",
            denied(&line(&off, "croot")),
            &format!(
                "{} (THE CONTROL: without this the repaired arm proves nothing)",
                line(&off, "croot")
            ),
        );
        report(
            fails,
            "work-write-permitted-repair-off",
            line(&off, "workwrite").starts_with("ok:"),
            &line(&off, "workwrite"),
        );

        let off_node = without_ancestor_repair(|| {
            absolute_require_arm(&f, &policy, "off", node, "repair-off")
        });
        report(
            fails,
            "repair-off-node-absolute-require-fails",
            !off_node.dep_loaded && off_node.blames_realpath(),
            &off_node.detail(),
        );

        // -- repaired ------------------------------------------------------------------
        println!("  ---- arm repair-on ----");
        // The repair's reach is a property of THIS machine — which capability SIDs its system
        // roots carry, and whether the kernel accepts them — so both are reported alongside the
        // arm. A repaired arm that FELL BACK is not the same finding as one whose capabilities
        // were accepted and did not help, and the two call for opposite next moves;
        // `capability-fallbacks` is what tells them apart.
        let fallbacks_before = nub_sandbox::windows_capability_fallbacks();
        for sid in nub_sandbox::windows_ancestor_capability_sids(&chain) {
            println!("  fact:capability-sid={sid}");
        }

        let mut ops = base.clone();
        ops.push((
            "ungranted".to_string(),
            "read",
            f.ungranted.to_string_lossy().into_owned(),
        ));
        ops.push((
            "siblingread".to_string(),
            "read",
            f.sibling_file.to_string_lossy().into_owned(),
        ));
        ops.push((
            "siblinglist".to_string(),
            "list",
            f.sibling.to_string_lossy().into_owned(),
        ));
        // A FACT, not a verdict, and deliberately so: `TRAVERSE_MASK` is byte-identical to the
        // mask Windows puts on `C:\` for its own capability SID, which includes
        // FILE_LIST_DIRECTORY — so a directory ON the chain is expected to be enumerable, and
        // asserting otherwise would ship a property that fails by design. What the repair must
        // not do is reach a directory OFF the chain, which `siblinglist` above is the verdict
        // for. This line makes the difference visible instead of leaving it implied.
        ops.push((
            "chainlist".to_string(),
            "list",
            f.root.to_string_lossy().into_owned(),
        ));
        let on = fsprobe(&f, &policy, "on", &ops);
        println!("  fact:chain-ancestor-listing={}", line(&on, "chainlist"));
        println!(
            "  fact:capability-fallbacks={}",
            nub_sandbox::windows_capability_fallbacks() - fallbacks_before
        );

        report(
            fails,
            "repair-on-lstat-c-root-permitted",
            line(&on, "croot").starts_with("ok:"),
            &line(&on, "croot"),
        );
        // Per-ancestor facts so a partial failure names the exact directory rather than
        // collapsing the chain into one boolean.
        let mut chain_ok = true;
        for (i, dir) in chain.iter().enumerate() {
            let result = line(&on, &format!("anc{i}"));
            println!("  fact:ancestor[{}]={result}", dir.display());
            chain_ok &= result.starts_with("ok:");
        }
        report(
            fails,
            "repair-on-lstat-chain-permitted",
            chain_ok,
            "every ancestor of the granted work dir must lstat (see the per-ancestor facts)",
        );
        report(
            fails,
            "work-write-permitted-repair-on",
            line(&on, "workwrite").starts_with("ok:"),
            &line(&on, "workwrite"),
        );

        // Confinement must SURVIVE the repair. The canary asserts the SPECIFIC error; the
        // sibling pair is what proves a NON-INHERITED ancestor ACE did not become a subtree
        // grant — its parent is on the chain, it is granted nothing itself.
        report(
            fails,
            "repair-on-ungranted-read-refused",
            denied(&line(&on, "ungranted")),
            &format!(
                "{} (a NotFound here would mean the canary tested nothing)",
                line(&on, "ungranted")
            ),
        );
        report(
            fails,
            "repair-on-ancestor-sibling-read-refused",
            denied(&line(&on, "siblingread")),
            &format!(
                "{} (the ancestor ACE must not reach a sibling of the granted dir)",
                line(&on, "siblingread")
            ),
        );
        report(
            fails,
            "repair-on-ancestor-sibling-listing-refused",
            denied(&line(&on, "siblinglist")),
            &format!(
                "{} (traverse through an ancestor must not enumerate a subdirectory of it)",
                line(&on, "siblinglist")
            ),
        );

        let on_node = absolute_require_arm(&f, &policy, "on", node, "repair-on");
        report(
            fails,
            "repair-on-node-absolute-require-resolves",
            on_node.dep_loaded,
            &on_node.detail(),
        );

        lifecycle_body(fails, &f, &policy, node);
    }

    struct RequireArm {
        main_ran: bool,
        dep_loaded: bool,
        outer: String,
        sink: String,
    }

    impl RequireArm {
        /// Node's own stderr naming the realpath refusal — the failure the repair exists to
        /// remove, distinguished from any other reason the interpreter might not start.
        fn blames_realpath(&self) -> bool {
            let s = &self.sink;
            s.contains("EPERM") || s.contains("lstat") || s.contains("realpath")
        }
        fn detail(&self) -> String {
            format!(
                "main_ran={} dep_loaded={} outer={} sink={}",
                self.main_ran,
                self.dep_loaded,
                self.outer,
                self.sink.trim().replace('\n', " | ")
            )
        }
    }

    /// THE HEADLINE SHAPE. A jailed `node <script>` whose script `require()`s a SECOND file by
    /// absolute path; that module writes the marker, so a green verdict means the whole
    /// resolution walk survived rather than that the interpreter merely started.
    fn absolute_require_arm(
        f: &Fixture,
        policy: &SandboxPolicy,
        tag: &str,
        node: &Path,
        arm: &str,
    ) -> RequireArm {
        let dir = f.work.join(format!("req-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        seed_package(&dir);
        let main_marker = dir.join("main.marker");
        let dep_marker = dir.join("dep.marker");
        let dep = dir.join("dep.js");
        std::fs::write(
            &dep,
            format!(
                "require(\"fs\").writeFileSync({marker}, \"dep-loaded\");\nmodule.exports = 1;\n",
                marker = js_literal(&dep_marker),
            ),
        )
        .unwrap();
        let _ = std::fs::remove_file(&main_marker);
        let _ = std::fs::remove_file(&dep_marker);

        let body = format!(
            r#"const fs = require("fs");
fs.writeFileSync({main_marker}, "main-ran");
require({dep});
"#,
            main_marker = js_literal(&main_marker),
            dep = js_literal(&dep),
        );
        println!("  ---- {arm} absolute require ----");
        let run = node_arm(f, policy, &format!("{arm}-require"), node, &dir, 30, &body);
        let _ = run.code;
        RequireArm {
            main_ran: std::fs::read_to_string(&main_marker).is_ok(),
            dep_loaded: std::fs::read_to_string(&dep_marker)
                .is_ok_and(|s| s.contains("dep-loaded")),
            outer: run.outer,
            sink: run.sink,
        }
    }

    /// Does the LowBox token bypass traverse checking? Reported as a FACT plus one verdict,
    /// because it reframes everything else rather than gating it: if the privilege is held, an
    /// open of a granted leaf never checks its ancestors, and the ancestor-ACE repair was only
    /// ever needed for the one operation that opens ancestors ON PURPOSE.
    fn token_privileges(fails: &mut u32) {
        let f = Fixture::new("privs");
        let marker = f.work.join("privs.marker");
        let policy = jail_shaped(&f, Vec::new(), &[]);
        let (code, text) =
            without_ancestor_repair(|| run_jailed(&f, &policy, &f.work, "privs", &marker, &[]));
        println!("  fact:lowbox-sechangenotify={text}");
        report(
            fails,
            "lowbox-token-privileges-readable",
            !text.starts_with("<no marker>") && !text.ends_with("-failed"),
            &format!("exit={code} {text}"),
        );
    }

    /// WHICH REQUIRE SHAPES COST A WALK ABOVE THE USER PROFILE — the blast-radius measurement.
    ///
    /// Every cell here runs with the repair OFF, which is the UNPRIVILEGED REALITY: a standard
    /// user owns `%USERPROFILE%` down and can repair that, but `C:\` is owned by TrustedInstaller
    /// and `C:\Users` by SYSTEM, neither grants a standard group WRITE_DAC, and the capability
    /// route is closed. So "repair off above the profile" is not a hypothetical arm — it is what
    /// ships.
    ///
    /// The shapes are separated because the source says the entry point ALONE is fatal and that
    /// needs confirming against a runtime rather than a reading. `resolveMainPath` realpaths the
    /// main script (`run_main.js`), `_findPath` realpaths every resolved filename including the
    /// main one (`loader.js`), and `realpathSync` lstats `splitRoot(p)` — `C:\` — before any
    /// component (`fs.js`). If that is right, `node <file>` cannot start whatever the file
    /// contains, and no require shape matters because no require is ever reached. `-e` is the
    /// only way to ask the second question at all: it has no main module, so it is the control
    /// that separates the ENTRY cost from the REQUIRE cost.
    ///
    /// The junction cell is the one that matters most for nub specifically. The default linker is
    /// `NodeLinker::Isolated`, so a dependency is a LINK into a store cell rather than a real
    /// directory — exactly the shape that forces a realpath even where a plain directory might
    /// not. `mklink /J` is used rather than a symlink because a junction needs no privilege,
    /// which keeps the fixture representative of what an unprivileged install produces.
    fn require_shapes(fails: &mut u32, node: &Path, arm: &str, extra_env: &[(&str, String)]) {
        let f = Fixture::new(&format!("shapes-{arm}"));
        let dir = f.work.join("shapes");
        std::fs::create_dir_all(&dir).unwrap();
        seed_package(&dir);

        // A dependency reached three ways: by absolute path, by a relative path from cwd, and by
        // bare specifier through a junction, which is nub's own layout.
        let store = dir.join("store").join("dep@1.0.0");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(
            store.join("package.json"),
            "{\"name\":\"dep\",\"version\":\"1.0.0\",\"main\":\"index.js\"}\n",
        )
        .unwrap();
        let modules = dir.join("node_modules");
        std::fs::create_dir_all(&modules).unwrap();
        let junction = modules.join("dep");
        let linked = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&junction)
            .arg(&store)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        println!("  fact:junction-created={linked}");

        let marker = |name: &str| dir.join(format!("{name}.marker"));
        let dep_body = |name: &str| {
            format!(
                "require(\"fs\").writeFileSync({m}, \"dep-loaded\");\nmodule.exports = 1;\n",
                m = js_literal(&marker(name))
            )
        };
        std::fs::write(store.join("index.js"), dep_body("bare")).unwrap();
        let flat = dir.join("dep.js");
        std::fs::write(&flat, dep_body("path")).unwrap();

        let cells: Vec<(&str, String)> = vec![
            // No main module and no require: does Node start under the jail AT ALL?
            (
                "eval-only",
                format!(
                    "-e:require('fs').writeFileSync({m}, 'eval-ran')",
                    m = js_literal(&marker("eval"))
                ),
            ),
            (
                "eval-then-absolute-require",
                format!("-e:require({p})", p = js_literal(&flat)),
            ),
            (
                "eval-then-relative-require",
                "-e:require('./dep.js')".to_string(),
            ),
            // A builtin needs no resolution and no realpath. If even THIS cannot run, the wall is
            // not the module system at all and every other cell is over-attributed.
            (
                "eval-builtin-only",
                format!(
                    "-e:require('fs').writeFileSync({m},'eval-ran')",
                    m = js_literal(&marker("eval"))
                ),
            ),
            (
                "eval-then-bare-require-through-junction",
                "-e:require('dep')".to_string(),
            ),
        ];

        println!("  ---- require shapes [{arm}], repair OFF (the shipping configuration) ----");
        for (id, script) in &cells {
            for m in ["eval", "path", "bare"] {
                let _ = std::fs::remove_file(marker(m));
            }
            let policy = jail_shaped(&f, vec![read_rule(node)], extra_env);
            let run = without_ancestor_repair(|| {
                node_arm(
                    &f,
                    &policy,
                    &format!("{arm}-{id}"),
                    node,
                    &dir,
                    30,
                    script.as_str(),
                )
            });
            let reached = ["eval", "path", "bare"]
                .iter()
                .filter(|m| std::fs::read_to_string(marker(m)).is_ok())
                .count()
                > 0;
            report(
                fails,
                &format!("shape-{arm}-{id}-completes-unrepaired"),
                reached,
                &format!(
                    "marker-written={reached} outer={} sink={}",
                    run.outer, run.sink
                ),
            );
        }

        // The entry-point cell, stated as its own property because it is the one that decides
        // the blast radius: if `node <file>` cannot start, every shape above is unreachable in
        // practice regardless of how it scored.
        let entry = without_ancestor_repair(|| {
            let policy = jail_shaped(&f, vec![read_rule(node)], extra_env);
            node_arm(
                &f,
                &policy,
                &format!("{arm}-entry-file"),
                node,
                &dir,
                30,
                &format!(
                    "require(\"fs\").writeFileSync({m}, \"entry-ran\");\n",
                    m = js_literal(&marker("entry"))
                ),
            )
        });
        report(
            fails,
            &format!("shape-{arm}-entry-file-completes-unrepaired"),
            std::fs::read_to_string(marker("entry")).is_ok(),
            &format!("outer={} sink={}", entry.outer, entry.sink),
        );
    }

    /// THE SUCCESS CRITERION for repair 1. Not rc=0 and not "no denial logged": a
    /// lifecycle-shaped script body, run in a package directory, reading a granted file and
    /// writing its own marker naming its own cwd.
    fn lifecycle_body(fails: &mut u32, f: &Fixture, policy: &SandboxPolicy, node: &Path) {
        let pkg = f.work.join("node_modules").join("demo-pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        seed_package(&pkg);
        let granted = pkg.join("granted.txt");
        std::fs::write(&granted, "granted-content").unwrap();
        let marker = pkg.join("postinstall.marker");
        let _ = std::fs::remove_file(&marker);

        let body = format!(
            r#"const fs = require("fs");
const body = fs.readFileSync({granted}, "utf8").trim();
fs.writeFileSync({marker}, "done cwd=" + process.cwd() + " read=" + body);
"#,
            granted = js_literal(&granted),
            marker = js_literal(&marker),
        );
        println!("  ---- repair-on lifecycle body ----");
        let run = node_arm(f, policy, "repair-on-lifecycle", node, &pkg, 30, &body);
        let _ = run.code;
        let text = std::fs::read_to_string(&marker).unwrap_or_else(|_| "<no marker>".into());
        println!("  fact:lifecycle-marker={text}");
        report(
            fails,
            "repair-on-lifecycle-script-body-completed",
            text.starts_with("done ") && text.contains("read=granted-content"),
            &format!("{text} (outer={})", run.outer),
        );
    }

    // ── group 2: the stdio shim ──────────────────────────────────────────────────────

    /// Every arm here stamps `NODE_OPTIONS` through the policy's constructed env, which is
    /// exactly how production delivers it. The one unshimmed arm is the control that says the
    /// shim is what changed the outcome rather than some other difference in the run.
    fn stdio_shim(fails: &mut u32, node: &Path) {
        let f = Fixture::new("shim");
        let options = nub_sandbox::windows_build_jail_node_options();
        println!("  fact:node-options-bytes={}", options.len());

        let extra = vec![read_rule(node)];
        let shimmed = jail_shaped(&f, extra.clone(), &[("NODE_OPTIONS", options)]);
        let unshimmed = jail_shaped(&f, extra.clone(), &[]);

        let dir = f.work.join("shim");
        std::fs::create_dir_all(&dir).unwrap();
        seed_package(&dir);

        // THE ENV-BLOCK SIZE CONTROL, and the reason this arm ran before any Node did.
        //
        // A `CreateProcessW` environment block is capped at 32767 CHARACTERS in total, and the
        // percent-encoded shim alone is ~23.8 KB of that — so the shimmed policy sits close
        // enough to the ceiling that a real install's `npm_config_*` and `PATH` could cross it.
        // Two earlier runs died in this group with no output past the byte count, which is
        // consistent with the size and not with anything the shim does, so the two candidates
        // are separated here rather than argued: `tiny` carries a NODE_OPTIONS the size of a
        // flag, `shimmed` carries the real one, and both run the same trivial launch. If tiny
        // passes and shimmed does not, the payload is too large for this delivery and the shim
        // belongs in a FILE — which Fix 1 has just made loadable — rather than a `data:` URL.
        println!("  fact:env-block-chars={}", env_block_chars(&shimmed));
        let tiny = jail_shaped(&f, extra, &[("NODE_OPTIONS", "--title=nub".to_string())]);
        for (id, policy) in [("tiny", &tiny), ("full", &shimmed)] {
            flush();
            let probe = fsprobe(
                &f,
                policy,
                &format!("size-{id}"),
                &[(
                    "workwrite".to_string(),
                    "write",
                    dir.join(format!("size-{id}.txt"))
                        .to_string_lossy()
                        .into_owned(),
                )],
            );
            report(
                fails,
                &format!("node-options-{id}-launches"),
                line(&probe, "workwrite").starts_with("ok:"),
                &line(&probe, "workwrite"),
            );
        }

        // Liveness for this arm, measured before any spawn shape: if a jailed child cannot
        // write into the granted work dir, the arm is broken rather than restrictive.
        let live = fsprobe(
            &f,
            &shimmed,
            "shimlive",
            &[(
                "workwrite".to_string(),
                "write",
                dir.join("liveness.txt").to_string_lossy().into_owned(),
            )],
        );
        report(
            fails,
            "work-write-permitted-shimmed",
            line(&live, "workwrite").starts_with("ok:"),
            &line(&live, "workwrite"),
        );

        let pong = format!(
            "{node}, [\"-e\", \"process.stdout.write('pong')\"]",
            node = js_literal(node)
        );

        // Each shape is its own launch. Sharing one script would let a single hang discard
        // every later measurement, which is the failure three sibling lanes have already had.
        //
        // The expected substring is per-shape and deliberately not just "pong": a THREW marker
        // can carry the failed command line, which contains `pong` too, so the loose check
        // would go green on the exact failure it exists to catch.
        let shapes: &[(&str, &str, &str, String)] = &[
            (
                "shim-execfile-async-returns",
                "execfile-async",
                "RETURNED pong",
                format!(
                    r#"const fs = require("fs"), cp = require("child_process");
const M = {{MARKER}};
fs.writeFileSync(M, "start");
cp.execFile({pong}, (err, stdout) => {{
  fs.writeFileSync(M, err
    ? "THREW " + (err.code || "") + " " + String(err.message).split("\n")[0]
    : "RETURNED " + String(stdout).trim());
}});
"#
                ),
            ),
            (
                "shim-execfilesync-returns",
                "execfilesync",
                "RETURNED pong",
                format!(
                    r#"const fs = require("fs"), cp = require("child_process");
const M = {{MARKER}};
fs.writeFileSync(M, "start");
try {{
  const out = cp.execFileSync({pong}, {{ encoding: "utf8" }});
  fs.writeFileSync(M, "RETURNED " + String(out).trim());
}} catch (e) {{
  fs.writeFileSync(M, "THREW " + (e.code || "") + " " + String(e.message).split("\n")[0]);
}}
"#
                ),
            ),
            (
                "shim-spawnsync-returns",
                "spawnsync",
                "RETURNED pong status=0",
                format!(
                    r#"const fs = require("fs"), cp = require("child_process");
const M = {{MARKER}};
fs.writeFileSync(M, "start");
const r = cp.spawnSync({pong}, {{ encoding: "utf8" }});
fs.writeFileSync(M, r.error
  ? "THREW " + (r.error.code || "") + " " + String(r.error.message).split("\n")[0]
  : "RETURNED " + String(r.stdout).trim() + " status=" + r.status);
"#
                ),
            ),
            (
                // The one that can find a NEW bug: if `close` fires before the synthesised
                // stream drains, `out=` comes back empty and the shim's deferred-close
                // bookkeeping is wrong.
                "shim-spawn-stream-returns",
                "spawn-stream",
                "out=pong",
                format!(
                    r#"const fs = require("fs"), cp = require("child_process");
const M = {{MARKER}};
fs.writeFileSync(M, "start");
let buf = "";
const child = cp.spawn({pong});
child.stdout.on("data", (d) => {{ buf += d; }});
child.on("error", (e) => fs.writeFileSync(M, "THREW " + (e.code || "") + " " + e.message));
child.on("close", (code) => fs.writeFileSync(M, "CLOSED code=" + code + " out=" + buf.trim()));
"#
                ),
            ),
        ];

        for (prop, tag, expect, body) in shapes {
            let marker = dir.join(format!("{tag}.marker"));
            let _ = std::fs::remove_file(&marker);
            let body = body.replace("{MARKER}", &js_literal(&marker));
            println!("  ---- shim shape {tag} ----");
            let run = node_arm(&f, &shimmed, tag, node, &dir, 30, &body);
            let inner = std::fs::read_to_string(&marker).unwrap_or_else(|_| "<no marker>".into());
            println!("  fact:{tag}-inner={inner}");
            let _ = run.code;
            report(
                fails,
                prop,
                inner.contains(expect) && run.outer.starts_with("EXITED"),
                &format!("inner={inner} outer={} (expected {expect})", run.outer),
            );
        }

        fork_fails_fast(fails, &f, &shimmed, node, &dir);
        unshimmed_control(fails, &f, &unshimmed, node, &dir);
    }

    /// An IPC channel is a duplex pipe and a file cannot emulate one, so `fork()` must throw
    /// SYNCHRONOUSLY with a diagnostic naming the opt-out. A hang here is a FAIL: the whole
    /// point of failing fast is that it is strictly better than the unkillable spin.
    fn fork_fails_fast(
        fails: &mut u32,
        f: &Fixture,
        policy: &SandboxPolicy,
        node: &Path,
        dir: &Path,
    ) {
        let marker = dir.join("fork.marker");
        let target = dir.join("fork-target.js");
        std::fs::write(&target, "process.exit(0);\n").unwrap();
        let _ = std::fs::remove_file(&marker);
        let body = format!(
            r#"const fs = require("fs"), cp = require("child_process");
const M = {marker};
fs.writeFileSync(M, "start");
try {{
  cp.fork({target});
  fs.writeFileSync(M, "NO-THROW");
}} catch (e) {{
  fs.writeFileSync(M, "THREW code=" + (e.code || "") + " msg=" + String(e.message));
}}
"#,
            marker = js_literal(&marker),
            target = js_literal(&target),
        );
        println!("  ---- shim shape fork ----");
        let run = node_arm(f, policy, "fork", node, dir, 30, &body);
        let inner = std::fs::read_to_string(&marker).unwrap_or_else(|_| "<no marker>".into());
        println!("  fact:fork-inner={inner}");
        let _ = run.code;
        report(
            fails,
            "shim-fork-fails-fast",
            inner.contains("ERR_NUB_SANDBOX_NO_IPC")
                && inner.contains("dependenciesMeta")
                && run.outer.starts_with("EXITED"),
            &format!("inner={inner} outer={}", run.outer),
        );
    }

    /// THE CONTROL FOR REPAIR 2, and the only 15-second arm in the run. Same policy, same
    /// script shape, `NODE_OPTIONS` withheld: libuv's retry loop has no bound of its own, so
    /// the harness's external kill is what ends it. The marker's `cpu_ms` separates a spin
    /// (cpu ≈ wall) from a blocking wait (cpu ≈ 0).
    fn unshimmed_control(
        fails: &mut u32,
        f: &Fixture,
        policy: &SandboxPolicy,
        node: &Path,
        dir: &Path,
    ) {
        let marker = dir.join("unshimmed.marker");
        let liveness = dir.join("unshimmed-liveness.txt");
        let _ = std::fs::remove_file(&marker);
        let _ = std::fs::remove_file(&liveness);
        let body = format!(
            r#"const fs = require("fs"), cp = require("child_process");
const M = {marker};
fs.writeFileSync(M, "start");
fs.writeFileSync({liveness}, "child-wrote-this");
try {{
  const out = cp.execFileSync({node}, ["-e", "process.stdout.write('pong')"], {{ encoding: "utf8" }});
  fs.writeFileSync(M, "RETURNED " + String(out).trim());
}} catch (e) {{
  fs.writeFileSync(M, "THREW " + (e.code || "") + " " + String(e.message).split("\n")[0]);
}}
"#,
            marker = js_literal(&marker),
            liveness = js_literal(&liveness),
            node = js_literal(node),
        );
        println!("  ---- unshimmed piped control ----");
        let run = node_arm(f, policy, "unshimmed", node, dir, 15, &body);
        let inner = std::fs::read_to_string(&marker).unwrap_or_else(|_| "<no marker>".into());
        println!("  fact:unshimmed-inner={inner}");
        let _ = run.code;
        report(
            fails,
            "work-write-permitted-unshimmed",
            std::fs::read_to_string(&liveness).is_ok_and(|s| s.contains("child-wrote-this")),
            "the jailed node wrote into the granted work dir before reaching the spawn",
        );
        report(
            fails,
            "unshimmed-piped-hangs",
            run.outer.starts_with("HUNG"),
            &format!(
                "outer={} inner={inner} (a RETURNED here would mean the shim is not what \
                 changed the outcome)",
                run.outer
            ),
        );
    }

    // ── group 3: do the SHELLS do their JOB inside the jail? ─────────────────────────
    //
    // WHY THIS GROUP EXISTS AND WHY EVERY EARLIER ANSWER WAS VOID. `aube-scripts` runs a
    // dependency's lifecycle script through a SHELL, not through `node` — so whether the jail
    // is usable is a question about `cmd.exe` first and Node second. Two prior rounds measured
    // the shells on a bare AppContainer that reproduced NONE of the production repairs: zero
    // capabilities requested, no ancestor ACE, no stamped `NODE_OPTIONS`. The first concluded
    // everything worked (it only measured STARTUP — `cmd /c echo`), the second that cmd and
    // PowerShell were badly broken. Neither transferred, and both were retracted.
    //
    // THE THREE THINGS THAT MAKE THIS FAITHFUL. (1) The launch goes through `apply`, i.e. the
    // real `backend/windows.rs`, so the ancestor traverse ACE, the harvested capability SIDs
    // and the leaf grants are the product's own. (2) `NODE_OPTIONS` carries the production
    // stdio shim through the policy's constructed env, exactly as `pm_engine/build_jail.rs`
    // stamps it — and `shell-gate-stdio-shim-live` proves it ARRIVED, from inside the confined
    // child, rather than assuming the env plumbing worked. (3) Every arm has an UNCONFINED twin
    // in the same run on the same fixture, so a cell is judged by a byte-for-byte diff against
    // what the shell does with no jail at all — never by an exit code.
    //
    // STARTING IS NOT WORKING, so no op here is a startup check. Each shell resolves paths,
    // `cd`s into a granted and an ungranted directory, enumerates, expands a glob, reads,
    // writes, searches `%PATH%` twice (`where.exe` and a bare-name spawn), redirects to a file
    // and to the NULL DEVICE, spawns a nested `node` on a granted entry point, calls a `.cmd`
    // shim, and checks that an exit code propagates. A tool that exits 0 having silently done
    // nothing is the failure class this whole effort cares most about, which is why the verdict
    // is a value comparison and the transport is separated from the measurement: the PowerShell
    // battery reports through `[IO.File]::AppendAllText` and tests the FileSystem PROVIDER as
    // one op among many, so provider breakage degrades one cell instead of erasing the table.
    //
    // WHAT IT MEASURED — windows-latest, Windows Server 2025 10.0.26100, Node 22.23.1,
    // pwsh 7.6.3, busybox-w32 FRP-6075. Runs 30531385502 (harness defects), 30532139228,
    // 30532759630. All gates green in the last two: `sentinel=true`, `croot` refused OFF and
    // permitted ON, ungranted canary refused in both arms.
    //
    //  - `cmd.exe` with the repair ON is BYTE-IDENTICAL to unconfined on every functional cell —
    //    `if exist` (with and without a trailing separator), `if exist %SystemRoot%`,
    //    `if exist %ProgramFiles%`, `cd` into a granted dir, `dir`, glob, `type`, `>`,
    //    `where.exe node.exe`, a bare-name `node --version`, `> NUL`, a nested `node <granted.js>`,
    //    a `.cmd` shim, and `%ERRORLEVEL%` propagation. The only divergence is the ungranted
    //    sibling, which must diverge. With the repair OFF cmd writes NOTHING and its stderr is
    //    `Access is denied.` — so the ancestor ACE is what makes cmd work AT ALL, and every
    //    earlier "cmd is broken under the jail" cell was measuring its absence.
    //  - `busybox sh` matches unconfined on its OWN operations with the repair OFF. It needs the
    //    repair only for the PROGRAMS it spawns (`node`, `cmd.exe`).
    //  - `pwsh 7.6.3` with the repair ON matches unconfined everywhere except the null device.
    //    That build already carries the fix for `PowerShell/PowerShell#27253`, so this is a
    //    statement about a FIXED PowerShell, not about PowerShell.
    //  - `/dev/null` is REFUSED (`can't create /dev/null: Permission denied`) while cmd's `> NUL`
    //    succeeds, on the same build in the same run — so the two spellings do not reach the same
    //    thing, and the earlier `>NUL` retraction was right.
    //  - **Windows PowerShell 5.1's whole FileSystem-provider surface is UNAVAILABLE in every
    //    configuration measured**, and the ancestor repair does not touch it: `Test-Path`,
    //    `Get-ChildItem`, `Get-Content`, `Set-Content`, `Set-Location` all raise
    //    `CommandNotFoundException` — the cmdlets are MISSING, not refused — while `Get-Command`
    //    (`Microsoft.PowerShell.Core`, always loaded) works and, with the repair on, external
    //    invocation and `$LASTEXITCODE` are correct. A read grant on `$PSHOME` was measured as a
    //    one-variable arm and is **NOT** the cause: `psmodules` is cell-identical to `on`. The
    //    mechanism is NOT established. 5.1 also costs ~21 s per invocation confined, against
    //    0.9 s for pwsh and 0.5 s for busybox.
    //
    // ⚠️ THE ELEVATION CAVEAT, MEASURED RATHER THAN ASSUMED. `C:\` and `C:\Users` are not
    // DACL-writable by a standard user, so on a real user's machine the repair cannot reach
    // them and only `%USERPROFILE%` and below get the traverse ACE. A GitHub runner is
    // `runneradmin`, which CAN write them — so the repaired arm here is a CEILING, not the
    // shipping configuration. `shell-ace-writable[…]` reports per-ancestor whether that write
    // succeeds and `probe-elevated` reports the token, so no cell can be misread as unprivileged
    // evidence. The repair-OFF arm is the matching FLOOR (it also drops the grants the profile
    // WOULD get unprivileged). A genuinely de-elevated arm needs a second logon and is not
    // attempted here.

    /// One interpreter, and how to hand it the battery. `invoke` receives the script path and
    /// the output path and returns the argv AFTER the executable.
    struct ShellPlan {
        name: &'static str,
        exe: PathBuf,
        script: PathBuf,
        invoke: fn(&Path, &Path) -> Vec<String>,
    }

    /// Every label the battery emits, in the order it emits them. A label ABSENT from an arm is
    /// reported as `<absent>`, which is a finding rather than a skip: it means the shell stopped
    /// there.
    const SHELL_LABELS: &[&str] = &[
        "alive",
        "cwd",
        "existdir",
        "existdirslash",
        "existfile",
        "existwin",
        "existpf",
        "cdgranted",
        "cwdafter",
        "cdungranted",
        "cdungrantedlist",
        "list",
        "glob",
        "read",
        "write",
        "whereexe",
        "pathspawn",
        "nullredir",
        "nestedspawn",
        "cmdshim",
        "exitprop",
        "done",
    ];

    /// A `key=value` transcript, with the repeated `listitem`/`globitem` lines folded into one
    /// sorted `list`/`glob` value so an arm's whole result is a flat label→value map.
    fn shell_results(text: &str) -> BTreeMap<String, String> {
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        let mut items: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for raw in text.lines() {
            let line = raw.trim_end_matches(['\r', '\n']).trim();
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k {
                "listitem" => items.entry("list").or_default().push(v.to_string()),
                "globitem" => items.entry("glob").or_default().push(v.to_string()),
                _ => {
                    out.insert(k.to_string(), v.to_string());
                }
            }
        }
        for (key, mut values) in items {
            values.sort();
            out.insert(key.to_string(), values.join(","));
        }
        out
    }

    /// Whether this process's token reports elevated. Reported as a FACT because it decides
    /// whether the repaired arm's `C:\` cells are unprivileged evidence or a ceiling.
    fn probe_elevated() -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{
            GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        // SAFETY: out-params are initialised before use and the handle is closed on both paths.
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                std::ptr::from_mut(&mut elevation).cast(),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            );
            CloseHandle(token);
            ok != 0 && elevation.TokenIsElevated != 0
        }
    }

    /// Can the ancestor repair's traverse ACE actually be WRITTEN on `dir` by this token? Uses
    /// the product's own writer with a synthetic capability SID that names nothing on this
    /// machine, granted and immediately revoked — so the answer is the real `WRITE_DAC` verdict
    /// on the real path, and nothing is left behind. The SID is deliberately not one the
    /// backend harvests: a residue would then be indistinguishable from a real grant.
    fn ace_writable(dir: &Path) -> String {
        const SYNTHETIC: &str = "S-1-15-3-1024-1-2-3-4-5-6-7-8";
        match nub_sandbox::windows_object_traverse_ace(dir, SYNTHETIC, true) {
            Ok(()) => {
                let revoked = nub_sandbox::windows_object_traverse_ace(dir, SYNTHETIC, false);
                match revoked {
                    Ok(()) => "writable".to_string(),
                    Err(e) => format!("writable-BUT-REVOKE-FAILED:{:?}", e.raw_os_error()),
                }
            }
            Err(e) => format!("refused:{:?}", e.raw_os_error()),
        }
    }

    /// Plant a Node at the canonical ALL-USERS install path so the leaf-grant question can be
    /// asked about the shape that SHIPS instead of about `C:\hostedtoolcache`, which exists only
    /// on a runner. `pm_engine/build_jail.rs:120` derives the jail's interpreter from
    /// `npm_node_execpath` / `NODE` in the AMBIENT env, so for an MSI all-users install it is
    /// `C:\Program Files\nodejs\node.exe` — whatever `preset.rs`'s "nub provisions its own Node"
    /// comment intends, the call site does not guarantee it.
    ///
    /// ADMIN-ONLY SETUP, run by the PARENT before it de-elevates. The owner reset is the
    /// load-bearing step, not tidiness: without it the creating user picks up `%ProgramFiles%`'s
    /// `CREATOR OWNER` full-control ACE, and the de-elevated token — which retains that user SID
    /// — would hold `WRITE_DAC` for a reason no real installer grants, yielding a `writable`
    /// verdict and the opposite conclusion. Skipped if the path already exists: overwriting a
    /// real Node install is not this probe's business.
    fn plant_allusers_node(node: &Path) -> Option<PathBuf> {
        let root = PathBuf::from(
            std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into()),
        )
        .join("nodejs");
        // A REAL install is better evidence than a planted one, so when the image already has
        // one it is used AS IS and nothing is written. Run 30535709104 took this branch and
        // skipped the measurement, which is the defect this returns a path for.
        if root.exists() {
            let exe = root.join("node.exe");
            println!(
                "  fact:allusers-node={} source=PRE-EXISTING-UNMODIFIED exists={}",
                exe.display(),
                exe.is_file()
            );
            println!(
                "  fact:allusers-node-dacl={}",
                std::process::Command::new("icacls")
                    .arg(&exe)
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout)
                        .trim()
                        .replace(['\r', '\n'], " | "))
                    .unwrap_or_else(|e| format!("icacls-failed:{e}"))
            );
            return exe.is_file().then_some(exe);
        }
        if let Err(e) = std::fs::create_dir_all(&root) {
            println!("  fact:allusers-node=MKDIR-REFUSED {e}");
            return None;
        }
        let exe = root.join("node.exe");
        if let Err(e) = std::fs::copy(node, &exe) {
            println!("  fact:allusers-node=COPY-FAILED {e}");
            return None;
        }
        for args in [
            ["/setowner", "BUILTIN\\Administrators", "/T", "/C"].as_slice(),
            ["/reset", "/T", "/C"].as_slice(),
        ] {
            let ok = std::process::Command::new("icacls")
                .arg(&root)
                .args(args)
                .output()
                .map(|o| o.status.success().to_string())
                .unwrap_or_else(|e| format!("spawn-failed:{e}"));
            println!("  fact:allusers-node-icacls[{}]={ok}", args[0]);
        }
        println!(
            "  fact:allusers-node-dacl={}",
            std::process::Command::new("icacls")
                .arg(&exe)
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .replace(['\r', '\n'], " | "))
                .unwrap_or_else(|e| format!("icacls-failed:{e}"))
        );
        println!("  fact:allusers-node={}", exe.display());
        Some(exe)
    }

    /// ONE `apply` launch whose only variable is which interpreter path carries the leaf read
    /// grant. The verdict text is reported verbatim, because `grant_leaf_ace` is fail-CLOSED: a
    /// refusal is not a degraded jail, it is no child and no lifecycle script at all.
    fn launch_with_interpreter_grant(
        f: &Fixture,
        node_options: &str,
        interp: &Path,
        tag: &str,
    ) -> String {
        let mut grants = vec![read_rule(interp)];
        if let Some(dir) = interp.parent() {
            grants.push(read_rule(dir));
        }
        let policy = jail_shaped(f, grants, &[("NODE_OPTIONS", node_options.to_string())]);
        let marker = f.work.join(format!("{tag}.interp"));
        let _ = std::fs::remove_file(&marker);
        let args = vec![
            "fsprobe".to_string(),
            marker.to_string_lossy().into_owned(),
            format!("granted|read|{}", f.work.join("granted.txt").display()),
        ];
        breadcrumb(&format!("launch_with_interpreter_grant {tag}"));
        flush();
        match apply(&policy, spec(f, &f.work, &args)).map(|p| p.status()) {
            Ok(Ok(status)) => format!(
                "LAUNCHED code={:?} marker={}",
                status.code(),
                std::fs::read_to_string(&marker)
                    .unwrap_or_else(|_| "<none>".into())
                    .trim()
                    .replace(['\r', '\n'], " ")
            ),
            Ok(Err(error)) => format!("LAUNCH-FAILED {error}"),
            Err(degradation) => format!("POLICY-REJECTED {degradation:?}"),
        }
    }

    /// The whole shell group. One fixture, one policy, one interpreter set; three arms per
    /// interpreter (unconfined reference, repair-off floor, repair-on ceiling).
    fn shell_tools(fails: &mut u32, node: &Path) {
        let f = Fixture::new("sh");
        println!("  fact:shell-fixture-root={}", f.root.display());
        println!("  fact:probe-elevated={}", probe_elevated());
        // The oracle that DECIDES whether this arm's `C:\` cells are a ceiling. Reported next to
        // `probe-elevated` because that flag LIES under the de-elevated route:
        // `CreateRestrictedToken` copies it instead of recomputing it, so the de-elevated arm
        // still says `probe-elevated=true` while holding no administrative authority (measured,
        // run 30423750288). Gate on this line, never on the one above it.
        println!("  fact:probe-admin-authority={}", admin_authority());
        println!(
            "  fact:shell-fixture-has-space={}",
            f.root.to_string_lossy().contains(' ')
        );

        // Battery targets, all inside the granted work dir except `f.sibling`.
        let sub = f.work.join("sub");
        let globdir = f.work.join("globdir");
        let outroot = f.work.join("out");
        for d in [&sub, &globdir, &outroot] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(f.work.join("granted.txt"), "granted-content\n").unwrap();
        std::fs::write(globdir.join("one.txt"), "1\n").unwrap();
        std::fs::write(globdir.join("two.txt"), "2\n").unwrap();
        std::fs::write(globdir.join("three.dat"), "3\n").unwrap();
        // argv[2] rather than a fixed path: each arm passes its own scratch token, so a stale
        // file from a previous arm can never satisfy one whose spawn never ran.
        std::fs::write(
            f.work.join("spawned.js"),
            "require('fs').writeFileSync(process.argv[2], 'spawned-ok');\n",
        )
        .unwrap();
        std::fs::write(f.work.join("shim.cmd"), "@echo off\r\necho shim-ran\r\n").unwrap();
        seed_package(&f.work);

        let cmd_exe =
            PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()))
                .join(r"System32\cmd.exe");
        let ps_exe =
            PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()))
                .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let pwsh_exe = std::env::var_os("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .map(|d| d.join("pwsh.exe"))
            .find(|c| c.is_file());
        let busybox = std::env::var_os("PROBE_BUSYBOX_EXE")
            .map(PathBuf::from)
            .filter(|p| p.is_file());

        // Every shell gets its own copy inside the fixture where it is not already a system
        // binary, so the policy grants a real leaf rather than relying on the image's DACLs.
        let staged_busybox = busybox.as_ref().map(|src| {
            let dst = f.root.join("bin").join("busybox.exe");
            std::fs::copy(src, &dst).expect("stage busybox into the fixture");
            dst
        });

        // ── WHERE CAN NUB'S LEAF READ-GRANT ACTUALLY BE WRITTEN? ────────────────────────
        //
        // Run 30534565887 answered the shell question with a NON-answer: de-elevated, EVERY
        // confined launch died in step 2 of `AppContainerLaunch::run` with
        // `installing read grant ACE on …\node.exe failed: Access is denied (os error 5)`, and
        // all nineteen cells came back absent. `grant_leaf_ace` is fail-CLOSED, and the AAP skip
        // cannot rescue a FILE: `already_granted_to_appcontainers` requires an INHERITABLE ace,
        // which a file can never carry, so `leaf-grant-redundant` is false for every interpreter
        // binary regardless of the read+execute the parent directory already publishes to
        // AppContainers. Whether that abort fires on a REAL machine therefore turns entirely on
        // per-path `WRITE_DAC`, so each interpreter location's verdict is a first-class
        // measurement here rather than a footnote. Paired with `leaf-grant-redundant`, the two
        // together say whether nub would try the write at all and whether it could succeed.
        let bin_dir = f.root.join("bin");
        for (label, path) in [
            ("node-exe", Some(node.to_path_buf())),
            ("node-dir", node.parent().map(Path::to_path_buf)),
            ("cmd-exe", Some(cmd_exe.clone())),
            ("system32", cmd_exe.parent().map(Path::to_path_buf)),
            ("windir", std::env::var_os("SystemRoot").map(PathBuf::from)),
            ("powershell-exe", Some(ps_exe.clone())),
            ("pshome", ps_exe.parent().map(Path::to_path_buf)),
            ("pwsh-exe", pwsh_exe.clone()),
            (
                "pwsh-dir",
                pwsh_exe
                    .as_deref()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf),
            ),
            (
                "programfiles",
                std::env::var_os("ProgramFiles").map(PathBuf::from),
            ),
            ("staged-busybox", staged_busybox.clone()),
            ("fixture-bin", Some(bin_dir.clone())),
        ] {
            let Some(path) = path else {
                println!("  fact:shell-leafgrant-{label}=<absent-on-this-image>");
                continue;
            };
            if !path.exists() {
                println!("  fact:shell-leafgrant-{label}=<path-does-not-exist>");
                continue;
            }
            println!(
                "  fact:shell-leafgrant-{label}=redundant:{} writable:{} path:{}",
                nub_sandbox::windows_leaf_grant_redundant(&path),
                ace_writable(&path),
                path.display()
            );
        }

        // Stage `node.exe` INSIDE the fixture and put that dir first on `PATH`. Without this the
        // de-elevated battery is UNMEASURABLE — the grant on the image's own interpreter aborts
        // the launch before any shell starts — and a user-owned interpreter is also the norm
        // under nub, which provisions Node into its own cache. The production shape (a grant on
        // a system-owned interpreter) is not engineered away: it gets its own one-variable
        // property, `shell-prodshape-interpreter-grant-launches`, below.
        let staged_node = bin_dir.join("node.exe");
        std::fs::copy(node, &staged_node).expect("stage node into the fixture");

        // `PATH` is REPLACED, not PREPENDED — every directory that carries a `node.exe` is
        // dropped. A prepend leaves the ambient interpreter reachable through a shim's own
        // re-search fallback (stock `npm.cmd` carries
        // `IF NOT EXIST "%~dp0\node.exe" SET "NODE_EXE=node"`), and then a `pathspawn` or
        // `nestedspawn` cell that resolved the AMBIENT, UNGRANTED node would read as a success
        // for the staged one. The unconfined reference gets the same block, so the diff stays
        // honest either way.
        let staged_path = {
            let mut parts = vec![bin_dir.display().to_string()];
            let dropped: Vec<String> =
                std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                    .filter(|d| {
                        if d.join("node.exe").is_file() {
                            true
                        } else {
                            parts.push(d.display().to_string());
                            false
                        }
                    })
                    .map(|d| d.display().to_string())
                    .collect();
            println!("  fact:shell-path-dropped-node-dirs={}", dropped.join(";"));
            parts.join(";")
        };

        let mut plans: Vec<ShellPlan> = Vec::new();
        if cmd_exe.is_file() {
            std::fs::write(f.work.join("probe.cmd"), cmd_battery(&f, &sub, &globdir)).unwrap();
            plans.push(ShellPlan {
                name: "cmd",
                exe: cmd_exe.clone(),
                script: f.work.join("probe.cmd"),
                // `/d /s /c` is production's shape, but the tail is passed as SEPARATE
                // UNQUOTED args: `std::process::Command` escapes an embedded quote, so a
                // pre-quoted single-string tail reached cmd as a literal `\"…\"` and every
                // cell came back absent — including UNCONFINED, which is what caught it. Sound
                // only while the fixture path has no space; `shell-fixture-has-space` reports
                // that rather than leaving it implied.
                invoke: |script, out| {
                    vec![
                        "/d".into(),
                        "/s".into(),
                        "/c".into(),
                        script.display().to_string(),
                        out.display().to_string(),
                    ]
                },
            });
        }
        let ps_argv: fn(&Path, &Path) -> Vec<String> = |script, out| {
            vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                script.display().to_string(),
                out.display().to_string(),
            ]
        };
        if ps_exe.is_file() {
            std::fs::write(
                f.work.join("probe.ps1"),
                ps_battery(&f, &sub, &globdir, &cmd_exe),
            )
            .unwrap();
            plans.push(ShellPlan {
                name: "powershell",
                exe: ps_exe.clone(),
                script: f.work.join("probe.ps1"),
                invoke: ps_argv,
            });
        }
        if let Some(pwsh) = &pwsh_exe {
            plans.push(ShellPlan {
                name: "pwsh",
                exe: pwsh.clone(),
                script: f.work.join("probe.ps1"),
                invoke: ps_argv,
            });
        }
        if let Some(bb) = &staged_busybox {
            std::fs::write(
                f.work.join("probe.sh"),
                sh_battery(&f, &sub, &globdir, &cmd_exe),
            )
            .unwrap();
            plans.push(ShellPlan {
                name: "busybox",
                exe: bb.clone(),
                script: f.work.join("probe.sh"),
                invoke: |script, out| {
                    vec![
                        "sh".into(),
                        script.display().to_string(),
                        out.display().to_string(),
                    ]
                },
            });
        }
        for name in ["cmd", "powershell", "pwsh", "busybox"] {
            println!(
                "  fact:shell-present-{name}={}",
                plans.iter().any(|p| p.name == name)
            );
        }

        // The policy: build-jail shaped, with the PRODUCTION `NODE_OPTIONS` stamped through the
        // constructed env.
        //
        // EVERY LEAF GRANT IS ON A USER-OWNED PATH, in BOTH arms — that is what keeps the
        // elevated and de-elevated arms one variable apart. Granting the SYSTEM interpreters
        // (`cmd.exe`, `powershell.exe`, `%ProgramFiles%\PowerShell\7`) is what aborted the whole
        // de-elevated run, so an arm that grants them for `runneradmin` and cannot for a standard
        // user differs in the grant set as well as the token, and neither arm's cells would then
        // attribute. They are omitted here instead, which relies on the read+execute
        // `C:\Windows` already publishes to ALL APPLICATION PACKAGES — a claim the
        // `leafgrant-*` facts above and the `cmdexe` gate op below both measure rather than assume.
        let node_options = nub_sandbox::windows_build_jail_node_options();
        let mut grants = vec![read_rule(&bin_dir), read_rule(&staged_node)];
        if let Some(bb) = &staged_busybox {
            grants.push(read_rule(bb));
        }
        let policy = jail_shaped(
            &f,
            grants,
            &[
                ("NODE_OPTIONS", node_options.clone()),
                ("PATH", staged_path.clone()),
            ],
        );
        println!("  fact:shell-node-options-bytes={}", node_options.len());
        println!("  fact:shell-staged-node={}", staged_node.display());

        // ── THE DECISIVE PAIR: WHICH INTERPRETER PATH CAN THE JAIL EVEN LAUNCH WITH? ────
        //
        // Three launches, identical but for the one path carrying the interpreter leaf grant.
        // `staged` is a directory nub owns; `ambient` is whatever this image put on PATH;
        // `allusers` is the canonical MSI location `C:\Program Files\nodejs\node.exe`, planted by
        // the elevated parent precisely so the shipping shape is measured rather than inferred
        // from the runner's `C:\hostedtoolcache`. Because `grant_leaf_ace` is fail-CLOSED, the
        // difference between these three is the difference between a lifecycle script running and
        // no child existing at all — which makes this the single most load-bearing group here.
        for (tag, interp) in [
            ("staged", Some(staged_node.clone())),
            ("ambient", Some(node.to_path_buf())),
            (
                "allusers",
                std::env::var_os("PROBE_ALLUSERS_NODE").map(PathBuf::from),
            ),
        ] {
            let Some(interp) = interp.filter(|p| p.is_file()) else {
                println!("  fact:shell-interp-{tag}=<absent-on-this-image>");
                continue;
            };
            let verdict = launch_with_interpreter_grant(&f, &node_options, &interp, tag);
            println!(
                "  fact:shell-interp-{tag}={verdict} path={} redundant={} writable={}",
                interp.display(),
                nub_sandbox::windows_leaf_grant_redundant(&interp),
                ace_writable(&interp)
            );
            // A MEASUREMENT: a FAIL on `ambient`/`allusers` is the finding, so it is reported and
            // never gated. Only `staged` is expected to hold under both principals.
            report(
                fails,
                &format!("shell-interpreter-grant-launches-{tag}"),
                verdict.starts_with("LAUNCHED"),
                &verdict,
            );
        }

        // ── the gates. No arm below counts as evidence unless these pass. ───────────────
        let chain: Vec<PathBuf> = {
            let mut v: Vec<PathBuf> = f.work.ancestors().map(Path::to_path_buf).collect();
            v.reverse();
            v
        };
        for sid in nub_sandbox::windows_ancestor_capability_sids(&chain) {
            println!("  fact:shell-capability-sid={sid}");
        }
        for dir in &chain {
            println!(
                "  fact:shell-ace-writable[{}]={}",
                dir.display(),
                ace_writable(dir)
            );
        }

        // GATE 1: the stdio shim really arrived, asserted from inside the confined child on the
        // SAME policy the shells run under. Without this the shell arms could be measuring a
        // launch whose `NODE_OPTIONS` never took effect.
        let shim_marker = f.work.join("shimlive.txt");
        // `-e`, NOT a script FILE. Run 30536525468 had this gate as a `.js` entry point and it
        // FAILED de-elevated for the very reason the shell cells exist to measure: `node <abs
        // file>` dies in `resolveMainPath` when `C:\` is unreachable, so the gate was
        // unsatisfiable by construction in the arm that needed it most. `-e` reaches a require
        // with no main module, which is the only shape that isolates "did the preload arrive"
        // from "can an entry point resolve" — and `--import` still runs ahead of the eval, so the
        // sentinel is a real test of the preload.
        let shim_body = format!(
            "-e:{}",
            format!(
                r#"const fs = require("node:fs");
fs.writeFileSync({marker}, "sentinel=" + String(globalThis.__nubJailStdioShim) +
  " nodeopts=" + String(process.env.NODE_OPTIONS || "").length);"#,
                marker = js_literal(&shim_marker),
            )
            .replace('\n', " ")
        );
        // The STAGED node, not the ambient one: the policy grants only user-owned paths, so a
        // gate that launched `C:\hostedtoolcache\…\node.exe` measured its own ACCESS_DENIED
        // instead of the shim (run 30535709104, `outer=spawn-refused raw=Some(5)` in BOTH arms).
        let shim_run = node_arm(
            &f,
            &policy,
            "shellshim",
            &staged_node,
            &f.work,
            30,
            &shim_body,
        );
        let shim_text =
            std::fs::read_to_string(&shim_marker).unwrap_or_else(|_| "<no marker>".into());
        println!("  fact:shell-stdio-shim={shim_text}");
        report(
            fails,
            "shell-gate-stdio-shim-live",
            shim_text.contains("sentinel=true"),
            &format!(
                "{shim_text} outer={} (the production NODE_OPTIONS must be in force before any shell cell counts)",
                shim_run.outer
            ),
        );

        // GATE 2 + 3: the repair state differs between arms, and confinement holds in both. One
        // fsprobe per arm, on the same paths, so the arms are separated by exactly the seam.
        let gate_ops: Vec<(String, &str, String)> = vec![
            ("croot".into(), "lstat", r"C:\".into()),
            // THE LIVENESS GATE THAT SURVIVES DE-ELEVATION. `C:\` is only ACE-writable by an
            // administrator, so `croot` cannot prove the repair ran for a standard user — but the
            // fixture root is an ancestor of the write grant, sits inside the user's own profile,
            // and carries a PROTECTED dacl (`secure_root` strips the inheritable ALL APPLICATION
            // PACKAGES grant `C:\Users` propagates), so it is refused without the repair and
            // reachable with it whatever the token. One variable, both arms, either principal.
            (
                "profileanc".into(),
                "lstat",
                f.root.to_string_lossy().into_owned(),
            ),
            // Is a SYSTEM interpreter reachable with NO nub grant on it? The policy deliberately
            // omits `cmd.exe`, relying on the read+execute `C:\Windows` already publishes to
            // ALL APPLICATION PACKAGES; this asks the kernel rather than assuming it, and it is
            // the cell that says whether omitting those grants was sound.
            (
                "cmdexe".into(),
                "lstat",
                cmd_exe.to_string_lossy().into_owned(),
            ),
            (
                "ungranted".into(),
                "read",
                f.ungranted.to_string_lossy().into_owned(),
            ),
            (
                "sibling".into(),
                "read",
                f.sibling_file.to_string_lossy().into_owned(),
            ),
        ];
        let gate_off = without_ancestor_repair(|| fsprobe(&f, &policy, "shgate-off", &gate_ops));
        let fallbacks_before = nub_sandbox::windows_capability_fallbacks();
        let gate_on = fsprobe(&f, &policy, "shgate-on", &gate_ops);
        println!(
            "  fact:shell-capability-fallbacks={}",
            nub_sandbox::windows_capability_fallbacks() - fallbacks_before
        );
        println!("  fact:shell-croot-off={}", line(&gate_off, "croot"));
        println!("  fact:shell-croot-on={}", line(&gate_on, "croot"));
        report(
            fails,
            "shell-gate-repair-off-croot-refused",
            denied(&line(&gate_off, "croot")),
            &format!(
                "{} (THE CONTROL: the floor arm must still reproduce the defect)",
                line(&gate_off, "croot")
            ),
        );
        report(
            fails,
            "shell-gate-repair-on-croot-permitted",
            line(&gate_on, "croot").starts_with("ok:"),
            &format!(
                "{} (the ceiling arm's repair must be live — see shell-ace-writable if this fails)",
                line(&gate_on, "croot")
            ),
        );
        println!("  fact:shell-cmdexe-off={}", line(&gate_off, "cmdexe"));
        println!("  fact:shell-cmdexe-on={}", line(&gate_on, "cmdexe"));
        report(
            fails,
            "shell-gate-system-interpreter-reachable-ungranted",
            line(&gate_on, "cmdexe").starts_with("ok:"),
            &format!(
                "cmdexe={} (if this fails, omitting the system-interpreter leaf grants was \
                 unsound and every shell cell below is void)",
                line(&gate_on, "cmdexe")
            ),
        );
        println!(
            "  fact:shell-profileanc-off={}",
            line(&gate_off, "profileanc")
        );
        println!(
            "  fact:shell-profileanc-on={}",
            line(&gate_on, "profileanc")
        );
        report(
            fails,
            "shell-gate-repair-off-profile-ancestor-refused",
            denied(&line(&gate_off, "profileanc")),
            &format!(
                "{} (THE CONTROL for the ancestor ace nub CAN write unprivileged)",
                line(&gate_off, "profileanc")
            ),
        );
        report(
            fails,
            "shell-gate-repair-on-profile-ancestor-permitted",
            line(&gate_on, "profileanc").starts_with("ok:"),
            &format!(
                "{} (the repair's ACE half must be live, or no shell cell in this arm is evidence)",
                line(&gate_on, "profileanc")
            ),
        );
        for (arm, map) in [("off", &gate_off), ("on", &gate_on)] {
            report(
                fails,
                &format!("shell-gate-{arm}-ungranted-read-refused"),
                denied(&line(map, "ungranted")),
                &format!(
                    "{} (a NotFound would mean this arm tested nothing)",
                    line(map, "ungranted")
                ),
            );
        }

        // ── DOES `node <ABSOLUTE FILE>` STILL RESOLVE? ──────────────────────────────────
        //
        // This is what repair 1 EXISTS for, and it is the cell that says whether the repair
        // delivers anything to a standard user. Node's `realpathSync` opens every prefix of the
        // entry path as a TARGET starting at the volume root, so with `C:\` refused it dies in
        // `resolveMainPath` before user code — and `croot-on` above already differs by principal.
        // It also ATTRIBUTES the batteries' `nestedspawn` cell, which is this same operation
        // reached through a shell and would otherwise read as a shell defect.
        std::fs::write(f.work.join("dep.js"), "module.exports = 42;\n").unwrap();
        let entry_marker = f.work.join("entryok.txt");
        let entry_body = format!(
            r#"
const fs = require("node:fs");
const dep = require({dep});
fs.writeFileSync({marker}, "entry=ok require=" + String(dep));
"#,
            dep = js_literal(&f.work.join("dep.js")),
            marker = js_literal(&entry_marker),
        );
        // THE THIRD ARM asks whether a REALPATH PRELOAD rescues resolution where the ancestor
        // repair could not reach. `windows_native_realpath_shim_node_options` is the only such
        // preload in this tree, it is `#[doc(hidden)]`, and its own doc calls it "the REFUTED
        // native-realpath shim" — so this is a re-measurement of a rejected string in the ONE
        // context it was never tried in (an unprivileged token, where the ancestor repair
        // genuinely does not land) rather than a validation of a shipping configuration.
        // `build_jail.rs` stamps exactly ONE term on every branch in this repo; there is no
        // roots-scoped realpath term to compose with.
        let native_options = format!(
            "{node_options} {}",
            nub_sandbox::windows_native_realpath_shim_node_options()
        );
        // THE PRODUCTION TERM, reproduced byte-for-byte rather than approximated. It is a pure
        // function of the shim JS and the roots, so the probe can build the exact string
        // `build_jail.rs` stamps: substitute the roots JSON into the one placeholder, then
        // base64 into a `data:` import, plus the `--preserve-symlinks-main` the entry point needs
        // because `--import` preloads run after `resolveMainPath`. Roots are scoped as production
        // scopes them — `[project_root, package_dir, ...interpreter]`, which here is the work dir
        // (both, since the fixture package IS the project) and the staged interpreter FILE.
        let roots_json = format!(
            "[{}]",
            [f.work.as_path(), staged_node.as_path()]
                .iter()
                .map(|p| format!("{:?}", p.to_string_lossy()))
                .collect::<Vec<_>>()
                .join(",")
        );
        let shim_js = include_str!("fixtures/windows_realpath_shim.js")
            .replace("__NUB_REALPATH_ROOTS_JSON__", &roots_json);
        assert!(
            !shim_js.contains("__NUB_REALPATH_ROOTS_JSON__"),
            "the roots placeholder must be substituted, never appended"
        );
        let prod_options = {
            use base64::Engine as _;
            format!(
                "{node_options} --preserve-symlinks-main --import data:text/javascript;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&shim_js)
            )
        };
        println!("  fact:shell-realpath-roots={roots_json}");
        let mk = |opts: &str| {
            jail_shaped(
                &f,
                vec![read_rule(&bin_dir), read_rule(&staged_node)],
                &[
                    ("NODE_OPTIONS", opts.to_string()),
                    ("PATH", staged_path.clone()),
                ],
            )
        };
        let native_policy = mk(&native_options);
        let realpath_policy = mk(&prod_options);
        // The delivered block is bounded by `CreateProcessW`'s documented 32,767 CHARACTERS for
        // the WHOLE environment, not by the variable — so the ceiling is a property of the
        // policy, and it is reported for both so an overflow is a measured shipping defect
        // rather than a guess.
        println!(
            "  fact:shell-nodeopts-chars-stdio-only={} env-block={}",
            node_options.len(),
            env_block_chars(&policy)
        );
        println!(
            "  fact:shell-nodeopts-chars-with-realpath={} env-block={} ceiling=32767",
            prod_options.len(),
            env_block_chars(&realpath_policy)
        );
        println!(
            "  fact:shell-nodeopts-chars-with-native-shim={} env-block={}",
            native_options.len(),
            env_block_chars(&native_policy)
        );
        report(
            fails,
            "shell-nodeopts-with-realpath-fits-the-env-block",
            env_block_chars(&realpath_policy) <= 32767,
            &format!(
                "{} chars of 32767 (an overflow means production could not stamp both terms \
                 either)",
                env_block_chars(&realpath_policy)
            ),
        );

        for arm in ["off", "on", "on-realpath", "on-native-shim"] {
            let _ = std::fs::remove_file(&entry_marker);
            let arm_policy = match arm {
                "on-realpath" => &realpath_policy,
                "on-native-shim" => &native_policy,
                _ => &policy,
            };
            let go = || {
                node_arm(
                    &f,
                    arm_policy,
                    &format!("shellentry-{arm}"),
                    &staged_node,
                    &f.work,
                    30,
                    &entry_body,
                )
            };
            let run = if arm == "off" {
                without_ancestor_repair(go)
            } else {
                go()
            };
            let text =
                std::fs::read_to_string(&entry_marker).unwrap_or_else(|_| "<no marker>".into());
            println!(
                "  fact:shell-node-entry-{arm}={} outer={}",
                text.trim(),
                run.outer
            );
            let resolved = text.contains("entry=ok");
            if arm == "off" {
                // THE CONTROL: unrepaired, this must still die, or the repaired cell beside it
                // is measuring nothing.
                report(
                    fails,
                    "shell-gate-node-absolute-entry-refused-repair-off",
                    !resolved,
                    &format!(
                        "{} (the floor must still reproduce the realpath defect)",
                        text.trim()
                    ),
                );
            } else if arm == "on-realpath" {
                // THE ANSWER. In the ELEVATED arm this doubles as the POSITIVE CONTROL for the
                // whole port: the ancestor repair already landed there, so the shim's tolerance
                // rule never fires and the entry point must still resolve. If it does, the term
                // is well-formed, base64-clean and actually delivered — which is what makes a
                // de-elevated FAIL attributable to the mechanism instead of to this encoding.
                report(
                    fails,
                    "shell-node-absolute-entry-resolves-repair-on-plus-realpath-shim",
                    resolved,
                    &format!(
                        "{} (repair on PLUS the PRODUCTION roots-scoped realpath term)",
                        text.trim()
                    ),
                );
            } else if arm == "on-native-shim" {
                // The REFUTED native shim, kept beside it so the two mechanisms are separated in
                // one run rather than conflated: this one points realpath at
                // `GetFinalPathNameByHandleW`, which the AppContainer also refuses.
                report(
                    fails,
                    "shell-node-absolute-entry-resolves-repair-on-plus-native-shim",
                    resolved,
                    &format!("{} (the refuted native-realpath shim)", text.trim()),
                );
            } else {
                // A MEASUREMENT: whether repair 1 reaches far enough for THIS principal.
                report(
                    fails,
                    "shell-node-absolute-entry-resolves-repair-on",
                    resolved,
                    &format!(
                        "{} (repair on — a FAIL means repair 1 does not reach this token)",
                        text.trim()
                    ),
                );
            }
        }

        // ── the batteries ─────────────────────────────────────────────────────────────
        let mut ps_reference: Option<BTreeMap<String, String>> = None;
        for plan in &plans {
            println!("  ---- shell {} ----", plan.name);
            println!(
                "  fact:shell-{}-leaf-grant-redundant={}",
                plan.name,
                nub_sandbox::windows_leaf_grant_redundant(&plan.exe)
            );
            let reference = shell_arm_unconfined(&f, plan, &outroot, &policy);
            if plan.name == "powershell" {
                ps_reference = Some(reference.clone());
            }
            let floor =
                without_ancestor_repair(|| shell_arm_confined(&f, &policy, plan, &outroot, "off"));
            let ceiling = shell_arm_confined(&f, &policy, plan, &outroot, "on");

            for (arm, map) in [
                ("unconfined", &reference),
                ("off", &floor),
                ("on", &ceiling),
            ] {
                for label in SHELL_LABELS {
                    println!(
                        "  fact:shell-{}-{arm}-{label}={}",
                        plan.name,
                        line(map, label)
                    );
                }
            }

            // The reference is the only GATE here: a shell that cannot complete the battery
            // UNCONFINED means the fixture or the script is wrong, and every confined cell
            // beside it would be uninterpretable.
            let complete = line(&reference, "alive") == "1" && line(&reference, "done") == "1";
            report(
                fails,
                &format!("shell-{}-unconfined-completes", plan.name),
                complete,
                "the unconfined reference must run the whole battery or the confined arms mean nothing",
            );

            // MEASUREMENTS, NOT REQUIREMENTS. A divergence here is the finding this group
            // exists to produce, so it is reported as a property whose FAIL is informative and
            // gated only on being PRESENT.
            for (arm, map) in [("off", &floor), ("on", &ceiling)] {
                // `cwd`/`cwdafter` are excluded from the diff because `%TEMP%` reaches the child
                // 8.3-shortened (`RUNNER~1`) on one arm and long on another; they are reported as
                // facts instead. Every other cell must match byte for byte.
                let diffs: Vec<String> = SHELL_LABELS
                    .iter()
                    .copied()
                    .filter(|l| *l != "cwd" && *l != "cwdafter" && *l != "cdungranted")
                    .filter(|l| line(map, l) != line(&reference, l))
                    .map(|l| format!("{l}:{}!={}", line(map, l), line(&reference, l)))
                    .collect();
                println!(
                    "  diag:shell-{}-{arm}-diff={}",
                    plan.name,
                    if diffs.is_empty() {
                        "none".to_string()
                    } else {
                        diffs.join(" | ")
                    }
                );
                // `complete` is load-bearing here, not decoration: run 30531385502 reported
                // `0 diverging cell(s)` for cmd when BOTH arms were empty, because the harness
                // had mis-quoted the invocation. An empty transcript must never read as
                // agreement.
                report(
                    fails,
                    &format!("shell-{}-{arm}-matches-unconfined", plan.name),
                    complete && diffs.is_empty(),
                    &format!(
                        "{} diverging cell(s){}",
                        diffs.len(),
                        if complete {
                            ""
                        } else {
                            " — VACUOUS: the unconfined reference never completed"
                        }
                    ),
                );
            }
            // `cd` into an UNGRANTED directory is the one cell whose CORRECT confined answer
            // differs from unconfined: it must fail. Asserted separately so the diff above is
            // not permanently one cell dirty, and so a shell that lets it through is caught.
            report(
                fails,
                &format!("shell-{}-on-ungranted-cd-refused", plan.name),
                line(&ceiling, "cdungranted") == "FAIL",
                &format!(
                    "cdungranted={} (unconfined={})",
                    line(&ceiling, "cdungranted"),
                    line(&reference, "cdungranted")
                ),
            );
        }

        // ── ATTRIBUTING WINDOWS POWERSHELL 5.1's FAILURE ─────────────────────────────
        //
        // Run 30532139228 measured every FileSystem-provider cell failing in BOTH arms with
        // `CommandNotFoundException: The term 'Test-Path' is not recognized` — the cmdlet is
        // MISSING, not refused. `Test-Path`, `Get-ChildItem`, `Get-Content`, `Set-Content` and
        // `Set-Location` all live in the `Microsoft.PowerShell.Management` MODULE under
        // `$PSHOME\Modules`, while `Get-Command` — the one cell that worked — is in the
        // always-loaded `Microsoft.PowerShell.Core` snap-in. That says a missing READ GRANT, not
        // an OS limit, and the difference decides whether 5.1 is fixable or disqualified. One
        // variable: the same battery, the same repair, plus a read grant on `$PSHOME`.
        if let (Some(reference), Some(plan)) = (
            ps_reference.as_ref(),
            plans.iter().find(|p| p.name == "powershell"),
        ) {
            let pshome = plan.exe.parent().map(Path::to_path_buf);
            if let Some(pshome) = pshome {
                let mut grants = vec![read_rule(node), read_rule(&pshome)];
                for extra in [
                    Some(cmd_exe.clone()),
                    node.parent().map(Path::to_path_buf),
                    staged_busybox.clone(),
                ]
                .into_iter()
                .flatten()
                {
                    grants.push(read_rule(&extra));
                }
                let ps_policy = jail_shaped(&f, grants, &[("NODE_OPTIONS", node_options.clone())]);
                println!("  fact:shell-psmodules-grant={}", pshome.display());
                let arm = shell_arm_confined(&f, &ps_policy, plan, &outroot, "psmodules");
                for label in SHELL_LABELS {
                    println!(
                        "  fact:shell-powershell-psmodules-{label}={}",
                        line(&arm, label)
                    );
                }
                let diffs: Vec<String> = SHELL_LABELS
                    .iter()
                    .copied()
                    .filter(|l| *l != "cwd" && *l != "cwdafter" && *l != "cdungranted")
                    .filter(|l| line(&arm, l) != line(reference, l))
                    .map(|l| format!("{l}:{}!={}", line(&arm, l), line(reference, l)))
                    .collect();
                println!(
                    "  diag:shell-powershell-psmodules-diff={}",
                    if diffs.is_empty() {
                        "none".to_string()
                    } else {
                        diffs.join(" | ")
                    }
                );
                report(
                    fails,
                    "shell-powershell-psmodules-matches-unconfined",
                    diffs.is_empty(),
                    &format!(
                        "{} diverging cell(s) with $PSHOME granted read (a PASS attributes 5.1's \
                         breakage to a missing grant rather than to the jail)",
                        diffs.len()
                    ),
                );
            }
        }
    }

    /// The battery with NO jail at all: same script, same cwd, same constructed env, same
    /// bound. This is what every confined cell is judged against.
    fn shell_arm_unconfined(
        f: &Fixture,
        plan: &ShellPlan,
        outroot: &Path,
        policy: &SandboxPolicy,
    ) -> BTreeMap<String, String> {
        let dir = outroot.join(format!("{}-unconfined", plan.name));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("battery.txt");
        let sink = dir.join("stdio.txt");
        let argv = (plan.invoke)(&plan.script, &out);
        breadcrumb(&format!("shell_arm_unconfined {}", plan.name));
        flush();
        let handle = std::fs::File::create(&sink).unwrap();
        let status = std::process::Command::new(&plan.exe)
            .args(&argv)
            .current_dir(&f.work)
            .env_clear()
            .envs(&policy.env.constructed)
            .stderr(std::process::Stdio::from(handle.try_clone().unwrap()))
            .stdout(std::process::Stdio::from(handle))
            .status();
        println!(
            "  fact:shell-{}-unconfined-outer={}",
            plan.name,
            match &status {
                Ok(s) => format!("EXITED code={:?}", s.code()),
                Err(e) => format!("SPAWN-FAILED {e:?}"),
            }
        );
        shell_report(plan.name, "unconfined", &out, &sink)
    }

    /// The battery CONFINED. The shell is a grandchild of the launch rather than the launch's
    /// own program, purely so the wait can be BOUNDED: `apply` owns spawn+wait synchronously, so
    /// a hang there can only be caught by the 120-second watchdog, which ends the whole run. The
    /// token is identical either way — it is inherited — and a direct-launch arm runs last in
    /// `probe_main` for the production process shape.
    fn shell_arm_confined(
        f: &Fixture,
        policy: &SandboxPolicy,
        plan: &ShellPlan,
        outroot: &Path,
        arm: &str,
    ) -> BTreeMap<String, String> {
        let dir = outroot.join(format!("{}-{arm}", plan.name));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("battery.txt");
        let sink = dir.join("stdio.txt");
        let outer = dir.join("outer.txt");
        let mut argv = vec![
            "30".to_string(),
            sink.to_string_lossy().into_owned(),
            plan.exe.to_string_lossy().into_owned(),
        ];
        argv.extend((plan.invoke)(&plan.script, &out));
        let (code, outer_text) = run_jailed(f, policy, &f.work, "exec", &outer, &argv);
        println!(
            "  fact:shell-{}-{arm}-outer={outer_text} launch={code}",
            plan.name
        );
        shell_report(plan.name, arm, &out, &sink)
    }

    /// Read one arm's transcript, echoing its interpreter stdio as a diagnostic — a shell that
    /// wrote nothing has its reason there and nowhere else.
    fn shell_report(name: &str, arm: &str, out: &Path, sink: &Path) -> BTreeMap<String, String> {
        let text = std::fs::read_to_string(out).unwrap_or_default();
        let stdio = std::fs::read_to_string(sink).unwrap_or_default();
        if text.trim().is_empty() {
            println!("  fact:shell-{name}-{arm}-transcript=EMPTY");
        }
        if !stdio.trim().is_empty() {
            println!(
                "  diag:shell-{name}-{arm}-stdio={}",
                stdio.trim().replace(['\r', '\n'], " | ")
            );
        }
        shell_results(&text)
    }

    /// `cmd.exe` battery. Written as separate top-level statements rather than `&&` chains
    /// because a parenthesised block is expanded BEFORE the command inside it runs, so `%CD%`
    /// after a `cd` would report the old directory. `(call )` resets `%errorlevel%` to 0 ahead
    /// of each op: a SUCCESSFUL builtin does not reset it, which is the staleness artifact that
    /// produced a retracted `>NUL` finding in an earlier round.
    fn cmd_battery(f: &Fixture, sub: &Path, globdir: &Path) -> String {
        let w = f.work.display().to_string();
        let sib = f.sibling.display().to_string();
        let g = globdir.display().to_string();
        let s = sub.display().to_string();
        format!(
            "@echo off\r\n\
             setlocal enableextensions\r\n\
             set \"OUT=%~1\"\r\n\
             for %%i in (\"%OUT%\") do set \"SC=%%~dpi\"\r\n\
             >>\"%OUT%\" echo alive=1\r\n\
             >>\"%OUT%\" echo cwd=%CD%\r\n\
             if exist \"{w}\" (>>\"%OUT%\" echo existdir=yes) else (>>\"%OUT%\" echo existdir=no)\r\n\
             if exist \"{w}\\\" (>>\"%OUT%\" echo existdirslash=yes) else (>>\"%OUT%\" echo existdirslash=no)\r\n\
             if exist \"{w}\\granted.txt\" (>>\"%OUT%\" echo existfile=yes) else (>>\"%OUT%\" echo existfile=no)\r\n\
             if exist \"%SystemRoot%\" (>>\"%OUT%\" echo existwin=yes) else (>>\"%OUT%\" echo existwin=no)\r\n\
             if exist \"%ProgramFiles%\" (>>\"%OUT%\" echo existpf=yes) else (>>\"%OUT%\" echo existpf=no)\r\n\
             (call )\r\n\
             cd /d \"{s}\"\r\n\
             set \"RC=%errorlevel%\"\r\n\
             if \"%RC%\"==\"0\" (>>\"%OUT%\" echo cdgranted=OK) else (>>\"%OUT%\" echo cdgranted=FAIL)\r\n\
             >>\"%OUT%\" echo cwdafter=%CD%\r\n\
             (call )\r\n\
             cd /d \"{sib}\"\r\n\
             set \"RC=%errorlevel%\"\r\n\
             if \"%RC%\"==\"0\" (>>\"%OUT%\" echo cdungranted=OK-BAD) else (>>\"%OUT%\" echo cdungranted=FAIL)\r\n\
             (call )\r\n\
             dir /b \"{sib}\" > \"%SC%cul.tmp\" 2>&1\r\n\
             set \"RC=%errorlevel%\"\r\n\
             if \"%RC%\"==\"0\" (>>\"%OUT%\" echo cdungrantedlist=READABLE) else (>>\"%OUT%\" echo cdungrantedlist=FAIL)\r\n\
             cd /d \"{w}\"\r\n\
             (call )\r\n\
             dir /b \"{g}\" > \"%SC%ls.tmp\" 2>&1\r\n\
             set \"RC=%errorlevel%\"\r\n\
             if \"%RC%\"==\"0\" (for /f \"usebackq delims=\" %%l in (\"%SC%ls.tmp\") do >>\"%OUT%\" echo listitem=%%l) else (>>\"%OUT%\" echo listitem=FAIL)\r\n\
             (call )\r\n\
             for %%f in (\"{g}\\*.txt\") do >>\"%OUT%\" echo globitem=%%~nxf\r\n\
             (call )\r\n\
             type \"{w}\\granted.txt\" > \"%SC%rd.tmp\" 2>&1\r\n\
             set \"RC=%errorlevel%\"\r\n\
             set \"V=\"\r\n\
             for /f \"usebackq delims=\" %%l in (\"%SC%rd.tmp\") do if not defined V set \"V=%%l\"\r\n\
             if \"%RC%\"==\"0\" (>>\"%OUT%\" echo read=%V%) else (>>\"%OUT%\" echo read=FAIL rc=%RC%)\r\n\
             (call )\r\n\
             >\"%SC%wr.tmp\" echo wrote-ok\r\n\
             set \"RC=%errorlevel%\"\r\n\
             set \"V=\"\r\n\
             for /f \"usebackq delims=\" %%l in (\"%SC%wr.tmp\") do if not defined V set \"V=%%l\"\r\n\
             if \"%RC%\"==\"0\" (>>\"%OUT%\" echo write=%V%) else (>>\"%OUT%\" echo write=FAIL rc=%RC%)\r\n\
             (call )\r\n\
             where.exe node.exe > \"%SC%wh.tmp\" 2>&1\r\n\
             set \"RC=%errorlevel%\"\r\n\
             set \"V=\"\r\n\
             for /f \"usebackq delims=\" %%l in (\"%SC%wh.tmp\") do if not defined V set \"V=%%~nxl\"\r\n\
             if \"%RC%\"==\"0\" (>>\"%OUT%\" echo whereexe=%V%) else (>>\"%OUT%\" echo whereexe=FAIL rc=%RC%)\r\n\
             (call )\r\n\
             node --version > \"%SC%nv.tmp\" 2>&1\r\n\
             set \"RC=%errorlevel%\"\r\n\
             set \"V=\"\r\n\
             for /f \"usebackq delims=\" %%l in (\"%SC%nv.tmp\") do if not defined V set \"V=%%l\"\r\n\
             if \"%RC%\"==\"0\" (>>\"%OUT%\" echo pathspawn=%V%) else (>>\"%OUT%\" echo pathspawn=FAIL rc=%RC%)\r\n\
             (call )\r\n\
             echo nulltest > NUL\r\n\
             set \"RC=%errorlevel%\"\r\n\
             if \"%RC%\"==\"0\" (>>\"%OUT%\" echo nullredir=OK) else (>>\"%OUT%\" echo nullredir=FAIL rc=%RC%)\r\n\
             (call )\r\n\
             if exist \"%SC%token\" del /q \"%SC%token\"\r\n\
             node \"{w}\\spawned.js\" \"%SC%token\" > \"%SC%sp.tmp\" 2>&1\r\n\
             set \"RC=%errorlevel%\"\r\n\
             set \"V=\"\r\n\
             for /f \"usebackq delims=\" %%l in (\"%SC%token\") do if not defined V set \"V=%%l\"\r\n\
             if not \"%RC%\"==\"0\" (>>\"%OUT%\" echo nestedspawn=FAIL rc=%RC%) else if defined V (>>\"%OUT%\" echo nestedspawn=%V%) else (>>\"%OUT%\" echo nestedspawn=NO-TOKEN)\r\n\
             (call )\r\n\
             call \"{w}\\shim.cmd\" > \"%SC%sh.tmp\" 2>&1\r\n\
             set \"RC=%errorlevel%\"\r\n\
             set \"V=\"\r\n\
             for /f \"usebackq delims=\" %%l in (\"%SC%sh.tmp\") do if not defined V set \"V=%%l\"\r\n\
             if \"%RC%\"==\"0\" (>>\"%OUT%\" echo cmdshim=%V%) else (>>\"%OUT%\" echo cmdshim=FAIL rc=%RC%)\r\n\
             (call )\r\n\
             cmd.exe /d /s /c \"exit /b 3\"\r\n\
             set \"RC=%errorlevel%\"\r\n\
             if \"%RC%\"==\"3\" (>>\"%OUT%\" echo exitprop=OK) else (>>\"%OUT%\" echo exitprop=FAIL rc=%RC%)\r\n\
             >>\"%OUT%\" echo done=1\r\n"
        )
    }

    /// PowerShell battery, run by both Windows PowerShell 5.1 and pwsh 7.
    ///
    /// THE TRANSPORT IS SEPARATED FROM THE MEASUREMENT, deliberately. `L` writes through
    /// `[IO.File]::AppendAllText`, which is a raw .NET call and not a FileSystem PROVIDER
    /// operation — the exact surface `PowerShell/PowerShell#27253` breaks when
    /// `GetFileAttributesEx("C:\")` returns ACCESS_DENIED. Reporting through the provider would
    /// mean provider breakage erased the whole table instead of failing one cell; `write` is
    /// the cell that measures the provider, on purpose.
    fn ps_battery(f: &Fixture, sub: &Path, globdir: &Path, cmd_exe: &Path) -> String {
        let w = f.work.display().to_string();
        let sib = f.sibling.display().to_string();
        let g = globdir.display().to_string();
        let s = sub.display().to_string();
        let c = cmd_exe.display().to_string();
        // Substituted rather than read from `$env:SystemRoot`: with `$ErrorActionPreference =
        // 'Continue'` a null argument makes `Test-Path` emit a non-terminating error and return
        // NOTHING, which the `if` reads as `no` — a missing env var would be indistinguishable
        // from a refused path. Caught in a dry run on a host where those vars do not exist.
        let winroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let progfiles =
            std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
        format!(
            r#"param([string]$Out)
$ErrorActionPreference = 'Continue'
function L([string]$s) {{ [System.IO.File]::AppendAllText($Out, $s + "`r`n") }}
function T([string]$k) {{ $e = $Error[0]; $m = ''; if ($e) {{ $m = ' ' + $e.Exception.GetType().Name + ':' + (($e.Exception.Message -replace '[\r\n]+',' ') -replace '=','~') }}; L ($k + '=THREW' + $m.Substring(0, [Math]::Min(160, $m.Length))) }}
$SC = [System.IO.Path]::GetDirectoryName($Out)
L 'alive=1'
try {{ L ('cwd=' + (Get-Location).Path) }} catch {{ T 'cwd' }}
foreach ($pair in @(,@('existdir','{w}')) + @(,@('existdirslash','{w}\')) + @(,@('existfile','{w}\granted.txt')) + @(,@('existwin','{winroot}')) + @(,@('existpf','{progfiles}'))) {{
  try {{ L ($pair[0] + '=' + $(if (Test-Path -LiteralPath $pair[1]) {{'yes'}} else {{'no'}})) }} catch {{ T $pair[0] }}
}}
try {{ Set-Location -LiteralPath '{s}' -ErrorAction Stop; L 'cdgranted=OK' }} catch {{ L 'cdgranted=FAIL' }}
try {{ L ('cwdafter=' + (Get-Location).Path) }} catch {{ T 'cwdafter' }}
try {{ Set-Location -LiteralPath '{sib}' -ErrorAction Stop; L 'cdungranted=OK-BAD' }} catch {{ L 'cdungranted=FAIL' }}
try {{ L ('cdungrantedlist=' + @(Get-ChildItem -LiteralPath '{sib}' -ErrorAction Stop).Count) }} catch {{ L 'cdungrantedlist=FAIL' }}
try {{ Set-Location -LiteralPath '{w}' -ErrorAction Stop }} catch {{ }}
try {{ foreach ($e in @(Get-ChildItem -LiteralPath '{g}' -ErrorAction Stop)) {{ L ('listitem=' + $e.Name) }} }} catch {{ L 'listitem=FAIL' }}
try {{ foreach ($e in @(Get-ChildItem -Path '{g}\*.txt' -ErrorAction Stop)) {{ L ('globitem=' + $e.Name) }} }} catch {{ L 'globitem=FAIL' }}
try {{ L ('read=' + ((Get-Content -LiteralPath '{w}\granted.txt' -Raw -ErrorAction Stop).Trim())) }} catch {{ L 'read=FAIL' }}
try {{ Set-Content -LiteralPath ([System.IO.Path]::Combine($SC,'ps-wr.tmp')) -Value 'wrote-ok' -ErrorAction Stop
       L ('write=' + ((Get-Content -LiteralPath ([System.IO.Path]::Combine($SC,'ps-wr.tmp')) -Raw -ErrorAction Stop).Trim())) }} catch {{ L 'write=FAIL' }}
try {{ $cn = @(Get-Command node -CommandType Application -ErrorAction Stop)[0]; L ('whereexe=' + [System.IO.Path]::GetFileName($cn.Source)) }} catch {{ L 'whereexe=FAIL' }}
try {{ $v = @(& node --version 2>&1); if ($LASTEXITCODE -eq 0 -and $v.Count -gt 0) {{ L ('pathspawn=' + [string]$v[0]) }} else {{ L ('pathspawn=FAIL last=[' + [string]$LASTEXITCODE + '] out=[' + ([string]::Join(' ', $v)) + ']') }} }} catch {{ T 'pathspawn' }}
try {{ [System.IO.File]::AppendAllText('\\.\NUL', 'nulltest'); L 'nullredir=OK' }} catch {{ L 'nullredir=FAIL' }}
try {{ $tk = [System.IO.Path]::Combine($SC,'token'); if (Test-Path -LiteralPath $tk) {{ Remove-Item -LiteralPath $tk -Force }} }} catch {{ }}
try {{ $null = @(& node '{w}\spawned.js' $tk 2>&1)
       if ($LASTEXITCODE -ne 0) {{ L ('nestedspawn=FAIL last=[' + [string]$LASTEXITCODE + ']') }}
       elseif (Test-Path -LiteralPath $tk) {{ L ('nestedspawn=' + (([System.IO.File]::ReadAllText($tk)).Trim())) }}
       else {{ L 'nestedspawn=NO-TOKEN' }} }} catch {{ T 'nestedspawn' }}
try {{ $o = @(& '{c}' /d /s /c ('"' + '{w}\shim.cmd' + '"') 2>&1); if ($LASTEXITCODE -eq 0 -and $o.Count -gt 0) {{ L ('cmdshim=' + ([string]$o[0]).Trim()) }} else {{ L ('cmdshim=FAIL last=[' + [string]$LASTEXITCODE + ']') }} }} catch {{ T 'cmdshim' }}
try {{ $null = @(& '{c}' /d /s /c 'exit /b 3' 2>&1); L ('exitprop=' + $(if ($LASTEXITCODE -eq 3) {{'OK'}} else {{'FAIL last=[' + [string]$LASTEXITCODE + ']'}})) }} catch {{ T 'exitprop' }}
L 'done=1'
"#
        )
    }

    /// busybox-w32 `sh` battery. Forward-slash paths throughout: busybox resolves those against
    /// the Windows namespace, and a backslash is an escape to it.
    fn sh_battery(f: &Fixture, sub: &Path, globdir: &Path, cmd_exe: &Path) -> String {
        let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let w = fwd(&f.work);
        let sib = fwd(&f.sibling);
        let g = fwd(globdir);
        let s = fwd(sub);
        let cwin = cmd_exe.display().to_string();
        // The system paths are substituted from Rust rather than read out of `$SystemRoot`,
        // whose value is BACKSLASHED — busybox reads a backslash as an escape, so an env-var
        // spelling would make `[ -d ]` report absent for a reason that is not the jail.
        let winroot = fwd(Path::new(
            &std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()),
        ));
        let progfiles = fwd(Path::new(
            &std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into()),
        ));
        // The `.cmd` shim's path is handed to `cmd.exe`, which reads a leading `/` as a switch,
        // so that one argument stays backslashed while everything busybox itself resolves is
        // forward-slashed.
        let shim_win = f.work.join("shim.cmd").display().to_string();
        format!(
            r#"OUT="$1"
SC=$(dirname "$OUT")
p() {{ printf '%s\n' "$1" >> "$OUT"; }}
p alive=1
p "cwd=$(pwd)"
if [ -d "{w}" ]; then p existdir=yes; else p existdir=no; fi
if [ -d "{w}/" ]; then p existdirslash=yes; else p existdirslash=no; fi
if [ -f "{w}/granted.txt" ]; then p existfile=yes; else p existfile=no; fi
if [ -d "{winroot}" ]; then p existwin=yes; else p existwin=no; fi
if [ -d "{progfiles}" ]; then p existpf=yes; else p existpf=no; fi
E="$SC/err.log"
if cd "{s}" 2>>"$E"; then p cdgranted=OK; else p cdgranted=FAIL; fi
p "cwdafter=$(pwd)"
if cd "{sib}" 2>>"$E"; then p cdungranted=OK-BAD; else p cdungranted=FAIL; fi
if ls -1 . > "$SC/cul.tmp" 2>>"$E"; then p "cdungrantedlist=$(wc -l < "$SC/cul.tmp" | tr -d ' ')"; else p cdungrantedlist=FAIL; fi
cd "{w}" 2>>"$E"
if ls -1 "{g}" > "$SC/ls.tmp" 2>>"$E"; then while read -r n; do p "listitem=$n"; done < "$SC/ls.tmp"; else p listitem=FAIL; fi
for x in "{g}"/*.txt; do if [ -e "$x" ]; then p "globitem=$(basename "$x")"; else p globitem=FAIL; fi; done
if cat "{w}/granted.txt" > "$SC/rd.tmp" 2>>"$E"; then p "read=$(cat "$SC/rd.tmp")"; else p read=FAIL; fi
if printf 'wrote-ok\n' > "$SC/wr.tmp" 2>>"$E"; then p "write=$(cat "$SC/wr.tmp")"; else p write=FAIL; fi
if wexe=$(command -v node 2>>"$E"); then p "whereexe=$(basename "$wexe")"; else p whereexe=FAIL; fi
if node --version > "$SC/nv.tmp" 2>>"$E"; then p "pathspawn=$(cat "$SC/nv.tmp")"; else p pathspawn=FAIL; fi
if printf 'nulltest\n' > /dev/null 2>>"$E"; then p nullredir=OK; else p nullredir=FAIL; fi
rm -f "$SC/token"
if node "{w}/spawned.js" "$SC/token" > "$SC/sp.tmp" 2>&1; then
  if [ -f "$SC/token" ]; then p "nestedspawn=$(cat "$SC/token")"; else p nestedspawn=NO-TOKEN; fi
else p "nestedspawn=FAIL rc=$?"; fi
if "{cwin}" /d /s /c "{shim_win}" > "$SC/sh.tmp" 2>&1; then p "cmdshim=$(head -n 1 "$SC/sh.tmp" | tr -d '\r')"; else p cmdshim=FAIL; fi
"{cwin}" /d /s /c "exit /b 3" > "$SC/ep.tmp" 2>&1
rc=$?
if [ "$rc" = 3 ]; then p exitprop=OK; else p "exitprop=FAIL rc=$rc"; fi
p done=1
"#
        )
    }

    /// `cmd.exe` as the AppContainer child ITSELF — the production process shape
    /// (`aube-scripts` runs `cmd.exe /d /s /c "<script body>"`). The grandchild arms above
    /// answer WHAT the shell can do; this one answers whether the shell can be the confined
    /// program at all, which is a different question and the one production actually asks.
    ///
    /// UNBOUNDED BY CONSTRUCTION, hence last: `apply` spawns and waits synchronously in this
    /// process, so a hang is only visible as the watchdog's `watchdog-stalled-at` line.
    fn shell_direct_launch(fails: &mut u32) {
        let f = Fixture::new("shd");
        let cmd_exe =
            PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()))
                .join(r"System32\cmd.exe");
        if !cmd_exe.is_file() {
            println!("  fact:shell-direct-cmd-present=false");
            return;
        }
        let out = f.work.join("direct.txt");
        std::fs::write(
            f.work.join("direct.cmd"),
            "@echo off\r\n>>\"%~1\" echo alive=1\r\n>>\"%~1\" echo cwd=%CD%\r\n>>\"%~1\" echo done=1\r\n",
        )
        .unwrap();
        let policy = jail_shaped(
            &f,
            vec![read_rule(&cmd_exe)],
            &[(
                "NODE_OPTIONS",
                nub_sandbox::windows_build_jail_node_options(),
            )],
        );
        let argv = vec![
            "/d".to_string(),
            "/s".to_string(),
            "/c".to_string(),
            f.work.join("direct.cmd").display().to_string(),
            out.display().to_string(),
        ];
        breadcrumb("shell_direct_launch");
        flush();
        let spec = CommandSpec::new(cmd_exe.as_os_str())
            .args(argv)
            .cwd(f.work.as_path());
        let outcome = apply(&policy, spec).map(|p| p.status());
        let code = match outcome {
            Ok(Ok(status)) => format!("EXITED code={:?}", status.code()),
            Ok(Err(error)) => format!("LAUNCH-FAILED {error}"),
            Err(degradation) => format!("POLICY-REJECTED {degradation:?}"),
        };
        let transcript = std::fs::read_to_string(&out).unwrap_or_default();
        println!("  fact:shell-direct-outer={code}");
        println!(
            "  fact:shell-direct-transcript={}",
            if transcript.trim().is_empty() {
                "EMPTY".to_string()
            } else {
                transcript.trim().replace(['\r', '\n'], " | ")
            }
        );
        let results = shell_results(&transcript);
        report(
            fails,
            "shell-direct-cmd-is-the-confined-program",
            line(&results, "alive") == "1" && line(&results, "done") == "1",
            &format!(
                "alive={} done={} cwd={} outer={code}",
                line(&results, "alive"),
                line(&results, "done"),
                line(&results, "cwd")
            ),
        );
    }

    // ── DE-ELEVATION: measuring the SHIPPING principal instead of the runner's ───────
    //
    // Every Windows cell to date was taken as `runneradmin`, which holds `WRITE_DAC` on `C:\`
    // and `C:\Users`. So the ancestor repair reached ancestors an ordinary user's token cannot,
    // and the repaired arm was a CEILING rather than the shipping configuration. This group
    // re-runs the WHOLE shell battery under a principal with no administrative authority — in
    // the SAME session and window station, because a LowBox launch from session 0 returns
    // 0xC0000142 unconditionally (§5e), which is also why CI is the only venue.
    //
    // WHY A RESTRICTED TOKEN AND NOT A SECOND ACCOUNT. `windows-latest`'s `runneradmin` is
    // `TokenElevationTypeDefault`, so there is no linked standard-user token to borrow
    // (measured, run 30424255514), and a freshly created local account has no logon session on
    // this window station — `CreateProcessAsUserW` with a `LogonUserW` token lands it in session
    // 0, where the launch cannot succeed for reasons that have nothing to do with the repair.
    // `CreateRestrictedToken` with `BUILTIN\Administrators` reduced to DENY-ONLY,
    // `DISABLE_MAX_PRIVILEGE`, and a drop to medium integrity is STRICTLY MORE confined than a
    // standard user on the group, privilege and integrity axes, and IDENTICAL on the axis that
    // decides this question: `WRITE_DAC` on `C:\` is reachable only through the Administrators
    // ACE or the owner check, and a deny-only SID satisfies neither.
    //
    // WHAT IT DOES NOT EMULATE, on the record: the user PROFILE stays `runneradmin`'s (`HKCU`,
    // `%LOCALAPPDATA%`, `%TEMP%`). That is the one axis the ancestor chain provably does not
    // depend on — a real standard user owns their own profile too, so the chain's shape is the
    // same either way: writable from `%USERPROFILE%` down, refused at `C:\Users` and `C:\`.

    /// Whether this process WIELDS administrative authority, decided by an access check.
    ///
    /// `TokenIsElevated` is not a substitute: `CreateRestrictedToken` COPIES that flag rather
    /// than recomputing it, so the de-elevated token reports `IsElevated=1` while being unable
    /// to do anything an administrator can. `SC_MANAGER_CREATE_SERVICE` is the canonical probe —
    /// the SCM grants it to Administrators only, a deny-only SID does not match, and the call
    /// mutates nothing.
    fn admin_authority() -> bool {
        use windows_sys::Win32::System::Services::{
            CloseServiceHandle, OpenSCManagerW, SC_MANAGER_CREATE_SERVICE,
        };
        // SAFETY: opens the local SCM for a right that is never exercised; NULL machine/database.
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

    /// This process's integrity RID (`8192` medium, `12288` high) and the names of every
    /// privilege still in its token. Both are reported so "de-elevated" is observed rather than
    /// asserted — the restricted token should be at medium with `SeChangeNotifyPrivilege` alone.
    fn token_shape() -> (u32, String) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{
            GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, LUID_AND_ATTRIBUTES,
            LookupPrivilegeNameW, TOKEN_MANDATORY_LABEL, TOKEN_PRIVILEGES, TOKEN_QUERY,
            TokenIntegrityLevel, TokenPrivileges,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        // SAFETY: each class is queried size-first, then into an exactly-sized buffer; the token
        // handle is closed on every path.
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return (0, "open-token-failed".into());
            }
            let mut need = 0u32;
            GetTokenInformation(
                token,
                TokenIntegrityLevel,
                std::ptr::null_mut(),
                0,
                &mut need,
            );
            let mut buf = vec![0u8; need.max(4) as usize];
            let mut il = 0u32;
            if GetTokenInformation(
                token,
                TokenIntegrityLevel,
                buf.as_mut_ptr().cast(),
                need,
                &mut need,
            ) != 0
            {
                let label = buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
                let sid = (*label).Label.Sid;
                let count = *GetSidSubAuthorityCount(sid);
                if count > 0 {
                    il = *GetSidSubAuthority(sid, u32::from(count) - 1);
                }
            }
            let mut need = 0u32;
            GetTokenInformation(token, TokenPrivileges, std::ptr::null_mut(), 0, &mut need);
            let mut buf = vec![0u8; need.max(4) as usize];
            let mut names: Vec<String> = Vec::new();
            if GetTokenInformation(
                token,
                TokenPrivileges,
                buf.as_mut_ptr().cast(),
                need,
                &mut need,
            ) != 0
            {
                let header = buf.as_ptr().cast::<TOKEN_PRIVILEGES>();
                let entries =
                    std::ptr::addr_of!((*header).Privileges).cast::<LUID_AND_ATTRIBUTES>();
                for i in 0..(*header).PrivilegeCount as usize {
                    let mut wide = [0u16; 128];
                    let mut len = wide.len() as u32;
                    if LookupPrivilegeNameW(
                        std::ptr::null(),
                        &(*entries.add(i)).Luid,
                        wide.as_mut_ptr(),
                        &mut len,
                    ) != 0
                    {
                        names.push(String::from_utf16_lossy(&wide[..len as usize]));
                    }
                }
            }
            CloseHandle(token);
            names.sort();
            (il, names.join(","))
        }
    }

    /// Drop `token` to MEDIUM integrity — the level a standard user's token carries.
    /// `CreateRestrictedToken` strips groups and privileges but leaves integrity UNTOUCHED, so
    /// without this the de-elevated arm would still run at High and would be WEAKER than the
    /// principal it stands in for, which is the one direction the substitution must never go.
    /// Lowering an integrity level needs no privilege; only raising one does.
    fn set_medium_integrity(token: windows_sys::Win32::Foundation::HANDLE) -> std::io::Result<()> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
        use windows_sys::Win32::Security::{
            GetLengthSid, PSID, SID_AND_ATTRIBUTES, SetTokenInformation, TOKEN_MANDATORY_LABEL,
            TokenIntegrityLevel,
        };
        // windows-sys files this under System_SystemServices; spelled out rather than pulling a
        // whole feature in for one integer.
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
        // SAFETY: `label` and the SID it points at both outlive the call.
        let ok = unsafe {
            SetTokenInformation(
                token,
                TokenIntegrityLevel,
                std::ptr::from_mut(&mut label).cast(),
                std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + GetLengthSid(sid),
            )
        };
        let err = std::io::Error::last_os_error();
        // SAFETY: the SID was allocated by ConvertStringSidToSidW and is no longer referenced.
        unsafe { LocalFree(sid.cast()) };
        if ok == 0 { Err(err) } else { Ok(()) }
    }

    /// A PRIMARY token for this session with administrative authority removed. Prefers the
    /// linked (genuine standard-user) token where the logon is UAC-split, and logs why it was
    /// unavailable when it falls through, so the report never has to guess which principal it
    /// measured.
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
        // SAFETY: opens our own token with exactly the rights the routes below need.
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

        // Route 1 — the linked standard-user token, if this logon is UAC-split.
        let mut linked = TOKEN_LINKED_TOKEN {
            LinkedToken: std::ptr::null_mut(),
        };
        let mut ret = 0u32;
        // SAFETY: out-buffer is exactly a TOKEN_LINKED_TOKEN; an unsplit logon fails the call.
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
                "    [deelev] no linked token: {}",
                std::io::Error::last_os_error()
            );
        } else if !linked.LinkedToken.is_null() {
            let mut primary: HANDLE = std::ptr::null_mut();
            // SAFETY: duplicating a token handle we own into a primary token.
            let dup = unsafe {
                DuplicateTokenEx(
                    linked.LinkedToken,
                    TOKEN_ALL_ACCESS,
                    std::ptr::null(),
                    SecurityImpersonation,
                    TokenPrimary,
                    &mut primary,
                )
            };
            // SAFETY: the impersonation handle is no longer needed either way.
            unsafe { CloseHandle(linked.LinkedToken) };
            if dup != 0 {
                // SAFETY: our own process-token handle, no longer needed.
                unsafe { CloseHandle(me) };
                return Ok((primary, "linked-token"));
            }
            println!(
                "    [deelev] DuplicateTokenEx on the linked token failed: {}",
                std::io::Error::last_os_error()
            );
        }

        // Route 2 — a restricted token: Administrators deny-only, every privilege dropped.
        let admins_text: Vec<u16> = "S-1-5-32-544".encode_utf16().chain([0]).collect();
        let mut admins: PSID = std::ptr::null_mut();
        // SAFETY: converts a well-formed SDDL SID string; freed below.
        if unsafe { ConvertStringSidToSidW(admins_text.as_ptr(), &mut admins) } == 0 {
            let e = std::io::Error::last_os_error();
            // SAFETY: our own token handle.
            unsafe { CloseHandle(me) };
            return Err(e);
        }
        let disable = [SID_AND_ATTRIBUTES {
            Sid: admins,
            Attributes: 0,
        }];
        let mut restricted: HANDLE = std::ptr::null_mut();
        // SAFETY: `disable` outlives the call; `me` is primary, so the derived token is too.
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
        // SAFETY: the SID and our token handle are both done with.
        unsafe {
            LocalFree(admins.cast());
            CloseHandle(me);
        }
        if ok == 0 {
            return Err(err);
        }
        // `CreateRestrictedToken` hands back a handle carrying only the SOURCE handle's rights,
        // and `SetTokenInformation(TokenIntegrityLevel)` needs `TOKEN_ADJUST_DEFAULT` —
        // ACCESS_DENIED without it (run 30424678530). Re-open the token we already own instead
        // of widening the process-token handle above.
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
        // SAFETY: superseded by `full`.
        unsafe { CloseHandle(restricted) };
        if dup == 0 {
            return Err(dup_err);
        }
        set_medium_integrity(full).map_err(|e| {
            // SAFETY: abandoning the token we just built.
            unsafe { CloseHandle(full) };
            std::io::Error::other(format!("could not drop to medium integrity: {e}"))
        })?;
        Ok((full, "restricted-token+medium-il"))
    }

    /// Spawn `args` under `token` in THIS session and window station, with stdio going to
    /// `log`, bounded by `secs`.
    ///
    /// The transcript goes to a FILE rather than being inherited for two reasons: the child's
    /// property names collide with the parent's until the parent re-emits them under a
    /// `deelev-` prefix, and a child that has to be KILLED still leaves its partial transcript
    /// on disk, where an inherited-and-then-terminated stream would have left nothing.
    fn spawn_as_token_captured(
        token: windows_sys::Win32::Foundation::HANDLE,
        program: &Path,
        args: &[&str],
        log: &Path,
        secs: u32,
    ) -> std::io::Result<(i32, bool)> {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{
            CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_TIMEOUT,
        };
        use windows_sys::Win32::System::Threading::{
            CreateProcessAsUserW, GetExitCodeProcess, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
            STARTUPINFOW, TerminateProcess, WaitForSingleObject,
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

        let sink = std::fs::File::create(log)?;
        let sink_handle: HANDLE = sink.as_raw_handle().cast();
        let stdin: HANDLE = std::io::stdin().as_raw_handle().cast();
        for h in [stdin, sink_handle] {
            // SAFETY: marking handles we own inheritable, as `std`'s inherited-stdio spawn does.
            unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
        }
        // SAFETY: zeroed POD structs, fields set below.
        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = STARTF_USESTDHANDLES;
        si.hStdInput = stdin;
        si.hStdOutput = sink_handle;
        si.hStdError = sink_handle;
        // SAFETY: zeroed POD out-param.
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        // NULL lpEnvironment ⇒ the child inherits OURS. Deliberate: the arms must differ only in
        // the token, and route 2 is the same user, so TEMP/PATH/LOCALAPPDATA are already right.
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
        // SAFETY: bounded wait, then read the code and close both handles exactly once.
        let out = unsafe {
            let waited = WaitForSingleObject(pi.hProcess, secs * 1000);
            let timed_out = waited == WAIT_TIMEOUT;
            if timed_out {
                TerminateProcess(pi.hProcess, 0xDEAD);
                WaitForSingleObject(pi.hProcess, 30_000);
            }
            let mut code = 0u32;
            GetExitCodeProcess(pi.hProcess, &mut code);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            (code as i32, timed_out)
        };
        Ok(out)
    }

    /// The de-elevated child's entry point: report the principal it is, then run the shell
    /// battery. Its property names are the ordinary ones — the PARENT re-emits them under a
    /// `deelev-` prefix, so the two arms cannot be confused in the log while the names stay
    /// mechanically derived from one source.
    pub fn deelev_shell_main() -> i32 {
        arm_watchdog();
        let (il, privs) = token_shape();
        println!(
            "  fact:token-user={}",
            std::env::var("USERNAME").unwrap_or_default()
        );
        println!("  fact:token-admin-authority={}", admin_authority());
        println!("  fact:token-is-elevated-flag={}", probe_elevated());
        println!("  fact:token-integrity={il}");
        println!("  fact:token-privileges={privs}");
        let Some(node) = path_node() else {
            eprintln!("no node.exe on PATH");
            return 1;
        };
        let node = std::fs::canonicalize(&node)
            .map(|p| unverbatim(&p))
            .unwrap_or(node);
        let mut fails = 0u32;
        shell_tools(&mut fails, node.as_path());
        println!("  fact:arm-completed=true");
        // The count is reported, not returned as a failure: every shell cell is a MEASUREMENT
        // whose FAIL is the finding. The parent gates on the arm having RUN and on the
        // principal, which are the only two things that could make the cells meaningless.
        println!("  fact:arm-report-fails={fails}");
        0
    }

    /// The parent half. Builds the de-elevated principal, runs the whole shell group under it,
    /// and re-emits the child's transcript with `deelev-` inserted into every property name.
    fn deelev_shell_arm(fails: &mut u32) {
        println!("  fact:parent-admin-authority={}", admin_authority());
        let Ok(exe) = std::env::current_exe() else {
            report(fails, "deelev-arm-ran", false, "current_exe failed");
            return;
        };
        let log = std::env::temp_dir().join(format!("nub-jr-deelev-{}.log", std::process::id()));
        let (token, route) = match deelevated_primary_token() {
            Ok(pair) => pair,
            Err(e) => {
                report(
                    fails,
                    "deelev-arm-ran",
                    false,
                    &format!("could not build a de-elevated primary token: {e}"),
                );
                return;
            }
        };
        println!("  fact:deelev-route={route}");
        flush();
        let spawned = spawn_as_token_captured(token, &exe, &["__deelevshell__"], &log, 15 * 60);
        // SAFETY: the token is ours and the child has already been waited on.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(token) };
        let transcript = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(&log);
        match &spawned {
            Ok((code, timed_out)) => println!(
                "  fact:deelev-arm-exit={code} timed-out={timed_out} lines={}",
                transcript.lines().count()
            ),
            Err(e) => println!("  fact:deelev-arm-exit=SPAWN-FAILED {e}"),
        }
        for raw in transcript.lines() {
            let line = raw.trim_end_matches(['\r', '\n']);
            let prefixed = ["prop:", "fact:", "diag:"].iter().find_map(|kind| {
                line.strip_prefix(&format!("  {kind}"))
                    .map(|rest| format!("  {kind}deelev-{rest}"))
            });
            println!(
                "{}",
                prefixed.unwrap_or_else(|| format!("  [deelev] {line}"))
            );
        }
        // THE TWO GATES. Nothing else about the child is asserted, because every shell cell it
        // produced is a measurement — but a cell from an arm that never ran, or from one that
        // still held administrative authority, is not evidence of anything.
        report(
            fails,
            "deelev-arm-ran",
            transcript.contains("fact:arm-completed=true"),
            &format!(
                "{} lines, exit {:?} (a partial transcript means the arm was killed mid-battery)",
                transcript.lines().count(),
                spawned.as_ref().map(|(c, _)| *c)
            ),
        );
        report(
            fails,
            "deelev-arm-holds-no-admin-authority",
            transcript.contains("fact:token-admin-authority=false"),
            "the arm must FAIL an access check only an administrator passes, or it measured the \
             ceiling a second time",
        );
    }

    // ── the probe ────────────────────────────────────────────────────────────────────

    pub fn probe_main() -> i32 {
        let mut fails = 0u32;
        println!("PROBE windows jail repairs under AppContainer");
        arm_watchdog();

        let Some(node) = path_node() else {
            eprintln!("no node.exe on PATH — the probe cannot run");
            return 1;
        };
        let node = std::fs::canonicalize(&node)
            .map(|p| unverbatim(&p))
            .unwrap_or(node);
        println!("  fact:node-exe={}", node.display());

        // Each group announces itself, flushed, BEFORE it runs. CI logs are unavailable until a
        // job finishes, so a stalled arm is otherwise invisible — with these, the last line
        // printed names where it stopped.
        let step = |name: &str| {
            use std::io::Write;
            println!("STEP {name}");
            let _ = std::io::stdout().flush();
        };

        // The CHEAP, CANNOT-STALL groups come first, deliberately: ordering a group that cannot
        // stall behind one that can is how three runs lost the answer they were for.
        // `require_shapes` is the blast-radius measurement and writes no ancestor ace at all
        // (every cell is repair-OFF). `ace_cost` bounds itself — its propagating-writer control
        // runs on a synthetic 4000-entry tree, never on `%TEMP%` — and it is the group that says
        // whether the stall is gone, so it must not sit behind anything that could reproduce it.
        step("token_privileges");
        token_privileges(&mut fails);

        // THE SHIPPING ARM, FIRST. Everything below it is already measured on this branch
        // (runs 30532759630 / 30533110308) as the elevated CEILING; the de-elevated answer is
        // what this round exists for, so it must not sit behind a group that could stall and
        // take the log with it. Its own battery still runs the repair-OFF floor internally, so
        // the shipping/floor comparison is inside this one child.
        //
        // The all-users Node is planted HERE, by the still-elevated parent, and handed to both
        // arms through the environment — the de-elevated child could not create it, and that is
        // the whole point: a standard user meets that path as an installer left it.
        step("plant_allusers_node");
        let programfiles_nodejs_preexisted = PathBuf::from(
            std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into()),
        )
        .join("nodejs")
        .exists();
        let allusers = plant_allusers_node(node.as_path());
        let planted_allusers = allusers.is_some() && !programfiles_nodejs_preexisted;
        if let Some(exe) = &allusers {
            // SAFETY: the probe is single-threaded here; the child inherits this block.
            unsafe { std::env::set_var("PROBE_ALLUSERS_NODE", exe) };
        }

        step("deelev_shell_arm");
        deelev_shell_arm(&mut fails);

        step("ace_cost");
        ace_cost(&mut fails);

        // The shipping configuration. A second arm under `--preserve-symlinks` was measured here
        // and has been REMOVED: the flag is off the table, because a hoisted layout still symlinks
        // workspace members, so it is not the semantic no-op the layout argument claimed, and
        // because changing module resolution process-wide to route around one failing operation
        // was the wrong shape of fix regardless.
        step("require_shapes plain");
        require_shapes(&mut fails, node.as_path(), "plain", &[]);

        step("ancestor_repair");
        ancestor_repair(&mut fails, node.as_path());

        step("aap_skip_teardown");
        aap_skip_teardown(&mut fails, node.as_path());

        step("stdio_shim");
        stdio_shim(&mut fails, node.as_path());

        // THE SHELLS. Placed after everything that can be answered without them, because a
        // shell that hangs can only be caught by the watchdog, which ends the run — so an
        // earlier group must never sit behind this one.
        step("shell_tools");
        shell_tools(&mut fails, node.as_path());

        // LAST, deliberately: `cmd.exe` as the AppContainer child ITSELF, which is the shape
        // `aube-scripts` uses. `apply` owns spawn+wait, so a hang here is a watchdog exit — and
        // nothing after it would be measured.
        step("shell_direct_launch");
        shell_direct_launch(&mut fails);

        // Remove the planted all-users tree: it is probe scaffolding, and leaving a 90 MB
        // `node.exe` under `%ProgramFiles%` would make a later run on a reused image take the
        // PRE-EXISTING-LEFT-ALONE branch and silently skip the measurement.
        if let Some(exe) = &allusers
            && let Some(root) = exe.parent()
            && planted_allusers
        {
            println!(
                "  fact:allusers-node-cleanup={:?}",
                std::fs::remove_dir_all(root).map_err(|e| e.raw_os_error())
            );
        }
        step("done");

        if fails == 0 {
            println!("WINDOWS JAIL REPAIRS COMPLETE");
            0
        } else {
            eprintln!("{fails} propert(y/ies) failed");
            1
        }
    }
}

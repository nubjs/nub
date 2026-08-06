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
//! returns a `NODE_OPTIONS` carrying an `--import data:` preload that creates the pipe ITSELF in
//! the AppContainer-private namespace (`\\.\pipe\LOCAL\…`, which the same jail permits) and hands
//! the child an already-connected end as a raw fd. The five shapes a real lifecycle script uses
//! are measured through it, plus `fork()`, whose channel rides the same private namespace.
//!
//! `shim-spawn-stream-returns` is the one that can find a NEW bug rather than confirm an old fix:
//! the shim has to hold `close` back until the streams it published have drained
//! (`_closesNeeded`), and it drives that bookkeeping by hand because Node's own `maybeClose` does
//! not count a slot the caller supplied. If `close` fires first the marker comes back empty, and
//! that is a real defect, reported as measured.
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
        // `-e:<code>` runs Node with NO MAIN FILE, which is the only way to get past
        // `resolveMainPath` without a flag and therefore the only way to ask what a REQUIRE
        // costs separately from what the entry point costs.
        let mut command = std::process::Command::new(node);
        match script.strip_prefix("-e:") {
            Some(code) => command.arg("-e").arg(code),
            None => command.arg(script),
        };
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
        // The repair's reach is a property of THIS machine: it is the ACE half alone now, so
        // what bounds it is where this principal holds `WRITE_DAC`. The capability half that
        // used to be reported here is GONE — the kernel refuses the AppSilo RID class outright,
        // measured in both principals, so it never once widened a launch.

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
        // `None, None` — this arm isolates the stdio shim, so it wants the net gate's
        // no-package shape rather than a specific package's egress. The second argument is
        // the version-scoped-egress selector (added by `375fd1ee4c`); with no package there
        // is nothing for it to scope.
        let options = nub_sandbox::windows_build_jail_node_options(None, None);
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

        fork_round_trips(fails, &f, &shimmed, node, &dir);
        unshimmed_control(fails, &f, &unshimmed, node, &dir);
    }

    /// `fork()` USED to throw here, because a scratch file cannot emulate a duplex pipe. It now
    /// rides a channel over `\\.\pipe\LOCAL\…`, the AppContainer-private namespace, so the arm
    /// asserts a real round trip — and NESTED, because the repair has to survive its own recursion:
    /// the grandchild is forked BY a forked child, so a second private pipe must be creatable from
    /// inside an already-confined descendant and the preload must reach two levels down.
    ///
    /// A hang is still a FAIL. That was the original point of failing fast, and it does not stop
    /// being the bar just because the operation now succeeds.
    fn fork_round_trips(
        fails: &mut u32,
        f: &Fixture,
        policy: &SandboxPolicy,
        node: &Path,
        dir: &Path,
    ) {
        let marker = dir.join("fork.marker");
        let leaf = dir.join("fork-leaf.js");
        std::fs::write(
            &leaf,
            "process.on('message', (m) => process.send({ pong: m.ping, connected: process.connected }));\n",
        )
        .unwrap();
        let relay = dir.join("fork-relay.js");
        std::fs::write(
            &relay,
            format!(
                "const cp = require('child_process');\n\
                 const g = cp.fork({leaf});\n\
                 g.on('message', (m) => {{ process.send({{ relayed: m.pong }}); g.kill(); }});\n\
                 process.on('message', (m) => g.send({{ ping: m.ping }}));\n",
                leaf = js_literal(&leaf),
            ),
        )
        .unwrap();
        let _ = std::fs::remove_file(&marker);
        let body = format!(
            r#"const fs = require("fs"), cp = require("child_process");
const M = {marker};
fs.writeFileSync(M, "start");
const done = (s) => {{ fs.writeFileSync(M, s); process.exit(0); }};
const guard = setTimeout(() => done("HUNG no reply within 20s"), 20000);
try {{
  const direct = cp.fork({leaf});
  direct.on("message", (m) => {{
    direct.kill();
    const nested = cp.fork({relay});
    nested.on("message", (n) => {{
      clearTimeout(guard);
      nested.kill();
      done("OK direct=" + JSON.stringify(m) + " nested=" + JSON.stringify(n));
    }});
    nested.send({{ ping: "p2" }});
  }});
  direct.send({{ ping: "p1" }});
}} catch (e) {{
  clearTimeout(guard);
  done("THREW code=" + (e.code || "") + " msg=" + String(e.message));
}}
"#,
            marker = js_literal(&marker),
            leaf = js_literal(&leaf),
            relay = js_literal(&relay),
        );
        println!("  ---- shim shape fork ----");
        let run = node_arm(f, policy, "fork", node, dir, 40, &body);
        let inner = std::fs::read_to_string(&marker).unwrap_or_else(|_| "<no marker>".into());
        println!("  fact:fork-inner={inner}");
        report(
            fails,
            "shim-fork-roundtrip",
            inner.contains(r#""pong":"p1""#)
                && inner.contains(r#""connected":true"#)
                && run.outer.starts_with("EXITED"),
            &format!("inner={inner} outer={}", run.outer),
        );
        report(
            fails,
            "shim-fork-nested",
            inner.contains(r#""relayed":"p2""#),
            &format!("inner={inner}"),
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

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
            _ => 2,
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
        let spawned = std::process::Command::new(node)
            .arg(script)
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
        let script = dir.join(format!("{tag}.js"));
        let sink = dir.join(format!("{tag}.sink"));
        let outer_marker = dir.join(format!("{tag}.outer"));
        std::fs::write(&script, body).unwrap();
        let (code, outer) = run_jailed(
            f,
            policy,
            dir,
            "node",
            &outer_marker,
            &[
                secs.to_string(),
                node.to_string_lossy().into_owned(),
                script.to_string_lossy().into_owned(),
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
        let unshimmed = jail_shaped(&f, extra, &[]);

        let dir = f.work.join("shim");
        std::fs::create_dir_all(&dir).unwrap();
        seed_package(&dir);

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

    // ── the probe ────────────────────────────────────────────────────────────────────

    pub fn probe_main() -> i32 {
        let mut fails = 0u32;
        println!("PROBE windows jail repairs under AppContainer");

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

        step("ancestor_repair");
        ancestor_repair(&mut fails, node.as_path());
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

//! Windows: WHICH pipe shapes can an AppContainer child create, and why does Node hang?
//!
//! THE TENSION THIS RESOLVES. Two measurements from the same jail contradicted each other:
//! a direct `CreateNamedPipeW` under `\\.\pipe\` came back `ERROR_ACCESS_DENIED`, while a
//! Rust `Stdio::piped()` spawn — documented in Rust's own source as a NAMED pipe under the
//! same prefix — succeeded and returned the child's bytes. One of those says NPFS is closed
//! to a LowBox token and the other says it is open, so neither could be used until the
//! variable separating them was named. This probe varies ONE field at a time across eleven
//! `CreateNamedPipeW` cells that differ only in `dwOpenMode`, `dwPipeMode`, and the name.
//!
//! ROUND 1 (run 30467811349) answered the flag half: EVERY cell was refused under the jail,
//! `ERROR_ACCESS_DENIED`, in both the net-deny and net-unconfined arms — so no `dwOpenMode`
//! or `dwPipeMode` bit is the gate, and no capability nub can grant changes it either
//! (`internetClient` is the only one the policy surface reaches). The anonymous `CreatePipe`
//! was permitted in every arm. That leaves the NAMESPACE as the live hypothesis, which is
//! also where the independent evidence points: `CreateNamedPipeA`'s reference says an app
//! container must spell the name `\\.\pipe\LOCAL\…`, and Compiler Explorer hit this exact
//! wall under an AppContainer (ninja-build/ninja#2354 — "there seems to be no capability
//! that we can give the AppContainer environment to enable the creation of named pipes").
//! Round 2 adds the `LOCAL\` cells and asks the succeeding std handle for its own NPFS name,
//! so what std creates is read off the running binary rather than inferred from its source.
//!
//! WHY IT MATTERS. libuv creates a named pipe per piped stdio stream, and
//! `uv__pipe_server` (`deps/uv/src/win/pipe.c`) treats `ERROR_ACCESS_DENIED` as a NAME
//! COLLISION and retries with a new name — forever, with no bound:
//!
//! ```c
//! for (;;) {
//!   uv__unique_pipe_name(random, name, nameSize);
//!   pipeHandle = CreateNamedPipeA(name, access | FILE_FLAG_FIRST_PIPE_INSTANCE,
//!     PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT, 1, 65536, 65536, 0, NULL);
//!   if (pipeHandle != INVALID_HANDLE_VALUE) break;
//!   err = GetLastError();
//!   if (err != ERROR_PIPE_BUSY && err != ERROR_ACCESS_DENIED) goto error;
//!   random++;
//! }
//! ```
//!
//! That is why a confined `child_process` spawn with piped stdio does not FAIL, it HANGS,
//! and why Node's own `timeout` option cannot break it: the loop runs inside `uv_spawn`
//! before any timer arms. A hung postinstall is worse than a refused one, so the node arms
//! here bound every spawn EXTERNALLY and sample the child's CPU time before killing it — a
//! spin shows CPU ≈ wall, which distinguishes libuv's retry loop from a blocking wait.
//!
//! EVERY VERDICT IS A CHILD-WRITTEN MARKER, never a status the harness reports about
//! itself, and every path a child touches is baked in as an absolute literal (an earlier
//! lane's control went vacuous because its canary path arrived through an env var the jail
//! strips). The node scripts are written to FILES rather than passed with `-e`, so no
//! command-line quoting sits between the intent and the measurement.
//!
//! BOTH DIRECTIONS. Every cell runs in three arms: the real build-jail policy (net
//! deny-all), the same policy with net unconfined (which is the ONLY lever that changes the
//! token's capability set — nub grants `internetClient` iff `!policy.net.enforce`), and
//! UNCONFINED, where every cell must succeed. A cell that fails in all three arms is a
//! broken cell, not a finding.
//!
//! CI IS THE ONLY VENUE. AppContainer cannot be launched over SSH (session 0 has no window
//! station; every launch returns 0xC0000142). Runs branch-scoped via
//! `.github/workflows/win-pipe-shapes-probe.yml`, no pull request.

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

    // CreateNamedPipeW bit fields, spelled out because the whole probe is about which of
    // them is the gate.
    const ACCESS_INBOUND: u32 = 0x0000_0001;
    const ACCESS_OUTBOUND: u32 = 0x0000_0002;
    const FIRST_INSTANCE: u32 = 0x0008_0000;
    const OVERLAPPED: u32 = 0x4000_0000;
    // 0x8. The first round of this probe used 0x2, which is PIPE_READMODE_MESSAGE and is
    // invalid alongside PIPE_TYPE_BYTE — every cell carrying it came back ERROR_INVALID_
    // PARAMETER (87) UNCONFINED, which is exactly what the unconfined control exists to
    // catch. A cell nobody can create would otherwise have read as a jail refusal.
    const REJECT_REMOTE: u32 = 0x0000_0008;

    /// One `CreateNamedPipeW` cell. `name` is a template resolved in the CHILD (`{pid}` and
    /// `{n}`), so a pipe name never crosses a command line and cannot be mangled in transit.
    struct Cell {
        id: &'static str,
        name: &'static str,
        open_mode: u32,
        pipe_mode: u32,
        /// What this cell isolates against its neighbours.
        note: &'static str,
    }

    /// The matrix. Cells 1-4 are libuv's own shape and its single-field variants; 5-7 are
    /// Rust's; 8-9 strip the remaining flags. Read as a table, each row differs from a
    /// neighbour in exactly one field.
    const CELLS: &[Cell] = &[
        Cell {
            id: "libuv-exact-outbound",
            name: r"\\?\pipe\uv\{n}-{pid}",
            open_mode: ACCESS_OUTBOUND | OVERLAPPED | FIRST_INSTANCE,
            pipe_mode: 0,
            note: "libuv's literal call for a child's stdin",
        },
        Cell {
            id: "libuv-exact-inbound",
            name: r"\\?\pipe\uv\{n}-{pid}",
            open_mode: ACCESS_INBOUND | OVERLAPPED | FIRST_INSTANCE,
            pipe_mode: 0,
            note: "libuv's literal call for a child's stdout/stderr",
        },
        Cell {
            id: "libuv-plus-reject-remote",
            name: r"\\?\pipe\uv\{n}-{pid}",
            open_mode: ACCESS_INBOUND | OVERLAPPED | FIRST_INSTANCE,
            pipe_mode: REJECT_REMOTE,
            note: "libuv + PIPE_REJECT_REMOTE_CLIENTS — the one flag Rust sets and libuv does not",
        },
        Cell {
            id: "libuv-flags-flat-name",
            name: r"\\.\pipe\nubprobe-{pid}-{n}",
            open_mode: ACCESS_INBOUND | OVERLAPPED | FIRST_INSTANCE,
            pipe_mode: 0,
            note: "libuv's flags without the `uv\\` subdirectory or the `\\\\?\\` prefix",
        },
        Cell {
            id: "rust-exact",
            name: r"\\.\pipe\__rust_anonymous_pipe1__.{pid}.{n}",
            open_mode: ACCESS_INBOUND | OVERLAPPED | FIRST_INSTANCE,
            pipe_mode: REJECT_REMOTE,
            note: "std's literal call — the shape measured to SUCCEED under this jail",
        },
        Cell {
            id: "rust-minus-reject-remote",
            name: r"\\.\pipe\__rust_anonymous_pipe1__.{pid}.{n}",
            open_mode: ACCESS_INBOUND | OVERLAPPED | FIRST_INSTANCE,
            pipe_mode: 0,
            note: "std's shape with only PIPE_REJECT_REMOTE_CLIENTS removed",
        },
        Cell {
            id: "rust-minus-overlapped",
            name: r"\\.\pipe\__rust_anonymous_pipe1__.{pid}.{n}",
            open_mode: ACCESS_INBOUND | FIRST_INSTANCE,
            pipe_mode: REJECT_REMOTE,
            note: "std's shape with only FILE_FLAG_OVERLAPPED removed",
        },
        Cell {
            id: "reject-remote-only",
            name: r"\\.\pipe\nubprobe-{pid}-{n}",
            open_mode: ACCESS_INBOUND,
            pipe_mode: REJECT_REMOTE,
            note: "minimum: no FIRST_PIPE_INSTANCE, no OVERLAPPED",
        },
        Cell {
            id: "original-anchor",
            name: r"\\.\pipe\nub-interp-probe-{pid}-{n}",
            open_mode: ACCESS_INBOUND | FIRST_INSTANCE,
            pipe_mode: 0,
            note: "the shape the sibling probe measured REFUSED — the anchor to reproduce",
        },
        // The `LOCAL\` namespace is the OTHER live hypothesis, and it has independent
        // support: `CreateNamedPipeA`'s own reference says an app container must spell the
        // name `\\.\pipe\LOCAL\…`, and Compiler Explorer hit this exact wall under an
        // AppContainer (ninja-build/ninja#2354, "there seems to be no capability that we can
        // give the AppContainer environment to enable the creation of named pipes"). These
        // two cells are the anchor and libuv's flags with ONLY the namespace changed.
        Cell {
            id: "original-anchor-local-ns",
            name: r"\\.\pipe\LOCAL\nub-interp-probe-{pid}-{n}",
            open_mode: ACCESS_INBOUND | FIRST_INSTANCE,
            pipe_mode: 0,
            note: "the refused anchor with ONLY the LOCAL namespace added",
        },
        Cell {
            id: "libuv-flags-local-ns",
            name: r"\\.\pipe\LOCAL\nubprobe-{pid}-{n}",
            open_mode: ACCESS_INBOUND | OVERLAPPED | FIRST_INSTANCE,
            pipe_mode: 0,
            note: "libuv's flags in the LOCAL namespace — the shape a libuv fix could adopt",
        },
        Cell {
            id: "libuv-uvdir-local-ns",
            name: r"\\.\pipe\LOCAL\uv\{n}-{pid}",
            open_mode: ACCESS_INBOUND | OVERLAPPED | FIRST_INSTANCE,
            pipe_mode: 0,
            note: "LOCAL plus libuv's own `uv\\` subdirectory — the smallest patch to libuv",
        },
    ];

    fn resolve_name(t: &str, n: u32) -> Vec<u16> {
        t.replace("{pid}", &std::process::id().to_string())
            .replace("{n}", &n.to_string())
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }

    // ── child ────────────────────────────────────────────────────────────────────────

    pub fn child_main(a: &[String]) -> i32 {
        match a.first().map(String::as_str) {
            // np <marker> <cell-id> — create ONE pipe cell. The marker carries the raw
            // Win32 error, which is the whole point: 5 is a denial, 87 is a bad call.
            Some("np") => {
                use windows_sys::Win32::System::Pipes::CreateNamedPipeW;
                let marker = Path::new(&a[1]);
                let Some(cell) = CELLS.iter().find(|c| c.id == a[2]) else {
                    let _ = std::fs::write(marker, format!("unknown-cell {}", a[2]));
                    return 2;
                };
                let name = resolve_name(cell.name, std::process::id().wrapping_mul(7) | 1);
                let h = unsafe {
                    CreateNamedPipeW(
                        name.as_ptr(),
                        cell.open_mode,
                        cell.pipe_mode,
                        1,
                        65536,
                        65536,
                        0,
                        std::ptr::null(),
                    )
                };
                if h.is_null() || h == (-1isize as *mut std::ffi::c_void) {
                    let e = std::io::Error::last_os_error();
                    let _ = std::fs::write(marker, format!("REFUSED raw={:?}", e.raw_os_error()));
                    5
                } else {
                    unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
                    let _ = std::fs::write(marker, "CREATED");
                    0
                }
            }
            // anon <marker> — CreatePipe, the anonymous NPFS instance. The control that
            // must pass in every arm; if it fails the arm is broken, not restrictive.
            Some("anon") => {
                use windows_sys::Win32::System::Pipes::CreatePipe;
                let marker = Path::new(&a[1]);
                let mut r: windows_sys::Win32::Foundation::HANDLE = std::ptr::null_mut();
                let mut w: windows_sys::Win32::Foundation::HANDLE = std::ptr::null_mut();
                let ok = unsafe { CreatePipe(&mut r, &mut w, std::ptr::null(), 0) };
                if ok == 0 {
                    let e = std::io::Error::last_os_error();
                    let _ = std::fs::write(marker, format!("REFUSED raw={:?}", e.raw_os_error()));
                    5
                } else {
                    let name = object_name(r.cast());
                    unsafe {
                        windows_sys::Win32::Foundation::CloseHandle(r);
                        windows_sys::Win32::Foundation::CloseHandle(w);
                    }
                    let _ = std::fs::write(marker, format!("CREATED npfs-name={name}"));
                    0
                }
            }
            // ruststdio <marker> <exe> [args…] — std's own piped spawn, reproduced in the
            // same run as the cells so the comparison is not across probes.
            Some("ruststdio") => {
                use std::os::windows::io::AsRawHandle;
                let marker = Path::new(&a[1]);
                match std::process::Command::new(&a[2])
                    .args(&a[3..])
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                {
                    Ok(child) => {
                        // THE DECISIVE FACT. Every `CreateNamedPipeW` cell is refused while
                        // this spawn succeeds, and the two can only be reconciled by what
                        // std ACTUALLY created. `CreatePipe` names its instances
                        // `\Win32Pipes.<pid>.<n>`; a `CreateNamedPipeW` call names them
                        // whatever the caller asked for. Asking the handle removes the last
                        // inference from the chain.
                        let name = child
                            .stdout
                            .as_ref()
                            .map(|s| object_name(s.as_raw_handle().cast()))
                            .unwrap_or_else(|| "<no stdout handle>".to_string());
                        match child.wait_with_output() {
                            Ok(o) => {
                                let _ = std::fs::write(
                                    marker,
                                    format!(
                                        "CREATED npfs-name={name} out={}",
                                        String::from_utf8_lossy(&o.stdout).trim()
                                    ),
                                );
                                0
                            }
                            Err(e) => {
                                let _ = std::fs::write(marker, format!("waiterr {e:?}"));
                                9
                            }
                        }
                    }
                    Err(e) => {
                        let _ =
                            std::fs::write(marker, format!("REFUSED raw={:?}", e.raw_os_error()));
                        5
                    }
                }
            }
            // node <marker> <node.exe> <script.js> <sink> — run a Node shape under an
            // EXTERNAL bound. Node's own `timeout` cannot break libuv's retry loop, so the
            // bound has to live out here, in the process that can still act.
            Some("node") => run_node_bounded(&a[1], &a[2], &a[3], &a[4]),
            _ => 2,
        }
    }

    /// Spawn Node on a script with FILE-backed stdio (the jail refuses `NUL`, and a pipe is
    /// the thing under test), wait with a deadline, and report CPU-vs-wall before killing.
    /// A spin reads as CPU ≈ wall; a blocking wait reads as CPU ≈ 0.
    fn run_node_bounded(marker: &str, node: &str, script: &str, sink: &str) -> i32 {
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
        let deadline = std::time::Duration::from_secs(15);
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

    /// The NPFS path behind a handle, relative to `\Device\NamedPipe`. This is what
    /// separates "std creates a named pipe" from "std creates an anonymous one" — a claim
    /// the sibling probe took from Rust's source rather than from the running binary.
    fn object_name(h: windows_sys::Win32::Foundation::HANDLE) -> String {
        use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;
        // FileNameInfo. Declared inline because the buffer must be 4-byte aligned and long
        // enough for the name, which the generated FILE_NAME_INFO (a 1-element array) is not.
        const FILE_NAME_INFO: i32 = 2;
        #[repr(C)]
        struct NameInfo {
            length: u32,
            name: [u16; 512],
        }
        let mut info = NameInfo {
            length: 0,
            name: [0; 512],
        };
        let ok = unsafe {
            GetFileInformationByHandleEx(
                h,
                FILE_NAME_INFO as _,
                (&raw mut info).cast(),
                std::mem::size_of::<NameInfo>() as u32,
            )
        };
        if ok == 0 {
            let e = std::io::Error::last_os_error();
            return format!("<query-failed raw={:?}>", e.raw_os_error());
        }
        let chars = ((info.length as usize) / 2).min(info.name.len());
        String::from_utf16_lossy(&info.name[..chars])
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

    // ── fixture + policy (mirrors `windows_interpreter_exec`) ────────────────────────

    struct Fixture {
        root: PathBuf,
        child: PathBuf,
        work: PathBuf,
    }

    /// PROTECTED DACL on the fixture root: inherited ACEs stripped, only the current user
    /// granted — otherwise an inherited `ALL APPLICATION PACKAGES` grant from `C:\Users`
    /// would satisfy the LowBox check before default-deny is reached, and every arm would
    /// be measuring `%TEMP%`'s ACL rather than the backend's.
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
            let root = std::env::temp_dir().join(format!("nub-pipe-{tag}-{nonce:x}"));
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

    /// A build-jail-SHAPED policy. `net_enforce` is the ONE capability lever the public API
    /// exposes: `windows.rs` grants `internetClient` iff `!policy.net.enforce`, so passing
    /// `false` is how this probe asks whether a capability changes the NPFS verdict.
    fn jail_shaped(
        f: &Fixture,
        extra: Vec<FsRule>,
        net_enforce: bool,
        extra_env: &[(&str, String)],
    ) -> SandboxPolicy {
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
                enforce: net_enforce,
                rules: Vec::new(),
                default_effect: Effect::Deny,
                ..Default::default()
            },
            // `enforce` MUST be set, not merely `resolved`: the Windows backend hands the
            // child the constructed map only when the env axis enforces, and otherwise lets
            // it inherit the parent's. The first round used `resolved` alone, so the node
            // arms silently ran on the PARENT's environment without NODE_OPTIONS and died
            // in `resolveMainPath` before reaching the measurement.
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
        mode: &str,
        marker: &Path,
        argv: &[String],
    ) -> (i32, String) {
        let mut args = vec![mode.to_string(), marker.to_string_lossy().into_owned()];
        args.extend(argv.iter().cloned());
        let _ = std::fs::remove_file(marker);
        let outcome = apply(policy, spec(f, &f.work, &args)).map(|p| p.status());
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

    /// The same child arm with NO jail — the control half of every cell. Stdio is left
    /// inherited so the harness never reads a pipe it also has to wait on.
    fn run_unconfined(f: &Fixture, mode: &str, marker: &Path, argv: &[String]) -> (i32, String) {
        let mut args = vec!["__sbxchild__".to_string(), mode.to_string()];
        args.push(marker.to_string_lossy().into_owned());
        args.extend(argv.iter().cloned());
        let _ = std::fs::remove_file(marker);
        let code = std::process::Command::new(&f.child)
            .args(&args)
            .current_dir(&f.work)
            .status()
            .map(|s| s.code().unwrap_or(-1))
            .unwrap_or(-2);
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

    // ── the probe ────────────────────────────────────────────────────────────────────

    pub fn probe_main() -> i32 {
        let mut fails = 0u32;
        println!("PROBE windows pipe shapes under AppContainer");

        pipe_shape_matrix(&mut fails);
        node_stdio_shapes(&mut fails);

        if fails == 0 {
            println!("WINDOWS PIPE SHAPE MATRIX COMPLETE");
            0
        } else {
            eprintln!("{fails} propert(y/ies) failed");
            1
        }
    }

    /// THE MATRIX. Every cell in three arms. The two things that must hold for the run to
    /// mean anything are asserted as properties: the anonymous pipe must be creatable in
    /// EVERY arm (else the arm is broken), and every cell must be creatable UNCONFINED
    /// (else the cell's arguments are wrong and its jailed refusal says nothing).
    fn pipe_shape_matrix(fails: &mut u32) {
        let f = Fixture::new("matrix");
        let jail = jail_shaped(&f, Vec::new(), true, &[]);
        let jail_net = jail_shaped(&f, Vec::new(), false, &[]);
        let jail_loose = jail_shaped(&f, loosest_reachable_grants(), false, &[]);

        // `jail-loose` carries only the two cells that decide the question. Its grants are
        // applied as INHERITABLE ACEs on each granted tree and revoked afterwards, so a broad
        // grant costs a full DACL propagation across the user profile PER LAUNCH — the first
        // attempt ran all twelve cells there and blew a 25-minute step budget partway through
        // the arm. That cost is itself a finding about how coarse a Windows grant can be.
        let loose_cells = ["libuv-exact-inbound", "original-anchor-local-ns"];

        for (label, policy) in [
            ("jail-net-deny", Some(&jail)),
            ("jail-net-open", Some(&jail_net)),
            ("jail-loose", Some(&jail_loose)),
            ("unconfined", None),
        ] {
            println!("  ---- arm {label} ----");
            let cells: Vec<&Cell> = if label == "jail-loose" {
                CELLS
                    .iter()
                    .filter(|c| loose_cells.contains(&c.id))
                    .collect()
            } else {
                CELLS.iter().collect()
            };

            let m = f.work.join(format!("anon-{label}.txt"));
            let (code, text) = match policy {
                Some(p) => run_jailed(&f, p, "anon", &m, &[]),
                None => run_unconfined(&f, "anon", &m, &[]),
            };
            report(
                fails,
                &format!("anon-pipe-{label}"),
                code == 0 && text.starts_with("CREATED"),
                &format!("exit={code} marker={text} (the arm's liveness control)"),
            );

            for cell in &cells {
                let m = f.work.join(format!("{}-{label}.txt", cell.id));
                let (code, text) = match policy {
                    Some(p) => run_jailed(&f, p, "np", &m, &[cell.id.to_string()]),
                    None => run_unconfined(&f, "np", &m, &[cell.id.to_string()]),
                };
                // Measured, not judged: a refusal is an OS verdict, not nub's pass/fail.
                // What IS judged is that the measurement happened, and that the cell works
                // unconfined — a cell nobody can create proves nothing about the jail.
                println!(
                    "  fact:{cell}-{label}={text} (open=0x{om:08x} mode=0x{pm:x} — {note})",
                    cell = cell.id,
                    om = cell.open_mode,
                    pm = cell.pipe_mode,
                    note = cell.note,
                );
                report(
                    fails,
                    &format!("{}-{label}-measured", cell.id),
                    text != "<no marker>",
                    &format!("exit={code}"),
                );
                if policy.is_none() {
                    report(
                        fails,
                        &format!("{}-creatable-unconfined", cell.id),
                        text.starts_with("CREATED"),
                        &format!(
                            "marker={text} (a cell that cannot be created at all is a broken cell)"
                        ),
                    );
                }
            }

            // std's piped spawn, in the same run as the cells rather than across probes.
            let cmd = PathBuf::from(std::env::var("SystemRoot").unwrap_or_default())
                .join("System32")
                .join("cmd.exe");
            let m = f.work.join(format!("ruststdio-{label}.txt"));
            let argv = [
                cmd.to_string_lossy().into_owned(),
                "/c".to_string(),
                "ver".to_string(),
            ];
            let (code, text) = match policy {
                Some(p) => run_jailed(&f, p, "ruststdio", &m, &argv),
                None => run_unconfined(&f, "ruststdio", &m, &argv),
            };
            println!("  fact:rust-stdio-piped-{label}={text}");
            report(
                fails,
                &format!("rust-stdio-piped-{label}-measured"),
                text != "<no marker>",
                &format!("exit={code}"),
            );
        }
    }

    /// THE LOOSE END OF THE BISECTION. Granting MORE never needs elevation — a lifecycle
    /// script runs with the user's complete access by default, so every grant is a reduction
    /// from the status quo. What is genuinely unreachable unprivileged is authoring an ACE
    /// on a path the user does not own (`C:\` was measured refused de-elevated), so this is
    /// the loosest configuration nub can actually produce: read+write on the user's whole
    /// profile, the temp tree, the tool cache, and `ProgramData`, with net unconfined on top.
    /// If a shape is still refused here, no amount of loosening reaches it.
    fn loosest_reachable_grants() -> Vec<FsRule> {
        let mut rules = Vec::new();
        for key in ["USERPROFILE", "LOCALAPPDATA", "TEMP", "ProgramData"] {
            if let Ok(v) = std::env::var(key) {
                rules.push(FsRule {
                    matcher: CanonGlob(canon(Path::new(&v))),
                    effect: Effect::Allow,
                    access: FsAccess::ReadWrite,
                    origin: FsOrigin::Authored,
                });
            }
        }
        for dir in [r"C:\hostedtoolcache", r"C:\Program Files", r"C:\Windows"] {
            if Path::new(dir).is_dir() {
                rules.push(read_rule(Path::new(dir)));
            }
        }
        rules
    }

    /// The Node shapes a lifecycle script actually uses. Each script is a FILE, so no
    /// command-line quoting sits between the intent and the measurement, and each writes
    /// its OWN marker before and after the call — a marker still reading `start` after the
    /// external kill locates the block at the spawn itself.
    fn node_stdio_shapes(fails: &mut u32) {
        let Some(node) = path_node() else {
            eprintln!("no node.exe on PATH — the node arms cannot run");
            *fails += 1;
            return;
        };
        let node = std::fs::canonicalize(&node)
            .map(|p| unverbatim(&p))
            .unwrap_or(node);
        println!("  node: {}", node.display());

        let f = Fixture::new("node");
        let mut extra = vec![read_rule(&node)];
        if let Some(dir) = node.parent() {
            extra.push(read_rule(dir));
        }
        // `--preserve-symlinks-main` via NODE_OPTIONS is not optional here: Node realpaths
        // its main script before running a line of it, and BOTH realpath implementations are
        // refused under this jail (the sibling probe's finding, landed as
        // `windows_realpath_node_options`). Without it every node arm dies in
        // `resolveMainPath` — which is exactly what the first round measured.
        let jail = jail_shaped(
            &f,
            extra,
            true,
            &[("NODE_OPTIONS", nub_sandbox::windows_realpath_node_options())],
        );

        // The loose end of the bisection, applied to the shape that actually hangs. Same
        // grants as `jail-loose` above plus what Node itself needs.
        let mut loose_extra = loosest_reachable_grants();
        loose_extra.push(read_rule(&node));
        if let Some(dir) = node.parent() {
            loose_extra.push(read_rule(dir));
        }
        let jail_loose = jail_shaped(
            &f,
            loose_extra,
            false,
            &[("NODE_OPTIONS", nub_sandbox::windows_realpath_node_options())],
        );

        // `SHIM` is the candidate mitigation: `stdio: [0, fd, fd]` is measured to work, so a
        // preload that rewrites a buffered `pipe` into file-backed descriptors would recover
        // the callee's bytes without ever asking NPFS for a name. Measured here rather than
        // argued, because a mitigation nobody ran is a proposal, not a finding.
        const SHIM: &str = r#"
const _orig = cp.execFileSync;
cp.execFileSync = function (file, args, options) {
  const o = Object.assign({}, options);
  const fd = fs.openSync(CAPTURE, "w");
  o.stdio = [0, fd, fd];
  try { _orig(file, args, o); } finally { fs.closeSync(fd); }
  const buf = fs.readFileSync(CAPTURE);
  return o.encoding && o.encoding !== "buffer" ? buf.toString(o.encoding) : buf;
};
"#;

        // `piped` runs LAST in each policy deliberately: it is the shape that hangs, and an
        // arm that has to be killed must not discard the arms that answer everything else.
        let shapes: &[(&str, &str, &str)] = &[
            ("inherit", "", r#"{ stdio: "inherit" }"#),
            ("fdfile", "", r#"{ stdio: [0, FD, FD] }"#),
            ("ignore", "", r#"{ stdio: "ignore" }"#),
            ("shimmed", SHIM, r#"{ encoding: "utf8" }"#),
            ("piped", "", r#"{ encoding: "utf8" }"#),
        ];

        for (id, preamble, opts) in shapes {
            node_shape(fails, &f, &jail, "tight", &node, id, preamble, opts);
        }
        // Only the two that decide the question are repeated at the loose end; each `piped`
        // arm costs the full 25s bound.
        for (id, preamble, opts) in shapes.iter().filter(|s| s.0 == "piped") {
            node_shape(fails, &f, &jail_loose, "loose", &node, id, preamble, opts);
        }
    }

    /// Run ONE Node stdio shape under ONE policy, and report both markers.
    #[allow(clippy::too_many_arguments)]
    fn node_shape(
        fails: &mut u32,
        f: &Fixture,
        policy: &SandboxPolicy,
        arm: &str,
        node: &Path,
        id: &str,
        preamble: &str,
        opts: &str,
    ) {
        let nodemarker = f.work.join(format!("node-{arm}-{id}.marker"));
        let capture = f.work.join(format!("node-{arm}-{id}.capture"));
        let sink = f.work.join(format!("node-{arm}-{id}.sink"));
        let script = f.work.join(format!("node-{arm}-{id}.js"));
        let body = format!(
            r#"const fs = require("fs");
const cp = require("child_process");
const M = {marker};
const CAPTURE = {capture};
fs.writeFileSync(M, "start");
const FD = fs.openSync(CAPTURE, "w");
{preamble}
try {{
  const out = cp.execFileSync({node}, ["-e", "process.stdout.write('pong')"], {opts});
  fs.writeFileSync(M, "RETURNED " + (out === undefined ? "<no-capture>" : String(out).trim()));
}} catch (e) {{
  fs.writeFileSync(M, "THREW " + (e.code || "") + " | " + String(e.message).split("\n")[0]);
}}
"#,
            marker = js_literal(&nodemarker),
            capture = js_literal(&capture),
            node = js_literal(node),
        );
        std::fs::write(&script, body).unwrap();

        println!("  ---- node stdio shape {id} ({arm}) ----");
        let m = f.work.join(format!("node-{arm}-{id}.outer"));
        let (code, outer) = run_jailed(
            f,
            policy,
            "node",
            &m,
            &[
                node.to_string_lossy().into_owned(),
                script.to_string_lossy().into_owned(),
                sink.to_string_lossy().into_owned(),
            ],
        );
        let inner =
            std::fs::read_to_string(&nodemarker).unwrap_or_else(|_| "<no marker>".to_string());
        let stderr = std::fs::read_to_string(&sink).unwrap_or_default();
        println!("  fact:node-{arm}-{id}-outer={outer}");
        println!("  fact:node-{arm}-{id}-inner={inner}");
        if !stderr.trim().is_empty() {
            println!("  diag:node-{arm}-{id}-sink={}", stderr.trim());
        }
        report(
            fails,
            &format!("node-{arm}-{id}-measured"),
            outer != "<no marker>" && inner != "<no marker>",
            &format!("exit={code} outer={outer} inner={inner}"),
        );
    }

    /// A Windows path as a JS string literal. Backslashes are escaped rather than using a
    /// raw template, so the script text stays valid whatever the path contains.
    fn js_literal(p: &Path) -> String {
        format!("\"{}\"", p.to_string_lossy().replace('\\', "\\\\"))
    }
}

//! Windows dedicated-account backend — REAL enforcement probe (a provisioned Windows box only).
//!
//! Drives the REAL public seam (`apply` → `Prepared::status`): each case compiles a policy
//! that routes to the account backend, launches a child AS the `nub-sandbox` local account
//! through seclogon, and asserts the account's ACL reach / SID-keyed WFP fence allowed or
//! denied the action. Every confinement assertion is paired with a NEGATIVE CONTROL — almost
//! always an UNCONFINED run of the identical action — so a pass cannot be hollow: the control
//! proves the file or the TCP endpoint is genuinely reachable and the denial is confinement.
//!
//! THE MARQUEE CASE is the secret DENIED INSIDE a granted tree. That is precisely what the
//! AppContainer backend cannot express (an allowlist has no "deny inside allow", and an
//! inherited `ALL APPLICATION PACKAGES` grant satisfies the LowBox check before default-deny
//! is reached), and it is the reason this second backend exists at all.
//!
//! `harness = false`: this binary is BOTH the runner AND the probe child — an
//! `__acctchild__ <role>` invocation acts as the child (read/write/connect/spawnconnect/whoami
//! → a numeric exit-code contract), any other invocation runs the cases. The exit code is the
//! ONLY channel out of a seclogon-launched child, so the contract carries every verdict.
//!
//! PRECONDITION, NON-NEGOTIABLE: the account backend needs a one-time ELEVATED setup. This
//! probe never passes silently over an un-provisioned machine — it exits NON-ZERO naming the
//! command a human must run. When it IS elevated it may provision itself and tears that down
//! again at the end.

#[cfg(not(target_os = "windows"))]
fn main() {
    // Non-Windows host: nothing to enforce. (`harness = false` needs a `main`.)
}

#[cfg(target_os = "windows")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("__acctchild__") {
        std::process::exit(win::child_main(&args[2..]));
    }
    std::process::exit(win::run());
}

#[cfg(target_os = "windows")]
mod win {
    use nub_sandbox::policy::{
        CanonGlob, Effect, EnvPolicy, FsAccess, FsOrigin, FsPolicy, FsRule, FsRuleSet, NetPolicy,
        PidPolicy, SandboxPolicy, TmpMode,
    };
    use nub_sandbox::{CommandSpec, apply, windows_admin};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// The account nub provisions (`windows_account::SANDBOX_ACCOUNT`, not public).
    const ACCOUNT: &str = "nub-sandbox";

    // ── the probe child ─────────────────────────────────────────────────────────

    /// The child's exit-code contract. Distinct codes so a denial is never confused with a
    /// crash — and so the WFP fence is never confused with an ordinary access denial:
    /// 0 ok, 2 unknown-role, 3 grandchild-spawn-failed, 5 DENIED (fs `ERROR_ACCESS_DENIED`
    /// or net `WSAEACCES`), 6 timeout, 8 net refused with a plain `ERROR_ACCESS_DENIED`
    /// (NOT the fence — never a pass), 9 other error, 10 wrong identity.
    pub fn child_main(a: &[String]) -> i32 {
        match a.first().map(String::as_str) {
            Some("read") => classify_fs(std::fs::read(&a[1]).map(|_| ())),
            Some("write") => classify_fs(std::fs::write(&a[1], b"x")),
            Some("connect") => connect(&a[1], a[2].parse().unwrap_or(0)),
            Some("spawnconnect") => spawn_grandchild(&a[1], &a[2]),
            Some("whoami") => whoami(&a[1]),
            _ => 2,
        }
    }

    fn classify_fs(r: std::io::Result<()>) -> i32 {
        match r {
            Ok(()) => 0,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => 5,
            Err(e) => {
                eprintln!("  child fs: {e} os={:?}", e.raw_os_error());
                9
            }
        }
    }

    fn connect(host: &str, port: u16) -> i32 {
        let Ok(addr) = format!("{host}:{port}").parse::<SocketAddr>() else {
            return 9;
        };
        match TcpStream::connect_timeout(&addr, Duration::from_secs(6)) {
            Ok(_) => 0,
            // 10013 == WSAEACCES: the WFP `ALE_AUTH_CONNECT` block keyed on the account SID.
            // A plain ERROR_ACCESS_DENIED (5) comes from somewhere else entirely and must
            // never read as the fence, so it gets its own code and fails the assertion.
            Err(e) if e.raw_os_error() == Some(10013) => 5,
            Err(e) if e.raw_os_error() == Some(5) => 8,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => 6,
            Err(e) => {
                eprintln!("  child connect {addr}: {e} os={:?}", e.raw_os_error());
                9
            }
        }
    }

    /// Re-exec THIS binary as a grandchild attempting the same connect, propagating its code
    /// verbatim so the surrogate is judged by the identical contract.
    fn spawn_grandchild(host: &str, port: &str) -> i32 {
        let Ok(exe) = std::env::current_exe() else {
            return 3;
        };
        match std::process::Command::new(exe)
            .args(["__acctchild__", "connect", host, port])
            .status()
        {
            Ok(s) => s.code().unwrap_or(9),
            Err(e) => {
                eprintln!("  grandchild spawn: {e}");
                3
            }
        }
    }

    /// The anti-vacuity guard: prove the child really runs as the dedicated account. Without
    /// it every denial below could be an ordinary spawn failure rather than confinement.
    /// Identity comes from the profile environment seclogon builds for the account — the
    /// launch passes a NULL env block, so `USERNAME`/`USERPROFILE` are the ACCOUNT's.
    fn whoami(expected: &str) -> i32 {
        let user = std::env::var("USERNAME").unwrap_or_default();
        let profile = std::env::var("USERPROFILE").unwrap_or_default();
        println!("  CHILD USERNAME={user} USERPROFILE={profile}");
        let low = expected.to_ascii_lowercase();
        if user.eq_ignore_ascii_case(expected) || profile.to_ascii_lowercase().contains(&low) {
            0
        } else {
            10
        }
    }

    // ── the fixture ───────────────────────────────────────────────────────────────

    struct Fixture {
        _root: tempfile::TempDir,
        child: PathBuf,
        work: PathBuf,
        ok: PathBuf,
        secret: PathBuf,
        outside: PathBuf,
    }

    impl Fixture {
        fn new() -> std::io::Result<Self> {
            // An ORDINARY %TEMP% tree with NO DACL pre-hardening. The AppContainer probe has
            // to strip inherited ACEs because an inherited `ALL APPLICATION PACKAGES` grant
            // satisfies a LowBox check; no AAP grant ever covers a USER SID, so here the
            // account simply has no reach until this run's ACEs land. Traverse to the leaf
            // rides SeChangeNotifyPrivilege, which every local user holds.
            let root = tempfile::Builder::new().prefix("nub-acct-").tempdir()?;
            let bin = root.path().join("bin");
            let work = root.path().join("work");
            let outside = root.path().join("outside");
            for d in [&bin, &work, &outside] {
                std::fs::create_dir_all(d)?;
            }
            let child = bin.join("child.exe");
            std::fs::copy(std::env::current_exe()?, &child)?;
            let ok = work.join("ok.txt");
            std::fs::write(&ok, b"this-is-fine")?;
            let secret = work.join(".env");
            std::fs::write(&secret, b"TOPSECRET_TOKEN=do-not-leak")?;
            Ok(Fixture {
                _root: root,
                child,
                work,
                ok,
                secret,
                outside,
            })
        }
    }

    // ── policies (direct IR — full control over what routes where) ────────────────

    /// One fs rule over a real path, spelled the way the compiler would emit it — forward
    /// slashes, which the backend re-nativizes.
    fn rule(p: &Path, effect: Effect, access: FsAccess) -> FsRule {
        FsRule {
            matcher: CanonGlob(p.to_string_lossy().replace('\\', "/")),
            effect,
            access,
            origin: FsOrigin::Authored,
        }
    }

    /// The agent-sandbox fs shape this backend exists for: one granted tree with a secret
    /// DENIED inside it. The deny landing under the grant is exactly what `deny_shadows_grant`
    /// detects, and that is what routes the policy off the AppContainer allowlist.
    fn secret_in_grant(f: &Fixture) -> SandboxPolicy {
        SandboxPolicy {
            fs: FsPolicy {
                rules: FsRuleSet {
                    entries: vec![
                        rule(&f.work, Effect::Allow, FsAccess::ReadWrite),
                        rule(&f.secret, Effect::Deny, FsAccess::DENY),
                    ],
                    default_effect: Effect::Deny,
                },
                tmp: TmpMode::Shared,
            },
            net: NetPolicy::default(),
            // Direct-IR policies must carry an explicit target-environment snapshot: `apply`
            // refuses an unresolved env rather than guess between inheritance and an empty
            // environment, so a defaulted `EnvPolicy` fails every case before a child runs.
            // Inert for this backend's launch (`build_child_env` keys off `enforce`, which
            // stays false, so seclogon still builds the account's own profile environment).
            env: EnvPolicy::resolved(std::env::vars().collect()),
            pid: PidPolicy::default(),
        }
    }

    /// The same fs shape plus coarse egress-deny. The account is fenced by the persistent WFP
    /// filters either way; enforcing net states the intent the egress cases are testing.
    fn net_denied(f: &Fixture) -> SandboxPolicy {
        let mut p = secret_in_grant(f);
        p.net = NetPolicy {
            enforce: true,
            default_effect: Effect::Deny,
            ..Default::default()
        };
        p
    }

    /// NOT sandboxed at all: `fs_confines` is false and net is off, so `apply` spawns a plain
    /// child as the INVOKING user. Every negative control rides this — it is what proves the
    /// target is genuinely reachable, so the confined run's failure can only be confinement.
    fn unconfined() -> SandboxPolicy {
        SandboxPolicy {
            fs: FsPolicy {
                rules: FsRuleSet {
                    entries: Vec::new(),
                    default_effect: Effect::Allow,
                },
                tmp: TmpMode::Shared,
            },
            env: EnvPolicy::resolved(std::env::vars().collect()),
            ..Default::default()
        }
    }

    // ── run helpers ───────────────────────────────────────────────────────────────

    fn code(f: &Fixture, policy: &SandboxPolicy, args: &[&str]) -> i32 {
        // The cwd is PINNED to the granted dir: `CreateProcessWithLogonW` resolves the working
        // directory under the CHILD's token, and nub's own cwd is unreachable to the account,
        // so an unpinned cwd fails the spawn outright — a failure that is not the case under
        // test. `apply` folds the cwd into the read grants for us.
        let spec = CommandSpec::new(f.child.as_os_str())
            .args(args.iter().copied())
            .cwd(&f.work);
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

    /// A live loopback listener the RUNNER owns, so a connect failure can only be the fence
    /// and never a dead endpoint.
    fn listen(port: u16) -> Option<TcpListener> {
        let l = TcpListener::bind(("127.0.0.1", port)).ok()?;
        let acceptor = l.try_clone().ok()?;
        std::thread::spawn(move || {
            for c in acceptor.incoming() {
                drop(c);
            }
        });
        Some(l)
    }

    /// A listener on an ephemeral port OUTSIDE the WFP-permitted window. The ephemeral range
    /// overlaps the window, so a landing inside it is retried rather than silently testing the
    /// permitted case.
    fn listen_outside(window: (u16, u16)) -> Option<(TcpListener, u16)> {
        for _ in 0..8 {
            let l = TcpListener::bind(("127.0.0.1", 0)).ok()?;
            let port = l.local_addr().ok()?.port();
            if port < window.0 || port > window.1 {
                let acceptor = l.try_clone().ok()?;
                std::thread::spawn(move || {
                    for c in acceptor.incoming() {
                        drop(c);
                    }
                });
                return Some((l, port));
            }
        }
        None
    }

    // ── preconditions ─────────────────────────────────────────────────────────────

    /// Only the healthy arm of `windows_admin::status()` prints this line, so its presence IS
    /// the provisioning check and its value is the window the installed WFP permit covers.
    fn parse_window(report: &str) -> Option<(u16, u16)> {
        let line = report
            .lines()
            .find_map(|l| l.trim().strip_prefix("proxy port window: "))?;
        let (low, high) = line.trim().split_once('-')?;
        Some((low.trim().parse().ok()?, high.trim().parse().ok()?))
    }

    fn is_elevated() -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{
            GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        // SAFETY: standard token-query sequence; the buffer is exactly a TOKEN_ELEVATION.
        unsafe {
            let mut tok = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
                return false;
            }
            let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut ret = 0u32;
            let ok = GetTokenInformation(
                tok,
                TokenElevation,
                std::ptr::from_mut(&mut elev).cast(),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut ret,
            );
            CloseHandle(tok);
            ok != 0 && elev.TokenIsElevated != 0
        }
    }

    // ── the cases ─────────────────────────────────────────────────────────────────

    pub fn run() -> i32 {
        let report = windows_admin::status()
            .unwrap_or_else(|e| format!("sandbox account: status unavailable ({e})\n"));
        print!("{report}");

        let mut provisioned_here = false;
        let mut window = parse_window(&report);
        if window.is_none() {
            if !is_elevated() {
                eprintln!(
                    "\nABORT: this machine has no provisioned nub sandbox account, so there is \
                     nothing to probe.\n       Run this ONCE from an elevated (Run as \
                     administrator) prompt:\n\n           nub run --sandbox-admin setup\n\n       then \
                     re-run this probe. (Run the probe itself elevated and it will provision + \
                     tear down for you.)"
                );
                return 2;
            }
            println!(
                "ELEVATED and un-provisioned: running windows_admin::setup(None) for this probe; \
                 it is torn down again at the end."
            );
            match windows_admin::setup(None) {
                Ok(sid) => {
                    provisioned_here = true;
                    println!("provisioned the sandbox account ({sid})");
                }
                Err(e) => {
                    eprintln!("ABORT: sandbox setup failed: {e}");
                    return 2;
                }
            }
            let after = windows_admin::status().unwrap_or_default();
            print!("{after}");
            window = parse_window(&after);
        }

        let mut fails = match window {
            Some(w) => run_cases(w).unwrap_or_else(|e| {
                eprintln!("ABORT: fixture setup failed: {e}");
                u32::MAX
            }),
            None => {
                eprintln!("ABORT: could not read the WFP-permitted proxy port window from status");
                u32::MAX
            }
        };

        if provisioned_here {
            match windows_admin::teardown() {
                Ok(()) => println!("tore down the sandbox account this probe provisioned"),
                Err(e) => {
                    fails = fails.saturating_add(1);
                    eprintln!("FAIL teardown: the machine is LEFT PROVISIONED by this probe — {e}");
                }
            }
        }

        if fails == 0 {
            println!("ALL WINDOWS ACCOUNT ENFORCEMENT PROBES PASSED");
            0
        } else {
            eprintln!("{fails} WINDOWS ACCOUNT ENFORCEMENT PROBE(S) FAILED");
            1
        }
    }

    fn run_cases(window: (u16, u16)) -> std::io::Result<u32> {
        let f = Fixture::new()?;
        let mut fails = 0u32;
        let confine = secret_in_grant(&f);
        let open = unconfined();
        let deny_net = net_denied(&f);
        let secret = f.secret.to_string_lossy().into_owned();
        let ok = f.ok.to_string_lossy().into_owned();
        let inside = f.work.join("w.txt").to_string_lossy().into_owned();
        let outside = f.outside.join("w.txt").to_string_lossy().into_owned();
        let go = |p: &SandboxPolicy, args: &[&str]| code(&f, p, args);

        expect(
            &mut fails,
            "the child runs AS the dedicated sandbox account, not the invoking user",
            go(&confine, &["__acctchild__", "whoami", ACCOUNT]),
            0,
        );

        // ── 1. secret denied INSIDE a granted tree (the case AppContainer cannot express) ─
        expect(
            &mut fails,
            "KEY: the denied secret inside the granted tree is UNREADABLE",
            go(&confine, &["__acctchild__", "read", &secret]),
            5,
        );
        expect(
            &mut fails,
            "NC same run, same tree: the granted file still reads (the deny is surgical, not a dead grant)",
            go(&confine, &["__acctchild__", "read", &ok]),
            0,
        );
        expect(
            &mut fails,
            "NC unconfined: the secret is readable absent the sandbox (the file is not simply broken)",
            go(&open, &["__acctchild__", "read", &secret]),
            0,
        );

        // ── 2. writes jailed to the granted tree ─────────────────────────────────
        expect(
            &mut fails,
            "write inside the granted tree succeeds",
            go(&confine, &["__acctchild__", "write", &inside]),
            0,
        );
        expect(
            &mut fails,
            "write to the UNGRANTED sibling dir is denied",
            go(&confine, &["__acctchild__", "write", &outside]),
            5,
        );
        expect(
            &mut fails,
            "NC unconfined: the sibling dir is writable absent the sandbox",
            go(&open, &["__acctchild__", "write", &outside]),
            0,
        );

        // ── 3 + 4. egress fence, and that a surrogate spawn does not escape it ────
        match listen_outside(window) {
            None => {
                fails += 1;
                eprintln!(
                    "FAIL egress setup: no ephemeral loopback port outside the {}-{} window",
                    window.0, window.1
                );
            }
            Some((_listener, port)) => {
                let p = port.to_string();
                expect(
                    &mut fails,
                    "egress to a loopback endpoint OUTSIDE the permitted window is blocked (WSAEACCES)",
                    go(&deny_net, &["__acctchild__", "connect", "127.0.0.1", &p]),
                    5,
                );
                expect(
                    &mut fails,
                    "NC unconfined: the same endpoint is reachable (the target is live; the block is the fence)",
                    go(&open, &["__acctchild__", "connect", "127.0.0.1", &p]),
                    0,
                );
                expect(
                    &mut fails,
                    "a GRANDCHILD the sandboxed child spawns is fenced too (the filter keys on the SID, not the process)",
                    go(
                        &deny_net,
                        &["__acctchild__", "spawnconnect", "127.0.0.1", &p],
                    ),
                    5,
                );
                expect(
                    &mut fails,
                    "NC unconfined: the same grandchild reaches the endpoint",
                    go(&open, &["__acctchild__", "spawnconnect", "127.0.0.1", &p]),
                    0,
                );
            }
        }

        // ── 5. the proxy window is genuinely open (else per-host net is unreachable) ─
        match (window.0..=window.1).find_map(listen) {
            Some(listener) => {
                let p = listener.local_addr()?.port().to_string();
                expect(
                    &mut fails,
                    "a listener INSIDE the permitted window IS reachable from the sandboxed child",
                    go(&deny_net, &["__acctchild__", "connect", "127.0.0.1", &p]),
                    0,
                );
            }
            None => eprintln!(
                "SKIP proxy-window reachability — every port in the {}-{} window is already bound, \
                 so this property was NOT verified on this run",
                window.0, window.1
            ),
        }

        Ok(fails)
    }
}

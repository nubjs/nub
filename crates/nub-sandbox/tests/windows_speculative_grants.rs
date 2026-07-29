//! Windows: does an ABSENT speculative read grant still fail the launch?
//!
//! THE DIVERGENCE THIS PINS. The policy IR marks some fs grants `FsOrigin::Speculative` —
//! paths guessed at rather than named, which are absent on most machines by construction
//! (`policy::FsOrigin`). Linux honours that: `linux_grants::compile_mount_plan` skips a
//! missing speculative bind source and refuses only a missing AUTHORED one. Windows used to
//! drop `FsOrigin` on the floor in `derive_grants`, turn every read grant into an ACE, and
//! `set_ace` hard-fails on a nonexistent path — so the two OSes disagreed about whether an
//! absent guessed path is an error.
//!
//! That is not academic for the build jail, whose read set is mostly speculated: the project
//! manifest and `node_modules`, and nub's own PM store/tools caches. Any one of them not yet
//! created meant the jail could not launch AT ALL on Windows — a fresh clone before the first
//! install being the obvious case, and the FIRST native build on a machine (nothing has
//! created the node-gyp `tools` dir yet) being the routine one.
//!
//! THE ANTI-HOLLOW CONTRACT. "Skip a missing grant" is one edit away from "grant nothing and
//! call it a pass," so a green run here has to be impossible to reach that way. Three
//! properties, and each fails for a DIFFERENT reason if the fix is wrong:
//!
//!   - `speculative-absent-launches` (TREATMENT) — the defect. Fails if the skip is absent.
//!   - `authored-absent-refuses` (CONTROL) — the guarantee that must NOT be traded away. Same
//!     fixture, same absent path, `FsOrigin::Authored`: the launch must still fail with the
//!     ACE diagnostic, and the child must not run. A fix that skipped every missing grant
//!     regardless of origin passes the treatment and fails HERE.
//!   - `speculative-present-enforced` (CONTROL) — a speculative grant on a path that DOES
//!     exist is still installed, and the confinement around it still bites (a granted read
//!     succeeds, an ungranted one is denied). A fix that skipped speculative grants
//!     unconditionally, or that widened the policy, fails HERE.
//!
//! Plus `production-jail-absent-roots`: the real `compile_build_jail` policy against a
//! fixture whose project and cache roots do not exist — the shape the install flow actually
//! produces, rather than a hand-built approximation of it.
//!
//! Every property asserts an OBSERVABLE EFFECT of the child having run — a marker file it
//! wrote, or its exit code — never a status the harness reported about itself. A launch that
//! did not happen cannot leave a marker.
//!
//! CI IS THE ONLY VENUE. AppContainer cannot be launched over SSH: OpenSSH lands in session 0
//! with no window station, so every launch returns 0xC0000142 STATUS_DLL_INIT_FAILED. Runs
//! branch-scoped via `.github/workflows/win-speculative-grants-probe.yml`, no PR.

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

    /// Same exit-code contract as `windows_enforcement`: 0 ok, 5 access-denied, 9 other.
    pub fn child_main(a: &[String]) -> i32 {
        match a.first().map(String::as_str) {
            Some("read") => match std::fs::read(&a[1]) {
                Ok(_) => 0,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => 5,
                Err(_) => 9,
            },
            Some("write") => match std::fs::write(&a[1], b"x") {
                Ok(_) => 0,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => 5,
                Err(_) => 9,
            },
            _ => 2,
        }
    }

    struct Fixture {
        root: PathBuf,
        child: PathBuf,
        work: PathBuf,
        granted: PathBuf,
        ungranted: PathBuf,
        /// A path under the protected root that is deliberately NEVER created. The whole
        /// probe turns on what a grant naming it does.
        absent: PathBuf,
    }

    /// PROTECTED DACL on the fixture root: inherited ACEs stripped, only the current user
    /// granted. Without it an inherited `ALL APPLICATION PACKAGES` grant would satisfy the
    /// LowBox check before default-deny is reached, and `speculative-present-enforced`'s
    /// denied-read leg would be measuring `%TEMP%`'s ACL rather than the backend's.
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
            let root = std::env::temp_dir().join(format!("nub-spec-{tag}-{nonce:x}"));
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
            let granted = work.join("granted.txt");
            std::fs::write(&granted, b"this-is-fine").unwrap();
            let ungranted = vault.join("secret.env");
            std::fs::write(&ungranted, b"TOPSECRET_TOKEN=do-not-leak").unwrap();
            Fixture {
                absent: root.join("never-created"),
                root,
                child,
                work,
                granted,
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

    fn read_rule(p: &Path, origin: FsOrigin) -> FsRule {
        FsRule {
            matcher: CanonGlob(canon(p)),
            effect: Effect::Allow,
            access: FsAccess::Read,
            origin,
        }
    }

    /// The Windows-essential env a LowBox child needs to START (CreateProcessW resolves the
    /// per-container storage from the passed block, so a too-minimal env fails with
    /// ERROR_ENVVAR_NOT_FOUND). Mirrors `windows_enforcement::base_env`.
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
    /// egress deny — `build_jail: true` because that is the posture under test.
    fn jail_shaped(f: &Fixture, extra: Vec<FsRule>) -> SandboxPolicy {
        let mut entries = vec![FsRule {
            matcher: CanonGlob(canon(&f.work)),
            effect: Effect::Allow,
            access: FsAccess::ReadWrite,
            origin: FsOrigin::Authored,
        }];
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

    fn spec(f: &Fixture, args: &[&str]) -> CommandSpec {
        let mut a = vec!["__sbxchild__".to_string()];
        a.extend(args.iter().map(|s| s.to_string()));
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

    pub fn probe_main() -> i32 {
        let mut fails = 0u32;
        println!("PROBE windows speculative fs grants");

        speculative_absent_launches(&mut fails);
        authored_absent_refuses(&mut fails);
        speculative_present_enforced(&mut fails);
        production_jail_absent_roots(&mut fails);

        if fails == 0 {
            println!(
                "WINDOWS HONOURS FsOrigin::Speculative AND STILL REFUSES A MISSING AUTHORED GRANT"
            );
            0
        } else {
            eprintln!("{fails} propert(y/ies) failed");
            1
        }
    }

    /// TREATMENT. An absent SPECULATIVE grant must not stop the launch. The marker is the
    /// evidence the child actually ran — a launch that failed cannot have written it.
    fn speculative_absent_launches(fails: &mut u32) {
        let f = Fixture::new("specabs");
        let marker = f.work.join("speculative-ran.txt");
        let policy = jail_shaped(&f, vec![read_rule(&f.absent, FsOrigin::Speculative)]);
        let arg = marker.to_string_lossy().into_owned();
        let outcome = apply(&policy, spec(&f, &["write", &arg])).map(|p| p.status());
        match outcome {
            Ok(Ok(status)) if status.success() && marker.exists() => report(
                fails,
                "speculative-absent-launches",
                true,
                "child ran and wrote its marker with a missing speculative grant in the policy",
            ),
            Ok(Ok(status)) => report(
                fails,
                "speculative-absent-launches",
                false,
                &format!("status={status:?} marker={}", marker.exists()),
            ),
            Ok(Err(error)) => report(
                fails,
                "speculative-absent-launches",
                false,
                &format!("launch failed: {error}"),
            ),
            Err(degradation) => report(
                fails,
                "speculative-absent-launches",
                false,
                &format!("policy rejected: {degradation:?}"),
            ),
        }
    }

    /// CONTROL — the guarantee the treatment must not buy at any price. Byte-identical to
    /// the treatment except `FsOrigin`, so nothing but origin can explain the difference.
    fn authored_absent_refuses(fails: &mut u32) {
        let f = Fixture::new("authabs");
        let marker = f.work.join("authored-must-not-run.txt");
        let policy = jail_shaped(&f, vec![read_rule(&f.absent, FsOrigin::Authored)]);
        let arg = marker.to_string_lossy().into_owned();
        let outcome = apply(&policy, spec(&f, &["write", &arg])).map(|p| p.status());
        let refused = match &outcome {
            Ok(Err(error)) => {
                let text = error.to_string();
                let named = text.contains("read grant ACE") && text.contains("never-created");
                if !named {
                    eprintln!("  unexpected launch error text: {text}");
                }
                named
            }
            Ok(Ok(status)) => {
                eprintln!("  authored missing grant LAUNCHED: status={status:?}");
                false
            }
            Err(degradation) => {
                // A rejection before launch is also a refusal, but not the one this pins —
                // the promise is the ACE diagnostic naming the path.
                eprintln!("  policy rejected instead of failing at the ACE: {degradation:?}");
                false
            }
        };
        report(
            fails,
            "authored-absent-refuses",
            refused && !marker.exists(),
            &format!("marker_written={}", marker.exists()),
        );
    }

    /// CONTROL. Speculative is skipped for being ABSENT, never for being speculative — and
    /// the confinement around a kept grant still bites. Both legs, one policy.
    fn speculative_present_enforced(fails: &mut u32) {
        let f = Fixture::new("specpre");
        // `work` carries the speculative READ grant here (it exists), so the kept grant is
        // the one under test rather than the authored rw entry.
        let policy = jail_shaped(&f, vec![read_rule(&f.granted, FsOrigin::Speculative)]);

        let granted_arg = f.granted.to_string_lossy().into_owned();
        let allowed = apply(&policy, spec(&f, &["read", &granted_arg]))
            .map(|p| p.status().map(|s| s.code().unwrap_or(-1)));
        report(
            fails,
            "speculative-present-enforced",
            matches!(&allowed, Ok(Ok(0))),
            &format!("granted read exit={allowed:?} (want 0)"),
        );

        let ungranted_arg = f.ungranted.to_string_lossy().into_owned();
        let denied = apply(&policy, spec(&f, &["read", &ungranted_arg]))
            .map(|p| p.status().map(|s| s.code().unwrap_or(-1)));
        report(
            fails,
            "speculative-present-confines",
            matches!(&denied, Ok(Ok(5))),
            &format!("ungranted read exit={denied:?} (want 5 = access-denied)"),
        );
    }

    /// The REAL policy, not a hand-built approximation: `compile_build_jail` against a
    /// fixture whose project root and cache roots were never created — the fresh-clone /
    /// first-native-build shape. Every root it speculates is therefore absent at once.
    fn production_jail_absent_roots(fails: &mut u32) {
        let f = Fixture::new("prodjail");
        // The package dir is real (it is the write grant and the script's own cwd); the
        // project manifest, `node_modules`, and the whole cache tree are not.
        let package_dir = f.work.clone();
        let homes = nub_sandbox::Homes {
            home: f.root.join("no-home"),
            tmp: std::env::temp_dir(),
            cache: f.root.join("no-cache"),
            project: f.root.join("no-project"),
        };
        let interpreter = vec![f.child.clone()];
        // Shaped like the embedder's `node_toolchain_grant` output — POSIX-layout subtrees
        // that do not exist under a Windows Node install.
        let extra_reads = vec![
            f.root.join("no-node-root/include/node"),
            f.root.join("no-node-root/lib/node_modules"),
        ];
        let policy = match nub_sandbox::compile_build_jail(
            homes,
            &package_dir,
            interpreter,
            extra_reads,
            os_essential_env(),
        ) {
            Ok(p) => p,
            Err(error) => {
                report(
                    fails,
                    "production-jail-absent-roots",
                    false,
                    &format!("compile_build_jail failed: {error}"),
                );
                return;
            }
        };

        let marker = package_dir.join("production-jail-ran.txt");
        let arg = marker.to_string_lossy().into_owned();
        let outcome = apply(&policy, spec(&f, &["write", &arg])).map(|p| p.status());
        match outcome {
            Ok(Ok(status)) if status.success() && marker.exists() => report(
                fails,
                "production-jail-absent-roots",
                true,
                "the production build-jail policy launched with every speculated root absent",
            ),
            Ok(Ok(status)) => report(
                fails,
                "production-jail-absent-roots",
                false,
                &format!("status={status:?} marker={}", marker.exists()),
            ),
            Ok(Err(error)) => report(
                fails,
                "production-jail-absent-roots",
                false,
                &format!("launch failed: {error}"),
            ),
            Err(degradation) => report(
                fails,
                "production-jail-absent-roots",
                false,
                &format!("policy rejected: {degradation:?}"),
            ),
        }
    }
}

//! Purpose-built Linux direct-nesting COMPOSITION harness (epic D7/D8).
//!
//! Proves that a sandboxed Nub launches a nested sandbox by ORDINARY composition —
//! the same `earliest_bootstrap` -> compile-from-current-view -> `apply_with_runtime`
//! path a level-1 launch uses — with NO broker, listener, `SCM_RIGHTS`, request
//! router, or shared session registry. It drives THREE real nested levels, with two
//! concurrent level-2 siblings each creating a level-3 child, and asserts the
//! composition invariants directly inside each level:
//!
//!   - masks + write ceilings persist: the declared write root accepts every
//!     sibling/deep write; an adjacent path is read-only at every level;
//!   - the empty `.env` mask stays empty and the host secret is unchanged;
//!   - env never refills: a host-only value never crosses level 1, a scrubbed value
//!     propagates, and a value dropped at level 2 stays dropped (monotone tighten);
//!   - the inherited network + seccomp ceiling holds: an inner level that requests
//!     FULL networking still cannot reach the network (outer-deny wins);
//!   - PID namespaces are distinct at every level and the driver's own namespaces
//!     are never mutated;
//!   - the retained monitor preserves raw wait status ACROSS a nesting boundary,
//!     distinguishing a literal `exit(143)` from a real `SIGTERM`;
//!   - cancelling a deep nested chain reaps every descendant with zero survivors.
//!
//! The binary is ALSO the runtime image + PID-1 monitor for every level it launches:
//! `earliest_bootstrap` dispatches the monitor/describe/candidate-probe sentinels and
//! exits for those roles, so `main` reaches the harness verbs only for an ordinary
//! invocation. A gated cargo test (`tests/linux_nested_composition.rs`) locates and
//! runs this binary under an Ubuntu host set up for nesting; off a set-up host it
//! skips with a reason.

use std::process::ExitCode;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    imp::main()
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("the sandbox nesting-composition harness requires Linux");
    ExitCode::from(2)
}

#[cfg(target_os = "linux")]
mod imp {
    use nub_sandbox::{
        CommandSpec, CompileCtx, Homes, Prepared, PreparedChild, RuntimeCapability,
        ScopeCapabilities, apply_with_runtime, compile,
    };
    use serde_json::{Map, Value, json};
    use std::fs;
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::time::{Duration, Instant};

    // The scrubbed value the parent chose to pass down; it must reach every level.
    const INHERITED_VALUE: &str = "scrubbed-parent-value";
    // A host-only value; it must be withheld at level 1 and never cross.
    const HOST_SECRET_VALUE: &str = "must-not-cross";
    // Present at level 1, dropped at level 2 — proves a restriction TIGHTENS monotonically.
    const TIGHTEN_VALUE: &str = "present-at-level-1";
    // The on-disk secret the `.env` mask hides; the host copy must survive untouched.
    const ENV_SECRET_CONTENT: &str = "HOSTENVSECRET";
    // Bounded wait for the park chain to come fully up before cancellation.
    const READY_TIMEOUT: Duration = Duration::from_secs(60);
    // Bounded wait for a cancelled chain's host processes to disappear.
    const SURVIVOR_TIMEOUT: Duration = Duration::from_secs(15);

    pub fn main() -> ExitCode {
        // `earliest_bootstrap` handles the monitor/describe/candidate-probe sentinels
        // and `process::exit`s for those roles; a normal invocation gets the runtime.
        let runtime = match nub_sandbox::earliest_bootstrap() {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("nesting harness bootstrap failed: {error}");
                return ExitCode::from(125);
            }
        };
        let args = std::env::args().collect::<Vec<_>>();
        let result = match args.get(1).map(String::as_str) {
            Some("drive") => drive(&runtime),
            Some("nest") => nest(&runtime, &args),
            _ => {
                eprintln!(
                    "usage: nub-sandbox-nesting-harness <drive | nest <level> <label> <disp> <fixture-root>>"
                );
                return ExitCode::from(2);
            }
        };
        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::from(1)
            }
        }
    }

    // ── level 0: the driver ────────────────────────────────────────────────────

    fn drive(runtime: &RuntimeCapability) -> Result<(), String> {
        let base = make_fixture()?;
        let fixture = base.path();
        let proj = fixture.join("proj");
        let allowed = proj.join("allowed");
        let env_file = proj.join(".env");

        // The driver's namespaces must never be mutated by launching nested children.
        let driver_pidns = read_ns("pid")?;
        let driver_netns = read_ns("net")?;

        // Phase A — composition + lifecycle. The level-1 target fans out the two
        // concurrent siblings and their level-3 children and self-asserts every axis;
        // a non-zero exit means a deep assertion failed (its stderr is inherited up).
        let status = observe_level(
            runtime,
            &LaunchPlan {
                level: 1,
                label: "root",
                disp: Disp::Normal,
                fixture,
                ambient_env: driver_ambient_env(),
            },
        )?;
        if !status.success() {
            return Err(format!(
                "phase A: the level-1 nested composition did not succeed cleanly: {status:?}"
            ));
        }

        assert_host_side(&proj, &allowed, &env_file)?;

        // Phase B — cancel a deep parked chain; every descendant must be reaped.
        cancel_phase(runtime, fixture)?;

        // The driver's own namespaces are untouched by any of the above.
        if read_ns("pid")? != driver_pidns || read_ns("net")? != driver_netns {
            return Err(
                "the driver's own namespaces changed while launching nested sandboxes".into(),
            );
        }
        if let Some(survivor) = scan_harness_survivors()? {
            return Err(format!(
                "phase B: a harness process survived teardown: {survivor}"
            ));
        }

        println!("verified-nested-composition");
        Ok(())
    }

    /// Host-visible outcome of the whole Phase A tree: exactly the five declared writes
    /// landed, no adjacent write escaped, the masked secret is intact, and the two
    /// concurrently-live level-2 siblings ran in independent PID namespaces. Per-level
    /// parent/child PID-namespace distinctness is self-asserted inside each level (the
    /// parent stays alive there, so recycling can't spoof it); this host pass adds the
    /// sibling-independence check, which is sound because left and right overlap in time.
    fn assert_host_side(proj: &Path, allowed: &Path, env_file: &Path) -> Result<(), String> {
        let expected = [
            "root-level-1",
            "left-level-2",
            "right-level-2",
            "left-child-level-3",
            "right-child-level-3",
        ];
        let mut pidns = std::collections::BTreeMap::new();
        for name in expected {
            let path = allowed.join(name);
            let recorded = fs::read_to_string(&path)
                .map_err(|error| format!("declared write {name} is missing: {error}"))?;
            let value = recorded
                .lines()
                .find_map(|line| line.strip_prefix("pidns="))
                .ok_or_else(|| format!("write {name} did not record its PID namespace"))?
                .to_string();
            pidns.insert(name, value);
        }
        if pidns["left-level-2"] == pidns["right-level-2"] {
            return Err(format!(
                "the two level-2 siblings share PID namespace {} — they are not independent",
                pidns["left-level-2"]
            ));
        }
        // No adjacent write escaped the read-only parent at any level.
        for entry in
            fs::read_dir(proj).map_err(|error| format!("reading the project dir: {error}"))?
        {
            let entry = entry.map_err(|error| format!("reading a project entry: {error}"))?;
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("outside-") {
                return Err(format!(
                    "an adjacent write escaped the read-only parent: {}",
                    entry.path().display()
                ));
            }
        }
        // The masked secret's host copy is byte-for-byte unchanged.
        let host_secret = fs::read_to_string(env_file)
            .map_err(|error| format!("reading the host .env: {error}"))?;
        if host_secret.trim_end() != ENV_SECRET_CONTENT {
            return Err(format!(
                "the masked host secret was altered: {host_secret:?} != {ENV_SECRET_CONTENT:?}"
            ));
        }
        Ok(())
    }

    /// Launch a deep parked chain, wait for it to come fully up, then cancel it by
    /// dropping the outer handle (the retained monitor's fail-closed teardown). Prove
    /// every host process of the chain is gone — descendant kill/reap with zero
    /// survivors, exercised through three real nested levels.
    fn cancel_phase(runtime: &RuntimeCapability, fixture: &Path) -> Result<(), String> {
        let signals = fixture.join("proj").join("signals");
        // A stale ready marker from a prior run would let us cancel too early.
        clear_dir(&signals)?;

        let child = hold_level(
            runtime,
            &LaunchPlan {
                level: 1,
                label: "park-root",
                disp: Disp::Park,
                fixture,
                ambient_env: driver_ambient_env(),
            },
        )?;
        // Owning the handle here means the drop at the end of this scope triggers the
        // retained monitor's fail-closed teardown of the whole nested tree.
        let outer_pid = child.id();

        wait_for_ready(&signals, "park-leaf")
            .map_err(|error| format!("phase B: the parked chain never came up: {error}"))?;
        // Snapshot the chain's host PIDs BEFORE cancelling so we can prove they die. The
        // scan MUST see the retained MONITORS — PID 1 of each nested namespace, owner of
        // reap/teardown — else "zero survivors" is vacuous for the very processes that
        // matter, so require at least one monitor and a multi-level chain here.
        let before = scan_all_harness_pids()?;
        let monitors = before.iter().filter(|(_, role)| *role == "monitor").count();
        if monitors < 3 || before.len() < 6 {
            return Err(format!(
                "phase B: survivor scan is not seeing the full nested chain (saw {} monitors, {} processes total) — the zero-survivors proof would be vacuous",
                monitors,
                before.len()
            ));
        }

        // Cancel: dropping the outer handle runs the retained monitor's authenticated
        // fail-closed teardown (SIGKILL + reap of the whole PID-namespace tree).
        drop(child);

        let deadline = Instant::now() + SURVIVOR_TIMEOUT;
        loop {
            let survivors = scan_all_harness_pids()?;
            if survivors.iter().all(|(pid, _)| *pid == std::process::id()) {
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "phase B: the cancelled chain left survivors: {survivors:?} (outer pid {outer_pid})"
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    // ── levels 1..3: the nested target ──────────────────────────────────────────

    fn nest(runtime: &RuntimeCapability, args: &[String]) -> Result<(), String> {
        let level: u32 = args
            .get(2)
            .and_then(|value| value.parse().ok())
            .ok_or("nest: missing/invalid <level>")?;
        let label = args.get(3).ok_or("nest: missing <label>")?.clone();
        let disp = Disp::parse(args.get(4).map(String::as_str))?;
        let fixture = PathBuf::from(args.get(5).ok_or("nest: missing <fixture-root>")?);
        let parent_pidns = args.get(6).ok_or("nest: missing <parent-pidns>")?.clone();
        let proj = fixture.join("proj");
        let allowed = proj.join("allowed");
        let signals = proj.join("signals");

        set_process_name(&format!("nub-nest-{level}"));
        let pidns = read_ns("pid")?;
        eprintln!(
            "nest level={level} label={label} disp={disp:?} pidns={pidns} netns={}",
            read_ns("net").unwrap_or_default()
        );

        // Each level runs in a FRESH PID namespace distinct from its parent's. The
        // parent is still alive (it launched us and is waiting), so its inode is valid
        // and this comparison cannot be defeated by inode recycling.
        if pidns == parent_pidns {
            return Err(format!(
                "level {level} ({label}): PID namespace {pidns} is shared with the parent"
            ));
        }

        // The two level-2 siblings must be provably CONCURRENT for the sibling
        // PID-namespace-independence check to mean anything: rendezvous so both are alive
        // at the same instant. Their namespace inodes then demonstrably coexist and are
        // distinct — a sequential run could free one sibling and have the kernel recycle
        // its inode number for the other, spoofing a "shared" namespace.
        if level == 2 && (label == "left" || label == "right") {
            sibling_barrier(&signals, &label)?;
        }

        // Every level asserts its confinement is real; a parked node runs the cheap
        // adjacent-write check so a broken (unconfined) sandbox cannot masquerade as a
        // cancellation success.
        assert_env(level, &label)?;
        assert_write_ceiling(level, &label, &proj, &allowed, matches!(disp, Disp::Park))?;
        if !matches!(disp, Disp::Park) {
            assert_mask(&proj)?;
            assert_network_ceiling(level)?;
            record_write(&allowed, &label, level)?;
        }

        // Launch this level's children by ordinary composition (never a broker),
        // compiling the child policy from THIS process's current, already-narrowed view.
        // Spawn ALL children BEFORE waiting any, so a multi-child node (level 1's two
        // siblings) runs them CONCURRENTLY — a sequential launch would let one sibling's
        // whole subtree finish (and its PID namespace be recycled) before the next even
        // started, defeating both concurrency and any sibling comparison.
        let mut running = Vec::new();
        for (child_label, child_disp) in children_of(level, &label) {
            let plan = LaunchPlan {
                level: level + 1,
                label: child_label,
                disp: child_disp,
                fixture: fixture.as_path(),
                ambient_env: current_ambient_env(),
            };
            running.push((child_label, child_disp, hold_level(runtime, &plan)?));
        }
        for (child_label, child_disp, mut child) in running {
            let status = child.wait().map_err(|error| {
                format!(
                    "waiting on the level-{} {child_label} child: {error}",
                    level + 1
                )
            })?;
            // A parked child's wait returns only when the chain is torn down (nothing to
            // assert). Otherwise the retained monitor must report the child's RAW status
            // across the nesting boundary — a literal exit vs a real signal, not collapsed.
            if !matches!(child_disp, Disp::Park) {
                assert_disposition(child_disp, status, child_label, level + 1)?;
            }
        }

        if matches!(disp, Disp::Park) {
            signal_ready(&signals, &label)?;
        }
        apply_own_disposition(disp)
    }

    /// The fixed topology: level 1 fans out two concurrent siblings; each level-2
    /// sibling creates one level-3 child whose exit DISPOSITION differs so the parent
    /// proves the raw-status distinction survives nesting. The park chain descends
    /// single-file so a cancel has a real three-level tree to reap.
    fn children_of(level: u32, label: &str) -> Vec<(&'static str, Disp)> {
        match (level, label) {
            (1, "root") => vec![("left", Disp::Normal), ("right", Disp::Normal)],
            (2, "left") => vec![("left-child", Disp::Exit143)],
            (2, "right") => vec![("right-child", Disp::Sigterm)],
            (1, "park-root") => vec![("park-mid", Disp::Park)],
            (2, "park-mid") => vec![("park-leaf", Disp::Park)],
            _ => Vec::new(),
        }
    }

    // ── composition primitive: compile-from-current-view -> apply_with_runtime ──

    struct LaunchPlan<'a> {
        level: u32,
        label: &'static str,
        disp: Disp,
        fixture: &'a Path,
        ambient_env: std::collections::BTreeMap<String, String>,
    }

    /// Spawn the composed child, wait, and return its raw exit status — for a level
    /// whose disposition the caller asserts.
    fn observe_level(
        runtime: &RuntimeCapability,
        plan: &LaunchPlan<'_>,
    ) -> Result<std::process::ExitStatus, String> {
        prepare_level(runtime, plan)?
            .status()
            .map_err(|error| format!("running the level-{} child: {error}", plan.level))
    }

    /// Spawn the composed child and hand back the LIVE handle so the caller owns its
    /// lifetime — a parked node kept alive under its parent, or the driver's
    /// cancellation subject whose drop triggers fail-closed teardown.
    fn hold_level(
        runtime: &RuntimeCapability,
        plan: &LaunchPlan<'_>,
    ) -> Result<PreparedChild, String> {
        prepare_level(runtime, plan)?
            .spawn()
            .map_err(|error| format!("spawning the level-{} child: {error}", plan.level))
    }

    /// The production composition step: compile the child policy from the CALLER's
    /// current view, then `apply_with_runtime` a fresh helper Bubblewrap + fresh
    /// retained monitor + fresh PID namespace. `require_nesting` selects the dedicated
    /// helper so the child (and, transitively, its own children) can nest under an
    /// AppArmor-restricted host — the whole chain, including the outermost level,
    /// launches through the helper. No ancestor sandbox is entered, selected, or mutated.
    fn prepare_level(
        runtime: &RuntimeCapability,
        plan: &LaunchPlan<'_>,
    ) -> Result<Prepared, String> {
        let proj = plan.fixture.join("proj");
        let allowed = proj.join("allowed");
        let signals = proj.join("signals");
        let homes = Homes {
            home: plan.fixture.join("home"),
            tmp: std::env::temp_dir(),
            cache: plan.fixture.join("cache"),
            project: proj.clone(),
        };
        let ctx = CompileCtx::new(
            homes,
            proj.clone(),
            ScopeCapabilities::approved(),
            plan.ambient_env.clone(),
        );
        let policy = compile(&policy_surface(plan.level, &allowed, &signals), &ctx)
            .map_err(|error| format!("compiling the level-{} policy: {error}", plan.level))?;

        let program = std::env::current_exe()
            .map_err(|error| format!("resolving the harness executable: {error}"))?;
        // The LAUNCHER's own PID namespace, passed so the child can assert it received a
        // DISTINCT one. The launcher stays alive across the child's run, so its inode
        // cannot be recycled underneath the comparison (unlike comparing recorded inodes
        // of sequential short-lived processes, which the kernel is free to reuse).
        let spec = CommandSpec::new(program)
            .arg("nest")
            .arg(plan.level.to_string())
            .arg(plan.label)
            .arg(plan.disp.as_str())
            .arg(plan.fixture.as_os_str())
            .arg(read_ns("pid")?)
            .cwd(&proj)
            .deny_search_roots([proj])
            // Every level in a nesting chain is launched through the dedicated helper:
            // a stock outer cannot transition to the helper's AppArmor profile under
            // no_new_privs, so a descendant could not nest further.
            .require_nesting(true);

        apply_with_runtime(&policy, spec, runtime).map_err(|degradation| {
            format!(
                "applying the level-{} policy failed closed: {}",
                plan.level,
                degradation.reason.unwrap_or_else(|| "no reason".into())
            )
        })
    }

    /// The per-level surface: read all of `/` (so the harness binary + helper stay
    /// visible for the next composition), a writable declared root + signals dir,
    /// the built-in `.env` mask, a monotone env ceiling, and a network ceiling that
    /// denies at level 1 and asks FULL below it (the inner FULL request must still be
    /// denied by the inherited ceiling).
    fn policy_surface(level: u32, allowed: &Path, signals: &Path) -> Value {
        let mut fs_map = Map::new();
        fs_map.insert("/".into(), json!("r"));
        fs_map.insert(allowed.to_string_lossy().into_owned(), json!("rw"));
        fs_map.insert(signals.to_string_lossy().into_owned(), json!("rw"));
        // Level 1 keeps the scrubbed + tighten values (dropping the host-only secret);
        // level 2+ additionally drop the tighten value — proving a monotone tighten.
        let env = if level == 1 {
            json!(["INHERITED", "TIGHTEN_ME"])
        } else {
            json!(["INHERITED"])
        };
        let net = if level == 1 {
            json!(false)
        } else {
            json!(true)
        };
        json!({ "fs": Value::Object(fs_map), "vars": env, "net": net })
    }

    // ── per-level assertions ────────────────────────────────────────────────────

    fn assert_env(level: u32, label: &str) -> Result<(), String> {
        if std::env::var("HOST_SECRET").is_ok() {
            return Err(format!(
                "level {level} ({label}): the host-only secret crossed the boundary"
            ));
        }
        match std::env::var("INHERITED").as_deref() {
            Ok(INHERITED_VALUE) => {}
            other => {
                return Err(format!(
                    "level {level} ({label}): the scrubbed value did not propagate: {other:?}"
                ));
            }
        }
        let tighten = std::env::var("TIGHTEN_ME").is_ok();
        // Present only at level 1; dropped at level 2 and never refilled below.
        if level == 1 && !tighten {
            return Err(format!(
                "level {level} ({label}): TIGHTEN_ME missing at level 1"
            ));
        }
        if level > 1 && tighten {
            return Err(format!(
                "level {level} ({label}): TIGHTEN_ME refilled after being dropped (env widened)"
            ));
        }
        Ok(())
    }

    fn assert_mask(proj: &Path) -> Result<(), String> {
        let env_file = proj.join(".env");
        let bytes = fs::read(&env_file).map_err(|error| {
            format!("the .env mask was not readable inside the sandbox: {error}")
        })?;
        if !bytes.is_empty() {
            return Err(format!(
                "the .env mask exposed {} bytes; it must be an empty file",
                bytes.len()
            ));
        }
        Ok(())
    }

    fn assert_write_ceiling(
        level: u32,
        label: &str,
        proj: &Path,
        allowed: &Path,
        park: bool,
    ) -> Result<(), String> {
        // An adjacent write (sibling of the declared root, under the read-only parent)
        // must fail at every level — the write ceiling is never widened by nesting.
        let outside = proj.join(format!("outside-{label}-{level}"));
        if fs::write(&outside, b"forbidden").is_ok() {
            let _ = fs::remove_file(&outside);
            return Err(format!(
                "level {level} ({label}): a write outside the declared root unexpectedly succeeded"
            ));
        }
        // A parked node skips the positive write so the Phase-A declared-write count
        // stays exactly five; its adjacent-deny check above still proves confinement.
        if park {
            let _ = allowed;
        }
        Ok(())
    }

    fn assert_network_ceiling(level: u32) -> Result<(), String> {
        // Level 1 denies the network (isolated netns + a socket-family seccomp filter);
        // level 2+ ask for FULL networking but inherit that ceiling (seccomp stacks and
        // the empty netns persists), so socket creation must still be refused.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd >= 0 {
            // A socket slipped past the seccomp ceiling — a real connect must still be
            // impossible in the inherited empty netns, but creating it at all is a widen.
            unsafe { libc::close(fd) };
            return Err(format!(
                "level {level}: an AF_INET socket was created despite the inherited network ceiling"
            ));
        }
        Ok(())
    }

    fn record_write(allowed: &Path, label: &str, level: u32) -> Result<(), String> {
        let path = allowed.join(format!("{label}-level-{level}"));
        let body = format!(
            "level={level}\nlabel={label}\npidns={}\nnetns={}\n",
            read_ns("pid")?,
            read_ns("net")?
        );
        fs::write(&path, body)
            .map_err(|error| format!("the declared write root rejected a write: {error}"))
    }

    // ── dispositions ────────────────────────────────────────────────────────────

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Disp {
        Normal,
        Exit143,
        Sigterm,
        Park,
    }

    impl Disp {
        fn parse(value: Option<&str>) -> Result<Self, String> {
            match value {
                Some("normal") => Ok(Self::Normal),
                Some("exit143") => Ok(Self::Exit143),
                Some("sigterm") => Ok(Self::Sigterm),
                Some("park") => Ok(Self::Park),
                other => Err(format!("nest: unknown disposition {other:?}")),
            }
        }
        fn as_str(self) -> &'static str {
            match self {
                Self::Normal => "normal",
                Self::Exit143 => "exit143",
                Self::Sigterm => "sigterm",
                Self::Park => "park",
            }
        }
    }

    fn apply_own_disposition(disp: Disp) -> Result<(), String> {
        match disp {
            Disp::Normal => Ok(()),
            // A literal exit code the monitor must report as an EXIT, not a signal.
            Disp::Exit143 => unsafe { libc::_exit(143) },
            // A real self-signal the monitor must report as SIGNALED, not exit 143.
            Disp::Sigterm => unsafe {
                libc::raise(libc::SIGTERM);
                // SIGTERM's default action terminates; reaching here means it was
                // blocked/ignored, which is itself a failure to reproduce.
                libc::_exit(70)
            },
            Disp::Park => loop {
                unsafe { libc::pause() };
            },
        }
    }

    /// The parent's proof that the retained monitor preserved the child's RAW status
    /// across the nesting boundary — a literal `exit(143)` and a real `SIGTERM` are
    /// distinct dispositions, never collapsed to one number.
    fn assert_disposition(
        expected: Disp,
        status: std::process::ExitStatus,
        label: &str,
        level: u32,
    ) -> Result<(), String> {
        match expected {
            Disp::Normal => {
                if !status.success() {
                    return Err(format!(
                        "level {level} ({label}): expected a clean exit, got {status:?}"
                    ));
                }
            }
            Disp::Exit143 => {
                if status.code() != Some(143) || status.signal().is_some() {
                    return Err(format!(
                        "level {level} ({label}): expected a literal exit(143), got code={:?} signal={:?}",
                        status.code(),
                        status.signal()
                    ));
                }
            }
            Disp::Sigterm => {
                if status.signal() != Some(libc::SIGTERM) || status.code().is_some() {
                    return Err(format!(
                        "level {level} ({label}): expected a real SIGTERM, got code={:?} signal={:?}",
                        status.code(),
                        status.signal()
                    ));
                }
            }
            Disp::Park => {
                return Err("a parked disposition is never observed by status".into());
            }
        }
        Ok(())
    }

    // ── fixture + host helpers ──────────────────────────────────────────────────

    /// Build the fixture under `/var/tmp` (NOT `/tmp`, so a default Shared tmp mode
    /// never remaps it): a project dir read-only inside the sandbox, a writable
    /// `allowed`/`signals` pair, and a `.env` holding the secret the mask must hide.
    fn make_fixture() -> Result<tempfile::TempDir, String> {
        let base = tempfile::Builder::new()
            .prefix("nub-nest-compose-")
            .tempdir_in("/var/tmp")
            .map_err(|error| format!("creating the fixture: {error}"))?;
        let proj = base.path().join("proj");
        fs::create_dir_all(proj.join("allowed"))
            .and_then(|()| fs::create_dir_all(proj.join("signals")))
            .and_then(|()| fs::write(proj.join(".env"), format!("{ENV_SECRET_CONTENT}\n")))
            .map_err(|error| format!("populating the fixture: {error}"))?;
        Ok(base)
    }

    fn driver_ambient_env() -> std::collections::BTreeMap<String, String> {
        [
            ("INHERITED", INHERITED_VALUE),
            ("HOST_SECRET", HOST_SECRET_VALUE),
            ("TIGHTEN_ME", TIGHTEN_VALUE),
            ("PATH", "/usr/bin:/bin"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
    }

    fn current_ambient_env() -> std::collections::BTreeMap<String, String> {
        std::env::vars().collect()
    }

    fn read_ns(kind: &str) -> Result<String, String> {
        fs::read_link(format!("/proc/self/ns/{kind}"))
            .map(|target| target.to_string_lossy().into_owned())
            .map_err(|error| format!("reading /proc/self/ns/{kind}: {error}"))
    }

    fn set_process_name(name: &str) {
        let mut bytes = name.as_bytes().to_vec();
        bytes.truncate(15);
        bytes.push(0);
        unsafe { libc::prctl(libc::PR_SET_NAME, bytes.as_ptr().cast::<libc::c_char>()) };
    }

    fn signal_ready(signals: &Path, label: &str) -> Result<(), String> {
        fs::write(signals.join(format!("{label}-ready")), b"1")
            .map_err(|error| format!("writing the {label} readiness marker: {error}"))
    }

    /// Rendezvous the two level-2 siblings: publish this one's marker, then block until
    /// BOTH are present. Passing proves both were alive at the same instant.
    fn sibling_barrier(signals: &Path, label: &str) -> Result<(), String> {
        fs::write(signals.join(format!("sib-{label}-up")), b"1")
            .map_err(|error| format!("writing the {label} sibling barrier marker: {error}"))?;
        let deadline = Instant::now() + READY_TIMEOUT;
        let (left, right) = (signals.join("sib-left-up"), signals.join("sib-right-up"));
        while !(left.exists() && right.exists()) {
            if Instant::now() >= deadline {
                return Err(
                    "sibling barrier timed out; the two siblings were not concurrently alive"
                        .into(),
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    fn wait_for_ready(signals: &Path, label: &str) -> Result<(), String> {
        let marker = signals.join(format!("{label}-ready"));
        let deadline = Instant::now() + READY_TIMEOUT;
        while !marker.exists() {
            if Instant::now() >= deadline {
                return Err(format!("{label} readiness marker never appeared"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    fn clear_dir(dir: &Path) -> Result<(), String> {
        for entry in
            fs::read_dir(dir).map_err(|error| format!("reading {}: {error}", dir.display()))?
        {
            let entry = entry.map_err(|error| format!("reading an entry: {error}"))?;
            let _ = fs::remove_file(entry.path());
        }
        Ok(())
    }

    /// Any live host process of the chain other than the driver itself. Returns the
    /// first survivor's pid/role/cmdline for a diagnostic, or `None` if clean.
    fn scan_harness_survivors() -> Result<Option<String>, String> {
        for (pid, role) in scan_all_harness_pids()? {
            if pid != std::process::id() {
                let cmd = fs::read_to_string(format!("/proc/{pid}/cmdline")).unwrap_or_default();
                return Ok(Some(format!(
                    "pid {pid} ({role}): {}",
                    cmd.replace('\0', " ").trim()
                )));
            }
        }
        Ok(None)
    }

    /// The chain role of a process by its exe link, or `None` if not ours. Matched
    /// mount-namespace-independently: `readlink(/proc/<pid>/exe)` returns the string the
    /// process's OWN mount view recorded at exec, readable from any namespace. A nest
    /// TARGET runs this very binary; a retained MONITOR is exec'd from the private
    /// runtime image under the reserved runtime root — as `ld.so` on a DYNAMIC (glibc)
    /// runtime and `nub-monitor` on a STATIC (musl) one, so match the ROOT, not a
    /// basename (a basename match is blind to the glibc monitor, the process that owns
    /// reap/relay/teardown); the outer bwrap HELPER is the dedicated helper path.
    fn harness_role(exe: &Path, self_exe: &Path) -> Option<&'static str> {
        if exe == self_exe {
            Some("target")
        } else if exe.starts_with("/dev/.nub-sandbox/runtime") {
            Some("monitor")
        } else if exe == Path::new("/usr/libexec/nub/nub-bwrap") {
            Some("helper")
        } else {
            None
        }
    }

    fn scan_all_harness_pids() -> Result<Vec<(u32, &'static str)>, String> {
        let self_exe = fs::read_link("/proc/self/exe")
            .map_err(|error| format!("reading self exe: {error}"))?;
        let mut found = Vec::new();
        for entry in fs::read_dir("/proc").map_err(|error| format!("reading /proc: {error}"))? {
            let entry = entry.map_err(|error| format!("reading a /proc entry: {error}"))?;
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            // Same-uid processes of the whole chain have a readable exe link; a foreign
            // or gone process is skipped — it cannot be one of ours.
            if let Ok(exe) = fs::read_link(format!("/proc/{pid}/exe"))
                && let Some(role) = harness_role(&exe, &self_exe)
            {
                found.push((pid, role));
            }
        }
        Ok(found)
    }
}

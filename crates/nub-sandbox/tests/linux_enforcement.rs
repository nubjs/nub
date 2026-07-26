//! Linux Bubblewrap backend — REAL enforcement tests.
//!
//! Each test compiles a surface policy, applies it, SPAWNS the child under
//! a private mount/PID/network view, and asserts the kernel allowed or denied the
//! action. Every confinement assertion is paired with a NEGATIVE CONTROL (the axis lifted → the
//! same action succeeds) so a pass can't be hollow. Linux-only, and a no-op (skips)
//! where Bubblewrap cannot create an unprivileged user namespace; the dev host proves
//! it in a Lima/Colima Ubuntu 24.04 VM.
//!
//! A tiny C probe compiled at runtime with `cc` checks namespace/capability behavior
//! without depending on shell or language-runtime interpretations of errno.
#![cfg(target_os = "linux")]

use nub_sandbox::compiler::{CompileCtx, ScopeCapabilities, ShellRunner};
use nub_sandbox::matcher::Homes;
use nub_sandbox::{
    CommandSpec, PreparedSignalTarget, RuntimeCapability, apply_with_runtime, compile,
};
use serde_json::Value;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn apply(
    policy: &nub_sandbox::policy::SandboxPolicy,
    spec: CommandSpec,
) -> Result<nub_sandbox::Prepared, nub_sandbox::Degradation> {
    // Every ordinary launch shares the test-only topology lock. Only the test
    // that temporarily changes the host mount table calls the explicit unlocked
    // helper while holding the exclusive side, so Bubblewrap can never inventory
    // another fixture's transient mount.
    let _mount_topology = host_mount_topology_lock()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    apply_without_mount_topology_lock(policy, spec)
}

fn apply_without_mount_topology_lock(
    policy: &nub_sandbox::policy::SandboxPolicy,
    spec: CommandSpec,
) -> Result<nub_sandbox::Prepared, nub_sandbox::Degradation> {
    static RUNTIME: std::sync::OnceLock<RuntimeCapability> = std::sync::OnceLock::new();
    let runtime = RUNTIME.get_or_init(|| {
        RuntimeCapability::from_verified_executable(env!(
            "CARGO_BIN_EXE_nub-sandbox-monitor-harness"
        ))
        .expect("verify the purpose-built sandbox monitor harness")
    });
    apply_with_runtime(policy, spec, runtime)
}

fn bwrap_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        ["/usr/bin/bwrap", "/bin/bwrap"].iter().any(|program| {
            std::process::Command::new(program)
                .args([
                    "--die-with-parent",
                    "--unshare-user",
                    "--ro-bind",
                    "/",
                    "/",
                    "--",
                    "/bin/true",
                ])
                .status()
                .is_ok_and(|status| status.success())
        })
    })
}

/// Returns true when the caller should skip. A conformance runner can require the
/// primitive so an unavailable user namespace never reads as a green hollow skip.
fn skip_without_bwrap() -> bool {
    if bwrap_available() {
        return false;
    }
    let required = matches!(
        std::env::var("NUB_SANDBOX_REQUIRE_BWRAP").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    );
    assert!(
        !required,
        "NUB_SANDBOX_REQUIRE_BWRAP set but Bubblewrap cannot create the required user namespace"
    );
    true
}

fn host_mount_topology_lock() -> &'static std::sync::RwLock<()> {
    static LOCK: std::sync::OnceLock<std::sync::RwLock<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::RwLock::new(()))
}

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    proj: PathBuf,
    home: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let proj = root.join("proj");
    let home = root.join("home");
    std::fs::create_dir_all(proj.join("sub")).unwrap();
    std::fs::create_dir_all(proj.join("nested")).unwrap();
    std::fs::create_dir_all(proj.join("writable")).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(proj.join("pub.txt"), "PUBLIC").unwrap();
    std::fs::write(proj.join("sub/nested.txt"), "N").unwrap();
    std::fs::write(proj.join(".env"), "ENVSECRET").unwrap();
    std::fs::write(proj.join("sub/.env"), "NESTEDENV").unwrap();
    std::fs::write(home.join(".ssh/id_rsa"), "IDRSA").unwrap();
    Fixture {
        _tmp: tmp,
        root,
        proj,
        home,
    }
}

impl Fixture {
    fn homes(&self) -> Homes {
        Homes {
            home: self.home.clone(),
            tmp: std::env::temp_dir(),
            cache: self.home.join(".cache"),
            project: self.proj.clone(),
        }
    }
    fn ctx(&self, env: &[(&str, &str)]) -> CompileCtx {
        CompileCtx {
            homes: self.homes(),
            cwd: self.proj.clone(),
            policy_file: None,
            caps: ScopeCapabilities::approved(),
            ambient_env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            runner: Box::new(ShellRunner),
        }
    }

    /// Spawn `program args…` under `surface` (with `env` as the ambient snapshot) and
    /// return `(exit_code, stdout)`. stderr → null; stdout captured for the /proc test.
    fn run(
        &self,
        surface: Value,
        env: &[(&str, &str)],
        program: &str,
        args: &[&str],
    ) -> (i32, String) {
        self.run_inner(surface, env, program, args, false)
    }

    fn run_without_mount_topology_lock(
        &self,
        surface: Value,
        env: &[(&str, &str)],
        program: &str,
        args: &[&str],
    ) -> (i32, String) {
        self.run_inner(surface, env, program, args, true)
    }

    fn run_inner(
        &self,
        surface: Value,
        env: &[(&str, &str)],
        program: &str,
        args: &[&str],
        mount_topology_already_locked: bool,
    ) -> (i32, String) {
        let policy = compile(&surface, &self.ctx(env)).expect("compiles");
        let spec = CommandSpec::new(program)
            .args(args.iter().copied())
            .cwd(&self.proj)
            // These stand in for the workspace root and known package roots the
            // frontend supplies. The backend never recursively discovers them.
            .deny_search_roots([
                self.proj.clone(),
                self.proj.join("sub"),
                self.proj.join("nested"),
            ]);
        let prepared = if mount_topology_already_locked {
            apply_without_mount_topology_lock(&policy, spec)
        } else {
            apply(&policy, spec)
        };
        let out = prepared.expect("apply").output().expect("spawn");
        if !out.status.success() && !out.stderr.is_empty() {
            eprintln!(
                "sandbox child stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    fn ok(&self, surface: Value, program: &str, args: &[&str]) -> bool {
        self.run(surface, &[], program, args).0 == 0
    }
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

const CAT: &str = "/bin/cat";
const TOUCH: &str = "/usr/bin/touch";
const SH: &str = "/bin/sh";
const TRUE: &str = "/bin/true";

// ── fs read-confine (allowlist) ────────────────────────────────────────────────

#[test]
fn read_confine_allows_project_denies_outside() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let confine = serde_json::json!({ "fs": ["./"] });
    assert!(
        f.ok(confine.clone(), CAT, &[&s(&f.proj.join("pub.txt"))]),
        "project read"
    );
    assert!(
        f.ok(confine.clone(), CAT, &[&s(&f.proj.join("sub/nested.txt"))]),
        "nested read"
    );
    assert!(
        !f.ok(confine.clone(), CAT, &[&s(&f.home.join(".ssh/id_rsa"))]),
        "home secret denied"
    );
    // negative control — relaxed fs reads the secret fine:
    assert!(
        f.ok(
            serde_json::json!({ "fs": true }),
            CAT,
            &[&s(&f.home.join(".ssh/id_rsa"))]
        ),
        "neg-control: relaxed fs reads outside"
    );
}

#[test]
fn generous_read_denies_dotenv() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    // The compiler-injected dotenv default becomes an empty readable file at each
    // declared root.
    let surface = serde_json::json!({ "fs": [s(&f.root)] });
    assert!(
        f.ok(surface.clone(), CAT, &[&s(&f.proj.join("pub.txt"))]),
        "pub readable"
    );
    for path in [f.proj.join(".env"), f.proj.join("sub/.env")] {
        let (code, out) = f.run(surface.clone(), &[], CAT, &[&s(&path)]);
        assert_eq!(code, 0, "dotenv compatibility mask stays readable");
        assert!(out.is_empty(), "{} reads as an empty file", s(&path));
    }
    assert!(
        f.ok(
            serde_json::json!({ "fs": true }),
            CAT,
            &[&s(&f.proj.join(".env"))]
        ),
        "neg-control: relaxed fs reads .env"
    );
}

#[test]
fn concurrent_launches_keep_argument_fds_bound_to_their_own_mount_views() {
    if skip_without_bwrap() {
        return;
    }
    let left = fixture();
    let right = fixture();
    std::fs::write(left.proj.join("pub.txt"), "LEFT-SENTINEL").unwrap();
    std::fs::write(right.proj.join("pub.txt"), "RIGHT-SENTINEL").unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let launch =
        |fixture: Fixture, expected: &'static str, barrier: std::sync::Arc<std::sync::Barrier>| {
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..8 {
                    let (code, output) = fixture.run(
                        serde_json::json!({ "fs": [s(&fixture.root)] }),
                        &[],
                        CAT,
                        &[&s(&fixture.proj.join("pub.txt"))],
                    );
                    assert_eq!(code, 0);
                    assert_eq!(output, expected);
                }
            })
        };
    let left = launch(left, "LEFT-SENTINEL", std::sync::Arc::clone(&barrier));
    let right = launch(right, "RIGHT-SENTINEL", std::sync::Arc::clone(&barrier));
    barrier.wait();
    left.join().unwrap();
    right.join().unwrap();
}

#[test]
fn existing_sandbox_config_files_are_unreadable_and_pinned() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let root_policy = f.proj.join("tool.sandbox.json");
    let package_policy = f.proj.join("nested/package.sandbox.json");
    std::fs::create_dir_all(package_policy.parent().unwrap()).unwrap();
    std::fs::write(&root_policy, "ROOTSECRET").unwrap();
    std::fs::write(&package_policy, "PACKAGESECRET").unwrap();
    let surface = serde_json::json!({
        "fs": ["...", "./", "!**/*.sandbox.json"]
    });

    for path in [&root_policy, &package_policy] {
        assert!(
            !f.ok(surface.clone(), CAT, &[&s(path)]),
            "{} is unreadable",
            s(path)
        );
        assert!(
            !f.ok(surface.clone(), "/bin/rm", &["-f", &s(path)]),
            "{} cannot be unlinked from beneath its mask mount",
            s(path)
        );
    }
    assert_eq!(std::fs::read_to_string(root_policy).unwrap(), "ROOTSECRET");
    assert_eq!(
        std::fs::read_to_string(package_policy).unwrap(),
        "PACKAGESECRET"
    );
}

#[test]
fn sandbox_config_created_after_start_is_out_of_scope() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let future = f.proj.join("future.sandbox.json");
    let surface = serde_json::json!({
        "fs": ["...", "./", "!**/*.sandbox.json"]
    });
    let script = format!(
        "printf CREATED > '{}' && test \"$(cat '{}')\" = CREATED",
        future.display(),
        future.display()
    );
    assert!(
        f.ok(surface, SH, &["-c", &script]),
        "a deny target missing from the startup snapshot is intentionally not tracked"
    );
}

#[test]
fn private_tmp_hides_shared_tmp_and_remains_writable() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let marker = std::env::temp_dir().join(format!("nub-shared-tmp-{}", std::process::id()));
    let private_probe = format!("nub-private-tmp-{}", std::process::id());
    std::fs::write(&marker, "HOST").unwrap();
    let script = format!(
        "test ! -e '{}' && test \"$TMPDIR\" = /tmp && printf PRIVATE > /tmp/{}",
        marker.display(),
        private_probe
    );
    assert!(
        f.ok(
            serde_json::json!({ "fs": { "./": "rw", "$tmp": "rw" } }),
            SH,
            &["-c", &script],
        ),
        "private tmp is writable and hides the shared host tmp"
    );
    assert!(!std::env::temp_dir().join(private_probe).exists());
    std::fs::remove_file(marker).unwrap();
}

#[test]
fn denied_tmp_still_allows_an_explicit_project_under_tmp() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let blocked = std::env::temp_dir().join(format!("nub-denied-tmp-{}", std::process::id()));
    let script = format!(
        "! touch '{}' && printf PROJECT > ./tmp-policy-project-write",
        blocked.display()
    );
    assert!(
        f.ok(
            serde_json::json!({ "fs": { "./": "rw", "$tmp": false } }),
            SH,
            &["-c", &script],
        ),
        "a project bind layered over denied /tmp stays writable"
    );
    assert!(!blocked.exists());
    assert_eq!(
        std::fs::read_to_string(f.proj.join("tmp-policy-project-write")).unwrap(),
        "PROJECT"
    );
}

// ── new secret deny-set additions (A2): each denied under a generous read ──────

#[test]
fn new_secret_paths_denied_under_generous_read() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    // Home-anchored secret files/dirs (the deny-set additions), anchored to the
    // fixture home so the walk carves fixture paths, never the real `~`.
    std::fs::write(f.home.join(".pgpass"), "PGPASS").unwrap();
    std::fs::write(f.home.join(".pypirc"), "PYPIRC").unwrap();
    std::fs::create_dir_all(f.home.join(".gnupg")).unwrap();
    std::fs::write(f.home.join(".gnupg/secring.gpg"), "GPGKEY").unwrap();
    std::fs::create_dir_all(f.home.join(".config/git")).unwrap();
    std::fs::write(f.home.join(".config/git/credentials"), "GITCREDS").unwrap();
    // Project-local: `.env` + `.env.local` DIRECTORY subtrees (not just leaves) + direnv
    // `.envrc`.
    std::fs::create_dir_all(f.proj.join("nested/.env")).unwrap();
    std::fs::write(f.proj.join("nested/.env/prod"), "ENVDIRSECRET").unwrap();
    std::fs::create_dir_all(f.proj.join("nested/.env.local")).unwrap();
    std::fs::write(f.proj.join("nested/.env.local/prod"), "ENVLOCALDIRSECRET").unwrap();
    std::fs::write(f.proj.join(".envrc"), "export SECRET=x").unwrap();

    let generous = serde_json::json!({ "fs": ["..."] });
    for (path, label) in [
        (f.home.join(".pgpass"), ".pgpass"),
        (f.home.join(".pypirc"), ".pypirc"),
        (f.home.join(".gnupg/secring.gpg"), ".gnupg subtree"),
        (
            f.home.join(".config/git/credentials"),
            "git credential store",
        ),
        (f.proj.join("nested/.env/prod"), ".env directory subtree"),
        (
            f.proj.join("nested/.env.local/prod"),
            ".env.local directory subtree",
        ),
        (f.proj.join(".envrc"), ".envrc"),
    ] {
        let (_code, out) = f.run(generous.clone(), &[], CAT, &[&s(&path)]);
        assert!(
            out.is_empty(),
            "{label} must not reveal bytes under generous read"
        );
    }
    // negative controls — relaxed fs reads each fine, so the deny (not a missing
    // file) is the cause above. Cover a home file + both new glob mechanisms.
    for path in [
        f.home.join(".pgpass"),
        f.proj.join("nested/.env/prod"),
        f.proj.join(".envrc"),
    ] {
        assert!(
            f.ok(serde_json::json!({ "fs": true }), CAT, &[&s(&path)]),
            "neg-control: relaxed fs reads {}",
            s(&path)
        );
    }
}

// ── fs write-confine ────────────────────────────────────────────────────────────

#[test]
fn write_confine_allows_target_denies_rest() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let wc = serde_json::json!({ "fs": ["...", "./writable"] });
    assert!(
        f.ok(wc.clone(), TOUCH, &[&s(&f.proj.join("writable/ok.txt"))]),
        "write inside grant"
    );
    assert!(
        !f.ok(wc.clone(), TOUCH, &[&s(&f.proj.join("blocked.txt"))]),
        "write project root denied"
    );
    assert!(
        !f.ok(wc, TOUCH, &[&s(&f.home.join("w.txt"))]),
        "write outside denied"
    );
    assert!(
        f.ok(
            serde_json::json!({ "fs": true }),
            TOUCH,
            &[&s(&f.home.join("w2.txt"))]
        ),
        "neg-control: relaxed fs writes outside"
    );
}

#[test]
fn ordered_mounts_preserve_readonly_child_and_writable_reopen() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let parent = f.proj.join("writable/layered");
    let child = parent.join("readonly");
    let reopened = child.join("reopened");
    std::fs::create_dir_all(&reopened).unwrap();

    let mut entries = serde_json::Map::new();
    entries.insert(s(&parent), serde_json::json!("rw"));
    entries.insert(s(&child), serde_json::json!("r"));
    entries.insert(s(&reopened), serde_json::json!("rw"));
    let surface = serde_json::json!({ "fs": serde_json::Value::Object(entries) });
    assert!(
        f.ok(surface.clone(), TOUCH, &[&s(&parent.join("parent-write"))]),
        "writable parent remains writable"
    );
    assert!(
        !f.ok(surface.clone(), TOUCH, &[&s(&child.join("blocked-write"))]),
        "read-only child caps the writable parent"
    );
    assert!(
        f.ok(surface, TOUCH, &[&s(&reopened.join("reopened-write"))]),
        "narrower later writable mount reopens only its subtree"
    );
}

// ── fs write-confine: dangerous-root guard (F2 cross-OS consistency) ────────────

#[test]
fn dangerous_write_root_grant_is_rejected() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    for surface in [
        serde_json::json!({ "fs": ["...", "/"] }),
        serde_json::json!({
            "fs": ["...", format!("{}/../../../../../../../../../../..", s(&f.proj))]
        }),
    ] {
        let policy = compile(&surface, &f.ctx(&[])).unwrap();
        let error = apply(
            &policy,
            CommandSpec::new(TRUE)
                .cwd(&f.proj)
                .deny_search_root(&f.proj),
        )
        .err()
        .expect("unsafe write root must fail setup");
        assert!(
            error
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("writable whole-filesystem")
                    || reason.contains("too broad")),
            "unsafe write root must fail setup: {error:?}"
        );
    }

    // POSITIVE CONTROL — the guard drops ONLY dangerous roots: a legitimate scoped
    // rw grant still writes inside the project.
    let scoped = serde_json::json!({ "fs": ["...", "./writable"] });
    assert!(
        f.ok(scoped, TOUCH, &[&s(&f.proj.join("writable/ok.txt"))]),
        "positive control: scoped rw grant still writes inside the project"
    );
}

// ── adversarial: a symlink or `..` must not dodge read-confine ──────────────────

#[test]
fn confine_not_dodgeable_via_symlink_or_dotdot() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    // A symlink inside the confined project pointing OUT to an unmounted home secret
    // cannot make that target appear in the private filesystem view.
    let link = f.proj.join("escape");
    std::os::unix::fs::symlink(f.home.join(".ssh/id_rsa"), &link).unwrap();
    let confine = serde_json::json!({ "fs": ["./"] });
    assert!(
        !f.ok(confine.clone(), CAT, &[&s(&link)]),
        "symlink escaping read-confine denied"
    );
    // A `..` traversal out of the project to the same key is likewise denied.
    let dotdot = f.proj.join("../home/.ssh/id_rsa");
    assert!(!f.ok(confine, CAT, &[&s(&dotdot)]), "'..' escape denied");
    // negative control — relaxed fs follows the symlink fine.
    assert!(
        f.ok(serde_json::json!({ "fs": true }), CAT, &[&s(&link)]),
        "neg-control: relaxed fs follows the symlink"
    );
}

// ── the env-read boundary: /proc/<ancestor>/environ ─────────────────────────────

#[test]
fn ancestor_proc_environ_is_unreadable() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    // The ancestor is THIS test process; its REAL environ holds `PATH=`. The child
    // tries to read the ancestor's `/proc/<ppid>/environ`; `$PPID` is the shell's
    // parent = the test process (which spawns sh directly). This isolates the
    // READ-CONFINE mechanism (no env axis): under `{fs:["./"]}`, /proc is never
    // granted → the read is denied. (The env-scrub mechanism — a scrubbed child
    // recovering the ancestor env — is proven separately by
    // env_scrub_alone_closes_proc_even_with_fs_relaxed.)
    let read_environ = "/bin/cat /proc/$PPID/environ 2>/dev/null || true";
    let (_c, out) = f.run(
        serde_json::json!({ "fs": ["./"] }),
        &[],
        SH,
        &["-c", read_environ],
    );
    assert!(
        !out.contains("PATH="),
        "ancestor environ must be unreadable under read-confine (got {} bytes)",
        out.len()
    );
    // negative control — nothing enforced → readable. `sandbox: false` is the
    // unambiguous fully-unjailed surface (a bare `{ fs: true }` now floors net+env).
    let (_c2, out2) = f.run(serde_json::json!(false), &[], SH, &["-c", read_environ]);
    assert!(
        out2.contains("PATH="),
        "neg-control: relaxed fs CAN read the ancestor environ"
    );
}

#[test]
fn env_scrub_alone_closes_proc_even_with_fs_relaxed() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    // A pure env-scrub with fs RELAXED must STILL close /proc — otherwise the
    // scrubbed child recovers the ancestor's env via /proc/<ppid>/environ, defeating
    // the scrub. `{env:false}` with a non-empty ambient WITHHOLDS a var, so the
    // backend gives the child a private PID namespace and fresh procfs.
    let read_environ = "/bin/cat /proc/$PPID/environ 2>/dev/null || true";
    // `fs: true` is now NAMED explicitly: under the complete-statement model an
    // unlisted axis FLOORS (deny-all), so `{ env: false }` alone would confine fs
    // and the probe could not exec — this test wants env scrub with fs RELAXED.
    let (_c, out) = f.run(
        serde_json::json!({ "vars": false, "fs": true }),
        &[("AMBIENT_SECRET", "s")],
        SH,
        &["-c", read_environ],
    );
    assert!(
        !out.contains("PATH="),
        "env-scrub must close /proc even with fs relaxed (got {} bytes)",
        out.len()
    );
    // negative control — nothing enforced → /proc readable. `sandbox: false` is the
    // unambiguous "fully unjailed" surface (a bare `{ fs: true }` now floors net+env).
    let (_c2, out2) = f.run(serde_json::json!(false), &[], SH, &["-c", read_environ]);
    assert!(
        out2.contains("PATH="),
        "neg-control: unsandboxed reads /proc"
    );
}

#[test]
fn every_sandboxed_process_gets_a_private_proc_view() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    // Even with env passthrough, a sandboxed process gets a fresh procfs. It may see
    // itself and its in-namespace PID-1 monitor, never Nub's host-side parent environ.
    let read_environ = format!(
        "/bin/cat /proc/{}/environ 2>/dev/null || true",
        std::process::id()
    );
    // fs named RELAXED explicitly (an unlisted axis now floors → the probe couldn't
    // exec); this test is about env passthrough NOT closing /proc.
    let (_c, out) = f.run(
        serde_json::json!({ "vars": true, "fs": true }),
        &[("FOO", "bar")],
        SH,
        &["-c", &read_environ],
    );
    assert!(
        out.is_empty(),
        "host parent environ must stay outside the PID namespace"
    );
}

#[test]
fn explicit_denies_win_below_fresh_proc_and_dev_mounts() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    assert!(
        !f.ok(
            serde_json::json!({ "fs": ["...", "./", "!/proc/1/status"] }),
            CAT,
            &["/proc/1/status"]
        ),
        "a policy deny below the fresh procfs must remain effective"
    );
    assert!(
        !f.ok(
            serde_json::json!({ "fs": ["...", "./", "!/dev/null"] }),
            CAT,
            &["/dev/null"]
        ),
        "a policy deny below the fresh device tree must remain effective"
    );
}

#[test]
fn inherited_secret_file_descriptors_are_closed_before_bubblewrap_exec() {
    use std::os::fd::AsRawFd;

    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let secret = std::fs::File::open(f.proj.join(".env")).unwrap();
    let fd = secret.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) },
        0
    );

    let policy = compile(&serde_json::json!({ "fs": ["...", "./"] }), &f.ctx(&[])).unwrap();
    let script = format!("cat /proc/self/fd/{fd} 2>/dev/null || true");
    let prepared = apply(
        &policy,
        CommandSpec::new(SH)
            .args(["-c", &script])
            .cwd(&f.proj)
            .deny_search_root(&f.proj),
    )
    .unwrap();
    let output = prepared.output().unwrap();
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("ENVSECRET"),
        "an already-open descriptor must not bypass the path mask"
    );
}

#[test]
fn bind_mounted_procfs_at_nonstandard_path_is_masked() {
    if skip_without_bwrap() {
        return;
    }
    let _mount_topology = host_mount_topology_lock()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = fixture();
    // A host procfs bind-mounted outside `/proc` would otherwise be carried through a
    // broad root bind. The backend reads mountinfo before launch and masks every such
    // alternate mount, while replacing canonical `/proc` with the sandbox's fresh view.
    //
    // The bind-mount needs CAP_SYS_ADMIN. Prefer a PRE-PROVISIONED mount named by
    // `NUB_SANDBOX_ALTPROC` (a privileged setup step points it at a `mount --bind /proc`
    // target) so the residual is exercised UNPRIVILEGED — the setting it's documented for
    // and where the ptrace bound is crisp. Else bind-mount in-test (root). Skip cleanly
    // (never a hollow pass) when neither yields a live procfs.
    struct Unmount(Option<String>);
    impl Drop for Unmount {
        fn drop(&mut self) {
            if let Some(p) = &self.0 {
                let _ = std::process::Command::new("umount").arg(p).status();
            }
        }
    }
    let (altproc, _guard) = match std::env::var_os("NUB_SANDBOX_ALTPROC") {
        Some(p) => {
            let p = PathBuf::from(p);
            if !p.join("version").exists() {
                panic!("NUB_SANDBOX_ALTPROC={p:?} is not a live procfs bind mount");
            }
            (p, Unmount(None))
        }
        None => {
            let altproc = f.root.join("altproc");
            std::fs::create_dir_all(&altproc).unwrap();
            let mounted = std::process::Command::new("mount")
                .args(["--bind", "/proc", &s(&altproc)])
                .status()
                .map(|st| st.success())
                .unwrap_or(false);
            if !mounted {
                eprintln!(
                    "skipping: no NUB_SANDBOX_ALTPROC and cannot bind-mount /proc (needs CAP_SYS_ADMIN)"
                );
                return;
            }
            (altproc.clone(), Unmount(Some(s(&altproc))))
        }
    };

    // Exercise the coding-agent posture: broad host visibility with network/process
    // isolation forcing a Bubblewrap view. The alternate mount would be visible through
    // the root bind if the mount-table mask were absent.
    let broad_read = serde_json::json!({ "fs": true, "net": false, "vars": true });

    // Canonical `/proc` is a fresh procfs for the child PID namespace. The alternate
    // host procfs is masked even though its regular parent is readable.
    let alt_version = s(&altproc.join("version"));
    let (_c, altver) =
        f.run_without_mount_topology_lock(broad_read.clone(), &[], CAT, &[&alt_version]);
    assert!(
        !altver.contains("Linux"),
        "procfs bind-mounted outside `/proc` must be masked (got {altver:?})"
    );

    // The ancestor's environment is likewise unavailable through the alternate path.
    let read_environ = format!("/bin/cat {}/$PPID/environ 2>/dev/null || true", s(&altproc));
    let (_c, env_confined) =
        f.run_without_mount_topology_lock(broad_read, &[], SH, &["-c", &read_environ]);
    let (_c, env_control) = f.run_without_mount_topology_lock(
        serde_json::json!(false),
        &[],
        SH,
        &["-c", &read_environ],
    );
    if env_control.contains("PATH=") {
        assert!(
            !env_confined.contains("PATH="),
            "bound: the ancestor environ must stay closed under confinement via the alt \
             mount (unconfined control read it; confined must not)"
        );
    } else {
        eprintln!(
            "note: unconfined control could not read the ancestor environ; the mask still blocks procfs metadata"
        );
    }
}

// ── env-scrub (construction) ────────────────────────────────────────────────────

#[test]
fn env_scrub_strips_secret_keeps_baseline() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let env = &[("PATH", "/usr/bin:/bin"), ("MY_SECRET_TOKEN", "leaked")];
    let strip = serde_json::json!(true); // curated baseline (wrapper true → generous-read fs)
    assert!(
        f.run(strip.clone(), env, SH, &["-c", "test -n \"$PATH\""])
            .0
            == 0,
        "PATH kept"
    );
    assert!(
        f.run(strip, env, SH, &["-c", "test -z \"$MY_SECRET_TOKEN\""])
            .0
            == 0,
        "secret stripped"
    );
    // fs named RELAXED (an unlisted axis floors → SH couldn't exec); this checks
    // that axis `env: true` passthrough keeps the ambient secret.
    assert!(
        f.run(
            serde_json::json!({ "vars": true, "fs": true }),
            env,
            SH,
            &["-c", "test -n \"$MY_SECRET_TOKEN\""]
        )
        .0 == 0,
        "neg-control: passthrough keeps the secret"
    );
}

#[test]
fn withheld_environment_hardens_keyrings_without_restricting_full_network() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let Some(probe) = probe_bin(&f.proj) else {
        return;
    };
    let probe = s(&probe);
    let ambient = &[("MY_SECRET_TOKEN", "leaked")];
    let protected = serde_json::json!({ "vars": false, "fs": true, "net": true });
    assert_eq!(
        f.run(protected.clone(), ambient, &probe, &["keyring"]).0,
        0,
        "all three keyring-management syscalls are denied"
    );
    assert_eq!(
        f.run(protected.clone(), ambient, &probe, &["keyproc"]).0,
        0,
        "keyring procfs entries are unreadable"
    );
    assert_eq!(
        f.run(protected, ambient, &probe, &["unixsocket"]).0,
        0,
        "keyring-only hardening preserves Unix sockets under full networking"
    );
    let socket_path = f.root.join("container-engine.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
    assert_eq!(
        f.run(
            serde_json::json!({ "vars": false, "fs": true, "net": true }),
            ambient,
            &probe,
            &["unixconnect", &s(&socket_path)],
        )
        .0,
        0,
        "keyring-only hardening preserves Docker/Podman-style Unix socket access"
    );

    let passthrough = serde_json::json!({ "vars": true, "fs": ["...", "./"], "net": true });
    assert_eq!(
        f.run(passthrough.clone(), ambient, &probe, &["keyprocopen"])
            .0,
        0,
        "environment passthrough does not mask keyring procfs entries"
    );
    assert_eq!(
        f.run(passthrough, ambient, &probe, &["unixsocket"]).0,
        0,
        "environment passthrough preserves full-network Unix sockets"
    );
}

#[test]
fn target_path_resolves_relative_to_child_cwd_and_mounts_the_entry() {
    if skip_without_bwrap() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;

    let f = fixture();
    let bin = f.proj.join("target-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let tool = bin.join("tool");
    std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

    let surface = serde_json::json!({
        "fs": { "./writable": "rw" },
        "vars": { "PATH": true },
        "net": true
    });
    assert_eq!(
        f.run(surface, &[("PATH", "target-bin")], "tool", &[]).0,
        0,
        "the entry is resolved from the target PATH relative to the target cwd"
    );
}

#[test]
fn launcher_env_is_sterile_while_target_receives_adversarial_preload() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let source = f.proj.join("preload.c");
    let library = f.proj.join("preload.so");
    std::fs::write(
        &source,
        r#"
#include <fcntl.h>
#include <stdlib.h>
#include <unistd.h>
__attribute__((constructor)) static void mark(void) {
  const char *paths[] = { getenv("PRELOAD_OUTSIDE"), getenv("PRELOAD_INSIDE") };
  for (int i = 0; i < 2; i++) if (paths[i]) {
    int fd = open(paths[i], O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (fd >= 0) { write(fd, "loaded", 6); close(fd); }
  }
}
"#,
    )
    .unwrap();
    if !std::process::Command::new("cc")
        .args(["-shared", "-fPIC"])
        .arg(&source)
        .arg("-o")
        .arg(&library)
        .status()
        .is_ok_and(|status| status.success())
    {
        return;
    }
    let outside = f.root.join("preload-outside");
    let inside = f.proj.join("preload-inside");
    let library_string = s(&library);
    let outside_string = s(&outside);
    let inside_string = s(&inside);
    let env = [
        ("PATH", "/usr/bin:/bin"),
        ("LD_PRELOAD", library_string.as_str()),
        ("PRELOAD_OUTSIDE", outside_string.as_str()),
        ("PRELOAD_INSIDE", inside_string.as_str()),
        ("LAUNCH_SECRET", "not-in-bwrap-argv"),
    ];
    let surface = serde_json::json!({
        "fs": ["...", "./"],
        "net": true,
        "vars": {
            "PATH": true,
            "LD_PRELOAD": true,
            "PRELOAD_OUTSIDE": true,
            "PRELOAD_INSIDE": true,
            "LAUNCH_SECRET": true
        }
    });
    let policy = compile(&surface, &f.ctx(&env)).unwrap();
    let prepared = apply(
        &policy,
        CommandSpec::new(SH)
            .args([
                "-c",
                "test -n \"$LD_PRELOAD\" && test -e \"$PRELOAD_INSIDE\" && sleep 2",
            ])
            .cwd(&f.proj)
            .deny_search_root(&f.proj),
    )
    .unwrap();
    let mut child = prepared.spawn().unwrap();
    let cmdline = std::fs::read(format!("/proc/{}/cmdline", child.id())).unwrap();
    assert!(
        !cmdline
            .windows(b"not-in-bwrap-argv".len())
            .any(|window| window == b"not-in-bwrap-argv"),
        "target environment secrets stay in the anonymous argument descriptor"
    );
    assert!(child.wait().unwrap().success());
    assert!(inside.exists(), "the target received LD_PRELOAD");
    assert!(
        !outside.exists(),
        "the Bubblewrap launcher did not execute the target-controlled preload before confinement"
    );
}

#[test]
fn dropping_prepared_child_tears_down_the_pid_namespace() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let policy = compile(
        &serde_json::json!({ "fs": ["...", "./"], "net": true, "vars": true }),
        &f.ctx(&[("PATH", "/usr/bin:/bin")]),
    )
    .unwrap();
    let child = apply(
        &policy,
        CommandSpec::new(SH)
            .args(["-c", "sleep 30"])
            .cwd(&f.proj)
            .deny_search_root(&f.proj),
    )
    .unwrap()
    .spawn()
    .unwrap();
    let target = child.id() as i32;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let descendant = loop {
        let children = std::fs::read_to_string(format!("/proc/{target}/task/{target}/children"))
            .unwrap_or_default();
        if let Some(pid) = children
            .split_ascii_whitespace()
            .next()
            .and_then(|pid| pid.parse::<i32>().ok())
        {
            break pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the sandbox target did not create its sleep descendant"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    };
    drop(child);
    for (pid, label) in [(target, "target"), (descendant, "descendant")] {
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "{label} remained alive");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH),
            "the retained namespace {label} is gone before resources are released"
        );
    }
}

#[test]
fn abrupt_parent_helper_holds_pid_namespace() {
    let Some(report) = std::env::var_os("NUB_SANDBOX_ABRUPT_PARENT_REPORT") else {
        return;
    };
    let f = fixture();
    let policy = compile(
        &serde_json::json!({ "fs": ["...", "./"], "net": true, "vars": true }),
        &f.ctx(&[("PATH", "/usr/bin:/bin")]),
    )
    .unwrap();
    let child = apply(
        &policy,
        CommandSpec::new(SH)
            .args(["-c", "sleep 30"])
            .cwd(&f.proj)
            .deny_search_root(&f.proj),
    )
    .unwrap()
    .spawn()
    .unwrap();
    let target = child.id() as i32;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let descendant = loop {
        let children = std::fs::read_to_string(format!("/proc/{target}/task/{target}/children"))
            .unwrap_or_default();
        if let Some(pid) = children
            .split_ascii_whitespace()
            .next()
            .and_then(|pid| pid.parse::<i32>().ok())
        {
            break pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the sandbox target did not create its sleep descendant"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    };
    std::fs::write(
        report,
        format!("{target}\n{descendant}\n{}\n", f._tmp.path().display()),
    )
    .unwrap();
    std::mem::forget(f);
    std::mem::forget(child);
    loop {
        std::thread::park();
    }
}

#[test]
fn abrupt_parent_death_tears_down_the_pid_namespace() {
    if skip_without_bwrap() {
        return;
    }
    let report_dir = tempfile::tempdir().unwrap();
    let report = report_dir.path().join("pids");
    let mut helper = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "abrupt_parent_helper_holds_pid_namespace"])
        .env("NUB_SANDBOX_ABRUPT_PARENT_REPORT", &report)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let contents = loop {
        if let Ok(contents) = std::fs::read_to_string(&report) {
            break contents;
        }
        assert!(
            helper.try_wait().unwrap().is_none(),
            "abrupt-parent helper exited before publishing its target"
        );
        if std::time::Instant::now() >= deadline {
            let _ = helper.kill();
            let _ = helper.wait();
            panic!("abrupt-parent helper did not publish its target");
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    };
    let mut lines = contents.lines();
    let target = lines.next().unwrap().parse::<i32>().unwrap();
    let descendant = lines.next().unwrap().parse::<i32>().unwrap();
    let fixture_root = lines.next().unwrap().to_owned();
    helper.kill().unwrap();
    helper.wait().unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    for (pid, label) in [(target, "target"), (descendant, "descendant")] {
        loop {
            if unsafe { libc::kill(pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "abrupt launcher death left the sandbox {label} alive"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    let _ = std::fs::remove_dir_all(fixture_root);
}

#[test]
fn retained_launch_publishes_an_authenticated_signal_callback() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let policy = compile(
        &serde_json::json!({ "fs": ["...", "./writable"], "net": true, "vars": true }),
        &f.ctx(&[("PATH", "/usr/bin:/bin")]),
    )
    .unwrap();
    let callback = std::sync::Arc::new(std::sync::Mutex::new(None));
    let observed = std::sync::Arc::clone(&callback);
    let mut child = apply(
        &policy,
        CommandSpec::new(SH)
            .args(["-c", "sleep 30"])
            .cwd(&f.proj)
            .deny_search_root(&f.proj),
    )
    .unwrap()
    .spawn_with_signal_target(move |target| {
        let PreparedSignalTarget::Callback(target) = target else {
            panic!("retained Linux launch exposed a numeric signal target");
        };
        *observed.lock().unwrap() = Some(target);
        Ok(())
    })
    .expect("spawn retained launch");
    let callback = callback.lock().unwrap().take().expect("callback published");
    callback(libc::SIGTERM).expect("relay SIGTERM");
    let status = child.wait().expect("wait for signaled target");
    assert_eq!(status.signal(), Some(libc::SIGTERM));
}

#[test]
fn delayed_wait_preserves_authenticated_target_status() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let policy =
        compile(&serde_json::json!({ "fs": ["...", "./"] }), &f.ctx(&[])).expect("compiles");
    let mut child = apply(
        &policy,
        CommandSpec::new("/bin/sh")
            .args(["-c", "exit 23"])
            .cwd(&f.proj)
            .deny_search_root(&f.proj),
    )
    .expect("prepare delayed-wait sandbox")
    .spawn()
    .expect("spawn delayed-wait sandbox");
    std::thread::sleep(std::time::Duration::from_secs(6));
    let status = child.wait().expect("authenticate delayed target status");
    assert_eq!(status.code(), Some(23));
}

// ── namespace and capability probes ────────────────────────────────────────────

/// Compile the syscall probe once; `None` if `cc` is unavailable (probe tests skip).
fn probe_bin(dir: &Path) -> Option<PathBuf> {
    let src = dir.join("probe.c");
    std::fs::write(&src, PROBE_C).ok()?;
    let bin = dir.join("probe");
    let ok = std::process::Command::new("cc")
        .arg(&src)
        .arg("-o")
        .arg(&bin)
        .status()
        .ok()?
        .success();
    ok.then_some(bin)
}

/// Probe: `probe <cmd>` exits 42 when an operation was denied, 0 when it went through
/// (43 = an over-deny the test flags). Commands:
///   socket/unixsocket/unixconnect/vmread/ptrace — harmless syscalls checked per policy.
///   keyring/keyproc/keyprocopen — keyring syscall and procfs hardening probes.
///   connect PORT — connect to a host loopback listener (hidden by the net namespace).
///   unshare/clone3/clonens — prove nested namespace creation remains available.
///   mount — prove the target has no mount capability in its current namespace.
///   unmountmask PATH — enter nested user+mount namespaces and prove a mask stays pinned.
///   pidfd — prove self-scoped pidfd operations remain available.
///   pty      — open /dev/ptmx, grantpt/unlockpt, open the /dev/pts slave: 0 iff the
///              private device view admits a full PTY.
const PROBE_C: &str = r#"
#define _GNU_SOURCE
#include <errno.h>
#include <string.h>
#include <stdint.h>
#include <stdlib.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <netinet/in.h>
#include <sys/ptrace.h>
#include <sys/mount.h>
#include <sys/uio.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif
#ifndef SYS_pidfd_getfd
#define SYS_pidfd_getfd 438
#endif
#ifndef SYS_clone3
#define SYS_clone3 435
#endif
#ifndef SYS_io_uring_setup
#define SYS_io_uring_setup 425
#endif
#define P_X32_SYSCALL_BIT 0x40000000UL
#define P_CLONE_NEWNS   0x00020000
#define P_CLONE_NEWUSER 0x10000000
#define P_CLONE_FILES   0x00000400
struct clone_args_probe {
    uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls;
};
int main(int argc, char** argv) {
    if (argc < 2) return 2;
    if (!strcmp(argv[1], "socket")) {
        int fd = socket(AF_INET, SOCK_STREAM, 0);
        if (fd < 0 && errno == EPERM) return 42;
        return 0;
    }
    if (!strcmp(argv[1], "unixsocket")) {
        int fd = socket(AF_UNIX, SOCK_STREAM, 0);
        if (fd < 0) return 43;
        close(fd);
        return 0;
    }
    if (!strcmp(argv[1], "unixconnect")) {
        if (argc < 3 || strlen(argv[2]) >= sizeof(((struct sockaddr_un*)0)->sun_path)) return 2;
        int fd = socket(AF_UNIX, SOCK_STREAM, 0);
        if (fd < 0) return errno == EPERM ? 42 : 43;
        struct sockaddr_un addr; memset(&addr, 0, sizeof addr);
        addr.sun_family = AF_UNIX;
        strcpy(addr.sun_path, argv[2]);
        return connect(fd, (struct sockaddr*)&addr, sizeof addr) == 0 ? 0 : 44;
    }
    if (!strcmp(argv[1], "keyring")) {
        errno = 0;
        if (syscall(SYS_keyctl, 0, 0, 0, 0, 0) != -1 || errno != EPERM) return 43;
        errno = 0;
        if (syscall(SYS_add_key, 0, 0, 0, 0, 0) != -1 || errno != EPERM) return 44;
        errno = 0;
        if (syscall(SYS_request_key, 0, 0, 0, 0) != -1 || errno != EPERM) return 45;
        return 0;
    }
    if (!strcmp(argv[1], "keyproc") || !strcmp(argv[1], "keyprocopen")) {
        const char *paths[] = { "/proc/keys", "/proc/key-users" };
        int should_open = !strcmp(argv[1], "keyprocopen");
        for (int i = 0; i < 2; i++) {
            errno = 0;
            int fd = open(paths[i], O_RDONLY);
            if (should_open) {
                if (fd < 0) return 43 + i;
                close(fd);
            } else {
                if (fd >= 0) { close(fd); return 45 + i; }
                if (errno != EACCES && errno != EPERM) return 47 + i;
            }
        }
        return 0;
    }
#ifdef __x86_64__
    if (!strcmp(argv[1], "x32")) {
        errno = 0;
        if (syscall(SYS_socket | P_X32_SYSCALL_BIT, AF_INET, SOCK_STREAM, 0) != -1 || errno != EPERM) return 43;
        errno = 0;
        if (syscall(SYS_socket | P_X32_SYSCALL_BIT, AF_NETLINK, SOCK_RAW, 0) != -1 || errno != EPERM) return 44;
        errno = 0;
        if (syscall(SYS_io_uring_setup | P_X32_SYSCALL_BIT, 1, 0) != -1 || errno != EPERM) return 45;
        errno = 0;
        if (syscall(SYS_getpid | P_X32_SYSCALL_BIT) != -1 || errno != EPERM) return 46;
        return syscall(SYS_getpid) > 0 ? 0 : 47;
    }
#endif
    if (!strcmp(argv[1], "connect")) {
        if (argc < 3) return 2;
        int fd = socket(AF_INET, SOCK_STREAM, 0);
        if (fd < 0) return 42;
        struct sockaddr_in addr; memset(&addr, 0, sizeof addr);
        addr.sin_family = AF_INET;
        addr.sin_port = htons((uint16_t)atoi(argv[2]));
        addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        return connect(fd, (struct sockaddr*)&addr, sizeof addr) == 0 ? 0 : 42;
    }
    if (!strcmp(argv[1], "vmread")) {
        char buf[8]; char src[8] = "abcdefg";
        struct iovec l = { buf, sizeof buf }, r = { src, sizeof src };
        long n = process_vm_readv(getpid(), &l, 1, &r, 1, 0);
        if (n < 0 && errno == EPERM) return 42;
        return 0;
    }
    if (!strcmp(argv[1], "ptrace")) {
        long r = ptrace(PTRACE_TRACEME, 0, 0, 0);
        if (r == -1 && errno == EPERM) return 42;
        return 0;
    }
    if (!strcmp(argv[1], "unshare")) {
        int r = unshare(P_CLONE_FILES);
        if (r < 0 && errno == EPERM) return 42;
        return 0;
    }
    if (!strcmp(argv[1], "clone3")) {
        struct clone_args_probe a; memset(&a, 0, sizeof a);
        a.exit_signal = SIGCHLD;
        long r = syscall(SYS_clone3, &a, sizeof a);
        if (r == 0) _exit(0);                       /* child */
        if (r < 0 && (errno == ENOSYS || errno == EPERM)) return 42;
        if (r > 0) { int st; waitpid((pid_t)r, &st, 0); }
        return 0;
    }
    if (!strcmp(argv[1], "clonens")) {
        long r = syscall(SYS_clone, (long)(P_CLONE_NEWUSER | SIGCHLD), 0L, 0L, 0L, 0L);
        if (r == 0) _exit(0);                       /* child (unsandboxed: in a userns) */
        if (r < 0 && errno == EPERM) return 42;
        if (r > 0) { int st; waitpid((pid_t)r, &st, 0); }
        return 0;
    }
    if (!strcmp(argv[1], "mount")) {
        int r = mount("tmpfs", ".", "tmpfs", 0, "size=4096");
        if (r < 0 && errno == EPERM) return 42;
        return 0;
    }
    if (!strcmp(argv[1], "unmountmask")) {
        if (argc < 3) return 2;
        long child = syscall(SYS_clone, (long)(P_CLONE_NEWUSER | SIGCHLD), 0L, 0L, 0L, 0L);
        if (child < 0) return 43;
        if (child == 0) {
            if (unshare(CLONE_NEWNS) != 0) _exit(47);
            if (umount2(argv[2], 0) == 0 || umount2(argv[2], MNT_DETACH) == 0) _exit(44);
            int fd = open(argv[2], O_RDONLY);
            if (fd < 0) _exit(45);
            char byte;
            _exit(read(fd, &byte, 1) == 0 ? 0 : 46);
        }
        int status;
        if (waitpid((pid_t)child, &status, 0) < 0 || !WIFEXITED(status)) return 48;
        return WEXITSTATUS(status);
    }
    if (!strcmp(argv[1], "pidfd")) {
        long pfd = syscall(SYS_pidfd_open, getpid(), 0);
        if (pfd < 0) return 43;                     /* pidfd_open must stay allowed */
        long stolen = syscall(SYS_pidfd_getfd, (int)pfd, 0, 0);
        if (stolen < 0 && errno == EPERM) return 42;
        return 0;
    }
    if (!strcmp(argv[1], "pty")) {
        int m = posix_openpt(O_RDWR | O_NOCTTY);
        if (m < 0) return 10;                        /* /dev/ptmx open blocked */
        if (grantpt(m) != 0) return 11;
        if (unlockpt(m) != 0) return 12;
        char* sn = ptsname(m);
        if (!sn) return 13;
        int sfd = open(sn, O_RDWR | O_NOCTTY);
        if (sfd < 0) return 14;                      /* /dev/pts slave open blocked */
        return 0;
    }
    return 2;
}
"#;

#[test]
fn network_namespace_and_filter_block_host_egress() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let Some(probe) = probe_bin(&f.proj) else {
        return; // no cc — skip the syscall probes
    };
    let probe = s(&probe);

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();

    // Net enforcement creates a route-less namespace and blocks socket families that
    // can reach the host through a local IPC endpoint. Harmless self-inspection
    // syscalls remain available.
    let net_deny = serde_json::json!({ "net": false, "fs": true });
    assert_eq!(
        f.run(net_deny.clone(), &[], &probe, &["socket"]).0,
        42,
        "AF_INET socket creation is denied"
    );
    #[cfg(target_arch = "x86_64")]
    assert_eq!(
        f.run(net_deny.clone(), &[], &probe, &["x32"]).0,
        0,
        "the unsupported x32 ABI is denied before native syscall dispatch"
    );
    assert_eq!(
        f.run(net_deny.clone(), &[], &probe, &["ptrace"]).0,
        0,
        "self ptrace remains available"
    );
    assert_eq!(
        f.run(net_deny.clone(), &[], &probe, &["vmread"]).0,
        0,
        "self process_vm_readv remains available"
    );
    assert_eq!(
        f.run(net_deny, &[], &probe, &["connect", &port]).0,
        42,
        "the sandbox cannot reach a listener in the host network namespace"
    );

    let accept = std::thread::spawn(move || listener.accept().map(|_| ()).unwrap());
    assert_eq!(
        f.run(serde_json::json!(false), &[], &probe, &["connect", &port])
            .0,
        0,
        "negative control reaches the host listener without the net namespace"
    );
    accept.join().unwrap();
}

#[test]
fn nested_user_namespaces_remain_available_but_current_namespace_has_no_caps() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let Some(probe) = probe_bin(&f.proj) else {
        return;
    };
    let probe = s(&probe);
    let sandbox = serde_json::json!({ "net": false, "fs": true });

    assert_eq!(f.run(sandbox.clone(), &[], &probe, &["unshare"]).0, 0);
    assert_eq!(f.run(sandbox.clone(), &[], &probe, &["clone3"]).0, 0);
    assert_eq!(f.run(sandbox.clone(), &[], &probe, &["clonens"]).0, 0);
    assert_eq!(
        f.run(sandbox, &[], &probe, &["mount"]).0,
        42,
        "the target has no CAP_SYS_ADMIN in its current mount namespace"
    );
}

#[test]
fn direct_nested_mount_namespace_preserves_mask_when_available() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let Some(probe) = probe_bin(&f.proj) else {
        return;
    };
    let probe = s(&probe);
    let secret = s(&f.proj.join(".env"));
    let (code, _) = f.run(
        serde_json::json!({ "fs": ["...", "./"], "net": true, "vars": true }),
        &[],
        &probe,
        &["unmountmask", &secret],
    );
    if code == 47 {
        eprintln!(
            "direct nested mount namespaces are unavailable under this stock Ubuntu/AppArmor profile"
        );
        return;
    }
    assert_eq!(code, 0, "an inherited mask stays pinned and empty");
}

#[test]
fn sandboxed_execution_accepts_uid_zero_only_after_zero_capability_probe() {
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let f = fixture();
    let policy =
        compile(&serde_json::json!({ "fs": ["...", "./"] }), &f.ctx(&[])).expect("compiles");
    let output = apply(
        &policy,
        CommandSpec::new("/bin/sh")
            .args([
                "-c",
                "test \"$(awk '/^CapInh:|^CapPrm:|^CapEff:|^CapAmb:/ { if ($2 != \"0000000000000000\") bad=1 } END { print bad+0 }' /proc/self/status)\" = 0",
            ])
            .cwd(&f.proj)
            .deny_search_root(&f.proj),
    )
    .expect("the verified zero-capability root view is allowed")
    .output()
    .expect("spawn root sandbox");
    assert!(output.status.success(), "{output:?}");
}

// ── PID namespace self operations ──────────────────────────────────────────────

#[test]
fn self_pidfd_operations_remain_available_inside_the_pid_namespace() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let Some(probe) = probe_bin(&f.proj) else {
        return;
    };
    let probe = s(&probe);
    assert_eq!(
        f.run(
            serde_json::json!({ "net": false, "fs": true }),
            &[],
            &probe,
            &["pidfd"]
        )
        .0,
        0,
        "self pidfd_open/getfd remain available; host processes are outside this PID namespace"
    );
}

// ── private /dev view (PTY still works) ─────────────────────────────────────────

#[test]
fn private_dev_permits_pty_entropy_and_isolated_shm() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let confine = serde_json::json!({ "fs": ["./"] });

    // Allowlisted nodes are usable: write /dev/null + read /dev/urandom.
    assert!(
        f.ok(
            confine.clone(),
            SH,
            &[
                "-c",
                "printf x > /dev/null && head -c 4 /dev/urandom > /dev/null"
            ]
        ),
        "/dev/null + /dev/urandom usable under read-confine"
    );
    // `/dev/shm` is a fresh private tmpfs, not the host's shared-memory directory.
    assert!(
        f.ok(
            confine.clone(),
            SH,
            &["-c", "printf x > /dev/shm/nub_dev_probe"]
        ),
        "private /dev/shm remains usable under read-confine"
    );
    // A PTY-spawning program works: /dev/ptmx + the /dev/pts slave are present.
    if let Some(probe) = probe_bin(&f.proj) {
        assert_eq!(
            f.run(confine.clone(), &[], &s(&probe), &["pty"]).0,
            0,
            "PTY (ptmx + pts slave) opens under the private /dev view"
        );
    }
    // A dynamically-linked interpreter still spawns under the narrowed /dev.
    if Path::new("/usr/bin/python3").exists() {
        let (_c, out) = f.run(
            confine.clone(),
            &[],
            "/usr/bin/python3",
            &["-c", "print('ok')"],
        );
        assert!(
            out.contains("ok"),
            "python3 (dynamic interpreter) runs under the narrowed /dev"
        );
    }
    // The same private shm behavior remains usable with fs relaxed.
    assert!(
        f.ok(
            serde_json::json!({ "fs": true }),
            SH,
            &["-c", "printf x > /dev/shm/nub_dev_probe2"]
        ),
        "neg-control: relaxed fs writes /dev/shm"
    );
}

// ── node runs cleanly under the full hardening (the ship-blocker check) ──────────

/// A real Node binary if one is installed (CI runners + the dev VM have one). The
/// hardening MUST NOT break Node — it is the runtime nub augments.
fn node_bin() -> Option<String> {
    for p in ["/usr/local/bin/node", "/usr/bin/node"] {
        if Path::new(p).exists() {
            return Some(p.to_string());
        }
    }
    None
}

#[test]
fn node_threads_and_crypto_under_full_hardening() {
    if skip_without_bwrap() {
        return;
    }
    let Some(node) = node_bin() else {
        return; // no Node on this host — skip (VM/CI runners have one)
    };
    let f = fixture();
    // The Worker exercises normal thread creation; crypto.randomBytes exercises the
    // private device view from that worker.
    let script = "const {Worker}=require('worker_threads');\
        const w=new Worker(\"const{parentPort}=require('worker_threads');\
        parentPort.postMessage(require('crypto').randomBytes(4).toString('hex'))\",{eval:true});\
        w.on('message',m=>{console.log('NODEOK'+m);w.terminate();});\
        w.on('error',e=>{console.log('NODEERR'+e);});";
    let (code, out) = f.run(
        serde_json::json!({ "fs": ["./"] }),
        &[],
        &node,
        &["-e", script],
    );
    assert!(
        out.contains("NODEOK"),
        "node must start, spawn a Worker thread, and use crypto under Bubblewrap \
         (exit {code}, out {out:?})"
    );
}

// ── graceful degradation is honest ──────────────────────────────────────────────

#[test]
fn enforcement_is_full_when_bubblewrap_is_available() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    // A read-confine with no per-host net allow enforces fully — the Degradation
    // must be empty (no silent loss).
    let policy = compile(&serde_json::json!({ "fs": ["./"] }), &f.ctx(&[])).unwrap();
    let prepared = apply(
        &policy,
        CommandSpec::new(CAT).cwd(&f.proj).deny_search_root(&f.proj),
    )
    .unwrap();
    assert!(
        prepared.degradation.is_full(),
        "read-confine fully enforces when Bubblewrap is available"
    );
}

#[test]
fn hardlink_alias_of_denied_secret_fails_setup_without_rejecting_unrelated_aliases() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let secret = f.proj.join("secret.txt");
    std::fs::write(&secret, "REALSECRET").unwrap();
    let alias = f.proj.join("alias.txt");
    std::fs::hard_link(&secret, &alias).unwrap();
    let surface = serde_json::json!({ "fs": [s(&f.proj), format!("!{}", s(&secret))] });
    let policy = compile(&surface, &f.ctx(&[])).unwrap();
    let error = apply(
        &policy,
        CommandSpec::new(CAT)
            .args([s(&alias)])
            .cwd(&f.proj)
            .deny_search_root(&f.proj),
    )
    .err()
    .expect("denied hardlink aliases must fail setup");
    let reason = error.reason.unwrap_or_default();
    assert!(reason.contains(&s(&secret)), "{reason}");
    assert!(reason.contains("2 hard links"), "{reason}");

    let unrelated = fixture();
    let package = unrelated.proj.join("package-store-file");
    std::fs::write(&package, "PACKAGE").unwrap();
    std::fs::hard_link(&package, unrelated.proj.join("package-store-alias")).unwrap();
    let unrelated_secret = unrelated.proj.join("single-link-secret.txt");
    std::fs::write(&unrelated_secret, "REALSECRET").unwrap();
    let unrelated_surface = serde_json::json!({
        "fs": [s(&unrelated.proj), format!("!{}", s(&unrelated_secret))]
    });
    assert!(
        unrelated.ok(unrelated_surface, CAT, &[&s(&package)]),
        "unrelated package-store-style hardlinks remain usable"
    );

    let f2 = fixture();
    let secret2 = f2.proj.join("secret.txt");
    std::fs::write(&secret2, "REALSECRET").unwrap();
    let surf2 = serde_json::json!({ "fs": [s(&f2.proj), format!("!{}", s(&secret2))] });
    assert!(
        !f2.ok(surf2, CAT, &[&s(&secret2)]),
        "no hardlink: the path-deny denies the secret"
    );
}

#[test]
fn missing_write_grant_is_rejected_without_creating_the_path() {
    if skip_without_bwrap() {
        return;
    }
    let f = fixture();
    let target = f.proj.join("writable/newfile.txt");
    let surface = serde_json::json!({ "fs": ["...", s(&target)] });
    let policy = compile(&surface, &f.ctx(&[])).unwrap();
    let error = apply(
        &policy,
        CommandSpec::new(TRUE)
            .cwd(&f.proj)
            .deny_search_root(&f.proj),
    )
    .err()
    .expect("missing write grant must fail setup");
    assert!(
        error
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("does not exist")),
        "missing grant must fail setup precisely: {error:?}"
    );
    assert!(
        !target.exists(),
        "setup must not create the missing host path"
    );
}

#[test]
fn exact_etc_user_deny_is_masked_without_hiding_other_config() {
    if skip_without_bwrap() {
        return;
    }
    // The system config tree stays readable, but an explicit existing deny is mounted
    // over exactly. End-to-end proof needs a secret planted under
    // the REAL /etc (root only), so this runs only where /etc is writable (the
    // conformance real-kernel/root leg); it SKIPS as a non-root user rather than
    // reporting a hollow pass. The dir-agnostic derivation is covered portably by
    // the derivation unit tests.
    let secret = Path::new("/etc/nub_sandbox_etc_carve_secret");
    if std::fs::write(secret, "ETCSECRET").is_err() {
        return; // not root — the portable unit test covers the derivation
    }
    let f = fixture();
    let surface = serde_json::json!({ "fs": ["...", "!/etc/nub_sandbox_etc_carve_secret"] });
    let leaked = f.run(
        surface.clone(),
        &[],
        CAT,
        &["/etc/nub_sandbox_etc_carve_secret"],
    );
    let hostname_ok = f.ok(surface, CAT, &["/etc/hostname"]);
    let _ = std::fs::remove_file(secret);
    assert!(
        !leaked.1.contains("ETCSECRET"),
        "a user deny under /etc must be masked (secret not readable)"
    );
    assert!(
        hostname_ok,
        "the rest of /etc stays readable for the loader after the carve"
    );
}

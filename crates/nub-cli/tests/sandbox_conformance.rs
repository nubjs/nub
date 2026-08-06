//! Cross-platform sandbox CONFORMANCE matrix — the done-gate driver.
//!
//! Drives the REAL `nub run --sandbox <policy.json> -- <probe> <action> <target>`
//! CLI seam (design.md §2.6) against a committed, declarative fixture matrix
//! (`tests/sandbox_conformance/*.json`). Each fixture pairs a surface `sandbox`
//! policy with per-axis probe cases + an expected allow/deny verdict; every DENY
//! case is RE-RUN under the fixture's `relaxed` policy as a NEGATIVE CONTROL, so a
//! hollow / no-op enforcement (which would let the denied action through) cannot
//! pass — the relaxed run must succeed or the deny is proven meaningless.
//!
//! Scope split (deliberate): fs (read/write incl. the secret deny-set + `.env`
//! subtree) and env (scrub incl. the npm_config auth-not-leaked case) are proven
//! black-box on ALL THREE OSes here. The deeper per-host proxy filtering and the
//! Linux seccomp syscall denials (socket/ptrace/vmread) live in the per-backend
//! enforcement tests (`{macos,linux,windows}_enforcement`, `macos_proxy`)
//! that run alongside this in the conformance CI workflow — the probe is black-box
//! and cannot introspect a proxy's SNI decision or a raw syscall the way those do.
//! net here is the coarse egress-deny axis, which IS uniform across the backends.
//!
//! Hermetic: every case gets its own `TempDir` tree (project + a fake home holding
//! the secrets + an outside dir), so the suite is order- and thread-independent and
//! the secret denies target fixture paths, never the developer's real `~`.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

// ── fixture format ──────────────────────────────────────────────────────────────

/// One conformance fixture: a policy, its negative-control relaxation, an optional
/// ambient-env injection (env fixtures), and the probe cases.
#[derive(Debug, Deserialize)]
struct Fixture {
    name: String,
    /// The surface `sandbox` block under test (compiled by `nub run --sandbox`).
    policy: serde_json::Value,
    /// The relaxation that must ADMIT every deny case — the negative control.
    relaxed: serde_json::Value,
    /// Extra env layered onto nub's ambient before spawn (env fixtures inject the
    /// secrets + build hints the scrub is asserted against). `$HOME` expands to the
    /// fixture home; PATH is always seeded so nub and the probe run.
    #[serde(default)]
    ambient_env: BTreeMap<String, String>,
    cases: Vec<Case>,
}

/// One probe case: attempt `probe` against `target`, expect `expect` (allowed) under
/// the fixture policy. `platforms` restricts the case to the named OSes (default all).
#[derive(Debug, Deserialize)]
struct Case {
    /// `read` | `write` | `connect` | `env`.
    probe: String,
    /// Path spec (`proj:rel`, `home:rel`, `out:rel`), `host:port`, or an env key.
    target: String,
    /// Whether the action is expected to SUCCEED (be allowed) under `policy`.
    expect: bool,
    /// OSes this case applies to (`macos`/`linux`/`windows`); empty = all.
    #[serde(default)]
    platforms: Vec<String>,
}

// ── binary locations ──────────────────────────────────────────────────────────────

/// A sibling binary in the same target dir as this test's deps/ (cargo builds all of
/// nub-cli's bins — `nub` and `nub-sandbox-probe` — before its integration tests).
fn sibling_bin(name: &str) -> PathBuf {
    let mut p = std::env::current_exe().expect("current_exe");
    p.pop(); // deps/
    p.pop(); // debug/ (or fast/)
    p.push(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(p.exists(), "expected built binary at {}", p.display());
    p
}

fn nub_bin() -> PathBuf {
    sibling_bin("nub")
}
fn probe_bin() -> PathBuf {
    sibling_bin("nub-sandbox-probe")
}

// ── Bubblewrap behavior gate (Linux only) ─────────────────────────────────────────

/// Exercise the Bubblewrap operations the backend needs, rather than trusting a version
/// string. The candidates come from the engine's own resolver, not `bwrap` on PATH: on
/// an AppArmor-restricted host PATH resolves to the distro binary the kernel denies,
/// while the dedicated helper `nub setup-sandbox` installs is the one that works — so a
/// PATH probe declared a correctly set-up host unenforceable and skipped the suite.
///
/// The required CI leg turns an unavailable primitive into a hard failure; local hosts
/// may skip kernel-enforcement assertions.
#[cfg(target_os = "linux")]
fn linux_enforceable() -> bool {
    !nub_sandbox::host_probe::skip_without_bwrap_with(
        "sandbox_conformance",
        &[
            "--new-session",
            "--cap-drop",
            "ALL",
            "--unshare-pid",
            "--unshare-ipc",
        ],
        &["--dev", "/dev", "--proc", "/proc"],
    )
}

/// Give a Windows fixture root a PROTECTED DACL — strip inherited ACEs (incl. any inherited
/// `ALL APPLICATION PACKAGES`) and grant ONLY the current user full control — so a LowBox
/// child reaches only the backend's explicit AC-SID grants (a user-SID grant does not satisfy
/// the AppContainer access check). No-op off Windows. `icacls` is the reliable path here; the
/// alternative is a windows-sys `SetNamedSecurityInfoW` dance for a one-shot test setup.
fn secure_windows_root(root: &Path) {
    if !cfg!(windows) {
        return;
    }
    let user = std::env::var("USERNAME").expect("USERNAME set on Windows");
    let out = Command::new("icacls")
        .arg(root)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{user}:(OI)(CI)F"))
        .output()
        .expect("run icacls");
    assert!(
        out.status.success(),
        "icacls failed to secure the fixture root:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── the hermetic fixture tree ─────────────────────────────────────────────────────

/// A throwaway project + fake home + outside dir. `resolve` turns a `proj:`/`home:`/
/// `out:` target spec into a concrete path and materializes it (a read target as a
/// real file, a write target's parent dir) so a denial is a genuine deny, never a
/// missing-file artifact.
struct Tree {
    _tmp: TempDir,
    root: PathBuf,
    proj: PathBuf,
    home: PathBuf,
    outside: PathBuf,
    /// The probe to launch under the sandbox for THIS tree (see `new`).
    probe: PathBuf,
}

impl Tree {
    fn new() -> Self {
        // Fixture-root placement is per-OS:
        //   macOS   — /private/tmp, NOT $TMPDIR (= the DARWIN confstr scratch the backend
        //             always write-grants, which would make every write spuriously pass);
        //   Windows — an ORDINARY %TEMP% tree under the user profile (the default
        //             tempdir), deliberately NOT a special C:\ store: a LowBox token
        //             bypasses traverse checking on a standard NTFS volume (retained
        //             SeChangeNotifyPrivilege + the volume's FILE_DEVICE_ALLOW_APPCONTAINER_
        //             TRAVERSAL flag), so the backend's leaf-only AC-SID grant is reachable
        //             with NO ancestor grants and NO WRITE_DAC on the profile. This is the
        //             real production model (CI-proven, run 29033024137) — see
        //             tests/windows_enforcement.rs;
        //   else    — the default tempdir.
        let builder = tempfile::Builder::new().prefix("nub-conf-").to_owned();
        let tmp = if cfg!(target_os = "macos") {
            builder.tempdir_in("/private/tmp")
        } else {
            builder.tempdir()
        }
        .expect("tempdir");
        // Canonicalize off Windows (firmlink on macOS, /tmp symlinks on Linux); on Windows
        // canonicalize yields a `\\?\` path the matcher/backend don't expect — use the raw
        // %TEMP% path (policy/access/grant paths all derive from it, so they stay consistent
        // and the ACL lands on the same object regardless of 8.3-vs-long form).
        let root = if cfg!(windows) {
            tmp.path().to_path_buf()
        } else {
            std::fs::canonicalize(tmp.path()).expect("canonicalize")
        };
        // Windows: give the fixture root a PROTECTED DACL — strip inherited ACEs and grant
        // ONLY the current user — so a not-granted file cannot be reached by an inherited
        // `ALL APPLICATION PACKAGES` (AAP) ACE (AAP satisfies the LowBox check for the whole
        // subtree, which would collapse read-confine). A user-SID grant does NOT satisfy the
        // LowBox check, so the child still reaches only the backend's explicit AC-SID grants.
        // %TEMP% under a user profile carries no AAP today, so this is defensive; it puts the
        // root in the state the backend would otherwise build for itself before launch, so the
        // fixture's ACL stays under the fixture's control.
        secure_windows_root(&root);
        let proj = root.join("proj");
        let home = root.join("home");
        let outside = root.join("outside");
        for d in [&proj, &home, &outside] {
            std::fs::create_dir_all(d).unwrap();
        }
        // Security inventory never creates policy sources and never recursively scans
        // the project. Materialize the write root and declare the nested package so
        // each case supplies the same bounded current-path roots the real frontend uses.
        std::fs::create_dir_all(proj.join("writable")).unwrap();
        std::fs::create_dir_all(proj.join("nested")).unwrap();
        // A directory NAMED by a policy must exist for EVERY case, not just the ones
        // probing under it: a backend binds an authored fs entry and rightly refuses to
        // invent a missing source, while `resolve` materializes only the current case's
        // own target into a fresh tree. Hence the ordered-reopen fixture's subtree here.
        std::fs::create_dir_all(proj.join("private/reopened")).unwrap();
        let manifest = if cfg!(target_os = "linux") {
            br#"{"workspaces":["nested"]}"#.as_slice()
        } else {
            // Linux deliberately rejects recursive workspace inventory. Other
            // backends do not consume that inventory, so this must not preflight-
            // fail otherwise-valid native enforcement.
            br#"{"workspaces":["nested/**"]}"#.as_slice()
        };
        std::fs::write(proj.join("package.json"), manifest).unwrap();
        std::fs::write(proj.join("nested/package.json"), b"{}").unwrap();
        // On Windows the sandboxed child must ALSO be C:\-traversable — the sibling probe
        // under the CI checkout is unreachable from a LowBox token — so copy it into the
        // C:\-rooted tree, in a dedicated `bin/` subdir. The Windows backend grants the
        // program file explicitly; separating it from fixture data also prevents an
        // executable grant from obscuring the read-confine oracle.
        // Elsewhere the sibling probe is reachable (the program auto-grant + its system
        // libs), so use it in place.
        let probe = if cfg!(windows) {
            let bin = root.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let dest = bin.join(format!("probe{}", std::env::consts::EXE_SUFFIX));
            std::fs::copy(probe_bin(), &dest).expect("copy probe into the C:\\ tree");
            dest
        } else {
            probe_bin()
        };
        Tree {
            _tmp: tmp,
            root,
            proj,
            home,
            outside,
            probe,
        }
    }

    /// Serialize `policy` and substitute the fixture-path placeholders
    /// (`{ROOT}`/`{PROJ}`/`{HOME}`/`{OUT}`) with real forward-slash paths. Forward
    /// slashes are cross-OS safe: the matcher normalizes `\`→`/` before matching, and a
    /// JSON string can't carry a raw `\`. `{ROOT}` lets the generous-read fixture scope
    /// its broad allow to the bounded fixture tree instead of a whole-fs `sandbox: true` — the
    /// Linux generous-`**` grant walks every top-level under a MAX_GRANTS budget, so on a
    /// busy host (a large `/home` checkout in CI) it can overflow before reaching a
    /// deeply-nested file and fail-closed-deny it; a bounded root keeps the walk
    /// deterministic while still proving the secret deny-set carves holes in a broad read.
    fn render_policy(&self, policy: &serde_json::Value) -> String {
        let mut s = serde_json::to_string(policy).unwrap();
        for (token, path) in [
            ("{ROOT}", &self.root),
            ("{PROJ}", &self.proj),
            ("{HOME}", &self.home),
            ("{OUT}", &self.outside),
        ] {
            s = s.replace(token, &path.to_string_lossy().replace('\\', "/"));
        }
        s
    }

    /// Resolve a `proj:`/`home:`/`out:` spec (fs probes) to a concrete path and
    /// materialize it. Non-fs specs are returned unchanged (host:port, env key).
    fn resolve(&self, probe: &str, target: &str) -> String {
        if probe != "read" && probe != "read-content" && probe != "write" {
            return target.to_string();
        }
        let (anchor, rel) = target.split_once(':').unwrap_or(("proj", target));
        let base = match anchor {
            "proj" => &self.proj,
            "home" => &self.home,
            "out" => &self.outside,
            other => panic!("unknown path anchor {other:?} in {target:?}"),
        };
        let path = base.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        // A read target must EXIST so a deny is a real deny; a write target must not
        // (write creates it) but its parent (made above) must. The non-empty body is what
        // makes `read-content` an oracle: zero bytes in-sandbox can only come from the deny.
        if probe == "read" || probe == "read-content" {
            std::fs::write(&path, b"CONFORMANCE-SECRET").unwrap();
        }
        path.to_string_lossy().into_owned()
    }
}

// ── the driver ────────────────────────────────────────────────────────────────────

fn load_fixture(name: &str) -> Fixture {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/sandbox_conformance")
        .join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading fixture {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parsing fixture {name}: {e}"))
}

fn applies_here(case: &Case) -> bool {
    case.platforms.is_empty() || case.platforms.iter().any(|p| p == std::env::consts::OS)
}

/// One probe run: whether the action was ALLOWED, plus the child's captured streams. A
/// bare verdict makes an unexpected deny unattributable in CI (the probe's own reason,
/// nub's warning, and a degradation notice all read identically as "false"), so the
/// streams ride along for the assertion message.
struct Verdict {
    allowed: bool,
    output: String,
}

/// Panic if `nub` did not run to completion. A CRASH is a HARNESS error, never a policy
/// verdict, and the distinction is not cosmetic: every verdict in this file is
/// `status.code() == Some(0)`, so a `nub` that dies reads as `allowed = false` — i.e. as a
/// perfect deny on every axis. That is exactly what happened on Windows, where a debug
/// `nub.exe` overflowed its 1 MiB main-thread stack inside clap and aborted 0xC00000FD
/// before parsing an argument: the `expect: true` cases failed loudly while every
/// `expect: false` case was one crashed binary away from certifying an enforcement that
/// never ran. Fail on the exit STATUS instead, so a crash can never wear a deny's clothes.
///
/// Windows surfaces an unhandled exception as the NTSTATUS itself, whose severity bits make
/// it negative as `i32` (`STATUS_STACK_OVERFLOW` = 0xC00000FD = -1073741571); unix reports a
/// signal death as no code at all. Neither can be a verdict: nub exits through
/// `exit_code_from_status`, which maps a signalled child to `128 + signo`, so every verdict
/// this matrix can legitimately produce is a small non-negative number.
///
/// It also catches the second shape of the same lie — a confined child that never reaches
/// `main` (the LowBox DLL-init crash the Windows leg's `crt-static` build exists to avoid).
/// nub propagates that NTSTATUS as its own exit code, which would otherwise read as a
/// flawless deny from a probe that never ran.
#[track_caller]
fn assert_ran_to_completion(out: &std::process::Output, what: &str) {
    let died = match out.status.code() {
        None => Some(format!("terminated by signal ({})", out.status)),
        Some(code) if code < 0 => Some(format!("abnormal exit {code} (0x{:08X})", code as u32)),
        Some(_) => None,
    };
    if let Some(why) = died {
        panic!(
            "HARNESS ERROR — nub did not run to completion ({why}); this is a crash, not a \
             policy verdict.\n  case: {what}\n  stdout: {}\n  stderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Run one probe under `policy` in `tree`. A nub-side compile/apply failure is a HARNESS
/// error (never a silent "deny"), surfaced loudly with nub's stderr.
fn run_case(
    tree: &Tree,
    policy: &serde_json::Value,
    ambient: &BTreeMap<String, String>,
    probe: &str,
    target: &str,
) -> Verdict {
    let policy_path = tree.proj.join("__policy.json");
    std::fs::write(&policy_path, tree.render_policy(policy)).unwrap();

    let mut cmd = Command::new(nub_bin());
    cmd.arg("run").arg("--sandbox").arg(&policy_path).arg("--");
    cmd.arg(&tree.probe).arg(probe);
    // `connect` takes host + port as two probe args; everything else is one.
    if probe == "connect" {
        let (host, port) = target.split_once(':').expect("connect target host:port");
        cmd.arg(host).arg(port);
    } else {
        cmd.arg(target);
    }
    cmd.current_dir(&tree.proj);
    // Homes come from the ambient env inside nub (`sandbox_homes`), so point them at
    // the fixture home → the `...` secret denies target fixture paths, not real `~`.
    cmd.env("HOME", &tree.home)
        .env("USERPROFILE", &tree.home)
        .env("XDG_CACHE_HOME", tree.home.join(".cache"));
    for (k, v) in ambient {
        // The registry-scoped `npm_config_//host/:_auth…` key holds a `/`, which the
        // Windows environment block does not represent; its assertion case is already
        // platform-gated to unix, so skip injecting it on Windows rather than risk
        // disturbing the child env block.
        if cfg!(windows) && k.contains('/') {
            continue;
        }
        let v = v.replace("$HOME", &tree.home.to_string_lossy());
        cmd.env(k, v);
    }

    let out = cmd.output().expect("spawn nub");
    assert_ran_to_completion(&out, &format!("{probe} {target}"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !(stderr.contains("did not compile") || stderr.contains("could not be applied")),
        "HARNESS ERROR — nub failed to set up the sandbox, not a probe verdict:\n{stderr}"
    );
    Verdict {
        allowed: out.status.code() == Some(0),
        output: format!(
            "  exit: {:?}\n  stdout: {}\n  stderr: {stderr}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout)
        ),
    }
}

/// Drive one fixture: assert every case's verdict, and re-run each DENY case under
/// the relaxed policy as its negative control.
fn drive(name: &str) {
    #[cfg(target_os = "linux")]
    let enforceable = linux_enforceable();
    #[cfg(not(target_os = "linux"))]
    let enforceable = true;

    let fx = load_fixture(name);
    assert_eq!(fx.name, name, "fixture name mismatch in {name}.json");

    // Every confined launch on Linux goes through Bubblewrap — including an env-only
    // policy, because the backend enforces `env` by namespacing the process view (so a
    // sibling cannot read the pre-scrub values back out of /proc), not by handing the
    // child a filtered environment. With the primitive missing the ENV axis is no more
    // reachable than fs/net, so carving `env` out of this gate made the harness demand a
    // launch it could not get and report a HARNESS ERROR. `linux_enforceable` still hard-
    // fails under NUB_SANDBOX_REQUIRE_BWRAP, which is what keeps a prepared conformance
    // runner from reporting a hollow green.
    if !enforceable {
        return;
    }

    for case in &fx.cases {
        if !applies_here(case) {
            continue;
        }

        let tree = Tree::new();
        let target = tree.resolve(&case.probe, &case.target);

        let verdict = run_case(&tree, &fx.policy, &fx.ambient_env, &case.probe, &target);
        assert_eq!(
            verdict.allowed, case.expect,
            "[{name}] {} {}: expected allowed={}, got allowed={}\n{}",
            case.probe, case.target, case.expect, verdict.allowed, verdict.output
        );

        if !case.expect {
            // Negative control: the SAME action under the relaxed policy MUST be
            // allowed — otherwise the deny above proves nothing (missing file,
            // unreachable host, a probe that can't even start).
            let nc = run_case(&tree, &fx.relaxed, &fx.ambient_env, &case.probe, &target);
            assert!(
                nc.allowed,
                "[{name}] neg-control {} {}: must be ALLOWED under the relaxed policy \
                 (the deny is hollow otherwise)\n{}",
                case.probe, case.target, nc.output
            );
        }
    }
}

// ── the matrix — one test per fixture (parallel, granular failure attribution) ────

#[test]
#[cfg(not(target_os = "windows"))]
fn fs_read_confine() {
    drive("fs-read-confine");
}

/// Windows cannot carve the compiler's `.env*` read deny out of a granted project
/// subtree. The backend rejection must surface through the CLI before the child creates
/// a marker.
#[test]
#[cfg(target_os = "windows")]
fn windows_read_deny_inside_grant_fails_closed() {
    let tree = Tree::new();
    let policy_path = tree.proj.join("__policy.json");
    std::fs::write(
        &policy_path,
        tree.render_policy(&serde_json::json!({ "fs": ["./"], "vars": true, "net": true })),
    )
    .unwrap();
    let marker = tree.proj.join("must-not-run.txt");

    let out = Command::new(nub_bin())
        .arg("run")
        .arg("--sandbox")
        .arg(&policy_path)
        .arg("--")
        .arg(&tree.probe)
        .arg("write")
        .arg(&marker)
        .current_dir(&tree.proj)
        .env("HOME", &tree.home)
        .env("USERPROFILE", &tree.home)
        .output()
        .expect("spawn nub");
    // A crash also exits non-zero and also writes no marker, so the fail-closed assertions
    // below cannot tell a refusal from a dead binary. Rule the crash out first.
    assert_ran_to_completion(&out, "windows read-deny inside a grant");
    assert!(!out.status.success());
    assert!(
        !marker.exists(),
        "the rejected sandbox must not run its child"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("fail-closed"), "{stderr}");
    assert!(stderr.contains("read deny"), "{stderr}");
}

/// Windows AppContainer grants are inheritable. The compiler's secret-read deny set
/// therefore cannot be carved out of either of these ordinary fixture grants; reject
/// before running the child instead of reporting an unenforceable sandbox as applied.
#[cfg(target_os = "windows")]
fn fixture_read_deny_fails_closed(name: &str, writable_marker: &str) {
    let fx = load_fixture(name);
    let tree = Tree::new();
    let policy_path = tree.proj.join("__policy.json");
    std::fs::write(&policy_path, tree.render_policy(&fx.policy)).unwrap();
    let marker = tree.proj.join(writable_marker);

    let out = Command::new(nub_bin())
        .arg("run")
        .arg("--sandbox")
        .arg(&policy_path)
        .arg("--")
        .arg(&tree.probe)
        .arg("write")
        .arg(&marker)
        .current_dir(&tree.proj)
        .env("HOME", &tree.home)
        .env("USERPROFILE", &tree.home)
        .output()
        .expect("spawn nub");
    assert_ran_to_completion(&out, name);
    assert!(!out.status.success(), "{name} must fail closed");
    assert!(
        !marker.exists(),
        "{name} must reject the sandbox before running its child"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("fail-closed"), "{stderr}");
    assert!(stderr.contains("read deny"), "{stderr}");
}

#[test]
fn fs_generous_read_secret_denyset() {
    drive("fs-generous-read-secrets");
}

/// §6.12 — the CLI threads the WHOLE parsed document (not the extracted `sandbox`
/// block) as `CompileCtx::document`, so a `...:#/shared/fs` reuse pointing at a SIBLING
/// of `sandbox` resolves. The project subtree is granted ONLY through the reused list,
/// so a successful read proves the wiring end-to-end; if `run_sandboxed` passed the
/// block instead of the whole value, `#/shared/fs` would dangle and nub would fail to
/// compile (run_case asserts loudly on a compile failure).
#[test]
#[cfg(not(target_os = "windows"))]
fn reuse_pointer_resolves_against_the_whole_document() {
    #[cfg(target_os = "linux")]
    if !linux_enforceable() {
        return;
    }
    let tree = Tree::new();
    let doc = serde_json::json!({
        "shared": { "fs": ["{PROJ}"] },
        "sandbox": { "fs": ["...:#/shared/fs"], "net": false, "vars": true }
    });
    let ambient = BTreeMap::new();
    // `resolve` materializes the read target and returns its absolute path (as `drive`
    // does); the project subtree is granted ONLY through the reused `#/shared/fs` list.
    let target = tree.resolve("read", "proj:reused-ok.txt");
    let verdict = run_case(&tree, &doc, &ambient, "read", &target);
    assert!(
        verdict.allowed,
        "a `...:#/shared/fs` reuse must resolve against the whole document\n{}",
        verdict.output
    );
}

/// The general spread end-to-end: a `...:#/pointer` OBJECT KEY resolves against the whole
/// document and splices the reused object's entries, so the project subtree granted ONLY
/// through the spliced object is reachable. Proves the object-form path compiles + enforces
/// through the real CLI (the object twin of `reuse_pointer_resolves_against_the_whole_document`).
#[test]
#[cfg(not(target_os = "windows"))]
fn reuse_object_spread_resolves_against_the_whole_document() {
    #[cfg(target_os = "linux")]
    if !linux_enforceable() {
        return;
    }
    let tree = Tree::new();
    let doc = serde_json::json!({
        "shared": { "fs": { "{PROJ}": "r" } },
        "sandbox": { "fs": { "...:#/shared/fs": true }, "net": false, "vars": true }
    });
    let ambient = BTreeMap::new();
    let target = tree.resolve("read", "proj:spread-ok.txt");
    let verdict = run_case(&tree, &doc, &ambient, "read", &target);
    assert!(
        verdict.allowed,
        "a `...:#/shared/fs` object-key spread must resolve against the whole document\n{}",
        verdict.output
    );
}

#[test]
#[cfg(not(target_os = "windows"))]
fn fs_write_confine() {
    drive("fs-write-confine");
}

#[test]
#[cfg(target_os = "windows")]
fn fs_write_confine() {
    fixture_read_deny_fails_closed("fs-write-confine", "writable/must-not-run.txt");
}

/// Non-Windows regression guard for the `\\?\` verbatim-prefix strip
/// (`matcher/path.rs`). A `./`-relative policy compiled through the real compiler +
/// `std::fs::canonicalize` must still grant the child its own project dir. See the
/// fixture note.
#[test]
#[cfg(target_os = "windows")]
fn fs_windows_relative_canon_grant() {
    fixture_read_deny_fails_closed("fs-windows-relative-canon", "must-not-run.txt");
}

#[test]
fn env_scrub_including_npm_config_auth() {
    drive("env-scrub-auth");
}

#[test]
fn net_coarse_egress_deny() {
    drive("net-coarse-deny");
}

/// `$trusted` (built-in net host set, Phase 3) expands into a fine-grained allow that
/// starts the egress proxy; an unlisted IP target is denied end-to-end.
#[test]
fn net_trusted_set() {
    drive("net-trusted-set");
}

/// `$tooldirs` (built-in fs cache/store set, Phase 3) grants read on the tool caches so
/// a build resolves deps from the store. Windows-gated: the AppContainer allowlist cannot
/// carve the compiler's `.env*` read-deny out of the grant (see the other fs fixtures).
#[test]
#[cfg(not(target_os = "windows"))]
fn fs_tooldirs_read() {
    drive("fs-tooldirs-read");
}

/// The empty object grants nothing, so on Linux it cannot LAUNCH — and that refusal is the
/// property worth asserting, not a per-case verdict.
///
/// ⛔ THIS USED TO CALL `drive`, AND IT COULD NEVER HAVE PASSED. `backend/linux.rs`'s
/// `validate_cwd_visibility` refuses a `RootView::Minimal` launch whose cwd no rule covers, and
/// `{}` covers nothing — so nub exits non-zero before the child runs and every case verdict is
/// unreachable. The guard is deliberate and unit-pinned in that same file
/// (`cwd_visibility_rejects_masks_and_tmp_but_accepts_a_real_submount`); `TmpMode` here is
/// `Shared`, so its `/tmp` branch never even fires.
///
/// ⛔ THE FIX IS NOT TO GRANT SOMETHING. Any path that makes this launch destroys the premise —
/// the fixture exists to pin what an EMPTY policy does. What the design actually delivers is
/// strictly stronger than "the child ran and was denied": the sandbox is refused outright and the
/// child never starts, so there is no window in which it holds a usable handle. That is what is
/// asserted below, in the same shape as `fixture_read_deny_fails_closed` uses on Windows.
///
/// The marker is the load-bearing half. Without it a non-zero exit would equally describe nub
/// crashing for an unrelated reason, and the assertion would be hollow.
/// ⛔ DELIBERATELY NOT GATED ON `linux_enforceable()`, AND THAT GATE WOULD HAVE MADE THIS TEST
/// VACUOUS. It probes whether BUBBLEWRAP can create a user namespace and returns early — reporting
/// a PASS with zero assertions executed — when it cannot. Every `drive()` fixture accepts that
/// because it needs a working confined launch to observe anything. This test needs the opposite:
/// it asserts nub REFUSES to apply the policy, and that refusal is a compile-time validation
/// (`validate_cwd_visibility`) reached before any namespace is required. Gating it on a bwrap probe
/// would silence it on exactly the machines where a regression is most likely to slip through.
///
/// A missing bwrap cannot turn this green either way: the stderr assertions pin the REASON, so a
/// launch that failed for an unrelated reason fails this test rather than passing it.
#[test]
#[cfg(target_os = "linux")]
fn empty_object_default_deny_fails_closed() {
    let fx = load_fixture("empty-object-default-deny");
    let tree = Tree::new();
    let policy_path = tree.proj.join("__policy.json");
    std::fs::write(&policy_path, tree.render_policy(&fx.policy)).unwrap();
    let marker = tree.proj.join("writable/must-not-run.txt");

    let out = Command::new(nub_bin())
        .arg("run")
        .arg("--sandbox")
        .arg(&policy_path)
        .arg("--")
        .arg(&tree.probe)
        .arg("write")
        .arg(&marker)
        .current_dir(&tree.proj)
        .env("HOME", &tree.home)
        .env("USERPROFILE", &tree.home)
        .output()
        .expect("spawn nub");

    assert_ran_to_completion(&out, "empty-object default deny");
    assert!(
        !out.status.success(),
        "an empty policy must be refused, not applied"
    );
    assert!(
        !marker.exists(),
        "the child must never run under a policy that grants nothing"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("fail-closed"), "{stderr}");
    assert!(
        stderr.contains("absent from the final filesystem view"),
        "the refusal must name WHY it is unenforceable, or a future regression that fails for an \
         unrelated reason reads as this assertion passing:\n{stderr}"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn fs_last_match_reopen() {
    drive("fs-last-match-reopen");
}

/// The twin of the fixture above, and the reason it is safe: emitting mounts in policy
/// order must reopen only what the policy still allows at the END of the rule list. An
/// allow the dotenv floor shadows must stay dead, or ordered emission would lay that bind
/// over the mask and hand the secret back.
#[test]
#[cfg(target_os = "linux")]
fn fs_shadowed_allow_not_reopened() {
    drive("fs-shadowed-allow-not-reopened");
}

#[test]
#[cfg(target_os = "linux")]
fn build_jail_preset() {
    drive("build-jail-preset");
}

#[test]
fn sandbox_activation_requires_the_exact_run_flag_position() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    let policy = temp.path().join("policy.json");
    let marker = temp.path().join("marker");
    std::fs::create_dir_all(&project).unwrap();
    // `nub run` hands the script body to the PLATFORM's script shell, so the probe needs a
    // per-OS spelling: a `#!/bin/sh` file never executes under cmd.exe, and the marker
    // assertion below would then fail on Windows for a reason unrelated to flag placement.
    // It stays a script FILE rather than an inline one-liner because the misplaced-flag
    // forms append `--sandbox <policy>` to the script's argv — a shell/batch file ignores
    // the extra args, while an inline `node -e …` rejects them as unknown options and never
    // reaches the marker write.
    #[cfg(windows)]
    {
        std::fs::write(
            project.join("package.json"),
            r#"{ "scripts": { "probe": ".\\probe.cmd" } }"#,
        )
        .unwrap();
        std::fs::write(
            project.join("probe.cmd"),
            format!("@echo off\r\n> \"{}\" echo invoked\r\n", marker.display()),
        )
        .unwrap();
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(
            project.join("package.json"),
            r#"{ "scripts": { "probe": "./probe.sh" } }"#,
        )
        .unwrap();
        std::fs::write(
            project.join("probe.sh"),
            format!("#!/bin/sh\nprintf invoked > {}\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(
            project.join("probe.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    std::fs::write(&policy, r#"{ "vars": 42 }"#).unwrap();

    let exact = Command::new(nub_bin())
        .args(["run", "--sandbox", policy.to_str().unwrap(), "--", "true"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert_ran_to_completion(&exact, "exact --sandbox flag position");
    assert!(!exact.status.success());
    assert!(
        String::from_utf8_lossy(&exact.stderr).contains("sandbox policy did not compile"),
        "{}",
        String::from_utf8_lossy(&exact.stderr)
    );

    // Every run is recorded, not just the last: the marker assertion below is about the
    // FIRST TWO forms, so a diagnostic carrying only the third would name the one run that
    // is not expected to write it.
    let mut runs = Vec::new();
    for args in [
        vec!["run", "probe", "--sandbox", policy.to_str().unwrap()],
        vec!["run", "--", "probe", "--sandbox", policy.to_str().unwrap()],
        vec!["--sandbox", policy.to_str().unwrap(), "run", "probe"],
    ] {
        let output = Command::new(nub_bin())
            .args(&args)
            .current_dir(&project)
            .output()
            .unwrap();
        assert_ran_to_completion(&output, &format!("misplaced flag form {args:?}"));
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("sandbox policy did not compile"),
            "misplaced {args:?} activated the hidden sandbox: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        runs.push(format!(
            "{args:?} -> exit {:?}\n  stdout: {}\n  stderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    assert!(
        marker.exists(),
        "post-positional and -- forms must run the script:\n{}",
        runs.join("\n")
    );
}

#[test]
#[cfg(target_os = "linux")]
fn ambient_sandbox_named_environment_cannot_select_a_linux_bootstrap_role() {
    for key in [
        "NUB_SANDBOX_MONITOR",
        "NUB_SANDBOX_BOOTSTRAP",
        "__NUB_SANDBOX_MONITOR",
        "__NUB_SANDBOX_BOOTSTRAP",
    ] {
        let output = Command::new(nub_bin())
            .arg("--help")
            .env(key, "__nub-sandbox-monitor")
            .output()
            .unwrap();
        assert!(output.status.success(), "{key}: {:?}", output.status);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("sandbox monitor bootstrap"),
            "{key} selected a bootstrap role: {stderr}"
        );
    }
}

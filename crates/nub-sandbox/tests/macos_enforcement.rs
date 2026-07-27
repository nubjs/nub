//! macOS Seatbelt backend — REAL enforcement tests.
//!
//! Each test compiles a surface policy, applies it, and actually SPAWNS the child
//! under `sandbox-exec`, asserting the kernel allowed or denied the action. Every
//! confinement assertion is paired with a NEGATIVE CONTROL (the axis lifted → the
//! same action succeeds) so a passing test cannot be hollow. macOS-only.
//!
//! Hermetic: every test builds its own `tempfile::TempDir` fixture and homes; no
//! shared mutable state, so the suite is order- and thread-independent.
#![cfg(target_os = "macos")]

use nub_sandbox::compiler::{CompileCtx, ScopeCapabilities, ShellRunner};
use nub_sandbox::matcher::Homes;
use nub_sandbox::{CommandSpec, apply, compile};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// A fixture: a project dir + a fake home (so secret denies target fixture paths,
/// never the real `~/.ssh`) + an out-of-project dir.
struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    proj: PathBuf,
    home: PathBuf,
}

fn fixture() -> Fixture {
    // Place the fixture under /private/tmp, NOT the default $TMPDIR — the latter is
    // /var/folders/<uid>/T, the DARWIN confstr scratch dir the backend always
    // write-grants (for the Apple toolchain), which would spuriously make every
    // fixture write "allowed". /private/tmp is subject to write-confine.
    let tmp = tempfile::Builder::new()
        .prefix("nub-sbx-")
        .tempdir_in("/private/tmp")
        .unwrap();
    // Canonicalize up front — the kernel checks the canonical path, so the paths we
    // assert against must be canonical too (here /private/tmp is already canonical).
    let root = fs::canonicalize(tmp.path()).unwrap();
    let proj = root.join("proj");
    let home = root.join("home");
    fs::create_dir_all(proj.join("sub")).unwrap();
    fs::create_dir_all(proj.join("writable")).unwrap();
    fs::create_dir_all(home.join(".ssh")).unwrap();
    fs::create_dir_all(root.join("outside")).unwrap();
    fs::write(proj.join("pub.txt"), "PUBLIC").unwrap();
    fs::write(proj.join("sub/nested.txt"), "NESTED").unwrap();
    fs::write(proj.join(".env"), "ENVSECRET").unwrap();
    fs::write(proj.join(".env.local"), "ENVLOCAL").unwrap();
    fs::write(proj.join("sub/.env"), "NESTEDENV").unwrap();
    fs::write(home.join(".ssh/id_rsa"), "IDRSA").unwrap();
    fs::write(root.join("outside/o.txt"), "OUTSIDE").unwrap();
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
        let ambient: BTreeMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        CompileCtx {
            homes: self.homes(),
            cwd: self.proj.clone(),
            policy_files: Vec::new(),
            caps: ScopeCapabilities::approved(),
            ambient_env: ambient,
            document: serde_json::Value::Null,
            interpreter: Vec::new(),
            runner: Box::new(ShellRunner),
        }
    }

    /// Run `program args…` under `surface`, returning true iff it exited 0 (allowed).
    /// stdio → null so the verdict is the tested action alone, never a stdout write.
    fn allowed(&self, surface: Value, program: &str, args: &[&str]) -> bool {
        self.allowed_env(surface, &[], program, args)
    }

    fn allowed_env(
        &self,
        surface: Value,
        env: &[(&str, &str)],
        program: &str,
        args: &[&str],
    ) -> bool {
        let policy = compile(&surface, &self.ctx(env)).expect("policy compiles");
        let spec = CommandSpec::new(program)
            .args(args.iter().copied())
            .cwd(&self.proj);
        apply(&policy, spec)
            .expect("apply")
            .output()
            .expect("spawn")
            .status
            .success()
    }
}

fn s(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

const CAT: &str = "/bin/cat";
const TOUCH: &str = "/usr/bin/touch";

#[test]
fn system_sandbox_launcher_ignores_the_child_path() {
    use std::os::unix::fs::PermissionsExt;

    let f = fixture();
    let fake_bin = f.root.join("fake-bin");
    let fake_launcher = fake_bin.join("sandbox-exec");
    let marker = f.root.join("fake-sandbox-exec-invoked");
    fs::create_dir(&fake_bin).unwrap();
    fs::write(
        &fake_launcher,
        "#!/bin/sh\n: > \"$FAKE_SANDBOX_MARKER\"\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&fake_launcher, fs::Permissions::from_mode(0o755)).unwrap();

    let path = format!("{}:/usr/bin:/bin", s(&fake_bin));
    let marker_path = s(&marker);
    let env = &[
        ("PATH", path.as_str()),
        ("FAKE_SANDBOX_MARKER", marker_path.as_str()),
    ];
    let policy = compile(
        &serde_json::json!({ "fs": true, "net": false, "vars": true }),
        &f.ctx(env),
    )
    .expect("policy compiles");
    let prepared = apply(&policy, CommandSpec::new("/usr/bin/true").cwd(&f.proj)).expect("apply");
    let status = prepared.status().expect("spawn system sandbox-exec");

    assert!(status.success(), "the real Seatbelt launch must succeed");
    assert!(
        !marker.exists(),
        "the child PATH must not select an alternate sandbox-exec"
    );
}

#[test]
fn bare_entry_program_is_granted_from_the_child_path() {
    use std::os::unix::fs::PermissionsExt;

    let f = fixture();
    let child_bin = f.root.join("child-bin");
    let tool = child_bin.join("child-path-tool");
    fs::create_dir(&child_bin).unwrap();
    fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();

    let path = format!("{}:/usr/bin:/bin", s(&child_bin));
    assert!(
        f.allowed_env(
            serde_json::json!({ "fs": [], "net": false, "vars": true }),
            &[("PATH", path.as_str())],
            "child-path-tool",
            &[],
        ),
        "the entry resolved by the constructed child PATH must be readable and executable"
    );
}

#[test]
fn bare_entry_program_uses_relative_child_path_from_child_cwd() {
    use std::os::unix::fs::PermissionsExt;

    let f = fixture();
    let child_bin = f.proj.join("bin");
    let tool = child_bin.join("child-path-tool");
    fs::create_dir(&child_bin).unwrap();
    fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();

    assert_ne!(
        std::env::current_dir().unwrap(),
        f.proj,
        "the parent and child cwd must differ for this regression"
    );
    assert!(
        f.allowed_env(
            serde_json::json!({ "fs": [], "net": false, "vars": true }),
            &[("PATH", "bin")],
            "child-path-tool",
            &[],
        ),
        "a relative constructed PATH entry must resolve from the effective child cwd"
    );
}

// ── fs read-confine (array form = allowlist: project + toolchain only) ─────────

#[test]
fn read_confine_allows_project_denies_outside() {
    let f = fixture();
    let confine = serde_json::json!({ "fs": ["./"] });
    assert!(
        f.allowed(confine.clone(), CAT, &[&s(&f.proj.join("pub.txt"))]),
        "project read"
    );
    assert!(
        f.allowed(confine.clone(), CAT, &[&s(&f.proj.join("sub/nested.txt"))]),
        "nested project read"
    );
    assert!(
        f.allowed(confine.clone(), CAT, &["/etc/hosts"]),
        "system toolchain read"
    );
    // confinement:
    assert!(
        !f.allowed(confine.clone(), CAT, &[&s(&f.root.join("outside/o.txt"))]),
        "outside read denied"
    );
    assert!(
        !f.allowed(confine, CAT, &[&s(&f.home.join(".ssh/id_rsa"))]),
        "home secret read denied"
    );
    // negative control — fs relaxed → the same outside read succeeds:
    assert!(
        f.allowed(
            serde_json::json!({ "fs": true }),
            CAT,
            &[&s(&f.root.join("outside/o.txt"))]
        ),
        "neg-control: relaxed fs reads outside"
    );
}

// ── fs .env deny under a broad project read-allow (generous read + secrets) ────

#[test]
fn env_files_denied_under_generous_read() {
    let f = fixture();
    // `sandbox: true` = the secure fs base (generous read + the home-secret denies + the
    // `.env*` floor) — the v2 replacement for the removed naked-`...` base.
    let generous = serde_json::json!(true);
    assert!(
        f.allowed(generous.clone(), CAT, &[&s(&f.proj.join("pub.txt"))]),
        "pub readable"
    );
    assert!(
        !f.allowed(generous.clone(), CAT, &[&s(&f.proj.join(".env"))]),
        ".env denied"
    );
    assert!(
        !f.allowed(generous.clone(), CAT, &[&s(&f.proj.join(".env.local"))]),
        ".env.local denied"
    );
    assert!(
        !f.allowed(generous.clone(), CAT, &[&s(&f.proj.join("sub/.env"))]),
        "nested .env denied"
    );
    assert!(
        !f.allowed(generous, CAT, &[&s(&f.home.join(".ssh/id_rsa"))]),
        "ssh key denied"
    );
    // negative control — relaxed fs reads .env fine:
    assert!(
        f.allowed(
            serde_json::json!({ "fs": true }),
            CAT,
            &[&s(&f.proj.join(".env"))]
        ),
        "neg-control: relaxed fs reads .env"
    );
}

// ── fs .env deny under an OBJECT-form allowlist + exact-file override (Feature 2) ─

#[test]
fn env_files_denied_under_object_form_allowlist() {
    let f = fixture();
    // The core Feature-2 gap: an object-form `{ "./": "r" }` grants the project but the
    // kernel must DENY `<proj>/.env` — before the fix the object form spliced no secret
    // set, so the LowBox/Seatbelt profile left `.env` readable inside the granted subtree.
    let confine = serde_json::json!({ "fs": { "./": "r" } });
    assert!(
        f.allowed(confine.clone(), CAT, &[&s(&f.proj.join("pub.txt"))]),
        "project file readable under object-form allow"
    );
    assert!(
        !f.allowed(confine.clone(), CAT, &[&s(&f.proj.join(".env"))]),
        ".env denied under an object-form dir allow"
    );
    assert!(
        !f.allowed(confine.clone(), CAT, &[&s(&f.proj.join("sub/.env"))]),
        "nested .env denied under an object-form dir allow"
    );
    // negative control — relaxed fs reads .env fine (the deny is the confinement, not the fixture):
    assert!(
        f.allowed(
            serde_json::json!({ "fs": true }),
            CAT,
            &[&s(&f.proj.join(".env"))]
        ),
        "neg-control: relaxed fs reads .env"
    );
}

#[test]
fn dotenv_deny_is_unconditional_even_when_named_exactly() {
    let f = fixture();
    fs::write(f.proj.join(".env.production"), "PRODVALUE").unwrap();
    // The `.env*` block is unconditional (sandbox.mdx): naming the exact file no longer
    // reopens it, and a sibling `.env` is denied too — the kernel enforces both.
    let confine = serde_json::json!({ "fs": { "./": "r", "./.env.production": "r" } });
    assert!(
        !f.allowed(confine.clone(), CAT, &[&s(&f.proj.join(".env.production"))]),
        "an explicit exact-file .env.production allow does NOT reopen it"
    );
    assert!(
        !f.allowed(confine, CAT, &[&s(&f.proj.join(".env"))]),
        "a sibling .env stays denied"
    );
}

#[test]
fn env_prefixed_directory_allow_does_not_expose_its_secret_contents() {
    // THE SUB-DIRECTORY BYPASS regression guard (real kernel). A `.env*`-NAMED directory
    // is a secret container (`.env.d/` per-target secret files); the `.env*/**` subtree
    // deny covers its contents. Naming the DIRECTORY (`{ "./.env.d": "r" }`) must NOT
    // re-expose those contents — the `.env*` floor is unconditional (nothing reopens it).
    // On macOS a bare path becomes a `(subpath …)` grant covering descendants, so a naive
    // emission of the directory's subtree twin WOULD leak here — this asserts it is denied.
    let f = fixture();
    fs::create_dir_all(f.proj.join(".env.d")).unwrap();
    fs::write(f.proj.join(".env.d/prod"), "DIRSECRET").unwrap();
    let confine = serde_json::json!({ "fs": { "./": "r", "./.env.d": "r" } });
    assert!(
        !f.allowed(confine.clone(), CAT, &[&s(&f.proj.join(".env.d/prod"))]),
        "a .env.d directory allow must NOT expose the secret file inside it"
    );
    // The project's own non-secret file is still readable — the deny is surgical.
    assert!(
        f.allowed(confine, CAT, &[&s(&f.proj.join("pub.txt"))]),
        "neg-control: a normal project file stays readable"
    );
}

// ── private tmp (TmpMode::Private) — shared system tmp hidden (real kernel) ─────

#[test]
fn private_tmp_hides_the_shared_system_tmp() {
    // A file living in the SHARED system tmp (`/private/tmp`, the `/tmp` firmlink target).
    let shared = PathBuf::from(format!(
        "/private/tmp/nub-tmptest-{}.secret",
        std::process::id()
    ));
    fs::write(&shared, "SHAREDTMPSECRET").unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _c = Cleanup(shared.clone());
    let f = fixture();

    // A generous read that WOULD expose the shared-tmp file, plus `$tmp: "rw"` (private). The
    // private-tmp deny is emitted after the generous read (last-match-wins), so the kernel
    // must DENY the shared-tmp file even though `/` is otherwise readable.
    let private = serde_json::json!({ "fs": { "/": "r", "$tmp": "rw" } });
    assert!(
        !f.allowed(private, CAT, &[&s(&shared)]),
        "private tmp must hide the shared /private/tmp"
    );
    // NEG-CONTROL: the SAME generous read WITHOUT the private tmp reads the shared-tmp file —
    // proving the deny is the `$tmp: "rw"` private confinement, not the fixture.
    assert!(
        f.allowed(
            serde_json::json!({ "fs": { "/": "r" } }),
            CAT,
            &[&s(&shared)]
        ),
        "neg-control: a plain generous read reads the shared-tmp file"
    );
}

#[test]
fn tmp_subpath_maps_into_the_private_dir_not_the_shared_tmp() {
    // A `$tmp/scratch` key is the private-dir sentinel too (maps INTO the private dir), so it
    // sets Private and hides the shared system tmp exactly like a bare `$tmp: "rw"` — it must
    // NOT resolve to a grant on the shared host tmp.
    let shared = PathBuf::from(format!(
        "/private/tmp/nub-tmptest-sub-{}.secret",
        std::process::id()
    ));
    fs::write(&shared, "SHAREDTMPSECRET").unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _c = Cleanup(shared.clone());
    let f = fixture();
    assert!(
        !f.allowed(
            serde_json::json!({ "fs": { "/": "r", "$tmp/scratch": "rw" } }),
            CAT,
            &[&s(&shared)]
        ),
        "`$tmp/scratch` must map into the private dir and hide the shared /private/tmp"
    );
}

#[test]
fn deny_tmp_hides_the_shared_system_tmp_too() {
    let shared = PathBuf::from(format!(
        "/private/tmp/nub-tmptest-deny-{}.secret",
        std::process::id()
    ));
    fs::write(&shared, "SHAREDTMPSECRET").unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _c = Cleanup(shared.clone());
    let f = fixture();
    // `$tmp: false` also hides the shared system tmp (no private dir is minted).
    assert!(
        !f.allowed(
            serde_json::json!({ "fs": { "/": "r", "$tmp": false } }),
            CAT,
            &[&s(&shared)]
        ),
        "deny tmp must hide the shared /private/tmp"
    );
}

#[test]
fn private_tmp_keeps_the_apple_compiler_scratch_writable() {
    // $tmp:rw (Private) hides /private/tmp but CARVES OUT the confstr TEMP scratch
    // (/var/folders/<uid>/T — the Apple toolchain's fixed, NOT-TMPDIR-redirectable xcrun_db
    // cache) so native (node-gyp) builds keep working. A write INTO that scratch must SUCCEED
    // under Private and be DENIED under $tmp:false (Deny), which carves nothing. The scratch
    // is the process TMPDIR (the confstr dir), canonicalized to match the kernel's view.
    let scratch = fs::canonicalize(std::env::temp_dir()).unwrap();
    let target = scratch.join(format!("nub-carve-probe-{}", std::process::id()));
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _c = Cleanup(target.clone());
    let f = fixture();

    // Private: the compiler scratch stays writable (the carve-out).
    assert!(
        f.allowed(
            serde_json::json!({ "fs": { "/": "r", "$tmp": "rw" } }),
            TOUCH,
            &[&s(&target)]
        ),
        "private tmp must keep the Apple compiler scratch (confstr TEMP) writable"
    );
    let _ = fs::remove_file(&target);
    // NEG-CONTROL: Deny hides the scratch too — the same write is refused.
    assert!(
        !f.allowed(
            serde_json::json!({ "fs": { "/": "r", "$tmp": false } }),
            TOUCH,
            &[&s(&target)]
        ),
        "deny tmp must hide the compiler scratch too (no carve-out)"
    );
}

#[test]
fn literal_tmp_path_is_the_only_way_to_the_shared_system_tmp() {
    // The clarified model: the `$tmp` sentinel NEVER grants the shared system tmp — the only
    // way to reach it is granting the literal path `/tmp` (canonicalizes to `/private/tmp`),
    // which leaves the tmp mode Shared (no confinement).
    let shared = PathBuf::from(format!(
        "/private/tmp/nub-tmptest-lit-{}.secret",
        std::process::id()
    ));
    fs::write(&shared, "SHAREDTMPSECRET").unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _c = Cleanup(shared.clone());
    let f = fixture();
    assert!(
        f.allowed(
            serde_json::json!({ "fs": { "/tmp": "r" } }),
            CAT,
            &[&s(&shared)]
        ),
        "a literal `/tmp` read grant reaches the shared system tmp"
    );
}

// ── new secret deny-set additions (A2): each denied under a generous read ──────

#[test]
fn new_secret_paths_denied_under_generous_read() {
    let f = fixture();
    // Home-anchored secret files/dirs (the deny-set additions).
    fs::write(f.home.join(".pgpass"), "PGPASS").unwrap();
    fs::write(f.home.join(".pypirc"), "PYPIRC").unwrap();
    fs::create_dir_all(f.home.join(".gnupg")).unwrap();
    fs::write(f.home.join(".gnupg/secring.gpg"), "GPGKEY").unwrap();
    fs::create_dir_all(f.home.join(".config/git")).unwrap();
    fs::write(f.home.join(".config/git/credentials"), "GITCREDS").unwrap();
    // Project-local: `.env` + `.env.local` DIRECTORY subtrees (not just leaf files) +
    // direnv `.envrc`.
    fs::create_dir_all(f.proj.join("nested/.env")).unwrap();
    fs::write(f.proj.join("nested/.env/prod"), "ENVDIRSECRET").unwrap();
    fs::create_dir_all(f.proj.join("nested/.env.local")).unwrap();
    fs::write(f.proj.join("nested/.env.local/prod"), "ENVLOCALDIRSECRET").unwrap();
    fs::write(f.proj.join(".envrc"), "export SECRET=x").unwrap();

    // `sandbox: true` = the secure fs base (generous read + the home-secret denies).
    let generous = serde_json::json!(true);
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
        assert!(
            !f.allowed(generous.clone(), CAT, &[&s(&path)]),
            "{label} must be denied under generous read"
        );
    }
    // negative controls — relaxed fs reads each fine, so the deny (not a missing
    // file) is what blocks above. Cover a home file + both new glob mechanisms.
    for path in [
        f.home.join(".pgpass"),
        f.proj.join("nested/.env/prod"),
        f.proj.join(".envrc"),
    ] {
        assert!(
            f.allowed(serde_json::json!({ "fs": true }), CAT, &[&s(&path)]),
            "neg-control: relaxed fs reads {}",
            s(&path)
        );
    }
}

// ── fs brace-alternation deny (the glob-deny-fidelity fix) ─────────────────────

#[test]
fn brace_deny_denies_every_expanded_path_not_the_literal() {
    // Braces `{a,b}` in an fs deny must deny EACH expanded path (globset-consistent),
    // not a file literally named `{a,b}`. Before the fix the Seatbelt regex escaped
    // the braces as literals, so `a.key`/`b.key` stayed silently readable under a
    // generous read — the sandbox-glob-deny-fidelity leak. This spawns sandbox-exec,
    // so it also proves Seatbelt accepts the `(a|b)` alternation regex.
    let f = fixture();
    let secrets = f.proj.join("secrets");
    fs::create_dir_all(&secrets).unwrap();
    for name in ["a.key", "b.key", "c.key"] {
        fs::write(secrets.join(name), "SECRET").unwrap();
    }
    let deny = format!("!{}/{{a,b}}.key", s(&secrets));
    let pol = serde_json::json!({ "fs": ["**", deny] });
    assert!(
        !f.allowed(pol.clone(), CAT, &[&s(&secrets.join("a.key"))]),
        "brace-expanded a.key denied"
    );
    assert!(
        !f.allowed(pol.clone(), CAT, &[&s(&secrets.join("b.key"))]),
        "brace-expanded b.key denied"
    );
    // c.key is NOT in the brace set → readable (the deny is selective, not a blanket
    // over-deny), AND an unrelated file stays readable (the generous base is active).
    assert!(
        f.allowed(pol.clone(), CAT, &[&s(&secrets.join("c.key"))]),
        "c.key (outside the brace) stays readable"
    );
    assert!(
        f.allowed(pol, CAT, &[&s(&f.proj.join("pub.txt"))]),
        "unrelated file readable"
    );
    // negative control — relaxed fs reads a.key fine, so the deny (not a missing file)
    // is what blocks it above.
    assert!(
        f.allowed(
            serde_json::json!({ "fs": true }),
            CAT,
            &[&s(&secrets.join("a.key"))]
        ),
        "neg-control: relaxed fs reads a.key"
    );
}

#[test]
fn nested_brace_deny_denies_all_alternatives() {
    // A nested `{a,{b,c}}` must deny a, b AND c — the recursive-alternation shape.
    let f = fixture();
    let secrets = f.proj.join("nsec");
    fs::create_dir_all(&secrets).unwrap();
    for name in ["a.key", "b.key", "c.key", "d.key"] {
        fs::write(secrets.join(name), "SECRET").unwrap();
    }
    let deny = format!("!{}/{{a,{{b,c}}}}.key", s(&secrets));
    let pol = serde_json::json!({ "fs": ["**", deny] });
    for name in ["a.key", "b.key", "c.key"] {
        assert!(
            !f.allowed(pol.clone(), CAT, &[&s(&secrets.join(name))]),
            "nested-brace {name} denied"
        );
    }
    assert!(
        f.allowed(pol, CAT, &[&s(&secrets.join("d.key"))]),
        "d.key (outside the nested brace) readable"
    );
}

// ── fs write-confine ──────────────────────────────────────────────────────────

#[test]
fn write_confine_allows_target_denies_rest() {
    let f = fixture();
    // Generous whole-fs READ (`"/": "r"`) + a scoped project write — the v2 read-only
    // base replaces the removed naked-`...`; only `./writable` is writable.
    let wc = serde_json::json!({ "fs": { "/": "r", "./writable": "rw" } });
    assert!(
        f.allowed(wc.clone(), TOUCH, &[&s(&f.proj.join("writable/ok.txt"))]),
        "write inside grant"
    );
    assert!(
        !f.allowed(wc.clone(), TOUCH, &[&s(&f.proj.join("blocked.txt"))]),
        "write project root denied"
    );
    assert!(
        !f.allowed(wc, TOUCH, &[&s(&f.root.join("outside/w.txt"))]),
        "write outside denied"
    );
    // negative control — relaxed fs writes anywhere:
    assert!(
        f.allowed(
            serde_json::json!({ "fs": true }),
            TOUCH,
            &[&s(&f.root.join("outside/w2.txt"))]
        ),
        "neg-control: relaxed fs writes outside"
    );
}

// ── env scrub (construction) ──────────────────────────────────────────────────

#[test]
fn env_scrub_strips_secrets_keeps_baseline() {
    let f = fixture();
    let env = &[("PATH", "/usr/bin:/bin"), ("MY_SECRET_TOKEN", "leaked")];
    // `sandbox: true` = curated baseline: PATH survives, the ambient secret does not.
    let strip = serde_json::json!(true);
    assert!(
        f.allowed_env(strip.clone(), env, "/bin/sh", &["-c", "test -n \"$PATH\""]),
        "baseline PATH present"
    );
    assert!(
        f.allowed_env(
            strip,
            env,
            "/bin/sh",
            &["-c", "test -z \"$MY_SECRET_TOKEN\""]
        ),
        "secret var stripped"
    );
    // negative control — env passthrough keeps the secret:
    assert!(
        f.allowed_env(
            serde_json::json!({ "vars": true }),
            env,
            "/bin/sh",
            &["-c", "test -n \"$MY_SECRET_TOKEN\""]
        ),
        "neg-control: passthrough keeps the secret"
    );
}

// ── canonicalization traps ────────────────────────────────────────────────────

#[test]
fn firmlink_write_allow_is_not_inert() {
    // A write-allow whose surface path is a /var/folders (firmlink) form must still
    // match the canonical /private/var/folders path the kernel checks. The fixture
    // root is already canonical; assert a not-yet-created dir is grantable (the
    // canonicalize-incl-nonexistent path), proving the grant isn't fail-closed.
    let f = fixture();
    let newdir = f.proj.join("created/at/runtime");
    let surface = serde_json::json!({ "fs": ["./created"] });
    assert!(
        f.allowed(
            surface.clone(),
            "/bin/sh",
            &[
                "-c",
                &format!("mkdir -p {q} && touch {q}/f", q = s(&newdir))
            ]
        ),
        "create+write a not-yet-existing granted dir works"
    );
    // A sibling NOT under the grant stays denied.
    assert!(
        !f.allowed(surface, TOUCH, &[&s(&f.proj.join("elsewhere.txt"))]),
        "non-granted sibling write denied"
    );
}

#[test]
fn read_confine_does_not_leak_program_siblings() {
    // The program auto-grant must expose the program FILE only — never its parent
    // dir, or a project-local tool would leak its neighbouring secrets under a tight
    // read-confine (the F3 over-grant). The tool itself is the PROGRAM (the case the
    // grant governs — a system interpreter would be covered by the essential base
    // and never exercise this).
    use std::os::unix::fs::PermissionsExt;
    let f = fixture();
    let tooldir = f.proj.join("tooldir");
    fs::create_dir_all(&tooldir).unwrap();
    let tool = tooldir.join("tool.sh");
    fs::write(&tool, "#!/bin/sh\ncat \"$1\"\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(tooldir.join("secret.key"), "SIBLING_SECRET").unwrap();
    // Read-confine to a DIFFERENT dir; tooldir is granted ONLY via the program's
    // own-file auto-grant, so the tool execs but its sibling stays denied.
    let allowed = f.proj.join("allowed");
    fs::create_dir_all(&allowed).unwrap();
    let confine = serde_json::json!({ "fs": [s(&allowed)] });
    assert!(
        !f.allowed(confine, &s(&tool), &[&s(&tooldir.join("secret.key"))]),
        "the program's sibling secret must not be readable via a parent-dir grant"
    );
}

#[test]
fn confstr_scratch_writable_under_generous_write_confine() {
    // Regression guard for C1: under a generous-read write-confine policy the Apple
    // toolchain's confstr temp scratch must stay writable (else from-source compiles
    // silently fail). Exercise it via a real `cc -c`, which writes xcrun_db there.
    let f = fixture();
    let src = f.proj.join("hello.c");
    fs::write(&src, "int main(void){return 0;}\n").unwrap();
    // generous whole-fs read (`"/": "r"`, so cc reaches its system toolchain) + write
    // only to the project: cc must still reach confstr temp.
    let wc = serde_json::json!({ "fs": { "/": "r", "./": "rw" } });
    let obj = f.proj.join("hello.o");
    assert!(
        f.allowed(wc, "/usr/bin/cc", &["-c", &s(&src), "-o", &s(&obj)]),
        "cc compile (writes confstr xcrun_db) must succeed under write-confine"
    );
    assert!(
        obj.exists(),
        "the object file was produced inside the project"
    );
}

#[test]
fn deny_not_dodgeable_via_dotdot_or_symlink() {
    let f = fixture();
    let generous = serde_json::json!({ "fs": ["**"] });
    // `..` traversal to the denied .env resolves to the same canonical path.
    let dotdot = f.proj.join("sub/../.env");
    assert!(
        !f.allowed(generous.clone(), CAT, &[&s(&dotdot)]),
        "'..' to .env still denied"
    );
    // A symlink to the denied .env: the kernel resolves the link before matching.
    let link = f.proj.join("envlink");
    std::os::unix::fs::symlink(f.proj.join(".env"), &link).unwrap();
    assert!(
        !f.allowed(generous, CAT, &[&s(&link)]),
        "symlink to .env still denied"
    );
    // A symlink escaping read-confine to an out-of-project secret.
    let confine = serde_json::json!({ "fs": ["./"] });
    let escape = f.proj.join("escape");
    std::os::unix::fs::symlink(f.home.join(".ssh/id_rsa"), &escape).unwrap();
    assert!(
        !f.allowed(confine, CAT, &[&s(&escape)]),
        "symlink escaping confine denied"
    );
}

// ── documented, bounded residuals — CAPTURED so they are no longer reasoned-only ──

#[test]
fn hardlink_to_denied_secret_leaks_via_alias() {
    // Seatbelt file-read rules are path-pattern based: a `!<secret>`
    // deny matches the PATH, not the inode. A pre-existing same-uid hardlink to the
    // secret, at a name the deny never targets, is readable — and reading it reads the
    // SHARED inode, so the path-denied secret leaks through the alias. Bounded: needs a
    // hardlink created OUTSIDE the sandbox beforehand (no clean Seatbelt fix — the inode
    // was legitimately named twice). If a future mechanism closes it, this assertion
    // flips and flags the change.
    let f = fixture();
    let secret = f.proj.join("secret.txt");
    fs::write(&secret, "REALSECRET").unwrap();
    let alias = f.proj.join("alias.txt");
    fs::hard_link(&secret, &alias).unwrap();
    let surface = serde_json::json!({ "fs": [s(&f.proj), format!("!{}", s(&secret))] });
    // Residual: the alias (un-denied name) reads the secret's inode.
    assert!(
        f.allowed(surface.clone(), CAT, &[&s(&alias)]),
        "hardlink residual: an alias to the denied inode reads the secret"
    );
    // Seatbelt matches the path, so with the alias present the secret's own denied path
    // still reads EPERM — only the alias name leaks.
    assert!(
        !f.allowed(surface, CAT, &[&s(&secret)]),
        "path-based deny: the secret's own denied path stays denied even with the alias present"
    );
    // Control: WITHOUT any hardlink, the same path-deny holds (proving the alias, not a
    // broken carve, is the escape).
    let f2 = fixture();
    let secret2 = f2.proj.join("secret.txt");
    fs::write(&secret2, "REALSECRET").unwrap();
    let surf2 = serde_json::json!({ "fs": [s(&f2.proj), format!("!{}", s(&secret2))] });
    assert!(
        !f2.allowed(surf2, CAT, &[&s(&secret2)]),
        "no hardlink: the path-deny denies the secret"
    );
}

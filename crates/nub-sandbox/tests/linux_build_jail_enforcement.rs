//! End-to-end enforcement of nub's build-jail on LINUX (Bubblewrap) — the counterpart
//! to the macOS `build_jail_enforcement.rs`. Same load-bearing security contract, one
//! OS apart: the package dir is writable, home SECRETS stay both unreadable AND
//! unwritable, the `.env*`/`.npmrc` floor holds at any project depth, `/etc/shadow`
//! stays denied inside the read-only `/etc` the minimal root grants, and egress is
//! denied. Keep the two files in step — when either grows an assertion, the other
//! wants it too, because the jail they pin runs on EVERY install and a gap on one OS
//! is a gap in production.
//!
//! ASSERT ON DENIAL, NEVER ON AN ERRNO — the single most portable-looking trap here.
//! macOS Seatbelt denies a path that still RESOLVES, so the child gets EPERM
//! ("Operation not permitted"); Bubblewrap never mounts the path into the child's view
//! at all, so the same read is ENOENT ("No such file or directory"). A macOS assertion
//! ported verbatim (`stdout.contains("Operation not permitted")`) fails here for the
//! wrong reason. The `.env*` floor is a third shape again: the Linux backend masks a
//! dotenv file as an EMPTY, mode-444 regular file (`MaskKind::EmptyReadable`) so a
//! dotenv loader sees no secret rather than a hard error — the read SUCCEEDS and yields
//! nothing. Every assertion below is therefore "the secret bytes never reached the
//! child", which is true under all three shapes and survives a mask-kind change.
//!
//! NON-VACUOUSNESS. A denial proves nothing if the child never ran, and a masked file
//! proves nothing if the whole directory vanished. Each test shows, in the SAME run,
//! the package dir being read AND written, and places a readable sentinel file at the
//! SAME DEPTH as every denied one — so a merely-lexical or whole-subtree failure would
//! have taken the sentinel with it.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The same gate `linux_enforcement.rs` uses: skip where Bubblewrap cannot create an
/// unprivileged user namespace, but turn that skip into a HARD FAILURE under
/// `NUB_SANDBOX_REQUIRE_BWRAP` so the conformance leg proves the tests actually ran.
/// Duplicated per test binary (each is its own process) rather than shared — the
/// crate's existing convention in `linux_enforcement`/`linux_proxy`/`ir`.
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

fn require_enforcement() -> bool {
    matches!(
        std::env::var("NUB_SANDBOX_REQUIRE_BWRAP").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// Returns true when the caller should skip.
fn skip_without_bwrap() -> bool {
    if bwrap_available() {
        return false;
    }
    assert!(
        !require_enforcement(),
        "NUB_SANDBOX_REQUIRE_BWRAP set but Bubblewrap cannot create the required user namespace"
    );
    true
}

fn runtime() -> &'static nub_sandbox::RuntimeCapability {
    static RUNTIME: std::sync::OnceLock<nub_sandbox::RuntimeCapability> =
        std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        nub_sandbox::RuntimeCapability::from_verified_executable(env!(
            "CARGO_BIN_EXE_nub-sandbox-monitor-harness"
        ))
        .expect("verify the purpose-built sandbox monitor harness")
    })
}

/// One dependency lifecycle spawn, compiled and launched exactly as
/// `nub-cli`'s `pm_engine::build_jail` does it in production: the same
/// `compile_build_jail` entry, the same `deny_search_roots` pair (project root +
/// package dir — the dirs whose `.env*`/`.npmrc` children the backend must materialize
/// masks for), the same fail-closed apply.
struct Jail {
    home: PathBuf,
    project: PathBuf,
    package_dir: PathBuf,
}

impl Jail {
    fn policy(&self) -> nub_sandbox::policy::SandboxPolicy {
        let ambient: BTreeMap<String, String> = [("PATH", "/usr/bin:/bin")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        nub_sandbox::compile_build_jail(
            nub_sandbox::Homes {
                home: self.home.clone(),
                tmp: std::env::temp_dir(),
                cache: self.home.join(".cache"),
                project: self.project.clone(),
            },
            &self.package_dir,
            Vec::new(),
            Vec::new(),
            ambient,
        )
        .expect("compile build-jail")
    }

    fn spec(&self, program: &str) -> nub_sandbox::CommandSpec {
        nub_sandbox::CommandSpec::new(program)
            .cwd(&self.package_dir)
            .deny_search_roots([self.project.clone(), self.package_dir.clone()])
    }

    fn launch(&self, spec: nub_sandbox::CommandSpec) -> std::process::Output {
        let policy = self.policy();
        // `.expect` rather than a skip: an apply error is the jail failing to confine,
        // which must never read as a denial.
        nub_sandbox::apply_with_runtime(&policy, spec, runtime())
            .expect("apply build-jail (fail-closed on error)")
            .output()
            .expect("run the confined child")
    }

    /// Run `script` under `/bin/sh -c` in the package dir; return its stdout.
    fn run_script(&self, script: &str) -> String {
        let out = self.launch(self.spec("/bin/sh").arg("-c").arg(script));
        if !out.stderr.is_empty() {
            eprintln!(
                "confined child stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Run a compiled probe binary with `args`; return its exit code.
    fn run_probe(&self, program: &Path, args: &[&str]) -> i32 {
        let spec = self
            .spec(&program.to_string_lossy())
            .args(args.iter().copied());
        self.launch(spec).status.code().unwrap_or(-1)
    }
}

/// `cat <path>` with a per-probe outcome marker. `_READ_OK` also covers the dotenv
/// EMPTY-mask shape (a successful read of nothing) — which is why every caller pairs
/// the marker with a sentinel-token check on the whole stdout.
fn probe(tag: &str, path: &Path) -> String {
    format!(
        "cat '{}' 2>/dev/null && echo {tag}_READ_OK || echo {tag}_DENIED\n",
        path.display()
    )
}

fn assert_no_leak(stdout: &str, sentinel: &str, what: &str) {
    assert!(
        !stdout.contains(sentinel),
        "{what} leaked into the confined child (found {sentinel:?}):\n{stdout}"
    );
}

/// The production shape: a native build/postinstall reads its own source and writes its
/// own package dir, while every secret class the jail promises to withhold stays
/// withheld — the home secret (read AND write), the project `.env` floor at root and
/// nested depth, the `.npmrc` floor at both depths, and `/etc/shadow`.
#[test]
fn build_jail_confines_writes_and_withholds_every_secret_class() {
    if skip_without_bwrap() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // Canonicalized up front: the mount plan binds canonical paths, so a `/tmp` that is
    // itself a symlink would otherwise make the fixture paths and the child's view differ.
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let home = root.join("home");
    let project = root.join("project");
    let package_dir = project.join("node_modules/.aube/native@1.0.0/node_modules/native");
    std::fs::create_dir_all(&package_dir).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::create_dir_all(project.join("apps/web")).unwrap();
    std::fs::create_dir_all(project.join("packages/api")).unwrap();

    std::fs::write(home.join(".ssh/id_rsa"), b"HOME-SECRET-DO-NOT-LEAK").unwrap();
    std::fs::write(package_dir.join("binding.gyp"), b"{}").unwrap();
    // The `.env*` floor, at the project root and nested in real source. Both sit inside
    // the build-jail's `"./"` project READ grant, so only the reasserted secret floor
    // (Linux: the recursive deny-snapshot's per-file mask) keeps their bytes out.
    std::fs::write(project.join(".env"), b"ROOT-ENV-SECRET").unwrap();
    std::fs::write(project.join("apps/web/.env"), b"NESTED-ENV-SECRET").unwrap();
    std::fs::write(
        project.join(".npmrc"),
        b"//registry.npmjs.org/:_authToken=ROOT-NPMRC-SECRET",
    )
    .unwrap();
    std::fs::write(
        project.join("packages/api/.npmrc"),
        b"//registry.npmjs.org/:_authToken=NESTED-NPMRC-SECRET",
    )
    .unwrap();
    // Readable sentinels at the SAME DEPTH as each denied file. If the floor were
    // enforced by blanking a directory — or if the child simply never started — these
    // would be missing too, and the denials above would be worthless.
    std::fs::write(project.join("README.md"), b"ROOT-SIBLING-VISIBLE").unwrap();
    std::fs::write(
        project.join("apps/web/index.js"),
        b"NESTED-ENV-SIBLING-VISIBLE",
    )
    .unwrap();
    std::fs::write(
        project.join("packages/api/index.js"),
        b"NESTED-NPMRC-SIBLING-VISIBLE",
    )
    .unwrap();

    let jail = Jail {
        home: home.clone(),
        project: project.clone(),
        package_dir: package_dir.clone(),
    };
    let secret_write = home.join(".ssh/planted.txt");
    let script = format!(
        "cat binding.gyp >/dev/null 2>&1 && echo PKG_READ_OK || echo PKG_READ_FAIL\n\
         echo built > build_out.txt 2>/dev/null && echo PKG_WRITE_OK || echo PKG_WRITE_FAIL\n\
         echo evil > '{secret_write}' 2>/dev/null && echo SECRET_WROTE || echo SECRET_WRITE_BLOCKED\n\
         {home_secret}{root_env}{nested_env}{root_npmrc}{nested_npmrc}\
         {root_sibling}{env_sibling}{npmrc_sibling}{passwd}{shadow}",
        secret_write = secret_write.display(),
        home_secret = probe("HOME_SECRET", &home.join(".ssh/id_rsa")),
        root_env = probe("ROOT_ENV", &project.join(".env")),
        nested_env = probe("NESTED_ENV", &project.join("apps/web/.env")),
        root_npmrc = probe("ROOT_NPMRC", &project.join(".npmrc")),
        nested_npmrc = probe("NESTED_NPMRC", &project.join("packages/api/.npmrc")),
        root_sibling = probe("ROOT_SIBLING", &project.join("README.md")),
        env_sibling = probe("ENV_SIBLING", &project.join("apps/web/index.js")),
        npmrc_sibling = probe("NPMRC_SIBLING", &project.join("packages/api/index.js")),
        passwd = probe("PASSWD", Path::new("/etc/passwd")),
        shadow = probe("SHADOW", Path::new("/etc/shadow")),
    );
    let stdout = jail.run_script(&script);

    // Non-vacuousness first: the child ran, read its own source, and wrote its own dir.
    assert!(
        stdout.contains("PKG_READ_OK"),
        "the package source must be readable — without this every denial below is hollow:\n{stdout}"
    );
    assert!(
        stdout.contains("PKG_WRITE_OK") && package_dir.join("build_out.txt").exists(),
        "the package dir must be writable:\n{stdout}"
    );
    for (sentinel, tag) in [
        ("ROOT-SIBLING-VISIBLE", "ROOT_SIBLING"),
        ("NESTED-ENV-SIBLING-VISIBLE", "ENV_SIBLING"),
        ("NESTED-NPMRC-SIBLING-VISIBLE", "NPMRC_SIBLING"),
    ] {
        assert!(
            stdout.contains(sentinel) && stdout.contains(&format!("{tag}_READ_OK")),
            "the readable sentinel beside the denied file must survive, otherwise the \
             denial is a missing directory rather than an enforced floor:\n{stdout}"
        );
    }

    assert!(
        stdout.contains("HOME_SECRET_DENIED"),
        "the home secret must be unreadable:\n{stdout}"
    );
    assert_no_leak(&stdout, "HOME-SECRET-DO-NOT-LEAK", "the home secret");
    assert!(
        stdout.contains("SECRET_WRITE_BLOCKED") && !secret_write.exists(),
        "a write into the home secret dir must be blocked:\n{stdout}"
    );
    assert_eq!(
        std::fs::read(home.join(".ssh/id_rsa")).unwrap(),
        b"HOME-SECRET-DO-NOT-LEAK",
        "the home secret file is untouched"
    );

    // The `.env*` floor. Deliberately NOT asserted as a denial: the dotenv mask is
    // present-but-empty by design, so the contract is that no BYTE of either file
    // reaches the child.
    assert_no_leak(&stdout, "ROOT-ENV-SECRET", "the project root .env");
    assert_no_leak(&stdout, "NESTED-ENV-SECRET", "a nested project .env");
    // `.npmrc` has no compatibility mask — it is a hard denial at both depths.
    assert!(
        stdout.contains("ROOT_NPMRC_DENIED") && stdout.contains("NESTED_NPMRC_DENIED"),
        "a project .npmrc must be unreadable at any depth:\n{stdout}"
    );
    assert_no_leak(&stdout, "ROOT-NPMRC-SECRET", "the project root .npmrc");
    assert_no_leak(&stdout, "NESTED-NPMRC-SECRET", "a nested project .npmrc");

    // `/etc` is granted read-only wholesale by the minimal root; the two password-hash
    // files are denied within it. `/etc/passwd` is the same-directory readable control —
    // it proves `/etc` is mounted, so the shadow denial is targeted rather than a
    // missing mount.
    assert!(
        stdout.contains("PASSWD_READ_OK"),
        "/etc must be readable — otherwise the /etc/shadow denial is just a missing mount:\n{stdout}"
    );
    assert!(
        stdout.contains("SHADOW_DENIED"),
        "/etc/shadow must be denied inside the granted /etc:\n{stdout}"
    );
}

/// The jail denies egress. The confined child cannot reach a listener the SAME binary
/// reaches unconfined a moment earlier — the differential is what rules out a probe that
/// simply never connects.
///
/// Two mechanisms can produce the denial (an empty network namespace, or the seccomp
/// filter refusing `AF_INET` socket creation); the probe reports either as 42, because
/// which one fired is the backend's business, not this contract's.
#[test]
fn build_jail_denies_egress_to_a_reachable_listener() {
    if skip_without_bwrap() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let project = root.join("project");
    let package_dir = project.join("node_modules/.aube/native@1.0.0/node_modules/native");
    std::fs::create_dir_all(&package_dir).unwrap();

    let Some(probe_bin) = compile_connect_probe(&package_dir) else {
        // A host with Bubblewrap but no C compiler can still run every other test here;
        // the enforcing conformance leg has `cc`, and requiring it there keeps this
        // axis from becoming a silent skip on the one leg that can prove it.
        assert!(
            !require_enforcement(),
            "NUB_SANDBOX_REQUIRE_BWRAP set but no working `cc` to build the egress probe"
        );
        return;
    };

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let accept = std::thread::spawn(move || listener.accept().map(|_| ()));

    let unconfined = std::process::Command::new(&probe_bin)
        .arg(&port)
        .status()
        .expect("run the probe unconfined")
        .code();
    assert_eq!(
        unconfined,
        Some(0),
        "control: the probe must reach the listener unconfined, or the confined \
         failure below says nothing about the jail"
    );
    accept.join().unwrap().expect("the control connected");

    let jail = Jail {
        home: root.join("home"),
        project,
        package_dir,
    };
    assert_eq!(
        jail.run_probe(&probe_bin, &[&port]),
        42,
        "the build-jail must deny egress to a listener the same probe just reached"
    );
}

/// A FETCHED git dependency's checkout is confined as a whole, and every byte of it is
/// attacker-authored — `prepare_scratch_copy` uses `cp -a`, so a symlink committed in
/// the repo survives into the scratch tree the jail grants. Bubblewrap never mounts the
/// symlink's target, so the read fails; the macOS twin gets the same denial through
/// Seatbelt's canonical-path check.
///
/// The second escape is the jail's PROJECT anchor: scratch copies are siblings in the
/// system temp dir, and a checkout picks its own importer directory via
/// `workspaces: ["../**"]`. Re-running the identical script with ONE variable changed —
/// the anchor moved to the sibling — makes the sibling readable, which is what turns the
/// anchored run's denial into evidence rather than a hollow pass.
#[test]
fn a_fetched_checkout_reads_nothing_outside_itself_through_a_symlink_or_a_steered_anchor() {
    if skip_without_bwrap() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let home = root.join("home");
    let checkout = root.join("checkout");
    let sibling = root.join("sibling-scratch");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(checkout.join("src")).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(home.join(".nub-secret"), b"HOME-SECRET-DO-NOT-LEAK").unwrap();
    std::fs::write(sibling.join("scratch.txt"), b"SIBLING-SCRATCH-DO-NOT-LEAK").unwrap();
    std::fs::write(checkout.join("src/own.txt"), b"CHECKOUT-OWN-FILE").unwrap();
    std::os::unix::fs::symlink(&home, checkout.join("evil")).unwrap();

    let script = format!(
        "cat src/own.txt 2>/dev/null && echo OWN_READ_OK || echo OWN_READ_FAIL\n\
         echo built > out.txt 2>/dev/null && echo OWN_WRITE_OK || echo OWN_WRITE_FAIL\n\
         {symlinked}{sibling_file}",
        symlinked = probe("SYMLINK", &checkout.join("evil/.nub-secret")),
        sibling_file = probe("SIBLING", &sibling.join("scratch.txt")),
    );
    let run = |project: &Path| -> String {
        Jail {
            home: home.clone(),
            project: project.to_path_buf(),
            package_dir: checkout.clone(),
        }
        .run_script(&script)
    };

    let anchored = run(&checkout);
    assert!(
        anchored.contains("OWN_READ_OK")
            && anchored.contains("CHECKOUT-OWN-FILE")
            && anchored.contains("OWN_WRITE_OK"),
        "the checkout must be readable and writable — without this the denials prove nothing:\n{anchored}"
    );
    assert!(
        anchored.contains("SYMLINK_DENIED"),
        "a symlink committed in the checkout must not reach the home secret:\n{anchored}"
    );
    assert_no_leak(&anchored, "HOME-SECRET-DO-NOT-LEAK", "the home secret");
    assert!(
        anchored.contains("SIBLING_DENIED"),
        "the checkout must not read a sibling scratch dir:\n{anchored}"
    );
    assert_no_leak(
        &anchored,
        "SIBLING-SCRATCH-DO-NOT-LEAK",
        "a sibling scratch",
    );

    let steered = run(&sibling);
    assert!(
        steered.contains("SIBLING-SCRATCH-DO-NOT-LEAK"),
        "control failed: anchoring the project on the sibling must expose it, otherwise \
         the anchored run's denial is not evidence that the anchor withheld it:\n{steered}"
    );
    assert!(
        steered.contains("OWN_READ_OK") && steered.contains("OWN_WRITE_OK"),
        "control sanity: the checkout's own grants are unaffected by the anchor:\n{steered}"
    );
}

/// Build the egress probe: exit 0 when it connects to `127.0.0.1:<argv[1]>`, 42 when the
/// socket or the connect was refused. `None` when no working `cc` exists.
fn compile_connect_probe(dir: &Path) -> Option<PathBuf> {
    const SRC: &str = r#"
#include <stdlib.h>
#include <string.h>
#include <netinet/in.h>
#include <sys/socket.h>
int main(int argc, char **argv) {
    if (argc < 2) return 2;
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) return 42;
    struct sockaddr_in addr;
    memset(&addr, 0, sizeof addr);
    addr.sin_family = AF_INET;
    addr.sin_port = htons((unsigned short)atoi(argv[1]));
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    return connect(fd, (struct sockaddr *)&addr, sizeof addr) == 0 ? 0 : 42;
}
"#;
    let src = dir.join("connect_probe.c");
    let bin = dir.join("connect_probe");
    std::fs::write(&src, SRC).ok()?;
    std::process::Command::new("cc")
        .arg(&src)
        .arg("-o")
        .arg(&bin)
        .status()
        .ok()?
        .success()
        .then_some(bin)
}

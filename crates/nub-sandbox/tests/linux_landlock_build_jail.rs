//! Landlock build-jail regressions that only the NO-NAMESPACE mechanism can exhibit.
//!
//! The bubblewrap suite next door cannot cover these. Bubblewrap gives the child a fresh
//! mount view, so an ungranted path is simply ABSENT (`ENOENT`); Landlock leaves the real
//! filesystem in place and denies with `EACCES` — the path stays VISIBLE but unreadable.
//! Every bug pinned here is a program that probed for a file, was told it exists, and then
//! could not read it. That shape is invisible to bubblewrap by construction.
//!
//! WHY THE SUBJECT IS NUB'S OWN BINARY AND NOT `node`. The defect these pin killed the nub
//! process itself, not the script: plain `node` survives an unreadable `/proc/self/maps`
//! (measured), while nub aborts, because nub's earliest embedder action pins its runtime
//! identity from that file. Under a build jail every `node` a lifecycle script invokes is
//! nub's PATH shim re-execing the nub binary, so nub's startup IS the lifecycle script's
//! startup. The monitor harness is the binary that reproduces it — it calls the same
//! `earliest_bootstrap()` as its first action — which makes it the correct subject here,
//! not a stand-in for one.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Landlock is the build jail's ONLY mechanism: `linux::preflight` returns the Landlock arm
/// unconditionally for a `build_jail` policy whenever the kernel provides it, with no
/// bubblewrap arm below. So a usable ABI here is not merely a skip gate — it is what makes
/// every assertion in this file a statement about Landlock.
fn skip_without_landlock() -> bool {
    if nub_sandbox::host_probe::landlock_abi().is_some() {
        return false;
    }
    eprintln!("skipping linux_landlock_build_jail: kernel has no usable Landlock (needs 5.13+)");
    true
}

/// The capability an embedder holds for the whole process. Built through the real
/// `earliest_bootstrap` rather than a verified-executable stub, because on this path the
/// capture's own outcome is part of what is under test: the Landlock arm must launch
/// whether or not it succeeded.
fn runtime() -> &'static nub_sandbox::RuntimeCapability {
    static RUNTIME: std::sync::OnceLock<nub_sandbox::RuntimeCapability> =
        std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| nub_sandbox::earliest_bootstrap().expect("earliest bootstrap"))
}

struct Jail {
    home: PathBuf,
    project: PathBuf,
    package_dir: PathBuf,
}

impl Jail {
    /// `package_name` is the installer-resolved identity the egress catalog is keyed on, so it
    /// is the ONLY variable between the two arms of the egress test below.
    fn policy_for(&self, package_name: Option<&str>) -> nub_sandbox::policy::SandboxPolicy {
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
            package_name,
            Some("1.0.0"),
            Vec::new(),
            Vec::new(),
            ambient,
        )
        .expect("compile build-jail")
    }

    fn launch(&self, spec: nub_sandbox::CommandSpec) -> std::process::Output {
        self.launch_as(None, spec)
    }

    fn launch_as(
        &self,
        package_name: Option<&str>,
        spec: nub_sandbox::CommandSpec,
    ) -> std::process::Output {
        // `.expect` rather than a skip: an apply error is the jail failing to confine,
        // which must never read as a denial.
        nub_sandbox::apply_with_runtime(&self.policy_for(package_name), spec, runtime())
            .expect("apply build-jail (fail-closed on error)")
            .output()
            .expect("run the confined child")
    }

    fn spec(&self, program: &str) -> nub_sandbox::CommandSpec {
        nub_sandbox::CommandSpec::new(program).cwd(&self.package_dir)
    }
}

fn jail() -> (tempfile::TempDir, Jail) {
    let root = tempfile::tempdir().expect("temp root");
    let home = root.path().join("home");
    let project = home.join("proj");
    let package_dir = project.join("node_modules/dep");
    std::fs::create_dir_all(&package_dir).unwrap();
    std::fs::write(package_dir.join("own.txt"), "PACKAGE_OWN_FILE").unwrap();
    std::fs::write(home.join("secret.txt"), "HOME_SECRET_TOKEN").unwrap();
    let jail = Jail {
        home,
        project,
        package_dir,
    };
    (root, jail)
}

/// THE REGRESSION. nub's earliest embedder action pins its runtime identity from
/// `/proc/self/maps`, which no build-jail grant can cover: the Landlock ruleset is built
/// before `fork`, so `/proc/self` would resolve to nub's own PID rather than the child's,
/// and granting `/proc` wholesale would expose every same-uid process's `environ`. The
/// capture must therefore be allowed to FAIL — it is consumed only by the bubblewrap
/// launch — and failing it eagerly aborted every nub process nested inside a jail before
/// its work began, which is what took 121 packages' lifecycle scripts down.
///
/// NON-VACUOUSNESS. A clean exit proves nothing if the jail never engaged, so the same
/// jail is shown DENYING in the same test: a home secret stays unreadable while the
/// package's own file reads back. Without that pair, a Landlock ruleset that silently
/// restricted nothing would satisfy the startup assertion perfectly.
#[test]
fn a_nub_process_starts_under_the_jail_that_denies_it_proc_self_maps() {
    if skip_without_landlock() {
        return;
    }
    let (_root, jail) = jail();

    let probe = jail.launch(
        jail.spec(env!("CARGO_BIN_EXE_nub-sandbox-monitor-harness"))
            .arg("startup-probe"),
    );
    assert_eq!(
        probe.status.code(),
        Some(0),
        "a nub binary must complete its earliest bootstrap inside the jail; \
         exit 125 is the bootstrap abort this pins:\n{}",
        String::from_utf8_lossy(&probe.stderr)
    );

    // The control, in the SAME jail configuration.
    let out = jail.launch(jail.spec("/bin/sh").arg("-c").arg(format!(
        "cat '{}' 2>/dev/null; echo; cat own.txt 2>/dev/null",
        jail.home.join("secret.txt").display()
    )));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("HOME_SECRET_TOKEN"),
        "the jail must still withhold the home secret — a startup pass means nothing \
         if the ruleset restricted nothing:\n{stdout}"
    );
    assert!(
        stdout.contains("PACKAGE_OWN_FILE"),
        "the confined child must still read its own package dir; losing this means the \
         denial above proves only that the child never ran:\n{stdout}"
    );
}

/// PER-PACKAGE EGRESS, end to end on the only mechanism production uses. The catalog gate used
/// to resolve correctly and then change nothing here: `apply_landlock` passed a hardcoded
/// `per_host=false` into `build_seccomp`, so a granted package's `["$downloads"]` and an
/// ungranted package's `false` compiled to the same all-families deny and a granted package got
/// no network at all.
///
/// ONE VARIABLE, and it is the package name. The same probe binary, the same jail, the same
/// listener — `node-gyp` carries a catalog entry and `left-pad` does not, so a difference between
/// the two runs can only come from the identity gate. Both arms are asserted, because the deny
/// half is the whole defense against the Shai-Hulud shape and a fix that granted everything
/// would satisfy the allow half perfectly.
///
/// THE LISTENER IS LOOPBACK, and deliberately so: it makes the allow arm provable with no
/// external network and no DNS, and reaching it is an honest statement of what this mechanism
/// grants. With no netns there is nothing to confine a permitted `AF_INET` to a host set, which
/// is why `apply_landlock` documents the grant as coarse.
///
/// EXIT CODES SEPARATE THE TWO FAILURES that would otherwise look alike: 10 is the socket
/// ceiling refusing to create the socket, 11 is a socket that exists but could not connect. A
/// test asserting only "non-zero" would pass on a jail that broke connect for an unrelated
/// reason.
#[test]
fn egress_is_granted_by_catalog_entry_and_refused_without_one() {
    if skip_without_landlock() {
        return;
    }
    let (_root, jail) = jail();
    let Some(probe) = compile_socket_probe(&jail.package_dir) else {
        eprintln!("skipping the egress arm: no working `cc` to build the socket probe");
        return;
    };

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    std::thread::spawn(move || while listener.accept().is_ok() {});

    let run = |name: Option<&str>| {
        let out = jail.launch_as(name, jail.spec(&probe.to_string_lossy()).arg(&port));
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };

    // GRANTED. `node-gyp` is catalogued (it fetches Node headers from nodejs.org when the
    // header cache is cold), so the ceiling admits the IP families and the probe reaches the
    // listener.
    //
    // ASSERTED, NOT ASSUMED. An uncatalogued fixture does not weaken this test into a false
    // pass — it makes the granted arm UNSATISFIABLE, and it reports as a bare `Some(10)` that
    // reads exactly like the egress mechanism being broken. That is not hypothetical: this arm
    // was originally written against `canvas`, which is not in the table and never was, so it
    // could not pass from the commit that introduced it. Check the table directly, so a catalog
    // edit that drops the entry fails HERE naming the cause.
    const GRANTED: &str = "node-gyp";
    // ⛔ THE VERSION ARGUMENT IS NOT DECORATION. `build_jail_net_allowed` became version-scoped in
    // 375fd1ee4c, which updated all 19 call sites EXCEPT this one — and because this file is
    // Landlock-specific, no macOS or Windows gate ever compiles it, so the E0061 was invisible
    // locally and would have failed CI's Linux leg. `node-gyp` carries no version band in the
    // generated egress table, so any concrete version admits it; passing one keeps this arm honest
    // if a band is ever added.
    const GRANTED_VERSION: &str = "1.0.0";
    assert!(
        nub_sandbox::build_jail_net_allowed(Some(GRANTED), Some(GRANTED_VERSION)),
        "fixture `{GRANTED}` is absent from the generated egress table {:?}, so the granted arm \
         cannot pass — pick a catalogued package rather than granting this one network",
        nub_sandbox::PACKAGE_NETWORK_ALLOWED
    );

    let (code, stdout) = run(Some(GRANTED));
    assert_eq!(
        code,
        Some(0),
        "a catalogued package must reach the listener; 10 means the socket ceiling still \
         refuses it, which is the defect this pins:\n{stdout}"
    );

    // REFUSED. `left-pad` has no entry — the Shai-Hulud shape, an ordinary dependency that
    // could acquire a lifecycle script nobody reviewed.
    let (code, stdout) = run(Some("left-pad"));
    assert_eq!(
        code,
        Some(10),
        "an uncatalogued package must be refused AT SOCKET CREATION; 0 means the grant \
         leaked to every package and 11 means it was denied for the wrong reason:\n{stdout}"
    );

    // `None` — aube withholds the identity when the spawn root is a checkout it FETCHED — reads
    // as uncatalogued, the conservative direction.
    assert_eq!(
        run(None).0,
        Some(10),
        "a withheld package identity must be refused, not treated as unlisted-but-fine"
    );

    // THE CEILING'S OTHER FAMILIES ARE NOT PART OF THE GRANT. AF_UNIX reaches host daemons
    // through a filesystem path nothing here scopes and AF_VSOCK is CID-addressed to the
    // hypervisor, so both stay denied for the GRANTED package too — the arm where a widened
    // carve-out would show up.
    let (_, stdout) = run(Some(GRANTED));
    for family in ["UNIX", "VSOCK"] {
        assert!(
            stdout.contains(&format!("{family}=DENIED")),
            "a granted package must still be denied AF_{family}:\n{stdout}"
        );
    }
}

/// Build the socket probe. Exit 0 = connected to `127.0.0.1:<argv[1]>`; 10 = `socket(AF_INET)`
/// was refused; 11 = the socket existed but the connect failed. Also prints one line per extra
/// family so the ceiling's unchanged half is asserted in the same run. `None` when no `cc`
/// exists, which is a skip rather than a failure — every other test in this file still runs.
fn compile_socket_probe(dir: &std::path::Path) -> Option<PathBuf> {
    const SRC: &str = r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <netinet/in.h>
#include <sys/socket.h>
static void fam(const char *label, int family) {
    int fd = socket(family, SOCK_STREAM, 0);
    if (fd < 0) printf("%s=DENIED\n", label);
    else { printf("%s=CREATED\n", label); close(fd); }
}
int main(int argc, char **argv) {
    if (argc < 2) return 2;
    fam("UNIX", AF_UNIX);
    fam("VSOCK", 40);
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) { printf("INET=DENIED\n"); fflush(stdout); return 10; }
    printf("INET=CREATED\n");
    fflush(stdout);
    struct sockaddr_in addr;
    memset(&addr, 0, sizeof addr);
    addr.sin_family = AF_INET;
    addr.sin_port = htons((unsigned short)atoi(argv[1]));
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    return connect(fd, (struct sockaddr *)&addr, sizeof addr) == 0 ? 0 : 11;
}
"#;
    let src = dir.join("socket_probe.c");
    let bin = dir.join("socket_probe");
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

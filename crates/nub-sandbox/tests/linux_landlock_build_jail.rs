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

/// The version every jail here is compiled at. Shared so a test that asks the catalog gate a
/// question directly gets the answer for the policy it is about to launch — the two drifting apart
/// is how an arm turns into a silent false pass. Both fixtures below are entries with no version
/// band, so any concrete version resolves to their `default`.
const JAIL_VERSION: &str = "1.0.0";

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
            Some(JAIL_VERSION),
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
/// listener, so a difference between runs can only come from the identity gate. Both directions
/// are asserted, because a fix that granted everything would satisfy the allow half perfectly.
///
/// ⛔ WHAT "REFUSED" MEANS CHANGED, AND THE UNCATALOGUED ARM SWAPPED SIDES. Until `4001cec5c5`
/// (2026-08-16) an absent entry meant no egress, and this test asserted `left-pad` and `None` were
/// refused. An absent entry now resolves to `catalog_v2::baseline_caps()`, which GRANTS network —
/// the catalog ships compiled in with `include_str!` while npm publishes continuously, so refusing
/// every package released after a nub build is not a posture that converges. Those two arms
/// therefore assert reachability now, and the refusal is pinned where it still exists: an entry the
/// catalog NAMES while withholding `network`, which is the only shape reaching `ip_egress_for`'s
/// `Denied` and so the only one that installs the socket ceiling at all. The Shai-Hulud shape is
/// answered on the FILESYSTEM axis instead — an unknown package reads nothing worth exfiltrating
/// down the socket it is now allowed to open.
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
fn egress_is_granted_by_catalog_entry_or_baseline_and_refused_by_a_withholding_entry() {
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
    // ⛔ THE VERSION ARGUMENT IS NOT DECORATION. The gate became version-scoped in 375fd1ee4c, which
    // updated all 19 call sites EXCEPT this one — and because this file is Landlock-specific, no
    // macOS or Windows gate ever compiles it, so the E0061 was invisible locally and would have
    // failed CI's Linux leg. It is `JAIL_VERSION` rather than a local literal so it cannot drift
    // from the policy the arm actually launches.
    //
    // ⛔ ASK THE GATE THE POLICY ASKS. This read `build_jail_net_allowed`, the v1 table, while
    // `compile_build_jail` decides through `build_jail_net_allowed_for` — v2 plus the baseline. The
    // two agree for `node-gyp`, so the guard happened to hold, but it was pinning a different
    // oracle than the one under test and a v2-only change would have slipped straight past it.
    assert!(
        nub_sandbox::build_jail_net_allowed_for(Some(GRANTED), Some(JAIL_VERSION)),
        "fixture `{GRANTED}` is no longer granted egress by the catalog, so the granted arm \
         cannot pass — pick a catalogued package rather than granting this one network"
    );

    let (code, stdout) = run(Some(GRANTED));
    assert_eq!(
        code,
        Some(0),
        "a catalogued package must reach the listener; 10 means the socket ceiling still \
         refuses it, which is the defect this pins:\n{stdout}"
    );

    // REFUSED. `@bufbuild/buf` IS named by the catalog, and its entry carries no `network` field —
    // which is how the catalog spells a withheld grant, and the ONLY remaining shape that compiles
    // to no Allow rule and so installs the socket ceiling.
    //
    // ASSERTED, NOT ASSUMED, for the same reason the granted arm is: a catalog edit that added
    // `network` to this entry would turn the refusal into a silent false pass, so check the gate
    // directly and fail HERE naming the cause.
    const REFUSED: &str = "@bufbuild/buf";
    assert!(
        !nub_sandbox::build_jail_net_allowed_for(Some(REFUSED), Some(JAIL_VERSION)),
        "fixture `{REFUSED}` is now GRANTED egress by the catalog, so the refused arm cannot pass \
         — pick an entry that still withholds `network` rather than relaxing this assertion"
    );
    let (code, stdout) = run(Some(REFUSED));
    assert_eq!(
        code,
        Some(10),
        "an entry that withholds `network` must be refused AT SOCKET CREATION; 0 means the grant \
         leaked to every package and 11 means it was denied for the wrong reason:\n{stdout}"
    );

    // THE BASELINE, both spellings of it. `left-pad` has no entry; `None` is what aube withholds
    // when the spawn root is a checkout it FETCHED. Both resolve to `baseline_caps()`, which grants
    // network — see the note above for why that is the contract and not a leak.
    for uncatalogued in [Some("left-pad"), None] {
        let (code, stdout) = run(uncatalogued);
        assert_eq!(
            code,
            Some(0),
            "{uncatalogued:?} has no catalog entry, so it takes the BASELINE grant and must reach \
             the listener; 10 means the baseline stopped granting network:\n{stdout}"
        );
    }

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

/// A fixture whose engine cache ROOT exists while the tool cache below it does NOT — the fresh
/// machine both tests below are about. [`jail`] deliberately leaves `.cache` absent, which
/// switches off every compile-time side effect gated on it (the private home included), so a
/// test about those side effects cannot reuse it.
fn jail_with_engine_cache() -> (tempfile::TempDir, Jail) {
    let root = tempfile::tempdir().expect("temp root");
    let home = root.path().join("home");
    let project = home.join("proj");
    let package_dir = project.join("node_modules/dep");
    std::fs::create_dir_all(&package_dir).unwrap();
    std::fs::create_dir_all(home.join(".cache")).unwrap();
    std::fs::write(package_dir.join("own.txt"), "PACKAGE_OWN_FILE").unwrap();
    std::fs::write(home.join("secret.txt"), "HOME_SECRET_TOKEN").unwrap();
    (
        root,
        Jail {
            home,
            project,
            package_dir,
        },
    )
}

/// The `Homes` the jail compiles against, rebuilt so a test can ask the same questions
/// `compile_build_jail` answers. Kept beside [`Jail::policy_for`]'s literal rather than shared
/// with it only because that method returns a policy, not the homes it used.
fn homes_of(jail: &Jail) -> nub_sandbox::Homes {
    nub_sandbox::Homes {
        home: jail.home.clone(),
        tmp: std::env::temp_dir(),
        cache: jail.home.join(".cache"),
        project: jail.project.clone(),
    }
}

/// ⛔ THE THREE `$cache/nub/pm/tools` REDIRECT TARGETS ARE WRITABLE BY A CONFINED CHILD ON A
/// MACHINE THAT NEVER RAN AN UNJAILED INSTALL — AND `tools` ITSELF IS STILL NOT.
///
/// nub NAMES these three at the package through `npm_config_prefix`,
/// `PLAYWRIGHT_BROWSERS_PATH` and `electron_config_cache`, so a package that finds no grant on
/// one has to `mkdir` it against a parent that is read-only ON PURPOSE. `push_rw_path` stamps
/// `FsOrigin::Speculative` and `compile_mount_plan` DROPS such a rule when its path is absent —
/// Landlock cannot attach a rule to a path `open(O_PATH)` cannot answer — so before
/// `preset::materialize_tool_leaf` the leaf carried no grant at all.
///
/// WHY THIS TEST AND NOT THE UNIT ONE NEXT DOOR. `preset.rs`'s guard asks the compiled matcher
/// whether a write is allowed, which is a statement about nub's own model. Only a real kernel
/// can say whether the rule ATTACHED, and attachment is the half that was broken: the model
/// said `Allow` throughout, and the backend threw the rule away.
///
/// THE `tools` HALF IS THE SAME FIX, which is why it is one test. The repair a reviewer must
/// never accept is widening the parent — `tools` also holds the node-gyp bootstraps nub runs on
/// every later install, so a write grant spanning it lets one package's lifecycle script replace
/// a binary every subsequent install then executes. Asserting the leaves are writable without
/// pinning the parent denied would leave that wrong repair passing.
///
/// NON-VACUOUSNESS. The same run reads the package's own file back and fails to read the home
/// secret, so a ruleset that restricted nothing — or a child that never ran — cannot satisfy it.
#[test]
fn the_confined_child_writes_the_tool_redirect_leaves_and_still_cannot_write_tools() {
    if skip_without_landlock() {
        return;
    }
    let (_root, jail) = jail_with_engine_cache();
    let tools = jail.home.join(".cache/nub/pm/tools");
    assert!(
        !tools.exists(),
        "the fixture must start with NO tool cache — on a host that already ran an unjailed \
         install the leaves are already there and this test passes with the fix reverted"
    );

    let script = format!(
        r#"
        cat own.txt 2>/dev/null || echo OWN_READ_DENIED
        cat '{secret}' 2>/dev/null && echo SECRET_LEAK || echo SECRET_HIDDEN
        for leaf in npm-prefix ms-playwright electron-cache; do
          echo payload > '{tools}'/$leaf/downloaded-artifact 2>/dev/null \
            && echo "WRITE_OK $leaf" || echo "WRITE_DENIED $leaf"
          mkdir -p '{tools}'/$leaf/nested/deeper 2>/dev/null \
            && echo "MKDIR_OK $leaf" || echo "MKDIR_DENIED $leaf"
        done
        echo evil > '{tools}'/planted.bin 2>/dev/null && echo TOOLS_FILE_WROTE || echo TOOLS_FILE_BLOCKED
        mkdir '{tools}'/planted-dir 2>/dev/null && echo TOOLS_MKDIR_MADE || echo TOOLS_MKDIR_BLOCKED
        "#,
        secret = jail.home.join("secret.txt").display(),
        tools = tools.display(),
    );
    let out = jail.launch(jail.spec("/bin/sh").arg("-c").arg(&script));
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("PACKAGE_OWN_FILE"),
        "the confined child never read its own package dir, so every denial below would prove \
         only that it did not run:\n{stdout}"
    );
    assert!(
        !stdout.contains("HOME_SECRET_TOKEN") && stdout.contains("SECRET_HIDDEN"),
        "the jail stopped withholding the home secret, so it is restricting nothing and the \
         write results below say nothing about a grant:\n{stdout}"
    );

    for leaf in ["npm-prefix", "ms-playwright", "electron-cache"] {
        assert!(
            tools.join(leaf).is_dir(),
            "the compile did not materialize {leaf}, so its write grant was dropped as absent"
        );
        assert!(
            stdout.contains(&format!("WRITE_OK {leaf}")),
            "the confined child could not create a file in {leaf} — the grant nub hands it \
             through its own redirect env var did not reach the kernel:\n{stdout}"
        );
        assert!(
            stdout.contains(&format!("MKDIR_OK {leaf}")),
            "the confined child could not create a subdirectory in {leaf}; a browser or \
             Electron download lands in a tree, not one file:\n{stdout}"
        );
        assert!(
            tools.join(leaf).join("downloaded-artifact").is_file(),
            "{leaf} reported a successful write whose bytes are not on disk"
        );
    }

    assert!(
        stdout.contains("TOOLS_FILE_BLOCKED") && stdout.contains("TOOLS_MKDIR_BLOCKED"),
        "`tools` itself became writable — it holds the node-gyp bootstraps nub executes on every \
         later install, so a write grant spanning it is a persistence channel:\n{stdout}"
    );
    assert!(
        !tools.join("planted.bin").exists() && !tools.join("planted-dir").exists(),
        "the child planted something directly in `tools` despite reporting a denial"
    );
}

/// ⛔ A CONFINED LIFECYCLE SCRIPT REALLY CAN PLANT A SYMLINK, which is the precondition the
/// promotion floor in `nub-cli`'s `pm_engine::build_jail` exists for.
///
/// `ACCESS_MAKE_SYM` is in the Landlock read-write mask (`backend/linux_landlock.rs`), so
/// dropping a link under a granted prefix — the package's own directory, or the private `$HOME`
/// the jail hands it — is INSIDE the jail's own grant rather than an escape from it. Promotion
/// then runs UNCONFINED and moves declared subpaths out of that private home into the real one,
/// so a mover that renamed or descended a link would install a package-aimed pointer into a home
/// only the user should write.
///
/// Pinned HERE rather than assumed by the mover's unit tests, which build their fixtures with
/// `std::os::unix::fs::symlink` from the test process. That proves the mover's behaviour given a
/// link; it cannot say whether a confined script can produce one. This is the other half.
///
/// NON-VACUOUSNESS: the nested `mkdir` and the ordinary package-dir link are shown succeeding in
/// the same run, and each reported link is re-checked from the parent with `symlink_metadata`, so
/// a shell that printed `_OK` without a link on disk cannot pass.
#[test]
fn a_confined_script_can_plant_a_symlink_under_a_granted_prefix() {
    if skip_without_landlock() {
        return;
    }
    let (_root, jail) = jail_with_engine_cache();
    let private = nub_sandbox::jail_private_home(&homes_of(&jail), &jail.package_dir)
        .expect("the jail hands the package a private home");

    let script = format!(
        r#"
        cat own.txt 2>/dev/null || echo OWN_READ_DENIED
        mkdir -p '{private}'/.cache/nested/deeper 2>/dev/null && echo MKDIR_OK || echo MKDIR_DENIED
        ln -s '{aim}' '{private}'/.cache/evil-link 2>/dev/null && echo LINK_OK || echo LINK_DENIED
        ln -s '{aim}' '{private}'/.cache/nested/deeper/evil-link 2>/dev/null \
          && echo NESTED_LINK_OK || echo NESTED_LINK_DENIED
        ln -s '{aim}' pkgdir-link 2>/dev/null && echo PKGDIR_LINK_OK || echo PKGDIR_LINK_DENIED
        "#,
        private = private.display(),
        aim = jail.home.join("secret.txt").display(),
    );
    let out = jail.launch(jail.spec("/bin/sh").arg("-c").arg(&script));
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("PACKAGE_OWN_FILE"),
        "the confined child never ran:\n{stdout}"
    );
    for marker in ["MKDIR_OK", "LINK_OK", "NESTED_LINK_OK", "PKGDIR_LINK_OK"] {
        assert!(
            stdout.contains(marker),
            "{marker} missing — if a confined script cannot create a symlink at all the \
             promotion floor is guarding nothing, and this test is the place that says so:\n\
             {stdout}"
        );
    }
    for link in [
        private.join(".cache/evil-link"),
        private.join(".cache/nested/deeper/evil-link"),
        jail.package_dir.join("pkgdir-link"),
    ] {
        assert!(
            std::fs::symlink_metadata(&link)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false),
            "{} was reported created but is not a symlink on disk",
            link.display()
        );
    }
}

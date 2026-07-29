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
            None,
            Vec::new(),
            Vec::new(),
            ambient,
        )
        .expect("compile build-jail")
    }

    fn launch(&self, spec: nub_sandbox::CommandSpec) -> std::process::Output {
        // `.expect` rather than a skip: an apply error is the jail failing to confine,
        // which must never read as a denial.
        nub_sandbox::apply_with_runtime(&self.policy(), spec, runtime())
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

//! Why `@prisma/client`'s curated grant does not work on Linux, pinned per STAGE.
//!
//! The grant carries three needs and each masks the next, so a single pass says only
//! "still broken". A differential run on Linux returned `DIFFERS` identically with AND
//! without the grant, which means at least one stage is denied for a reason the grant
//! cannot reach. This file isolates which one.
//!
//! # The differential is the NAME, not a patched binary
//!
//! `compile_build_jail`'s `package_name` argument IS the arm selector: `Some(name)` looks
//! the package up in the curated table, `None` grants no exception at all. Both arms
//! therefore run identical code over identical fixtures with one variable changed, and
//! neither needs a cargo feature or an env seam.
//!
//! # THE FINDING: a Landlock rule cannot be attached to a path that does not exist
//!
//! `landlock_add_rule` takes an `O_PATH` descriptor, so `linux_landlock::add_rule` returns
//! `Ok(false)` — rule silently not added — for an absent path. `sibling_dirs` names
//! `.prisma`, which is exactly a directory the postinstall CREATES, so on Linux the grant
//! for it is dropped at launch and the `mkdir` is denied.
//!
//! It cannot be fixed by adding a right either, and that is the part worth stating: on
//! Linux the right to create an entry (`LANDLOCK_ACCESS_FS_MAKE_DIR`) lives on the PARENT
//! directory, and the parent here is the package's own enclosing `node_modules` — which
//! `curated.rs` deliberately never grants, because it holds `.bin` (executed unconfined)
//! and the virtual store (every dependency's source before it runs). macOS does not have
//! this problem: Seatbelt matches on path PATTERNS, so a rule for a not-yet-existent
//! `.prisma` is installed and works, and there is no separate parent-scoped create right.
//!
//! So the same catalog entry is correct on macOS and structurally unsatisfiable on Linux
//! under any grant that keeps the enclosing `node_modules` ungranted. `MISSING_DIR_STAGE`
//! pins that, and `PRECREATED_DIR_STAGE` is its control: with the directory already
//! present the very same grant works, which is what proves the failure is the absent-path
//! rule drop rather than a missing access right.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Landlock is the build jail's only Linux mechanism, so an unusable ABI makes every
/// assertion here vacuous rather than merely unrun.
fn skip_without_landlock() -> bool {
    if nub_sandbox::host_probe::landlock_abi().is_some() {
        return false;
    }
    eprintln!("skipping linux_curated_sibling_dir: kernel has no usable Landlock (needs 5.13+)");
    true
}

fn runtime() -> &'static nub_sandbox::RuntimeCapability {
    static RUNTIME: std::sync::OnceLock<nub_sandbox::RuntimeCapability> =
        std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| nub_sandbox::earliest_bootstrap().expect("earliest bootstrap"))
}

/// The HOISTED layout, deliberately: `@prisma/client`'s postinstall reaches its sibling as
/// `path.join(__dirname, '../../../.prisma')`, which from
/// `<nm>/@prisma/client/scripts` resolves to `<nm>` — and `enclosing_node_modules` lands on
/// the same directory. Under the isolated layout both move to the store cell's
/// `node_modules` together, so the arithmetic this pins is layout-independent.
struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    package_dir: PathBuf,
    enclosing_nm: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temp root");
        let home = root.path().join("home");
        let project = home.join("proj");
        let enclosing_nm = project.join("node_modules");
        let package_dir = enclosing_nm.join("@prisma/client");
        std::fs::create_dir_all(package_dir.join("scripts")).unwrap();
        std::fs::create_dir_all(project.join("prisma")).unwrap();
        std::fs::write(project.join("package.json"), "{}").unwrap();
        // The schema stands in for the codegen INPUT the third stage reads. A marker rather
        // than a real schema: this file asserts REACHABILITY per stage, and running the real
        // generator would make the result depend on prisma's engine download too.
        std::fs::write(
            project.join("prisma/schema.prisma"),
            "model NubJailProbeMarker {}\n",
        )
        .unwrap();
        std::fs::write(package_dir.join("own.txt"), "PACKAGE_OWN_FILE").unwrap();
        std::fs::write(home.join("secret.txt"), "HOME_SECRET_TOKEN").unwrap();
        Self {
            _root: root,
            home,
            project,
            package_dir,
            enclosing_nm,
        }
    }

    /// `granted` is the whole differential: it chooses whether the curated table is
    /// consulted for this spawn.
    fn policy(&self, granted: bool) -> nub_sandbox::policy::SandboxPolicy {
        let ambient: BTreeMap<String, String> = [
            ("PATH", "/usr/bin:/bin"),
            ("INIT_CWD", &self.project.to_string_lossy()),
        ]
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
            granted.then_some("@prisma/client"),
            Vec::new(),
            Vec::new(),
            ambient,
        )
        .expect("compile build-jail")
    }

    fn run(&self, granted: bool, script: &str) -> std::process::Output {
        let spec = nub_sandbox::CommandSpec::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .cwd(&self.package_dir);
        // `.expect` rather than a skip: an apply error is the jail failing to confine, and
        // must never be read as the child being denied.
        nub_sandbox::apply_with_runtime(&self.policy(granted), spec, runtime())
            .expect("apply build-jail (fail-closed on error)")
            .output()
            .expect("run the confined child")
    }

    /// Did the probe's own success marker reach stdout? Deliberately not the exit code:
    /// `sh -c` reports the last command, and the whole reason this catalog exists is that
    /// an exit status is not evidence about what a script actually managed to do.
    fn reaches(&self, granted: bool, script: &str, marker: &str) -> bool {
        let out = self.run(granted, &format!("{script} && echo {marker}"));
        String::from_utf8_lossy(&out.stdout).contains(marker)
    }
}

fn q(path: &Path) -> String {
    format!("'{}'", path.display())
}

/// THE FINDING. `mkdir` of the granted sibling fails in BOTH arms, so the grant buys
/// nothing on Linux — which is exactly the `DIFFERS`-either-way result that put this defect
/// in the catalog, reproduced here at the stage that causes it.
#[test]
fn a_sibling_dir_grant_cannot_create_a_missing_directory_on_linux() {
    if skip_without_landlock() {
        return;
    }
    let fx = Fixture::new();
    let mkdir = format!("mkdir {}", q(&fx.enclosing_nm.join(".prisma")));

    assert!(
        !fx.reaches(false, &mkdir, "MADE"),
        "ungranted must not create the sibling — if this passes the jail is not confining, \
         and every other assertion here is vacuous"
    );
    assert!(
        !fx.reaches(true, &mkdir, "MADE"),
        "REGRESSION IN THE GOOD DIRECTION: the sibling grant now creates a MISSING \
         directory on Linux. If Landlock gained a way to attach a rule to an absent path, \
         or the grant now covers the parent's MAKE_DIR right, then @prisma/client's grant \
         may work here after all — re-run the real postinstall differential and re-scope the \
         catalog entry's `platform` field"
    );
}

/// THE CONTROL, and the reason the finding above is a rule-drop rather than a missing
/// right: with `.prisma` already on disk the SAME grant makes it writable, and the
/// ungranted arm still cannot touch it. Without this pair, "both arms fail" would be
/// equally consistent with the sibling grant being broken outright.
#[test]
fn the_same_sibling_grant_works_once_the_directory_exists() {
    if skip_without_landlock() {
        return;
    }
    let fx = Fixture::new();
    std::fs::create_dir_all(fx.enclosing_nm.join(".prisma")).unwrap();
    let write = format!("touch {}", q(&fx.enclosing_nm.join(".prisma/client.js")));

    assert!(
        fx.reaches(true, &write, "WROTE"),
        "the sibling grant must make an EXISTING .prisma writable — if this fails the grant \
         is broken for a second, independent reason"
    );
    assert!(
        !fx.reaches(false, &write, "WROTE"),
        "ungranted must not write it; without this the assertion above could pass on a jail \
         that grants the whole node_modules"
    );
}

/// Stage 2 of the three, isolated. `project_cwd` is what `process.chdir(INIT_CWD)` needs,
/// and it is measured here because "the grant does not work on Linux" had to be narrowed to
/// a specific stage rather than left as a property of the whole entry.
#[test]
fn the_project_cwd_grant_works_on_linux() {
    if skip_without_landlock() {
        return;
    }
    let fx = Fixture::new();
    let chdir = format!("cd {} && pwd", q(&fx.project));

    assert!(
        fx.reaches(true, &chdir, "CHDIR"),
        "project_cwd must let the child chdir into the project and resolve its cwd"
    );
    assert!(
        !fx.reaches(false, &chdir, "CHDIR"),
        "ungranted must not reach the project root node"
    );
}

/// Stage 3, isolated: the codegen input `prisma generate` reads.
#[test]
fn the_project_reads_grant_works_on_linux() {
    if skip_without_landlock() {
        return;
    }
    let fx = Fixture::new();
    let read = format!("cat {}", q(&fx.project.join("prisma/schema.prisma")));

    assert!(
        fx.reaches(true, &read, "READ"),
        "projectReads must make the schema directory readable"
    );
    assert!(
        !fx.reaches(false, &read, "READ"),
        "ungranted must not read the consumer's schema"
    );
}

/// The jail is still a jail in both arms. A curated grant is a NARROW addition, so if it
/// ever came with the home secret attached, every per-stage result above would be measuring
/// a hole rather than a grant.
#[test]
fn neither_arm_leaks_the_home_secret_or_loses_the_package_dir() {
    if skip_without_landlock() {
        return;
    }
    let fx = Fixture::new();
    for granted in [false, true] {
        let out = fx.run(
            granted,
            &format!(
                "cat {} 2>/dev/null; cat own.txt",
                q(&fx.home.join("secret.txt"))
            ),
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout.contains("HOME_SECRET_TOKEN"),
            "granted={granted}: the home secret must stay unreadable:\n{stdout}"
        );
        assert!(
            stdout.contains("PACKAGE_OWN_FILE"),
            "granted={granted}: the child must still read its own package dir, or the \
             denials above prove only that it never ran:\n{stdout}"
        );
    }
}

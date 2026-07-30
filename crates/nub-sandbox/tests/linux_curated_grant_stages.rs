//! The curated project grant, measured on Linux against a real kernel.
//!
//! # The arm selector is `package_name`, not a patched binary
//!
//! `compile_build_jail`'s `package_name` IS the differential: `Some(name)` consults the
//! curated table, `None` grants no exception. Both arms run identical code over identical
//! fixtures with one variable changed.
//!
//! # EVERY ARM GETS ITS OWN TARGET PATH, and that is load-bearing
//!
//! The first version of this file reused one path across arms and ran the GRANTED arm first.
//! Its second assertion then measured `touch` against a file the first assertion had already
//! created — and `touch` on an EXISTING file is not a Landlock-handled access, so the
//! ungranted control "passed" the write and the test reported a hole that does not exist.
//! A per-arm path removes the ordering dependency entirely rather than fixing it by
//! reordering, which would have left the same trap for the next reader.
//!
//! # What the schema migration changed here, and why the file shrank
//!
//! This file used to measure four SEPARATE grant fields one stage at a time — `siblingDirs`,
//! `projectCwd`, `projectReads`, `projectWrites.literal` — because `@prisma/client`'s entry
//! carried three needs and each masked the next. Two of those measurements are why the settled
//! schema has one field instead of six, and both are recorded rather than lost:
//!
//! 1. **A Landlock rule cannot be attached to an ABSENT path.** `landlock_add_rule` takes an
//!    `O_PATH` descriptor, so a `siblingDirs` grant for a directory the package was about to
//!    CREATE evaporated at launch and `mkdir` was denied identically with and without it. It
//!    needed a `create_dir_all` side effect during policy compilation to work at all, and the
//!    same defect then reappeared on `projectWrites.literal` when `.git/hooks` was missing.
//!    Granting the project root instead dissolves it: the root always exists, so the rule
//!    always attaches and the script creates its own children.
//! 2. **`chdir` is not a Landlock-handled access at all**, so `projectCwd` was a measured
//!    NO-OP on Linux while being load-bearing on macOS Seatbelt (`EPERM … uv_cwd`). That
//!    asymmetry is how the Prisma entry shipped broken after being measured only on macOS. A
//!    tree grant subsumes it on both backends.
//!
//! What remains to measure is the one contract that is left: a listed package reaches the
//! consumer's project tree, an unlisted one does not, and neither reaches anything else.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn skip_without_landlock() -> bool {
    if nub_sandbox::host_probe::landlock_abi().is_some() {
        return false;
    }
    eprintln!("skipping linux_curated_grant_stages: kernel has no usable Landlock (needs 5.13+)");
    true
}

fn runtime() -> &'static nub_sandbox::RuntimeCapability {
    static RUNTIME: std::sync::OnceLock<nub_sandbox::RuntimeCapability> =
        std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| nub_sandbox::earliest_bootstrap().expect("earliest bootstrap"))
}

/// HOISTED layout deliberately: it is the one where a package's own `../..` arithmetic reaches
/// the project, so a grant that works here is not relying on the isolated layout's store cell
/// to contain the damage.
struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    package_dir: PathBuf,
    enclosing_nm: PathBuf,
}

impl Fixture {
    fn new(package: &str) -> Self {
        let root = tempfile::tempdir().expect("temp root");
        let home = root.path().join("home");
        let project = home.join("proj");
        let enclosing_nm = project.join("node_modules");
        let package_dir = enclosing_nm.join(package);
        std::fs::create_dir_all(package_dir.join("scripts")).unwrap();
        std::fs::create_dir_all(project.join("prisma")).unwrap();
        std::fs::create_dir_all(home.join(".cache")).unwrap();
        std::fs::write(project.join("package.json"), "{}").unwrap();
        std::fs::write(project.join("prisma/schema.prisma"), "model Probe {}\n").unwrap();
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

    fn policy(&self, package: &str, granted: bool) -> nub_sandbox::policy::SandboxPolicy {
        let ambient: BTreeMap<String, String> = [
            ("PATH", "/usr/bin:/bin".to_string()),
            ("INIT_CWD", self.project.to_string_lossy().into_owned()),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect();
        nub_sandbox::compile_build_jail(
            nub_sandbox::Homes {
                home: self.home.clone(),
                tmp: std::env::temp_dir(),
                cache: self.home.join(".cache"),
                project: self.project.clone(),
            },
            &self.package_dir,
            granted.then_some(package),
            Vec::new(),
            Vec::new(),
            ambient,
        )
        .expect("compile build-jail")
    }

    /// Did the probe's own marker reach stdout? Never the exit code — `sh -c` reports only
    /// its last command, and this whole catalog exists because an exit status is not
    /// evidence about what a script managed to do.
    fn reaches(&self, package: &str, granted: bool, script: &str) -> bool {
        let spec = nub_sandbox::CommandSpec::new("/bin/sh")
            .arg("-c")
            .arg(format!("{script} && echo REACHED"))
            .cwd(&self.package_dir);
        // `.expect`, not a skip: an apply error is the jail failing to confine and must
        // never read as the child being denied.
        let out = nub_sandbox::apply_with_runtime(&self.policy(package, granted), spec, runtime())
            .expect("apply build-jail (fail-closed on error)")
            .output()
            .expect("run the confined child");
        String::from_utf8_lossy(&out.stdout).contains("REACHED")
    }
}

fn q(path: &Path) -> String {
    format!("'{}'", path.display())
}

/// The grant reaches a path that DID NOT EXIST, which is the case the enumerated-path schema
/// could not serve on Linux at all: `landlock_add_rule` takes an `O_PATH` descriptor, so a rule
/// naming an absent directory could not be attached and the grant evaporated at launch.
///
/// Each arm creates a DIFFERENTLY-NAMED child, so neither can be reading the other's work.
#[test]
fn the_project_grant_creates_a_directory_that_did_not_exist() {
    if skip_without_landlock() {
        return;
    }
    let fx = Fixture::new("@prisma/client");
    let target = fx.enclosing_nm.join(".prisma");
    assert!(
        !target.exists(),
        "the fixture must start WITHOUT .prisma — that absence is the condition under test"
    );

    assert!(
        fx.reaches(
            "@prisma/client",
            true,
            &format!(
                "mkdir -p {} && echo g > {}",
                q(&target),
                q(&target.join("g.js"))
            ),
        ),
        "the project grant must reach a .prisma that did not exist. Under the enumerated-path \
         schema this needed a create_dir_all side effect during policy compilation; granting the \
         project root removes the absent-path problem instead of working around it"
    );

    let fx2 = Fixture::new("@prisma/client");
    let target2 = fx2.enclosing_nm.join(".prisma");
    assert!(
        !fx2.reaches(
            "@prisma/client",
            false,
            &format!(
                "mkdir -p {} && echo u > {}",
                q(&target2),
                q(&target2.join("u.js"))
            ),
        ),
        "ungranted must NOT reach it. Fresh fixture and a differently-named file, so this \
         cannot be passing on the granted arm's leftovers — the trap that made an earlier \
         version of this test report a hole that was not there"
    );
}

/// The consumer's project tree, read and write, for a listed package only. Read is the codegen
/// INPUT (`prisma/schema.prisma`); write is the hook file a hook installer exists to place —
/// and `.git/hooks` is measured in BOTH the present and ABSENT states, because the absent one
/// is what the `projectWrites.literal` schema could not do (`git-commit-msg-linter` mkdirs it
/// when missing) and was pinned here as a known limitation.
#[test]
fn a_listed_package_reads_and_writes_the_project_tree() {
    if skip_without_landlock() {
        return;
    }
    let fx = Fixture::new("@prisma/client");
    let read = format!("cat {}", q(&fx.project.join("prisma/schema.prisma")));
    assert!(fx.reaches("@prisma/client", true, &read));
    assert!(
        !fx.reaches("@prisma/client", false, &read),
        "ungranted must not read the consumer's schema"
    );

    let present = Fixture::new("ghooks");
    std::fs::create_dir_all(present.project.join(".git/hooks")).unwrap();
    assert!(
        present.reaches(
            "ghooks",
            true,
            &format!(
                "echo '#!/bin/sh' > {}",
                q(&present.project.join(".git/hooks/pre-commit"))
            )
        ),
        "the project write grant must reach an EXISTING .git/hooks — the case every real \
         checkout presents, and what all nine hook-installer grants depend on"
    );

    let absent = Fixture::new("ghooks");
    std::fs::create_dir_all(absent.project.join(".git")).unwrap();
    assert!(
        absent.reaches(
            "ghooks",
            true,
            &format!("mkdir -p {}", q(&absent.project.join(".git/hooks")))
        ),
        "and an ABSENT one, which the enumerated `literal` grant could NOT: a rule for a path \
         that does not exist cannot be attached to Landlock, so the grant evaporated. If this \
         fails, the project-root grant has lost MAKE_DIR and the old limitation is back"
    );

    let ungranted = Fixture::new("ghooks");
    std::fs::create_dir_all(ungranted.project.join(".git/hooks")).unwrap();
    assert!(
        !ungranted.reaches(
            "ghooks",
            false,
            &format!(
                "echo x > {}",
                q(&ungranted.project.join(".git/hooks/pre-commit"))
            )
        ),
        "ungranted must not write the consumer's git hooks — the control that makes the grant \
         above meaningful, and a git hook is code that runs on the next commit"
    );
}

/// The grant is the PROJECT, not the machine. `node_modules` being writable for a listed
/// package is deliberate (it is what replaced the enumerated sibling names), so the boundary
/// that has to hold is the project root itself — and the home secret on both arms, since a
/// curated grant that arrived with it attached would make every result above a hole rather
/// than a grant.
#[test]
fn the_grant_stops_at_the_project_root_and_never_reaches_the_home_secret() {
    if skip_without_landlock() {
        return;
    }
    let fx = Fixture::new("@prisma/client");
    for granted in [false, true] {
        assert!(
            !fx.reaches(
                "@prisma/client",
                granted,
                &format!("cat {}", q(&fx.home.join("secret.txt")))
            ),
            "granted={granted}: the home secret must stay unreadable"
        );
        assert!(
            !fx.reaches(
                "@prisma/client",
                granted,
                &format!("touch {}", q(&fx.home.join("outside.txt")))
            ),
            "granted={granted}: a path OUTSIDE the project must stay unwritable"
        );
        assert!(
            fx.reaches("@prisma/client", granted, "cat own.txt"),
            "granted={granted}: the child must still read its own package dir, or every \
             denial above proves only that it never ran"
        );
    }
    // The positive half of the same boundary, asserted with its control: inside the project is
    // writable for the listed package and not otherwise. Distinct fixtures so the ungranted arm
    // cannot pass on a file the granted arm created (`touch` on an existing file is not a
    // Landlock-handled access — the trap this file's header records).
    let inside = Fixture::new("@prisma/client");
    assert!(inside.reaches(
        "@prisma/client",
        true,
        &format!("touch {}", q(&inside.project.join("granted.txt")))
    ));
    let control = Fixture::new("@prisma/client");
    assert!(
        !control.reaches(
            "@prisma/client",
            false,
            &format!("touch {}", q(&control.project.join("ungranted.txt")))
        ),
        "the project root must stay unwritable without the grant"
    );
}

//! The curated grants, measured on Linux ONE STAGE AT A TIME.
//!
//! `@prisma/client`'s entry carries three needs and each masks the next, so the differential
//! that put it in `knownDefects` — `DIFFERS` identically with and without the grant — said
//! only "still broken". These tests say which stage, and they cover the other grant shapes
//! that share the same mechanism.
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
//! # What was measured (6.17.0-1021-gcp, Landlock ABI 7)
//!
//! | stage | field | ungranted | granted |
//! | --- | --- | --- | --- |
//! | create an ABSENT sibling dir | `siblingDirs` | denied | **was denied — the defect** |
//! | write an EXISTING sibling dir | `siblingDirs` | denied | allowed |
//! | `chdir` into the project | `projectCwd` | **allowed** | allowed |
//! | read the project subtree | `projectReads` | denied | allowed |
//!
//! Two findings, and neither was visible from the macOS measurement:
//!
//! 1. **A Landlock rule cannot be attached to an absent path**, so a `sibling_dirs` grant
//!    for a directory the package is about to create evaporated at launch. Fixed by
//!    `curated::materialize_sibling`, which creates it during compilation; the argument for
//!    why that is not a widening is at that function.
//! 2. **`chdir` is not a Landlock-handled access at all**, so `projectCwd` is a no-op on
//!    Linux — the operation it exists to permit was never denied here. It stays because
//!    macOS Seatbelt DOES gate it (`uv_cwd` was the measured failure there). A grant that is
//!    load-bearing on one backend and inert on another is worth stating rather than
//!    discovering twice.
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

/// HOISTED layout deliberately: `@prisma/client`'s postinstall reaches its sibling as
/// `path.join(__dirname, '../../../.prisma')`, which from `<nm>/@prisma/client/scripts`
/// resolves to `<nm>` — the same directory `enclosing_node_modules` lands on. Under the
/// isolated layout both move to the store cell's `node_modules` together.
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

/// THE DEFECT, and its fix. Creating the sibling directory is the FIRST of the three staged
/// needs, so before this worked nothing behind it could be measured at all.
///
/// Each arm creates a DIFFERENTLY-NAMED child, so neither can be reading the other's work.
#[test]
fn a_sibling_grant_creates_a_directory_that_did_not_exist() {
    if skip_without_landlock() {
        return;
    }
    let fx = Fixture::new("@prisma/client");
    let target = fx.enclosing_nm.join(".prisma");
    assert!(
        !target.exists(),
        "the fixture must start WITHOUT .prisma — that absence is the condition under test"
    );

    let granted = fx.reaches(
        "@prisma/client",
        true,
        &format!(
            "mkdir -p {} && echo g > {}",
            q(&target),
            q(&target.join("g.js"))
        ),
    );
    assert!(
        granted,
        "the sibling grant must reach a .prisma that did not exist. Landlock cannot attach a \
         rule to an absent path, so this needs curated::materialize_sibling to have created \
         it during compilation"
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

/// `chdir` is NOT a Landlock-handled access, so the ungranted arm reaches the project root
/// too. Asserted rather than left implicit: it means `projectCwd` buys nothing on Linux, and
/// a future reader comparing the backends needs that recorded — the macOS measurement's
/// `EPERM … uv_cwd` has no Linux counterpart.
#[test]
fn project_cwd_is_inert_on_linux_because_chdir_is_ungoverned() {
    if skip_without_landlock() {
        return;
    }
    let fx = Fixture::new("@prisma/client");
    let chdir = format!("cd {} && pwd", q(&fx.project));

    assert!(fx.reaches("@prisma/client", true, &chdir));
    assert!(
        fx.reaches("@prisma/client", false, &chdir),
        "if chdir is denied ungranted, Landlock gained a traverse right and projectCwd is \
         load-bearing on Linux after all — re-scope the catalog note"
    );
    // The pair that keeps the line above from reading as "the jail does nothing": the same
    // ungranted arm cannot READ the directory it just entered.
    assert!(
        !fx.reaches(
            "@prisma/client",
            false,
            &format!("cat {}", q(&fx.project.join("prisma/schema.prisma")))
        ),
        "entering the project must not imply reading it"
    );
}

/// Stage 3, isolated: the codegen INPUT `prisma generate` reads.
#[test]
fn project_reads_grants_the_schema_subtree() {
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
}

/// IGNORED, AND IT NEVER PASSED — this measures a grant shape the tree does not have.
///
/// It was written against `projectWrites: {"literal": [".git/hooks"]}`, and at the commit
/// that added it (`8866e5fed9`) `ProjectWrites` had exactly two variants, `None` and
/// `ManifestField`; `data/README.md` still lists a literal project write under "Known
/// gaps". The catalog there holds three entries — `@prisma/client`,
/// `@danmarshall/deckgl-typings`, `msw` — and no `ghooks`. `grant_from_table` returns early
/// on a lookup miss, so the GRANTED arm compiles a policy identical to the ungranted one:
/// the positive assertion below fails deterministically and the two negative ones pass
/// vacuously. Nothing about Landlock is involved.
///
/// The eleven literal-write entries it was written for — the nine `.git/hooks` installers
/// plus `@cypress/snapshot` and `@nativescript/core` — exist on `161570f2f9`
/// (`sandbox/jail-catalog-v2`), which is unlanded. Three catalog schemas are in contention
/// there: that one, today's `manifestField`-only, and `catalog-migrate`'s (`9823d56a27`)
/// coarse per-package `{"project": "readwrite"}` over ~160 packages. Choosing between them
/// decides how the catalog expresses a grant at all, so un-ignoring this belongs to that
/// decision rather than to whoever next reads the failure.
#[test]
#[ignore = "measures a ProjectWrites::Literal shape this tree does not implement"]
fn a_literal_project_write_reaches_an_existing_dir_and_not_an_absent_one() {
    if skip_without_landlock() {
        return;
    }
    let present = Fixture::new("ghooks");
    std::fs::create_dir_all(present.project.join(".git/hooks")).unwrap();
    let hook = present.project.join(".git/hooks/pre-commit");
    assert!(
        present.reaches("ghooks", true, &format!("echo '#!/bin/sh' > {}", q(&hook))),
        "the literal write grant must reach an EXISTING .git/hooks — this is the case every \
         real checkout presents. THIS IS THE ASSERTION THAT FAILS while the entry is absent \
         from the catalog: with no `ghooks` row the granted arm compiles the ungranted policy"
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
        "ungranted must not write the consumer's git hooks — this is the control that makes \
         the grant above meaningful, and a git hook is code that runs on the next commit"
    );

    let absent = Fixture::new("ghooks");
    std::fs::create_dir_all(absent.project.join(".git")).unwrap();
    assert!(
        !absent.reaches(
            "ghooks",
            true,
            &format!("mkdir -p {}", q(&absent.project.join(".git/hooks")))
        ),
        "KNOWN LIMITATION: with .git/hooks absent the literal grant cannot create it on \
         Linux, same absent-path rule drop as siblingDirs. NOT pinned while this test is \
         ignored — it currently passes for the wrong reason, since an absent catalog entry \
         denies the write too. It only becomes a pin once the entry exists"
    );
}

/// Every arm is still a jail. A curated grant is a NARROW addition, so if one arrived with
/// the home secret or the project tree attached, every per-stage result above would be
/// measuring a hole rather than a grant.
#[test]
fn no_arm_leaks_the_home_secret_or_the_project_tree() {
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
                &format!("touch {}", q(&fx.project.join("evil.txt")))
            ),
            "granted={granted}: the project root must stay unwritable"
        );
        assert!(
            !fx.reaches(
                "@prisma/client",
                granted,
                &format!("touch {}", q(&fx.enclosing_nm.join("evil.txt")))
            ),
            "granted={granted}: the enclosing node_modules must stay unwritable — it holds \
             .bin and the virtual store"
        );
        assert!(
            fx.reaches("@prisma/client", granted, "cat own.txt"),
            "granted={granted}: the child must still read its own package dir, or every \
             denial above proves only that it never ran"
        );
    }
}

//! The build jail's project-grant experimental seam: inert and unreachable in a default
//! build, and a real policy differential in a featured one.
//!
//! ONE test in its own binary, deliberately. The seam latches into a process-global
//! `OnceLock`, so the default-build control and the post-latch differential have to be
//! ordered within a single test — and no sibling test may share the process and observe
//! a latch it did not set.
//!
//! Every assertion below is paired with its own control. Asserting only that a featured
//! build withholds the project grant would pass just as well against a compiler that
//! granted nothing at all, or against a `homes()` whose project path never matched; so
//! the grant is asserted PRESENT first, and the PM-cache grants — which the seam must not
//! touch — are asserted present in both arms.

use nub_sandbox::matcher::{Homes, PathMatcher};
use nub_sandbox::policy::Effect;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Fixed anchors, drive-qualified on Windows because a drive-less path is not absolute
/// there and the compiler would fail to anchor every candidate — a green proving nothing.
fn homes() -> Homes {
    let drive = if cfg!(windows) { "C:" } else { "" };
    let at = |p: &str| PathBuf::from(format!("{drive}{p}"));
    Homes {
        home: at("/home/u"),
        tmp: at("/tmp"),
        cache: at("/home/u/.cache"),
        project: at("/proj"),
    }
}

/// The ISOLATED store shape, which is the layout the corpus study runs under and the
/// only one where the seam is observable: a package built there has its own enclosing
/// `node_modules`, so `grant_build_jail_dependency_reads`'s per-package root is distinct
/// from the project's. Under a HOISTED layout the two coincide and the arm cannot drop
/// the project's `node_modules` — asserted below rather than left to be rediscovered.
fn compile(homes: Homes, package_dir: &Path) -> nub_sandbox::SandboxPolicy {
    nub_sandbox::compile_build_jail(
        homes.clone(),
        package_dir,
        None,
        Vec::new(),
        Vec::new(),
        BTreeMap::new(),
    )
    .expect("the build jail compiles")
}

fn isolated(homes: &Homes) -> PathBuf {
    homes
        .project
        .join("node_modules/.store/dep@1.0.0/node_modules/dep")
}

fn allows(policy: &nub_sandbox::SandboxPolicy, path: &Path) -> bool {
    matches!(
        PathMatcher::new(&policy.fs.rules).decide(path).effect,
        Effect::Allow
    )
}

#[test]
fn the_project_grant_seam_is_inert_by_default_and_drops_only_the_project_roots() {
    let homes = homes();
    let package_dir = isolated(&homes);
    let manifest = homes.project.join("package.json");
    let modules = homes.project.join("node_modules/some-dep/index.js");
    // Not part of the seam, and the control that a "grant gone" assertion below is the
    // seam firing rather than the whole grant list collapsing: the PM cache roots share
    // one `roots` vector with the project pair, and the package's own enclosing
    // `node_modules` is a separate grant with a separate rationale (workspace members).
    let store = homes.cache.join("nub/pm/store");
    // Read only by the feature-gated arm assertions below; binding it unconditionally
    // trips `unused_variable` on a default build and fails the workspace clippy gate.
    #[cfg(feature = "build-jail-project-grant-arm")]
    let own_modules = homes
        .project
        .join("node_modules/.store/dep@1.0.0/node_modules/sibling/index.js");

    // Control, and the shipped behaviour in every build: unset means untouched.
    assert_eq!(
        nub_sandbox::arm::decide(None),
        Ok(nub_sandbox::arm::Decision::Default)
    );
    let default = compile(homes.clone(), &package_dir);
    assert!(
        allows(&default, &manifest) && allows(&default, &modules),
        "the default build must grant the project manifest and node_modules"
    );
    assert!(
        allows(&default, &store),
        "the PM store grant is not the seam"
    );

    // A set variable is never a silent no-op — the property the prior corpus run's
    // unread `NUB_CORPUS_NO_JAIL` lacked, which let a "jail off" arm measure jail on.
    #[cfg(not(feature = "build-jail-project-grant-arm"))]
    {
        let refusal = nub_sandbox::arm::decide(Some("off"))
            .expect_err("a default build must refuse the variable, not ignore it");
        assert!(
            refusal.contains("build-jail-project-grant-arm"),
            "the refusal must name the feature that would honour it: {refusal}"
        );
    }

    #[cfg(feature = "build-jail-project-grant-arm")]
    {
        assert_eq!(
            nub_sandbox::arm::decide(Some("off")),
            Ok(nub_sandbox::arm::Decision::ProjectGrantDropped)
        );
        nub_sandbox::arm::latch_for_test(true);
        let dropped = compile(homes.clone(), &package_dir);
        assert!(
            !allows(&dropped, &manifest) && !allows(&dropped, &modules),
            "the active arm must withhold both project roots"
        );
        assert!(
            allows(&dropped, &store) && allows(&dropped, &own_modules),
            "the arm drops the two PROJECT roots only — the PM cache and the package's \
             own enclosing node_modules stay"
        );

        // THE ARM'S ONE BLIND SPOT, pinned so it is a known limit rather than a silent
        // one: under a HOISTED layout the package's enclosing `node_modules` IS the
        // project's, so that separate grant re-admits it and only `package.json` is
        // actually withheld. The study runs the isolated linker, where they differ.
        let hoisted = compile(homes.clone(), &homes.project.join("node_modules/dep"));
        assert!(!allows(&hoisted, &manifest));
        assert!(
            allows(&hoisted, &modules),
            "hoisted: the per-package grant re-admits the project node_modules"
        );
    }
}

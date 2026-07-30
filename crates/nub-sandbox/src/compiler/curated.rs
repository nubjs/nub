//! The per-package PROJECT filesystem grant for the build jail.
//!
//! §0c asks one question — "what is the smallest positive grant set that works, and which
//! packages have earned an exception?" — and this module is the second half of it. A handful
//! of long-known codegen and hook-installer packages write outside the one subtree the jail's
//! baseline grants them (their own `package_dir`), and without an exception each fails an
//! install that works everywhere else. §0g settles the direction: carve them automatically,
//! curated by nub, because a jail that breaks installs gets turned off and then protects
//! nothing.
//!
//! # The authorship invariant, held structurally (§0e)
//!
//! The table is a nub-side `static`, exactly like [`DOWNLOAD_HOSTS`](super::DOWNLOAD_HOSTS).
//! Nothing here is read from a dependency's manifest, so there is no spelling by which a
//! package names itself into it. The KEY is aube's installer-resolved `registry_name()` — the
//! name the RESOLVER assigned, never the `name` a package wrote into its own manifest — so a
//! `file:` dep, a workspace member, or a symlinked manifest declaring `"name": "prisma"`
//! resolves under its own real identity and matches nothing. aube withholds the name entirely
//! (`None`) whenever its root is a checkout it FETCHED, so attacker-authored content at the
//! root cannot reach this table at all. The VALUE is now an access LEVEL rather than a path
//! list, which removes the second channel outright: there is no longer any path for a
//! contributor to get wrong, and nothing to clamp.
//!
//! # Why the grant is the whole project tree
//!
//! The shape this replaced had six per-package path fields — `siblingDirs`, `projectReads`,
//! `projectNodes`, `projectWrites` in two sub-shapes, and `projectCwd` — each with its own
//! resolution rule, its own clamp, and its own platform caveat. It bought precision the
//! threat model does not spend: the control that protects a user is that an UNLISTED package
//! gets nothing, and every package in this table was reviewed in a pull request. Narrowing a
//! reviewed package's reach to `.git/hooks` instead of the project tree constrains somebody
//! already trusted while doing nothing about the unvetted package a worm just reached.
//!
//! The precision also did not survive contact with the backends. `projectCwd` was
//! load-bearing on Seatbelt and a measured NO-OP on Landlock (`chdir` is not a
//! Landlock-handled access), which is how the Prisma entry shipped broken after being
//! measured only on macOS. A `siblingDirs` grant for a directory the package was about to
//! CREATE could not be attached at all on Linux — `landlock_add_rule` takes an `O_PATH`
//! descriptor — and needed a `create_dir_all` side effect during policy compilation to work.
//! Both problems are properties of granting a path that may not exist yet. The project root
//! always exists, so both dissolve rather than being fixed.
//!
//! # What the grant costs, stated plainly
//!
//! `ReadWrite` includes `<project>/node_modules`, which is the maintainer's explicit call and
//! is what makes `.prisma` and `@types` work with no compiler special-casing. It also covers
//! `node_modules/.bin` (shims later tooling runs UNCONFINED) and the virtual store (every
//! materialized dependency's source before it executes), and `<project>/.git/hooks` (code that
//! runs on the developer's next commit). Those are the persistence vectors the jail exists to
//! close, and they are handed to fourteen named, reviewed packages rather than to the
//! dependency tree at large.
//!
//! IT ALSO REACHES THE PROJECT'S SECRET FILES — `<project>/.env*`, `<project>/.npmrc` and
//! `.git/config`'s credential-bearing remote URL — and that is the sharpest edge of the
//! tradeoff, so it is stated rather than left to be discovered. The `.env*`/`.npmrc` deny band
//! is ALREADY inert in the build jail (`preset::enforce_pure_allowlist` strips every deny per
//! §0b.1), so those files were protected by not being GRANTED, and a grant over the tree is
//! exactly what removes that protection. The exception cannot be expressed: withholding one
//! file from inside a granted subtree is the deny-in-grant shape neither Landlock nor Windows
//! AppContainer can represent. Pinned by
//! `preset::tests::a_project_grant_reaches_the_consumers_secret_files_and_an_unlisted_package_does_not`,
//! whose control is that an UNLISTED package still reaches none of them.
//!
//! DO NOT generalize either level into a baseline grant. Package identity is the entire thing
//! that makes this sound.

use crate::compiler::defaults;
use crate::matcher::path::canonicalize_glob_prefix;
use crate::policy::{CanonGlob, Effect, FsAccess, FsOrigin, FsRule, SandboxPolicy};
use std::path::Path;

/// How much of the consumer's project tree one package may reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectAccess {
    /// The project tree, readable. The smaller grant — prefer it: a code generator needs its
    /// schema readable, not writable.
    ///
    /// No SHIPPED entry uses it, hence the `allow`. It is not speculative schema: `build.rs`
    /// accepts `"project": "read"` and emits this variant, so deleting it would turn the
    /// smaller of the two grants into a build error and push the next contributor to
    /// `readwrite` for a package that only needs to read. Every one of the fourteen entries
    /// today writes, which is why the corpus produced no read-only case.
    #[allow(dead_code)]
    Read,
    /// The project tree, read-write, `node_modules` included.
    ReadWrite,
}

// THE TABLE IS DATA, NOT CODE. `CURATED_GRANTS` is generated by `build.rs` from
// `data/build-jail-catalog.json`, whose entries carry only an access level — the evidence that
// earned each one is prose in `wiki/research/build-jail-provenance.md`. Moving to a data file
// did NOT move the authorship invariant: this is still a nub-side `static` baked in at compile
// time, still keyed on aube's installer-resolved name (see the module docs).
include!(concat!(env!("OUT_DIR"), "/curated_grants.rs"));

/// Grant `package_name`'s project-tree access, if it has any.
///
/// `None` — aube's root is a checkout it fetched, so there is no consumer-anchored identity —
/// grants nothing, which is the conservative direction and the reading §0e requires.
///
/// Appended rather than front-inserted: this is the NARROWEST grant in the policy and must win
/// under last-match-wins over the front-inserted dependency-tree READ it nests inside, exactly
/// as the surface's own `package_dir` rw entry does.
pub fn grant_curated_package(
    policy: &mut SandboxPolicy,
    project_root: &Path,
    package_name: Option<&str>,
) {
    let Some(access) = package_name.and_then(lookup) else {
        return;
    };
    let access = match access {
        ProjectAccess::Read => FsAccess::Read,
        ProjectAccess::ReadWrite => FsAccess::ReadWrite,
    };
    // The node AND its subtree: `project_cwd` used to grant the node alone so `uv_cwd` would
    // resolve without handing over the contents. A tree grant subsumes it.
    for glob in defaults::subtree_globs(&project_root.to_string_lossy()) {
        policy.fs.rules.entries.push(FsRule {
            matcher: CanonGlob(canonicalize_glob_prefix(&glob)),
            effect: Effect::Allow,
            access,
            // The project root exists by construction, but `Speculative` is still right: the
            // subtree glob's prefix is canonicalized against a live tree, and
            // `compile_mount_plan` REFUSES a missing AUTHORED source — which would abort the
            // very install this exception exists to keep working.
            origin: FsOrigin::Speculative,
        });
    }
}

fn lookup(name: &str) -> Option<ProjectAccess> {
    CURATED_GRANTS
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, access)| *access)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn project() -> PathBuf {
        PathBuf::from(if cfg!(windows) { "C:/proj" } else { "/proj" })
    }

    fn rules_for(name: Option<&str>) -> Vec<(String, FsAccess)> {
        let mut policy = SandboxPolicy::default();
        grant_curated_package(&mut policy, &project(), name);
        policy
            .fs
            .rules
            .entries
            .iter()
            .map(|r| (r.matcher.as_str().to_string(), r.access))
            .collect()
    }

    /// The grant is the project tree at the entry's access level, and an unlisted package gets
    /// nothing. The negative arm is what keeps the positive one from passing against a
    /// function that grants unconditionally.
    #[test]
    fn a_listed_package_gets_the_project_tree_and_an_unlisted_one_gets_nothing() {
        let granted = rules_for(Some("ghooks"));
        let root = project().to_string_lossy().into_owned();
        assert!(
            granted
                .iter()
                .any(|(g, a)| *g == root && *a == FsAccess::ReadWrite),
            "expected the project root read-write, got {granted:?}"
        );
        assert!(
            granted
                .iter()
                .any(|(g, a)| *g == format!("{root}/**") && *a == FsAccess::ReadWrite),
            "expected the project SUBTREE read-write, got {granted:?}"
        );
        assert!(rules_for(Some("evil")).is_empty());
        assert!(rules_for(None).is_empty());
    }

    /// A package cannot rename itself into the table: the lookup is exact, with no prefix,
    /// suffix or case folding.
    #[test]
    fn only_the_installer_resolved_name_matches() {
        assert!(!rules_for(Some("@prisma/client")).is_empty(), "the control");
        for impostor in [
            "@prisma/client-x",
            "x@prisma/client",
            "@Prisma/Client",
            "prisma",
        ] {
            assert!(
                rules_for(Some(impostor)).is_empty(),
                "{impostor} must not match the @prisma/client entry"
            );
        }
    }

    /// Every grant is a pure positive Allow, which is what keeps the build jail's
    /// pure-allowlist invariant (§0b.1) and Windows' `deny_shadows_grant` satisfied.
    #[test]
    fn no_curated_grant_emits_a_deny() {
        for (name, _) in CURATED_GRANTS {
            let mut policy = SandboxPolicy::default();
            grant_curated_package(&mut policy, &project(), Some(name));
            assert!(
                policy
                    .fs
                    .rules
                    .entries
                    .iter()
                    .all(|r| r.effect == Effect::Allow),
                "{name} emitted a deny rule"
            );
        }
    }

    /// The two grants whose absence was MEASURED against a real denial, pinned so a catalog
    /// edit cannot drop them silently. `@prisma/client` is the macOS+Linux three-stage
    /// measurement and `ghooks` the hook-installer cohort's representative; both need write,
    /// so a downgrade to `Read` fails here too.
    #[test]
    fn the_measured_grants_survive_the_schema_migration() {
        for name in [
            "@prisma/client",
            "@danmarshall/deckgl-typings",
            "msw",
            "ghooks",
        ] {
            assert_eq!(
                lookup(name),
                Some(ProjectAccess::ReadWrite),
                "{name} lost its measured read-write project grant"
            );
        }
    }
}

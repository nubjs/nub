//! Which package manager's rules a project runs under.
//!
//! Every project is exactly one of two things, and the answer decides which
//! configuration nub reads and which names the engine writes:
//!
//! - **pnpm-incumbent** — the project already declares pnpm, by a
//!   `packageManager` pin, a `pnpm-lock.yaml`, or a `pnpm-workspace.yaml`.
//!   The engine resolves its own configuration and writes pnpm's own names,
//!   so the project sees pnpm 12 behaviour exactly.
//! - **nub-incumbent** — everything else, a fresh directory included. nub
//!   resolves the configuration itself and the engine writes nub's names.
//!
//! There is no third answer. npm, yarn and bun no longer confer an identity:
//! their lockfiles survive only as something `nub pm migrate` converts, and a
//! first command in such a repo behaves like pnpm.
//!
//! The marker search walks up from the starting directory, because a command
//! run inside a workspace member has to reach the same verdict as the same
//! command run at the root. The nearest ancestor carrying ANY marker decides:
//! a nub project nested inside a pnpm monorepo is nub's, and a pnpm project
//! nested inside a nub one is pnpm's. The one bound on that walk is nub's own
//! PM cache root, which an install running inside must not escape (#489).
//!
//! What a single directory declares is [`nub_core::pm::identity`]'s rule, not
//! this module's: workspace detection reads the same answer to decide which
//! members `run -r` and `--filter` see, and a second copy is how the two once
//! disagreed about a pnpm workspace with no lockfile yet.

use crate::project_config::InstallConfig;
use anyhow::{Result, bail};
pub(crate) use nub_core::pm::identity::ProjectIdentity;
use nub_core::pm::identity::identity_of_dir;
use std::path::Path;

/// The identity of the project containing `start_dir`.
///
/// Cheap enough to call on the command path: at most four `stat`-shaped
/// probes and one small manifest read per ancestor, and it stops at the
/// first directory that carries a marker.
pub(crate) fn detect(start_dir: &Path) -> ProjectIdentity {
    detect_within(start_dir, super::pm_cache_clamp(start_dir).as_deref())
}

/// Pure core of [`detect`] (unit-tested without touching the environment).
///
/// `clamp`, when set, is the last directory the walk may consider — nub's own
/// PM cache root. An install running inside that root is nub-internal by
/// construction (the node-gyp bootstrap's nested install, a dlx scratch dir),
/// so it must not inherit an identity from whatever sits above the cache
/// (#489). Unclamped the failure is silent rather than loud: a
/// `pnpm-lock.yaml` anywhere above `~/.cache` hands the internal install pnpm's
/// profile, which then reads pnpm's configuration and writes `pnpm-lock.yaml`
/// into nub's own cache.
fn detect_within(start_dir: &Path, clamp: Option<&Path>) -> ProjectIdentity {
    for dir in start_dir.ancestors() {
        if let Some(identity) = identity_of_dir(dir) {
            return identity;
        }
        // Checked after `identity_of_dir`, so the cache root itself still gets
        // to declare an identity — it is the level ABOVE it that is off limits.
        if clamp == Some(dir) {
            break;
        }
    }
    ProjectIdentity::Nub
}

/// Reject an `install` block written in a project pnpm already owns.
///
/// The two configurations describe the same install in two different
/// vocabularies, and nothing decides which wins: under pnpm incumbency the
/// engine reads `pnpm-workspace.yaml` and never sees nub's block, so
/// honouring the block would need nub to override the very configuration it
/// promised to defer to. Naming both files is the whole point of the
/// message — the reader has to know which two to reconcile.
pub(crate) fn check_install_block(
    identity: ProjectIdentity,
    config_path: &Path,
    install: &InstallConfig,
) -> Result<()> {
    if identity == ProjectIdentity::Nub || install == &InstallConfig::default() {
        return Ok(());
    }
    bail!(
        "{} sets an `install` block, but this project already belongs to pnpm.\n\
         A pnpm project takes its install settings from pnpm's own configuration, so the \
         block would be ignored. Move those settings into pnpm-workspace.yaml, or remove \
         pnpm's files to make this a nub project.",
        config_path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create the fixture directory");
        }
        fs::write(path, body).expect("write the fixture file");
    }

    #[test]
    fn each_pnpm_marker_on_its_own_claims_the_project() {
        for marker in ["pnpm-lock.yaml", "pnpm-workspace.yaml"] {
            let dir = tempdir().unwrap();
            write(&dir.path().join("package.json"), r#"{"name":"fx"}"#);
            write(&dir.path().join(marker), "");
            assert_eq!(
                detect(dir.path()),
                ProjectIdentity::Pnpm,
                "{marker} must claim the project"
            );
        }
        for manifest in [
            r#"{"name":"fx","packageManager":"pnpm@12.4.1"}"#,
            r#"{"name":"fx","devEngines":{"packageManager":{"name":"pnpm"}}}"#,
        ] {
            let dir = tempdir().unwrap();
            write(&dir.path().join("package.json"), manifest);
            assert_eq!(
                detect(dir.path()),
                ProjectIdentity::Pnpm,
                "a pnpm pin must claim the project"
            );
        }
    }

    /// The control for the four above: with no marker at all the same walk
    /// must reach nub, or those assertions would pass on a detector that
    /// always answers `Pnpm`.
    #[test]
    fn a_project_with_no_marker_is_nubs() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("package.json"), r#"{"name":"fx"}"#);
        assert_eq!(detect(dir.path()), ProjectIdentity::Nub);
        assert_eq!(
            detect(tempdir().unwrap().path()),
            ProjectIdentity::Nub,
            "a fresh directory is nub's"
        );
    }

    /// npm, yarn and bun no longer confer an identity of their own — a repo
    /// that declares one of them is nub's, and the first command behaves
    /// like pnpm rather than like the named tool.
    #[test]
    fn another_package_managers_pin_does_not_claim_the_project() {
        for name in ["npm@11.0.0", "yarn@4.0.0", "bun@1.1.0"] {
            let dir = tempdir().unwrap();
            write(
                &dir.path().join("package.json"),
                &format!(r#"{{"name":"fx","packageManager":"{name}"}}"#),
            );
            write(&dir.path().join("package-lock.json"), "{}");
            assert_eq!(
                detect(dir.path()),
                ProjectIdentity::Nub,
                "{name} must not claim the project"
            );
        }
    }

    #[test]
    fn a_command_run_inside_a_member_reaches_the_roots_verdict() {
        let root = tempdir().unwrap();
        write(
            &root.path().join("pnpm-workspace.yaml"),
            "packages:\n  - packages/*\n",
        );
        let member = root.path().join("packages").join("a");
        write(&member.join("package.json"), r#"{"name":"@fx/a"}"#);
        assert_eq!(detect(&member), ProjectIdentity::Pnpm);
    }

    /// The nearest ancestor decides, so a nub project nested inside a pnpm
    /// monorepo keeps its own identity instead of inheriting the root's.
    #[test]
    fn the_nearest_marker_wins_over_a_further_ancestor() {
        let root = tempdir().unwrap();
        write(
            &root.path().join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n",
        );
        let nested = root.path().join("vendored").join("tool");
        write(&nested.join("package.json"), r#"{"name":"tool"}"#);
        write(&nested.join("nub.lock"), "lockfileVersion: '9.0'\n");
        assert_eq!(detect(&nested), ProjectIdentity::Nub);
    }

    #[test]
    fn an_install_block_is_rejected_only_in_a_pnpm_project() {
        let configured = InstallConfig {
            public_hoist: Some(vec!["*".to_string()]),
            ..Default::default()
        };
        let path = Path::new("/fx/nub.jsonc");

        let err = check_install_block(ProjectIdentity::Pnpm, path, &configured)
            .expect_err("an install block under pnpm incumbency is not a sound configuration");
        let message = err.to_string();
        assert!(
            message.contains("nub.jsonc"),
            "the message must name nub's file: {message}"
        );
        assert!(
            message.contains("pnpm-workspace.yaml"),
            "the message must name pnpm's file: {message}"
        );

        check_install_block(ProjectIdentity::Nub, path, &configured)
            .expect("a nub project is exactly where an install block belongs");
        check_install_block(ProjectIdentity::Pnpm, path, &InstallConfig::default())
            .expect("a pnpm project that writes no install settings has nothing to reconcile");
    }

    /// An install inside nub's PM cache is nub-internal by construction, so a
    /// pnpm marker above the cache must not reach it (#489).
    ///
    /// Both halves matter and the unclamped one is the control: without it the
    /// clamped assertion passes whenever the fixture fails to confer pnpm
    /// identity for some unrelated reason. Driving [`detect_within`] rather
    /// than [`detect`] keeps the clamp explicit instead of resolving it from
    /// `$XDG_CACHE_HOME`, which is process-global and would make this test race
    /// its siblings under cargo's thread pool.
    #[test]
    fn the_cache_root_stops_an_identity_walk() {
        let outer = tempdir().unwrap();
        write(
            &outer.path().join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n",
        );
        let cache_root = outer.path().join("cache").join("nub").join("pm");
        let internal = cache_root.join("tools").join("node-gyp");
        write(
            &internal.join("package.json"),
            r#"{"name":"node-gyp-host"}"#,
        );

        assert_eq!(
            detect_within(&internal, Some(&cache_root)),
            ProjectIdentity::Nub,
            "a marker above the cache root must not claim an internal install"
        );
        assert_eq!(
            detect_within(&internal, None),
            ProjectIdentity::Pnpm,
            "unclamped the same tree DOES inherit pnpm, which is what the clamp prevents"
        );
    }
}
